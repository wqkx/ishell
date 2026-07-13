//! AI/MCP 控制通道：本地 Unix domain socket 桥接，供独立的 `ishell-mcp` stdio 代理进程
//! （见 `src/bin/mcp_stdio.rs`）连接，把 list/run/poll/read/interrupt 请求转发到本进程
//! 持有的活跃 SSH 会话。默认关闭（`store::load_mcp_consent()`），一次 socket 连接 = 一问一答。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::mcp_protocol::{McpReqKind, McpReqResult, McpRequest, McpResponse, McpRunResult, McpSessionInfo};
use crate::proto::UiCommand;

use super::App;

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
                let prefix = format!("\x1eAI_DONE_{nonce}:");
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
        }
    }
}
