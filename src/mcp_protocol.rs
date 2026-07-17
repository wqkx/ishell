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
    /// 这个会话是不是 AI 自己用 `open_session` 开的。
    ///
    /// `false` = 用户自己打开的会话。这类会话用户本人随时可能在里面敲字，AI 再往同一个
    /// shell 里写就是两路输入交织：轻则互相打断，重则把 `run_command` 用来判断命令结束的
    /// 哨兵标记行搅乱。所以写入类操作（见 `McpReqKind::write_target_uids`）默认走不通，
    /// 需要用户当面授权一次。调用方应当优先 `open_session` 开自己的专用会话。
    pub ai_owned: bool,
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
    /// 询问对端 iShell 的实例标识。这是**唯一**允许 `McpRequest::instance` 为 `None` 的请求：
    /// 代理进程在还不知道对面是谁的时候，只能先问一句。
    ///
    /// 为什么身份必须靠问、不能靠 socket 路径推断：反向转发出来的 socket 路径每次 SSH
    /// 重连都会换一个随机名字（这是刻意的，见 `src/ssh/mod.rs` 里的说明——固定路径会被
    /// 服务器当成尚未失效的旧注册而拒绝），而且同一个 iShell 对同一台远端主机开两个会话时，
    /// 会在那台主机上注册出**两个通向同一个实例**的 socket。所以路径既不稳定、也不唯一，
    /// 代理必须按这里返回的 id 去重和认人。
    Identify,
    /// 请求把发起方这个 AI 客户端绑定到本 iShell 实例——弹窗让用户当面确认。
    ///
    /// 只在代理发现**多个不同实例**时才会发出：代理向每一个实例各发一条，于是每个 iShell
    /// 窗口上都会弹出确认框，用户在想用的那个窗口上点「允许」即可。代理拿到第一个 `Ok`
    /// 之后就挂断其余连接，落选窗口的弹窗随之自动消失（GUI 侧靠对端连接关闭来感知）。
    ///
    /// 选择之所以做成「点窗口」而不是「报出实例名让用户填配置」：用户本来就是看着窗口决定
    /// 的，实例 id 是纯内部标识，不该出现在任何 UI 或配置里。
    Bind,
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
    /// 把文本内容写入远端指定路径（存在则直接覆盖，不做外部改动冲突检测——这条通道只给
    /// AI 自己用）。复用 iShell 编辑器已有的 SFTP 写入通路，不用另开一条 scp。
    WriteFile {
        session_uid: u64,
        path: String,
        /// 文本内容（UTF-8），按 LF 换行写入
        content: String,
        timeout_ms: u64,
    },
    /// 读取远端指定路径的文本文件内容（自动探测编码，行尾统一为 LF）。
    ReadFile {
        session_uid: u64,
        path: String,
        /// false（默认）：遵守 20MB 软上限、拒绝二进制内容（含 NUL 字节直接报错，不强行当
        /// 文本解码）；true：放宽到 128MB，且跳过二进制检测——确实需要读大文件/强制当文本
        /// 读时才应该传 true，否则读到二进制文件会得到乱码而不是清楚的报错。
        force: bool,
        timeout_ms: u64,
    },
    /// 把本地文件/目录复制到远端（走 SFTP 上传通道，字节不经过这条 JSON-RPC 连接）——
    /// 大文件/整个目录用这个，不要用 `write_file`（那条路要求把全部内容内联进请求 JSON，
    /// 大文件会撑爆传输层、也很浪费）。
    CopyToRemote {
        session_uid: u64,
        /// 本地绝对路径（文件或目录）
        local_path: String,
        /// 远端目标绝对路径，可以和 `local_path` 的文件名不同（自动改名）
        remote_path: String,
        timeout_ms: u64,
    },
    /// 与 `CopyToRemote` 语义相同，但源文件由运行 `ishell-mcp` 的调用方机器以原始字节流
    /// 紧跟在本请求后发送。这个变体不是 MCP tool 的公开参数，而是代理与 GUI 间的内部协议，
    /// 用来避免把工作机文件全文塞进 JSON/LLM 上下文。
    CopyToRemoteFromCaller {
        session_uid: u64,
        remote_path: String,
        size: u64,
        timeout_ms: u64,
    },
    /// 与 `CopyToRemoteFromCaller` 对称：`copy_from_remote` 工具真正的实现。GUI 侧把远端单文件内容
    /// 通过本条 socket 流回代理进程，由代理进程在自己的机器上落盘到 `local_path`——不是
    /// MCP tool 的公开参数，是代理与 GUI 间的内部协议，用来避免"代理进程本地"和
    /// "GUI 所在机器"这两个不同机器对 `local_path` 的解析点不一致（此前 `CopyFromRemote`
    /// 直接由 GUI 自己写盘，跟 `copy_to_remote` 的解析点对不上）。仅支持单个文件，
    /// 目录场景由 GUI 侧探测后直接回错误。
    CopyFromRemoteToCaller {
        session_uid: u64,
        remote_path: String,
        timeout_ms: u64,
    },
    /// 把一个已打开远端会话（源）上的文件复制到另一个已打开远端会话（目标），两边都是远端
    /// 主机，不经过运行 iShell 的机器落盘（内存中转）。当前仅支持单个文件。
    CopyBetweenSessions {
        src_session_uid: u64,
        src_remote_path: String,
        dest_session_uid: u64,
        dest_remote_path: String,
        timeout_ms: u64,
    },
}

impl McpReqKind {
    /// 本请求会「往 shell 里打字」或「改远端状态」的目标会话 uid。
    ///
    /// 用户自己打开的会话，对这些操作要用户当面一次性授权后才放行；只读类操作不在此列——
    /// 它们既不干扰用户的 shell、也不改远端状态，而「让 AI 看看你会话里出了什么事」本身
    /// 是有用的能力，没必要拦。
    ///
    /// 集中判定放在这里、而不是散在各分支里，是为了让「新加一个工具要不要授权」变成一个
    /// **必须显式回答**的问题：下面的 match 故意不写 `_ =>` 通配，新增变体时编译器会在这里
    /// 报错，逼着作者表态，而不是默默继承「不需要授权」。
    pub fn write_target_uids(&self) -> Vec<u64> {
        match self {
            McpReqKind::RunCommand { session_uid, .. }
            | McpReqKind::SendInput { session_uid, .. }
            | McpReqKind::Interrupt { session_uid }
            | McpReqKind::WriteFile { session_uid, .. }
            | McpReqKind::CopyToRemote { session_uid, .. }
            | McpReqKind::CopyToRemoteFromCaller { session_uid, .. } => vec![*session_uid],
            // 源和目标都要授权：直连模式会往「源」主机落一份临时私钥、往「目标」主机的
            // authorized_keys 里临时写一行，两边都是在改远端状态。
            McpReqKind::CopyBetweenSessions {
                src_session_uid,
                dest_session_uid,
                ..
            } => vec![*src_session_uid, *dest_session_uid],
            // 连接级握手，根本不涉及任何会话（连 session_uid 字段都没有），自然谈不上授权。
            // Bind 本身就是一个弹窗确认，再套一层会话授权既无对象也无意义。
            McpReqKind::Identify
            | McpReqKind::Bind
            // 只读：不往 shell 里发东西，也不改远端。
            | McpReqKind::ListSessions
            | McpReqKind::ListSavedConnections
            | McpReqKind::OpenSession { .. }
            | McpReqKind::PollRun { .. }
            | McpReqKind::ReadScreen { .. }
            | McpReqKind::ReadHistory { .. }
            | McpReqKind::ReadFile { .. }
            | McpReqKind::CopyFromRemoteToCaller { .. } => Vec::new(),
            // CloseSession 走的是更严的门禁（只能关 AI 自己开的，不接受授权——关闭权限不
            // 应该超过打开权限），不走这条授权路。
            McpReqKind::CloseSession { .. } => Vec::new(),
        }
    }
}

#[cfg(test)]
mod write_gate_tests {
    use super::*;

    /// 会往 shell 里打字或改远端状态的操作，必须报出目标会话——漏一个，AI 就能不经用户授权
    /// 插手用户正在用的 shell。
    #[test]
    fn write_ops_report_their_target_session() {
        let cases: Vec<McpReqKind> = vec![
            McpReqKind::RunCommand {
                session_uid: 7,
                command: "rm -rf /tmp/x".into(),
                timeout_ms: 0,
            },
            McpReqKind::SendInput {
                session_uid: 7,
                text: "y\n".into(),
            },
            McpReqKind::Interrupt { session_uid: 7 },
            McpReqKind::WriteFile {
                session_uid: 7,
                path: "/etc/hosts".into(),
                content: String::new(),
                timeout_ms: 0,
            },
            McpReqKind::CopyToRemote {
                session_uid: 7,
                local_path: "/a".into(),
                remote_path: "/b".into(),
                timeout_ms: 0,
            },
            McpReqKind::CopyToRemoteFromCaller {
                session_uid: 7,
                remote_path: "/b".into(),
                size: 1,
                timeout_ms: 0,
            },
        ];
        for kind in cases {
            assert_eq!(kind.write_target_uids(), vec![7], "漏判写入操作：{kind:?}");
        }
    }

    /// 直连模式会往「源」主机落临时私钥、往「目标」主机的 authorized_keys 临时写一行——
    /// 两边都在改远端状态，所以两个 uid 都得授权，不能只拦目标。
    #[test]
    fn cross_session_copy_gates_both_hosts() {
        let kind = McpReqKind::CopyBetweenSessions {
            src_session_uid: 3,
            src_remote_path: "/src".into(),
            dest_session_uid: 9,
            dest_remote_path: "/dst".into(),
            timeout_ms: 0,
        };
        assert_eq!(kind.write_target_uids(), vec![3, 9]);
    }

    /// 只读操作不该要授权：它们不碰用户的 shell、也不改远端，而「让 AI 看看用户会话里出了
    /// 什么事」本身有用。拦了它们只会逼 AI 绕路，并不会更安全。
    #[test]
    fn read_only_ops_need_no_authorisation() {
        let cases: Vec<McpReqKind> = vec![
            McpReqKind::ListSessions,
            McpReqKind::ListSavedConnections,
            McpReqKind::OpenSession { name: "s2".into() },
            McpReqKind::PollRun {
                session_uid: 7,
                run_id: None,
                timeout_ms: 0,
            },
            McpReqKind::ReadScreen { session_uid: 7 },
            McpReqKind::ReadHistory {
                session_uid: 7,
                max_lines: 0,
            },
            McpReqKind::ReadFile {
                session_uid: 7,
                path: "/a".into(),
                force: false,
                timeout_ms: 0,
            },
            McpReqKind::CopyFromRemoteToCaller {
                session_uid: 7,
                remote_path: "/a".into(),
                timeout_ms: 0,
            },
            // 关会话走的是更严的 ai_owned 门禁（只能关 AI 自己开的，不接受授权），不是这条路。
            McpReqKind::CloseSession { session_uid: 7 },
            // 连接级握手，不涉及任何会话。
            McpReqKind::Identify,
            McpReqKind::Bind,
        ];
        for kind in cases {
            assert!(
                kind.write_target_uids().is_empty(),
                "只读操作被误判成需要授权：{kind:?}"
            );
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRequest {
    pub id: u64,
    /// 这条请求点名要发给哪个 iShell 实例（`store::mcp_instance_id()` 的值）。收到的实例
    /// 一旦发现不是在叫自己，直接拒绝执行。
    ///
    /// 这是「一个代理只操作一个 iShell」这条隔离承诺的**唯一硬保证**，而且刻意放在权威侧
    /// （目标进程自己校验），不是放在代理侧。代理的路径探测无论怎么错——缓存的路径失效后
    /// 被另一个实例复用、反向转发目录里混进了别人的 socket——请求都到不了错误的实例身上。
    /// 隔离不能指望发起方自觉。
    ///
    /// 只有 `Identify` 允许填 `None`（那时还不知道对面是谁）。其余请求带 `None` 一律拒绝：
    /// 「没点名」不等于「随便谁都行」，那正是要根除的静默走错实例。
    pub instance: Option<String>,
    pub kind: McpReqKind,
}

impl McpRequest {
    /// 这条请求是不是该由标识为 `own_instance` 的实例来执行。
    ///
    /// 规则只有三条，故意写得极简——这是隔离的最后一道闸，越简单越查得清：
    ///   1. `Identify` 永远放行：它就是用来问「你是谁」的，那时对方当然还填不出名字；
    ///   2. 点名点中自己 → 放行；
    ///   3. 其余（点了别人、或压根没点名）→ 拒绝。
    ///
    /// 第 3 条里「没点名也拒绝」是关键：把 `None` 当成「随便谁都行」正是要根除的那个
    /// bug——今天代理按 socket 文件的 mtime 挑一个实例，挑中谁纯属偶然，而多开时
    /// 每个实例的会话 uid 都从 1 开始，走错实例不会报错，只会安静地操作错的机器。
    pub fn is_addressed_to(&self, own_instance: &str) -> bool {
        match (&self.kind, &self.instance) {
            (McpReqKind::Identify, _) => true,
            (_, Some(named)) => named == own_instance,
            (_, None) => false,
        }
    }
}

#[cfg(test)]
mod addressing_tests {
    use super::*;

    fn req(instance: Option<&str>, kind: McpReqKind) -> McpRequest {
        McpRequest {
            id: 1,
            instance: instance.map(str::to_string),
            kind,
        }
    }

    /// 点名点中自己才执行——这是「一个代理只操作一个 iShell」的硬保证。
    #[test]
    fn only_requests_naming_this_instance_are_executed() {
        let kind = McpReqKind::RunCommand {
            session_uid: 1,
            command: "rm -rf /".into(),
            timeout_ms: 0,
        };
        assert!(req(Some("me"), kind.clone()).is_addressed_to("me"));
        assert!(!req(Some("someone-else"), kind).is_addressed_to("me"));
    }

    /// 不点名 ≠ 随便谁都行。多开时每个实例的会话 uid 都从 1 开始，放行一条没点名的
    /// `RunCommand` 就等于允许命令落到一台完全不相干的机器上，而且不会有任何报错。
    #[test]
    fn unaddressed_requests_are_refused() {
        let kind = McpReqKind::RunCommand {
            session_uid: 1,
            command: "echo hi".into(),
            timeout_ms: 0,
        };
        assert!(!req(None, kind).is_addressed_to("me"));
    }

    /// 唯一的例外：Identify 就是用来问「你是谁」的，此时对方还没法点名。
    #[test]
    fn identify_is_the_only_unaddressed_request_allowed() {
        assert!(req(None, McpReqKind::Identify).is_addressed_to("me"));
        // 连 Bind 都不例外：代理只会在 Identify 问出 id 之后才发 Bind，填得出名字。
        assert!(!req(None, McpReqKind::Bind).is_addressed_to("me"));
        assert!(req(Some("me"), McpReqKind::Bind).is_addressed_to("me"));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpReqResult {
    /// `Identify` 的结果：对端 iShell 的实例标识，代理据此去重、认人、填进后续每条请求的
    /// `McpRequest::instance`。纯内部标识，不面向用户。
    Instance { id: String },
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
    /// `WriteFile` 成功后的新 mtime。
    FileWritten { path: String, mtime: u32 },
    /// `ReadFile` 的结果。
    FileContent { path: String, content: String },
    /// `CopyToRemote`/`CopyFromRemote` 成功后的目标路径。
    Copied { path: String },
    /// `CopyFromRemoteToCaller` 的响应头：先以这一行 JSON 单独送达，代理进程解析出 `size`
    /// 后再从同一条 socket 连接上读取紧随其后的 `size` 字节原始文件内容（无额外分隔符/
    /// trailer）。GUI 侧提前判定失败（远端不存在/是目录等）时仍按普通 `Err` 响应，不会
    /// 发送这个变体，代理进程据此区分两种情况，不需要另外猜测。
    CopyStreamHeader { path: String, size: u64 },
    /// `CopyBetweenSessions` 成功后的目标路径；`method` 目前恒为 `"relay"`（经 iShell 内存
    /// 中转，两端都不落盘）——为直连优先模式预留，届时会出现 `"direct"`。
    CopiedBetweenSessions { path: String, method: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResponse {
    pub id: u64,
    /// `Err` 携带人类可读的错误信息（会话不存在、已有命令在跑、socket 未连上等）。
    pub result: Result<McpReqResult, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_stream_upload_request_round_trips_without_content_field() {
        let request = McpRequest {
            id: 7,
            instance: Some("1234-a1b2c3d4".into()),
            kind: McpReqKind::CopyToRemoteFromCaller {
                session_uid: 11,
                remote_path: "/srv/project/cuda_eri.py".into(),
                size: 95_232,
                timeout_ms: 300_000,
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("content"));
        let decoded: McpRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded.kind,
            McpReqKind::CopyToRemoteFromCaller { size: 95_232, .. }
        ));
    }
}
