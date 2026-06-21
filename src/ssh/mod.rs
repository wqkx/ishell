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
use russh::ChannelMsg;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::proto::{AuthMethod, ConnectConfig, FileEntry, UiCommand, WorkerEvent};
use sysinfo::{SysSampler, PROBE_CMD};

/// 同一会话同时进行的最大传输数（不同会话各自独立）。
const MAX_CONCURRENT_XFER: usize = 6;

/// 待执行/进行中的传输任务描述。
enum PendingXfer {
    Download { id: u64, remote: String, local: String },
    Upload { id: u64, local: String, remote_dir: String },
}

impl PendingXfer {
    fn id(&self) -> u64 {
        match self {
            PendingXfer::Download { id, .. } | PendingXfer::Upload { id, .. } => *id,
        }
    }
}

/// 启动一个传输任务：登记取消标志，spawn 后台任务，完成时通过 `done_tx` 通知主循环。
fn start_xfer(
    handle: &Arc<Handle<ClientHandler>>,
    sink: &UiSink,
    done_tx: &UnboundedSender<u64>,
    cancels: &mut HashMap<u64, Arc<AtomicBool>>,
    p: PendingXfer,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let id = p.id();
    cancels.insert(id, cancel.clone());
    let h = handle.clone();
    let s = sink.clone();
    let dtx = done_tx.clone();
    tokio::spawn(async move {
        match open_sftp(&h).await {
            Ok(sftp) => {
                let sftp = Arc::new(sftp);
                match p {
                    PendingXfer::Download { id, remote, local } => download(sftp, id, remote, local, &s, cancel).await,
                    PendingXfer::Upload { id, local, remote_dir } => upload(sftp.as_ref(), id, local, remote_dir, &s, cancel).await,
                }
            }
            Err(e) => s.send(WorkerEvent::TransferDone {
                id, ok: false, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"), crate::i18n::Lang::En => format!("SFTP unavailable: {e}") }, refresh_dir: None,
            }),
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
struct ClientHandler {
    host: String,
    port: u16,
    sink: UiSink,
    /// UI 对"是否信任新主机"的回复
    decision_rx: UnboundedReceiver<bool>,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

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
                self.sink.send(WorkerEvent::HostKeyPrompt {
                    host: format!("{}:{}", self.host, self.port),
                    fingerprint: fp,
                });
                match self.decision_rx.recv().await {
                    Some(true) => {
                        let _ = russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, server_public_key);
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            // 已记录但密钥不一致 -> 可能中间人攻击，拒绝并提示手动处理
            Err(_) => {
                self.sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("主机密钥已改变（{}），可能存在中间人攻击！请手动编辑 ~/.ssh/known_hosts 删除旧行后重试。", fp), crate::i18n::Lang::En => format!("Host key changed ({})! Possible MITM. Remove the old line in ~/.ssh/known_hosts and retry.", fp) }));
                Ok(false)
            }
        }
    }
}

/// 跳板机回调处理器：校验其 known_hosts，未知则自动信任并记录（首次连接）。
struct JumpHandler {
    host: String,
    port: u16,
    sink: UiSink,
}

impl Handler for JumpHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                let _ = russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, server_public_key);
                self.sink.send(WorkerEvent::Status(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已信任跳板机 {} 的主机指纹", self.host), crate::i18n::Lang::En => format!("Trusted jump host key: {}", self.host) }));
                Ok(true)
            }
            Err(_) => {
                self.sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("跳板机 {} 主机密钥已改变，可能存在中间人攻击，已拒绝。", self.host), crate::i18n::Lang::En => format!("Jump host {} key changed; possible MITM. Rejected.", self.host) }));
                Ok(false)
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
    let (handle, _jump_handle) = match connect(&cfg, &sink, hostkey_rx).await {
        Ok(h) => h,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("连接失败：{e}"), crate::i18n::Lang::En => format!("Connect failed: {e}") }));
            return;
        }
    };
    let handle = Arc::new(handle);

    // 1) 交互式 shell 通道
    let mut shell = match open_shell(&handle).await {
        Ok(c) => c,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开 shell 失败：{e}"), crate::i18n::Lang::En => format!("Open shell failed: {e}") }));
            return;
        }
    };

    // 2) SFTP 通道（Arc 共享，供并发任务使用，避免阻塞主循环）
    let sftp = match open_sftp(&handle).await {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            sink.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"), crate::i18n::Lang::En => format!("SFTP unavailable: {e}") }));
            None
        }
    };

    // 3) 系统信息采集任务（独立 handle 克隆，互不阻塞）
    let probe_handle = handle.clone();
    let probe_sink = sink.clone();
    let probe_task = tokio::spawn(async move {
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
    let mut xfer_cancels: HashMap<u64, Arc<AtomicBool>> = HashMap::new();
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
                            tokio::spawn(async move {
                                match list_dir(&sftp, &path).await {
                                    Ok((canon, entries)) => {
                                        s.send(WorkerEvent::DirListing { path: canon, entries });
                                    }
                                    Err(e) => {
                                        s.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("读取目录失败：{e}"), crate::i18n::Lang::En => format!("List dir failed: {e}") }));
                                        // 回送空列表以清除该目录的 loading 状态，避免卡在“加载中”
                                        s.send(WorkerEvent::DirListing { path, entries: Vec::new() });
                                    }
                                }
                            });
                        }
                    }
                    // 传输：独立任务（独立 SFTP 通道），不阻塞交互 shell
                    Some(UiCommand::Download { id, remote, local }) => {
                        let p = PendingXfer::Download { id, remote, local };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::Upload { id, local, remote_dir }) => {
                        let p = PendingXfer::Upload { id, local, remote_dir };
                        if active_xfer < MAX_CONCURRENT_XFER {
                            start_xfer(&handle, &sink, &xfer_done_tx, &mut xfer_cancels, p);
                            active_xfer += 1;
                        } else {
                            pending_xfer.push_back(p);
                        }
                    }
                    Some(UiCommand::CancelTransfer(id)) => {
                        if let Some(c) = xfer_cancels.get(&id) {
                            // 进行中：置标志，任务会尽快中止并上报「已取消」
                            c.store(true, Ordering::Relaxed);
                        } else if let Some(pos) = pending_xfer.iter().position(|p| p.id() == id) {
                            // 仍在排队：直接移除并上报已取消
                            pending_xfer.remove(pos);
                            sink.send(WorkerEvent::TransferDone {
                                id, ok: false, message: crate::i18n::tr("已取消", "Canceled").into(), refresh_dir: None,
                            });
                        }
                    }
                    Some(UiCommand::ReadFile { path, force }) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                match read_text_file(&sftp, &path, force).await {
                                    Ok(content) => s.send(WorkerEvent::FileOpened { path, content }),
                                    Err(e) => s.send(WorkerEvent::Error(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开失败：{e}"), crate::i18n::Lang::En => format!("Open failed: {e}") })),
                                }
                            });
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
                    Some(cmd @ (UiCommand::Mkdir(_)
                        | UiCommand::CreateFile(_)
                        | UiCommand::Chmod { .. }
                        | UiCommand::Delete { .. }
                        | UiCommand::Rename { .. }
                        | UiCommand::WriteFile { .. })) => {
                        if let Some(sftp) = &sftp {
                            let sftp = sftp.clone();
                            let s = sink.clone();
                            tokio::spawn(async move {
                                handle_fs_op(&sftp, cmd, &s).await;
                            });
                        } else {
                            sink.send(WorkerEvent::Error(crate::i18n::tr("SFTP 不可用", "SFTP unavailable").into()));
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
                            let _ = exec_capture(&h, &format!("kill -9 {pid}")).await;
                            s.send(WorkerEvent::Status(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已发送 kill -9 {pid}"), crate::i18n::Lang::En => format!("Sent kill -9 {pid}") }));
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
) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    let ok = match auth {
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

/// 建立 TCP + SSH 握手并完成认证。可选经跳板机（ProxyJump）连接。
/// 返回目标主机句柄，以及需保持存活的跳板机句柄（None 表示直连）。
async fn connect(
    cfg: &ConnectConfig,
    sink: &UiSink,
    hostkey_rx: UnboundedReceiver<bool>,
) -> anyhow::Result<(Handle<ClientHandler>, Option<Handle<JumpHandler>>)> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let target_handler = ClientHandler {
        host: cfg.host.clone(),
        port: cfg.port,
        sink: sink.clone(),
        decision_rx: hostkey_rx,
    };

    let (mut handle, jump_keep) = if let Some(jump) = &cfg.jump {
        // 1) 先连跳板机并认证
        sink.send(WorkerEvent::Status(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("正在连接跳板机 {}:{} …", jump.host, jump.port), crate::i18n::Lang::En => format!("Connecting jump {}:{} …", jump.host, jump.port) }));
        let jhandler = JumpHandler { host: jump.host.clone(), port: jump.port, sink: sink.clone() };
        let mut jhandle = client::connect(config.clone(), (jump.host.as_str(), jump.port), jhandler).await?;
        if !authenticate(&mut jhandle, &jump.username, &jump.auth).await? {
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
    if !authenticate(&mut handle, &cfg.username, &cfg.auth).await? {
        anyhow::bail!("{}", crate::i18n::tr("认证被拒绝（用户名/密码或密钥错误）", "Authentication rejected (bad credentials)"));
    }
    Ok((handle, jump_keep))
}

/// 打开带 PTY 的交互式 shell 通道。
async fn open_shell(handle: &Handle<ClientHandler>) -> anyhow::Result<russh::Channel<client::Msg>> {
    // request_pty/request_shell 均为 &self，channel 之后按值返回，无需 mut
    let channel = handle.channel_open_session().await?;
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

/// 读取远程目录，返回（规范化后的绝对路径, 条目列表）。目录在前、按名排序。
async fn list_dir(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> anyhow::Result<(String, Vec<FileEntry>)> {
    let canon = sftp.canonicalize(path).await.unwrap_or_else(|_| path.to_string());
    let dir = sftp.read_dir(&canon).await?;
    let mut entries = Vec::new();
    for item in dir {
        let name = item.file_name();
        if name == "." || name == ".." {
            continue;
        }
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
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok((canon, entries))
}

/// 下载（单文件或整个目录）并上报进度。大文件用多个并发分段读取流水线化，
/// 抵消 SFTP「单请求等一个往返」的吞吐瓶颈（高延迟链路上提速明显）。
async fn download(
    sftp: Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote: String,
    local: String,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = basename(&remote);
    let is_dir = sftp.metadata(&remote).await.map(|m| m.is_dir()).unwrap_or(false);

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
            id, name: name.clone(), total, dir: crate::proto::TransferDir::Download,
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
                download_file(&sftp, &rpath, &lpath, size, &cancel, &done).await?;
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

/// 下载单个文件：大文件按偏移并发分段读取，定位写入本地，显著提升高延迟链路吞吐。
async fn download_file(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    rpath: &str,
    lpath: &std::path::Path,
    size: u64,
    cancel: &Arc<AtomicBool>,
    done: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    if let Some(parent) = lpath.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // 小文件（或大小未知）：单流顺序读取；瞬时失败整体重试（重新建文件）。
    if size <= DL_CHUNK {
        let mut attempt = 0u32;
        loop {
            match download_small(sftp, rpath, lpath, cancel, done).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if cancel.load(Ordering::Relaxed) || attempt >= XFER_RETRIES {
                        return Err(e);
                    }
                    attempt += 1;
                    tokio::time::sleep(xfer_backoff(attempt)).await;
                }
            }
        }
    }

    // 大文件：预分配，按偏移并发分段；用「分段完成位图」实现断点续传——
    // 重试时只补未完成的分段，已完成分段既不重下也不重复计入进度。
    let n_chunks = size.div_ceil(DL_CHUNK);
    let out = Arc::new(std::fs::File::create(lpath)?);
    out.set_len(size)?;
    let chunk_done: Arc<Vec<AtomicBool>> = Arc::new((0..n_chunks).map(|_| AtomicBool::new(false)).collect());

    let mut attempt = 0u32;
    loop {
        let cursor = Arc::new(AtomicU64::new(0)); // 本轮分段游标
        let workers = DL_PARALLEL.min(n_chunks.max(1));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..workers {
            let (sftp, out, cursor, done, cancel, chunk_done) =
                (sftp.clone(), out.clone(), cursor.clone(), done.clone(), cancel.clone(), chunk_done.clone());
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
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("canceled");
        }
        if attempt >= XFER_RETRIES {
            return Err(first_err.unwrap_or_else(|| anyhow::anyhow!("incomplete transfer")));
        }
        attempt += 1;
        tokio::time::sleep(xfer_backoff(attempt)).await;
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
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = basename(&local);
    let is_dir = tokio::fs::metadata(&local).await.map(|m| m.is_dir()).unwrap_or(false);

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
            id, name: name.clone(), total, dir: crate::proto::TransferDir::Upload,
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
                match upload_file_once(sftp, &lpath, &rpath, &cancel, done_base, id, sink, &last).await {
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
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    let local_size = tokio::fs::metadata(lpath).await.map(|m| m.len()).unwrap_or(0);
    // 远端已存在的字节数作为续传起点（仅当不超过本地大小才续传，否则从头覆盖）
    let remote_size = sftp.metadata(rpath).await.ok().and_then(|m| m.size).unwrap_or(0);
    let start = if remote_size > 0 && remote_size <= local_size { remote_size } else { 0 };

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

/// 读取远程文本文件（拒绝含 NUL 的二进制文件；非 force 时限制 4MB）。
async fn read_text_file(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    force: bool,
) -> anyhow::Result<String> {
    let data = sftp.read(path).await?;
    let limit = if force { 16 * 1024 * 1024 } else { 4 * 1024 * 1024 };
    if data.len() > limit {
        anyhow::bail!("{}", match crate::i18n::current() { crate::i18n::Lang::Zh => format!("文件过大（>{}MB）", limit / 1024 / 1024), crate::i18n::Lang::En => format!("File too large (>{}MB)", limit / 1024 / 1024) });
    }
    if data.iter().take(8000).any(|b| *b == 0) {
        anyhow::bail!("{}", crate::i18n::tr("非文本文件，无法以文本方式打开", "Not a text file"));
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// 读取图片文件原始字节（带大小上限，避免误开超大文件拖慢界面）。
async fn read_image_file(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> anyhow::Result<Vec<u8>> {
    let data = sftp.read(path).await?;
    let limit = 32 * 1024 * 1024;
    if data.len() > limit {
        anyhow::bail!("{}", match crate::i18n::current() { crate::i18n::Lang::Zh => format!("图片过大（>{}MB）", limit / 1024 / 1024), crate::i18n::Lang::En => format!("Image too large (>{}MB)", limit / 1024 / 1024) });
    }
    Ok(data)
}

/// 执行一次 SFTP 写类操作，结果以 [`WorkerEvent::OpDone`]/`Error` 上报。
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
            let parent = remote_parent(&path);
            sftp.write(&path, b"")
                .await
                .map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已创建文件：{path}"), crate::i18n::Lang::En => format!("Created file: {path}") }, Some(parent)))
                .map_err(Into::into)
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
        UiCommand::Delete { path, is_dir } => {
            let parent = remote_parent(&path);
            let r = if is_dir {
                sftp.remove_dir(&path).await
            } else {
                sftp.remove_file(&path).await
            };
            r.map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已删除：{path}"), crate::i18n::Lang::En => format!("Deleted: {path}") }, Some(parent))).map_err(Into::into)
        }
        UiCommand::Rename { from, to } => {
            let parent = remote_parent(&to);
            sftp.rename(&from, &to)
                .await
                .map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已重命名为：{to}"), crate::i18n::Lang::En => format!("Renamed to: {to}") }, Some(parent)))
                .map_err(Into::into)
        }
        UiCommand::WriteFile { path, content } => {
            sftp.write(&path, content.as_bytes())
                .await
                .map(|_| (match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已保存：{path}"), crate::i18n::Lang::En => format!("Saved: {path}") }, None))
                .map_err(Into::into)
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
