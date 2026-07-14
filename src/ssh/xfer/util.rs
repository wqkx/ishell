//! 传输共用工具：路径、退避、tar 解压、冲突重命名。

use std::time::Duration;

use super::super::sftp::{is_sftp_not_found, join_remote};
use super::super::sh_quote;

pub(super) fn extract_tar_gz(path: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    let f = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    std::fs::create_dir_all(dest)?;
    let dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    for entry in ar.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        if !tar_entry_path_safe(&entry_path) {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh =>
                        format!("拒绝不安全的归档路径：{}", entry_path.display()),
                    crate::i18n::Lang::En =>
                        format!("Refusing unsafe archive path: {}", entry_path.display()),
                }
            );
        }
        // unpack_in 将相对路径落在 dest 下；返回 false 表示被跳过（含 ..）
        if !entry.unpack_in(&dest)? {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh =>
                        format!("归档条目无法安全解压：{}", entry_path.display()),
                    crate::i18n::Lang::En => format!(
                        "Archive entry could not be unpacked safely: {}",
                        entry_path.display()
                    ),
                }
            );
        }
    }
    Ok(())
}

/// 归档条目路径是否可安全解压到目标目录内（相对路径、无 `..`、非绝对）。
pub(super) fn tar_entry_path_safe(p: &std::path::Path) -> bool {
    use std::path::Component;
    if p.as_os_str().is_empty() || p.is_absolute() {
        return false;
    }
    for c in p.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}
/// 下载（单文件或整个目录）并上报进度。大文件用多个并发分段读取流水线化，
/// 抵消 SFTP「单请求等一个往返」的吞吐瓶颈（高延迟链路上提速明显）。
/// 压缩下载一个目录：远端 tar.gz 打包到临时文件 → 单文件并发下载 → 本地解包。
/// 进度按压缩包字节上报。返回 Err 表示不支持/失败（上层回退到逐文件）。
pub(super) fn join_quoted(items: &[String]) -> String {
    let mut s = String::new();
    for p in items {
        s.push_str(&sh_quote(p));
        s.push(' ');
    }
    s
}

/// 直传临时私钥目录的清理守卫：无论正常返回、`?` 早退，还是被取消（future 被 drop），
/// Drop 时都异步清除源主机上的临时私钥目录——避免目标主机私钥残留在源主机（凭据泄露）。
/// 取消路径下本函数栈已被展开，无法 `.await`，故 detach 一个清理任务到当前运行时。
pub(super) fn local_nonexistent(path: &str) -> String {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return path.to_string();
    }
    let is_dir = p.is_dir();
    let parent = p.parent();
    let fname = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = split_name(&fname, is_dir);
    for n in 1..10000u32 {
        let cand_name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let cand = match parent {
            Some(d) => d.join(&cand_name),
            None => std::path::PathBuf::from(&cand_name),
        };
        if !cand.exists() {
            return cand.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// 给远端目录里的名字找一个不冲突的变体。
/// 找一个远端不存在的候选名（"name (1)"、"name (2)" ...）。metadata 探测遇到明确的
/// "不存在"（NoSuchFile）才当作候选可用；权限不足、网络超时、SFTP 会话异常等其它错误
/// 不能被当成"不存在"直接放行——那样可能把一个探测失败、但其实已经存在的候选名
/// 错误地当成安全目标，交给上层决定要不要重试/放弃（返回错误，而不是悄悄继续猜下一个）。
pub(super) async fn remote_nonexistent(
    sftp: &russh_sftp::client::SftpSession,
    dir: &str,
    name: &str,
    is_dir: bool,
) -> anyhow::Result<String> {
    let (stem, ext) = split_name(name, is_dir);
    for n in 1..10000u32 {
        let cand = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        match sftp.metadata(&join_remote(dir, &cand)).await {
            Ok(_) => continue, // 候选已存在，试下一个
            Err(e) if is_sftp_not_found(&e) => return Ok(cand),
            Err(e) => anyhow::bail!("探测远端候选名失败：{e}"),
        }
    }
    Ok(name.to_string())
}

/// 拆分文件名为 (主名, 扩展)；目录或无扩展时扩展为 None（首字符的点不算扩展）。
pub(super) fn split_name(fname: &str, is_dir: bool) -> (String, Option<String>) {
    if is_dir {
        return (fname.to_string(), None);
    }
    match fname.rfind('.') {
        Some(d) if d > 0 => (fname[..d].to_string(), Some(fname[d + 1..].to_string())),
        _ => (fname.to_string(), None),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) const DL_PARALLEL: u64 = 8;
/// 每个分段一次抢占的字节数。
pub(super) const DL_CHUNK: u64 = 1024 * 1024;
/// 单个文件传输遇到瞬时错误时的最大额外重试次数（配合断点续传）。
pub(super) const XFER_RETRIES: u32 = 3;

/// 第 attempt 次重试前的退避时长（300ms·2^n，封顶约 4.8s）。
pub(super) fn xfer_backoff(attempt: u32) -> Duration {
    Duration::from_millis(300u64 * (1u64 << attempt.min(4)))
}

/// 断点信息 sidecar 路径：`<local>.ishellpart`。
pub(super) fn part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart");
    std::path::PathBuf::from(p)
}

/// 下载数据的临时文件路径：`<local>.ishellpart.data`。
/// 数据先写这里，全部完成后 rename 到目标——成功前绝不动目标文件；
/// 取消/失败只留 part 文件，目标（若原本存在）保持完好。
pub(super) fn data_part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart.data");
    std::path::PathBuf::from(p)
}

/// 容纳 n 个分段标志位所需的字节数。
pub(super) fn bitmap_len(n_chunks: u64) -> usize {
    n_chunks.div_ceil(8) as usize
}

/// 下载单个文件：大文件按偏移并发分段读取，定位写入本地，显著提升高延迟链路吞吐。
/// 数据全程写 `<local>.ishellpart.data`，完整后原子 rename 到目标——成功前不动目标文件。
/// `remote_mtime` 参与断点校验（0 = 不允许跨次续传，如临时打包文件）。
pub(super) fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// 取「本地」路径的文件名：同时按 `/` 和 `\` 切分，正确处理 Windows 路径
/// （否则 `C:\Users\x\a.txt` 会被当成整体文件名上传，远端文件名也带上盘符路径）。
pub(super) fn local_basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::tar_entry_path_safe;
    use std::path::Path;

    #[test]
    fn tar_paths_reject_traversal() {
        assert!(tar_entry_path_safe(Path::new("ok/file.txt")));
        assert!(tar_entry_path_safe(Path::new("./nested/a")));
        assert!(!tar_entry_path_safe(Path::new("../escape")));
        assert!(!tar_entry_path_safe(Path::new("a/../../b")));
        assert!(!tar_entry_path_safe(Path::new("/abs/path")));
        assert!(!tar_entry_path_safe(Path::new("")));
    }
}
