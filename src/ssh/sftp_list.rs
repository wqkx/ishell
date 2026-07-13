//! SFTP directory listing and symlink resolution.

use std::sync::Arc;

use crate::proto::FileEntry;

use super::join_remote;

/// 读取远程目录，返回（规范化后的绝对路径, 条目列表）。目录在前、按名排序。
pub(in crate::ssh) async fn list_dir(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    path: &str,
) -> Result<(String, Vec<FileEntry>), russh_sftp::client::error::Error> {
    let canon = sftp
        .canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_string());
    let dir = sftp.read_dir(&canon).await?;
    let mut entries = Vec::new();
    for item in dir {
        let name = item.file_name();
        if name == "." || name == ".." {
            continue;
        }
        // read_dir 的元数据为 lstat 语义（针对链接自身），故能据类型位判出 is_link；
        // 链接的真实目标/类型由 resolve_symlinks 通过 stat（跟随）二次解析。
        let meta = item.metadata();
        let perm = meta.permissions.unwrap_or(0);
        let is_dir = meta.is_dir();
        let is_link = perm & 0o170000 == 0o120000;
        entries.push(FileEntry {
            name,
            is_dir,
            is_link,
            size: meta.size.unwrap_or(0),
            mtime: meta.mtime.unwrap_or(0) as u64,
            perm: perm & 0o777,
            owner: meta.uid.map(|u| u.to_string()).unwrap_or_default(),
            link_target: None,
            link_dir: false,
        });
    }
    resolve_symlinks(sftp, &canon, &mut entries).await;
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((canon, entries))
}

/// 跟随解析目录中的符号链接：填入「规范目标路径」「目标是否目录」「目标大小」。
///
/// - 用 `metadata`（stat，跟随链接）判目标类型/大小；用 `canonicalize` 取最终真实路径供展示。
/// - 仅当 stat 成功（目标存在）才视为「已解析」并回填 target；断链则 target 留 `None`（UI 标红提示）。
/// - 文件型链接顺带把 `size` 改为目标大小（lstat 给的是链接本身长度，对用户无意义）。
/// - 并发受 `Semaphore` 限制、总数设上限，避免目录含大量链接时在高延迟链路上拖慢列目录。
pub(in crate::ssh) async fn resolve_symlinks(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    dir: &str,
    entries: &mut [FileEntry],
) {
    /// 单次列目录最多解析的链接数（超出的链接仅显示为链接、不带目标/跟随能力）。
    const MAX_LINKS: usize = 256;
    /// 并发解析的上限（每个链接 1~2 次 SFTP 往返）。
    const CONCURRENCY: usize = 16;

    let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let mut tasks = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !e.is_link {
            continue;
        }
        if tasks.len() >= MAX_LINKS {
            break;
        }
        let full = join_remote(dir, &e.name);
        let sftp = sftp.clone();
        let sem = sem.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            // 先 stat（跟随）；失败即断链，不再 canonicalize（避免回填指向不存在路径）。
            let meta = sftp.metadata(&full).await.ok();
            match meta {
                Some(m) => {
                    let target = sftp.canonicalize(&full).await.ok();
                    (i, target, m.is_dir(), m.size)
                }
                None => (i, None, false, None),
            }
        }));
    }
    for t in tasks {
        if let Ok((i, target, is_dir, size)) = t.await {
            if let Some(e) = entries.get_mut(i) {
                e.link_target = target;
                e.link_dir = is_dir;
                // 文件型链接：展示目标大小（目录链接大小列显示 "-"，无需回填）
                if !is_dir {
                    if let Some(sz) = size {
                        e.size = sz;
                    }
                }
            }
        }
    }
}
