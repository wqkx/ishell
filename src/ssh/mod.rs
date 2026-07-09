//! SSH 后台 worker：运行在 tokio 运行时上，负责建立连接、维护交互式 shell
//! 通道、SFTP 通道，并周期性采集系统信息。通过 channel 与 UI 线程通信。

mod auth;
mod forward;
mod sftp;
pub mod sysinfo;
mod xfer;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::proto::{ConnectConfig, UiCommand, WorkerEvent};
use sysinfo::{SysSampler, PROBE_CMD};

pub(super) use auth::ClientHandler;
use auth::{connect, exec_capture, exec_capture_bytes, exec_status, open_sftp, open_shell};
use sftp::{handle_fs_op, list_dir, read_file_chunked, read_image_file, remote_parent, tail_file};
use xfer::{start_xfer, PendingXfer, XferCancel, MAX_CONCURRENT_XFER};

/// 发往 UI 的事件通道（std mpsc），附带 egui 上下文用于主动请求重绘。
#[derive(Clone)]
pub struct UiSink {
    tx: std::sync::mpsc::Sender<WorkerEvent>,
    ctx: egui::Context,
}

impl UiSink {
    pub fn new(tx: std::sync::mpsc::Sender<WorkerEvent>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }
    fn send(&self, ev: WorkerEvent) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
}

/// worker 入口：在 tokio 任务中运行，直到断开。所有错误都转成 UI 事件上报。
pub async fn run(
    cfg: ConnectConfig,
    mut cmd_rx: UnboundedReceiver<UiCommand>,
    sink: UiSink,
    hostkey_rx: UnboundedReceiver<bool>,
) {
    sink.send(WorkerEvent::Status(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("正在连接 {}:{} …", cfg.host, cfg.port), crate::i18n::Lang::En => format!("Connecting {}:{} …", cfg.host, cfg.port) }));

    // `_jump_handle` 须保持存活：目标连接的底层流跑在它的 direct-tcpip 通道上
    let (handle, _jump_handle) = match connect(&cfg, &sink, hostkey_rx, &mut cmd_rx).await {
        Ok(h) => h,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("连接失败：{e}"), crate::i18n::Lang::En => format!("Connect failed: {e}") }));
            return;
        }
    };
    let handle = Arc::new(handle);

    // 1) 交互式 shell 通道
    let mut shell = match open_shell(&handle, cfg.forward_agent).await {
        Ok(c) => c,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开 shell 失败：{e}"), crate::i18n::Lang::En => format!("Open shell failed: {e}") }));
            return;
        }
    };

    // 2) SFTP 通道（Arc 共享，供并发任务使用，避免阻塞主循环）
    let mut sftp = match open_sftp(&handle).await {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"), crate::i18n::Lang::En => format!("SFTP unavailable: {e}") }));
            None
        }
    };
    // SFTP 会话「假死」自愈：弱网下底层通道被扰动后，russh-sftp 的请求可能永久挂起（既不返回
    // 也不报错），且本会话不会自愈——后续所有列目录/读取都卡死、界面一直转圈。为此引入：
    //   · sftp_gen  ：会话代号。热替换会话时自增，用于忽略「上一代」会话迟到的死亡上报；
    //   · dead 通道 ：某个 SFTP 操作超时（判定为假死）时上报其所属代号，触发主循环重连；
    //   · new 通道  ：后台重连任务把新会话（或失败 None）回送主循环热替换。
    let mut sftp_gen: u64 = 0;
    let (sftp_dead_tx, mut sftp_dead_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let (sftp_new_tx, mut sftp_new_rx) = tokio::sync::mpsc::unbounded_channel::<Option<russh_sftp::client::SftpSession>>();
    let mut sftp_reconnecting = false;

    // 3) 系统信息采集任务（独立 handle 克隆，互不阻塞）
    // 先探测 uname：非 Linux 或无 /proc 则禁用监控，避免空数据/误杀进程。
    let probe_handle = handle.clone();
    let probe_sink = sink.clone();
    let probe_task = tokio::spawn(async move {
        let linux_ok = match exec_capture(&probe_handle, "uname -s 2>/dev/null; test -r /proc/stat && echo HAS_PROC").await {
            Ok(out) => {
                let u = out.to_ascii_lowercase();
                u.contains("linux") && u.contains("has_proc")
            }
            Err(_) => false,
        };
        probe_sink.send(WorkerEvent::MonitorSupport(linux_ok));
        if !linux_ok {
            return;
        }
        let mut sampler = SysSampler::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await;
            match exec_capture(&probe_handle, PROBE_CMD).await {
                Ok(out) => {
                    let info = sampler.parse(&out);
                    probe_sink.send(WorkerEvent::SysInfo(Box::new(info)));
                }
                Err(_) => break, // 连接已断
            }
        }
    });

    sink.send(WorkerEvent::Connected);

    // 端口转发监听任务：id -> JoinHandle
    let mut forwards: HashMap<u64, tokio::task::JoinHandle<()>> = HashMap::new();

    // 传输并发控制：进行中计数 + 排队 + 取消标志；完成时由任务经 xfer_done 通知主循环
    let mut active_xfer = 0usize;
    let mut pending_xfer: VecDeque<PendingXfer> = VecDeque::new();
    let mut xfer_cancels: HashMap<u64, XferCancel> = HashMap::new();
    let (xfer_done_tx, mut xfer_done_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();

    // 4) 主循环：转发终端数据、处理 UI 指令
    loop {
        tokio::select! {
            // 某个传输结束：腾出名额并尝试启动排队中的任务
            Some(_done_id) = xfer_done_rx.recv() => {
                xfer_cancels.remove(&_done_id);
                active_xfer = active_xfer.saturating_sub(1);
                while active_xfer < MAX_CONCURRENT_XFER {
                    if let Some(p) = pending_xfer.pop_front() {
                        start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                        active_xfer += 1;
                    } else {
                        break;
                    }
                }
            }
            // SFTP 操作上报会话假死：仅当上报的是「当前代」会话、且未在重连时，才后台重开一条
            // SFTP 通道（去重，避免多任务同时上报引发重连风暴）。开通/失败均经 sftp_new_rx 回送。
            Some(dead_gen) = sftp_dead_rx.recv() => {
                if dead_gen == sftp_gen && !sftp_reconnecting {
                    sftp_reconnecting = true;
                    let h = handle.clone();
                    let tx = sftp_new_tx.clone();
                    tokio::spawn(async move {
                        let fresh = match tokio::time::timeout(Duration::from_secs(15), open_sftp(&h)).await {
                            Ok(Ok(s)) => Some(s),
                            _ => None, // 超时或失败：回送 None，主循环解锁重连位，下个操作会再次触发
                        };
                        let _ = tx.send(fresh);
                    });
                }
            }
            // 重连结果回送：成功则热替换会话并自增代号（旧会话的迟到死亡上报据此被忽略）。
            Some(fresh) = sftp_new_rx.recv() => {
                sftp_reconnecting = false;
                if let Some(s) = fresh {
                    sftp = Some(Arc::new(s));
                    sftp_gen += 1;
                }
            }
            msg = shell.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        sink.send(WorkerEvent::TerminalData(data.to_vec()));
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        sink.send(WorkerEvent::TerminalData(data.to_vec()));
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        sink.send(WorkerEvent::Disconnected(crate::i18n::tr("远程关闭了会话", "Remote closed the session").into()));
                        break;
                    }
                    _ => {}
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(UiCommand::TerminalInput(bytes)) => {
                        if shell.data(&bytes[..]).await.is_err() {
                            sink.send(WorkerEvent::Disconnected(crate::i18n::tr("写入通道失败", "Channel write failed").into()));
                            break;
                        }
                    }
                    Some(UiCommand::Resize { cols, rows }) => {
                        let _ = shell.window_change(cols as u32, rows as u32, 0, 0).await;
                    }
                    Some(UiCommand::ListDir(path)) => {
                        // 独立任务执行，避免慢/挂起的目录卡死整个 worker
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            // 该操作发起时的会话代号 + 死亡上报句柄：会话层错误即判定假死并上报，触发重连。
                            let gen = sftp_gen;
                            let dead_tx = sftp_dead_tx.clone();
                            tokio::spawn(async move {
                                // russh-sftp 自带每请求超时（见 open_sftp 的 set_timeout），弱网下请求要么
                                // 成功、要么返回错误，不会永久挂起——故不再叠加外层 timeout（那会抢在其前面
                                // 误杀「慢但能成」的列举）。据错误类型区分：路径级错误（Status，如不存在/无权限）
                                // 只回失败；会话级错误（超时/通道关闭/IO）判为假死，上报代号触发 SFTP 重连。
                                match list_dir(&sftp, &path).await {
                                    Ok((canon, entries)) => {
                                        s.send(WorkerEvent::DirListing { path: canon, entries });
                                    }
                                    Err(e) => {
                                        use russh_sftp::client::error::Error as SftpErr;
                                        // 路径级错误（Status：不存在/无权限）不可重试；其余（超时/通道关闭/IO）为
                                        // 会话级错误 → 可重试并触发 SFTP 重连。
                                        let retryable = !matches!(e, SftpErr::Status(_));
                                        if retryable {
                                            let _ = dead_tx.send(gen);
                                        }
                                        let message = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("读取目录失败：{e}"), crate::i18n::Lang::En => format!("List dir failed: {e}") };
                                        s.send(WorkerEvent::DirListFailed { path, message, retryable });
                                    }
                                }
                            });
                        } else {
                            // SFTP 未就绪（首次初始化失败等）：回报可重试失败——UI 保持自动重试，
                            // 而不是静默吞掉后永久停在「加载中」
                            sink.send(WorkerEvent::DirListFailed {
                                path,
                                message: crate::i18n::tr("SFTP 未就绪", "SFTP not ready").into(),
                                retryable: true,
                            });
                        }
                    }
                    // 传输：独立任务（独立 SFTP 通道），不阻塞交互 shell
                    Some(UiCommand::Download { id, remote, local, policy }) => {
                        let p = PendingXfer::Download { id, remote, local, policy };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::Upload { id, local, remote_dir, policy }) => {
                        let p = PendingXfer::Upload { id, local, remote_dir, policy };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::CancelTransfer(id)) => {
                        if let Some(c) = xfer_cancels.get_mut(&id) {
                            // 进行中：置协作标志 + 触发 stop 信号（drop 传输 future，
                            // 立即中止卡在 flush/shutdown 的上传），任务随后上报「已取消」
                            c.flag.store(true, Ordering::Relaxed);
                            if let Some(stop) = c.stop.take() {
                                let _ = stop.send(());
                            }
                        } else if let Some(pos) = pending_xfer.iter().position(|p| p.id() == id) {
                            // 仍在排队：直接移除并上报已取消
                            pending_xfer.remove(pos);
                            sink.send(WorkerEvent::TransferDone {
                                id, ok: false, message: crate::i18n::tr("已取消", "Canceled").into(), refresh_dir: None,
                            });
                        }
                    }
                    Some(UiCommand::ReadFile { id, path, force }) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                read_file_chunked(&sftp, &path, force, id, &s).await;
                            });
                        } else {
                            // SFTP 未就绪：移除占位标签并提示（否则永久「下载中」）
                            sink.send(WorkerEvent::FileLoadFailed { id, message: crate::i18n::tr("SFTP 未就绪", "SFTP not ready").into() });
                        }
                    }
                    Some(UiCommand::TailFile { path, offset }) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                tail_file(&sftp, &path, offset, &s).await;
                            });
                        }
                    }
                    Some(UiCommand::PdfInfo { id, path }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            let cmd = format!("pdfinfo {}", sh_quote(&path));
                            let hint = crate::i18n::tr(
                                "无法读取 PDF：远端需要 poppler-utils（Debian/Ubuntu: apt install poppler-utils）",
                                "Cannot read PDF: remote needs poppler-utils (Debian/Ubuntu: apt install poppler-utils)",
                            );
                            // 失败统一走 FileLoadFailed：复用编辑器占位标签的「移除 + 提示」路径
                            match exec_capture_bytes(&h, &cmd).await {
                                Ok((0, out, _)) => {
                                    let text = String::from_utf8_lossy(&out);
                                    let pages = text
                                        .lines()
                                        .find_map(|l| l.strip_prefix("Pages:"))
                                        .and_then(|v| v.trim().parse::<u32>().ok())
                                        .unwrap_or(0);
                                    if pages > 0 {
                                        s.send(WorkerEvent::PdfInfo { id, path, pages });
                                    } else {
                                        s.send(WorkerEvent::FileLoadFailed { id, message: crate::i18n::tr("无法解析 PDF 页数", "Cannot parse PDF page count").into() });
                                    }
                                }
                                Ok((127, _, _)) => s.send(WorkerEvent::FileLoadFailed { id, message: hint.into() }),
                                Ok((code, _, err)) => {
                                    let e = err.trim().to_string();
                                    let msg = if e.is_empty() { format!("pdfinfo exit {code}") } else { e };
                                    s.send(WorkerEvent::FileLoadFailed { id, message: msg });
                                }
                                Err(e) => s.send(WorkerEvent::FileLoadFailed { id, message: e.to_string() }),
                            }
                        });
                    }
                    Some(UiCommand::PdfPage { path, page, dpi }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            // pdftoppm 不指定输出名时把 PNG 写到 stdout（已在目标环境实测）
                            let cmd = format!("pdftoppm -png -r {} -f {} -l {} {}", dpi.clamp(36, 300), page, page, sh_quote(&path));
                            let data = match exec_capture_bytes(&h, &cmd).await {
                                Ok((0, out, _)) if out.starts_with(b"\x89PNG") => out,
                                _ => Vec::new(), // 失败：空数据，UI 显示该页加载失败
                            };
                            s.send(WorkerEvent::PdfPage { path, page, data });
                        });
                    }
                    Some(UiCommand::PdfSearch { path, query }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            // pdftotext 输出以 \f（换页符）分页 → 逐页找命中（不分大小写）
                            let cmd = format!("pdftotext {} -", sh_quote(&path));
                            match exec_capture_bytes(&h, &cmd).await {
                                Ok((0, out, _)) => {
                                    let text = String::from_utf8_lossy(&out);
                                    // 扫描件/无文本层：提取结果只剩换页符与空白，明确告知（而非「无结果」误导）
                                    if text.chars().all(|c| c.is_whitespace() || c == '\u{c}') {
                                        s.send(WorkerEvent::PdfSearch {
                                            path,
                                            query,
                                            hits: Vec::new(),
                                            message: Some(crate::i18n::tr("该 PDF 无文本层（可能是扫描件），无法搜索", "PDF has no text layer (scanned?), cannot search").into()),
                                        });
                                        return;
                                    }
                                    let needle = query.to_lowercase();
                                    // 跨行回退匹配：pdftotext 会把版面断行输出，中文词组常被
                                    // 换行截断（无空格分词）——去掉全部空白后再比一次
                                    let needle_ns: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
                                    let mut hits: Vec<(u32, String)> = Vec::new();
                                    for (pi, page) in text.split('\u{c}').enumerate() {
                                        if hits.len() >= 200 {
                                            break;
                                        }
                                        let lower = page.to_lowercase();
                                        let hit = lower.contains(&needle)
                                            || (!needle_ns.is_empty() && lower.chars().filter(|c| !c.is_whitespace()).collect::<String>().contains(&needle_ns));
                                        if hit {
                                            // 取首个命中行作为片段（截 ~80 字符）
                                            let snippet = page
                                                .lines()
                                                .find(|l| l.to_lowercase().contains(&needle))
                                                .map(|l| {
                                                    let t = l.trim();
                                                    t.chars().take(80).collect::<String>()
                                                })
                                                .unwrap_or_default();
                                            hits.push((pi as u32 + 1, snippet));
                                        }
                                    }
                                    s.send(WorkerEvent::PdfSearch { path, query, hits, message: None });
                                }
                                Ok((127, _, _)) => s.send(WorkerEvent::PdfSearch {
                                    path,
                                    query,
                                    hits: Vec::new(),
                                    message: Some(crate::i18n::tr("远端缺少 pdftotext（poppler-utils）", "Remote missing pdftotext (poppler-utils)").into()),
                                }),
                                Ok((code, _, err)) => {
                                    let e = err.trim().to_string();
                                    s.send(WorkerEvent::PdfSearch { path, query, hits: Vec::new(), message: Some(if e.is_empty() { format!("pdftotext exit {code}") } else { e }) });
                                }
                                Err(e) => s.send(WorkerEvent::PdfSearch { path, query, hits: Vec::new(), message: Some(e.to_string()) }),
                            }
                        });
                    }
                    Some(UiCommand::ReadDoc { id, path }) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                use tokio::io::AsyncReadExt;
                                // 文档查看上限 20MB（docx 通常远小于此）
                                const DOC_LIMIT: u64 = 20 * 1024 * 1024;
                                let size = sftp.metadata(&path).await.ok().and_then(|m| m.size).unwrap_or(0);
                                if size > DOC_LIMIT {
                                    s.send(WorkerEvent::FileLoadFailed { id, message: match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("文档过大（>{}MB）", DOC_LIMIT / 1024 / 1024),
                                        crate::i18n::Lang::En => format!("Document too large (>{}MB)", DOC_LIMIT / 1024 / 1024),
                                    } });
                                    return;
                                }
                                // 分块下载并上报进度（驱动占位标签的珊瑚线进度条，与编辑器一致）
                                s.send(WorkerEvent::FileLoadProgress { id, done: 0, total: size });
                                let res: anyhow::Result<Vec<u8>> = async {
                                    let mut f = sftp.open(&path).await?;
                                    let mut data = Vec::with_capacity(size as usize);
                                    let mut buf = vec![0u8; 128 * 1024];
                                    let mut last = 0usize;
                                    loop {
                                        let n = f.read(&mut buf).await?;
                                        if n == 0 {
                                            break;
                                        }
                                        data.extend_from_slice(&buf[..n]);
                                        if data.len() - last >= 256 * 1024 {
                                            last = data.len();
                                            s.send(WorkerEvent::FileLoadProgress { id, done: data.len() as u64, total: size.max(data.len() as u64) });
                                        }
                                    }
                                    Ok(data)
                                }
                                .await;
                                match res {
                                    Ok(data) => s.send(WorkerEvent::DocOpened { id, path, data }),
                                    Err(e) => s.send(WorkerEvent::FileLoadFailed { id, message: e.to_string() }),
                                }
                            });
                        } else {
                            sink.send(WorkerEvent::FileLoadFailed { id, message: crate::i18n::tr("SFTP 未就绪", "SFTP not ready").into() });
                        }
                    }
                    Some(UiCommand::ReadImage { path }) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                match read_image_file(&sftp, &path).await {
                                    Ok(data) => s.send(WorkerEvent::ImageOpened { path, data }),
                                    Err(e) => s.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开失败：{e}"), crate::i18n::Lang::En => format!("Open failed: {e}") })),
                                }
                            });
                        }
                    }
                    // 删除经 shell `rm` 执行：SFTP 的 remove_dir 只能删空目录、且对
                    // 指向目录的符号链接会失败；`rm -rf/-f` 与其它终端工具行为一致，
                    // 能删非空目录、各类链接与带特殊字符的文件名。
                    Some(UiCommand::DeleteMany { paths }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            if paths.is_empty() {
                                return;
                            }
                            let n = paths.len();
                            let parent = remote_parent(&paths[0]);
                            // 一条 rm -rf 处理所有路径（文件/目录通用）：单通道，避免多文件并发
                            // 开过多 SSH 会话（服务端 MaxSessions 默认 ~10）导致「只删掉几个」。
                            // `--` 终止选项解析，避免以 - 开头的文件名被当作开关。
                            let mut joined = String::new();
                            for p in &paths {
                                joined.push_str(&sh_quote(p));
                                joined.push(' ');
                            }
                            let cmd = format!("rm -rf -- {joined}");
                            match exec_status(&h, &cmd).await {
                                Ok((0, _)) => s.send(WorkerEvent::OpDone {
                                    message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已删除 {n} 项"), crate::i18n::Lang::En => format!("Deleted {n} item(s)") },
                                    refresh_dir: Some(parent),
                                }),
                                Ok((code, err)) => s.send(WorkerEvent::OpDone {
                                    message: match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("删除失败（码 {code}）：{}", err.trim()),
                                        crate::i18n::Lang::En => format!("Delete failed (code {code}): {}", err.trim()),
                                    },
                                    refresh_dir: Some(parent),
                                }),
                                Err(e) => s.send(WorkerEvent::Error(match crate::i18n::current() {
                                    crate::i18n::Lang::Zh => format!("删除失败：{e}"),
                                    crate::i18n::Lang::En => format!("Delete failed: {e}"),
                                })),
                            }
                        });
                    }
                    Some(cmd @ (UiCommand::Mkdir(_)
                        | UiCommand::CreateFile(_)
                        | UiCommand::Chmod { .. }
                        | UiCommand::Rename { .. }
                        | UiCommand::WriteFile { .. })) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                handle_fs_op(&sftp, cmd, &s).await;
                            });
                        } else {
                            // SFTP 未就绪：WriteFile 必须回专用失败事件，否则编辑器永久停在 saving=true。
                            if let UiCommand::WriteFile { path, .. } = &cmd {
                                sink.send(WorkerEvent::FileSaveFailed { path: path.clone(), message: crate::i18n::tr("SFTP 不可用", "SFTP unavailable").into() });
                            } else {
                                sink.send(WorkerEvent::Error(crate::i18n::tr("SFTP 不可用", "SFTP unavailable").into()));
                            }
                        }
                    }
                    Some(UiCommand::ProcDetail(pid)) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            // cmdline（NUL 转空格）/ cwd / exe，三行返回
                            let cmd = format!(
                                "cat /proc/{pid}/cmdline 2>/dev/null | tr '\\0' ' '; echo; readlink /proc/{pid}/cwd 2>/dev/null; readlink /proc/{pid}/exe 2>/dev/null"
                            );
                            if let Ok(out) = exec_capture(&h, &cmd).await {
                                let mut it = out.split('\n');
                                let cmdline = it.next().unwrap_or("").trim().to_string();
                                let cwd = it.next().unwrap_or("").trim().to_string();
                                let exe = it.next().unwrap_or("").trim().to_string();
                                s.send(WorkerEvent::ProcDetail { pid, cmd: cmdline, cwd, exe });
                            }
                        });
                    }
                    Some(UiCommand::KillProc(pid)) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            // 先 SIGTERM 再短等，仍存活则 SIGKILL；检查退出码如实反馈
                            let msg = match exec_status(&h, &format!(
                                "kill -15 {pid} 2>/dev/null; sleep 0.3; kill -0 {pid} 2>/dev/null && kill -9 {pid}; kill -0 {pid} 2>/dev/null && exit 1 || exit 0"
                            )).await {
                                Ok((0, _)) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已结束进程 {pid}"), crate::i18n::Lang::En => format!("Killed {pid}") },
                                Ok((_, err)) => {
                                    let e = err.trim();
                                    match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("结束进程失败：{}", if e.is_empty() { "权限不足或进程不存在" } else { e }),
                                        crate::i18n::Lang::En => format!("Kill failed: {}", if e.is_empty() { "permission denied or no such process" } else { e }),
                                    }
                                }
                                Err(e) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("结束进程失败：{e}"), crate::i18n::Lang::En => format!("Kill failed: {e}") },
                            };
                            s.send(WorkerEvent::Status(msg));
                        });
                    }
                    Some(UiCommand::AddForward(spec)) => {
                        let id = spec.id;
                        let h = handle.clone();
                        let s = sink.clone();
                        forwards.insert(id, tokio::spawn(forward::run_forward(h, spec, s)));
                    }
                    Some(UiCommand::RemoveForward(id)) => {
                        if let Some(task) = forwards.remove(&id) {
                            task.abort();
                        }
                    }
                    // 远端批量复制/移动：经 shell 执行 cp -a / mv，独立任务不阻塞交互
                    Some(UiCommand::CopyMove { srcs, dest_dir, do_move }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            let mut joined = String::new();
                            for p in &srcs {
                                joined.push_str(&sh_quote(p));
                                joined.push(' ');
                            }
                            // 目标强制以 "/" 结尾，令 mv/cp 必须把源「移入目录」：
                            // 若目标不是已存在目录则报错而非把单个文件重命名成目标名，
                            // 杜绝「拖到目录树后文件被改名、两个目录都找不到」的数据丢失。
                            let dest = format!("{}/", sh_quote(&dest_dir));
                            // `--` 终止选项解析，避免以 - 开头的文件名被当作开关
                            let cmd = if do_move {
                                format!("mv -f -- {joined}{dest}")
                            } else {
                                format!("cp -a -- {joined}{dest}")
                            };
                            let n = srcs.len();
                            // 失败时需刷新「源目录」，让前端乐观移除的项重新显示（文件其实还在源处）
                            let src_parent = srcs.first().map(|p| remote_parent(p));
                            match exec_status(&h, &cmd).await {
                                Ok((0, _)) => s.send(WorkerEvent::OpDone {
                                    message: match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("{}完成（{n} 项）", if do_move { "移动" } else { "复制" }),
                                        crate::i18n::Lang::En => format!("{} done ({n})", if do_move { "Move" } else { "Copy" }),
                                    },
                                    refresh_dir: Some(dest_dir.clone()),
                                }),
                                Ok((code, err)) => s.send(WorkerEvent::OpDone {
                                    message: match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("操作失败（码 {code}）：{}", err.trim()),
                                        crate::i18n::Lang::En => format!("Failed (code {code}): {}", err.trim()),
                                    },
                                    refresh_dir: src_parent,
                                }),
                                Err(e) => s.send(WorkerEvent::OpDone {
                                    message: match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("操作失败：{e}"),
                                        crate::i18n::Lang::En => format!("Failed: {e}"),
                                    },
                                    refresh_dir: src_parent,
                                }),
                            }
                        });
                    }
                    Some(UiCommand::DirectTransfer(spec)) => {
                        let p = PendingXfer::Direct(spec);
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    // 键盘交互回答仅在认证阶段由 connect() 消费；正常运行期收到则忽略
                    Some(UiCommand::KbdResponse(_)) => {}
                    Some(UiCommand::Disconnect) | None => {
                        let _ = shell.eof().await;
                        sink.send(WorkerEvent::Disconnected(crate::i18n::tr("已断开", "Disconnected").into()));
                        break;
                    }
                }
            }
        }
    }

    for (_, task) in forwards {
        task.abort();
    }
    probe_task.abort();
}
/// POSIX 单引号转义，用于把路径安全嵌入 shell 命令。
pub(super) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::sh_quote;

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn pdfinfo_pages_parse() {
        let sample = "Title: x
Pages:  12
Encrypted: no
";
        let pages = sample
            .lines()
            .find_map(|l| l.strip_prefix("Pages:"))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        assert_eq!(pages, 12);
    }
}
