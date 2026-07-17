//! iShell 的 AI/MCP 控制通道代理：Claude Code 等 AI 客户端按 stdio 方式 spawn 的独立小进程。
//! 只是把工具调用转发到本机正在运行的 iShell 主进程（经本地 Unix domain socket，一次连接
//! 一问一答），主进程再落到它已经持有的 SSH 会话上执行。
//!
//! 唯一的进程状态是**绑定**：这个代理一辈子只操作一个 iShell 实例（见 `BOUND_INSTANCE`）。
//! 用户可能同时开着多个 iShell——本机的、以及别的机器上反向转发过来的——而每个实例的会话
//! uid 都从 1 开始，所以「这次调用发给谁」绝不能靠猜。首次连接时定下实例，此后每条请求都
//! 点名，由对端自己校验（`McpRequest::instance`）。选哪个实例由用户当场点窗口决定，不靠
//! 配置文件里的名字，也不靠 socket 文件的 mtime。
//! 与主二进制共享同一份线协议类型（见 src/mcp_protocol.rs），这里用 #[path] 直接纳入，
//! 避免为共享几个 struct 拆出独立的 lib crate。

// 这一份是「代理侧」的编译产物：同一个文件也被主二进制编一遍，两边用到的子集不同——
// 授权门禁（`write_target_uids`）和实例校验（`is_addressed_to`）都只在 GUI 那侧执行，
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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;

use mcp_protocol::{McpReqKind, McpReqResult, McpRequest, McpResponse};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 本代理进程绑定的 iShell 实例标识。一经确定，进程生命周期内永不改变。
///
/// 「不改绑」是这里最重要的性质，不是优化：每个 iShell 实例的会话 uid 都从 1 开始
/// （见 `src/app/session.rs`），所以中途换一个实例执行，`session_uid=1` 会安静地落到
/// 另一台机器上，不报任何错。此前按 socket 文件 mtime 挑实例、且**每次调用都重挑**，
/// 正是这个 bug：用户在对话中途新开一个 iShell，后续调用就跟着跑了。
#[cfg(unix)]
static BOUND_INSTANCE: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// 绑定实例当前的 socket 路径缓存。**路径会变，实例不会**：反向转发的 socket 每次 SSH
/// 重连都换一个随机名（见 `src/ssh/mod.rs`，固定名字会被服务器当成尚未失效的旧注册而
/// 拒绝）。所以路径只是缓存，失效了就按实例标识重新找回来。
#[cfg(unix)]
static PATH_CACHE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

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

/// 连一条 socket 问出对端 iShell 的实例标识。连不上、对面不是 iShell、超时——一律返回
/// `None`：目录里躺着崩溃残留的死 socket 文件是常态，不值得报错，跳过就是了。
#[cfg(unix)]
async fn identify(path: &std::path::Path) -> Option<String> {
    let stream = tokio::time::timeout(CONNECT_WRITE_TIMEOUT, UnixStream::connect(path))
        .await
        .ok()?
        .ok()?;
    match exchange(stream, None, McpReqKind::Identify, CONNECT_WRITE_TIMEOUT).await {
        Ok(McpReqResult::Instance { id }) => Some(id),
        _ => None,
    }
}

/// 决定这个代理这辈子只操作哪个 iShell 实例。只在首次需要连接时跑一次。
#[cfg(unix)]
async fn bind_instance() -> Result<(String, std::path::PathBuf), String> {
    // 显式指定：脚本化/手动隧道场景的逃生口。最明确的意图，永远优先，也不弹任何窗。
    if let Some(p) = std::env::var_os("ISHELL_MCP_SOCKET") {
        let path = std::path::PathBuf::from(p);
        let id = identify(&path).await.ok_or_else(|| {
            format!(
                "ISHELL_MCP_SOCKET 指定的 socket 连不上、或对面不是 iShell：{}",
                path.display()
            )
        })?;
        return Ok((id, path));
    }
    let mut found: Vec<(String, std::path::PathBuf)> = Vec::new();
    for path in candidate_paths() {
        if let Some(id) = identify(&path).await {
            // 按实例去重：多条路径可能通向同一个 iShell（见 candidate_paths 的说明）。
            if !found.iter().any(|(known, _)| *known == id) {
                found.push((id, path));
            }
        }
    }
    match found.len() {
        0 => Err(
            "连不上 iShell（未运行，或未在设置里开启「允许 AI 通过 MCP 控制终端」）".into(),
        ),
        1 => Ok(found.pop().expect("上一行刚确认只有一个")),
        _ => choose_instance(found).await,
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
    found: Vec<(String, std::path::PathBuf)>,
) -> Result<(String, std::path::PathBuf), String> {
    let count = found.len();
    let mut set = tokio::task::JoinSet::new();
    for (id, path) in found {
        set.spawn(async move {
            let stream = UnixStream::connect(&path).await.map_err(|e| e.to_string())?;
            exchange(stream, Some(id.clone()), McpReqKind::Bind, BIND_TIMEOUT).await?;
            Ok::<_, String>((id, path))
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok(Ok(winner)) = joined {
            return Ok(winner); // set 在这里被丢弃 → 其余任务 abort → 落选窗口的框自动消失
        }
    }
    Err(format!(
        "本机同时开着 {count} 个 iShell，但没有任何一个窗口批准这次连接（用户拒绝，或 5 分钟\
         没有响应）。请让用户在他想让你操作的那个 iShell 窗口上点「允许」，然后重试。"
    ))
}

/// 拿一条连向**绑定实例**的连接，外加要填进请求的实例标识。
#[cfg(unix)]
async fn connect_bound() -> Result<(UnixStream, String), String> {
    let id = BOUND_INSTANCE
        .get_or_try_init(|| async {
            let (id, path) = bind_instance().await?;
            *PATH_CACHE.lock().unwrap() = Some(path);
            Ok::<_, String>(id)
        })
        .await?
        .clone();
    let cached = PATH_CACHE.lock().unwrap().clone();
    if let Some(path) = cached {
        if let Ok(stream) = UnixStream::connect(&path).await {
            return Ok((stream, id));
        }
    }
    // 缓存路径连不上了：多半是反向转发那条 SSH 重连、换了随机名。在候选里重新找回**同一个
    // 实例**——只认 id，绝不因为「反正只剩这一个连得上」就顺手绑到别人身上。
    for path in candidate_paths() {
        if identify(&path).await.as_deref() == Some(id.as_str()) {
            if let Ok(stream) = UnixStream::connect(&path).await {
                *PATH_CACHE.lock().unwrap() = Some(path);
                return Ok((stream, id));
            }
        }
    }
    Err("找不到当初绑定的那个 iShell 实例了（它可能已经退出）。代理不会自动改绑到别的实例——\
         多开时静默换一个实例执行，命令就会落到你没预期的机器上。请重新发起 MCP 连接。"
        .into())
}

/// 在一条已建立的连接上完成一问一答：写一行请求 JSON，读一行响应 JSON。
///
/// `instance` 点名这条请求发给谁，由对端自己校验（见 `McpRequest::is_addressed_to`）。
/// 只有 `Identify` 填 `None`——那时还不知道对面是谁。
#[cfg(unix)]
async fn exchange(
    stream: UnixStream,
    instance: Option<String>,
    kind: McpReqKind,
    response_timeout: std::time::Duration,
) -> Result<McpReqResult, String> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&McpRequest { id, instance, kind })
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
    let (stream, instance) = connect_bound().await?;
    exchange(stream, Some(instance), kind, RESPONSE_TIMEOUT).await
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
    let (stream, instance) = connect_bound().await?;
    let (read_half, mut write_half) = stream.into_split();
    let request = McpRequest {
        id,
        instance: Some(instance),
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
    tokio::io::copy(&mut source, &mut write_half)
        .await
        .map_err(|error| format!("发送调用方文件流失败: {error}"))?;
    write_half
        .shutdown()
        .await
        .map_err(|error| format!("结束调用方文件流失败: {error}"))?;

    let mut response = String::new();
    let mut reader = BufReader::new(read_half);
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(&mut response))
        .await
        .map_err(|_| "等待 iShell 上传响应超时（可能是 GUI、SFTP 或连接异常）".to_string())?
        .map_err(|error| error.to_string())?;
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
    let (stream, instance) = connect_bound().await?;
    let (read_half, mut write_half) = stream.into_split();
    let request = McpRequest {
        id,
        instance: Some(instance),
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
        // 精确拷贝声明的 size 字节：这条协议里响应头之后不再有分隔符/trailer，只能用长度
        // 而不是 EOF 来判断"这个文件读完了"——`take(size)` 保证既不少读也不多读。
        let mut limited = (&mut reader).take(size);
        let copied = tokio::io::copy(&mut limited, &mut dest)
            .await
            .map_err(|error| format!("接收调用方文件流失败: {error}"))?;
        if copied != size {
            return Err(format!(
                "下载数据不完整：期望 {size} 字节，实际收到 {copied} 字节（iShell 一侧可能中途出错）"
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

/// 启动后立即返回的命令参数。它复用 `run_command` 的哨兵和 `poll_run` 状态机，
/// 只是把等待窗口固定为协议允许的最小值，避免 MCP 客户端的空闲超时占住等待者。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StartCommandArgs {
    pub session_uid: u64,
    pub command: String,
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

fn default_copy_timeout_ms() -> u64 {
    300_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CopyToRemoteArgs {
    /// list_sessions 返回的会话 uid
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
    /// list_sessions 返回的会话 uid
    pub session_uid: u64,
    /// 远端绝对路径（文件或目录）
    pub remote_path: String,
    /// 本地目标绝对路径，文件名可以和 remote_path 不同；所在目录不存在会自动创建
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
        description = "列出 iShell 当前打开的所有终端会话（uid、标题、主机、连接状态、远端工作目录、\
                        是否为 AI 自己开的会话）。\
                        注意 ai_owned 字段：true = 你自己用 open_session 开的专用会话，只读给用户看、\
                        用户的键盘输入不会进去，你可以随便用；false = **用户本人正在用的会话**，他随时\
                        可能在里面敲字。默认不要往 ai_owned=false 的会话里写（run_command / send_input / \
                        interrupt / write_file / copy_to_remote 等）——两路输入会在同一个 shell 里交织，\
                        轻则互相打断、重则把 run_command 判断命令结束用的哨兵标记搅乱，还可能误操作用户\
                        正在做的事。需要执行东西就用 open_session 开一个自己的会话。确实必须用用户那个\
                        会话时（比如要复用他已经 cd 到的目录、已经激活的 venv、已经 sudo 的状态），调用\
                        会照常发出，但 iShell 会弹窗让用户当面授权一次，用户同意后该会话不再询问。"
    )]
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
                        或者改用 send_input 应对交互式场景。\
                        两个解读输出时容易踩的坑：① output 末尾常带一段 shell 提示符残留（比如 \
                        `(venv) user@host:~$`，有时只剩一个 `$`）——这是刻意不做的清理（早期试过按\
                        「最后一行大概率是提示符」启发式剥掉，但 PS1 为空/不可见时会把真实输出误删，\
                        权衡后选择宁可留一点噪声也不丢数据），解析时自己按需忽略即可；② 超时返回的是\
                        finished=false 加**这一轮已产生的部分输出**（可能是空字符串）——空输出不代表\
                        命令什么都没打印，只代表还没等到完成哨兵，用 poll_run 续等或用 read_screen \
                        看实时内容。"
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
        description = "启动一条可能很长的命令，并在至多 100ms 后返回，不会因 MCP 客户端的长时间\
                        空闲限制而占住等待连接。返回 finished=false 时保存 run_id，之后用 poll_run\
                        以较短 timeout_ms 查询；命令已很快结束时会直接返回 finished=true。"
    )]
    async fn start_command(
        &self,
        Parameters(StartCommandArgs { session_uid, command }): Parameters<StartCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
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
                        interrupt 才能恢复。）"
    )]
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
        text_result(copy_to_remote_from_caller(session_uid, local_path, remote_path, timeout_ms).await)
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
        text_result(copy_from_remote_to_caller(session_uid, remote_path, local_path, timeout_ms).await)
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
    instructions = "涉及“在远端服务器上执行命令/查看文件/运行程序”这类需求时，优先用这个\
                    工具集操作 iShell 已打开的终端会话，而不是自己直接跑 `ssh host cmd`——\
                    直接开 ssh 会丢失用户已经建立的会话上下文（cwd、环境变量、shell 历史、\
                    已登录状态），也不会显示给用户看。只有确认 iShell 没在跑、或用户明确要求\
                    你自己开一条独立 ssh 连接时，才退回直接用 ssh。\
                    用法：list_sessions 看有哪些已打开的会话，重点看 ai_owned 字段 → **默认给自己\
                    开一个专用会话**，而不是接管列表里现成的：用 list_saved_connections 核对有哪些\
                    已保存连接、名字怎么拼，再用 open_session（首次使用某条连接会让用户当面确认）\
                    新开一个（这类会话 ai_owned=true，只读，仅供你操作，用户不能往里打字，你怎么\
                    折腾都不会干扰到他）→ run_command 执行命令并等待完成 → 超时用 poll_run 续等\
                    （不会重发命令）→ 遇到 run_command/poll_run 覆盖不到的交互式提示（sudo 密码、\
                    vim/REPL 里继续输入）用 send_input 直接发原始按键 → read_screen 看当前屏幕\
                    （适合 vim/top 等交互式程序）→ read_history 看这个会话从头到现在的完整历史\
                    （不止当前一屏）→ interrupt 发 Ctrl+C 中断。用 open_session 开的会话不再需要\
                    时应主动用 close_session 关掉（只能关自己开的，不能关用户自己的会话），\
                    避免一直占着连接。\
                    不要直接拿用户自己打开的会话（list_sessions 里 ai_owned=false 的那些）执行命令\
                    或写文件：他随时可能正在里面敲字，两路输入会在同一个 shell 里交织，轻则互相\
                    打断、重则把 run_command 用来判断命令结束的哨兵标记搅乱，也容易误操作他正在\
                    做的事。确实必须复用他那个会话的上下文时（比如要用他已经 cd 到的目录、已经\
                    激活的 venv、已经 sudo 的状态），照常发调用即可——iShell 会弹窗让用户当面授权\
                    一次，同意后这个会话不再询问；用户拒绝或 5 分钟没响应你会收到报错，那就改用 \
                    open_session 开自己的会话。只读类操作（read_screen/read_history/read_file）\
                    不受此限，随时可以用来看用户会话里发生了什么。\
                    需要同步/生成远端文件时用 write_file/read_file（复用 SFTP，不用另开 scp）。\
                    长任务不要用 `sleep N` 反复轮询——run_command/poll_run 的 timeout_ms 最长支持\
                    24 小时，直接传一个足够长的值（比如几十分钟），一次调用等到跑完更省事。\
                    如果确实想先把任务丢到后台、之后再回来看结果（比如中途还要做别的事），\
                    不要写 `sleep N; tail log` 这种盲等轮询，改用“等进程退出”的写法一步等到位：\
                    `nohup cmd > out.log 2>&1 & echo $! > pid; ...`启动后，之后另开一次\
                    run_command 执行 `tail --pid=$(cat pid) -f /dev/null; cat out.log`，\
                    这一条命令会一直阻塞到目标进程真正退出才返回，配合足够大的 timeout_ms 用，\
                    不需要猜测任务要跑多久、也不需要一遍遍轮询。注意 `&` 的优先级低于 `&&`：\
                    写成 `md5sum f && nohup cmd &` 会把整条 `&&` 链一起丢进后台，md5sum \
                    这半段的输出/退出码是否已经落地就变得不确定；要后台的只应该是 `nohup` \
                    这一条命令本身，前面需要立刻确认结果的步骤请用 `;` 分成独立语句，或拆成两次\
                    run_command 调用确认。\
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
