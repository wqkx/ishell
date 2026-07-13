//! AI/MCP 控制通道：本地 Unix domain socket 桥接，供独立的 `ishell-mcp` stdio 代理进程
//! （见 `src/bin/mcp_stdio.rs`）连接，把 list/run/poll/read/interrupt 请求转发到本进程
//! 持有的活跃 SSH 会话。默认关闭（`store::load_mcp_consent()`），一次 socket 连接 = 一问一答。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::mcp_protocol::{McpReqKind, McpReqResult, McpRequest, McpResponse, McpRunResult, McpSessionInfo};
use crate::proto::{AuthMethod, ConnectConfig, JumpHost, UiCommand};
use crate::store::SavedConnection;

use super::App;

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
        let _ = std::fs::remove_file(&sock_path); // 清理上次异常退出残留的 socket 文件
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
        return;
    };
    let Ok(req) = serde_json::from_str::<McpRequest>(&line) else {
        return;
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
            if let Some((code, output)) = s.terminal.take_ai_done() {
                let resp = McpResponse {
                    id: pending.req_id,
                    result: Ok(McpReqResult::Run(McpRunResult {
                        run_id: pending.run_id,
                        finished: true,
                        output,
                        exit_code: Some(code),
                    })),
                };
                if let Some(tx) = pending.resp_tx.take() {
                    let _ = tx.send(resp);
                }
                s.pending_ai_run = None;
            } else if Instant::now() >= pending.deadline {
                let output = s.terminal.peek_ai_output().unwrap_or_default();
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
                    Some(p) if p.run_id == run_id => {
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
                let _ = self.sessions[idx]
                    .cmd_tx
                    .send(UiCommand::TerminalInput(vec![0x03]));
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
        }
    }
}
