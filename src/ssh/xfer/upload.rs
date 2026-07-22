//! SFTP 上传。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::proto::{ConflictPolicy, WorkerEvent};

use super::super::sftp::{create_remote_dir_all, is_sftp_not_found, join_remote, remote_parent};
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

/// 将运行 MCP 代理的调用方机器提供的原始字节流写入远端单文件。
///
/// 这条路径故意不落地 iShell 宿主机：代理进程直接读取工作机文件，字节经受权限保护的
/// MCP Unix socket（或其 SSH 反向转发）流入现有 SFTP 会话。`size` 是协议承诺值，EOF
/// 过早或多余字节都会报错，避免网络中断时把截断内容当成成功文件。
pub(super) async fn upload_from_mcp(
    sftp: &russh_sftp::client::SftpSession,
    id: u64,
    mut source: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    size: u64,
    remote_path: String,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let name = remote_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("file")
        .to_string();
    sink.send(WorkerEvent::TransferStart {
        id,
        name,
        total: size,
        dir: crate::proto::TransferDir::Upload,
        local: None,
    });

    // 目标父目录不存在时先递归建好（copy_from_remote_to_caller/copy_between_sessions 的
    // 中转落盘承诺"目录不存在自动创建"；copy_to_remote 一并受益）。先探一次 metadata，仅在
    // 确实缺失时才逐级建，避免对已存在的深路径做无谓的多次 create_dir 往返。best-effort：
    // 建目录若因权限等失败，紧接着 open_with_flags 会以清晰错误报出，不在这里抢先判定。
    let parent = remote_parent(&remote_path);
    if sftp.metadata(&parent).await.is_err() {
        create_remote_dir_all(sftp, &parent).await;
    }

    // 事务写：先把调用方字节流写进**临时文件**，全部校验通过后才原子换入最终路径。
    // 直接以 TRUNCATE 打开最终路径的旧写法，一旦断线 / 超时 / 源文件变化中途失败，就会在远端
    // 留下一个空/半截文件、破坏原有内容——事务写保证「失败即原文件分毫未动」。
    let tmp = format!("{remote_path}.ishell-mcp-tmp-{}", super::rand_hex(6));
    let result: anyhow::Result<()> = async {
        // 1) 写入临时文件（绝不触碰最终路径）
        let mut remote = sftp
            .open_with_flags(
                &tmp,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
            )
            .await?;
        let mut buffer = vec![0_u8; 128 * 1024];
        let mut written = 0_u64;
        let mut last_reported = 0_u64;
        while written < size {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("canceled");
            }
            let wanted = (size - written).min(buffer.len() as u64) as usize;
            let read = source.read(&mut buffer[..wanted]).await?;
            if read == 0 {
                anyhow::bail!("调用方文件流提前结束：期望 {size} 字节，实际收到 {written} 字节");
            }
            remote.write_all(&buffer[..read]).await?;
            written += read as u64;
            if written.saturating_sub(last_reported) >= 256 * 1024 || written == size {
                last_reported = written;
                sink.send(WorkerEvent::TransferProgress { id, done: written });
            }
        }
        // 再读一个字节，拒绝声明大小之外的尾随数据，防止下一次协议复用时发生串流。
        let mut trailing = [0_u8; 1];
        if source.read(&mut trailing).await? != 0 {
            anyhow::bail!("调用方文件流超过声明的 {size} 字节");
        }
        remote.flush().await?;
        remote.shutdown().await?;
        drop(remote);
        // 2) 换入前校验 tmp 落盘字节数（个别服务器会静默截断；不符则中止，原文件未动）
        let tmp_size = sftp
            .metadata(&tmp)
            .await
            .ok()
            .and_then(|m| m.size)
            .unwrap_or(0);
        if tmp_size != size {
            anyhow::bail!("临时文件落盘校验失败：应为 {size} 字节，实际 {tmp_size} 字节");
        }
        // 3) 原子换入：SFTP rename 在目标已存在时可能失败（非 POSIX rename），故先把已存在的
        //    原文件挪到 .bak，再换入 tmp，失败则从 .bak 还原——任一步失败原文件都不丢。
        let bak = format!("{remote_path}.ishell-mcp-bak-{}", super::rand_hex(6));
        let backed_up = match sftp.rename(&remote_path, &bak).await {
            Ok(()) => true,
            Err(e) if is_sftp_not_found(&e) => false, // 原文件不存在：全新写入，无需备份
            Err(e) => anyhow::bail!("无法确认原文件状态（备份步骤失败，非「文件不存在」）：{e}"),
        };
        if let Err(e) = sftp.rename(&tmp, &remote_path).await {
            // 换入失败：尽力从备份还原原文件
            let restored = backed_up && sftp.rename(&bak, &remote_path).await.is_ok();
            if backed_up && !restored {
                anyhow::bail!("换入失败且未能还原，原文件备份在 {bak}：{e}");
            }
            anyhow::bail!("换入失败，原文件未改动：{e}"); // 新建：目标仍不存在；有备份：已还原
        }
        if backed_up {
            let _ = sftp.remove_file(&bak).await; // 换入成功：删除备份
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        // 写入 / 校验失败：清理半截临时文件（换入成功后 tmp 已不存在，remove 失败无害）
        let _ = sftp.remove_file(&tmp).await;
    }

    match result {
        Ok(()) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: true,
            message: format!("Uploaded {remote_path}"),
            refresh_dir: Some(
                remote_path
                    .rsplit_once('/')
                    .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                    .unwrap_or("/")
                    .to_string(),
            ),
        }),
        Err(error) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: false,
            message: format!("Upload failed: {error}"),
            refresh_dir: None,
        }),
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
