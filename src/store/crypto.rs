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
/// 钥匙串暂时读不到（含无 D-Bus）且磁盘上已有密文时同样拒绝新建——新旧密钥混存比暂时
/// 存不了密码更糟。真正的全新安装（无密文）才在本地 MintNew。
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
            } else if has_ciphertext {
                // 无 D-Bus / 会话总线暂时不在，与「钥匙串永久卸掉」分不清。磁盘上已有
                // 密文时绝不能 MintNew：新密码会用本地新钥加密，总线恢复后优先选回
                // 钥匙串旧钥，新旧密文就解不开同一把。先拒绝保存，等钥匙串恢复，
                // 或用户清掉 connections.json 后再当全新安装。
                MasterKeyPlan::None
            } else {
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

/// 磁盘上已存密文的盘点结果。区分「确认没有」「确认有且可读」「说不清」——
/// 说不清时绝不能当成「空样本 → 任意密钥都验证通过」。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CiphertextInventory {
    /// 没有配置文件，或合法 JSON 里没有任何 `enc:v1:` 字段。
    Empty,
    /// 已解析出的密文字符串（至少一条）。
    Samples(Vec<String>),
    /// 文件在但读不到 / JSON 坏了却仍能看见 `enc:v1:`：验证结果未知。
    Unknown,
}

impl CiphertextInventory {
    fn needs_existing_key(&self) -> bool {
        !matches!(self, Self::Empty)
    }
}

/// 从已读到的配置文本归类。供单测直接喂损坏内容，不依赖真实路径。
fn inventory_from_text(text: &str) -> CiphertextInventory {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Array(arr)) => {
            let mut out = Vec::new();
            for item in &arr {
                for key in ["password", "passphrase", "jump_password", "jump_passphrase"] {
                    if let Some(s) = item.get(key).and_then(|x| x.as_str()) {
                        if s.starts_with(ENC_PREFIX) {
                            out.push(s.to_string());
                        }
                    }
                }
            }
            if out.is_empty() {
                CiphertextInventory::Empty
            } else {
                CiphertextInventory::Samples(out)
            }
        }
        Ok(_) => {
            // 合法 JSON 但不是连接数组：若正文里仍有密文前缀，按未知处理，免得漏保护。
            if text.contains(ENC_PREFIX) {
                CiphertextInventory::Unknown
            } else {
                CiphertextInventory::Empty
            }
        }
        Err(_) => {
            if text.contains(ENC_PREFIX) {
                CiphertextInventory::Unknown
            } else {
                CiphertextInventory::Empty
            }
        }
    }
}

fn inventory_saved_ciphertexts() -> CiphertextInventory {
    let Some(path) = super::paths::config_path() else {
        return CiphertextInventory::Empty;
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => inventory_from_text(&text),
        Err(_) => {
            // 读都读不到：无法确认有没有密文。文件存在时按未知处理，避免把钥匙串密钥
            // 当成「已验证」去盖掉本地备份。
            if path.exists() {
                CiphertextInventory::Unknown
            } else {
                CiphertextInventory::Empty
            }
        }
    }
}

fn saved_secrets_need_existing_key() -> bool {
    inventory_saved_ciphertexts().needs_existing_key()
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

/// 能解开**全部**已存密文才算可用。只抽第一条会漏掉「新旧密钥混存」。
fn key_opens_all_samples(k: &[u8; 32], samples: &[String]) -> bool {
    !samples.is_empty() && samples.iter().all(|s| decrypt_with_key(k, s).is_some())
}

fn key_opens_inventory(k: &[u8; 32], inv: &CiphertextInventory) -> bool {
    match inv {
        // 确认没有密文：任意密钥都「兼容」（首次安装）。
        CiphertextInventory::Empty => true,
        // 读不清时不能当验证成功——否则 persist_local_backup 会用未验证的钥匙串密钥
        // 覆盖仍能解开旧密文的本地备份。
        CiphertextInventory::Unknown => false,
        CiphertextInventory::Samples(samples) => key_opens_all_samples(k, samples),
    }
}

fn key_opens_saved_secrets(k: &[u8; 32]) -> bool {
    key_opens_inventory(k, &inventory_saved_ciphertexts())
}

/// 解不开已存密文的钥匙串密钥不能写成本地备份：以后钥匙串一超时就会稳定地用这把废钥匙。
fn may_persist_as_local_backup(key_opens_secrets: bool, has_ciphertext: bool) -> bool {
    key_opens_secrets || !has_ciphertext
}

fn persist_local_backup(path: &std::path::Path, k: &[u8; 32]) {
    let inv = inventory_saved_ciphertexts();
    let key_opens = key_opens_inventory(k, &inv);
    if !may_persist_as_local_backup(key_opens, inv.needs_existing_key()) {
        log::warn!("钥匙串密钥解不开已存密码（或配置无法验证），不把它写成本地备份");
        return;
    }
    if let Some(existing) = read_local_key(path) {
        if existing == *k {
            return;
        }
        // 本地还是旧密钥、且它能解开已存密码：绝不覆盖，那是唯一的找回手段。
        if key_opens_inventory(&existing, &inv) && !key_opens {
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

/// 拿不到跨进程初始化锁时，禁止一切会改写共享密钥材料的动作。
/// 只读已有钥匙串/本地密钥仍可；MintNew 与向钥匙串迁移必须等锁。
fn demote_plan_without_init_lock(plan: MasterKeyPlan) -> MasterKeyPlan {
    match plan {
        MasterKeyPlan::MintNew => MasterKeyPlan::None,
        MasterKeyPlan::UseLocal { migrate: true } => MasterKeyPlan::UseLocal { migrate: false },
        other => other,
    }
}

/// 跨进程互斥：多实例同时首次初始化时串行化 mint / 写本地 key / 写钥匙串。
/// 锁文件本身不是密钥；进程退出或 drop 后自动释放。
struct MasterKeyDirLock {
    /// 持有打开的锁文件句柄；关闭即解锁。字段本身不被读。
    _file: std::fs::File,
}

fn lock_master_key_dir(dir: &std::path::Path) -> Option<MasterKeyDirLock> {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("key.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .ok()?;
    // std::fs::File::lock：阻塞直到拿到独占锁（跨进程）。
    file.lock().ok()?;
    Some(MasterKeyDirLock { _file: file })
}

/// 本地 key：已有则采用，否则写入候选并**读回**磁盘上的最终内容。
/// 调用方须已持有 `lock_master_key_dir`（或明确知道不会并发创建）。
fn install_or_adopt_local_key(path: &std::path::Path, candidate: &[u8; 32]) -> Option<[u8; 32]> {
    if let Some(existing) = read_local_key(path) {
        return Some(existing);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 锁下再看一次：等锁期间别人可能已经写好了。
    if let Some(existing) = read_local_key(path) {
        return Some(existing);
    }
    write_key_file(path, candidate).ok()?;
    read_local_key(path)
}

/// `may_create`：仅在已持有目录锁时为 true。否则只采用已有本地/钥匙串密钥，
/// 绝不 `set_password` / 新建本地 key——配置目录只读但钥匙串可写时，无锁并发
/// mint 会互相覆盖钥匙串条目。
fn mint_new_key(path: &std::path::Path, may_create: bool) -> Option<[u8; 32]> {
    // 等锁期间其它进程可能已经完成初始化——先采用现成的，不要再掷一次骰子。
    if let Some(existing) = read_local_key(path) {
        let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
        return Some(existing);
    }
    if let KeychainRead::Found(k) = keychain_read() {
        if may_create {
            persist_local_backup(path, &k);
        }
        let _ = KEY_STORAGE.set(KeyStorage::Keychain);
        return Some(k);
    }

    if !may_create {
        log::error!(
            "无法取得主密钥目录锁，又没有现成的本地/钥匙串密钥；拒绝新建以免并发覆盖"
        );
        return None;
    }

    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).ok()?;
    if keychain_set_key(&k) {
        // set 成功不等于我们赢了：并发写入时以读回为准。
        match keychain_read() {
            KeychainRead::Found(got) => {
                persist_local_backup(path, &got);
                let _ = KEY_STORAGE.set(KeyStorage::Keychain);
                return Some(got);
            }
            _ => {
                log::warn!("钥匙串写入后读不回，改走本地主密钥文件");
            }
        }
    }
    let adopted = install_or_adopt_local_key(path, &k)?;
    let _ = KEY_STORAGE.set(KeyStorage::LocalFile);
    Some(adopted)
}

fn compute_master_key() -> Option<[u8; 32]> {
    let dir = match config_dir() {
        Some(dir) => dir,
        None => {
            let _ = KEY_STORAGE.set(KeyStorage::None);
            return None;
        }
    };
    let path = dir.join("key");
    let dir_lock = lock_master_key_dir(&dir);
    let have_init_lock = dir_lock.is_some();
    if !have_init_lock {
        log::warn!(
            "无法锁定主密钥目录（{}）：只使用已有密钥，不会新建或写入钥匙串",
            dir.display()
        );
    }
    let local = read_local_key(&path);
    let has_ciphertext = saved_secrets_need_existing_key();
    let read = keychain_read();
    let keychain_key = match read {
        KeychainRead::Found(k) => Some(k),
        _ => None,
    };
    let plan = {
        let p = plan_master_key(read, local.is_some(), has_ciphertext);
        if have_init_lock {
            p
        } else {
            demote_plan_without_init_lock(p)
        }
    };
    match plan {
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
            // 无锁时不写本地备份：写备份本身也需要跨进程协调。
            if have_init_lock {
                persist_local_backup(&path, &k);
            }
            let _ = KEY_STORAGE.set(KeyStorage::Keychain);
            Some(k)
        }
        MasterKeyPlan::UseLocal { migrate } => {
            let k = local.expect("has_local 才走 UseLocal");
            // migrate 仅在 have_init_lock 时仍为 true（见 demote_plan_without_init_lock）。
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
        MasterKeyPlan::MintNew => mint_new_key(&path, have_init_lock),
        MasterKeyPlan::None => {
            log::error!(
                "主密钥不可用（钥匙串暂时读不到，又没有本地备份，或无法取得初始化锁）。已存密码不会被一把新密钥毁掉。"
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
            MasterKeyPlan::None,
            "无 D-Bus 且磁盘上已有密文时不能 MintNew，否则新旧密文会分属两把钥"
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

    #[test]
    fn key_must_open_every_saved_ciphertext_not_just_the_first() {
        use chacha20poly1305::aead::Aead;
        use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};

        fn seal(k: &[u8; 32], plain: &str) -> String {
            let c = ChaCha20Poly1305::new(Key::from_slice(k));
            let nonce = [7u8; 12];
            let ct = c.encrypt(Nonce::from_slice(&nonce), plain.as_bytes()).unwrap();
            let mut blob = nonce.to_vec();
            blob.extend_from_slice(&ct);
            format!(
                "{}{}",
                super::ENC_PREFIX,
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, blob)
            )
        }

        let k_old = [1u8; 32];
        let k_new = [2u8; 32];
        let samples = vec![seal(&k_old, "old-pw"), seal(&k_new, "new-pw")];
        assert!(
            !super::key_opens_all_samples(&k_old, &samples),
            "只能解开第一条时不能算可用"
        );
        assert!(!super::key_opens_all_samples(&k_new, &samples));
        assert!(super::key_opens_all_samples(&k_old, &samples[..1]));
        assert!(
            !super::key_opens_all_samples(&k_old, &[]),
            "空切片不是 Empty 盘点：Samples 路径下没有可验证条目应失败"
        );
        assert!(super::key_opens_inventory(
            &k_old,
            &super::CiphertextInventory::Empty
        ));
        assert!(!super::key_opens_inventory(
            &k_old,
            &super::CiphertextInventory::Unknown
        ));
    }

    #[test]
    fn corrupt_config_with_ciphertext_is_unknown_not_verified_empty() {
        use super::{
            inventory_from_text, key_opens_inventory, may_persist_as_local_backup,
            CiphertextInventory,
        };

        let corrupt = format!(
            "{{ not json but has {}abc...",
            super::ENC_PREFIX
        );
        let inv = inventory_from_text(&corrupt);
        assert_eq!(inv, CiphertextInventory::Unknown);
        assert!(inv.needs_existing_key());
        assert!(
            !key_opens_inventory(&[9u8; 32], &inv),
            "配置损坏时不能把任意密钥当成验证通过"
        );
        assert!(
            !may_persist_as_local_backup(false, true),
            "未验证的钥匙串密钥不得覆盖本地备份"
        );

        assert_eq!(inventory_from_text("[]"), CiphertextInventory::Empty);
        assert_eq!(
            inventory_from_text(r#"[{"host":"x","password":"plain"}]"#),
            CiphertextInventory::Empty
        );
        let sealed = format!(
            r#"[{{"password":"{}AAAA"}}]"#,
            super::ENC_PREFIX
        );
        assert!(matches!(
            inventory_from_text(&sealed),
            CiphertextInventory::Samples(s) if s.len() == 1
        ));
    }

    #[test]
    fn corrupt_config_must_not_let_unverified_key_overwrite_local_backup() {
        use super::{
            install_or_adopt_local_key, inventory_from_text, key_opens_inventory,
            may_persist_as_local_backup, read_local_key, write_key_file, CiphertextInventory,
        };

        let dir = std::env::temp_dir().join(format!(
            "ishell-corrupt-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("key");
        let good = [0x11u8; 32];
        write_key_file(&key_path, &good).unwrap();

        // 模拟「损坏配置里有密文」：盘点 Unknown → 钥匙串候选不得落盘覆盖。
        let inv = inventory_from_text(&format!("broken {}", super::ENC_PREFIX));
        assert_eq!(inv, CiphertextInventory::Unknown);
        let keychain_candidate = [0x22u8; 32];
        let opens = key_opens_inventory(&keychain_candidate, &inv);
        assert!(!opens);
        assert!(!may_persist_as_local_backup(opens, inv.needs_existing_key()));
        // 调用方若遵守 may_persist，就不会 write；这里直接确认本地仍是 good。
        assert_eq!(read_local_key(&key_path), Some(good));

        // adopt 已有文件时也不会改掉它
        assert_eq!(
            install_or_adopt_local_key(&key_path, &keychain_candidate),
            Some(good)
        );
        assert_eq!(read_local_key(&key_path), Some(good));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_local_key_install_converges_under_dir_lock() {
        use std::sync::{Arc, Barrier};

        use super::{install_or_adopt_local_key, lock_master_key_dir, read_local_key};

        let dir = std::env::temp_dir().join(format!(
            "ishell-key-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for i in 0..2u8 {
            let dir = dir.clone();
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let _lock = lock_master_key_dir(&dir).expect("拿目录锁");
                let candidate = [i.wrapping_add(1); 32];
                install_or_adopt_local_key(&path, &candidate)
            }));
        }
        let a = handles.pop().unwrap().join().unwrap().expect("a");
        let b = handles.pop().unwrap().join().unwrap().expect("b");
        assert_eq!(a, b, "两进程/线程应收敛到同一把最终密钥");
        assert_eq!(read_local_key(&path), Some(a));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_init_lock_mint_and_migrate_are_refused() {
        use super::{demote_plan_without_init_lock, MasterKeyPlan};
        assert_eq!(
            demote_plan_without_init_lock(MasterKeyPlan::MintNew),
            MasterKeyPlan::None,
            "无锁时绝不能新建钥匙串/本地主密钥"
        );
        assert_eq!(
            demote_plan_without_init_lock(MasterKeyPlan::UseLocal { migrate: true }),
            MasterKeyPlan::UseLocal { migrate: false },
            "无锁时不能往钥匙串迁移（写入）"
        );
        assert_eq!(
            demote_plan_without_init_lock(MasterKeyPlan::UseKeychain),
            MasterKeyPlan::UseKeychain
        );
        assert_eq!(
            demote_plan_without_init_lock(MasterKeyPlan::UseLocal { migrate: false }),
            MasterKeyPlan::UseLocal { migrate: false }
        );
        assert_eq!(
            demote_plan_without_init_lock(MasterKeyPlan::None),
            MasterKeyPlan::None
        );
    }

    #[test]
    fn mint_without_create_permission_only_adopts_existing() {
        use super::{mint_new_key, read_local_key, write_key_file};

        let dir = std::env::temp_dir().join(format!(
            "ishell-mint-nocreate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");

        // 没有任何本地密钥：may_create=false 不得新建本地文件，也不得 set_password。
        // 若本机钥匙串里已有条目，允许只读 adopt（返回 Some），但磁盘上仍不能出现 key。
        let adopted = mint_new_key(&path, false);
        assert!(
            read_local_key(&path).is_none(),
            "无锁时不得新建本地 key 文件"
        );
        if adopted.is_none() {
            // 钥匙串也空：必须彻底放弃
        } else {
            // 来自已有钥匙串——再次调用仍应得到同一把，且仍不落盘
            assert_eq!(mint_new_key(&path, false), adopted);
            assert!(read_local_key(&path).is_none());
        }

        let existing = [0xABu8; 32];
        write_key_file(&path, &existing).unwrap();
        assert_eq!(mint_new_key(&path, false), Some(existing));
        assert_eq!(read_local_key(&path), Some(existing));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn lock_fails_when_config_dir_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;

        use super::lock_master_key_dir;

        let dir = std::env::temp_dir().join(format!(
            "ishell-ro-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&dir, perms).unwrap();

        assert!(
            lock_master_key_dir(&dir).is_none(),
            "只读配置目录应拿不到 key.lock，调用方须 demote MintNew"
        );

        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(&dir, perms);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两个**独立进程**在目录锁下安装本地 key，应收敛到同一把（不是同进程线程）。
    #[test]
    fn two_os_processes_converge_on_one_local_key() {
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!(
            "ishell-proc-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("key");
        let marker = dir.join("ready");

        // 子进程用与 `lock_master_key_dir` + `install_or_adopt_local_key` 相同的
        // flock→若无则写入→读回 协议（Python 便于起两个真 OS 进程，不依赖测试 bin hook）。
        let py = format!(
            r#"
import fcntl, os, pathlib, sys, time
d = pathlib.Path({dir:?})
key, lockp, ready = d / "key", d / "key.lock", d / "ready"
while not ready.exists():
    time.sleep(0.005)
fd = os.open(lockp, os.O_CREAT | os.O_RDWR, 0o600)
fcntl.flock(fd, fcntl.LOCK_EX)
if key.exists():
    data = key.read_bytes()
else:
    data = os.urandom(32)
    tmp = d / f"tmp.{{os.getpid()}}"
    tmp.write_bytes(data)
    os.chmod(tmp, 0o600)
    tmp.replace(key)
    data = key.read_bytes()
sys.stdout.buffer.write(data)
fcntl.flock(fd, fcntl.LOCK_UN)
os.close(fd)
"#,
            dir = dir
        );
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                Command::new("python3")
                    .arg("-c")
                    .arg(&py)
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn python child"),
            );
        }
        std::fs::write(&marker, b"1").unwrap();
        let mut outs = Vec::new();
        for c in children {
            let out = c.wait_with_output().expect("child");
            assert!(out.status.success(), "child failed: {:?}", out.status);
            assert_eq!(out.stdout.len(), 32, "child must print 32-byte key");
            outs.push(out.stdout);
        }
        assert_eq!(outs[0], outs[1], "两独立进程应收敛到同一本地密钥");
        assert_eq!(
            super::read_local_key(&key_path).as_ref().map(|k| k.as_slice()),
            Some(outs[0].as_slice())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
