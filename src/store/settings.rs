use std::path::PathBuf;

use super::paths::config_dir;

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
        let _ = std::fs::write(p, dir);
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
        let _ = std::fs::write(p, code);
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
        let _ = std::fs::write(p, if on { "1" } else { "0" });
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
        let _ = std::fs::write(p, if on { "1" } else { "0" });
    }
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
        let _ = std::fs::write(p, i.to_string());
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
        let _ = std::fs::write(p, policy);
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
        let _ = std::fs::write(p, format!("{zoom:.2}"));
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
        let _ = std::fs::write(p, s);
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
        let _ = std::fs::write(p, s);
    }
}
