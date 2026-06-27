//! 已保存连接的持久化（`~/.config/ishell/connections.json`）。
//!
//! 密码/口令在磁盘上以 **ChaCha20-Poly1305** 加密存储（前缀 `enc:v1:`），密钥随机生成
//! 保存在 `~/.config/ishell/key`（0600）。**内存中始终为明文**，其余代码无需感知加密。
//!
//! 说明：这是「at-rest」加密——能挡住直接读文件偷密码；但密钥就在同机，无法防御
//! 拿到本机完整权限的攻击者（个人工具取舍）。换机时需同时带上 `key` 才能解密。
//!
//! 兼容性：所有字段带 `#[serde(default)]`，新增字段不致旧文件解析失败；读到旧明文密码
//! 会自动改写为密文（迁移）。解析失败时把原文件备份为 `.bak`，避免被覆盖丢失。

use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use serde::{Deserialize, Serialize};

const ENC_PREFIX: &str = "enc:v1:";

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SavedConnection {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// "password" / "key" / "agent" / "interactive"
    #[serde(default = "default_auth")]
    pub auth_kind: String,
    /// 转发本机 ssh-agent（-A）
    #[serde(default)]
    pub forward_agent: bool,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub key_path: String,
    #[serde(default)]
    pub passphrase: String,
    // —— 可选跳板机 ——
    #[serde(default)]
    pub use_jump: bool,
    #[serde(default)]
    pub jump_host: String,
    #[serde(default = "default_port")]
    pub jump_port: u16,
    #[serde(default)]
    pub jump_username: String,
    #[serde(default = "default_auth")]
    pub jump_auth_kind: String,
    #[serde(default)]
    pub jump_password: String,
    #[serde(default)]
    pub jump_key_path: String,
    #[serde(default)]
    pub jump_passphrase: String,
    // —— 组织 ——
    /// 分组（文件夹）名；空表示「未分组」
    #[serde(default)]
    pub group: String,
    /// 标签（逗号分隔，自由文本），参与搜索
    #[serde(default)]
    pub tags: String,
}

fn default_auth() -> String {
    "password".into()
}
fn default_port() -> u16 {
    22
}

fn config_dir() -> Option<PathBuf> {
    // Windows: %APPDATA%\ishell；类 Unix：$HOME/.config/ishell
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("ishell"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("ishell"))
    }
}
fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("connections.json"))
}

// ---------- 密钥与加解密 ----------

/// 读取本地密钥；不存在则随机生成并以 0600 写入。
const KEYCHAIN_SERVICE: &str = "ishell";
const KEYCHAIN_USER: &str = "master-key";

/// 钥匙串是否可用：Linux 上必须有 D-Bus 会话总线，否则跳过（避免无总线时阻塞）。
fn keychain_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            return true;
        }
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            return std::path::Path::new(&rt).join("bus").exists();
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// 在限定时间内执行可能阻塞的钥匙串操作；超时则放弃（线程将自行结束/泄漏一次，因主密钥已缓存）。
fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(3)).ok()
}

fn keychain_entry() -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).ok()
}

/// 从系统钥匙串读取 32 字节主密钥（base64 存储）。
fn keychain_get_key() -> Option<[u8; 32]> {
    if !keychain_available() {
        return None;
    }
    let s = with_timeout(|| keychain_entry()?.get_password().ok())??;
    let b = STANDARD.decode(s).ok()?;
    (b.len() == 32).then(|| {
        let mut k = [0u8; 32];
        k.copy_from_slice(&b);
        k
    })
}

/// 写入系统钥匙串；成功返回 true。
fn keychain_set_key(k: &[u8; 32]) -> bool {
    if !keychain_available() {
        return false;
    }
    let v = STANDARD.encode(k);
    with_timeout(move || keychain_entry().and_then(|e| e.set_password(&v).ok()).is_some()).unwrap_or(false)
}

/// 进程内缓存的主密钥（仅加载一次，避免反复访问钥匙串）。
static MASTER_KEY: std::sync::OnceLock<Option<[u8; 32]>> = std::sync::OnceLock::new();

/// 主密钥的存放方式——用于向用户**透明展示**所存密码的保护级别。
/// 注意：无论哪种方式，密码本身都已用 ChaCha20Poly1305 加密；差异在于「主密钥存哪」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyStorage {
    /// 系统钥匙串（最佳：受 OS 登录态/钥匙串口令保护）
    Keychain,
    /// 本地 0600 文件（钥匙串不可用时的回退：能读到该文件者可解密）
    LocalFile,
    /// 无可用密钥（加密不可用）
    None,
}
static KEY_STORAGE: std::sync::OnceLock<KeyStorage> = std::sync::OnceLock::new();
/// 本地 key 文件读取时权限是否曾比 0600 宽松（已自动收紧；用于提醒曾存在暴露风险）。
static KEY_PERMS_LOOSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// 主密钥的存放方式（首次查询会触发密钥加载/创建）。供 UI 展示安全级别。
pub fn key_storage() -> KeyStorage {
    load_or_create_key();
    KEY_STORAGE.get().copied().unwrap_or(KeyStorage::None)
}

/// 本地 key 文件权限是否曾过宽（group/other 可访问），现已自动收紧为 0600。
pub fn key_perms_were_loose() -> bool {
    load_or_create_key();
    KEY_PERMS_LOOSE.get().copied().unwrap_or(false)
}

/// 取（或创建）加密主密钥。优先系统钥匙串；不可用时回退到本地 `key` 文件（0600）。
fn load_or_create_key() -> Option<[u8; 32]> {
    *MASTER_KEY.get_or_init(compute_master_key)
}

fn compute_master_key() -> Option<[u8; 32]> {
    // 1) 系统钥匙串优先
    if let Some(k) = keychain_get_key() {
        let _ = KEY_STORAGE.set(KeyStorage::Keychain);
        return Some(k);
    }
    let path = config_dir()?.join("key");
    // 2) 迁移：旧 key 文件 → 钥匙串；确认可读回后删除明文文件
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            // 权限校验：本地 key 文件应为 0600；过宽（group/other 可访问）则记录并立即收紧
            check_key_perms(&path);
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes);
            if keychain_set_key(&k) && keychain_get_key() == Some(k) {
                let _ = std::fs::remove_file(&path);
                let _ = KEY_STORAGE.set(KeyStorage::Keychain);
            } else {
                let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
            }
            return Some(k);
        }
    }
    // 3) 新建密钥：优先存钥匙串，不可用则落本地文件（保持旧行为）
    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).ok()?;
    if keychain_set_key(&k) {
        let _ = KEY_STORAGE.set(KeyStorage::Keychain);
        return Some(k);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 原子创建并落 0600：经 0600 临时文件写入 + rename 到位，消除「先 write(默认 umask 0644) 再 chmod」
    // 的暴露窗口——该文件是解密全部已存密码的主密钥。
    if write_key_file(&path, &k).is_err() {
        let _ = KEY_STORAGE.set(KeyStorage::None);
        return None;
    }
    let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
    Some(k)
}

/// 以 0600 原子写入主密钥文件：写 0600 临时文件后 rename 覆盖到位（rename 保留临时文件权限）。
#[cfg(unix)]
fn write_key_file(path: &std::path::Path, k: &[u8; 32]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp); // 清理可能的同名残留，确保 create_new 成功
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // 全新创建：mode 在此刻生效，无 0644 窗口
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(k)?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)
}
#[cfg(not(unix))]
fn write_key_file(path: &std::path::Path, k: &[u8; 32]) -> std::io::Result<()> {
    std::fs::write(path, k)
}

/// 校验本地 key 文件权限：若 group/other 有任何位（过宽），记录并收紧为 0600。
#[cfg(unix)]
fn check_key_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            let _ = KEY_PERMS_LOOSE.set(true);
            restrict_perms(path);
        }
    }
}
#[cfg(not(unix))]
fn check_key_perms(_path: &std::path::Path) {}

#[cfg(unix)]
fn restrict_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_perms(_path: &std::path::Path) {}

fn cipher() -> Option<ChaCha20Poly1305> {
    let k = load_or_create_key()?;
    Some(ChaCha20Poly1305::new(Key::from_slice(&k)))
}

/// 加密一个秘密字段为 `enc:v1:<base64(nonce||ciphertext)>`；
/// 空串原样返回；任何失败则退化为明文（不阻断保存）。
pub fn encrypt_secret(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    if plain.starts_with(ENC_PREFIX) {
        return plain.to_string(); // 已是密文
    }
    let Some(c) = cipher() else { return plain.to_string() };
    let mut nonce = [0u8; 12];
    if getrandom::getrandom(&mut nonce).is_err() {
        return plain.to_string();
    }
    match c.encrypt(Nonce::from_slice(&nonce), plain.as_bytes()) {
        Ok(ct) => {
            let mut blob = nonce.to_vec();
            blob.extend_from_slice(&ct);
            format!("{ENC_PREFIX}{}", STANDARD.encode(blob))
        }
        Err(_) => plain.to_string(),
    }
}

/// 解密；非 `enc:v1:` 前缀视为明文（旧数据）原样返回；解密失败返回空串。
pub fn decrypt_secret(s: &str) -> String {
    let Some(rest) = s.strip_prefix(ENC_PREFIX) else {
        return s.to_string();
    };
    let Some(c) = cipher() else { return String::new() };
    let Ok(blob) = STANDARD.decode(rest) else {
        return String::new();
    };
    if blob.len() < 12 {
        return String::new();
    }
    let (nonce, ct) = blob.split_at(12);
    c.decrypt(Nonce::from_slice(nonce), ct)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default()
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
    if let Some(s) = config_dir().map(|d| d.join("term_dark")).and_then(|p| std::fs::read_to_string(p).ok()) {
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
    let s = std::fs::read_to_string(conflict_policy_path()?).ok()?.trim().to_string();
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

// ---------- 读写 ----------

/// 读取已保存连接列表（内存中为明文密码）。
///
/// - 文件不存在 → 空列表
/// - 解析失败 → 先把原文件备份为 `connections.json.bak`，再返回空（避免被覆盖丢失）
/// - 读到旧明文密码 → 自动改写为密文（迁移），无需用户操作
pub fn load() -> Vec<SavedConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut list: Vec<SavedConnection> = match serde_json::from_str(&text) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("connections.json 解析失败：{e}，已备份为 .bak");
            let _ = std::fs::write(path.with_extension("json.bak"), &text);
            return Vec::new();
        }
    };
    // 检测是否存在旧明文（需要迁移）
    let is_plain = |s: &str| !s.is_empty() && !s.starts_with(ENC_PREFIX);
    let needs_migrate = list.iter().any(|c| {
        is_plain(&c.password) || is_plain(&c.passphrase) || is_plain(&c.jump_password) || is_plain(&c.jump_passphrase)
    });
    // 解密到内存明文
    for c in &mut list {
        c.password = decrypt_secret(&c.password);
        c.passphrase = decrypt_secret(&c.passphrase);
        c.jump_password = decrypt_secret(&c.jump_password);
        c.jump_passphrase = decrypt_secret(&c.jump_passphrase);
    }
    if needs_migrate {
        save(&list); // 以密文重写，完成迁移
    }
    list
}

/// 写回连接列表（密码/口令加密后落盘）。
pub fn save(list: &[SavedConnection]) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let encrypted: Vec<SavedConnection> = list
        .iter()
        .map(|c| {
            let mut e = c.clone();
            e.password = encrypt_secret(&c.password);
            e.passphrase = encrypt_secret(&c.passphrase);
            e.jump_password = encrypt_secret(&c.jump_password);
            e.jump_passphrase = encrypt_secret(&c.jump_passphrase);
            e
        })
        .collect();
    if let Ok(json) = serde_json::to_string_pretty(&encrypted) {
        if std::fs::write(&path, json).is_ok() {
            restrict_perms(&path);
        }
    }
}

// ———————————————————————— 导入 ~/.ssh/config ————————————————————————

/// 用户主目录（跨平台）。
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// 把 `~/...` 展开为绝对路径。
fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

/// 一个 Host 块的原始字段。
#[derive(Default, Clone)]
struct SshHostBlock {
    patterns: Vec<String>,
    hostname: String,
    user: String,
    port: String,
    identity: String,
    proxyjump: String,
}

/// 拆分一行为 (关键字, 值)，兼容 `Key Value` 与 `Key=Value`（含周围空格）。
fn split_kv(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
        i += 1;
    }
    let key = &line[..i];
    let rest = line[i..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    (key, rest.trim_matches('"'))
}

/// 解析 ProxyJump 值：可能是配置内别名，或 `user@host:port`。返回 (use_jump, host, port, user)。
fn parse_proxyjump(val: &str, blocks: &[SshHostBlock], default_user: &str) -> (bool, String, u16, String) {
    let v = val.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") {
        return (false, String::new(), 22, String::new());
    }
    let first = v.split(',').next().unwrap_or(v).trim(); // 多跳取第一跳
    if let Some(b) = blocks.iter().find(|b| b.patterns.iter().any(|p| p == first)) {
        let host = if b.hostname.is_empty() { first.to_string() } else { b.hostname.clone() };
        let user = if b.user.is_empty() { default_user.to_string() } else { b.user.clone() };
        return (true, host, b.port.parse().unwrap_or(22), user);
    }
    let (user, rest) = match first.split_once('@') {
        Some((u, r)) => (u.to_string(), r),
        None => (default_user.to_string(), first),
    };
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (rest.to_string(), 22),
    };
    (true, host, port, user)
}

/// 解析 `~/.ssh/config`，产出可导入的连接列表（跳过通配 Host；无 IdentityFile 默认用 agent）。
pub fn import_ssh_config() -> Vec<SavedConnection> {
    let Some(home) = home_dir() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(home.join(".ssh").join("config")) else {
        return Vec::new();
    };
    let default_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into());
    parse_ssh_config_text(&text, &default_user)
}

/// 纯解析（便于测试）：从 ssh_config 文本生成连接列表。
fn parse_ssh_config_text(text: &str, default_user: &str) -> Vec<SavedConnection> {
    let mut blocks: Vec<SshHostBlock> = Vec::new();
    let mut cur: Option<SshHostBlock> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kw, val) = split_kv(line);
        match kw.to_ascii_lowercase().as_str() {
            "host" => {
                if let Some(b) = cur.take() {
                    blocks.push(b);
                }
                cur = Some(SshHostBlock {
                    patterns: val.split_whitespace().map(|s| s.to_string()).collect(),
                    ..Default::default()
                });
            }
            other => {
                if let Some(b) = cur.as_mut() {
                    match other {
                        "hostname" => b.hostname = val.to_string(),
                        "user" => b.user = val.to_string(),
                        "port" => b.port = val.to_string(),
                        "identityfile" if b.identity.is_empty() => b.identity = val.to_string(),
                        "proxyjump" => b.proxyjump = val.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    if let Some(b) = cur.take() {
        blocks.push(b);
    }

    let mut out = Vec::new();
    for b in &blocks {
        // 取第一个非通配模式作为名称；纯通配块（如 Host *）跳过
        let Some(name) = b.patterns.iter().find(|p| !p.contains('*') && !p.contains('?')) else {
            continue;
        };
        let (auth_kind, key_path) = if b.identity.is_empty() {
            ("agent".to_string(), String::new())
        } else {
            ("key".to_string(), expand_tilde(&b.identity))
        };
        let (use_jump, jh, jp, ju) = parse_proxyjump(&b.proxyjump, &blocks, default_user);
        out.push(SavedConnection {
            name: name.clone(),
            host: if b.hostname.is_empty() { name.clone() } else { b.hostname.clone() },
            port: b.port.parse().unwrap_or(22),
            username: if b.user.is_empty() { default_user.to_string() } else { b.user.clone() },
            auth_kind,
            forward_agent: false,
            password: String::new(),
            key_path,
            passphrase: String::new(),
            use_jump,
            jump_host: jh,
            jump_port: jp,
            jump_username: ju,
            jump_auth_kind: "agent".into(),
            jump_password: String::new(),
            jump_key_path: String::new(),
            jump_passphrase: String::new(),
            group: crate::i18n::tr("导入", "Imported").to_string(),
            tags: String::new(),
        });
    }
    out
}

// ———————————————————————— 命令片段库（snippets.json） ————————————————————————

fn default_true() -> bool {
    true
}

/// 一条命令片段：名称 + 命令文本；`run` 决定发送后是否自动回车执行。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Snippet {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default = "default_true")]
    pub run: bool,
}

fn snippets_path() -> Option<PathBuf> {
    Some(config_dir()?.join("snippets.json"))
}

/// 读取命令片段列表（文件不存在或解析失败均返回空）。
pub fn load_snippets() -> Vec<Snippet> {
    let Some(path) = snippets_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写回命令片段列表。
pub fn save_snippets(list: &[Snippet]) {
    let Some(path) = snippets_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(&path, json);
    }
}

// ———————————————————————— 文件夹收藏（favorites.json，按服务器分组） ————————————————————————

fn favorites_path() -> Option<PathBuf> {
    Some(config_dir()?.join("favorites.json"))
}

fn load_favorites_map() -> std::collections::HashMap<String, Vec<String>> {
    favorites_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 读取某服务器的收藏路径列表。
pub fn load_favorites(server: &str) -> Vec<String> {
    load_favorites_map().remove(server).unwrap_or_default()
}

/// 写回某服务器的收藏路径列表（合并进总表后落盘）。
pub fn save_favorites(server: &str, list: &[String]) {
    let Some(path) = favorites_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut map = load_favorites_map();
    if list.is_empty() {
        map.remove(server);
    } else {
        map.insert(server.to_string(), list.to_vec());
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_config_basic() {
        let cfg = "\
# comment
Host *
    ServerAliveInterval 30

Host web
    HostName 10.0.0.5
    User deploy
    Port 2222
    IdentityFile /home/me/.ssh/id_ed25519

Host db
    HostName db.internal
    ProxyJump web

Host pat-*
    User skip
";
        let list = parse_ssh_config_text(cfg, "fallback");
        // 通配块（Host *、Host pat-*）被跳过，剩 web / db
        assert_eq!(list.len(), 2);

        let web = &list[0];
        assert_eq!(web.name, "web");
        assert_eq!(web.host, "10.0.0.5");
        assert_eq!(web.username, "deploy");
        assert_eq!(web.port, 2222);
        assert_eq!(web.auth_kind, "key");
        assert_eq!(web.key_path, "/home/me/.ssh/id_ed25519");
        assert!(!web.use_jump);

        let db = &list[1];
        assert_eq!(db.name, "db");
        assert_eq!(db.host, "db.internal");
        assert_eq!(db.username, "fallback"); // 无 User → 默认用户
        assert_eq!(db.auth_kind, "agent"); // 无 IdentityFile → agent
        assert!(db.use_jump);
        // ProxyJump web → 解析为 web 的 HostName/User/Port
        assert_eq!(db.jump_host, "10.0.0.5");
        assert_eq!(db.jump_username, "deploy");
        assert_eq!(db.jump_port, 2222);
    }

    #[test]
    fn parse_proxyjump_userhostport() {
        let (uj, h, p, u) = parse_proxyjump("bastion@gw.example.com:2200", &[], "me");
        assert!(uj);
        assert_eq!(h, "gw.example.com");
        assert_eq!(p, 2200);
        assert_eq!(u, "bastion");
    }
}
