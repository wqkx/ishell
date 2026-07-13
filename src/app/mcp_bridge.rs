//! AI/MCP 控制通道：本地 Unix domain socket 桥接，供独立的 `ishell-mcp` stdio 代理进程
//! （见 `src/bin/mcp_stdio.rs`）连接，把 list/run/poll/read/interrupt 请求转发到本进程
//! 持有的活跃 SSH 会话。默认关闭（`store::load_mcp_consent()`），一次 socket 连接 = 一问一答。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::mcp_protocol::{
    McpReqKind, McpReqResult, McpRequest, McpResponse, McpRunResult, McpSavedConn, McpSessionInfo,
};
use crate::proto::{AuthMethod, ConnectConfig, JumpHost, UiCommand};
use crate::store::SavedConnection;

use super::{App, Session};

impl Session {
    /// 放弃当前挂起的 AI 命令运行：给还在等待的 `poll_run` 一个明确的"未完成"响应
    /// （而不是让它继续空等到超时），并取消哨兵捕获、清空 `pending_ai_run`。
    /// 用于打断（`interrupt`）、断线等"这条运行注定等不到哨兵了"的场景。
    pub(super) fn cancel_pending_ai_run(&mut self) {
        if let Some(mut pending) = self.pending_ai_run.take() {
            if let Some(tx) = pending.resp_tx.take() {
                let output = self.terminal.peek_ai_output().unwrap_or_default();
                let _ = tx.send(McpResponse {
                    id: pending.req_id,
                    result: Ok(McpReqResult::Run(McpRunResult {
                        run_id: pending.run_id,
                        finished: false,
                        output,
                        exit_code: None,
                    })),
                });
            }
        }
        self.terminal.cancel_ai_capture();
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
fn trim_leading_echo(output: &str, command: &str) -> String {
    match output.strip_prefix(command.trim()) {
        Some(rest) => rest.trim_start_matches(['\r', '\n']).to_string(),
        None => output.to_string(),
    }
}

/// 命令已跑完时，再裁掉结尾那一小段没有换行收尾的提示符残片（标记行是紧跟在提示符后面
/// 同一行打的，提示符本身没被吞，但这一行天然没有换行结尾）。**只有确实找得到换行符才裁**：
/// 找不到换行符时无法区分"就是这一小段提示符残片"和"命令本身输出就没有换行"（比如
/// `printf 'done'`），裁了可能把真实输出整段删掉——宁可保留这一小段噪音，也不能丢真实数据。
/// **不能用于还没跑完的中途快照**：命令仍在运行时结尾没换行完全可能是真实输出（比如进度条），
/// 裁掉会误伤。
/// 注意：多行自定义 prompt 里提示符上方还可能残留一两行噪音，属已知的、有意保守的取舍
/// （避免用不可靠的启发式误伤真实输出）。
fn trim_command_echo_and_prompt(output: &str, command: &str) -> String {
    let s = trim_leading_echo(output, command);
    match s.rfind('\n') {
        Some(i) => s[..i].to_string(),
        None => s,
    }
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
        // 本机可能同时开着好几个 iShell 实例，默认本机直连 socket 路径是固定的
        // （~/.config/ishell/mcp.sock）——但这里不能靠"探测到已有监听者就放弃绑定"来避让：
        // 每个进程自己的 SSH 反向转发桥接（bridge_local_mcp）在收到远端连进来的请求时，
        // 都是连去这同一个固定路径找"本进程的" mcp_socket_path()；如果本进程因为探测到
        // 别的实例在监听就放弃了绑定，本进程自己那些会话就永远够不到自己的本地监听器
        // （反而落到了别的实例头上，那个实例根本不认识这些会话）——这比"后启动的实例
        // 顶掉前一个实例的 socket 文件"更糟：这里退回最简单可靠的做法，每次都
        // remove_file+bind，谁最后启动/重连谁就拿到这个共享路径，符合直觉、且保证
        // "当前正在被 AI 操作的这个进程"自己的反向转发链路始终是通的。
        let _ = std::fs::remove_file(&sock_path); // 清理上次异常退出/被顶替残留的 socket 文件
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
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(handle_conn(stream, tx.clone(), ctx.clone()));
        }
    });
    rx
}

/// 一条连接只处理一问一答：读一行 JSON 请求，转发进 mpsc，等 App 帧循环回填后写一行 JSON 响应。
async fn handle_conn(stream: UnixStream, tx: mpsc::UnboundedSender<McpCall>, ctx: egui::Context) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let Ok(Some(line)) = lines.next_line().await else {
        return; // 对端没写完就断了连接，没东西可读也没法回应
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
                    let output = trim_command_echo_and_prompt(&output, &pending.command);
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
                        result: Err("等待用户确认超时，已自动拒绝".into()),
                    });
                }
                self.pending_open_consent = None;
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
                    send_err(resp_tx, "会话不存在".into());
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
                    send_err(resp_tx, "该会话已有一条 AI 命令正在执行，请先 poll_run 或等待完成".into());
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
                let _ = s
                    .cmd_tx
                    .send(UiCommand::TerminalInput(format!("{command}\r").into_bytes()));
                let _ = s
                    .cmd_tx
                    .send(UiCommand::TerminalInput(format!("{marker}\r").into_bytes()));
                s.terminal.expect_echo(&marker);
                s.terminal.arm_ai_capture(prefix.into_bytes());
                s.pending_ai_run = Some(PendingAiRun {
                    run_id: nonce as u64,
                    deadline: Instant::now() + Duration::from_millis(timeout_ms.max(100)),
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
                    send_err(resp_tx, "会话不存在".into());
                    return;
                };
                match self.sessions[idx].pending_ai_run.as_mut() {
                    Some(p) if run_id.map_or(true, |r| r == p.run_id) => {
                        p.deadline = Instant::now() + Duration::from_millis(timeout_ms.max(100));
                        p.resp_tx = Some(resp_tx);
                        p.req_id = id;
                    }
                    _ => send_err(resp_tx, "run_id 不存在或已结束".into()),
                }
            }
            McpReqKind::ReadScreen { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, "会话不存在".into());
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
                    send_err(resp_tx, "会话不存在".into());
                    return;
                };
                let s = &mut self.sessions[idx];
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(vec![0x03]));
                // 打断意味着放弃这条命令的哨兵检测：被打断的程序（比如 `cat`）很可能会把
                // 紧跟着排的标记行当成自己的输入吃掉，哨兵永远等不到。
                s.cancel_pending_ai_run();
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
                    deadline: Instant::now() + Duration::from_secs(60),
                });
            }
            McpReqKind::CloseSession { session_uid } => {
                let Some(idx) = self.session_idx_by_uid(session_uid) else {
                    send_err(resp_tx, "会话不存在".into());
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
                    send_err(resp_tx, "会话不存在".into());
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
                    send_err(resp_tx, "会话不存在".into());
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{trim_command_echo_and_prompt, trim_leading_echo};

    #[test]
    fn trim_leading_echo_strips_command_line_only() {
        let out = "hostname && whoami\r\nhost\nuser\n";
        assert_eq!(trim_leading_echo(out, "hostname && whoami"), "host\nuser\n");
        // 命令文本对不上时原样返回（不误伤）
        assert_eq!(trim_leading_echo(out, "other"), out);
    }

    #[test]
    fn trim_command_echo_and_prompt_strips_leading_echo_and_trailing_prompt_fragment() {
        let out = "hostname && whoami && pwd\ns3-server\ns3\n/home/s3\n(env) s3@s3-server:~\n$ ";
        assert_eq!(
            trim_command_echo_and_prompt(out, "hostname && whoami && pwd"),
            "s3-server\ns3\n/home/s3\n(env) s3@s3-server:~"
        );
    }

    #[test]
    fn trim_command_echo_and_prompt_keeps_output_without_any_newline() {
        // 命令自身输出没有换行（如 `printf 'done'`），裁掉开头回显后就再没有 '\n' 了——
        // 这时不能把整段当成「提示符残片」删掉，真实输出必须保留。
        let out = "printf 'done'\ndone$ ";
        assert_eq!(
            trim_command_echo_and_prompt(out, "printf 'done'"),
            "done$ "
        );
    }
}
