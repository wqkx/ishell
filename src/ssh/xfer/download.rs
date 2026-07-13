//! SFTP 下载：单文件并发分段、目录压缩下载。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::Handle;

use crate::proto::{ConflictPolicy, WorkerEvent};

use super::super::auth::{exec_status, ClientHandler};
use super::super::sftp::{join_remote, remote_parent};
use super::super::{sh_quote, UiSink};
#[path = "download_io.rs"]
mod download_io;

use super::rand_hex;
use super::util::{
    basename, bitmap_len, data_part_path, extract_tar_gz, local_nonexistent, part_path,
    xfer_backoff, DL_CHUNK, DL_PARALLEL, XFER_RETRIES,
};

pub(super) async fn download_dir_compressed(
    handle: &Arc<Handle<ClientHandler>>,
    sftp: &Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote: &str,
    local: &str,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let name = basename(remote);
    let parent = remote_parent(remote);
    // 随机文件名：防止可预测路径在共享 /tmp 上被预置 symlink 抢占（竞态/越权写）
    let tmp_remote = format!("/tmp/.ishell_dl_{id}_{}.tar.gz", rand_hex(8));

    // 先登记传输行（total 未知），并把「打包中…」作为阶段提示上报——
    // 大目录 tar 打包可能耗时数十秒，此前 UI 一片空白，用户以为卡死。
    sink.send(WorkerEvent::TransferStart {
        id,
        name: name.clone(),
        total: 0,
        dir: crate::proto::TransferDir::Download,
        local: Some(local.to_string()),
    });
    sink.send(WorkerEvent::TransferNote {
        id,
        note: crate::i18n::tr("打包中…", "Packing…").into(),
    });

    // 远端打包（czf：gzip 默认级别；-C 进入父目录，仅打包目标目录名）
    let cmd = format!(
        "tar czf {} -C {} {}",
        sh_quote(&tmp_remote),
        sh_quote(&parent),
        sh_quote(&name)
    );
    let (code, err) = exec_status(handle, &cmd).await?;
    if code != 0 {
        let _ = exec_status(handle, &format!("rm -f {}", sh_quote(&tmp_remote))).await;
        anyhow::bail!(
            "{}",
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("tar 打包失败（{code}）：{err}"),
                crate::i18n::Lang::En => format!("tar pack failed ({code}): {err}"),
            }
        );
    }
    let size = sftp
        .metadata(&tmp_remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    // 打包完成：更新真实总量并清除阶段提示，进入正常字节进度
    sink.send(WorkerEvent::TransferStart {
        id,
        name: name.clone(),
        total: size,
        dir: crate::proto::TransferDir::Download,
        local: Some(local.to_string()),
    });
    sink.send(WorkerEvent::TransferNote {
        id,
        note: String::new(),
    });

    // 下载压缩包到本地临时文件（并发分段 + 进度）
    let local_tgz = std::path::PathBuf::from(format!("{local}.ishelldl.{}.tgz", rand_hex(6)));
    let done = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let prog = {
        let (d, s, st) = (done.clone(), sink.clone(), stop.clone());
        tokio::spawn(async move {
            let mut last = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let v = d.load(Ordering::Relaxed);
                if v != last {
                    last = v;
                    s.send(WorkerEvent::TransferProgress { id, done: v });
                }
                if st.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
    };
    let dl = download_file(sftp, &tmp_remote, &local_tgz, size, 0, cancel, &done).await; // 临时打包文件不跨次续传（mtime=0）
    stop.store(true, Ordering::Relaxed);
    let _ = prog.await;
    // 清理远端临时包（无论成败）
    let _ = exec_status(handle, &format!("rm -f {}", sh_quote(&tmp_remote))).await;
    dl?;
    sink.send(WorkerEvent::TransferProgress { id, done: size });

    // 本地解包到 local 的父目录（归档顶层即目录名，解包后落在 local）
    let dest = std::path::Path::new(local)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let tgz = local_tgz.clone();
    // 本地解包也可能耗时（大量小文件），提示「解包中…」避免进度条满了却迟迟不完成
    sink.send(WorkerEvent::TransferNote {
        id,
        note: crate::i18n::tr("解包中…", "Extracting…").into(),
    });
    tokio::task::spawn_blocking(move || extract_tar_gz(&tgz, &dest)).await??;
    sink.send(WorkerEvent::TransferNote {
        id,
        note: String::new(),
    });
    let _ = std::fs::remove_file(&local_tgz);
    Ok(())
}

/// 把一组路径用 POSIX 单引号转义后空格拼接（末尾带一个空格），用于安全嵌入 shell 命令。
pub(super) async fn download(
    handle: Arc<Handle<ClientHandler>>,
    sftp: Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote: String,
    local: String,
    policy: ConflictPolicy,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = basename(&remote);
    let is_dir = sftp
        .metadata(&remote)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    // 冲突处理：本地目标已存在时，按策略 跳过 / 重命名 / 覆盖
    let local = if std::path::Path::new(&local).exists() {
        match policy {
            ConflictPolicy::Skip => {
                sink.send(WorkerEvent::TransferDone {
                    id,
                    ok: true,
                    message: match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("已跳过（本地已存在）：{name}"),
                        crate::i18n::Lang::En => format!("Skipped (exists): {name}"),
                    },
                    refresh_dir: None,
                });
                return;
            }
            ConflictPolicy::Rename => local_nonexistent(&local),
            ConflictPolicy::Overwrite => local,
        }
    } else {
        local
    };

    // 目录优先走压缩下载（远端 tar.gz 打包 → 单文件并发下载 → 本地解包），
    // 大幅减少多小文件的逐个 SFTP 往返；任何失败则回退到逐文件下载。
    if is_dir {
        match download_dir_compressed(&handle, &sftp, id, &remote, &local, sink, &cancel).await {
            Ok(()) => {
                sink.send(WorkerEvent::TransferDone {
                    id,
                    ok: true,
                    message: match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("已下载 {name}"),
                        crate::i18n::Lang::En => format!("Downloaded {name}"),
                    },
                    refresh_dir: None,
                });
                return;
            }
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    sink.send(WorkerEvent::TransferDone {
                        id,
                        ok: false,
                        message: crate::i18n::tr("已取消", "Canceled").into(),
                        refresh_dir: None,
                    });
                    return;
                }
                log::warn!("压缩下载失败，回退逐文件：{e}");
            }
        }
    }

    let res: anyhow::Result<()> = async {
        // 收集待下载文件：(远程绝对路径, 本地路径, 大小)
        let mut files: Vec<(String, std::path::PathBuf, u64)> = Vec::new();
        if is_dir {
            // 迭代遍历整棵目录树（避免 async 递归）
            let mut stack = vec![remote.clone()];
            while let Some(dir) = stack.pop() {
                let rd = sftp.read_dir(&dir).await?;
                for item in rd {
                    let n = item.file_name();
                    if n == "." || n == ".." {
                        continue;
                    }
                    let full = join_remote(&dir, &n);
                    let meta = item.metadata();
                    if meta.is_dir() {
                        stack.push(full);
                    } else {
                        let rel = full
                            .strip_prefix(remote.as_str())
                            .unwrap_or(&full)
                            .trim_start_matches('/');
                        files.push((
                            full.clone(),
                            std::path::Path::new(&local).join(rel),
                            meta.size.unwrap_or(0),
                        ));
                    }
                }
            }
        } else {
            let sz = sftp
                .metadata(&remote)
                .await
                .ok()
                .and_then(|m| m.size)
                .unwrap_or(0);
            files.push((remote.clone(), std::path::PathBuf::from(&local), sz));
        }

        let total: u64 = files.iter().map(|f| f.2).sum();
        sink.send(WorkerEvent::TransferStart {
            id,
            name: name.clone(),
            total,
            dir: crate::proto::TransferDir::Download,
            local: Some(local.clone()),
        });

        // 累计已下载字节（多任务共享）+ 周期性上报进度
        let done = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let prog = {
            let (d, s, st) = (done.clone(), sink.clone(), stop.clone());
            tokio::spawn(async move {
                let mut last = 0u64;
                loop {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let v = d.load(Ordering::Relaxed);
                    if v != last {
                        last = v;
                        s.send(WorkerEvent::TransferProgress { id, done: v });
                    }
                    if st.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })
        };

        let result = async {
            for (rpath, lpath, size) in files {
                download_file(
                    &sftp,
                    &rpath,
                    &lpath,
                    size,
                    sftp.metadata(&rpath)
                        .await
                        .ok()
                        .and_then(|m| m.mtime)
                        .unwrap_or(0),
                    &cancel,
                    &done,
                )
                .await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        stop.store(true, Ordering::Relaxed);
        let _ = prog.await;
        sink.send(WorkerEvent::TransferProgress {
            id,
            done: done.load(Ordering::Relaxed),
        });
        result
    }
    .await;

    match res {
        Ok(_) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: true,
            message: match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已下载 {name}"),
                crate::i18n::Lang::En => format!("Downloaded {name}"),
            },
            refresh_dir: None,
        }),
        Err(e) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("下载失败：{e}"),
                    crate::i18n::Lang::En => format!("Download failed: {e}"),
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

/// 同一文件内的并发分段数（流水线深度）。8 路足以在常见高延迟链路上跑满带宽。
pub(super) async fn download_file(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    rpath: &str,
    lpath: &std::path::Path,
    size: u64,
    remote_mtime: u32,
    cancel: &Arc<AtomicBool>,
    done: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    if let Some(parent) = lpath.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let data_part = data_part_path(lpath);

    // 小文件（或大小未知）：单流顺序读取；瞬时失败整体重试（重新建临时文件）。
    if size <= DL_CHUNK {
        let mut attempt = 0u32;
        loop {
            match download_small(sftp, rpath, &data_part, cancel, done).await {
                Ok(()) => {
                    finish_download(&data_part, lpath)?;
                    return Ok(());
                }
                Err(e) => {
                    if cancel.load(Ordering::Relaxed) || attempt >= XFER_RETRIES {
                        let _ = std::fs::remove_file(&data_part);
                        return Err(e);
                    }
                    attempt += 1;
                    tokio::time::sleep(xfer_backoff(attempt)).await;
                }
            }
        }
    }

    // 大文件：预分配，按偏移并发分段；用「分段完成位图」实现断点续传——
    // 位图持久化到 sidecar（<local>.ishellpart），重连/重发后只补未完成分段。
    let n_chunks = size.div_ceil(DL_CHUNK);
    let part = part_path(lpath);

    // 能否续传：sidecar 存在、记录的大小与远端 mtime 均一致、临时数据文件仍在。
    // 绑定 mtime：远端文件内容变化但大小不变时，旧分段不能复用（否则拼出混合损坏文件）。
    let resume_bm: Option<Vec<u8>> = if data_part.exists() && remote_mtime != 0 {
        std::fs::read(&part).ok().and_then(|d| {
            let ok = d.len() == 12 + bitmap_len(n_chunks)
                && u64::from_le_bytes(d[0..8].try_into().unwrap()) == size
                && u32::from_le_bytes(d[8..12].try_into().unwrap()) == remote_mtime;
            ok.then(|| d[12..].to_vec())
        })
    } else {
        None
    };

    let out = if resume_bm.is_some() {
        Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&data_part)?,
        ) // 续传：保留已写分段
    } else {
        let f = std::fs::File::create(&data_part)?;
        f.set_len(size)?;
        Arc::new(f)
    };
    let chunk_done: Arc<Vec<AtomicBool>> = Arc::new(
        (0..n_chunks)
            .map(|i| {
                let d = resume_bm
                    .as_ref()
                    .is_some_and(|b| (b[(i / 8) as usize] >> (i % 8)) & 1 == 1);
                AtomicBool::new(d)
            })
            .collect(),
    );
    // 已完成分段计入进度（续传时进度条从断点开始）
    let pre: u64 = (0..n_chunks)
        .filter(|&i| chunk_done[i as usize].load(Ordering::Relaxed))
        .map(|i| std::cmp::min(DL_CHUNK, size - i * DL_CHUNK))
        .sum();
    if pre > 0 {
        done.fetch_add(pre, Ordering::Relaxed);
    }
    // sidecar 句柄（写头部 size+mtime + 预留位图区，保留续传位）
    let part_file = {
        let f = std::fs::File::create(&part)?;
        f.set_len(12 + bitmap_len(n_chunks) as u64)?;
        pwrite(&f, &size.to_le_bytes(), 0)?;
        pwrite(&f, &remote_mtime.to_le_bytes(), 8)?;
        if let Some(b) = &resume_bm {
            pwrite(&f, b, 12)?;
        }
        Arc::new(std::sync::Mutex::new(f))
    };

    let mut attempt = 0u32;
    loop {
        let cursor = Arc::new(AtomicU64::new(0)); // 本轮分段游标
        let workers = DL_PARALLEL.min(n_chunks.max(1));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..workers {
            let (sftp, out, cursor, done, cancel, chunk_done) = (
                sftp.clone(),
                out.clone(),
                cursor.clone(),
                done.clone(),
                cancel.clone(),
                chunk_done.clone(),
            );
            let part_file = part_file.clone();
            let rpath = rpath.to_string();
            set.spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut rf = sftp.open(&rpath).await?;
                let mut buf = vec![0u8; DL_CHUNK as usize];
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        anyhow::bail!("canceled");
                    }
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    if idx >= n_chunks {
                        break;
                    }
                    if chunk_done[idx as usize].load(Ordering::Relaxed) {
                        continue; // 上一轮已完成
                    }
                    let off = idx * DL_CHUNK;
                    let want = std::cmp::min(DL_CHUNK, size - off) as usize;
                    rf.seek(std::io::SeekFrom::Start(off)).await?;
                    let mut got = 0usize;
                    while got < want {
                        let n = rf.read(&mut buf[got..want]).await?;
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                    if got != want {
                        anyhow::bail!("short read");
                    }
                    pwrite(&out, &buf[..want], off)?;
                    chunk_done[idx as usize].store(true, Ordering::Relaxed);
                    done.fetch_add(want as u64, Ordering::Relaxed); // 每段只计一次
                                                                    // 持久化该分段所在的位图字节（断点信息落盘）
                    let byte_i = (idx / 8) as usize;
                    let mut b = 0u8;
                    for bit in 0..8u64 {
                        let ci = byte_i as u64 * 8 + bit;
                        if ci < n_chunks && chunk_done[ci as usize].load(Ordering::Relaxed) {
                            b |= 1 << bit;
                        }
                    }
                    if let Ok(g) = part_file.lock() {
                        let _ = pwrite(&g, &[b], 12 + byte_i as u64);
                    }
                }
                Ok::<(), anyhow::Error>(())
            });
        }
        let mut first_err: Option<anyhow::Error> = None;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => {
                    first_err.get_or_insert(e.into());
                }
            }
        }
        if chunk_done.iter().all(|b| b.load(Ordering::Relaxed)) {
            let _ = std::fs::remove_file(&part); // 完成则清理断点文件
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&part); // 用户取消则不保留断点
            let _ = std::fs::remove_file(&data_part); // 数据临时文件一并清理，目标文件从未被动过
            anyhow::bail!("canceled");
        }
        if attempt >= XFER_RETRIES {
            return Err(first_err.unwrap_or_else(|| anyhow::anyhow!("incomplete transfer")));
        }
        attempt += 1;
        tokio::time::sleep(xfer_backoff(attempt)).await;
    }
    drop(out); // 关闭数据句柄后再 rename（Windows 需要）
    finish_download(&data_part, lpath)?;
    Ok(())
}

/// 下载完成收尾：临时数据文件原子替换到目标（先删已存在目标，Windows rename 不覆盖）。
pub(super) fn finish_download(
    data_part: &std::path::Path,
    lpath: &std::path::Path,
) -> anyhow::Result<()> {
    if !lpath.exists() {
        // 目标不存在：直接换入
        std::fs::rename(data_part, lpath)?;
        return Ok(());
    }
    // 覆盖已有：备份 → 换入 → 删备份；换入失败则还原备份，原文件绝不丢失。
    let bak = lpath.with_extension(format!("ishell-bak-{}", rand_hex(6)));
    std::fs::rename(lpath, &bak)?; // 原文件安全存于 bak
    match std::fs::rename(data_part, lpath) {
        Ok(_) => {
            let _ = std::fs::remove_file(&bak);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&bak, lpath); // 换入失败：还原原文件
            let _ = std::fs::remove_file(data_part); // 清理未换入的临时数据文件，避免残留
            Err(e.into())
        }
    }
}

/// 小文件顺序下载；失败时回退本次已计入的进度字节，便于上层整体重试不重复计数。
pub(super) async fn download_small(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    rpath: &str,
    lpath: &std::path::Path,
    cancel: &Arc<AtomicBool>,
    done: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut added = 0u64;
    let res: anyhow::Result<()> = async {
        let mut rf = sftp.open(rpath).await?;
        let mut lf = tokio::fs::File::create(lpath).await?;
        let mut buf = vec![0u8; 128 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("canceled");
            }
            let n = rf.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            lf.write_all(&buf[..n]).await?;
            done.fetch_add(n as u64, Ordering::Relaxed);
            added += n as u64;
        }
        lf.flush().await?;
        Ok(())
    }
    .await;
    if res.is_err() {
        done.fetch_sub(added, Ordering::Relaxed); // 回退，避免重试重复累加
    }
    res
}

use download_io::pwrite;
