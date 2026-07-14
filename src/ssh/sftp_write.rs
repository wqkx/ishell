//! SFTP write and mutating filesystem operations.

use crate::proto::{UiCommand, WorkerEvent};

use crate::ssh::xfer::rand_hex;
use crate::ssh::UiSink;

use super::remote_parent;

/// 执行一次 SFTP 写类操作，结果以 [`WorkerEvent::OpDone`]/`Error` 上报。
/// 完整覆盖写一个远端文件：`CREATE | WRITE | TRUNCATE` 打开 → write_all → flush → **shutdown**。
///
/// 不用 russh-sftp 的便捷 `sftp.write()`——它只用 `OpenFlags::WRITE`：既不 `TRUNCATE`（内容变短时
/// 残留旧文件尾部）、也不关闭句柄（部分 SFTP 服务端要 CLOSE 才落盘 → 出现「保存了却没变化」）。
/// 与上传路径（`upload`）用的收尾方式一致。
pub(in crate::ssh) async fn sftp_overwrite(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    data: &[u8],
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::AsyncWriteExt;
    let mut f = sftp
        .open_with_flags(
            path,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
        )
        .await?;
    f.write_all(data).await?;
    f.flush().await?;
    f.shutdown().await?;
    Ok(())
}

/// 校验远端文件确实有 `expect` 字节。**优先用便宜的 metadata.size**——绝大多数服务器（含本项目
/// 遇到过的截断服务器）都如实回报 size，一次 stat 即可判定。只有服务器不回报 size（退化实现、
/// 返回 None）时，才退回**实际读回**比对长度。
/// 注意：`sftp.read` 会把整个文件读进内存，大文件代价极高，故绝不作为常规路径——仅在 size 缺失时兜底。
pub(in crate::ssh) async fn sftp_verify_size(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    expect: usize,
) -> bool {
    if let Some(sz) = sftp.metadata(path).await.ok().and_then(|m| m.size) {
        return sz == expect as u64;
    }
    // 无 size：读回校验（唯一需要整文件下载的分支，仅退化服务器会走到）。
    matches!(sftp.read(path).await, Ok(b) if b.len() == expect)
}

/// 事务性保存：先把新内容完整写入同目录临时文件，**校验 tmp 落盘无误**后，把原文件挪到 `.bak`、
/// 换入 tmp，**再校验最终目标字节数**——只有目标确认写对了才删 `.bak`；任何一步出错都从 `.bak`
/// 还原，绝不把空/残缺文件留给用户。原文件全程存在于 目标 或 目标.bak。
/// 特殊情形：
/// - **符号链接**：解析到真实目标后在其上做同样的事务替换，链接语义保留、且仍然原子；
///   链接损坏（无法解析）时把 target 落回链接自身路径，仍走事务写（换入后该路径变为普通文件），
///   而**不**退回会截断原文件的直写——保证任何情形都不留残缺文件。
/// - **权限**：把原文件的权限位复制到临时文件，避免保存可执行脚本丢失执行位。
pub(in crate::ssh) async fn sftp_write_atomic(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    data: &[u8],
    sink: &UiSink,
) -> anyhow::Result<()> {
    // 符号链接 → 解析到真实目标，替换发生在目标上（链接不变）；损坏链接则就地走事务写。
    let is_symlink = sftp
        .symlink_metadata(path)
        .await
        .map(|m| m.is_symlink())
        .unwrap_or(false);
    let target = if is_symlink {
        sftp.canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_string())
    } else {
        path.to_string()
    };

    // 仅用于「保存前继承原文件权限」；取不到（新建 / 断链）则不设权限，不影响保存成败。
    let orig_perm = sftp
        .metadata(&target)
        .await
        .ok()
        .and_then(|m| m.permissions);

    let tmp = format!("{target}.ishell-tmp-{}", rand_hex(6));
    if let Err(e) = sftp_overwrite_progress_to(sftp, &tmp, path, data, sink).await {
        let _ = sftp.remove_file(&tmp).await; // 写失败：清理临时文件，原文件未动
        return Err(e);
    }
    // 【闸一】换入前校验 tmp 是否完整落盘；不对就中止，原文件分毫未动。
    if !sftp_verify_size(sftp, &tmp, data.len()).await {
        let _ = sftp.remove_file(&tmp).await;
        return Err(anyhow::anyhow!(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("保存校验失败：临时文件未完整写入（应为 {} 字节），已中止换入（原文件未改动）", data.len()),
            crate::i18n::Lang::En => format!("save verify failed: temp file not fully written (expected {} bytes) — aborted (original unchanged)", data.len()),
        }));
    }
    // 权限回填到临时文件（保存前继承原文件的 mode）。
    // 【坑】个别服务器（外置盘 / 非 OpenSSH 实现）会在 SETSTAT 时把文件**截断为 0**！
    // 这正是「保存清空文件」的真凶：tmp 写满→设权限被清空→rename 搬走空文件。
    // 故设完权限必须**复验 tmp**；一旦发现被截断，就**重写 tmp 内容、放弃权限保留**（数据优先）。
    if let Some(mode) = orig_perm {
        let _ = sftp
            .set_metadata(
                &tmp,
                russh_sftp::protocol::FileAttributes {
                    permissions: Some(mode),
                    ..Default::default()
                },
            )
            .await;
        if !sftp_verify_size(sftp, &tmp, data.len()).await {
            // 该服务器 SETSTAT 会截断文件：重写内容、放弃权限保留（数据优先）。
            if let Err(e) = sftp_overwrite_progress_to(sftp, &tmp, path, data, sink).await {
                let _ = sftp.remove_file(&tmp).await;
                return Err(e);
            }
        }
    }

    // 换入策略：先把原文件挪到 .bak（若存在），再换入 tmp，最后校验；任一步失败从 .bak 还原。
    // **是否已备份由 rename 结果判定，不依赖可能瞬时失败的 stat**：
    // rename(target->bak) 成功 = 原文件存在且已妥善备份；明确的 NoSuchFile = 原文件不存在
    // （新建，backed_up=false 安全）；其它错误（权限不足/网络异常/目标被占用等）既不代表
    // "不存在"也不代表"已备份"——真相不明时绝不能当成"新建"直接往下走 rename(tmp->target)，
    // 那样如果原文件其实存在，会在没有备份的情况下把它直接覆盖丢失。这种情况下直接中止，
    // 原文件（如果存在）分毫未动，tmp 里的新内容也还在、不会丢。
    let bak = format!("{target}.ishell-bak-{}", rand_hex(6));
    let backed_up = match sftp.rename(&target, &bak).await {
        Ok(()) => true,
        Err(e) if super::is_sftp_not_found(&e) => false,
        Err(e) => {
            let _ = sftp.remove_file(&tmp).await;
            return Err(anyhow::anyhow!(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!(
                    "无法确认原文件状态（备份步骤失败，原因不明，非「文件不存在」）：{e}，\
                     已中止保存，原文件未改动"
                ),
                crate::i18n::Lang::En => format!(
                    "could not determine original file state (backup step failed for a reason \
                     other than \"not found\"): {e}, save aborted, original unchanged"
                ),
            }));
        }
    };

    // 换入：tmp → target。失败则从 bak 还原原文件（若有备份）。
    if let Err(e) = sftp.rename(&tmp, &target).await {
        let restored = backed_up && sftp.rename(&bak, &target).await.is_ok();
        let _ = sftp.remove_file(&tmp).await;
        return Err(anyhow::anyhow!(
            match (crate::i18n::current(), backed_up, restored) {
                (crate::i18n::Lang::Zh, false, _) =>
                    format!("保存失败（原文件未改动，新内容在 {tmp}）：{e}"),
                (crate::i18n::Lang::Zh, true, true) => format!("替换失败，已还原原文件：{e}"),
                (crate::i18n::Lang::Zh, true, false) =>
                    format!("替换失败且未能还原，原文件在 {bak}：{e}"),
                (crate::i18n::Lang::En, false, _) =>
                    format!("save failed (original unchanged, new content at {tmp}): {e}"),
                (crate::i18n::Lang::En, true, true) =>
                    format!("replace failed, original restored: {e}"),
                (crate::i18n::Lang::En, true, false) =>
                    format!("replace failed and not restored; original is at {bak}: {e}"),
            }
        ));
    }

    // 【闸二】换入后校验最终目标字节数——根治「保存却清空」：只有目标确认写对了才提交。
    if !sftp_verify_size(sftp, &target, data.len()).await {
        if backed_up {
            // 有备份：移除写坏的目标、从 .bak 还原原文件。
            let _ = sftp.remove_file(&target).await;
            let restored = sftp.rename(&bak, &target).await.is_ok();
            return Err(anyhow::anyhow!(match (crate::i18n::current(), restored) {
                (crate::i18n::Lang::Zh, true) => format!(
                    "保存后校验失败：目标字节数不符（应为 {}），已还原原文件",
                    data.len()
                ),
                (crate::i18n::Lang::Zh, false) =>
                    format!("保存后校验失败且未能还原，原文件在 {bak}"),
                (crate::i18n::Lang::En, true) => format!(
                    "post-save verify failed: wrong byte count (expected {}); original restored",
                    data.len()
                ),
                (crate::i18n::Lang::En, false) =>
                    format!("post-save verify failed and not restored; original is at {bak}"),
            }));
        }
        // 无备份=新建文件：target 是用户新内容的**唯一副本**，绝不删除（校验多为误报，删了才真丢数据）。
        // 如实告知、保留文件交用户核对。
        return Err(anyhow::anyhow!(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("保存后校验失败：目标字节数不符（应为 {}），文件已写入 {target}，请核对", data.len()),
            crate::i18n::Lang::En => format!("post-save verify failed: wrong byte count (expected {}); file written to {target}, please verify", data.len()),
        }));
    }

    if backed_up {
        let _ = sftp.remove_file(&bak).await; // 目标已校验无误，清理备份
    }
    Ok(())
}

/// 分块写 `write_path` 并以 `report_path` 上报保存进度。
pub(in crate::ssh) async fn sftp_overwrite_progress_to(
    sftp: &russh_sftp::client::SftpSession,
    write_path: &str,
    report_path: &str,
    data: &[u8],
    sink: &UiSink,
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::AsyncWriteExt;
    const CHUNK: usize = 256 * 1024;
    let total = data.len() as u64;
    sink.send(WorkerEvent::FileSaveProgress {
        path: report_path.to_string(),
        done: 0,
        total,
    });
    let mut f = sftp
        .open_with_flags(
            write_path,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
        )
        .await?;
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + CHUNK).min(data.len());
        f.write_all(&data[off..end]).await?;
        off = end;
        sink.send(WorkerEvent::FileSaveProgress {
            path: report_path.to_string(),
            done: off as u64,
            total,
        });
    }
    f.flush().await?;
    f.shutdown().await?;
    sink.send(WorkerEvent::FileSaveProgress {
        path: report_path.to_string(),
        done: total,
        total,
    });
    Ok(())
}

pub(in crate::ssh) async fn handle_fs_op(
    sftp: &russh_sftp::client::SftpSession,
    cmd: UiCommand,
    sink: &UiSink,
) {
    let result: anyhow::Result<(String, Option<String>)> = match cmd {
        UiCommand::Mkdir(path) => {
            let parent = remote_parent(&path);
            sftp.create_dir(&path)
                .await
                .map(|_| {
                    (
                        match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("已创建目录：{path}"),
                            crate::i18n::Lang::En => format!("Created dir: {path}"),
                        },
                        Some(parent),
                    )
                })
                .map_err(Into::into)
        }
        UiCommand::CreateFile(path) => {
            // 服务端原子独占创建（O_CREAT|O_EXCL）：同名文件由服务器直接拒绝，
            // 杜绝「先 try_exists 再 TRUNCATE 创建」的检查—执行竞态（期间被别的进程建文件、
            // 或 try_exists 因网络/权限失败误判不存在，都会清空已有文件）。
            use russh_sftp::protocol::OpenFlags;
            let parent = remote_parent(&path);
            match sftp
                .open_with_flags(
                    &path,
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                )
                .await
            {
                Ok(mut f) => {
                    use tokio::io::AsyncWriteExt;
                    let _ = f.shutdown().await;
                    Ok((
                        match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("已创建文件：{path}"),
                            crate::i18n::Lang::En => format!("Created file: {path}"),
                        },
                        Some(parent),
                    ))
                }
                Err(_) => Err(anyhow::anyhow!(crate::i18n::tr(
                    "同名文件已存在或无法创建",
                    "File exists or cannot be created"
                ))),
            }
        }
        UiCommand::Chmod { path, mode } => {
            let parent = remote_parent(&path);
            let attrs = russh_sftp::protocol::FileAttributes {
                permissions: Some(mode & 0o777),
                ..Default::default()
            };
            sftp.set_metadata(&path, attrs)
                .await
                .map(|_| {
                    (
                        match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("已修改权限：{:o}", mode & 0o777),
                            crate::i18n::Lang::En => format!("Chmod: {:o}", mode & 0o777),
                        },
                        Some(parent),
                    )
                })
                .map_err(Into::into)
        }
        UiCommand::Rename { from, to } => {
            let parent = remote_parent(&to);
            sftp.rename(&from, &to)
                .await
                .map(|_| {
                    (
                        match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("已重命名为：{to}"),
                            crate::i18n::Lang::En => format!("Renamed to: {to}"),
                        },
                        Some(parent),
                    )
                })
                .map_err(Into::into)
        }
        UiCommand::WriteFile {
            id,
            path,
            content,
            encoding,
            eol,
            expect_mtime,
            force,
        } => {
            // 外部改动检测：非 force 时若远端当前 mtime 与打开时不一致，拒绝写入、回报冲突。
            let conflict = if !force && expect_mtime != 0 {
                let cur = sftp
                    .metadata(&path)
                    .await
                    .ok()
                    .and_then(|m| m.mtime)
                    .unwrap_or(0);
                cur != 0 && cur != expect_mtime
            } else {
                false
            };
            if conflict {
                sink.send(WorkerEvent::FileSaveConflict { id, path });
                Ok((String::new(), None))
            } else {
                // 内部统一 LF → 按原文件行尾还原；再按原编码编码后写回，避免破坏非 UTF-8 文件 / 改动行尾。
                let text = match eol {
                    crate::proto::Eol::Crlf => content.replace('\n', "\r\n"),
                    crate::proto::Eol::Lf => content,
                };
                let enc = encoding_rs::Encoding::for_label(encoding.as_bytes())
                    .unwrap_or(encoding_rs::UTF_8);
                // 第三个返回值 had_unmappable=true 表示有字符无法用目标编码表示（被替换为
                // 数字字符引用等），保存不再静默——提示用户该编码丢失了字符。
                let (bytes, _, had_unmappable) = enc.encode(&text);
                if had_unmappable {
                    sink.send(WorkerEvent::Status(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("⚠ 部分字符无法用 {encoding} 编码，已按替代形式写入：{path}"),
                        crate::i18n::Lang::En => format!("⚠ Some chars aren't representable in {encoding}; written as substitutions: {path}"),
                    }));
                }
                match sftp_write_atomic(sftp, &path, bytes.as_ref(), sink).await {
                    Ok(_) => {
                        let nm = sftp
                            .metadata(&path)
                            .await
                            .ok()
                            .and_then(|m| m.mtime)
                            .unwrap_or(0);
                        sink.send(WorkerEvent::FileSaved {
                            id,
                            path: path.clone(),
                            mtime: nm,
                        });
                        Ok((
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("已保存：{path}"),
                                crate::i18n::Lang::En => format!("Saved: {path}"),
                            },
                            None,
                        ))
                    }
                    Err(e) => {
                        // 专用失败事件（带路径）：UI 据此复位 saving、保留 dirty，不再只有匿名 Error
                        sink.send(WorkerEvent::FileSaveFailed {
                            id,
                            path: path.clone(),
                            message: e.to_string(),
                        });
                        Ok((String::new(), None))
                    }
                }
            }
        }
        _ => Ok(("".into(), None)),
    };

    match result {
        Ok((message, refresh_dir)) => sink.send(WorkerEvent::OpDone {
            message,
            refresh_dir,
        }),
        Err(e) => sink.send(WorkerEvent::Error(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("操作失败：{e}"),
            crate::i18n::Lang::En => format!("Operation failed: {e}"),
        })),
    }
}
