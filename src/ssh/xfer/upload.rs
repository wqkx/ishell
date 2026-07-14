//! SFTP 上传。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::proto::{ConflictPolicy, WorkerEvent};

use super::super::sftp::{is_sftp_not_found, join_remote};
use super::super::UiSink;
use super::util::{local_basename, remote_nonexistent, xfer_backoff, XFER_RETRIES};

#[allow(clippy::too_many_arguments)] // 跟 download()（同样 8 个参数）一致，未拆结构体
pub(super) async fn upload(
    sftp: &russh_sftp::client::SftpSession,
    id: u64,
    local: String,
    remote_dir: String,
    remote_name: Option<String>,
    policy: ConflictPolicy,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    // 远端文件名默认按本地路径取（Windows 兼容，处理反斜杠/盘符）；调用方要求改名时
    // （AI/MCP copy_to_remote）用 remote_name 覆盖，不需要为此另建符号链接绕路。
    let name = remote_name.unwrap_or_else(|| local_basename(&local));
    let is_dir = tokio::fs::metadata(&local)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    // 冲突处理：远端目标已存在时，按策略 跳过 / 重命名 / 覆盖。
    // metadata 探测失败不能一律当成"目标不存在"：权限不足、网络超时、SFTP 会话异常
    // 都会导致探测失败，但目标完全可能真实存在——把这些情况误判为"不存在"会直接跳过
    // 冲突检测、覆盖一个本该被保护的文件。只有明确的 NoSuchFile 才当作真的不存在。
    let name = match sftp.metadata(&join_remote(&remote_dir, &name)).await {
        Ok(_) => match policy {
            ConflictPolicy::Skip => {
                sink.send(WorkerEvent::TransferDone {
                    id,
                    ok: true,
                    message: match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("已跳过（远端已存在）：{name}"),
                        crate::i18n::Lang::En => format!("Skipped (exists): {name}"),
                    },
                    refresh_dir: None,
                });
                return;
            }
            ConflictPolicy::Rename => match remote_nonexistent(sftp, &remote_dir, &name, is_dir).await {
                Ok(n) => n,
                Err(e) => {
                    sink.send(WorkerEvent::TransferDone {
                        id,
                        ok: false,
                        message: match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("寻找可用文件名失败：{e}"),
                            crate::i18n::Lang::En => format!("Failed to find an available name: {e}"),
                        },
                        refresh_dir: None,
                    });
                    return;
                }
            },
            ConflictPolicy::Overwrite => name,
        },
        Err(e) if is_sftp_not_found(&e) => name, // 确实不存在：直接用原名
        Err(e) => {
            // 探测失败、原因不明：不能假装"不存在"直接写入，可能正是要保护的那个已有文件。
            sink.send(WorkerEvent::TransferDone {
                id,
                ok: false,
                message: match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("检查远端目标是否存在失败：{e}"),
                    crate::i18n::Lang::En => format!("Failed to check remote target: {e}"),
                },
                refresh_dir: None,
            });
            return;
        }
    };

    let res: anyhow::Result<()> = async {
        // 收集待上传文件：(本地路径, 远程路径, 大小)；目录则递归并记录要创建的远端目录
        let mut files: Vec<(std::path::PathBuf, String, u64)> = Vec::new();
        let mut mkdirs: Vec<String> = Vec::new();
        if is_dir {
            let local_root = std::path::PathBuf::from(&local);
            let root_remote = join_remote(&remote_dir, &name);
            mkdirs.push(root_remote.clone());
            let mut stack = vec![local_root.clone()];
            while let Some(dir) = stack.pop() {
                let mut rd = tokio::fs::read_dir(&dir).await?;
                while let Some(entry) = rd.next_entry().await? {
                    let p = entry.path();
                    let rel = p
                        .strip_prefix(&local_root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let rpath = format!("{root_remote}/{rel}");
                    let ft = entry.file_type().await?;
                    if ft.is_dir() {
                        mkdirs.push(rpath);
                        stack.push(p);
                    } else if ft.is_file() {
                        let sz = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                        files.push((p, rpath, sz));
                    }
                }
            }
        } else {
            let sz = tokio::fs::metadata(&local)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            files.push((
                std::path::PathBuf::from(&local),
                join_remote(&remote_dir, &name),
                sz,
            ));
        }

        let total: u64 = files.iter().map(|f| f.2).sum();
        sink.send(WorkerEvent::TransferStart {
            id,
            name: name.clone(),
            total,
            dir: crate::proto::TransferDir::Upload,
            local: None,
        });

        // 先按深度建好远端目录（父先于子），已存在则忽略
        mkdirs.sort_by_key(|d| d.matches('/').count());
        for d in &mkdirs {
            let _ = sftp.create_dir(d.clone()).await;
        }

        // 逐文件上传：每个文件可断点续传 + 瞬时失败自动重试。
        let mut done_base = 0u64; // 已完成文件累计字节
        let last = AtomicU64::new(0); // 上次上报点（跨文件单调）
        for (lpath, rpath, sz) in files {
            // 本次传输开始前锁定本地文件的大小 + mtime：重试续传时用它验证本地文件在
            // 重试期间没有被改动过——否则续传偏移量建立在一份"已经不是这份内容"的假设上，
            // 可能拼出「旧前缀 + 新后缀」的混合文件（本地源文件被并发修改这种场景，
            // 单靠远端大小续传无法察觉）。
            let pinned_mtime = tokio::fs::metadata(&lpath)
                .await
                .ok()
                .and_then(|m| m.modified().ok());
            let mut attempt = 0u32;
            loop {
                match upload_file_once(
                    sftp,
                    &lpath,
                    &rpath,
                    &cancel,
                    done_base,
                    id,
                    sink,
                    &last,
                    attempt > 0,
                    sz,
                    pinned_mtime,
                )
                .await
                {
                    Ok(()) => break,
                    Err(e) => {
                        if cancel.load(Ordering::Relaxed) || attempt >= XFER_RETRIES {
                            return Err(e);
                        }
                        attempt += 1;
                        tokio::time::sleep(xfer_backoff(attempt)).await;
                    }
                }
            }
            done_base += sz;
            sink.send(WorkerEvent::TransferProgress {
                id,
                done: done_base,
            });
        }
        Ok(())
    }
    .await;
    match res {
        Ok(_) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: true,
            message: match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已上传 {name}"),
                crate::i18n::Lang::En => format!("Uploaded {name}"),
            },
            refresh_dir: Some(remote_dir),
        }),
        Err(e) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("上传失败：{e}"),
                    crate::i18n::Lang::En => format!("Upload failed: {e}"),
                }
            };
            sink.send(WorkerEvent::TransferDone {
                id,
                ok: false,
                message,
                refresh_dir: None,
            });
        }
    }
}

/// 上传单个文件：以远端已有大小为起点续传；带进度节流上报。
pub(super) async fn upload_file_once(
    sftp: &russh_sftp::client::SftpSession,
    lpath: &std::path::Path,
    rpath: &str,
    cancel: &Arc<AtomicBool>,
    done_base: u64,
    id: u64,
    sink: &UiSink,
    last: &AtomicU64,
    allow_resume: bool,
    pinned_size: u64,
    pinned_mtime: Option<std::time::SystemTime>,
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    let local_meta = tokio::fs::metadata(lpath).await.ok();
    let local_size = local_meta.as_ref().map(|m| m.len()).unwrap_or(0);
    // 续传只允许发生在**本次传输的失败重试**（allow_resume）：此时远端内容必然是
    // 本进程刚写入的本地前缀，按大小续写安全。首次尝试一律 TRUNCATE 从 0 全量写——
    // 盲按「远端大小 ≤ 本地大小」续传会把无关同名文件误判为已传前缀
    //（大小恰好相等时一个字节不写就报成功；远端较小时保留错误前缀再续尾部）。
    // 但即使是本次传输的重试，也不能假设本地源文件在两次尝试之间纹丝不动——如果本地
    // 文件被并发修改了（大小或 mtime 变化），远端已写入的那段前缀就不再对应现在要读取的
    // 内容，继续按偏移续写会拼出「旧前缀 + 新后缀」的混合文件。这里用传输开始时锁定的
    // 大小/mtime 校验，任何一项对不上就放弃续传、退回全量重传（更安全，虽然慢一点）。
    let local_unchanged = local_size == pinned_size
        && pinned_mtime.is_some_and(|pinned| {
            local_meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .is_some_and(|now| now == pinned)
        });
    let start = if allow_resume && local_unchanged {
        let remote_size = sftp
            .metadata(rpath)
            .await
            .ok()
            .and_then(|m| m.size)
            .unwrap_or(0);
        if remote_size > 0 && remote_size <= local_size {
            remote_size
        } else {
            0
        }
    } else {
        0
    };

    // 续传(start>0)保留已传字节；从头(start==0)则 TRUNCATE 覆盖，避免残留旧尾部
    let flags = if start > 0 {
        OpenFlags::CREATE | OpenFlags::WRITE
    } else {
        OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE
    };
    let mut rf = sftp.open_with_flags(rpath, flags).await?;
    rf.seek(std::io::SeekFrom::Start(start)).await?;
    let mut lf = tokio::fs::File::open(lpath).await?;
    if start > 0 {
        lf.seek(std::io::SeekFrom::Start(start)).await?;
    }

    let mut buf = vec![0u8; 128 * 1024];
    let mut pos = start;
    loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("canceled");
        }
        let n = lf.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        rf.write_all(&buf[..n]).await?;
        pos += n as u64;
        let done = done_base + pos;
        if done.saturating_sub(last.load(Ordering::Relaxed)) >= 256 * 1024 {
            last.store(done, Ordering::Relaxed);
            sink.send(WorkerEvent::TransferProgress { id, done });
        }
    }
    rf.flush().await?;
    rf.shutdown().await?;
    Ok(())
}
