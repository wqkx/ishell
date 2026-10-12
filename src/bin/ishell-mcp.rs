//! iShell 的 AI/MCP 控制通道代理：Claude Code 等 AI 客户端按 stdio 方式 spawn 的独立小进程。
//! 只是把工具调用转发到**启动它的那个终端所属的 iShell**（经 Unix domain socket，一次连接一问
//! 一答），iShell 再落到它已经持有的 SSH 会话上执行。
//!
//! 连哪台 iShell 只由环境里的 `ISHELL_MCP_ENDPOINT` + `ISHELL_MCP_KEY` 决定（iShell 在自己的终端里
//! 注入）。**没有发现、没有候选、没有降级**：缺环境就拒绝，握手不过就失败，iShell 重启后旧
//! agent 的密钥作废、只能重开。见下面「连接层」一节与 `src/mcp_protocol.rs` 的「认证层」。
//! 与主二进制共享同一份线协议类型（见 src/mcp_protocol.rs），这里用 #[path] 直接纳入，
//! 避免为共享几个 struct 拆出独立的 lib crate。

// 这一份是「代理侧」的编译产物：同一个文件也被主二进制编一遍，两边用到的子集不同——
// 会话归属门禁（`session_target_uids`）和认证握手的服务端一半都只在 GUI 那侧执行，
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

// ───────────────────────── 连接层：环境给什么就连什么 ─────────────────────────
//
// 这个代理**只**操作启动它的那个终端所属的 iShell，依据只有环境里的两个变量：
//   `ISHELL_MCP_ENDPOINT`  这条 SSH 连接专属的 socket 路径；
//   `ISHELL_MCP_KEY`       那台 iShell 本次运行的随机密钥。
// 二者由 iShell 在自己的终端里注入（见 `session::mcp_env_export_cmd`），AI 客户端启动代理时
// 原样继承。**这里没有任何兜底**：
//   · 不列目录、不探测、没有候选，所以不存在「选哪个」；
//   · 缺任何一个变量就拒绝，不退到别的变量名、别的路径、别的办法；
//   · 握手失败就是失败，不重试别的端点；iShell 重启后旧 agent 的密钥作废，只能重开；
//   · 没有「改绑」：端点连不上就是连不上。
// 线协议见 `src/mcp_protocol.rs` 的「认证层」一节。

/// 终端注入的连接信息。
#[cfg(unix)]
#[derive(Debug, Clone)]
struct Launch {
    endpoint: std::path::PathBuf,
    key: String,
}

/// 把两个环境变量的原始值判成 [`Launch`]。纯函数，便于单测。
///
/// 空白值视同没设；端点必须是绝对路径（注入的永远是绝对路径，相对路径只可能是手写错了，
/// 而相对路径的解析取决于代理的当前目录——那就成了「碰巧连到哪个算哪个」）。
/// 失败返回**缺失/非法的变量名**，由调用方拼成给人看的文字。
#[cfg(unix)]
fn launch_from(endpoint: Option<String>, key: Option<String>) -> Result<Launch, Vec<String>> {
    let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (endpoint, key) = (clean(endpoint), clean(key));
    let mut bad = Vec::new();
    match &endpoint {
        None => bad.push(mcp_protocol::ENV_ENDPOINT.to_string()),
        Some(e) if !e.starts_with('/') => {
            bad.push(format!("{}（不是绝对路径：{e}）", mcp_protocol::ENV_ENDPOINT))
        }
        Some(_) => {}
    }
    if key.is_none() {
        bad.push(mcp_protocol::ENV_KEY.to_string());
    }
    match (endpoint, key) {
        (Some(e), Some(k)) if bad.is_empty() => Ok(Launch {
            endpoint: std::path::PathBuf::from(e),
            key: k,
        }),
        _ => Err(bad),
    }
}

/// 缺环境时给 agent 和用户看的话。除了说清规则，还报出**这个代理是被谁拉起来的**、环境里实际
/// 看得到哪些 `ISHELL_*` 变量——「环境变量明明刚才还在、过一会儿代理里就没了」这类问题，光看
/// 「缺变量」无从判断是被 agent 过滤了环境、还是代理被换了个环境重新拉起，这两行就是用来分辨的。
#[cfg(unix)]
fn missing_env_message(bad: &[String]) -> String {
    let (ppid, parent) = parent_process_summary();
    let seen: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("ISHELL_"))
        .collect();
    format!(
        "这个 ishell-mcp 没有拿到连接 iShell 所需的环境（缺少或无效：{}）。它只在**从某台 iShell \
         的终端里启动的 AI** 里可用：iShell 连上服务器后会往终端注入连接环境，在那个 shell 里启动的 \
         AI 才连得上**那一台** iShell；不是从 iShell 终端启动的（本机直接启动、IDE、别人的终端），\
         一律拒绝，也不会去连别的 iShell。请停止重试，并告诉用户：在他自己的 iShell 终端里（重新）\
         启动 AI；若那个终端是连上后敲过键盘而被跳过注入的，先右键「立即注入 MCP 连接环境」再启动。\n\
         诊断：本代理的父进程 pid={ppid} 进程名={parent}；环境里看得到的 ISHELL_* 变量：{}。\
         若这些变量在 AI 启动时还在、后来才没了，多半是 AI 客户端用一份被过滤的环境重新拉起了代理；\
         Codex 默认只给 MCP 子进程一份白名单环境，需要在 ~/.codex/config.toml 的 [mcp_servers.ishell] \
         里加 env_vars = [\"ISHELL_MCP_ENDPOINT\", \"ISHELL_MCP_KEY\"]。",
        bad.join("、"),
        if seen.is_empty() {
            "（无）".to_string()
        } else {
            seen.join("、")
        }
    )
}

/// 父进程（拉起代理的那个 AI 客户端）的 `(pid, 进程名)`。取不到就给占位——诊断用，不影响逻辑。
///
/// 只取**进程名**（`/proc/<pid>/comm`）和可执行文件名，**不取命令行**：命令行里常带着提示词、
/// 路径、令牌之类的参数，而这段话会发给 LLM、还会写进诊断文件。
#[cfg(unix)]
fn parent_process_summary() -> (u32, String) {
    let ppid = std::os::unix::process::parent_id();
    let comm = std::fs::read_to_string(format!("/proc/{ppid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let exe = std::fs::read_link(format!("/proc/{ppid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let name = match (comm, exe) {
        (Some(c), Some(e)) if c != e => format!("{c}（可执行文件 {e}）"),
        (Some(c), _) => c,
        (None, Some(e)) => e,
        (None, None) => "（读不到）".into(),
    };
    (ppid, name)
}

/// 缺环境的诊断也落一份本地文件（尽力而为）：agent 的报错会被它自己的界面折叠、截断，事后
/// 想查「它到底是怎么被拉起的」时有处可看。只存缺环境这一类事件，超过 64KB 就从头重写。
#[cfg(unix)]
fn write_diag(line: &str) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = std::path::PathBuf::from(home).join(".ishell-mcp");
    // 目录、文件都只给本人读写（iShell 反向转发建这个目录时也是 0700）
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
    let path = dir.join("diag.log");
    let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > 64 * 1024);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(!too_big)
        .truncate(too_big)
        .mode(0o600)
        .open(&path);
    if let Ok(mut f) = file {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] pid={} {line}", std::process::id());
    }
}

/// 本进程的连接信息，读一次就定：环境在进程生命周期内不会变，每次调用重读只会多出
/// 「刚才还行、现在不行」的错觉。
#[cfg(unix)]
fn launch() -> Result<&'static Launch, String> {
    static LAUNCH: std::sync::OnceLock<Result<Launch, String>> = std::sync::OnceLock::new();
    LAUNCH
        .get_or_init(|| {
            launch_from(
                std::env::var(mcp_protocol::ENV_ENDPOINT).ok(),
                std::env::var(mcp_protocol::ENV_KEY).ok(),
            )
            .map_err(|bad| {
                let msg = missing_env_message(&bad);
                write_diag(&msg.replace('\n', " | "));
                msg
            })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// 带超时的 connect。同机 Unix socket 上 connect 基本瞬时，这个护栏平时轮不到触发；但反向
/// 转发的 socket 背后是一条 SSH 通道，对端成了网络黑洞时 connect 会一直挂着，谁都不该无限等。
#[cfg(unix)]
async fn connect_endpoint(l: &Launch) -> Result<UnixStream, String> {
    match tokio::time::timeout(CONNECT_WRITE_TIMEOUT, UnixStream::connect(&l.endpoint)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(format!(
            "连不上 iShell 端点 {}（{e}）。可能原因：iShell 已退出或重启（旧 agent 的密钥随之作废）、\
             它到这台服务器的 SSH 断了或重连过（端点每次连接都换）、设置里关掉了 AI 控制。\
             不会去连别的 iShell。请在 iShell 终端里重新启动 AI。",
            l.endpoint.display()
        )),
        Err(_) => Err(format!(
            "连接 iShell 端点 {} 超时（对端 SSH 连接可能已成黑洞）。请检查 iShell 到这台服务器的 \
             SSH 是否还活着，必要时在 iShell 终端里重新启动 AI。",
            l.endpoint.display()
        )),
    }
}

/// 握手成功后得到的东西。
#[cfg(unix)]
#[derive(Debug, Clone)]
struct Granted {
    ticket: String,
    peer: mcp_protocol::Peer,
}

/// 本进程的连接凭据：握手一次、之后每条请求都带。**只有「还没握手」和「握手成功」两种状态**；
/// 握手失败不缓存（下次调用重新握手同一个端点、同一个密钥），但绝不换端点、换密钥。
#[cfg(unix)]
static GRANTED: tokio::sync::Mutex<Option<Granted>> = tokio::sync::Mutex::const_new(None);

/// 在一条**新连接**上跑完认证握手（理由与线格式见 `mcp_protocol` 的「认证层」）。
///
/// 失败一律是 `Err(给人看的话)`，每种失败说的是**不同的事**：对端不应答（密钥不符 / 不是这台
/// iShell）、对端证明不了自己（不是你的 iShell）、版本对不上（要重新部署）——合并成一句
/// 「握手失败」就没法照着去处理。
#[cfg(unix)]
async fn handshake(l: &Launch) -> Result<Granted, String> {
    use mcp_protocol as p;
    let stream = connect_endpoint(l).await?;
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let timeout = CONNECT_WRITE_TIMEOUT;
    let cn = p::auth_nonce().ok_or("本机熵源不可用，无法完成握手")?;

    // 1. hello：证明我知道密钥。
    let hello = serde_json::to_string(&p::HelloFrame {
        hello: p::Hello {
            auth: p::AUTH_VERSION,
            cn: cn.clone(),
            mac: p::auth_mac(&l.key, p::AuthStep::Hello, &cn, ""),
        },
    })
    .map_err(|e| e.to_string())?;
    send_line(&mut w, &hello, timeout).await?;

    // 2. 对端的证明。EOF = 它一个字节都没回就断开了：这是 iShell 对「持有的密钥不是它的」的
    //    唯一回应（见 `handle_hello`）。
    let silent = || {
        format!(
            "iShell 端点 {} 没有应答就断开了连接。最常见的原因：这份环境里的密钥不是那台 iShell 本次运行\
             的密钥（环境来自别的 iShell、来自 iShell 重启之前，或被别的配置覆盖了）；也可能是那台 iShell \
             启动时关着 AI 控制、正忙于处理大量连接、或本机熵源不可用。不会改连别的 iShell。请在**你自己\
             的 iShell 终端**里重新启动 AI；仍不行就看那台 iShell 的日志。",
            l.endpoint.display()
        )
    };
    let Some(line) = read_line(&mut reader, timeout).await? else {
        return Err(silent());
    };
    let ok: p::HelloOkFrame = serde_json::from_str(line.trim()).map_err(|e| {
        format!("iShell 的认证应答无法解析（{e}）：对面不是配套版本的 iShell，请升级 iShell 并重新部署 ishell-mcp")
    })?;
    let ok = ok.hello_ok;
    // **验过才证明自己**：对面必须也持有密钥，否则就是别人的 socket，不能把自己的证明交给它。
    // （形状不对的 `sn` 同样当冒充者处理，理由见 `is_hex64`。）
    if !p::is_hex64(&ok.sn) || !p::auth_mac_matches(&l.key, p::AuthStep::Server, &cn, &ok.sn, &ok.smac) {
        return Err(format!(
            "端点 {} 后面的对端无法证明它持有这份密钥——它不是启动你的那台 iShell。已放弃，\
             没有向它出示任何证明。",
            l.endpoint.display()
        ));
    }
    let Some(app) = p::negotiate_app(p::MCP_APP_MIN, p::MCP_APP_MAX, ok.app_min, ok.app_max)
    else {
        return Err(version_msg(ok.app_min, ok.app_max));
    };

    // 3. prove：证明我知道密钥，并声明我是谁（actor）。
    let prove = serde_json::to_string(&p::ProveFrame {
        prove: p::Prove {
            cmac: p::auth_mac(&l.key, p::AuthStep::Client, &cn, &ok.sn),
            actor: current_actor(),
            app,
        },
    })
    .map_err(|e| e.to_string())?;
    send_line(&mut w, &prove, timeout).await?;
    let Some(line) = read_line(&mut reader, timeout).await? else {
        return Err(silent());
    };
    if let Ok(t) = serde_json::from_str::<p::TicketFrame>(line.trim()) {
        return Ok(Granted {
            ticket: t.ticket.ticket,
            peer: ok.peer,
        });
    }
    // 认证已通过之后 iShell 才会开口说话：这时的错误是人话，原样带回去。
    match serde_json::from_str::<McpResponse>(line.trim()) {
        Ok(McpResponse { result: Err(msg), .. }) => Err(msg),
        _ => Err("iShell 在握手最后一步给出了无法理解的应答".into()),
    }
}

#[cfg(unix)]
fn version_msg(peer_min: u32, peer_max: u32) -> String {
    format!(
        "iShell 与 ishell-mcp 的业务协议版本对不上（iShell 支持 v{peer_min}–v{peer_max}，本代理支持 v{}–v{}）。\
         请用与当前 iShell 配套的 ishell-mcp 重新部署（终端右键「安装 AI 控制代理到这台服务器…」），\
         然后在 iShell 终端里重新启动 AI。",
        mcp_protocol::MCP_APP_MIN,
        mcp_protocol::MCP_APP_MAX
    )
}

#[cfg(unix)]
async fn send_line(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    line: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let mut buf = line.to_string();
    buf.push('\n');
    tokio::time::timeout(timeout, w.write_all(buf.as_bytes()))
        .await
        .map_err(|_| "发送请求给 iShell 超时".to_string())?
        .map_err(|e| e.to_string())
}

/// 读一行（握手阶段）；`Ok(None)` = 对端在换行前关闭了连接、或行超长（> [`mcp_protocol::REPLY_LINE_MAX`]）。
/// **有界**：对端此刻还没证明过自己，不能让它靠不发换行把本进程的内存灌满。
#[cfg(unix)]
async fn read_line(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    timeout: std::time::Duration,
) -> Result<Option<String>, String> {
    tokio::time::timeout(
        timeout,
        mcp_protocol::read_line_bounded(reader, mcp_protocol::REPLY_LINE_MAX),
    )
    .await
    .map_err(|_| "等待 iShell 应答超时".to_string())
}

/// 拿一条连向**启动本代理的那台 iShell** 的连接，外加要填进请求的连接凭据。
///
/// 第一次调用时握手；握手成功的凭据缓存起来给之后每条请求用。没有「凭据被拒就重新握手」、
/// 没有「连不上就按实例 id 去找」：凭据只会因为 iShell 重启而失效，而那时端点早就连不上了，
/// 该做的是让用户重开 agent，不是替他猜。
#[cfg(unix)]
async fn connect_bound() -> Result<(UnixStream, String), String> {
    connect_with(launch()?, &GRANTED).await
}

/// [`connect_bound`] 的本体：连接信息与凭据槽由调用方给，测试可以各自用一份。
#[cfg(unix)]
async fn connect_with(
    l: &Launch,
    slot: &tokio::sync::Mutex<Option<Granted>>,
) -> Result<(UnixStream, String), String> {
    let ticket = {
        let mut g = slot.lock().await;
        if g.is_none() {
            let granted = handshake(l).await?;
            eprintln!(
                "ishell-mcp: 已连接 iShell（主机 {}，运行 {}）",
                granted.peer.host, granted.peer.run
            );
            *g = Some(granted);
        }
        g.as_ref().map(|g| g.ticket.clone()).unwrap_or_default()
    };
    let stream = connect_endpoint(l).await?;
    Ok((stream, ticket))
}

/// 对端 iShell 的自我介绍（握手通过后才有）：「现在连的是哪台电脑的 iShell」。附在列表类工具的
/// 返回里，让 agent 和用户一眼看出连对了没有，不必等到命令落错机器才发现。
#[cfg(unix)]
async fn peer_note() -> Option<String> {
    let g = GRANTED.lock().await;
    g.as_ref().map(|g| {
        format!(
            "已连接的 iShell：主机 {}（本次运行 {}）。你只能操作这一台。",
            g.peer.host, g.peer.run
        )
    })
}

#[cfg(not(unix))]
async fn peer_note() -> Option<String> {
    None
}

/// 业务连接要发的前两行（不含末尾换行）：`{"bearer":<凭据>}` + 请求 JSON。凭据单独成行，
/// iShell 核对通过之后才去读后面那行（见 `mcp_protocol` 的「认证层」）。
#[cfg(unix)]
fn request_lines(id: u64, ticket: &str, kind: McpReqKind) -> Result<String, String> {
    let bearer = serde_json::to_string(&mcp_protocol::BearerFrame {
        bearer: ticket.to_string(),
    })
    .map_err(|e| e.to_string())?;
    let request = serde_json::to_string(&McpRequest {
        id,
        origin: Some(caller_origin()),
        actor: None, // 不上线（`#[serde(skip)]`）
        kind,
    })
    .map_err(|e| e.to_string())?;
    Ok(format!("{bearer}\n{request}"))
}

/// 在一条已建立的连接上完成一问一答：写凭据行 + 请求行，读一行响应 JSON。
///
/// `origin`（可读来源）仅供 iShell 在会话旁标注「谁开的」，不是身份；身份只来自 `ticket`
/// （绑定了握手时声明的 actor，iShell 以它为准）。
#[cfg(unix)]
async fn exchange(
    stream: UnixStream,
    ticket: String,
    kind: McpReqKind,
    response_timeout: std::time::Duration,
) -> Result<McpReqResult, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (r, mut w) = stream.into_split();
    let mut line = request_lines(
        id,
        &ticket,
        kind,
    )?;
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
        return Err(
            "iShell 没有应答就断开了连接：连接凭据无效（iShell 重启过，或凭据已被淘汰——极少见），\
             或 iShell 正在退出。请在 iShell 终端里重新启动 AI。"
                .into(),
        );
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
    let (stream, ticket) = connect_bound().await?;
    exchange(stream, ticket, kind, RESPONSE_TIMEOUT).await
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
        return Err(format!(
            "{field} 必须是运行 ishell-mcp 的调用方机器上的绝对路径"
        ));
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
        return Err(
            "调用方流式上传当前只支持单个普通文件；目录请使用 git/rsync，或逐文件上传".into(),
        );
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (stream, ticket) = connect_bound().await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut header = request_lines(
        id,
        &ticket,
        McpReqKind::CopyToRemoteFromCaller {
            session_uid,
            remote_path,
            size: metadata.len(),
            timeout_ms,
        },
    )?;
    header.push('\n');
    tokio::time::timeout(
        CONNECT_WRITE_TIMEOUT,
        write_half.write_all(header.as_bytes()),
    )
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
                // 追读也没读到东西：iShell 在认证阶段就把连接静默关了（凭据无效——iShell 重启过或
                // 凭据已被淘汰），或 iShell 正在退出。别只丢一句 Broken pipe。
                return Err(format!(
                    "发送调用方文件流失败: {error}（iShell 没有应答就断开了连接：连接凭据可能已失效——\
                     iShell 重启过，或凭据已被淘汰——或 iShell 正在退出；请在 iShell 终端里重新启动 AI）"
                ));
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
    let (stream, ticket) = connect_bound().await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut header = request_lines(
        id,
        &ticket,
        McpReqKind::CopyFromRemoteToCaller {
            session_uid,
            remote_path,
            timeout_ms,
        },
    )?;
    header.push('\n');
    tokio::time::timeout(
        CONNECT_WRITE_TIMEOUT,
        write_half.write_all(header.as_bytes()),
    )
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
    let resp: McpResponse =
        serde_json::from_str(header_line.trim()).map_err(|error| error.to_string())?;
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
        dest.flush()
            .await
            .map_err(|error| format!("落盘调用方文件失败: {error}"))?;
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
                Some(_) => {}   // 还在连接/认证中，继续等
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

/// 同 [`text_result`]，并在后面附一行「现在连的是哪台电脑的 iShell」（握手回报的，已认证）。
/// 只给列表类工具用：不改任何既有返回的形状，只多一个文本块。
async fn text_result_with_peer(body: Result<McpReqResult, String>) -> Result<CallToolResult, McpError> {
    let mut result = text_result(body)?;
    if let Some(peer) = peer_note().await {
        result.content.push(ContentBlock::text(peer));
    }
    Ok(result)
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
                        的一小段时间可能为 null）、endpoint_injected（连接 iShell 的环境是否已注入该 shell；\
                        false = 在这里面再启动 AI 会连不上 iShell，等 shell 空下来会自动补注），\
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
                        s.title.to_lowercase().contains(&f) || s.host.to_lowercase().contains(&f)
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
                         你连的是启动你的那个终端所属的 iShell，不是用户当前正在看的那个窗口。"
                    };
                    let mut blocks = vec![
                        ContentBlock::text("[]"),
                        ContentBlock::text(note.to_string()),
                    ];
                    if let Some(peer) = peer_note().await {
                        blocks.push(ContentBlock::text(peer));
                    }
                    return Ok(CallToolResult::success(blocks));
                }
                text_result_with_peer(Ok(McpReqResult::Sessions(list))).await
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
        Parameters(StartCommandArgs {
            session_uid,
            command,
        }): Parameters<StartCommandArgs>,
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
        text_result_with_peer(call(McpReqKind::ListSavedConnections).await).await
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
        Parameters(SendInputArgs {
            session_uid,
            text,
            escapes,
        }): Parameters<SendInputArgs>,
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
            copy_to_remote_from_caller(session_uid, local_path, remote_path, timeout_ms).await,
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
            copy_from_remote_to_caller(session_uid, remote_path, local_path, timeout_ms).await,
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
                    者不要了就 interrupt 释放。连不上 iShell（缺环境、端点连不上、握手被拒）：**按错误文案做，不要重试，也\
                    不要自己去找别的 iShell**——你只可能连到启动你的那个终端所属的那一台，没有别的\
                    选项，也没有降级。常见原因：① 你不是从 iShell 终端里启动的（请用户在自己的 iShell \
                    终端里启动 AI；若那个终端连上后敲过键盘被跳过了注入，右键「立即注入 MCP 连接\
                    环境」后重新启动 AI）；② iShell 重启过或它到服务器的 SSH 重连过（环境里的端点与\
                    密钥随之作废，在 iShell 终端里重新启动 AI）；③ 设置里关掉了 AI 控制；④ tmux/screen \
                    继承的是会话**之前**的环境，里面启动的 AI 拿到的可能是旧的（或别的会话的）\
                    环境，在 iShell 终端里直接启动。\n\
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
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
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
/// 在握手时记下这个值并绑到连接凭据上；业务请求里根本没有 actor 这个字段。
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
/// 不是保密凭据：持有同一运行密钥的代理本来就能声明任意 actor，身份隔离靠的是 iShell 签发、
/// 绑定 actor 的连接凭据，以及「密钥只在这台 iShell 的终端里」。
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
    let boot: String = boot_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(16)
        .collect();
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

/// 给 iShell 在会话旁标注「这是谁的 AI 开的」的来源描述：哪个 unix 用户、哪个进程在发起。
/// 纯展示信息，不构成身份——身份只来自握手签发的连接凭据。
///
/// AI 可用环境变量 `ISHELL_MCP_ORIGIN` 自报更好认的名字（如 `Codex CLI (e5-1)`）；
/// 没设就用 `USER (ishell-mcp pid N)`，用户凭 user/pid 一眼判断是不是自己的 AI（`ps` 可查）。
/// 空白与控制字符会被压平/截断（上限 80 字符），它毕竟是要显示在标签旁的一行字。
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
    use super::current_actor;
    use super::{launch_from, missing_env_message};

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

    /// actor 跟着 AI 客户端（父进程）走：同一个客户端换代理进程（`/mcp` 重连）得到同一个值，
    /// 否则它刚开的会话立刻变成「不是你的」。这里钉住派生规则本身：同输入同输出、任一要素
    /// 不同即不同。反向对照：把 `actor_from_parts` 改成混入随机数，第一条断言挂。
    #[test]
    fn actor_is_derived_from_the_client_not_the_proxy_process() {
        use super::actor_from_parts;
        let boot = "3f2a9c1e-7b4d-4e11-9a0b-5c6d7e8f9012\n";
        let a = actor_from_parts(boot, 4242, 987654).expect("完整输入应能派生");
        assert_eq!(
            Some(a.clone()),
            actor_from_parts(boot, 4242, 987654),
            "同一个客户端 → 同一个 actor"
        );
        assert_eq!(a, "3f2a9c1e7b4d4e11-4242-987654");
        assert_ne!(
            Some(a.clone()),
            actor_from_parts(boot, 4243, 987654),
            "不同父进程"
        );
        assert_ne!(
            Some(a.clone()),
            actor_from_parts(boot, 4242, 987655),
            "pid 被复用给了新进程"
        );
        assert_ne!(
            Some(a),
            actor_from_parts("0000000000000000", 4242, 987654),
            "另一台机器/另一次开机"
        );
        assert_eq!(
            actor_from_parts("short", 1, 1),
            None,
            "开机标识不完整就不派生"
        );
    }

    /// `/proc/<pid>/stat` 的进程名在括号里、可以含空格和括号——必须从最后一个 `)` 往后数。
    #[test]
    fn proc_start_time_survives_hostile_process_names() {
        use super::proc_start_time;
        let tail = "S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 555000 19 20";
        assert_eq!(
            proc_start_time(&format!("123 (claude) {tail}")),
            Some(555000)
        );
        assert_eq!(
            proc_start_time(&format!("123 (a) b (c d) {tail}")),
            Some(555000)
        );
        assert_eq!(proc_start_time("garbage"), None);
    }

    #[test]
    fn current_actor_is_stable_within_a_process() {
        let a = current_actor();
        assert_eq!(a, current_actor(), "同进程内 actor 必须稳定");
        assert!(!a.is_empty());
    }

    #[test]
    fn caller_path_rejects_root_like_gui_local_path() {
        use super::validate_caller_path;
        assert!(validate_caller_path("/", "local_path").is_err());
        assert!(validate_caller_path("/tmp/notes.txt", "local_path").is_ok());
        assert!(validate_caller_path("/tmp/./notes.txt", "local_path").is_err());
        assert!(validate_caller_path("notes.txt", "local_path").is_err());
    }

    /// 连接环境的判定：缺哪个就报哪个，空白等于没设，端点必须是绝对路径。
    /// 反向对照：把 `launch_from` 改成缺密钥也放行，第二条断言当场挂。
    #[test]
    fn launch_requires_both_variables_and_an_absolute_endpoint() {
        let s = |v: &str| Some(v.to_string());
        let ok = launch_from(s("/home/u/.ishell-mcp/mcp-1.sock"), s("k")).expect("两个都有应放行");
        assert_eq!(ok.key, "k");
        let bad = launch_from(s("/x/mcp.sock"), None).unwrap_err();
        assert_eq!(bad, vec!["ISHELL_MCP_KEY".to_string()]);
        let bad = launch_from(None, s("k")).unwrap_err();
        assert_eq!(bad, vec!["ISHELL_MCP_ENDPOINT".to_string()]);
        assert_eq!(launch_from(None, None).unwrap_err().len(), 2);
        assert!(launch_from(s("  "), s("  ")).is_err(), "空白等于没设");
        let rel = launch_from(s("mcp.sock"), s("k")).unwrap_err();
        assert!(rel[0].contains("绝对路径"), "{rel:?}");
    }

    /// 缺环境的报错必须说清三件事：规则（只在 iShell 终端里启动才可用、不会连别的）、该做什么、
    /// 诊断（谁拉起的、看得到哪些变量）——后者是分辨「环境被过滤」与「代理被换了环境重拉」的依据。
    #[test]
    fn missing_env_message_states_the_rule_and_carries_diagnostics() {
        let msg = missing_env_message(&["ISHELL_MCP_KEY".to_string()]);
        for want in ["ISHELL_MCP_KEY", "不会去连别的 iShell", "停止重试", "父进程 pid=", "ISHELL_*"] {
            assert!(msg.contains(want), "缺 {want:?}：{msg}");
        }
    }
}

/// 用「假 iShell」验证代理**只连环境指定的那一个**。假对端按 `mcp_protocol` 的认证层规范实现
/// （同一份 `auth_mac`），各自监听一条 socket、各有各的密钥，并数自己被连了几次——
/// 「B 的 agent 绝不会碰 C」就落成「C 的计数恒为 0」。
#[cfg(all(test, unix))]
mod isolation_tests {
    use super::mcp_protocol::{self as p, McpReqKind, McpReqResult, McpRequest, McpResponse};
    use super::{connect_with, exchange, handshake, Granted, Launch};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// 按规范应答。
        Honest,
        /// 能收到 hello 但算不出对的 `smac`（不知道密钥的冒充者）。
        Impostor,
        /// 业务协议区间与代理对不上。
        FutureVersion,
    }

    struct Fake {
        path: std::path::PathBuf,
        /// 被连接的次数（任何连接，哪怕马上断开）。
        hits: Arc<AtomicUsize>,
        /// 收到「第三步 prove」的次数——冒充者/版本不拢时代理**不该**发出它。
        proves: Arc<AtomicUsize>,
        tickets: Arc<Mutex<Vec<String>>>,
    }

    fn unique_dir() -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "ishell-mcp-iso-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spawn_fake(dir: &std::path::Path, name: &str, key: &str, host: &str, mode: Mode) -> Fake {
        let path = dir.join(name);
        let listener = UnixListener::bind(&path).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let proves = Arc::new(AtomicUsize::new(0));
        let tickets = Arc::new(Mutex::new(Vec::new()));
        let (h, pr, tk) = (hits.clone(), proves.clone(), tickets.clone());
        let (key, host) = (key.to_string(), host.to_string());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                h.fetch_add(1, Ordering::Relaxed);
                let (key, host, pr, tk) = (key.clone(), host.clone(), pr.clone(), tk.clone());
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    let Ok(Some(first)) = lines.next_line().await else {
                        return;
                    };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&first) else {
                        return;
                    };
                    if v.get("hello").is_some() {
                        let Ok(f) = serde_json::from_value::<p::HelloFrame>(v) else {
                            return;
                        };
                        let cn = f.hello.cn;
                        // 与真 iShell 一样：mac 不对就**静默断开**。
                        if !p::auth_mac_matches(&key, p::AuthStep::Hello, &cn, "", &f.hello.mac) {
                            return;
                        }
                        let sn = "ab".repeat(32);
                        let smac = if mode == Mode::Impostor {
                            "0".repeat(64)
                        } else {
                            p::auth_mac(&key, p::AuthStep::Server, &cn, &sn)
                        };
                        let (lo, hi) = if mode == Mode::FutureVersion { (9, 9) } else { (8, 8) };
                        let ok = p::HelloOkFrame {
                            hello_ok: p::HelloOk {
                                sn: sn.clone(),
                                smac,
                                app_min: lo,
                                app_max: hi,
                                caps: vec![],
                                peer: p::Peer {
                                    host,
                                    run: "run-1".into(),
                                },
                            },
                        };
                        let _ = w
                            .write_all(format!("{}\n", serde_json::to_string(&ok).unwrap()).as_bytes())
                            .await;
                        let second = tokio::time::timeout(
                            std::time::Duration::from_millis(400),
                            lines.next_line(),
                        )
                        .await;
                        let Ok(Ok(Some(line2))) = second else {
                            return;
                        };
                        pr.fetch_add(1, Ordering::Relaxed);
                        let Ok(pf) = serde_json::from_str::<p::ProveFrame>(&line2) else {
                            return;
                        };
                        if !p::auth_mac_matches(&key, p::AuthStep::Client, &cn, &sn, &pf.prove.cmac)
                        {
                            return;
                        }
                        let t = p::TicketFrame {
                            ticket: p::TicketGrant {
                                ticket: format!("ticket-for-{}", pf.prove.actor),
                                app: pf.prove.app,
                            },
                        };
                        let _ = w
                            .write_all(format!("{}\n", serde_json::to_string(&t).unwrap()).as_bytes())
                            .await;
                    } else if let Some(bearer) = v.get("bearer").and_then(|b| b.as_str()) {
                        // 业务连接：第一行出示凭据，第二行才是请求（与真 iShell 一样）
                        let bearer = bearer.to_string();
                        let Ok(Some(line2)) = lines.next_line().await else {
                            return;
                        };
                        let Ok(req) = serde_json::from_str::<McpRequest>(&line2) else {
                            return;
                        };
                        tk.lock().unwrap().push(bearer);
                        let resp = McpResponse {
                            id: req.id,
                            result: Ok(McpReqResult::Sessions(vec![])),
                        };
                        let _ = w
                            .write_all(format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes())
                            .await;
                    }
                });
            }
        });
        Fake {
            path,
            hits,
            proves,
            tickets,
        }
    }

    fn launch(path: &std::path::Path, key: &str) -> Launch {
        Launch {
            endpoint: path.to_path_buf(),
            key: key.to_string(),
        }
    }

    fn slot() -> tokio::sync::Mutex<Option<Granted>> {
        tokio::sync::Mutex::const_new(None)
    }

    /// **核心要求**：B、C 两台电脑的 iShell 把 socket 反向转发进了同一台 AI 服务器的同一个目录。
    /// 在 B 的终端里启动的 agent（环境指向 B）能操作 B，并且**C 一次连接都没收到**——代理既不会
    /// 扫目录，也不会「顺便问一下」别的端点。
    /// 反向对照：让代理在连接前先枚举同目录下的 `*.sock` 逐个探测，C 的 hits 立刻大于 0。
    #[tokio::test]
    async fn an_agent_started_in_b_never_touches_c() {
        let dir = unique_dir();
        let b = spawn_fake(&dir, "mcp-b.sock", "key-of-b", "host-b", Mode::Honest);
        let c = spawn_fake(&dir, "mcp-c.sock", "key-of-c", "host-c", Mode::Honest);
        let g = slot();
        let l = launch(&b.path, "key-of-b");
        let (stream, ticket) = connect_with(&l, &g).await.expect("B 的 agent 应连上 B");
        let r = exchange(stream, ticket.clone(), McpReqKind::ListSessions, std::time::Duration::from_secs(3))
            .await
            .expect("业务请求应成功");
        assert!(matches!(r, McpReqResult::Sessions(_)));
        assert_eq!(b.tickets.lock().unwrap().as_slice(), [ticket], "请求带着握手签发的凭据");
        assert_eq!(g.lock().await.as_ref().unwrap().peer.host, "host-b", "握手回报了对端是谁");
        assert_eq!(c.hits.load(Ordering::Relaxed), 0, "B 的 agent 碰了 C！");
    }

    /// 环境里的端点连不上（iShell 重启 / SSH 重连换了端点）：报错，**不退到同目录里别的端点**。
    /// 「不能连就是不能连」——即使旁边就躺着一台能连上的 iShell。
    #[tokio::test]
    async fn a_dead_endpoint_fails_instead_of_falling_back_to_a_neighbour() {
        let dir = unique_dir();
        let c = spawn_fake(&dir, "mcp-c.sock", "key-of-c", "host-c", Mode::Honest);
        let g = slot();
        let l = launch(&dir.join("mcp-gone.sock"), "key-of-b");
        let err = connect_with(&l, &g).await.expect_err("端点不存在必须失败");
        assert!(err.contains("连不上") && err.contains("不会去连别的 iShell"), "{err}");
        assert_eq!(c.hits.load(Ordering::Relaxed), 0, "失败后不得去试邻居");
    }

    /// 端点指向的是 C（环境被拷贝/覆盖成了别人的端点），但密钥是 B 的：C 对它一个字节都不回，
    /// 代理报「密钥不是那台 iShell 的」，而不是改连别处或把证明交出去。
    #[tokio::test]
    async fn a_foreign_endpoint_with_the_wrong_key_gets_silence_and_a_clear_error() {
        let dir = unique_dir();
        let c = spawn_fake(&dir, "mcp-c.sock", "key-of-c", "host-c", Mode::Honest);
        let g = slot();
        let err = connect_with(&launch(&c.path, "key-of-b"), &g)
            .await
            .expect_err("密钥不对必须失败");
        assert!(err.contains("没有应答就断开"), "{err}");
        assert!(g.lock().await.is_none(), "失败不得留下凭据");
    }

    /// 冒充者（不知道密钥，算不出对的 `smac`）：代理必须**验过才证明自己**——绝不把自己的证明
    /// 交给一个没证明过的对端。
    /// 反向对照：把 `handshake` 里验 `smac` 的那一步删掉，proves 立刻变成 1。
    #[tokio::test]
    async fn an_impostor_never_receives_our_proof() {
        let dir = unique_dir();
        let fake = spawn_fake(&dir, "mcp-x.sock", "shared-key", "host-x", Mode::Impostor);
        let err = handshake(&launch(&fake.path, "shared-key")).await.expect_err("必须失败");
        assert!(err.contains("无法证明"), "{err}");
        assert_eq!(fake.proves.load(Ordering::Relaxed), 0, "代理把证明交给了没证明过的对端");
    }

    /// 业务协议区间对不上：认证通过后明确报「请升级/重新部署」，且不继续发 prove。
    #[tokio::test]
    async fn a_version_gap_is_reported_after_authentication_not_guessed() {
        let dir = unique_dir();
        let fake = spawn_fake(&dir, "mcp-v.sock", "k", "host-v", Mode::FutureVersion);
        let err = handshake(&launch(&fake.path, "k")).await.expect_err("必须失败");
        assert!(err.contains("版本对不上") && err.contains("重新部署"), "{err}");
        assert_eq!(fake.proves.load(Ordering::Relaxed), 0);
    }

    /// 流氓对端（端点 socket 被别的进程换了）：回一行超过上限、且永不换行的数据，然后保持连接沉默。
    /// 代理读应答是有界的——越过 `REPLY_LINE_MAX` 就放弃；无界的话它会一直等下去，直到 5 秒读
    /// 超时（那期间本地 socket 能灌进几 GB）。所以断言「2 秒内就放弃」。
    /// 反向对照：把 `REPLY_LINE_MAX` 改成 `usize::MAX / 2`，耗时顶到 5 秒读超时，本条挂。
    #[tokio::test]
    async fn a_rogue_peer_that_floods_without_newline_is_cut_off_at_the_cap() {
        let dir = unique_dir();
        let path = dir.join("mcp-rogue.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let junk = vec![b'a'; p::REPLY_LINE_MAX + 4096];
            let _ = s.write_all(&junk).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await; // 连接保持，沉默
        });
        let t0 = std::time::Instant::now();
        let err = handshake(&launch(&path, "k")).await.expect_err("流氓对端必须失败");
        assert!(err.contains("没有应答就断开"), "{err}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "没有在上限处放弃：{:?}",
            t0.elapsed()
        );
    }

    /// 握手失败不缓存任何东西：下次调用重新握手**同一个端点、同一个密钥**（不是换别的）。
    #[tokio::test]
    async fn a_failed_handshake_is_retried_against_the_same_endpoint_only() {
        let dir = unique_dir();
        let c = spawn_fake(&dir, "mcp-c.sock", "key-of-c", "host-c", Mode::Honest);
        let g = slot();
        let bad = launch(&dir.join("mcp-b.sock"), "key-of-b");
        for _ in 0..3 {
            assert!(connect_with(&bad, &g).await.is_err());
        }
        assert_eq!(c.hits.load(Ordering::Relaxed), 0);
        // B 的端点后来出现了（SSH 重连注册完成）：同一份环境现在连得上。
        let b = spawn_fake(&dir, "mcp-b.sock", "key-of-b", "host-b", Mode::Honest);
        assert!(connect_with(&bad, &g).await.is_ok());
        assert!(b.hits.load(Ordering::Relaxed) >= 1);
        assert_eq!(c.hits.load(Ordering::Relaxed), 0);
    }
}
