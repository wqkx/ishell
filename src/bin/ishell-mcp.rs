//! iShell 的 AI/MCP 控制通道代理：Claude Code 等 AI 客户端按 stdio 方式 spawn 的独立小进程。
//! 本身不持有任何状态，只是把工具调用转发到本机正在运行的 iShell 主进程（经本地 Unix
//! domain socket，一次连接一问一答），主进程再落到它已经持有的 SSH 会话上执行。
//! 与主二进制共享同一份线协议类型（见 src/mcp_protocol.rs），这里用 #[path] 直接纳入，
//! 避免为共享几个 struct 拆出独立的 lib crate。

#[path = "../mcp_protocol.rs"]
mod mcp_protocol;

use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use mcp_protocol::{McpReqKind, McpReqResult, McpRequest, McpResponse};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn socket_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME 环境变量未设置");
    std::path::PathBuf::from(home)
        .join(".config")
        .join("ishell")
        .join("mcp.sock")
}

/// 连接 iShell 主进程的本地 socket，发一条请求、等一行 JSON 响应（一次连接=一问一答）。
async fn call(kind: McpReqKind) -> Result<McpReqResult, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = socket_path();
    let stream = UnixStream::connect(&path).await.map_err(|_| {
        "连不上 iShell（未运行，或未在设置里开启「允许 AI 通过 MCP 控制终端」）".to_string()
    })?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&McpRequest { id, kind }).map_err(|e| e.to_string())?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(r);
    let mut resp_line = String::new();
    reader
        .read_line(&mut resp_line)
        .await
        .map_err(|e| e.to_string())?;
    let resp: McpResponse = serde_json::from_str(resp_line.trim()).map_err(|e| e.to_string())?;
    resp.result
}

fn text_result(body: Result<McpReqResult, String>) -> Result<CallToolResult, McpError> {
    let text = match body {
        Ok(r) => serde_json::to_string_pretty(&r).unwrap_or_else(|e| e.to_string()),
        Err(e) => format!("error: {e}"),
    };
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

#[derive(Debug, Clone)]
pub struct IshellMcp;

fn default_timeout_ms() -> u64 {
    15_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunCommandArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
    /// 要在该终端里执行的 shell 命令（会像用户手动输入一样实时显示在终端里）
    pub command: String,
    /// 等待命令结束的超时毫秒数；超时仍未结束会返回 finished=false + run_id，可用 poll_run 续等
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PollRunArgs {
    pub session_uid: u64,
    /// run_command / 上一次 poll_run 返回的 run_id
    pub run_id: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
}

#[tool_router]
impl IshellMcp {
    pub fn new() -> Self {
        Self
    }

    #[tool(description = "列出 iShell 当前打开的所有终端会话（uid、标题、主机、连接状态、远端工作目录）")]
    async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::ListSessions).await)
    }

    #[tool(
        description = "在指定终端会话里运行一条命令，等待其执行完成（或超时）后返回输出与退出码。\
                        命令和输出会实时显示在用户正在看的那个终端标签里，效果等同于用户亲自输入。"
    )]
    async fn run_command(
        &self,
        Parameters(RunCommandArgs {
            session_uid,
            command,
            timeout_ms,
        }): Parameters<RunCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(
            call(McpReqKind::RunCommand {
                session_uid,
                command,
                timeout_ms,
            })
            .await,
        )
    }

    #[tool(description = "继续等待一次因超时而未完成的 run_command，不会重新发送命令，可反复调用直到 finished=true")]
    async fn poll_run(
        &self,
        Parameters(PollRunArgs {
            session_uid,
            run_id,
            timeout_ms,
        }): Parameters<PollRunArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(
            call(McpReqKind::PollRun {
                session_uid,
                run_id,
                timeout_ms,
            })
            .await,
        )
    }

    #[tool(
        description = "读取指定终端当前可见屏幕的纯文本内容（类似 tmux capture-pane），\
                        用于查看正在运行的交互式程序（如 vim/top/一个未结束的长任务）而不必等它退出"
    )]
    async fn read_screen(
        &self,
        Parameters(SessionArgs { session_uid }): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::ReadScreen { session_uid }).await)
    }

    #[tool(description = "向指定终端发送 Ctrl+C，用于中断一个卡住或不需要的命令")]
    async fn interrupt(
        &self,
        Parameters(SessionArgs { session_uid }): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::Interrupt { session_uid }).await)
    }
}

#[tool_handler(
    instructions = "驱动本机 iShell 已打开的终端会话（而不是另开一条无上下文的 ssh 连接）：\
                    list_sessions 看有哪些会话 → run_command 执行命令并等待完成 → \
                    超时用 poll_run 续等 → read_screen 看当前屏幕（适合交互式程序）→ \
                    interrupt 发 Ctrl+C。命令会实时显示在用户正在看的终端标签里。"
)]
impl ServerHandler for IshellMcp {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = IshellMcp::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
