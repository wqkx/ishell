//! SFTP read/open helpers.

use crate::proto::WorkerEvent;

use crate::ssh::UiSink;

/// 探测字节的字符编码并解码为 String，返回 (文本, 编码名)。
/// UTF-8(含 BOM) 优先；非 UTF-8 用 chardetng 猜测（中文环境多为 GBK/GB18030）。
pub(in crate::ssh) fn decode_text(data: &[u8]) -> (String, String) {
    // UTF-8 BOM
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(&data[3..]).into_owned(),
            "UTF-8".into(),
        );
    }
    // 无损 UTF-8 直接用
    if let Ok(s) = std::str::from_utf8(data) {
        return (s.to_string(), "UTF-8".into());
    }
    // 非 UTF-8：探测后解码
    let mut det = chardetng::EncodingDetector::new();
    det.feed(data, true);
    let enc = det.guess(None, true);
    let (cow, actual, _) = enc.decode(data);
    (cow.into_owned(), actual.name().to_string())
}

/// 分块读取远程文本文件并上报进度（驱动占位标签上的珊瑚色进度条），与下载文件一致地分块读取。
/// 非 force 时限制 20MB 并拒绝含 NUL 的二进制；force（用户确认后）放宽到 128MB 且跳过二进制检查。
/// 跟随读取（tail -f）：从 offset 读到文件末尾（单次 ≤512KB）。
/// offset=u64::MAX 只返回当前大小（跟随开启时的初始化，相当于 `tail -f -n 0`）；
/// 文件变小（截断/轮转）时回报 truncated 并把 offset 重置为新大小。
pub(in crate::ssh) async fn tail_file(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    offset: u64,
    sink: &UiSink,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let size = match sftp.metadata(path).await {
        Ok(m) => m.size.unwrap_or(0),
        Err(_) => {
            // 瞬时错误（弱网等）：offset 原样返回，UI 下一轮重试
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
        sink.send(WorkerEvent::FileTail {
            path: path.to_string(),
            data: Vec::new(),
            offset: size,
            truncated: false,
        });
        return;
    }
    if size < offset {
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
    let res: anyhow::Result<Vec<u8>> = async {
        let mut f = sftp.open(path).await?;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; want];
        let mut read = 0usize;
        while read < want {
            let n = f.read(&mut buf[read..]).await?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(buf)
    }
    .await;
    match res {
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

pub(in crate::ssh) async fn read_file_chunked(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    force: bool,
    id: u64,
    sink: &UiSink,
) {
    use tokio::io::AsyncReadExt;
    let limit = if force {
        crate::limits::FILE_HARD_LIMIT as usize
    } else {
        crate::limits::FILE_SOFT_LIMIT as usize
    };
    let meta = sftp.metadata(path).await.ok();
    let total = meta.as_ref().and_then(|m| m.size).unwrap_or(0);
    let file_mtime = meta.as_ref().and_then(|m| m.mtime).unwrap_or(0);
    // 先报 0 进度：占位标签立即显示空进度条
    sink.send(WorkerEvent::FileLoadProgress { id, done: 0, total });
    let too_large = || match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("文件过大（>{}MB）", limit / 1024 / 1024),
        crate::i18n::Lang::En => format!("File too large (>{}MB)", limit / 1024 / 1024),
    };
    if total as usize > limit {
        // 非 force 超限：列表里的旧大小可能已过时（小文件被写大），交 UI 弹确认可强制打开；
        // force 时仍超（>128MB）才真正失败。
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
    // 分块读入内存（与 download_small 一致：128KB 一块），每累计 ~256KB 上报一次进度
    let res: anyhow::Result<Vec<u8>> = async {
        let mut rf = sftp.open(path).await?;
        let mut data: Vec<u8> =
            Vec::with_capacity((total as usize).min(limit).min(16 * 1024 * 1024));
        let mut buf = vec![0u8; 128 * 1024];
        let mut last = 0usize;
        loop {
            let n = rf.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.len() > limit {
                anyhow::bail!("__TOO_LARGE__");
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
        Ok(data)
    }
    .await;
    match res {
        Ok(data) => {
            if !force && data.iter().take(8000).any(|b| *b == 0) {
                sink.send(WorkerEvent::FileLoadFailed {
                    id,
                    message: crate::i18n::tr("非文本文件，无法以文本方式打开", "Not a text file")
                        .into(),
                });
                return;
            }
            // 探测编码并解码（UTF-8 优先，非 UTF-8 用 chardetng 猜 GBK/GB18030 等），再把行尾统一成 LF
            let (decoded, encoding) = decode_text(&data);
            let (content, eol) = if decoded.contains("\r\n") {
                (decoded.replace("\r\n", "\n"), crate::proto::Eol::Crlf)
            } else {
                (decoded, crate::proto::Eol::Lf)
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
        Err(e) => {
            let msg = if e.to_string().contains("__TOO_LARGE__") {
                too_large()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("打开失败：{e}"),
                    crate::i18n::Lang::En => format!("Open failed: {e}"),
                }
            };
            sink.send(WorkerEvent::FileLoadFailed { id, message: msg });
        }
    }
}

/// 读取图片文件原始字节（带大小上限，避免误开超大文件拖慢界面）。
pub(in crate::ssh) async fn read_image_file(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> anyhow::Result<Vec<u8>> {
    let limit = 32 * 1024 * 1024;
    // 先按元数据判大小再读，避免远端超大文件在限制检查前被整体读入内存（OOM/DoS）。
    if let Some(sz) = sftp.metadata(path).await.ok().and_then(|m| m.size) {
        if sz > limit as u64 {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("图片过大（>{}MB）", limit / 1024 / 1024),
                    crate::i18n::Lang::En =>
                        format!("Image too large (>{}MB)", limit / 1024 / 1024),
                }
            );
        }
    }
    // 不用一次性的 sftp.read(path)：那是把整个远端文件读进内存后才能拿到长度，
    // 上面基于 metadata.size 的检查形同虚设——服务端不报告/请求失败/文件读取期间持续
    // 增长/上报不准确的大小，都能绕过检查，峰值内存实际上取决于远端文件真实大小，
    // 不受这个 32MB 限制约束。改成分块读、每次追加后立即检查累计长度，真正做到硬上限。
    use tokio::io::AsyncReadExt;
    let mut f = sftp.open(path).await?;
    let mut data = Vec::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.len() > limit {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("图片过大（>{}MB）", limit / 1024 / 1024),
                    crate::i18n::Lang::En =>
                        format!("Image too large (>{}MB)", limit / 1024 / 1024),
                }
            );
        }
    }
    Ok(data)
}
