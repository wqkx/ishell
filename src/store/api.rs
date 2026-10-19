//! 本机 API（HTTP+JSON，只监听回环）的开关、运行状态与发现文件。
//!
//! 与 MCP 的开关（`load_mcp_consent`）**相互独立**：MCP 经 SSH 反向转发给服务器上的 agent，靠进程祖先
//! 认主人；本机 API 给的是桌面上的本地程序，没有祖先可认，门只有一把令牌——所以单独授权、默认关。
//!
//! 发现文件 `api.json`（`~/.config/ishell/`，Unix 上 0600）写着 `url` 与本次开启时随机生成的 `token`；
//! 调用方读它、**不扫描、不猜端口**。只有一个实例能占着这个端口：后来者绑定失败就明说，不换端口。

use std::path::PathBuf;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::Mutex;

use super::paths::config_dir;
use super::settings::write_setting;

/// 默认端口。固定值而不是随机：它同时就是“谁拥有 API”的唯一答案（多开时只有先到者占得到）。
/// `ISHELL_API_PORT` 可显式改（测试、端口冲突时用）——是显式配置，不是失败后的自动退让。
pub const API_DEFAULT_PORT: u16 = 47621;

pub fn api_port() -> u16 {
    std::env::var("ISHELL_API_PORT")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(API_DEFAULT_PORT)
}

fn api_enabled_path() -> Option<PathBuf> {
    Some(config_dir()?.join("local_api"))
}

/// 发现文件路径。
pub fn api_info_path() -> Option<PathBuf> {
    Some(config_dir()?.join("api.json"))
}

/// 进程内缓存：`-1` 未读盘，`0/1` 已知（帧循环每帧都要读，不能每次读文件）。
static API_ENABLED: AtomicI8 = AtomicI8::new(-1);

/// 本机 API 是否开启。**默认关**；只有用户在左侧面板点开才写 `1`。
pub fn load_api_enabled() -> bool {
    match API_ENABLED.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = api_enabled_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|s| s.trim() == "1");
            API_ENABLED.store(on as i8, Ordering::Relaxed);
            on
        }
    }
}

pub fn save_api_enabled(on: bool) {
    API_ENABLED.store(on as i8, Ordering::Relaxed);
    if let Some(p) = api_enabled_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, if on { "1" } else { "0" });
    }
}

/// 服务的运行状态（UI 显示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiState {
    Off,
    Starting,
    Listening { port: u16 },
    /// 起不来（端口被占、写不了发现文件…）：给人看的原因。
    Failed(String),
}

static API_STATE: Mutex<ApiState> = Mutex::new(ApiState::Off);

pub fn api_state() -> ApiState {
    API_STATE.lock().map(|s| s.clone()).unwrap_or(ApiState::Off)
}

pub fn set_api_state(s: ApiState) {
    if let Ok(mut g) = API_STATE.lock() {
        *g = s;
    }
}

/// 写发现文件（Unix 上创建时即 0600，不存在“先宽后窄”的窗口）。
pub fn write_api_info(url: &str, token: &str) -> Result<(), String> {
    let path = api_info_path().ok_or("找不到配置目录")?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("无法创建 {}：{e}", d.display()))?;
    }
    let body = serde_json::json!({
        "url": url,
        "token": token,
        "pid": std::process::id(),
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string();
    let tmp = path.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("无法写发现文件 {}：{e}", path.display())
    })
}

/// 启动时清掉**崩溃/被杀后留下的**发现文件：里面是一枚早已作废的令牌，指向一个可能已被别的程序占用的端口，
/// 本地 agent 读到它会把旧令牌发给不该收的对象。只在写它的进程确已不存在时才删（Unix；其它平台不处理）。
pub fn remove_stale_api_info() {
    #[cfg(unix)]
    {
        let Some(path) = api_info_path() else { return };
        let pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("pid").and_then(|p| p.as_i64()));
        let Some(pid) = pid.filter(|p| *p > 1 && *p != std::process::id() as i64) else {
            return;
        };
        // SAFETY：kill(pid, 0) 只探测进程是否存在，不发信号。
        let gone = unsafe { libc::kill(pid as libc::pid_t, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 删发现文件（关闭服务/退出时）。只删**自己写的**：文件里的 pid 不是本进程就不动——
/// 否则后来者被占端口、起不来时，也会把先到者的发现文件删掉。
pub fn remove_api_info() {
    let Some(path) = api_info_path() else { return };
    let mine = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
        .is_some_and(|pid| pid == std::process::id() as u64);
    if mine {
        let _ = std::fs::remove_file(path);
    }
}
