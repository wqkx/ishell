//! SSH 认证与通道建立：从 ssh God Object 拆出，行为不变。

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle, Handler};
use russh::keys::ssh_key;
use russh::{Channel, ChannelMsg};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::proto::{AuthMethod, ConnectConfig, UiCommand, WorkerEvent};

use super::UiSink;

/// russh-sftp 每请求超时（秒）。默认 10s 对弱网大目录略紧，放宽到 20s；通道真死时会以
/// 「sender dropped / session closed」快速报错，不会被此超时拖满。
const SFTP_REQUEST_TIMEOUT_SECS: u64 = 20;

/// russh 客户端回调处理器：校验主机密钥（known_hosts + 首次信任 TOFU）。
/// UI 对主机密钥确认的回复通道；跳板机与目标主机共享（顺序询问，不并发）。
type HostKeyDecision = Arc<tokio::sync::Mutex<UnboundedReceiver<bool>>>;

pub(crate) struct ClientHandler {
    host: String,
    port: u16,
    sink: UiSink,
    /// UI 对"是否信任新主机/接受变更密钥"的回复
    decision_rx: HostKeyDecision,
    /// 是否转发本机 ssh-agent：为真时桥接远端回连的 auth-agent 通道到本地 agent
    agent_forward: bool,
    /// 是否把本机 AI/MCP 控制 socket 反向转发到这台远端主机
    /// （远端能连到转发出来的 socket，等于能控制本机 iShell）
    mcp_forward: bool,
}

/// 用户主目录下的 known_hosts 路径（与 russh 内部一致）。
fn known_hosts_file() -> anyhow::Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                crate::i18n::tr("找不到用户主目录", "Home directory not found")
            )
        })?;
    Ok(std::path::PathBuf::from(home)
        .join(".ssh")
        .join("known_hosts"))
}

/// 主机密钥变更后用户确认接受：删除 known_hosts 中该主机的旧行，再写入新键。
fn replace_known_host(host: &str, port: u16, new_key: &ssh_key::PublicKey) -> anyhow::Result<()> {
    // 收集匹配该主机的行号（russh 的匹配能处理哈希主机名）
    let remove: std::collections::HashSet<usize> =
        russh::keys::known_hosts::known_host_keys(host, port)
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
                self.sink
                    .send(WorkerEvent::Status(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => "主机密钥确认超时，已拒绝连接".into(),
                        crate::i18n::Lang::En => {
                            "Host key confirmation timed out; connection rejected".into()
                        }
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

    // 远端连接我们此前用 streamlocal_forward 反向登记的 socket 路径时，服务器经此回调开一个
    // 通道；桥接到本机 AI/MCP 控制 socket，即实现「远端能连到转发出来的 socket 就能控制本机」。
    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        channel: Channel<client::Msg>,
        _socket_path: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if self.mcp_forward {
            tokio::spawn(async move {
                if let Err(e) = bridge_local_mcp(channel).await {
                    log::debug!("AI/MCP 反向转发桥接结束：{e}");
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
                    let _ = russh::keys::known_hosts::learn_known_hosts(
                        &self.host,
                        self.port,
                        server_public_key,
                    );
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            // 已记录但密钥不一致 -> 可能中间人攻击；在 UI 内确认是否接受新键并替换旧行
            Err(_) => {
                if self.ask_trust(fp, true).await {
                    if let Err(e) = replace_known_host(&self.host, self.port, server_public_key) {
                        self.sink
                            .send(WorkerEvent::Error(match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("更新 known_hosts 失败：{e}"),
                                crate::i18n::Lang::En => {
                                    format!("Failed to update known_hosts: {e}")
                                }
                            }));
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
pub(super) struct JumpHandler {
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
        let fp = server_public_key
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string();
        match russh::keys::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true),
            // 跳板机首次连接也需 TOFU 用户确认（不再自动信任，防中间人冒充堡垒机）
            Ok(false) => {
                if self.ask_trust(fp, false).await {
                    let _ = russh::keys::known_hosts::learn_known_hosts(
                        &self.host,
                        self.port,
                        server_public_key,
                    );
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(_) => {
                if self.ask_trust(fp, true).await {
                    if let Err(e) = replace_known_host(&self.host, self.port, server_public_key) {
                        self.sink
                            .send(WorkerEvent::Error(match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("更新 known_hosts 失败：{e}"),
                                crate::i18n::Lang::En => {
                                    format!("Failed to update known_hosts: {e}")
                                }
                            }));
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
                    russh::keys::PrivateKeyWithHashAlg::new(
                        Arc::new(key),
                        Some(russh::keys::HashAlg::Sha512),
                    ),
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
        anyhow::bail!(
            "{}",
            crate::i18n::tr(
                "ssh-agent 中没有可用私钥（先 ssh-add）",
                "No keys in ssh-agent (run ssh-add)"
            )
        );
    }
    for id in ids {
        let AgentIdentity::PublicKey { key, .. } = id else {
            continue;
        };
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
        let sock = std::env::var("SSH_AUTH_SOCK").map_err(|_| {
            anyhow::anyhow!(
                "{}",
                crate::i18n::tr("SSH_AUTH_SOCK 未设置", "SSH_AUTH_SOCK not set")
            )
        })?;
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

/// 把一条反向转发来的通道桥接到本机 AI/MCP 控制 socket（`~/.config/ishell/mcp.sock`）。
async fn bridge_local_mcp(channel: Channel<client::Msg>) -> anyhow::Result<()> {
    let sock = crate::store::mcp_socket_path()
        .ok_or_else(|| anyhow::anyhow!("mcp socket 路径不可用（无法确定用户目录）"))?;
    let mut remote = channel.into_stream();
    let mut local = tokio::net::UnixStream::connect(&sock).await?;
    tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
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
            Resp::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                // 空提示组（部分服务器仅发指示信息）：直接回空响应推进
                if prompts.is_empty() {
                    resp = handle
                        .authenticate_keyboard_interactive_respond(Vec::new())
                        .await?;
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
                resp = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
}

/// 建立 TCP + SSH 握手并完成认证。可选经跳板机（ProxyJump）连接。
/// 返回目标主机句柄，以及需保持存活的跳板机句柄（None 表示直连）。
pub(super) async fn connect(
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
        mcp_forward: crate::store::load_mcp_consent(),
    };

    let (mut handle, jump_keep) = if let Some(jump) = &cfg.jump {
        // 1) 先连跳板机并认证
        sink.send(WorkerEvent::Status(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("正在连接跳板机 {}:{} …", jump.host, jump.port),
            crate::i18n::Lang::En => format!("Connecting jump {}:{} …", jump.host, jump.port),
        }));
        let jhandler = JumpHandler {
            host: jump.host.clone(),
            port: jump.port,
            sink: sink.clone(),
            decision_rx: decision_rx.clone(),
        };
        let mut jhandle =
            client::connect(config.clone(), (jump.host.as_str(), jump.port), jhandler).await?;
        if !authenticate(&mut jhandle, &jump.username, &jump.auth, sink, cmd_rx).await? {
            anyhow::bail!(
                "{}",
                crate::i18n::tr("跳板机认证被拒绝", "Jump host auth rejected")
            );
        }
        // 2) 经跳板机打开到目标主机的 direct-tcpip 通道，并在该流上完成目标 SSH 握手
        sink.send(WorkerEvent::Status(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("经跳板机连接 {}:{} …", cfg.host, cfg.port),
            crate::i18n::Lang::En => format!("Via jump to {}:{} …", cfg.host, cfg.port),
        }));
        let ch = jhandle
            .channel_open_direct_tcpip(cfg.host.clone(), cfg.port as u32, "127.0.0.1", 0)
            .await?;
        let handle = client::connect_stream(config, ch.into_stream(), target_handler).await?;
        (handle, Some(jhandle))
    } else {
        let handle = client::connect(config, (cfg.host.as_str(), cfg.port), target_handler).await?;
        (handle, None)
    };

    sink.send(WorkerEvent::Status(
        crate::i18n::tr("正在认证 …", "Authenticating …").into(),
    ));
    if !authenticate(&mut handle, &cfg.username, &cfg.auth, sink, cmd_rx).await? {
        anyhow::bail!(
            "{}",
            crate::i18n::tr(
                "认证被拒绝（用户名/密码或密钥错误）",
                "Authentication rejected (bad credentials)"
            )
        );
    }
    Ok((handle, jump_keep))
}

/// 打开带 PTY 的交互式 shell 通道。`forward_agent` 为真时请求 agent 转发。
pub(super) async fn open_shell(
    handle: &Handle<ClientHandler>,
    forward_agent: bool,
) -> anyhow::Result<russh::Channel<client::Msg>> {
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
    // 显式声明 TERM / 真彩色：许多工具（ls、bat、rg、nvim 等）靠 COLORTERM=truecolor
    // 才输出 SGR 38;2 / 48;2；仅 PTY 类型 xterm-256color 不足以开启 24 位色。
    let _ = channel.set_env(false, "TERM", "xterm-256color").await;
    let _ = channel.set_env(false, "COLORTERM", "truecolor").await;
    channel.request_shell(false).await?;
    Ok(channel)
}

/// 在独立通道上打开 SFTP 子系统。
pub(super) async fn open_sftp(
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
pub(super) async fn exec_capture(
    handle: &Handle<ClientHandler>,
    cmd: &str,
) -> anyhow::Result<String> {
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
pub(super) async fn exec_capture_bytes(
    handle: &Handle<ClientHandler>,
    cmd: &str,
) -> anyhow::Result<(i32, Vec<u8>, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut code = -1i32;
    // 读到通道关闭为止（ExitStatus 可能在 Eof 前后到达，不能提前 break）
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, ext: 1 } => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, out, String::from_utf8_lossy(&err).into_owned()))
}

/// 执行命令，返回 (退出码, stderr)。
pub(super) async fn exec_status(
    handle: &Handle<ClientHandler>,
    cmd: &str,
) -> anyhow::Result<(i32, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut code = -1i32;
    let mut err = Vec::new();
    // 注意：ExitStatus 通常在 Eof 之前到达，但不能在 Eof 处提前 break，
    // 否则可能漏掉退出码；这里一直读到通道关闭（wait 返回 None）。
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::ExtendedData { data, ext: 1 } => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, String::from_utf8_lossy(&err).into_owned()))
}
