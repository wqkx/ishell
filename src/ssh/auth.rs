use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
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
    /// X11 转发凭据。远端只拿到一次性假 cookie；回连时换成这里的真 cookie 再接本机 socket。
    x11: X11Slot,
    /// 是否把本机 AI/MCP 控制 socket 反向转发到这台远端主机
    /// （远端能连到转发出来的 socket，等于能控制本机 iShell）
    mcp_forward: bool,
    /// 用户级 `-R`：远端 (listen_addr, port) → 本机 (host, port)。
    /// 远端有人连上监听口时，经 `server_channel_open_forwarded_tcpip` 查表桥接。
    remote_fwds: RemoteFwdTable,
}

/// 用户 `-R` 转发路由表（与 `ClientHandler` / `run_forward` 共享）。
pub(super) type RemoteFwdTable = Arc<Mutex<HashMap<(String, u32), (String, u16)>>>;

/// 本机 X11 转发材料。`fake` 发给远端，`real` 只留在本机，回连握手时替换。
#[derive(Clone)]
pub(super) struct X11Auth {
    sock: std::path::PathBuf,
    real: [u8; 16],
    fake: [u8; 16],
}

pub(super) type X11Slot = Arc<Mutex<Option<X11Auth>>>;

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

/// 进程内锁：串行化对 known_hosts 的读-改-写，避免多个并发连接同时触发主机密钥变更确认时
/// 互相覆盖对方的改动（比如两条连接都在过滤旧行，后写的那个会带着自己那份「过滤前」的
/// 旧内容覆盖回去，把另一条连接刚学会的新键连带丢掉）。
static KNOWN_HOSTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 同目录临时文件 + rename 的原子写：避免半截内容被读到，或进程崩溃时整个 known_hosts
/// 变成空文件（`std::fs::write` 是"先截断再写"，中途失败/崩溃会丢光原有内容）。
///
/// 权限：`rename` **保留的是临时文件的 mode**，所以临时文件一律以 0600 创建（不留
/// 「先按 umask 建、再收紧」的暴露窗口），再把原文件的权限继承过来。少了这一步，
/// 用户特意收紧到 0600 的 `~/.ssh/known_hosts` 会在一次「接受变更的主机密钥」之后被
/// 静默放宽成 umask 默认（通常 0644）——文件本身不是密钥材料，但它记录了这台机器连过
/// 哪些主机。原文件不存在时（首次写入）保持 0600。
fn atomic_write_text(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("ishell-tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp); // 清理崩溃残留，确保 create_new 能成功
    {
        let mut f = create_private(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    inherit_perms(path, &tmp);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 新建一个仅属主可读写的文件（权限在 `open(2)` 时即生效，非事后 chmod）。
#[cfg(unix)]
fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
#[cfg(not(unix))]
fn create_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// 把 `orig` 的权限位复制到 `tmp`（`orig` 不存在则保持 `tmp` 现有的 0600）。尽力而为：
/// 设不上就按 0600 换入，那是更严而不是更松的一侧，不值得让整次写入失败。
#[cfg(unix)]
fn inherit_perms(orig: &std::path::Path, tmp: &std::path::Path) {
    if let Ok(meta) = std::fs::metadata(orig) {
        let _ = std::fs::set_permissions(tmp, meta.permissions());
    }
}
#[cfg(not(unix))]
fn inherit_perms(_orig: &std::path::Path, _tmp: &std::path::Path) {}

/// 首次信任新主机（TOFU）：直接追加，复用 russh 自带的 append 逻辑，但要跟
/// `replace_known_host` 共用同一把锁——否则一条连接正在读取整份文件准备重写（过滤旧行）
/// 时，另一条连接这边的追加可能夹在中间又被前者的重写覆盖掉，白白学会的新主机记录丢失。
fn learn_known_host_locked(host: &str, port: u16, key: &ssh_key::PublicKey) {
    let _guard = KNOWN_HOSTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = russh::keys::known_hosts::learn_known_hosts(host, port, key);
}

/// 主机密钥变更后用户确认接受：删除 known_hosts 中该主机的旧行，再写入新键。
/// 过滤旧行和写入新键合并成一次原子写（不再是"先覆盖删除旧行"+"再调用 russh 自己的追加
/// 逻辑"这两个独立步骤）：避免中间崩溃时该主机的记录暂时性缺失，也避免两次独立文件操作
/// 之间出现别的并发写入插进来。
fn replace_known_host(host: &str, port: u16, new_key: &ssh_key::PublicKey) -> anyhow::Result<()> {
    let _guard = KNOWN_HOSTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 收集匹配该主机的行号（russh 的匹配能处理哈希主机名）
    let remove: std::collections::HashSet<usize> =
        russh::keys::known_hosts::known_host_keys(host, port)
            .unwrap_or_default()
            .into_iter()
            .map(|(line, _)| line)
            .collect();
    let path = known_hosts_file()?;
    // known_host_keys 的行号从 1 计；过滤掉这些行，保留其余内容
    let mut kept: String = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .enumerate()
        .filter(|(i, _)| !remove.contains(&(i + 1)))
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    // 追加新键这一行，格式与 russh 的 learn_known_hosts_path 保持一致
    if port != 22 {
        kept.push_str(&format!("[{host}]:{port} "));
    } else {
        kept.push_str(&format!("{host} "));
    }
    kept.push_str(&new_key.to_openssh()?);
    kept.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_text(&path, &kept)?;
    Ok(())
}

/// 主机密钥确认等待上限：UI 异常关闭/卡住时避免 worker 永久挂起。
const HOSTKEY_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

/// TCP 连接 + SSH 版本协商/密钥交换的等待上限。这一步纯粹是网络层握手，不等待任何用户交互
/// （用户交互在 authenticate() 里，主机密钥确认另有 HOSTKEY_DECISION_TIMEOUT 兜底），弱网/
/// 目标不可达时 russh 的 `client::connect`/`connect_stream` 在此之前完全没有超时保护，会一直
/// 挂在 TCP 握手或 kex 上——远超用户等待意愿，且看起来和"卡死无响应"没有区别；每次重连都会
/// 再次卡住同样长时间，表现为"第一次连不上、后面怎么点重连都连不上"。加一层超时让它老老实实
/// 报错，交回既有的指数退避重连循环（session_events.rs）去重试。
const NET_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

async fn with_connect_timeout<T>(
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(NET_CONNECT_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!(
            "{}",
            crate::i18n::tr(
                "连接超时（网络较弱或目标不可达）",
                "Connect timed out (weak network or host unreachable)"
            )
        ),
    }
}

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

    // 远端 X11 客户端连上时，服务器经此打开 x11 通道；桥到本机 DISPLAY 的 unix socket。
    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<client::Msg>,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if let Some(auth) = self.x11.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            tokio::spawn(async move {
                if let Err(e) = bridge_local_x11(channel, auth).await {
                    log::debug!("X11 转发桥接结束：{e}");
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

    /// 远端 `-R` 监听口有新连接时，服务器经此开通道；查表连到本机目标。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let target = lookup_remote_fwd(&self.remote_fwds, connected_address, connected_port);
        let Some((host, port)) = target else {
            log::debug!("收到未登记的 forwarded-tcpip {connected_address}:{connected_port}，忽略");
            return Ok(());
        };
        tokio::spawn(async move {
            if let Err(e) = bridge_local_tcp(channel, &host, port).await {
                log::debug!("-R 桥接结束 {host}:{port}：{e}");
            }
        });
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
                    learn_known_host_locked(&self.host, self.port, server_public_key);
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
        // 与目标机同款 120s 超时：跳板确认若无限等，用户关弹窗后会话会永久挂死。
        let mut rx = self.decision_rx.lock().await;
        match tokio::time::timeout(HOSTKEY_DECISION_TIMEOUT, rx.recv()).await {
            Ok(Some(true)) => true,
            Ok(Some(false)) | Ok(None) => false,
            Err(_) => {
                self.sink
                    .send(WorkerEvent::Status(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => "跳板机主机密钥确认超时，已拒绝连接".into(),
                        crate::i18n::Lang::En => {
                            "Jump host key confirmation timed out; connection rejected".into()
                        }
                    }));
                false
            }
        }
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
                    learn_known_host_locked(&self.host, self.port, server_public_key);
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
    prelude: &mut VecDeque<UiCommand>,
) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    let ok = match auth {
        AuthMethod::Interactive => {
            authenticate_interactive(handle, username, sink, cmd_rx, prelude).await?
        }
        AuthMethod::Password(pw) => {
            authenticate_password_login(handle, username, pw, sink, cmd_rx, prelude).await?
        }
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

/// 落盘密文的前缀（与 `store::crypto::ENC_PREFIX` 相同）。解密失败时 `decrypt_secret`
/// 会把密文原串交回来，拿去当密码必然认证失败。
const UNDECRYPTED_SECRET_PREFIX: &str = "enc:v1:";

fn password_looks_undecrypted(pw: &str) -> bool {
    pw.starts_with(UNDECRYPTED_SECRET_PREFIX)
}

/// 「密码」认证：先走 SSH 协议里的 `password` 方法（0.23 及更早就是这样）。
///
/// kbd-int 只在 password **已经不在** remaining_methods 里、但还有
/// keyboard-interactive 时再试（`PasswordAuthentication no` 那一类）。
/// 两种方法都开着时，密码错了 remaining 里仍有 password——再打一遍 kbd-int
/// 会把同一密码送两次，MaxAuthTries / fail2ban / 域账号锁定都按两次失败计。
///
/// `partial_success` 表示这一步被接受、还要继续别的方法：可以接着做 kbd-int，
/// 但不要再自动填登录密码。
fn should_try_kbd_after_password(
    remaining_methods: &russh::MethodSet,
    partial_success: bool,
) -> bool {
    use russh::MethodKind;
    remaining_methods.contains(&MethodKind::KeyboardInteractive)
        && (!remaining_methods.contains(&MethodKind::Password) || partial_success)
}

fn should_autofill_kbd_password(already_filled: bool, echo: &[bool]) -> bool {
    !already_filled && echo.len() == 1 && !echo[0]
}

async fn authenticate_password_login<H>(
    handle: &mut Handle<H>,
    username: &str,
    pw: &str,
    sink: &UiSink,
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
    prelude: &mut VecDeque<UiCommand>,
) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    if password_looks_undecrypted(pw) {
        anyhow::bail!(
            "{}",
            crate::i18n::tr(
                "保存的密码无法解密：主密钥与密文不匹配。请重新编辑该连接并填写密码。",
                "Saved password could not be decrypted (master key mismatch). Re-edit the connection and enter the password again."
            )
        );
    }
    match handle.authenticate_password(username, pw).await {
        Ok(russh::client::AuthResult::Success) => return Ok(true),
        Ok(russh::client::AuthResult::Failure {
            remaining_methods,
            partial_success,
        }) => {
            // russh 在回复通道关掉时（对端拆连接）给的是 Ok(Failure) 而不是 Err。
            // 这时不能报成密码错。
            if handle.is_closed() {
                anyhow::bail!(
                    "{}",
                    crate::i18n::tr(
                        "连接在认证过程中中断",
                        "Connection lost during authentication"
                    )
                );
            }
            if !should_try_kbd_after_password(&remaining_methods, partial_success) {
                return Ok(false);
            }
            authenticate_password_via_kbd(
                handle,
                username,
                pw,
                sink,
                cmd_rx,
                prelude,
                !partial_success,
            )
            .await
        }
        Err(e) => Err(auth_transport_error(e)),
    }
}

fn auth_transport_error(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: {e}",
        crate::i18n::tr(
            "连接在认证过程中中断",
            "Connection lost during authentication",
        )
    )
}

/// 用已保存的密码应答 kbd-int：最多自动填**第一轮、且只有一个不回显提示**。
/// 常见 PAM OTP（Google Authenticator）也是 echo=false，后续轮次必须弹窗。
async fn authenticate_password_via_kbd<H>(
    handle: &mut Handle<H>,
    username: &str,
    pw: &str,
    sink: &UiSink,
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
    prelude: &mut VecDeque<UiCommand>,
    auto_fill: bool,
) -> anyhow::Result<bool>
where
    H: Handler,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    use client::KeyboardInteractiveAuthResponse as Resp;
    let mut resp = handle
        .authenticate_keyboard_interactive_start(username.to_string(), None)
        .await
        .map_err(auth_transport_error)?;
    let mut already_filled = false;
    loop {
        match resp {
            Resp::Success => return Ok(true),
            Resp::Failure { .. } => {
                if handle.is_closed() {
                    anyhow::bail!("disconnected");
                }
                return Ok(false);
            }
            Resp::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                if prompts.is_empty() {
                    resp = handle
                        .authenticate_keyboard_interactive_respond(Vec::new())
                        .await
                        .map_err(auth_transport_error)?;
                    continue;
                }
                let echoes: Vec<bool> = prompts.iter().map(|p| p.echo).collect();
                let answers = if auto_fill && should_autofill_kbd_password(already_filled, &echoes)
                {
                    already_filled = true;
                    vec![pw.to_string()]
                } else {
                    sink.send(WorkerEvent::KbdPrompt {
                        name,
                        instructions,
                        prompts: prompts.iter().map(|p| (p.prompt.clone(), p.echo)).collect(),
                    });
                    match wait_kbd_answers(cmd_rx, prelude).await {
                        Ok(a) => a,
                        Err(end) => return Err(kbd_aborted(end)),
                    }
                };
                resp = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(auth_transport_error)?;
            }
        }
    }
}

#[derive(Debug)]
enum KbdWaitEnd {
    /// 用户在提示框里点了断开。
    Cancelled,
    /// 命令通道关了（worker 正在退出）。
    Closed,
}

fn kbd_aborted(end: KbdWaitEnd) -> anyhow::Error {
    match end {
        KbdWaitEnd::Cancelled => anyhow::anyhow!(
            "{}",
            crate::i18n::tr("已取消认证", "Authentication cancelled")
        ),
        KbdWaitEnd::Closed => anyhow::anyhow!(
            "{}",
            crate::i18n::tr(
                "连接在认证过程中中断",
                "Connection lost during authentication"
            )
        ),
    }
}

/// 等键盘交互的回答。Resize / 转发 / 按键等非认证命令先存进 `prelude`，
/// 认证结束后由开 PTY 的逻辑消化——首帧 Resize 若在这里被丢掉，会话会
/// 以 80×24 打开并再空等一轮尺寸。
async fn wait_kbd_answers(
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
    prelude: &mut VecDeque<UiCommand>,
) -> Result<Vec<String>, KbdWaitEnd> {
    loop {
        match cmd_rx.recv().await {
            Some(UiCommand::KbdResponse(a)) => return Ok(a),
            Some(UiCommand::Disconnect) => return Err(KbdWaitEnd::Cancelled),
            None => return Err(KbdWaitEnd::Closed),
            Some(other) => prelude.push_back(other),
        }
    }
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

/// 解析本机 `DISPLAY` → `(screen, unix socket, 真 MIT cookie)`。
/// 真 cookie 不离开本进程；发给远端的是一次性假 cookie。
///
/// `xauth list` 是同步子进程。XAUTHORITY 落在卡住的 NFS 上时会一直不返回，
/// 所以放到 `spawn_blocking`，不占着 SSH worker 的异步任务。
async fn resolve_local_x11() -> Option<(u32, std::path::PathBuf, [u8; 16])> {
    tokio::task::spawn_blocking(resolve_local_x11_blocking)
        .await
        .ok()
        .flatten()
}

fn resolve_local_x11_blocking() -> Option<(u32, std::path::PathBuf, [u8; 16])> {
    #[cfg(not(unix))]
    {
        return None;
    }
    #[cfg(unix)]
    {
        let display = std::env::var("DISPLAY").ok()?;
        let (disp_n, screen) = parse_x11_display(&display)?;
        let sock = std::path::PathBuf::from(format!("/tmp/.X11-unix/X{disp_n}"));
        if !sock.exists() {
            return None;
        }
        let real = x11_mit_cookie(&display)?;
        Some((screen, sock, real))
    }
}

/// `DISPLAY` 形如 `:0` / `:0.1` / `host:10.0` → `(display_number, screen)`。
fn parse_x11_display(display: &str) -> Option<(u32, u32)> {
    let rest = display.rsplit_once(':').map(|(_, r)| r)?;
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.split('.');
    let n: u32 = parts.next()?.parse().ok()?;
    let screen: u32 = parts.next().unwrap_or("0").parse().ok()?;
    Some((n, screen))
}

fn decode_hex_cookie(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn cookie_hex(c: &[u8; 16]) -> String {
    c.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_x11_cookie() -> [u8; 16] {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut b).is_ok() {
            return b;
        }
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(1);
    for (i, slot) in b.iter_mut().enumerate() {
        *slot = ((t >> (i * 5)) as u8).wrapping_add((i as u8).wrapping_mul(41));
    }
    b
}

/// 从 `xauth list $DISPLAY` 取 16 字节 MIT-MAGIC-COOKIE-1。
fn x11_mit_cookie(display: &str) -> Option<[u8; 16]> {
    let out = std::process::Command::new("xauth")
        .args(["list", display])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[parts.len() - 2] == "MIT-MAGIC-COOKIE-1" {
            if let Some(cookie) = decode_hex_cookie(parts[parts.len() - 1]) {
                return Some(cookie);
            }
        }
    }
    None
}

/// 生成假 cookie 写入 `slot`。返回 `(screen, 假 cookie 的十六进制)`，后者才是 `request_x11` 的载荷。
async fn install_fake_x11(slot: &X11Slot) -> Option<(u32, String)> {
    let (screen, sock, real) = resolve_local_x11().await?;
    let mut fake = random_x11_cookie();
    if fake == real {
        fake[0] ^= 0xff;
    }
    let hex = cookie_hex(&fake);
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(X11Auth { sock, real, fake });
    Some((screen, hex))
}

fn x11_pad4(n: usize) -> usize {
    (4 - (n % 4)) % 4
}

/// 把 X11 连接握手里的假 cookie 换成真 cookie。对不上假 cookie 就拒绝，不接到本机 X。
fn rewrite_x11_client_prefix(
    buf: &[u8],
    fake: &[u8; 16],
    real: &[u8; 16],
) -> Result<Vec<u8>, &'static str> {
    if buf.len() < 12 {
        return Err("x11 prefix short");
    }
    let be = match buf[0] {
        b'B' => true,
        b'l' => false,
        _ => return Err("x11 byte order"),
    };
    let u16_at = |off: usize| -> u16 {
        let pair = [buf[off], buf[off + 1]];
        if be {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    };
    let name_len = u16_at(6) as usize;
    let data_len = u16_at(8) as usize;
    if name_len > 256 || data_len > 256 {
        return Err("x11 auth too long");
    }
    let total = 12 + name_len + x11_pad4(name_len) + data_len + x11_pad4(data_len);
    if buf.len() < total {
        return Err("x11 prefix truncated");
    }
    let name = &buf[12..12 + name_len];
    if name != b"MIT-MAGIC-COOKIE-1" {
        return Err("x11 auth protocol");
    }
    let data_at = 12 + name_len + x11_pad4(name_len);
    if &buf[data_at..data_at + data_len] != fake {
        return Err("x11 cookie mismatch");
    }
    let mut out = buf[..total].to_vec();
    out[data_at..data_at + data_len].copy_from_slice(real);
    Ok(out)
}

/// 远端连上 X 转发口之后若一个字节都不发，两次 `read_exact` 会把这条通道和任务
/// 永远挂住。握手本身只有十几字节，超时就拆掉。
#[cfg(unix)]
const X11_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(unix)]
async fn read_x11_exact(
    remote: &mut (impl tokio::io::AsyncRead + Unpin),
    buf: &mut [u8],
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;
    match tokio::time::timeout(X11_HANDSHAKE_TIMEOUT, remote.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => anyhow::bail!(
            "{}",
            crate::i18n::tr("X11 握手超时", "X11 handshake timed out")
        ),
    }
}

#[cfg(unix)]
async fn bridge_local_x11(channel: Channel<client::Msg>, auth: X11Auth) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut remote = channel.into_stream();
    let mut hdr = [0u8; 12];
    read_x11_exact(&mut remote, &mut hdr).await?;
    let be = match hdr[0] {
        b'B' => true,
        b'l' => false,
        _ => anyhow::bail!("X11 字节序无效"),
    };
    let name_len = {
        let pair = [hdr[6], hdr[7]];
        if be {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    } as usize;
    let data_len = {
        let pair = [hdr[8], hdr[9]];
        if be {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    } as usize;
    if name_len > 256 || data_len > 256 {
        anyhow::bail!("X11 认证字段过长");
    }
    let rest = name_len + x11_pad4(name_len) + data_len + x11_pad4(data_len);
    let mut prefix = vec![0u8; 12 + rest];
    prefix[..12].copy_from_slice(&hdr);
    read_x11_exact(&mut remote, &mut prefix[12..]).await?;
    let rewritten = rewrite_x11_client_prefix(&prefix, &auth.fake, &auth.real)
        .map_err(|e| anyhow::anyhow!(e))?;
    let mut local = tokio::net::UnixStream::connect(&auth.sock).await?;
    local.write_all(&rewritten).await?;
    tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn bridge_local_x11(_channel: Channel<client::Msg>, _auth: X11Auth) -> anyhow::Result<()> {
    anyhow::bail!("X11 转发仅支持 Unix")
}

/// 把一条反向转发来的通道桥接到本机 AI/MCP 控制 socket（每进程一份
/// `~/.config/ishell/mcp-<pid>.sock`）。MCP 本地 IPC 建立在 Unix domain socket 上，
/// 目前只支持 unix 平台；Windows 上直接报错（不静默丢弃这条转发通道）。
#[cfg(unix)]
async fn bridge_local_mcp(channel: Channel<client::Msg>) -> anyhow::Result<()> {
    let sock = crate::store::mcp_socket_path()
        .ok_or_else(|| anyhow::anyhow!("mcp socket 路径不可用（无法确定用户目录）"))?;
    let mut remote = channel.into_stream();
    let mut local = tokio::net::UnixStream::connect(&sock).await?;
    tokio::io::copy_bidirectional(&mut remote, &mut local).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn bridge_local_mcp(_channel: Channel<client::Msg>) -> anyhow::Result<()> {
    anyhow::bail!("AI/MCP 控制目前仅支持 Unix（Linux/macOS）系统，暂不支持 Windows")
}

/// `-R` 路由：按远端报告的 connected_address:port 精确匹配。
/// 同一端口只有一条登记时，才按端口兜底（sshd 常把 `0.0.0.0` 报成 `127.0.0.1`）。
/// 同端口多条时不做地址别名，避免串到另一条本机目标。
fn lookup_remote_fwd(table: &RemoteFwdTable, addr: &str, port: u32) -> Option<(String, u16)> {
    let map = table.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = map.get(&(addr.to_string(), port)) {
        return Some(v.clone());
    }
    let mut by_port = map.iter().filter(|((_, p), _)| *p == port);
    let first = by_port.next()?;
    if by_port.next().is_none() {
        return Some(first.1.clone());
    }
    None
}

/// 把 forwarded-tcpip 通道桥到本机 TCP 目标。
async fn bridge_local_tcp(
    channel: Channel<client::Msg>,
    host: &str,
    port: u16,
) -> anyhow::Result<()> {
    let mut remote = channel.into_stream();
    let mut local = tokio::net::TcpStream::connect((host, port)).await?;
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
    prelude: &mut VecDeque<UiCommand>,
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
                // 等 UI 回答。断开是用户取消，不能再报成凭据错误。
                let answers = match wait_kbd_answers(cmd_rx, prelude).await {
                    Ok(a) => a,
                    Err(end) => return Err(kbd_aborted(end)),
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
) -> anyhow::Result<(
    Handle<ClientHandler>,
    Option<Handle<JumpHandler>>,
    RemoteFwdTable,
    X11Slot,
    VecDeque<UiCommand>,
)> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(120)), // keepalive 30s × ~4；原 3600 时 TCP 黑洞最坏近 1h 才感知
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    // 主机密钥确认通道：跳板机与目标主机共享（顺序询问，不并发）
    let decision_rx: HostKeyDecision = Arc::new(tokio::sync::Mutex::new(hostkey_rx));
    let remote_fwds: RemoteFwdTable = Arc::new(Mutex::new(HashMap::new()));
    let x11: X11Slot = Arc::new(Mutex::new(None));
    // 键盘交互等待期间到达的 Resize 等命令。认证函数只往这里追加。
    let mut cmd_prelude = VecDeque::new();

    let target_handler = ClientHandler {
        host: cfg.host.clone(),
        port: cfg.port,
        sink: sink.clone(),
        decision_rx: decision_rx.clone(),
        agent_forward: cfg.forward_agent,
        x11: x11.clone(),
        // cfg!(unix)：与 ssh/mod.rs 里那个反向转发任务保持同一条件——Windows 上没有
        // 监听方，转发出去的 socket 后面空无一人。
        mcp_forward: cfg!(unix) && crate::store::load_mcp_consent(),
        remote_fwds: remote_fwds.clone(),
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
        let mut jhandle = with_connect_timeout(async {
            Ok(client::connect(config.clone(), (jump.host.as_str(), jump.port), jhandler).await?)
        })
        .await?;
        if !authenticate(
            &mut jhandle,
            &jump.username,
            &jump.auth,
            sink,
            cmd_rx,
            &mut cmd_prelude,
        )
        .await?
        {
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
        let handle = with_connect_timeout(async {
            let ch = jhandle
                .channel_open_direct_tcpip(cfg.host.clone(), cfg.port as u32, "127.0.0.1", 0)
                .await?;
            Ok(client::connect_stream(config, ch.into_stream(), target_handler).await?)
        })
        .await?;
        (handle, Some(jhandle))
    } else {
        let handle = with_connect_timeout(async {
            Ok(client::connect(config, (cfg.host.as_str(), cfg.port), target_handler).await?)
        })
        .await?;
        (handle, None)
    };

    sink.send(WorkerEvent::Status(
        crate::i18n::tr("正在认证 …", "Authenticating …").into(),
    ));
    if !authenticate(
        &mut handle,
        &cfg.username,
        &cfg.auth,
        sink,
        cmd_rx,
        &mut cmd_prelude,
    )
    .await?
    {
        anyhow::bail!(
            "{}",
            crate::i18n::tr(
                "认证被拒绝（用户名/密码或密钥错误）",
                "Authentication rejected (bad credentials)"
            )
        );
    }
    Ok((handle, jump_keep, remote_fwds, x11, cmd_prelude))
}

/// 打开带 PTY 的交互式 shell 通道。`forward_agent` 为真时请求 agent 转发；
/// `forward_x11` 为真时请求 X11 转发（本机 DISPLAY + xauth cookie）。
/// `cols`/`rows` 用 UI 上报的真实窗口尺寸，避免先以 80×24 起 shell 再 resize 闪屏。
pub(super) async fn open_shell(
    handle: &Handle<ClientHandler>,
    forward_agent: bool,
    forward_x11: bool,
    cols: u16,
    rows: u16,
    x11: &X11Slot,
    sink: &UiSink,
) -> anyhow::Result<russh::Channel<client::Msg>> {
    // request_pty/request_shell 均为 &self；等 X11 的 CHANNEL_SUCCESS/FAILURE 需要 wait()
    let mut channel = handle.channel_open_session().await?;
    // 在该会话通道上请求 agent 转发；服务器随后回连的 auth-agent 通道由
    // ClientHandler::server_channel_open_agent_forward 桥接到本机 agent。
    if forward_agent {
        let _ = channel.agent_forward(false).await;
    }
    let cols = cols.max(1);
    let rows = rows.max(1);
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await?;
    // 固定 C.UTF-8，不用本机 LANG：远端没装 zh_CN.UTF-8 这类区域时每个会话都会报 locale 错误。
    // C.UTF-8 在常见发行版里都有，足够让中文文件名不被转义。
    let lang = "C.UTF-8";
    if let Err(e) = channel.set_env(false, "LANG", lang).await {
        log::debug!("set_env LANG={lang} 被拒或失败：{e}");
    }
    if let Err(e) = channel.set_env(false, "LC_ALL", lang).await {
        log::debug!("set_env LC_ALL={lang} 被拒或失败：{e}");
    }
    // 显式声明 TERM / 真彩色：许多工具（ls、bat、rg、nvim 等）靠 COLORTERM=truecolor
    // 才输出 SGR 38;2 / 48;2；仅 PTY 类型 xterm-256color 不足以开启 24 位色。
    let _ = channel.set_env(false, "TERM", "xterm-256color").await;
    let _ = channel.set_env(false, "COLORTERM", "truecolor").await;
    if forward_x11 {
        match install_fake_x11(x11).await {
            Some((screen, fake_hex)) => {
                // want_reply=false 时 sshd 不回 CHANNEL_FAILURE，拒绝路径永远走不到。
                // 先等 Success/Failure，再开 shell，避免这条应答混进后面的终端数据。
                let rejected = match channel
                    .request_x11(true, false, "MIT-MAGIC-COOKIE-1", fake_hex, screen)
                    .await
                {
                    Err(e) => Some(e.to_string()),
                    Ok(()) => {
                        if x11_request_accepted(&mut channel).await {
                            None
                        } else {
                            Some(
                                crate::i18n::tr(
                                    "远端拒绝了 X11 转发（X11Forwarding 未开，或未安装 xauth）",
                                    "remote rejected X11 forwarding (X11Forwarding off, or xauth missing)",
                                )
                                .into(),
                            )
                        }
                    }
                };
                if let Some(why) = rejected {
                    *x11.lock().unwrap_or_else(|err| err.into_inner()) = None;
                    sink.send(WorkerEvent::Status(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("X11 转发请求被拒绝：{why}"),
                        crate::i18n::Lang::En => format!("X11 forwarding request rejected: {why}"),
                    }));
                }
            }
            None => {
                sink.send(WorkerEvent::Status(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => {
                        "已开启 X11 转发，但本机 DISPLAY/xauth 不可用，已跳过".into()
                    }
                    crate::i18n::Lang::En => {
                        "X11 forwarding enabled, but local DISPLAY/xauth unavailable; skipped"
                            .into()
                    }
                }));
            }
        }
    }
    channel.request_shell(false).await?;
    Ok(channel)
}

/// `request_x11(want_reply=true)` 只是把请求送出去。sshd 接受与否要看随后的
/// CHANNEL_SUCCESS / CHANNEL_FAILURE。
const X11_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

async fn x11_request_accepted(channel: &mut Channel<client::Msg>) -> bool {
    let deadline = tokio::time::Instant::now() + X11_REPLY_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, channel.wait()).await {
            Ok(Some(ChannelMsg::Success)) => return true,
            Ok(Some(ChannelMsg::Failure)) | Ok(None) | Err(_) => return false,
            Ok(Some(_)) => {}
        }
    }
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

/// 一次 exec 捕获允许在内存里累积的 stdout 上限。
///
/// 这几个函数原先是无界 `extend_from_slice`：远端命令输出多少就在本进程里攒多少。调用点
/// 现在都是自己拼的命令，但其中两条的输出**由远端文件决定大小**——`pdftotext <书> -` 会把
/// 整本 PDF 的文本全倒出来，`pdftoppm -r 300` 渲染一张 A0 图纸是上亿像素的 PNG。都不需要
/// 恶意构造，一个大文件就够把 GUI 进程撑爆。
///
/// 64 MiB 的取法：单页 PNG 常见是几 MB（A4/300dpi 约 1240×1754），整本书的纯文本通常几 MB，
/// 都留了一个数量级的余量；真超过了，报错也远好过 OOM。
const EXEC_CAPTURE_LIMIT: usize = 64 * 1024 * 1024;

/// stderr 只是给人看的诊断文本，攒够这些就不再追加（但**继续读到通道关闭**，否则拿不到
/// 退出码）。不像 stdout 那样超限即失败：一条命令刷了一堆警告到 stderr 但其实成功了，
/// 为此把整个操作判失败是本末倒置。
const EXEC_STDERR_LIMIT: usize = 256 * 1024;

fn exec_overflow(cmd: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!(
                "远端命令输出超过 {} MiB 上限，已中止读取：{cmd}",
                EXEC_CAPTURE_LIMIT / 1024 / 1024
            ),
            crate::i18n::Lang::En => format!(
                "remote command output exceeded the {} MiB limit, aborted: {cmd}",
                EXEC_CAPTURE_LIMIT / 1024 / 1024
            ),
        }
    )
}

/// 往诊断缓冲里追加，攒到上限就不再增长（内容截断，不报错）。
fn push_capped(buf: &mut Vec<u8>, data: &[u8], limit: usize) {
    if buf.len() >= limit {
        return;
    }
    let room = limit - buf.len();
    buf.extend_from_slice(&data[..room.min(data.len())]);
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
            ChannelMsg::Data { data } => {
                if buf.len() + data.len() > EXEC_CAPTURE_LIMIT {
                    return Err(exec_overflow(cmd));
                }
                buf.extend_from_slice(&data);
            }
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
            ChannelMsg::Data { data } => {
                if out.len() + data.len() > EXEC_CAPTURE_LIMIT {
                    return Err(exec_overflow(cmd));
                }
                out.extend_from_slice(&data);
            }
            ChannelMsg::ExtendedData { data, ext: 1 } => {
                push_capped(&mut err, &data, EXEC_STDERR_LIMIT)
            }
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
            ChannelMsg::ExtendedData { data, ext: 1 } => {
                push_capped(&mut err, &data, EXEC_STDERR_LIMIT)
            }
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, String::from_utf8_lossy(&err).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::{
        password_looks_undecrypted, should_autofill_kbd_password, should_try_kbd_after_password,
        UNDECRYPTED_SECRET_PREFIX,
    };
    use russh::{MethodKind, MethodSet};

    fn methods(kinds: &[MethodKind]) -> MethodSet {
        MethodSet::from(kinds)
    }

    #[test]
    fn undecrypted_ciphertext_is_not_sent_as_a_password() {
        assert!(password_looks_undecrypted(&format!(
            "{UNDECRYPTED_SECRET_PREFIX}abc"
        )));
        assert!(!password_looks_undecrypted("hunter2"));
        assert!(!password_looks_undecrypted(""));
    }

    #[test]
    fn kbd_not_tried_when_password_still_offered() {
        let remaining = methods(&[MethodKind::Password, MethodKind::KeyboardInteractive]);
        assert!(
            !should_try_kbd_after_password(&remaining, false),
            "密码错了 remaining 里通常还有 password，再打 kbd-int 会把同一密码计两次失败"
        );
        assert!(
            should_try_kbd_after_password(&remaining, true),
            "partial_success 表示这一步被接受，可以接着做 kbd-int"
        );
    }

    #[test]
    fn kbd_tried_when_only_keyboard_interactive_remains() {
        let remaining = methods(&[MethodKind::KeyboardInteractive]);
        assert!(should_try_kbd_after_password(&remaining, false));
        assert!(should_try_kbd_after_password(&remaining, true));
    }

    #[test]
    fn kbd_not_tried_when_keyboard_interactive_absent() {
        let remaining = methods(&[MethodKind::Password]);
        assert!(!should_try_kbd_after_password(&remaining, false));
        assert!(!should_try_kbd_after_password(&remaining, true));
        let none = MethodSet::empty();
        assert!(!should_try_kbd_after_password(&none, false));
    }

    #[test]
    fn autofill_only_first_single_non_echo_prompt() {
        assert!(should_autofill_kbd_password(false, &[false]));
        assert!(
            !should_autofill_kbd_password(true, &[false]),
            "第二轮 OTP 不能再填登录密码"
        );
        assert!(!should_autofill_kbd_password(false, &[false, false]));
        assert!(!should_autofill_kbd_password(false, &[true]));
        assert!(!should_autofill_kbd_password(false, &[]));
        assert!(!should_autofill_kbd_password(false, &[false, true]));
    }

    #[test]
    fn parse_x11_display_variants() {
        assert_eq!(super::parse_x11_display(":0"), Some((0, 0)));
        assert_eq!(super::parse_x11_display(":0.1"), Some((0, 1)));
        assert_eq!(super::parse_x11_display("localhost:10.0"), Some((10, 0)));
        assert_eq!(super::parse_x11_display("unix:1"), Some((1, 0)));
        assert_eq!(super::parse_x11_display("nodisplay"), None);
    }

    #[test]
    fn remote_fwd_same_port_does_not_alias() {
        let table = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::from([
            (("0.0.0.0".into(), 9000u32), ("127.0.0.1".into(), 1111u16)),
            (("10.0.0.8".into(), 9000u32), ("127.0.0.1".into(), 2222u16)),
        ])));
        assert_eq!(
            super::lookup_remote_fwd(&table, "10.0.0.8", 9000),
            Some(("127.0.0.1".into(), 2222))
        );
        assert_eq!(super::lookup_remote_fwd(&table, "127.0.0.1", 9000), None);
        let one = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::from([(
            ("0.0.0.0".into(), 9000u32),
            ("127.0.0.1".into(), 1111u16),
        )])));
        assert_eq!(
            super::lookup_remote_fwd(&one, "127.0.0.1", 9000),
            Some(("127.0.0.1".into(), 1111))
        );
    }

    #[test]
    fn x11_prefix_swaps_fake_cookie_only() {
        let fake = [1u8; 16];
        let real = [2u8; 16];
        let name = b"MIT-MAGIC-COOKIE-1";
        let mut buf = vec![b'B', 0, 0, 11, 0, 0, 0, name.len() as u8, 0, 16, 0, 0];
        buf.extend_from_slice(name);
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&fake);
        let out = super::rewrite_x11_client_prefix(&buf, &fake, &real).unwrap();
        assert!(out.windows(16).any(|w| w == real));
        assert!(!out.windows(16).any(|w| w == fake));
        assert!(super::rewrite_x11_client_prefix(&buf, &[9u8; 16], &real).is_err());
    }

    #[tokio::test]
    async fn kbd_wait_keeps_non_auth_commands() {
        use crate::proto::UiCommand;
        use tokio::sync::mpsc::unbounded_channel;
        let (tx, mut rx) = unbounded_channel();
        tx.send(UiCommand::Resize {
            cols: 100,
            rows: 40,
        })
        .unwrap();
        tx.send(UiCommand::TerminalInput(b"ls".to_vec())).unwrap();
        tx.send(UiCommand::KbdResponse(vec!["otp".into()])).unwrap();
        let mut prelude = std::collections::VecDeque::new();
        let answers = super::wait_kbd_answers(&mut rx, &mut prelude)
            .await
            .expect("回答");
        assert_eq!(answers, vec!["otp".to_string()]);
        assert!(matches!(
            prelude.pop_front(),
            Some(UiCommand::Resize {
                cols: 100,
                rows: 40
            })
        ));
        assert!(matches!(
            prelude.pop_front(),
            Some(UiCommand::TerminalInput(_))
        ));

        let (tx, mut rx) = unbounded_channel();
        tx.send(UiCommand::Disconnect).unwrap();
        let err = super::wait_kbd_answers(&mut rx, &mut prelude)
            .await
            .unwrap_err();
        assert!(matches!(err, super::KbdWaitEnd::Cancelled));
        let cancel = super::kbd_aborted(super::KbdWaitEnd::Cancelled).to_string();
        let closed_msg = super::kbd_aborted(super::KbdWaitEnd::Closed).to_string();
        assert_ne!(cancel, closed_msg);
        assert!(
            !cancel.contains("密钥") && !cancel.to_ascii_lowercase().contains("credential"),
            "取消不该说成凭据错误：{cancel}"
        );

        let (tx, mut rx) = unbounded_channel();
        drop(tx);
        let err = super::wait_kbd_answers(&mut rx, &mut prelude)
            .await
            .unwrap_err();
        assert!(matches!(err, super::KbdWaitEnd::Closed));
    }

    #[test]
    fn x11_prefix_little_endian_and_rejects_a_short_header() {
        let fake = [3u8; 16];
        let real = [4u8; 16];
        let name = b"MIT-MAGIC-COOKIE-1";
        let mut buf = vec![b'l', 0, 0, 11, 0, 0, name.len() as u8, 0, 16, 0, 0, 0];
        buf.extend_from_slice(name);
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&fake);
        let out = super::rewrite_x11_client_prefix(&buf, &fake, &real).unwrap();
        assert!(out.windows(16).any(|w| w == real));
        assert!(super::rewrite_x11_client_prefix(&buf[..8], &fake, &real).is_err());
        buf[12] = b'X';
        assert!(super::rewrite_x11_client_prefix(&buf, &fake, &real).is_err());
    }
}
