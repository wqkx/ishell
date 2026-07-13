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

/// 原子写文本文件：写同目录唯一命名的临时文件 → fsync 文件 → rename 替换 →
/// best-effort fsync 父目录。进程崩溃/断电时目标要么旧内容、要么新内容，不出现半截 JSON。
/// 临时名带 pid + 单调计数，避免与并发写/前次崩溃残留的临时文件互相覆盖。
pub(super) fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let uniq = format!(
        "ishell-tmp.{}.{}",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_extension(uniq);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)?;
    // 目录项持久化：rename 后 fsync 父目录，断电时新名不至于丢失（Unix；其它平台 no-op）
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}
