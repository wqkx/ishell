//! 本机 API：回环上的 HTTP/1.1 + JSON，给桌面上的本地程序（尤其是本地 AI agent）用，
//! 能力与 MCP 一一对应，业务层完全复用（同一条 `McpCall` 通道、同一份会话归属规则）。
//!
//! 和 MCP 的区别只在“怎么进来”：
//! - MCP 经 SSH 反向转发给服务器上的 agent，靠**进程祖先**认主人；
//! - 本机 API 面向本机进程，没有祖先可认，门只有一把**令牌**——所以有独立开关（左侧面板，默认关），
//!   令牌每次开启随机生成，只写进 `~/.config/ishell/api.json`（Unix 上 0600）。
//!
//! 安全模型（为什么这样写）：
//! - 只绑 `127.0.0.1`，固定端口；被占就明说，**不换端口**（“谁拥有 API”只有一个答案）。
//! - 回环 HTTP 浏览器里的网页也打得到，所以认证之前先过三关：`Host` 必须是回环地址（防 DNS 重绑定）、
//!   带 `Origin` 一律拒绝（浏览器跨站请求必带）、`Authorization: Bearer` 令牌（常量时间比较）。
//!   不发任何 CORS 头，`OPTIONS` 不应答，浏览器的预检过不去。
//! - **认证之前不读请求体**：认证前只缓冲最多 [`HEAD_MAX`] 字节的请求头，且有时限与并发上限。
//! - 每个调用方用 `X-Ishell-Actor` 自报名字，iShell 内部记成 `api:<名字>`；会话归属沿用 MCP 的硬规则
//!   （只能读写自己用 `open_session` 开的会话）。`api:` 前缀保证 API 调用方永远冒用不了 MCP 代理的身份。
//!   名字是自报的：持令牌的进程能冒用另一个 API 调用方的名字——与 MCP 同样的、已写进文档的边界。
//! - 一条连接一个请求（`Connection: close`），不支持 keep-alive/分块/`Expect`，免去一整类解析歧义。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

use super::mcp_bridge::McpCall;
use crate::mcp_protocol::{McpReqKind, McpReqResult, McpRequest, McpResponse};
use crate::store::{self, ApiState};

/// 认证前最多缓冲的请求头字节数。
const HEAD_MAX: usize = 16 * 1024;
/// 读完整个请求头的时限（认证前，防慢速占坑）。
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// 读请求体的时限（已认证）。
const BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// 请求体上限，与 MCP 的单行请求上限一致（write_file 的内容内联在里面）。
const BODY_MAX: usize = 32 * 1024 * 1024;
/// 认证前并发连接上限（占的是请求头缓冲 + 一个任务）。
const MAX_PREAUTH: usize = 64;
/// 已认证的并发请求上限（与 MCP 的工作信号量同值）。
const MAX_WORK: usize = 32;
/// actor 名字的最大长度。
const ACTOR_MAX: usize = 64;
/// API 调用方在内部的 actor 命名空间。MCP 握手**拒绝**以它开头的 actor（`mcp_bridge::actor_acceptable`），
/// 所以 API 调用方与 MCP 代理的身份永不相等。
pub(super) const ACTOR_NS: &str = "api:";

/// 每个服务实例共享的东西。
struct Shared {
    token: String,
    port: u16,
    tx: mpsc::UnboundedSender<McpCall>,
    ctx: egui::Context,
    next_id: AtomicU64,
    preauth: Arc<Semaphore>,
    work: Arc<Semaphore>,
}

/// 服务的生命周期：跟着“开关”走。每帧调 [`ApiService::sync`]，开关变了它就起/停。
pub(super) struct ApiService {
    task: Option<tokio::task::JoinHandle<()>>,
    tx: mpsc::UnboundedSender<McpCall>,
    /// 代次。`stop()` 在锁内把它加一再清理状态；`run_server` 发布状态（写发现文件、置 Listening/Failed）
    /// 也在同一把锁内先核对自己的代次。`abort` 只在 await 点生效，被取消的任务可能还跑完一小段同步代码，
    /// 没有这把锁，“刚开就关”会留下一个没人监听却显示绿点、令牌文件也在的假象。
    epoch: Arc<std::sync::Mutex<u64>>,
}

impl ApiService {
    pub(super) fn new(tx: mpsc::UnboundedSender<McpCall>) -> Self {
        Self {
            task: None,
            tx,
            epoch: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// 让运行状态跟上开关。起不来（端口被占…）时**不自动重试**：任务结束、状态是 `Failed`，
    /// 等用户关掉再开一次——每帧重试只会刷屏。
    pub(super) fn sync(&mut self, runtime: &Arc<tokio::runtime::Runtime>, ctx: &egui::Context) {
        let want = store::load_api_enabled();
        match (want, self.task.is_some()) {
            (true, false) => {
                store::set_api_state(ApiState::Starting);
                let tx = self.tx.clone();
                let ctx = ctx.clone();
                let epoch = self.epoch.clone();
                let mine = *epoch.lock().unwrap_or_else(|e| e.into_inner());
                self.task = Some(runtime.spawn(run_server(ctx, tx, store::api_port(), epoch, mine)));
            }
            (false, true) => self.stop(),
            _ => {}
        }
    }

    /// 关闭服务。App 退出时也要调（见 `App::on_exit`）：发现文件里是活的令牌，不能因为没走 Drop 就留在磁盘上。
    pub(super) fn stop(&mut self) {
        let mut g = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        *g += 1; // 之后任何旧代的 run_server 都发布不了状态
        if let Some(t) = self.task.take() {
            t.abort(); // 连带丢弃它的 JoinSet：在途连接一并终止
        }
        store::set_api_state(ApiState::Off);
        store::remove_api_info();
    }
}

impl Drop for ApiService {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run_server(
    ctx: egui::Context,
    tx: mpsc::UnboundedSender<McpCall>,
    port: u16,
    epoch: Arc<std::sync::Mutex<u64>>,
    mine: u64,
) {
    // 在锁内核对代次后再发布状态；代次已变说明 `stop()` 来过，什么都不要动。
    let fail = |msg: String| {
        let g = epoch.lock().unwrap_or_else(|e| e.into_inner());
        if *g == mine {
            log::warn!("本机 API 启动失败：{msg}");
            store::set_api_state(ApiState::Failed(msg));
            ctx.request_repaint();
        }
    };
    let listener = match bind_loopback(port).await {
        Ok(l) => l,
        Err(e) => {
            fail(if e.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "端口 {port} 已被占用（可能是另一个 iShell 实例已开启了本机 API，或别的程序占着）"
                )
            } else {
                format!("无法监听 127.0.0.1:{port}：{e}")
            });
            return;
        }
    };
    let Some(token) = crate::mcp_protocol::random_hex(32) else {
        fail("本机熵源不可用，无法生成令牌".into());
        return;
    };
    {
        let g = epoch.lock().unwrap_or_else(|e| e.into_inner());
        if *g != mine {
            return; // 刚开就关：别写发现文件、别置 Listening
        }
        if let Err(e) = store::write_api_info(&format!("http://127.0.0.1:{port}"), &token) {
            drop(g);
            fail(e);
            return;
        }
        store::set_api_state(ApiState::Listening { port });
    }
    ctx.request_repaint();
    let shared = Arc::new(Shared {
        token,
        port,
        tx,
        ctx,
        next_id: AtomicU64::new(1),
        preauth: Arc::new(Semaphore::new(MAX_PREAUTH)),
        work: Arc::new(Semaphore::new(MAX_WORK)),
    });
    serve(listener, shared).await;
}

/// 绑回环。刚关掉又立刻打开时，上一次的监听 socket 要等被取消的任务真正落地才释放，
/// 所以对 `AddrInUse` 短暂重试——只等**自己**的残留；等满还占着就如实报告，不换端口。
async fn bind_loopback(port: u16) -> std::io::Result<TcpListener> {
    let mut last = None;
    for _ in 0..10 {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::ErrorKind::AddrInUse.into()))
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let mut conns = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else {
                    tokio::time::sleep(Duration::from_millis(50)).await; // fd 耗尽等：别空转
                    continue;
                };
                if !peer.ip().is_loopback() {
                    continue; // 绑的是回环，来者不该是别的；纵深防御
                }
                let Ok(permit) = shared.preauth.clone().try_acquire_owned() else {
                    continue; // 认证前名额满：直接丢
                };
                conns.spawn(handle_conn(stream, shared.clone(), permit));
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

// ───────────────────────────── 左侧面板的开关 ─────────────────────────────

/// 左侧面板底部的“本机 API”开关行：点一下开/关（写入设置、立即生效），状态一眼可见。
pub(super) fn sidebar_toggle(ui: &mut egui::Ui) {
    use crate::i18n::tr;
    use crate::theme::Palette;
    use egui::RichText;

    let enabled = store::load_api_enabled();
    let state = store::api_state();
    let (dot, status): (egui::Color32, String) = match (&state, enabled) {
        (ApiState::Listening { port }, _) => (Palette::OK, format!("127.0.0.1:{port}")),
        (ApiState::Starting, _) => (Palette::WARN, tr("启动中…", "starting…").into()),
        (ApiState::Failed(_), _) => (Palette::DANGER, tr("启动失败", "failed").into()),
        (ApiState::Off, _) => (Palette::TEXT_DIM, tr("已关闭", "off").into()),
    };
    let icon = if matches!(state, ApiState::Listening { .. }) {
        egui_phosphor::regular::PLUGS_CONNECTED
    } else {
        egui_phosphor::regular::PLUGS
    };
    ui.add_space(4.0);
    let resp = ui
        .horizontal(|ui| {
            let b = ui.add(
                egui::Button::new(
                    RichText::new(format!("{icon}  {}", tr("本机 API", "Local API"))).color(Palette::TEXT),
                )
                .fill(if enabled { Palette::ACCENT_SOFT } else { Palette::PANEL_2 }),
            );
            ui.label(RichText::new("●").color(dot).size(11.0));
            ui.label(RichText::new(status).color(Palette::TEXT_DIM).size(11.0));
            b
        })
        .inner;
    let tip = match &state {
        ApiState::Failed(m) => m.clone(),
        _ => tr(
            "开启后，本机的其它程序（尤其是本地 AI agent）可以通过 HTTP API 控制 iShell，能力与 MCP 相同。\n\
             · 只监听 127.0.0.1；令牌每次开启随机生成，写在 ~/.config/ishell/api.json（仅本人可读）\n\
             · 持有令牌的本机进程就能用——只在你信任本机其它程序时开启\n\
             · 与 MCP 开关互相独立；只能读写 API 调用方自己开的会话，看不到你的窗口\n\
             · 右键可复制地址",
            "Lets other programs on this computer (especially local AI agents) control iShell over an \
             HTTP API, with the same capabilities as MCP.\n\
             · Listens on 127.0.0.1 only; the token is random per enable and stored in \
             ~/.config/ishell/api.json (owner-only)\n\
             · Any local process that holds the token can use it — enable only if you trust the other \
             programs on this machine\n\
             · Independent of the MCP switch; a caller can only touch sessions it opened itself\n\
             · Right-click to copy the address",
        )
        .to_string(),
    };
    let resp = resp.on_hover_text(tip);
    if resp.clicked() {
        store::save_api_enabled(!enabled);
    }
    resp.context_menu(|ui| {
        if let ApiState::Listening { port } = store::api_state() {
            if ui.button(tr("复制 API 地址", "Copy API address")).clicked() {
                ui.ctx().copy_text(format!("http://127.0.0.1:{port}"));
                ui.close();
            }
        }
        if let Some(p) = store::api_info_path() {
            if ui.button(tr("复制令牌文件路径", "Copy token file path")).clicked() {
                ui.ctx().copy_text(p.display().to_string());
                ui.close();
            }
        }
    });
    ui.add_space(2.0);
}

// ───────────────────────────── HTTP 解析 ─────────────────────────────

struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    /// 读请求头时顺带读进来的、属于请求体开头的字节。
    body_prefix: Vec<u8>,
}

impl Head {
    /// 取一个请求头。出现多次返回 `Err`（这些头里没有合法的重复用法，重复一律当畸形）。
    fn header(&self, name: &str) -> Result<Option<&str>, ()> {
        let mut found = None;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(());
                }
                found = Some(v.as_str());
            }
        }
        Ok(found)
    }
}

enum HeadError {
    Malformed,
    TooLarge,
    Closed,
}

/// 读到完整请求头为止（最多 [`HEAD_MAX`]）。
async fn read_head(stream: &mut TcpStream) -> Result<Head, HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 48];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => {
                if req.version != Some(1) {
                    return Err(HeadError::Malformed); // 只认 HTTP/1.1
                }
                let (Some(method), Some(path)) = (req.method, req.path) else {
                    return Err(HeadError::Malformed);
                };
                let mut hs = Vec::with_capacity(req.headers.len());
                for h in req.headers.iter() {
                    let v = std::str::from_utf8(h.value).map_err(|_| HeadError::Malformed)?;
                    hs.push((h.name.to_string(), v.trim().to_string()));
                }
                return Ok(Head {
                    method: method.to_string(),
                    path: path.to_string(),
                    headers: hs,
                    body_prefix: buf[n..].to_vec(),
                });
            }
            Ok(httparse::Status::Partial) => {}
            Err(_) => return Err(HeadError::Malformed),
        }
        if buf.len() >= HEAD_MAX {
            return Err(HeadError::TooLarge);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| HeadError::Closed)?;
        if n == 0 {
            return Err(HeadError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 常量时间比较（令牌长度是公开的固定值，长度不等直接 false）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Host` 必须指向回环地址和本服务的端口——挡 DNS 重绑定（攻击者域名解析到 127.0.0.1 后，
/// 浏览器发来的 Host 仍是攻击者的域名）。
fn host_allowed(host: &str, port: u16) -> bool {
    let h = host.to_ascii_lowercase();
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .iter()
    .any(|a| *a == h)
}

/// 取 `Bearer` 令牌。
fn bearer(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(rest.trim())
}

/// actor 名字：1..=64 个 `[A-Za-z0-9._-]`。
fn valid_actor_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= ACTOR_MAX
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// ───────────────────────────── 响应 ─────────────────────────────

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        422 => "Unprocessable Entity",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

async fn respond(stream: &mut TcpStream, code: u16, body: &serde_json::Value) {
    let body = body.to_string();
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        status_text(code),
        body.len()
    );
    if code == 401 {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
    // 提前拒绝（401/413/…）时请求体可能还没读：此刻直接关 socket，内核会因“接收队列里还有没读的数据”发 RST，
    // 把刚写出去的响应一起冲掉，调用方只看到 ECONNRESET、不知道真正的原因。所以先限量排空再关。
    let mut sink = [0u8; 8192];
    let mut drained = 0usize;
    let _ = tokio::time::timeout(Duration::from_millis(500), async {
        while let Ok(n) = stream.read(&mut sink).await {
            drained += n;
            if n == 0 || drained > 1024 * 1024 {
                break;
            }
        }
    })
    .await;
}

fn err_body(msg: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": msg.into() })
}

/// `McpReqResult` → `{"ok":true,"type":<变体名>,"data":<内容>}`（单元变体 data 为 null）。
fn result_body(r: &McpReqResult) -> serde_json::Value {
    let v = serde_json::to_value(r).unwrap_or(serde_json::Value::Null);
    match v {
        serde_json::Value::String(t) => serde_json::json!({ "ok": true, "type": t, "data": null }),
        serde_json::Value::Object(m) if m.len() == 1 => {
            let (t, d) = m.into_iter().next().unwrap();
            serde_json::json!({ "ok": true, "type": t, "data": d })
        }
        other => serde_json::json!({ "ok": true, "type": "Unknown", "data": other }),
    }
}

// ───────────────────────────── 请求 → 业务 ─────────────────────────────

/// 允许经 API 调用的操作 → `McpReqKind` 变体名。**白名单**：`*FromCaller` / `*ToCaller` 两个变体是
/// 代理与 GUI 之间的内部字节流协议，不对外；`copy_from_remote`（下载到本机文件）见文档里的说明。
const OPS: &[(&str, &str)] = &[
    ("list_sessions", "ListSessions"),
    ("list_saved_connections", "ListSavedConnections"),
    ("run_command", "RunCommand"),
    ("poll_run", "PollRun"),
    ("read_screen", "ReadScreen"),
    ("interrupt", "Interrupt"),
    ("open_session", "OpenSession"),
    ("close_session", "CloseSession"),
    ("read_history", "ReadHistory"),
    ("send_input", "SendInput"),
    ("write_file", "WriteFile"),
    ("read_file", "ReadFile"),
    ("copy_to_remote", "CopyToRemote"),
    ("copy_between_sessions", "CopyBetweenSessions"),
];

/// 无参数的操作：允许 GET。
const UNIT_OPS: &[&str] = &["list_sessions", "list_saved_connections"];

/// 与 MCP 工具一致的默认值（省略时补上）。
fn default_field(op: &str, field: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    match (op, field) {
        ("run_command" | "poll_run", "timeout_ms") => Some(json!(15_000)),
        ("write_file" | "read_file", "timeout_ms") => Some(json!(20_000)),
        ("copy_to_remote" | "copy_between_sessions", "timeout_ms") => Some(json!(300_000)),
        ("read_history", "max_lines") => Some(json!(200)),
        ("read_file", "force") => Some(json!(false)),
        _ => None,
    }
}

/// 把 `(操作名, JSON 参数)` 变成 `McpReqKind`。纯函数，便于测。
fn build_kind(op: &str, body: &[u8]) -> Result<McpReqKind, String> {
    // start_command = run_command 的最小等待窗口版（与 MCP 工具同义），之后用 poll_run 续等。
    let (op, forced_timeout) = if op == "start_command" {
        ("run_command", Some(100))
    } else {
        (op, None)
    };
    let Some(&(_, variant)) = OPS.iter().find(|(name, _)| *name == op) else {
        return Err(format!("未知操作 {op:?}"));
    };
    if UNIT_OPS.contains(&op) {
        return serde_json::from_value(serde_json::Value::String(variant.into()))
            .map_err(|e| e.to_string());
    }
    let mut args: serde_json::Map<String, serde_json::Value> = if body.iter().all(u8::is_ascii_whitespace) {
        serde_json::Map::new()
    } else {
        match serde_json::from_slice(body) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => return Err("请求体必须是 JSON 对象".into()),
            Err(e) => return Err(format!("请求体不是合法 JSON：{e}")),
        }
    };
    for field in ["timeout_ms", "max_lines", "force"] {
        if !args.contains_key(field) {
            if let Some(d) = default_field(op, field) {
                args.insert(field.into(), d);
            }
        }
    }
    if let Some(t) = forced_timeout {
        args.insert("timeout_ms".into(), t.into());
    }
    // send_input 的 escapes：与 MCP 代理一致，在这一层解析，线上协议不变。
    if op == "send_input" {
        let escapes = args.remove("escapes").and_then(|v| v.as_bool()).unwrap_or(false);
        if escapes {
            if let Some(serde_json::Value::String(t)) = args.get("text") {
                let parsed = crate::mcp_protocol::unescape_key_text(t)?;
                args.insert("text".into(), parsed.into());
            }
        }
    }
    let mut tagged = serde_json::Map::new();
    tagged.insert(variant.into(), serde_json::Value::Object(args));
    serde_json::from_value(serde_json::Value::Object(tagged)).map_err(|e| format!("参数不合法：{e}"))
}

// ───────────────────────────── 连接处理 ─────────────────────────────

async fn handle_conn(mut stream: TcpStream, sh: Arc<Shared>, preauth: OwnedSemaphorePermit) {
    let _ = stream.set_nodelay(true);
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(h)) => h,
        Ok(Err(HeadError::Malformed)) => return respond(&mut stream, 400, &err_body("请求格式不对")).await,
        Ok(Err(HeadError::TooLarge)) => {
            return respond(&mut stream, 431, &err_body("请求头过大")).await;
        }
        Ok(Err(HeadError::Closed)) | Err(_) => return,
    };

    // —— 认证前的三道关：Host / Origin / 令牌。任何一关不过都不读请求体。——
    let (Ok(host), Ok(origin), Ok(auth)) = (
        head.header("host"),
        head.header("origin"),
        head.header("authorization"),
    ) else {
        return respond(&mut stream, 400, &err_body("重复的请求头")).await;
    };
    if !host.is_some_and(|h| host_allowed(h, sh.port)) {
        return respond(&mut stream, 403, &err_body("Host 不是本机回环地址")).await;
    }
    if origin.is_some() {
        // 浏览器发起的跨站请求必带 Origin；本机程序不会带。不给网页任何可乘之机。
        return respond(&mut stream, 403, &err_body("不接受浏览器来源的请求")).await;
    }
    if !auth.and_then(bearer).is_some_and(|t| ct_eq(t.as_bytes(), sh.token.as_bytes())) {
        return respond(&mut stream, 401, &err_body("缺少或错误的令牌（读 ~/.config/ishell/api.json）")).await;
    }
    drop(preauth); // 已认证：不再占认证前名额

    // —— 已认证。再看请求形态。——
    if head.header("transfer-encoding").ok().flatten().is_some() {
        return respond(&mut stream, 501, &err_body("不支持 Transfer-Encoding（分块传输），请用 Content-Length")).await;
    }
    // curl 对较大的 POST 体会带 `Expect: 100-continue` 并等一个临时应答再发体：认证已过，回 100 即可。
    let expect_continue = match head.header("expect") {
        Ok(None) => false,
        Ok(Some(v)) if v.eq_ignore_ascii_case("100-continue") => true,
        _ => return respond(&mut stream, 417, &err_body("不支持的 Expect")).await,
    };
    let path = head.path.split('?').next().unwrap_or("");
    let Some(op) = path.strip_prefix("/v1/") else {
        return respond(&mut stream, 404, &err_body("路径应为 /v1/<操作>")).await;
    };
    if op == "info" {
        if head.method != "GET" {
            return respond(&mut stream, 405, &err_body("info 只接受 GET")).await;
        }
        let body = serde_json::json!({ "ok": true, "type": "Info", "data": {
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "host": crate::mcp_protocol::local_hostname(),
        }});
        return respond(&mut stream, 200, &body).await;
    }
    let is_unit = UNIT_OPS.contains(&op);
    let is_download = op == "copy_from_remote";
    match head.method.as_str() {
        "POST" => {}
        "GET" if is_unit => {}
        _ => {
            return respond(&mut stream, 405, &err_body("操作用 POST（无参数的 list_* 也可用 GET）")).await;
        }
    }
    if !is_unit && !is_download && !OPS.iter().any(|(n, _)| *n == op) && op != "start_command" {
        return respond(&mut stream, 404, &err_body(format!("未知操作 {op:?}"))).await;
    }

    // actor
    let actor_name = match head.header("x-ishell-actor") {
        Ok(Some(n)) if valid_actor_name(n) => n.to_string(),
        _ => {
            return respond(
                &mut stream,
                400,
                &err_body("缺少或非法的 X-Ishell-Actor（1–64 个字符，只能是字母数字 . _ -）：这是你这个调用方的名字，决定哪些会话算“你开的”"),
            )
            .await;
        }
    };

    // 请求体
    let content_length = match head.header("content-length") {
        Ok(None) => 0usize,
        Ok(Some(v)) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => match v.parse::<usize>() {
            Ok(n) => n,
            Err(_) => return respond(&mut stream, 413, &err_body("Content-Length 过大")).await,
        },
        _ => return respond(&mut stream, 400, &err_body("Content-Length 不合法")).await,
    };
    if content_length > BODY_MAX {
        return respond(&mut stream, 413, &err_body("请求体超过 32 MiB（大文件请用 copy_to_remote）")).await;
    }
    if head.body_prefix.len() > content_length {
        return respond(&mut stream, 400, &err_body("不支持流水线请求")).await;
    }
    if content_length > 0 {
        let ct = head.header("content-type").ok().flatten().unwrap_or("");
        if !ct.to_ascii_lowercase().starts_with("application/json") {
            return respond(&mut stream, 415, &err_body("Content-Type 须为 application/json")).await;
        }
    }
    // 并发名额在读体**之前**就占上：读体最长 60 秒、最大 32 MiB，不占名额的话，持令牌者开一堆
    // “声明大体、不发数据”的连接就能无界地吃内存。体也按实际到达的字节增长，而不是按声明值预分配。
    let Ok(_work) = sh.work.clone().try_acquire_owned() else {
        return respond(&mut stream, 503, &err_body("并发请求已达上限，请稍后重试")).await;
    };
    let mut body = head.body_prefix;
    if body.len() < content_length {
        if expect_continue {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        let need = (content_length - body.len()) as u64;
        let read = tokio::time::timeout(BODY_TIMEOUT, (&mut stream).take(need).read_to_end(&mut body)).await;
        if !matches!(read, Ok(Ok(_))) || body.len() != content_length {
            return respond(&mut stream, 400, &err_body("请求体没有按 Content-Length 送完")).await;
        }
    }

    // 下载有自己的形状（字节流写本机文件），单独走，不经 `build_kind`。
    let download = if is_download {
        match parse_download(&body) {
            Ok(d) => Some(d),
            Err(m) => return respond(&mut stream, 400, &err_body(m)).await,
        }
    } else {
        None
    };
    let kind = match &download {
        Some(d) => McpReqKind::CopyFromRemoteToCaller {
            session_uid: d.session_uid,
            remote_path: d.remote_path.clone(),
            timeout_ms: d.timeout_ms,
        },
        None => match build_kind(op, &body) {
            Ok(k) => k,
            Err(m) => return respond(&mut stream, 400, &err_body(m)).await,
        },
    };
    // `list_sessions` 的 filter：POST 体里的 `filter` 或 GET 的 `?filter=`，与 MCP 的同名参数同义。
    let body_filter = (op == "list_sessions")
        .then(|| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .flatten()
        .and_then(|v| v.get("filter").and_then(|f| f.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty());
    drop(body);

    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
    let req = McpRequest {
        id,
        origin: Some(format!("API: {actor_name}")),
        // `api:` 前缀：与 MCP 代理的 actor（纯进程标识）永不相等，API 调用方碰不到 MCP agent 的会话。
        actor: Some(format!("{ACTOR_NS}{actor_name}")),
        kind,
    };
    if let Some(d) = download {
        let (code, body) = match download_to_local(&sh, req, d).await {
            Ok(path) => (200, serde_json::json!({ "ok": true, "type": "Copied", "data": { "path": path } })),
            Err(m) => (422, err_body(m)),
        };
        return respond(&mut stream, code, &body).await;
    }
    let filter = list_filter(op, &head.path).or(body_filter);
    let (resp_tx, mut resp_rx) = oneshot::channel();
    if sh.tx.send(McpCall::from_api(req, resp_tx)).is_err() {
        return respond(&mut stream, 503, &err_body("iShell 正在退出")).await;
    }
    sh.ctx.request_repaint();
    // 调用方中途断开 = 不再有人要结果：丢掉 resp_rx（App 侧据此回收挂起的 poll_run 等待者），
    // 并催一帧让它真的去看（空闲窗口不转帧，否则这个信号没人读）。与 MCP 通道同一处理。
    // 只有读到 EOF / 出错才算“走了”：对端多发的字节（流水线的下一条请求等）读掉丢弃，请求照常等结果——
    // 此刻命令已经排进队列，不该因为多余的字节就不给调用方应答。
    let mut probe = [0u8; 256];
    let resp = loop {
        tokio::select! {
            biased;
            r = &mut resp_rx => break r.unwrap_or(McpResponse {
                id,
                result: Err("iShell 未能处理该请求（可能已关闭）".into()),
            }),
            n = stream.read(&mut probe) => match n {
                Ok(0) | Err(_) => {
                    sh.ctx.request_repaint();
                    return;
                }
                Ok(_) => {}
            },
        }
    };
    match resp.result {
        Ok(mut r) => {
            if let (Some(f), McpReqResult::Sessions(list)) = (filter, &mut r) {
                let f = f.to_lowercase();
                list.retain(|s| s.title.to_lowercase().contains(&f) || s.host.to_lowercase().contains(&f));
            }
            respond(&mut stream, 200, &result_body(&r)).await
        }
        Err(m) => respond(&mut stream, 422, &err_body(m)).await,
    }
}

/// `copy_from_remote` 的参数。
struct DownloadArgs {
    session_uid: u64,
    remote_path: String,
    local_path: String,
    timeout_ms: u64,
}

fn parse_download(body: &[u8]) -> Result<DownloadArgs, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        session_uid: u64,
        remote_path: String,
        local_path: String,
        #[serde(default = "default_copy_timeout")]
        timeout_ms: u64,
    }
    fn default_copy_timeout() -> u64 {
        300_000
    }
    let raw: Raw = serde_json::from_slice(body).map_err(|e| format!("参数不合法：{e}"))?;
    // 与 MCP 代理对 local_path 的规则一致：调用方机器上的绝对路径、不含 . / .. 段、有文件名。
    super::mcp_bridge::validate_local_path(&raw.local_path)?;
    // 下载的目标必须是文件：以分隔符结尾的路径会让临时文件落进一个并不存在的“目录”里。
    if raw.local_path.ends_with(['/', '\\']) {
        return Err("local_path 必须是文件路径，不能以 / 结尾".into());
    }
    Ok(DownloadArgs {
        session_uid: raw.session_uid,
        remote_path: raw.remote_path,
        local_path: raw.local_path,
        timeout_ms: raw.timeout_ms,
    })
}

/// 把远端单文件下载到本机 `local_path`。事务写：先写同目录临时文件，iShell 给出“传输成功”的最终判定、
/// 且字节数对得上，才原子改名换入——下载中断绝不破坏已有的同名文件（与 MCP 代理同一规则）。
async fn download_to_local(sh: &Shared, req: McpRequest, d: DownloadArgs) -> Result<String, String> {
    let (resp_tx, resp_rx) = oneshot::channel();
    let (dl_tx, dl_rx) = oneshot::channel();
    sh.tx
        .send(McpCall::from_api_download(req, resp_tx, dl_tx))
        .map_err(|_| "iShell 正在退出".to_string())?;
    sh.ctx.request_repaint();
    // `dl_rx` 是权威结果；它被丢弃而未发送，只会发生在 App 层校验就失败（会话不存在/非本调用方的会话…），
    // 此时真正的错误在 `resp_rx` 里。
    let source = match dl_rx.await {
        Ok(Ok(s)) => s,
        Ok(Err(m)) => return Err(m),
        Err(_) => {
            return Err(match resp_rx.await {
                Ok(McpResponse { result: Err(m), .. }) => m,
                Ok(_) => "iShell 返回了意料之外的响应".into(),
                Err(_) => "iShell 未能处理该请求（可能已关闭）".into(),
            })
        }
    };
    let path = std::path::PathBuf::from(&d.local_path);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("无法创建目标目录 {}：{e}", parent.display()))?;
    }
    let tmp = std::path::PathBuf::from(format!(
        "{}.ishell-part-{}-{}",
        d.local_path,
        std::process::id(),
        sh.next_id.fetch_add(1, Ordering::Relaxed)
    ));
    let outcome: Result<(), String> = async {
        let mut dest = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| format!("无法创建临时下载文件 {}：{e}", tmp.display()))?;
        let mut reader = source.reader;
        let received = tokio::io::copy(&mut reader, &mut dest)
            .await
            .map_err(|e| format!("下载中断：{e}"))?;
        // 先拿 iShell 的最终判定再换入：字节数对得上不等于传输是对的（远端文件传输期间可能在增长）。
        match tokio::time::timeout(Duration::from_secs(30), source.outcome).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(m))) => return Err(m),
            Ok(Err(_)) => return Err("iShell 未能给出这次传输的最终判定（worker 已退出）".into()),
            Err(_) => return Err("iShell 未能在字节流发完后及时给出传输判定".into()),
        }
        if received != source.size {
            return Err(format!("字节数与声明的不符：应为 {}，实收 {received}", source.size));
        }
        dest.flush().await.map_err(|e| format!("落盘失败：{e}"))?;
        let _ = dest.sync_all().await;
        drop(dest);
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| format!("换入下载文件失败 {}：{e}", d.local_path))
    }
    .await;
    if let Err(e) = outcome {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(d.local_path)
}

/// `GET /v1/list_sessions?filter=xxx`：与 MCP 的 `filter` 参数同义（标题或主机名包含，不分大小写）。
fn list_filter(op: &str, raw_path: &str) -> Option<String> {
    if op != "list_sessions" {
        return None;
    }
    let q = raw_path.split_once('?')?.1;
    q.split('&')
        .find_map(|kv| kv.strip_prefix("filter="))
        .map(percent_decode)
        .filter(|s| !s.is_empty())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?'));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_protocol::McpSessionInfo;

    // —— 纯函数 ——

    #[test]
    fn host_must_be_loopback_with_our_port() {
        assert!(host_allowed("127.0.0.1:47621", 47621));
        assert!(host_allowed("LOCALHOST:47621", 47621));
        assert!(host_allowed("[::1]:47621", 47621));
        for bad in ["evil.example:47621", "127.0.0.1:1", "127.0.0.1", "127.0.0.1.evil.com:47621", "", "0.0.0.0:47621"] {
            assert!(!host_allowed(bad, 47621), "{bad:?}");
        }
    }

    #[test]
    fn bearer_and_ct_eq() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("abc"), None);
        assert!(ct_eq(b"abcd", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abce"));
        assert!(!ct_eq(b"abcd", b"abc"));
    }

    #[test]
    fn actor_names_are_restricted() {
        for ok in ["claude-code", "my.agent_1", "A"] {
            assert!(valid_actor_name(ok), "{ok}");
        }
        for bad in ["", "a b", "a/b", "名字", &"x".repeat(65)] {
            assert!(!valid_actor_name(bad), "{bad}");
        }
    }

    #[test]
    fn build_kind_applies_documented_defaults_and_rejects_the_rest() {
        let k = build_kind("run_command", br#"{"session_uid":3,"command":"ls"}"#).unwrap();
        assert!(matches!(k, McpReqKind::RunCommand { session_uid: 3, timeout_ms: 15_000, .. }));
        let k = build_kind("start_command", br#"{"session_uid":3,"command":"ls","timeout_ms":999999}"#).unwrap();
        assert!(matches!(k, McpReqKind::RunCommand { timeout_ms: 100, .. }), "start_command 固定最小窗口");
        let k = build_kind("read_history", br#"{"session_uid":1}"#).unwrap();
        assert!(matches!(k, McpReqKind::ReadHistory { max_lines: 200, .. }));
        assert!(matches!(build_kind("list_sessions", b"").unwrap(), McpReqKind::ListSessions));
        let k = build_kind("send_input", br#"{"session_uid":1,"text":"ls\\r","escapes":true}"#).unwrap();
        assert!(matches!(k, McpReqKind::SendInput { ref text, .. } if text == "ls\r"));
        // 缺必填字段 / 类型不对 / 非对象 / 未知操作 / 内部流变体
        assert!(build_kind("run_command", br#"{"command":"ls"}"#).is_err());
        assert!(build_kind("run_command", br#"{"session_uid":"x","command":"ls"}"#).is_err());
        assert!(build_kind("run_command", b"[1]").is_err());
        assert!(build_kind("run_command", b"{oops").is_err());
        assert!(build_kind("nope", b"{}").is_err());
        for internal in ["copy_to_remote_from_caller", "copy_from_remote_to_caller", "CopyToRemoteFromCaller"] {
            assert!(build_kind(internal, br#"{"session_uid":1,"remote_path":"/x","size":1,"timeout_ms":1}"#).is_err(), "{internal}");
        }
    }

    #[test]
    fn every_whitelisted_op_maps_to_a_real_variant() {
        // 变体改名/删除时这里先挂，而不是线上才报“参数不合法”。
        for (op, _) in OPS {
            let probe = match *op {
                "list_sessions" | "list_saved_connections" => String::new(),
                _ => "{}".into(),
            };
            let r = build_kind(op, probe.as_bytes());
            // 带必填字段的会因缺字段失败，但绝不能是“未知操作”
            if let Err(e) = r {
                assert!(!e.contains("未知操作"), "{op}: {e}");
                assert!(e.contains("参数不合法"), "{op}: {e}");
            }
        }
    }

    #[test]
    fn result_body_flattens_the_enum() {
        let b = result_body(&McpReqResult::Ok);
        assert_eq!(b["ok"], true);
        assert_eq!(b["type"], "Ok");
        let b = result_body(&McpReqResult::Screen("hi".into()));
        assert_eq!((b["type"].as_str(), b["data"].as_str()), (Some("Screen"), Some("hi")));
    }

    #[test]
    fn download_args_follow_the_proxy_path_rules() {
        let ok = br#"{"session_uid":1,"remote_path":"/etc/hosts","local_path":"/tmp/x/hosts"}"#;
        let d = parse_download(ok).unwrap();
        assert_eq!((d.session_uid, d.timeout_ms), (1, 300_000));
        for bad in [
            r#"{"session_uid":1,"remote_path":"/a","local_path":"rel/x"}"#,
            r#"{"session_uid":1,"remote_path":"/a","local_path":"/tmp/../etc/x"}"#,
            r#"{"session_uid":1,"remote_path":"/a","local_path":"/tmp/./x"}"#,
            r#"{"session_uid":1,"remote_path":"/a","local_path":"/"}"#,
            r#"{"session_uid":1,"remote_path":"/a","local_path":"/tmp/dir/"}"#,
            r#"{"session_uid":1,"remote_path":"/a"}"#,
        ] {
            assert!(parse_download(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn filter_parsing() {
        assert_eq!(list_filter("list_sessions", "/v1/list_sessions?filter=e5%2D1+x"), Some("e5-1 x".into()));
        assert_eq!(list_filter("list_sessions", "/v1/list_sessions"), None);
        assert_eq!(list_filter("run_command", "/v1/run_command?filter=a"), None);
    }

    // —— 真 socket：服务器 + 一个假的“App”回应 McpCall ——

    const TOKEN: &str = "t0ken";

    /// 起一个监听在随机端口的服务，返回 (端口, 收到的请求记录)。假 App 对 list_sessions 回两条会话，
    /// 对 run_command 回 Err("boom")，其余回 Ok。
    async fn start() -> (u16, Arc<std::sync::Mutex<Vec<McpRequest>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::unbounded_channel::<McpCall>();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let (req, resp_tx) = call.into_parts();
                let id = req.id;
                let result = match &req.kind {
                    McpReqKind::ListSessions => Ok(McpReqResult::Sessions(vec![
                        sess(1, "alpha", "h1"),
                        sess(2, "beta", "h2"),
                    ])),
                    McpReqKind::RunCommand { .. } => Err("boom".to_string()),
                    _ => Ok(McpReqResult::Ok),
                };
                seen2.lock().unwrap().push(req);
                let _ = resp_tx.send(McpResponse { id, result });
            }
        });
        let shared = Arc::new(Shared {
            token: TOKEN.into(),
            port,
            tx,
            ctx: egui::Context::default(),
            next_id: AtomicU64::new(1),
            preauth: Arc::new(Semaphore::new(MAX_PREAUTH)),
            work: Arc::new(Semaphore::new(MAX_WORK)),
        });
        tokio::spawn(serve(listener, shared));
        (port, seen)
    }

    fn sess(uid: u64, title: &str, host: &str) -> McpSessionInfo {
        McpSessionInfo {
            uid,
            title: title.into(),
            host: host.into(),
            connected: true,
            cwd: None,
            ai_owned: true,
            ai_owner: None,
            mine: true,
            bound: false,
        }
    }

    /// 发一段原始字节，读到连接关闭，返回 (状态码, 响应体 JSON)。
    async fn raw(port: u16, bytes: &[u8]) -> (u16, serde_json::Value) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(bytes).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        let text = String::from_utf8_lossy(&out).into_owned();
        let code = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
        (code, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
    }

    fn req(port: u16, method: &str, path: &str, extra: &str, body: &str) -> Vec<u8> {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn authed(port: u16, method: &str, path: &str, body: &str) -> Vec<u8> {
        req(port, method, path, &format!("Authorization: Bearer {TOKEN}\r\nX-Ishell-Actor: tester\r\n"), body)
    }

    #[tokio::test]
    async fn happy_path_and_actor_namespacing() {
        let (port, seen) = start().await;
        let (code, b) = raw(port, &authed(port, "GET", "/v1/list_sessions", "")).await;
        assert_eq!(code, 200, "{b}");
        assert_eq!(b["type"], "Sessions");
        assert_eq!(b["data"].as_array().unwrap().len(), 2);
        // 内部身份带 api: 前缀，且 origin 供展示——MCP 代理的 actor 是纯进程标识，永不相等
        let r = seen.lock().unwrap()[0].clone();
        assert_eq!(r.actor.as_deref(), Some("api:tester"));
        assert_eq!(r.origin.as_deref(), Some("API: tester"));
        // filter
        let (_, b) = raw(port, &authed(port, "GET", "/v1/list_sessions?filter=ALP", "")).await;
        assert_eq!(b["data"].as_array().unwrap().len(), 1);
        // 业务错误 → 422 + ok:false
        let (code, b) = raw(port, &authed(port, "POST", "/v1/run_command", r#"{"session_uid":1,"command":"ls"}"#)).await;
        assert_eq!((code, b["ok"].clone(), b["error"].as_str()), (422, false.into(), Some("boom")));
        // info
        let (code, b) = raw(port, &authed(port, "GET", "/v1/info", "")).await;
        assert_eq!((code, b["type"].as_str()), (200, Some("Info")));
    }

    /// **门禁**：Host / Origin / 令牌任何一关不过都不能到业务层。
    /// 反向对照：去掉 `host_allowed` 或 Origin 检查，对应用例当场挂。
    #[tokio::test]
    async fn gates_reject_before_reaching_the_app() {
        let (port, seen) = start().await;
        let good = format!("Authorization: Bearer {TOKEN}\r\nX-Ishell-Actor: t\r\n");
        let cases: Vec<(&str, Vec<u8>, u16)> = vec![
            ("无令牌", req(port, "GET", "/v1/list_sessions", "X-Ishell-Actor: t\r\n", ""), 401),
            ("错令牌", req(port, "GET", "/v1/list_sessions", "Authorization: Bearer nope\r\nX-Ishell-Actor: t\r\n", ""), 401),
            ("令牌前缀对但更长", req(port, "GET", "/v1/list_sessions", &format!("Authorization: Bearer {TOKEN}x\r\nX-Ishell-Actor: t\r\n"), ""), 401),
            ("Basic 方案", req(port, "GET", "/v1/list_sessions", &format!("Authorization: Basic {TOKEN}\r\n"), ""), 401),
            ("带 Origin（浏览器跨站）", req(port, "GET", "/v1/list_sessions", &format!("{good}Origin: http://evil.example\r\n"), ""), 403),
            (
                "DNS 重绑定的 Host",
                format!("GET /v1/list_sessions HTTP/1.1\r\nHost: evil.example:{port}\r\n{good}\r\n").into_bytes(),
                403,
            ),
            ("缺 Host", format!("GET /v1/list_sessions HTTP/1.1\r\n{good}\r\n").into_bytes(), 403),
            ("重复的 Authorization", req(port, "GET", "/v1/list_sessions", &format!("{good}Authorization: Bearer {TOKEN}\r\n"), ""), 400),
            ("OPTIONS 预检", req(port, "OPTIONS", "/v1/list_sessions", &good, ""), 405),
            ("缺 actor", req(port, "GET", "/v1/list_sessions", &format!("Authorization: Bearer {TOKEN}\r\n"), ""), 400),
            ("非法 actor", req(port, "GET", "/v1/list_sessions", &format!("Authorization: Bearer {TOKEN}\r\nX-Ishell-Actor: a b\r\n"), ""), 400),
            ("未知操作", authed(port, "POST", "/v1/format_disk", "{}"), 404),
            ("内部流变体不对外", authed(port, "POST", "/v1/copy_from_remote_to_caller", "{}"), 404),
            ("分块传输", req(port, "POST", "/v1/run_command", &format!("{good}Transfer-Encoding: chunked\r\n"), ""), 501),
            (
                "体过大",
                format!("POST /v1/run_command HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{good}Content-Type: application/json\r\nContent-Length: 99999999999\r\n\r\n").into_bytes(),
                413,
            ),
            ("重复的 Content-Length", req(port, "POST", "/v1/run_command", &format!("{good}Content-Length: 2\r\n"), "{}"), 400),
            ("非 JSON 类型", format!("POST /v1/run_command HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{good}Content-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}").into_bytes(), 415),
            ("HTTP/1.0", format!("GET /v1/list_sessions HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n{good}\r\n").into_bytes(), 400),
            ("垃圾", b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03".to_vec(), 400),
        ];
        for (name, bytes, want) in cases {
            let (code, _) = raw(port, &bytes).await;
            assert_eq!(code, want, "{name}");
        }
        assert!(seen.lock().unwrap().is_empty(), "所有被拒的请求都不该到达业务层：{:?}", seen.lock().unwrap().len());
    }

    /// 认证前不读请求体：令牌错 + 声明了很大的体，服务器立刻回 401 而不是等体。
    #[tokio::test]
    async fn rejects_before_reading_the_body() {
        let (port, _) = start().await;
        let started = std::time::Instant::now();
        let head = format!(
            "POST /v1/write_file HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer nope\r\nContent-Type: application/json\r\nContent-Length: 30000000\r\n\r\n"
        );
        let (code, _) = raw(port, head.as_bytes()).await; // 一个体字节都不发
        assert_eq!(code, 401);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn oversized_head_is_cut_off() {
        let (port, _) = start().await;
        let big = format!("GET /v1/info HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Junk: {}\r\n\r\n", "a".repeat(40_000));
        let (code, _) = raw(port, big.as_bytes()).await;
        assert_eq!(code, 431);
    }

    #[tokio::test]
    async fn body_split_across_packets_is_reassembled() {
        let (port, seen) = start().await;
        let full = authed(port, "POST", "/v1/send_input", r#"{"session_uid":9,"text":"hello"}"#);
        let split = full.len() - 10;
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&full[..split]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        s.write_all(&full[split..]).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200"));
        assert!(matches!(seen.lock().unwrap()[0].kind, McpReqKind::SendInput { session_uid: 9, .. }));
    }

    /// 调用方中途断开 → 丢掉 `resp_rx`，App 侧据此回收挂起的等待者（与 MCP 通道同一处理）。
    /// 反向对照：去掉 `select!` 里对 `stream.read` 的那一支，`is_closed` 永远等不到。
    #[tokio::test]
    async fn client_disconnect_releases_the_pending_waiter() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::unbounded_channel::<McpCall>();
        let shared = Arc::new(Shared {
            token: TOKEN.into(),
            port,
            tx,
            ctx: egui::Context::default(),
            next_id: AtomicU64::new(1),
            preauth: Arc::new(Semaphore::new(MAX_PREAUTH)),
            work: Arc::new(Semaphore::new(MAX_WORK)),
        });
        tokio::spawn(serve(listener, shared));
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&authed(port, "POST", "/v1/poll_run", r#"{"session_uid":1}"#)).await.unwrap();
        let call = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        let (_req, resp_tx) = call.into_parts(); // App 不回应，模拟一个还在等的 poll_run
        assert!(!resp_tx.is_closed());
        drop(s); // 调用方走了
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !resp_tx.is_closed() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(resp_tx.is_closed(), "调用方断开后，服务器应丢掉响应接收端");
    }

    #[tokio::test]
    async fn expect_continue_and_early_rejection_with_unread_body() {
        let (port, seen) = start().await;
        // 1) Expect: 100-continue：认证过了先回 100，再收体、回 200
        let body = r#"{"session_uid":9,"text":"x"}"#;
        let head = format!(
            "POST /v1/send_input HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nX-Ishell-Actor: t\r\nContent-Type: application/json\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(head.as_bytes()).await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 100"), "应先回 100 Continue");
        s.write_all(body.as_bytes()).await.unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).await.unwrap();
        assert!(String::from_utf8_lossy(&rest).starts_with("HTTP/1.1 200"));
        assert_eq!(seen.lock().unwrap().len(), 1);
        // 其它 Expect 值 → 417
        let bad = format!(
            "POST /v1/send_input HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nX-Ishell-Actor: t\r\nContent-Type: application/json\r\nExpect: something-else\r\nContent-Length: 2\r\n\r\n{{}}"
        );
        let (code, _) = raw(port, bad.as_bytes()).await;
        assert_eq!(code, 417);
        // 2) 带着大体、令牌错：应读到 401 而不是被 RST 冲掉
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let big = vec![b'a'; 200_000];
        let head = format!(
            "POST /v1/write_file HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer nope\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            big.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        let _ = s.write_all(&big).await; // 服务器可能提前读完关闭，写失败不算
        tokio::time::sleep(Duration::from_millis(300)).await; // 让（可能的）RST 先到，再读响应
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 401"), "拿到的是：{:?}", String::from_utf8_lossy(&out[..out.len().min(80)]));
    }

    /// 请求体发完之后对端再多发字节：请求已经排进队列，应把多余字节读掉丢弃、照常等结果并应答，
    /// 而不是当成“调用方走了”。反向对照：把探测改回“任何读结果都算断开”，本条当场挂。
    #[tokio::test]
    async fn extra_bytes_while_waiting_do_not_cancel_the_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::unbounded_channel::<McpCall>();
        let shared = Arc::new(Shared {
            token: TOKEN.into(),
            port,
            tx,
            ctx: egui::Context::default(),
            next_id: AtomicU64::new(1),
            preauth: Arc::new(Semaphore::new(MAX_PREAUTH)),
            work: Arc::new(Semaphore::new(MAX_WORK)),
        });
        tokio::spawn(serve(listener, shared));
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&authed(port, "POST", "/v1/send_input", r#"{"session_uid":9,"text":"x"}"#)).await.unwrap();
        let call = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        let (req, resp_tx) = call.into_parts(); // App 还没答
        s.write_all(b"junk-after-request").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!resp_tx.is_closed(), "多余字节不该让服务器放弃这条请求");
        let _ = resp_tx.send(McpResponse { id: req.id, result: Ok(McpReqResult::Ok) });
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn pipelined_requests_are_refused() {
        let (port, seen) = start().await;
        let mut full = authed(port, "POST", "/v1/send_input", r#"{"session_uid":9,"text":"x"}"#);
        full.extend_from_slice(b"GET /junk HTTP/1.1\r\n\r\n");
        let (code, _) = raw(port, &full).await;
        assert_eq!(code, 400);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_sessions_filter_works_from_a_post_body_too() {
        let (port, _) = start().await;
        let (code, b) = raw(port, &authed(port, "POST", "/v1/list_sessions", r#"{"filter":"BET"}"#)).await;
        assert_eq!(code, 200);
        assert_eq!(b["data"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn mcp_handshake_cannot_claim_the_api_namespace() {
        use crate::app::mcp_bridge::actor_acceptable;
        assert!(actor_acceptable("12345-abcdef"));
        for bad in ["", "api:claude-code", "api:", &"x".repeat(129)] {
            assert!(!actor_acceptable(bad), "{bad:?}");
        }
    }
}
