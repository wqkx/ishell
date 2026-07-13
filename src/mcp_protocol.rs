//! AI/MCP 控制通道的本地线协议：iShell 主进程与独立的 `ishell-mcp` stdio 代理进程之间，
//! 经 Unix domain socket 传输的请求/响应类型。一次 socket 连接 = 一问一答（换行分隔的 JSON），
//! 不做多路复用。本文件被 `main.rs` 和 `src/bin/mcp_stdio.rs` 各自 `include!` 一份，
//! 避免为共享这几个类型而拆出独立的 lib crate。

use serde::{Deserialize, Serialize};

/// 单个终端会话的摘要（`list_sessions` 的返回项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSessionInfo {
    pub uid: u64,
    pub title: String,
    pub host: String,
    pub connected: bool,
    /// 远端当前工作目录（需用户已同意过 OSC 7 注入才有值）。
    pub cwd: Option<String>,
}

/// 一条已保存连接的摘要（`list_saved_connections` 的返回项）。不含密码/密钥等敏感字段——
/// 只用于让 AI 在 `open_session` 之前确认有哪些名字可用，不需要也不应该看到凭据本身。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSavedConn {
    pub name: String,
    pub host: String,
    pub username: String,
    pub port: u16,
}

/// 一次 `run_command`/`poll_run` 的执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRunResult {
    /// 本次运行的 id：未在超时前完成时，用它继续 `poll_run`。
    pub run_id: u64,
    /// 是否已经跑完（false 表示超时，仍在后台继续跑）。
    pub finished: bool,
    /// 命令产生的输出（已剥离 ANSI 转义与注入用的哨兵行）。
    pub output: String,
    /// 退出码；`finished=false` 时为 None。
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpReqKind {
    ListSessions,
    RunCommand {
        session_uid: u64,
        command: String,
        timeout_ms: u64,
    },
    PollRun {
        session_uid: u64,
        /// 省略即续等这个会话当前唯一挂起的那条运行（同一会话同一时刻只允许一条，
        /// 不会有歧义）；传了就额外校验是否对得上，防止误续等一条已经不相关的旧运行。
        run_id: Option<u64>,
        timeout_ms: u64,
    },
    ReadScreen {
        session_uid: u64,
    },
    Interrupt {
        session_uid: u64,
    },
    /// 用一个已保存的连接（按名称）开一个新会话/标签，等价于用户在侧栏双击那条已保存连接。
    OpenSession {
        /// `SavedConnection.name`（侧栏里显示的那个名字）
        name: String,
    },
    /// 关闭一个会话/标签。只允许关闭 AI 自己用 `OpenSession` 开的会话，不能关用户的。
    CloseSession {
        session_uid: u64,
    },
    /// 读取完整历史（回滚缓冲区 + 当前可见屏），不止 `ReadScreen` 那样只看当前一屏。
    ReadHistory {
        session_uid: u64,
        /// 只要最后这么多行；0 = 不限制（可能很长）。
        max_lines: u64,
    },
    /// 列出所有已保存连接（名称/主机/用户名/端口，不含凭据），供 `open_session` 前核对名字用。
    ListSavedConnections,
    /// 直接发送原始文本/按键，不等待、不做完成检测——用于 `RunCommand` 覆盖不到的交互式
    /// 场景（sudo 密码提示、vim/REPL 里继续输入等）。不会自动加回车。
    SendInput {
        session_uid: u64,
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRequest {
    pub id: u64,
    pub kind: McpReqKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpReqResult {
    Sessions(Vec<McpSessionInfo>),
    Run(McpRunResult),
    Screen(String),
    Ok,
    /// `OpenSession` 成功后新建会话的摘要（同 `McpSessionInfo`）。
    Opened(McpSessionInfo),
    /// `ReadHistory` 的结果：完整历史文本（已按 `max_lines` 截断）。
    History(String),
    /// `ListSavedConnections` 的结果。
    SavedConnections(Vec<McpSavedConn>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResponse {
    pub id: u64,
    /// `Err` 携带人类可读的错误信息（会话不存在、已有命令在跑、socket 未连上等）。
    pub result: Result<McpReqResult, String>,
}
