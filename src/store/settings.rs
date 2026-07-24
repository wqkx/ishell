use std::path::PathBuf;

use super::paths::config_dir;

/// 设置写入失败的消息队列。这些 setter 是 fire-and-forget、拿不到 UI 句柄，故把失败冒泡进一个
/// 全局队列，由 App 每帧 `take_setting_write_errors()` 取走弹顶部 toast——避免「UI 显示已切换、
/// 实际没落盘（磁盘满/只读/权限）、重启又变回旧值」这种毫无反馈的静默失败。
static WRITE_ERRORS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 取走并清空累积的设置写入错误（App 帧循环调用，转成顶部 toast）。
pub fn take_setting_write_errors() -> Vec<String> {
    WRITE_ERRORS
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// 写一个设置文件。失败不再静默吞掉：既 `log::warn!`，也冒泡进 `WRITE_ERRORS` 让 UI 弹提示。
fn write_setting(path: impl AsRef<std::path::Path>, data: impl AsRef<[u8]>) {
    let path = path.as_ref();
    if let Err(e) = std::fs::write(path, data) {
        log::warn!("写入设置失败 {}：{e}", path.display());
        let msg = match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("⚠ 设置未能保存（{}）：{e}", path.display()),
            crate::i18n::Lang::En => format!("⚠ Failed to save setting ({}): {e}", path.display()),
        };
        if let Ok(mut v) = WRITE_ERRORS.lock() {
            if v.len() < 16 {
                // 兜底防无限堆积（磁盘持续满 + 用户反复点开关）
                v.push(msg);
            }
        }
    }
}

// ---------- 默认下载目录设置 ----------

fn download_dir_path() -> Option<PathBuf> {
    Some(config_dir()?.join("download_dir"))
}

/// 读取用户设置的默认下载目录（未设置则 None）。
pub fn load_download_dir() -> Option<String> {
    let p = download_dir_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 保存默认下载目录。
pub fn save_download_dir(dir: &str) {
    if let Some(p) = download_dir_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, dir);
    }
}

// ---------- 语言设置 ----------

fn lang_path() -> Option<PathBuf> {
    Some(config_dir()?.join("lang"))
}

/// 读取语言代码（"zh"/"en"），未设置则 None。
pub fn load_lang() -> Option<String> {
    let p = lang_path()?;
    let s = std::fs::read_to_string(p).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 保存语言代码。
pub fn save_lang(code: &str) {
    if let Some(p) = lang_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, code);
    }
}

// ---------- 强制 X11 后端（修复 Wayland 下输入法失效） ----------

fn force_x11_path() -> Option<PathBuf> {
    Some(config_dir()?.join("force_x11"))
}

/// 是否强制走 X11（XWayland）：Wayland 下 winit 类应用 fcitx 输入法常失效，
/// 清空 WAYLAND_DISPLAY 改走 X11 的 XIM 可修复。文件内容 "1" 为开启。
pub fn load_force_x11() -> bool {
    force_x11_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// 保存「强制 X11」开关（下次启动生效）。
pub fn save_force_x11(on: bool) {
    if let Some(p) = force_x11_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, if on { "1" } else { "0" });
    }
}

fn osc7_consent_path() -> Option<PathBuf> {
    Some(config_dir()?.join("osc7_consent"))
}

/// 是否已同意「自动向 shell 注入 OSC 7 上报」（同意一次后续静默注入）。
pub fn load_osc7_consent() -> bool {
    osc7_consent_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// 保存 OSC 7 注入同意标志。
pub fn save_osc7_consent(on: bool) {
    if let Some(p) = osc7_consent_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, if on { "1" } else { "0" });
    }
}

fn mcp_consent_path() -> Option<PathBuf> {
    Some(config_dir()?.join("mcp_consent"))
}

/// 是否已开启「允许 AI 通过本地 MCP server 控制终端」（默认关闭，需用户在设置里显式开启）。
/// AI/MCP 控制开关的进程内缓存：`-1` 未知（还没读过盘），`0`/`1` 已知值。
/// 引入缓存的原因：帧循环要每帧读一次 consent 来决定是否维持 MCP 响应性重绘节奏
///（见 `frame.rs`），而原实现每次 `load_mcp_consent()` 都同步读文件——每帧一次磁盘 I/O
/// 不可接受。开关只经本进程的 `save_mcp_consent` 变更（UI 里唯一入口），故进程内缓存与磁盘
/// 天然一致，无需担心外部改文件不被感知。
static MCP_CONSENT_CACHE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

pub fn load_mcp_consent() -> bool {
    use std::sync::atomic::Ordering;
    match MCP_CONSENT_CACHE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let on = mcp_consent_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.trim() == "1")
                .unwrap_or(false);
            MCP_CONSENT_CACHE.store(on as i8, Ordering::Relaxed);
            on
        }
    }
}

/// 保存 AI/MCP 控制开关。
pub fn save_mcp_consent(on: bool) {
    // 先更新缓存，保证同帧内后续 `load_mcp_consent()` 立即反映新值（即便写盘失败也以用户
    // 意图为准；写盘失败会经 `write_setting` 冒泡 toast 提示）。
    MCP_CONSENT_CACHE.store(on as i8, std::sync::atomic::Ordering::Relaxed);
    if let Some(p) = mcp_consent_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, if on { "1" } else { "0" });
    }
}

/// 本进程的 AI/MCP 实例标识：`<pid>-<8 位随机十六进制>`，进程内全局唯一且固定。
///
/// 为什么不是纯 pid：pid 会被系统回收。iShell 退出后若残留了 socket 文件（崩溃、或反向
/// 转发那侧还没来得及清），而后来某个新 iShell 恰好拿到同一个 pid，代理就会认为「我绑定
/// 的那个实例还在」并接着往下发命令——实际上换了一个毫不相干的实例，且不会有任何报错。
/// 随机后缀让这种撞车不可能发生。
///
/// 为什么不持久化到磁盘：绑定关系活在代理进程的内存里，随 AI 客户端一起生灭，跨 iShell
/// 重启保持稳定没有意义；反倒是持久化会让「同机多开」的两个实例共用一个 id，直接摧毁隔离。
/// 这个值只需要在本进程活着期间唯一。
pub fn mcp_instance_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let mut buf = [0u8; 4];
        // 熵源不可用（极罕见）时退化到纳秒时间戳：仍能保证同机多开的几个实例互不相同，
        // 这正是这段后缀要解决的问题，没必要为此让 iShell 起不来。
        if getrandom::getrandom(&mut buf).is_err() {
            let ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            buf = ns.to_le_bytes();
        }
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}-{}", std::process::id(), hex)
    })
}

/// 本进程独占的 AI/MCP 控制 socket 路径。文件名带上实例标识，这样同机多开的几个 iShell
/// 各自监听各自的 socket，互不覆盖；代理进程按 `mcp-*.sock` 通配即可枚举出本机所有实例。
///
/// 为什么不是全实例共享一个固定文件名：新实例启动时 remove_file+bind 会顶掉旧实例已经绑定
/// 的 listener——删文件并不会让旧 listener 停止接受连接，但此后所有连到这个路径的请求都会
/// 进新实例，导致旧实例的 SSH 反向转发 MCP 通道被错误地路由到新实例、操作到旧实例都不认识
/// 的会话。每进程一个独立路径从根本上消除这个冲突，不需要任何「探测占用」之类的运行时逻辑。
///
/// 注意路径只是**发现**用的候选，不是身份：认人靠 `McpReqKind::Identify` 问出来的实例标识
/// （见 `src/mcp_protocol.rs`）。同一个 iShell 可能对应多条路径，残留的死文件也还躺在目录里。
pub fn mcp_socket_path() -> Option<PathBuf> {
    Some(config_dir()?.join(format!("mcp-{}.sock", mcp_instance_id())))
}

fn mcp_pairing_token_path() -> Option<PathBuf> {
    Some(config_dir()?.join("mcp_pairing_token"))
}

/// 本安装的 **MCP 配对 token**：稳定、每安装唯一、跨重启不变。首次调用时自动生成并持久化。
///
/// 用途：多台电脑共用同一台 AI 服务器的同一个账号时，各家 iShell 反向转发的 socket 会全堆在
/// 同一个目录里，代理无从区分谁是谁（同账号、可能同来源 IP、进程树不相干，服务器上没有任何
/// 自动信号能配对）。于是让每个操作者把自己这台 iShell 的 token 通过环境变量 `ISHELL_MCP_TOKEN`
/// 填进自己那份 Claude Code 的 MCP server 配置——代理只绑定 token 匹配的那个实例，请求就只会
/// 落到发起者自己的电脑上，互不串台。
///
/// 与 [`mcp_instance_id`] 的关键区别：instance_id 是**每进程**的、随进程生灭（用于同机多开
/// 去重），**绝不持久化**；pairing token 是**每安装**的、必须**稳定持久**——它要被贴进 Claude
/// 配置里，iShell 重启后若变了，操作者的配对就失效了。二者用途正交，不能相互替代。
pub fn mcp_pairing_token() -> String {
    static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let Some(p) = mcp_pairing_token_path() else {
                // 配置目录不可用（极罕见）：退化为本进程内一致的随机值，至少本次运行可用。
                return gen_pairing_token();
            };
            // 已有则用已有的（持久稳定是这个 token 的全部意义）。
            if let Ok(s) = std::fs::read_to_string(&p) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            // 首次：生成并落盘。写盘失败会经 write_setting 冒泡提示，但本次仍返回该值。
            let token = gen_pairing_token();
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            write_setting(&p, &token);
            token
        })
        .clone()
}

/// 生成一个 16 位十六进制随机 token（8 字节熵）。熵源不可用（极罕见）时退化到纳秒时间戳。
fn gen_pairing_token() -> String {
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_err() {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        buf[..4].copy_from_slice(&ns.to_le_bytes());
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- 终端配色（多套主题，按索引存储） ----------

fn term_theme_path() -> Option<PathBuf> {
    Some(config_dir()?.join("term_theme"))
}

/// 终端配色索引（0=暖黑 1=暖米 2=近白 3=柔和深 4=经典浅）；未设置默认 1（暖米浅色）。
/// 兼容旧的 `term_dark` 文件（"1"→暖黑0，否则→暖米1）。
pub fn load_term_theme() -> u8 {
    if let Some(v) = term_theme_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u8>().ok())
    {
        return v;
    }
    if let Some(s) = config_dir()
        .map(|d| d.join("term_dark"))
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        return if s.trim() == "1" { 0 } else { 1 };
    }
    1
}

/// 保存终端配色索引。
pub fn save_term_theme(i: u8) {
    if let Some(p) = term_theme_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, i.to_string());
    }
}

// ---------- 传输冲突策略 ----------

fn conflict_policy_path() -> Option<PathBuf> {
    Some(config_dir()?.join("conflict_policy"))
}

/// 读取冲突策略字符串（"overwrite"/"skip"/"rename"），未设置则 None（调用方默认覆盖）。
pub fn load_conflict_policy() -> Option<String> {
    let s = std::fs::read_to_string(conflict_policy_path()?)
        .ok()?
        .trim()
        .to_string();
    (!s.is_empty()).then_some(s)
}

/// 保存冲突策略字符串。
pub fn save_conflict_policy(policy: &str) {
    if let Some(p) = conflict_policy_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, policy);
    }
}

// ---------- 界面缩放（字体大小） ----------

fn zoom_path() -> Option<PathBuf> {
    Some(config_dir()?.join("zoom"))
}

/// 读取界面缩放系数（egui zoom_factor）；未设置默认 1.0，并夹在 [0.7, 2.0]。
pub fn load_zoom() -> f32 {
    zoom_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|z| z.clamp(0.7, 2.0))
        .unwrap_or(1.0)
}

/// 保存界面缩放系数。
pub fn save_zoom(zoom: f32) {
    if let Some(p) = zoom_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, format!("{zoom:.2}"));
    }
}

fn editor_font_path() -> Option<PathBuf> {
    Some(config_dir()?.join("editor_font"))
}

/// 读取编辑器字号（pt）；未设置返回 None（表示沿用全局等宽字号），有值时夹在 [8, 40]。
pub fn load_editor_font() -> Option<f32> {
    editor_font_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|z| z.clamp(8.0, 40.0))
}

/// 保存编辑器字号（pt）。
pub fn save_editor_font(pt: f32) {
    if let Some(p) = editor_font_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        write_setting(p, format!("{pt:.1}"));
    }
}

fn file_cols_path() -> Option<PathBuf> {
    Some(config_dir()?.join("file_cols"))
}

/// 读取文件面板列宽（名称/大小/修改时间/权限/所有者，空格分隔）；未设置或格式不符则 None。
pub fn load_file_cols() -> Option<[f32; 5]> {
    let s = std::fs::read_to_string(file_cols_path()?).ok()?;
    let v: Vec<f32> = s
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    let arr: [f32; 5] = v.try_into().ok()?;
    // 夹到合理范围，防止手改文件出现 0/负数把列挤没
    Some(arr.map(|w| w.clamp(40.0, 800.0)))
}

/// 保存文件面板列宽。
pub fn save_file_cols(cols: &[f32; 5]) {
    if let Some(p) = file_cols_path() {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let s = cols.map(|w| format!("{w:.0}")).join(" ");
        write_setting(p, s);
    }
}

fn cursors_path() -> Option<PathBuf> {
    Some(config_dir()?.join("cursors.json"))
}

/// 读取某文件（键 = "server|path"）上次的光标行（0 基）。
pub fn load_cursor_line(key: &str) -> Option<usize> {
    let s = std::fs::read_to_string(cursors_path()?).ok()?;
    let list: Vec<(String, usize)> = serde_json::from_str(&s).ok()?;
    list.iter().rev().find(|(k, _)| k == key).map(|(_, l)| *l)
}

/// 记录某文件的光标行；按最近使用保序，最多保留 500 条。
pub fn save_cursor_line(key: &str, line: usize) {
    let Some(p) = cursors_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut list: Vec<(String, usize)> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    list.retain(|(k, _)| k != key);
    list.push((key.to_string(), line));
    if list.len() > 500 {
        let cut = list.len() - 500;
        list.drain(..cut);
    }
    if let Ok(s) = serde_json::to_string(&list) {
        write_setting(p, s);
    }
}
