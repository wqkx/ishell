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
    place_extracted_dir, xfer_backoff, DL_CHUNK, DL_PARALLEL, XFER_RETRIES,
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

    // 解包到 local 父目录下的一次性临时子目录，而不是直接解包到父目录——归档顶层目录名
    // 固定是远端 basename（打包命令里的 `name`），调用方要求的本地目标名（`local` 的
    // basename）可以和它不同（copy_from_remote 允许改名），直接解包到父目录只会产生
    // `parent/name`，不是 `local`，会出现「报告成功但目标路径不存在」。这里统一解包到
    // 随机子目录，再原子移动/替换到 `local`，不管两边名字是否一致都能落到正确位置。
    let dest = std::path::Path::new(local)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let scratch = dest.join(format!(".ishelldl-extract-{id}-{}", rand_hex(6)));
    let tgz = local_tgz.clone();
    let scratch_for_extract = scratch.clone();
    // 本地解包也可能耗时（大量小文件），提示「解包中…」避免进度条满了却迟迟不完成
    sink.send(WorkerEvent::TransferNote {
        id,
        note: crate::i18n::tr("解包中…", "Extracting…").into(),
    });
    let extract_result =
        tokio::task::spawn_blocking(move || extract_tar_gz(&tgz, &scratch_for_extract)).await?;
    sink.send(WorkerEvent::TransferNote {
        id,
        note: String::new(),
    });
    let _ = std::fs::remove_file(&local_tgz);
    extract_result?;
    // 归档解包后的顶层目录固定叫 `name`（远端 basename）；移到调用方要求的 `local`——
    // 已存在则整体替换（镜像覆盖，见 place_extracted_dir）。
    let extracted = scratch.join(&name);
    let local_path = std::path::PathBuf::from(local);
    let place_result =
        tokio::task::spawn_blocking(move || place_extracted_dir(&extracted, &local_path)).await?;
    // 只有真正挪走成功才清理 scratch——挪动失败时里面还是刚解压出来的完整内容，
    // 删掉的话用户就得重新下载一遍，宁可留着等人工核实。
    if place_result.is_ok() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    place_result?;
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

    // 目录且目标已存在：只有 Overwrite 策略会走到这里（Skip 已在上面提前返回；
    // Rename 已用 local_nonexistent 换成一个确定不存在的名字）——先整体清空旧目录，
    // 保证「镜像覆盖」：下载完成后目标只包含这次下载来的内容，不残留旧目录里远端
    // 已经不存在的文件/子目录。压缩下载分支还有 place_extracted_dir 兜底同一逻辑；
    // 这里统一处理是因为逐文件下载分支只会覆盖同名文件，没有类似的「整体替换」步骤。
    if is_dir {
        if let Ok(meta) = std::fs::symlink_metadata(&local) {
            let clean = if meta.is_dir() {
                std::fs::remove_dir_all(&local)
            } else {
                std::fs::remove_file(&local)
            };
            if let Err(e) = clean {
                sink.send(WorkerEvent::TransferDone {
                    id,
                    ok: false,
                    message: match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("清理旧目标失败：{e}"),
                        crate::i18n::Lang::En => format!("Failed to clear old target: {e}"),
                    },
                    refresh_dir: None,
                });
                return;
            }
        }
    }

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

/// `copy_from_remote` 对称方向：把远端单文件流式回传给运行 ishell-mcp 的调用方机器，不落地
/// iShell 宿主机磁盘。探测到目录/远端不可访问时，直接经 `download_sink` 回错误（不发送
/// `DownloadStreamSource`，调用方据此得知这条路径当前只支持单文件）；探测到普通文件后，
/// 用 `tokio::io::duplex` 建一条内存管道，读端连同文件大小一并通过 `download_sink` 交给
/// `mcp_bridge::handle_conn`，本函数自己继续把 SFTP 读到的字节写进管道写端——分块大小、
/// 进度节流写法都照抄 `upload_from_mcp` 的对称实现。
pub(super) async fn download_to_mcp(
    sftp: Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote_path: String,
    download_sink: tokio::sync::oneshot::Sender<Result<crate::proto::DownloadStreamSource, String>>,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let meta = sftp.metadata(&remote_path).await;
    let size = match meta {
        Ok(m) if m.is_dir() => {
            let _ = download_sink.send(Err(match crate::i18n::current() {
                crate::i18n::Lang::Zh => {
                    "copy_from_remote 流式模式仅支持单个文件；目录请多次调用，或用 run_command 执行 tar/rsync".to_string()
                }
                crate::i18n::Lang::En => {
                    "copy_from_remote streaming mode only supports a single file; for directories, call it per-file or use run_command with tar/rsync".to_string()
                }
            }));
            return;
        }
        Ok(m) => m.size.unwrap_or(0),
        Err(e) => {
            let _ = download_sink.send(Err(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("远端文件不存在或无法访问：{e}"),
                crate::i18n::Lang::En => format!("Remote file missing or inaccessible: {e}"),
            }));
            return;
        }
    };

    let (mut writer, reader) = tokio::io::duplex(128 * 1024);
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    if download_sink
        .send(Ok(crate::proto::DownloadStreamSource {
            size,
            reader: Box::new(reader),
            outcome: outcome_rx,
        }))
        .is_err()
    {
        // 调用方（socket 对端）已经断开，没人会读这个管道，直接放弃，不发起 SFTP 读取。
        return;
    }
    sink.send(WorkerEvent::TransferStart {
        id,
        name: basename(&remote_path),
        total: size,
        dir: crate::proto::TransferDir::Download,
        local: None,
    });

    let result: anyhow::Result<()> = async {
        let mut remote = sftp.open(&remote_path).await?;
        let mut buffer = vec![0_u8; 128 * 1024];
        let mut sent = 0_u64;
        let mut last_reported = 0_u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("canceled");
            }
            let read = remote.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read]).await?;
            sent += read as u64;
            if sent.saturating_sub(last_reported) >= 256 * 1024 || sent == size {
                last_reported = sent;
                sink.send(WorkerEvent::TransferProgress { id, done: sent });
            }
        }
        // size 取自传输开始前的 metadata，它是发给对端的**长度承诺**。远端文件在这期间被追加
        // 过的话，这里读出来的字节数就不是那个数——而对端只按 size 收，多出来的它根本不读，
        // 于是「文件传输中长大了」表现为：调用方拿到一个成功返回的、被静默截断的文件。这里
        // 判出来，经 trailer 告诉对端别换入（见 DownloadStreamSource::outcome）。
        if sent != size {
            anyhow::bail!(
                "远端文件在传输期间发生了变化：开始时 {size} 字节，实际读出 {sent} 字节。\
                 这次拷贝已放弃，调用方本地的同名文件未被改动；请重试"
            );
        }
        writer.flush().await?;
        writer.shutdown().await?;
        Ok(())
    }
    .await;
    // trailer：把最终判定送给 handle_conn，由它写在字节流之后。对端已经断开时这里发不出去，
    // 无所谓——那种情况下也没人再需要这个判定。
    let _ = outcome_tx.send(result.as_ref().map(|_| ()).map_err(|e| e.to_string()));

    match result {
        Ok(()) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: true,
            message: format!("Downloaded {remote_path}"),
            refresh_dir: None,
        }),
        Err(error) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("下载失败：{error}"),
                    crate::i18n::Lang::En => format!("Download failed: {error}"),
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

/// 跨会话拷贝-中转模式（源会话侧）：把远端单文件读到 `writer` 这条内存管道里（App 层已经
/// 创建好 duplex 并把另一半发给目标会话）。探测到目录/远端不可访问时，经
/// `WorkerEvent::RelaySourceResult` 直接回错误（此时还没有 `TransferStart`，不需要
/// `TransferDone` 收尾）；探测到普通文件后，先回 `Ok(size)` 让 App 层去驱动目标会话，
/// 再照抄 `download_to_mcp` 的分块读取/进度节流写法把字节灌进 `writer`。
pub(super) async fn relay_read_file(
    sftp: Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote_path: String,
    mut writer: tokio::io::DuplexStream,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let meta = sftp.metadata(&remote_path).await;
    let size = match meta {
        Ok(m) if m.is_dir() => {
            sink.send(WorkerEvent::RelaySourceResult {
                id,
                result: Err(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => {
                        "copy_between_sessions 当前仅支持单个文件；目录请多次调用，或用 run_command 执行 rsync/scp".to_string()
                    }
                    crate::i18n::Lang::En => {
                        "copy_between_sessions currently only supports a single file; for directories, call it per-file or use run_command with rsync/scp".to_string()
                    }
                }),
            });
            return;
        }
        Ok(m) => m.size.unwrap_or(0),
        Err(e) => {
            sink.send(WorkerEvent::RelaySourceResult {
                id,
                result: Err(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("远端文件不存在或无法访问：{e}"),
                    crate::i18n::Lang::En => format!("Remote file missing or inaccessible: {e}"),
                }),
            });
            return;
        }
    };
    sink.send(WorkerEvent::RelaySourceResult { id, result: Ok(size) });
    sink.send(WorkerEvent::TransferStart {
        id,
        name: basename(&remote_path),
        total: size,
        dir: crate::proto::TransferDir::Upload,
        local: None,
    });

    let result: anyhow::Result<()> = async {
        let mut remote = sftp.open(&remote_path).await?;
        let mut buffer = vec![0_u8; 128 * 1024];
        let mut sent = 0_u64;
        let mut last_reported = 0_u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("canceled");
            }
            let read = remote.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read]).await?;
            sent += read as u64;
            if sent.saturating_sub(last_reported) >= 256 * 1024 || sent == size {
                last_reported = sent;
                sink.send(WorkerEvent::TransferProgress { id, done: sent });
            }
        }
        writer.flush().await?;
        writer.shutdown().await?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: true,
            message: format!("Relayed {remote_path}"),
            refresh_dir: None,
        }),
        Err(error) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("读取失败：{error}"),
                    crate::i18n::Lang::En => format!("Read failed: {error}"),
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

    // 能否续传：sidecar 存在、记录的大小与远端 mtime 均一致、临时数据文件仍在**且长度
    // 与预期一致**。只看 sidecar 记录、不验证临时数据文件本身的长度是不够的：如果临时文件
    // 被截断或没有完整落盘（异常退出、外部误删部分内容等），但 sidecar 的完成位图仍然
    // 声称某些分段"已完成"，续传时这些分段就不会被重新下载，最终换入的文件会是短文件/
    // 零填充/新旧数据混合，且不会被发现——这里先验证临时文件实际长度，不一致就直接放弃
    // 续传信息，走全新下载（数据文件下面会被重新 create+set_len，不会残留旧内容）。
    let data_len_ok = std::fs::metadata(&data_part)
        .map(|m| m.len() == size)
        .unwrap_or(false);
    let resume_bm: Option<Vec<u8>> = if data_len_ok && remote_mtime != 0 {
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
                    // sidecar（断点位图）写入失败不能忽略：忽略的话位图可能比实际数据更新
                    // （比如这次持久化失败，但内存里已经标记这一段"完成"），下次续传时
                    // 会误以为这段已经下载好而跳过重新下载，产生零填充/旧数据的损坏文件。
                    // 落盘失败直接当这个分段失败处理，交给上层整体重试。
                    match part_file.lock() {
                        Ok(g) => pwrite(&g, &[b], 12 + byte_i as u64)?,
                        Err(_) => anyhow::bail!("sidecar 断点文件锁获取失败"),
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
    // 换入前最后再校验一次实际长度：所有分段都标记"完成"不等于文件真的完整落盘
    // （比如某个分段的 pwrite 系统调用层面部分失败但没有被上面的 short-read 检测捕捉到），
    // 长度不对就直接报错、不换入，避免把损坏文件当成下载成功。
    match std::fs::metadata(&data_part) {
        Ok(m) if m.len() == size => {}
        Ok(m) => anyhow::bail!("下载文件长度不符（期望 {size}，实际 {}），未换入目标文件", m.len()),
        Err(e) => anyhow::bail!("下载完成后无法校验临时文件：{e}"),
    }
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
