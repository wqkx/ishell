//! SSH 后台 worker：运行在 tokio 运行时上，负责建立连接、维护交互式 shell
//! 通道、SFTP 通道，并周期性采集系统信息。通过 channel 与 UI 线程通信。

mod forward;
pub mod sysinfo;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle, Handler};
use russh::keys::ssh_key;
use russh::{Channel, ChannelMsg};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::proto::{AuthMethod, ConflictPolicy, ConnectConfig, FileEntry, UiCommand, WorkerEvent};
use sysinfo::{SysSampler, PROBE_CMD};

/// 同一会话同时进行的最大传输数（不同会话各自独立）。
const MAX_CONCURRENT_XFER: usize = 6;

/// russh-sftp 每请求超时（秒）。默认 10s 对弱网大目录略紧，放宽到 20s；通道真死时会以
/// 「sender dropped / session closed」快速报错，不会被此超时拖满。
const SFTP_REQUEST_TIMEOUT_SECS: u64 = 20;

/// 待执行/进行中的传输任务描述。
enum PendingXfer {
    Download { id: u64, remote: String, local: String, policy: ConflictPolicy },
    Upload { id: u64, local: String, remote_dir: String, policy: ConflictPolicy },
    /// 跨主机直传：在本（源）主机上 rsync/scp 直推到目标主机
    Direct(Box<crate::proto::DirectSpec>),
}

impl PendingXfer {
    fn id(&self) -> u64 {
        match self {
            PendingXfer::Download { id, .. } | PendingXfer::Upload { id, .. } => *id,
            PendingXfer::Direct(d) => d.id,
        }
    }
}

/// 单个传输的取消句柄：
/// - `flag` 供传输内部（含 download 已 detach 的分块子任务）协作式中止；
/// - `stop` 一次性信号，触发后直接 drop 整个传输 future——能立即中止卡在
///   SFTP `flush`/`shutdown`（pipelined 写的真正落地处）里的上传，标志位无法覆盖这里。
struct XferCancel {
    flag: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

/// 启动一个传输任务：登记取消句柄，spawn 后台任务，完成时通过 `done_tx` 通知主循环。
fn start_xfer(
    handle: &Arc<Handle<ClientHandler>>,
    sink: &UiSink,
    done_tx: &UnboundedSender<u64>,
    cancels: &mut HashMap<u64, XferCancel>,
    p: PendingXfer,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let id = p.id();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    cancels.insert(id, XferCancel { flag: cancel.clone(), stop: Some(stop_tx) });
    let h = handle.clone();
    let s = sink.clone();
    let s_cancel = sink.clone();
    let cancel_work = cancel.clone();
    let dtx = done_tx.clone();
    tokio::spawn(async move {
        // 实际传输；被取消时整个 future 在 select 中被 drop，正在进行的 SFTP 写/flush 立即中止
        let work = async move {
            match open_sftp(&h).await {
                Ok(sftp) => {
                    let sftp = Arc::new(sftp);
                    match p {
                        PendingXfer::Download { id, remote, local, policy } => download(h.clone(), sftp, id, remote, local, policy, &s, cancel_work).await,
                        PendingXfer::Upload { id, local, remote_dir, policy } => upload(sftp.as_ref(), id, local, remote_dir, policy, &s, cancel_work).await,
                        PendingXfer::Direct(spec) => direct_transfer(h.clone(), sftp, *spec, &s, cancel_work).await,
                    }
                }
                Err(e) => s.send(WorkerEvent::TransferDone {
                    id, ok: false, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"), crate::i18n::Lang::En => format!("SFTP unavailable: {e}") }, refresh_dir: None,
                }),
            }
        };
        tokio::select! {
            biased; // 先看传输是否已完成，避免「刚好完成」时还报成取消
            _ = work => {}
            _ = &mut stop_rx => {
                // 通知可能已 detach 的子任务（download 分块）也尽快停下
                cancel.store(true, Ordering::Relaxed);
                s_cancel.send(WorkerEvent::TransferDone {
                    id, ok: false, message: crate::i18n::tr("已取消", "Canceled").into(), refresh_dir: None,
                });
            }
        }
        let _ = dtx.send(id);
    });
}

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

/// russh 客户端回调处理器：校验主机密钥（known_hosts + 首次信任 TOFU）。
/// UI 对主机密钥确认的回复通道；跳板机与目标主机共享（顺序询问，不并发）。
type HostKeyDecision = Arc<tokio::sync::Mutex<UnboundedReceiver<bool>>>;

struct ClientHandler {
    host: String,
    port: u16,
    sink: UiSink,
    /// UI 对"是否信任新主机/接受变更密钥"的回复
    decision_rx: HostKeyDecision,
    /// 是否转发本机 ssh-agent：为真时桥接远端回连的 auth-agent 通道到本地 agent
    agent_forward: bool,
}

/// 用户主目录下的 known_hosts 路径（与 russh 内部一致）。
fn known_hosts_file() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::tr("找不到用户主目录", "Home directory not found")))?;
    Ok(std::path::PathBuf::from(home).join(".ssh").join("known_hosts"))
}

/// 主机密钥变更后用户确认接受：删除 known_hosts 中该主机的旧行，再写入新键。
fn replace_known_host(host: &str, port: u16, new_key: &ssh_key::PublicKey) -> anyhow::Result<()> {
    // 收集匹配该主机的行号（russh 的匹配能处理哈希主机名）
    let remove: std::collections::HashSet<usize> = russh::keys::known_hosts::known_host_keys(host, port)
        .unwrap_or_default()
        .into_iter()
        .map(|(line, _)| line)
        .collect();
    let path = known_hosts_file()?;
    if let Ok(content) = std::fs::read_to_string(&path) {
        // known_host_keys 的行号从 1 计；过滤掉这些行后回写
        let kept: String = content
            .lines()
            .enumerate()
            .filter(|(i, _)| !remove.contains(&(i + 1)))
            .map(|(_, l)| format!("{l}\n"))
            .collect();
        std::fs::write(&path, kept)?;
    }
    russh::keys::known_hosts::learn_known_hosts(host, port, new_key)?;
    Ok(())
}

/// 主机密钥确认等待上限：UI 异常关闭/卡住时避免 worker 永久挂起。
const HOSTKEY_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

impl ClientHandler {
    /// 未知或变更的主机密钥：弹窗请用户确认（changed=true 表示密钥已变更）。
    /// 超时或通道关闭视为拒绝。
    async fn ask_trust(&mut self, fp: String, changed: bool) -> bool {
        self.sink.send(WorkerEvent::HostKeyPrompt {
            host: format!("{}:{}", self.host, self.port),
            fingerprint: fp,
            changed,
        });
        let mut rx = self.decision_rx.lock().await;
        match tokio::time::timeout(HOSTKEY_DECISION_TIMEOUT, rx.recv()).await {
            Ok(Some(true)) => true,
            Ok(Some(false)) | Ok(None) => false,
            Err(_) => {
                self.sink.send(WorkerEvent::Status(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => "主机密钥确认超时，已拒绝连接".into(),
                    crate::i18n::Lang::En => "Host key confirmation timed out; connection rejected".into(),
                }));
                false
            }
        }
    }
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    // 远端进程使用 SSH_AUTH_SOCK 时，服务器经此回调打开 auth-agent 通道；
    // 把它与本机 ssh-agent socket 双向对接，即实现 agent 转发（-A）。
    async fn server_channel_open_agent_forward(
        &mut self,
        channel: Channel<client::Msg>,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if self.agent_forward {
            tokio::spawn(async move {
                if let Err(e) = bridge_local_agent(channel).await {
                    log::debug!("agent 转发桥接结束：{e}");
                }
            });
        }
        Ok(())
    }

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string();
        match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
            // 已记录且匹配
            Ok(true) => Ok(true),
            // 未知主机 -> 请 UI 确认（TOFU），同意则写入 known_hosts
            Ok(false) => {
                if self.ask_trust(fp, false).await {
                    let _ = russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, server_public_key);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            // 已记录但密钥不一致 -> 可能中间人攻击；在 UI 内确认是否接受新键并替换旧行
            Err(_) => {
                if self.ask_trust(fp, true).await {
                    if let Err(e) = replace_known_host(&self.host, self.port, server_public_key) {
                        self.sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("更新 known_hosts 失败：{e}"), crate::i18n::Lang::En => format!("Failed to update known_hosts: {e}") }));
                        return Ok(false);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }
}

/// 跳板机回调处理器：校验其 known_hosts，未知则自动信任并记录（首次连接）。
struct JumpHandler {
    host: String,
    port: u16,
    sink: UiSink,
    /// 与目标主机共享的确认通道（跳板机先于目标询问，不并发）
    decision_rx: HostKeyDecision,
}

impl JumpHandler {
    async fn ask_trust(&mut self, fp: String, changed: bool) -> bool {
        self.sink.send(WorkerEvent::HostKeyPrompt {
            host: format!("{}:{} (jump)", self.host, self.port),
            fingerprint: fp,
            changed,
        });
        matches!(self.decision_rx.lock().await.recv().await, Some(true))
    }
}

impl Handler for JumpHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key.fingerprint(ssh_key::HashAlg::Sha256).to_string();
        match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true),
            // 跳板机首次连接也需 TOFU 用户确认（不再自动信任，防中间人冒充堡垒机）
            Ok(false) => {
                if self.ask_trust(fp, false).await {
                    let _ = russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, server_public_key);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(_) => {
                if self.ask_trust(fp, true).await {
                    if let Err(e) = replace_known_host(&self.host, self.port, server_public_key) {
                        self.sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("更新 known_hosts 失败：{e}"), crate::i18n::Lang::En => format!("Failed to update known_hosts: {e}") }));
                        return Ok(false);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
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

/// 用指定认证方式完成一次 SSH 认证。
async fn authenticate<H>(
    handle: &mut Handle<H>,
    username: &str,
    auth: &AuthMethod,
    sink: &UiSink,
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    let ok = match auth {
        AuthMethod::Interactive => authenticate_interactive(handle, username, sink, cmd_rx).await?,
        AuthMethod::Password(pw) => handle.authenticate_password(username, pw).await?.success(),
        AuthMethod::KeyFile { path, passphrase } => {
            let key = russh::keys::load_secret_key(path, passphrase.as_deref())?;
            // RSA 密钥须用 rsa-sha2-512 签名（None 会退化为 SHA-1 的 ssh-rsa，被现代 OpenSSH 拒绝）。
            handle
                .authenticate_publickey(
                    username,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), Some(russh::keys::HashAlg::Sha512)),
                )
                .await?
                .success()
        }
        AuthMethod::Agent => authenticate_agent(handle, username).await?,
    };
    Ok(ok)
}

/// 用本机 ssh-agent 中的私钥逐个尝试认证。
async fn authenticate_agent<H>(handle: &mut Handle<H>, username: &str) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    use russh::keys::agent::client::AgentClient;
    use russh::keys::agent::AgentIdentity;

    let cannot = |e: String| match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("无法连接 ssh-agent（SSH_AUTH_SOCK 是否已设置？）：{e}"),
        crate::i18n::Lang::En => format!("Cannot reach ssh-agent: {e}"),
    };
    #[cfg(unix)]
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| anyhow::anyhow!("{}", cannot(e.to_string())))?
        .dynamic();
    #[cfg(windows)]
    let mut agent = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map_err(|e| anyhow::anyhow!("{}", cannot(e.to_string())))?
        .dynamic();

    let ids = agent.request_identities().await?;
    if ids.is_empty() {
        anyhow::bail!("{}", crate::i18n::tr("ssh-agent 中没有可用私钥（先 ssh-add）", "No keys in ssh-agent (run ssh-add)"));
    }
    for id in ids {
        let AgentIdentity::PublicKey { key, .. } = id else { continue };
        // RSA 须用 rsa-sha2-512；其它算法 hash_alg 用 None
        let hash_alg = if matches!(key.algorithm(), russh::keys::ssh_key::Algorithm::Rsa { .. }) {
            Some(russh::keys::HashAlg::Sha512)
        } else {
            None
        };
        if handle
            .authenticate_publickey_with(username, key, hash_alg, &mut agent)
            .await?
            .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 把远端 agent-forward 通道与本机 ssh-agent（unix socket / Windows 命名管道）双向对接。
async fn bridge_local_agent(channel: Channel<client::Msg>) -> anyhow::Result<()> {
    let mut remote = channel.into_stream();
    #[cfg(unix)]
    {
        let sock = std::env::var("SSH_AUTH_SOCK").map_err(|_| anyhow::anyhow!("{}", crate::i18n::tr("SSH_AUTH_SOCK 未设置", "SSH_AUTH_SOCK not set")))?;
        let mut local = tokio::net::UnixStream::connect(sock).await?;
        tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
    }
    #[cfg(windows)]
    {
        let mut local = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(r"\\.\pipe\openssh-ssh-agent")?;
        tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
    }
    Ok(())
}

/// 键盘交互（keyboard-interactive）认证：循环把服务器提示交给 UI、等回答再提交，
/// 直至成功或失败。支持 OTP / 二次验证等多步提示。响应经 `cmd_rx` 收 `KbdResponse`。
async fn authenticate_interactive<H>(
    handle: &mut Handle<H>,
    username: &str,
    sink: &UiSink,
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    use client::KeyboardInteractiveAuthResponse as Resp;
    let mut resp = handle
        .authenticate_keyboard_interactive_start(username.to_string(), None)
        .await?;
    loop {
        match resp {
            Resp::Success => return Ok(true),
            Resp::Failure { .. } => return Ok(false),
            Resp::InfoRequest { name, instructions, prompts } => {
                // 空提示组（部分服务器仅发指示信息）：直接回空响应推进
                if prompts.is_empty() {
                    resp = handle.authenticate_keyboard_interactive_respond(Vec::new()).await?;
                    continue;
                }
                sink.send(WorkerEvent::KbdPrompt {
                    name,
                    instructions,
                    prompts: prompts.iter().map(|p| (p.prompt.clone(), p.echo)).collect(),
                });
                // 等 UI 回答；连接前不会有其它指令，收到断开/通道关闭即视为取消
                let answers = loop {
                    match cmd_rx.recv().await {
                        Some(UiCommand::KbdResponse(a)) => break a,
                        Some(UiCommand::Disconnect) | None => return Ok(false),
                        _ => {}
                    }
                };
                resp = handle.authenticate_keyboard_interactive_respond(answers).await?;
            }
        }
    }
}

/// 建立 TCP + SSH 握手并完成认证。可选经跳板机（ProxyJump）连接。
/// 返回目标主机句柄，以及需保持存活的跳板机句柄（None 表示直连）。
async fn connect(
    cfg: &ConnectConfig,
    sink: &UiSink,
    hostkey_rx: UnboundedReceiver<bool>,
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
) -> anyhow::Result<(Handle<ClientHandler>, Option<Handle<JumpHandler>>)> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    // 主机密钥确认通道：跳板机与目标主机共享（顺序询问，不并发）
    let decision_rx: HostKeyDecision = Arc::new(tokio::sync::Mutex::new(hostkey_rx));

    let target_handler = ClientHandler {
        host: cfg.host.clone(),
        port: cfg.port,
        sink: sink.clone(),
        decision_rx: decision_rx.clone(),
        agent_forward: cfg.forward_agent,
    };

    let (mut handle, jump_keep) = if let Some(jump) = &cfg.jump {
        // 1) 先连跳板机并认证
        sink.send(WorkerEvent::Status(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("正在连接跳板机 {}:{} …", jump.host, jump.port), crate::i18n::Lang::En => format!("Connecting jump {}:{} …", jump.host, jump.port) }));
        let jhandler = JumpHandler { host: jump.host.clone(), port: jump.port, sink: sink.clone(), decision_rx: decision_rx.clone() };
        let mut jhandle = client::connect(config.clone(), (jump.host.as_str(), jump.port), jhandler).await?;
        if !authenticate(&mut jhandle, &jump.username, &jump.auth, sink, cmd_rx).await? {
            anyhow::bail!("{}", crate::i18n::tr("跳板机认证被拒绝", "Jump host auth rejected"));
        }
        // 2) 经跳板机打开到目标主机的 direct-tcpip 通道，并在该流上完成目标 SSH 握手
        sink.send(WorkerEvent::Status(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("经跳板机连接 {}:{} …", cfg.host, cfg.port), crate::i18n::Lang::En => format!("Via jump to {}:{} …", cfg.host, cfg.port) }));
        let ch = jhandle
            .channel_open_direct_tcpip(cfg.host.clone(), cfg.port as u32, "127.0.0.1", 0)
            .await?;
        let handle = client::connect_stream(config, ch.into_stream(), target_handler).await?;
        (handle, Some(jhandle))
    } else {
        let handle = client::connect(config, (cfg.host.as_str(), cfg.port), target_handler).await?;
        (handle, None)
    };

    sink.send(WorkerEvent::Status(crate::i18n::tr("正在认证 …", "Authenticating …").into()));
    if !authenticate(&mut handle, &cfg.username, &cfg.auth, sink, cmd_rx).await? {
        anyhow::bail!("{}", crate::i18n::tr("认证被拒绝（用户名/密码或密钥错误）", "Authentication rejected (bad credentials)"));
    }
    Ok((handle, jump_keep))
}

/// 打开带 PTY 的交互式 shell 通道。`forward_agent` 为真时请求 agent 转发。
async fn open_shell(handle: &Handle<ClientHandler>, forward_agent: bool) -> anyhow::Result<russh::Channel<client::Msg>> {
    // request_pty/request_shell 均为 &self，channel 之后按值返回，无需 mut
    let channel = handle.channel_open_session().await?;
    // 在该会话通道上请求 agent 转发；服务器随后回连的 auth-agent 通道由
    // ClientHandler::server_channel_open_agent_forward 桥接到本机 agent。
    if forward_agent {
        let _ = channel.agent_forward(false).await;
    }
    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await?;
    // 请求 UTF-8 locale：否则远端 ls 等会把中文文件名转义成 $'\345\277...'。
    // 多数 sshd 默认 AcceptEnv LANG LC_*；不被接受时忽略即可。
    let _ = channel.set_env(false, "LANG", "en_US.UTF-8").await;
    let _ = channel.set_env(false, "LC_ALL", "en_US.UTF-8").await;
    channel.request_shell(false).await?;
    Ok(channel)
}

/// 在独立通道上打开 SFTP 子系统。
async fn open_sftp(
    handle: &Handle<ClientHandler>,
) -> anyhow::Result<russh_sftp::client::SftpSession> {
    let channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await?;
    // 放宽每请求超时（默认 10s）：弱网下大目录列举/元数据往返较慢，给足时间避免误判失败；
    // 通道真死时 russh-sftp 会以「sender dropped / session closed」快速报错，不受此超时拖累。
    sftp.set_timeout(SFTP_REQUEST_TIMEOUT_SECS);
    Ok(sftp)
}

/// 打开一次性 exec 通道执行命令并收集 stdout。
async fn exec_capture(handle: &Handle<ClientHandler>, cmd: &str) -> anyhow::Result<String> {
    // wait(&mut self) 需要可变借用
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut buf = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => buf.extend_from_slice(&data),
            ChannelMsg::ExitStatus { .. } => {}
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 执行命令并捕获二进制 stdout：返回 (退出码, stdout 字节, stderr 文本)。
/// 与 exec_capture 的区别：stdout 不做 UTF-8 转换（PDF 页 PNG 等二进制输出用）。
async fn exec_capture_bytes(handle: &Handle<ClientHandler>, cmd: &str) -> anyhow::Result<(i32, Vec<u8>, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut code = -1i32;
    // 读到通道关闭为止（ExitStatus 可能在 Eof 前后到达，不能提前 break）
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, ext } if ext == 1 => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, out, String::from_utf8_lossy(&err).into_owned()))
}

/// 执行命令，返回 (退出码, stderr)。
async fn exec_status(handle: &Handle<ClientHandler>, cmd: &str) -> anyhow::Result<(i32, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut code = -1i32;
    let mut err = Vec::new();
    // 注意：ExitStatus 通常在 Eof 之前到达，但不能在 Eof 处提前 break，
    // 否则可能漏掉退出码；这里一直读到通道关闭（wait 返回 None）。
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::ExtendedData { data, ext } if ext == 1 => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, String::from_utf8_lossy(&err).into_owned()))
}

/// 生成 n 字节的随机十六进制串（用于临时文件名，避免可预测路径被 symlink 抢占）。
fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if getrandom::getrandom(&mut b).is_err() {
        // getrandom 失败（极罕见）：用 pid + 单调计数器 + 栈地址(ASLR) 混出非常量回退，
        // 避免退化为固定全零名——临时文件名靠它防 /tmp 共享目录上的可预测 symlink 抢占。
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let mut x = (std::process::id() as u64)
            ^ CTR.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            ^ (&b as *const _ as u64);
        for byte in b.iter_mut() {
            // splitmix64 扩展
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *byte = ((z ^ (z >> 31)) & 0xff) as u8;
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// POSIX 单引号转义，用于把路径安全嵌入 shell 命令。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 本地解压 tar.gz 到 dest 目录（纯 Rust，不依赖系统 tar）。
/// 逐条校验路径：拒绝绝对路径、`..` 组件与指向 dest 外的链接，防止路径穿越写任意本地文件。
fn extract_tar_gz(path: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    let f = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    std::fs::create_dir_all(dest)?;
    let dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    for entry in ar.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        if !tar_entry_path_safe(&entry_path) {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("拒绝不安全的归档路径：{}", entry_path.display()),
                    crate::i18n::Lang::En => format!("Refusing unsafe archive path: {}", entry_path.display()),
                }
            );
        }
        // unpack_in 将相对路径落在 dest 下；返回 false 表示被跳过（含 ..）
        if !entry.unpack_in(&dest)? {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("归档条目无法安全解压：{}", entry_path.display()),
                    crate::i18n::Lang::En => format!("Archive entry could not be unpacked safely: {}", entry_path.display()),
                }
            );
        }
    }
    Ok(())
}

/// 归档条目路径是否可安全解压到目标目录内（相对路径、无 `..`、非绝对）。
fn tar_entry_path_safe(p: &std::path::Path) -> bool {
    use std::path::Component;
    if p.as_os_str().is_empty() || p.is_absolute() {
        return false;
    }
    for c in p.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// 读取远程目录，返回（规范化后的绝对路径, 条目列表）。目录在前、按名排序。
async fn list_dir(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    path: &str,
) -> Result<(String, Vec<FileEntry>), russh_sftp::client::error::Error> {
    let canon = sftp.canonicalize(path).await.unwrap_or_else(|_| path.to_string());
    let dir = sftp.read_dir(&canon).await?;
    let mut entries = Vec::new();
    for item in dir {
        let name = item.file_name();
        if name == "." || name == ".." {
            continue;
        }
        // read_dir 的元数据为 lstat 语义（针对链接自身），故能据类型位判出 is_link；
        // 链接的真实目标/类型由 resolve_symlinks 通过 stat（跟随）二次解析。
        let meta = item.metadata();
        let perm = meta.permissions.unwrap_or(0);
        let is_dir = meta.is_dir();
        let is_link = perm & 0o170000 == 0o120000;
        entries.push(FileEntry {
            name,
            is_dir,
            is_link,
            size: meta.size.unwrap_or(0),
            mtime: meta.mtime.unwrap_or(0) as u64,
            perm: perm & 0o777,
            owner: meta.uid.map(|u| u.to_string()).unwrap_or_default(),
            link_target: None,
            link_dir: false,
        });
    }
    resolve_symlinks(sftp, &canon, &mut entries).await;
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok((canon, entries))
}

/// 跟随解析目录中的符号链接：填入「规范目标路径」「目标是否目录」「目标大小」。
///
/// - 用 `metadata`（stat，跟随链接）判目标类型/大小；用 `canonicalize` 取最终真实路径供展示。
/// - 仅当 stat 成功（目标存在）才视为「已解析」并回填 target；断链则 target 留 `None`（UI 标红提示）。
/// - 文件型链接顺带把 `size` 改为目标大小（lstat 给的是链接本身长度，对用户无意义）。
/// - 并发受 `Semaphore` 限制、总数设上限，避免目录含大量链接时在高延迟链路上拖慢列目录。
async fn resolve_symlinks(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    dir: &str,
    entries: &mut [FileEntry],
) {
    /// 单次列目录最多解析的链接数（超出的链接仅显示为链接、不带目标/跟随能力）。
    const MAX_LINKS: usize = 256;
    /// 并发解析的上限（每个链接 1~2 次 SFTP 往返）。
    const CONCURRENCY: usize = 16;

    let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let mut tasks = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !e.is_link {
            continue;
        }
        if tasks.len() >= MAX_LINKS {
            break;
        }
        let full = join_remote(dir, &e.name);
        let sftp = sftp.clone();
        let sem = sem.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            // 先 stat（跟随）；失败即断链，不再 canonicalize（避免回填指向不存在路径）。
            let meta = sftp.metadata(&full).await.ok();
            match meta {
                Some(m) => {
                    let target = sftp.canonicalize(&full).await.ok();
                    (i, target, m.is_dir(), m.size)
                }
                None => (i, None, false, None),
            }
        }));
    }
    for t in tasks {
        if let Ok((i, target, is_dir, size)) = t.await {
            if let Some(e) = entries.get_mut(i) {
                e.link_target = target;
                e.link_dir = is_dir;
                // 文件型链接：展示目标大小（目录链接大小列显示 "-"，无需回填）
                if !is_dir {
                    if let Some(sz) = size {
                        e.size = sz;
                    }
                }
            }
        }
    }
}

/// 下载（单文件或整个目录）并上报进度。大文件用多个并发分段读取流水线化，
/// 抵消 SFTP「单请求等一个往返」的吞吐瓶颈（高延迟链路上提速明显）。
/// 压缩下载一个目录：远端 tar.gz 打包到临时文件 → 单文件并发下载 → 本地解包。
/// 进度按压缩包字节上报。返回 Err 表示不支持/失败（上层回退到逐文件）。
async fn download_dir_compressed(
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
    sink.send(WorkerEvent::TransferStart { id, name: name.clone(), total: 0, dir: crate::proto::TransferDir::Download, local: Some(local.to_string()) });
    sink.send(WorkerEvent::TransferNote { id, note: crate::i18n::tr("打包中…", "Packing…").into() });

    // 远端打包（czf：gzip 默认级别；-C 进入父目录，仅打包目标目录名）
    let cmd = format!("tar czf {} -C {} {}", sh_quote(&tmp_remote), sh_quote(&parent), sh_quote(&name));
    let (code, err) = exec_status(handle, &cmd).await?;
    if code != 0 {
        let _ = exec_status(handle, &format!("rm -f {}", sh_quote(&tmp_remote))).await;
        anyhow::bail!("{}", match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("tar 打包失败（{code}）：{err}"),
            crate::i18n::Lang::En => format!("tar pack failed ({code}): {err}"),
        });
    }
    let size = sftp.metadata(&tmp_remote).await.ok().and_then(|m| m.size).unwrap_or(0);
    // 打包完成：更新真实总量并清除阶段提示，进入正常字节进度
    sink.send(WorkerEvent::TransferStart { id, name: name.clone(), total: size, dir: crate::proto::TransferDir::Download, local: Some(local.to_string()) });
    sink.send(WorkerEvent::TransferNote { id, note: String::new() });

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
    sink.send(WorkerEvent::TransferNote { id, note: crate::i18n::tr("解包中…", "Extracting…").into() });
    tokio::task::spawn_blocking(move || extract_tar_gz(&tgz, &dest)).await??;
    sink.send(WorkerEvent::TransferNote { id, note: String::new() });
    let _ = std::fs::remove_file(&local_tgz);
    Ok(())
}

/// 把一组路径用 POSIX 单引号转义后空格拼接（末尾带一个空格），用于安全嵌入 shell 命令。
fn join_quoted(items: &[String]) -> String {
    let mut s = String::new();
    for p in items {
        s.push_str(&sh_quote(p));
        s.push(' ');
    }
    s
}

/// 直传临时私钥目录的清理守卫：无论正常返回、`?` 早退，还是被取消（future 被 drop），
/// Drop 时都异步清除源主机上的临时私钥目录——避免目标主机私钥残留在源主机（凭据泄露）。
/// 取消路径下本函数栈已被展开，无法 `.await`，故 detach 一个清理任务到当前运行时。
struct TmpKeyGuard {
    handle: Arc<Handle<ClientHandler>>,
    dir: String,
}

impl Drop for TmpKeyGuard {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        let d = sh_quote(&self.dir);
        tokio::spawn(async move {
            let _ = exec_status(&handle, &format!("shred -uf {d}/key 2>/dev/null; rm -rf {d}")).await;
        });
    }
}

/// 跨主机「直传」：在源主机上用 rsync（无则 scp）把 srcs 直接推到目标主机，数据不经本地。
/// 目标认证仅支持「无口令密钥」：把 B 私钥临时投放到源主机 0700 私有目录，传完/取消即清（见 TmpKeyGuard）。
/// 任一步失败都回报失败，由上层弹「转中转」提醒。
async fn direct_transfer(
    handle: Arc<Handle<ClientHandler>>,
    sftp: Arc<russh_sftp::client::SftpSession>,
    spec: crate::proto::DirectSpec,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let id = spec.id;
    // 先用 du 估算源端总字节，让进度条与文件大小都按真实字节显示
    let total = direct_total_bytes(&handle, &spec.srcs).await;
    sink.send(WorkerEvent::TransferStart { id, name: spec.label.clone(), total, dir: crate::proto::TransferDir::Upload, local: None });
    let res = direct_transfer_inner(&handle, &sftp, &spec, sink, &cancel).await;
    match res {
        Ok(()) => {
            // 收尾把进度拉满（du 估算与实传可能有微小出入）
            if total > 0 {
                sink.send(WorkerEvent::TransferProgress { id, done: total });
            }
            sink.send(WorkerEvent::TransferDone {
                id, ok: true,
                message: crate::i18n::tr("直传完成", "Direct transfer done").into(),
                refresh_dir: None,
            });
        }
        Err(e) => sink.send(WorkerEvent::TransferDone {
            id, ok: false, message: format!("{e}"), refresh_dir: None,
        }),
    }
}

/// 用 du 估算一组源路径的总字节（grand total 行）；失败返回 0（进度条退化为不确定）。
async fn direct_total_bytes(handle: &Handle<ClientHandler>, srcs: &[String]) -> u64 {
    let q = join_quoted(srcs);
    // -s 汇总、-b 字节、-c 末尾输出 total 行；取 total 行的首列
    let out = exec_capture(handle, &format!("du -sbc -- {q}2>/dev/null | tail -1")).await.unwrap_or_default();
    out.split_whitespace().next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0)
}

async fn direct_transfer_inner(
    handle: &Arc<Handle<ClientHandler>>,
    sftp: &Arc<russh_sftp::client::SftpSession>,
    spec: &crate::proto::DirectSpec,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // 1) 读取本地 B 私钥（本进程可直接访问本地文件）
    let key_bytes = std::fs::read(&spec.key_path).map_err(|e| anyhow::anyhow!("{}", match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("读取目标私钥失败：{e}"),
        crate::i18n::Lang::En => format!("Read target key failed: {e}"),
    }))?;
    // 2) 先建 0700 私有目录（mkdir -m 700 在创建时即定权，杜绝「写入→改权」之间的可读窗口），
    //    密钥放其中。TmpKeyGuard 保证正常/失败/取消(future 被 drop) 各路径都清理该目录。
    let tmp_dir = format!("/tmp/.ishell_kd_{}_{}", spec.id, rand_hex(8));
    if !matches!(exec_status(handle, &format!("mkdir -m 700 {}", sh_quote(&tmp_dir))).await, Ok((0, _))) {
        anyhow::bail!("{}", match crate::i18n::current() {
            crate::i18n::Lang::Zh => "创建临时密钥目录失败".to_string(),
            crate::i18n::Lang::En => "Create temp key dir failed".to_string(),
        });
    }
    let _key_guard = TmpKeyGuard { handle: handle.clone(), dir: tmp_dir.clone() };
    let tmp_key = format!("{tmp_dir}/key");
    sftp_overwrite(sftp, &tmp_key, &key_bytes).await.map_err(|e| anyhow::anyhow!("{}", match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("投放临时密钥失败：{e}"),
        crate::i18n::Lang::En => format!("Place temp key failed: {e}"),
    }))?;
    let _ = exec_status(handle, &format!("chmod 600 {}", sh_quote(&tmp_key))).await; // 纵深防御（目录已 700）

    // 3) 拼接源路径与目标
    let srcs = join_quoted(&spec.srcs);
    // 目标强制以 "/" 结尾，令 rsync/scp 把源放「入目录」而非改名
    let dest = sh_quote(&format!("{}@{}:{}/", spec.dest_user, spec.dest_host, spec.dest_dir));
    // 主机密钥策略：本机已信任目标 → yes（拒绝未知/变更）；用户确认过的首次 → accept-new。
    // 绝不使用 StrictHostKeyChecking=no。
    let hk = if spec.dest_host_known { "yes" } else { "accept-new" };
    let ssh_opt = format!(
        "ssh -p {} -i {} -o StrictHostKeyChecking={} -o BatchMode=yes -o ConnectTimeout=15",
        spec.dest_port, sh_quote(&tmp_key), hk
    );

    // 4) 优先 rsync（可解析进度、可续传）；缺失则回退 scp（仅 spinner）
    let has_rsync = matches!(exec_status(handle, "command -v rsync >/dev/null 2>&1").await, Ok((0, _)));
    let cmd = if has_rsync {
        format!("rsync -a --info=progress2 -e {} -- {}{}", sh_quote(&ssh_opt), srcs, dest)
    } else {
        // scp 用大写 -P 指定端口；其余 ssh 选项同样适用
        format!(
            "scp -P {} -i {} -o StrictHostKeyChecking={} -o BatchMode=yes -o ConnectTimeout=15 -r -- {}{}",
            spec.dest_port, sh_quote(&tmp_key), hk, srcs, dest
        )
    };

    // 临时私钥的清理交由 _key_guard 在作用域结束（含取消时的 future drop）异步完成
    let (code, err) = exec_direct_progress(handle, &cmd, spec.id, sink, cancel).await?;
    if code != 0 {
        let reason = err.trim();
        anyhow::bail!("{}", match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("直传失败（码 {code}）：{}", if reason.is_empty() { "源主机无法连到目标主机" } else { reason }),
            crate::i18n::Lang::En => format!("Direct transfer failed (code {code}): {}", if reason.is_empty() { "source host cannot reach target" } else { reason }),
        });
    }
    Ok(())
}

/// 执行直传命令并解析 rsync `--info=progress2` 的「已传字节」（首列），按字节上报进度。
/// 返回 (退出码, stderr)。被取消时（cancel 置位）提前结束读取，外层 select 也会 drop 整个 future。
async fn exec_direct_progress(
    handle: &Handle<ClientHandler>,
    cmd: &str,
    id: u64,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<(i32, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut code = -1i32;
    let mut err = Vec::new();
    let mut tail = String::new();
    while let Some(msg) = channel.wait().await {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match msg {
            ChannelMsg::Data { data } => {
                // rsync 进度写在 stdout，用 \r 原地刷新；累积当前行并提取首列「已传字节」
                tail.push_str(&String::from_utf8_lossy(&data));
                if let Some(done) = last_rsync_bytes(&tail) {
                    sink.send(WorkerEvent::TransferProgress { id, done });
                }
                // 仅保留最后一段（CR/LF 之后），防止缓冲无限增长；CR/LF 为 ASCII，切分点是字符边界
                if let Some(p) = tail.rfind(['\r', '\n']) {
                    tail = tail[p + 1..].to_string();
                }
            }
            ChannelMsg::ExtendedData { data, ext } if ext == 1 => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, String::from_utf8_lossy(&err).into_owned()))
}

/// 从 rsync `--info=progress2` 输出里提取「最新一行」的首列已传字节数。
/// 进度行形如 `  1,234,567  45%  10.50MB/s  0:00:05`，用 \r 原地刷新；
/// 取 CR/LF 后最后一段非空行的首 token，去掉千分位逗号后解析为字节。
fn last_rsync_bytes(s: &str) -> Option<u64> {
    let line = s.rsplit(['\r', '\n']).find(|seg| !seg.trim().is_empty())?;
    let tok = line.split_whitespace().next()?;
    let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// 给本地路径找一个不冲突的变体：`file.ext` → `file (1).ext`；目录 `dir` → `dir (1)`。
fn local_nonexistent(path: &str) -> String {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return path.to_string();
    }
    let is_dir = p.is_dir();
    let parent = p.parent();
    let fname = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = split_name(&fname, is_dir);
    for n in 1..10000u32 {
        let cand_name = match &ext { Some(e) => format!("{stem} ({n}).{e}"), None => format!("{stem} ({n})") };
        let cand = match parent { Some(d) => d.join(&cand_name), None => std::path::PathBuf::from(&cand_name) };
        if !cand.exists() {
            return cand.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// 给远端目录里的名字找一个不冲突的变体。
async fn remote_nonexistent(sftp: &russh_sftp::client::SftpSession, dir: &str, name: &str, is_dir: bool) -> String {
    let (stem, ext) = split_name(name, is_dir);
    for n in 1..10000u32 {
        let cand = match &ext { Some(e) => format!("{stem} ({n}).{e}"), None => format!("{stem} ({n})") };
        if sftp.metadata(&join_remote(dir, &cand)).await.is_err() {
            return cand;
        }
    }
    name.to_string()
}

/// 拆分文件名为 (主名, 扩展)；目录或无扩展时扩展为 None（首字符的点不算扩展）。
fn split_name(fname: &str, is_dir: bool) -> (String, Option<String>) {
    if is_dir {
        return (fname.to_string(), None);
    }
    match fname.rfind('.') {
        Some(d) if d > 0 => (fname[..d].to_string(), Some(fname[d + 1..].to_string())),
        _ => (fname.to_string(), None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn download(
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
    let is_dir = sftp.metadata(&remote).await.map(|m| m.is_dir()).unwrap_or(false);

    // 冲突处理：本地目标已存在时，按策略 跳过 / 重命名 / 覆盖
    let local = if std::path::Path::new(&local).exists() {
        match policy {
            ConflictPolicy::Skip => {
                sink.send(WorkerEvent::TransferDone { id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已跳过（本地已存在）：{name}"), crate::i18n::Lang::En => format!("Skipped (exists): {name}") }, refresh_dir: None });
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
                    id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已下载 {name}"), crate::i18n::Lang::En => format!("Downloaded {name}") }, refresh_dir: None,
                });
                return;
            }
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    sink.send(WorkerEvent::TransferDone { id, ok: false, message: crate::i18n::tr("已取消", "Canceled").into(), refresh_dir: None });
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
                        let rel = full.strip_prefix(remote.as_str()).unwrap_or(&full).trim_start_matches('/');
                        files.push((full.clone(), std::path::Path::new(&local).join(rel), meta.size.unwrap_or(0)));
                    }
                }
            }
        } else {
            let sz = sftp.metadata(&remote).await.ok().and_then(|m| m.size).unwrap_or(0);
            files.push((remote.clone(), std::path::PathBuf::from(&local), sz));
        }

        let total: u64 = files.iter().map(|f| f.2).sum();
        sink.send(WorkerEvent::TransferStart {
            id, name: name.clone(), total, dir: crate::proto::TransferDir::Download, local: Some(local.clone()),
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
                download_file(&sftp, &rpath, &lpath, size, sftp.metadata(&rpath).await.ok().and_then(|m| m.mtime).unwrap_or(0), &cancel, &done).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        stop.store(true, Ordering::Relaxed);
        let _ = prog.await;
        sink.send(WorkerEvent::TransferProgress { id, done: done.load(Ordering::Relaxed) });
        result
    }
    .await;

    match res {
        Ok(_) => sink.send(WorkerEvent::TransferDone {
            id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已下载 {name}"), crate::i18n::Lang::En => format!("Downloaded {name}") }, refresh_dir: None,
        }),
        Err(e) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() { crate::i18n::Lang::Zh => format!("下载失败：{e}"), crate::i18n::Lang::En => format!("Download failed: {e}") }
            };
            sink.send(WorkerEvent::TransferDone { id, ok: false, message, refresh_dir: None });
        }
    }
}

/// 同一文件内的并发分段数（流水线深度）。8 路足以在常见高延迟链路上跑满带宽。
const DL_PARALLEL: u64 = 8;
/// 每个分段一次抢占的字节数。
const DL_CHUNK: u64 = 1024 * 1024;
/// 单个文件传输遇到瞬时错误时的最大额外重试次数（配合断点续传）。
const XFER_RETRIES: u32 = 3;

/// 第 attempt 次重试前的退避时长（300ms·2^n，封顶约 4.8s）。
fn xfer_backoff(attempt: u32) -> Duration {
    Duration::from_millis(300u64 * (1u64 << attempt.min(4)))
}

/// 断点信息 sidecar 路径：`<local>.ishellpart`。
fn part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart");
    std::path::PathBuf::from(p)
}

/// 下载数据的临时文件路径：`<local>.ishellpart.data`。
/// 数据先写这里，全部完成后 rename 到目标——成功前绝不动目标文件；
/// 取消/失败只留 part 文件，目标（若原本存在）保持完好。
fn data_part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart.data");
    std::path::PathBuf::from(p)
}

/// 容纳 n 个分段标志位所需的字节数。
fn bitmap_len(n_chunks: u64) -> usize {
    n_chunks.div_ceil(8) as usize
}

/// 下载单个文件：大文件按偏移并发分段读取，定位写入本地，显著提升高延迟链路吞吐。
/// 数据全程写 `<local>.ishellpart.data`，完整后原子 rename 到目标——成功前不动目标文件。
/// `remote_mtime` 参与断点校验（0 = 不允许跨次续传，如临时打包文件）。
async fn download_file(
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
        Arc::new(std::fs::OpenOptions::new().read(true).write(true).open(&data_part)?) // 续传：保留已写分段
    } else {
        let f = std::fs::File::create(&data_part)?;
        f.set_len(size)?;
        Arc::new(f)
    };
    let chunk_done: Arc<Vec<AtomicBool>> = Arc::new(
        (0..n_chunks)
            .map(|i| {
                let d = resume_bm.as_ref().is_some_and(|b| (b[(i / 8) as usize] >> (i % 8)) & 1 == 1);
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
            let (sftp, out, cursor, done, cancel, chunk_done) =
                (sftp.clone(), out.clone(), cursor.clone(), done.clone(), cancel.clone(), chunk_done.clone());
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
fn finish_download(data_part: &std::path::Path, lpath: &std::path::Path) -> anyhow::Result<()> {
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
async fn download_small(
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

/// 在指定偏移定位写入（跨平台）。
fn pwrite(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut off = offset;
        let mut b = buf;
        while !b.is_empty() {
            let n = file.seek_write(b, off)?;
            b = &b[n..];
            off += n as u64;
        }
        Ok(())
    }
}

/// 分块上传并上报进度。
async fn upload(
    sftp: &russh_sftp::client::SftpSession,
    id: u64,
    local: String,
    remote_dir: String,
    policy: ConflictPolicy,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = local_basename(&local); // 本地路径用 Windows 兼容的取名（处理反斜杠/盘符）
    let is_dir = tokio::fs::metadata(&local).await.map(|m| m.is_dir()).unwrap_or(false);

    // 冲突处理：远端目标已存在时，按策略 跳过 / 重命名 / 覆盖
    let name = if sftp.metadata(&join_remote(&remote_dir, &name)).await.is_ok() {
        match policy {
            ConflictPolicy::Skip => {
                sink.send(WorkerEvent::TransferDone { id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已跳过（远端已存在）：{name}"), crate::i18n::Lang::En => format!("Skipped (exists): {name}") }, refresh_dir: None });
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
                    let rel = p.strip_prefix(&local_root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
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
            let sz = tokio::fs::metadata(&local).await.map(|m| m.len()).unwrap_or(0);
            files.push((std::path::PathBuf::from(&local), join_remote(&remote_dir, &name), sz));
        }

        let total: u64 = files.iter().map(|f| f.2).sum();
        sink.send(WorkerEvent::TransferStart {
            id, name: name.clone(), total, dir: crate::proto::TransferDir::Upload, local: None,
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
                match upload_file_once(sftp, &lpath, &rpath, &cancel, done_base, id, sink, &last, attempt > 0).await {
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
            sink.send(WorkerEvent::TransferProgress { id, done: done_base });
        }
        Ok(())
    }
    .await;
    match res {
        Ok(_) => sink.send(WorkerEvent::TransferDone {
            id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已上传 {name}"), crate::i18n::Lang::En => format!("Uploaded {name}") }, refresh_dir: Some(remote_dir),
        }),
        Err(e) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() { crate::i18n::Lang::Zh => format!("上传失败：{e}"), crate::i18n::Lang::En => format!("Upload failed: {e}") }
            };
            sink.send(WorkerEvent::TransferDone { id, ok: false, message, refresh_dir: None });
        }
    }
}

/// 上传单个文件：以远端已有大小为起点续传；带进度节流上报。
async fn upload_file_once(
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

    let local_size = tokio::fs::metadata(lpath).await.map(|m| m.len()).unwrap_or(0);
    // 续传只允许发生在**本次传输的失败重试**（allow_resume）：此时远端内容必然是
    // 本进程刚写入的本地前缀，按大小续写安全。首次尝试一律 TRUNCATE 从 0 全量写——
    // 盲按「远端大小 ≤ 本地大小」续传会把无关同名文件误判为已传前缀
    //（大小恰好相等时一个字节不写就报成功；远端较小时保留错误前缀再续尾部）。
    let start = if allow_resume {
        let remote_size = sftp.metadata(rpath).await.ok().and_then(|m| m.size).unwrap_or(0);
        if remote_size > 0 && remote_size <= local_size { remote_size } else { 0 }
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

fn basename(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string()
}

/// 取「本地」路径的文件名：同时按 `/` 和 `\` 切分，正确处理 Windows 路径
/// （否则 `C:\Users\x\a.txt` 会被当成整体文件名上传，远端文件名也带上盘符路径）。
fn local_basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// 探测字节的字符编码并解码为 String，返回 (文本, 编码名)。
/// UTF-8(含 BOM) 优先；非 UTF-8 用 chardetng 猜测（中文环境多为 GBK/GB18030）。
fn decode_text(data: &[u8]) -> (String, String) {
    // UTF-8 BOM
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (String::from_utf8_lossy(&data[3..]).into_owned(), "UTF-8".into());
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
/// 非 force 时限制 4MB 并拒绝含 NUL 的二进制；force（用户确认后）放宽到 128MB 且跳过二进制检查。
/// 跟随读取（tail -f）：从 offset 读到文件末尾（单次 ≤512KB）。
/// offset=u64::MAX 只返回当前大小（跟随开启时的初始化，相当于 `tail -f -n 0`）；
/// 文件变小（截断/轮转）时回报 truncated 并把 offset 重置为新大小。
async fn tail_file(sftp: &russh_sftp::client::SftpSession, path: &str, offset: u64, sink: &UiSink) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let size = match sftp.metadata(path).await {
        Ok(m) => m.size.unwrap_or(0),
        Err(_) => {
            // 瞬时错误（弱网等）：offset 原样返回，UI 下一轮重试
            sink.send(WorkerEvent::FileTail { path: path.to_string(), data: Vec::new(), offset, truncated: false });
            return;
        }
    };
    if offset == u64::MAX {
        sink.send(WorkerEvent::FileTail { path: path.to_string(), data: Vec::new(), offset: size, truncated: false });
        return;
    }
    if size < offset {
        sink.send(WorkerEvent::FileTail { path: path.to_string(), data: Vec::new(), offset: size, truncated: true });
        return;
    }
    if size == offset {
        sink.send(WorkerEvent::FileTail { path: path.to_string(), data: Vec::new(), offset, truncated: false });
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
            sink.send(WorkerEvent::FileTail { path: path.to_string(), data, offset: offset + n, truncated: false });
        }
        Err(_) => sink.send(WorkerEvent::FileTail { path: path.to_string(), data: Vec::new(), offset, truncated: false }),
    }
}

async fn read_file_chunked(sftp: &russh_sftp::client::SftpSession, path: &str, force: bool, id: u64, sink: &UiSink) {
    use tokio::io::AsyncReadExt;
    let limit = if force { 128 * 1024 * 1024 } else { 4 * 1024 * 1024 };
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
            sink.send(WorkerEvent::FileTooLarge { id, path: path.to_string(), size: total });
        } else {
            sink.send(WorkerEvent::FileLoadFailed { id, message: too_large() });
        }
        return;
    }
    // 分块读入内存（与 download_small 一致：128KB 一块），每累计 ~256KB 上报一次进度
    let res: anyhow::Result<Vec<u8>> = async {
        let mut rf = sftp.open(path).await?;
        let mut data: Vec<u8> = Vec::with_capacity((total as usize).min(limit).min(16 * 1024 * 1024));
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
                sink.send(WorkerEvent::FileLoadProgress { id, done: data.len() as u64, total: total.max(data.len() as u64) });
            }
        }
        Ok(data)
    }
    .await;
    match res {
        Ok(data) => {
            if !force && data.iter().take(8000).any(|b| *b == 0) {
                sink.send(WorkerEvent::FileLoadFailed { id, message: crate::i18n::tr("非文本文件，无法以文本方式打开", "Not a text file").into() });
                return;
            }
            // 探测编码并解码（UTF-8 优先，非 UTF-8 用 chardetng 猜 GBK/GB18030 等），再把行尾统一成 LF
            let (decoded, encoding) = decode_text(&data);
            let (content, eol) = if decoded.contains("\r\n") {
                (decoded.replace("\r\n", "\n"), crate::proto::Eol::Crlf)
            } else {
                (decoded, crate::proto::Eol::Lf)
            };
            sink.send(WorkerEvent::FileOpened { id, path: path.to_string(), content, encoding, eol, mtime: file_mtime });
        }
        Err(e) => {
            let msg = if e.to_string().contains("__TOO_LARGE__") {
                too_large()
            } else {
                match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开失败：{e}"), crate::i18n::Lang::En => format!("Open failed: {e}") }
            };
            sink.send(WorkerEvent::FileLoadFailed { id, message: msg });
        }
    }
}

/// 读取图片文件原始字节（带大小上限，避免误开超大文件拖慢界面）。
async fn read_image_file(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> anyhow::Result<Vec<u8>> {
    let limit = 32 * 1024 * 1024;
    // 先按元数据判大小再读，避免远端超大文件在限制检查前被整体读入内存（OOM/DoS）。
    if let Some(sz) = sftp.metadata(path).await.ok().and_then(|m| m.size) {
        if sz > limit as u64 {
            anyhow::bail!("{}", match crate::i18n::current() { crate::i18n::Lang::Zh => format!("图片过大（>{}MB）", limit / 1024 / 1024), crate::i18n::Lang::En => format!("Image too large (>{}MB)", limit / 1024 / 1024) });
        }
    }
    let data = sftp.read(path).await?;
    // 兜底：元数据不可用时，读后再判一次
    if data.len() > limit {
        anyhow::bail!("{}", match crate::i18n::current() { crate::i18n::Lang::Zh => format!("图片过大（>{}MB）", limit / 1024 / 1024), crate::i18n::Lang::En => format!("Image too large (>{}MB)", limit / 1024 / 1024) });
    }
    Ok(data)
}

/// 执行一次 SFTP 写类操作，结果以 [`WorkerEvent::OpDone`]/`Error` 上报。
/// 完整覆盖写一个远端文件：`CREATE | WRITE | TRUNCATE` 打开 → write_all → flush → **shutdown**。
///
/// 不用 russh-sftp 的便捷 `sftp.write()`——它只用 `OpenFlags::WRITE`：既不 `TRUNCATE`（内容变短时
/// 残留旧文件尾部）、也不关闭句柄（部分 SFTP 服务端要 CLOSE 才落盘 → 出现「保存了却没变化」）。
/// 与上传路径（`upload`）用的收尾方式一致。
async fn sftp_overwrite(sftp: &russh_sftp::client::SftpSession, path: &str, data: &[u8]) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::AsyncWriteExt;
    let mut f = sftp
        .open_with_flags(path, OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE)
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
async fn sftp_verify_size(sftp: &russh_sftp::client::SftpSession, path: &str, expect: usize) -> bool {
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
async fn sftp_write_atomic(sftp: &russh_sftp::client::SftpSession, path: &str, data: &[u8], sink: &UiSink) -> anyhow::Result<()> {
    // 符号链接 → 解析到真实目标，替换发生在目标上（链接不变）；损坏链接则就地走事务写。
    let is_symlink = sftp.symlink_metadata(path).await.map(|m| m.is_symlink()).unwrap_or(false);
    let target = if is_symlink {
        sftp.canonicalize(path).await.unwrap_or_else(|_| path.to_string())
    } else {
        path.to_string()
    };

    // 仅用于「保存前继承原文件权限」；取不到（新建 / 断链）则不设权限，不影响保存成败。
    let orig_perm = sftp.metadata(&target).await.ok().and_then(|m| m.permissions);

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
            .set_metadata(&tmp, russh_sftp::protocol::FileAttributes { permissions: Some(mode), ..Default::default() })
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
    // rename(target->bak) 成功 = 原文件存在且已妥善备份；失败 = 原文件不存在(新建)或无法备份。
    // （避免「stat 瞬时失败→误判为新建→OpenSSH 拒绝覆盖已存在目标→报错却谎称已还原」。）
    let bak = format!("{target}.ishell-bak-{}", rand_hex(6));
    let backed_up = sftp.rename(&target, &bak).await.is_ok();

    // 换入：tmp → target。失败则从 bak 还原原文件（若有备份）。
    if let Err(e) = sftp.rename(&tmp, &target).await {
        let restored = backed_up && sftp.rename(&bak, &target).await.is_ok();
        let _ = sftp.remove_file(&tmp).await;
        return Err(anyhow::anyhow!(match (crate::i18n::current(), backed_up, restored) {
            (crate::i18n::Lang::Zh, false, _) => format!("保存失败（原文件未改动，新内容在 {tmp}）：{e}"),
            (crate::i18n::Lang::Zh, true, true) => format!("替换失败，已还原原文件：{e}"),
            (crate::i18n::Lang::Zh, true, false) => format!("替换失败且未能还原，原文件在 {bak}：{e}"),
            (crate::i18n::Lang::En, false, _) => format!("save failed (original unchanged, new content at {tmp}): {e}"),
            (crate::i18n::Lang::En, true, true) => format!("replace failed, original restored: {e}"),
            (crate::i18n::Lang::En, true, false) => format!("replace failed and not restored; original is at {bak}: {e}"),
        }));
    }

    // 【闸二】换入后校验最终目标字节数——根治「保存却清空」：只有目标确认写对了才提交。
    if !sftp_verify_size(sftp, &target, data.len()).await {
        if backed_up {
            // 有备份：移除写坏的目标、从 .bak 还原原文件。
            let _ = sftp.remove_file(&target).await;
            let restored = sftp.rename(&bak, &target).await.is_ok();
            return Err(anyhow::anyhow!(match (crate::i18n::current(), restored) {
                (crate::i18n::Lang::Zh, true) => format!("保存后校验失败：目标字节数不符（应为 {}），已还原原文件", data.len()),
                (crate::i18n::Lang::Zh, false) => format!("保存后校验失败且未能还原，原文件在 {bak}"),
                (crate::i18n::Lang::En, true) => format!("post-save verify failed: wrong byte count (expected {}); original restored", data.len()),
                (crate::i18n::Lang::En, false) => format!("post-save verify failed and not restored; original is at {bak}"),
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
async fn sftp_overwrite_progress_to(sftp: &russh_sftp::client::SftpSession, write_path: &str, report_path: &str, data: &[u8], sink: &UiSink) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::AsyncWriteExt;
    const CHUNK: usize = 256 * 1024;
    let total = data.len() as u64;
    sink.send(WorkerEvent::FileSaveProgress { path: report_path.to_string(), done: 0, total });
    let mut f = sftp.open_with_flags(write_path, OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE).await?;
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + CHUNK).min(data.len());
        f.write_all(&data[off..end]).await?;
        off = end;
        sink.send(WorkerEvent::FileSaveProgress { path: report_path.to_string(), done: off as u64, total });
    }
    f.flush().await?;
    f.shutdown().await?;
    sink.send(WorkerEvent::FileSaveProgress { path: report_path.to_string(), done: total, total });
    Ok(())
}

async fn handle_fs_op(sftp: &russh_sftp::client::SftpSession, cmd: UiCommand, sink: &UiSink) {
    let result: anyhow::Result<(String, Option<String>)> = match cmd {
        UiCommand::Mkdir(path) => {
            let parent = remote_parent(&path);
            sftp.create_dir(&path)
                .await
                .map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已创建目录：{path}"), crate::i18n::Lang::En => format!("Created dir: {path}") }, Some(parent)))
                .map_err(Into::into)
        }
        UiCommand::CreateFile(path) => {
            // 服务端原子独占创建（O_CREAT|O_EXCL）：同名文件由服务器直接拒绝，
            // 杜绝「先 try_exists 再 TRUNCATE 创建」的检查—执行竞态（期间被别的进程建文件、
            // 或 try_exists 因网络/权限失败误判不存在，都会清空已有文件）。
            use russh_sftp::protocol::OpenFlags;
            let parent = remote_parent(&path);
            match sftp.open_with_flags(&path, OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE).await {
                Ok(mut f) => {
                    use tokio::io::AsyncWriteExt;
                    let _ = f.shutdown().await;
                    Ok((match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已创建文件：{path}"), crate::i18n::Lang::En => format!("Created file: {path}") }, Some(parent)))
                }
                Err(_) => Err(anyhow::anyhow!(crate::i18n::tr("同名文件已存在或无法创建", "File exists or cannot be created"))),
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
                .map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已修改权限：{:o}", mode & 0o777), crate::i18n::Lang::En => format!("Chmod: {:o}", mode & 0o777) }, Some(parent)))
                .map_err(Into::into)
        }
        UiCommand::Rename { from, to } => {
            let parent = remote_parent(&to);
            sftp.rename(&from, &to)
                .await
                .map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已重命名为：{to}"), crate::i18n::Lang::En => format!("Renamed to: {to}") }, Some(parent)))
                .map_err(Into::into)
        }
        UiCommand::WriteFile { path, content, encoding, eol, expect_mtime, force } => {
            // 外部改动检测：非 force 时若远端当前 mtime 与打开时不一致，拒绝写入、回报冲突。
            let conflict = if !force && expect_mtime != 0 {
                let cur = sftp.metadata(&path).await.ok().and_then(|m| m.mtime).unwrap_or(0);
                cur != 0 && cur != expect_mtime
            } else {
                false
            };
            if conflict {
                sink.send(WorkerEvent::FileSaveConflict { path });
                Ok((String::new(), None))
            } else {
                // 内部统一 LF → 按原文件行尾还原；再按原编码编码后写回，避免破坏非 UTF-8 文件 / 改动行尾。
                let text = match eol {
                    crate::proto::Eol::Crlf => content.replace('\n', "\r\n"),
                    crate::proto::Eol::Lf => content,
                };
                let enc = encoding_rs::Encoding::for_label(encoding.as_bytes()).unwrap_or(encoding_rs::UTF_8);
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
                        let nm = sftp.metadata(&path).await.ok().and_then(|m| m.mtime).unwrap_or(0);
                        sink.send(WorkerEvent::FileSaved { path: path.clone(), mtime: nm });
                        Ok((match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已保存：{path}"), crate::i18n::Lang::En => format!("Saved: {path}") }, None))
                    }
                    Err(e) => {
                        // 专用失败事件（带路径）：UI 据此复位 saving、保留 dirty，不再只有匿名 Error
                        sink.send(WorkerEvent::FileSaveFailed { path: path.clone(), message: e.to_string() });
                        Ok((String::new(), None))
                    }
                }
            }
        }
        _ => Ok(("".into(), None)),
    };

    match result {
        Ok((message, refresh_dir)) => sink.send(WorkerEvent::OpDone { message, refresh_dir }),
        Err(e) => sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("操作失败：{e}"), crate::i18n::Lang::En => format!("Operation failed: {e}") })),
    }
}

fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::tar_entry_path_safe;
    use std::path::Path;

    #[test]
    fn tar_paths_reject_traversal() {
        assert!(tar_entry_path_safe(Path::new("ok/file.txt")));
        assert!(tar_entry_path_safe(Path::new("./nested/a")));
        assert!(!tar_entry_path_safe(Path::new("../escape")));
        assert!(!tar_entry_path_safe(Path::new("a/../../b")));
        assert!(!tar_entry_path_safe(Path::new("/abs/path")));
        assert!(!tar_entry_path_safe(Path::new("")));
    }
}
