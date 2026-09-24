use std::path::PathBuf;

pub(super) fn config_dir() -> Option<PathBuf> {
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

/// 崩溃/内部错误日志。GUI 从 `.desktop` 启动时 stderr 用户根本看不到，出事只剩「它崩了」
/// 一句话——落一份文件是让下一次事故可查的最低成本手段。
pub fn crash_log_path() -> Option<PathBuf> {
    Some(config_dir()?.join("crash.log"))
}

pub(super) fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("connections.json"))
}

/// 用户主目录（跨平台）。
pub(super) fn home_dir() -> Option<PathBuf> {
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
pub(super) fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

/// 把已完整写好的临时文件换到目标路径。
///
/// - Unix：`rename(2)` 可覆盖，进程崩溃时目标要么旧要么新。
/// - Windows：`MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 同 inode 级替换，避免
///   「先挪走旧文件再移入新文件」两次改名之间目标路径缺失、密钥卡在旁路名的窗口。
pub(super) fn replace_file(tmp: &std::path::Path, path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(tmp, path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        fn wide(p: &std::path::Path) -> Vec<u16> {
            p.as_os_str().encode_wide().chain(Some(0)).collect()
        }
        let from = wide(tmp);
        let to = wide(path);
        // SAFETY: 两个 NUL 结尾的宽路径，API 只读不持有指针。
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(all(not(unix), not(windows)))]
    {
        // 其它平台尽力而为：能覆盖最好，否则旁路换入。
        match std::fs::rename(tmp, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                let stale = path.with_extension(format!(
                    "ishell-stale.{}",
                    std::process::id()
                ));
                let _ = std::fs::remove_file(&stale);
                if path.exists() {
                    std::fs::rename(path, &stale)?;
                }
                match std::fs::rename(tmp, path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&stale);
                        Ok(())
                    }
                    Err(e) => {
                        let _ = std::fs::rename(&stale, path);
                        let _ = std::fs::remove_file(tmp);
                        Err(e)
                    }
                }
            }
        }
    }
}

/// 原子写文本文件：写同目录唯一命名的临时文件 → fsync 文件 → rename 替换 →
/// fsync 父目录。进程崩溃/断电时目标要么旧内容、要么新内容，不出现半截 JSON——但这个
/// 保证建立在两次 fsync 都真的成功的前提上，两处都不能忽略错误，否则"要么旧要么新"
/// 只是一句没有兑现的承诺。临时名带 pid + 单调计数，避免与并发写/前次崩溃残留的临时
/// 文件互相覆盖。
///
/// Unix 上临时文件以 0600 创建，`rename` 保留该 mode，避免凭据类内容先按 umask 落成 0644。
pub(super) fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    write_atomic_bytes(path, contents.as_bytes())
}

/// 与 [`write_atomic`] 相同，写入任意字节（本地主密钥备份等）。
pub(super) fn write_atomic_bytes(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = format!(
        "ishell-tmp.{}.{}",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(uniq);
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = create_private_tmp(&tmp)?;
        f.write_all(contents)?;
        if let Err(e) = f.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    replace_file(&tmp, path)?;
    // 目录项持久化：rename 后 fsync 父目录，断电时新名不至于丢失（Unix；其它平台 no-op）。
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            d.sync_all()?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_tmp(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_tmp(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::{write_atomic, write_atomic_bytes};

    #[test]
    fn write_atomic_can_replace_existing_target() {
        let dir = std::env::temp_dir().join(format!(
            "ishell-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.json");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        write_atomic_bytes(&path, b"third").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
