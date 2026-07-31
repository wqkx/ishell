//! AI/MCP 控制通道的本地线协议：iShell 主进程与独立的 `ishell-mcp` stdio 代理进程之间，
//! 经 Unix domain socket 传输的请求/响应类型。一次 socket 连接 = 一问一答（换行分隔的 JSON），
//! 不做多路复用——**唯一的例外**是 v4 的配对握手（`PairHello`→`PairProve`，一条连接两问两答），
//! 因为双向证明必须绑定同一对随机数，拆成两条连接就绑不住了。本文件被 `main.rs` 和
//! `src/bin/mcp_stdio.rs` 各自 `include!` 一份，避免为共享这几个类型而拆出独立的 lib crate。

use serde::{Deserialize, Serialize};

/// 分块字节流的一块最多这么大。纯粹是缓冲区尺寸，读端不依赖它——每块的长度都在线上写明。
pub const STREAM_CHUNK_BYTES: usize = 128 * 1024;

/// 写一段**分块字节流**：每块 `<len>\n` + len 字节，最后以 `0\n` 收尾。
///
/// `CopyFromRemoteToCaller` 的响应体用这个格式：header JSON 行 → 分块字节流 → 一行判定 JSON。
///
/// 为什么要分帧，而不是「按 header 里的 size 裸发 size 字节」：**判定必须能被对端找到**。
/// 裸字节流没有边界，读端只能靠 size 去数——可发送端中途失败时（用户取消、远端文件被截断、
/// SFTP 出错）只发得出 M < size 字节，读端却还在按 size 收，于是紧随其后的那行判定会被当成
/// 文件字节一起吞掉，读端只能报一句笼统的「数据不完整」，真正的原因彻底丢失。有了显式边界，
/// 判定的位置就和「实际发了多少字节」无关了。
///
/// 收尾的 `0\n` 只表示「字节流到此为止」，**不表示成功**——成功与否一律由后面那行判定说了算。
pub async fn write_framed_stream<R, W>(src: &mut R, w: &mut W) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0_u8; STREAM_CHUNK_BYTES];
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        w.write_all(format!("{n}\n").as_bytes()).await?;
        w.write_all(&buf[..n]).await?;
    }
    w.write_all(b"0\n").await
}

/// 读一段 `write_framed_stream` 写出来的分块字节流，写进 `dest`，返回总字节数。
///
/// 读到 `0\n` 就停，`r` 停在判定行的开头。只有代理侧用得到（GUI 侧是写端），但格式定义必须
/// 和写端待在同一个文件里——两侧各自手写一遍循环、靠「都记得对方怎么写的」来保持一致，正是
/// 这类协议出静默 bug 的地方。
#[allow(dead_code)] // 只有 ishell-mcp 那个 crate 用；本文件被两个 crate 各编一遍
pub async fn read_framed_stream<R, W>(r: &mut R, dest: &mut W) -> Result<u64, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut total = 0_u64;
    loop {
        let mut len_line = String::new();
        r.read_line(&mut len_line)
            .await
            .map_err(|e| format!("接收文件流失败：{e}"))?;
        let n: u64 = len_line
            .trim()
            .parse()
            .map_err(|_| format!("分块长度无法解析：{len_line:?}"))?;
        if n == 0 {
            return Ok(total);
        }
        let mut chunk = r.take(n);
        let copied = tokio::io::copy(&mut chunk, dest)
            .await
            .map_err(|e| format!("接收文件流失败：{e}"))?;
        if copied != n {
            return Err(format!(
                "文件流在一个分块中途断了：这块应有 {n} 字节，只收到 {copied} 字节"
            ));
        }
        total += n;
    }
}

// ---------- MCP 配对握手（v4）：双向挑战-应答 ----------
//
// 解决的问题：v3 让代理把 `ISHELL_MCP_TOKEN` **明文发给每一条候选 socket**（见
// `candidate_paths`，共用账号时那个目录里躺着别人的转发 socket）。方向虽然从"服务器公布
// 密钥"翻成了"调用方出示密钥"，但向一个**未经认证的验证方**出示 bearer secret 同样是泄露，
// 而且是一次泄露给所有验证方。改成挑战-应答后，线上只出现 `HMAC(token, nonce)`，token 本身
// 永不过线。
//
// 为什么必须**双向**、且服务器先证：只让客户端出示证明的话，一个假 socket 完全不需要知道
// token 就能把代理钓过来——它照答 `PairChallenge`、照收证明、照回 Ok，代理就绑到了它身上。
// （靠"真假两个实例 → 弹窗选"兜不住：实例 id 靠普通 `Identify` 就能问到，假 socket 冒用真
// id，`bind_instance` 按 id 去重后只剩一条，而同机假 socket 必然赢过 SSH 反向转发的真
// socket。）所以先由服务器出示 `Server` 证明，代理**验过才**发自己的 `Client` 证明。
//
// ⚠ 残留风险（明说，不粉饰）：同账号的攻击者若在受害者的代理与其真 iShell 之间**实时中继**
// 双方的挑战与证明，仍可冒充。这在这类 socket 上无解——没有信道绑定可用，`SO_PEERCRED`
// 在"大家共用同一个 uid"的威胁模型下也毫无意义。本次修的是**被动窃取一个永久有效的静态
// 密钥**，攻击门槛从"捡一次密钥、以后随便用"抬到"在正确的时刻跑一个实时中继"。

/// 握手中出示证明的角色。两个方向的证明必须**互不可换**——否则攻击者把服务器发来的证明
/// 原样反射回去就能冒充客户端（经典反射攻击）。下面的域分隔标签就是堵这个的，别去掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRole {
    Server,
    Client,
}

impl PairRole {
    fn tag(self) -> &'static str {
        match self {
            PairRole::Server => "S",
            PairRole::Client => "C",
        }
    }
}

/// 生成握手随机数（32 字节 → 64 位十六进制）。
///
/// **熵源失败一律返回 `None`，绝不退化成时间戳**——这一点刻意与 `store::mcp_instance_id`
/// 相反：那个是用来给同机多开去重的后缀，可预测也只是撞车；这个是安全随机数，可预测的
/// nonce 会让挑战-应答退化成一句可重放的静态口令，等于白改。拿到 `None` 的一方必须中止握手。
#[allow(dead_code)] // 两个 crate 各编一遍，各自只用得到其中一部分
pub fn pair_nonce() -> Option<String> {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 计算某一方的配对证明：`HMAC-SHA256(token, "<role>:<nonce_c>:<nonce_s>")`，十六进制。
///
/// 两个 nonce 都进 MAC：只带服务器的 nonce，客户端就无法确认对面是"这一次"在答话；只带
/// 客户端的 nonce，服务器同理。
#[allow(dead_code)]
pub fn pair_proof(token: &str, nonce_c: &str, nonce_s: &str, role: PairRole) -> String {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(token.as_bytes())
        .expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(format!("{}:{nonce_c}:{nonce_s}", role.tag()).as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 常量时间比对收到的证明。
#[allow(dead_code)]
pub fn pair_proof_matches(
    token: &str,
    nonce_c: &str,
    nonce_s: &str,
    role: PairRole,
    got: &str,
) -> bool {
    let want = pair_proof(token, nonce_c, nonce_s, role);
    let (a, b) = (want.as_bytes(), got.as_bytes());
    // 逐字节 OR 累积差异、**不提前返回**：`==` 会在首个不同的字节处短路，攻击者据此可以
    // 一个字节一个字节地把正确证明试出来。长度本身是公开常量（固定 64 位十六进制），
    // 先比长度不泄露任何信息。
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |d, (x, y)| d | (x ^ y)) == 0
}

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
    ///
    /// **绝不**在响应里回传配对 token（见 `McpReqResult::Instance::token`）：反向转发后任何
    /// 能连上该 socket 的人都能发 Identify，泄露 token 等于让对方跳过多机选择弹窗、静默绑
    /// 定到你的电脑。
    Identify,
    /// **已废弃（v3 的配对方式）**：把配对 token 明文发给对端。v4 起代理不再发送它——
    /// 明文出示密钥给一个尚未认证的对端，本身就是泄露（见本文件「配对握手」一节）。
    ///
    /// 变体保留、且 v4 的 GUI 仍会答话（但**忽略 token**、不做任何配对判定），纯粹是为了让
    /// v3 的旧代理能走到它自己的版本校验、打印「请重新部署 ishell-mcp」而不是误报成
    /// 「token 不匹配」。不构成泄露：它回的 id 与版本，普通 `Identify` 本来就照给。
    IdentifyPair {
        token: String,
    },
    /// 配对握手第一步（代理 → GUI）：代理送上自己的随机数，请对端先证明它知道配对 token。
    ///
    /// 对端答 [`McpReqResult::PairChallenge`]（含它的随机数与 `Server` 证明），代理**验过
    /// 之后**才在**同一条连接**上发第二行 [`McpReqKind::PairProve`]。
    ///
    /// 这是全协议里唯一一处「一条连接两问两答」，与本文件开头那条「一次连接 = 一问一答」的
    /// 约定不同——双向证明必须共享同一对随机数，拆成两条连接就没法把两次交换绑在一起了。
    PairHello {
        nonce_c: String,
    },
    /// 配对握手第二步（代理 → GUI，同一条连接的第二行）：代理出示自己的 `Client` 证明。
    /// 对端验过回 `Instance`，不符回 `Err`。
    ///
    /// 不带随机数：这一步必须用**本连接第一步**约定的那对随机数，让调用方在这里重新报一遍
    /// 等于允许它自选挑战，双向证明就白做了。
    PairProve {
        client_proof: String,
    },
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
            | McpReqKind::IdentifyPair { .. }
            | McpReqKind::PairHello { .. }
            | McpReqKind::PairProve { .. }
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
            McpReqKind::IdentifyPair {
                token: "x".into(),
            },
            McpReqKind::PairHello {
                nonce_c: "n".into(),
            },
            McpReqKind::PairProve {
                client_proof: "p".into(),
            },
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
    /// 只有 `Identify` / `IdentifyPair` 允许填 `None`（那时还不知道对面是谁）。其余请求带
    /// `None` 一律拒绝：「没点名」不等于「随便谁都行」，那正是要根除的静默走错实例。
    pub instance: Option<String>,
    pub kind: McpReqKind,
}

impl McpRequest {
    /// 这条请求是不是该由标识为 `own_instance` 的实例来执行。
    ///
    /// 规则只有三条，故意写得极简——这是隔离的最后一道闸，越简单越查得清：
    ///   1. `Identify` / `IdentifyPair` / `PairHello` / `PairProve` 永远放行：用来问「你是谁」
    ///      或走配对握手，那时对方还填不出名字；
    ///   2. 点名点中自己 → 放行；
    ///   3. 其余（点了别人、或压根没点名）→ 拒绝。
    ///
    /// 第 3 条里「没点名也拒绝」是关键：把 `None` 当成「随便谁都行」正是要根除的那个
    /// bug——今天代理按 socket 文件的 mtime 挑一个实例，挑中谁纯属偶然，而多开时
    /// 每个实例的会话 uid 都从 1 开始，走错实例不会报错，只会安静地操作错的机器。
    pub fn is_addressed_to(&self, own_instance: &str) -> bool {
        match (&self.kind, &self.instance) {
            (
                McpReqKind::Identify
                | McpReqKind::IdentifyPair { .. }
                | McpReqKind::PairHello { .. }
                | McpReqKind::PairProve { .. },
                _,
            ) => true,
            (_, Some(named)) => named == own_instance,
            (_, None) => false,
        }
    }
}

/// 分帧字节流的读写是一对必须严丝合缝的实现，而它们分别跑在两个进程里——出了分歧不会有人
/// 报错，只会是「文件内容不对」或「判定读成了乱码」。这个文件被两个 crate 各编一遍，所以
/// 这些往返测试也会在两侧各跑一遍。
#[cfg(test)]
mod framing_tests {
    use super::*;

    /// 往返：写进去什么，读出来就该是什么，一个字节不差。
    async fn round_trip(payload: &[u8]) -> (u64, Vec<u8>) {
        let mut wire = Vec::new();
        write_framed_stream(&mut &payload[..], &mut wire).await.expect("写不该失败");
        let mut got = Vec::new();
        let total = read_framed_stream(&mut std::io::Cursor::new(wire), &mut got)
            .await
            .expect("读不该失败");
        (total, got)
    }

    #[tokio::test]
    async fn round_trips_payloads_of_every_shape() {
        for payload in [
            Vec::new(),                                 // 空文件：只有一个 `0\n`
            b"hello".to_vec(),                          // 单块
            vec![0xAB; STREAM_CHUNK_BYTES],             // 恰好一块
            vec![0xCD; STREAM_CHUNK_BYTES + 1],         // 跨块边界
            vec![0x00; STREAM_CHUNK_BYTES * 2 + 7],     // 多块 + 零字节（不能被当成结束符）
            b"12\n34\n0\n".to_vec(),                    // 内容长得像帧头：必须靠长度而非内容分帧
        ] {
            let (total, got) = round_trip(&payload).await;
            assert_eq!(total, payload.len() as u64, "长度不符（payload {} 字节）", payload.len());
            assert_eq!(got, payload, "内容不符（payload {} 字节）", payload.len());
        }
    }

    /// 收尾的 `0\n` 之后，读端必须**恰好**停在判定行开头——判定能不能被找到，全靠这一点。
    #[tokio::test]
    async fn leaves_the_reader_exactly_at_the_verdict_line() {
        use tokio::io::AsyncBufReadExt;
        let mut wire = Vec::new();
        write_framed_stream(&mut &b"body"[..], &mut wire).await.unwrap();
        wire.extend_from_slice(b"{\"id\":1,\"result\":{\"Ok\":\"Ok\"}}\n");

        let mut r = std::io::Cursor::new(wire);
        let mut got = Vec::new();
        read_framed_stream(&mut r, &mut got).await.unwrap();
        assert_eq!(got, b"body");

        let mut verdict = String::new();
        r.read_line(&mut verdict).await.unwrap();
        let parsed: McpResponse = serde_json::from_str(verdict.trim()).expect("判定应能原样解析出来");
        assert!(parsed.result.is_ok());
    }

    /// 流在分块中途断掉：必须报错，绝不能把半截内容当成读完了。
    #[tokio::test]
    async fn refuses_a_stream_cut_mid_chunk() {
        let mut got = Vec::new();
        let err = read_framed_stream(&mut std::io::Cursor::new(b"10\nshort".to_vec()), &mut got)
            .await
            .expect_err("分块只有 5 字节却声明 10 字节，必须报错");
        assert!(err.contains("中途断了"), "错误信息应指明是分块断流：{err}");
    }

    /// 帧头是垃圾（比如把裸字节流喂进来）：报错，而不是把它当数据读。
    #[tokio::test]
    async fn refuses_a_garbled_chunk_header() {
        let mut got = Vec::new();
        let err = read_framed_stream(&mut std::io::Cursor::new(b"not-a-number\n".to_vec()), &mut got)
            .await
            .expect_err("帧头无法解析时必须报错");
        assert!(err.contains("分块长度无法解析"), "{err}");
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
        assert!(req(
            None,
            McpReqKind::IdentifyPair {
                token: "t".into()
            }
        )
        .is_addressed_to("me"));
        // 配对握手的两步同理：那时代理还没问出 id，填不了 instance。
        assert!(req(
            None,
            McpReqKind::PairHello {
                nonce_c: "n".into()
            }
        )
        .is_addressed_to("me"));
        assert!(req(
            None,
            McpReqKind::PairProve {
                client_proof: "p".into()
            }
        )
        .is_addressed_to("me"));
        // 连 Bind 都不例外：代理只会在 Identify 问出 id 之后才发 Bind，填得出名字。
        assert!(!req(None, McpReqKind::Bind).is_addressed_to("me"));
        assert!(req(Some("me"), McpReqKind::Bind).is_addressed_to("me"));
    }
}

/// MCP 线协议版本。GUI(`ishell`)与代理(`ishell-mcp`)把本文件各编一遍，二者的线协议必须
/// 配套；**任何会改变线格式/语义的改动都要给这个数 +1**。代理在 `Identify` 时拿到 GUI 的版本，
/// 与自身比对，不一致即拒绝并提示重新部署——根治「升了 GUI 忘换代理 → 静默错」这类问题。
///
/// v2：`McpReqResult::Instance` 新增 `token` 字段（多机配对，见 `store::mcp_pairing_token`）。
/// v3：新增 `IdentifyPair`；`Instance.token` **不再回传真实配对 token**（恒为空），配对改由
/// 调用方出示 token 证明，避免反向转发 socket 上的 Identify 把密钥泄露给同机其它人。
/// v4：配对改为**双向挑战-应答**（`PairHello`/`PairProve` + `PairChallenge`），token 本身
/// 不再过线；`IdentifyPair` 降为「只为让 v3 旧代理走到版本校验」的兼容答话，不再做配对判定。
///
/// 注意 `Identify` 的线格式在所有版本里**逐字节相同**（无字段的单元变体），这是刻意的：
/// 它是唯一一个跨版本都解得开的请求，版本不一致时全靠它问出对端版本、给出「重新部署」的
/// 提示。给它加字段会把 JSON 从 `"Identify"` 变成 `{"Identify":{…}}`，旧端直接解析失败、
/// 被当成死 socket 跳过，于是版本不匹配又会伪装成别的错误——别加。
pub const MCP_PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpReqResult {
    /// `Identify` 的结果：对端 iShell 的实例标识，代理据此去重、认人、填进后续每条请求的
    /// `McpRequest::instance`。纯内部标识，不面向用户。
    Instance {
        id: String,
        /// 对端 iShell 的 MCP 线协议版本（`MCP_PROTOCOL_VERSION`）。旧版 iShell 不带此字段
        /// → serde 默认落 0 → 必与当前版本不符 → 触发代理侧的「重新部署」提示（正是所需）。
        #[serde(default)]
        proto_version: u32,
        /// **已废弃（v3+ 恒为空）**。v2 曾在此回传配对 token，但反向转发后任何人都能
        /// `Identify` 读走密钥并静默绑定——v3 起真实配对改走 `IdentifyPair`（调用方出示
        /// token）。字段保留仅为线格式兼容；代理不得再依赖此字段做过滤。
        #[serde(default)]
        token: String,
    },
    /// `PairHello` 的应答：对端的随机数 + 它的 `Server` 证明。代理**先验这个证明**，
    /// 通过了才在同一条连接上发 `PairProve`——否则一个连 token 都不知道的假 socket 就能把
    /// 代理钓过去（见本文件「配对握手」一节）。
    ///
    /// 同时带上 `id`/`proto_version`：握手成功后代理正好拿它们去重与校验版本，不必再问一遍。
    /// 这两项本来就是 `Identify` 对任何人都给的，放在证明之前不构成额外泄露。
    PairChallenge {
        id: String,
        proto_version: u32,
        nonce_s: String,
        server_proof: String,
    },
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

    /// v3：Instance.token 仅作线格式兼容字段（恒空）；旧 JSON 缺省该字段时也落成空串。
    #[test]
    fn instance_token_field_defaults_empty() {
        let inst = McpReqResult::Instance {
            id: "1234-abcd".into(),
            proto_version: MCP_PROTOCOL_VERSION,
            token: String::new(),
        };
        let json = serde_json::to_string(&inst).unwrap();
        let back: McpReqResult = serde_json::from_str(&json).unwrap();
        match back {
            McpReqResult::Instance {
                id,
                proto_version,
                token,
            } => {
                assert_eq!(id, "1234-abcd");
                assert_eq!(proto_version, MCP_PROTOCOL_VERSION);
                assert_eq!(token, "");
            }
            other => panic!("应解析回 Instance，实际：{other:?}"),
        }
        let legacy = r#"{"Instance":{"id":"x","proto_version":1}}"#;
        match serde_json::from_str::<McpReqResult>(legacy).unwrap() {
            McpReqResult::Instance { token, .. } => assert_eq!(token, ""),
            other => panic!("应解析回 Instance，实际：{other:?}"),
        }
    }

    /// 配对证明的正路：同一个 token + 同一对随机数 + 同一个角色 → 对得上。
    #[test]
    fn pair_proof_verifies_for_the_matching_token() {
        let p = pair_proof("tok", "NC", "NS", PairRole::Server);
        assert!(pair_proof_matches("tok", "NC", "NS", PairRole::Server, &p));
        assert!(!pair_proof_matches("other", "NC", "NS", PairRole::Server, &p));
    }

    /// 换掉任何一个随机数，证明都必须失效——否则挑战-应答退化成可重放的静态口令，
    /// 攻击者抓一次就能永久冒充。
    #[test]
    fn pair_proof_is_bound_to_both_nonces() {
        let p = pair_proof("tok", "NC", "NS", PairRole::Client);
        assert!(!pair_proof_matches("tok", "NC2", "NS", PairRole::Client, &p));
        assert!(!pair_proof_matches("tok", "NC", "NS2", PairRole::Client, &p));
    }

    /// 反射攻击：把服务器的证明原样当成客户端的证明送回去，必须不通过。
    /// 这正是 `PairRole` 的域分隔标签存在的唯一理由——去掉它这条会挂。
    #[test]
    fn server_proof_cannot_be_reflected_as_the_client_proof() {
        let server = pair_proof("tok", "NC", "NS", PairRole::Server);
        assert!(
            !pair_proof_matches("tok", "NC", "NS", PairRole::Client, &server),
            "服务器证明被当成客户端证明接受了——域分隔标签丢了"
        );
        assert_ne!(server, pair_proof("tok", "NC", "NS", PairRole::Client));
    }

    /// 随机数每次都不一样，且长度固定（常量时间比对依赖这一点）。
    #[test]
    fn pair_nonce_is_fresh_and_fixed_width() {
        let (a, b) = (pair_nonce().expect("熵源应可用"), pair_nonce().expect("熵源应可用"));
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "两次生成的随机数相同——熵源有问题");
    }

    /// 畸形/截断的证明不能因为长度不同就 panic 或误判。
    #[test]
    fn malformed_proofs_are_rejected_not_panicking() {
        for got in ["", "zz", &"a".repeat(63), &"a".repeat(65)] {
            assert!(!pair_proof_matches("tok", "NC", "NS", PairRole::Server, got));
        }
    }

    #[test]
    fn identify_pair_round_trips() {
        let kind = McpReqKind::IdentifyPair {
            token: "aabbccdd".into(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: McpReqKind = serde_json::from_str(&json).unwrap();
        match back {
            McpReqKind::IdentifyPair { token } => assert_eq!(token, "aabbccdd"),
            other => panic!("应解析回 IdentifyPair，实际：{other:?}"),
        }
    }

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
