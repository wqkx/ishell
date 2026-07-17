//! AI/MCP 控制通道：本地 Unix domain socket 桥接，供独立的 `ishell-mcp` stdio 代理进程
//! （见 `src/bin/mcp_stdio.rs`）连接，把 list/run/poll/read/interrupt 请求转发到本进程
//! 持有的活跃 SSH 会话。默认关闭（`store::load_mcp_consent()`），一次 socket 连接 = 一问一答。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::mcp_protocol::{
    McpReqKind, McpReqResult, McpRequest, McpResponse, McpRunResult, McpSavedConn, McpSessionInfo,
};
use crate::proto::{AuthMethod, ConflictPolicy, ConnectConfig, Eol, JumpHost, UiCommand};
use crate::store::SavedConnection;

use super::{App, Session};

/// AI 侧 timeout_ms 没有类型上限（u64，调用方随便传），但 `Instant + Duration` 在结果超出
/// 底层时钟能表示的范围时会 panic（不是回绕，是直接崩掉整个 GUI 进程）——夹到一个安全上限
/// （24 小时，对"一次调用等一个长任务跑完"这种用法完全够用，又远小于任何平台时钟的表示上限）
/// 再构造 Duration，同时兜底下限 100ms（原有行为）。
const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

fn clamp_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.clamp(100, MAX_TIMEOUT_MS))
}

impl Session {
    /// 不消费任何东西，只回答"这个 id 的写结果是不是该被 MCP 接下来"（真有挂起的写操作
    /// 匹配、或者是个该被丢弃的迟到墓碑）。调用方据此决定要不要把事件内容 move 给
    /// `try_resolve_file_write`，还是原样转发给普通编辑器 UI——避免为了"先试一下能不能
    /// 匹配"而白白 clone 一份内容。
    pub(super) fn file_write_op_would_resolve(&self, id: u64) -> bool {
        self.file_op_tombstones.contains(&id)
            || self.pending_file_ops.iter().any(
                |op| matches!(op.kind, FileOpKind::Write { op_id } if op_id == id),
            )
    }

    /// 同上，读操作版本。
    pub(super) fn file_read_op_would_resolve(&self, id: u64) -> bool {
        self.file_op_tombstones.contains(&id)
            || self.pending_file_ops.iter().any(
                |op| matches!(op.kind, FileOpKind::Read { op_id } if op_id == id),
            )
    }

    /// 放弃当前挂起的 AI 命令运行：给还在等待的 `poll_run` 一个明确的"未完成"响应
    /// （而不是让它继续空等到超时），并取消哨兵捕获、清空 `pending_ai_run`。
    /// 用于打断（`interrupt`）、断线等"这条运行注定等不到哨兵了"的场景。
    pub(super) fn cancel_pending_ai_run(&mut self, reason: &str) {
        if let Some(mut pending) = self.pending_ai_run.take() {
            if let Some(tx) = pending.resp_tx.take() {
                // 明确回错误，而不是 Ok(Run{finished:false})——后者会让调用方以为"命令还在
                // 跑，可以继续 poll_run"，但这条 run_id 已经被清空、注定 poll 不到了。
                // 命令实际执行到哪一步、有没有副作用都无法确认，输出仅供参考。
                let output = self.terminal.peek_ai_output().unwrap_or_default();
                let _ = tx.send(McpResponse {
                    id: pending.req_id,
                    result: Err(format!(
                        "{reason}，这条运行已失效、无法再 poll_run。执行到哪一步、\
                         是否已产生副作用都无法确认。已知的部分输出（仅供参考，不保证完整）：\
                         {output}"
                    )),
                });
            }
        }
        self.terminal.cancel_ai_capture();
    }

    /// 放弃**全部**挂起的文件读写（write_file/read_file/copy_*）：给每个还在等待的响应
    /// 通道一个明确的"未完成"错误，而不是让它们一直空等到超时。用于断线等场景。
    pub(super) fn cancel_pending_file_op(&mut self) {
        for mut op in self.pending_file_ops.drain(..) {
            if let Some(tx) = op.resp_tx.take() {
                let _ = tx.send(McpResponse {
                    id: op.req_id,
                    result: Err("会话已断线，文件操作未完成".into()),
                });
            }
        }
    }

    /// `WriteFile` 对应的 worker 事件（`FileSaved`/`FileSaveFailed`/`FileSaveConflict`）到达时
    /// 调用：按请求 id（`WriteFile`/这三个事件现在都带同一个 id，跟编辑器标签的 `tid` 或
    /// AI/MCP 请求各自独立生成）匹配当前挂起的写操作，命中则回填响应并返回 true（调用方应
    /// 跳过把这个事件转发给普通编辑器 UI——MCP 触发的读写没有对应的编辑器标签页）。
    /// 按 id 而不是 path 匹配：同一路径可能同时有编辑器手动保存和 AI 写入两路请求，
    /// 仅按 path 字符串匹配会把两者的响应张冠李戴。
    pub(super) fn try_resolve_file_write(&mut self, id: u64, result: Result<u32, String>) -> bool {
        // 迟到的、已经因超时被判定失败过的操作结果：AI 早就收到过"超时"响应了，这里直接
        // 丢弃，绝不能落进普通编辑器的 pending 队列（那样会凭空建一个用户没开过的标签）。
        if self.file_op_tombstones.contains(&id) {
            return true;
        }
        let Some(pos) = self.pending_file_ops.iter().position(
            |op| matches!(op.kind, FileOpKind::Write { op_id } if op_id == id),
        ) else {
            return false;
        };
        let mut op = self.pending_file_ops.remove(pos);
        if let Some(tx) = op.resp_tx.take() {
            let req_id = op.req_id;
            let resp = match result {
                Ok(mtime) => McpResponse {
                    id: req_id,
                    result: Ok(McpReqResult::FileWritten { path: op.path, mtime }),
                },
                Err(msg) => McpResponse {
                    id: req_id,
                    result: Err(msg),
                },
            };
            let _ = tx.send(resp);
        }
        true
    }

    /// `ReadFile` 对应的 worker 事件（`FileOpened`/`FileLoadFailed`/`FileTooLarge`）到达时调用：
    /// 按 `id` 匹配当前挂起的读操作，命中则回填响应并返回 true（同上，跳过转发给编辑器 UI）。
    pub(super) fn try_resolve_file_read(&mut self, id: u64, result: Result<String, String>) -> bool {
        if self.file_op_tombstones.contains(&id) {
            return true;
        }
        let Some(pos) = self.pending_file_ops.iter().position(
            |op| matches!(op.kind, FileOpKind::Read { op_id } if op_id == id),
        ) else {
            return false;
        };
        let mut op = self.pending_file_ops.remove(pos);
        if let Some(tx) = op.resp_tx.take() {
            let req_id = op.req_id;
            let resp = match result {
                Ok(content) => McpResponse {
                    id: req_id,
                    result: Ok(McpReqResult::FileContent {
                        // op 已经是从 pending_file_ops 里取走的本地所有权、之后不再使用，
                        // 直接移动 path 而不是 clone。
                        path: op.path,
                        // read_file 允许放宽到 128MB（force），远超单条 JSON 响应该带的量，
                        // 跟 run_command 的输出上限用同一个裁剪策略。
                        content: cap_output_for_ai(content),
                    }),
                },
                Err(msg) => McpResponse {
                    id: req_id,
                    result: Err(msg),
                },
            };
            let _ = tx.send(resp);
        }
        true
    }

    /// 同上，`copy_file`（`CopyToRemote`/`CopyFromRemote`）版本：`op_id` 匹配
    /// `WorkerEvent::TransferDone`。与 write/read 不同的是，传输事件本身还要继续走
    /// `session_events.rs` 里已有的 `self.transfers` 记账（让用户在传输窗口里也能看到这次
    /// AI 发起的复制），所以调用方不应该像 write/read 那样把事件"吞掉"——这里只负责回填
    /// MCP 响应，不影响事件其余部分的处理。
    pub(super) fn file_copy_op_would_resolve(&self, id: u64) -> bool {
        self.file_op_tombstones.contains(&id)
            || self.pending_file_ops.iter().any(
                |op| matches!(op.kind, FileOpKind::Copy { op_id } if op_id == id),
            )
    }

    pub(super) fn try_resolve_file_copy(&mut self, id: u64, result: Result<(), String>) -> bool {
        if self.file_op_tombstones.contains(&id) {
            return true;
        }
        let Some(pos) = self.pending_file_ops.iter().position(
            |op| matches!(op.kind, FileOpKind::Copy { op_id } if op_id == id),
        ) else {
            return false;
        };
        let mut op = self.pending_file_ops.remove(pos);
        if let Some(tx) = op.resp_tx.take() {
            let req_id = op.req_id;
            let resp = match result {
                Ok(()) => McpResponse {
                    id: req_id,
                    result: Ok(McpReqResult::Copied { path: op.path }),
                },
                Err(msg) => McpResponse {
                    id: req_id,
                    result: Err(msg),
                },
            };
            let _ = tx.send(resp);
        }
        true
    }
}

/// Write 用 `op_id` 匹配 `FileSaved`/`FileSaveFailed`/`FileSaveConflict`；Read 用 `op_id`
/// 匹配 `FileOpened`/`FileLoadFailed`/`FileTooLarge`——两边现在都带请求 id（`proto.rs` 里
/// `WriteFile`/这三个事件都加了 id 字段），各自的 `op_id` 语义不同（Write 侧是这次
/// write_file 生成的临时 id；Read 侧同理），用各自的变量名区分。
pub(super) enum FileOpKind {
    Write { op_id: u64 },
    Read { op_id: u64 },
    /// `CopyToRemote`/`CopyFromRemote`：`op_id` 匹配 `WorkerEvent::TransferDone`（同一 id
    /// 空间，见 mcp_bridge.rs 里 nanosecond 时间戳生成 op_id 的既有写法，避免跟 GUI 自己
    /// 发起的传输、走 `Session::next_xfer` 的小整数 id 撞车）。
    Copy { op_id: u64 },
}

/// AI 的 `write_file`/`read_file`/`copy_*` 请求，正等待 worker 侧 SFTP 操作完成的事件。
/// 同一会话可同时挂多个（见 `Session::pending_file_ops`），各条用 `op_id` 区分。
pub(super) struct PendingAiFileOp {
    kind: FileOpKind,
    /// 请求的远端路径：Write 用它匹配事件，Read 用于响应里回填 `FileContent.path`。
    path: String,
    resp_tx: Option<oneshot::Sender<McpResponse>>,
    req_id: u64,
    deadline: Instant,
}

/// 单个会话同时挂起的 AI 文件操作上限。SFTP 天然支持并发、worker 侧的
/// `MAX_CONCURRENT_XFER` 也会把 copy 类操作限流+排队，所以这里给一个宽松但有界的上限——
/// 既让 AI 能一次并行发起多文件传输，又防止某个会话上无限堆积 resp_tx/占满 MCP 连接
/// 信号量（`MAX_MCP_CONNECTIONS`）而饿死 run_command 等其它调用。超过上限的新请求返回
/// "该会话并发文件操作已达上限，请稍候重试"，由 AI 侧自行退避。
const MAX_CONCURRENT_FILE_OPS: usize = 16;

/// `copy_between_sessions` 的多阶段状态：优先尝试直连（生成一次性密钥对，临时信任源→目标，
/// 直连 scp/rsync，无论成败都撤销信任），失败或超时再退化为中转（App 进程内存 duplex，
/// 源读→目标写）。这个操作要同时驱动两个会话各自的 worker，单个会话的 `pending_file_op`
/// 忙碌位不足以描述这个多阶段状态，所以单独用 `CrossCopyJob` 跟踪；两侧的 `pending_file_op`
/// 从一开始就各自占位（复用忙碌保护 + 断线清理），但 `resp_tx` 都是 `None`——真正的响应
/// 通道在这里的 `resp_tx` 字段上，跟问题 1 里 `CopyFromRemoteToCaller` 的处理方式是同一个
/// 思路。阶段流转：`TrustingB` → `DirectCopying` →（无论成败）`UntrustingAfterDirect` →
/// 直连成功则在这里直接 resolve；直连失败则转入 `RelayReading` → `RelayWriting`。
enum CrossCopyPhase {
    /// 已发 `TrustTempKey` 给目标会话，等 `TempKeyTrusted`。
    TrustingB,
    /// 已发 `DirectRelayCopy` 给源会话。`started=false` 时仍受 `phase_deadline`（较短的
    /// "建连+开始传输"超时）约束；一旦收到 `DirectRelayStarted` 就说明数据已经在传，
    /// 之后只受整个操作的总超时（`deadline`）约束，不再被这个短超时误杀。
    DirectCopying { started: bool },
    /// 已发 `UntrustTempKey` 给目标会话，等 `TempKeyUntrusted`（或 `phase_deadline` 到了
    /// 直接放弃等待）——这一步只是清理，`direct_result` 已经确定了最终走向。
    UntrustingAfterDirect,
    /// 已发 `RelayReadFile` 给源会话，等 `RelaySourceResult`。
    RelayReading,
    /// 已发 `RelayWriteFile` 给目标会话，等它的 `TransferDone`（经 `pending.copy_done`）。
    RelayWriting,
}

pub(super) struct CrossCopyJob {
    op_id: u64,
    req_id: u64,
    resp_tx: Option<oneshot::Sender<McpResponse>>,
    src_uid: u64,
    dest_uid: u64,
    src_remote_path: String,
    dest_remote_path: String,
    /// 中转模式要用的内存管道两端：直连尝试期间两个都还没发出去，直连失败时才真正派上
    /// 用场（写端发给源会话、读端在拿到 size 后发给目标会话）。直连成功则始终用不上，
    /// job 结束时随 `CrossCopyJob` 一起被丢弃。
    pipe_writer: Option<tokio::io::DuplexStream>,
    pipe_reader: Option<tokio::io::DuplexStream>,
    /// 这次直连尝试的一次性标记（authorized_keys 注释 + 撤销时的精确匹配 key）。
    marker: String,
    /// 目标主机的 authorized_keys 是否已经真的写入过这次的临时公钥（`TempKeyTrusted{ok:true}`
    /// 到达后置位）。之后任何提前退出/超时路径只要看到这个标志为真，都必须补发一次
    /// `UntrustTempKey`，否则会永久残留一把免密公钥——不能像"从没建立过信任"的早期失败
    /// 那样直接跳过撤销。
    trust_established: bool,
    /// 一次性私钥的 OpenSSH PEM 字节：只在 `TrustingB` 成功、真正发起直连尝试时取用一次
    /// （`take_priv_key_pem`），用完随 `UiCommand::DirectRelayCopy` 一起移动给源会话 worker，
    /// 不在 App 状态里滞留超过一次尝试所需的时间。
    priv_key_pem: Option<Vec<u8>>,
    /// 直连尝试的取消标志：`phase_deadline` 到了但还没收到 `DirectRelayStarted` 时置位，
    /// 源会话侧的 `exec_direct_progress` 循环会尽快感知并退出，不会真的无限空等。
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// `DirectCopying` 收尾后暂存的直连结果，供 `UntrustingAfterDirect` 阶段结束时决定
    /// 是直接 resolve 成功，还是转入中转。
    direct_result: Option<Result<(), String>>,
    phase: CrossCopyPhase,
    /// 整个操作（含可能的中转）的总超时点，来自调用方 `timeout_ms`。
    deadline: Instant,
    /// 仅在 `DirectCopying{started:false}` / `UntrustingAfterDirect` 阶段生效的短超时点；
    /// 其余阶段不受它约束。
    phase_deadline: Instant,
}

impl CrossCopyJob {
    fn take_priv_key_pem(&mut self) -> Option<Vec<u8>> {
        self.priv_key_pem.take()
    }
}

/// 直连尝试的短超时：只约束"发起到源会话真正建立连接、开始传输数据"这一段，一旦收到
/// `DirectRelayStarted` 就不再受它约束（大文件直连不会被误杀）。
const DIRECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
/// 撤销临时信任的等待上限：只是清理步骤，不需要等太久，超过就直接放弃等待、继续收尾。
const UNTRUST_WAIT: Duration = Duration::from_secs(10);

/// 把 `SavedConnection` 的 `auth_kind` 字符串 + 相应字段还原成 `AuthMethod`（跟
/// `ui/connect/form.rs::build()` 里对同一套字段的映射保持一致）。
fn auth_method(kind: &str, password: &str, key_path: &str, passphrase: &str) -> AuthMethod {
    match kind {
        "key" => AuthMethod::KeyFile {
            path: key_path.to_string(),
            passphrase: if passphrase.is_empty() {
                None
            } else {
                Some(passphrase.to_string())
            },
        },
        "agent" => AuthMethod::Agent,
        "interactive" => AuthMethod::Interactive,
        _ => AuthMethod::Password(password.to_string()),
    }
}

/// 把一条已保存连接直接转成 `ConnectConfig`，等价于侧栏双击该连接时
/// `load_saved()` + `build()` 这一对函数联合做的事。
fn connect_config_from_saved(c: &SavedConnection) -> ConnectConfig {
    let jump = c.use_jump.then(|| JumpHost {
        host: c.jump_host.clone(),
        port: c.jump_port,
        username: c.jump_username.clone(),
        auth: auth_method(&c.jump_auth_kind, &c.jump_password, &c.jump_key_path, &c.jump_passphrase),
    });
    ConnectConfig {
        host: c.host.clone(),
        port: c.port,
        username: c.username.clone(),
        auth: auth_method(&c.auth_kind, &c.password, &c.key_path, &c.passphrase),
        label: c.name.clone(),
        jump,
        forward_agent: c.forward_agent,
    }
}

/// 一次从 socket 收到的请求，附带用于回填响应的 oneshot。
pub(super) struct McpCall {
    req: McpRequest,
    resp_tx: oneshot::Sender<McpResponse>,
    /// 仅内部 `CopyToRemoteFromCaller` 携带。读取端属于本条 socket，交给 SSH worker
    /// 消费；常规 JSON 请求保持一问一答，不带任何额外数据。
    upload_source: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
    /// 仅内部 `CopyFromRemoteToCaller` 携带：worker 探测远端路径后，通过它把「文件大小 +
    /// 内存管道读端」（成功）或错误信息（目录/远端不可访问）直接回给 `handle_conn`——
    /// 这条通道自带成功/失败两种结果，`resp_tx` 对这个操作只在"App 层校验就失败"（会话
    /// 不存在、路径非法等，根本没到 worker）时才会被用到，两者互斥、不会竞争。
    download_sink: Option<oneshot::Sender<Result<crate::proto::DownloadStreamSource, String>>>,
}

/// 某会话上一次正在等待的 AI 命令运行（`run_command` 武装，`poll_run` 续等）。
pub(super) struct PendingAiRun {
    run_id: u64,
    deadline: Instant,
    /// 当前这次调用（run_command 或最近一次 poll_run）待回填的响应通道 + 请求 id。
    resp_tx: Option<oneshot::Sender<McpResponse>>,
    req_id: u64,
    /// 原始命令文本：用于把回填给 AI 的输出里，命令自身的回显行去掉（人看的终端不受影响，
    /// 只裁剪返回给 AI 的文本，省 token）。
    command: String,
    /// 哨兵命中时若恰好没有 waiter（resp_tx 为 None——比如上一次已经因超时把响应发出去，
    /// 下一次 poll_run 还没打进来），必须把结果缓存在这里，不能直接丢掉再清空
    /// pending_ai_run：否则随后的 poll_run 会看到"这个会话没有任何挂起的运行"，
    /// 返回"run_id 不存在或已结束"，而这条命令其实已经跑完了，结果却永久丢失。
    finished_result: Option<(i32, String)>,
}

/// 开头这条命令自己的回显行不是命令产生的内容，我们知道发的是什么，原样比对裁掉。
/// 命令是否已经跑完都适用（回显在命令刚发出时就到达，跟命令有没有跑完无关）。
/// 注意：命令长到被终端硬换行（回显中间插入了 `\r\n`）时，这里的整段精确匹配会失配，
/// 退回原样返回——不裁但也不会误伤，只是这一种情况下省不了这份 token，属已知取舍。
///
/// 这是目前唯一会对返回给 AI 的输出做裁剪的地方——曾经还有一个 `trim_command_echo_and_prompt`
/// 尝试顺手裁掉结尾那段"没有换行收尾的提示符残片"（标记行紧跟在提示符后面同一行打），
/// 但那是个无法证明安全的启发式：PS1 为空、prompt 本身不可见、shell 处于特殊非交互配置等
/// 情况下，最后一段完全可能是命令的真实输出而不是提示符，删掉就是丢真实数据。宁可在输出
/// 结尾保留一小段提示符噪音，也不能有丢真实内容的风险，所以已经把那个函数去掉了。
fn trim_leading_echo(output: &str, command: &str) -> String {
    match output.strip_prefix(command.trim()) {
        Some(rest) => rest.trim_start_matches(['\r', '\n']).to_string(),
        None => output.to_string(),
    }
}

/// 单次 run_command/poll_run 返回给 AI 的输出上限（字符数）：命令本身没有输出上限
/// （比如一次跑起来的构建日志可能几十 MB），但把这么大的内容整段塞进一条 JSON-RPC
/// 响应，容易撑爆传输层（stdio 管道、反向转发的 SSH 通道）导致整条响应都送不到——
/// 对 AI 来说比"输出被裁掉一部分"严重得多：会看到一个完全不透明的传输失败，
/// 分不清命令到底有没有跑、跑没跑完，贸然重试还可能重复执行有副作用的命令。
/// 裁的是给 AI 看的这一份，用户在终端上看到的原始内容不受影响；保留末尾（最新的输出
/// 通常最相关），開头加一句提示，和 AI_CAPTURE_CAP 截断时的提示保持一致的风格。
/// 按字节数而不是字符数限制：真正要保护的是传输层（stdio 管道、Unix socket、反向转发的
/// SSH 通道）能扛住的字节量，不是字符数量。中文等多字节字符下，字符数上限会让实际字节数
/// （UTF-8 里最多到 4 倍）远超预期——按字节限制才跟"防止 JSON-RPC 响应撑爆传输层"这个
/// 目的对得上。
const MAX_RUN_OUTPUT_BYTES: usize = 200_000;

/// `copy_file` 用的极简远端路径工具：SFTP 远端路径总是 POSIX 风格，不需要
/// `ssh::sftp`（不对 `src/app` 公开）里那套一致的 `remote_parent`/`basename` 实现，
/// 这里按同样的语义写一份小的，避免为了共享几行逻辑扩大那边的可见性。
/// 只应该在 `remote_path` 已经过 `validate_remote_path` 校验之后调用：对相对路径，
/// `rfind('/')` 找不到分隔符会退化成 `"/"`，把上传目标悄悄改到文件系统根目录。
fn remote_basename(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string()
}

fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// 校验一个 `copy_file` 用的远端 POSIX 路径：必须绝对、不含 `.`/`..` 路径段、拆出来的
/// 文件名非空。三者任一不满足，`remote_parent`/`remote_basename` 拆分出来的目标要么会
/// 落在意料之外的目录（`.`/`..` 段）、要么文件名为空（`"/"`、`"////"` 这类路径）——
/// 不校验的话这些都要等 SFTP 报错才会发现，报错还来得晚、含义也不清楚。
fn validate_remote_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("remote_path 必须是绝对路径：{path}"));
    }
    if path.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(format!("remote_path 不能包含 \".\" 或 \"..\" 路径段：{path}"));
    }
    if remote_basename(path).is_empty() {
        return Err(format!("remote_path 缺少有效的文件名：{path}"));
    }
    Ok(())
}

/// 校验一个 `copy_file` 用的本地路径：必须绝对、不含 `.`/`..` 路径段、有有效文件名
/// （拒绝文件系统根目录等 `Path::file_name()` 返回 `None` 的路径）。
fn validate_local_path(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err(format!("local_path 必须是绝对路径：{path}"));
    }
    // 按原始字符串拆分校验，不用 `Path::components()`——它会把内部的 "." 直接
    // 规整掉（只有开头的 "." 才会被保留成 `Component::CurDir`），导致
    // "/tmp/./notes.txt" 这类路径检测不到，字符串层面直接拆分才可靠。
    if path.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(format!("local_path 不能包含 \".\" 或 \"..\" 路径段：{path}"));
    }
    if p.file_name().is_none() {
        return Err(format!("local_path 缺少有效的文件名：{path}"));
    }
    Ok(())
}

fn cap_output_for_ai(output: String) -> String {
    if output.len() <= MAX_RUN_OUTPUT_BYTES {
        return output;
    }
    // 直接从「总字节数 - 上限」的位置起步找一个合法 UTF-8 字符边界（最多再往前挪 3 个
    // 字节），不需要像旧实现那样扫描整个字符串——常见的「没超限」情况是 O(1)，
    // 超限截断也只需要一次切片 + 一次 format! 分配，不会额外复制上百 MB 的中间副本。
    let mut start = output.len() - MAX_RUN_OUTPUT_BYTES;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    format!("[输出过长，已截断保留末尾部分]\n{}", &output[start..])
}

/// 4 字节随机后缀（十六进制），拼进跨会话拷贝的一次性 marker——`op_id`（纳秒时间戳）本身
/// 已经足够不重复，这里只是防止极端情况下同一纳秒内发起多次调用导致 marker 撞车。
fn rand_marker_suffix() -> String {
    let mut b = [0_u8; 4];
    if getrandom::getrandom(&mut b).is_err() {
        // 极罕见回退：不追求密码学随机性，只要求不同并发调用之间大概率不撞车即可。
        let seed = (std::process::id() as u64) ^ (&b as *const _ as u64);
        b.copy_from_slice(&seed.to_le_bytes()[..4]);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `ssh_key::PrivateKey::random` 需要一个实现 `rand_core::CryptoRng` 的生成器；
/// `rand_core`（经 `ssh_key::rand_core` 重导出，保证跟 `ssh_key` 内部用的是同一份类型，
/// 不会有版本不匹配问题）0.10 把 `Rng`/`CryptoRng` 都设计成基于 `TryRng`/`TryCryptoRng`
/// 的 blanket impl（`Error = Infallible` 时自动获得），所以只需要实现 `TryRng`——
/// 直接用项目已经在依赖的 `getrandom` crate 取系统随机源（跟 `ssh/xfer/mod.rs::rand_hex`
/// 同一个随机源）。系统随机源在正常运行的操作系统上不会失败，一旦失败说明环境本身已经
/// 严重异常，这里选择 panic 而不是静默退化为弱随机——密钥材料的随机性不能打折扣。
struct SysRandom;

impl russh::keys::ssh_key::rand_core::TryRng for SysRandom {
    type Error = std::convert::Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut buf = [0_u8; 4];
        getrandom::getrandom(&mut buf).expect("系统随机数源不可用");
        Ok(u32::from_le_bytes(buf))
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut buf = [0_u8; 8];
        getrandom::getrandom(&mut buf).expect("系统随机数源不可用");
        Ok(u64::from_le_bytes(buf))
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        getrandom::getrandom(dst).expect("系统随机数源不可用");
        Ok(())
    }
}
impl russh::keys::ssh_key::rand_core::TryCryptoRng for SysRandom {}

/// 生成跨会话拷贝-直连尝试用的一次性 ed25519 密钥对：返回（OpenSSH 格式私钥字节，待追加
/// 进目标主机 authorized_keys 的公钥行）。公钥行带 `restrict`（OpenSSH 7.2+ 组合开关：
/// 禁端口/agent/X11 转发 + 禁 PTY/login shell），把这把临时密钥的能力收紧到只能用于文件
/// 传输；`marker` 写进公钥注释，供撤销时精确匹配、也方便万一撤销失败后人工识别清理。
fn generate_temp_keypair(marker: &str) -> Result<(Vec<u8>, String), String> {
    use russh::keys::ssh_key;
    let mut rng = SysRandom;
    let mut key = ssh_key::PrivateKey::random(&mut rng, ssh_key::Algorithm::Ed25519).map_err(|e| e.to_string())?;
    key.set_comment(marker.to_string());
    let priv_pem = key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(|e| e.to_string())?
        .as_bytes()
        .to_vec();
    let pub_line = key.public_key().to_openssh().map_err(|e| e.to_string())?;
    Ok((priv_pem, format!("restrict {pub_line}")))
}

/// AI 调 `open_session` 时，若这条已保存连接本次运行期间还没被用户批准过，需要弹窗让用户
/// 确认后才真正建立连接——保留发起时刻的连接快照，避免确认期间用户又改了同名连接。
pub(super) struct PendingOpenConsent {
    pub(super) conn: SavedConnection,
    resp_tx: Option<oneshot::Sender<McpResponse>>,
    req_id: u64,
    deadline: Instant,
}

/// AI 想对**用户自己打开的**会话做写入类操作（往 shell 里打字、改远端文件），而这个会话
/// 本次运行期间还没被授权过——弹窗让用户当面确认。
///
/// 这里扣住的是**整个** `McpCall`（连同 `upload_source`/`download_sink` 两条字节流通道），
/// 用户点「允许」后原样丢回 `handle_mcp_call` 重跑一遍即可，不需要把请求拆开重建：那样既
/// 要为每个变体写一遍恢复逻辑，也很容易在新增变体时漏掉。重跑时 uid 已进批准集合，不会
/// 再次落到这个分支，所以不会递归。
pub(super) struct PendingUseConsent {
    call: McpCall,
    /// 待授权的会话 uid（`write_target_uids` 里第一个未获批的）。
    pub(super) uid: u64,
    /// 会话标签名，给弹窗显示用（发起时刻的快照）。
    pub(super) title: String,
    /// 给用户看的一句话：AI 具体想干什么。
    pub(super) action: String,
    deadline: Instant,
}

/// 一个尚未绑定的 AI 客户端请求接管本 iShell 窗口——弹窗让用户当面选。
///
/// 只在用户同时开着多个 iShell 时才会出现：代理发现多个实例后，会向**每一个**都发一条
/// `Bind`，于是每个窗口上都弹出这个框，用户在想用的那个窗口上点「允许」即可。选择因此
/// 发生在用户的眼睛和鼠标上，而不是配置文件里——实例标识是纯内部的，不该让用户去认。
///
/// 落选的那些窗口不需要用户逐个点「拒绝」：代理拿到第一个「允许」后就挂断其余连接，
/// `resp_tx.is_closed()` 随即为真，弹窗自动消失（见 `sweep_pending_consents`）。
pub(super) struct PendingBindConsent {
    resp_tx: oneshot::Sender<McpResponse>,
    req_id: u64,
    deadline: Instant,
}

/// 给用户看的一句话：AI 具体想拿这个会话干什么。
///
/// 授权弹窗必须靠它做到知情同意——只说一句「AI 想操作这个会话」，用户没有任何依据判断该不该
/// 点允许；把真实的命令/路径摆出来，才谈得上"当面确认"。
fn action_summary(kind: &McpReqKind) -> String {
    /// 命令和文件内容可能很长、还可能带换行，弹窗里要能一眼看完：压成单行再按**字符**截断
    /// （不能按字节切，中文会切出半个字导致 panic）。
    fn brief(s: &str, max: usize) -> String {
        let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
        match one.char_indices().nth(max) {
            None => one,
            Some((i, _)) => format!("{}…", &one[..i]),
        }
    }
    let zh = matches!(crate::i18n::current(), crate::i18n::Lang::Zh);
    match kind {
        McpReqKind::RunCommand { command, .. } => {
            let c = brief(command, 80);
            if zh {
                format!("执行命令：{c}")
            } else {
                format!("run the command: {c}")
            }
        }
        McpReqKind::SendInput { text, .. } => {
            let t = brief(text, 40);
            if zh {
                format!("直接发送按键/文本：{t}")
            } else {
                format!("send raw keystrokes/text: {t}")
            }
        }
        McpReqKind::Interrupt { .. } => {
            if zh {
                "发送 Ctrl+C 中断当前前台程序".into()
            } else {
                "send Ctrl+C to interrupt the foreground program".into()
            }
        }
        McpReqKind::WriteFile { path, .. } => {
            if zh {
                format!("覆盖写入远端文件：{path}")
            } else {
                format!("overwrite the remote file: {path}")
            }
        }
        McpReqKind::CopyToRemote { remote_path, .. }
        | McpReqKind::CopyToRemoteFromCaller { remote_path, .. } => {
            if zh {
                format!("复制文件到远端：{remote_path}")
            } else {
                format!("copy a file to: {remote_path}")
            }
        }
        McpReqKind::CopyBetweenSessions {
            src_remote_path,
            dest_remote_path,
            ..
        } => {
            if zh {
                format!("跨主机复制文件：{src_remote_path} → {dest_remote_path}")
            } else {
                format!("copy a file across hosts: {src_remote_path} → {dest_remote_path}")
            }
        }
        // 只读类操作的 `write_target_uids` 返回空，根本走不到授权弹窗。这里给兜底文案而不是
        // `unreachable!()`：万一将来改动漏了判定，弹一句"未知操作"让用户去拒绝，比让整个
        // 应用 panic 掉安全得多。
        _ => {
            if zh {
                "未知操作".into()
            } else {
                "an unrecognised operation".into()
            }
        }
    }
}

/// 启动 socket 监听（若用户未在设置里开启 AI 控制，返回一个永远收不到数据的空通道）。
/// 整套本地 IPC 建立在 Unix domain socket 上，tokio 的 UnixListener/UnixStream 只在 unix
/// 平台提供——Windows 上这个特性眼下确实不支持，下面 `#[cfg(not(unix))]` 版本直接返回一个
/// 永远收不到数据的空通道（等价于"用户没开启"的路径），其余 App 代码按同一通道消费事件，
/// 不需要为平台差异专门分叉。
#[cfg(unix)]
pub(super) fn spawn_mcp_listener(
    runtime: &Arc<tokio::runtime::Runtime>,
    ctx: egui::Context,
) -> mpsc::UnboundedReceiver<McpCall> {
    let (tx, rx) = mpsc::unbounded_channel::<McpCall>();
    if !crate::store::load_mcp_consent() {
        return rx;
    }
    let Some(sock_path) = crate::store::mcp_socket_path() else {
        return rx;
    };
    runtime.spawn(async move {
        // sock_path 现在是带 pid 的、本进程独占的路径（见 mcp_socket_path 的注释）——不会再
        // 有别的实例跟本进程抢同一个路径，remove_file 这里只是兜底极小概率的“同 pid 复用、
        // 上次异常退出没清理”这种情况，不是主要的安全网了。
        let _ = std::fs::remove_file(&sock_path);
        // 顺手清理本机其它已经死掉的旧实例留下的 socket 文件（不同 pid，早就没人监听了），
        // 避免 ~/.config/ishell/ 下 mcp-<pid>.sock 越攒越多；探测失败或没有权限就跳过，
        // 不影响本进程自己绑定新路径。
        if let Some(dir) = sock_path.parent() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    let is_other_mcp_sock = p != sock_path
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("mcp-") && n.ends_with(".sock"));
                    if is_other_mcp_sock && UnixStream::connect(&p).await.is_err() {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
        let listener = match UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                log::warn!("MCP socket 监听失败：{e}");
                return;
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
        }
        // 这个 socket 会被反向转发到远端主机（见 src/ssh/mod.rs），意味着任何能连到那台
        // 远端主机的人都摸得到它——不能像纯本地场景那样假设连接数天然很少。用信号量限制
        // 并发连接数，防止大量"连上但不发完整请求"的连接把任务/fd 堆起来。
        let conn_limit = Arc::new(tokio::sync::Semaphore::new(MAX_MCP_CONNECTIONS));
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = Arc::clone(&conn_limit).try_acquire_owned() else {
                // 并发已达上限：直接丢弃这个连接（不回应也不占用任务），
                // 对端会看到连接被关闭，比无限堆积任务安全。
                continue;
            };
            tokio::spawn(handle_conn(stream, tx.clone(), ctx.clone(), permit));
        }
    });
    rx
}

#[cfg(not(unix))]
pub(super) fn spawn_mcp_listener(
    _runtime: &Arc<tokio::runtime::Runtime>,
    _ctx: egui::Context,
) -> mpsc::UnboundedReceiver<McpCall> {
    let (_tx, rx) = mpsc::unbounded_channel::<McpCall>();
    rx
}

/// 并发连接数上限（见 spawn_mcp_listener 里的信号量）。
#[cfg(unix)]
const MAX_MCP_CONNECTIONS: usize = 32;
/// 首行请求的最大字节数：write_file 的 content 可能是个不小的源码/日志文件，读大文件场景
/// 也不该被卡得太死，但也不能真的无界——给一个远大于任何正常请求、又能兜住"恶意/异常连接
/// 持续灌数据不换行"这种情况的上限。
#[cfg(unix)]
const MAX_MCP_LINE_BYTES: u64 = 256 * 1024 * 1024;
/// 首行读取超时：连上但迟迟不发完整一行的连接（占位攻击/半开连接）不能无限占着任务和 fd。
#[cfg(unix)]
const FIRST_LINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 等 worker 给出下载传输判定（trailer）的上限。字节流都已经发完才轮到它，正常是立等可取；
/// 这个超时只防「worker 卡死导致连接和信号量名额被永久占住」，不该在正常使用中被触发。
#[cfg(unix)]
const VERDICT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 往连接上写一行 JSON 响应。失败一律忽略：对端已经走了的话，这里没有任何补救可做。
#[cfg(unix)]
async fn reply(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    id: u64,
    result: Result<McpReqResult, String>,
) {
    let resp = McpResponse { id, result };
    if let Ok(mut json) = serde_json::to_string(&resp) {
        json.push('\n');
        let _ = w.write_all(json.as_bytes()).await;
    }
}

/// 一条连接只处理一问一答：读一行 JSON 请求，转发进 mpsc，等 App 帧循环回填后写一行 JSON 响应。
/// `_permit` 只用来在这个连接存活期间占着信号量里的一个名额，函数退出时自动释放。
#[cfg(unix)]
async fn handle_conn(
    stream: UnixStream,
    tx: mpsc::UnboundedSender<McpCall>,
    ctx: egui::Context,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r.take(MAX_MCP_LINE_BYTES)).lines();
    let Ok(Ok(Some(line))) = tokio::time::timeout(FIRST_LINE_TIMEOUT, lines.next_line()).await
    else {
        return; // 对端没写完就断了连接/超时/超长，没东西可读也没法回应
    };
    let req: McpRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            // JSON 解不出来时，之前是直接静默关闭连接——对端只会看到一个空响应/EOF，
            // 完全不知道发生了什么。这里至少回一条结构化错误（id 用 0，因为请求本身
            // 都没解出来，不知道真实 id 是什么）。
            let resp = McpResponse {
                id: 0,
                result: Err(format!("请求 JSON 解析失败：{e}")),
            };
            if let Ok(mut json) = serde_json::to_string(&resp) {
                json.push('\n');
                let _ = w.write_all(json.as_bytes()).await;
            }
            return;
        }
    };
    let id = req.id;
    // 实例校验：先于任何会话逻辑，也先于把请求递进 App 帧循环。这里是「一个代理只操作一个
    // iShell」的执行点——校验放在被叫的一方，代理侧无论探测出什么错路径都越不过来。
    let own = crate::store::mcp_instance_id();
    if !req.is_addressed_to(own) {
        reply(
            &mut w,
            id,
            Err("这条请求点名的是另一个 iShell 实例（或没有点名）。请重新发起 MCP 连接：\
                 你绑定的那个 iShell 可能已经退出了，代理不会自动改绑到别的实例上——\
                 多开时静默换一个实例执行，命令就会落到你没预期的机器上"
                .to_string()),
        )
        .await;
        return;
    }
    // Identify 在连接层就地回答：它只是「你是谁」，不碰任何会话状态，没必要绕一趟 App
    // 帧循环。代理发现多个实例时会向每一个都问一次，让这条路尽量轻。
    if matches!(req.kind, McpReqKind::Identify) {
        reply(&mut w, id, Ok(McpReqResult::Instance { id: own.to_string() })).await;
        return;
    }
    let is_caller_upload = matches!(&req.kind, McpReqKind::CopyToRemoteFromCaller { .. });
    if is_caller_upload {
        // `Lines` 持有的 BufReader 可能已经预读了紧随 JSON 行的文件字节，不能丢掉它；
        // 连同缓冲区一起转给 worker，才能保证二进制流不丢首块。
        let upload_source = Some(Box::new(lines.into_inner()) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);
        let (resp_tx, resp_rx) = oneshot::channel();
        if tx.send(McpCall { req, resp_tx, upload_source, download_sink: None }).is_err() {
            return;
        }
        ctx.request_repaint();
        let resp = resp_rx.await.unwrap_or(McpResponse {
            id,
            result: Err("iShell 未能处理该请求（可能已关闭）".into()),
        });
        if let Ok(mut json) = serde_json::to_string(&resp) {
            json.push('\n');
            let _ = w.write_all(json.as_bytes()).await;
        }
        return;
    }
    // `CopyFromRemoteToCaller`：对称方向，GUI 把字节流回代理进程本地落盘。响应形状和其他
    // 请求不一样（成功时先写一行 header JSON 再紧跟原始字节，没有第二行 JSON），所以单独
    // 处理，不复用下面的通用一问一答路径。
    let is_caller_download = matches!(&req.kind, McpReqKind::CopyFromRemoteToCaller { .. });
    if is_caller_download {
        let McpReqKind::CopyFromRemoteToCaller { ref remote_path, .. } = req.kind else {
            unreachable!()
        };
        let stream_path = remote_path.clone();
        let (resp_tx, resp_rx) = oneshot::channel();
        let (dl_tx, dl_rx) = oneshot::channel::<Result<crate::proto::DownloadStreamSource, String>>();
        if tx
            .send(McpCall { req, resp_tx, upload_source: None, download_sink: Some(dl_tx) })
            .is_err()
        {
            return;
        }
        ctx.request_repaint();
        // dl_rx 是这个操作的权威结果通道（worker 探测远端路径后精确回 Ok(流)/Err(消息)）；
        // 它被 drop 且未发送，只会发生在 App 层校验就直接失败（会话不存在/路径非法等，
        // 请求根本没送到 worker）——这种情况下真正的错误信息在 resp_rx 里，回落读它。
        let outcome: Result<crate::proto::DownloadStreamSource, String> = match dl_rx.await {
            Ok(o) => o,
            Err(_) => match resp_rx.await {
                Ok(McpResponse { result: Err(msg), .. }) => Err(msg),
                Ok(McpResponse { result: Ok(_), .. }) => Err("iShell 返回了意料之外的响应".into()),
                Err(_) => Err("iShell 未能处理该请求（可能已关闭）".into()),
            },
        };
        match outcome {
            Ok(source) => {
                let header = McpResponse {
                    id,
                    result: Ok(McpReqResult::CopyStreamHeader { path: stream_path, size: source.size }),
                };
                if let Ok(mut json) = serde_json::to_string(&header) {
                    json.push('\n');
                    if w.write_all(json.as_bytes()).await.is_err() {
                        return;
                    }
                }
                // 分块发（线格式与理由见 mcp_protocol::write_framed_stream）：判定必须能被
                // 对端找到，而裸字节流没有边界——中途失败少发一个字节，那行判定就会被当成
                // 文件内容吞掉。收尾的 `0\n` 只表示字节到此为止，成不成功由后面那行判定说。
                let mut reader = source.reader;
                if crate::mcp_protocol::write_framed_stream(&mut reader, &mut w)
                    .await
                    .is_err()
                {
                    return; // 对端走了，或管道坏了——后者对端会因为收不到 `0\n` 而报错，不会误换入
                }
                // 给判定一个上限：worker 在 shutdown() 之后、送出判定之前卡死的话，这里会
                // 无限等下去，连同这条连接持有的信号量名额一起——名额被一点点吃光之后，新的
                // MCP 连接就再也排不进来了。字节流都发完了才轮到这一步，正常情况下判定是
                // 立等可取的，撞上这个超时基本可以断定 worker 出了问题。
                let verdict = match tokio::time::timeout(VERDICT_TIMEOUT, source.outcome).await {
                    Ok(Ok(Ok(()))) => Ok(McpReqResult::Ok),
                    Ok(Ok(Err(msg))) => Err(msg),
                    // worker 没吭声就没了（panic/被 drop）：只能按失败算。宁可让调用方重试，
                    // 也不能让它把一个来路不明的字节流换入原文件。
                    Ok(Err(_)) => Err("iShell 未能给出这次传输的最终判定（worker 已退出）".to_string()),
                    Err(_) => Err("iShell 未能在字节流发完后及时给出传输判定（worker 可能已卡死）".to_string()),
                };
                reply(&mut w, id, verdict).await;
            }
            Err(msg) => {
                let resp = McpResponse { id, result: Err(msg) };
                if let Ok(mut json) = serde_json::to_string(&resp) {
                    json.push('\n');
                    let _ = w.write_all(json.as_bytes()).await;
                }
            }
        }
        return;
    }
    let (resp_tx, mut resp_rx) = oneshot::channel();
    if tx.send(McpCall { req, resp_tx, upload_source: None, download_sink: None }).is_err() {
        return;
    }
    ctx.request_repaint(); // 唤醒 UI 线程尽快排空这条请求
    // 对端（ishell-mcp）自己可能有更短的超时（比如 MCP 客户端的空闲中止），在我们等到
    // App 处理完之前就提前断开连接——这种情况下这一行不再读到任何东西，`next_line()`
    // 会返回 Ok(None)（EOF）。如果只是死等 resp_rx，那么即使对端早就走了，
    // 这个 resp_tx 依旧会一直挂在 PendingAiRun 上，把后续 poll_run 卡死在
    // "已有一个 poll_run 在等待"——release 掉 resp_rx（丢弃它，触发发送端的
    // is_closed()），让 App 那边能识别出这个等待者其实已经没人要结果了。
    let resp = tokio::select! {
        biased;
        r = &mut resp_rx => r.unwrap_or(McpResponse {
            id,
            result: Err("iShell 未能处理该请求（可能已关闭）".into()),
        }),
        _ = lines.next_line() => {
            // 丢弃 resp_rx（return 即可）会让 App 那侧的 resp_tx.is_closed() 变真，但**没人会
            // 去看**：egui 按需重绘，此刻这个窗口没有任何输入事件，帧循环根本不转，清扫逻辑
            // 就永远不执行。绑定弹窗正是靠这条挂断感知自动消失的（见 sweep_pending_consents），
            // 少了这一下，落选窗口的框会一直杵到用户手动关掉——实测如此。
            ctx.request_repaint();
            return;
        }
    };
    if let Ok(mut json) = serde_json::to_string(&resp) {
        json.push('\n');
        let _ = w.write_all(json.as_bytes()).await;
    }
}

impl App {
    /// 每帧排空 MCP 请求 + 检查各会话待完成的 AI 命令运行（超时/完成）。
    pub(super) fn drain_mcp_calls(&mut self) {
        while let Ok(call) = self.mcp_rx.try_recv() {
            self.handle_mcp_call(call);
        }
        for s in &mut self.sessions {
            let Some(pending) = s.pending_ai_run.as_mut() else {
                continue;
            };
            // 哨兵命中后先存进 finished_result，不直接在这里判断有没有 waiter——
            // 上一轮超时已经把 resp_tx 发出去了的话，这里刚好没人等，绝不能就此丢掉结果。
            if pending.finished_result.is_none() {
                if let Some((code, output)) = s.terminal.take_ai_done() {
                    let output = trim_leading_echo(&output, &pending.command);
                    let output = cap_output_for_ai(output);
                    pending.finished_result = Some((code, output));
                }
            }
            if let Some((code, output)) = pending.finished_result.take() {
                if let Some(tx) = pending.resp_tx.take() {
                    let resp = McpResponse {
                        id: pending.req_id,
                        result: Ok(McpReqResult::Run(McpRunResult {
                            run_id: pending.run_id,
                            finished: true,
                            output,
                            exit_code: Some(code),
                        })),
                    };
                    let _ = tx.send(resp);
                    s.pending_ai_run = None;
                } else {
                    // 没有 waiter：把结果放回去缓存着，等下一次 poll_run 打进来再取走。
                    pending.finished_result = Some((code, output));
                }
            } else if Instant::now() >= pending.deadline {
                let output = s.terminal.peek_ai_output().unwrap_or_default();
                let output = trim_leading_echo(&output, &pending.command);
                let output = cap_output_for_ai(output);
                let resp = McpResponse {
                    id: pending.req_id,
                    result: Ok(McpReqResult::Run(McpRunResult {
                        run_id: pending.run_id,
                        finished: false,
                        output,
                        exit_code: None,
                    })),
                };
                if let Some(tx) = pending.resp_tx.take() {
                    let _ = tx.send(resp);
                }
                // 未完成：保留 pending_ai_run，等下一次 poll_run 续等（不重发命令）
            }
        }
        if let Some(pending) = self.pending_open_consent.as_mut() {
            if Instant::now() >= pending.deadline {
                if let Some(tx) = pending.resp_tx.take() {
                    let _ = tx.send(McpResponse {
                        id: pending.req_id,
                        result: Err(
                            "等待用户确认超时（5 分钟），已自动拒绝：请让用户切回 iShell 点击\
                             确认弹窗，或直接重新调用 open_session 再次发起确认请求".into(),
                        ),
                    });
                }
                self.pending_open_consent = None;
            }
        }
        if let Some(pending) = self.pending_use_consent.as_ref() {
            if Instant::now() >= pending.deadline {
                // 整个 McpCall 被扣在这里，超时必须把它取出来回一条错，否则调用方那边就是
                // 一条永远不回的请求（只能干等到它自己的 timeout_ms）。
                let pending = self.pending_use_consent.take().expect("上一行刚确认是 Some");
                let id = pending.call.req.id;
                let _ = pending.call.resp_tx.send(McpResponse {
                    id,
                    result: Err(
                        "等待用户确认超时（5 分钟），已自动拒绝：这是用户自己打开的会话，需要\
                         用户当面授权。请改用 open_session 开一个 AI 专用会话，或让用户切回\
                         iShell 点击确认弹窗后重试".into(),
                    ),
                });
            }
        }
        // 绑定弹窗的清扫比另外两个多一条「对端走了」：代理把 Bind 同时发给了每一个实例，
        // 用户在某个窗口点「允许」之后，代理立刻挂断其余连接。落选的窗口据此静默收起弹窗，
        // 用户不必挨个去点「拒绝」——他已经用点击表达过选择了，再逼他关掉 N-1 个框是骚扰。
        if let Some(pending) = self.pending_bind_consent.as_ref() {
            if pending.resp_tx.is_closed() {
                self.pending_bind_consent = None;
            } else if Instant::now() >= pending.deadline {
                let pending = self.pending_bind_consent.take().expect("上一行刚确认是 Some");
                let _ = pending.resp_tx.send(McpResponse {
                    id: pending.req_id,
                    result: Err(
                        "等待用户选择 iShell 窗口超时（5 分钟）：用户同时开着多个 iShell，\
                         需要他在想让你操作的那个窗口上点「允许」。请让用户切回 iShell 处理\
                         后重试".into(),
                    ),
                });
            }
        }
    }

    /// 给「最近的那个超时」排一次定时重绘。每帧末尾调用一次（见 frame.rs）。
    ///
    /// 这里所有的超时判定——AI 命令运行、文件读写、跨会话拷贝、三个确认框——全都是**每帧
    /// 轮询**的，而 egui 按需重绘：一个空闲窗口（终端没输出、用户也没碰鼠标）根本不转帧
    /// 循环，于是这些 deadline 到点都不会被求值。表现出来就是调用方那边一条请求挂到自己的
    /// timeout_ms 之后仍然不返回，直到有别的什么东西碰巧叫醒了 UI 线程。
    ///
    /// 最典型的例子：对一个空闲会话跑 `run_command("sleep 300", timeout_ms=5000)`——命令
    /// 回显那一波事件重绘了一帧，之后 300 秒里终端零输出，5 秒的超时到点没有任何东西叫醒
    /// UI 线程，AI 就一直挂着。
    ///
    /// 之所以在这里统一扫一遍、而不是给每处超时各打一个补丁：新加的超时只要挂在这些字段
    /// 上就自动被覆盖，不会重蹈「只想起了自己刚写的那个」的覆辙。每帧重排是幂等的，pending
    /// 的增删也能自动跟上。
    pub(super) fn arm_timeout_repaint(&self) {
        let consents = [
            self.pending_open_consent.as_ref().map(|p| p.deadline),
            self.pending_use_consent.as_ref().map(|p| p.deadline),
            self.pending_bind_consent.as_ref().map(|p| p.deadline),
        ];
        let sessions = self.sessions.iter().flat_map(|s| {
            s.pending_ai_run
                .as_ref()
                .map(|p| p.deadline)
                .into_iter()
                .chain(s.pending_file_ops.iter().map(|op| op.deadline))
        });
        let jobs = self
            .cross_copy_jobs
            .iter()
            .flat_map(|j| [j.deadline, j.phase_deadline]);
        let next = consents.into_iter().flatten().chain(sessions).chain(jobs).min();
        if let Some(deadline) = next {
            self.ctx
                .request_repaint_after(deadline.saturating_duration_since(Instant::now()));
        }
    }

    /// 检查各会话挂起的文件读写（write_file/read_file）是否超时。必须在每帧
    /// `for s in &mut self.sessions { s.drain_events(); ... }`（消化 worker 事件、
    /// 真正 resolve `pending_file_op` 的地方）**之后**调用——如果在那之前调用，
    /// 会跟"本帧里事件恰好也到达"打时序竞争：明明操作已经真正完成，却先被这里判定超时、
    /// 清空 pending_file_op，随后姗姗来迟的 drain_events 才处理那个事件，此时已经找不到
    /// 挂起记录去 resolve 了，AI 会收到一个错误的"超时"而不是正确的结果。
    pub(super) fn check_file_op_timeouts(&mut self) {
        // 允许每会话最多 MAX_CONCURRENT_FILE_OPS(16) 个并发操作，一次可能整批超时——墓碑
        // 缓冲给到 4 波的量，确保迟到事件在被挤出缓冲前几乎总能被识别为"已超时判定过"。
        const MAX_TOMBSTONES: usize = 64;
        let now = Instant::now();
        for s in &mut self.sessions {
            // 就地移除本会话里所有已超时的挂起文件操作（可能有多个并发在跑）；remove 会左移
            // 后续元素，故命中时不推进下标。
            let mut i = 0;
            while i < s.pending_file_ops.len() {
                if now < s.pending_file_ops[i].deadline {
                    i += 1;
                    continue;
                }
                let mut op = s.pending_file_ops.remove(i);
                if let Some(tx) = op.resp_tx.take() {
                    let _ = tx.send(McpResponse {
                        id: op.req_id,
                        result: Err("文件操作超时（worker 未在超时前返回结果）".into()),
                    });
                }
                // worker 侧的 SFTP 操作没法取消，超时不代表它已经停止——记一个"墓碑"，
                // 这样迟到的真实完成事件到达时能被认出来直接丢弃，不会因为这条已经从
                // pending_file_ops 移除就被误路由进普通编辑器 UI。
                let op_id = match op.kind {
                    FileOpKind::Write { op_id }
                    | FileOpKind::Read { op_id }
                    | FileOpKind::Copy { op_id } => op_id,
                };
                s.file_op_tombstones.push_back(op_id);
                if s.file_op_tombstones.len() > MAX_TOMBSTONES {
                    s.file_op_tombstones.pop_front();
                }
            }
        }
    }

    /// 推进 `copy_between_sessions` 作业：必须在每帧 `for s in &mut self.sessions
    /// { s.drain_events(); .. }` 之后调用（原因同 `check_file_op_timeouts`）——
    /// 所有传入的事件都是那个循环里从各会话 `pending` 里收集来的。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn advance_cross_copy_jobs(
        &mut self,
        temp_key_trusted: Vec<(u64, bool, String)>,
        temp_key_untrusted: Vec<u64>,
        direct_relay_started: Vec<u64>,
        direct_relay_done: Vec<(u64, bool, String)>,
        relay_source: Vec<(u64, Result<u64, String>)>,
        copy_done: Vec<(u64, u64, bool, String)>,
    ) {
        for (op_id, ok, message) in temp_key_trusted {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            if !matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::TrustingB) {
                continue;
            }
            if !ok {
                // 建立临时信任本身就失败：不需要撤销（从没成功过），直接转中转。
                log::debug!("copy_between_sessions 建立临时信任失败：{message}");
                self.start_relay_fallback(idx);
                continue;
            }
            // 从这一刻起，目标主机 authorized_keys 里真的多了一行临时公钥——之后任何
            // 提前退出的路径都必须补发一次撤销，不能再直接调用 start_relay_fallback。
            self.cross_copy_jobs[idx].trust_established = true;
            self.start_direct_attempt(idx);
        }
        for op_id in direct_relay_started {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            if let CrossCopyPhase::DirectCopying { started } = &mut self.cross_copy_jobs[idx].phase {
                *started = true;
            }
        }
        for (op_id, ok, message) in direct_relay_done {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            if !matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::DirectCopying { .. }) {
                continue; // 迟到事件（比如已经因超时转过一次中转）：直接忽略
            }
            self.finish_direct_attempt(idx, if ok { Ok(()) } else { Err(message) });
        }
        for op_id in temp_key_untrusted {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            if matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::UntrustingAfterDirect) {
                self.finish_after_untrust(idx);
            }
        }
        for (op_id, result) in relay_source {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            if !matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::RelayReading) {
                continue;
            }
            match result {
                Ok(size) => {
                    let dest_uid = self.cross_copy_jobs[idx].dest_uid;
                    let dest_path = self.cross_copy_jobs[idx].dest_remote_path.clone();
                    let Some(pipe_reader) = self.cross_copy_jobs[idx].pipe_reader.take() else {
                        self.fail_cross_copy_job(idx, "内部错误：中转管道读端已丢失".into());
                        continue;
                    };
                    let Some(dest_idx) = self.session_idx_by_uid(dest_uid) else {
                        self.fail_cross_copy_job(idx, "目标会话已不存在".into());
                        continue;
                    };
                    if !self.sessions[dest_idx].connected {
                        self.fail_cross_copy_job(idx, "目标会话已断线".into());
                        continue;
                    }
                    let sent = self.sessions[dest_idx]
                        .cmd_tx
                        .send(UiCommand::RelayWriteFile { id: op_id, remote_path: dest_path, size, reader: pipe_reader })
                        .is_ok();
                    if !sent {
                        self.fail_cross_copy_job(idx, "目标会话的后台连接似乎已经断开".into());
                        continue;
                    }
                    // 目标会话的 pending_file_op 从一开始（CopyBetweenSessions 请求刚到达时）
                    // 就已经占位，这里不需要重新设置。
                    self.cross_copy_jobs[idx].phase = CrossCopyPhase::RelayWriting;
                }
                Err(msg) => self.fail_cross_copy_job(idx, msg),
            }
        }
        for (uid, op_id, ok, message) in copy_done {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            let job = &self.cross_copy_jobs[idx];
            // 只有目标会话（写侧）的完成事件才代表整个跨会话拷贝结束；源会话自己的
            // TransferDone 只影响它自己的 `pending_file_op`/Transfers 记账（已经由
            // `try_resolve_file_copy`/`self.transfers` 独立处理），跟这里无关——即使源侧
            // 中途失败，目标侧的管道读取也会随之出错并同样报 `ok:false`，由那次事件收尾。
            if uid != job.dest_uid || !matches!(job.phase, CrossCopyPhase::RelayWriting) {
                continue;
            }
            if ok {
                self.resolve_cross_copy_job(idx, Ok(()), "relay");
            } else {
                self.resolve_cross_copy_job_err(idx, message);
            }
        }
        // 阶段级短超时：直连尝试迟迟没开始传输数据 → 主动放弃直连转中转；
        // 撤销临时信任迟迟没回执 → 不再等，直接按已确定的直连结果收尾。
        // 倒序处理，避免 Vec::remove 导致的下标错位（resolve/fail 会整条移除 job）。
        let now = Instant::now();
        for idx in (0..self.cross_copy_jobs.len()).rev() {
            let job = &self.cross_copy_jobs[idx];
            if now >= job.deadline {
                // 总超时必须按阶段处理，不能无条件直接判失败：
                // - 已经在 UntrustingAfterDirect：direct_result 早就确定了（可能是成功！），
                //   这里只是撤销回执迟迟不来——直接按已知结果收尾，而不是把"其实已经
                //   成功"误报成超时失败。
                // - 其余阶段但 trust_established 为真：目标主机上确实还留着一把临时公钥，
                //   必须补发一次撤销（不等待结果，job 马上就要整体移除），否则会永久残留。
                if matches!(job.phase, CrossCopyPhase::UntrustingAfterDirect) {
                    self.finish_after_untrust(idx);
                } else {
                    if job.trust_established {
                        let marker = job.marker.clone();
                        let op_id = job.op_id;
                        let dest_uid = job.dest_uid;
                        if let Some(dest_idx) = self.session_idx_by_uid(dest_uid) {
                            let _ = self.sessions[dest_idx]
                                .cmd_tx
                                .send(UiCommand::UntrustTempKey { op_id, marker });
                        }
                    }
                    self.fail_cross_copy_job(idx, "跨会话拷贝超时（源或目标 worker 未在超时前返回结果）".into());
                }
                continue;
            }
            match job.phase {
                CrossCopyPhase::DirectCopying { started: false } if now >= job.phase_deadline => {
                    job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.finish_direct_attempt(idx, Err("直连尝试超时（20s 内未建立连接），已转中转".into()));
                }
                CrossCopyPhase::UntrustingAfterDirect if now >= job.phase_deadline => {
                    self.finish_after_untrust(idx);
                }
                _ => {}
            }
        }
    }

    /// `TrustingB` 成功后：向源会话下发直连尝试。这个函数被调用时 `trust_established`
    /// 已经是 true（目标主机上真的多了一行临时公钥），所以下面每一条提前退出都必须走
    /// `untrust_then_relay_fallback`（补发撤销）而不是 `start_relay_fallback`（那个函数
    /// 的语义是"信任从没建立过，不需要撤销"，用在这里会导致公钥永久残留）。
    fn start_direct_attempt(&mut self, idx: usize) {
        let job = &self.cross_copy_jobs[idx];
        let op_id = job.op_id;
        let src_uid = job.src_uid;
        let dest_uid = job.dest_uid;
        let src_path = job.src_remote_path.clone();
        let dest_path = job.dest_remote_path.clone();
        let cancel = job.cancel.clone();
        let Some(src_idx) = self.session_idx_by_uid(src_uid) else {
            self.untrust_then_relay_fallback(idx);
            return;
        };
        let Some(dest_idx) = self.session_idx_by_uid(dest_uid) else {
            self.untrust_then_relay_fallback(idx);
            return;
        };
        if !self.sessions[src_idx].connected {
            self.untrust_then_relay_fallback(idx);
            return;
        }
        let (dest_host, dest_port, dest_user) = {
            let d = &self.sessions[dest_idx];
            (d.cfg.host.clone(), d.cfg.port, d.cfg.username.clone())
        };
        // 私钥字节仅在这一次尝试里短暂持有：CrossCopyJob 本身不保存密钥材料，用完即随
        // UiCommand 一起移动走，不在 App 状态里长期滞留敏感数据。
        let Some(priv_key_pem) = self.cross_copy_jobs[idx].take_priv_key_pem() else {
            self.untrust_then_relay_fallback(idx);
            return;
        };
        let sent = self.sessions[src_idx]
            .cmd_tx
            .send(UiCommand::DirectRelayCopy {
                op_id,
                src_path,
                dest_user,
                dest_host,
                dest_port,
                dest_path,
                priv_key_pem,
                cancel,
            })
            .is_ok();
        if !sent {
            self.untrust_then_relay_fallback(idx);
            return;
        }
        self.cross_copy_jobs[idx].phase = CrossCopyPhase::DirectCopying { started: false };
        self.cross_copy_jobs[idx].phase_deadline = Instant::now() + DIRECT_ATTEMPT_TIMEOUT;
    }

    /// `TrustingB` 已经成功、但源会话侧未能真正发起直连尝试（断线/不存在/发送失败等）：
    /// 目标主机上确实已经写入了临时公钥，必须补发一次撤销（不等待结果——job 马上就要
    /// 转中转继续跑，没必要为清理这一步阻塞主流程），再走中转 fallback。
    fn untrust_then_relay_fallback(&mut self, idx: usize) {
        let job = &self.cross_copy_jobs[idx];
        let marker = job.marker.clone();
        let op_id = job.op_id;
        let dest_uid = job.dest_uid;
        if let Some(dest_idx) = self.session_idx_by_uid(dest_uid) {
            let _ = self.sessions[dest_idx]
                .cmd_tx
                .send(UiCommand::UntrustTempKey { op_id, marker });
        }
        self.start_relay_fallback(idx);
    }

    /// 直连信任建立失败（或没能发出尝试请求）：跳过撤销步骤（从没建立过信任），直接转中转。
    fn start_relay_fallback(&mut self, idx: usize) {
        let Some(pipe_writer) = self.cross_copy_jobs[idx].pipe_writer.take() else {
            self.fail_cross_copy_job(idx, "内部错误：中转管道写端已丢失".into());
            return;
        };
        let job = &self.cross_copy_jobs[idx];
        let op_id = job.op_id;
        let src_uid = job.src_uid;
        let src_path = job.src_remote_path.clone();
        let Some(src_idx) = self.session_idx_by_uid(src_uid) else {
            self.fail_cross_copy_job(idx, "源会话已不存在".into());
            return;
        };
        let sent = self.sessions[src_idx]
            .cmd_tx
            .send(UiCommand::RelayReadFile { id: op_id, remote_path: src_path, writer: pipe_writer })
            .is_ok();
        if !sent {
            self.fail_cross_copy_job(idx, "源会话的后台连接似乎已经断开".into());
            return;
        }
        self.cross_copy_jobs[idx].phase = CrossCopyPhase::RelayReading;
    }

    /// 直连尝试结束（无论真正完成还是被判定超时）：无条件进入撤销临时信任阶段——
    /// 这一步用状态机的固定跳转保证"不管成不成功都会清理"，不依赖某个分支手动调用。
    fn finish_direct_attempt(&mut self, idx: usize, result: Result<(), String>) {
        let job = &mut self.cross_copy_jobs[idx];
        job.direct_result = Some(result);
        job.phase = CrossCopyPhase::UntrustingAfterDirect;
        job.phase_deadline = Instant::now() + UNTRUST_WAIT;
        let marker = job.marker.clone();
        let op_id = job.op_id;
        let dest_uid = job.dest_uid;
        if let Some(dest_idx) = self.session_idx_by_uid(dest_uid) {
            let _ = self.sessions[dest_idx]
                .cmd_tx
                .send(UiCommand::UntrustTempKey { op_id, marker });
        }
        // 发送失败或目标会话已经不存在都不额外处理：UNTRUST_WAIT 到了会自然收尾。
    }

    /// 撤销信任已完成（或等不到回执，超时放弃）：按之前确定的直连结果决定收尾——
    /// 成功则直接 resolve；失败则转入中转（这次真正把管道发给源/目标会话）。
    fn finish_after_untrust(&mut self, idx: usize) {
        match self.cross_copy_jobs[idx].direct_result.take() {
            Some(Ok(())) => self.resolve_cross_copy_job(idx, Ok(()), "direct"),
            Some(Err(_)) => self.start_relay_fallback(idx),
            None => {} // 不应该发生：还没收到 direct_result 就走到了这一步
        }
    }

    fn fail_cross_copy_job(&mut self, idx: usize, message: String) {
        self.resolve_cross_copy_job_err(idx, message);
    }

    fn resolve_cross_copy_job_err(&mut self, idx: usize, message: String) {
        self.resolve_cross_copy_job(idx, Err(message), "");
    }

    fn resolve_cross_copy_job(&mut self, idx: usize, result: Result<(), String>, method: &str) {
        let job = self.cross_copy_jobs.remove(idx);
        // 保险起见清一次两侧为这次 op_id 占位的 pending_file_op：正常路径下 TransferDone
        // 已经通过 try_resolve_file_copy 自然移除了；这里只处理"某一侧因为提前失败/超时而
        // 从没走到那一步"的情况，避免占位项永久占着并发名额。
        for uid in [job.src_uid, job.dest_uid] {
            if let Some(sidx) = self.session_idx_by_uid(uid) {
                self.sessions[sidx]
                    .pending_file_ops
                    .retain(|op| !matches!(op.kind, FileOpKind::Copy { op_id } if op_id == job.op_id));
            }
        }
        if let Some(tx) = job.resp_tx {
            let resp = match result {
                Ok(()) => McpResponse {
                    id: job.req_id,
                    result: Ok(McpReqResult::CopiedBetweenSessions {
                        path: job.dest_remote_path,
                        method: method.to_string(),
                    }),
                },
                Err(msg) => McpResponse { id: job.req_id, result: Err(msg) },
            };
            let _ = tx.send(resp);
        }
    }

    /// 真正建立会话：`open_session` 直接批准，或用户在确认弹窗里点了「允许」之后调用。
    fn do_open_session(&mut self, c: &SavedConnection, id: u64, resp_tx: oneshot::Sender<McpResponse>) {
        let cfg = connect_config_from_saved(c);
        self.spawn_session(cfg);
        let s = self.sessions.last_mut().expect("spawn_session 刚 push 了一个会话");
        s.ai_owned = true; // AI 新开的会话：只读，用户键盘输入不转发（见 layout_body.rs）
        let info = McpSessionInfo {
            uid: s.uid,
            title: s.title.clone(),
            host: s.cfg.host.clone(),
            connected: s.connected,
            cwd: s.terminal.cwd().map(|c| c.to_string()),
            ai_owned: s.ai_owned,
        };
        let _ = resp_tx.send(McpResponse {
            id,
            result: Ok(McpReqResult::Opened(info)),
        });
    }

    /// 用户在绑定弹窗里点了「允许」/「拒绝」——即在这个窗口上决定「让那个 AI 客户端操作我」。
    ///
    /// 注意这里**不需要**在 App 里记任何绑定状态：绑定活在代理进程的内存里（它记住 id，此后
    /// 每条请求都点名），而 iShell 只做「这条请求是不是在叫我」的校验（`is_addressed_to`）。
    /// 把绑定同时记在两侧只会多出一份能和对方失配的状态，却换不来任何额外保证。
    pub(super) fn resolve_bind_consent(&mut self, allow: bool) {
        let Some(pending) = self.pending_bind_consent.take() else {
            return;
        };
        let _ = pending.resp_tx.send(McpResponse {
            id: pending.req_id,
            result: if allow {
                Ok(McpReqResult::Ok)
            } else {
                Err("用户在这个 iShell 窗口上拒绝了绑定请求".into())
            },
        });
    }

    /// 用户在 `open_session` 确认弹窗里点了「允许」/「拒绝」。
    pub(super) fn resolve_open_consent(&mut self, allow: bool) {
        let Some(pending) = self.pending_open_consent.take() else {
            return;
        };
        let PendingOpenConsent {
            conn,
            resp_tx,
            req_id,
            ..
        } = pending;
        let Some(resp_tx) = resp_tx else {
            return;
        };
        if allow {
            self.mcp_open_approved.insert(conn.name.clone());
            self.do_open_session(&conn, req_id, resp_tx);
        } else {
            let _ = resp_tx.send(McpResponse {
                id: req_id,
                result: Err("用户拒绝了这次连接请求".into()),
            });
        }
    }

    /// "会话不存在"的报错要带上当前实际可用的会话列表，不然调用方只知道这个 uid 不对，
    /// 猜不出是自己传错了、还是 iShell 重启后整张会话表都换了（比如 MCP 更新后重启，
    /// 所有 uid 会从 1 重新分配）——报错里直接列出来，不用再额外调一次 list_sessions
    /// 才能定位问题。
    fn session_not_found_msg(&self, uid: u64) -> String {
        if self.sessions.is_empty() {
            return format!("会话不存在（uid={uid}）：当前没有任何打开的会话，可能是 iShell 刚重启");
        }
        let list = self
            .sessions
            .iter()
            .map(|s| format!("{}:{}", s.uid, s.title))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "会话不存在（uid={uid}）：可能是 iShell 已重启（重启后 uid 会重新分配）或这个会话\
             已被关闭。当前可用会话（uid:标题）：{list}"
        )
    }

    /// 写入类操作的会话门禁：目标若是**用户自己打开的**会话且本次运行还没授权过，扣下请求
    /// 弹窗等用户当面确认。放行则原样返回 `call`；扣下（或直接回错）返回 `None`。
    fn gate_user_session_write(&mut self, call: McpCall) -> Option<McpCall> {
        // 目标会话不存在时不在这里拦：让它照常走下去，由各分支回「会话不存在 + 当前可用
        // 会话列表」那条更有用的报错，而不是在这里含混地说一句"需要授权"。
        let Some(uid) = call.req.kind.write_target_uids().into_iter().find(|uid| {
            !self.mcp_use_approved.contains(uid)
                && self
                    .session_idx_by_uid(*uid)
                    .is_some_and(|idx| !self.sessions[idx].ai_owned)
        }) else {
            return Some(call);
        };
        let id = call.req.id;
        // 同一时刻只挂一个确认框：两个叠在一起用户根本分不清在批准哪个。
        if self.pending_use_consent.is_some() || self.pending_open_consent.is_some() {
            let _ = call.resp_tx.send(McpResponse {
                id,
                result: Err("已有一个请求正在等待用户确认，请稍候重试".into()),
            });
            return None;
        }
        let idx = self.session_idx_by_uid(uid).expect("上面刚确认过这个会话存在");
        let title = self.sessions[idx].title.clone();
        let action = action_summary(&call.req.kind);
        self.pending_use_consent = Some(PendingUseConsent {
            call,
            uid,
            title,
            action,
            // 与 open_session 的确认框同样给 5 分钟：用户可能不在电脑前。
            deadline: Instant::now() + Duration::from_secs(300),
        });
        None
    }

    /// 用户在写入授权弹窗里点了「允许」/「拒绝」。
    pub(super) fn resolve_use_consent(&mut self, allow: bool) {
        let Some(pending) = self.pending_use_consent.take() else {
            return;
        };
        if !allow {
            let id = pending.call.req.id;
            let _ = pending.call.resp_tx.send(McpResponse {
                id,
                result: Err(format!(
                    "用户拒绝了对会话 uid={} 的这次操作。这是用户自己打开的会话，AI 不应直接\
                     使用——请改用 open_session 开一个自己的专用会话",
                    pending.uid
                )),
            });
            return;
        }
        // 记住这个会话 uid，后续写入不再打扰用户。uid 由 next_uid 单调分配、只增不复用
        // （session.rs），且这个集合只活在进程内存里，所以不存在"授权被后开的会话捡到"。
        self.mcp_use_approved.insert(pending.uid);
        self.handle_mcp_call(pending.call);
    }

    fn handle_mcp_call(&mut self, call: McpCall) {
        // 先过会话门禁：放行的才继续。重跑时 uid 已在批准集合里，不会再落回这里，不递归。
        let Some(call) = self.gate_user_session_write(call) else {
            return;
        };
        let McpCall { req, resp_tx, upload_source, download_sink } = call;
        let id = req.id;
        let send_err = |resp_tx: oneshot::Sender<McpResponse>, msg: String| {
            let _ = resp_tx.send(McpResponse {
                id,
                result: Err(msg),
            });
        };
        match req.kind {
            // Identify 在连接层就地答完了（见 handle_conn），根本到不了这里。
            McpReqKind::Identify => {
                send_err(resp_tx, "Identify 不应该到达 App 层".into());
            }
            McpReqKind::Bind => {
                // 只跟「另一个绑定请求」互斥。这里**不能**因为恰好挂着一个 open/use 授权框
                // 就把 Bind 顶回去：代理会把那条 Err 当成「这个窗口不是胜出者」，于是用户想
                // 选的那个窗口压根不弹绑定框，他连点的机会都没有；若每个窗口都恰好忙着，
                // 整次绑定还会以「没有任何一个窗口批准」告终，而用户一个框都没见过。
                // 两个框不同时显示是**渲染**层的事，交给 handle_ai_bind_consent 去排队。
                if self.pending_bind_consent.is_some() {
                    send_err(resp_tx, "已有另一个 AI 客户端正在等待用户选择窗口，请稍候重试".into());
                    return;
                }
                self.pending_bind_consent = Some(PendingBindConsent {
                    resp_tx,
                    req_id: id,
                    // 与另外两个确认框同样给 5 分钟：用户可能不在电脑前。
                    deadline: Instant::now() + Duration::from_secs(300),
                });
            }
            McpReqKind::ListSessions => {
                let list = self
                    .sessions
                    .iter()
                    .map(|s| McpSessionInfo {
                        uid: s.uid,
                        title: s.title.clone(),
                        host: s.cfg.host.clone(),
                        connected: s.connected,
                        cwd: s.terminal.cwd().map(|c| c.to_string()),
                        ai_owned: s.ai_owned,
                    })
                    .collect();
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::Sessions(list)),
                });
            }
            McpReqKind::RunCommand {
                session_uid,
                command,
                timeout_ms,
            } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    // 未连上（还在连接/认证中）或已断线时，输入会被 worker 静默丢弃——
                    // 哨兵永远等不到，会话会被 pending_ai_run 占死。直接拒绝，让 AI 明确
                    // 知道要等连上了再试（可用 list_sessions 的 connected 字段确认）。
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_ai_run.is_some() {
                    send_err(
                        resp_tx,
                        "该会话已有一条 AI 命令正在执行，请先 poll_run 或等待完成；如果这条命令已经\
                         不需要了，调用 interrupt 可以立即中断并释放（但会丢失那条命令的结果）"
                            .into(),
                    );
                    return;
                }
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                // 前缀本身只含可打印字符：这段文本会被终端「原样打字回显」，如果直接嵌入原始
                // 0x1E 控制字节，大多数远端终端默认开着 ECHOCTL，回显时会把控制字节渲染成
                // `^^` 这样的两个可打印字符而不是原始字节，导致 expect_echo 的逐字节匹配失配。
                // 真正的分隔符 \x1e 只让 printf 在**执行后的输出**里产生（程序自己写 stdout，
                // 不经过终端的按键回显路径，不受 ECHOCTL 影响）。
                let prefix = format!("AI_DONE_{nonce}:");
                // 命令本身正常回显+输出，用户实时可见；标记行自身回显用 expect_echo 吞掉，
                // 其打印出的哨兵再用 \r\x1b[K 自擦除（不渲染进可见终端），无需改动 feed() 管线。
                let marker = format!("printf '{prefix}%d\\x1e' $?; printf '\\r\\x1b[K'");
                // worker 可能刚退出（Disconnected 事件这一帧还没被处理，s.connected 仍是
                // 上一帧的旧值）——cmd_tx 是 unbounded channel，send 只在接收端已经掉了才会
                // 失败。两条输入分两次 send，不是原子的：命令那条可能已经送达并可能已经
                // 产生副作用，标记那条却失败——这种情况下不能假装"什么都没发生"，必须明确
                // 告诉调用方"命令可能已执行、结果未知"，不能让它误以为可以直接重试。
                let command_sent = s
                    .cmd_tx
                    .send(UiCommand::TerminalInput(format!("{command}\r").into_bytes()))
                    .is_ok();
                if !command_sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，命令未发送，请稍后重试".into());
                    return;
                }
                let marker_sent = s
                    .cmd_tx
                    .send(UiCommand::TerminalInput(format!("{marker}\r").into_bytes()))
                    .is_ok();
                if !marker_sent {
                    send_err(
                        resp_tx,
                        "命令可能已经发送执行，但完成标记发送失败、结果未知——请用 read_screen \
                         或 read_history 核实实际状态，不要盲目重试有副作用的命令"
                            .into(),
                    );
                    return;
                }
                s.terminal.expect_echo(&marker);
                s.terminal.arm_ai_capture(prefix.into_bytes());
                s.pending_ai_run = Some(PendingAiRun {
                    run_id: nonce as u64,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    command,
                    finished_result: None,
                });
            }
            McpReqKind::PollRun {
                session_uid,
                run_id,
                timeout_ms,
            } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                match self.sessions[idx].pending_ai_run.as_mut() {
                    Some(p) if run_id.map_or(true, |r| r == p.run_id) => {
                        // 已经有一个 poll_run/run_command 在等这条运行的结果：不能覆盖它的
                        // resp_tx，否则旧调用的 oneshot 被直接丢弃，只会收到一个模糊的
                        // "iShell 未能处理该请求"，而不是正常的超时/完成语义。拒绝新调用，
                        // 让调用方明确知道"已经有一个等待者了"。
                        // 但如果那个等待者对应的连接已经先断开了（比如 MCP 客户端自己的空闲
                        // 超时提前中止了那次调用，见 handle_conn 里的 EOF 检测），resp_tx 的
                        // 接收端早就被丢弃，is_closed() 为真——这种情况不是"正在等待"，而是
                        // 孤儿等待者，不应该继续挡住新的 poll_run。
                        if p.resp_tx.as_ref().is_some_and(|tx| !tx.is_closed()) {
                            send_err(
                                resp_tx,
                                "这条运行已经有一个 poll_run 在等待，请勿并发调用；如果那个等待者\
                                 已经不需要了（比如它本身也超时卡住），调用 interrupt 可以立即释放"
                                    .into(),
                            );
                            return;
                        }
                        p.deadline = Instant::now() + clamp_timeout(timeout_ms);
                        p.resp_tx = Some(resp_tx);
                        p.req_id = id;
                    }
                    _ => send_err(resp_tx, "run_id 不存在或已结束".into()),
                }
            }
            McpReqKind::ReadScreen { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let text = self.sessions[idx].terminal.screen_text();
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::Screen(text)),
                });
            }
            McpReqKind::Interrupt { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                // Ctrl-C 没送出去就**绝不能**丢掉这条运行的跟踪状态：远端命令多半还在跑，
                // 而 pending_ai_run 一旦取消，poll_run 就再也认领不回它，调用方既中断不了、
                // 也查不到它到底怎么样了。此前这里不看 connected、也不看 send 的结果，
                // 一律先取消再回 Ok——断线时就是「谎报中断成功 + 丢掉运行状态」。
                if !self.sessions[idx].connected {
                    send_err(
                        resp_tx,
                        "会话尚未连接（可能在连接/认证中，或已断线），Ctrl-C 没有发出；\
                         远端命令可能仍在运行，这条运行的状态已保留，可用 poll_run 继续查".into(),
                    );
                    return;
                }
                let s = &mut self.sessions[idx];
                if s.cmd_tx.send(UiCommand::TerminalInput(vec![0x03])).is_err() {
                    send_err(
                        resp_tx,
                        "这个会话的后台连接似乎已经断开，Ctrl-C 没有送达；远端命令可能仍在\
                         运行，这条运行的状态已保留，可用 poll_run 继续查".into(),
                    );
                    return;
                }
                // 打断意味着放弃这条命令的哨兵检测：被打断的程序（比如 `cat`）很可能会把
                // 紧跟着排的标记行当成自己的输入吃掉，哨兵永远等不到。
                s.cancel_pending_ai_run("命令已被 interrupt 中断");
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::Ok),
                });
            }
            McpReqKind::OpenSession { name } => {
                let saved = crate::store::load();
                let Some(c) = saved.iter().find(|c| c.name == name).cloned() else {
                    send_err(resp_tx, format!("未找到名为 “{name}” 的已保存连接"));
                    return;
                };
                if self.mcp_open_approved.contains(&name) {
                    self.do_open_session(&c, id, resp_tx);
                    return;
                }
                // 两种确认框（新开会话 / 写入用户会话）任一挂着就不再叠第二个：两个 modal
                // 同时弹出来，用户根本分不清自己在批准哪一个。
                if self.pending_open_consent.is_some() || self.pending_use_consent.is_some() {
                    send_err(resp_tx, "已有一个请求正在等待用户确认，请稍候重试".into());
                    return;
                }
                // 首次用这条已保存连接给 AI 开会话：弹窗等用户当面批准，而不是仅凭 AI 传的
                // 名字字符串就信任（见 App::handle_ai_open_consent，在 dialogs.rs 渲染）。
                self.pending_open_consent = Some(PendingOpenConsent {
                    conn: c,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    // 60s 对"用户可能不在电脑前"这种常见情况太紧——超时会被下面的
                    // 拒绝分支吃掉,还得让 AI 重新发起一次 open_session 才能再弹一次
                    // 确认框。放宽到 5 分钟,给用户更充裕的反应时间。
                    deadline: Instant::now() + Duration::from_secs(300),
                });
            }
            McpReqKind::CloseSession { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                // 只允许关自己（AI）开的会话，不能关用户自己的会话——关闭权限不超过打开权限。
                if !self.sessions[idx].ai_owned {
                    send_err(resp_tx, "这不是 AI 自己开的会话，不能通过这个工具关闭".into());
                    return;
                }
                self.close_session(idx);
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::Ok),
                });
            }
            McpReqKind::ReadHistory {
                session_uid,
                max_lines,
            } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let text = self.sessions[idx]
                    .terminal
                    .history_text(max_lines as usize);
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::History(text)),
                });
            }
            McpReqKind::ListSavedConnections => {
                let list: Vec<McpSavedConn> = crate::store::load()
                    .into_iter()
                    .map(|c| McpSavedConn {
                        name: c.name,
                        host: c.host,
                        username: c.username,
                        port: c.port,
                    })
                    .collect();
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::SavedConnections(list)),
                });
            }
            McpReqKind::SendInput { session_uid, text } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                // 和 run_command 同样的前置检查。此前这里既不看 connected、也不看 send 的
                // 结果，一律回 Ok——断线时按键根本没送到远端，调用方却以为发出去了，接着
                // 按「已经输入过了」往下走（比如以为 sudo 密码已提交，继续等提示符）。
                if !self.sessions[idx].connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if self.sessions[idx]
                    .cmd_tx
                    .send(UiCommand::TerminalInput(text.into_bytes()))
                    .is_err()
                {
                    send_err(resp_tx, "这个会话的后台连接似乎已经断开，输入没有送达".into());
                    return;
                }
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Ok(McpReqResult::Ok),
                });
            }
            McpReqKind::WriteFile {
                session_uid,
                path,
                content,
                timeout_ms,
            } => {
                // 与 copy 家族同一套路径规则：绝对、无 `.`/`..` 段。此前这里不校验，于是同一个
                // MCP 接口里 copy_to_remote("foo.txt") 会被当场拒掉，write_file("foo.txt")
                // 却能落到 SFTP 的默认 cwd（通常是 $HOME）——同样是「远端相对路径」，两个工具
                // 一个报错一个照写，调用方无从预期。
                if let Err(e) = validate_remote_path(&path) {
                    send_err(resp_tx, e);
                    return;
                }
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "该会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                let op_id = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                // force:true + expect_mtime:0 跳过"外部改动"冲突检测——这条通道只给 AI
                // 自己用，默认信任调用方，直接覆盖。
                let sent = s
                    .cmd_tx
                    .send(UiCommand::WriteFile {
                        id: op_id,
                        path: path.clone(),
                        content,
                        encoding: "UTF-8".into(),
                        eol: Eol::Lf,
                        expect_mtime: 0,
                        force: true,
                    })
                    .is_ok();
                if !sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，写入未发送，请稍后重试".into());
                    return;
                }
                s.pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Write { op_id },
                    path,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                });
            }
            McpReqKind::ReadFile {
                session_uid,
                path,
                force,
                timeout_ms,
            } => {
                // 同 WriteFile：与 copy 家族保持同一套远端路径规则，避免同一接口里两个工具对
                // 「相对路径」给出不同答案。
                if let Err(e) = validate_remote_path(&path) {
                    send_err(resp_tx, e);
                    return;
                }
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "该会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                let op_id = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                // force 由调用方决定：默认 false（20MB 软上限 + 拒绝二进制内容），
                // 传 true 才放宽到 128MB 硬上限、跳过二进制检测——不能替调用方悄悄决定
                // "读到二进制就当文本硬解码"，那样只会得到一堆乱码。
                let sent = s
                    .cmd_tx
                    .send(UiCommand::ReadFile {
                        id: op_id,
                        path: path.clone(),
                        force,
                    })
                    .is_ok();
                if !sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，读取未发送，请稍后重试".into());
                    return;
                }
                s.pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Read { op_id },
                    path,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                });
            }
            McpReqKind::CopyToRemote {
                session_uid,
                local_path,
                remote_path,
                timeout_ms,
            } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "该会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                if let Err(e) = validate_local_path(&local_path) {
                    send_err(resp_tx, e);
                    return;
                }
                if let Err(e) = validate_remote_path(&remote_path) {
                    send_err(resp_tx, e);
                    return;
                }
                // 本地源是否存在留给 upload() 自己异步探测（它本来就要在 worker 侧
                // 做 tokio::fs::metadata）——这里不再同步 stat，避免本地路径落在慢速/
                // 挂起的挂载点（网络盘、FUSE）时卡住 UI 线程（这段处理逻辑本身跑在
                // egui 每帧的事件排空里，见 drain_mcp_calls）。源不存在时 upload()
                // 打开文件会失败，重试用尽后经 TransferDone 正常报错。
                let op_id = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let remote_dir = remote_parent(&remote_path);
                // 远端文件名总是按调用方要求的来（Upload 命令原生支持覆盖名字，见
                // proto.rs 的 `remote_name` 字段）——不再需要借符号链接“改名”绕过
                // upload() 按本地 basename 取名的旧限制，也就不再需要临时目录及其清理。
                let remote_name = Some(remote_basename(&remote_path));
                let sent = s
                    .cmd_tx
                    .send(UiCommand::Upload {
                        id: op_id,
                        local: local_path,
                        remote_dir,
                        remote_name,
                        policy: ConflictPolicy::Overwrite,
                    })
                    .is_ok();
                if !sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，复制未发送，请稍后重试".into());
                    return;
                }
                s.pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Copy { op_id },
                    path: remote_path,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                });
            }
            McpReqKind::CopyToRemoteFromCaller {
                session_uid,
                remote_path,
                size,
                timeout_ms,
            } => {
                let Some(source) = upload_source else {
                    send_err(resp_tx, "调用方上传请求缺少文件数据流".into());
                    return;
                };
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "该会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                if let Err(e) = validate_remote_path(&remote_path) {
                    send_err(resp_tx, e);
                    return;
                }
                let op_id = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let sent = s
                    .cmd_tx
                    .send(UiCommand::UploadFromMcp {
                        id: op_id,
                        source,
                        size,
                        remote_path: remote_path.clone(),
                    })
                    .is_ok();
                if !sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，复制未发送，请稍后重试".into());
                    return;
                }
                s.pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Copy { op_id },
                    path: remote_path,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                });
            }
            McpReqKind::CopyFromRemoteToCaller {
                session_uid,
                remote_path,
                timeout_ms,
            } => {
                // 这个操作的响应完全经 `download_sink` 送达（worker 探测远端路径后精确回
                // Ok(流)/Err(消息)）；`resp_tx` 只在下面这几条前置校验失败时才会被用到——
                // 两者互斥，见 handle_conn 里 `is_caller_download` 分支的说明。
                let Some(download_sink) = download_sink else {
                    send_err(resp_tx, "调用方下载请求缺少响应通道".into());
                    return;
                };
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "该会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                if let Err(e) = validate_remote_path(&remote_path) {
                    send_err(resp_tx, e);
                    return;
                }
                let op_id = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let sent = s
                    .cmd_tx
                    .send(UiCommand::DownloadToMcp {
                        id: op_id,
                        remote_path: remote_path.clone(),
                        download_sink,
                    })
                    .is_ok();
                if !sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，复制未发送，请稍后重试".into());
                    return;
                }
                // resp_tx 就此不再使用（响应已经交给 download_sink 那条路），直接丢弃；
                // pending_file_op 仍然占位以复用忙碌保护 + TransferDone 收尾记账。
                drop(resp_tx);
                s.pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Copy { op_id },
                    path: remote_path,
                    resp_tx: None,
                    req_id: id,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                });
            }
            McpReqKind::CopyBetweenSessions {
                src_session_uid,
                src_remote_path,
                dest_session_uid,
                dest_remote_path,
                timeout_ms,
            } => {
                if src_session_uid == dest_session_uid {
                    send_err(resp_tx, "源和目标不能是同一个会话；同会话内复制请用 run_command 执行 cp".into());
                    return;
                }
                let Some(src_idx) = self.session_idx_by_uid(src_session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(src_session_uid));
                    return;
                };
                if !self.sessions[src_idx].connected {
                    send_err(resp_tx, "源会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if self.sessions[src_idx].pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "源会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                let Some(dest_idx) = self.session_idx_by_uid(dest_session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(dest_session_uid));
                    return;
                };
                if !self.sessions[dest_idx].connected {
                    send_err(resp_tx, "目标会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if self.sessions[dest_idx].pending_file_ops.len() >= MAX_CONCURRENT_FILE_OPS {
                    send_err(resp_tx, "目标会话并发文件操作已达上限，请稍候重试".into());
                    return;
                }
                if let Err(e) = validate_remote_path(&src_remote_path) {
                    send_err(resp_tx, e);
                    return;
                }
                if let Err(e) = validate_remote_path(&dest_remote_path) {
                    send_err(resp_tx, e);
                    return;
                }
                let op_id = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                let deadline = Instant::now() + clamp_timeout(timeout_ms);
                let marker = format!(
                    "ishell-ai-relay-{op_id}-{}",
                    rand_marker_suffix()
                );
                // 一次性 ed25519 密钥对：只用于这一次直连尝试，成功与否都会在
                // UntrustingAfterDirect 阶段撤销公钥、源会话侧的私钥文件也会被清理，
                // 不留长期可用的免密信任。
                let (priv_key_pem, pub_key_line) = match generate_temp_keypair(&marker) {
                    Ok(pair) => pair,
                    Err(e) => {
                        send_err(resp_tx, format!("生成一次性密钥对失败：{e}"));
                        return;
                    }
                };
                // duplex 内存管道：直连尝试期间两端都先不发出去，只有直连失败转中转时
                // 才真正派上用场；字节全程只经过 iShell 进程内存，不落盘任何一方。
                let (pipe_writer, pipe_reader) = tokio::io::duplex(128 * 1024);
                let sent = self.sessions[dest_idx]
                    .cmd_tx
                    .send(UiCommand::TrustTempKey { op_id, pub_key_line })
                    .is_ok();
                if !sent {
                    send_err(resp_tx, "目标会话的后台连接似乎已经断开，复制未发送，请稍后重试".into());
                    return;
                }
                self.sessions[src_idx].pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Copy { op_id },
                    path: src_remote_path.clone(),
                    resp_tx: None,
                    req_id: id,
                    deadline,
                });
                self.sessions[dest_idx].pending_file_ops.push(PendingAiFileOp {
                    kind: FileOpKind::Copy { op_id },
                    path: dest_remote_path.clone(),
                    resp_tx: None,
                    req_id: id,
                    deadline,
                });
                self.cross_copy_jobs.push(CrossCopyJob {
                    op_id,
                    req_id: id,
                    resp_tx: Some(resp_tx),
                    src_uid: src_session_uid,
                    dest_uid: dest_session_uid,
                    src_remote_path,
                    dest_remote_path,
                    pipe_writer: Some(pipe_writer),
                    pipe_reader: Some(pipe_reader),
                    marker,
                    trust_established: false,
                    cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    direct_result: None,
                    phase: CrossCopyPhase::TrustingB,
                    deadline,
                    // TrustingB 阶段没有独立的短超时——建立信任本身很快，等到真正失败/
                    // 成功都由 TempKeyTrusted 事件驱动；这里先随便给个不会被用到的占位值
                    // （只有进入 DirectCopying/UntrustingAfterDirect 才会被重新赋值）。
                    phase_deadline: deadline,
                    priv_key_pem: Some(priv_key_pem),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        remote_basename, remote_parent, trim_leading_echo, validate_local_path,
        validate_remote_path,
    };

    #[test]
    fn remote_path_must_be_absolute() {
        assert!(validate_remote_path("notes.txt").is_err());
        assert!(validate_remote_path("/notes.txt").is_ok());
    }

    #[test]
    fn remote_path_rejects_dot_and_dotdot_segments() {
        assert!(validate_remote_path("/foo/../bar").is_err());
        assert!(validate_remote_path("/foo/./bar").is_err());
        assert!(validate_remote_path("/foo/bar").is_ok());
    }

    #[test]
    fn remote_path_rejects_empty_basename() {
        // "/" 和 "////" 拆分出来的文件名都是空串——不能悄悄当成合法目标。
        assert!(validate_remote_path("/").is_err());
        assert!(validate_remote_path("////").is_err());
    }

    #[test]
    fn remote_parent_and_basename_split_normal_paths() {
        assert_eq!(remote_parent("/foo/bar.txt"), "/foo");
        assert_eq!(remote_basename("/foo/bar.txt"), "bar.txt");
        assert_eq!(remote_parent("/bar.txt"), "/");
    }

    // `Path::is_absolute()` 的判定标准随平台而变（Windows 下 "/tmp/x" 没有盘符前缀，
    // 不算绝对路径）——这条 MCP 通道本身也只在 unix 上真正启用（见 spawn_mcp_listener
    // 的 `#[cfg(unix)]` 版本），这几个用 POSIX 风格路径断言"应该合法"的用例只在 unix
    // 上跑，避免在 Windows CI 上因为平台语义差异而不是真实回归失败。
    #[test]
    #[cfg(unix)]
    fn local_path_must_be_absolute() {
        assert!(validate_local_path("notes.txt").is_err());
        assert!(validate_local_path("/tmp/notes.txt").is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn local_path_rejects_dot_and_dotdot_segments() {
        assert!(validate_local_path("/tmp/../etc/passwd").is_err());
        assert!(validate_local_path("/tmp/./notes.txt").is_err());
    }

    #[test]
    fn local_path_rejects_root_and_missing_filename() {
        assert!(validate_local_path("/").is_err());
    }

    #[test]
    fn trim_leading_echo_strips_command_line_only() {
        let out = "hostname && whoami\r\nhost\nuser\n";
        assert_eq!(trim_leading_echo(out, "hostname && whoami"), "host\nuser\n");
        // 命令文本对不上时原样返回（不误伤）
        assert_eq!(trim_leading_echo(out, "other"), out);
    }

    #[test]
    fn trim_leading_echo_keeps_trailing_prompt_fragment_rather_than_guessing() {
        // 结尾那段没有换行收尾的提示符残片不再尝试猜测删除——宁可留一点噪音，
        // 也不能在 PS1 为空等场景把真实输出误删。
        let out = "hostname && whoami && pwd\ns3-server\ns3\n/home/s3\n(env) s3@s3-server:~\n$ ";
        assert_eq!(
            trim_leading_echo(out, "hostname && whoami && pwd"),
            "s3-server\ns3\n/home/s3\n(env) s3@s3-server:~\n$ "
        );
    }
}
