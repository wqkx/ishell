//! SFTP 上传。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::proto::{ConflictPolicy, WorkerEvent};

use super::super::sftp::join_remote;
use super::super::UiSink;
use super::util::{local_basename, remote_nonexistent, xfer_backoff, XFER_RETRIES};

pub(super) async fn upload(
    sftp: &russh_sftp::client::SftpSession,
    id: u64,
    local: String,
    remote_dir: String,
    policy: ConflictPolicy,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = local_basename(&local); // 本地路径用 Windows 兼容的取名（处理反斜杠/盘符）
    let is_dir = tokio::fs::metadata(&local)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    // 冲突处理：远端目标已存在时，按策略 跳过 / 重命名 / 覆盖
    let name = if sftp
        .metadata(&join_remote(&remote_dir, &name))
        .await
        .is_ok()
    {
        match policy {
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
            ConflictPolicy::Rename => remote_nonexistent(sftp, &remote_dir, &name, is_dir).await,
            ConflictPolicy::Overwrite => name,
        }
    } else {
        name
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
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    let local_size = tokio::fs::metadata(lpath)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    // 续传只允许发生在**本次传输的失败重试**（allow_resume）：此时远端内容必然是
    // 本进程刚写入的本地前缀，按大小续写安全。首次尝试一律 TRUNCATE 从 0 全量写——
    // 盲按「远端大小 ≤ 本地大小」续传会把无关同名文件误判为已传前缀
    //（大小恰好相等时一个字节不写就报成功；远端较小时保留错误前缀再续尾部）。
    let start = if allow_resume {
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
