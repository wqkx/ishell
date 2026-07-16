//! SSH 后台 worker：运行在 tokio 运行时上，负责建立连接、维护交互式 shell
//! 通道、SFTP 通道，并周期性采集系统信息。通过 channel 与 UI 线程通信。

mod auth;
mod commands;
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
use auth::{connect, exec_capture, exec_status, open_sftp, open_shell};
use sftp::{handle_fs_op, list_dir, read_file_chunked, read_image_file, remote_parent, tail_file};
use xfer::{
    direct_relay_copy, start_xfer, trust_temp_key, untrust_temp_key, PendingXfer, XferCancel,
    MAX_CONCURRENT_XFER,
};

/// 发往 UI 的事件通道（std mpsc），附带 egui 上下文用于主动请求重绘。
#[derive(Clone)]
pub struct UiSink {
    tx: std::sync::mpsc::Sender<WorkerEvent>,
    ctx: egui::Context,
    /// 系统信息走独立的 watch 通道而非 mpsc 队列：窗口最小化等导致 UI 长时间不排空时，
    /// mpsc 会把每 2 秒一份的快照无限堆积（真实内存暴涨根因）；watch 只保留「最新一份」，
    /// 天然与是否被及时消费无关，恢复窗口后也不会有积压可"爆发式重绘"。
    sysinfo_tx: Arc<tokio::sync::watch::Sender<Option<crate::proto::SysInfo>>>,
}

impl UiSink {
    pub fn new(
        tx: std::sync::mpsc::Sender<WorkerEvent>,
        ctx: egui::Context,
        sysinfo_tx: Arc<tokio::sync::watch::Sender<Option<crate::proto::SysInfo>>>,
    ) -> Self {
        Self {
            tx,
            ctx,
            sysinfo_tx,
        }
    }
    fn send(&self, ev: WorkerEvent) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
    /// 周期性系统信息快照：覆盖式发送（只留最新），不进入 mpsc 队列。
    fn send_sysinfo(&self, info: crate::proto::SysInfo) {
        let _ = self.sysinfo_tx.send(Some(info));
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
    sink.send(WorkerEvent::Status(match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("正在连接 {}:{} …", cfg.host, cfg.port),
        crate::i18n::Lang::En => format!("Connecting {}:{} …", cfg.host, cfg.port),
    }));

    // `_jump_handle` 须保持存活：目标连接的底层流跑在它的 direct-tcpip 通道上
    let (handle, _jump_handle) = match connect(&cfg, &sink, hostkey_rx, &mut cmd_rx).await {
        Ok(h) => h,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("连接失败：{e}"),
                crate::i18n::Lang::En => format!("Connect failed: {e}"),
            }));
            return;
        }
    };
    let handle = Arc::new(handle);

    // 1) 交互式 shell 通道
    let mut shell = match open_shell(&handle, cfg.forward_agent).await {
        Ok(c) => c,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("打开 shell 失败：{e}"),
                crate::i18n::Lang::En => format!("Open shell failed: {e}"),
            }));
            return;
        }
    };

    // 2) SFTP 通道（Arc 共享，供并发任务使用，避免阻塞主循环）
    let mut sftp = match open_sftp(&handle).await {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            sink.send(WorkerEvent::Error(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"),
                crate::i18n::Lang::En => format!("SFTP unavailable: {e}"),
            }));
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
    let (sftp_new_tx, mut sftp_new_rx) =
        tokio::sync::mpsc::unbounded_channel::<Option<russh_sftp::client::SftpSession>>();
    let mut sftp_reconnecting = false;
    // 后台重开一条 SFTP 通道，结果经 sftp_new_tx 回送；调用方需自行先把 sftp_reconnecting
    // 置 true 去重（两处调用点——会话假死上报、以及 sftp 本就是 None 时的 ListDir——共用同一
    // 套重连逻辑，不重复实现）。
    let spawn_sftp_reconnect = {
        let handle = handle.clone();
        let tx = sftp_new_tx.clone();
        move || {
            let h = handle.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let fresh = match tokio::time::timeout(Duration::from_secs(15), open_sftp(&h)).await {
                    Ok(Ok(s)) => Some(s),
                    _ => None, // 超时或失败：回送 None，主循环解锁重连位，下个操作会再次触发
                };
                let _ = tx.send(fresh);
            });
        }
    };

    // 2.5) AI/MCP 反向转发：把本机 mcp.sock 反向转发到这台远端主机，让"能 SSH 到这台服务器
    // 的人"也能连到转发出来的 socket 控制本机 iShell（复用这条已认证/加密的 SSH 连接，
    // 不额外开监听端口）。仅当设置里已开启 AI/MCP 控制时才注册；失败不影响正常连接使用。
    //
    // 远端路径带随机后缀（而非固定名字）：同一台主机短时间内被反复连接/重连时，服务器端
    // 可能因为迟迟未判定旧连接已死（无 keepalive 时 TCP 层探测很慢）而拒绝复用同一个固定
    // 路径的新注册请求（"rejected by the other party"）。加上随机后缀后各次连接互不冲突；
    // 对端按 `~/.ishell-mcp/mcp-*.sock` 通配、取最新一个即可发现当前有效的路径。
    //
    // 这些 socket 文件统一放进 `~/.ishell-mcp/` 子目录（而不是散落在 `$HOME` 根），且在本次
    // 连接正常断开时（见下方 `run()` 收尾处）主动删除本次注册的那一个——用一个共享的
    // `Option<String>` 槽位把闭包里算出的 remote_path 带到 `run()` 的外层作用域，因为
    // `streamlocal_forward` 的注册发生在一个独立 `tokio::spawn` 任务里，算出的路径出不了
    // 这个闭包。同时在注册前顺手清理这个子目录里 mtime 超过 24 小时的旧 socket 文件——不做
    // 连通性探测（不依赖远端装了 nc/socat），单纯用时间兜底崩溃/异常退出导致的遗留，避免
    // 无限堆积。
    let mcp_forward_path: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    // 保留注册任务句柄：连接收尾时要先给它一个收尾窗口，否则「连接先结束、注册后完成」会
    // 把已注册的 socket 遗留在远端（槽位被 take 时还是 None）。见 run() 末尾清理处。
    let mcp_forward_task: Option<tokio::task::JoinHandle<()>> = if crate::store::load_mcp_consent() {
        let fwd_handle = handle.clone();
        let path_slot = mcp_forward_path.clone();
        Some(tokio::spawn(async move {
            let home = match exec_capture(&fwd_handle, "echo -n $HOME").await {
                Ok(h) if !h.trim().is_empty() => h.trim().to_string(),
                _ => return, // 探测不到远端 HOME：放弃转发，不影响其余功能
            };
            let dir = format!("{home}/.ishell-mcp");
            let _ = exec_status(
                &fwd_handle,
                &format!(
                    "mkdir -p -m 700 {} && find {} -maxdepth 1 -name 'mcp-*.sock' -mmin +1440 -delete",
                    sh_quote(&dir),
                    sh_quote(&dir)
                ),
            )
            .await;
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let remote_path = format!("{dir}/mcp-{nonce}.sock");
            match fwd_handle.streamlocal_forward(remote_path.clone()).await {
                Ok(()) => *path_slot.lock().unwrap() = Some(remote_path),
                Err(e) => log::debug!("AI/MCP 反向转发注册失败：{e}"),
            }
        }))
    } else {
        None
    };

    // 3) 系统信息采集任务（独立 handle 克隆，互不阻塞）
    // 先探测 uname：非 Linux 或无 /proc 则禁用监控，避免空数据/误杀进程。
    let probe_handle = handle.clone();
    let probe_sink = sink.clone();
    let probe_task = tokio::spawn(async move {
        let linux_ok = match exec_capture(
            &probe_handle,
            "uname -s 2>/dev/null; test -r /proc/stat && echo HAS_PROC",
        )
        .await
        {
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
                    probe_sink.send_sysinfo(info);
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
                    spawn_sftp_reconnect();
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
                            // SFTP 未就绪（首次初始化失败，或上一轮重连本身超时/失败）：回报可重试
                            // 失败让 UI 保持自动重试的同时，这里也必须顺带再触发一次重连尝试——
                            // 否则 sftp 永远停在 None，之后每次 ListDir 都只会走到这个分支干等，
                            // 用户点多少次刷新都没用，只能整条 SSH 会话重连才能恢复（好网络下这条
                            // 分支只在启动瞬间命中一次，之前没暴露；弱网下一旦上一轮重连超时/失败
                            // 落到这里，就再也没有第二次机会）。
                            if !sftp_reconnecting {
                                sftp_reconnecting = true;
                                spawn_sftp_reconnect();
                            }
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
                    Some(UiCommand::Upload { id, local, remote_dir, remote_name, policy }) => {
                        let p = PendingXfer::Upload { id, local, remote_dir, remote_name, policy };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::UploadFromMcp { id, source, size, remote_path }) => {
                        let p = PendingXfer::UploadFromMcp { id, source, size, remote_path };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::DownloadToMcp { id, remote_path, download_sink }) => {
                        let p = PendingXfer::DownloadToMcp { id, remote_path, download_sink };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::RelayReadFile { id, remote_path, writer }) => {
                        let p = PendingXfer::RelayReadFile { id, remote_path, writer };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::RelayWriteFile { id, remote_path, size, reader }) => {
                        let p = PendingXfer::RelayWriteFile { id, remote_path, size, reader };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    // 跨会话拷贝-直连优先模式：这三条都是轻量的一次性操作（几条 exec 命令，
                    // 或一次 scp/rsync 直传），不经过 PendingXfer/active_xfer 那套 SFTP 传输
                    // 排队机制，直接各自 spawn 一个任务、结果经 WorkerEvent 回报即可。
                    Some(UiCommand::TrustTempKey { op_id, pub_key_line }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            match trust_temp_key(&h, &pub_key_line).await {
                                Ok(()) => s.send(WorkerEvent::TempKeyTrusted { op_id, ok: true, message: String::new() }),
                                Err(e) => s.send(WorkerEvent::TempKeyTrusted { op_id, ok: false, message: e.to_string() }),
                            }
                        });
                    }
                    Some(UiCommand::UntrustTempKey { op_id, marker }) => {
                        let h = handle.clone();
                        let s = sink.clone();
                        tokio::spawn(async move {
                            let _ = untrust_temp_key(&h, &marker).await;
                            s.send(WorkerEvent::TempKeyUntrusted { op_id });
                        });
                    }
                    Some(UiCommand::DirectRelayCopy {
                        op_id,
                        src_path,
                        dest_user,
                        dest_host,
                        dest_port,
                        dest_path,
                        priv_key_pem,
                        cancel,
                    }) => {
                        if let Some(sftp) = &sftp {
                            let h = handle.clone();
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                direct_relay_copy(
                                    h, sftp, op_id, src_path, dest_user, dest_host, dest_port,
                                    dest_path, priv_key_pem, cancel, &s,
                                )
                                .await;
                            });
                        } else {
                            sink.send(WorkerEvent::DirectRelayDone {
                                op_id,
                                ok: false,
                                message: crate::i18n::tr("SFTP 未就绪", "SFTP not ready").into(),
                            });
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
                        commands::spawn_pdf_info(handle.clone(), sink.clone(), id, path);
                    }
                    Some(UiCommand::PdfPage { path, page, dpi }) => {
                        commands::spawn_pdf_page(handle.clone(), sink.clone(), path, page, dpi);
                    }
                    Some(UiCommand::PdfSearch { path, query }) => {
                        commands::spawn_pdf_search(handle.clone(), sink.clone(), path, query);
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
                                    let mut data = Vec::with_capacity(size.min(DOC_LIMIT) as usize);
                                    let mut buf = vec![0u8; 128 * 1024];
                                    let mut last = 0usize;
                                    loop {
                                        let n = f.read(&mut buf).await?;
                                        if n == 0 {
                                            break;
                                        }
                                        data.extend_from_slice(&buf[..n]);
                                        // metadata.size 只是读取前的一次性快照：服务端不报告/请求失败/
                                        // 文件读取期间持续增长/上报了不准确的大小，都可能让实际读到的
                                        // 字节数远超上面检查过的 size。这里对累计长度做真正的硬上限，
                                        // 不能只靠开始前那一次检查。
                                        if data.len() as u64 > DOC_LIMIT {
                                            anyhow::bail!(match crate::i18n::current() {
                                                crate::i18n::Lang::Zh => format!("文档过大（>{}MB）", DOC_LIMIT / 1024 / 1024),
                                                crate::i18n::Lang::En => format!("Document too large (>{}MB)", DOC_LIMIT / 1024 / 1024),
                                            });
                                        }
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
                            if let UiCommand::WriteFile { id, path, .. } = &cmd {
                                sink.send(WorkerEvent::FileSaveFailed { id: *id, path: path.clone(), message: crate::i18n::tr("SFTP 不可用", "SFTP unavailable").into() });
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

    // 连接生命周期末尾清理本次注册的 AI/MCP 反向转发 socket 文件（`handle` 此时尚未被
    // drop，`Disconnect` 分支只 `shell.eof()` + `break`，仍可以再开一条 exec 通道）——
    // 照抄 `TmpKeyGuard`（xfer/direct.rs）"连接末尾再清理一次"的既有模式；失败/连接已经
    // 物理断开都 `let _ =` 吞掉，不阻塞退出。
    // 反向转发注册在独立任务里异步完成；若本次连接在它注册完成之前就结束，直接 take 槽位会
    // 读到 None → 漏清理，随后任务才把 socket 注册上去，就永久遗留（只能靠下次连接时的 24h
    // 兜底清理）。这里先给注册任务一个短暂收尾窗口（注册可能卡在 streamlocal_forward，故带
    // 超时、不无限等），让它有机会把 remote_path 落进槽位，再 take 清理。
    if let Some(task) = mcp_forward_task {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), task).await;
    }
    let forward_cleanup = mcp_forward_path.lock().unwrap().take();
    if let Some(remote_path) = forward_cleanup {
        let _ = exec_status(&handle, &format!("rm -f {}", sh_quote(&remote_path))).await;
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
