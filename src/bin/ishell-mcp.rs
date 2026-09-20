//! iShell 的 AI/MCP 控制通道代理：Claude Code 等 AI 客户端按 stdio 方式 spawn 的独立小进程。
//! 只是把工具调用转发到本机正在运行的 iShell 主进程（经本地 Unix domain socket，一次连接
//! 一问一答），主进程再落到它已经持有的 SSH 会话上执行。
//!
//! 唯一的进程状态是**绑定**：这个代理默认一辈子只操作一个 iShell 实例（见 `BOUND_INSTANCE`）。
//! 用户可能同时开着多个 iShell——本机的、以及别的机器上反向转发过来的——而每个实例的会话
//! uid 都从 1 开始，所以「这次调用发给谁」绝不能靠猜。首次连接时定下实例，此后每条请求都
//! 点名，由对端自己校验（`McpRequest::instance`）。原实例消失、恰好只剩一个 token 匹配者、
//! **且主机与当初绑定的一致**时才改绑（覆盖 iShell 重启）；多开或换到另一台机器仍要用户重连。
//! 与主二进制共享同一份线协议类型（见 src/mcp_protocol.rs），这里用 #[path] 直接纳入，
//! 避免为共享几个 struct 拆出独立的 lib crate。

// 这一份是「代理侧」的编译产物：同一个文件也被主二进制编一遍，两边用到的子集不同——
// 会话归属门禁（`session_target_uids`）和实例校验（`is_addressed_to`）都只在 GUI 那侧执行，
// 代理这边只用到线协议类型本身。故只在这个 crate 上整模块 allow(dead_code)，否则每加一个
// 「只有 GUI 用得到」的协议方法就要多一条假警告；主二进制那侧不 allow，真正的死代码照样抓得到。
#[allow(dead_code)]
#[path = "../mcp_protocol.rs"]
mod mcp_protocol;

use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;

use mcp_protocol::{McpReqKind, McpReqResult, McpRequest, McpResponse};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 本代理进程绑定的 iShell 实例标识。首次连接时确定；**原实例已消失、恰好只剩一个
/// token 匹配者、且主机与当初绑定的一致时允许改绑**（iShell 重启换了 instance_id 是最常见场景）。
///
/// 原实例仍活着时绝不改绑：每个 iShell 实例的会话 uid 都从 1 开始（见 `src/app/session.rs`），
/// 中途换一个实例执行，`session_uid=1` 会安静地落到另一台机器上。此前按 socket 文件 mtime
/// 挑实例、且**每次调用都重挑**，正是这个 bug。
#[cfg(unix)]
static BOUND_INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
#[cfg(unix)]
static BOUND_INSTANCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 绑定实例当前的 socket 路径缓存。**路径会变，实例不会**：反向转发的 socket 每次 SSH
/// 重连都换一个随机名（见 `src/ssh/mod.rs`，固定名字会被服务器当成尚未失效的旧注册而
/// 拒绝）。所以路径只是缓存，失效了就按实例标识重新找回来。
#[cfg(unix)]
static PATH_CACHE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// 绑定实例签发给本代理的**连接凭据**（v6，见 `McpRequest::ticket`）。每条业务请求都要带上；
/// 被拒（`TICKET_REJECTED`）时重新握手换一张，见 [`refresh_ticket`]。与 `BOUND_INSTANCE` 分开
/// 放：实例一辈子不变，凭据可以换。
#[cfg(unix)]
static TICKET: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 当初绑定时对端报告的主机名。改绑时即使没注入 `ISHELL_HOST`（连上 2s 内敲过键 / IDE 启动）
/// 也能挡住「原实例下线、唯一剩余匹配者在另一台机器」这条窄角。
#[cfg(unix)]
static BOUND_HOST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(unix)]
fn store_binding(id: String, path: std::path::PathBuf, ticket: String, host: String) {
    *BOUND_INSTANCE.lock().unwrap() = Some(id);
    *PATH_CACHE.lock().unwrap() = Some(path);
    *TICKET.lock().unwrap() = Some(ticket);
    *BOUND_HOST.lock().unwrap() = Some(host);
}

/// 等用户在某个 iShell 窗口上点「允许」的超时。GUI 侧的确认框自己有 5 分钟上限、到点会回
/// 一条 Err，正常情况下轮不到这个超时——它只防「GUI 卡死导致工具调用永远挂起」，所以比
/// 5 分钟稍宽一点即可。
#[cfg(unix)]
const BIND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6 * 60);

/// 枚举本机所有**可能**通向某个 iShell 实例的 socket 路径。
///
/// 两个来源：`~/.config/ishell/mcp-*.sock`（同机场景，每个 iShell 进程一个）和
/// `~/.ishell-mcp/mcp-*.sock`（iShell 反向转发到「这个代理所在的机器」时落在这里）。
///
/// 返回的是**候选路径，不是实例列表**——两者不是一一对应的：同一个 iShell 对同一台远端
/// 主机开两个会话，就会在那台主机上注册出两个通向它自己的转发 socket；崩溃残留的死文件也
/// 还躺在目录里。谁是谁、有几个，必须靠 `identify()` 一个个问出来再按 id 去重，不能从文件名
/// 推断——否则会把一个 iShell 当成两个，凭空要求用户去选。
#[cfg(unix)]
fn candidate_paths() -> Vec<std::path::PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dirs = [
        std::path::PathBuf::from(&home).join(".config").join("ishell"),
        std::path::PathBuf::from(&home).join(".ishell-mcp"),
    ];
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // 目录不存在很正常（没用过反向转发/没装过 iShell）
        };
        out.extend(entries.flatten().map(|e| e.path()).filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("mcp-") && n.ends_with(".sock"))
        }));
    }
    out
}

/// 带超时的 connect。同机 Unix socket 上 connect 基本瞬时，这个护栏平时轮不到触发；但反向
/// 转发的 socket 背后是一条 SSH 通道，对端成了网络黑洞时 connect 会一直挂着，谁都不该无限等。
#[cfg(unix)]
async fn connect_timeout(
    path: &std::path::Path,
) -> Result<std::io::Result<UnixStream>, tokio::time::error::Elapsed> {
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, UnixStream::connect(path)).await
}

/// 探测一条候选 socket 的结果。**三态而不是 `Option`**：把「没人应答」和「应答了但配对/
/// 版本不合」混成同一个 `None`，就没法给出对症的错误信息——版本不匹配会被报成「token 不
/// 匹配」，用户照着去核对 token，怎么核都对，实际该做的是重新部署 ishell-mcp。
#[cfg(unix)]
#[derive(Debug)]
enum Probe {
    /// 连不上/超时/对面不是 iShell：目录里躺着崩溃残留的死文件是常态，静默跳过。
    Dead,
    /// 答话了，但没通过配对（token 不符），或压根没配 token 时的普通发现。
    Answered { id: String, ver: u32 },
    /// 答话了，且双向配对握手通过；`ticket` 是对端签发的连接凭据。
    Paired { id: String, ver: u32, ticket: String, host: String },
}

#[cfg(unix)]
impl Probe {
    fn ident(&self) -> Option<(String, u32)> {
        match self {
            Probe::Dead => None,
            Probe::Answered { id, ver } | Probe::Paired { id, ver, .. } => Some((id.clone(), *ver)),
        }
    }
}

/// 连一条 socket 问出对端 iShell 的实例标识（可选地完成配对握手）。
///
/// `prove_token`：
/// - `None` → 发普通 `Identify` 问出实例标识（只用于 connect_bound 按 id 找回已绑定的
///   实例；v5 起无 token 不再用于发现绑定，bind_instance 直接拒绝）。
/// - `Some` → 走 v4 **双向挑战-应答**握手（`PairHello` → 验对端的 `Server` 证明 →
///   `PairProve`）。token 本身绝不过线；对端证明不过就地放弃，**不发**自己的证明——
///   否则一个不知道 token 的假 socket 也能把本代理钓过去（见 `mcp_protocol` 的说明）。
///   握手不过仍返回 `Answered`（对方是个活着的 iShell，只是不是"我的"），
///   好让上层区分「没找到人」和「找到了但都不是我的」。
#[cfg(unix)]
async fn probe(path: &std::path::Path, prove_token: Option<&str>) -> Probe {
    let Some((id, ver)) = identify(path).await else {
        return Probe::Dead;
    };
    let Some(token) = prove_token else {
        return Probe::Answered { id, ver };
    };
    // 版本对不上就别做握手了：对端解不出 PairHello，只会白等一个超时。直接报 Answered，
    // 让上层按版本给出「重新部署」的提示。
    if ver != mcp_protocol::MCP_PROTOCOL_VERSION {
        return Probe::Answered { id, ver };
    }
    match pair_handshake(path, token).await {
        Some((id, ver, ticket, host)) => Probe::Paired { id, ver, ticket, host },
        None => Probe::Answered { id, ver },
    }
}

/// 在一条**新连接**上跑完双向配对握手，成功返回对端的 (id, 版本, 连接凭据)。
///
/// 两问两答共用同一条连接：两个方向的证明必须绑定同一对随机数，拆连接就绑不住了。
#[cfg(unix)]
async fn pair_handshake(path: &std::path::Path, token: &str) -> Option<(String, u32, String, String)> {
    let nonce_c = mcp_protocol::pair_nonce()?; // 熵源不可用：中止，绝不用可预测值凑合
    let stream = connect_timeout(path).await.ok()?.ok()?;
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    let send = |kind: McpReqKind| -> Option<String> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::to_string(&McpRequest {
            id,
            instance: None,
            origin: None, // 握手阶段还没有「来源」可言，也不该有
            // v6：声明本进程身份——iShell 把签发的凭据绑到它上面，此后以它为准。
            actor: Some(current_actor()),
            ticket: None,
            kind,
        })
        .ok()?;
        line.push('\n');
        Some(line)
    };

    // 第一步：送上本方随机数，请对端先证明它知道 token。
    let hello = send(McpReqKind::PairHello {
        nonce_c: nonce_c.clone(),
    })?;
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, w.write_all(hello.as_bytes()))
        .await
        .ok()?
        .ok()?;
    let mut line = String::new();
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, reader.read_line(&mut line))
        .await
        .ok()?
        .ok()?;
    let resp: McpResponse = serde_json::from_str(line.trim()).ok()?;
    let (id, ver, nonce_s, server_proof) = match resp.result.ok()? {
        McpReqResult::PairChallenge {
            id,
            proto_version,
            nonce_s,
            server_proof,
        } => (id, proto_version, nonce_s, server_proof),
        _ => return None,
    };
    // **验过才证明自己**。这一步是整个握手的要害：跳过它，任何假 socket 都能收走本方证明。
    if !mcp_protocol::pair_proof_matches(
        token,
        &nonce_c,
        &nonce_s,
        mcp_protocol::PairRole::Server,
        &server_proof,
    ) {
        return None;
    }

    // 第二步：出示本方证明。
    let prove = send(McpReqKind::PairProve {
        client_proof: mcp_protocol::pair_proof(
            token,
            &nonce_c,
            &nonce_s,
            mcp_protocol::PairRole::Client,
        ),
    })?;
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, w.write_all(prove.as_bytes()))
        .await
        .ok()?
        .ok()?;
    let mut line2 = String::new();
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, reader.read_line(&mut line2))
        .await
        .ok()?
        .ok()?;
    let resp2: McpResponse = serde_json::from_str(line2.trim()).ok()?;
    match resp2.result.ok()? {
        // 没签发凭据的应答（空串）等于握手没成：拿着它发业务请求只会被拒。
        McpReqResult::Instance { ticket, host, .. } if !ticket.is_empty() => {
            Some((id, ver, ticket, host))
        }
        _ => None,
    }
}

/// 反向转发落在 `~/.ishell-mcp/mcp-*.sock` 的路径。只有这类孤儿该由探测方即时回收；
/// 本机 `~/.config/ishell/mcp-*.sock` 属于仍可能活着的 iShell 进程，误删会让本机 MCP 失联。
#[cfg(unix)]
fn is_reverse_forward_sock(path: &std::path::Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == ".ishell-mcp")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("mcp-") && n.ends_with(".sock"))
}

/// 连一条 socket 问出对端 iShell 的实例标识。连不上、对面不是 iShell、超时——一律返回
/// `None`：目录里躺着崩溃残留的死 socket 文件是常态，不值得报错，跳过就是了。
///
/// 只发无字段的 `Identify`：它的线格式跨所有协议版本逐字节相同，是**唯一**一个连版本不
/// 匹配的对端也解得开的请求，因而也是「问出对方版本、给出重新部署提示」的唯一途径。
#[cfg(unix)]
async fn identify(path: &std::path::Path) -> Option<(String, u32)> {
    let stream = match connect_timeout(path).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Linux 上 accept 队列满时 connect 同样返回 ECONNREFUSED，会误删存活的 socket。
            // 先短延迟再试一次；仍拒绝才回收，且只动反向转发目录里的孤儿。
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            match connect_timeout(path).await {
                Ok(Ok(s)) => s,
                Ok(Err(e2)) if e2.kind() == std::io::ErrorKind::ConnectionRefused => {
                    if is_reverse_forward_sock(path) {
                        let _ = std::fs::remove_file(path);
                    }
                    return None;
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    match exchange(stream, None, None, None, None, McpReqKind::Identify, CONNECT_WRITE_TIMEOUT).await {
        Ok(McpReqResult::Instance { id, proto_version, .. }) => Some((id, proto_version)),
        _ => None,
    }
}

/// 校验对端 iShell 的 MCP 协议版本与本代理是否一致；不一致给出可操作的重新部署提示。
/// 旧版 iShell 不带版本字段 → 经 serde 默认收到 0 → 判为不一致（正是所需行为）。
#[cfg(unix)]
fn check_proto_version(peer: u32) -> Result<(), String> {
    if peer == mcp_protocol::MCP_PROTOCOL_VERSION {
        return Ok(());
    }
    Err(format!(
        "iShell 与 ishell-mcp 版本不一致（iShell 端 MCP 协议 v{peer}，本代理 v{}）。二者的线\
         协议是配套编译的，版本错位会导致静默错误——请用与当前 iShell 配套的 ishell-mcp 重新\
         部署（默认位置 ~/.ishell-mcp/bin/ishell-mcp）后，重新连接 MCP。",
        mcp_protocol::MCP_PROTOCOL_VERSION
    ))
}

/// 并发问遍所有候选路径，收齐「答得上话的」实例及其路径。
///
/// 必须并发：候选里混着崩溃残留的死文件，更要命的是反向转发的 socket 背后是一条 SSH 通道，
/// 对端网络黑洞时 connect/读响应会各挂满 `CONNECT_WRITE_TIMEOUT`。串行探测的话，几个卡住的
/// 候选就能把「弹出选择框」推迟几十秒，期间工具调用毫无动静。
///
/// 同一个实例可能从多条路径答话（见 `candidate_paths`），这里不去重——去重规则由调用方定：
/// `bind_instance` 要按 id 收敛成实例列表，`connect_bound` 只关心某个特定 id。
///
/// `prove_token`：见 [`probe`]——配对场景对每个候选跑一遍双向握手，token 不过线。
/// 返回值保留 [`Probe`] 的三态，调用方据此区分「没人应答」「应答了但版本不对」「应答了但
/// 不是我的」——混成一个 `Option` 就只能给出笼统且往往误导的错误信息。
#[cfg(unix)]
async fn identify_all(prove_token: Option<String>) -> Vec<(Probe, std::path::PathBuf)> {
    let mut set = tokio::task::JoinSet::new();
    for path in candidate_paths() {
        let prove = prove_token.clone();
        set.spawn(async move { (probe(&path, prove.as_deref()).await, path) });
    }
    let mut out = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((p, path)) = joined {
            if !matches!(p, Probe::Dead) {
                out.push((p, path));
            }
        }
    }
    out
}

/// 终端注入专用的配对变量名。iShell 往自己的终端会话里注入它（同时注入旧名
/// `ISHELL_MCP_TOKEN` 兼容旧代理）。**它必须是一个 MCP 配置里不会出现的名字**，理由见
/// [`pairing_token`]。
#[cfg(unix)]
const LAUNCH_TOKEN_VAR: &str = "ISHELL_PAIR_TOKEN";
/// 旧名：既是早期终端注入的变量，也是「复制配对配置」让用户写进 MCP server env 的变量。
#[cfg(unix)]
const CONFIG_TOKEN_VAR: &str = "ISHELL_MCP_TOKEN";

#[cfg(unix)]
const LAUNCH_HOST_VAR: &str = "ISHELL_HOST";

/// 本代理该用哪个配对 token。**「启动这个 AI 的那个终端」注入的专用变量优先，MCP 配置
/// 里写的值只作兜底。**
///
/// 为什么不能直接读 `ISHELL_MCP_TOKEN`（多机串台的真正根因，2026-09-18 在生产服务器上实测）：
/// AI 客户端（Claude Code 等）spawn MCP server 时，会用配置里 `env` 块的值**覆盖**从终端继承
/// 来的同名变量。共用服务器账号时，`~/.claude.json` 的 user 级配置是**所有人共用**的——只要
/// 有一个人把自己的 token 写进了那里，服务器上所有人的代理就都带着**他的** token：iShell 在
/// 各自终端里注入的正确值被静默覆盖，所有人的请求都落到他的电脑上。实测：来自 4 个不同 IP 的
/// claude 进程，自身环境里 token 各不相同，它们起的代理却全都带着同一个 token。
///
/// 取值顺序：
/// 1. [`LAUNCH_TOKEN_VAR`]：只由终端注入、没人往配置里写，所以不会被覆盖。
/// 2. `ISHELL_MCP_TOKEN`：AI 不是从 iShell 终端启动的（IDE 里跑等），只能靠配置或启动前缀。
///
/// 两者不一致时按 1，并在 stderr 告警（配置里那个多半是别人写进共享配置的）。
/// **只有配置值**（注入缺失）时也会告警一次——这是静默串台最危险的形态。
///
/// 这里**刻意不去读父进程环境里的旧变量**：v6 起配对 token 已整体换新（见
/// `store::mcp_pairing_token`），旧版 iShell 注入的 `ISHELL_MCP_TOKEN` 只可能是作废的旧值，
/// 读它只会压过配置里正确的新值、再打一条方向相反的告警。
#[cfg(unix)]
fn pairing_token() -> Option<String> {
    let (token, overridden, config_only) = resolve_pairing_token(
        std::env::var(LAUNCH_TOKEN_VAR).ok(),
        std::env::var(CONFIG_TOKEN_VAR).ok(),
    );
    if overridden {
        eprintln!(
            "ishell-mcp: 终端注入的 {LAUNCH_TOKEN_VAR} 与 MCP 配置里 env.{CONFIG_TOKEN_VAR} 不一致，\
             按终端的来（配置里的值多半是别人写进共享配置的，会把请求路由到他的电脑）。\
             建议从 AI 的 MCP 配置（如 ~/.claude.json）里删掉 {CONFIG_TOKEN_VAR}。"
        );
    }
    if config_only {
        eprintln!(
            "ishell-mcp: 未检测到终端注入的 {LAUNCH_TOKEN_VAR}，正在使用配置中的 {CONFIG_TOKEN_VAR}。\
             若它不是你本机的 token，请求会静默路由到别的机器。建议从 iShell 终端启动 AI，\
             或在终端右键「立即注入配对标识」后再启动；不要把别人的 token 写进 ~/.claude.json。"
        );
    }
    token
}

#[cfg(unix)]
fn launch_host() -> Option<String> {
    std::env::var(LAUNCH_HOST_VAR)
        .ok()
        .map(|s| mcp_protocol::sanitize_hostname(&s))
        .filter(|s| !s.is_empty() && s != "unknown-host")
}

#[cfg(unix)]
fn reject_if_host_mismatch(peer_host: &str) -> Result<(), String> {
    let Some(mine) = launch_host() else {
        return Ok(());
    };
    if peer_host.is_empty() || peer_host == "unknown-host" {
        return Ok(()); // 旧 GUI 没带主机名，无法校验
    }
    if mcp_protocol::hosts_match(&mine, peer_host) {
        return Ok(());
    }
    Err(host_mismatch_msg(&mine, std::slice::from_ref(&peer_host)))
}

/// 改绑时对照**当初绑定记下的主机**，不依赖终端是否注入了 `ISHELL_HOST`。
///
/// 窄角：IDE 启动 / 连上 2s 内敲过键 → 没有 `ISHELL_HOST`；原实例下线后附近只剩另一台
/// 共用 token 的机器。`reject_if_host_mismatch` 此时无主机可校验会放行。拿绑定当时的
/// `Instance.host` 就能挡住。旧 GUI（空/`unknown-host`）无法对照，仍放行。
#[cfg(unix)]
fn rebind_host_ok(old_host: Option<&str>, new_host: &str) -> Result<(), String> {
    let Some(old) = old_host.filter(|h| !h.is_empty() && *h != "unknown-host") else {
        return Ok(());
    };
    if new_host.is_empty() || new_host == "unknown-host" {
        return Ok(());
    }
    if mcp_protocol::hosts_match(old, new_host) {
        return Ok(());
    }
    Err(format!(
        "当初绑定的 iShell 在主机 {old}，现在唯一匹配的实例在 {new_host}。\
         不会静默改绑到另一台机器。请重新发起 MCP 连接；若确实要换机器，\
         请在那台 iShell 的终端里启动 AI（会注入 {LAUNCH_HOST_VAR}）。"
    ))
}

#[cfg(unix)]
fn reject_if_rebind_cross_host(new_host: &str) -> Result<(), String> {
    let old = BOUND_HOST.lock().unwrap().clone();
    rebind_host_ok(old.as_deref(), new_host)
}

/// 绑定弹窗发出去之前，按终端注入的 `ISHELL_HOST` 筛掉别的机器。
///
/// 只在绑定成功后再 `reject_if_host_mismatch` 会先让别的电脑弹出「允许绑定」，用户点了
/// 才报主机不一致——一对多场景里这是误导，也打扰了不相关的人。旧 GUI（空/`unknown-host`）
/// 无法校验，仍保留给后续 `reject_if_host_mismatch` 放行。没注入主机名时不过滤（IDE 启动）。
#[cfg(unix)]
fn keep_instances_for_launch_host(
    found: Vec<(String, u32, std::path::PathBuf, String, String)>,
    launch: Option<&str>,
) -> Result<Vec<(String, u32, std::path::PathBuf, String, String)>, String> {
    let Some(mine) = launch.filter(|h| !h.is_empty() && *h != "unknown-host") else {
        return Ok(found);
    };
    // 本来就没人配对成功：把「空列表」交给外层报 token/版本，不要冒充主机不一致。
    if found.is_empty() {
        return Ok(found);
    }
    let seen: Vec<String> = found
        .iter()
        .map(|(_, _, _, _, h)| h.clone())
        .collect();
    let kept: Vec<_> = found
        .into_iter()
        .filter(|(_, _, _, _, h)| {
            h.is_empty() || h == "unknown-host" || mcp_protocol::hosts_match(mine, h)
        })
        .collect();
    if kept.is_empty() {
        let listed: Vec<&str> = seen
            .iter()
            .map(String::as_str)
            .filter(|h| !h.is_empty() && *h != "unknown-host")
            .collect();
        return Err(host_mismatch_msg(mine, &listed));
    }
    Ok(kept)
}

#[cfg(unix)]
fn host_mismatch_msg(mine: &str, peers: &[&str]) -> String {
    let where_ = if peers.is_empty() {
        "未知主机".to_string()
    } else {
        peers.join("、")
    };
    format!(
        "配对 token 对应的 iShell 不在主机 {mine} 上（发现的实例在 {where_}）。\
         这通常意味着配置里的 {CONFIG_TOKEN_VAR} 来自另一台机器，或多台机器同步了同一份 token。\
         请在你当前这台 iShell 的终端里启动 AI（会自动注入 {LAUNCH_TOKEN_VAR} 与 {LAUNCH_HOST_VAR}），\
         并从 MCP 配置中删掉别人的 token。"
    )
}

/// [`pairing_token`] 的纯判定：`(选定的 token, 是否推翻了配置里的值, 是否仅用了配置兜底)`。
/// 空白值一律视同没设。
#[cfg(unix)]
fn resolve_pairing_token(
    launch: Option<String>,
    config: Option<String>,
) -> (Option<String>, bool, bool) {
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match (clean(launch), clean(config)) {
        (Some(l), Some(c)) if l != c => (Some(l), true, false),
        (Some(l), _) => (Some(l), false, false),
        (None, Some(c)) => (Some(c), false, true),
        (None, None) => (None, false, false),
    }
}

/// 决定这个代理这辈子只操作哪个 iShell 实例。只在首次需要连接时跑一次。
/// 返回 `(id, path, ticket, host)`。
#[cfg(unix)]
async fn bind_instance() -> Result<(String, std::path::PathBuf, String, String), String> {
    // 配对 token（多机共用同一 AI 服务器账号时的隔离）：设了 `ISHELL_MCP_TOKEN` 就走双向
    // 挑战-应答握手、**只认握手通过的实例**，请求绝不会串到别人的电脑上；没设则从协议 v5
    // 起直接拒绝并给出配置指引（匿名绑定的广播弹窗是它要根治的东西，见下面的拒绝分支）。
    // 见 `store::mcp_pairing_token`。
    let want_token = pairing_token();

    // 显式指定：脚本化/手动隧道场景的逃生口。最明确的意图，永远优先，也不弹任何窗。
    //
    // 但「指定了哪条 socket」和「这条 socket 后面是不是我的 iShell」是两件事：同时设了
    // token 就必须照样过握手。此前这里走的是裸 `identify()`，于是一条过期的
    // ISHELL_MCP_SOCKET（手工隧道留下的很常见）会让多机隔离**静默失效**——命令照常执行，
    // 只是落到了别人的电脑上，而这正是 token 要防的唯一一件事。宁可报错也不能猜。
    if let Some(p) = std::env::var_os("ISHELL_MCP_SOCKET") {
        let path = std::path::PathBuf::from(p);
        let dead = || {
            format!(
                "ISHELL_MCP_SOCKET 指定的 socket 连不上、或对面不是 iShell：{}",
                path.display()
            )
        };
        let Some(token) = want_token.as_deref() else {
            // v6 起没有 token 就拿不到连接凭据，iShell 不会执行任何请求——指定了 socket 也一样。
            return Err(format!(
                "ISHELL_MCP_SOCKET 指定了 {}，但没有配对 token：iShell（协议 v6 起）只执行完成\
                 配对握手的请求。请同时设置 {LAUNCH_TOKEN_VAR}（在你那台 iShell 的 MCP 设置里\
                 「复制配对配置」）",
                path.display()
            ));
        };
        return match probe(&path, Some(token)).await {
            Probe::Paired { id, ver, ticket, host } => {
                check_proto_version(ver)?;
                reject_if_host_mismatch(&host)?;
                Ok((id, path, ticket, host))
            }
            // 活着但没通过握手。先看版本——版本不符时 `probe` 本来就跳过握手，
            // 报成 token 不符会把人引向死胡同（怎么核对 token 都是对的）。
            Probe::Answered { ver, .. } => {
                check_proto_version(ver)?;
                Err(format!(
                    "ISHELL_MCP_SOCKET 指向的 iShell 没有通过配对握手：{}\n\
                     同时设置了 ISHELL_MCP_TOKEN，说明你要求只连自己那台 iShell，\
                     所以这里不会退而求其次去连它。常见原因是这条 ISHELL_MCP_SOCKET \
                     是早先手工隧道留下的、已经指向别人（或别的）iShell——去掉它即可\
                     改回按 token 自动发现；若确实要用这条 socket，请核对两边的配对 token。",
                    path.display()
                ))
            }
            Probe::Dead => Err(dead()),
        };
    }

    // v5：未配置配对 token 就不再「匿名发现 + 弹窗选择」。无 token 代理的广播 Bind 正是
    // 「绑定弹窗落到服务器上每一台 iShell」的那条路径（0.21 的调查结论）；堵住它的正确位置
    // 是代理自己——单由 GUI 静默不应答，旧代理只会报成一句莫名其妙的「连不上」，排查无门。
    // 这里直接给出可操作的指引，一次说清。（显式 ISHELL_MCP_SOCKET 的手动隧道是另一回事：
    // 那是用户点名的路径，意图明确，在上面放行，见上面的分支。）
    let Some(token) = want_token else {
        return Err(
            "这份 ishell-mcp 没有配置配对 token（ISHELL_MCP_TOKEN），而这台 iShell 只响应携\
             带配对 token 的请求：匿名绑定会让绑定弹窗广播到服务器上**每一台** iShell，先点\
             「允许」的窗口胜出——误点允许会把别人的 AI 绑到你的电脑上，所以从协议 v5 起不\
             再提供无 token 的匿名绑定。\n\
             解决办法（任选其一）：\n\
             1. 在你自己那台 iShell 的终端会话里启动 AI：iShell 会自动注入配对 token，多数情\
             况零配置即可；\n\
             2. AI 不在 iShell 终端里启动时：在那台 iShell 的 MCP 设置里点「复制配对配置」，\
             启动 AI 时把它加在命令前面（如 `ISHELL_PAIR_TOKEN=… ISHELL_HOST=… claude`）。**不要**写进 AI 的\
             全局 MCP 配置（如 ~/.claude.json 的 user 级 env）——多人共用服务器账号时那份配置\
             是所有人共用的，会把所有人的 AI 都绑到你的电脑上。"
                .into(),
        );
    };

    let all = identify_all(Some(token)).await;
    // 答话了但**版本不符**的实例：它们是「重新部署 ishell-mcp」这条提示的依据，混进下面的
    // 候选里只会让用户去核对一个根本没错的 token。
    //
    // 但只在**没有任何一个同版本对端**时才给这条提示：共用账号时同事跑着一台旧版 iShell 是
    // 常态，拿别人的版本去解释「我这边配对没成」，就又变成一条误导性建议（和它要修的那个
    // 问题同一个形状）。只要有同版本的对端答了话，配对没成就该往 token 上说。
    let stale = all
        .iter()
        .filter_map(|(p, _)| p.ident())
        .map(|(_, ver)| ver)
        .find(|v| *v != mcp_protocol::MCP_PROTOCOL_VERSION)
        .filter(|_| {
            !all.iter()
                .filter_map(|(p, _)| p.ident())
                .any(|(_, v)| v == mcp_protocol::MCP_PROTOCOL_VERSION)
        });
    let answered = all
        .iter()
        .filter(|(p, _)| matches!(p, Probe::Answered { .. }))
        .count();
    // 只收握手通过的实例（连同它签发的凭据）。
    let mut found: Vec<(String, u32, std::path::PathBuf, String, String)> = Vec::new();
    for (p, path) in all {
        let Probe::Paired { id, ver, ticket, host } = p else {
            continue;
        };
        // 按实例去重：多条路径可能通向同一个 iShell（见 candidate_paths 的说明）。
        if !found.iter().any(|(known, ..)| *known == id) {
            found.push((id, ver, path, ticket, host));
        }
    }
    // 多机共用 token 时先告警（过滤前，才能看见「不同主机」），再按 ISHELL_HOST 丢掉
    // 别人的电脑——否则 choose_instance 会先向那些窗口发 Bind 弹窗。
    if found.len() > 1 {
        warn_if_shared_token_across_hosts(&found);
    }
    let mut found = keep_instances_for_launch_host(found, launch_host().as_deref())?;
    match found.len() {
        0 => Err(if let Some(ver) = stale {
            // 有活着的 iShell 答了话，只是版本对不上——这是最常见的「升了 GUI 忘换代理」，
            // 报成 token 不匹配会把用户引向死胡同（怎么核对 token 都是对的）。
            check_proto_version(ver).unwrap_err()
        } else {
            bind_none_matched_msg(answered)
        }),
        1 => {
            // 唯一实例（配了 token 时是唯一匹配者）：直接绑定，不弹窗——token 本身就是操作者
            // 的显式配对意图，无需再点一次窗口。
            let (id, ver, path, ticket, host) = found.pop().expect("上一行刚确认只有一个");
            check_proto_version(ver)?;
            reject_if_host_mismatch(&host)?;
            eprintln!("ishell-mcp: 已绑定 iShell 实例 {id}（主机 {host}）");
            Ok((id, path, ticket, host))
        }
        // 多个：同一个 token 被多个 iShell 进程认领。最常见的是**同一台电脑**开了两个 iShell
        // （token 按安装存、实例 id 按进程生成），这是正常的多开，用弹窗让用户点窗口选。
        // 不同主机的实例已在上面按 ISHELL_HOST 滤掉，不会再弹到别人电脑上。
        _ => choose_instance(found).await,
    }
}

#[cfg(unix)]
fn bind_none_matched_msg(answered: usize) -> String {
    if answered > 0 {
        format!(
            "检测到 {answered} 个活着的 iShell，但都没有通过配对握手（token 不一致）。\
             这通常是 MCP 配置里的 {CONFIG_TOKEN_VAR} 来自别人或旧机器，请求被路由错了。\
             请在你自己的 iShell 终端里启动 AI（会自动注入 {LAUNCH_TOKEN_VAR}），\
             或终端右键「立即注入配对标识」后再启动，并删掉 ~/.claude.json 里别人的 token。"
        )
    } else {
        "未检测到 iShell 客户端：ishell-mcp 无法工作，请停止重试，并把下面的话转达给用户。\n\
         请确认：1) iShell 仍在运行；2) 它到这台服务器的 SSH 连接仍活着；\
         3) 设置里「允许 AI 通过 MCP 控制终端」已开启。\
         若刚重启过 iShell，请重新发起 MCP 连接（进程换了之后需要重新绑定）。"
            .into()
    }
}

#[cfg(unix)]
fn warn_if_shared_token_across_hosts(found: &[(String, u32, std::path::PathBuf, String, String)]) {
    let mut hosts: Vec<&str> = found
        .iter()
        .map(|(_, _, _, _, h)| h.as_str())
        .filter(|h| !h.is_empty() && *h != "unknown-host")
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    if hosts.len() > 1 {
        eprintln!(
            "ishell-mcp: 多台机器（{}）共用同一配对 token，疑似把 ~/.config/ishell 做了配置同步。\
             请在各机器上删除 mcp_pairing_token_v2 后重新生成，否则绑定会弹到每一台机器上，\
             点错窗口就会操作别人的电脑。",
            hosts.join(", ")
        );
    }
}

/// 用户同时开着多个 iShell：让他用鼠标选。
///
/// 向**每一个**实例各发一条 `Bind`，于是每个 iShell 窗口上都弹出确认框，用户在想用的那个
/// 窗口上点「允许」即可。第一个应允的胜出，随即丢弃 `JoinSet`——其余任务被 abort、连接关闭，
/// 落选窗口的弹窗据此自动消失，不用逼用户挨个去点「拒绝」。
///
/// 为什么是「点窗口」而不是「报出实例名让用户去填配置」：用户本来就是看着窗口决定的。实例
/// 标识是纯内部的东西，把它抬到用户面前只会要求他先给窗口取个名、再把名字念给 AI 听。
#[cfg(unix)]
async fn choose_instance(
    found: Vec<(String, u32, std::path::PathBuf, String, String)>,
) -> Result<(String, std::path::PathBuf, String, String), String> {
    let count = found.len();
    let mut set = tokio::task::JoinSet::new();
    for (id, ver, path, ticket, host) in found {
        set.spawn(async move {
            let stream = connect_timeout(&path)
                .await
                .map_err(|_| "连接 iShell socket 超时".to_string())?
                .map_err(|e| e.to_string())?;
            exchange(
                stream,
                Some(id.clone()),
                Some(caller_origin()),
                Some(current_actor()),
                Some(ticket.clone()),
                McpReqKind::Bind,
                BIND_TIMEOUT,
            )
            .await?;
            Ok::<_, String>((id, ver, path, ticket, host))
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok(Ok((id, ver, path, ticket, host))) = joined {
            // set 在这里被丢弃 → 其余任务 abort → 落选窗口的弹窗据此自动消失。
            check_proto_version(ver)?;
            reject_if_host_mismatch(&host)?;
            eprintln!("ishell-mcp: 已绑定 iShell 实例 {id}（主机 {host}，用户在窗口上点了允许）");
            return Ok((id, path, ticket, host));
        }
    }
    Err(format!(
        "发现 {count} 个 iShell 实例，但没有任何一个窗口批准这次连接（用户拒绝，或 5 分钟\
         没有响应）。请让用户在他想让你操作的那个 iShell 窗口上点「允许」，然后重试。\
         注意这些窗口未必在你这台机器上：iShell 是经 SSH 反向转发接进来的，窗口在用户\
         自己的电脑上。若多台机器弹了窗，它们可能共用了同一配对 token（配置同步），\
         点错就会操作别人的电脑。"
    ))
}

/// 拿一条连向**绑定实例**的连接，外加要填进请求的实例标识与连接凭据。
#[cfg(unix)]
async fn connect_bound() -> Result<(UnixStream, String, String), String> {
    BOUND_INIT
        .get_or_try_init(|| async {
            let (id, path, ticket, host) = bind_instance().await?;
            store_binding(id, path, ticket, host);
            Ok::<_, String>(())
        })
        .await?;
    let id = BOUND_INSTANCE
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "内部错误：绑定未完成".to_string())?;
    let cached = PATH_CACHE.lock().unwrap().clone();
    let ticket = TICKET.lock().unwrap().clone().unwrap_or_default();
    if let Some(path) = cached {
        if let Ok(Ok(stream)) = connect_timeout(&path).await {
            return Ok((stream, id, ticket));
        }
    }
    // 缓存路径连不上了：多半是反向转发那条 SSH 重连、换了随机名。先按旧 id 找回；
    // 找不到且恰好只剩一个 token 匹配实例时才改绑（iShell 重启）。必须重新握手（id
    // 不是秘密，只认 id 的话，同账号的人摆一个冒充该 id 的 socket 就能接管后续请求）。
    let (path, ticket) = rediscover_bound(&id).await?;
    let id = BOUND_INSTANCE
        .lock()
        .unwrap()
        .clone()
        .unwrap_or(id);
    let stream = connect_timeout(&path)
        .await
        .map_err(|_| "连接 iShell socket 超时".to_string())?
        .map_err(|e| e.to_string())?;
    Ok((stream, id, ticket))
}

/// rediscover 的纯判定，便于单测。原 id 还在就沿用；原 id 消失且恰好 1 个 token 匹配者
/// 才允许改绑；0 个或多于 1 个都报 lost（多开时静默换绑会串 uid）。
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum RediscoverOutcome {
    Same { path: std::path::PathBuf, ticket: String },
    Rebound { new_id: String, path: std::path::PathBuf, ticket: String, host: String },
    Lost { paired: Vec<(String, String)>, answered: usize },
}

#[cfg(unix)]
fn classify_rediscover(want_id: &str, probes: &[(Probe, std::path::PathBuf)]) -> RediscoverOutcome {
    let mut paired: Vec<(String, std::path::PathBuf, String, String)> = Vec::new();
    let mut answered = 0usize;
    for (p, path) in probes {
        match p {
            Probe::Paired { id, ticket, host, .. } => {
                if !paired.iter().any(|(known, ..)| known == id) {
                    paired.push((id.clone(), path.clone(), ticket.clone(), host.clone()));
                }
            }
            Probe::Answered { .. } => answered += 1,
            Probe::Dead => {}
        }
    }
    if let Some((_, path, ticket, _)) = paired.iter().find(|(id, ..)| id == want_id) {
        return RediscoverOutcome::Same {
            path: path.clone(),
            ticket: ticket.clone(),
        };
    }
    if paired.len() == 1 {
        let (new_id, path, ticket, host) = paired.pop().expect("上一行刚确认只有一个");
        return RediscoverOutcome::Rebound {
            new_id,
            path,
            ticket,
            host,
        };
    }
    RediscoverOutcome::Lost {
        paired: paired
            .into_iter()
            .map(|(id, _, _, host)| (id, host))
            .collect(),
        answered,
    }
}

#[cfg(unix)]
fn format_lost(want_id: &str, paired: &[(String, String)], answered: usize) -> String {
    if paired.is_empty() && answered == 0 {
        return format!(
            "找不到当初绑定的那个 iShell 实例了（id {want_id}）。可能原因：iShell 已退出、\
             它到这台服务器的 SSH 断了、或设置里关掉了 AI 控制。请确认客户端仍在运行且 SSH \
             仍活着；若刚重启过 iShell，请重新发起 MCP 连接。"
        );
    }
    if paired.is_empty() {
        return format!(
            "当初绑定的 iShell 实例（id {want_id}）已消失，附近有 {answered} 个活着的 iShell \
             但都没通过配对握手（token 不一致）。请核对 {LAUNCH_TOKEN_VAR}/{CONFIG_TOKEN_VAR}，\
             并重新发起 MCP 连接。"
        );
    }
    let others: Vec<String> = paired
        .iter()
        .map(|(id, host)| {
            if host.is_empty() {
                id.clone()
            } else {
                format!("{id}@{host}")
            }
        })
        .collect();
    format!(
        "当初绑定的 iShell 实例（id {want_id}）已消失，但还有 {} 个其它匹配实例（{}）。\
         多开时不会自动改绑——静默换一个窗口执行，命令会落到你没预期的机器上。\
         请重新发起 MCP 连接，并在想用的那个窗口上点「允许」。",
        others.len(),
        others.join(", ")
    )
}

/// 在候选里重新握手，找回已绑定的那个实例：更新路径缓存与凭据并返回。
/// 连续几次都找不到旧 id 时，若恰好只剩一个 token 匹配者则改绑（覆盖 iShell 重启）。
#[cfg(unix)]
async fn rediscover_bound(id: &str) -> Result<(std::path::PathBuf, String), String> {
    let token = pairing_token().ok_or_else(|| format_lost(id, &[], 0))?;
    const ATTEMPTS: u32 = 3;
    let mut last = RediscoverOutcome::Lost {
        paired: Vec::new(),
        answered: 0,
    };
    for attempt in 0..ATTEMPTS {
        let all = identify_all(Some(token.clone())).await;
        last = classify_rediscover(id, &all);
        match &last {
            RediscoverOutcome::Same { path, ticket } => {
                *PATH_CACHE.lock().unwrap() = Some(path.clone());
                *TICKET.lock().unwrap() = Some(ticket.clone());
                return Ok((path.clone(), ticket.clone()));
            }
            RediscoverOutcome::Rebound { .. } | RediscoverOutcome::Lost { .. } => {
                // SSH 重连有一个瞬时空窗：旧 socket 已没、新反向转发还没注册完。
                // 此时可能暂时只看见另一台同 token 的机器——先等再判，避免误改绑。
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
            }
        }
    }
    match last {
        RediscoverOutcome::Same { path, ticket } => Ok((path, ticket)),
        RediscoverOutcome::Rebound {
            new_id,
            path,
            ticket,
            host,
        } => {
            reject_if_host_mismatch(&host)?;
            reject_if_rebind_cross_host(&host)?;
            store_binding(new_id.clone(), path.clone(), ticket.clone(), host.clone());
            eprintln!(
                "ishell-mcp: 原实例 {id} 已消失，已改绑到 {new_id}（主机 {host}）。\
                 此前所有 session uid 全部失效，请重新 list_sessions / open_session。"
            );
            Ok((path, ticket))
        }
        RediscoverOutcome::Lost { paired, answered } => Err(format_lost(id, &paired, answered)),
    }
}

/// 凭据被拒（iShell 淘汰/过期）后重新握手换一张：先试缓存路径，不行再全量找回同一实例。
#[cfg(unix)]
async fn refresh_ticket() -> Result<(), String> {
    let Some(id) = BOUND_INSTANCE.lock().unwrap().clone() else {
        return Err("尚未绑定任何 iShell 实例".into());
    };
    let token = pairing_token().ok_or("没有配对 token，无法重新握手")?;
    let cached = PATH_CACHE.lock().unwrap().clone();
    if let Some(path) = cached {
        if let Some((found, _, ticket, _)) = pair_handshake(&path, &token).await {
            if found == id {
                *TICKET.lock().unwrap() = Some(ticket);
                return Ok(());
            }
        }
    }
    rediscover_bound(&id).await.map(|_| ())
}

/// 在一条已建立的连接上完成一问一答：写一行请求 JSON，读一行响应 JSON。
///
/// `instance` 点名这条请求发给谁，由对端自己校验（见 `McpRequest::is_addressed_to`）。
/// 只有 `Identify` 填 `None`——那时还不知道对面是谁。
/// `origin`（可读来源）与 `actor`（本进程标识）随每条业务请求携带：`origin` 给对端弹窗
/// 展示用，`actor` 让 iShell 落实「AI 窗口归开它的 AI 专用」。纯元数据，不构成身份凭证。
/// 匿名探测（Identify）两者都传 `None`。
#[cfg(unix)]
async fn exchange(
    stream: UnixStream,
    instance: Option<String>,
    origin: Option<String>,
    actor: Option<String>,
    ticket: Option<String>,
    kind: McpReqKind,
    response_timeout: std::time::Duration,
) -> Result<McpReqResult, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&McpRequest {
        id,
        instance,
        origin,
        actor,
        ticket,
        kind,
    })
    .map_err(|e| e.to_string())?;
    line.push('\n');
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, w.write_all(line.as_bytes()))
        .await
        .map_err(|_| "发送请求给 iShell 超时".to_string())?
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(r);
    let mut resp_line = String::new();
    let read = tokio::time::timeout(response_timeout, reader.read_line(&mut resp_line))
        .await
        .map_err(|_| {
            "等待 iShell 响应超时（远超请求本身的 timeout_ms，可能是 GUI 卡死或连接异常）"
                .to_string()
        })?
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Err("iShell 未返回任何响应就关闭了连接".into());
    }
    let resp: McpResponse = serde_json::from_str(resp_line.trim()).map_err(|e| e.to_string())?;
    resp.result
}

/// 本地 socket connect/写请求的超时：正常情况下应该是瞬时的（同机 Unix socket，GUI 活着的
/// 话），卡住这么久基本可以断定对端有问题，没必要陪它无限等。
const CONNECT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// 等 GUI 写出传输判定的上限。必须 **长于** GUI 侧 `VERDICT_TIMEOUT`（30s）：判定本身
/// 只在字节流发完后才产生，慢主机上 worker 收尾可能超过 5s 的 connect 超时，agent 会误报
/// 「等待 iShell 的传输判定超时」而 GUI 其实还在 30s 窗口内。
const VERDICT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// 等响应的超时：某些调用（run_command/poll_run/write_file/read_file）自带 timeout_ms，
/// GUI 侧已经把它 clamp 到最长 24 小时——这里给一个比那个上限稍宽松的兜底，只用来防
/// "GUI 卡死/半关闭连接导致这次工具调用永远挂起"，不应该在正常使用中被触发。
const RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25 * 60 * 60);

/// 连接 iShell 主进程的本地 socket，发一条请求、等一行 JSON 响应（一次连接=一问一答）。
/// 整套本地 IPC 建立在 Unix domain socket 上（tokio 的 UnixListener/UnixStream 只在 unix
/// 平台提供），Windows 上没有等价实现——不是漏掉了 cfg 门，而是这个特性眼下确实不支持
/// Windows；这里给出清晰的运行时报错而不是让编译直接失败，其余工具定义/MCP server 骨架
/// 在所有平台上都能正常编译。
#[cfg(unix)]
async fn call(kind: McpReqKind) -> Result<McpReqResult, String> {
    let send = |stream, instance, ticket, kind| {
        exchange(
            stream,
            Some(instance),
            Some(caller_origin()),
            Some(current_actor()),
            Some(ticket),
            kind,
            RESPONSE_TIMEOUT,
        )
    };
    let (stream, instance, ticket) = connect_bound().await?;
    match send(stream, instance, ticket, kind.clone()).await {
        // 凭据被拒（iShell 按空闲时长淘汰了它）：重新握手换一张，重试一次。被拒发生在 iShell
        // 执行任何东西之前（`handle_conn` 先验凭据），所以重试不会让命令执行两遍。
        Err(e) if e.starts_with(mcp_protocol::TICKET_REJECTED) => {
            refresh_ticket().await?;
            let (stream, instance, ticket) = connect_bound().await?;
            send(stream, instance, ticket, kind).await
        }
        other => other,
    }
}

/// 流式拷贝版的「凭据被拒就重新握手、重试一次」（普通请求见 [`call`]）。拷贝函数每次调用都
/// 重新打开文件、重新建连接，整体重跑一遍是安全的；被拒发生在 iShell 执行之前（上传的文件体
/// 会被 iShell 排干后才回错，错误不会被 RST 吞掉），不会拷两遍。
#[cfg(unix)]
async fn with_ticket_retry<F, Fut>(f: F) -> Result<McpReqResult, String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<McpReqResult, String>>,
{
    match f().await {
        Err(e) if e.starts_with(mcp_protocol::TICKET_REJECTED) => {
            refresh_ticket().await?;
            f().await
        }
        other => other,
    }
}

#[cfg(not(unix))]
async fn with_ticket_retry<F, Fut>(f: F) -> Result<McpReqResult, String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<McpReqResult, String>>,
{
    f().await
}

/// 校验一个调用方本机路径：必须绝对、不含 `.`/`..` 路径段。跟 GUI 侧
/// `mcp_bridge.rs::validate_local_path` 是同一套规则（包括"按原始字符串拆分而不是
/// `Path::components()`"这个细节——后者会把非开头的 "." 直接规整掉，导致
/// "/tmp/./notes.txt" 这类路径检测不到）——那一份校验的是"GUI 所在机器"的路径，
/// 这里校验的是"ishell-mcp 代理进程所在机器"的路径，两者解析点不同、代码没法共享，
/// 但规则本身应该保持一致。
fn validate_caller_path(path_str: &str, field: &str) -> Result<(), String> {
    let path = std::path::Path::new(path_str);
    if !path.is_absolute() {
        return Err(format!("{field} 必须是运行 ishell-mcp 的调用方机器上的绝对路径"));
    }
    if path_str.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(format!("{field} 不能包含 \".\" 或 \"..\" 路径段"));
    }
    if path.file_name().is_none() {
        return Err(format!("{field} 缺少有效的文件名"));
    }
    Ok(())
}

/// 从运行 MCP client 的机器读取一个文件，并把原始字节紧随内部 JSON 请求写入 iShell。
/// 该函数只在代理进程本地打开 `local_path`；iShell GUI 从未解析该路径，因此跨主机使用时
/// 不会再把工作机路径错误地当成桌面机路径。
#[cfg(unix)]
async fn copy_to_remote_from_caller(
    session_uid: u64,
    local_path: String,
    remote_path: String,
    timeout_ms: u64,
) -> Result<McpReqResult, String> {
    validate_caller_path(&local_path, "local_path")?;
    let path = std::path::PathBuf::from(&local_path);
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("无法读取调用方文件 {local_path}: {error}"))?;
    if !metadata.is_file() {
        return Err("调用方流式上传当前只支持单个普通文件；目录请使用 git/rsync，或逐文件上传".into());
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (stream, instance, ticket) = connect_bound().await?;
    let (read_half, mut write_half) = stream.into_split();
    let request = McpRequest {
        id,
        instance: Some(instance),
        origin: Some(caller_origin()),
        actor: Some(current_actor()),
        ticket: Some(ticket),
        kind: McpReqKind::CopyToRemoteFromCaller {
            session_uid,
            remote_path,
            size: metadata.len(),
            timeout_ms,
        },
    };
    let mut header = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    header.push('\n');
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, write_half.write_all(header.as_bytes()))
        .await
        .map_err(|_| "发送上传请求给 iShell 超时".to_string())?
        .map_err(|error| error.to_string())?;

    let mut source = tokio::fs::File::open(&path)
        .await
        .map_err(|error| format!("无法打开调用方文件 {local_path}: {error}"))?;
    // 边推文件体、边读响应。GUI 可能在我们还在写时就拒绝（会话不存在/路径非法/并发满/
    // 用户拒绝授权）——先写完整个文件才读响应的话，超过内核缓冲的大文件拿到的是 EPIPE，
    // 真实错误被掩盖，同一错误大小文件表现不同。并发读后，拒绝一到手立刻丢弃写入 future、
    // 关闭连接（对端读端随之 EOF），把 GUI 给出的真实错误原样带给调用方。
    let mut response = String::new();
    let mut reader = BufReader::new(read_half);
    let body = async {
        tokio::io::copy(&mut source, &mut write_half).await?;
        write_half.shutdown().await?;
        Ok::<(), std::io::Error>(())
    };
    tokio::pin!(body);
    // biased + 读分支优先：GUI 拒绝请求是「先写响应、再关连接」——响应字节和对端关闭导致的
    // EPIPE 会同时就绪，而 tokio::select! 默认**随机**挑分支：若先轮上 body，报错路径返回
    // 「Broken pipe」，真实错误又被盖住。biased 让读确定性地优先；body 先出错时再用短超时
    // 追读一次响应行——追到了返回 GUI 的真实错误，追不到才报 body 的错误。
    let early = tokio::select! {
        biased;
        r = reader.read_line(&mut response) => Some(r),
        r = &mut body => match r {
            Ok(()) => None,
            Err(error) => {
                // 典型场景：GUI 已回错并关连接，我们的写撞上 EPIPE。它关连接前写下的
                // 响应行可能还躺在 socket 缓冲里——短超时追读，追到就把真实错误带回去。
                let mut late = String::new();
                if let Ok(Ok(n)) = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    reader.read_line(&mut late),
                )
                .await
                {
                    if n > 0 && !late.trim().is_empty() {
                        return serde_json::from_str::<McpResponse>(late.trim())
                            .map_err(|e| e.to_string())?
                            .result;
                    }
                }
                return Err(format!("发送调用方文件流失败: {error}"));
            }
        },
    };
    let read = match early {
        Some(r) => r.map_err(|error| error.to_string())?,
        None => {
            // 文件体已推完：按既有路径等判定行。注意 tokio 的 read_line **不能安全取消**：
            // 它开始时会 mem::take 走整个 String（已读到的部分字节在内），被取消时随
            // future 一起丢掉、不会还原——所以这里依赖的事实是「GUI 的响应是一小行、一次
            // write 写完」，select 里被丢弃的 read_line 不会产生需要接着读的半行。
            tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(&mut response))
                .await
                .map_err(|_| "等待 iShell 上传响应超时（可能是 GUI、SFTP 或连接异常）".to_string())?
                .map_err(|error| error.to_string())?
        }
    };
    if read == 0 {
        return Err("iShell 未返回上传结果就关闭了连接".into());
    }
    serde_json::from_str::<McpResponse>(response.trim())
        .map_err(|error| error.to_string())?
        .result
}

#[cfg(not(unix))]
async fn copy_to_remote_from_caller(
    _session_uid: u64,
    _local_path: String,
    _remote_path: String,
    _timeout_ms: u64,
) -> Result<McpReqResult, String> {
    Err("ishell-mcp 目前仅支持 Unix（Linux/macOS）系统的本地 IPC，暂不支持 Windows".into())
}

/// 把远端单文件流式下载到运行 MCP client 的机器，原始字节紧随 iShell 回的响应头之后收取。
/// 对称 `copy_to_remote_from_caller`：这个函数在代理进程本地打开/写 `local_path`，
/// iShell GUI 从未解析该路径——避免 `copy_from_remote` 和 `copy_to_remote` 的"本地"分别落在
/// 两台不同机器上这个此前存在的不一致 bug（GUI 侧只负责探测远端路径、把字节流回本连接）。
#[cfg(unix)]
async fn copy_from_remote_to_caller(
    session_uid: u64,
    remote_path: String,
    local_path: String,
    timeout_ms: u64,
) -> Result<McpReqResult, String> {
    validate_caller_path(&local_path, "local_path")?;
    let path = std::path::PathBuf::from(&local_path);

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (stream, instance, ticket) = connect_bound().await?;
    let (read_half, mut write_half) = stream.into_split();
    let request = McpRequest {
        id,
        instance: Some(instance),
        origin: Some(caller_origin()),
        actor: Some(current_actor()),
        ticket: Some(ticket),
        kind: McpReqKind::CopyFromRemoteToCaller { session_uid, remote_path, timeout_ms },
    };
    let mut header = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    header.push('\n');
    tokio::time::timeout(CONNECT_WRITE_TIMEOUT, write_half.write_all(header.as_bytes()))
        .await
        .map_err(|_| "发送下载请求给 iShell 超时".to_string())?
        .map_err(|error| error.to_string())?;

    let mut reader = BufReader::new(read_half);
    let mut header_line = String::new();
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(&mut header_line))
        .await
        .map_err(|_| "等待 iShell 下载响应超时（可能是 GUI、SFTP 或连接异常）".to_string())?
        .map_err(|error| error.to_string())?;
    if read == 0 {
        return Err("iShell 未返回下载结果就关闭了连接".into());
    }
    let resp: McpResponse = serde_json::from_str(header_line.trim()).map_err(|error| error.to_string())?;
    let size = match resp.result? {
        McpReqResult::CopyStreamHeader { size, .. } => size,
        _ => return Err("iShell 返回了意料之外的下载响应".into()),
    };

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("无法创建调用方目标目录 {}: {error}", parent.display()))?;
    }
    // 事务写：先写同目录临时文件，字节数校验通过后再原子改名换入。下载中断 / 不完整
    // 绝不能破坏调用方已有的同名原文件——直接 File::create(local_path) 的旧写法会先把原文件
    // 截断为 0，随后下载失败就只剩一个空/半截文件。改名在同一文件系统内原子，临时文件与目标
    // 同目录保证这一点。
    let tmp_path = std::path::PathBuf::from(format!(
        "{local_path}.ishell-part-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let outcome: Result<(), String> = async {
        let mut dest = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|error| format!("无法创建临时下载文件 {}: {error}", tmp_path.display()))?;
        // 分块收（线格式与理由见 mcp_protocol::read_framed_stream）。有了帧，判定的位置就与
        // 「实际收了多少字节」无关——iShell 中途失败时也能把真正的原因原样送到这里。
        let received = tokio::time::timeout(
            RESPONSE_TIMEOUT,
            mcp_protocol::read_framed_stream(&mut reader, &mut dest),
        )
        .await
        .map_err(|_| "等待 iShell 发送文件数据超时".to_string())??;
        // 判定：iShell 读完源文件后给出的最终结论，拿到它才敢换入。字节数对得上**不等于**
        // 这次传输是对的——size 取自传输开始前的 metadata，远端文件在传输期间长大的话，
        // 光看字节数是发现不了的（那正是这条判定要解决的问题）。
        let mut verdict_line = String::new();
        tokio::time::timeout(VERDICT_READ_TIMEOUT, reader.read_line(&mut verdict_line))
            .await
            .map_err(|_| "等待 iShell 的传输判定超时".to_string())?
            .map_err(|error| format!("读取 iShell 的传输判定失败: {error}"))?;
        let verdict: McpResponse = serde_json::from_str(verdict_line.trim())
            .map_err(|error| format!("iShell 的传输判定无法解析（{error}）：{verdict_line}"))?;
        verdict.result?;
        // 判定说成功，就该恰好是 header 里承诺的那个数。对不上说明两侧对协议的理解有分歧，
        // 属于程序错误而非传输故障——宁可报错也不能把一个来路不明的文件换入。
        if received != size {
            return Err(format!(
                "iShell 判定传输成功，但字节数与它自己声明的不符：应为 {size}，实收 {received}"
            ));
        }
        dest.flush().await.map_err(|error| format!("落盘调用方文件失败: {error}"))?;
        let _ = dest.sync_all().await; // 尽力 fsync，换入前确保字节真正落盘
        drop(dest);
        // 原子换入：同目录改名，替换调用方已有的同名文件（Unix rename 原子且直接覆盖）
        tokio::fs::rename(&tmp_path, &path)
            .await
            .map_err(|error| format!("换入下载文件失败 {local_path}: {error}"))
    }
    .await;
    if let Err(e) = outcome {
        let _ = tokio::fs::remove_file(&tmp_path).await; // 失败：清理临时文件，原文件未动
        return Err(e);
    }
    Ok(McpReqResult::Copied { path: local_path })
}

#[cfg(not(unix))]
async fn copy_from_remote_to_caller(
    _session_uid: u64,
    _remote_path: String,
    _local_path: String,
    _timeout_ms: u64,
) -> Result<McpReqResult, String> {
    Err("ishell-mcp 目前仅支持 Unix（Linux/macOS）系统的本地 IPC，暂不支持 Windows".into())
}

#[cfg(not(unix))]
async fn call(_kind: McpReqKind) -> Result<McpReqResult, String> {
    Err("ishell-mcp 目前仅支持 Unix（Linux/macOS）系统的本地 IPC，暂不支持 Windows".into())
}

/// 等一条会话连上（最多 `max`）：`open_session` 刚返回时 connected=false，此刻 run_command
/// 会原样报错，逼着调用方「先 list_sessions 确认、再 run_command」，每个新会话固定多一轮
/// 往返。这里代它等：每 300ms 问一次 GUI，连上即返回。超时或会话不存在也返回——把报错
/// 留给真正的命令调用（会话不存在时那条报错自带可用会话列表，比这里干等更有用）。
async fn wait_connected(session_uid: u64, max: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        match call(McpReqKind::ListSessions).await {
            Ok(McpReqResult::Sessions(list)) => match list.iter().find(|s| s.uid == session_uid) {
                Some(s) if s.connected => return,
                Some(_) => {} // 还在连接/认证中，继续等
                None => return, // 没这个会话：留给后续调用报错
            },
            _ => return, // 查询本身失败（GUI 未运行等）：同样留给后续调用
        }
        if start.elapsed() >= max {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
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
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    /// 要在该终端里执行的 shell 命令（会像用户手动输入一样实时显示在终端里）
    pub command: String,
    /// 等待命令结束的超时毫秒数；超时仍未结束会返回 finished=false + run_id，可用 poll_run 续等
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListSessionsArgs {
    /// 可选过滤子串：只返回标题或主机名包含它（不区分大小写）的会话。会话多时用它省上下文
    pub filter: Option<String>,
}

/// 启动后立即返回的命令参数。它复用 `run_command` 的哨兵和 `poll_run` 状态机，
/// 只是把等待窗口固定为协议允许的最小值，避免 MCP 客户端的空闲超时占住等待者。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StartCommandArgs {
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    pub command: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PollRunArgs {
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
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
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
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
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    /// 只要最后这么多行（默认 200，够用再加大）；传 0 表示不限制（回滚很长时可能很大）
    #[serde(default = "default_max_lines")]
    pub max_lines: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendInputArgs {
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    /// 要发送的文本/按键，不会自动加回车。默认逐字节原样发送：反斜杠就是反斜杠，"\r" 两个
    /// 字符收到的是两个字符、不是回车；发控制键（回车/Ctrl-D/Esc 等）必须设 escapes=true
    /// （写 "\r" 字面转义即真实回车；真实控制字节也会原样通过）
    pub text: String,
    /// 可选，默认 false。true 时把 text 里的字面转义解析成按键：\r \n \t \\ 与 \xHH
    /// （00-7F）；写错（未定义转义、\x 后不足两位十六进制）会直接报错、不发送
    #[serde(default)]
    pub escapes: bool,
}

fn default_file_timeout_ms() -> u64 {
    20_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
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
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    pub path: String,
    /// 默认 false：遵守 20MB 软上限、二进制内容直接报错。true：放宽到 128MB 且把二进制也
    /// 当文本硬解码——只在确实需要读大文件、且确定是文本时才传 true，否则会得到乱码。
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_file_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_copy_timeout_ms() -> u64 {
    300_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CopyToRemoteArgs {
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    /// 运行 ishell-mcp 的调用方机器上的单个文件绝对路径
    pub local_path: String,
    /// 远端目标绝对路径，文件名可以和 local_path 不同
    pub remote_path: String,
    #[serde(default = "default_copy_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CopyFromRemoteArgs {
    /// 会话 uid：`list_sessions` 返回的整数——**原样复制传入**，不要凭记忆猜、也不要自己编
    pub session_uid: u64,
    /// 远端绝对路径（仅单个文件；目录请逐文件拉取或用 tar/rsync）
    pub remote_path: String,
    /// 运行 ishell-mcp 的调用方机器上的目标绝对路径；所在目录不存在会自动创建
    pub local_path: String,
    #[serde(default = "default_copy_timeout_ms")]
    pub timeout_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CopyBetweenSessionsArgs {
    /// 源会话 uid（list_sessions 返回），文件从它的远端主机读出
    pub src_session_uid: u64,
    /// 源会话远端主机上的绝对路径（当前仅支持单个文件，目录会报错）
    pub src_remote_path: String,
    /// 目标会话 uid，文件写入它的远端主机
    pub dest_session_uid: u64,
    /// 目标会话远端主机上的绝对路径，可以和源文件名不同；所在目录不存在会自动创建
    pub dest_remote_path: String,
    #[serde(default = "default_copy_timeout_ms")]
    pub timeout_ms: u64,
}

#[tool_router]
impl IshellMcp {
    pub fn new() -> Self {
        Self
    }

    #[tool(
        description = "列出**你自己**（本 AI 进程）用 open_session 开的终端会话。用户自己打开的\
                        窗口、其它 AI 开的窗口**不会出现在这里**，你也不能读取或操作它们（硬规则，\
                        没有授权弹窗）。每个会话返回：uid（整数，后续所有工具的 session_uid 都直接\
                        复制它）、标题、主机、连接状态、远端工作目录（上报片段注入后才有值，刚连上\
                        的一小段时间可能为 null）、token_injected（配对 token 是否已注入该 shell；\
                        false = 在这里面再启动 AI 会拿不到配对身份，等 shell 空下来会自动补注），\
                        以及兼容字段 ai_owned/ai_owner/mine（列表里全是你的会话，mine 恒为 true）。\
                        同名会话凭 uid 和 host 区分。可传 filter 只返回标题/主机名匹配的会话，省上下文。\
                        列表为空或没有目标机器：list_saved_connections 查连接名 → open_session。"
    )]
    async fn list_sessions(
        &self,
        Parameters(ListSessionsArgs { filter }): Parameters<ListSessionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        match call(McpReqKind::ListSessions).await {
            Ok(McpReqResult::Sessions(mut list)) => {
                let had_filter = filter.as_ref().is_some_and(|f| !f.trim().is_empty());
                if let Some(f) = filter.map(|f| f.to_lowercase()).filter(|f| !f.is_empty()) {
                    list.retain(|s| {
                        s.title.to_lowercase().contains(&f)
                            || s.host.to_lowercase().contains(&f)
                    });
                }
                if list.is_empty() {
                    let note = if had_filter {
                        "没有标题或主机名匹配该 filter 的会话。list_sessions 只列出你自己用 \
                         open_session 开的会话；放宽或去掉 filter 再试，或 list_saved_connections \
                         → open_session 开新会话。"
                    } else {
                        "说明：列表为空是因为你只能看到自己用 open_session 开的会话；\
                         用户自己打开的窗口不会出现在这里（硬规则，没有授权弹窗）。\
                         要操作某台机器请 list_saved_connections → open_session。\
                         绑定的是启动时选定的 iShell 实例，不是用户当前正在看的那个窗口。"
                    };
                    return Ok(CallToolResult::success(vec![
                        ContentBlock::text("[]"),
                        ContentBlock::text(note.to_string()),
                    ]));
                }
                text_result(Ok(McpReqResult::Sessions(list)))
            }
            other => text_result(other),
        }
    }

    #[tool(
        description = "在指定终端会话里运行一条命令，等待其执行完成（或超时）后返回输出与退出码。\
                        命令和输出会实时显示在该会话对应的终端标签里（AI 自己开的会话通常在后台，\
                        不会抢走用户当前正在看的窗口），效果等同于用户亲自输入。\
                        会话还在连接/认证中时（open_session 刚返回就是这状态）**会自动等它连上，最\
                        多约 20 秒**——一般不用先 list_sessions 确认 connected 再发。另一个该知道的\
                        边界：本工具的等待可能被 MCP 客户端的空闲超时切断（客户端等不到响应会自行\
                        断开）——预计命令可能跑得很久的，改用 start_command 启动、poll_run 续等。\
                        重要限制：这是往一个真实交互 shell 里打字+回车，不是独立执行通道——\
                        前台如果正跑着 vim/top/REPL/sudo 密码提示等非 shell 程序，或者上一条命令\
                        有反斜杠续行、未闭合引号、heredoc 还没结束，这条命令文本会被当成那个\
                        程序/续行的输入吃掉，完成检测可能永远等不到，且可能改动那个程序里的\
                        数据。不确定当前前台状态时，先用 read_screen 看一眼再决定要不要发命令，\
                        或者改用 send_input 应对交互式场景。\
                        **command 必须是一条单行命令**：里面的换行等于按下 Enter，shell 会把它拆成\
                        多条依次执行，而完成检测只认得第一条——所以含换行（含 heredoc）或为空的 \
                        command 会被直接拒绝并附改写建议。要多步就用 `;` / `&&` 连接成一行，\
                        要跑长脚本就先 write_file 落一个文件再执行它。\
                        还有一类命令形态会连**完成哨兵**一起吞掉、让运行永远等不到结束：命令以 `#`\
                        注释结尾、引号未闭合、以反斜杠续行结尾——表现为反复超时、输出却在增长。\
                        这时调用 interrupt 释放，用 read_screen/read_history 核实，并把命令改写为\
                        完整单行重试——不要反复 poll_run 干等（每次都会烧满一个完整超时）。\
                        **交互式命令（cat/REPL/ssh）在 AI 专用会话里可以正常收尾**：那里的 shell \
                        装了完成上报（OSC 133），命令的开始/结束/退出码由 shell 自己发，不经过任何\
                        程序的 stdin。流程照旧：start_command 启动 → send_input 逐键交互（控制键写法\
                        见 send_input 描述）→ read_screen 观察 → 程序自己退出（如给 cat 发 EOF）即\
                        收到 finished=true 与退出码。\
                        没装上这个上报的会话（刚连上、上报片段还没注入时的头一条命令，或 fish/csh \
                        这类 shell）只能退回老办法（往终端多打一行完成哨兵），那里仍有这条限制：读 \
                        stdin 的程序会把哨兵当输入吃掉，运行永远 finished=false；此时用 interrupt 释放，\
                        再用 read_screen 确认提示符干净、无残留的 printf 'AI_DONE_…' 行，然后发新命令。\
                        （用户自己打开的窗口 AI 碰不到，不在此列。）\
                        两个解读输出时容易踩的坑：① output 末尾常带一段 shell 提示符残留（比如 \
                        `(venv) user@host:~$`，有时只剩一个 `$`）——这是刻意不做的清理（早期试过按\
                        「最后一行大概率是提示符」启发式剥掉，但 PS1 为空/不可见时会把真实输出误删，\
                        权衡后选择宁可留一点噪声也不丢数据），解析时自己按需忽略即可；② 超时返回的是\
                        finished=false 加**这一轮已产生的部分输出**（可能是空字符串）——空输出不代表\
                        命令什么都没打印，只代表还没等到完成哨兵，用 poll_run 续等或用 read_screen \
                        看实时内容。**命令最后一段是 exit [n] 时（`exit 42`、`make || exit 1`）会自动改写到子 \
                        shell 执行**：退出码照拿（exit 42 返回 42）、登录 shell 不受影响。exit 后面\
                        **还接着命令**会被拒绝——改写后的 exit 拦不住后续命令（`cond || exit 1; rm …` \
                        的守卫会失效），请改成条件分支 `cond && rm …`。logout 一律拒绝。想关掉 AI \
                        自己开的会话请用 close_session，不要靠 exit/logout。"
    )]
    async fn run_command(
        &self,
        Parameters(RunCommandArgs {
            session_uid,
            command,
            timeout_ms,
        }): Parameters<RunCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        wait_connected(session_uid, std::time::Duration::from_secs(20)).await;
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
        description = "启动一条可能很长的命令；命令开始运行后至多 100ms 返回（会话还在连接/\
                        认证中时会先等它连上，最多约 20 秒），不会因 MCP 客户端的长时间空闲\
                        限制而占住等待连接。返回 finished=false 时保存 run_id，之后用 poll_run\
                        以较短 timeout_ms 查询；命令已很快结束时会直接返回 finished=true。\
                        **预计运行时间可能超过 MCP 客户端空闲超时（几十秒到几分钟）的命令优先用\
                        本工具**：run_command 的等待会被客户端空闲超时切断，本工具不会。\
                        command 的要求与 run_command 完全相同：**必须是一条单行非空命令**（含换行\
                        会被拒绝，用 `;`/`&&` 连接或先 write_file 落脚本）。完成检测也是同一套：\
                        若命令形态把哨兵吞掉（以 # 注释结尾、引号未闭合、反斜杠续行结尾），运行\
                        同样永远等不到结束——按 run_command 描述里的处理办法 interrupt 释放、\
                        改写成完整单行重试。"
    )]
    async fn start_command(
        &self,
        Parameters(StartCommandArgs { session_uid, command }): Parameters<StartCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        wait_connected(session_uid, std::time::Duration::from_secs(20)).await;
        text_result(
            call(McpReqKind::RunCommand {
                session_uid,
                command,
                timeout_ms: 100,
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

    #[tool(
        description = "向指定终端发送 Ctrl+C，用于中断一个卡住或不需要的命令。同一会话同一时刻只允许\
                        一条挂起的运行，新 run_command 会被「已有一条 AI 命令正在执行」拒绝——这种情况下\
                        调用一次 interrupt 会立即清空这条挂起的运行（之后马上就能发新命令），但代价是\
                        那条被中断的命令彻底失去结果——执行到哪一步、是否已产生副作用都无法再确认，仅会\
                        拿到已知的部分输出作参考。（同一时刻只允许一个 poll_run 等待者的限制会在上一个\
                        等待者所在的连接断开——比如它自己的调用方超时放弃——之后自动解除，不需要靠 \
                        interrupt 才能恢复。）\n\
                        AI 专用会话里中断本身就有结论：shell 的完成上报会把 Ctrl-C 结束的命令报成 \
                        exit 130，不需要额外补救。没装上完成上报、走哨兵回退的会话才有这两个注意点：\
                        ① 被中断的程序若把终端设成 raw 模式（vim 这类全屏程序），Ctrl-C 不会冲刷输入\
                        队列——排队中的完成哨兵可能随后以一条自擦除的 printf 'AI_DONE_…' 泄漏为可见行：\
                        无害，但会混进后续命令的输出文本。② 读 stdin 的程序（cat/REPL）会把哨兵当输入\
                        吃掉、运行永远 finished=false——程序退出后调一次 interrupt 释放（在提示符上按 \
                        Ctrl-C 无害）。"
    )]
    async fn interrupt(
        &self,
        Parameters(SessionArgs { session_uid }): Parameters<SessionArgs>,
    ) -> Result<CallToolResult, McpError> {
        text_result(call(McpReqKind::Interrupt { session_uid }).await)
    }

    #[tool(
        description = "**你自己已经开着合适的会话时优先复用（见 list_sessions），别为同一台机器重复开新会话**\
                        ——新会话是全新登录，没有现成的 cwd/venv/登录态。确实需要新登录时：用一\
                        个已保存的连接（按名称）新开一个终端会话/标签，等价于用户在 iShell 侧栏里\
                        双击这条已保存连接。name 是已保存连接的名字，不是主机地址，也不是 \
                        list_sessions 里的会话标题——不确定具体拼写时先调 list_saved_connections \
                        核对。返回新会话的 uid；此时通常还没连上（connected=false，正在连接/\
                        认证中）——直接对它用任何会话工具即可：run_command/start_command 以及 \
                        read_file/write_file/copy_to_remote/copy_from_remote/copy_between_sessions \
                        都会自动等它连上（最多约 20 秒），不必先 list_sessions 确认 connected。\
                        只有 send_input/interrupt 不等（这两个遇到「会话尚未连接」等它变 true 再试）。"
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
                        read_screen 看效果。不会自动加回车。\n\
                        **默认 text 逐字节原样发送**：反斜杠就是反斜杠——写成 \"\\r\" 收到的就是\
                        反斜杠+r 两个字符、**不是回车**；往 REPL/vim 里敲带 \\n 的代码、密码、正则都\
                        不会被改动。**要发控制键（回车、Ctrl-D、Esc 等）必须设 escapes=true**：此时 \
                        text 里的字面转义才会被解析成真实按键——\\r 回车、\\n 换行、\\t、\\\\（字面\
                        反斜杠）与 \\xHH（两位十六进制、00-7F，如 \\x04=Ctrl-D、\\x1b=Esc），写错\
                        直接报错、不发送；真实控制字节则原样通过，两种写法都安全。例：提交一行 \
                        \"ls -l\\r\"、给 cat 发 EOF \"\\x04\"、vim 保存退出 \"\\x1b:wq\\r\"。注意 \
                        escapes=true 时正文里原本的反斜杠必须写成 \\\\，含代码/密码的长文本不要开\
                        它；混合场景分两次调用：先 escapes=true 发控制键，再默认模式发正文。"
    )]
    async fn send_input(
        &self,
        Parameters(SendInputArgs { session_uid, text, escapes }): Parameters<SendInputArgs>,
    ) -> Result<CallToolResult, McpError> {
        // 转义在代理侧解析、线协议不变：GUI 永远原样发送收到的 text，新旧版本任意组合行为一致。
        let text = if escapes {
            match mcp_protocol::unescape_key_text(&text) {
                Ok(t) => t,
                Err(e) => return text_result(Err(e)),
            }
        } else {
            text
        };
        text_result(call(McpReqKind::SendInput { session_uid, text }).await)
    }

    #[tool(
        description = "把文本内容写入远端指定路径（存在会被直接覆盖，不做外部改动冲突检测——\
                        这条通道只给你自己用，默认信任调用方）。复用 iShell 编辑器已有的 SFTP \
                        写入通路，用于代码同步、生成文件等场景，不需要再单独走一条 scp。content \
                        必须是合法 UTF-8 文本（走 JSON-RPC 传输）——二进制文件（.so/.tar/图片等）\
                        或较大的文件请用 copy_to_remote，不要尝试塞进这里。"
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
        wait_connected(session_uid, std::time::Duration::from_secs(20)).await;
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
        wait_connected(session_uid, std::time::Duration::from_secs(20)).await;
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

    #[tool(
        description = "把运行 ishell-mcp 的调用方机器上的单个文件流式复制到远端（走既有 SFTP \
                        上传，文件字节不进入 MCP JSON 或模型上下文）。这是跨主机同步大源码/\
                        二进制的首选，不要用 write_file。local_path 必须是调用方机器的绝对路径；\
                        remote_path 必须是远端绝对路径且不能含 \".\"/\"..\" 路径段。当前流式模式\
                        支持单个普通文件，目录请用 git/rsync 或逐文件上传；远端目标存在会直接覆盖。\
                        传多个文件时可以对同一会话并行发起多次本工具调用（会同时传输，无需等\
                        上一个传完），同一会话并发上限约 16，超了会返回「并发已达上限」让你稍后重试。"
    )]
    async fn copy_to_remote(
        &self,
        Parameters(CopyToRemoteArgs {
            session_uid,
            local_path,
            remote_path,
            timeout_ms,
        }): Parameters<CopyToRemoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        wait_connected(session_uid, std::time::Duration::from_secs(20)).await;
        text_result(
            with_ticket_retry(|| {
                copy_to_remote_from_caller(session_uid, local_path.clone(), remote_path.clone(), timeout_ms)
            })
            .await,
        )
    }

    #[tool(
        description = "把远端单个文件流式复制到运行 ishell-mcp 的调用方机器（走既有 SFTP 下载，\
                        文件字节不进入 MCP JSON 或模型上下文），是 copy_to_remote 的反方向。\
                        拉取大文件时用这个，而不是 read_file——read_file 会把全部内容内联进\
                        响应 JSON，大文件既浪费上下文又可能被截断。local_path 必须是调用方机器\
                        的绝对路径；remote_path 必须是远端绝对路径，且不能含 \".\"/\"..\" 路径段。\
                        当前流式模式仅支持单个文件，remote_path 是目录会报错——目录请多次调用\
                        本工具逐文件拉取，或用 run_command 执行 tar/rsync。本地目标存在会被\
                        直接覆盖，不做冲突检测；所在目录不存在会自动创建。拉多个文件时可以对同一\
                        会话并行发起多次本工具调用（会同时传输），同一会话并发上限约 16。"
    )]
    async fn copy_from_remote(
        &self,
        Parameters(CopyFromRemoteArgs {
            session_uid,
            remote_path,
            local_path,
            timeout_ms,
        }): Parameters<CopyFromRemoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        wait_connected(session_uid, std::time::Duration::from_secs(20)).await;
        text_result(
            with_ticket_retry(|| {
                copy_from_remote_to_caller(session_uid, remote_path.clone(), local_path.clone(), timeout_ms)
            })
            .await,
        )
    }

    #[tool(
        description = "把一个已打开远端会话（源）上的文件复制到另一个已打开远端会话（目标），\
                        两边都必须是已连接的远端主机——不是本地文件，本地文件请用 copy_to_remote/\
                        copy_from_remote。会优先尝试源主机直连目标主机（不经过运行 iShell 的\
                        机器中转，适合两台主机在同一局域网/集群、直连明显更快的场景）：iShell 会\
                        生成一个仅限本次使用的一次性密钥对，临时授权源主机免密连接目标主机，\
                        传输完成后立即撤销这个临时授权、删除临时密钥，不留长期可用的免密信任；\
                        因为是程序化操作、没有人工确认目标主机指纹的环节，直连时主机密钥策略是\
                        accept-new。如果直连不可行（网络不通、权限受限等），会自动降级为经 \
                        iShell 进程内存中转（不落盘任何一方磁盘）；调用方不需要关心具体走了哪条\
                        路径，返回结果里的 method 字段会标明（\"direct\" 或 \"relay\"）。当前仅\
                        支持单个文件，src_remote_path 是目录会报错——目录请多次调用本工具逐文件\
                        复制，或用 run_command 执行 rsync/scp。src_remote_path/dest_remote_path \
                        都必须是各自远端主机上的绝对路径，且不能含 \".\"/\"..\" 路径段；目标已\
                        存在会被直接覆盖；源和目标不能是同一个会话（同会话内复制请用 \
                        run_command 执行 cp）。"
    )]
    async fn copy_between_sessions(
        &self,
        Parameters(CopyBetweenSessionsArgs {
            src_session_uid,
            src_remote_path,
            dest_session_uid,
            dest_remote_path,
            timeout_ms,
        }): Parameters<CopyBetweenSessionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        wait_connected(src_session_uid, std::time::Duration::from_secs(20)).await;
        wait_connected(dest_session_uid, std::time::Duration::from_secs(20)).await;
        text_result(
            call(McpReqKind::CopyBetweenSessions {
                src_session_uid,
                src_remote_path,
                dest_session_uid,
                dest_remote_path,
                timeout_ms,
            })
            .await,
        )
    }
}

#[tool_handler(
    instructions = "这是 iShell——一个由用户盯着运行的真实终端管理器——的 MCP 桥。你操作的是**真\
                    实的交互式终端**（能看到你在打字，前台程序、提示符、sudo 都会受影响），不是\
                    无状态的执行沙箱。用户能看见你做的每件事。**你只能读写自己用 open_session \
                    开的会话**：用户自己打开的窗口、其它 AI 的窗口对你不可见，读和写都会被直接拒绝\
                    （硬规则，没有授权弹窗，重试也没用）。\n\
                    概念：已保存连接（list_saved_connections）是凭据模板，用来 open_session 发起\
                    全新登录；会话（list_sessions）是**你自己**已经开着的终端标签。用户说「在 e5 上\
                    跑一下」：先 list_sessions 看你自己有没有连着 e5 的会话，有就复用（保留你之前的\
                    cwd/环境，也不多占连接）；没有就 list_saved_connections 核对连接名 → \
                    open_session。即使用户说「用我那个窗口」，你也碰不到它——开你自己的会话去做，\
                    并告诉用户原因。\n\
                    定位会话：报「会话不存在」时别猜新 uid——错误里自带你当前的会话列表（uid:标题），\
                    按它重新匹配。uid 在同一次 iShell 运行内稳定、断线重连不变；iShell 重启后重新\
                    分配，历史上下文里的旧 uid 可能已失效——所以一律从最新的 list_sessions 原样复制。\n\
                    会话状态与报错：connected=false = 还在连接/认证中或已断线（重连中）；run_command \
                    /start_command 会自动等它连上（最多约 20 秒），send_input/interrupt 不会等——\
                    后两个遇到「会话尚未连接」就等它变 true。一个会话同时只能有一条 AI 命令在跑，再发\
                    会报「已有一条正在执行」——用 poll_run 续等或 interrupt 释放。运行中途断线：进\
                    行中的 run_command/poll_run 会收到「运行已失效、无法再 poll_run」加已知部分输\
                    出，按它判断执行到哪一步，不要整条重发。每条错误文案都写明了类别和下一步（重试\
                    /换会话/找用户），按文案行事。\n\
                    标准流程：定位会话（见上）→ run_command 执行 → 超时用 poll_run 续等（省略 \
                    run_id，不重发）→ 交互场景（sudo/vim/REPL）用 send_input，看屏用 read_screen → \
                    自己开的会话用完 close_session（只能关自己开的；开一堆不关会占着连接和标签）。\n\
                    失败恢复：只读调用（list_sessions/read_screen/read_history/list_*）结果丢失或\
                    超时就直接重调，幂等安全。写调用结果丢失：先 poll_run 确认有没有在跑；报\
                    「run_id 不存在或已结束」而你不确定命令执行没有，用 read_screen/read_history \
                    核实再决定，勿盲目重试有副作用的命令。看到「命令可能已执行、结果未知」先核实\
                    屏幕。报「已经有一个 poll_run 在等待」说明旧等待者还挂着：别并发 poll，旧等待\
                    者不要了就 interrupt 释放。报「未检测到 iShell」或绑定/握手被拒：按错误文案排查，\
                    常见原因不只是缺 token——① 启动你的那个终端没拿到配对 token（请用户在自己的 \
                    iShell 终端里启动 AI，或右键「立即注入配对标识」后重启 AI；tmux/screen 继承的是\
                    会话**之前**的环境）；② iShell 刚重启过（进程换了，重新发起 MCP 连接）；③ 配对 \
                    token 来自另一台机器（错误会写明主机不一致，从 MCP 配置删掉别人的 token）；④ \
                    SSH 反向转发断了，或设置里关掉了 AI 控制。不要把绑定失败一律当成缺 token。\n\
                    定位目录与环境：先 list_sessions 的 cwd 字段 → 再看 read_screen 里提示符显示的\
                    路径 → 还定不了就 run_command 跑 `pwd; ls; git rev-parse --show-toplevel \
                    2>/dev/null` 这类只读探测；用户没告诉你路径就问他，别猜。\n\
                    上下文管理：你的上下文是宝贵的，终端输出不是。\n\
                    · read_screen 只看一屏；read_history 从 max_lines 小的值开始（默认 200），\
                    别一次性吞整个回滚。\n\
                    · 大文件/二进制一律走 copy_to_remote/copy_from_remote（字节不进你的上下文）；\
                    read_file 用于真正需要读内容的文本；grep/sed/head 等就地过滤优先于拉全量。\n\
                    · run_command 的输出会原样进你的上下文：能定向到文件的别打印，能 tail -1 的\
                    别 cat。\n\
                    可靠性：run_command 是往交互 shell 打字+回车——前台全屏程序/REPL/未闭合语法时\
                    完成检测可能挂起，发命令前先 read_screen；中断用 interrupt；timeout_ms 最长 24h，\
                    长任务直接等完，勿 sleep 轮询；`&` 优先级低于 `&&`，勿把前置步骤一并丢进后台。远\
                    端命令/文件优先走本工具集，不要另开 `ssh host cmd`（会丢 cwd/环境/历史，用户也看\
                    不见）。\n\
                    典型任务：\n\
                    · 在 e5 上跑测试：list_sessions 看你自己有没有连着 e5 的会话；没有就 \
                    open_session 开一个 → run_command 先 cd 到仓库（路径不确定就问用户）再 \
                    ./run_tests.sh → 超时 poll_run 续等 → read_screen 看结果。\n\
                    · 机器还没有打开会话：list_saved_connections 核对名字 → open_session → \
                    直接 run_command `pwd; ls`（会自动等连上）探测环境 → 干活 → close_session。\n\
                    · 长构建/长测试：run_command timeout_ms 给足直接等；MCP 客户端自己超时断了就 \
                    poll_run（省略 run_id）续等同一条运行，绝不重发命令。\n\
                    · 终端卡着交互程序（vim/top/sudo 密码提示）：先 read_screen 看前台是什么 → \
                    send_input 逐键应对（top 用 \"q\"；回车/EOF/Esc 等控制键写法见 send_input 描述，\
                    如 escapes=true 时 vim 退出 \"\\x1b:q!\\r\"）→ sudo 密码提示符让用户自己输，或征得同意后 \
                    send_input。没装上完成上报、走哨兵回退的会话里，交互程序会把完成哨兵当输入吃掉（程序\
                    退出后运行仍 finished=false，调一次 interrupt 释放再开新命令）；装好上报的 AI 专用\
                    会话不受此限。"
)]
impl ServerHandler for IshellMcp {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `--version`：打印 crate 版本与 MCP 线协议版本。安装脚本与（后续）GUI 自动部署都靠它
    // 比对「已部署的代理」与「当前 iShell」是否配套——协议版本才是决定线格式兼容性的关键。
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!(
            "ishell-mcp {} proto {}",
            env!("CARGO_PKG_VERSION"),
            mcp_protocol::MCP_PROTOCOL_VERSION
        );
        return Ok(());
    }
    let service = IshellMcp::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// 本代理的身份标识（`actor`）。iShell 用它落实「窗口归开它的那个 AI 专用」：`open_session`
/// 记录它，此后只有同一个 actor 能读写那个会话（用户的、别的 AI 的窗口一律拒绝）。v6 起 iShell
/// 在配对握手时记下这个值并绑到连接凭据上，业务请求里自报的 actor 不再被信任。
///
/// **它标识的是 AI 客户端，不是代理进程。** 代理是 AI 客户端（Claude Code 等）的子进程，而
/// 客户端会在 `/mcp` 重连、MCP server 崩溃重启时换一个新的代理进程，自己却没变。此前 actor 是
/// 代理启动时的随机数，一重连就换——AI 刚开的会话立刻变成「不是你的」：看不见、读不了、关不掉，
/// 里面在跑的命令结果也拿不回来。所以在 Linux 上从**父进程**（即 AI 客户端）派生：本机开机
/// 标识 + 父进程 pid + 父进程启动时刻，三者合起来在一台机器的一次开机内唯一、跨机器不撞，同一个
/// 客户端换多少次代理都得到同一个值。
///
/// 客户端**整个重启**后是新进程，actor 随之改变，旧会话不再归它——这是刻意的：放宽「接管」
/// 规则就会回到「谁都能碰别人的窗口」。取不到父进程信息（非 Linux、/proc 不可读）时退回
/// 进程级随机值，行为同旧版。
///
/// 不是保密凭据：同一配对 token 下的代理本来就能声明任意 actor，身份隔离靠的是 iShell 签发、
/// 绑定 actor 的连接凭据。
fn current_actor() -> String {
    static ACTOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ACTOR
        .get_or_init(|| {
            client_actor()
                .or_else(|| mcp_protocol::random_hex(8))
                // 熵源失败（实际不会发生）：退回 pid——只是进程区分，允许弱。
                .unwrap_or_else(|| format!("fallback-pid{}", std::process::id()))
        })
        .clone()
}

/// 从父进程（AI 客户端）派生 actor：`<开机标识前 16 位>-<父 pid>-<父进程启动时刻>`。
/// 任何一项取不到都返回 `None`，由调用方退回随机值。
fn client_actor() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let ppid = std::os::unix::process::parent_id();
        if ppid <= 1 {
            return None; // 父进程已退出、被 init 收养：没有可跟随的客户端
        }
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{ppid}/stat")).ok()?;
        actor_from_parts(&boot, ppid, proc_start_time(&stat)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 拼出 actor。开机标识去掉连字符取前 16 位十六进制（64 位，足以区分不同机器/不同次开机）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn actor_from_parts(boot_id: &str, ppid: u32, start_ticks: u64) -> Option<String> {
    let boot: String = boot_id.chars().filter(|c| c.is_ascii_hexdigit()).take(16).collect();
    (boot.len() == 16).then(|| format!("{boot}-{ppid}-{start_ticks}"))
}

/// 从 `/proc/<pid>/stat` 取进程启动时刻（第 22 个字段，开机以来的时钟滴答数）。
///
/// 第 2 个字段是括号包着的进程名，里面可以有空格和括号（进程名可被任意设置），所以必须从
/// **最后一个** `)` 之后开始数：那之后第一个字段是第 3 个（状态），第 22 个即下标 19。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// 绑定弹窗里给被询问用户看的来源描述：哪个 unix 用户、哪个进程在发起。纯展示信息，
/// 不构成身份凭证——真正决定绑定的是用户在弹窗里的点击，或配对 token 握手。
///
/// AI 可用环境变量 `ISHELL_MCP_ORIGIN` 自报更好认的名字（如 `Codex CLI (e5-1)`）；
/// 没设就用 `USER (ishell-mcp pid N)`——共享服务器上，被弹窗打扰的用户凭 user/pid
/// 一眼判断是不是自己的 AI（`ps` 可查）。空白与控制字符会被压平/截断（上限 80 字符），
/// 它毕竟是要上弹窗的一行字。
fn caller_origin() -> String {
    if let Some(custom) = std::env::var_os("ISHELL_MCP_ORIGIN") {
        let flattened: String = custom
            .to_string_lossy()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let trimmed: String = flattened.chars().take(80).collect();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown-user".into());
    format!("{user} (ishell-mcp pid {})", std::process::id())
}

#[cfg(all(test, unix))]
mod tests {
    use super::caller_origin;
    use super::check_proto_version;
    use super::current_actor;
    use super::mcp_protocol::MCP_PROTOCOL_VERSION;

    #[test]
    fn accepts_matching_version() {
        assert!(check_proto_version(MCP_PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn rejects_mismatch_and_legacy_zero() {
        assert!(check_proto_version(MCP_PROTOCOL_VERSION + 1).is_err());
        // 旧版 iShell 不带版本字段 → serde 默认收到 0 → 必须判为不一致。
        assert!(check_proto_version(0).is_err());
    }

    /// 来源描述：优先用 AI 自报的名字（空白/控制字符压平、截断），没有才退回 user+pid。
    /// 两个场景放进同一条测试：它们共用同一个环境变量，并行跑会互相踩。
    #[test]
    fn caller_origin_prefers_env_override_then_falls_back() {
        let old = std::env::var_os("ISHELL_MCP_ORIGIN");
        std::env::remove_var("ISHELL_MCP_ORIGIN");
        let fallback = caller_origin();
        assert!(
            fallback.contains("(ishell-mcp pid"),
            "退回形态应含进程标识：{fallback}"
        );

        std::env::set_var("ISHELL_MCP_ORIGIN", "  Codex\nCLI\t(e5-1)  ");
        assert_eq!(caller_origin(), "Codex CLI (e5-1)");

        std::env::set_var("ISHELL_MCP_ORIGIN", "   \n\t  ");
        assert!(
            caller_origin().contains("(ishell-mcp pid"),
            "空白覆盖值应视为未设置：{}",
            caller_origin()
        );

        match old {
            Some(v) => std::env::set_var("ISHELL_MCP_ORIGIN", v),
            None => std::env::remove_var("ISHELL_MCP_ORIGIN"),
        }
    }

    /// 进程标识：16 位十六进制，同进程内多次取必须一致（OnceLock 缓存）。
    /// **多机串台的根因回归门禁**（2026-09-18 生产服务器实测）：共用账号时 `~/.claude.json`
    /// 的 user 级 MCP 配置里写死了某人的 token，AI 客户端 spawn 代理时用它**覆盖**了终端注入
    /// 的同名变量——所有人的代理都带着同一个 token，全部路由到那一台电脑。终端注入的专用变量
    /// （配置里没人写它）必须赢过配置。
    /// 反向对照：把 `resolve_pairing_token` 改成优先 `config`，第一条断言当场挂。
    #[test]
    fn the_launching_terminal_beats_a_token_hardcoded_in_shared_mcp_config() {
        use super::resolve_pairing_token;
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            resolve_pairing_token(s("mine"), s("someone-elses")),
            (s("mine"), true, false),
            "配置里写死的别人的 token 不能覆盖终端注入的 token"
        );
        assert_eq!(
            resolve_pairing_token(s("mine"), s("mine")),
            (s("mine"), false, false)
        );
        // 终端没注入（AI 在 IDE 里启动等）：只能用配置。第三位 true = 配置兜底，必须告警。
        assert_eq!(resolve_pairing_token(None, s("cfg")), (s("cfg"), false, true));
        // 空白等于没设。
        assert_eq!(
            resolve_pairing_token(s("  "), s("cfg")),
            (s("cfg"), false, true)
        );
        assert_eq!(resolve_pairing_token(None, None), (None, false, false));
    }

    /// actor 跟着 AI 客户端（父进程）走：同一个客户端换代理进程（`/mcp` 重连）得到同一个值，
    /// 否则它刚开的会话立刻变成「不是你的」。这里钉住派生规则本身：同输入同输出、任一要素
    /// 不同即不同。反向对照：把 `actor_from_parts` 改成混入随机数，第一条断言挂。
    #[test]
    fn actor_is_derived_from_the_client_not_the_proxy_process() {
        use super::actor_from_parts;
        let boot = "3f2a9c1e-7b4d-4e11-9a0b-5c6d7e8f9012\n";
        let a = actor_from_parts(boot, 4242, 987654).expect("完整输入应能派生");
        assert_eq!(Some(a.clone()), actor_from_parts(boot, 4242, 987654), "同一个客户端 → 同一个 actor");
        assert_eq!(a, "3f2a9c1e7b4d4e11-4242-987654");
        assert_ne!(Some(a.clone()), actor_from_parts(boot, 4243, 987654), "不同父进程");
        assert_ne!(Some(a.clone()), actor_from_parts(boot, 4242, 987655), "pid 被复用给了新进程");
        assert_ne!(
            Some(a),
            actor_from_parts("0000000000000000", 4242, 987654),
            "另一台机器/另一次开机"
        );
        assert_eq!(actor_from_parts("short", 1, 1), None, "开机标识不完整就不派生");
    }

    /// `/proc/<pid>/stat` 的进程名在括号里、可以含空格和括号——必须从最后一个 `)` 往后数。
    #[test]
    fn proc_start_time_survives_hostile_process_names() {
        use super::proc_start_time;
        let tail = "S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555000 19 20";
        assert_eq!(proc_start_time(&format!("123 (claude) {tail}")), Some(555000));
        assert_eq!(proc_start_time(&format!("123 (a) b (c d) {tail}")), Some(555000));
        assert_eq!(proc_start_time("garbage"), None);
    }

    #[test]
    fn current_actor_is_stable_within_a_process() {
        let a = current_actor();
        assert_eq!(a, current_actor(), "同进程内 actor 必须稳定");
        assert!(!a.is_empty());
    }

    fn paired(id: &str, host: &str, path: &str) -> (super::Probe, std::path::PathBuf) {
        (
            super::Probe::Paired {
                id: id.into(),
                ver: super::mcp_protocol::MCP_PROTOCOL_VERSION,
                ticket: "t".into(),
                host: host.into(),
            },
            std::path::PathBuf::from(path),
        )
    }

    fn answered(id: &str, path: &str) -> (super::Probe, std::path::PathBuf) {
        (
            super::Probe::Answered {
                id: id.into(),
                ver: super::mcp_protocol::MCP_PROTOCOL_VERSION,
            },
            std::path::PathBuf::from(path),
        )
    }

    #[test]
    fn rediscover_keeps_the_original_instance_when_it_is_still_there() {
        use super::{classify_rediscover, RediscoverOutcome};
        let probes = vec![paired("old", "box", "/tmp/a.sock"), paired("other", "box", "/tmp/b.sock")];
        match classify_rediscover("old", &probes) {
            RediscoverOutcome::Same { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/a.sock"))
            }
            other => panic!("应沿用旧实例，实际 {other:?}"),
        }
    }

    #[test]
    fn rediscover_rebinds_when_the_only_paired_instance_has_a_new_id() {
        use super::{classify_rediscover, RediscoverOutcome};
        let probes = vec![paired("new", "box", "/tmp/n.sock")];
        match classify_rediscover("old", &probes) {
            RediscoverOutcome::Rebound { new_id, host, .. } => {
                assert_eq!(new_id, "new");
                assert_eq!(host, "box");
            }
            other => panic!("唯一匹配者应改绑，实际 {other:?}"),
        }
    }

    #[test]
    fn rediscover_does_not_rebind_when_several_other_instances_remain() {
        use super::{classify_rediscover, RediscoverOutcome};
        let probes = vec![
            paired("a", "box", "/tmp/a.sock"),
            paired("b", "box", "/tmp/b.sock"),
        ];
        match classify_rediscover("old", &probes) {
            RediscoverOutcome::Lost { paired, answered } => {
                assert_eq!(paired.len(), 2);
                assert_eq!(answered, 0);
            }
            other => panic!("多开必须拒绝改绑，实际 {other:?}"),
        }
    }

    #[test]
    fn rediscover_lost_distinguishes_token_mismatch_from_nobody_home() {
        use super::{bind_none_matched_msg, classify_rediscover, format_lost, RediscoverOutcome};
        let probes = vec![answered("x", "/tmp/x.sock")];
        match classify_rediscover("old", &probes) {
            RediscoverOutcome::Lost { paired, answered } => {
                assert!(paired.is_empty());
                assert_eq!(answered, 1);
                let msg = format_lost("old", &paired, answered);
                assert!(msg.contains("token"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
        let none = bind_none_matched_msg(0);
        assert!(none.contains("SSH") && none.contains("仍在运行"), "{none}");
        let mismatch = bind_none_matched_msg(2);
        assert!(mismatch.contains("token"), "{mismatch}");
    }

    #[test]
    fn only_reverse_forward_sockets_are_treated_as_orphans() {
        use super::is_reverse_forward_sock;
        assert!(is_reverse_forward_sock(std::path::Path::new(
            "/home/u/.ishell-mcp/mcp-123.sock"
        )));
        assert!(!is_reverse_forward_sock(std::path::Path::new(
            "/home/u/.config/ishell/mcp-123.sock"
        )));
        assert!(!is_reverse_forward_sock(std::path::Path::new(
            "/tmp/mcp-123.sock"
        )));
    }

    fn found_inst(id: &str, host: &str) -> (String, u32, std::path::PathBuf, String, String) {
        (
            id.into(),
            super::mcp_protocol::MCP_PROTOCOL_VERSION,
            std::path::PathBuf::from(format!("/tmp/{id}.sock")),
            "t".into(),
            host.into(),
        )
    }

    /// 一对多：带 ISHELL_HOST 时必须在发 Bind 弹窗之前丢掉别人的电脑。
    /// 反向对照：把 keep_instances_for_launch_host 改成原样返回，第一条断言挂。
    #[test]
    fn host_filter_drops_other_machines_before_bind_popup() {
        use super::keep_instances_for_launch_host;
        let kept = keep_instances_for_launch_host(
            vec![found_inst("a", "other"), found_inst("b", "mine")],
            Some("mine"),
        )
        .unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "b");
    }

    #[test]
    fn host_filter_errors_when_all_instances_are_elsewhere() {
        use super::keep_instances_for_launch_host;
        let err = keep_instances_for_launch_host(vec![found_inst("a", "other")], Some("mine"))
            .unwrap_err();
        assert!(err.contains("other") && err.contains("mine"), "{err}");
    }

    #[test]
    fn host_filter_keeps_unknown_host_for_old_gui() {
        use super::keep_instances_for_launch_host;
        let kept = keep_instances_for_launch_host(
            vec![
                found_inst("a", "unknown-host"),
                found_inst("b", ""),
                found_inst("c", "other"),
            ],
            Some("mine"),
        )
        .unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].0, "a");
        assert_eq!(kept[1].0, "b");
    }

    #[test]
    fn host_filter_noop_without_launch_host() {
        use super::keep_instances_for_launch_host;
        let kept = keep_instances_for_launch_host(vec![found_inst("a", "other")], None).unwrap();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn host_filter_leaves_empty_list_for_token_mismatch() {
        use super::keep_instances_for_launch_host;
        let kept = keep_instances_for_launch_host(Vec::new(), Some("mine")).unwrap();
        assert!(kept.is_empty());
    }

    #[test]
    fn host_filter_accepts_short_name_vs_fqdn() {
        use super::keep_instances_for_launch_host;
        let kept =
            keep_instances_for_launch_host(vec![found_inst("a", "box.lan")], Some("box")).unwrap();
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn caller_path_rejects_root_like_gui_local_path() {
        use super::validate_caller_path;
        assert!(validate_caller_path("/", "local_path").is_err());
        assert!(validate_caller_path("/tmp/notes.txt", "local_path").is_ok());
        assert!(validate_caller_path("/tmp/./notes.txt", "local_path").is_err());
        assert!(validate_caller_path("notes.txt", "local_path").is_err());
    }

    /// 改绑窄角：没注入 ISHELL_HOST 时，仍用当初绑定记下的主机挡住换到别人电脑。
    /// 反向对照：把 rebind_host_ok 改成一律 Ok，第三条断言挂。
    #[test]
    fn rebind_refuses_a_unique_match_on_another_host() {
        use super::rebind_host_ok;
        assert!(rebind_host_ok(Some("box"), "box").is_ok());
        assert!(rebind_host_ok(Some("box"), "box.lan").is_ok(), "短名 vs FQDN 算同一台");
        assert!(
            rebind_host_ok(Some("box"), "other").is_err(),
            "原实例下线后唯一匹配者在另一台机器，不能静默改绑"
        );
        assert!(rebind_host_ok(None, "other").is_ok(), "没记下旧主机时无法对照");
        assert!(rebind_host_ok(Some("unknown-host"), "other").is_ok(), "旧 GUI 无法对照");
        assert!(rebind_host_ok(Some("box"), "unknown-host").is_ok());
        assert!(rebind_host_ok(Some(""), "other").is_ok());
    }
}
