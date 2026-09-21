//! AI/MCP 控制通道：本地 Unix domain socket 桥接，供独立的 `ishell-mcp` stdio 代理进程
//! （见 `src/bin/ishell-mcp.rs`）连接，把 list/run/poll/read/interrupt 请求转发到本进程
//! 持有的活跃 SSH 会话。由 `store::load_mcp_consent()` 开关把门（0.19 起默认开），
//! 一次 socket 连接 = 一问一答。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd};
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
        self.end_pending_ai_run(reason, true);
    }

    /// 无条件丢弃这条运行，**连已缓存的结果一起**：`interrupt` 与「卡死自动回收」用。
    /// 两者的语义都是「把这个会话的闸门还回来」——保留缓存结果会让 `pending_ai_run` 继续
    /// 占着闸门，调用方收到「已中断」却发不出下一条命令。
    pub(super) fn discard_pending_ai_run(&mut self, reason: &str) {
        self.end_pending_ai_run(reason, false);
    }

    fn end_pending_ai_run(&mut self, reason: &str, keep_finished: bool) {
        if let Some(mut pending) = self.pending_ai_run.take() {
            if keep_finished && pending.finished_result.is_some() {
                // 运行**其实已经完成了**（exit N 的退出码经 ShellExited 先于断线到达、
                // 已缓存进 finished_result）：结果是有效的，保留给 poll_run 取走——断线
                // 不能把它抹成一句「已断线」。
                // 捕获仍要撤：结果已经在手上，留着它只会让 `ai_capture_pending()` 一直为真、
                // 把注入闸门堵到断线重建 Terminal 为止。
                self.terminal.cancel_ai_capture();
                self.pending_ai_run = Some(pending);
                return;
            }
            if let Some(tx) = pending.resp_tx.take() {
                // 明确回错误，而不是 Ok(Run{finished:false})——后者会让调用方以为"命令还在
                // 跑，可以继续 poll_run"，但这条 run_id 已经被清空、注定 poll 不到了。
                // 命令实际执行到哪一步、有没有副作用都无法确认，输出仅供参考。
                let output = self.terminal.peek_ai_output().unwrap_or_default();
                let output = output.trim_end();
                let output = if output.is_empty() {
                    output.to_string()
                } else {
                    // 部分输出常以半截行收尾（被 ^C 打断的行没有换行）：补一个换行，
                    // 错误文案的边界才清晰，不会和后面的字串粘在一起。
                    format!("{output}\n")
                };
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

    /// shell 退出（`WorkerEvent::ShellExited`，`exit N`/崩溃）时调用：排队的哨兵注定
    /// 不会再打印，用通道上报的退出码直接给这条运行一个 finished=true 的收尾。
    /// 有等待者直发；没有（此前已超时返回过 finished=false）则缓存进 finished_result，
    /// 等下一次 poll_run 取走——`exit 42` 的 42 因此不再丢失。
    pub(super) fn finish_ai_run_with_exit_code(&mut self, code: i32) {
        if let Some(mut pending) = self.pending_ai_run.take() {
            // 输出与另外两条收尾路径同样处理：去掉命令行回显、按上限截断。少一处都会让
            // 「同一条命令、不同收尾路径」给出形状不一样的 output（超长输出还会撑爆响应）。
            let output = self.terminal.peek_ai_output().unwrap_or_default();
            let output = cap_output_for_ai(trim_leading_echo(&output, &pending.command));
            if let Some(tx) = pending.resp_tx.take() {
                let _ = tx.send(McpResponse {
                    id: pending.req_id,
                    result: Ok(McpReqResult::Run(McpRunResult {
                        run_id: pending.run_id,
                        finished: true,
                        output,
                        exit_code: Some(code),
                    })),
                });
                self.terminal.cancel_ai_capture();
            } else {
                // 与哨兵完成的缓存路径同语义：结果留待 poll_run 取走，运行继续保持挂起态。
                // 捕获同样要撤（理由见 `end_pending_ai_run`）：退出码已经拿到，shell 也死了，
                // 再等下去只是把注入闸门堵着。
                self.terminal.cancel_ai_capture();
                pending.finished_result = Some((code, output));
                self.pending_ai_run = Some(pending);
            }
        }
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

/// 临时公钥撤销能否真正送达目标 worker。入参是目标会话的 `connected`（`None` = 会话已不存在）。
///
/// **断线时不能拿 `cmd_tx.send` 的结果当「会被执行」**：重连（`reconnect_session`）换的是全新
/// 的通道，发到旧通道的命令永不处理；而旧 worker 退出主循环后还要做收尾（等 MCP 转发注册、
/// 远端 `rm -f`，网络真断时可能卡很久），期间 `cmd_rx` 仍活着，`send` 照样返回 Ok——撤销
/// 被静默吞掉，公钥残留却没有任何告警。所以断线一律判「送不达」，由调用方直接告警。
fn untrust_route(dest_connected: Option<bool>) -> Result<(), &'static str> {
    match dest_connected {
        None => Err("目标会话已不存在，无法自动撤销"),
        Some(false) => Err("目标会话已断线，撤销无法送达"),
        Some(true) => Ok(()),
    }
}

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

/// 把一条已保存连接转成 `ConnectConfig`。闸门与侧栏双击相同：这次认证用得上、
/// 但解不开的密文拒绝直接连，并说明要用户在连接编辑里重填。故意留空的密码可以连。
fn connect_config_from_saved(c: &SavedConnection) -> Result<ConnectConfig, String> {
    if let Some(msg) = direct_connect_refusal(c) {
        return Err(msg);
    }
    if c.auth_kind == "key" && c.key_path.trim().is_empty() {
        return Err(
            "这条连接用私钥登录，但没有私钥路径。请让用户在 iShell 的连接编辑里补上后再试 open_session。"
                .into(),
        );
    }
    if c.use_jump && c.jump_auth_kind == "key" && c.jump_key_path.trim().is_empty() {
        return Err(
            "这条连接的跳板用私钥登录，但没有跳板私钥路径。请让用户在 iShell 的连接编辑里补上后再试 open_session。"
                .into(),
        );
    }
    let jump = c.use_jump.then(|| JumpHost {
        host: c.jump_host.clone(),
        port: c.jump_port,
        username: c.jump_username.clone(),
        auth: auth_method(
            &c.jump_auth_kind,
            &c.jump_password,
            &c.jump_key_path,
            &c.jump_passphrase,
        ),
    });
    Ok(ConnectConfig {
        host: c.host.clone(),
        port: c.port,
        username: c.username.clone(),
        auth: auth_method(&c.auth_kind, &c.password, &c.key_path, &c.passphrase),
        label: c.name.clone(),
        jump,
        forward_agent: c.forward_agent,
        transport: crate::proto::Transport::Ssh,
    })
}

fn direct_connect_refusal(c: &SavedConnection) -> Option<String> {
    let blocked = c.blocked_secrets();
    if blocked.is_empty() {
        return None;
    }
    let names: Vec<&str> = blocked
        .iter()
        .map(|s| match s {
            crate::store::BlockedSecret::Password => "登录密码",
            crate::store::BlockedSecret::Passphrase => "私钥口令",
            crate::store::BlockedSecret::JumpPassword => "跳板密码",
            crate::store::BlockedSecret::JumpPassphrase => "跳板私钥口令",
        })
        .collect();
    Some(format!(
        "连接「{}」的{}解不开（主密钥与密文不匹配），不能直接连接。请让用户在 iShell 的连接编辑里重新填写，保存后再试 open_session。不要猜测或编造密码。",
        c.name,
        names.join("、")
    ))
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

/// `run_command`/`start_command` 的命令文本校验：必须是**一条单行命令**。
///
/// 为什么非拦不可（而不是"尽力而为地跑"）：命令是当作按键打进真实 tty 的，里面的换行就是
/// 用户按下的回车，shell 会把它拆成**多条**命令依次执行。两种完成检测都对不上：
/// - shell 集成：每一行各发一对 `OSC 133;C/D`，捕获在**第一行**的 `D` 就收束——调用方拿到
///   `finished=true` 和第一行的退出码，后面几行还在跑，它们的 `D` 又会被下一条运行的捕获
///   吃掉，一路错位。实测（bash 5.1）：`echo one\necho two` 产生两对 C/D。
/// - 哨兵：哨兵行排在最后一行之后，多行本身能跑通，但未闭合的引号/heredoc 会把哨兵行当作
///   内容吞掉，表现为永远等不到完成（工具描述里那条"反复超时、输出却在增长"就是它）。
///
/// 两条路各有各的错法，而**它们的正确用法是同一个**：改写成单行（`;`/`&&`），或者用
/// `write_file` 落一个脚本再执行。所以这里一律拒绝，错误文案直接给出改写方式——比让调用方
/// 从一个形状古怪的结果里反推发生了什么便宜得多。
fn validate_run_command(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("command 为空：空命令只会让 shell 打一个新提示符，不会执行任何东西（\
                    shell 集成下还会误报成上一条命令的退出码）。要单纯看屏幕请用 read_screen"
            .into());
    }
    if command.contains('\n') || command.contains('\r') {
        return Err("command 含换行：命令是当作按键打进真实终端的，换行会被 shell 当成回车、\
                    拆成多条命令依次执行，完成检测只认得第一条——请改写成单行（用 `;` 或 \
                    `&&` 连接），或先用 write_file 写一个脚本再执行它"
            .into());
    }
    // 整条命令、或命令链里单独成段的一环是 `logout`：它的本意是「关掉这个会话」，那是
    // close_session 的事。不能照 `exit` 的办法包进子 shell——子 shell 永远不是登录 shell，
    // 实测 `(logout)` 必报「logout: not login shell」并返回 1（父进程是登录 shell 也一样），
    // 调用方拿到的是一个莫名其妙的失败；直接发出去又会杀掉登录 shell、作废这条运行。
    // 两条路都不对，拒绝。（分段扫描与改写 exit 共用同一份判据，两处不会漂移。）
    for (s, e) in top_level_command_segments(command) {
        if normalized_session_ender(&command[s..e]).is_some_and(|(head, _)| head == "logout") {
            return Err("command 里含有 logout：run_command 用来执行命令，不用来关会话——关掉你\
                        自己开的会话请用 close_session；只是想拿一个退出码请用 `exit N`（会在\
                        子 shell 里执行，会话不受影响）"
                .into());
        }
    }
    // `exit` 后面还接着命令：原语义里 exit 一执行 shell 就终止，后面的命令**一条都不会跑**
    // ——`cond || exit 1; rm -rf build` 这类守卫写法全靠这一点。可 run_command 为了保住会话
    // 要把 exit 改写进子 shell，`(exit 1)` 只结束子 shell，后面的命令照跑，守卫就此失效
    // （实测 bash：`test -f /nonexistent || (exit 1); echo DANGER` 打印 DANGER）。改写保不住
    // 这个语义，原样发出去又会杀掉登录 shell——拒绝，让调用方把后续命令改成条件分支。
    if exit_followed_by_command(command) {
        return Err("command 里的 exit 后面还有命令：在 run_command 里 exit 会被改写进子 shell \
                    执行（为了不杀掉会话），它**无法终止**后面的命令——`cond || exit 1; rm …` \
                    这类守卫会失效、rm 照跑。请把后续命令改成条件分支（`cond && rm …`），或把 \
                    exit 放到命令最后"
            .into());
    }
    Ok(())
}

/// 命令里是否有「顶层独立的 `exit [n]` 段，且它后面还有非空的命令段」。注释与末尾的空段
/// （`cmd; exit;`）不算后续命令。
fn exit_followed_by_command(command: &str) -> bool {
    let segs: Vec<&str> = top_level_command_segments(command)
        .into_iter()
        .map(|(s, e)| &command[s..e])
        .filter(|seg| !seg.trim().is_empty())
        .collect();
    let Some((_, init)) = segs.split_last() else {
        return false;
    };
    init.iter()
        .any(|seg| normalized_session_ender(seg).is_some_and(|(head, _)| head == "exit"))
}

/// 把「整条命令就是 exit/logout」这种形态规范化出来：返回 `(命令字, 规范化后的整条)`。
/// 容忍首尾空白与行尾的 `;`/`&`；参数只认一个可选的整数（`exit $var`/`exit 1 2` 等形态
/// 太多、误判风险配不上收益，一律不认）。`wrap_session_ender` 与 `validate_run_command`
/// 共用这一份判据，两处不会漂移。
fn normalized_session_ender(command: &str) -> Option<(&str, &str)> {
    let trimmed = command
        .trim()
        .trim_end_matches(|c: char| c == ';' || c == '&' || c.is_whitespace());
    let mut parts = trimmed.split_whitespace();
    let head = parts.next()?;
    if head != "exit" && head != "logout" {
        return None;
    }
    let numeric = |s: &str| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    match (parts.next(), parts.next()) {
        (None, None) => {}
        (Some(arg), None) if numeric(arg) => {}
        _ => return None,
    }
    Some((head, trimmed))
}

/// `#` 是否处于「词首」位置（注释只能从这里开始）：命令开头、空白后、或命令分隔/括号
/// 操作符之后。
///
/// **不含 `{`/`}`**：它们在 bash 里不是操作符，只有后面跟空白时才是保留字——`${#arr[@]}`
/// 里的 `#` 是取长度，不是注释（实测 `x=abc; echo ${#x}` 输出 3）。算进来的话扫描会在
/// `{#` 处截断，其后的 `logout`/`exit` 全部漏检。
fn is_shell_word_start(prev: u8) -> bool {
    prev == 0
        || prev.is_ascii_whitespace()
        || matches!(prev, b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>')
}

/// 把单行命令切成顶层命令段，返回每段的字节区间（不含分隔符）。
///
/// 状态机跟踪单引号、双引号（含 `\` 转义）、反引号与 `()` 深度（子 shell / 命令替换 /
/// 算术替换）：它们内部的 `;` 不是顶层分隔符——子 shell 里的 `exit` 本来就杀不掉登录
/// shell，命令替换里的更是连触发都触发不了。顶层分隔符为 `;`、`&&`、`||` 与单个 `&`、
/// `|`；单个 `&`/`|` 要避让 `>&`、`<&`、`&>`、`>|` 这些双字符重定向操作符。词首的 `#`
/// 把剩余内容全部当作注释（当前段截到这里、扫描结束）。返回的段保证不含任何顶层注释
/// 与分隔符，切点都在 ASCII 字节上，天然是合法的字符串切分边界。
fn top_level_command_segments(command: &str) -> Vec<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Normal,
        SQuote,
        DQuote,
        Backtick,
    }
    let bytes = command.as_bytes();
    let mut segs = Vec::new();
    let (mut st, mut depth, mut start) = (St::Normal, 0usize, 0usize);
    let mut prev = 0u8;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let mut next = i + 1;
        match st {
            St::Normal => match b {
                b'\'' => st = St::SQuote,
                b'"' => st = St::DQuote,
                b'`' => st = St::Backtick,
                b'\\' => next = (i + 2).min(bytes.len()),
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b'#' if depth == 0 && is_shell_word_start(prev) => {
                    segs.push((start, i));
                    return segs;
                }
                b';' if depth == 0 => {
                    segs.push((start, i));
                    start = i + 1;
                }
                b'&' if depth == 0 => match bytes.get(i + 1) {
                    _ if prev == b'>' || prev == b'<' => {} // >&、<&：重定向操作符的一部分
                    Some(b'>') => {}                         // &>
                    Some(b'&') => {
                        segs.push((start, i));
                        start = i + 2;
                        next = i + 2;
                    }
                    _ => {
                        segs.push((start, i));
                        start = i + 1;
                    }
                },
                b'|' if depth == 0 => match bytes.get(i + 1) {
                    _ if prev == b'>' => {} // >|
                    Some(b'|') | Some(b'&') => {
                        segs.push((start, i));
                        start = i + 2;
                        next = i + 2;
                    }
                    _ => {
                        segs.push((start, i));
                        start = i + 1;
                    }
                },
                _ => {}
            },
            St::SQuote => {
                if b == b'\'' {
                    st = St::Normal;
                }
            }
            St::DQuote => match b {
                b'"' => st = St::Normal,
                b'\\' => next = (i + 2).min(bytes.len()),
                _ => {}
            },
            St::Backtick => match b {
                b'`' => st = St::Normal,
                b'\\' => next = (i + 2).min(bytes.len()),
                _ => {}
            },
        }
        prev = b;
        i = next;
    }
    segs.push((start, bytes.len()));
    segs
}

/// `exit [n]` 会直接杀掉登录 shell：会话断线重连，正在跑的这条 run 被作废、退出码丢失，
/// cwd/环境变量等 shell 状态全丢。而 run_command 的语义是「执行命令并拿回退出码」，不是
/// 「关掉会话」——关会话有专用工具 close_session，且只能关 AI 自己的。所以**命令的最后
/// 一段**是顶层独立的 `exit [n]` 时（整条就是它，或 `cd /x; exit 7`、`make || exit 1`），
/// 把这一段改写进子 shell 执行：`(exit 42)` 的退出码与原命令一致（`(exit)` 同样沿用上一条
/// 命令的 `$?`），shell 集成与哨兵两条完成检测路都能照常捕获，会话本身不受影响。只替换
/// 这一段本身，分隔符与注释保持原位，其余文本逐字节不动。
///
/// **只改最后一段，这是语义等价的前提**：exit 后面没有命令时，「shell 终止」与「子 shell
/// 结束」对调用方唯一可见的差别就是会话死不死；exit 后面**还有**命令时，原语义是那些命令
/// 永远不执行（`cond || exit 1; rm …` 的守卫），而 `(exit 1)` 拦不住它们——那种形态由
/// `validate_run_command` 直接拒绝，根本到不了这里。
///
/// **只包 exit，不包 logout**：子 shell 不是登录 shell，`(logout)` 必然报错返回 1，与原命令
/// 的语义毫不相干——logout 由 `validate_run_command` 直接拒绝（含复合命令里的段）。
///
/// 仍拦不住、维持「shell 死掉 → 运行作废、报错明确、自动重连兜底」旧路径的形态：
/// - 控制流体内打头的 exit（`if …; then exit; fi`、`while …; do exit; done`、`f() { exit; }`）；
/// - 非数值参数（`exit $?`、`exit $code`、`exit "$(cmd)"`）——形态太多，误判风险配不上收益。
fn wrap_session_ender(command: &str) -> std::borrow::Cow<'_, str> {
    let Some((s, e)) = top_level_command_segments(command)
        .into_iter()
        .rev()
        .find(|&(s, e)| !command[s..e].trim().is_empty())
    else {
        return std::borrow::Cow::Borrowed(command);
    };
    let seg = &command[s..e];
    let Some(("exit", trimmed)) = normalized_session_ender(seg) else {
        return std::borrow::Cow::Borrowed(command);
    };
    // 只替换段内 trim 后的主体，段首/段尾空白、分隔符与注释原样保留。
    let lead = seg.len() - seg.trim_start().len();
    let body_end = s + lead + seg.trim().len();
    std::borrow::Cow::Owned(format!(
        "{}({trimmed}){}",
        &command[..s + lead],
        &command[body_end..]
    ))
}

/// 一条运行「被放弃」的判据：没人在等、终端也不再有输出、且已经很久没人来 `poll_run`。
///
/// 三个条件缺一不可——**有人等 / 有输出 / 有轮询，任一成立就不回收**：
/// - 有等待者（`run_command` 还在等，或 `poll_run` 挂着）→ 有人在意，不回收；
/// - 终端仍在输出（构建/测试在跑）→ 命令活着，不回收；
/// - 最近轮询过 → AI 还在跟进，不回收。
///
/// 反过来，「完成信号被 `cat`/REPL 吃掉 + AI 不再 poll」正好三条全中，闸门被永久占死的
/// 那种状态会被回收。**不按声明超时推算绝对期限**——`start_command` 的声明超时是协议
/// 最小值（100ms），按倍数推算会把一条正常的长命令在几分钟内误杀。
///
/// **残余误判（明说，别再指望它不存在）**：一条长时间既不输出、AI 又不轮询的命令
/// （`start_command("sleep 3600")` 之后只用 `read_screen` 盯着）同样三条全中，会在
/// `RECLAIM_IDLE` 后被回收。闸门还回来了，但回收会取消捕获——命令真跑完时那个完成信号
/// 落到空处，**那条命令的退出码再也取不回来**。回收文案里写明了这一点。
fn run_is_abandoned(
    since_last_poll: Duration,
    terminal_output_idle: bool,
    has_live_waiter: bool,
) -> bool {
    !has_live_waiter && terminal_output_idle && since_last_poll >= RECLAIM_IDLE
}

/// 自动回收的静默时长：既要远大于 AI 正常的轮询间隔（秒级），又要让卡死的闸门在一次
/// 人机会话里能自己解开。
const RECLAIM_IDLE: Duration = Duration::from_secs(600);

/// 某会话上一次正在等待的 AI 命令运行（`run_command` 武装，`poll_run` 续等）。
pub(super) struct PendingAiRun {
    run_id: u64,
    /// 当前这次调用的耐心：到期先把 finished=false + 部分输出回给等待者，**不**清运行——
    /// 等 poll_run 续等时会按它自己的 timeout_ms 重新武装。
    deadline: Instant,
    /// 最近一次「有人在意这条运行」的时刻：武装时置位，每次 `poll_run` 续等时刷新。
    /// 与终端输出静止一起构成自动回收的判据，见 [`run_is_abandoned`]。
    last_poll_at: Instant,
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
    /// 发起方代理的进程标识与可读来源（请求快照，真正开窗口时记到会话归属上）。
    pub(super) actor: Option<String>,
    pub(super) origin: Option<String>,
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
    /// 发起方的可读来源描述（代理 `Bind` 时携带，见 `McpRequest::origin`），弹窗展示用。
    pub(super) origin: Option<String>,
    deadline: Instant,
}

/// **AI 只能碰它自己开的会话**：读和写一样。
///
/// 用户拍板（2026-09-18）：「AI 只能读取自己打开的窗口，不允许读取和操作客户自己的窗口，否则
/// 太乱了」「严禁操作其他人的客户端」。所以不再区分「用户的窗口」「另一个 AI 的窗口」「只读还是
/// 写入」，也不再有授权弹窗——判据只剩一条：这个会话是不是**发起这条请求的 AI 进程**用
/// `open_session` 开的（按代理进程标识 `actor` 与会话记录的 `ai_owner` 比对）。
///
/// 此前的版本：用户窗口只读随意、写入弹窗一次后整个运行期放行，且那次授权不分发起方——点一次
/// 允许，之后任何 AI（包括被错误路由过来的别人的 AI）都能读写这个窗口。
///
/// 旧版代理（不带 `actor`）开的共享池窗口不属于任何人；不带 `actor` 的请求也不拥有任何窗口。
/// 两者在 v6 起本来就过不了协议版本校验，这里按「不是你的」处理，不留兼容口子。
///
/// `actor` 由 iShell 在配对握手时记下、此后按连接凭据回填（见 `handle_conn`），不是调用方
/// 自报的值——自报的话，冒用别人的 actor 就能拿走别人的窗口。
fn session_owned_by(ai_owned: bool, ai_owner: Option<&str>, actor: Option<&str>) -> bool {
    ai_owned && actor.is_some() && ai_owner == actor
}

/// 一条请求涉及的会话里，第一个**不属于发起方**的 uid（`None` = 全部是它自己的，放行）。
///
/// `lookup(uid)` 给出会话的 `(ai_owned, ai_owner)`；返回 `None` 表示这个 uid 没有对应会话——
/// **不在这道门拦**：放过去，由各分支回「会话不存在 + 你自己的会话列表」那条有用的报错。
/// 抽成纯函数是为了能测：调用方 `App` 在测试里造不出来。
fn first_foreign_session<'a>(
    uids: &[u64],
    lookup: impl Fn(u64) -> Option<(bool, Option<&'a str>)>,
    actor: Option<&str>,
) -> Option<u64> {
    uids.iter().copied().find(|&uid| {
        lookup(uid).is_some_and(|(ai_owned, owner)| !session_owned_by(ai_owned, owner, actor))
    })
}

/// 拒绝时回给 AI 的话：说清规则与下一步，不透露那个会话的任何信息（标题/主机都不给）。
fn foreign_session_refusal(uid: u64) -> String {
    format!(
        "会话 uid={uid} 不是你用 open_session 开的：AI 只能读写**自己开的**会话，用户自己打开的\
         窗口和其它 AI 的窗口一律不许读取或操作（硬规则，没有授权弹窗，重试也没用）。需要在\
         某台机器上干活，请用 list_saved_connections 查连接名、open_session 开你自己的会话"
    )
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
        // 配置目录可能压根还不存在：0.19 起这个开关默认是开的，全新安装的用户可能一次设置
        // 都没存过就直接起来了，此时 ~/.config/ishell/ 还没被任何 save_* 建出来，
        // `UnixListener::bind` 会以 ENOENT 失败——现象是「默认开着，但 AI 就是连不上」，
        // 而且只有看日志才知道。先把目录建出来。
        if let Some(dir) = sock_path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("创建配置目录 {} 失败：{e}", dir.display());
            }
        }
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
        // 远端主机的人都摸得到它——不能像纯本地场景那样假设连接数天然很少。握手/Identify
        // 与长操作分两档信号量：上传卡住时仍要让新的 Identify 进得来，否则整个 MCP 发现层
        // 会被 32 个挂死的 copy_to_remote 堵死约 24h。
        let handshake_limit = Arc::new(tokio::sync::Semaphore::new(MAX_MCP_HANDSHAKES));
        let work_limit = Arc::new(tokio::sync::Semaphore::new(MAX_MCP_CONNECTIONS));
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = Arc::clone(&handshake_limit).try_acquire_owned() else {
                // 半开连接已达上限：直接丢弃这个连接（不回应也不占用任务），
                // 对端会看到连接被关闭，比无限堆积任务安全。
                continue;
            };
            tokio::spawn(handle_conn(
                stream,
                tx.clone(),
                ctx.clone(),
                permit,
                Arc::clone(&work_limit),
            ));
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

/// 并发连接数上限（见 spawn_mcp_listener 里的工作信号量）。长操作（上传/命令）占这一档。
#[cfg(unix)]
const MAX_MCP_CONNECTIONS: usize = 32;
/// 读首行 + Identify/握手的并发上限。必须高于 `MAX_MCP_CONNECTIONS`，否则挂死的上传会把
/// 发现层一起堵死。
#[cfg(unix)]
const MAX_MCP_HANDSHAKES: usize = 64;
/// 首行请求的最大字节数：整条请求（含 write_file 的 content）作为一行 JSON 读入内存，故这个
/// 上限同时是「单连接请求缓冲的峰值」。32 MiB 对 write_file 的文本/源码同步已绰绰有余（真正的
/// 大文件/二进制走 copy_to_remote 的分块字节流，不塞进 JSON 行），又把「MAX_MCP_CONNECTIONS
/// 条连接同时灌满大请求」的最坏并发内存从 256MiB×32≈8GiB 压到 32MiB×32≈1GiB；同时兜住「恶意/
/// 异常连接持续灌数据不换行」。注意该 socket 可能经 SSH 反向转发暴露到远端主机，攻击面不止本机。
///
/// ⚠ 它是靠 `AsyncReadExt::take` 实现的，而 `take` 套的是**整条连接**，不是"一行"。
/// `copy_to_remote` 的文件字节紧跟在请求行之后走同一个 reader，所以上传路径**必须**在读完
/// 请求行后把 `Take` 摘掉（见 `handle_conn` 里 `is_caller_upload` 那段），否则这个上限会连
/// 文件流一起限死。别把那段"多余的"拆包代码优化掉。
#[cfg(unix)]
const MAX_MCP_LINE_BYTES: u64 = 32 * 1024 * 1024;
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

/// iShell 签发的**连接凭据**（v6，见 `McpRequest::ticket`）：配对握手验过对方的 token 证明
/// 之后才签发，之后每条业务请求都要出示。凭据 → 握手时声明的 actor。
///
/// 只活在本进程内存里：iShell 重启即全部作废（实例 id 也随之换掉）。代理在找不到旧 id、
/// 且恰好只剩一个 token 匹配实例时会改绑到新实例并换一张凭据；多开时仍拒绝静默换绑。
/// 有界：按空闲时长过期，超过上限淘汰最久没用过的——代理被拒后会自动重新握手（见
/// `TICKET_REJECTED`），所以淘汰只多一次握手、不会让 AI 失联。
#[cfg(unix)]
mod tickets {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// 空闲这么久没用过的凭据作废。远长于一次 AI 会话里的调用间隔。
    const IDLE_EXPIRY: Duration = Duration::from_secs(7 * 24 * 3600);
    /// 存量上限：每次代理探测/重连都会握手签发一张，不设上限会随运行时长无界增长。
    const CAP: usize = 4096;

    struct Entry {
        actor: String,
        last_used: Instant,
    }

    static STORE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

    /// 为握手通过的一方签发凭据，绑定它声明的 actor。熵源不可用返回 `None`（宁可握手失败，
    /// 也不签一张可预测的凭据）。
    pub(super) fn issue(actor: String) -> Option<String> {
        // 两个 32 字节随机数拼起来：512 位，远超猜测可能。
        let ticket = format!(
            "{}{}",
            crate::mcp_protocol::pair_nonce()?,
            crate::mcp_protocol::pair_nonce()?
        );
        let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        map.retain(|_, e| now.duration_since(e.last_used) < IDLE_EXPIRY);
        if map.len() >= CAP {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(ticket.clone(), Entry { actor, last_used: now });
        Some(ticket)
    }

    /// 核对凭据：有效则刷新使用时刻并返回它绑定的 actor。
    pub(super) fn redeem(ticket: &str) -> Option<String> {
        let mut guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard.as_mut()?.get_mut(ticket)?;
        let now = Instant::now();
        if now.duration_since(entry.last_used) >= IDLE_EXPIRY {
            return None;
        }
        entry.last_used = now;
        Some(entry.actor.clone())
    }
}

/// 调用方上传字节流的包装：本身只是透传读取，被 drop 时顺带丢弃 `_drop_tx`，让持有对应
/// `oneshot::Receiver` 的一方知道「再没有别的读取者了」（见 `handle_conn` 的排干逻辑）。
#[cfg(unix)]
struct NotifyOnDrop<R> {
    inner: R,
    _drop_tx: oneshot::Sender<()>,
}

#[cfg(unix)]
impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for NotifyOnDrop<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Identify / 握手应答共用：带上本机主机名（v7），旧代理会忽略未知字段。
#[cfg(unix)]
fn instance_hello(ticket: String) -> McpReqResult {
    McpReqResult::Instance {
        id: crate::store::mcp_instance_id().to_string(),
        proto_version: crate::mcp_protocol::MCP_PROTOCOL_VERSION,
        token: String::new(),
        ticket,
        host: crate::mcp_protocol::local_hostname(),
    }
}

/// 一条连接只处理一问一答：读一行 JSON 请求，转发进 mpsc，等 App 帧循环回填后写一行 JSON 响应。
/// `handshake_permit` 覆盖读首行与 Identify/握手；业务请求随后换成 `work_limit` 里的名额，
/// 好让卡住的上传不堵住新的发现探测。
#[cfg(unix)]
async fn handle_conn(
    stream: UnixStream,
    tx: mpsc::UnboundedSender<McpCall>,
    ctx: egui::Context,
    handshake_permit: tokio::sync::OwnedSemaphorePermit,
    work_limit: Arc<tokio::sync::Semaphore>,
) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r.take(MAX_MCP_LINE_BYTES)).lines();
    let Ok(Ok(Some(line))) = tokio::time::timeout(FIRST_LINE_TIMEOUT, lines.next_line()).await
    else {
        return; // 对端没写完就断了连接/超时/超长，没东西可读也没法回应
    };
    let mut req: McpRequest = match serde_json::from_str(&line) {
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
                 绑定的那个 iShell 可能已经退出或重启；代理会在只剩一个匹配实例时改绑，\
                 多开时仍不会静默换到别的窗口——命令落到你没预期的机器上更糟"
                .to_string()),
        )
        .await;
        return;
    }
    // Identify 在连接层就地回答：它只是「你是谁」，不碰任何会话状态，没必要绕一趟 App
    // 帧循环。代理发现多个实例时会向每一个都问一次，让这条路尽量轻。
    // Identify / IdentifyPair：连接层就地回答，不进 App 帧循环。
    // v3 起 Instance.token 恒为空——真实配对走双向挑战-应答让调用方出示证明，
    // 绝不在响应里回传（反向转发后同账号他人也能连上 socket 发 Identify）。
    //
    // v5 起匿名发现的语义（与代理侧「无 token 直接拒绝绑定」配套，见 MCP_PROTOCOL_VERSION
    // 的版本志）：
    // - 匿名 `Identify` 照常应答，当**版本信标**用：它是全协议唯一跨版本可解析的请求，
    //   旧代理的「版本不符，请重新部署」提示全靠它问出版本号。应答不构成泄露——实例 id
    //   本就不是秘密（`ishell-mcp` 的 `connect_bound` 注释写明并接受了这一点），真正的
    //   授权边界是绑定 consent 弹窗与配对握手，不在发现层。
    // - 匿名 `IdentifyPair`（v3 明文配对）只对 **token 正确**的调用方应答：v3 旧代理凭
    //   正确 token 仍能走到版本校验、打印「请重新部署」；token 不符的调用方零应答（v4 及
    //   以前 GUI 忽略 token 无条件应答，等于向同账号任何人确认「这里有一台 iShell」，
    //   v5 收紧）。配对握手（PairHello/PairProve）不受影响。
    match &req.kind {
        McpReqKind::Identify => {
            reply(
                &mut w,
                id,
                Ok(instance_hello(String::new())),
            )
            .await;
            return;
        }
        // v3 的旧代理才会发这个。v5 起**校验 token 才答话**（见上面块注释）：token 正确，
        // v3 代理才能走到自己的 `check_proto_version`、打印「请重新部署 ishell-mcp」；
        // token 不符则静默丢弃——调用方什么也学不到。
        McpReqKind::IdentifyPair { token } => {
            if *token != crate::store::mcp_pairing_token() {
                return; // 静默丢弃：probe 分类为 Dead，实例从候选集里消失
            }
            reply(
                &mut w,
                id,
                Ok(instance_hello(String::new())),
            )
            .await;
            return;
        }
        // 配对握手（v4）：本进程先出示 `Server` 证明，对端验过再送 `Client` 证明上来。
        // 全协议唯一一处「一条连接两问两答」——双向证明必须共用同一对随机数。
        McpReqKind::PairHello { nonce_c } => {
            let nonce_c = nonce_c.clone();
            // 握手时声明的进程身份：握手通过后绑到签发的凭据上，此后以它为准（见 `tickets`）。
            let hello_actor = req.actor.clone();
            let token = crate::store::mcp_pairing_token();
            // 熵源失败一律拒绝握手，绝不用可预测的值凑合（那等于把挑战-应答降级成静态口令）。
            let Some(nonce_s) = crate::mcp_protocol::pair_nonce() else {
                reply(&mut w, id, Err("本机熵源不可用，无法完成配对握手".into())).await;
                return;
            };
            reply(
                &mut w,
                id,
                Ok(McpReqResult::PairChallenge {
                    id: own.to_string(),
                    proto_version: crate::mcp_protocol::MCP_PROTOCOL_VERSION,
                    nonce_s: nonce_s.clone(),
                    server_proof: crate::mcp_protocol::pair_proof(
                        &token,
                        &nonce_c,
                        &nonce_s,
                        crate::mcp_protocol::PairRole::Server,
                    ),
                }),
            )
            .await;
            // 第二行必须带超时：对端开了个头就赖着不说话的话，这条连接会一直占着握手
            // 信号量里的名额（`handshake_permit`），几条半开握手就能把发现层堵死。
            let second = tokio::time::timeout(FIRST_LINE_TIMEOUT, lines.next_line()).await;
            let Ok(Ok(Some(line2))) = second else {
                return; // 超时/断开：没什么可回的，直接收工
            };
            let ok = match serde_json::from_str::<McpRequest>(&line2) {
                Ok(McpRequest {
                    id: id2,
                    kind: McpReqKind::PairProve { client_proof },
                    ..
                }) => {
                    let pass = crate::mcp_protocol::pair_proof_matches(
                        &token,
                        &nonce_c,
                        &nonce_s,
                        crate::mcp_protocol::PairRole::Client,
                        &client_proof,
                    );
                    Some((id2, pass))
                }
                // 握手中途改发别的请求：不接受。握手没走完就不算配对成功，
                // 更不能让一条没证明过身份的连接顺势去执行别的东西。
                _ => None,
            };
            match ok {
                Some((id2, true)) => {
                    // 验过了才签发凭据。没声明 actor 的握手不签：凭据要绑一个身份，否则 iShell
                    // 没法把「窗口归开它的那个 AI」落到实处。
                    let Some(actor) = hello_actor else {
                        reply(&mut w, id2, Err("配对握手缺少 actor（代理进程标识），不签发连接凭据".into())).await;
                        return;
                    };
                    let Some(ticket) = tickets::issue(actor) else {
                        reply(&mut w, id2, Err("本机熵源不可用，无法签发连接凭据".into())).await;
                        return;
                    };
                    reply(
                        &mut w,
                        id2,
                        Ok(instance_hello(ticket)),
                    )
                    .await;
                }
                Some((id2, false)) => {
                    reply(&mut w, id2, Err("配对证明不符".into())).await;
                }
                None => {
                    reply(&mut w, 0, Err("配对握手第二步应当是 PairProve".into())).await;
                }
            }
            return;
        }
        // 没先握手就直接出示证明：无从校验（这一步必须用本连接第一步约定的那对随机数），拒绝。
        McpReqKind::PairProve { .. } => {
            reply(&mut w, id, Err("配对握手未开始（应先发 PairHello）".into())).await;
            return;
        }
        _ => {}
    }
    // 业务请求换到工作信号量：Identify/握手已在上面 return，不再占握手名额。
    // 上传卡死时新的发现探测仍能进来。
    let Ok(_work) = work_limit.try_acquire_owned() else {
        reply(
            &mut w,
            id,
            Err("iShell MCP 并发已达上限，请稍后重试".into()),
        )
        .await;
        return;
    };
    drop(handshake_permit);
    // **iShell 端身份校验（v6）**：除上面的发现与握手，其余每条请求都必须出示本进程签发过的
    // 有效连接凭据。这是「严禁操作其他人的客户端」的执行点，而且放在权威侧——此前这里只核对
    // 公开的实例 id，谁拿到 id 谁就能驱动这台 iShell，隔离全靠代理自觉。
    // 凭据绑定的 actor 覆盖请求里自报的值：归属判定（只能碰自己开的会话）用的是 iShell 自己
    // 记下的身份，冒用别人的 actor 没有用。
    match req.ticket.as_deref().and_then(tickets::redeem) {
        Some(actor) => req.actor = Some(actor),
        None => {
            reply(
                &mut w,
                id,
                Err(format!(
                    "{}：这台 iShell 只接受完成配对握手的请求（需要与它配对 token 一致的 \
                     ishell-mcp，协议 v{}）。代理会自动重新握手；仍失败请重新连接 MCP",
                    crate::mcp_protocol::TICKET_REJECTED,
                    crate::mcp_protocol::MCP_PROTOCOL_VERSION
                )),
            )
            .await;
            // 上传请求：调用方此刻多半还在推文件体。直接关连接，它后续的写会撞上 RST，尚未
            // 读出的这条错误随 RST 被内核丢掉，只剩一句 Broken pipe——代理就认不出「凭据被拒」、
            // 也就不会自动重新握手。先把残留字节读到 EOF 再关（与下方上传报错路径同一个道理），
            // 带兜底时限。
            if matches!(req.kind, McpReqKind::CopyToRemoteFromCaller { .. }) {
                let mut rest = lines.into_inner().into_inner().into_inner();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    tokio::io::copy(&mut rest, &mut tokio::io::sink()),
                )
                .await;
            }
            return;
        }
    }
    let is_caller_upload = matches!(&req.kind, McpReqKind::CopyToRemoteFromCaller { .. });
    if is_caller_upload {
        // 这里必须把 `Take` 摘掉。`MAX_MCP_LINE_BYTES` 要限的是**请求行**，而文件字节紧跟在
        // 那行 JSON 之后走同一个 reader——`Take` 套在整条连接上，于是这个"请求行上限"把
        // 文件流也一起限死了：超过 32 MiB 的 `copy_to_remote` 必失败，报的还是「调用方文件流
        // 提前结束」或裸 Broken pipe，跟真正的原因毫无关系。实测的分界线甚至不是整数，而是
        // 32 MiB 减去那行请求 JSON 的长度（会随远端路径长短浮动）。
        // 请求行此刻已经读完，上限该起的作用已经起过了；上传本身是分块读写、内存有界，
        // 不需要长度上限。
        //
        // `Lines` 持有的 BufReader 可能已经预读了紧随 JSON 行的文件字节，不能丢掉它——
        // 取出缓冲区里的首块，和摘掉 `Take` 后的裸读端接起来交给 worker。
        let mut buffered_reader = lines.into_inner();
        let head = buffered_reader.buffer().to_vec();
        let rest = buffered_reader.into_inner().into_inner();
        // dup 一份 socket 句柄，留给下面「报错后排干文件体」用（理由见该处注释）。tokio 的
        // UnixStream 没有 try_clone，用 libc::dup 复制 fd 再包回——原句柄来自 tokio accept、
        // 已是 non-blocking，dup 与它共享同一 open file description。失败不致命，只是不排干。
        let drain_dup = unsafe {
            let fd = libc::dup(rest.as_ref().as_raw_fd());
            (fd >= 0)
                .then(|| std::os::unix::net::UnixStream::from_raw_fd(fd))
                .and_then(|std_stream| tokio::net::UnixStream::from_std(std_stream).ok())
        };
        // source 被释放（App 拒绝时随 McpCall 丢弃；worker 读完/失败后随上传函数返回丢弃）
        // 的那一刻 `source_dropped` 就绪——排干必须等到这之后，见下方注释。
        let (drop_tx, source_dropped) = oneshot::channel::<()>();
        let source = NotifyOnDrop {
            inner: tokio::io::AsyncReadExt::chain(std::io::Cursor::new(head), rest),
            _drop_tx: drop_tx,
        };
        let upload_source = Some(Box::new(source) as Box<dyn tokio::io::AsyncRead + Send + Unpin>);
        let (resp_tx, resp_rx) = oneshot::channel();
        if tx.send(McpCall { req, resp_tx, upload_source, download_sink: None }).is_err() {
            return;
        }
        ctx.request_repaint();
        let resp = resp_rx.await.unwrap_or(McpResponse {
            id,
            result: Err("iShell 未能处理该请求（可能已关闭）".into()),
        });
        let is_err = resp.result.is_err();
        if let Ok(mut json) = serde_json::to_string(&resp) {
            json.push('\n');
            let _ = w.write_all(json.as_bytes()).await;
        }
        // App 校验拒绝（会话不存在/路径非法/并发满/未授权）或传输中途失败时，调用方此刻
        // 可能还在推文件体。直接返回关连接的话，它后续的数据段撞上已关闭的 socket 会收到
        // RST，而它尚未读出的错误响应会被内核随 RST 从接收队列里丢弃——最终只报一句
        // Broken pipe，真实错误彻底丢失（实测 8MB 文件必现；100KB 因 body 一次写完不受影响）。
        // 所以先把残留在管道里的字节读到 EOF 再关：新代理读到错误会立即停写断开，EOF 来得
        // 很快；旧代理会把整个 body 推完。
        //
        // **必须等 source 被释放后才排干**：报错不等于 worker 已停——文件操作超时
        // （`check_file_op_timeouts`）到点就回 Err，而 worker 的上传无法取消、仍在读 source。
        // 此时排干就是两个读取者抢同一路字节流（worker 必然「提前结束」失败，本可能成功的
        // 上传被搅黄）。source 释放 = 再没有别的读取者，排干才安全。整段共用一个兜底时限，
        // 等不到释放就放弃排干、退回旧行为。
        if is_err {
            if let Some(mut dr) = drain_dup {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                    let _ = source_dropped.await; // 发送端被丢弃即就绪（Err(Canceled)）
                    let _ = tokio::io::copy(&mut dr, &mut tokio::io::sink()).await;
                })
                .await;
            }
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
            // 自动回收要在借用 pending 之前判定：哨兵被交互程序吃掉后运行永远完不成，AI 又
            // 弃疗不再 poll——这条运行会把「一个会话只允许一条挂起」的闸门**永久**占死，
            // 后续命令全被「已有一条」拒绝且毫无解释。判据见 [`run_is_abandoned`]。
            let abandoned = s.pending_ai_run.as_ref().is_some_and(|p| {
                p.finished_result.is_none()
                    && run_is_abandoned(
                        p.last_poll_at.elapsed(),
                        s.terminal.output_idle_for(RECLAIM_IDLE),
                        p.resp_tx.as_ref().is_some_and(|tx| !tx.is_closed()),
                    )
            });
            if abandoned {
                s.discard_pending_ai_run(
                    "运行长时间无输出、也无人续等，已自动回收（完成信号很可能被命令或前台程序\
                     当作输入吃掉——交互式命令常见）：会话闸门已释放，可以发新命令。这条运行的\
                     退出码已不可恢复，即使命令后来跑完了也拿不回来；要确认它的实际状态用 \
                     read_screen/read_history。长时间静默但确实在跑的命令（如 sleep/等待型任务）\
                     请用 poll_run 续等，轮询本身就会阻止回收",
                );
            }
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
            // 下面的投递要清/换 pending_ai_run 本体，改为 take-改-放回的所有权结构：
            // as_mut 借用跨分支存活会被借用检查器拒（完成分支要清空、超时分支要改 deadline）。
            // 不用 let-else：循环 + 条件移动组合会触发 rustc 的再初始化分析误报（E0382）。
            let mut pending = match s.pending_ai_run.take() {
                Some(p) => p,
                None => continue,
            };
            match pending.finished_result.take() {
                Some((code, output)) => {
                    if let Some(tx) = pending.resp_tx.take() {
                        let _ = tx.send(McpResponse {
                            id: pending.req_id,
                            result: Ok(McpReqResult::Run(McpRunResult {
                                run_id: pending.run_id,
                                finished: true,
                                output,
                                exit_code: Some(code),
                            })),
                        });
                        // 已投递，整条移除（哨兵命中时捕获已自清）。
                    } else {
                        // 没有 waiter：把结果放回去缓存着，等下一次 poll_run 打进来再取走。
                        pending.finished_result = Some((code, output));
                        s.pending_ai_run = Some(pending);
                    }
                }
                None => {
                    if Instant::now() >= pending.deadline {
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
                        // 本次等待的耐心已兑现：把 deadline 拨回未来。否则「停在过去的
                        // deadline」会让 `arm_timeout_repaint` 每帧算出 request_repaint_after(0)
                        // ——GUI 全速空转（且每帧对捕获缓冲做一次 ANSI 剥离），直到哨兵到达/
                        // interrupt/poll_run 重新武装 deadline 为止。重绘心跳与数据事件照常
                        // 驱动帧，完成检测不会丢；poll_run 会按自己的 timeout_ms 重新武装。
                        pending.deadline = Instant::now() + std::time::Duration::from_secs(3600);
                    }
                    // 未完成：放回，等下一次 poll_run 续等（不重发命令）
                    s.pending_ai_run = Some(pending);
                }
            }
        }
        // open consent 超时（**App 级**，不属于任何会话）：整个 McpCall 被扣在这里，超时
        // 必须回一条错，否则调用方那边就是一条永远不回的请求。
        // 写法与下面的 use consent 对齐（`as_ref` + `take`），两处收尾方式一致，便于对读。
        if let Some(pending) = self.pending_open_consent.as_ref() {
            if Instant::now() >= pending.deadline {
                let pending = self.pending_open_consent.take().expect("上一行刚确认是 Some");
                if let Some(tx) = pending.resp_tx {
                    let _ = tx.send(McpResponse {
                        id: pending.req_id,
                        result: Err(
                            "等待用户确认超时（5 分钟），已自动拒绝：请让用户切回 iShell 点击\
                             确认弹窗，或直接重新调用 open_session 再次发起确认请求".into(),
                        ),
                    });
                }
            }
        }
        // 绑定弹窗的清扫比新开会话那个多一条「对端走了」：代理把 Bind 同时发给了每一个实例，
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
            self.pending_bind_consent.as_ref().map(|p| p.deadline),
        ];
        let sessions = self.sessions.iter().flat_map(|s| {
            s.pending_ai_run
                .as_ref()
                .into_iter()
                // 两个时刻都要排重绘：本次等待的耐心（deadline），以及自动回收的静默期
                // （last_poll_at + RECLAIM_IDLE）——漏掉后者，卡死的闸门要等某个不相干的
                // 事件偶然唤起一帧才解得开。
                .flat_map(|p| [p.deadline, p.last_poll_at + RECLAIM_IDLE])
                .chain(s.pending_file_ops.iter().map(|op| op.deadline))
                // 「重连后恢复 cwd」的截止时刻：到点要有一帧把过期的意图清掉
                // （`expire_cwd_restore_intents`），否则它会一直挂到某个不相干的事件
                // 偶然唤起一帧为止。
                .chain(s.restore_cwd_until)
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
                        result: Err(
                            "文件操作超时（worker 未在超时前返回结果）。注意：写入/复制类操作\
                             无法取消、可能已经在远端生效——重试覆盖同一文件前，先用 read_file \
                             或到远端核实"
                                .into(),
                        ),
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
        temp_key_untrusted: Vec<(u64, bool, String)>,
        direct_relay_started: Vec<u64>,
        direct_relay_done: Vec<(u64, bool, String)>,
        relay_source: Vec<(u64, Result<u64, String>)>,
        copy_done: Vec<(u64, u64, bool, String)>,
    ) {
        for (op_id, ok, message) in temp_key_trusted {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                // 作业刚因总超时/失败被移除、回执才到：总超时分支已按「TrustingB 在途窗口」
                // 补发过撤销，这里属良性迟到——但留一行日志，排查残留时才知道发生过什么。
                log::debug!("TempKeyTrusted 迟到：op {op_id} 的作业已不存在");
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
        for (op_id, ok, message) in temp_key_untrusted {
            let Some(idx) = self.cross_copy_jobs.iter().position(|j| j.op_id == op_id) else {
                continue;
            };
            // 撤销失败**不改变**这次拷贝的成败（`direct_result` 早就定了），但绝不能咽下去：
            // 目标机的 `~/.ssh/authorized_keys` 里可能留着一把带 `restrict` 的临时公钥，
            // 而用户完全不知道该去清——交给 `warn_temp_key_residue` 落日志 + 摆到状态栏/toast。
            if !ok {
                let dest_uid = self.cross_copy_jobs[idx].dest_uid;
                let marker = self.cross_copy_jobs[idx].marker.clone();
                self.warn_temp_key_residue(dest_uid, marker, &format!("撤销失败（{message}）"));
            }
            // 回执确认公钥已删：把 trust_established 清回去。否则作业进入中转/后续阶段后
            // 它仍为真，快败与总超时分支会再补发一次撤销——目标会话恰好不可达时，一条
            // 其实早已删掉的 key 会触发「请手工删除 authorized_keys」的误报。
            if ok {
                self.cross_copy_jobs[idx].trust_established = false;
            }
            if matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::UntrustingAfterDirect) {
                self.finish_after_untrust(idx, true);
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
        // 参与会话被关闭或断线的作业立即判败：底层 scp/管道已随连接死掉，等它自愈没有意义
        // （断线重连是全新 worker，作业持有的旧 cmd_tx/传输不可能复活）——不检查的话作业
        // 会一直挂到总超时（最长 24h）才以一句笼统的「超时」收场。
        // 刻意放在事件循环**之后**：本帧到达的成功事件（直连完成/中转 TransferDone）会先把
        // 作业收掉，不会因为同一帧里恰好有断线而把一次成功的拷贝报成失败。
        for idx in (0..self.cross_copy_jobs.len()).rev() {
            // UntrustingAfterDirect 的成败早已确定（direct_result），只是等撤销回执，断线不构成
            // 判败理由（它有自己的 phase_deadline 收尾）。但**目标**断线时回执永远不会来了：
            // 撤销是断线前发的，旧 worker 未必处理到——等 phase_deadline 收尾时一声不吭，
            // 公钥就可能静默残留。这里当场告警，并把 trust_established 清掉（已告警、也已
            // 无通道可补发），避免快败/总超时分支之后再对同一把 key 重复告警。
            if matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::UntrustingAfterDirect) {
                let (dest_uid, marker, trusted) = {
                    let job = &self.cross_copy_jobs[idx];
                    (job.dest_uid, job.marker.clone(), job.trust_established)
                };
                let dest_connected =
                    self.session_idx_by_uid(dest_uid).map(|i| self.sessions[i].connected);
                if trusted && untrust_route(dest_connected).is_err() {
                    self.warn_temp_key_residue(dest_uid, marker, "撤销回执未到，目标会话已断线");
                    self.cross_copy_jobs[idx].trust_established = false;
                }
                continue;
            }
            let dead = {
                let job = &self.cross_copy_jobs[idx];
                [("源", job.src_uid), ("目标", job.dest_uid)]
                    .into_iter()
                    .find_map(|(role, uid)| match self.session_idx_by_uid(uid) {
                        None => Some(format!("{role}会话已不存在（uid={uid}），跨会话拷贝中止")),
                        Some(i) if !self.sessions[i].connected => {
                            Some(format!("{role}会话已断线（uid={uid}），跨会话拷贝中止——重连后请重试"))
                        }
                        Some(_) => None,
                    })
            };
            if let Some(msg) = dead {
                let (cancel, dest_uid, op_id, marker, maybe_trusted) = {
                    let job = &self.cross_copy_jobs[idx];
                    (
                        job.cancel.clone(),
                        job.dest_uid,
                        job.op_id,
                        job.marker.clone(),
                        job.trust_established || matches!(job.phase, CrossCopyPhase::TrustingB),
                    )
                };
                // 直连传输已在进行：先置取消标志——直连循环只看这个 Arc<AtomicBool>，
                // CancelTransfer 管不到它；不置位的话源 worker 的 scp/rsync 会照跑完，
                // AI 据失败重试就是两个进程并发写同一目标文件。
                if matches!(self.cross_copy_jobs[idx].phase, CrossCopyPhase::DirectCopying { .. }) {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // TrustingB 在途窗口（信任已发给 worker、回执未到）里判败必须先补发撤销——
                // worker 可能在我们断线前已经把公钥写进了 authorized_keys。断的若是目标会话，
                // 撤销送不达（重连换新通道，旧 worker 不再处理命令），`best_effort_untrust`
                // 会直接告警而不是假装发出去了。注意 DirectCopying 进行中其实还没到
                // finish_direct_attempt（那里才发撤销），
                // 这里补发只会更早清理、不影响已建立的 scp 连接（认证只在建连时发生）；
                // UntrustingAfterDirect/Relay 阶段确认已发过——回执 ok 时上面已把
                // trust_established 清回 false，不会再走到这里。
                if maybe_trusted {
                    self.best_effort_untrust(dest_uid, op_id, marker);
                }
                self.fail_cross_copy_job(idx, msg);
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
                    self.finish_after_untrust(idx, false);
                } else {
                    // 直连传输已在进行：先把取消标志置上——job 随即移除，否则源 worker 的
                    // scp/rsync 会照跑完，AI 据「超时」重试就是两个进程并发写同一目标文件。
                    if matches!(job.phase, CrossCopyPhase::DirectCopying { .. }) {
                        job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // 「信任已确立」或「还在 TrustingB」（信任已发给 worker、回执未到的在途
                    // 窗口）都可能真的在目标机 authorized_keys 里留了临时公钥，必须补发撤销：
                    // 只按 trust_established 判定会漏掉在途窗口——总超时命中后 job 立即移除，
                    // 迟到的 TempKeyTrusted 找不到 job 被静默吞掉，公钥永久残留且无告警。
                    let need_untrust = {
                        let j = &self.cross_copy_jobs[idx];
                        j.trust_established || matches!(j.phase, CrossCopyPhase::TrustingB)
                    };
                    if need_untrust {
                        let (dest_uid, op_id, marker) = {
                            let j = &self.cross_copy_jobs[idx];
                            (j.dest_uid, j.op_id, j.marker.clone())
                        };
                        self.best_effort_untrust(dest_uid, op_id, marker);
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
                    // 只是撤销回执迟到，总超时还没到——时间预算还在，照常可以转中转。
                    self.finish_after_untrust(idx, true);
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
        self.best_effort_untrust(dest_uid, op_id, marker);
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
        // 发送失败不再静默：helper 内部会以状态栏/toast 告警（残留必须可见）。
        // 回执迟迟不来则由上面的 UNTRUST_WAIT 兜底收尾，与告警不冲突。
        self.best_effort_untrust(dest_uid, op_id, marker);
    }

    /// 撤销信任已完成（或等不到回执，超时放弃）：按之前确定的直连结果决定收尾——
    /// 成功则直接 resolve；失败则转入中转（这次真正把管道发给源/目标会话）。
    ///
    /// `allow_relay_fallback=false` 用于「总超时才走到这一步」的场合：此时已经没有时间预算
    /// 了，再起一次中转纯属白干——它下一帧就会被总超时判失败、管道读端随 job 一起 drop，
    /// 源会话那次 SFTP 打开注定作废。直连**成功**的情况仍然照常 resolve：结果早就确定了，
    /// 不该因为撤销回执迟到就把一次已经成功的拷贝误报成超时失败。
    fn finish_after_untrust(&mut self, idx: usize, allow_relay_fallback: bool) {
        match self.cross_copy_jobs[idx].direct_result.take() {
            Some(Ok(())) => self.resolve_cross_copy_job(idx, Ok(()), "direct"),
            Some(Err(e)) if allow_relay_fallback => {
                log::debug!("copy_between_sessions 直连失败，转中转：{e}");
                self.start_relay_fallback(idx)
            }
            Some(Err(e)) => self.fail_cross_copy_job(
                idx,
                format!("跨会话拷贝超时：直连未成功（{e}），且已无时间预算改走中转"),
            ),
            None => {
                // 不应该发生：`UntrustingAfterDirect` 只由 `finish_direct_attempt` 进入，
                // 而它必定先写好 direct_result。真到了这里就把 job 收掉，不能让它悬着
                // 永远占着两侧的 pending_file_op。
                self.fail_cross_copy_job(idx, "内部错误：直连结果丢失".into())
            }
        }
    }

    /// 尽力而为补发一次临时公钥撤销；送不达（见 [`untrust_route`]）或发送失败时走
    /// `warn_temp_key_residue` 告警（残留必须可见），调用方不再各自处理失败。
    fn best_effort_untrust(&mut self, dest_uid: u64, op_id: u64, marker: String) {
        let dest_idx = self.session_idx_by_uid(dest_uid);
        let route = untrust_route(dest_idx.map(|i| self.sessions[i].connected));
        let (Some(dest_idx), Ok(())) = (dest_idx, route) else {
            let reason = route.err().unwrap_or("目标会话已不存在，无法自动撤销");
            self.warn_temp_key_residue(dest_uid, marker, reason);
            return;
        };
        let sent = self.sessions[dest_idx]
            .cmd_tx
            .send(UiCommand::UntrustTempKey { op_id, marker: marker.clone() })
            .is_ok();
        if !sent {
            self.warn_temp_key_residue(dest_uid, marker, "撤销消息发送失败（目标会话后台连接已断开）");
        }
    }

    /// 临时公钥可能残留在目标机 authorized_keys 的告警：落日志 + 目标会话状态栏；会话已经不在
    /// （连状态栏都没了）时退化到全局 toast。原则：**残留必须对用户可见**，不能只剩一行日志。
    fn warn_temp_key_residue(&mut self, dest_uid: u64, marker: String, reason: &str) {
        log::warn!(
            "copy_between_sessions 临时公钥可能残留（目标 uid {dest_uid}，标记 {marker}）：{reason}"
        );
        let text = match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!(
                "⚠ 临时公钥可能残留在目标机上（{reason}）。请删除对应主机 ~/.ssh/authorized_keys 里含 {marker} 的那一行。"
            ),
            crate::i18n::Lang::En => format!(
                "⚠ The temporary key may remain on the destination host ({reason}). Remove the line containing {marker} from ~/.ssh/authorized_keys there."
            ),
        };
        match self.session_idx_by_uid(dest_uid) {
            Some(i) if self.sessions[i].connected => self.sessions[i].status = text,
            // 断线会话的状态栏随后会被「重连中 …」覆盖，只写它等于转瞬即逝——同时弹 toast。
            Some(i) => {
                self.sessions[i].status = text.clone();
                self.toast = Some((text, self.ctx.input(|i| i.time)));
            }
            None => self.toast = Some((text, self.ctx.input(|i| i.time))),
        }
    }

    /// 进程退出收尾（`on_exit` 调用）：所有「信任已确立」或「还在 TrustingB 在途窗口」的
    /// 跨会话拷贝作业，补发一次尽力而为的临时公钥撤销。on_exit 返回后进程随即退出、worker
    /// 随之死掉，这条命令未必来得及生效，所以同时落 warning 日志——authorized_keys 里的
    /// 标记注释是事后人工清理的唯一线索。
    pub(super) fn revoke_temp_keys_on_exit(&mut self) {
        for job in &self.cross_copy_jobs {
            let maybe_trusted =
                job.trust_established || matches!(job.phase, CrossCopyPhase::TrustingB);
            if !maybe_trusted {
                continue;
            }
            log::warn!(
                "退出时补发临时公钥撤销（尽力而为，可能来不及生效；目标 uid {}，标记 {}）",
                job.dest_uid,
                job.marker
            );
            if let Some(dest_idx) = self.session_idx_by_uid(job.dest_uid) {
                let _ = self.sessions[dest_idx].cmd_tx.send(UiCommand::UntrustTempKey {
                    op_id: job.op_id,
                    marker: job.marker.clone(),
                });
            }
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
        // 失败/超时收尾时必须**真的把还在跑的传输停掉**。此前只是移除 job 并回错误给调用方，
        // 两侧 worker 的 RelayReadFile/RelayWriteFile 照跑不误——于是出现「告诉 AI 超时了，
        // 远端文件其实写成功了」这种分歧，而 AI 多半会据此重试一次。
        //
        // 直连尝试早有 `cancel` 标志，中转这条路一直漏着；好在中转两侧都走 `start_xfer`、
        // 按同一个 op_id 注册进 worker 的 `xfer_cancels`，所以一条既有的 `CancelTransfer`
        // 就够，不需要新的协议。没有对应在跑的传输时它是空操作（早期失败路径常如此）。
        //
        // 只在失败路径发：成功时 TransferDone 已经到了，传输本就结束了。
        let cancel_inflight = result.is_err();
        for uid in [job.src_uid, job.dest_uid] {
            if let Some(sidx) = self.session_idx_by_uid(uid) {
                if cancel_inflight {
                    let _ = self.sessions[sidx]
                        .cmd_tx
                        .send(UiCommand::CancelTransfer(job.op_id));
                }
                // 保险起见清一次两侧为这次 op_id 占位的 pending_file_op：正常路径下
                // TransferDone 已经通过 try_resolve_file_copy 自然移除了；这里只处理
                // "某一侧因为提前失败/超时而从没走到那一步"的情况，避免占位项永久占着
                // 并发名额。
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
    fn do_open_session(
        &mut self,
        c: &SavedConnection,
        id: u64,
        resp_tx: oneshot::Sender<McpResponse>,
        owner: Option<String>,
        owner_label: Option<String>,
    ) {
        let cfg = match connect_config_from_saved(c) {
            Ok(cfg) => cfg,
            Err(msg) => {
                let _ = resp_tx.send(McpResponse {
                    id,
                    result: Err(msg),
                });
                return;
            }
        };
        // AI 开的会话要**静默**落在新标签：spawn_session 会把新标签置为活动并请求滚动过去，
        // 焦点被从用户正在交互的会话里拽走是最糟糕的打扰——先把当前活动标签记下来，开完恢复。
        let prev_active = self.active;
        self.spawn_session(cfg);
        self.active = prev_active;
        let s = self.sessions.last_mut().expect("spawn_session 刚 push 了一个会话");
        s.ai_owned = true; // AI 新开的会话：只读，用户键盘输入不转发（见 layout_body.rs）
        s.ai_owner = owner; // 归属：只归开它的那个 AI 进程使用（见 session_owned_by）
        // 标签 hover 上展示「谁开的」：渲染时由 layout_tabs::tab_hover_text 拼接（#uid 要插在
        // user@host 与来源之间，所以不再把来源预先拼进 tip）。
        s.ai_owner_label = owner_label.clone();
        let info = McpSessionInfo {
            uid: s.uid,
            title: s.title.clone(),
            host: s.cfg.host.clone(),
            connected: s.connected,
            cwd: s.terminal.cwd().map(|c| c.to_string()),
            ai_owned: s.ai_owned,
            ai_owner: s.ai_owner_label.clone(),
            mine: true, // 开它的就是发起者自己
            token_injected: s.mcp_token_injected,
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
            actor,
            origin,
            ..
        } = pending;
        let Some(resp_tx) = resp_tx else {
            return;
        };
        if allow {
            self.mcp_open_approved.insert(conn.name.clone());
            self.do_open_session(&conn, req_id, resp_tx, actor, origin);
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
    fn session_not_found_msg(&self, uid: u64, actor: Option<&str>) -> String {
        // 只列发起方自己的会话：别人的窗口连 uid/标题都不该透露（见 `session_owned_by`）。
        let list = self
            .sessions
            .iter()
            .filter(|s| session_owned_by(s.ai_owned, s.ai_owner.as_deref(), actor))
            .map(|s| format!("{}:{}", s.uid, s.title))
            .collect::<Vec<_>>();
        if list.is_empty() {
            return format!(
                "会话不存在（uid={uid}）：你当前没有自己开的会话（iShell 重启后 uid 会重新分配）。\
                 用 open_session 开一个"
            );
        }
        format!(
            "会话不存在（uid={uid}）：可能是 iShell 已重启（重启后 uid 会重新分配）或这个会话\
             已被关闭。你自己的会话（uid:标题）：{}",
            list.join(", ")
        )
    }

    /// 会话门禁：请求涉及的每一个会话都必须是**发起方自己开的**，否则整条直接拒绝。
    /// 放行则原样返回 `call`；拒绝（已回错）返回 `None`。
    ///
    /// # 这道闸门没有任何开关可以绕过，也没有授权弹窗
    ///
    /// 判据见 [`session_owned_by`]；覆盖范围见 `McpReqKind::session_target_uids`（读写都在内）。
    /// 任何设置都不影响它：`mcp_auto_approve` 只管「AI 新开会话」那一档，绝不能拿到这里来短路。
    fn gate_foreign_sessions(&mut self, call: McpCall) -> Option<McpCall> {
        let actor = call.req.actor.clone();
        let uids = call.req.kind.session_target_uids();
        let foreign = first_foreign_session(
            &uids,
            |uid| {
                self.session_idx_by_uid(uid).map(|idx| {
                    let s = &self.sessions[idx];
                    (s.ai_owned, s.ai_owner.as_deref())
                })
            },
            actor.as_deref(),
        );
        let Some(uid) = foreign else {
            return Some(call);
        };
        let id = call.req.id;
        let _ = call.resp_tx.send(McpResponse {
            id,
            result: Err(foreign_session_refusal(uid)),
        });
        None
    }

    fn handle_mcp_call(&mut self, call: McpCall) {
        // 设置里关掉 MCP 后立刻拒绝业务请求（含 Bind 弹窗）。监听 socket 仍要等重启才撤，
        // 但用户把开关拧掉的意图是「现在别再动我的终端」，不能再等一次重启才生效。
        if !crate::store::load_mcp_consent() {
            let id = call.req.id;
            let _ = call.resp_tx.send(McpResponse {
                id,
                result: Err(
                    "用户已在设置里关闭「允许 AI 通过 MCP 控制终端」，本次请求未执行。\
                     请在设置里重新打开该开关（本次启动时监听已在跑的话，不必重启 iShell）。"
                        .into(),
                ),
            });
            return;
        }
        // 先过会话门禁：只有「涉及的会话全是发起方自己开的」才继续。
        let Some(call) = self.gate_foreign_sessions(call) else {
            return;
        };
        let McpCall { req, resp_tx, upload_source, download_sink } = call;
        // 发起方代理的进程标识：归属判定（CloseSession）与 list_sessions 的 mine 标记都要
        // 用，先在这里快照——match req.kind 之后 req 就被部分移动了。
        let caller_actor = req.actor.clone();
        let id = req.id;
        let send_err = |resp_tx: oneshot::Sender<McpResponse>, msg: String| {
            let _ = resp_tx.send(McpResponse {
                id,
                result: Err(msg),
            });
        };
        match req.kind {
            // Identify / IdentifyPair / 配对握手都在连接层就地答完了（见 handle_conn），
            // 根本到不了这里。
            McpReqKind::Identify
            | McpReqKind::IdentifyPair { .. }
            | McpReqKind::PairHello { .. }
            | McpReqKind::PairProve { .. } => {
                send_err(resp_tx, "Identify/配对握手 不应该到达 App 层".into());
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
                    // 代理（≥某个版本起）会带上发起方的来源描述，弹窗原样展示给用户看。
                    // 旧代理不带：None，弹窗就不显示这一行。
                    origin: req.origin.clone(),
                    // 与另外两个确认框同样给 5 分钟：用户可能不在电脑前。
                    deadline: Instant::now() + Duration::from_secs(300),
                });
            }
            McpReqKind::ListSessions => {
                // 只列发起方自己开的会话：用户的窗口、其它 AI 的窗口连标题/主机/cwd 都不给看
                // （见 `session_owned_by`）。
                let list = self
                    .sessions
                    .iter()
                    .filter(|s| {
                        session_owned_by(s.ai_owned, s.ai_owner.as_deref(), caller_actor.as_deref())
                    })
                    .map(|s| McpSessionInfo {
                        uid: s.uid,
                        title: s.title.clone(),
                        host: s.cfg.host.clone(),
                        connected: s.connected,
                        cwd: s.terminal.cwd().map(|c| c.to_string()),
                        ai_owned: s.ai_owned,
                        // 归属展示：谁开的（可读标签）。旧代理开的窗口没有标签。
                        ai_owner: s.ai_owner_label.clone(),
                        // 列表里只剩自己的会话，恒为 true；字段保留是线格式兼容。
                        mine: true,
                        token_injected: s.mcp_token_injected,
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                if let Err(msg) = validate_run_command(&command) {
                    send_err(resp_tx, msg);
                    return;
                }
                // 注：这里**曾经**有「自动注入后 1 秒内拒发命令」的守卫——防 expect_echo 整体
                // 覆写注入吞除。回显吞除改为 FIFO 队列（terminal::EchoArm）后覆写不再发生：
                // 注入回显与哨兵回显按打字顺序各吞各的，守卫连同它的误拒一起退役。
                // exit 改写进子 shell（整条或命令链单独成段的一环，见 wrap_session_ender）：
                // 退出码照拿、会话不死。logout 已在上面的校验里拒绝。
                let sent_command = wrap_session_ender(&command);
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                // 完成检测有两条路，**优先 shell 集成**（见 `terminal::CaptureMode`）：
                //
                // - 集成可用（本会话见过 OSC 133 = AI 会话的集成片段已注入生效）：只发命令
                //   本身，一个字节的哨兵都不打。开始/结束/退出码由 shell 自己在执行前与打印
                //   提示符时发出，`cat`/REPL 吃不掉、interrupt 不留残渣、也没有哨兵回显要吞。
                // - 集成不可用（fish/csh、片段还没注入、用户会话）：回退到哨兵。它脆弱的地方
                //   一条没少（命令形态吞掉哨兵、回显吞除失配……），所以只在这一分支里还活着。
                //
                // 哨兵前缀分两个形态：捕获前缀以**真实** 0x1E 开头，哨兵输出因此不可能与
                // 「标记行的打字回显」撞车（回显里是字面 `\x1e` 四个字符）；打字文本里保持字面
                // 4 字符、交给远端 printf 转成真实控制字节——若直接嵌原始控制字节，ECHOCTL 会
                // 把它渲染成 `^^` 导致回显吞除逐字节失配（程序输出不走按键回显路径，不受影响）。
                let marker = (!s.terminal.shell_integration_active()).then(|| {
                    let capture_prefix = format!("\x1eAI_DONE_{nonce}:");
                    let typed_prefix = capture_prefix.replace('\x1e', "\\x1e");
                    // 标记行自身回显用 expect_echo 吞掉，其打印出的哨兵再用 \r\x1b[K 自擦除。
                    let typed = format!("printf '{typed_prefix}%d\\x1e' $?; printf '\\r\\x1b[K'");
                    (capture_prefix, typed)
                });
                // worker 可能刚退出（Disconnected 事件这一帧还没被处理，s.connected 仍是
                // 上一帧的旧值）——cmd_tx 是 unbounded channel，send 只在接收端已经掉了才会
                // 失败。哨兵模式下两条输入分两次 send，不是原子的：命令那条可能已经送达并
                // 可能已经产生副作用，标记那条却失败——这种情况下不能假装"什么都没发生"，
                // 必须明确告诉调用方"命令可能已执行、结果未知"。（集成模式没有第二条 send，
                // 这个半成功窗口自然也就不存在了。）
                let command_sent = s
                    .cmd_tx
                    .send(UiCommand::TerminalInput(format!("{sent_command}\r").into_bytes()))
                    .is_ok();
                if !command_sent {
                    send_err(resp_tx, "会话的后台连接似乎已经断开，命令未发送，请稍后重试".into());
                    return;
                }
                match &marker {
                    Some((capture_prefix, typed)) => {
                        let marker_sent = s
                            .cmd_tx
                            .send(UiCommand::TerminalInput(format!("{typed}\r").into_bytes()))
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
                        s.terminal.expect_echo(typed);
                        s.terminal.arm_ai_capture(capture_prefix.clone().into_bytes());
                    }
                    // 集成模式：命令已经发出去了，此外什么都不用打。
                    None => s.terminal.arm_ai_capture_integration(),
                }
                let born = Instant::now();
                s.pending_ai_run = Some(PendingAiRun {
                    run_id: nonce as u64,
                    deadline: born + clamp_timeout(timeout_ms),
                    last_poll_at: born,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    // 存**实际打进终端的**那一版：`trim_leading_echo` 拿它和回显做前缀匹配，
                    // 存原文的话 `exit 42` 对不上回显里的 `(exit 42)`，哨兵模式下输出开头
                    // 会多出一行命令回显。
                    command: sent_command.into_owned(),
                    finished_result: None,
                });
            }
            McpReqKind::PollRun {
                session_uid,
                run_id,
                timeout_ms,
            } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                        p.last_poll_at = Instant::now(); // 有人在等 = 这条运行没被放弃
                        p.resp_tx = Some(resp_tx);
                        p.req_id = id;
                    }
                    // run_id 对不上 ≠ 运行结束：会话上挂着的是**另一条**运行（还在执行，
                    // 或结果待取回）——措辞必须区分，否则 AI 会以为这条运行已结束而放弃续等。
                    Some(_) => send_err(
                        resp_tx,
                        "run_id 对不上：该会话上当前是另一条运行（可能仍在执行，或结果待取回）。\
                         从最新上下文核对 run_id；要释放该会话的运行，调用 interrupt"
                            .into(),
                    ),
                    None => send_err(
                        resp_tx,
                        "run_id 不存在或已结束：结果可能已被之前的 poll_run 取走，或运行被 \
                         interrupt/断线作废。要确认命令最终状态，用 read_screen/read_history 核实"
                            .into(),
                    ),
                }
            }
            McpReqKind::ReadScreen { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                // 用 discard 而不是 cancel：interrupt 的语义就是「把闸门还回来」，连已缓存
                // 的结果一起丢——保留的话 pending 还占着闸门，调用方收到「已中断」却发不出
                // 下一条命令。
                s.discard_pending_ai_run("命令已被 interrupt 中断");
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
                // 已批准过、或「AI 操作无需逐次确认」开着（默认）→ 直接开。
                if self.mcp_open_approved.contains(&name) || crate::store::load_mcp_auto_approve() {
                    self.mcp_open_approved.insert(name);
                    self.do_open_session(&c, id, resp_tx, req.actor, req.origin);
                    return;
                }
                // 同一时刻只挂一个确认框：两个 modal 叠在一起，用户根本分不清自己在批准哪一个。
                if self.pending_open_consent.is_some() {
                    send_err(resp_tx, "已有一个请求正在等待用户确认，请稍候重试".into());
                    return;
                }
                // 首次用这条已保存连接给 AI 开会话：弹窗等用户当面批准，而不是仅凭 AI 传的
                // 名字字符串就信任（见 App::handle_ai_open_consent，在 dialogs.rs 渲染）。
                self.pending_open_consent = Some(PendingOpenConsent {
                    conn: c,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    actor: req.actor,
                    origin: req.origin,
                    // 60s 对"用户可能不在电脑前"这种常见情况太紧——超时会被下面的
                    // 拒绝分支吃掉,还得让 AI 重新发起一次 open_session 才能再弹一次
                    // 确认框。放宽到 5 分钟,给用户更充裕的反应时间。
                    deadline: Instant::now() + Duration::from_secs(300),
                });
            }
            McpReqKind::CloseSession { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
                    return;
                };
                // 只允许关「自己（这个 AI 进程）」开的会话：不能关用户的，也不能关另一个
                // AI 开的——关闭权限不超过归属权（归属见 session_owned_by）。
                let s = &self.sessions[idx];
                let can_close = s.ai_owned
                    && match (&s.ai_owner, caller_actor.as_deref()) {
                        // 旧版共享池窗口：维持旧行为——只有不带 actor 的旧代理能关。
                        (None, None) => true,
                        (None, Some(_)) => false,
                        (Some(owner), actor) => actor == Some(owner.as_str()),
                    };
                if !can_close {
                    let msg = if !s.ai_owned {
                        "这不是 AI 自己开的会话，不能通过这个工具关闭".to_string()
                    } else {
                        match &s.ai_owner_label {
                            Some(label) => format!(
                                "这是另一个 AI（{label}）开的会话，只能由它自己关闭"
                            ),
                            None => "这不是你这个 AI 开的会话，不能关闭".to_string(),
                        }
                    };
                    send_err(resp_tx, msg);
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(src_session_uid, caller_actor.as_deref()));
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
                    send_err(resp_tx, self.session_not_found_msg(dest_session_uid, caller_actor.as_deref()));
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
mod timeout_chokepoint_tests {
    /// **所有超时判定都是每帧轮询的**，而 egui 按需重绘：没人给「最近的那个 deadline」排一次
    /// 定时重绘，空闲窗口里它到点也不会被求值——调用方那边就是一条永远不回的请求。
    /// `arm_timeout_repaint` 是这件事唯一的收口点，它自己的注释也承诺「新加的超时只要挂在
    /// 这些字段上就自动被覆盖」。这条测试把那句承诺变成门禁。
    ///
    /// **覆盖面写明白，别当成全面保障**：只扫**本文件**里形如 `<名字>: Instant,` 的字段
    /// 声明——`Option<Instant>`、结构体最后一个不带逗号的字段、以及别的文件（如 `session.rs`）
    /// 里的截止时刻都不在范围内。它挡的是「照着现有字段照抄一个新的超时，却忘了排重绘」
    /// 这一种最常见的漏法；换个写法加超时仍然要靠人。
    ///
    /// 反向对照：把 `p.last_poll_at + RECLAIM_IDLE` 从 `arm_timeout_repaint` 里删掉，本条
    /// 当场挂——那正是「自动回收要等某个不相干的事件偶然唤起一帧」的漏法（历史上
    /// `abs_deadline` 就是这么漏的）。
    #[test]
    fn every_instant_field_is_reachable_from_arm_timeout_repaint() {
        let src = include_str!("mcp_bridge.rs");
        let body = {
            let at = src
                .find("pub(super) fn arm_timeout_repaint")
                .expect("收口函数改名了？同步改这条测试");
            let rest = &src[at..];
            // 函数体到下一个顶层 `    }` 为止（本文件里 impl 内的函数都是 4 空格缩进）。
            let end = rest.find("\n    }\n").expect("找不到函数体结尾");
            // **必须剔掉注释行**：否则函数体里一句提到字段名的说明就能让这条门禁误判通过
            // （第一版就是这么漏的：注释里写着 `last_poll_at + RECLAIM_IDLE`，把它从代码里
            // 删掉测试照样绿）。
            rest[..end]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // 显式豁免：**不是**帧循环轮询的截止时刻、到点也不需要谁来处理的 `Instant` 字段。
        // 每加一条都要写明理由——这份清单就是门禁的「逃生口」，不写理由等于把门拆了。
        // - `last_used`（`tickets::Entry`）：连接凭据的最近使用时刻。过期是在下一次签发/核对时
        //   顺手判断的（惰性），过期那一刻没有任何人在等结果，不需要排重绘。
        const NOT_FRAME_POLLED: &[&str] = &["last_used"];
        let mut missing: Vec<&str> = Vec::new();
        for line in src.lines() {
            let line = line.trim();
            // 同理：字段声明的扫描也只看代码行
            // 形如 `deadline: Instant,` 的字段声明（跳过注释与函数签名里的参数）
            let Some(name) = line.strip_suffix(": Instant,") else {
                continue;
            };
            if line.starts_with("//") || name.contains(' ') || name.contains('(') {
                continue;
            }
            if !body.contains(name) && !NOT_FRAME_POLLED.contains(&name) {
                missing.push(name);
            }
        }
        assert!(
            missing.is_empty(),
            "这些超时字段没被 arm_timeout_repaint 覆盖，空闲窗口里它们到点不会被求值：{missing:?}"
        );
    }
}

#[cfg(test)]
mod run_command_validation_tests {
    use super::validate_run_command;

    /// 命令必须是单行非空。多行的后果见 `validate_run_command` 的说明——实测（bash 5.1）
    /// `echo one\necho two` 会产生**两对** OSC 133 C/D，集成模式在第一对就收束，调用方拿到
    /// 的是「第一行的退出码 + 第一行的输出 + finished=true」，剩下的还在跑。
    /// 反向对照：把 `contains('\n')` 那条去掉，第三、四条断言当场挂。
    #[test]
    fn only_a_single_non_empty_line_is_accepted() {
        assert!(validate_run_command("ls -l").is_ok());
        assert!(validate_run_command("cd /tmp && ls; echo done").is_ok(), "单行里的 ; && 照常");
        assert!(validate_run_command("echo one\necho two").is_err());
        assert!(validate_run_command("cat <<EOF\nbody\nEOF").is_err(), "heredoc 也是多行");
        assert!(validate_run_command("ls\r").is_err(), "裸回车同样是「按下 Enter」");
        assert!(validate_run_command("").is_err());
        assert!(validate_run_command("   \t ").is_err(), "只有空白等于空命令");
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::{run_is_abandoned, RECLAIM_IDLE};
    use std::time::Duration;

    /// 自动回收只该打中「闸门被占死」那一种状态：没人等 + 没输出 + 很久没人 poll。
    /// 反向对照：把判据换回「按声明超时推算绝对期限」（历史实现），第二条断言当场挂——
    /// `start_command` 的声明超时是协议最小值，正常长命令几分钟内就会被误杀。
    #[test]
    fn only_an_abandoned_run_is_reclaimed() {
        let long = RECLAIM_IDLE + Duration::from_secs(1);
        assert!(run_is_abandoned(long, true, false), "没人等、无输出、久未轮询：该回收");
        assert!(
            !run_is_abandoned(long, false, false),
            "终端还在输出（构建/测试在跑）：命令活着，不能回收"
        );
        assert!(
            !run_is_abandoned(long, true, true),
            "有等待者挂着：有人在意这条运行，不能回收"
        );
        assert!(
            !run_is_abandoned(Duration::from_secs(1), true, false),
            "刚轮询过：AI 还在跟进，不能回收"
        );
    }
}

#[cfg(test)]
mod untrust_route_tests {
    use super::untrust_route;

    /// 目标会话断线时撤销必须判「送不达」，让调用方直接告警。
    /// 反向对照：把 `Some(false)` 改判 `Ok(())`（即旧实现「断线照发、只看 send 结果」），
    /// 第一条断言当场挂——那正是公钥静默残留的路径。
    #[test]
    fn disconnected_destination_is_never_treated_as_deliverable() {
        assert!(untrust_route(Some(false)).is_err(), "断线会话的 send 可能 Ok 却永不执行");
        assert!(untrust_route(None).is_err(), "会话已不存在");
        assert!(untrust_route(Some(true)).is_ok(), "连着的会话照常发撤销");
    }
}

#[cfg(test)]
mod session_ownership_tests {
    use super::{first_foreign_session, session_owned_by};

    /// **AI 只能碰自己开的会话**（用户 2026-09-18 拍板：「AI 只能读取自己打开的窗口，不允许
    /// 读取和操作客户自己的窗口」）。用户的窗口、其它 AI 的窗口、旧版代理的共享池窗口，一律
    /// 不是你的；不带 actor 的请求也不拥有任何窗口。
    /// 反向对照：把 `session_owned_by` 改回「用户窗口放行」（`!ai_owned ||`），第一条挂。
    #[test]
    fn only_the_opener_owns_a_session() {
        assert!(!session_owned_by(false, None, Some("me")), "用户自己打开的窗口：不是任何 AI 的");
        assert!(session_owned_by(true, Some("me"), Some("me")));
        assert!(!session_owned_by(true, Some("other"), Some("me")), "另一个 AI 的窗口");
        assert!(!session_owned_by(true, None, Some("me")), "旧版代理的共享池窗口");
        assert!(!session_owned_by(true, None, None), "不带 actor 的请求不拥有任何窗口");
        assert!(!session_owned_by(true, Some("me"), None));
    }

    /// 门禁的合并规则：涉及的会话里**任何一个**不是自己的，整条拒绝（跨会话拷贝两端都要是
    /// 自己的）；不存在的 uid 不在这里拦（放过去回「会话不存在 + 你的会话列表」）。
    /// 反向对照：把 `first_foreign_session` 改成只看第一个 uid，第二条断言挂。
    #[test]
    fn any_foreign_session_in_the_request_refuses_it() {
        let table = |uid: u64| match uid {
            1 => Some((true, Some("me"))),
            2 => Some((false, None)),             // 用户的窗口
            3 => Some((true, Some("other"))),     // 别的 AI 的窗口
            _ => None,                            // 不存在
        };
        assert_eq!(first_foreign_session(&[1], table, Some("me")), None);
        assert_eq!(first_foreign_session(&[1, 2], table, Some("me")), Some(2));
        assert_eq!(first_foreign_session(&[3], table, Some("me")), Some(3));
        assert_eq!(first_foreign_session(&[99], table, Some("me")), None, "不存在的 uid 放过去");
        assert_eq!(first_foreign_session(&[], table, Some("me")), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        direct_connect_refusal, remote_basename, remote_parent, trim_leading_echo,
        validate_local_path, validate_remote_path, wrap_session_ender,
    };
    use crate::store::SavedConnection;

    #[test]
    fn open_session_refuses_undecryptable_password_and_says_to_reenter() {
        let c = SavedConnection {
            name: "web".into(),
            auth_kind: "password".into(),
            password_decrypt_failed: true,
            ..SavedConnection::default()
        };
        let msg = direct_connect_refusal(&c).expect("解不开的密码必须拒绝");
        assert!(msg.contains("web"));
        assert!(msg.contains("重新填写"));
        assert!(msg.contains("不要猜测"));
        let empty = SavedConnection {
            auth_kind: "password".into(),
            ..SavedConnection::default()
        };
        assert!(direct_connect_refusal(&empty).is_none());
    }

    #[test]
    fn session_enders_get_subshell_wrapped() {
        // 纯 exit（带可选数值码、容忍首尾空白与行尾分隔符）→ 子 shell 包裹，
        // 退出码保留、登录 shell 不死。
        assert_eq!(wrap_session_ender("exit"), "(exit)");
        assert_eq!(wrap_session_ender("exit 42"), "(exit 42)");
        // 段首尾空白、分隔符原样保留，只替换 exit 主体。
        assert_eq!(wrap_session_ender("  exit 0 ;  "), "  (exit 0) ;  ");
        assert_eq!(wrap_session_ender("exit -1"), "(exit -1)");
        // 命令链的**最后一段**：只替换该段，分隔符与注释保持原位。
        assert_eq!(wrap_session_ender("cd /x; exit"), "cd /x; (exit)");
        assert_eq!(wrap_session_ender("cd /x; exit 7"), "cd /x; (exit 7)");
        assert_eq!(wrap_session_ender("make && exit 1"), "make && (exit 1)");
        assert_eq!(wrap_session_ender("make || exit 1"), "make || (exit 1)");
        assert_eq!(wrap_session_ender("cmd; exit 3 # done"), "cmd; (exit 3) # done");
        assert_eq!(wrap_session_ender("cmd; exit 3;"), "cmd; (exit 3);", "末尾空段不算后续命令");
        assert_eq!(wrap_session_ender("cmd & exit 3"), "cmd & (exit 3)");
        assert_eq!(wrap_session_ender("cmd | exit 3"), "cmd | (exit 3)");
        // 重定向双字符操作符不能被当成段分隔符。
        assert_eq!(wrap_session_ender("echo hi >&2; exit 1"), "echo hi >&2; (exit 1)");
        assert_eq!(wrap_session_ender("echo hi 2>&1; exit 1"), "echo hi 2>&1; (exit 1)");
        // 引号里的分隔符不成段。
        assert_eq!(
            wrap_session_ender("echo \"a; exit\"; exit 3"),
            "echo \"a; exit\"; (exit 3)"
        );
        assert_eq!(wrap_session_ender("echo '; exit'; exit 3"), "echo '; exit'; (exit 3)");
    }

    #[test]
    fn non_session_enders_pass_through_unchanged() {
        // 普通命令、exit 只是参数/词干/非顶层段、非数值参数——一律原样，不动调用方的文本。
        for cmd in [
            "ls -l",
            "exit $code",
            "exit $(false; echo 1)",
            "exiting",
            "echo exit",
            "exit 1 2",
            "EXIT 3",
            // 子 shell / 命令替换里的 exit 本来就杀不掉登录 shell。
            "(exit 3)",
            "$(exit 3)",
            // 控制流体内的 exit 拦不住（文档已声明，走断线重连兜底）。
            "if true; then exit 1; fi",
            "while true; do exit; done",
            // -exec 的参数不是顶层命令；分号被转义、不成段。
            "find . -name x -exec exit 1 \\;",
            // 分隔符在引号 / 注释里：段根本不存在，exit 也从未真正存在。
            "echo \"; exit\"",
            "echo '; exit'",
            "echo hi # ; exit 3",
            "echo a#b",
            // exit 不是最后一段：不改写（这种形态由 validate_run_command 拒绝，见下面的测试）。
            "exit && reboot",
            "cmd; exit 1; echo after",
        ] {
            assert_eq!(wrap_session_ender(cmd), cmd, "不应改写：{cmd}");
        }
        // 形近但不同：trim 后为空也不是 exit。
        assert_eq!(wrap_session_ender(""), "");
        // logout 不包：`(logout)` 在子 shell 里必报「not login shell」，由校验直接拒绝。
        assert_eq!(wrap_session_ender("logout"), "logout");
        assert_eq!(wrap_session_ender("cmd; logout"), "cmd; logout");
    }

    /// logout 必须被拒绝而不是包进子 shell——实测 `bash -lic "(logout)"` 报
    /// 「logout: not login shell」返回 1。反向对照：删掉 validate_run_command 里的 logout
    /// 分支，前几条断言当场挂。
    #[test]
    fn logout_is_rejected_with_a_pointer_to_close_session() {
        use super::validate_run_command;
        let err = validate_run_command("logout").expect_err("logout 应被拒绝");
        assert!(err.contains("close_session"), "报错要指明替代做法：{err}");
        assert!(validate_run_command("  logout ; ").is_err());
        assert!(validate_run_command("cmd; logout").is_err(), "命令链里的 logout 也要拒绝");
        assert!(validate_run_command("cmd && logout 2").is_err());
        assert!(validate_run_command("logout # 注释").is_err());
        assert!(validate_run_command("exit 3").is_ok(), "exit 走子 shell 包裹，不拒绝");
        assert!(validate_run_command("cd /x; exit 7").is_ok(), "复合命令里的 exit 同样不拒绝");
        assert!(validate_run_command("echo logout").is_ok());
        assert!(validate_run_command("echo \"logout\"").is_ok());
        assert!(validate_run_command("echo hi # ; logout").is_ok(), "注释里的 logout 不是命令");
        // `${#x}` 是取长度不是注释：扫描不能在 `{#` 处截断，否则其后的 logout 漏检。
        // 反向对照：把 `{`/`}` 加回 is_shell_word_start，这条当场挂。
        assert!(validate_run_command("echo ${#x}; logout").is_err(), "${{#…}} 后面的 logout 必须检出");
    }

    /// **exit 后面还有命令必须拒绝。** 原语义里 exit 一执行 shell 就终止、后面一条都不跑，
    /// `cond || exit 1; rm -rf build` 这类守卫全靠它；改写成 `(exit 1)` 只结束子 shell，rm
    /// 照跑（实测 bash：`test -f /nonexistent || (exit 1); echo DANGER` 打印 DANGER）。
    /// 反向对照：删掉 validate_run_command 里 `exit_followed_by_command` 那道检查，前三条挂。
    #[test]
    fn exit_followed_by_more_commands_is_rejected() {
        use super::validate_run_command;
        let err = validate_run_command("test -f x || exit 1; rm -rf build")
            .expect_err("守卫式 exit 后接命令必须拒绝");
        assert!(err.contains("条件分支"), "报错要给出改写方式：{err}");
        assert!(validate_run_command("exit && reboot").is_err());
        assert!(validate_run_command("cmd; exit 1; echo after").is_err());
        // exit 是最后一段：语义可以保住，放行（由 wrap_session_ender 改写）。
        assert!(validate_run_command("cd /x; exit 7").is_ok());
        assert!(validate_run_command("make || exit 1").is_ok());
        assert!(validate_run_command("cmd; exit 3; # 注释").is_ok(), "末尾空段与注释不算后续命令");
        // 不是顶层 exit 段：与这条规则无关。
        assert!(validate_run_command("echo exit; ls").is_ok());
        assert!(validate_run_command("(exit 3); ls").is_ok(), "子 shell 里的 exit 本来就不终止");
    }

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

/// 配对握手（v4）在**真 socket 上**的行为测试。
///
/// 为什么非要开真连接、把 `handle_conn` 整个拉起来跑：握手是全协议唯一一处「一条连接两问
/// 两答」，它的分帧、第二行的超时、以及"证明不过就不放行"这些性质，全都不在纯函数里——
/// `mcp_protocol` 那几个单测只证明 HMAC 算得对，证明不了这条连接会不会在第二行上挂死、
/// 或者干脆把没证明过的连接放过去。编译绿 ≠ 能用。
///
/// 仍然没被覆盖的部分（必须由人在真机上过）：`ishell-mcp` 那半边的时序，以及跨 SSH 反向
/// 转发时的实际行为。这里的客户端是照协议手写的，与真代理共用 `mcp_protocol` 的证明算法，
/// 但发包顺序是各写各的。
#[cfg(all(test, unix))]
mod pair_handshake_tests {
    use super::*;
    use crate::mcp_protocol::{pair_nonce, pair_proof, PairRole};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// 起一个只服务一条连接的 `handle_conn`，返回客户端那一端。
    async fn serve_one() -> UnixStream {
        let dir = std::env::temp_dir().join(format!(
            "ishell-pair-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&dir);
        let listener = UnixListener::bind(&dir).expect("绑定测试 socket");
        let (tx, _rx) = mpsc::unbounded_channel();
        let ctx = egui::Context::default();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            let permit = sem.acquire_owned().await.expect("permit");
            let work = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            handle_conn(stream, tx, ctx, permit, work).await;
        });
        let s = UnixStream::connect(&dir).await.expect("连接测试 socket");
        let _ = std::fs::remove_file(&dir); // 路径已不需要，连接照常存活
        s
    }

    /// 发一行请求、收一行响应。
    async fn round(
        r: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        w: &mut tokio::net::unix::OwnedWriteHalf,
        id: u64,
        kind: McpReqKind,
    ) -> Result<McpReqResult, String> {
        let mut line = serde_json::to_string(&McpRequest {
            id,
            instance: None,
            origin: None,
            // v6：握手要声明 actor 才签发凭据（签发的凭据绑定它）。
            actor: Some(TEST_ACTOR.into()),
            ticket: None,
            kind,
        })
        .unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.expect("写请求");
        let mut resp = String::new();
        r.read_line(&mut resp).await.expect("读响应");
        assert!(!resp.is_empty(), "对端没回任何东西就断了");
        serde_json::from_str::<McpResponse>(resp.trim())
            .expect("响应应能解析")
            .result
    }

    /// 走完整条握手；`token` 是客户端用来算证明的密钥。返回最终那一步的结果。
    async fn handshake_with(token: &str) -> Result<McpReqResult, String> {
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        let nonce_c = pair_nonce().expect("熵源");
        let challenge = round(
            &mut r,
            &mut w,
            1,
            McpReqKind::PairHello {
                nonce_c: nonce_c.clone(),
            },
        )
        .await?;
        let (nonce_s, server_proof) = match challenge {
            McpReqResult::PairChallenge {
                nonce_s,
                server_proof,
                ..
            } => (nonce_s, server_proof),
            other => panic!("第一步应回 PairChallenge，实际：{other:?}"),
        };
        // 客户端本该在这里验服务器证明；测试里按需分别断言，所以此处只把它带出去。
        assert_eq!(
            server_proof,
            pair_proof(
                &crate::store::mcp_pairing_token(),
                &nonce_c,
                &nonce_s,
                PairRole::Server
            ),
            "服务器证明与共享算法算出来的不一致"
        );
        round(
            &mut r,
            &mut w,
            2,
            McpReqKind::PairProve {
                client_proof: pair_proof(token, &nonce_c, &nonce_s, PairRole::Client),
            },
        )
        .await
    }

    /// 正路：知道 token 的调用方能走完握手，拿到实例标识。
    #[tokio::test]
    async fn correct_token_completes_the_handshake() {
        let token = crate::store::mcp_pairing_token();
        match handshake_with(&token).await {
            Ok(McpReqResult::Instance { id, .. }) => {
                assert_eq!(id, crate::store::mcp_instance_id())
            }
            other => panic!("正确 token 应握手成功，实际：{other:?}"),
        }
    }

    /// 反路：token 不对就必须被拒——这是整条隔离的意义所在。
    #[tokio::test]
    async fn wrong_token_is_refused() {
        let err = handshake_with("definitely-not-the-token")
            .await
            .expect_err("错误 token 必须被拒");
        assert!(err.contains("证明不符"), "错误信息应指明是证明不符：{err}");
    }

    /// 每条连接的服务器随机数必须是新的：复用随机数会让抓到的证明可以重放。
    #[tokio::test]
    async fn each_connection_gets_a_fresh_server_nonce() {
        async fn nonce_of() -> String {
            let (r, mut w) = serve_one().await.into_split();
            let mut r = BufReader::new(r);
            match round(
                &mut r,
                &mut w,
                1,
                McpReqKind::PairHello {
                    nonce_c: "fixed".into(),
                },
            )
            .await
            .expect("第一步应成功")
            {
                McpReqResult::PairChallenge { nonce_s, .. } => nonce_s,
                other => panic!("应回 PairChallenge：{other:?}"),
            }
        }
        assert_ne!(nonce_of().await, nonce_of().await, "服务器随机数被复用了");
    }

    /// 没先握手就直接出示证明：拒绝。否则调用方就能自选挑战，双向证明形同虚设。
    #[tokio::test]
    async fn prove_without_hello_is_refused() {
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        let err = round(
            &mut r,
            &mut w,
            1,
            McpReqKind::PairProve {
                client_proof: "whatever".into(),
            },
        )
        .await
        .expect_err("未开始握手就出示证明必须被拒");
        assert!(err.contains("未开始"), "{err}");
    }

    /// 握手中途改发别的请求：不放行。没走完握手的连接不该顺势拿到执行能力。
    #[tokio::test]
    async fn switching_to_another_request_mid_handshake_is_refused() {
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        let _ = round(
            &mut r,
            &mut w,
            1,
            McpReqKind::PairHello {
                nonce_c: pair_nonce().expect("熵源"),
            },
        )
        .await
        .expect("第一步应成功");
        let err = round(&mut r, &mut w, 2, McpReqKind::ListSessions)
            .await
            .expect_err("握手第二步只接受 PairProve");
        assert!(err.contains("PairProve"), "{err}");
    }

    /// 测试握手声明的进程身份。
    const TEST_ACTOR: &str = "test-actor";

    /// 发一条点名本实例的业务请求（`ListSessions`），带上给定凭据与自报 actor，返回原始响应。
    async fn business_request(ticket: Option<String>, claimed_actor: &str) -> Result<McpReqResult, String> {
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        let mut line = serde_json::to_string(&McpRequest {
            id: 1,
            instance: Some(crate::store::mcp_instance_id().to_string()),
            origin: None,
            actor: Some(claimed_actor.into()),
            ticket,
            kind: McpReqKind::ListSessions,
        })
        .unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.expect("写请求");
        let mut resp = String::new();
        r.read_line(&mut resp).await.expect("读响应");
        match serde_json::from_str::<McpResponse>(resp.trim()) {
            Ok(r) => r.result,
            // serve_one 的接收端已丢弃：通过了门禁的请求递不进 App，连接会被直接收掉——
            // 这正说明它**通过了**凭据校验。
            Err(_) => Ok(McpReqResult::Ok),
        }
    }

    /// **iShell 端身份校验（v6）**：点名点对了实例、却没带有效凭据的业务请求，必须拒绝。
    /// 此前这里只核对实例 id——而 id 任何人发一句匿名 Identify 就能问到，于是任何客户端都能
    /// 驱动别人的 iShell（用户：「严禁操作其他人的客户端」）。
    /// 反向对照：删掉 `handle_conn` 里凭据核对那一段，前两条断言当场挂。
    #[tokio::test]
    async fn a_request_without_a_valid_ticket_is_refused_even_with_the_right_instance() {
        let err = business_request(None, TEST_ACTOR).await.expect_err("没带凭据必须拒绝");
        assert!(err.starts_with(crate::mcp_protocol::TICKET_REJECTED), "{err}");
        let err = business_request(Some("f".repeat(128)), TEST_ACTOR)
            .await
            .expect_err("伪造的凭据必须拒绝");
        assert!(err.starts_with(crate::mcp_protocol::TICKET_REJECTED), "{err}");
        // 握手拿到的凭据：放行。
        let ticket = match handshake_with(&crate::store::mcp_pairing_token()).await {
            Ok(McpReqResult::Instance { ticket, .. }) => ticket,
            other => panic!("握手应签发凭据，实际：{other:?}"),
        };
        assert!(!ticket.is_empty(), "握手成功的应答里必须带凭据");
        assert!(business_request(Some(ticket), TEST_ACTOR).await.is_ok());
    }

    /// 凭据绑定握手时声明的 actor，iShell 以它为准——请求里自报别人的 actor 没用。
    /// 反向对照：把 `req.actor = Some(actor)` 那一行删掉，本条挂（自报的 impostor 会被采信）。
    #[tokio::test]
    async fn the_ticket_pins_the_actor_declared_at_handshake() {
        let ticket = match handshake_with(&crate::store::mcp_pairing_token()).await {
            Ok(McpReqResult::Instance { ticket, .. }) => ticket,
            other => panic!("握手应签发凭据，实际：{other:?}"),
        };
        let (stream, mut rx) = serve_keep_rx().await;
        let (r, mut w) = stream.into_split();
        let mut line = serde_json::to_string(&McpRequest {
            id: 1,
            instance: Some(crate::store::mcp_instance_id().to_string()),
            origin: None,
            actor: Some("impostor".into()),
            ticket: Some(ticket),
            kind: McpReqKind::ListSessions,
        })
        .unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.expect("写请求");
        let call = rx.recv().await.expect("带有效凭据的请求应递进 App");
        assert_eq!(call.req.actor.as_deref(), Some(TEST_ACTOR), "应以凭据绑定的 actor 为准");
        drop(r);
    }

    /// 与 `serve_one` 相同，但保留接收端，用来看请求递进 App 时的样子。
    async fn serve_keep_rx() -> (UnixStream, mpsc::UnboundedReceiver<McpCall>) {
        let dir = std::env::temp_dir().join(format!(
            "ishell-ticket-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&dir);
        let listener = UnixListener::bind(&dir).expect("绑定测试 socket");
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = egui::Context::default();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            let permit = sem.acquire_owned().await.expect("permit");
            let work = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            handle_conn(stream, tx, ctx, permit, work).await;
        });
        let s = UnixStream::connect(&dir).await.expect("连接测试 socket");
        let _ = std::fs::remove_file(&dir);
        (s, rx)
    }

    /// 发一行请求，断言**零应答**（对端静默关闭连接）：token 不符的 `IdentifyPair` 探测在
    /// v5 下的期望行为——调用方什么也学不到。
    async fn assert_silenced(kind: McpReqKind) {
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        let mut line = serde_json::to_string(&McpRequest {
            id: 1,
            instance: None,
            origin: None,
            actor: None,
            ticket: None,
            kind,
        })
        .unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.expect("写请求");
        let mut resp = String::new();
        let n = r.read_line(&mut resp).await.expect("读响应");
        assert_eq!(n, 0, "token 不符的探测必须零应答（连接被静默关闭）");
    }

    /// v5 的匿名发现语义：匿名 `Identify` 是**版本信标**，照常应答（旧代理全靠它拿到
    /// 「版本不符，请重新部署」的提示）；匿名 `IdentifyPair` 只对 **token 正确**的调用方
    /// 应答（v3 旧代理凭正确 token 走到版本校验），token 不符零应答；配对握手照常完成。
    #[tokio::test]
    async fn anonymous_identify_beacons_version_but_pair_probe_requires_the_token() {
        // 匿名 Identify：照答——版本信标，全协议唯一跨版本可解析的请求。
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        match round(&mut r, &mut w, 1, McpReqKind::Identify).await {
            Ok(McpReqResult::Instance {
                id, proto_version, ..
            }) => {
                assert_eq!(id, crate::store::mcp_instance_id());
                assert_eq!(
                    proto_version,
                    crate::mcp_protocol::MCP_PROTOCOL_VERSION
                );
            }
            other => panic!("匿名 Identify 应当照答（版本信标），实际：{other:?}"),
        }
        // 匿名 IdentifyPair：token 不符零应答；token 正确照答（v3 兼容，走到版本校验）。
        assert_silenced(McpReqKind::IdentifyPair {
            token: "definitely-not-the-token".into(),
        })
        .await;
        let token = crate::store::mcp_pairing_token();
        let (r, mut w) = serve_one().await.into_split();
        let mut r = BufReader::new(r);
        match round(
            &mut r,
            &mut w,
            1,
            McpReqKind::IdentifyPair {
                token: token.clone(),
            },
        )
        .await
        {
            Ok(McpReqResult::Instance { id, .. }) => {
                assert_eq!(id, crate::store::mcp_instance_id())
            }
            other => panic!("正确 token 的 IdentifyPair 应当照答，实际：{other:?}"),
        }
        // 配对握手：知道 token 的调用方照常拿到实例标识
        match handshake_with(&token).await {
            Ok(McpReqResult::Instance { id, .. }) => {
                assert_eq!(id, crate::store::mcp_instance_id())
            }
            other => panic!("配对握手不应被挡，实际：{other:?}"),
        }
    }
}

#[cfg(all(unix, test))]
mod upload_stream_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 起一个只服务一条连接的 `handle_conn`，把 `McpCall` 的接收端一并交回给测试
    /// （握手测试那份 helper 丢掉了 rx，这里必须留着才能拿到 upload_source）。
    async fn serve_one_keep_rx() -> (UnixStream, mpsc::UnboundedReceiver<McpCall>) {
        let path = std::env::temp_dir().join(format!(
            "ishell-upload-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("绑定测试 socket");
        let (tx, rx) = mpsc::unbounded_channel();
        let ctx = egui::Context::default();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            let permit = sem.acquire_owned().await.expect("permit");
            let work = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
            handle_conn(stream, tx, ctx, permit, work).await;
        });
        let s = UnixStream::connect(&path).await.expect("连接测试 socket");
        let _ = std::fs::remove_file(&path);
        (s, rx)
    }

    /// 发一条 `CopyToRemoteFromCaller` 请求 + `size` 字节负载，返回 worker 侧实际收到的字节数。
    /// `err_while_reading`：模拟「App 已回报错（如文件操作超时）、worker 却还活着在读」。
    async fn upload_bytes_with(size: usize, err_while_reading: bool) -> usize {
        let (stream, mut rx) = serve_one_keep_rx().await;
        let (_r, mut w) = stream.into_split();
        let mut line = serde_json::to_string(&McpRequest {
            id: 1,
            // 非握手类请求必须点名实例，否则会被 `is_addressed_to` 挡在门外
            instance: Some(crate::store::mcp_instance_id().to_string()),
            origin: None,
            actor: None,
            // v6：业务请求必须带 iShell 签发的凭据（这里直接签一张，握手本身另有测试）
            ticket: super::tickets::issue("upload-test".into()),
            kind: McpReqKind::CopyToRemoteFromCaller {
                session_uid: 1,
                remote_path: "/tmp/whatever".into(),
                size: size as u64,
                timeout_ms: 60_000,
            },
        })
        .expect("序列化请求");
        line.push('\n');
        w.write_all(line.as_bytes()).await.expect("写请求行");

        // 负载分块写：写端要和下面的读端并发跑，否则 socket 缓冲区一满就双双卡死。
        let writer = tokio::spawn(async move {
            let chunk = vec![0xABu8; 64 * 1024];
            let mut left = size;
            while left > 0 {
                let n = left.min(chunk.len());
                if w.write_all(&chunk[..n]).await.is_err() {
                    break; // 对端提前不读了：正是要检测的失败，交给下面的字节数断言
                }
                left -= n;
            }
            let _ = w.shutdown().await;
        });

        let call = rx.recv().await.expect("handle_conn 应把请求转进来");
        let mut src = call.upload_source.expect("上传请求必须带字节流");
        if err_while_reading {
            let _ = call.resp_tx.send(McpResponse { id: 1, result: Err("文件操作超时".into()) });
            // 给「报错即排干」的错误实现留足抢数据的时间
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let mut got = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match src.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        // 必须 abort 而不是 await：一旦对端提前不读了（正是这个测试要抓的回归），
        // 写端会永远卡在 `write_all` 上——`await` 它就把"断言失败"变成了"测试挂死"，
        // CI 只会超时，给不出任何诊断。
        writer.abort();
        got
    }

    async fn upload_bytes(size: usize) -> usize {
        upload_bytes_with(size, false).await
    }

    /// 报错 ≠ worker 已停：文件操作超时到点就回 Err，worker 的上传却无法取消、仍在读 source。
    /// 这时 `handle_conn` 若立刻排干，就是两个读取者抢同一路字节流——worker 收不全、上传
    /// 必然「提前结束」。排干必须等 source 被释放。
    /// 反向对照：去掉排干前的 `source_dropped.await`，本条当场挂（worker 收到的字节少于 size）。
    #[tokio::test]
    async fn error_response_does_not_steal_bytes_from_live_worker() {
        let size = 4 * 1024 * 1024;
        assert_eq!(
            upload_bytes_with(size, true).await,
            size,
            "worker 仍在读时排干抢走了字节"
        );
    }

    /// `MAX_MCP_LINE_BYTES` 只该限请求行，不该限跟在它后面的文件流。
    ///
    /// 这个上限是用 `take` 实现的，而 `take` 套的是整条连接——上传路径若不把它摘掉，
    /// 超过 32 MiB 的 `copy_to_remote` 就必然失败（实测分界线是 32 MiB 减去请求行长度，
    /// 报错还是「调用方文件流提前结束」这种指不到根因的话）。
    #[tokio::test]
    async fn upload_stream_is_not_capped_by_request_line_limit() {
        let size = MAX_MCP_LINE_BYTES as usize + 5 * 1024 * 1024; // 稳稳越过上限
        assert_eq!(
            upload_bytes(size).await,
            size,
            "上传字节流被请求行上限截断了"
        );
    }

    /// 顺带确认没把小文件搞坏（预读进 BufReader 的首块必须原样接上）。
    #[tokio::test]
    async fn small_upload_stream_is_intact() {
        let size = 3 * 1024 * 1024;
        assert_eq!(upload_bytes(size).await, size);
    }
}
