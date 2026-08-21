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
                        // 目录状态未知（可能已不存在），刷新一致化
                        refresh_dir: Some(remote_dir.clone()),
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
                // 同上：刷新目标目录，暴露「目录已消失/权限变化」等真实状态
                refresh_dir: Some(remote_dir.clone()),
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
            // 符号链接解析 + 「目标是不是目录」的判定：每个文件只做一次，放在重试循环
            // **之外**——这两次探测与重试次数无关，而且下面清理临时文件时算出来的路径
            // 必须和 upload_file_once 内部用的是同一个。目标是目录属于永久性错误，
            // 重试没有任何意义，直接失败。
            let target = resolve_upload_target(sftp, &rpath).await?;
            let mut attempt = 0u32;
            loop {
                match upload_file_once(
                    sftp,
                    &lpath,
                    &target,
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
                            // 彻底放弃这个文件：清掉它的分段临时文件。留着没有意义——
                            // 续传只在**本次**传输的重试之间生效（见 upload_file_once 的
                            // allow_resume），下一次上传第一件事就是把它截断重写；留着只是
                            // 在用户的远端目录里堆垃圾。目标文件全程未被触碰，无需还原。
                            let _ = sftp.remove_file(&upload_part_path(&target)).await;
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
                // 失败（非用户取消）：目标目录可能已被外部删除/改动、或残留部分写入，
                // 刷新做一致化，避免面板继续显示陈旧缓存（含此前乐观插入的占位）。
                refresh_dir: if cancel.load(Ordering::Relaxed) {
                    None
                } else {
                    Some(remote_dir.clone())
                },
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
            // 失败也刷新目标父目录：写入/换入可能已部分生效，且目标目录可能已不存在
            refresh_dir: Some(
                remote_path
                    .rsplit_once('/')
                    .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
                    .unwrap_or("/")
                    .to_string(),
            ),
        }),
    }
}

/// 解析一个上传目标：跟随符号链接到真实路径，并确认它不是目录。
///
/// **符号链接**：替换必须发生在链接指向的真实文件上（链接本身保留）。旧的直写路径是
/// 写穿链接、改的是目标文件；事务写若不解析，rename 会把链接本身换成一个普通文件，
/// 语义完全变了。与编辑器保存 `sftp_write_atomic` 的处理一致。链接损坏（canonicalize
/// 失败）时就地按普通文件处理——换入后该路径变成普通文件，但至少不留残缺文件。
///
/// **目录**：直接拒绝，不能用一个文件顶替它。旧的直写路径在 `open` 时会自然失败；事务写
/// 不显式挡住的话，rename 会先把整个目录挪到 `.bak`、再把文件放进原位——数据虽然还在
/// 备份里，但用户的目录已经被换成了文件，比报一句错糟糕得多。
///
/// 探测失败（多半是目标不存在，这是新建文件的正常情形）不视为错误：让后续的写入/换入
/// 自然报错即可。**绝不**据此认定"文件不存在"去跳过备份——那是 `upload_file_once` 里
/// 用 rename 结果判定备份的原因。
pub(super) async fn resolve_upload_target(
    sftp: &russh_sftp::client::SftpSession,
    rpath: &str,
) -> anyhow::Result<String> {
    let link_meta = sftp.symlink_metadata(rpath).await;
    let target = match &link_meta {
        Ok(m) if m.is_symlink() => sftp
            .canonicalize(rpath)
            .await
            .unwrap_or_else(|_| rpath.to_string()),
        _ => rpath.to_string(),
    };
    // 目录判定要看**解析之后**的路径：指向目录的链接同样不能被文件顶替。
    let is_dir = match &link_meta {
        Ok(m) if m.is_symlink() => sftp
            .metadata(&target)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false),
        Ok(m) => m.is_dir(),
        Err(_) => false,
    };
    if is_dir {
        anyhow::bail!(
            "{}",
            match crate::i18n::current() {
                crate::i18n::Lang::Zh =>
                    format!("远端同名路径是一个目录，不能用文件覆盖：{target}"),
                crate::i18n::Lang::En => format!(
                    "Remote path is a directory; refusing to replace it with a file: {target}"
                ),
            }
        );
    }
    Ok(target)
}

/// 单个文件上传的**分段临时文件**路径：`<远端目标>.ishell-part`。
///
/// 刻意是**确定性**的（不带随机后缀）：同一次传输的多次重试要能接着上一次的残段续写，
/// 随机名每次都从零开始，等于把断点续传废掉。与下载侧 `<local>.ishellpart` 同一思路。
///
/// 不怕上一轮遗留的同名残段被误当成本次的进度：首次尝试一律 TRUNCATE 从 0 重写
/// （`allow_resume == false`），只有本次传输的重试才会去读它的长度。
pub(super) fn upload_part_path(rpath: &str) -> String {
    format!("{rpath}.ishell-part")
}

/// 上传单个文件（**事务写**）：字节先全部写进 `<目标>.ishell-part`，校验落盘长度无误后，
/// 才把原文件挪到 `.bak`、换入临时文件、删备份——**成功换入之前绝不触碰目标文件**。
///
/// 这里曾经是直接 `open(目标, CREATE|WRITE|TRUNCATE)` 再流式写：目标在第一个字节写进去
/// 之前就已经被截断成 0，此后任何一次断线/取消/本地读错/服务器出错，都会把用户的远端
/// 文件留成一个空的或半截的文件，**原内容不可恢复**。同一个文件里的 `upload_from_mcp`、
/// 编辑器保存的 `sftp_write_atomic`、以及下载侧的 `download_file` 三条路径早就都改成事务
/// 写了，唯独用户日常最常走的这条没跟上。别改回去。
///
/// 续传语义随之调整：续的是**临时文件**的长度，不再是目标文件的长度——目标文件在整个
/// 过程中都保持原样，拿它的长度当偏移毫无意义。
///
/// `target` 必须是 [`resolve_upload_target`] 的返回值（符号链接已解析、已确认不是目录），
/// 由调用方在重试循环**之外**求一次：那两次探测每个文件只该付一遍，而且换入用的临时文件
/// 必须和调用方清理时算出来的是同一个路径。
pub(super) async fn upload_file_once(
    sftp: &russh_sftp::client::SftpSession,
    lpath: &std::path::Path,
    target: &str,
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
    let part = upload_part_path(target);
    // 续传读的是**临时文件**的长度（目标文件全程不动，它的长度与本次进度无关）。
    let start = if allow_resume && local_unchanged {
        let part_size = sftp
            .metadata(&part)
            .await
            .ok()
            .and_then(|m| m.size)
            .unwrap_or(0);
        if part_size > 0 && part_size <= local_size {
            part_size
        } else {
            0
        }
    } else {
        0
    };

    // 1) 写临时文件。续传(start>0)保留已传字节；从头(start==0)则 TRUNCATE，
    //    避免上一轮/上一次遗留的残段留下旧尾部。
    let flags = if start > 0 {
        OpenFlags::CREATE | OpenFlags::WRITE
    } else {
        OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE
    };
    let mut rf = sftp.open_with_flags(&part, flags).await?;
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
    drop(rf); // 换入前确保句柄已关（部分服务端要 CLOSE 才真正落盘）

    // 2) 换入前校验临时文件落盘长度。比对的是**本次实际写出的字节数** `pos`，而不是
    //    枚举阶段记下的 `pinned_size`：本地文件在传输期间被改大/改小是允许的（旧实现
    //    也是读到 EOF 为止），这里要保证的是「服务器确实原样收下了我们发出去的每一个
    //    字节」——个别服务端会静默截断，那正是这道闸要挡的。
    //
    //    只在**确实拿到了 size** 时才判定。服务器不回报 size（退化实现返回 None）时跳过
    //    这道校验直接换入：拿不到长度就无从比对，把「没测到」当成「不相符」会让这类
    //    服务器上的每一次上传都失败——那是比不校验严重得多的回归。不像编辑器保存那样
    //    退回「整文件读回比对」：上传动辄几个 GB，为校验再下载一遍完全不成比例。
    if let Some(part_size) = sftp.metadata(&part).await.ok().and_then(|m| m.size) {
        if part_size != pos {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!(
                        "临时文件落盘校验失败：应为 {pos} 字节，实际 {part_size} 字节，已中止换入（目标文件未改动）"
                    ),
                    crate::i18n::Lang::En => format!(
                        "temp file verify failed: expected {pos} bytes, got {part_size}; swap aborted (target unchanged)"
                    ),
                }
            );
        }
    }

    // 3) 原子换入：SFTP 的 rename 在目标已存在时可能失败（非 POSIX rename），故先把已存在
    //    的原文件挪到 .bak，再换入临时文件，失败则从 .bak 还原——任一步失败原文件都不丢。
    //
    //    **是否已备份由 rename 的结果判定，不依赖可能瞬时失败的 stat**：明确的 NoSuchFile
    //    才是「原文件不存在」；其它错误（权限/网络/被占用）真相不明，此时绝不能当成"新建"
    //    继续往下换入——万一原文件其实存在，就会在没有备份的情况下被直接覆盖。
    let bak = format!("{target}.ishell-bak-{}", super::rand_hex(6));
    let backed_up = match sftp.rename(target, &bak).await {
        Ok(()) => true,
        Err(e) if is_sftp_not_found(&e) => false,
        Err(e) => anyhow::bail!(
            "{}",
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!(
                    "无法确认原文件状态（备份步骤失败，非「文件不存在」）：{e}，已中止换入，原文件未改动"
                ),
                crate::i18n::Lang::En => format!(
                    "could not determine original file state (backup failed, not \"not found\"): {e}; swap aborted, original unchanged"
                ),
            }
        ),
    };
    if let Err(e) = sftp.rename(&part, target).await {
        let restored = backed_up && sftp.rename(&bak, target).await.is_ok();
        anyhow::bail!(
            "{}",
            match (crate::i18n::current(), backed_up, restored) {
                (crate::i18n::Lang::Zh, false, _) => format!("换入失败，原文件未改动：{e}"),
                (crate::i18n::Lang::Zh, true, true) => format!("换入失败，已还原原文件：{e}"),
                (crate::i18n::Lang::Zh, true, false) =>
                    format!("换入失败且未能还原，原文件备份在 {bak}：{e}"),
                (crate::i18n::Lang::En, false, _) =>
                    format!("swap failed, original unchanged: {e}"),
                (crate::i18n::Lang::En, true, true) =>
                    format!("swap failed, original restored: {e}"),
                (crate::i18n::Lang::En, true, false) =>
                    format!("swap failed and not restored; original is at {bak}: {e}"),
            }
        );
    }
    if backed_up {
        let _ = sftp.remove_file(&bak).await; // 换入成功：清理备份
    }
    Ok(())
}

/// 上传事务写的**真·SFTP** 集成测试。
///
/// 这条路径的核心承诺是「换入成功之前绝不触碰目标文件」，而它只有在一个真实 SFTP 服务器上
/// 才成立得了——rename 语义、TRUNCATE 时机、备份/还原都在服务端。纯单元测试碰不到，所以
/// 这里连一个真的 sshd。默认 `#[ignore]`（CI/开发机没有服务器时 `cargo test` 照样全绿），
/// 需要时用环境变量指定服务器再显式跑：
///
/// ```text
/// ISHELL_TEST_SSH_HOST=127.0.0.1 ISHELL_TEST_SSH_PORT=22 \
/// ISHELL_TEST_SSH_USER=me ISHELL_TEST_SSH_KEY=/path/to/id_ed25519 \
///   cargo test --  --ignored live_sftp
/// ```
#[cfg(test)]
mod live_sftp_tests {
    use super::*;

    struct TestHandler;
    impl russh::client::Handler for TestHandler {
        type Error = russh::Error;
        // 测试连的是自己指定的那台机器，指纹校验没有意义
        async fn check_server_key(
            &mut self,
            _key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    struct Env {
        sftp: russh_sftp::client::SftpSession,
        dir: String,
        /// 持有连接句柄，drop 了通道就断
        _handle: russh::client::Handle<TestHandler>,
    }

    impl Env {
        fn path(&self, name: &str) -> String {
            format!("{}/{name}", self.dir)
        }
        async fn read(&self, path: &str) -> Option<Vec<u8>> {
            self.sftp.read(path).await.ok()
        }
        async fn exists(&self, path: &str) -> bool {
            self.sftp.metadata(path).await.is_ok()
        }
    }

    /// 连服务器并建一个一次性工作目录；环境变量没配齐就返回 None（测试自行跳过）。
    async fn connect() -> Option<Env> {
        let host = std::env::var("ISHELL_TEST_SSH_HOST").ok()?;
        let port: u16 = std::env::var("ISHELL_TEST_SSH_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(22);
        let user = std::env::var("ISHELL_TEST_SSH_USER").ok()?;
        let key_path = std::env::var("ISHELL_TEST_SSH_KEY").ok()?;

        let cfg = Arc::new(russh::client::Config::default());
        let mut handle = russh::client::connect(cfg, (host.as_str(), port), TestHandler)
            .await
            .expect("连接测试 sshd");
        let key = russh::keys::load_secret_key(&key_path, None).expect("读取测试私钥");
        let ok = handle
            .authenticate_publickey(
                &user,
                russh::keys::PrivateKeyWithHashAlg::new(
                    Arc::new(key),
                    Some(russh::keys::HashAlg::Sha512),
                ),
            )
            .await
            .expect("公钥认证")
            .success();
        assert!(ok, "测试 sshd 公钥认证失败");

        let channel = handle.channel_open_session().await.expect("开通道");
        channel.request_subsystem(true, "sftp").await.expect("sftp");
        let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .expect("sftp 会话");
        let dir = format!("/tmp/ishell-upload-it-{}", super::super::rand_hex(8));
        sftp.create_dir(dir.clone()).await.expect("建测试目录");
        Some(Env {
            sftp,
            dir,
            _handle: handle,
        })
    }

    fn sink() -> (crate::ssh::UiSink, std::sync::mpsc::Receiver<WorkerEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (sys_tx, _sys_rx) = tokio::sync::watch::channel(None);
        (
            crate::ssh::UiSink::new(tx, egui::Context::default(), Arc::new(sys_tx)),
            rx,
        )
    }

    /// 造一个本地临时文件，返回路径（同一测试内唯一）。
    fn local_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ishell-upload-it-{tag}-{}",
            super::super::rand_hex(6)
        ));
        std::fs::write(&p, bytes).expect("写本地临时文件");
        p
    }

    async fn upload_one(
        env: &Env,
        lpath: &std::path::Path,
        target: &str,
        cancel: bool,
    ) -> anyhow::Result<()> {
        let (s, _rx) = sink();
        let flag = Arc::new(AtomicBool::new(cancel));
        let last = AtomicU64::new(0);
        let sz = std::fs::metadata(lpath).map(|m| m.len()).unwrap_or(0);
        upload_file_once(
            &env.sftp, lpath, target, &flag, 0, 1, &s, &last, false, sz, None,
        )
        .await
    }

    macro_rules! env_or_skip {
        () => {
            match connect().await {
                Some(e) => e,
                None => {
                    eprintln!("跳过：未设置 ISHELL_TEST_SSH_* 环境变量");
                    return;
                }
            }
        };
    }

    /// **这就是那个 bug 的守门人**：上传中途失败时，远端目标文件必须分毫未动。
    ///
    /// 旧实现是 `open(目标, CREATE|WRITE|TRUNCATE)` 再流式写——目标在第一个字节写进去之前
    /// 就已经被截断成 0 字节了，此后任何失败都会把用户的文件留成空的/半截的，原内容不可
    /// 恢复。这里用「一开始就置位的 cancel」精确复现那一刻：旧代码到这里目标已经没了，
    /// 新代码连碰都没碰过它。别把这条断言删了。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_failed_upload_leaves_target_untouched() {
        let env = env_or_skip!();
        let target = env.path("precious.txt");
        let original = b"ORIGINAL CONTENT THAT MUST SURVIVE";
        crate::ssh::sftp::sftp_overwrite(&env.sftp, &target, original)
            .await
            .expect("预置原文件");

        let src = local_file("cancel", &vec![b'x'; 512 * 1024]);
        let res = upload_one(&env, &src, &target, true).await;
        assert!(res.is_err(), "取消状态下上传应当失败");

        assert_eq!(
            env.read(&target).await.as_deref(),
            Some(&original[..]),
            "上传失败后原文件被改动了——事务写被破坏"
        );
        let _ = std::fs::remove_file(&src);
    }

    /// 正常覆盖：内容替换成功，且不留下临时文件/备份文件。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_overwrite_swaps_in_and_leaves_no_litter() {
        let env = env_or_skip!();
        let target = env.path("swap.txt");
        crate::ssh::sftp::sftp_overwrite(&env.sftp, &target, b"old")
            .await
            .expect("预置原文件");

        let src = local_file("swap", b"brand new content");
        upload_one(&env, &src, &target, false).await.expect("上传");

        assert_eq!(env.read(&target).await.as_deref(), Some(&b"brand new content"[..]));
        assert!(
            !env.exists(&upload_part_path(&target)).await,
            "换入后不该留下 .ishell-part"
        );
        let names = env.sftp.read_dir(&env.dir).await.expect("列目录");
        for e in names {
            assert!(
                !e.file_name().contains("ishell-bak"),
                "换入成功后备份必须删掉：{}",
                e.file_name()
            );
        }
        let _ = std::fs::remove_file(&src);
    }

    /// 目标不存在（新建）：正常写入，且不留临时文件。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_new_file_is_created() {
        let env = env_or_skip!();
        let target = env.path("fresh.txt");
        let src = local_file("fresh", b"hello");
        upload_one(&env, &src, &target, false).await.expect("上传");
        assert_eq!(env.read(&target).await.as_deref(), Some(&b"hello"[..]));
        assert!(!env.exists(&upload_part_path(&target)).await);
        let _ = std::fs::remove_file(&src);
    }

    /// 目标是**目录**：必须直接拒绝，且那个目录连同里面的内容都不能被动。
    ///
    /// 旧的直写路径在 open 时会自然失败；事务写若不显式挡住，rename 会把整个目录挪到
    /// .bak、再把文件放进原位——数据虽然还在备份里，但用户的目录已经被换成了文件。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_refuses_to_replace_a_directory() {
        let env = env_or_skip!();
        let target = env.path("adir");
        env.sftp.create_dir(target.clone()).await.expect("建目录");
        let inside = format!("{target}/keepme.txt");
        crate::ssh::sftp::sftp_overwrite(&env.sftp, &inside, b"keep")
            .await
            .expect("目录里放个文件");

        let err = resolve_upload_target(&env.sftp, &target)
            .await
            .expect_err("目标是目录时必须报错");
        assert!(
            format!("{err}").contains("目录") || format!("{err}").contains("directory"),
            "错误信息应说明目标是目录：{err}"
        );
        assert!(
            env.sftp.metadata(&target).await.expect("目录还在").is_dir(),
            "目录被换掉了"
        );
        assert_eq!(env.read(&inside).await.as_deref(), Some(&b"keep"[..]));
    }

    /// 目标是**符号链接**：写穿到链接指向的真实文件，链接本身仍然是链接。
    ///
    /// 旧的直写路径是 open 链接 → 改的是目标文件。事务写若不先解析链接，rename 会把链接
    /// 本身换成一个普通文件，语义就变了。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_writes_through_a_symlink() {
        let env = env_or_skip!();
        let real = env.path("real.txt");
        let link = env.path("link.txt");
        crate::ssh::sftp::sftp_overwrite(&env.sftp, &real, b"old real")
            .await
            .expect("预置真实文件");
        // russh-sftp 的 `symlink(path, target)` 到底哪个是链接、哪个是被指向者，各服务端
        // 实现历来有分歧（OpenSSH 的 SSH_FXP_SYMLINK 当年就把两个字段发反了）。这里不猜：
        // 两种顺序各试一次，谁真的在 `link` 处造出了符号链接就用谁。
        let mut made = env.sftp.symlink(link.clone(), real.clone()).await.is_ok()
            && env
                .sftp
                .symlink_metadata(&link)
                .await
                .map(|m| m.is_symlink())
                .unwrap_or(false);
        if !made {
            let _ = env.sftp.remove_file(&link).await;
            made = env.sftp.symlink(real.clone(), link.clone()).await.is_ok()
                && env
                    .sftp
                    .symlink_metadata(&link)
                    .await
                    .map(|m| m.is_symlink())
                    .unwrap_or(false);
        }
        assert!(made, "两种参数顺序都没能建出符号链接，测试环境有问题");

        let target = resolve_upload_target(&env.sftp, &link)
            .await
            .expect("解析链接");
        let src = local_file("link", b"new real");
        upload_one(&env, &src, &target, false).await.expect("上传");

        assert_eq!(
            env.read(&real).await.as_deref(),
            Some(&b"new real"[..]),
            "应当写穿到链接指向的真实文件"
        );
        assert!(
            env.sftp
                .symlink_metadata(&link)
                .await
                .expect("链接还在")
                .is_symlink(),
            "链接本身被换成普通文件了"
        );
        let _ = std::fs::remove_file(&src);
    }

    /// `plan_copy_move` 的真·SFTP 行为：「目标在不在」判错了，「跳过 / 重命名」就全是空话。
    ///
    /// 被测函数住在 `util.rs`，但活的 SFTP 夹具在这个模块里，就近放这儿。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_copy_move_plan_respects_policy() {
        use super::super::util::{plan_copy_move, CopyDest};
        use crate::proto::ConflictPolicy as P;

        let env = env_or_skip!();
        let dst = env.path("dst");
        env.sftp.create_dir(dst.clone()).await.expect("建目标目录");
        crate::ssh::sftp::sftp_overwrite(&env.sftp, &format!("{dst}/a.txt"), b"OLD")
            .await
            .expect("预置冲突项");
        let srcs = vec![env.path("a.txt"), env.path("b.txt")];
        for src in &srcs {
            crate::ssh::sftp::sftp_overwrite(&env.sftp, src, b"NEW")
                .await
                .expect("预置源文件");
        }

        // 覆盖：一次探测都不做，全部照常移入——默认值的行为一个字都不能变
        let p = plan_copy_move(&env.sftp, &srcs, &dst, P::Overwrite)
            .await
            .expect("覆盖策略不该失败");
        assert_eq!(
            p,
            vec![
                (srcs[0].clone(), CopyDest::Into),
                (srcs[1].clone(), CopyDest::Into)
            ]
        );

        // 跳过：冲突的那项跳过，不冲突的照做
        let p = plan_copy_move(&env.sftp, &srcs, &dst, P::Skip)
            .await
            .expect("跳过策略");
        assert_eq!(p[0].1, CopyDest::Skip, "目标已存在的项应当跳过");
        assert_eq!(p[1].1, CopyDest::Into, "目标不存在的项应当照常");

        // 重命名：冲突的换个不冲突的名字，且那个新名字确实还没被占
        let p = plan_copy_move(&env.sftp, &srcs, &dst, P::Rename)
            .await
            .expect("重命名策略");
        match &p[0].1 {
            CopyDest::Renamed(n) => {
                assert_eq!(n, "a (1).txt");
                assert!(
                    !env.exists(&format!("{dst}/{n}")).await,
                    "换出来的名字必须是没被占的"
                );
            }
            other => panic!("目标已存在的项应当换名，实际是 {other:?}"),
        }
        assert_eq!(p[1].1, CopyDest::Into);
    }

    /// 重试续传：临时文件里已有前缀时，第二次尝试只补剩下的，最终内容完整。
    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_sftp_resume_completes_the_file() {
        let env = env_or_skip!();
        let target = env.path("resume.bin");
        let content: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let src = local_file("resume", &content);
        let meta = std::fs::metadata(&src).expect("本地元数据");
        let sz = meta.len();
        let mtime = meta.modified().ok();

        // 预置一个「已传了一半」的分段文件
        let part = upload_part_path(&target);
        crate::ssh::sftp::sftp_overwrite(&env.sftp, &part, &content[..120_000])
            .await
            .expect("预置半截分段");

        let (s, _rx) = sink();
        let flag = Arc::new(AtomicBool::new(false));
        let last = AtomicU64::new(0);
        upload_file_once(
            &env.sftp, &src, &target, &flag, 0, 1, &s, &last, true, sz, mtime,
        )
        .await
        .expect("续传上传");

        assert_eq!(
            env.read(&target).await.as_deref(),
            Some(&content[..]),
            "续传拼出来的内容与源文件不一致"
        );
        assert!(!env.exists(&part).await, "换入后分段文件应当已被 rename 走");
        let _ = std::fs::remove_file(&src);
    }
}
