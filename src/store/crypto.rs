use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};

use super::paths::config_dir;

pub(super) const ENC_PREFIX: &str = "enc:v1:";

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

/// 同时进行的钥匙串超时调用上限，避免反复超时堆积泄漏线程。
static KEYCHAIN_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const KEYCHAIN_MAX_INFLIGHT: usize = 2;

/// 在限定时间内执行可能阻塞的钥匙串操作；超时则放弃。
/// 超时线程可能短暂存活至钥匙串返回，但并发数有上限，避免无限堆积。
fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    use std::sync::atomic::Ordering;
    let prev = KEYCHAIN_INFLIGHT.fetch_add(1, Ordering::SeqCst);
    if prev >= KEYCHAIN_MAX_INFLIGHT {
        KEYCHAIN_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        KEYCHAIN_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
    });
    // 超时：工作线程结束后自行减计数
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
    with_timeout(move || {
        keychain_entry()
            .and_then(|e| e.set_password(&v).ok())
            .is_some()
    })
    .unwrap_or(false)
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
pub(super) fn restrict_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
pub(super) fn restrict_perms(_path: &std::path::Path) {}

fn cipher() -> Option<ChaCha20Poly1305> {
    let k = load_or_create_key()?;
    Some(ChaCha20Poly1305::new(Key::from_slice(&k)))
}

/// 加密一个秘密字段为 `enc:v1:<base64(nonce||ciphertext)>`；
/// 空串原样返回；失败返回 Err（fail-closed，绝不静默落盘明文）。
pub fn encrypt_secret(plain: &str) -> Result<String, String> {
    if plain.is_empty() {
        return Ok(String::new());
    }
    if plain.starts_with(ENC_PREFIX) {
        return Ok(plain.to_string()); // 已是密文
    }
    let Some(c) = cipher() else {
        return Err(match crate::i18n::current() {
            crate::i18n::Lang::Zh => "无法初始化密码加密（主密钥不可用）".into(),
            crate::i18n::Lang::En => {
                "Cannot init secret encryption (master key unavailable)".into()
            }
        });
    };
    let mut nonce = [0u8; 12];
    if getrandom::getrandom(&mut nonce).is_err() {
        return Err(match crate::i18n::current() {
            crate::i18n::Lang::Zh => "无法生成加密随机数".into(),
            crate::i18n::Lang::En => "Failed to generate encryption nonce".into(),
        });
    }
    match c.encrypt(Nonce::from_slice(&nonce), plain.as_bytes()) {
        Ok(ct) => {
            let mut blob = nonce.to_vec();
            blob.extend_from_slice(&ct);
            Ok(format!("{ENC_PREFIX}{}", STANDARD.encode(blob)))
        }
        Err(_) => Err(match crate::i18n::current() {
            crate::i18n::Lang::Zh => "密码加密失败".into(),
            crate::i18n::Lang::En => "Secret encryption failed".into(),
        }),
    }
}

/// 解密；非 `enc:v1:` 前缀视为明文（旧数据）原样返回。
/// 解密失败（含罕见的「明文密码恰好以 enc:v1: 开头」被误判为密文）时**返回原串**——
/// 当明文用会导致一次登录失败，但绝不把已存密码静默变成空串。
pub fn decrypt_secret(s: &str) -> String {
    let Some(rest) = s.strip_prefix(ENC_PREFIX) else {
        return s.to_string();
    };
    let fallback = || s.to_string();
    let Some(c) = cipher() else { return fallback() };
    let Ok(blob) = STANDARD.decode(rest) else {
        return fallback();
    };
    if blob.len() < 12 {
        return fallback();
    }
    let (nonce, ct) = blob.split_at(12);
    c.decrypt(Nonce::from_slice(nonce), ct)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_secret_roundtrip_or_fail_closed() {
        // 空串始终成功
        assert_eq!(encrypt_secret("").unwrap(), "");
        // 有主密钥时加密应产出 enc:v1: 前缀；无密钥环境则 Err（不得返回明文）
        match encrypt_secret("s3cret") {
            Ok(ct) => {
                assert!(
                    ct.starts_with(ENC_PREFIX),
                    "ciphertext must use enc prefix, got {ct}"
                );
                assert_ne!(ct, "s3cret");
                assert_eq!(decrypt_secret(&ct), "s3cret");
            }
            Err(e) => assert!(!e.is_empty()),
        }
    }
}
