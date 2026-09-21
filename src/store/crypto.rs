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

/// 归还 `KEYCHAIN_INFLIGHT` 名额的守卫。
///
/// 计数**必须**走 Drop、不能写在工作线程末尾：`keyring` 是第三方 crate，其调用一旦
/// panic（或将来某次改动引入了 `?`/提前返回），写在末尾的 `fetch_sub` 就永远执行不到，
/// 名额只减不增。攒够 `KEYCHAIN_MAX_INFLIGHT` 次之后 `with_timeout` 会对**每一次**调用
/// 直接返回超时，于是 `keychain_get_key`/`keychain_set_key` 永久失效 → 主密钥被判为
/// 「不可用」→ `decrypt_secret` 全线失败 → 所有已保存的密码都登录失败。
/// 线程展开时 Drop 照常执行，这一条就堵死了。
struct KeychainSlot;
impl Drop for KeychainSlot {
    fn drop(&mut self) {
        KEYCHAIN_INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KeychainWait<T> {
    Ok(T),
    TimedOut,
}

/// 在限定时间内执行可能阻塞的钥匙串操作；超时则放弃。
/// 超时线程可能短暂存活至钥匙串返回，但并发数有上限，避免无限堆积。
fn with_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> KeychainWait<T> {
    use std::sync::atomic::Ordering;
    let prev = KEYCHAIN_INFLIGHT.fetch_add(1, Ordering::SeqCst);
    if prev >= KEYCHAIN_MAX_INFLIGHT {
        KEYCHAIN_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        return KeychainWait::TimedOut;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // 名额在本线程结束时归还，无论正常返回还是 panic 展开（见 KeychainSlot）。
        let _slot = KeychainSlot;
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(v) => KeychainWait::Ok(v),
        Err(_) => KeychainWait::TimedOut,
    }
}

fn keychain_entry() -> Option<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).ok()
}

/// 读钥匙串必须分清「没有条目」和「暂时读不到」。后者若当成没有，就会生成一把新主密钥
/// 写进去，把还能解开已存密码的旧密钥覆盖掉——用户只会看到所有保存的密码突然解不开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeychainRead {
    Found([u8; 32]),
    NotFound,
    Unavailable,
    Transient,
}

fn keychain_read() -> KeychainRead {
    if !keychain_available() {
        return KeychainRead::Unavailable;
    }
    // Entry::new / get_password 都必须在工作线程里：async-secret-service 在 UI 线程上
    // 调会和 tokio/zbus 死锁，窗口就彻底点不动了。
    match with_timeout(|| {
        let entry = match keychain_entry() {
            Some(e) => e,
            None => return Err(None),
        };
        entry.get_password().map_err(Some)
    }) {
        KeychainWait::TimedOut => {
            log::warn!("读取系统钥匙串超时，不会为此生成新的主密钥");
            KeychainRead::Transient
        }
        KeychainWait::Ok(Err(None)) => KeychainRead::Transient,
        KeychainWait::Ok(Err(Some(keyring::Error::NoEntry))) => KeychainRead::NotFound,
        KeychainWait::Ok(Err(Some(e))) => {
            log::warn!("读取系统钥匙串失败（{e}），不会为此生成新的主密钥");
            KeychainRead::Transient
        }
        KeychainWait::Ok(Ok(s)) => match decode_master_key(&s) {
            Some(k) => KeychainRead::Found(k),
            None => {
                log::warn!("钥匙串里的主密钥格式无法识别，不会覆盖它");
                KeychainRead::Transient
            }
        },
    }
}

fn decode_master_key(s: &str) -> Option<[u8; 32]> {
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
    match with_timeout(move || {
        keychain_entry()
            .and_then(|e| e.set_password(&v).ok())
            .is_some()
    }) {
        KeychainWait::Ok(ok) => ok,
        KeychainWait::TimedOut => false,
    }
}

/// 进程内缓存的主密钥。成功的密钥一直留着；读失败只记时间戳，过一会儿再试，
/// 避免 OnceLock 把一次超时钉成整次进程都没有主密钥。
#[derive(Clone, Copy)]
enum MasterKeyCache {
    Unset,
    Key([u8; 32]),
    FailedAt(std::time::Instant),
}
static MASTER_KEY: std::sync::Mutex<MasterKeyCache> = std::sync::Mutex::new(MasterKeyCache::Unset);
const MASTER_KEY_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

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
    let mut cache = match MASTER_KEY.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match *cache {
        MasterKeyCache::Key(k) => return Some(k),
        MasterKeyCache::FailedAt(at) if at.elapsed() < MASTER_KEY_RETRY => return None,
        MasterKeyCache::Unset | MasterKeyCache::FailedAt(_) => {}
    }
    let k = compute_master_key();
    *cache = match k {
        Some(key) => MasterKeyCache::Key(key),
        None => MasterKeyCache::FailedAt(std::time::Instant::now()),
    };
    k
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MasterKeyPlan {
    UseKeychain,
    UseLocal { migrate: bool },
    MintNew,
    None,
}

/// 纯决策：超时/锁定绝不能当成「没有密钥」去生成一把新的（那会覆盖钥匙串里还能用的密钥）。
/// 钥匙串根本不存在时仍在本地新建，哪怕磁盘上已有旧密文——否则卸掉钥匙串后重新填写也无法落盘。
fn plan_master_key(read: KeychainRead, has_local: bool, has_ciphertext: bool) -> MasterKeyPlan {
    match read {
        KeychainRead::Found(_) => MasterKeyPlan::UseKeychain,
        KeychainRead::NotFound => {
            if has_local {
                MasterKeyPlan::UseLocal { migrate: true }
            } else if has_ciphertext {
                MasterKeyPlan::None
            } else {
                MasterKeyPlan::MintNew
            }
        }
        KeychainRead::Unavailable => {
            if has_local {
                MasterKeyPlan::UseLocal { migrate: false }
            } else {
                // 钥匙串根本不在（不是超时）。旧版本迁到钥匙串后会删掉本地 key，
                // 这时若拒绝新建，用户重新填写也落不了盘，除非手工清掉 connections.json。
                // 新密钥只写本地文件（钥匙串不可用时 set 直接失败），旧密文仍由保存路径留在磁盘上。
                MasterKeyPlan::MintNew
            }
        }
        KeychainRead::Transient => {
            if has_local {
                MasterKeyPlan::UseLocal { migrate: false }
            } else {
                MasterKeyPlan::None
            }
        }
    }
}

fn read_local_key(path: &std::path::Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    (bytes.len() == 32).then(|| {
        check_key_perms(path);
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        k
    })
}

fn saved_secrets_need_existing_key() -> bool {
    let Some(path) = super::paths::config_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return text.contains(ENC_PREFIX);
    };
    let Some(arr) = v.as_array() else {
        return false;
    };
    arr.iter().any(|item| {
        ["password", "passphrase", "jump_password", "jump_passphrase"]
            .iter()
            .any(|key| {
                item.get(*key)
                    .and_then(|x| x.as_str())
                    .is_some_and(|s| s.starts_with(ENC_PREFIX))
            })
    })
}

fn decrypt_with_key(k: &[u8; 32], s: &str) -> Option<String> {
    let rest = s.strip_prefix(ENC_PREFIX)?;
    let blob = STANDARD.decode(rest).ok()?;
    if blob.len() < 12 {
        return None;
    }
    let c = ChaCha20Poly1305::new(Key::from_slice(k));
    let (nonce, ct) = blob.split_at(12);
    c.decrypt(Nonce::from_slice(nonce), ct)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

fn first_ciphertext_sample() -> Option<String> {
    let path = super::paths::config_path()?;
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    for item in v.as_array()? {
        for key in ["password", "passphrase", "jump_password", "jump_passphrase"] {
            if let Some(s) = item.get(key).and_then(|x| x.as_str()) {
                if s.starts_with(ENC_PREFIX) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn key_opens_saved_secrets(k: &[u8; 32]) -> bool {
    match first_ciphertext_sample() {
        Some(sample) => decrypt_with_key(k, &sample).is_some(),
        None => true,
    }
}

/// 解不开已存密文的钥匙串密钥不能写成本地备份：以后钥匙串一超时就会稳定地用这把废钥匙。
fn may_persist_as_local_backup(key_opens_secrets: bool, has_ciphertext: bool) -> bool {
    key_opens_secrets || !has_ciphertext
}

fn persist_local_backup(path: &std::path::Path, k: &[u8; 32]) {
    let key_opens = key_opens_saved_secrets(k);
    if !may_persist_as_local_backup(key_opens, saved_secrets_need_existing_key()) {
        log::warn!("钥匙串密钥解不开已存密码，不把它写成本地备份");
        return;
    }
    if let Some(existing) = read_local_key(path) {
        if existing == *k {
            return;
        }
        // 本地还是旧密钥、且它能解开已存密码：绝不覆盖，那是唯一的找回手段。
        if key_opens_saved_secrets(&existing) && !key_opens {
            log::warn!("本地 key 文件仍能解开已存密码，钥匙串里的密钥对不上；保留本地文件不覆盖");
            return;
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = write_key_file(path, k) {
        log::warn!("写入本地主密钥备份失败：{e}");
    }
}

fn mint_new_key(path: &std::path::Path) -> Option<[u8; 32]> {
    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).ok()?;
    if keychain_set_key(&k) {
        persist_local_backup(path, &k);
        let _ = KEY_STORAGE.set(KeyStorage::Keychain);
        return Some(k);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if write_key_file(path, &k).is_err() {
        let _ = KEY_STORAGE.set(KeyStorage::None);
        return None;
    }
    let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
    Some(k)
}

fn compute_master_key() -> Option<[u8; 32]> {
    let path = match config_dir() {
        Some(dir) => dir.join("key"),
        None => {
            let _ = KEY_STORAGE.set(KeyStorage::None);
            return None;
        }
    };
    let local = read_local_key(&path);
    let has_ciphertext = saved_secrets_need_existing_key();
    let read = keychain_read();
    let keychain_key = match read {
        KeychainRead::Found(k) => Some(k),
        _ => None,
    };
    match plan_master_key(read, local.is_some(), has_ciphertext) {
        MasterKeyPlan::UseKeychain => {
            let k = keychain_key.expect("Found 才走 UseKeychain");
            if has_ciphertext && !key_opens_saved_secrets(&k) {
                if let Some(local_k) = local {
                    if key_opens_saved_secrets(&local_k) {
                        log::warn!("钥匙串主密钥解不开已存密码，改用仍能解开的本地 key 文件");
                        let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
                        return Some(local_k);
                    }
                }
                log::error!(
                    "钥匙串里的主密钥与已保存的密码不匹配。不会再生成新密钥去覆盖。请重新填写那些连接的密码。"
                );
                let _ = KEY_STORAGE.set(KeyStorage::Keychain);
                return Some(k);
            }
            persist_local_backup(&path, &k);
            let _ = KEY_STORAGE.set(KeyStorage::Keychain);
            Some(k)
        }
        MasterKeyPlan::UseLocal { migrate } => {
            let k = local.expect("has_local 才走 UseLocal");
            if migrate && keychain_set_key(&k) {
                match keychain_read() {
                    KeychainRead::Found(got) if got == k => {
                        let _ = KEY_STORAGE.set(KeyStorage::Keychain);
                        return Some(k);
                    }
                    _ => {}
                }
            }
            let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
            Some(k)
        }
        MasterKeyPlan::MintNew => mint_new_key(&path),
        MasterKeyPlan::None => {
            log::error!(
                "主密钥不可用（钥匙串暂时读不到，又没有本地备份）。已存密码不会被一把新密钥毁掉。"
            );
            // 不写入 KEY_STORAGE：超时这次失败，钥匙串恢复后这次进程里还应该能再读。
            None
        }
    }
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

/// 只有前缀匹配还不够：真实密码恰好以 `enc:v1:` 开头时，光看前缀会把它误判成"已经是
/// 密文"，从而完全跳过加密、原样存成明文——这正是"绝不静默落盘明文"这条承诺被破坏的
/// 地方。这里额外验证"前缀后面那段确实能被当前密钥解出来"（base64 解出来的字节数够长、
/// AEAD 认证也通过）才真正当作已加密；哪怕前缀凑巧对上，只要解不出来就说明这就是一段
/// 普通明文（只是恰好长得像密文前缀），必须继续走下面的真正加密流程。
fn is_genuine_ciphertext(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(ENC_PREFIX) else {
        return false;
    };
    let Some(c) = cipher() else { return false };
    let Ok(blob) = STANDARD.decode(rest) else {
        return false;
    };
    if blob.len() < 12 {
        return false;
    }
    let (nonce, ct) = blob.split_at(12);
    c.decrypt(Nonce::from_slice(nonce), ct)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .is_some()
}

/// 加密一个秘密字段为 `enc:v1:<base64(nonce||ciphertext)>`；
/// 空串原样返回；失败返回 Err（fail-closed，绝不静默落盘明文）。
pub fn encrypt_secret(plain: &str) -> Result<String, String> {
    if plain.is_empty() {
        return Ok(String::new());
    }
    // 能用当前密钥解开的密文原样落盘。解不开则当成明文再加密——真实密码碰巧以
    // `enc:v1:` 开头时必须能保存。解不开的密文不会走到这里：读盘时已清空并打标，
    // 保存路径只在那个标志为真时才把磁盘上的旧密文填回去。
    if is_genuine_ciphertext(plain) {
        return Ok(plain.to_string());
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

/// 解密。非 `enc:v1:` 前缀视为明文（旧数据）。解不开返回 Err，由调用方决定是留空还是报错；
/// 不要把密文原串当密码发出去。
pub(super) fn try_decrypt_secret(s: &str) -> Result<String, String> {
    let Some(rest) = s.strip_prefix(ENC_PREFIX) else {
        return Ok(s.to_string());
    };
    let Some(c) = cipher() else {
        return Err("主密钥不可用，可能是系统钥匙串访问不了或换了一台机器".into());
    };
    let Ok(blob) = STANDARD.decode(rest) else {
        return Err("密文不是合法的 base64，格式已损坏".into());
    };
    if blob.len() < 12 {
        return Err("密文长度不足，格式已损坏".into());
    }
    let (nonce, ct) = blob.split_at(12);
    c.decrypt(Nonce::from_slice(nonce), ct)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| "AEAD 认证未通过（密钥不匹配，或数据已损坏/被篡改）".to_string())
}

/// 解密；非 `enc:v1:` 前缀视为明文（旧数据）原样返回。解密失败时**返回原串**——当明文
/// 用会导致一次登录失败，但绝不把已存密码静默变成空串。新代码请走 [`try_decrypt_secret`]。
#[cfg_attr(not(test), allow(dead_code))]
pub fn decrypt_secret(s: &str) -> String {
    match try_decrypt_secret(s) {
        Ok(p) => p,
        Err(reason) => {
            log::warn!(
                "已保存的密码/密钥口令解密失败（{reason}），将按原文尝试，很可能导致后续认证失败"
            );
            s.to_string()
        }
    }
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

    #[test]
    fn plaintext_that_starts_with_the_ciphertext_prefix_can_be_saved() {
        let weird = "enc:v1:not-ciphertext";
        match encrypt_secret(weird) {
            Ok(ct) => {
                assert!(ct.starts_with(ENC_PREFIX));
                assert_ne!(ct, weird);
                assert_eq!(decrypt_secret(&ct), weird);
            }
            Err(e) => assert!(
                !e.contains("旧密文") && !e.contains("cannot open"),
                "碰巧带 enc:v1: 前缀的明文必须能保存，不能当成解不开的密文拒绝：{e}"
            ),
        }
    }
}

#[cfg(test)]
mod keychain_slot_tests {
    use super::{with_timeout, KEYCHAIN_MAX_INFLIGHT};

    /// **回归门禁**：钥匙串调用 panic 之后，并发名额必须归还。
    ///
    /// `with_timeout` 原先把 `KEYCHAIN_INFLIGHT.fetch_sub` 写在工作线程末尾。keyring 是第三方
    /// crate，其调用一旦 panic，线程展开、那句 fetch_sub 永远执行不到，名额只减不增。攒够
    /// `KEYCHAIN_MAX_INFLIGHT` 次之后，`with_timeout` 会对**每一次**调用直接返回 None，于是
    /// keychain_get_key/set_key 永久失效 → 主密钥被判为「不可用」→ decrypt_secret 全线走
    /// fallback 返回密文原串 → 所有已保存的密码都登录失败，且没有任何提示指向真正的原因。
    ///
    /// 这里不去断言那个计数器的绝对值（它是全局的，别的测试也可能碰），而是直接验**用户
    /// 能感知的那件事**：连续 panic 若干次之后，下一次正常调用还能不能拿到结果。
    #[test]
    fn a_panicking_call_still_returns_its_slot() {
        // panic 的默认 hook 会往 stderr 打一堆栈，这里是**故意**制造 panic，静音掉免得
        // 测试输出看起来像是出了事。
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        for _ in 0..(KEYCHAIN_MAX_INFLIGHT + 2) {
            let _ = with_timeout(|| -> u8 { panic!("模拟 keyring 内部 panic") });
        }

        std::panic::set_hook(prev_hook);

        assert_eq!(
            with_timeout(|| 42_u8),
            super::KeychainWait::Ok(42),
            "连续 panic 之后名额没还回来——钥匙串会被永久判定为不可用，\
             已保存的密码将全部解不开"
        );
    }

    /// 正常路径：超时封装本身不改变返回值。
    #[test]
    fn normal_calls_pass_the_value_through() {
        assert_eq!(
            with_timeout(|| "ok".to_string()),
            super::KeychainWait::Ok("ok".to_string())
        );
    }

    #[test]
    fn keychain_timeout_or_error_never_mints_a_new_key() {
        use super::{plan_master_key, KeychainRead, MasterKeyPlan};
        assert_eq!(
            plan_master_key(KeychainRead::Transient, false, true),
            MasterKeyPlan::None,
            "钥匙串暂时读不到时，哪怕磁盘上有密文，也绝不能生成新主密钥"
        );
        assert_eq!(
            plan_master_key(KeychainRead::NotFound, false, true),
            MasterKeyPlan::None,
            "钥匙串是空的但磁盘上已有密文：生成新密钥也解不开，只会把局面搞得更糟"
        );
        assert_eq!(
            plan_master_key(KeychainRead::NotFound, false, false),
            MasterKeyPlan::MintNew
        );
        assert_eq!(
            plan_master_key(KeychainRead::Transient, true, true),
            MasterKeyPlan::UseLocal { migrate: false }
        );
        assert_eq!(
            plan_master_key(KeychainRead::Found([0; 32]), false, true),
            MasterKeyPlan::UseKeychain
        );
        assert_eq!(
            plan_master_key(KeychainRead::Unavailable, false, false),
            MasterKeyPlan::MintNew,
            "没有会话总线、磁盘上也没有密文时，应在本地生成主密钥，否则密码永远存不了"
        );
        assert_eq!(
            plan_master_key(KeychainRead::Unavailable, false, true),
            MasterKeyPlan::MintNew,
            "钥匙串已卸掉、只剩旧密文时仍应在本地新建，否则重新填写也无法落盘"
        );
        assert_eq!(
            plan_master_key(KeychainRead::Transient, false, false),
            MasterKeyPlan::None,
            "超时不能生成新密钥，以免覆盖钥匙串里还能用的那把"
        );
        assert_eq!(
            plan_master_key(KeychainRead::Unavailable, true, false),
            MasterKeyPlan::UseLocal { migrate: false }
        );
        assert!(super::may_persist_as_local_backup(true, true));
        assert!(super::may_persist_as_local_backup(false, false));
        assert!(
            !super::may_persist_as_local_backup(false, true),
            "对不上已存密文的钥匙串密钥不能写成本地备份"
        );
    }
}
