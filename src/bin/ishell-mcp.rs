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

/// 找目录下匹配前缀/后缀的文件里，mtime 最新且真的连得上的那一个（探测连接立刻丢弃）。
/// 用于同机发现（`mcp-*.sock`，每进程一个独立文件）和反向转发发现
/// （`.ishell-mcp-*.sock`）——两处逻辑完全一样，抽出来共用。
async fn newest_connectable(dir: &std::path::Path, prefix: &str, suffix: &str) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix))
        })
        .filter_map(|p| p.metadata().and_then(|m| m.modified()).ok().map(|t| (t, p)))
        .collect();
    candidates.sort_by_key(|(t, _)| std::cmp::Reverse(*t)); // 新的在前，优先试
    for (_, p) in candidates {
        if UnixStream::connect(&p).await.is_ok() {
            return Some(p);
        }
    }
    None
}

/// 每次调用都重新探测（而非启动时定死一个路径），因为反向转发的 socket 路径每次 iShell
/// 重连都会换一个带随机后缀的新名字（见 `src/ssh/mod.rs`），本机 socket 路径也是每个 iShell
/// 进程各带一个 pid（见 `src/store/settings.rs::mcp_socket_path`）——固定住任何一个都只会
/// 导致每次重连/重启都要手动改 `ISHELL_MCP_SOCKET`，体验很差。按优先级探测：
///   1. `ISHELL_MCP_SOCKET` 显式指定（手动隧道场景，最明确，始终优先）；
///   2. `~/.config/ishell/mcp-*.sock` 里 mtime 最新且连得上的一个（同机场景：iShell 和这个
///      代理跑在同一台机器上；每个 iShell 进程一个独立文件，不用猜具体 pid）；
///   3. `~/.ishell-mcp-*.sock` 里 mtime 最新且连得上的一个（iShell 反向转发到「这个代理
///      所在的机器」时会落在这里；每次重连都是新文件，取最新的即可自动跟上）。
/// 都没找到时回退到本机默认目录下的固定占位路径，让错误信息保持原样好懂（不会是任何一个
/// 真实存在的进程的路径，纯粹是给 `call()` 里的 connect 失败提供一个统一的报错落点）。
async fn socket_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("ISHELL_MCP_SOCKET") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").expect("HOME 环境变量未设置");
    let config_dir = std::path::PathBuf::from(&home).join(".config").join("ishell");
    if let Some(p) = newest_connectable(&config_dir, "mcp-", ".sock").await {
        return p;
    }
    if let Some(p) = newest_connectable(std::path::Path::new(&home), ".ishell-mcp-", ".sock").await {
        return p;
    }
    config_dir.join("mcp.sock")
}

/// 本地 socket connect/写请求的超时：正常情况下应该是瞬时的（同机 Unix socket，GUI 活着的
/// 话），卡住这么久基本可以断定对端有问题，没必要陪它无限等。
const CONNECT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 等响应的超时：某些调用（run_command/poll_run/write_file/read_file）自带 timeout_ms，
/// GUI 侧已经把它 clamp 到最长 24 小时——这里给一个比那个上限稍宽松的兜底，只用来防
/// "GUI 卡死/半关闭连接导致这次工具调用永远挂起"，不应该在正常使用中被触发。
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25 * 60 * 60);

/// 连接 iShell 主进程的本地 socket，发一条请求、等一行 JSON 响应（一次连接=一问一答）。
async fn call(kind: McpReqKind) -> Result<McpReqResult, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = socket_path().await;
    let stream = tokio::time::timeout(CONNECT_WRITE_TIMEOUT, UnixStream::connect(&path))
        .await
        .map_err(|_| "连接 iShell 本地 socket 超时".to_string())?
        .map_err(|_| {
            "连不上 iShell（未运行，或未在设置里开启「允许 AI 通过 MCP 控制终端」）".to_string()
        })?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&McpRequest { id, kind }).map_err(|e| e.to_string())?;
    line.push('\n');
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, w.write_all(line.as_bytes()))
        .await
        .map_err(|_| "发送请求给 iShell 超时".to_string())?
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(r);
    let mut resp_line = String::new();
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(&mut resp_line))
        .await
        .map_err(|_| "等待 iShell 响应超时（远超请求本身的 timeout_ms，可能是 GUI 卡死或连接异常）".to_string())?
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Err("iShell 未返回任何响应就关闭了连接".into());
    }
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
    /// 可省略：同一会话同一时刻只会有一条挂起的运行，省略就直接续等它，不需要精确转述
    /// run_command 返回的那个长数字 id。传了会做一致性校验（防止误续等一条不相关的旧运行）。
    #[serde(default)]
    pub run_id: Option<u64>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OpenSessionArgs {
    /// 已保存连接的名称（iShell 侧栏里显示的那个名字，不是主机地址）
    pub name: String,
}

fn default_max_lines() -> u64 {
    200
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadHistoryArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
    /// 只要最后这么多行（默认 200，够用再加大）；传 0 表示不限制（回滚很长时可能很大）
    #[serde(default = "default_max_lines")]
    pub max_lines: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendInputArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
    /// 要发送的原始文本/按键，不会自动加回车——要按 Enter 就在末尾加 "\r"
    pub text: String,
}

fn default_file_timeout_ms() -> u64 {
    20_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
    /// 远端绝对路径，已存在会被直接覆盖
    pub path: String,
    /// 文本内容（UTF-8），按 LF 换行写入
    pub content: String,
    #[serde(default = "default_file_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileArgs {
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
    pub path: String,
    /// 默认 false：遵守 20MB 软上限、二进制内容直接报错。true：放宽到 128MB 且把二进制也
    /// 当文本硬解码——只在确实需要读大文件、且确定是文本时才传 true，否则会得到乱码。
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_file_timeout_ms")]
    pub timeout_ms: u64,
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
                        命令和输出会实时显示在用户正在看的那个终端标签里，效果等同于用户亲自输入。\
                        会话还没连上（open_session 刚返回时可能仍在连接/认证中）会直接报错，\
                        不会挂起——遇到这个错误就用 list_sessions 确认 connected 变 true 后再试。\
                        重要限制：这是往一个真实交互 shell 里打字+回车，不是独立执行通道——\
                        前台如果正跑着 vim/top/REPL/sudo 密码提示等非 shell 程序，或者上一条命令\
                        有反斜杠续行、未闭合引号、heredoc 还没结束，这条命令文本会被当成那个\
                        程序/续行的输入吃掉，完成检测可能永远等不到，且可能改动那个程序里的\
                        数据。不确定当前前台状态时，先用 read_screen 看一眼再决定要不要发命令，\
                        或者改用 send_input 应对交互式场景。"
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

    #[tool(
        description = "继续等待一次因超时而未完成的 run_command，不会重新发送命令，可反复调用直到 \
                        finished=true。run_id 可以不填——同一会话同一时刻只会有一条挂起的运行，\
                        直接省略就行，不需要精确转述那个长数字。"
    )]
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

    #[tool(
        description = "用一个已保存的连接（按名称）新开一个终端会话/标签，等价于用户在 iShell 侧栏里\
                        双击这条已保存连接。name 是已保存连接的名字，不是主机地址，也不是 \
                        list_sessions 里的会话标题——不确定具体拼写时先调 list_saved_connections \
                        核对。返回新会话的 uid；此时通常还没连上（connected=false，正在连接/\
                        认证中），直接对它调 run_command 会报错——用 list_sessions 确认 \
                        connected 变 true 后再执行命令。"
    )]
    async fn open_session(
        &self,
        Parameters(OpenSessionArgs { name }): Parameters<OpenSessionArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::OpenSession { name }).await)
    }

    #[tool(
        description = "关闭一个终端会话/标签。只能关闭你自己用 open_session 开的会话（用户自己开的\
                        会话即使有权限操作也不能用这个工具关掉）。不再需要某个 open_session 开的\
                        会话时应该主动关掉，避免一直占着连接。"
    )]
    async fn close_session(
        &self,
        Parameters(SessionArgs { session_uid }): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::CloseSession { session_uid }).await)
    }

    #[tool(
        description = "读取指定终端的完整历史（回滚缓冲区 + 当前可见屏，从最早到最新），\
                        不止 read_screen 那样只看当前一屏——适合回顾这个会话从头到现在都发生了什么。\
                        max_lines 只要最后这么多行（默认 200，避免一次性读回过长历史）。"
    )]
    async fn read_history(
        &self,
        Parameters(ReadHistoryArgs {
            session_uid,
            max_lines,
        }): Parameters<ReadHistoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(
            call(McpReqKind::ReadHistory {
                session_uid,
                max_lines,
            })
            .await,
        )
    }

    #[tool(
        description = "列出所有已保存的连接（名称/主机/用户名/端口，不含密码/密钥），\
                        用在 open_session 之前确认名字拼写对不对、有哪些机器可以连。"
    )]
    async fn list_saved_connections(&self) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::ListSavedConnections).await)
    }

    #[tool(
        description = "往指定终端直接发送原始文本/按键，不等待、不做完成检测——用于 run_command \
                        覆盖不到的交互式场景（sudo 密码提示、vim/REPL 里继续输入等）。发送后配合 \
                        read_screen 看效果。不会自动加回车，要按 Enter 就在 text 末尾加 \"\\r\"。"
    )]
    async fn send_input(
        &self,
        Parameters(SendInputArgs { session_uid, text }): Parameters<SendInputArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::SendInput { session_uid, text }).await)
    }

    #[tool(
        description = "把文本内容写入远端指定路径（存在会被直接覆盖，不做外部改动冲突检测——\
                        这条通道只给你自己用，默认信任调用方）。复用 iShell 编辑器已有的 SFTP \
                        写入通路，用于代码同步、生成文件等场景，不需要再单独走一条 scp。"
    )]
    async fn write_file(
        &self,
        Parameters(WriteFileArgs {
            session_uid,
            path,
            content,
            timeout_ms,
        }): Parameters<WriteFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(
            call(McpReqKind::WriteFile {
                session_uid,
                path,
                content,
                timeout_ms,
            })
            .await,
        )
    }

    #[tool(
        description = "读取远端指定路径的文本文件内容（自动探测编码，行尾统一为 LF）。默认\
                        遵守 20MB 软上限、二进制文件直接报错；确实需要读取更大的文件（最多\
                        128MB）或强制把内容当文本读时传 force=true（否则读到二进制文件只会\
                        得到乱码，不如直接报错清楚）。内容过长时会截断保留末尾部分。"
    )]
    async fn read_file(
        &self,
        Parameters(ReadFileArgs {
            session_uid,
            path,
            force,
            timeout_ms,
        }): Parameters<ReadFileArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(
            call(McpReqKind::ReadFile {
                session_uid,
                path,
                force,
                timeout_ms,
            })
            .await,
        )
    }
}

#[tool_handler(
    instructions = "涉及“在远端服务器上执行命令/查看文件/运行程序”这类需求时，优先用这个\
                    工具集操作 iShell 已打开的终端会话，而不是自己直接跑 `ssh host cmd`——\
                    直接开 ssh 会丢失用户已经建立的会话上下文（cwd、环境变量、shell 历史、\
                    已登录状态），也不会显示给用户看。只有确认 iShell 没在跑、或用户明确要求\
                    你自己开一条独立 ssh 连接时，才退回直接用 ssh。\
                    用法：list_sessions 看有哪些已打开的会话 → 需要的目标不在列表里时先用 \
                    list_saved_connections 核对有哪些已保存连接、名字怎么拼，再用 open_session\
                    （首次使用某条连接会让用户当面确认）新开一个（这类会话只读，仅供你操作，\
                    用户不能往里打字）→ run_command 执行命令并等待完成 → 超时用 poll_run 续等\
                    （不会重发命令）→ 遇到 run_command/poll_run 覆盖不到的交互式提示（sudo 密码、\
                    vim/REPL 里继续输入）用 send_input 直接发原始按键 → read_screen 看当前屏幕\
                    （适合 vim/top 等交互式程序）→ read_history 看这个会话从头到现在的完整历史\
                    （不止当前一屏）→ interrupt 发 Ctrl+C 中断。用 open_session 开的会话不再需要\
                    时应主动用 close_session 关掉（只能关自己开的，不能关用户自己的会话），\
                    避免一直占着连接。用户自己在用的会话里，命令和输出会实时显示在其正在看的\
                    终端标签里。\
                    需要同步/生成远端文件时用 write_file/read_file（复用 SFTP，不用另开 scp）。\
                    长任务不要用 `sleep N` 反复轮询——run_command/poll_run 的 timeout_ms 最长支持\
                    24 小时，直接传一个足够长的值（比如几十分钟），一次调用等到跑完更省事。\
                    如果某次 run_command/poll_run 的返回意外丢失或报错（比如工具调用本身失败），\
                    不确定命令有没有跑完时不要盲目重试有副作用的命令——先用 poll_run（不填 run_id）\
                    去认领那个会话当前挂起的运行，能问出真实状态。\
                    run_command 是往真实交互 shell 打字，不是独立执行通道：前台在跑全屏程序/\
                    REPL，或命令本身语法不完整（未闭合引号、续行、heredoc）时完成检测可能永远\
                    等不到，见 run_command 自己的详细说明。"
)]
impl ServerHandler for IshellMcp {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = IshellMcp::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
