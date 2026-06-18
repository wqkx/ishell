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
    /// "password" 或 "key"
    #[serde(default = "default_auth")]
    pub auth_kind: String,
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
}

fn default_auth() -> String {
    "password".into()
}
fn default_port() -> u16 {
    22
}

fn config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".config").join("ishell"))
}
fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("connections.json"))
}

// ---------- 密钥与加解密 ----------

/// 读取本地密钥；不存在则随机生成并以 0600 写入。
fn load_or_create_key() -> Option<[u8; 32]> {
    let path = config_dir()?.join("key");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes);
            return Some(k);
        }
    }
    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).ok()?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, k).is_err() {
        return None;
    }
    restrict_perms(&path);
    Some(k)
}

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
