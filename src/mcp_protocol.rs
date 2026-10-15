//! AI/MCP 控制通道的线协议：iShell 主进程与独立的 `ishell-mcp` stdio 代理进程之间，经 Unix
//! domain socket 传输的请求/响应类型（换行分隔的 JSON）。分两层，各自独立版本化：
//!
//! - **认证层**（`AUTH_VERSION`，冻结）：谁能连、连的是不是「启动我的那个终端所属的 iShell」。
//!   一次握手（一条连接上三问三答）换一张连接凭据，见下面「认证层」一节。
//! - **业务层**（`MCP_PROTOCOL_VERSION`）：握手之后的请求/响应。一次 socket 连接 = 一问一答，
//!   不做多路复用，每条请求带那张凭据。
//!
//! 本文件被 `main.rs` 和 `src/bin/ishell-mcp.rs` 各自 `include!` 一份，避免为共享这几个类型
//! 而拆出独立的 lib crate。

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

// ---------- 认证层（`ishell-mcp/auth/1`）：冻结，不随业务协议升级 ----------
//
// 这一层只回答一个问题：**对面是不是「启动我的那个终端所属的 iShell」**。它与业务协议（下面的
// `McpReqKind` / `McpReqResult`）分开版本化——业务层加功能、改字段都不碰这里；这里的线格式一旦
// 定下就不再变，所以「版本不一致」永远能在认证通过之后、以明确的文字告诉对方，而不必靠一个
// 匿名可问的版本信标。
//
// 身份来自「这个进程是哪个登录 shell 的后代」，不来自扫描、也不来自环境变量：iShell 连上后，在
// 服务器上给这条连接的登录 shell 登记一个只有本人可读的会话文件 `~/.ishell-mcp/sess/<主机名>/<shell pid>`
// （内容：端点路径、本次运行的随机密钥、shell 的 pid 与启动时间）；代理从自己往上找父进程，第一个
// 有合法会话文件的祖先就是它的主人。进程祖先是内核给的、伪造不了的事实：B 终端里启动的 agent，
// 祖先里不可能有 C 的登录 shell。代理只连那一个端点，不列目录、不探测、没有候选，也就没有「选哪个」。
// 不靠环境变量还有一层好处：agent 过滤环境（Codex）、不是紧跟着连接后第一个键（注入时机）、shell 历史
// 里留下密钥，这三类问题一并不存在。
//
// 握手（一条连接上三问三答；密钥永不过线，线上只有 HMAC）：
//   1. 代理 → iShell  `{"hello":{"auth":1,"cn":<随机数>,"mac":HMAC(key,"hello",cn)}}`
//      mac 不对 → iShell **直接断开，一个字节都不回**（匿名者什么也学不到）。
//   2. iShell → 代理  `{"hello_ok":{"sn":<随机数>,"smac":HMAC(key,"server",cn,sn),
//                       "app_min":…,"app_max":…,"caps":[…],"peer":{"host":…,"run":…}}}`
//      代理验 `smac`：对面必须也持有密钥，否则就是别人的 socket，当场放弃、**不出示自己的证明**。
//   3. 代理 → iShell  `{"prove":{"cmac":HMAC(key,"client",cn,sn),"actor":…,"app":N}}`
//      iShell 验 `cmac`（含 sn，防重放）；不对 → 静默断开。通过后回
//      `{"ticket":{"ticket":<随机凭据>,"app":N}}`；`app` 协商不拢则回一条明确的错误（此时对方已
//      认证，可以告诉它「请升级 / 重新部署」）。
// 之后每条业务请求（新连接、一问一答）是**两行**：第一行 `{"bearer":<ticket>}`（≤ [`AUTH_LINE_MAX`]、
// [`AUTH_LINE_TIMEOUT`] 内），iShell 核对通过之后才去读第二行（真正的请求，可以很大）；无效凭据同样
// 静默断开。凭据与请求分行是刻意的：认证前 iShell 只会缓冲几 KB、占名额几秒——否则一个没有密钥的
// 调用方发一行 32MB 的垃圾，就能让 iShell 在**认证之前**先把它整行读进内存、再整体解析。
// 凭据活到本次 iShell 运行结束，不续期、不重握手——失效就是失效，由用户重开 agent。
//
// 三个 HMAC 的角色标签互不相同（反射攻击）；`cn` 与 `sn` 都进 MAC（重放）。

/// 认证层版本，写进 hello。冻结：改它等于换一套协议。
pub const AUTH_VERSION: u32 = 1;
/// 认证阶段每一行的字节上限（hello / prove / bearer）。认证前对端是谁都有可能，所以只肯为它缓冲这么点。
#[allow(dead_code)] // 两个 crate 各编一遍；非 unix 的 GUI 侧没有使用方
pub const AUTH_LINE_MAX: usize = 4096;
/// 认证阶段每一行必须在多久内到齐。正常的代理在验过对端之后立刻就发，几秒绰绰有余；
/// 沉默/慢速的占位连接最多占一个握手名额这么久。
#[allow(dead_code)]
pub const AUTH_LINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 代理读 iShell 应答（认证阶段）的字节上限：对端此刻还没证明过自己，同样不能让它无限灌。
#[allow(dead_code)]
pub const REPLY_LINE_MAX: usize = 64 * 1024;
/// 会话文件所在的子目录（`~/.ishell-mcp/` 下）。
#[allow(dead_code)]
pub const SESSION_SUBDIR: &str = "sess";
/// 会话文件第一行的魔数 + 格式版本。
#[allow(dead_code)]
pub const SESSION_MAGIC: &str = "ishell-mcp-session/1";
/// 会话文件里记的 shell 启动时间与代理自己算出来的允许差多少秒（两边各算一次，各有 ±1 秒的取整误差）。
/// 它防的是 pid 复用：旧会话文件残留、同一个 pid 后来被别的登录会话用上时，启动时间对不上就不认。
#[allow(dead_code)]
pub const START_TOLERANCE_SECS: u64 = 3;

/// 握手中各步 MAC 的角色标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AuthStep {
    Hello,
    Server,
    Client,
}

impl AuthStep {
    fn label(self) -> &'static str {
        match self {
            AuthStep::Hello => "hello",
            AuthStep::Server => "server",
            AuthStep::Client => "client",
        }
    }
}

/// 生成握手随机数（32 字节 → 64 位十六进制）。
///
/// **熵源失败一律返回 `None`，绝不退化成时间戳**：可预测的 nonce 会让挑战-应答退化成可重放的
/// 静态口令。拿到 `None` 的一方必须中止握手。
#[allow(dead_code)] // 两个 crate 各编一遍，各自只用得到其中一部分
pub fn auth_nonce() -> Option<String> {
    random_hex(32)
}

/// 是不是恰好 64 位十六进制（握手随机数 `cn`/`sn` 的形状）。MAC 输入用 NUL 分隔各字段，随机数的
/// 形状必须收紧，否则字段边界是模糊的。
#[allow(dead_code)]
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 生成 `n` 字节的随机十六进制串（如代理进程标识 `actor`）。熵源失败返回 `None`——
/// 调用方自行决定是报错还是退回弱一些的形态（`actor` 只是进程区分，不是保密凭据）。
#[allow(dead_code)] // 两个 crate 各编一遍，各自只用得到其中一部分
pub fn random_hex(n: usize) -> Option<String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).ok()?;
    Some(hex(&buf))
}

/// 字节 → 小写十六进制（本协议里所有十六进制串——随机数、MAC、密钥——都走这一份）。
#[allow(dead_code)]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 某一步的 MAC：`HMAC-SHA256(key, "ishell-mcp/auth/1/<角色>\0<cn>\0<sn>")`，十六进制。
/// `Hello` 一步还没有 `sn`，传空串。
#[allow(dead_code)]
pub fn auth_mac(key: &str, step: AuthStep, cn: &str, sn: &str) -> String {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(key.as_bytes())
        .expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(format!("ishell-mcp/auth/{AUTH_VERSION}/{}\0{cn}\0{sn}", step.label()).as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// 常量时间比对收到的 MAC。
#[allow(dead_code)]
pub fn auth_mac_matches(key: &str, step: AuthStep, cn: &str, sn: &str, got: &str) -> bool {
    let want = auth_mac(key, step, cn, sn);
    let (a, b) = (want.as_bytes(), got.as_bytes());
    // 逐字节 OR 累积差异、不提前返回：`==` 在首个不同字节处短路，攻击者据此能逐字节试出正确值。
    // 长度本身是公开常量（固定 64 位十六进制），先比长度不泄露任何信息。
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// 握手第 1 步（代理 → iShell）。外层的 `hello` 键就是这一帧的类型标记：iShell 读到第一行
/// 先看有没有这个键，没有就必须是带有效凭据的业务请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct HelloFrame {
    pub hello: Hello,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Hello {
    /// 认证层版本，恒为 [`AUTH_VERSION`]。
    pub auth: u32,
    pub cn: String,
    pub mac: String,
}

/// 握手第 2 步（iShell → 代理）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct HelloOkFrame {
    pub hello_ok: HelloOk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct HelloOk {
    pub sn: String,
    pub smac: String,
    /// iShell 支持的业务协议版本区间。
    pub app_min: u32,
    pub app_max: u32,
    /// 能力开关：业务层新增可选功能走这里，不再为每个功能 bump 协议号。
    #[serde(default)]
    pub caps: Vec<String>,
    /// 对端是谁（已通过认证，才给）：让 agent 与用户一眼看出连到了哪台电脑的 iShell。
    pub peer: Peer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Peer {
    /// 运行 iShell 的机器主机名。
    pub host: String,
    /// 本次 iShell 运行的标识（`store::mcp_instance_id`）；重启即变。
    pub run: String,
}

/// 握手第 3 步（代理 → iShell）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ProveFrame {
    pub prove: Prove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Prove {
    pub cmac: String,
    /// 代理进程标识：iShell 把签发的凭据绑到它上面，此后以它为准（归属判定用）。
    pub actor: String,
    /// 代理选定的业务协议版本（须落在 iShell 通告的区间内）。
    pub app: u32,
}

/// 业务连接的第一行（代理 → iShell）：出示握手签发的凭据。**单独成行**，理由见「认证层」一节。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct BearerFrame {
    pub bearer: String,
}

/// 握手第 4 行（iShell → 代理）：签发的凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TicketFrame {
    pub ticket: TicketGrant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TicketGrant {
    pub ticket: String,
    pub app: u32,
}

/// 读一行，**最多缓冲 `cap` 字节**。`None` = 对端在换行前关闭了连接、行超长、或不是合法 UTF-8。
///
/// 认证前（以及代理读一个尚未证明过自己的对端时）一律用它，而不是 `read_line`/`Lines`：后者要等
/// 到换行才返回，对端不发换行就能让这一侧无限缓冲。调用方自行套超时（[`AUTH_LINE_TIMEOUT`]）。
#[allow(dead_code)]
pub async fn read_line_bounded<R>(r: &mut R, cap: usize) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = r.fill_buf().await.ok()?;
        if chunk.is_empty() {
            return None; // 换行之前对端就关了
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if buf.len() + i > cap {
                    return None;
                }
                buf.extend_from_slice(&chunk[..i]);
                r.consume(i + 1);
                break;
            }
            None => {
                if buf.len() + chunk.len() > cap {
                    return None;
                }
                let n = chunk.len();
                buf.extend_from_slice(chunk);
                r.consume(n);
            }
        }
    }
    String::from_utf8(buf).ok()
}

/// 两端各自支持的业务协议区间求交，取最高的公共版本。
#[allow(dead_code)]
pub fn negotiate_app(a_min: u32, a_max: u32, b_min: u32, b_max: u32) -> Option<u32> {
    let (lo, hi) = (a_min.max(b_min), a_max.min(b_max));
    (lo <= hi).then_some(hi)
}

// ---------- 会话文件与进程祖先 ----------

/// 一个进程的 `(父进程, 启动时刻)`。启动时刻是 Unix 秒——只用来在 pid 被复用时认出「不是原来那个」，
/// 不需要精确，两端各算一次、允许差 [`START_TOLERANCE_SECS`]。
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ProcInfo {
    pub ppid: u32,
    pub start: u64,
}

/// Linux：读 `/proc/<pid>/stat`（父进程 = 第 4 项，启动时刻 = 第 22 项，单位是开机后的时钟节拍）
/// 加上开机时刻 `btime`。
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm 可能含空格和括号：只信最后一个 `)` 之后的字段
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ppid: u32 = f.get(1)?.parse().ok()?;
    let ticks: u64 = f.get(19)?.parse().ok()?;
    let hz = match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        n if n > 0 => n as u64,
        _ => 100,
    };
    let btime: u64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(ProcInfo {
        ppid,
        start: btime + ticks / hz,
    })
}

/// macOS：`proc_pidinfo(PROC_PIDTBSDINFO)` 直接给父进程与启动秒数，不起子进程。
#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (n == size).then_some(ProcInfo {
        ppid: info.pbi_ppid,
        start: info.pbi_start_tvsec,
    })
}

/// 其它 unix（BSD 等）：退到 `ps -o ppid=,etime= -p <pid>`，启动时刻 = 现在 − 已运行时间。
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
#[allow(dead_code)]
pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=,etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let ppid: u32 = it.next()?.parse().ok()?;
    let elapsed = parse_etime(it.next()?)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(ProcInfo {
        ppid,
        start: now.saturating_sub(elapsed),
    })
}

/// `ps` 的 `etime` 格式 `[[dd-]hh:]mm:ss` → 秒。
#[allow(dead_code)]
pub fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, sec] => m * 60 + sec,
        [h, m, sec] => h * 3600 + m * 60 + sec,
        _ => return None,
    };
    Some(days * 86400 + secs)
}

/// 会话文件的内容：一个登录 shell 对应的 iShell 连接信息。
///
/// 纯文本、固定五行（魔数 / 端点 / 密钥 / shell pid / shell 启动秒数）——远端是用一小段 POSIX
/// `sh` 写的，不指望那边有 JSON 工具。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SessionRecord {
    pub endpoint: String,
    pub key: String,
    pub shell_pid: u32,
    pub shell_start: u64,
}

#[allow(dead_code)]
impl SessionRecord {
    pub fn render(&self) -> String {
        format!(
            "{SESSION_MAGIC}\n{}\n{}\n{}\n{}\n",
            self.endpoint, self.key, self.shell_pid, self.shell_start
        )
    }

    /// 解析并校验形状：魔数对、端点是绝对路径、密钥恰为 64 位十六进制、数字能解。
    /// 多一行、少一行、多余内容都不认——格式是我们自己写的，不该有「宽容」。
    pub fn parse(text: &str) -> Option<Self> {
        let mut it = text.lines();
        if it.next()? != SESSION_MAGIC {
            return None;
        }
        let endpoint = it.next()?.to_string();
        let key = it.next()?.to_string();
        let shell_pid: u32 = it.next()?.parse().ok()?;
        let shell_start: u64 = it.next()?.parse().ok()?;
        if it.next().is_some() || !endpoint.starts_with('/') || !is_hex64(&key) {
            return None;
        }
        Some(Self {
            endpoint,
            key,
            shell_pid,
            shell_start,
        })
    }
}

/// 本机主机名，规整成可作目录名的一段：只留 `[A-Za-z0-9._-]`，其余换成 `_`，空、`.`、`..` 则为 `unknown`。
/// 远端登记脚本用 `uname -n` 做同样的规整（`uname -n` 与 `gethostname` 是同一个内核字段）。
#[allow(dead_code)]
pub fn host_tag() -> String {
    #[cfg(unix)]
    let raw = {
        let mut buf = [0u8; 256];
        // SAFETY：缓冲区长度如实传入；失败时 buf 保持全零，落到 "unknown"。
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        if ok { String::from_utf8_lossy(&buf[..end]).into_owned() } else { String::new() }
    };
    #[cfg(not(unix))]
    let raw = String::new();
    sanitize_host_tag(&raw)
}

#[allow(dead_code)]
fn sanitize_host_tag(raw: &str) -> String {
    // 逐字节替换，与远端脚本里的 `tr -c` 一致
    let t: String = raw
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') { b as char } else { '_' })
        .collect();
    if matches!(t.as_str(), "" | "." | "..") { "unknown".into() } else { t }
}

/// 会话文件目录：`<home>/.ishell-mcp/sess/<主机名>`。按主机分目录是因为 `$HOME` 可能被多台机器共享
/// （NFS 家目录的集群）：文件以 pid 命名、清扫按「pid 在本机是否存在」判断，混在一起会互相误删、
/// 撞名覆盖。
#[allow(dead_code)]
pub fn session_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".ishell-mcp").join(SESSION_SUBDIR).join(host_tag())
}

/// 从 `start_pid` 起沿父进程往上找，返回第一个有**合法**会话文件的祖先对应的记录。
///
/// 「合法」= 文件读得出、格式对、记录里的 pid 就是这个祖先、记录里的 shell 启动时间与这个祖先
/// 实际的启动时间相差不超过 [`START_TOLERANCE_SECS`]（挡 pid 复用）。找不到返回已经检查过的
/// 祖先 pid 链（诊断用）。`info` / `load` 由调用方给，便于测试用假进程表。
#[allow(dead_code)]
pub fn find_session_record(
    start_pid: u32,
    dir: &std::path::Path,
    info: &dyn Fn(u32) -> Option<(u32, u64)>,
    load: &dyn Fn(&std::path::Path) -> Option<String>,
) -> Result<SessionRecord, Vec<u32>> {
    let mut chain = Vec::new();
    let mut pid = start_pid;
    for _ in 0..64 {
        if pid <= 1 || chain.contains(&pid) {
            break;
        }
        chain.push(pid);
        let Some((ppid, start)) = info(pid) else {
            break;
        };
        if let Some(rec) = load(&dir.join(pid.to_string()))
            .as_deref()
            .and_then(SessionRecord::parse)
        {
            if rec.shell_pid == pid && rec.shell_start.abs_diff(start) <= START_TOLERANCE_SECS {
                return Ok(rec);
            }
        }
        pid = ppid;
    }
    Err(chain)
}

/// 单个终端会话的摘要（`list_sessions` 的返回项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSessionInfo {
    pub uid: u64,
    pub title: String,
    pub host: String,
    pub connected: bool,
    /// 远端当前工作目录（用户会话需已同意过 OSC 7 注入才有值；AI 专用会话自动注入，
    /// 刚连上的一小段时间内可能还是 null——上报片段要等 shell 闲下来才注入）。
    pub cwd: Option<String>,
    /// 这个会话是不是 AI 自己用 `open_session` 开的。
    ///
    /// `false` = 用户自己打开的会话。这类会话用户本人随时可能在里面敲字，AI 再往同一个
    /// shell 里写就是两路输入交织：轻则互相打断，重则把 `run_command` 用来判断命令结束的
    /// 哨兵标记行搅乱。所以（v6 起）AI 对这类会话读写都不允许，`list_sessions` 也根本不会列出它。
    pub ai_owned: bool,
    /// 开启它的 AI 的可读来源标签（代理请求里的 `origin` 快照，如 `e5-1 (ishell-mcp pid
    /// 42)`）。旧代理开的会话没有（`None`），`ai_owned=false` 时为 `None`。
    #[serde(default)]
    pub ai_owner: Option<String>,
    /// 这个 AI 窗口是不是**发起本次 list_sessions 的这个 AI** 自己开的（按代理进程的
    /// `actor` 身份比对）。v6 起 `list_sessions` 只返回自己的会话，所以列表里恒为 `true`；
    /// 字段保留是线格式兼容。其它 AI 的窗口不会出现在列表里，读写也是硬拒绝（没有授权弹窗）。
    /// 旧代理（不带 actor）开的窗口对任何调用方都报 `false`——它们属于旧版的共享池。
    #[serde(default)]
    pub mine: bool,
    /// 这个会话的登录 shell 是否已登记为「属于这台 iShell」（服务器上有它的会话文件）。`false` 时在它里面
    /// 启动的 AI 连不上 iShell：登记失败（原因见会话状态栏的 ⚠ 提示），或 AI 控制本次启动时是关的。
    #[serde(default)]
    pub bound: bool,
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
    /// 本请求**涉及的会话** uid——读和写都算。这些会话必须全部是发起方自己用 `open_session`
    /// 开的，否则 iShell 直接拒绝整条请求（见 GUI 侧 `mcp_bridge::session_owned_by`）。
    ///
    /// 用户拍板（2026-09-18）：AI 只能读写自己开的窗口，用户的窗口、其它 AI 的窗口连读都不行。
    /// 所以这里不再区分读写，只回答「这条请求碰了哪些会话」。
    ///
    /// match 故意不写 `_ =>` 通配：新增变体时编译器会在这里报错，逼作者表态它碰不碰会话，
    /// 而不是默默继承「不涉及会话、不用校验」。
    pub fn session_target_uids(&self) -> Vec<u64> {
        match self {
            McpReqKind::RunCommand { session_uid, .. }
            | McpReqKind::SendInput { session_uid, .. }
            | McpReqKind::Interrupt { session_uid }
            | McpReqKind::WriteFile { session_uid, .. }
            | McpReqKind::CopyToRemote { session_uid, .. }
            | McpReqKind::CopyToRemoteFromCaller { session_uid, .. }
            | McpReqKind::PollRun { session_uid, .. }
            | McpReqKind::ReadScreen { session_uid }
            | McpReqKind::ReadHistory { session_uid, .. }
            | McpReqKind::ReadFile { session_uid, .. }
            | McpReqKind::CopyFromRemoteToCaller { session_uid, .. }
            | McpReqKind::CloseSession { session_uid } => vec![*session_uid],
            McpReqKind::CopyBetweenSessions {
                src_session_uid,
                dest_session_uid,
                ..
            } => vec![*src_session_uid, *dest_session_uid],
            // 列表 / 新开会话：不指向任何已有会话。
            McpReqKind::ListSessions
            | McpReqKind::ListSavedConnections
            | McpReqKind::OpenSession { .. } => Vec::new(),
        }
    }
}

/// `send_input` 在 `escapes=true` 时的字面转义解析（代理侧做，线协议不变）。
///
/// 支持 `\r` `\n` `\t` `\\` 与 `\xHH`（恰好两位十六进制、值 00-7F——结果要作为 JSON
/// 字符串上线，必须是合法 UTF-8；控制键全在这个范围内）。这是调用方**显式开启**的模式，
/// 所以写错一律报错、不发送：未定义转义、`\x` 后不足两位或非十六进制（含 `+` 号）、
/// 超出 7F、末尾孤立反斜杠。宁可让调用方改一次，也不把半截按键序列打进终端。
#[allow(dead_code)] // 只有 ishell-mcp 那个 crate 用；本文件被两个 crate 各编一遍
pub fn unescape_key_text(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                // 先确认恰好两位十六进制数字：from_str_radix 自己会接受 `+` 号和一位数
                let byte = (hex.len() == 2 && hex.chars().all(|h| h.is_ascii_hexdigit()))
                    .then(|| u8::from_str_radix(&hex, 16).ok())
                    .flatten()
                    .filter(u8::is_ascii)
                    .ok_or_else(|| {
                        format!("escapes=true：\\x 后必须是两位十六进制且不超过 7F，实际是 \\x{hex}")
                    })?;
                out.push(byte as char);
            }
            Some(other) => {
                return Err(format!(
                    "escapes=true：未定义的转义 \\{other}（支持 \\r \\n \\t \\\\ \\xHH；字面反斜杠写成 \\\\）"
                ))
            }
            None => return Err("escapes=true：text 以孤立的反斜杠结尾（字面反斜杠写成 \\\\）".into()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod unescape_key_text_tests {
    use super::unescape_key_text;

    #[test]
    fn documented_escapes_become_keys() {
        assert_eq!(unescape_key_text("ls -l\\r").unwrap(), "ls -l\r");
        assert_eq!(unescape_key_text("a\\tb\\n").unwrap(), "a\tb\n");
        assert_eq!(unescape_key_text("\\x04").unwrap(), "\u{4}");
        assert_eq!(unescape_key_text("\\x1b:wq\\r").unwrap(), "\u{1b}:wq\r");
        assert_eq!(unescape_key_text("C:\\\\tmp").unwrap(), "C:\\tmp");
        assert_eq!(unescape_key_text("中文 hello").unwrap(), "中文 hello");
    }

    /// 显式开启的模式写错必须报错、绝不半截发送。
    /// 反向对照：把 `\x` 的校验放宽回 `from_str_radix` 直接解析，`\x+1`、`\x4` 两条当场挂。
    #[test]
    fn malformed_escapes_are_rejected() {
        for bad in ["a\\qb", "a\\", "\\xzz", "\\x+1", "\\x4", "\\xff"] {
            assert!(unescape_key_text(bad).is_err(), "应当报错：{bad:?}");
        }
    }
}

#[cfg(test)]
mod session_target_tests {
    use super::*;

    /// 碰会话的操作（**读和写都算**）必须报出目标会话——漏一个，AI 就能读到或操作不是它开的
    /// 窗口（用户 2026-09-18 拍板：AI 只能读写自己开的会话）。
    /// 反向对照：把 `ReadScreen` 挪回「不涉及会话」那一组，本条当场挂。
    #[test]
    fn session_ops_report_their_target_session() {
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
            McpReqKind::CloseSession { session_uid: 7 },
        ];
        for kind in cases {
            assert_eq!(
                kind.session_target_uids(),
                vec![7],
                "漏报了目标会话：{kind:?}"
            );
        }
    }

    /// 跨会话拷贝两端都是会话：两个 uid 都必须是发起方自己的，不能只看目标。
    #[test]
    fn cross_session_copy_gates_both_hosts() {
        let kind = McpReqKind::CopyBetweenSessions {
            src_session_uid: 3,
            src_remote_path: "/src".into(),
            dest_session_uid: 9,
            dest_remote_path: "/dst".into(),
            timeout_ms: 0,
        };
        assert_eq!(kind.session_target_uids(), vec![3, 9]);
    }

    /// 不指向任何已有会话的操作：列表、新开会话。
    #[test]
    fn non_session_ops_have_no_target() {
        let cases: Vec<McpReqKind> = vec![
            McpReqKind::ListSessions,
            McpReqKind::ListSavedConnections,
            McpReqKind::OpenSession { name: "s2".into() },
        ];
        for kind in cases {
            assert!(
                kind.session_target_uids().is_empty(),
                "不涉及会话的操作被报出了目标：{kind:?}"
            );
        }
    }
}

/// 一条业务请求（认证通过之后；一次连接 = 一问一答）。线上它是第二行；第一行是 [`BearerFrame`]
/// （连接凭据，绑定握手时声明的 actor，iShell 先于一切别的处理核对它）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRequest {
    pub id: u64,
    /// 发起方的可读来源描述（如 `e5-1 (ishell-mcp pid 42)` / `Codex CLI`）——**仅供展示**
    /// （会话标题旁标注「这是谁的 AI 开的」）。不是身份：身份只来自连接凭据。
    #[serde(default)]
    pub origin: Option<String>,
    /// 本请求所属代理进程的标识。**不上线**：iShell 在核对凭据后按凭据回填，调用方自报的
    /// 值根本没有地方可填——冒用别人的 actor 拿不走别人的窗口（归属判定见
    /// `mcp_bridge::session_owned_by`）。
    #[serde(skip)]
    pub actor: Option<String>,
    pub kind: McpReqKind,
}

/// 本机主机名：填进握手应答的 `Peer.host`（只用于展示，让人一眼看出连的是哪台电脑）。
/// 取不到时返回 `"unknown-host"`。
#[allow(dead_code)] // 只有 GUI（unix）的握手应答用
pub fn local_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let raw = String::from_utf8_lossy(&buf[..len]);
            let cleaned = sanitize_hostname(&raw);
            if !cleaned.is_empty() {
                return cleaned;
            }
        }
    }
    let fallback = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown-host".into());
    let cleaned = sanitize_hostname(&fallback);
    if cleaned.is_empty() {
        "unknown-host".into()
    } else {
        cleaned
    }
}

/// 主机名只保留 DNS 安全字符，免得带着 shell 元字符进到日志 / 提示文字里。
#[allow(dead_code)]
pub fn sanitize_hostname(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
        .take(128)
        .collect()
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
        write_framed_stream(&mut &payload[..], &mut wire)
            .await
            .expect("写不该失败");
        let mut got = Vec::new();
        let total = read_framed_stream(&mut std::io::Cursor::new(wire), &mut got)
            .await
            .expect("读不该失败");
        (total, got)
    }

    #[tokio::test]
    async fn round_trips_payloads_of_every_shape() {
        for payload in [
            Vec::new(),                             // 空文件：只有一个 `0\n`
            b"hello".to_vec(),                      // 单块
            vec![0xAB; STREAM_CHUNK_BYTES],         // 恰好一块
            vec![0xCD; STREAM_CHUNK_BYTES + 1],     // 跨块边界
            vec![0x00; STREAM_CHUNK_BYTES * 2 + 7], // 多块 + 零字节（不能被当成结束符）
            b"12\n34\n0\n".to_vec(),                // 内容长得像帧头：必须靠长度而非内容分帧
        ] {
            let (total, got) = round_trip(&payload).await;
            assert_eq!(
                total,
                payload.len() as u64,
                "长度不符（payload {} 字节）",
                payload.len()
            );
            assert_eq!(got, payload, "内容不符（payload {} 字节）", payload.len());
        }
    }

    /// 收尾的 `0\n` 之后，读端必须**恰好**停在判定行开头——判定能不能被找到，全靠这一点。
    #[tokio::test]
    async fn leaves_the_reader_exactly_at_the_verdict_line() {
        use tokio::io::AsyncBufReadExt;
        let mut wire = Vec::new();
        write_framed_stream(&mut &b"body"[..], &mut wire)
            .await
            .unwrap();
        wire.extend_from_slice(b"{\"id\":1,\"result\":{\"Ok\":\"Ok\"}}\n");

        let mut r = std::io::Cursor::new(wire);
        let mut got = Vec::new();
        read_framed_stream(&mut r, &mut got).await.unwrap();
        assert_eq!(got, b"body");

        let mut verdict = String::new();
        r.read_line(&mut verdict).await.unwrap();
        let parsed: McpResponse =
            serde_json::from_str(verdict.trim()).expect("判定应能原样解析出来");
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
        let err = read_framed_stream(
            &mut std::io::Cursor::new(b"not-a-number\n".to_vec()),
            &mut got,
        )
        .await
        .expect_err("帧头无法解析时必须报错");
        assert!(err.contains("分块长度无法解析"), "{err}");
    }
}

/// 业务协议版本（认证层另有冻结的 [`AUTH_VERSION`]）。GUI(`ishell`)与代理(`ishell-mcp`)把本文件
/// 各编一遍，业务线格式必须配套。
///
/// 版本不是靠一个匿名可问的信标来比对的：握手通过后，iShell 在 `hello_ok` 里通告自己支持的区间
/// （[`MCP_APP_MIN`]..=[`MCP_APP_MAX`]），代理与自己的区间求交（[`negotiate_app`]），求不出交集就
/// 报明确的「请升级 / 重新部署」。**可选的新功能走 `caps` 开关，不要再为它们 bump 版本**；只有
/// 业务线格式发生不兼容变化时才动这个区间。
///
/// 历史：v1–v7 是认证与发现层的反复修补（共用配对 token、扫描 socket、多实例弹窗选择、静默改绑
/// ……）——它们是同一个设计缺陷的七次补丁：「用一个会被拷贝、被覆盖、被同步的静态口令，去
/// 在一堆候选 socket 里猜哪个是我的 iShell」。v8 整体换掉：身份由终端注入的「端点 + 本次运行
/// 密钥」决定，没有扫描、没有候选、没有降级；旧协议不再兼容。
/// 本构建支持的业务协议区间（目前只有 v8）。
#[allow(dead_code)]
pub const MCP_APP_MIN: u32 = 8;
#[allow(dead_code)]
pub const MCP_APP_MAX: u32 = 8;
/// 本构建「说」的业务协议版本（`--version` 打印它）：就是区间的上界，不单独再写一个数，免得与区间漂移。
#[allow(dead_code)] // 只有代理的 `--version` 用它
pub const MCP_PROTOCOL_VERSION: u32 = MCP_APP_MAX;
/// 本构建通告的能力开关（目前没有）。
#[allow(dead_code)] // 只有 GUI 一侧会通告
pub const MCP_CAPS: &[&str] = &[];

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
    /// `WriteFile` 成功后的新 mtime。
    FileWritten {
        path: String,
        mtime: u32,
    },
    /// `ReadFile` 的结果。
    FileContent {
        path: String,
        content: String,
    },
    /// `CopyToRemote`/`CopyFromRemote` 成功后的目标路径。
    Copied {
        path: String,
    },
    /// `CopyFromRemoteToCaller` 的响应头：先以这一行 JSON 单独送达，代理进程解析出 `size`
    /// 后再从同一条 socket 连接上读取紧随其后的 `size` 字节原始文件内容（无额外分隔符/
    /// trailer）。GUI 侧提前判定失败（远端不存在/是目录等）时仍按普通 `Err` 响应，不会
    /// 发送这个变体，代理进程据此区分两种情况，不需要另外猜测。
    CopyStreamHeader {
        path: String,
        size: u64,
    },
    /// `CopyBetweenSessions` 成功后的目标路径；`method` 目前恒为 `"relay"`（经 iShell 内存
    /// 中转，两端都不落盘）——为直连优先模式预留，届时会出现 `"direct"`。
    CopiedBetweenSessions {
        path: String,
        method: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResponse {
    pub id: u64,
    /// `Err` 携带人类可读的错误信息（会话不存在、已有命令在跑、socket 未连上等）。
    pub result: Result<McpReqResult, String>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_tag_is_a_single_safe_path_component() {
        use super::sanitize_host_tag as t;
        assert_eq!(t("node-01.cluster_a"), "node-01.cluster_a");
        assert_eq!(t("a/b c"), "a_b_c");
        assert_eq!(t("../etc"), ".._etc");
        for bad in ["", ".", ".."] {
            assert_eq!(t(bad), "unknown", "{bad:?}");
        }
        assert!(!super::host_tag().contains('/'));
        assert!(super::session_dir(std::path::Path::new("/h")).starts_with("/h/.ishell-mcp/sess"));
    }

    use super::*;

    /// MAC 的正路：同一密钥 + 同一对随机数 + 同一步 → 对得上；换密钥不行。
    #[test]
    fn auth_mac_verifies_only_for_the_matching_key() {
        let m = auth_mac("key", AuthStep::Server, "NC", "NS");
        assert!(auth_mac_matches("key", AuthStep::Server, "NC", "NS", &m));
        assert!(!auth_mac_matches("other", AuthStep::Server, "NC", "NS", &m));
    }

    /// 换掉任何一个随机数，MAC 都必须失效——否则挑战-应答退化成可重放的静态口令。
    #[test]
    fn auth_mac_is_bound_to_both_nonces() {
        let m = auth_mac("key", AuthStep::Client, "NC", "NS");
        assert!(!auth_mac_matches("key", AuthStep::Client, "NC2", "NS", &m));
        assert!(!auth_mac_matches("key", AuthStep::Client, "NC", "NS2", &m));
    }

    /// 三个角色的 MAC 互不可换：把 iShell 的证明原样反射回去不能冒充代理（经典反射攻击），
    /// 代理的 hello 也不能当成 iShell 的证明。这正是角色标签存在的唯一理由——去掉它这条会挂。
    #[test]
    fn roles_cannot_be_swapped() {
        let steps = [AuthStep::Hello, AuthStep::Server, AuthStep::Client];
        for a in steps {
            for b in steps {
                let m = auth_mac("key", a, "NC", "NS");
                assert_eq!(
                    auth_mac_matches("key", b, "NC", "NS", &m),
                    a == b,
                    "{a:?} 的 MAC 被当成 {b:?} 接受了"
                );
            }
        }
    }

    /// 随机数每次都不一样，且长度固定（常量时间比对依赖这一点）。
    #[test]
    fn auth_nonce_is_fresh_and_fixed_width() {
        let (a, b) = (
            auth_nonce().expect("熵源应可用"),
            auth_nonce().expect("熵源应可用"),
        );
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "两次生成的随机数相同——熵源有问题");
    }

    /// 畸形/截断的 MAC 不能因为长度不同就 panic 或误判。
    #[test]
    fn malformed_macs_are_rejected_not_panicking() {
        for got in ["", "zz", &"a".repeat(63), &"a".repeat(65)] {
            assert!(!auth_mac_matches("key", AuthStep::Hello, "NC", "", got));
        }
    }

    /// 有界读行：行内容在上限内正常返回；超限、无换行即关闭、非 UTF-8 都是 `None`，且超限时
    /// **不会**先把整行读进内存。
    #[tokio::test]
    async fn bounded_line_reader_refuses_to_buffer_past_its_cap() {
        let mut ok = std::io::Cursor::new(b"hello\nrest".to_vec());
        assert_eq!(read_line_bounded(&mut ok, 16).await.as_deref(), Some("hello"));
        // 恰好等于上限
        let mut exact = std::io::Cursor::new(b"12345\n".to_vec());
        assert_eq!(read_line_bounded(&mut exact, 5).await.as_deref(), Some("12345"));
        let mut over = std::io::Cursor::new(b"123456\n".to_vec());
        assert_eq!(read_line_bounded(&mut over, 5).await, None);
        // 没有换行、也超限：一边读一边就该放弃（用一个会无限产出数据的读端证明它不会读穷）
        let mut endless = tokio::io::BufReader::new(tokio::io::repeat(b'a'));
        assert_eq!(read_line_bounded(&mut endless, 4096).await, None);
        let mut eof = std::io::Cursor::new(b"no newline".to_vec());
        assert_eq!(read_line_bounded(&mut eof, 64).await, None);
        let mut bad = std::io::Cursor::new(b"\xff\xfe\n".to_vec());
        assert_eq!(read_line_bounded(&mut bad, 64).await, None);
    }

    /// 会话文件往返；形状不对一律不认（多一行、少一行、端点不是绝对路径、密钥长度不对、魔数不对）。
    #[test]
    fn session_record_round_trips_and_rejects_odd_shapes() {
        let rec = SessionRecord {
            endpoint: "/home/u/.ishell-mcp/mcp-abc.sock".into(),
            key: "ab".repeat(32),
            shell_pid: 4242,
            shell_start: 1_790_000_000,
        };
        assert_eq!(SessionRecord::parse(&rec.render()).as_ref(), Some(&rec));
        let good = rec.render();
        for bad in [
            good.replace(SESSION_MAGIC, "ishell-mcp-session/2"),
            format!("{good}extra\n"),
            good.lines().take(4).collect::<Vec<_>>().join("\n"),
            good.replace("/home/u", "home/u"),
            good.replace(&rec.key, &"ab".repeat(31)),
            good.replace("4242", "not-a-pid"),
        ] {
            assert_eq!(SessionRecord::parse(&bad), None, "不该认：{bad:?}");
        }
    }

    /// `ps` 的 etime 格式：`mm:ss`、`hh:mm:ss`、`dd-hh:mm:ss`。
    #[test]
    fn etime_formats_parse() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-03:04:05"), Some(2 * 86400 + 3 * 3600 + 4 * 60 + 5));
        assert_eq!(parse_etime("garbage"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
    }

    /// **祖先查找**：用假进程表验证性质——
    /// ① 命中最近的、合法的祖先；② 记录里的 pid 与文件名不一致不认；③ 启动时间对不上（pid 被复用）
    /// 不认；④ 兄弟/无关进程的记录不会被拿来用（只沿父进程链往上找）；⑤ 环与深度有界。
    #[test]
    fn ancestry_lookup_only_trusts_a_valid_ancestor() {
        use std::collections::HashMap;
        let key = "cd".repeat(32);
        let rec = |pid: u32, start: u64, ep: &str| SessionRecord {
            endpoint: ep.into(),
            key: key.clone(),
            shell_pid: pid,
            shell_start: start,
        };
        // 进程树：agent(30) → 代理(31)；agent 的父 = B 的登录 shell(20)，shell 的父 = sshd(10)；
        // C 的登录 shell(21) 是 sshd(10) 的另一个子进程（兄弟）。
        let procs: HashMap<u32, (u32, u64)> = HashMap::from([
            (31, (30, 1000)),
            (30, (20, 990)),
            (20, (10, 100)),
            (21, (10, 200)),
            (10, (1, 50)),
        ]);
        let files: HashMap<String, String> = HashMap::from([
            ("/s/20".to_string(), rec(20, 100, "/ep/b").render()),
            ("/s/21".to_string(), rec(21, 200, "/ep/c").render()),
        ]);
        let info = |p: u32| procs.get(&p).copied();
        let load = |path: &std::path::Path| files.get(path.to_str().unwrap()).cloned();
        let dir = std::path::Path::new("/s");
        // ① 代理(31) 的祖先里有 B 的 shell(20)，只会找到 B，永远不会是兄弟 C
        let found = find_session_record(31, dir, &info, &load).expect("应命中 B");
        assert_eq!(found.endpoint, "/ep/b");
        // ④ 从 C 的 shell 下面起，只会找到 C
        assert_eq!(find_session_record(21, dir, &info, &load).unwrap().endpoint, "/ep/c");
        // ③ pid 被复用：同一个 pid 20 现在是另一个启动更晚的进程，旧记录不认
        let reused: HashMap<u32, (u32, u64)> = HashMap::from([(31, (20, 5000)), (20, (10, 4990)), (10, (1, 50))]);
        let info2 = |p: u32| reused.get(&p).copied();
        let chain = find_session_record(31, dir, &info2, &load).expect_err("pid 复用必须不认");
        assert_eq!(chain, vec![31, 20, 10]);
        // ② 文件名与记录里的 pid 不一致（有人把 C 的记录拷成 20 号文件）不认
        let swapped: HashMap<String, String> = HashMap::from([("/s/20".to_string(), rec(21, 200, "/ep/c").render())]);
        let load3 = |path: &std::path::Path| swapped.get(path.to_str().unwrap()).cloned();
        assert!(find_session_record(31, dir, &info, &load3).is_err());
        // ⑤ 环：父进程指回自己，不会死循环
        let cyc: HashMap<u32, (u32, u64)> = HashMap::from([(5, (6, 1)), (6, (5, 1))]);
        let info4 = |p: u32| cyc.get(&p).copied();
        assert_eq!(find_session_record(5, dir, &info4, &|_: &std::path::Path| None).unwrap_err(), vec![5, 6]);
    }

    /// 业务协议区间求交：取最高公共版本；无交集即 `None`（报「请升级」）。
    #[test]
    fn app_version_negotiation() {
        assert_eq!(negotiate_app(8, 8, 8, 8), Some(8));
        assert_eq!(negotiate_app(8, 10, 9, 12), Some(10));
        assert_eq!(negotiate_app(8, 8, 9, 12), None, "对端最低版本高于我方最高");
        assert_eq!(negotiate_app(9, 12, 8, 8), None, "对端最高版本低于我方最低");
    }

    /// 认证层线格式冻结：外层键名（`hello`/`hello_ok`/`prove`/`ticket`）就是帧类型标记，
    /// iShell 靠它在不知道对方是谁的时候分辨第一行。改了它，新旧两端在认证层就对不上。
    #[test]
    fn auth_frames_have_a_frozen_shape() {
        let h = serde_json::to_value(HelloFrame {
            hello: Hello {
                auth: AUTH_VERSION,
                cn: "c".into(),
                mac: "m".into(),
            },
        })
        .unwrap();
        assert_eq!(h, serde_json::json!({"hello":{"auth":1,"cn":"c","mac":"m"}}));
        let p = serde_json::to_value(ProveFrame {
            prove: Prove {
                cmac: "x".into(),
                actor: "a".into(),
                app: 8,
            },
        })
        .unwrap();
        assert_eq!(p, serde_json::json!({"prove":{"cmac":"x","actor":"a","app":8}}));
        let t = serde_json::to_value(TicketFrame {
            ticket: TicketGrant {
                ticket: "t".into(),
                app: 8,
            },
        })
        .unwrap();
        assert_eq!(t, serde_json::json!({"ticket":{"ticket":"t","app":8}}));
        let b = serde_json::to_value(BearerFrame { bearer: "t".into() }).unwrap();
        assert_eq!(b, serde_json::json!({"bearer":"t"}));
        let ok = serde_json::to_value(HelloOkFrame {
            hello_ok: HelloOk {
                sn: "s".into(),
                smac: "m".into(),
                app_min: 8,
                app_max: 8,
                caps: vec![],
                peer: Peer {
                    host: "h".into(),
                    run: "r".into(),
                },
            },
        })
        .unwrap();
        assert_eq!(
            ok,
            serde_json::json!({"hello_ok":{"sn":"s","smac":"m","app_min":8,"app_max":8,
                "caps":[],"peer":{"host":"h","run":"r"}}})
        );
    }

    /// `actor` 不上线：线上没有它的位置，调用方自报不了（iShell 按凭据回填）。
    #[test]
    fn actor_never_travels_on_the_wire() {
        let request = McpRequest {
            id: 1,
            origin: None,
            actor: Some("forged".into()),
            kind: McpReqKind::ListSessions,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("forged") && !json.contains("actor"), "{json}");
        let back: McpRequest =
            serde_json::from_str(r#"{"id":1,"actor":"forged","kind":"ListSessions"}"#)
                .unwrap();
        assert_eq!(back.actor, None, "线上带来的 actor 必须被忽略");
    }

    #[test]
    fn caller_stream_upload_request_round_trips_without_content_field() {
        let request = McpRequest {
            id: 7,
            origin: None,
            actor: None,
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

    #[test]
    fn sanitize_hostname_strips_shell_metacharacters() {
        assert_eq!(sanitize_hostname("box.lan"), "box.lan");
        assert_eq!(sanitize_hostname("  my-host_1  "), "my-host_1");
        assert_eq!(sanitize_hostname("bad;rm -rf /"), "badrm-rf");
        assert_eq!(sanitize_hostname("$(id)"), "id");
        assert_eq!(sanitize_hostname(""), "");
    }
}
