//! AI/MCP 控制通道：本地 Unix domain socket 桥接，供独立的 `ishell-mcp` stdio 代理进程
//! （见 `src/bin/mcp_stdio.rs`）连接，把 list/run/poll/read/interrupt 请求转发到本进程
//! 持有的活跃 SSH 会话。默认关闭（`store::load_mcp_consent()`），一次 socket 连接 = 一问一答。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::mcp_protocol::{
    McpReqKind, McpReqResult, McpRequest, McpResponse, McpRunResult, McpSavedConn, McpSessionInfo,
};
use crate::proto::{AuthMethod, ConnectConfig, Eol, JumpHost, UiCommand};
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
            || matches!(
                &self.pending_file_op,
                Some(op) if matches!(op.kind, FileOpKind::Write { op_id } if op_id == id)
            )
    }

    /// 同上，读操作版本。
    pub(super) fn file_read_op_would_resolve(&self, id: u64) -> bool {
        self.file_op_tombstones.contains(&id)
            || matches!(
                &self.pending_file_op,
                Some(op) if matches!(op.kind, FileOpKind::Read { op_id } if op_id == id)
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

    /// 放弃当前挂起的文件读写（write_file/read_file）：给等待中的响应通道一个明确的
    /// "未完成"错误，而不是让它一直空等到超时。用于断线等场景。
    pub(super) fn cancel_pending_file_op(&mut self) {
        if let Some(mut op) = self.pending_file_op.take() {
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
        let matches = matches!(
            &self.pending_file_op,
            Some(op) if matches!(op.kind, FileOpKind::Write { op_id } if op_id == id)
        );
        if !matches {
            return false;
        }
        let Some(mut op) = self.pending_file_op.take() else {
            return false;
        };
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
        let matches = matches!(
            &self.pending_file_op,
            Some(op) if matches!(op.kind, FileOpKind::Read { op_id } if op_id == id)
        );
        if !matches {
            return false;
        }
        let Some(mut op) = self.pending_file_op.take() else {
            return false;
        };
        if let Some(tx) = op.resp_tx.take() {
            let req_id = op.req_id;
            let resp = match result {
                Ok(content) => McpResponse {
                    id: req_id,
                    result: Ok(McpReqResult::FileContent {
                        // op 已经是从 pending_file_op 里取走的本地所有权、之后不再使用，
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
}

/// Write 用 `op_id` 匹配 `FileSaved`/`FileSaveFailed`/`FileSaveConflict`；Read 用 `op_id`
/// 匹配 `FileOpened`/`FileLoadFailed`/`FileTooLarge`——两边现在都带请求 id（`proto.rs` 里
/// `WriteFile`/这三个事件都加了 id 字段），各自的 `op_id` 语义不同（Write 侧是这次
/// write_file 生成的临时 id；Read 侧同理），用各自的变量名区分。
pub(super) enum FileOpKind {
    Write { op_id: u64 },
    Read { op_id: u64 },
}

/// AI 的 `write_file`/`read_file` 请求，正等待 worker 侧 SFTP 操作完成的事件
/// （同一会话同一时刻只允许一个，跟 `PendingAiRun` 的忙碌保护是同一个思路）。
pub(super) struct PendingAiFileOp {
    kind: FileOpKind,
    /// 请求的远端路径：Write 用它匹配事件，Read 用于响应里回填 `FileContent.path`。
    path: String,
    resp_tx: Option<oneshot::Sender<McpResponse>>,
    req_id: u64,
    deadline: Instant,
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

/// AI 调 `open_session` 时，若这条已保存连接本次运行期间还没被用户批准过，需要弹窗让用户
/// 确认后才真正建立连接——保留发起时刻的连接快照，避免确认期间用户又改了同名连接。
pub(super) struct PendingOpenConsent {
    pub(super) conn: SavedConnection,
    resp_tx: Option<oneshot::Sender<McpResponse>>,
    req_id: u64,
    deadline: Instant,
}

/// 启动 socket 监听（若用户未在设置里开启 AI 控制，返回一个永远收不到数据的空通道）。
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

/// 并发连接数上限（见 spawn_mcp_listener 里的信号量）。
const MAX_MCP_CONNECTIONS: usize = 32;
/// 首行请求的最大字节数：write_file 的 content 可能是个不小的源码/日志文件，读大文件场景
/// 也不该被卡得太死，但也不能真的无界——给一个远大于任何正常请求、又能兜住"恶意/异常连接
/// 持续灌数据不换行"这种情况的上限。
const MAX_MCP_LINE_BYTES: u64 = 256 * 1024 * 1024;
/// 首行读取超时：连上但迟迟不发完整一行的连接（占位攻击/半开连接）不能无限占着任务和 fd。
const FIRST_LINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 一条连接只处理一问一答：读一行 JSON 请求，转发进 mpsc，等 App 帧循环回填后写一行 JSON 响应。
/// `_permit` 只用来在这个连接存活期间占着信号量里的一个名额，函数退出时自动释放。
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
    let (resp_tx, resp_rx) = oneshot::channel();
    if tx.send(McpCall { req, resp_tx }).is_err() {
        return;
    }
    ctx.request_repaint(); // 唤醒 UI 线程尽快排空这条请求
    let resp = resp_rx.await.unwrap_or(McpResponse {
        id,
        result: Err("iShell 未能处理该请求（可能已关闭）".into()),
    });
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
    }

    /// 检查各会话挂起的文件读写（write_file/read_file）是否超时。必须在每帧
    /// `for s in &mut self.sessions { s.drain_events(); ... }`（消化 worker 事件、
    /// 真正 resolve `pending_file_op` 的地方）**之后**调用——如果在那之前调用，
    /// 会跟"本帧里事件恰好也到达"打时序竞争：明明操作已经真正完成，却先被这里判定超时、
    /// 清空 pending_file_op，随后姗姗来迟的 drain_events 才处理那个事件，此时已经找不到
    /// 挂起记录去 resolve 了，AI 会收到一个错误的"超时"而不是正确的结果。
    pub(super) fn check_file_op_timeouts(&mut self) {
        const MAX_TOMBSTONES: usize = 32;
        for s in &mut self.sessions {
            if let Some(op) = s.pending_file_op.as_mut() {
                if Instant::now() >= op.deadline {
                    if let Some(tx) = op.resp_tx.take() {
                        let _ = tx.send(McpResponse {
                            id: op.req_id,
                            result: Err("文件操作超时（worker 未在超时前返回结果）".into()),
                        });
                    }
                    // worker 侧的 SFTP 操作没法取消，超时不代表它已经停止——记一个"墓碑"，
                    // 这样迟到的真实完成事件到达时能被认出来直接丢弃，不会因为
                    // pending_file_op 已经是 None 就被误路由进普通编辑器 UI。
                    let op_id = match op.kind {
                        FileOpKind::Write { op_id } | FileOpKind::Read { op_id } => op_id,
                    };
                    s.file_op_tombstones.push_back(op_id);
                    if s.file_op_tombstones.len() > MAX_TOMBSTONES {
                        s.file_op_tombstones.pop_front();
                    }
                    s.pending_file_op = None;
                }
            }
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
        };
        let _ = resp_tx.send(McpResponse {
            id,
            result: Ok(McpReqResult::Opened(info)),
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

    fn handle_mcp_call(&mut self, call: McpCall) {
        let McpCall { req, resp_tx } = call;
        let id = req.id;
        let send_err = |resp_tx: oneshot::Sender<McpResponse>, msg: String| {
            let _ = resp_tx.send(McpResponse {
                id,
                result: Err(msg),
            });
        };
        match req.kind {
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
                        if p.resp_tx.is_some() {
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
                let s = &mut self.sessions[idx];
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(vec![0x03]));
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
                if self.pending_open_consent.is_some() {
                    send_err(resp_tx, "已有一个连接请求正在等待用户确认，请稍候重试".into());
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
                let _ = self.sessions[idx]
                    .cmd_tx
                    .send(UiCommand::TerminalInput(text.into_bytes()));
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
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_op.is_some() {
                    send_err(resp_tx, "该会话已有一个文件读写操作正在进行，请稍候重试".into());
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
                s.pending_file_op = Some(PendingAiFileOp {
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
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, self.session_not_found_msg(session_uid));
                    return;
                };
                let s = &mut self.sessions[idx];
                if !s.connected {
                    send_err(resp_tx, "会话尚未连接（可能在连接/认证中，或已断线），请稍后重试".into());
                    return;
                }
                if s.pending_file_op.is_some() {
                    send_err(resp_tx, "该会话已有一个文件读写操作正在进行，请稍候重试".into());
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
                s.pending_file_op = Some(PendingAiFileOp {
                    kind: FileOpKind::Read { op_id },
                    path,
                    resp_tx: Some(resp_tx),
                    req_id: id,
                    deadline: Instant::now() + clamp_timeout(timeout_ms),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::trim_leading_echo;

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
