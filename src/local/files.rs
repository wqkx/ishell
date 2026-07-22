//! 本机文件系统操作：把 SSH worker 那套 SFTP 文件命令（列目录/读写/增删改/复制移动）复刻到
//! 本地文件系统，产出与 SFTP 侧**完全相同**的 `WorkerEvent`。文件面板、编辑器、看图/文档查看器
//! 因此对本机会话零改动地工作——它们只认协议事件，不关心字节来自 SFTP 还是本地 FS。
//!
//! 与 SSH 侧的差异都源于「本地」：无会话级重连（失败一律 `retryable:false`）；写文件不需要
//! SFTP 那套「SETSTAT 截断」的防御，本地 `rename` 天然原子；`"."` 解析到本机家目录而非进程 cwd。

use std::path::{Path, PathBuf};

use crate::proto::{Eol, FileEntry, UiCommand, WorkerEvent};
use crate::ssh::UiSink;
use crate::textcodec::decode_text;

/// 本机文件命令的统一入口：worker 把非终端类命令都交给它。能处理的就地执行并回报事件，
/// 其余（传输/端口转发/进程/PDF 等尚不支持的）静默忽略。
pub(super) async fn handle(cmd: UiCommand, sink: &UiSink) {
    match cmd {
        UiCommand::ListDir { path, gen } => list_dir_event(&path, gen, sink).await,
        UiCommand::ReadFile { id, path, force } => read_file(&path, force, id, sink).await,
        UiCommand::WriteFile {
            id,
            path,
            content,
            encoding,
            eol,
            expect_mtime,
            force,
        } => write_file(id, &path, content, &encoding, eol, expect_mtime, force, sink).await,
        UiCommand::ReadImage { path } => read_image(&path, sink).await,
        UiCommand::ReadDoc { id, path } => read_doc(id, &path, sink).await,
        UiCommand::TailFile { path, offset } => tail_file(&path, offset, sink).await,
        UiCommand::Mkdir(path) => mkdir(&path, sink).await,
        UiCommand::CreateFile(path) => create_file(&path, sink).await,
        UiCommand::Chmod { path, mode } => chmod(&path, mode, sink).await,
        UiCommand::Rename { from, to } => rename(&from, &to, sink).await,
        UiCommand::DeleteMany { paths } => delete_many(paths, sink).await,
        UiCommand::CopyMove {
            srcs,
            dest_dir,
            do_move,
        } => copy_move(srcs, &dest_dir, do_move, sink).await,
        // 系统监控侧栏的进程详情 / 结束进程：本机直接读 /proc、本机 kill。
        UiCommand::ProcDetail(pid) => super::sys::proc_detail(pid, sink).await,
        UiCommand::KillProc(pid) => super::sys::kill_proc(pid, sink).await,
        // 传输 / 端口转发 / PDF / MCP 中转等：本机会话尚不支持（或不适用），忽略。
        _ => {}
    }
}

// ————————————————————————— 列目录 —————————————————————————

async fn list_dir_event(path: &str, gen: u64, sink: &UiSink) {
    match list_dir(path).await {
        Ok((canon, entries)) => sink.send(WorkerEvent::DirListing {
            path: canon,
            entries,
            gen,
        }),
        Err(e) => sink.send(WorkerEvent::DirListFailed {
            path: path.to_string(),
            message: match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("读取目录失败：{e}"),
                crate::i18n::Lang::En => format!("List dir failed: {e}"),
            },
            // 本地无会话级重连概念，路径错误一律不可重试。
            retryable: false,
        }),
    }
}

/// 读取本地目录，返回（规范化后的绝对路径, 条目列表）。目录在前、按名排序（与 SFTP 侧一致）。
async fn list_dir(path: &str) -> std::io::Result<(String, Vec<FileEntry>)> {
    // 初始化时文件面板发的是 "."，本地应解析到**家目录**（而非进程 cwd）——与 SFTP 把 "." 解析
    // 到远端家目录的行为对齐。其余路径来自 UI，均为规范化后的绝对路径。
    let target = resolve_dir(path);
    let canon = tokio::fs::canonicalize(&target)
        .await
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.to_string_lossy().into_owned());

    let mut rd = tokio::fs::read_dir(&canon).await?;
    let mut entries = Vec::new();
    while let Some(item) = rd.next_entry().await? {
        let name = item.file_name().to_string_lossy().into_owned();
        // DirEntry::metadata() 是 lstat 语义（不跟随符号链接），故能据类型判 is_link；
        // 链接的真实目标/类型下面用 stat（跟随）二次解析。取不到元数据的条目跳过（如竞态删除）。
        let Ok(meta) = item.metadata().await else {
            continue;
        };
        let is_link = meta.file_type().is_symlink();
        let (perm, owner) = perm_owner(&meta);
        entries.push(FileEntry {
            name,
            is_dir: meta.is_dir(),
            is_link,
            size: meta.len(),
            mtime: mtime_secs(&meta),
            perm,
            owner,
            link_target: None,
            link_dir: false,
        });
    }
    resolve_symlinks(&canon, &mut entries).await;
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((canon, entries))
}

/// 跟随解析目录中的符号链接：填「规范目标路径」「目标是否目录」「文件型链接的目标大小」。
/// 本地 stat 很快，顺序解析即可；仍设一个上限，避免病态目录（海量链接）拖慢列目录。
async fn resolve_symlinks(dir: &str, entries: &mut [FileEntry]) {
    const MAX_LINKS: usize = 512;
    let base = PathBuf::from(dir);
    let mut done = 0usize;
    for e in entries.iter_mut() {
        if !e.is_link {
            continue;
        }
        if done >= MAX_LINKS {
            break;
        }
        done += 1;
        let full = base.join(&e.name);
        // stat（跟随）成功才算「已解析」；断链则 target 留 None（UI 标红）。
        if let Ok(m) = tokio::fs::metadata(&full).await {
            e.link_dir = m.is_dir();
            if !m.is_dir() {
                e.size = m.len(); // lstat 给的是链接自身长度，对用户无意义 → 换成目标大小
            }
            e.link_target = tokio::fs::canonicalize(&full)
                .await
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
        }
    }
}

// ————————————————————————— 读文件（编辑器打开） —————————————————————————

async fn read_file(path: &str, force: bool, id: u64, sink: &UiSink) {
    use tokio::io::AsyncReadExt;
    let limit = if force {
        crate::limits::FILE_HARD_LIMIT as usize
    } else {
        crate::limits::FILE_SOFT_LIMIT as usize
    };
    let meta = tokio::fs::metadata(path).await.ok();
    let total = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let file_mtime = meta.as_ref().map(|m| mtime_secs(m) as u32).unwrap_or(0);
    sink.send(WorkerEvent::FileLoadProgress { id, done: 0, total });

    let too_large = || match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("文件过大（>{}MB）", limit / 1024 / 1024),
        crate::i18n::Lang::En => format!("File too large (>{}MB)", limit / 1024 / 1024),
    };
    if total as usize > limit {
        // 非 force 超软限：列表里的旧大小可能过时，交 UI 弹确认可强制打开；force 仍超才真失败。
        if !force {
            sink.send(WorkerEvent::FileTooLarge {
                id,
                path: path.to_string(),
                size: total,
            });
        } else {
            sink.send(WorkerEvent::FileLoadFailed {
                id,
                message: too_large(),
            });
        }
        return;
    }

    // 分块读入内存并硬性限幅（不能只靠开始前那次 size 检查——文件可能在读取期间增长）。
    let res: std::io::Result<Result<Vec<u8>, ()>> = async {
        let mut f = tokio::fs::File::open(path).await?;
        let mut data: Vec<u8> = Vec::with_capacity((total as usize).min(limit).min(16 * 1024 * 1024));
        let mut buf = vec![0u8; 128 * 1024];
        let mut last = 0usize;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.len() > limit {
                return Ok(Err(())); // 超限标记，外层转「过大」文案
            }
            if data.len() - last >= 256 * 1024 {
                last = data.len();
                sink.send(WorkerEvent::FileLoadProgress {
                    id,
                    done: data.len() as u64,
                    total: total.max(data.len() as u64),
                });
            }
        }
        Ok(Ok(data))
    }
    .await;

    match res {
        Ok(Ok(data)) => {
            if !force && data.iter().take(8000).any(|b| *b == 0) {
                sink.send(WorkerEvent::FileLoadFailed {
                    id,
                    message: crate::i18n::tr("非文本文件，无法以文本方式打开", "Not a text file")
                        .into(),
                });
                return;
            }
            let (decoded, encoding) = decode_text(&data);
            let (content, eol) = if decoded.contains("\r\n") {
                (decoded.replace("\r\n", "\n"), Eol::Crlf)
            } else {
                (decoded, Eol::Lf)
            };
            sink.send(WorkerEvent::FileOpened {
                id,
                path: path.to_string(),
                content,
                encoding,
                eol,
                mtime: file_mtime,
            });
        }
        Ok(Err(())) => sink.send(WorkerEvent::FileLoadFailed {
            id,
            message: too_large(),
        }),
        Err(e) => sink.send(WorkerEvent::FileLoadFailed {
            id,
            message: match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("打开失败：{e}"),
                crate::i18n::Lang::En => format!("Open failed: {e}"),
            },
        }),
    }
}

// ————————————————————————— 写文件（编辑器保存） —————————————————————————

#[allow(clippy::too_many_arguments)]
async fn write_file(
    id: u64,
    path: &str,
    content: String,
    encoding: &str,
    eol: Eol,
    expect_mtime: u32,
    force: bool,
    sink: &UiSink,
) {
    // 外部改动检测：非 force 且打开时记过 mtime，若当前 mtime 不同则拒绝写、回报冲突。
    if !force && expect_mtime != 0 {
        let cur = tokio::fs::metadata(path)
            .await
            .ok()
            .map(|m| mtime_secs(&m) as u32)
            .unwrap_or(0);
        if cur != 0 && cur != expect_mtime {
            sink.send(WorkerEvent::FileSaveConflict {
                id,
                path: path.to_string(),
            });
            return;
        }
    }

    // 内部统一 LF → 按原文件行尾还原；再按原编码编码，避免破坏非 UTF-8 文件 / 改动行尾。
    let text = match eol {
        Eol::Crlf => content.replace('\n', "\r\n"),
        Eol::Lf => content,
    };
    let enc = encoding_rs::Encoding::for_label(encoding.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    let (bytes, _, had_unmappable) = enc.encode(&text);
    if had_unmappable {
        sink.send(WorkerEvent::Status(match crate::i18n::current() {
            crate::i18n::Lang::Zh => {
                format!("⚠ 部分字符无法用 {encoding} 编码，已按替代形式写入：{path}")
            }
            crate::i18n::Lang::En => {
                format!("⚠ Some chars aren't representable in {encoding}; written as substitutions: {path}")
            }
        }));
    }

    let total = bytes.len() as u64;
    sink.send(WorkerEvent::FileSaveProgress {
        path: path.to_string(),
        done: 0,
        total,
    });
    match atomic_write(path, bytes.as_ref()).await {
        Ok(()) => {
            sink.send(WorkerEvent::FileSaveProgress {
                path: path.to_string(),
                done: total,
                total,
            });
            let nm = tokio::fs::metadata(path)
                .await
                .ok()
                .map(|m| mtime_secs(&m) as u32)
                .unwrap_or(0);
            sink.send(WorkerEvent::FileSaved {
                id,
                path: path.to_string(),
                mtime: nm,
            });
        }
        Err(e) => sink.send(WorkerEvent::FileSaveFailed {
            id,
            path: path.to_string(),
            message: match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("保存失败：{e}"),
                crate::i18n::Lang::En => format!("Save failed: {e}"),
            },
        }),
    }
}

/// 本地事务性保存：写同目录临时文件 → 继承原文件权限 → `rename` 原子换入。本地 `rename`
/// 在同一文件系统上原子，无需 SFTP 那套换入前后复验（那是防个别服务器 SETSTAT 截断的）。
/// 目标是符号链接时解析到真实目标后替换（链接语义保留）。
async fn atomic_write(path: &str, data: &[u8]) -> std::io::Result<()> {
    // 符号链接 → 写到真实目标上（链接不变）；断链/普通文件就地写。
    let target: PathBuf = match tokio::fs::symlink_metadata(path).await {
        Ok(m) if m.file_type().is_symlink() => tokio::fs::canonicalize(path)
            .await
            .unwrap_or_else(|_| PathBuf::from(path)),
        _ => PathBuf::from(path),
    };
    let parent = target
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let fname = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let orig_perm = tokio::fs::metadata(&target)
        .await
        .ok()
        .map(|m| m.permissions());

    let tmp = parent.join(format!(".{fname}.ishell-tmp-{}", rand_suffix()));
    if let Err(e) = tokio::fs::write(&tmp, data).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Some(perm) = orig_perm {
        let _ = tokio::fs::set_permissions(&tmp, perm).await; // 尽力继承权限，失败不阻断保存
    }
    if let Err(e) = tokio::fs::rename(&tmp, &target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

// ————————————————————————— 看图 / 文档 —————————————————————————

async fn read_image(path: &str, sink: &UiSink) {
    match read_capped(path, 32 * 1024 * 1024).await {
        Ok(data) => sink.send(WorkerEvent::ImageOpened {
            path: path.to_string(),
            data,
        }),
        Err(e) => sink.send(WorkerEvent::Error(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("打开失败：{e}"),
            crate::i18n::Lang::En => format!("Open failed: {e}"),
        })),
    }
}

async fn read_doc(id: u64, path: &str, sink: &UiSink) {
    const DOC_LIMIT: u64 = 20 * 1024 * 1024;
    let total = tokio::fs::metadata(path).await.ok().map(|m| m.len()).unwrap_or(0);
    sink.send(WorkerEvent::FileLoadProgress { id, done: 0, total });
    match read_capped(path, DOC_LIMIT as usize).await {
        Ok(data) => sink.send(WorkerEvent::DocOpened {
            id,
            path: path.to_string(),
            data,
        }),
        Err(e) => sink.send(WorkerEvent::FileLoadFailed {
            id,
            message: e.to_string(),
        }),
    }
}

/// 读整个文件但对累计字节做硬上限（分块，边读边判——防文件在读取期间增长绕过上限）。
async fn read_capped(path: &str, limit: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut data = Vec::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("文件过大（>{}MB）", limit / 1024 / 1024),
                    crate::i18n::Lang::En => format!("File too large (>{}MB)", limit / 1024 / 1024),
                },
            ));
        }
    }
    Ok(data)
}

// ————————————————————————— 跟随读取（tail -f） —————————————————————————

async fn tail_file(path: &str, offset: u64, sink: &UiSink) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let size = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => {
            sink.send(WorkerEvent::FileTail {
                path: path.to_string(),
                data: Vec::new(),
                offset,
                truncated: false,
            });
            return;
        }
    };
    if offset == u64::MAX {
        // 初始化：只报当前大小，不读数据（相当于 tail -f -n 0）
        sink.send(WorkerEvent::FileTail {
            path: path.to_string(),
            data: Vec::new(),
            offset: size,
            truncated: false,
        });
        return;
    }
    if size < offset {
        // 文件被截断/轮转：重置到新大小
        sink.send(WorkerEvent::FileTail {
            path: path.to_string(),
            data: Vec::new(),
            offset: size,
            truncated: true,
        });
        return;
    }
    if size == offset {
        sink.send(WorkerEvent::FileTail {
            path: path.to_string(),
            data: Vec::new(),
            offset,
            truncated: false,
        });
        return;
    }
    let want = (size - offset).min(512 * 1024) as usize;
    let read: std::io::Result<Vec<u8>> = async {
        let mut f = tokio::fs::File::open(path).await?;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; want];
        let mut got = 0usize;
        while got < want {
            let n = f.read(&mut buf[got..]).await?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.truncate(got);
        Ok(buf)
    }
    .await;
    match read {
        Ok(data) => {
            let n = data.len() as u64;
            sink.send(WorkerEvent::FileTail {
                path: path.to_string(),
                data,
                offset: offset + n,
                truncated: false,
            });
        }
        Err(_) => sink.send(WorkerEvent::FileTail {
            path: path.to_string(),
            data: Vec::new(),
            offset,
            truncated: false,
        }),
    }
}

// ————————————————————————— 增 / 改 / 删 / 复制移动 —————————————————————————

async fn mkdir(path: &str, sink: &UiSink) {
    let parent = parent_of(path);
    match tokio::fs::create_dir(path).await {
        Ok(()) => op_done(
            sink,
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已创建目录：{path}"),
                crate::i18n::Lang::En => format!("Created dir: {path}"),
            },
            Some(parent),
        ),
        Err(e) => op_err(sink, e.to_string()),
    }
}

async fn create_file(path: &str, sink: &UiSink) {
    let parent = parent_of(path);
    // O_CREAT|O_EXCL：同名直接失败，杜绝「先判断再创建」的竞态清空已有文件。
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(_) => op_done(
            sink,
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已创建文件：{path}"),
                crate::i18n::Lang::En => format!("Created file: {path}"),
            },
            Some(parent),
        ),
        Err(_) => op_err(
            sink,
            crate::i18n::tr("同名文件已存在或无法创建", "File exists or cannot be created").into(),
        ),
    }
}

async fn chmod(path: &str, mode: u32, sink: &UiSink) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = parent_of(path);
        let perm = std::fs::Permissions::from_mode(mode & 0o777);
        match tokio::fs::set_permissions(path, perm).await {
            Ok(()) => op_done(
                sink,
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("已修改权限：{:o}", mode & 0o777),
                    crate::i18n::Lang::En => format!("Chmod: {:o}", mode & 0o777),
                },
                Some(parent),
            ),
            Err(e) => op_err(sink, e.to_string()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        op_err(
            sink,
            crate::i18n::tr("本平台不支持修改权限", "chmod not supported on this platform").into(),
        );
    }
}

async fn rename(from: &str, to: &str, sink: &UiSink) {
    let parent = parent_of(to);
    match tokio::fs::rename(from, to).await {
        Ok(()) => op_done(
            sink,
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已重命名为：{to}"),
                crate::i18n::Lang::En => format!("Renamed to: {to}"),
            },
            Some(parent),
        ),
        Err(e) => op_err(sink, e.to_string()),
    }
}

async fn delete_many(paths: Vec<String>, sink: &UiSink) {
    if paths.is_empty() {
        return;
    }
    let n = paths.len();
    let parent = parent_of(&paths[0]);
    let mut failed: Option<String> = None;
    for p in &paths {
        // lstat（不跟随）：符号链接按「删链接本身」处理（remove_file），不去 remove_dir_all 目标。
        let is_dir = match tokio::fs::symlink_metadata(p).await {
            Ok(m) => m.is_dir(), // 符号链接的 is_dir 为 false → 走 remove_file，正确
            Err(e) => {
                failed = Some(e.to_string());
                continue;
            }
        };
        let r = if is_dir {
            tokio::fs::remove_dir_all(p).await
        } else {
            tokio::fs::remove_file(p).await
        };
        if let Err(e) = r {
            failed = Some(e.to_string());
        }
    }
    let message = match &failed {
        None => match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("已删除 {n} 项"),
            crate::i18n::Lang::En => format!("Deleted {n} item(s)"),
        },
        Some(e) => match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("部分删除失败：{}", e.trim()),
            crate::i18n::Lang::En => format!("Some deletions failed: {}", e.trim()),
        },
    };
    op_done(sink, message, Some(parent));
}

async fn copy_move(srcs: Vec<String>, dest_dir: &str, do_move: bool, sink: &UiSink) {
    if srcs.is_empty() {
        return;
    }
    let n = srcs.len();
    let dest = PathBuf::from(dest_dir);
    // 目标必须是已存在目录：把源「移入目录」，杜绝把单个文件重命名成目标名导致的数据错位。
    if !tokio::fs::metadata(&dest)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        op_err(
            sink,
            crate::i18n::tr("目标不是已存在的目录", "Destination is not an existing directory")
                .into(),
        );
        return;
    }
    let src_parent = srcs.first().map(|p| parent_of(p));
    let mut failed: Option<String> = None;
    for s in &srcs {
        let src = PathBuf::from(s);
        let Some(name) = src.file_name() else {
            failed = Some(format!("非法源路径：{s}"));
            continue;
        };
        let target = dest.join(name);
        let r = if do_move {
            move_path(&src, &target).await
        } else {
            copy_recursive(&src, &target).await
        };
        if let Err(e) = r {
            failed = Some(e.to_string());
        }
    }
    // 失败时刷新「源目录」，让前端乐观移除的项重新显示（文件其实还在源处）；成功刷新目标目录。
    let refresh = if failed.is_some() {
        src_parent
    } else {
        Some(dest_dir.to_string())
    };
    let (verb_zh, verb_en) = if do_move {
        ("移动", "Moved")
    } else {
        ("复制", "Copied")
    };
    let message = match &failed {
        None => match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("已{verb_zh} {n} 项"),
            crate::i18n::Lang::En => format!("{verb_en} {n} item(s)"),
        },
        Some(e) => match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("{verb_zh}失败：{}", e.trim()),
            crate::i18n::Lang::En => format!("{verb_en} failed: {}", e.trim()),
        },
    };
    op_done(sink, message, refresh);
}

/// 移动：先试 `rename`（同盘原子、最快）；跨文件系统（`rename` 报错）时退回「递归复制 + 删源」。
async fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if tokio::fs::rename(src, dst).await.is_ok() {
        return Ok(());
    }
    copy_recursive(src, dst).await?;
    // 复制成功后删源（目录递归、文件/链接直删）。
    let is_dir = tokio::fs::symlink_metadata(src)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if is_dir {
        tokio::fs::remove_dir_all(src).await
    } else {
        tokio::fs::remove_file(src).await
    }
}

type IoFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>>;

/// 递归复制：目录建好后逐项复制；符号链接按「复制链接本身」处理（不解引用）；普通文件字节复制。
fn copy_recursive<'a>(src: &'a Path, dst: &'a Path) -> IoFuture<'a> {
    Box::pin(async move {
        let meta = tokio::fs::symlink_metadata(src).await?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            let link_target = tokio::fs::read_link(src).await?;
            #[cfg(unix)]
            {
                tokio::fs::symlink(&link_target, dst).await?;
            }
            #[cfg(not(unix))]
            {
                let _ = link_target; // 非 unix 不复刻链接，退化为跳过（best-effort）
            }
        } else if ft.is_dir() {
            tokio::fs::create_dir_all(dst).await?;
            let mut rd = tokio::fs::read_dir(src).await?;
            while let Some(e) = rd.next_entry().await? {
                copy_recursive(&e.path(), &dst.join(e.file_name())).await?;
            }
        } else {
            tokio::fs::copy(src, dst).await?;
        }
        Ok(())
    })
}

// ————————————————————————— 小工具 —————————————————————————

fn op_done(sink: &UiSink, message: String, refresh_dir: Option<String>) {
    sink.send(WorkerEvent::OpDone {
        message,
        refresh_dir,
    });
}

fn op_err(sink: &UiSink, e: String) {
    sink.send(WorkerEvent::Error(match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("操作失败：{e}"),
        crate::i18n::Lang::En => format!("Operation failed: {e}"),
    }));
}

/// 初始/相对目录解析："." 或空 → 家目录（对齐 SFTP 把 "." 解析到远端家目录）；其余原样。
fn resolve_dir(path: &str) -> PathBuf {
    if path.is_empty() || path == "." {
        if let Some(home) = super::home_dir() {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

/// 取父目录字符串（用于 OpDone 的 refresh_dir）；无父则回退根 "/"。
fn parent_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/".into())
}

#[cfg(unix)]
fn perm_owner(m: &std::fs::Metadata) -> (u32, String) {
    use std::os::unix::fs::MetadataExt;
    (m.mode() & 0o777, m.uid().to_string())
}
#[cfg(not(unix))]
fn perm_owner(_m: &std::fs::Metadata) -> (u32, String) {
    (0, String::new())
}

#[cfg(unix)]
fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime().max(0) as u64
}
#[cfg(not(unix))]
fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rand_suffix() -> String {
    let mut b = [0u8; 4];
    if getrandom::getrandom(&mut b).is_err() {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        b = ns.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}
