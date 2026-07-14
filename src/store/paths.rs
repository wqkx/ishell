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
/// fsync 父目录。进程崩溃/断电时目标要么旧内容、要么新内容，不出现半截 JSON——但这个
/// 保证建立在两次 fsync 都真的成功的前提上，两处都不能忽略错误，否则"要么旧要么新"
/// 只是一句没有兑现的承诺。临时名带 pid + 单调计数，避免与并发写/前次崩溃残留的临时
/// 文件互相覆盖。
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
        if let Err(e) = f.sync_all() {
            // 内容都不能保证落盘，这份临时文件没有留着的价值，清理掉再把错误交给调用方
            // （调用方看到的是原样的 io::Error，不会误以为写入已经安全完成）。
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    std::fs::rename(&tmp, path)?;
    // 目录项持久化：rename 后 fsync 父目录，断电时新名不至于丢失（Unix；其它平台 no-op）。
    // 这一步失败时目标文件本身已经是新内容了（rename 已经生效），但"断电后目录项也保证
    // 落盘"这条强保证就打了折扣——如实报错，不能假装什么都没发生。
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            d.sync_all()?;
        }
    }
    Ok(())
}
