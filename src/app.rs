//! 应用主体：会话管理 + 顶部标签 + 三区布局（系统信息 / 终端 / 文件）。

use std::sync::{Arc, Mutex};

use egui::{RichText, Sense};
use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{AuthMethod, ConflictPolicy, ConnectConfig, UiCommand, WorkerEvent};
use crate::ssh::{self, UiSink};
use crate::terminal::Terminal;
use crate::theme::Palette;
use crate::ui::connect::ConnectForm;
use crate::ui::file_panel::{self, FileAction, FilePanelState};
use crate::ui::sidebar::{self, NetHistory};

/// 是否已同意「向 shell 注入 OSC 7 上报工作目录」。同意一次后持久化，后续静默注入。
/// 用全局原子（启动时从 store 载入），便于 Session 的连接回调直接读取。
static OSC7_CONSENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn osc7_consent() -> bool {
    OSC7_CONSENT.load(std::sync::atomic::Ordering::Relaxed)
}
fn set_osc7_consent(v: bool) {
    OSC7_CONSENT.store(v, std::sync::atomic::Ordering::Relaxed);
    crate::store::save_osc7_consent(v);
}
/// 注入到交互式 shell 的 OSC 7 上报片段（bash 用 PROMPT_COMMAND，zsh 用 precmd）。
/// 仅作用于当前会话、不写 rc、不持久化；前导空格尽量不进 history。
const OSC7_SNIPPET: &str = r#" __ishell_cwd(){ printf '\033]7;file://localhost%s\007' "$PWD"; }; if [ -n "$ZSH_VERSION" ]; then autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __ishell_cwd 2>/dev/null; else case "$PROMPT_COMMAND" in *__ishell_cwd*) ;; *) PROMPT_COMMAND="__ishell_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}";; esac; fi; __ishell_cwd"#;

/// 单个 SSH 会话的前台状态。
struct Session {
    /// 稳定唯一 id（用于标签滑动动画在重排后仍追踪同一标签）
    uid: u64,
    title: String,
    /// 悬停提示（user@host，用于标签去掉 IP 后的消歧）
    tip: String,
    cmd_tx: UnboundedSender<UiCommand>,
    evt_rx: std::sync::mpsc::Receiver<WorkerEvent>,
    connected: bool,
    status: String,
    terminal: Terminal,
    sysinfo: Option<crate::proto::SysInfo>,
    net_hist: NetHistory,
    files: FilePanelState,
    last_size: (u16, u16),
    initialized: bool,
    transfers: Vec<Transfer>,
    next_xfer: u64,
    /// 侧栏网络监控选中的网卡（空 = 全部）
    selected_nic: String,
    /// 进程列表是否按内存排序（false = 按 CPU）
    proc_sort_mem: bool,
    /// 已读取待填充到占位编辑器标签的文件（id, path, content, encoding, eol, mtime）
    pending_open: Vec<(u64, String, String, String, crate::proto::Eol, u32)>,
    /// 保存成功回报的新 mtime（path, mtime）/ 外部改动冲突（path）
    pending_saved: Vec<(String, u32)>,
    /// 保存写入进度（path, done, total）——驱动编辑器标签「珊瑚→绿」保存动画
    pending_save_progress: Vec<(String, u64, u64)>,
    pending_conflict: Vec<String>,
    /// 打开时发现实际超限（id, path, size）——移除占位标签 + 弹「打开大文件」确认
    pending_too_large: Vec<(u64, String, u64)>,
    /// 待新建的占位编辑器标签（id, path）——双击打开时立即建，显示文件名 + 进度条
    pending_placeholder: Vec<(u64, String)>,
    /// 文件下载进度（id, done, total），驱动占位标签进度条
    pending_load_progress: Vec<(u64, u64, u64)>,
    /// 文件打开失败（id, message）——移除占位标签 + 提示
    pending_load_fail: Vec<(u64, String)>,
    /// 已读取待打开到看图工具的图片（path, 原始字节）
    pending_image: Vec<(String, Vec<u8>)>,
    /// 向 worker 回复"是否信任未知主机"
    hostkey_tx: UnboundedSender<bool>,
    /// 待确认的主机（host, 指纹, 是否为密钥变更）
    pending_hostkey: Option<(String, String, bool)>,
    /// 待回答的键盘交互认证提示（None = 无）
    kbd_prompt: Option<KbdPrompt>,
    /// 端口转发列表
    forwards: Vec<ForwardEntry>,
    next_forward: u64,
    /// 进程详情返回（pid, cmd, cwd, exe），由 App 取用后清空
    proc_detail: Option<(u32, String, String, String)>,
    /// 连接配置（用于断线重连）
    cfg: ConnectConfig,
    /// 是否曾成功连接（仅对掉线的会话自动重连，避免错误配置死循环）
    was_connected: bool,
    /// 计划在此刻自动重连
    reconnect_at: Option<std::time::Instant>,
    /// 已自动重连次数
    reconnect_tries: u32,
    /// 由 OSC 7 记录的终端工作目录（断线重连后用于 cd 恢复）
    last_cwd: String,
    /// 重连后待恢复 cwd
    restore_cwd: bool,
    /// 待弹出「注入 OSC 7」确认框（右键功能在无 cwd 时触发）
    osc7_confirm: bool,
    /// 已注入、等下个提示符上报 cwd 后把文件区跳过去
    osc7_pending_reveal: bool,
}

/// 传输的重发规格（断线重连/手动重试时据此重新发起，底层自动续传）。
#[derive(Clone)]
enum XferSpec {
    Download { remote: String, local: String },
    Upload { local: String, remote_dir: String },
}

/// 传输列表的状态筛选。
#[derive(Clone, Copy, PartialEq, Eq)]
enum XferFilter {
    All,
    Active,
    Done,
    Failed,
}

/// 本地端口预占用探测（添加端口转发前）：仅当明确 `AddrInUse` 才判为占用；
/// 其它错误（如 bind 地址非本机网卡）返回 false——不武断拦截，交给 worker 实际绑定决定。
fn local_port_in_use(host: &str, port: u16) -> bool {
    let h = if host.trim().is_empty() { "127.0.0.1" } else { host };
    match std::net::TcpListener::bind((h, port)) {
        Ok(_) => false, // 立即 drop 释放，worker 随后真正绑定
        Err(e) => e.kind() == std::io::ErrorKind::AddrInUse,
    }
}

/// 取远端路径的所在目录：去尾斜杠后截到最后一个 `/`；根下或无斜杠返回 `/`。
fn parent_dir(path: &str) -> String {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => t[..i].to_string(),
    }
}

/// 把秒数格式化为紧凑时长（用于传输 ETA）：`45s` / `3m20s` / `1h2m`。
fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// UI 侧的一条传输记录。
struct Transfer {
    id: u64,
    name: String,
    dir: crate::proto::TransferDir,
    /// 重发规格（用于断线重连续传 / 手动重试）；演示记录为 None
    spec: Option<XferSpec>,
    /// 因断线被中断、等待重连后自动续传
    paused: bool,
    done: u64,
    total: u64,
    /// None=进行中，Some(true/false)=完成/失败
    ok: Option<bool>,
    /// 下载到的本地路径（用于「打开所在文件夹」）
    local: Option<String>,
    /// 完成/失败原因（点击状态可展开查看）
    message: String,
    /// 是否展开显示失败原因
    show_err: bool,
    /// 实时速度（字节/秒，指数平滑）
    speed: f64,
    /// 上次采样的已传字节数与时刻（用于计算速度）
    last_done: u64,
    last_t: Option<std::time::Instant>,
    /// 进行中的阶段提示（如「打包中…」「解包中…」「等待源端…」「直传中…」）；非空时在详情行替代字节读数
    note: String,
    /// 排队等待态（如跨服务器中转的目标端，正等源端下载完成）：显示「等待」而非进度数字
    queued: bool,
    /// 模式徽标（如「直传」）：显示在文件大小之后，标注传输方式；空串不显示
    tag: String,
}

impl Transfer {
    /// 新建一条「进行中」的传输记录（note/queued 默认空，speed 等计量字段归零）。
    fn new(id: u64, name: String, dir: crate::proto::TransferDir, total: u64, local: Option<String>, spec: Option<XferSpec>) -> Self {
        Transfer {
            id, name, dir, spec, paused: false, done: 0, total, ok: None, local,
            message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None,
            note: String::new(), queued: false, tag: String::new(),
        }
    }
}

/// App 级文件剪贴板（跨 tab 共享）。
struct FileClip {
    /// (绝对路径, 是否目录)
    items: Vec<(String, bool)>,
    /// true=剪切（粘贴时移动），false=复制
    is_cut: bool,
    src_uid: u64,
    src_host: String,
    src_port: u16,
    src_label: String,
}

/// 待确认的粘贴计划（剪切，或跨服务器复制/剪切，执行前二次确认）。
#[derive(Clone)]
struct PendingPaste {
    items: Vec<(String, bool)>,
    is_cut: bool,
    /// 源与目标是否不同服务器（需经本地中转或直传）
    cross: bool,
    src_uid: u64,
    dest_uid: u64,
    /// 源目录（被复制/剪切项的所在目录，用于确认弹框展示）
    src_dir: String,
    dest_dir: String,
    src_label: String,
    dest_label: String,
    /// 跨服务器时是否走「直传」（true=源主机直推目标；false=经本地中转）。一级确认后才据传输方式设定。
    direct: bool,
}

/// 直传任务追踪（App 侧）：源会话里一条直传传输（id）的归属与善后信息。
/// 成功 → 剪切则删源 + 刷新目标目录；失败 → 弹「转中转」提醒。
struct DirectJob {
    /// 源会话里的传输 id（真实数据通路在此）
    id: u64,
    /// 目标会话里的「镜像」进度行 id（直传数据不经 B，App 据源端进度同步显示）
    mir_id: u64,
    src_uid: u64,
    dest_uid: u64,
    /// 源目录（回退中转确认弹框展示用）
    src_dir: String,
    dest_dir: String,
    is_cut: bool,
    /// 原始条目（失败回退中转、或剪切删源时用）
    items: Vec<(String, bool)>,
    src_label: String,
    dest_label: String,
    /// 用户主动取消（经取消按钮标记）：失败收尾时据此跳过「转中转」提醒，避免误弹
    cancelled: bool,
}

/// 直传失败后，等待用户确认「转中转」的计划 + 失败原因。
struct DirectFallback {
    plan: PendingPaste,
    reason: String,
}

/// 跨服务器中转任务：源会话下载到本地临时 → 目标会话上传 →（剪切则删源）。
struct Relay {
    src_path: String,
    is_dir: bool,
    src_uid: u64,
    dest_uid: u64,
    dest_dir: String,
    is_cut: bool,
    tmp: std::path::PathBuf,
    phase: RelayPhase,
    /// 目标会话里预占的上传传输 id（粘贴时即登记「等待」占位行，源端下载完才真正发起上传）
    up_id: u64,
}

/// 中转任务阶段：保存对应会话里的传输 id，用于轮询完成状态。
enum RelayPhase {
    Down(u64),
    Up(u64),
}

/// 键盘交互认证的一组待回答提示（每项 (提示文本, 是否回显) + 用户输入缓冲）。
struct KbdPrompt {
    name: String,
    instructions: String,
    /// (提示文本, 是否回显)
    prompts: Vec<(String, bool)>,
    /// 与 prompts 等长的回答缓冲
    answers: Vec<String>,
}

impl Session {
    /// 排空后台事件，更新本地状态。
    fn drain_events(&mut self) {
        while let Ok(ev) = self.evt_rx.try_recv() {
            match ev {
                WorkerEvent::Status(s) => self.status = s,
                WorkerEvent::Connected => {
                    self.connected = true;
                    self.was_connected = true;
                    self.reconnect_tries = 0;
                    self.reconnect_at = None;
                    self.status = crate::i18n::tr("已连接", "Connected").into();
                    // 重连后恢复工作目录（若断线前由 OSC 7 记录过）
                    if self.restore_cwd && !self.last_cwd.is_empty() {
                        let quoted = format!("'{}'", self.last_cwd.replace('\'', "'\\''"));
                        let _ = self.cmd_tx.send(UiCommand::TerminalInput(format!("cd {quoted}\r").into_bytes()));
                    }
                    self.restore_cwd = false;
                    // OSC 7 注入改为「点菜单时按需注入」（那时 shell 闲置在提示符，回显可被可靠吞掉），
                    // 不在连接时自动注入，避免与 MOTD/首个提示符的输出竞争、以及每次连接都注入。
                    // 断线前被中断的传输：重连后用新通道重发，底层据本地/远端已有字节自动续传
                    for t in &mut self.transfers {
                        if !t.paused {
                            continue;
                        }
                        match &t.spec {
                            Some(XferSpec::Download { remote, local }) => {
                                let _ = self.cmd_tx.send(UiCommand::Download { id: t.id, remote: remote.clone(), local: local.clone(), policy: ConflictPolicy::Overwrite });
                            }
                            Some(XferSpec::Upload { local, remote_dir }) => {
                                let _ = self.cmd_tx.send(UiCommand::Upload { id: t.id, local: local.clone(), remote_dir: remote_dir.clone(), policy: ConflictPolicy::Overwrite });
                            }
                            None => continue,
                        }
                        t.paused = false;
                        t.message = crate::i18n::tr("续传中 …", "Resuming …").into();
                    }
                    // 断线前建立的端口转发：用新通道重建（沿用原 id/配置）。首次连接时 forwards 为空，无操作。
                    let readd: Vec<crate::proto::ForwardSpec> = self
                        .forwards
                        .iter()
                        .map(|f| crate::proto::ForwardSpec {
                            id: f.id,
                            bind_host: f.bind_host.clone(),
                            bind_port: f.bind_port,
                            kind: f.kind.clone(),
                        })
                        .collect();
                    for spec in readd {
                        let _ = self.cmd_tx.send(UiCommand::AddForward(spec));
                    }
                }
                WorkerEvent::Disconnected(reason) => {
                    self.connected = false;
                    self.status = reason;
                    // 进行中的传输标记为暂停，等重连后续传（不计为失败）
                    for t in &mut self.transfers {
                        if t.spec.is_some() && t.ok != Some(true) {
                            t.ok = None;
                            t.paused = true;
                            t.speed = 0.0;
                            t.message = crate::i18n::tr("已中断，重连后续传", "Interrupted; will resume").into();
                        }
                    }
                    // 仅对"曾连上又掉线"的会话自动重连，最多 5 次，指数退避
                    const MAX_TRIES: u32 = 5;
                    if self.was_connected && self.reconnect_tries < MAX_TRIES {
                        let secs = (2u64 << self.reconnect_tries.min(4)).min(30); // 2,4,8,16,30
                        self.reconnect_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(secs));
                        let tail = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("{secs}s 后重连"), crate::i18n::Lang::En => format!("reconnect in {secs}s") };
                        self.status = format!("{} · {}", self.status, tail);
                    }
                }
                WorkerEvent::TerminalData(bytes) => {
                    self.terminal.feed(&bytes);
                    if let Some(c) = self.terminal.cwd() {
                        self.last_cwd = c.to_string();
                    }
                }
                WorkerEvent::SysInfo(info) => {
                    // 历史曲线记录当前选中网卡（空=全部）的速率
                    let (rx, tx) = if self.selected_nic.is_empty() {
                        (info.net_rx_bps, info.net_tx_bps)
                    } else {
                        info.nets
                            .iter()
                            .find(|n| n.name == self.selected_nic)
                            .map(|n| (n.rx_bps, n.tx_bps))
                            .unwrap_or((info.net_rx_bps, info.net_tx_bps))
                    };
                    self.net_hist.push(rx, tx);
                    self.sysinfo = Some(*info);
                }
                WorkerEvent::DirListing { path, entries } => {
                    self.files.on_listing(path, entries);
                }
                WorkerEvent::DirListFailed { path, message, retryable } => {
                    self.status = message;
                    self.files.on_list_failed(path, retryable);
                }
                WorkerEvent::ProcDetail { pid, cmd, cwd, exe } => {
                    self.proc_detail = Some((pid, cmd, cwd, exe));
                }
                WorkerEvent::ForwardStatus { id, ok, message } => {
                    if let Some(f) = self.forwards.iter_mut().find(|f| f.id == id) {
                        f.ok = ok;
                        f.status = message;
                    }
                }
                WorkerEvent::KbdPrompt { name, instructions, prompts } => {
                    let answers = vec![String::new(); prompts.len()];
                    self.kbd_prompt = Some(KbdPrompt { name, instructions, prompts, answers });
                }
                WorkerEvent::HostKeyPrompt { host, fingerprint, changed } => {
                    self.pending_hostkey = Some((host, fingerprint, changed));
                    self.status = crate::i18n::tr("等待确认主机指纹 …", "Awaiting host key …").into();
                }
                WorkerEvent::FileOpened { id, path, content, encoding, eol, mtime } => {
                    self.pending_open.push((id, path, content, encoding, eol, mtime));
                    self.status = crate::i18n::tr("已打开文件", "File opened").into();
                }
                WorkerEvent::FileSaved { path, mtime } => {
                    self.pending_saved.push((path, mtime));
                }
                WorkerEvent::FileSaveProgress { path, done, total } => {
                    self.pending_save_progress.push((path, done, total));
                }
                WorkerEvent::FileTooLarge { id, path, size } => {
                    self.pending_too_large.push((id, path, size));
                }
                WorkerEvent::FileSaveConflict { path } => {
                    self.pending_conflict.push(path);
                }
                WorkerEvent::FileLoadProgress { id, done, total } => {
                    self.pending_load_progress.push((id, done, total));
                }
                WorkerEvent::FileLoadFailed { id, message } => {
                    self.pending_load_fail.push((id, message.clone()));
                    self.status = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开失败：{message}"), crate::i18n::Lang::En => format!("Open failed: {message}") };
                }
                WorkerEvent::ImageOpened { path, data } => {
                    self.pending_image.push((path, data));
                    self.status = crate::i18n::tr("已打开图片", "Image opened").into();
                }
                WorkerEvent::OpDone { message, refresh_dir } => {
                    self.status = message;
                    // 刷新操作目标目录。拖拽移动到「非当前目录」的文件夹时，源目录(cwd)
                    // 已在前端乐观移除被移动项、不在此刷新，避免整目录重载导致的跳动。
                    self.refresh_dir(refresh_dir);
                }
                WorkerEvent::TransferStart { id, name, total, dir, local } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.name = name;
                        t.total = total;
                        t.dir = dir;
                        // worker 已真正开传：清除「等待」占位态
                        t.queued = false;
                        // 冲突重命名后，worker 上报的是最终本地路径；更新它，使「打开所在文件夹」定位到重命名后的文件
                        if local.is_some() {
                            t.local = local;
                        }
                    } else {
                        self.transfers.push(Transfer::new(id, name, dir, total, local, None));
                    }
                }
                WorkerEvent::TransferProgress { id, done } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        let now = std::time::Instant::now();
                        match t.last_t {
                            Some(prev) => {
                                let dt = now.duration_since(prev).as_secs_f64();
                                if dt >= 0.25 {
                                    let inst = done.saturating_sub(t.last_done) as f64 / dt;
                                    // 指数平滑，读数更稳
                                    t.speed = if t.speed <= 0.0 { inst } else { t.speed * 0.6 + inst * 0.4 };
                                    t.last_done = done;
                                    t.last_t = Some(now);
                                }
                            }
                            None => {
                                t.last_done = done;
                                t.last_t = Some(now);
                            }
                        }
                        t.done = done;
                    }
                }
                WorkerEvent::TransferNote { id, note } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.note = note;
                    }
                }
                WorkerEvent::TransferDone { id, ok, message, refresh_dir } => {
                    let connected = self.connected;
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.note = String::new();
                        t.queued = false;
                        if !ok && !connected && t.spec.is_some() {
                            // 断线引起的失败：转为暂停，等重连续传
                            t.paused = true;
                            t.speed = 0.0;
                            t.message = crate::i18n::tr("已中断，重连后续传", "Interrupted; will resume").into();
                        } else {
                            t.ok = Some(ok);
                            if ok && t.total == 0 {
                                t.total = t.done;
                            }
                            t.message = message.clone();
                            t.speed = 0.0;
                        }
                    }
                    self.status = message;
                    // 上传成功：记下「待选中」的文件名，列表刷新后在该目录选中它（拖动上传后高亮所传文件）
                    if ok {
                        if let Some((dir, name)) = self.transfers.iter().find(|t| t.id == id).and_then(|t| match &t.spec {
                            Some(XferSpec::Upload { remote_dir, .. }) => Some((remote_dir.clone(), t.name.clone())),
                            _ => None,
                        }) {
                            match &mut self.files.pending_select {
                                Some((d, names)) if *d == dir => {
                                    names.insert(name);
                                }
                                _ => self.files.pending_select = Some((dir, std::iter::once(name).collect())),
                            }
                        }
                    }
                    self.refresh_dir(refresh_dir);
                }
                WorkerEvent::Error(e) => self.status = e,
            }
        }
    }

    /// 刷新指定目录的列表（操作/传输完成后调用）。
    /// 静默刷新：不先移除旧列表——否则当前目录会瞬间变「加载中…」造成闪动。保留旧列表可见，
    /// 待 `ListDir` 返回时由 `on_listing` 原地覆盖（回填新建条目的 owner/权限/mtime 等真实元数据）。
    fn refresh_dir(&mut self, dir: Option<String>) {
        if let Some(dir) = dir {
            self.files.loading.insert(dir.clone());
            let _ = self.cmd_tx.send(UiCommand::ListDir(dir));
        }
    }

    /// 连接成功后初始化文件树：根 "/"，并定位到家目录。
    fn init_files(&mut self) {
        self.files.root = "/".into();
        self.files.expanded.insert("/".into());
        // 只请求 "."（服务端解析为家目录）作为 cwd；树的其余层级由 sync_tree 自动补全。
        // 不预先请求 "/"，避免它先返回把 cwd 设成根目录。
        let _ = self.cmd_tx.send(UiCommand::ListDir(".".into()));
    }
}

// ===== 全局视图状态（折叠监控栏/文件栏、界面缩放）=====
// 设为进程级全局，便于侧栏背景层与各子控件（进程行/网卡/IP 等）共用同一右键菜单，
// 避免「右键到子控件上弹不出完整菜单」的不一致。
static SIDEBAR_COLLAPSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FILES_COLLAPSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ZOOM_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0); // 0 哨兵=未初始化(按 1.0)
/// 「关于」弹框是否显示（右键菜单触发；自由函数态，与折叠/缩放一致用全局）。
static ABOUT_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn about_open() -> bool { ABOUT_OPEN.load(std::sync::atomic::Ordering::Relaxed) }
fn set_about_open(v: bool) { ABOUT_OPEN.store(v, std::sync::atomic::Ordering::Relaxed); }

fn sidebar_collapsed() -> bool { SIDEBAR_COLLAPSED.load(std::sync::atomic::Ordering::Relaxed) }
fn set_sidebar_collapsed(v: bool) { SIDEBAR_COLLAPSED.store(v, std::sync::atomic::Ordering::Relaxed); }
fn files_collapsed() -> bool { FILES_COLLAPSED.load(std::sync::atomic::Ordering::Relaxed) }
fn set_files_collapsed(v: bool) { FILES_COLLAPSED.store(v, std::sync::atomic::Ordering::Relaxed); }
fn ui_zoom() -> f32 {
    let b = ZOOM_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 { 1.0 } else { f32::from_bits(b) }
}
fn set_ui_zoom(z: f32) {
    // 量化到 5% 网格、夹在 70%–200%，变化才持久化
    let z = ((z * 20.0).round() / 20.0).clamp(0.7, 2.0);
    if (z - ui_zoom()).abs() > f32::EPSILON {
        ZOOM_BITS.store(z.to_bits(), std::sync::atomic::Ordering::Relaxed);
        crate::store::save_zoom(z);
    }
}
/// 启动时把已保存的缩放载入全局。
fn init_view_state() {
    ZOOM_BITS.store(crate::store::load_zoom().to_bits(), std::sync::atomic::Ordering::Relaxed);
    OSC7_CONSENT.store(crate::store::load_osc7_consent(), std::sync::atomic::Ordering::Relaxed);
}

/// 监控栏（左侧菜单栏）统一右键菜单：语言 / 字体大小 / 折叠视图 / 强制 X11。
/// 背景层与各子控件、以及折叠后的细条都调用它，保证右键处处一致。
/// 终端边框色：偏向终端底色 b 的加权混合（1/3 窗口色 a + 2/3 终端色 b）。
/// 比 50/50 更贴近终端——深色主题更深、浅色主题更接近自身底色，过渡更自然。
fn blend_color(a: egui::Color32, b: egui::Color32) -> egui::Color32 {
    let mix = |x: u8, y: u8| ((x as u16 + 2 * y as u16) / 3) as u8;
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

pub fn view_context_menu(resp: &egui::Response) {
    resp.context_menu(|ui| {
        // 菜单项不换行，避免较长英文项折行
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

        // —— 语言 ——
        ui.label(RichText::new(crate::i18n::tr("语言", "Language")).color(Palette::TEXT_DIM).size(11.0));
        crate::i18n::language_menu(ui);
        ui.separator();

        // —— 字体大小（全局界面缩放）——
        ui.label(RichText::new(format!("{}  {:.0}%", crate::i18n::tr("字体大小", "Font size"), ui_zoom() * 100.0)).color(Palette::TEXT_DIM).size(11.0));
        ui.horizontal(|ui| {
            // +/- 不关闭菜单，便于连续调整；百分比实时更新
            if ui.button(RichText::new(egui_phosphor::regular::MINUS).size(13.0)).clicked() {
                set_ui_zoom(ui_zoom() - 0.1);
            }
            if ui.button(RichText::new(egui_phosphor::regular::PLUS).size(13.0)).clicked() {
                set_ui_zoom(ui_zoom() + 0.1);
            }
            if ui.button(crate::i18n::tr("复位", "Reset")).clicked() {
                set_ui_zoom(1.0);
            }
        });
        ui.separator();

        // —— 视图折叠 ——
        let s_label = if sidebar_collapsed() {
            format!("{}  {}", egui_phosphor::regular::SIDEBAR_SIMPLE, crate::i18n::tr("显示系统监控栏", "Show monitor sidebar"))
        } else {
            format!("{}  {}", egui_phosphor::regular::SIDEBAR_SIMPLE, crate::i18n::tr("隐藏系统监控栏", "Hide monitor sidebar"))
        };
        if ui.button(s_label).clicked() {
            set_sidebar_collapsed(!sidebar_collapsed());
            ui.close();
        }
        let f_label = if files_collapsed() {
            format!("{}  {}", egui_phosphor::regular::TREE_VIEW, crate::i18n::tr("显示文件栏", "Show file panel"))
        } else {
            format!("{}  {}", egui_phosphor::regular::TREE_VIEW, crate::i18n::tr("隐藏文件栏", "Hide file panel"))
        };
        if ui.button(f_label).clicked() {
            set_files_collapsed(!files_collapsed());
            ui.close();
        }

        // —— 强制 X11（仅 Linux；修复 Wayland 下输入法）——
        #[cfg(target_os = "linux")]
        {
            ui.separator();
            let mut fx = crate::store::load_force_x11();
            if ui
                .checkbox(&mut fx, crate::i18n::tr("强制 X11（修复输入法·重启生效）", "Force X11 (fix IME · restart)"))
                .on_hover_text(crate::i18n::tr("Wayland 下输入法常失效；开启后下次启动改走 X11", "IME often fails on Wayland; enabling switches to X11 on next launch"))
                .clicked()
            {
                crate::store::save_force_x11(fx);
                ui.close();
            }
        }

        // —— 关于 ——
        ui.separator();
        if ui.button(format!("{}  {}", egui_phosphor::regular::INFO, crate::i18n::tr("关于 iShell", "About iShell"))).clicked() {
            set_about_open(true);
            ui.close();
        }
    });
}

/// 「关于」弹框：软件名 / 版本 / 主页 / 发布 / 许可 / 技术栈。版本号取自 Cargo.toml（编译期内嵌）。
fn about_window(ctx: &egui::Context) {
    if !about_open() {
        return;
    }
    let ver = env!("CARGO_PKG_VERSION"); // 编译期内嵌的 Cargo.toml 版本
    let mut open = true;
    // 版本号同时放进标题，确保一定可见
    let title = match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("关于 iShell  ·  v{ver}"),
        crate::i18n::Lang::En => format!("About iShell  ·  v{ver}"),
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open) // 标题栏 X 关闭
        .show(ctx, |ui| {
            ui.set_width(330.0);
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("iShell").size(28.0).strong().color(Palette::ACCENT));
                ui.label(RichText::new(crate::i18n::tr("现代化 Rust SSH 客户端", "A modern Rust SSH client")).size(12.0).color(Palette::TEXT_DIM));
                ui.add_space(8.0);
                // 版本行：强调色加大，显式着色，确保醒目可见
                ui.label(RichText::new(format!("{} {ver}", crate::i18n::tr("版本", "Version"))).size(16.0).strong().color(Palette::ACCENT));
                ui.add_space(6.0);
            });
            ui.separator();
            ui.add_space(6.0);
            egui::Grid::new("about_grid").num_columns(2).spacing([12.0, 7.0]).show(ui, |ui| {
                ui.label(RichText::new(crate::i18n::tr("项目主页", "Repository")).color(Palette::TEXT_DIM));
                ui.hyperlink_to("github.com/wqkx/ishell", "https://github.com/wqkx/ishell");
                ui.end_row();
                ui.label(RichText::new(crate::i18n::tr("下载发布", "Releases")).color(Palette::TEXT_DIM));
                ui.hyperlink_to(crate::i18n::tr("最新版本与各平台包", "Latest & platform builds"), "https://github.com/wqkx/ishell/releases");
                ui.end_row();
                ui.label(RichText::new(crate::i18n::tr("许可", "License")).color(Palette::TEXT_DIM));
                ui.label("MIT");
                ui.end_row();
                ui.label(RichText::new(crate::i18n::tr("技术栈", "Built with")).color(Palette::TEXT_DIM));
                ui.label("Rust · egui/eframe · russh");
                ui.end_row();
            });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if ui.add(egui::Button::new(crate::i18n::tr("关闭", "Close")).min_size(egui::vec2(80.0, 0.0))).clicked() {
                    set_about_open(false);
                }
            });
        });
    if !open {
        set_about_open(false);
    }
}

pub struct App {
    runtime: Arc<tokio::runtime::Runtime>,
    ctx: egui::Context,
    sessions: Vec<Session>,
    active: Option<usize>,
    /// 正在拖拽排序的标签源索引
    dragging_tab: Option<usize>,
    /// 拖拽起点在标签内的横向抓取偏移（让被拖标签跟手而不跳到光标处）
    tab_grab_dx: f32,
    /// 标签条总宽缓存（用于撑出横向滚动内容宽度）
    tab_total_w: f32,
    /// 请求把激活标签滚动到可视区（新建/点击/Ctrl+Tab 切换时置位）
    scroll_to_active: bool,
    /// 会话唯一 id 计数器（标签滑动动画用）
    next_uid: u64,
    connect_form: ConnectForm,
    /// 默认下载目录（可在传输窗中修改，持久化）
    download_dir: std::path::PathBuf,
    /// 传输列表的状态筛选（全部 / 进行中 / 已完成 / 失败）
    xfer_filter: XferFilter,
    /// 传输进度浮窗是否显示
    show_transfers: bool,
    /// 传输浮窗刚打开（本帧跳过"点击外部关闭"判定）
    xfer_just_opened: bool,
    /// 顶部浮层提示 (文案, 起始时刻)：用于撤销等需要醒目反馈的操作，数秒后自动淡出
    toast: Option<(String, f64)>,
    /// 显示"确认退出"对话框
    show_close_confirm: bool,
    /// 待确认关闭的标签（仅当该会话仍连接中时弹确认）
    pending_close_tab: Option<usize>,
    /// 已确认可以关闭
    allow_close: bool,
    /// 编辑器状态：放在 Arc<Mutex> 里，供 deferred viewport 回调（'static + Send + Sync，
    /// 无法借用 &mut self）与主 update() 共享。改 deferred 是为根治 macOS 多窗口闪烁
    /// （immediate viewport 与主窗口同帧渲染、强耦合焦点，触发 Stage Manager 不停重拍）。
    editor_state: Arc<Mutex<EditorState>>,
    /// 看图工具：已打开的图片标签
    image_tabs: Vec<ImageTab>,
    active_image: usize,
    /// 一次性请求：新开/切换后把看图窗口置前并聚焦
    image_focus: bool,
    /// 上次渲染时的激活看图标签（用于侦测切换后滚到可视区）
    image_shown: usize,
    /// 看图标签拖动重排状态（仿主窗口）
    img_tab_drag: Option<usize>,
    img_grab_dx: f32,
    img_total_w: f32,
    /// 下一个编辑器 TextEdit Id 序号
    next_editor_id: u64,
    /// 关闭大文件编辑器后延迟若干帧再 malloc_trim（等 galley 缓存被淘汰）
    trim_after: Option<u32>,
    /// 端口转发管理窗口是否显示
    show_forwards: bool,
    /// 转发浮窗刚打开（本帧跳过"点击外部关闭"判定）
    fwd_just_opened: bool,
    /// 待删除确认的转发 id（行内二次确认：点垃圾桶先武装，确认后才删）
    fwd_confirm_del: Option<u64>,
    /// 新增转发表单
    fwd_form: ForwardForm,
    /// 正在编辑的转发 id（Some=编辑模式，按钮变「保存」，提交时先删旧再加新）
    fwd_editing: Option<u64>,
    /// 转发表单的内联校验错误（端口占用/参数无效等），红字显示在表单下方
    fwd_error: Option<String>,
    /// 上一帧活动会话的 uid——切换会话时复位「跨会话易串台」的临时 UI 态（转发确认/编辑、进程弹窗）
    active_uid_prev: Option<u64>,
    /// 命令广播栏是否显示 + 输入内容
    show_broadcast: bool,
    broadcast_input: String,
    // 折叠监控栏/文件栏与界面缩放改为进程级全局状态（见本文件底部 view 状态），
    // 以便侧栏背景层与各子控件共用同一右键菜单。
    /// 传输冲突策略（目标已存在时；默认覆盖）
    conflict_policy: ConflictPolicy,
    /// 文件剪贴板（跨 tab 共享）：复制/剪切的源项
    file_clip: Option<FileClip>,
    /// 待确认的粘贴（剪切 或 跨服务器：执行前二次确认）
    pending_paste: Option<PendingPaste>,
    /// 跨服务器中转任务（下载→上传→可选删源）
    relays: Vec<Relay>,
    /// 中转临时目录去重计数
    relay_seq: u64,
    /// 粘贴确认弹框里「直传/中转」互斥选择的当前值（true=直传，默认）；每次打开确认时复位为直传
    confirm_direct: bool,
    /// 进行中的直传任务追踪（成功删源/刷新；失败弹回退）
    direct_jobs: Vec<DirectJob>,
    /// 直传失败、待确认「转中转」的计划 + 原因（队列：多个失败依次弹，避免同帧互相覆盖）
    pending_direct_fallback: Vec<DirectFallback>,
    /// 命令片段库（snippets）窗口 + 数据 + 编辑缓冲
    show_snippets: bool,
    /// 片段浮窗刚打开（本帧跳过"点击外部关闭"判定）
    snip_just_opened: bool,
    snippets: Vec<crate::store::Snippet>,
    /// 正在编辑的片段索引（None = 新建）+ 表单缓冲
    snip_editing: Option<usize>,
    snip_name: String,
    snip_cmd: String,
    snip_run: bool,
    /// 进程详情小窗
    proc_popup: Option<ProcPopup>,
    proc_popup_just_opened: bool,
    /// GPU 详情小窗（仅记录弹出位置，数据每帧从活动会话取）
    gpu_popup: Option<egui::Pos2>,
    gpu_popup_just_opened: bool,
    /// 自检：每帧注入假 GPU 数据并保持详情窗打开（仅截图核对用）
    demo_gpu: bool,
    /// 自检：注入网络曲线波形（仅截图核对密度用）
    demo_net: bool,
    /// 自检截图模式（由环境变量触发，正常使用时为 None）
    shot: Option<Shot>,
    /// Logo 生成模式（ISHELL_LOGO）：只画 logo 圆角矩形
    logo: bool,
}

/// 进程详情小窗状态。
struct ProcPopup {
    pid: u32,
    name: String,
    cpu: f32,
    mem: f32,
    pos: egui::Pos2,
    cmd: String,
    cwd: String,
    exe: String,
    /// 最近一次复制的时刻（ctx 时间，秒），用于短暂显示「已复制」
    copied_t: Option<f64>,
    /// 是否处于「强制结束」二次确认态：kill 按钮先置此标志，确认后才真正下发 KillProc
    confirm_kill: bool,
    /// 打开弹窗时所属会话的 uid——kill 据此定位会话，避免切会话后误 kill 别的主机
    uid: u64,
}

/// UI 侧的一条端口转发记录。
struct ForwardEntry {
    id: u64,
    label: String,
    status: String,
    ok: bool,
    /// 结构化参数：用于「编辑」时把该条回填到表单，以及本地端口占用检测。
    bind_host: String,
    bind_port: u16,
    kind: crate::proto::ForwardKind,
}

/// "新增转发"表单状态。
struct ForwardForm {
    /// 0 = 本地转发，1 = 动态 SOCKS5
    kind: usize,
    bind: String,
    local_port: String,
    target_host: String,
    target_port: String,
}

impl Default for ForwardForm {
    fn default() -> Self {
        Self {
            kind: 0,
            bind: "127.0.0.1".into(),
            local_port: String::new(),
            target_host: String::new(),
            target_port: String::new(),
        }
    }
}

/// 一个编辑器标签（含来源服务器，用于回写）。
struct EditorTab {
    editor: crate::ui::editor::Editor,
    /// 所属会话标题（仅用于标签显示）
    server: String,
    /// 所属会话稳定唯一 id（身份匹配用：保存/冲突/去重，避免同名会话串台）
    uid: u64,
    cmd_tx: UnboundedSender<UiCommand>,
    /// 该编辑器固定的 TextEdit Id（关闭时据此清理 egui 状态/撤销历史）
    text_id: egui::Id,
    /// 下载中关联的 ReadFile id（Some=加载中占位，None=已就绪）；以及下载进度
    load_id: Option<u64>,
    load_done: u64,
    load_total: u64,
    /// 保存时检测到文件被外部修改 → 显示冲突横幅
    save_conflict: bool,
    /// 保存动画起始时刻（ctx 时间，秒）：驱动标签底部珊瑚线「绿扫→珊瑚扫」表示已保存；None=无动画
    save_at: Option<f64>,
    /// 保存进行中（已发 WriteFile、未收到结果）：大文件保存耗时较长，期间屏蔽再次保存
    saving: bool,
    /// 保存写入进度（done/total 字节）：驱动绿扫跟随实际上传速度
    save_done: u64,
    save_total: u64,
    /// 绿扫完成、进入「珊瑚扫回」阶段的起始时刻（ctx 时间）；None=仍在绿扫阶段
    save_done_at: Option<f64>,
}

/// 编辑器窗口的共享状态（主窗口与 deferred viewport 回调共用，见 App::editor_state）。
#[derive(Default)]
struct EditorState {
    tabs: Vec<EditorTab>,
    /// 当前激活标签
    active: usize,
    /// 上次渲染的激活标签（用于切换后滚到可视区）
    shown: usize,
    /// 一次性请求：新开/切换后把编辑器窗口置前并聚焦
    focus: bool,
    /// 「关闭全部」时若有未保存修改，弹确认框
    close_confirm: bool,
    /// 关闭单个「脏」标签前的确认（标签索引）
    close_tab_confirm: Option<usize>,
    /// 关闭标签后请求主循环归还内存（trim）
    trim_request: bool,
    /// 标签拖动重排状态（仿主窗口）：拖动索引 / 抓取偏移 / 内容总宽缓存
    tab_drag: Option<usize>,
    tab_grab_dx: f32,
    tab_total_w: f32,
}

impl EditorState {
    /// 关闭全部标签并清理各自的 TextEdit 内存状态；请求 trim。
    fn close_all(&mut self, ctx: &egui::Context) {
        for tab in self.tabs.drain(..) {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(tab.text_id));
        }
        self.active = 0;
        self.close_confirm = false;
        self.trim_request = true;
    }
}

/// 看图工具的一个标签页（一张已加载的图片）。
struct ImageTab {
    /// 所属会话标题（仅显示）
    server: String,
    /// 所属会话稳定唯一 id（身份匹配用）
    uid: u64,
    path: String,
    tex: egui::TextureHandle,
    /// 原始字节（用于「另存为」，保留源格式/质量）
    data: Vec<u8>,
    /// 原始像素尺寸
    size: egui::Vec2,
    /// 缩放系数；0 表示「首帧自动适应窗口」
    zoom: f32,
    /// 平移偏移（像素）
    offset: egui::Vec2,
}

/// 自检截图状态。
struct Shot {
    path: String,
    deadline: std::time::Instant,
    requested: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply(&cc.egui_ctx);
        // 载入已保存的界面缩放到全局视图状态
        init_view_state();
        // 载入已保存语言（默认中文）
        if let Some(code) = crate::store::load_lang() {
            crate::i18n::set(crate::i18n::Lang::from_code(&code));
        }
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("无法创建 tokio 运行时"),
        );
        let mut form = ConnectForm::default();
        form.open = true; // 启动即弹出连接框

        let shot = std::env::var("ISHELL_SHOT").ok().map(|path| {
            let secs: u64 = std::env::var("ISHELL_SHOT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
            Shot {
                path,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(secs),
                requested: false,
            }
        });

        let mut app = Self {
            runtime,
            ctx: cc.egui_ctx.clone(),
            sessions: Vec::new(),
            active: None,
            dragging_tab: None,
            tab_grab_dx: 0.0,
            tab_total_w: 1.0,
            scroll_to_active: false,
            next_uid: 0,
            connect_form: form,
            download_dir: crate::store::load_download_dir().map(std::path::PathBuf::from).unwrap_or_else(downloads_dir),
            xfer_filter: XferFilter::All,
            show_transfers: false,
            xfer_just_opened: false,
            toast: None,
            show_close_confirm: false,
            pending_close_tab: None,
            allow_close: false,
            editor_state: Arc::new(Mutex::new(EditorState::default())),
            image_tabs: Vec::new(),
            active_image: 0,
            image_focus: false,
            image_shown: 0,
            img_tab_drag: None,
            img_grab_dx: 0.0,
            img_total_w: 0.0,
            next_editor_id: 0,
            trim_after: None,
            show_forwards: false,
            fwd_just_opened: false,
            fwd_confirm_del: None,
            fwd_form: ForwardForm::default(),
            fwd_editing: None,
            fwd_error: None,
            active_uid_prev: None,
            show_broadcast: false,
            broadcast_input: String::new(),
            conflict_policy: crate::store::load_conflict_policy().map(|s| ConflictPolicy::from_str(&s)).unwrap_or(ConflictPolicy::Overwrite),
            file_clip: None,
            pending_paste: None,
            relays: Vec::new(),
            relay_seq: 0,
            confirm_direct: true,
            direct_jobs: Vec::new(),
            pending_direct_fallback: Vec::new(),
            show_snippets: false,
            snip_just_opened: false,
            snippets: crate::store::load_snippets(),
            snip_editing: None,
            snip_name: String::new(),
            snip_cmd: String::new(),
            snip_run: true,
            proc_popup: None,
            proc_popup_just_opened: false,
            gpu_popup: None,
            gpu_popup_just_opened: false,
            demo_gpu: std::env::var("ISHELL_DEMO_GPU").is_ok(),
            demo_net: std::env::var("ISHELL_DEMO_NET").is_ok(),
            shot,
            logo: std::env::var("ISHELL_LOGO").is_ok() || std::env::var("ISHELL_ICON").is_ok(),
        };

        // 自检：自动连接（格式 host|port|user|keypath），免去手动登录
        if let Ok(spec) = std::env::var("ISHELL_AUTOCONNECT") {
            let parts: Vec<&str> = spec.split('|').collect();
            if parts.len() == 4 {
                if let Ok(port) = parts[1].parse() {
                    app.connect_form.open = false;
                    app.spawn_session(ConnectConfig {
                        host: parts[0].into(),
                        port,
                        username: parts[2].into(),
                        auth: if parts[3] == "agent" { AuthMethod::Agent } else { AuthMethod::KeyFile { path: parts[3].into(), passphrase: None } },
                        label: String::new(),
                        // 自检：ISHELL_JUMP="host|port|user|key" 时经跳板机连接
                        jump: std::env::var("ISHELL_JUMP").ok().and_then(|s| {
                            let p: Vec<&str> = s.split('|').collect();
                            (p.len() == 4).then(|| crate::proto::JumpHost {
                                host: p[0].into(),
                                port: p[1].parse().unwrap_or(22),
                                username: p[2].into(),
                                auth: AuthMethod::KeyFile { path: p[3].into(), passphrase: None },
                            })
                        }),
                        forward_agent: false,
                    });
                }
            }
        }

        // 自检：直接打开新建表单（截图核对输入框样式）
        if std::env::var("ISHELL_DEMO_FORM").is_ok() {
            app.connect_form.open_form_for_demo();
        }
        // 自检：打开快速连接列表（截图核对导入按钮）
        if std::env::var("ISHELL_DEMO_CONN").is_ok() {
            app.connect_form.open_dialog();
        }
        if std::env::var("ISHELL_DEMO_IMPORT").is_ok() {
            app.connect_form.open_import_demo();
        }
        if std::env::var("ISHELL_DEMO_DELETE").is_ok() {
            app.connect_form.open_delete_demo();
        }
        if std::env::var("ISHELL_DEMO_LIST").is_ok() {
            app.connect_form.open_list_demo();
        }
        // 自检：注入演示编辑器内容（截图核对代码高亮 + 多标签）
        if std::env::var("ISHELL_DEMO_EDIT").is_ok() {
            if let Some((server, uid, tx)) = app.sessions.first().map(|s| (s.title.clone(), s.uid, s.cmd_tx.clone())) {
                let code = "use std::io;\n\n// 示例：读取并打印\nfn main() {\n    let mut s = String::new();\n    io::stdin().read_line(&mut s).unwrap();\n    let n: i32 = s.trim().parse().unwrap_or(0);\n    for i in 0..n {\n        println!(\"line {}\", i);\n    }\n}\n".to_string();
                let t1 = app.alloc_editor_id();
                // 大文件（>1MB）→ 只读模式，核对「改为可编辑」按钮
                let big: String = (0..40000).map(|i| format!("{i}: the quick brown fox jumps over the lazy dog\n")).collect();
                let t2 = app.alloc_editor_id();
                let t3 = app.alloc_editor_id();
                let mut ed = app.editor_state.lock().unwrap();
                ed.tabs.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/home/e5-1/demo.rs".into(), code),
                    server: server.clone(),
                    uid,
                    cmd_tx: tx.clone(),
                    text_id: t1,
                    load_id: None,
                    load_done: 0,
            load_total: 0,
            save_conflict: false,
            save_at: None,
            saving: false,
            save_done: 0,
            save_total: 0,
            save_done_at: None,
                });
                ed.tabs.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/var/log/huge.log".into(), big),
                    server: server.clone(),
                    uid,
                    cmd_tx: tx.clone(),
                    text_id: t2,
                    load_id: None,
                    load_done: 0,
            load_total: 0,
            save_conflict: false,
            save_at: None,
            saving: false,
            save_done: 0,
            save_total: 0,
            save_done_at: None,
                });
                ed.tabs.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/etc/hosts".into(), "127.0.0.1 localhost\n::1 localhost\n".into()),
                    server,
                    uid,
                    cmd_tx: tx,
                    text_id: t3,
                    load_id: None,
                    load_done: 0,
            load_total: 0,
            save_conflict: false,
            save_at: None,
            saving: false,
            save_done: 0,
            save_total: 0,
            save_done_at: None,
                });
                ed.active = 1; // 默认显示大文件标签
            }
        }

        // 自检：看图工具——合成一张彩色渐变图打开
        if std::env::var("ISHELL_DEMO_IMG").is_ok() {
            if let Some((server, uid)) = app.sessions.first().map(|s| (s.title.clone(), s.uid)) {
                let (w, h) = (240usize, 160usize);
                let mut px = vec![0u8; w * h * 4];
                for y in 0..h {
                    for x in 0..w {
                        let i = (y * w + x) * 4;
                        px[i] = (x * 255 / w) as u8;
                        px[i + 1] = (y * 255 / h) as u8;
                        px[i + 2] = 128;
                        px[i + 3] = 255;
                    }
                }
                let color = egui::ColorImage::from_rgba_unmultiplied([w, h], &px);
                let tex = cc.egui_ctx.load_texture("demo_img", color, egui::TextureOptions::LINEAR);
                let mut data = Vec::new();
                if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, px) {
                    let _ = image::DynamicImage::ImageRgba8(buf).write_to(&mut std::io::Cursor::new(&mut data), image::ImageFormat::Png);
                }
                app.image_tabs.push(ImageTab {
                    server,
                    uid,
                    path: "/home/e5-1/pic/gradient.png".into(),
                    tex,
                    data,
                    size: egui::vec2(w as f32, h as f32),
                    zoom: 0.0,
                    offset: egui::Vec2::ZERO,
                });
            }
        }

        // 自检：自动建立一条本地转发（127.0.0.1:18022 → 127.0.0.1:22）
        if std::env::var("ISHELL_DEMO_FORWARD").is_ok() {
            use crate::proto::{ForwardKind, ForwardSpec};
            if let Some(s) = app.sessions.first_mut() {
                let id = s.next_forward;
                s.next_forward += 1;
                s.forwards.push(ForwardEntry {
                    id,
                    label: "127.0.0.1:18022 → 127.0.0.1:22".into(),
                    status: crate::i18n::tr("启动中 …", "Starting …").into(),
                    ok: true,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 18022,
                    kind: ForwardKind::Local { remote_host: "127.0.0.1".into(), remote_port: 22 },
                });
                let _ = s.cmd_tx.send(UiCommand::AddForward(ForwardSpec {
                    id,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 18022,
                    kind: ForwardKind::Local { remote_host: "127.0.0.1".into(), remote_port: 22 },
                }));
            }
            app.show_forwards = true;
        }

        // 自检：进程详情小窗
        if std::env::var("ISHELL_DEMO_PROC").is_ok() {
            app.proc_popup = Some(ProcPopup {
                pid: 1234,
                name: "gromacs_mpi".into(),
                cpu: 98.5,
                mem: 12.3,
                pos: egui::pos2(150.0, 300.0),
                cmd: "/opt/gromacs/bin/gmx mdrun -deffnm md -nb gpu".into(),
                cwd: "/home/e5-1/sim/run1".into(),
                exe: "/opt/gromacs/bin/gmx".into(),
                copied_t: None,
                confirm_kill: false,
                uid: app.sessions.first().map(|s| s.uid).unwrap_or(0),
            });
        }

        // 自检：生成多个标签，核对溢出渐隐 + 固定的「新建」按钮
        if std::env::var("ISHELL_DEMO_TABS").is_ok() {
            for n in 1..=12 {
                app.spawn_session(ConnectConfig {
                    host: "127.0.0.1".into(),
                    port: 9,
                    username: format!("srv-{n:02}"),
                    auth: AuthMethod::Password(String::new()),
                    label: String::new(),
                    jump: None,
                    forward_agent: false,
                });
            }
        }

        // 自检：命令广播栏
        if std::env::var("ISHELL_DEMO_BCAST").is_ok() {
            app.show_broadcast = true;
            app.broadcast_input = "systemctl status nginx".into();
        }

        // 自检：显示退出确认框
        if std::env::var("ISHELL_DEMO_CLOSE").is_ok() {
            app.show_close_confirm = true;
        }

        // 自检：注入演示传输条目，便于截图核对传输浮窗
        if std::env::var("ISHELL_DEMO_XFER").is_ok() {
            if let Some(s) = app.sessions.first_mut() {
                use crate::proto::TransferDir::*;
                let mut demo = |id, name: &str, dir, done, total, ok, local: Option<String>| {
                    let mut t = Transfer::new(id, name.into(), dir, total, local, None);
                    t.done = done;
                    t.ok = ok;
                    s.transfers.push(t);
                };
                demo(1, "backup.tar.gz", Download, 73_400_320, 104_857_600, None, None);
                demo(2, "deploy.sh", Upload, 2048, 2048, Some(true), None);
                demo(3, "huge.bin", Download, 1024, 2048, Some(true), Some("/root/Downloads/huge.bin".into()));
                // 自检：再塞一批，验证滚动
                for i in 4..16u64 {
                    demo(i, &format!("file_{i}.dat"), Download, i * 1000, 20000, if i % 3 == 0 { Some(true) } else { None }, None);
                }
            }
            app.show_transfers = true;
        }
        app
    }

    /// 根据配置建立一个新会话（spawn worker）。
    /// 分配一个唯一的编辑器 TextEdit Id。
    fn alloc_editor_id(&mut self) -> egui::Id {
        let id = egui::Id::new(("ed_txt", self.next_editor_id));
        self.next_editor_id += 1;
        id
    }

    /// 创建通道并在运行时启动一个 worker，返回 (cmd_tx, evt_rx, hostkey_tx)。
    fn spawn_worker(
        &self,
        cfg: ConnectConfig,
    ) -> (UnboundedSender<UiCommand>, std::sync::mpsc::Receiver<WorkerEvent>, UnboundedSender<bool>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let (hostkey_tx, hostkey_rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = UiSink::new(evt_tx, self.ctx.clone());
        self.runtime.spawn(ssh::run(cfg, cmd_rx, sink, hostkey_rx));
        (cmd_tx, evt_rx, hostkey_tx)
    }

    fn spawn_session(&mut self, cfg: ConnectConfig) {
        self.show_close_confirm = false; // 新建会话则取消退出提示
        let (cmd_tx, evt_rx, hostkey_tx) = self.spawn_worker(cfg.clone());

        self.next_uid += 1;
        self.sessions.push(Session {
            uid: self.next_uid,
            title: if cfg.label.trim().is_empty() { cfg.username.clone() } else { cfg.label.trim().to_string() },
            tip: format!("{}@{}:{}", cfg.username, cfg.host, cfg.port),
            cmd_tx,
            evt_rx,
            connected: false,
            status: crate::i18n::tr("连接中 …", "Connecting …").into(),
            terminal: Terminal::new(),
            sysinfo: None,
            net_hist: NetHistory::default(),
            files: {
                let key = format!("{}@{}:{}", cfg.username, cfg.host, cfg.port);
                FilePanelState {
                    favorites: crate::store::load_favorites(&key),
                    server_key: key,
                    ..Default::default()
                }
            },
            last_size: (0, 0),
            initialized: false,
            transfers: Vec::new(),
            next_xfer: 1,
            selected_nic: String::new(),
            proc_sort_mem: false,
            pending_open: Vec::new(),
            pending_saved: Vec::new(),
            pending_save_progress: Vec::new(),
            pending_conflict: Vec::new(),
            pending_too_large: Vec::new(),
            pending_placeholder: Vec::new(),
            pending_load_progress: Vec::new(),
            pending_load_fail: Vec::new(),
            pending_image: Vec::new(),
            hostkey_tx,
            pending_hostkey: None,
            kbd_prompt: None,
            forwards: Vec::new(),
            next_forward: 1,
            proc_detail: None,
            cfg,
            was_connected: false,
            reconnect_at: None,
            reconnect_tries: 0,
            last_cwd: String::new(),
            restore_cwd: false,
            osc7_confirm: false,
            osc7_pending_reveal: false,
        });
        self.active = Some(self.sessions.len() - 1);
        self.scroll_to_active = true; // 新建标签后滚动到可视区
    }

    /// 重连指定会话：用原配置重启 worker，重置连接相关状态，保留标签/目录等。
    fn reconnect_session(&mut self, idx: usize) {
        let Some(s) = self.sessions.get(idx) else { return };
        let cfg = s.cfg.clone();
        let (cmd_tx, evt_rx, hostkey_tx) = self.spawn_worker(cfg);
        let Some(s) = self.sessions.get_mut(idx) else { return };
        let uid = s.uid;
        s.cmd_tx = cmd_tx.clone();
        s.evt_rx = evt_rx;
        s.hostkey_tx = hostkey_tx;
        s.connected = false;
        s.initialized = false;
        s.terminal = Terminal::new();
        s.sysinfo = None;
        // M3：保留端口转发（不再 clear），标记「重连中」；Connected 事件里用新 worker 重建
        for f in &mut s.forwards {
            f.ok = true;
            f.status = crate::i18n::tr("重连中 …", "Reconnecting …").into();
        }
        s.pending_hostkey = None;
        s.kbd_prompt = None;
        s.reconnect_at = None;
        s.restore_cwd = true; // 重连成功后尝试 cd 回 last_cwd（保留不清空）
        s.status = crate::i18n::tr("重连中 …", "Reconnecting …").into();
        // M1：刷新该会话已打开编辑器标签的 cmd_tx——旧句柄随 worker 失效，否则重连后保存静默丢失。
        if let Ok(mut es) = self.editor_state.lock() {
            for t in es.tabs.iter_mut().filter(|t| t.uid == uid) {
                t.cmd_tx = cmd_tx.clone();
            }
        }
    }

    /// 拖动排序：把会话从 `from` 移动到放置目标 `to` 处。
    fn reorder_session(&mut self, from: usize, to: usize) {
        let len = self.sessions.len();
        if from >= len || to >= len || from == to {
            return;
        }
        let moved = self.sessions.remove(from);
        // 让被拖动标签落在放置目标的原始位置 `to`（双向一致，避免相邻正向拖动变成空操作）
        let dest = to.min(self.sessions.len());
        self.sessions.insert(dest, moved);
        // 重算当前激活索引
        self.active = self.active.map(|a| {
            if a == from {
                dest
            } else {
                let mut x = a;
                if a > from {
                    x -= 1;
                }
                if x >= dest {
                    x += 1;
                }
                x
            }
        });
    }

    fn close_session(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        let s = self.sessions.remove(idx);
        let _ = s.cmd_tx.send(UiCommand::Disconnect);
        if self.sessions.is_empty() {
            self.active = None;
        } else {
            // 据「关闭项」与「当前 active」的相对位置正确调整，避免关闭非激活标签时误切会话：
            // 关在 active 左侧 → active 左移一位；关在右侧 → 不变；关的正是 active（或无 active）→ 落到邻近项。
            let new_len = self.sessions.len();
            self.active = Some(match self.active {
                Some(a) if a > idx => a - 1,
                Some(a) if a < idx => a,
                _ => idx.min(new_len - 1),
            });
        }
    }

    /// 切换会话标签（delta=+1 下一个 / -1 上一个，循环）。
    fn switch_tab(&mut self, delta: i32) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let cur = self.active.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        self.active = Some(next);
        self.scroll_to_active = true; // 切换后滚动到可视区
        if let Some(s) = self.sessions.get_mut(next) {
            s.terminal.request_focus();
        }
    }

    /// 翻译文件面板动作为 SFTP 指令或剪贴板操作。
    fn handle_file_action(&mut self, idx: usize, action: FileAction) {
        let policy = self.conflict_policy;
        // 剪贴板 / 粘贴需同时访问 App 级剪贴板与会话信息，单独前置处理
        match action {
            FileAction::ClipCopy { items } => return self.set_clip(idx, items, false),
            FileAction::ClipCut { items } => return self.set_clip(idx, items, true),
            FileAction::Paste { dest_dir } => return self.start_paste(idx, dest_dir),
            _ => {}
        }
        let Some(s) = self.sessions.get_mut(idx) else { return };
        match action {
            FileAction::List(path) => {
                let p = if path == "~" || path.is_empty() { ".".into() } else { path };
                let _ = s.cmd_tx.send(UiCommand::ListDir(p));
            }
            FileAction::Download(remote) => {
                let name = remote.rsplit('/').next().unwrap_or("download").to_string();
                let local = self.download_dir.join(&name).to_string_lossy().into_owned();
                // 去重：同一远端文件 → 同一本地路径 的任务已存在时复用它，避免重复任务，
                // 也顺带恢复此前失败/中断的同一传输（复用 id：worker 据本地已有分段续传，
                // 并通过覆盖取消句柄停掉可能仍在后台运行的旧任务）。
                let existing = s.transfers.iter().find(|t| {
                    t.ok != Some(true) // 已成功完成的不复用（本地可能已变，避免按旧偏移续传出错）
                        && matches!(&t.spec, Some(XferSpec::Download { remote: r, local: l }) if *r == remote && *l == local)
                }).map(|t| (t.id, t.ok, t.paused));
                match existing {
                    Some((_, None, false)) => {
                        s.status = match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{name} 正在下载中"),
                            crate::i18n::Lang::En => format!("{name} is already downloading"),
                        };
                    }
                    Some((id, _, _)) => {
                        let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy });
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None; t.paused = false; t.show_err = false;
                            t.speed = 0.0; t.last_done = 0; t.last_t = None;
                            t.message = crate::i18n::tr("重新下载 …", "Re-downloading …").into();
                        }
                    }
                    None => {
                        let id = s.next_xfer;
                        s.next_xfer += 1;
                        s.transfers.push(Transfer::new(
                            id, name, crate::proto::TransferDir::Download, 0, Some(local.clone()),
                            Some(XferSpec::Download { remote: remote.clone(), local: local.clone() }),
                        ));
                        let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy });
                    }
                }
                self.show_transfers = true;
                self.xfer_just_opened = true;
            }
            FileAction::Upload { local, remote_dir } => {
                // 同时按 / 和 \ 取名，兼容 Windows 路径（否则显示带盘符的整段路径）
                let name = local.rsplit(['/', '\\']).next().unwrap_or("upload").to_string();
                // 去重：同一本地文件 → 同一远端目录 的任务已存在时复用它（理由同 Download）。
                // 这是「上传中途失败/中断后再次上传出现两个任务、旧任务又续传」的根因修复。
                let existing = s.transfers.iter().find(|t| {
                    t.ok != Some(true) // 已成功完成的不复用（本地可能已变，避免按旧偏移续传出错）
                        && matches!(&t.spec, Some(XferSpec::Upload { local: l, remote_dir: r }) if *l == local && *r == remote_dir)
                }).map(|t| (t.id, t.ok, t.paused));
                match existing {
                    Some((_, None, false)) => {
                        // 已在进行中：忽略重复请求，仅提示并打开传输窗
                        s.status = match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{name} 正在上传中"),
                            crate::i18n::Lang::En => format!("{name} is already uploading"),
                        };
                    }
                    Some((id, _, _)) => {
                        // 失败/中断/已完成：复用该任务重新上传（同 id，自动续传/覆盖）
                        let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy });
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None; t.paused = false; t.show_err = false;
                            t.speed = 0.0; t.last_done = 0; t.last_t = None;
                            t.message = crate::i18n::tr("重新上传 …", "Re-uploading …").into();
                        }
                    }
                    None => {
                        let id = s.next_xfer;
                        s.next_xfer += 1;
                        s.transfers.push(Transfer::new(
                            id, name, crate::proto::TransferDir::Upload, 0, None,
                            Some(XferSpec::Upload { local: local.clone(), remote_dir: remote_dir.clone() }),
                        ));
                        let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy });
                    }
                }
                self.show_transfers = true;
                self.xfer_just_opened = true;
            }
            FileAction::Mkdir(path) => {
                let _ = s.cmd_tx.send(UiCommand::Mkdir(path));
            }
            FileAction::CreateFile(path) => {
                let _ = s.cmd_tx.send(UiCommand::CreateFile(path));
            }
            FileAction::Chmod { path, mode } => {
                let _ = s.cmd_tx.send(UiCommand::Chmod { path, mode });
            }
            FileAction::DeleteMany(paths) => {
                let _ = s.cmd_tx.send(UiCommand::DeleteMany { paths });
            }
            FileAction::Rename { from, to } => {
                let _ = s.cmd_tx.send(UiCommand::Rename { from, to });
            }
            FileAction::CopyPath(p) => {
                self.ctx.copy_text(p.clone());
                s.status = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已复制路径：{p}"), crate::i18n::Lang::En => format!("Copied: {p}") };
            }
            FileAction::OpenFile { path, force } => {
                s.status = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开中：{path} …"), crate::i18n::Lang::En => format!("Opening: {path} …") };
                let id = s.next_xfer;
                s.next_xfer += 1;
                // 立即建占位标签（显示文件名 + 进度条），下载完成后由 FileOpened 填充内容
                s.pending_placeholder.push((id, path.clone()));
                let _ = s.cmd_tx.send(UiCommand::ReadFile { id, path, force });
            }
            FileAction::OpenImage { path } => {
                s.status = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("打开中：{path} …"), crate::i18n::Lang::En => format!("Opening: {path} …") };
                let _ = s.cmd_tx.send(UiCommand::ReadImage { path });
            }
            FileAction::Move { srcs, dest_dir } => {
                // 同会话内拖拽移动：直接走远端 mv（CopyMove 的 do_move 分支）
                let n = srcs.len();
                let _ = s.cmd_tx.send(UiCommand::CopyMove { srcs, dest_dir, do_move: true });
                s.status = match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("移动 {n} 项 …"),
                    crate::i18n::Lang::En => format!("Moving {n} item(s) …"),
                };
            }
            FileAction::Status(msg) => {
                // 状态栏留底 + 顶部醒目浮层（撤销等操作需要明确反馈，避免误操作）
                let now = self.ctx.input(|i| i.time);
                s.status = msg.clone();
                self.toast = Some((msg, now));
            }
            FileAction::CdTerminal(path) => {
                // 以 POSIX 单引号转义路径后在终端 cd，并聚焦终端
                let quoted = format!("'{}'", path.replace('\'', "'\\''"));
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("cd {quoted}\r").into_bytes()));
                s.terminal.request_focus();
            }
            // 已在函数开头前置处理并 return，此处仅为穷尽匹配
            FileAction::ClipCopy { .. } | FileAction::ClipCut { .. } | FileAction::Paste { .. } => {}
        }
    }

    fn session_idx_by_uid(&self, uid: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.uid == uid)
    }

    /// 与指定会话「同一台服务器」（host:port 相同）的所有会话下标，活动会话排在最前。
    /// 用于把多个标签页对同一服务器的传输任务汇总到同一个传输列表里。
    fn same_server_idxs(&self, idx: usize) -> Vec<usize> {
        let Some(base) = self.sessions.get(idx) else { return Vec::new() };
        let (host, port) = (base.cfg.host.clone(), base.cfg.port);
        let mut out = vec![idx];
        for (i, s) in self.sessions.iter().enumerate() {
            if i != idx && s.cfg.host == host && s.cfg.port == port {
                out.push(i);
            }
        }
        out
    }

    /// 复制 / 剪切选中项到 App 级剪贴板（跨 tab 共享）。
    fn set_clip(&mut self, idx: usize, items: Vec<(String, bool)>, is_cut: bool) {
        let (uid, host, port, label) = match self.sessions.get(idx) {
            Some(s) => (s.uid, s.cfg.host.clone(), s.cfg.port, s.title.clone()),
            None => return,
        };
        let n = items.len();
        self.file_clip = Some(FileClip { items, is_cut, src_uid: uid, src_host: host, src_port: port, src_label: label });
        if let Some(s) = self.sessions.get_mut(idx) {
            s.status = match (is_cut, crate::i18n::current()) {
                (true, crate::i18n::Lang::Zh) => format!("已剪切 {n} 项（粘贴时移动）"),
                (false, crate::i18n::Lang::Zh) => format!("已复制 {n} 项到剪贴板"),
                (true, crate::i18n::Lang::En) => format!("Cut {n} item(s)"),
                (false, crate::i18n::Lang::En) => format!("Copied {n} item(s)"),
            };
        }
    }

    /// 粘贴到目标目录：同机直接 cp/mv；剪切或跨服务器需先二次确认。
    fn start_paste(&mut self, idx: usize, dest_dir: String) {
        let Some(clip) = self.file_clip.as_ref() else { return };
        let Some(dest) = self.sessions.get(idx) else { return };
        let cross = clip.src_host != dest.cfg.host || clip.src_port != dest.cfg.port;
        let src_dir = clip.items.first().map(|(p, _)| parent_dir(p)).unwrap_or_default();
        let plan = PendingPaste {
            items: clip.items.clone(),
            is_cut: clip.is_cut,
            cross,
            src_uid: clip.src_uid,
            dest_uid: dest.uid,
            src_dir,
            dest_dir,
            src_label: clip.src_label.clone(),
            dest_label: dest.title.clone(),
            direct: false, // 传输方式由确认弹框里的互斥选择决定
        };
        // 仅「跨服务器」需执行前确认（重操作 + 选直传/中转）；同机无论复制还是移动都直接执行——
        // 同机移动是原子 mv，源在目标写成功前不会丢，无需二次确认。
        if plan.cross {
            self.confirm_direct = true; // 每次打开确认默认「直传」
            self.pending_paste = Some(plan);
        } else {
            self.execute_paste(plan);
        }
    }

    /// 真正执行粘贴：同机服务器端 cp/mv；跨机建中转任务（下载→上传）。
    fn execute_paste(&mut self, plan: PendingPaste) {
        let is_cut = plan.is_cut; // 提前取出：plan 在直传分支会被移动
        if !plan.cross {
            let srcs: Vec<String> = plan.items.iter().map(|(p, _)| p.clone()).collect();
            if let Some(di) = self.session_idx_by_uid(plan.dest_uid) {
                let n = srcs.len();
                let s = &mut self.sessions[di];
                let _ = s.cmd_tx.send(UiCommand::CopyMove { srcs, dest_dir: plan.dest_dir.clone(), do_move: plan.is_cut });
                s.status = match (plan.is_cut, crate::i18n::current()) {
                    (true, crate::i18n::Lang::Zh) => format!("移动 {n} 项 …"),
                    (false, crate::i18n::Lang::Zh) => format!("复制 {n} 项 …"),
                    (true, crate::i18n::Lang::En) => format!("Moving {n} …"),
                    (false, crate::i18n::Lang::En) => format!("Copying {n} …"),
                };
            }
        } else if plan.direct {
            self.execute_direct(plan);
        } else {
            // 跨服务器中转：源会话与目标会话都须在线
            let Some(di) = self.session_idx_by_uid(plan.dest_uid) else { return };
            let Some(si) = self.session_idx_by_uid(plan.src_uid) else {
                self.sessions[di].status = crate::i18n::tr("源会话已关闭，无法跨服务器粘贴", "Source session closed; cannot paste across servers").into();
                return;
            };
            for (src_path, is_dir) in &plan.items {
                let base = src_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("item").to_string();
                self.relay_seq += 1;
                let tmp = std::env::temp_dir()
                    .join("ishell-relay")
                    .join(format!("{}-{}", std::process::id(), self.relay_seq))
                    .join(&base);
                if let Some(parent) = tmp.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let dlid = {
                    let s = &mut self.sessions[si];
                    let id = s.next_xfer;
                    s.next_xfer += 1;
                    let _ = s.cmd_tx.send(UiCommand::Download { id, remote: src_path.clone(), local: tmp.to_string_lossy().into_owned(), policy: ConflictPolicy::Overwrite });
                    id
                };
                // 在目标会话预占一条「等待」上传占位行：源端下载期间 B 也有可见状态
                let up_id = {
                    let s = &mut self.sessions[di];
                    let id = s.next_xfer;
                    s.next_xfer += 1;
                    let mut t = Transfer::new(id, base.clone(), crate::proto::TransferDir::Upload, 0, None, None);
                    t.queued = true;
                    t.note = crate::i18n::tr("等待源端下载…", "Waiting for source download…").into();
                    s.transfers.push(t);
                    id
                };
                self.relays.push(Relay {
                    src_path: src_path.clone(),
                    is_dir: *is_dir,
                    src_uid: plan.src_uid,
                    dest_uid: plan.dest_uid,
                    dest_dir: plan.dest_dir.clone(),
                    is_cut: plan.is_cut,
                    tmp,
                    phase: RelayPhase::Down(dlid),
                    up_id,
                });
            }
            self.show_transfers = true;
            self.xfer_just_opened = true;
            let n = plan.items.len();
            self.sessions[di].status = match (plan.is_cut, crate::i18n::current()) {
                (true, crate::i18n::Lang::Zh) => format!("跨服务器移动 {n} 项（经本地中转）…"),
                (false, crate::i18n::Lang::Zh) => format!("跨服务器复制 {n} 项（经本地中转）…"),
                (true, crate::i18n::Lang::En) => format!("Cross-server move {n} (via local relay) …"),
                (false, crate::i18n::Lang::En) => format!("Cross-server copy {n} (via local relay) …"),
            };
        }
        // 剪切粘贴后清空剪贴板（复制保留，便于多次粘贴）
        if is_cut {
            self.file_clip = None;
        }
    }

    /// 删除中转临时文件/目录及其空的任务目录。
    fn cleanup_relay_tmp(tmp: &std::path::Path, is_dir: bool) {
        if is_dir {
            let _ = std::fs::remove_dir_all(tmp);
        } else {
            let _ = std::fs::remove_file(tmp);
        }
        if let Some(parent) = tmp.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    /// 查询某会话某传输的完成状态（None=进行中/未知）。
    fn transfer_ok(&self, uid: u64, id: u64) -> Option<bool> {
        let si = self.session_idx_by_uid(uid)?;
        self.sessions[si].transfers.iter().find(|t| t.id == id).and_then(|t| t.ok)
    }

    /// 取消目标解析：若 (uid,id) 命中某直传任务（源行或目标镜像行），标记该任务「已取消」
    /// 并返回源端真实传输 (src_uid, id)；否则原样返回。使镜像行的取消也能真正生效，
    /// 且用 cancelled 标记替代脆弱的取消文案比对（见 process_direct_jobs）。
    fn cancel_target(&mut self, uid: u64, id: u64) -> (u64, u64) {
        if let Some(j) = self
            .direct_jobs
            .iter_mut()
            .find(|j| (j.src_uid == uid && j.id == id) || (j.dest_uid == uid && j.mir_id == id))
        {
            j.cancelled = true;
            (j.src_uid, j.id)
        } else {
            (uid, id)
        }
    }

    /// 查询某会话某传输的 (已传, 总量)（用于把源端下载进度反映到目标端「等待」行）。
    fn transfer_done_total(&self, uid: u64, id: u64) -> Option<(u64, u64)> {
        let si = self.session_idx_by_uid(uid)?;
        self.sessions[si].transfers.iter().find(|t| t.id == id).map(|t| (t.done, t.total))
    }

    /// 把目标会话里预占的「等待」上传占位行标记为失败（源端下载失败/源会话关闭时）。
    fn fail_placeholder(&mut self, dest_uid: u64, up_id: u64, msg: &str) {
        if let Some(di) = self.session_idx_by_uid(dest_uid) {
            if let Some(t) = self.sessions[di].transfers.iter_mut().find(|t| t.id == up_id) {
                t.ok = Some(false);
                t.queued = false;
                t.note = String::new();
                t.message = msg.to_string();
            }
        }
    }

    /// 推进跨服务器中转任务：下载完成→发起上传；上传完成→（剪切则删源）+ 清理临时。
    fn process_relays(&mut self) {
        let mut i = 0;
        while i < self.relays.len() {
            enum Step { Wait, ToUpload, Done, Failed }
            let step = {
                let r = &self.relays[i];
                match r.phase {
                    RelayPhase::Down(id) => match self.transfer_ok(r.src_uid, id) {
                        Some(true) => Step::ToUpload,
                        Some(false) => Step::Failed,
                        None if self.session_idx_by_uid(r.src_uid).is_none() => Step::Failed,
                        None => Step::Wait,
                    },
                    RelayPhase::Up(id) => match self.transfer_ok(r.dest_uid, id) {
                        Some(true) => Step::Done,
                        Some(false) => Step::Failed,
                        None if self.session_idx_by_uid(r.dest_uid).is_none() => Step::Failed,
                        None => Step::Wait,
                    },
                }
            };
            match step {
                Step::Wait => {
                    // 仍在下载：把源端进度实时反映到目标端「等待」占位行的提示上
                    if let RelayPhase::Down(dlid) = self.relays[i].phase {
                        let (src_uid, dest_uid, up_id) = (self.relays[i].src_uid, self.relays[i].dest_uid, self.relays[i].up_id);
                        if let Some((done, total)) = self.transfer_done_total(src_uid, dlid) {
                            let note = if total > 0 {
                                let pct = (done as f64 / total as f64 * 100.0).round() as u32;
                                match crate::i18n::current() {
                                    crate::i18n::Lang::Zh => format!("等待源端下载 {pct}%…"),
                                    crate::i18n::Lang::En => format!("Waiting for source {pct}%…"),
                                }
                            } else {
                                crate::i18n::tr("等待源端下载…", "Waiting for source download…").into()
                            };
                            if let Some(di) = self.session_idx_by_uid(dest_uid) {
                                if let Some(t) = self.sessions[di].transfers.iter_mut().find(|t| t.id == up_id && t.queued) {
                                    t.note = note;
                                }
                            }
                        }
                    }
                    i += 1;
                }
                Step::ToUpload => {
                    let dest_uid = self.relays[i].dest_uid;
                    let tmp = self.relays[i].tmp.to_string_lossy().into_owned();
                    let dest_dir = self.relays[i].dest_dir.clone();
                    let up_id = self.relays[i].up_id;
                    if let Some(di) = self.session_idx_by_uid(dest_uid) {
                        // 复用粘贴时预占的 up_id：worker 的 TransferStart 会把占位行就地转为进行中
                        let _ = self.sessions[di].cmd_tx.send(UiCommand::Upload { id: up_id, local: tmp, remote_dir: dest_dir, policy: ConflictPolicy::Overwrite });
                        if let Some(t) = self.sessions[di].transfers.iter_mut().find(|t| t.id == up_id) {
                            t.queued = false;
                            t.note = String::new();
                        }
                        self.relays[i].phase = RelayPhase::Up(up_id);
                        i += 1;
                    } else {
                        let (t, d) = (self.relays[i].tmp.clone(), self.relays[i].is_dir);
                        Self::cleanup_relay_tmp(&t, d);
                        self.relays.remove(i);
                    }
                }
                Step::Done => {
                    let (t, d, is_cut, src_uid, src_path) = (
                        self.relays[i].tmp.clone(),
                        self.relays[i].is_dir,
                        self.relays[i].is_cut,
                        self.relays[i].src_uid,
                        self.relays[i].src_path.clone(),
                    );
                    // 剪切：上传成功后才删源（安全）
                    if is_cut {
                        if let Some(sidx) = self.session_idx_by_uid(src_uid) {
                            let _ = self.sessions[sidx].cmd_tx.send(UiCommand::DeleteMany { paths: vec![src_path] });
                        }
                    }
                    Self::cleanup_relay_tmp(&t, d);
                    self.relays.remove(i);
                }
                Step::Failed => {
                    // 若在下载阶段失败：目标端占位行还停在「等待」，标记其失败避免空挂
                    if let RelayPhase::Down(_) = self.relays[i].phase {
                        let (dest_uid, up_id) = (self.relays[i].dest_uid, self.relays[i].up_id);
                        self.fail_placeholder(dest_uid, up_id, crate::i18n::tr("源端下载失败", "Source download failed"));
                    }
                    let (t, d) = (self.relays[i].tmp.clone(), self.relays[i].is_dir);
                    Self::cleanup_relay_tmp(&t, d);
                    self.relays.remove(i);
                }
            }
        }
    }

    /// 执行跨服务器「直传」：据目标会话配置构造 DirectSpec，交源会话 worker 在源主机上
    /// 直接 rsync/scp 推到目标主机。仅目标为「无口令密钥」认证时可用；否则立即弹「转中转」。
    fn execute_direct(&mut self, plan: PendingPaste) {
        let Some(si) = self.session_idx_by_uid(plan.src_uid) else {
            if let Some(di) = self.session_idx_by_uid(plan.dest_uid) {
                self.sessions[di].status = crate::i18n::tr("源会话已关闭，无法直传", "Source session closed; cannot direct-transfer").into();
            }
            return;
        };
        let Some(di) = self.session_idx_by_uid(plan.dest_uid) else { return };
        // 目标主机连接参数
        let dest_cfg = self.sessions[di].cfg.clone();
        // 仅支持「无口令密钥」认证的目标：取私钥本地路径
        let key_path = match &dest_cfg.auth {
            crate::proto::AuthMethod::KeyFile { path, passphrase } if passphrase.as_deref().map(|s| s.is_empty()).unwrap_or(true) => path.clone(),
            _ => {
                // 直传不可用（密码/agent/交互/口令密钥）：直接进入「转中转」提醒
                self.pending_direct_fallback.push(DirectFallback {
                    plan: PendingPaste { direct: false, ..plan.clone() },
                    reason: crate::i18n::tr(
                        "目标会话非「无口令密钥」认证，无法直传。",
                        "Target session is not passphrase-less key auth; direct transfer unavailable.",
                    ).into(),
                });
                return;
            }
        };
        let n = plan.items.len();
        let first = plan.items.first().map(|(p, _)| p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p).to_string()).unwrap_or_default();
        let label = if n > 1 { format!("{first} +{}", n - 1) } else { first };
        let tag = crate::i18n::tr("直传", "Direct").to_string();
        // 源会话（A）：真实数据通路所在行。total 由 worker 的 du 估算后回填（字节计）
        let id = {
            let s = &mut self.sessions[si];
            let id = s.next_xfer;
            s.next_xfer += 1;
            let mut t = Transfer::new(id, label.clone(), crate::proto::TransferDir::Upload, 0, None, None);
            t.tag = tag.clone();
            s.transfers.push(t);
            id
        };
        // 目标会话（B）：镜像进度行（直传不经 B，App 据源端进度同步显示），方向取「接收=下载」绿色
        let mir_id = {
            let s = &mut self.sessions[di];
            let mid = s.next_xfer;
            s.next_xfer += 1;
            let mut t = Transfer::new(mid, label.clone(), crate::proto::TransferDir::Download, 0, None, None);
            t.tag = tag.clone();
            s.transfers.push(t);
            mid
        };
        let spec = crate::proto::DirectSpec {
            id,
            srcs: plan.items.iter().map(|(p, _)| p.clone()).collect(),
            dest_user: dest_cfg.username.clone(),
            dest_host: dest_cfg.host.clone(),
            dest_port: dest_cfg.port,
            dest_dir: plan.dest_dir.clone(),
            key_path,
            label: label.clone(),
        };
        let _ = self.sessions[si].cmd_tx.send(UiCommand::DirectTransfer(Box::new(spec)));
        self.direct_jobs.push(DirectJob {
            id,
            mir_id,
            src_uid: plan.src_uid,
            dest_uid: plan.dest_uid,
            src_dir: plan.src_dir.clone(),
            dest_dir: plan.dest_dir.clone(),
            is_cut: plan.is_cut,
            items: plan.items.clone(),
            src_label: plan.src_label.clone(),
            dest_label: plan.dest_label.clone(),
            cancelled: false,
        });
        self.show_transfers = true;
        self.xfer_just_opened = true;
        self.sessions[si].status = match (plan.is_cut, crate::i18n::current()) {
            (true, crate::i18n::Lang::Zh) => format!("跨服务器移动 {n} 项（直传）…"),
            (false, crate::i18n::Lang::Zh) => format!("跨服务器复制 {n} 项（直传）…"),
            (true, crate::i18n::Lang::En) => format!("Cross-server move {n} (direct) …"),
            (false, crate::i18n::Lang::En) => format!("Cross-server copy {n} (direct) …"),
        };
        // 目标会话（B）也给一句状态（其传输浮窗里有镜像进度行）
        self.sessions[di].status = match (plan.is_cut, crate::i18n::current()) {
            (_, crate::i18n::Lang::Zh) => format!("正从源主机直传 {n} 项到此目录…"),
            (_, crate::i18n::Lang::En) => format!("Direct transfer of {n} item(s) into this folder…"),
        };
    }

    /// 收尾直传在目标端（B）的镜像进度行：标记完成/失败；完成时进度拉满。
    fn finish_mirror(sessions: &mut [Session], job: &DirectJob, ok: bool, msg: &str) {
        if let Some(s) = sessions.iter_mut().find(|s| s.uid == job.dest_uid) {
            if let Some(t) = s.transfers.iter_mut().find(|t| t.id == job.mir_id) {
                t.ok = Some(ok);
                t.message = msg.to_string();
                if ok {
                    t.done = t.total;
                }
            }
        }
    }

    /// 推进直传任务：源会话上的直传传输完成 → 剪切则删源 + 刷新目标目录；失败 → 弹「转中转」提醒。
    fn process_direct_jobs(&mut self) {
        let mut i = 0;
        while i < self.direct_jobs.len() {
            let (src_uid, sid, dest_uid, mir_id) = {
                let j = &self.direct_jobs[i];
                (j.src_uid, j.id, j.dest_uid, j.mir_id)
            };
            let status = match self.transfer_ok(src_uid, sid) {
                Some(ok) => Some(ok),
                None if self.session_idx_by_uid(src_uid).is_none() => Some(false),
                None => None,
            };
            match status {
                None => {
                    // 进行中：把源端（A）的真实进度同步到目标端（B）的镜像行
                    if let Some((done, total)) = self.transfer_done_total(src_uid, sid) {
                        if let Some(didx) = self.session_idx_by_uid(dest_uid) {
                            if let Some(t) = self.sessions[didx].transfers.iter_mut().find(|t| t.id == mir_id && t.ok.is_none()) {
                                t.total = total;
                                t.done = done;
                            }
                        }
                    }
                    i += 1;
                }
                Some(true) => {
                    let job = self.direct_jobs.remove(i);
                    // 目标端镜像行收尾为「完成」
                    Self::finish_mirror(&mut self.sessions, &job, true, crate::i18n::tr("直传完成", "Direct transfer done"));
                    // 剪切：直传成功后删源
                    if job.is_cut {
                        if let Some(sidx) = self.session_idx_by_uid(job.src_uid) {
                            let paths: Vec<String> = job.items.iter().map(|(p, _)| p.clone()).collect();
                            let _ = self.sessions[sidx].cmd_tx.send(UiCommand::DeleteMany { paths });
                        }
                    }
                    // 刷新目标目录（直传不经目标会话，需主动让其重列目录）
                    if let Some(didx) = self.session_idx_by_uid(job.dest_uid) {
                        let _ = self.sessions[didx].cmd_tx.send(UiCommand::ListDir(job.dest_dir.clone()));
                    }
                }
                Some(false) => {
                    let job = self.direct_jobs.remove(i);
                    // 用户主动取消（取消按钮已置 job.cancelled）不弹回退；据此与真失败区分，
                    // 不再靠比对本地化的「已取消」文案（脆弱：worker 可能回报「直传失败（码 -1）」）。
                    let cancelled = job.cancelled;
                    // 目标端镜像行收尾为「失败/取消」
                    Self::finish_mirror(&mut self.sessions, &job, false,
                        if cancelled { crate::i18n::tr("已取消", "Canceled") } else { crate::i18n::tr("直传失败", "Direct failed") });
                    // 真失败：入队「转中转」提醒，确认后走中转链路（队列避免多任务同帧互相覆盖）
                    if !cancelled && self.session_idx_by_uid(job.src_uid).is_some() && self.session_idx_by_uid(job.dest_uid).is_some() {
                        self.pending_direct_fallback.push(DirectFallback {
                            plan: PendingPaste {
                                items: job.items,
                                is_cut: job.is_cut,
                                cross: true,
                                src_uid: job.src_uid,
                                dest_uid: job.dest_uid,
                                src_dir: job.src_dir,
                                dest_dir: job.dest_dir,
                                src_label: job.src_label,
                                dest_label: job.dest_label,
                                direct: false,
                            },
                            reason: crate::i18n::tr("直传失败（源主机无法直推到目标主机）。", "Direct transfer failed (source cannot push to target).").into(),
                        });
                    }
                }
            }
        }
    }

    /// 直传失败后的「必须改用中转」提醒弹框。
    fn direct_fallback_dialog(&mut self, ctx: &egui::Context) {
        let Some(fb) = self.pending_direct_fallback.first() else { return };
        let mut go = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("direct_fallback")).show(ctx, |ui| {
            dialog_body(ui, |ui| {
            ui.label(RichText::new(crate::i18n::tr("直传未成功，必须改用中转", "Direct transfer failed — relay is required")).size(16.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(&fb.reason).color(Palette::DANGER).size(11.0));
            ui.add_space(6.0);
            // 源主机 + 源目录 → 目标主机 + 目标目录
            ui.label(RichText::new(format!("{}  →  {}", fb.plan.src_label, fb.plan.dest_label)).color(Palette::TEXT).size(12.0).strong());
            ui.label(RichText::new(format!("{}  →  {}", fb.plan.src_dir, fb.plan.dest_dir)).monospace().size(11.0).color(Palette::TEXT_DIM));
            ui.add_space(6.0);
            ui.label(RichText::new(crate::i18n::tr("将改为经本地「下载→上传」中转，较慢但最通用。", "Will switch to local download→upload relay; slower but most compatible.")).color(Palette::TEXT_DIM).size(11.0));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 110.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(ui, crate::i18n::tr("改用中转", "Use relay"), Some(Palette::OK), bw) {
                    go = true;
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    cancel = true;
                }
            });
            });
        });
        if go {
            if !self.pending_direct_fallback.is_empty() {
                let fb = self.pending_direct_fallback.remove(0);
                self.execute_paste(fb.plan);
            }
        } else if cancel {
            if !self.pending_direct_fallback.is_empty() {
                self.pending_direct_fallback.remove(0);
            }
        }
    }
}

impl eframe::App for App {
    // 窗口清屏色用主题背景，避免各区域间隙露出黑色
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.logo {
            return [1.0, 1.0, 1.0, 1.0]; // logo 模式白底，圆角矩形（米色）落在白底上便于裁切/置于浅色页
        }
        Palette::BG.to_normalized_gamma_f32()
    }

    // eframe 0.34 的现代入口：所有面板通过 `show_inside` 嵌入根 `ui`。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Logo 生成模式：透明画布上画一个圆角矩形（初始界面背景色）+ iShell（accent 色、同字体）
        if self.logo {
            {
                let square = std::env::var("ISHELL_ICON").is_ok();
                if square {
                    // 应用图标：填满方形画布
                    let painter = ui.painter();
                    let rect = ui.max_rect().shrink(8.0);
                    painter.rect_filled(rect, 30.0, Palette::BG);
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "iShell",
                        egui::FontId::proportional(76.0),
                        Palette::ACCENT,
                    );
                } else {
                    // logo：圆角矩形贴合文字，四周边距大致相等（避免左右过宽）
                    let galley = ui.ctx().fonts_mut(|f| {
                        f.layout_no_wrap("iShell".to_owned(), egui::FontId::proportional(76.0), Palette::ACCENT)
                    });
                    let sz = galley.size();
                    // 上下内边距小一些（galley 自带行间距），让视觉四边接近
                    let rect = egui::Rect::from_center_size(ui.max_rect().center(), sz + egui::vec2(64.0, 40.0));
                    let painter = ui.painter();
                    painter.rect_filled(rect, 26.0, Palette::BG);
                    painter.galley(rect.center() - sz / 2.0, galley, Palette::ACCENT);
                }
            }
            let ctx = ui.ctx().clone();
            self.drive_screenshot(&ctx);
            return;
        }

        // 注：曾在此「聚焦文本框时每帧持续重绘」以试图修复 X11/fcitx 的输入法提交延迟，
        // 但该延迟实为 winit X11/XIM 的事件投递限制（提交事件晚到一拍），重绘并不能解决；
        // 反而导致开了编辑器/多窗口后两窗口 60fps 永动重绘 → macOS Stage Manager 缩略图不停闪。
        // 故移除：egui 本就在收到按键/IME 事件时反应式重绘，正常输入不受影响。

        // 全局界面缩放（左侧栏可调）：仅在变化时设置，避免每帧触发重排
        if (ui.ctx().zoom_factor() - ui_zoom()).abs() > f32::EPSILON {
            ui.ctx().set_zoom_factor(ui_zoom());
        }

        // 活动会话切换时，复位「跨会话易串台」的临时 UI 态：转发的删除确认/编辑、进程详情弹窗
        // （否则 Ctrl+Tab 切走后，转发窗按 id 的确认/编辑可能命中新会话同 id 的另一条；进程弹窗显示陈旧）。
        let cur_active_uid = self.active.and_then(|i| self.sessions.get(i)).map(|s| s.uid);
        if cur_active_uid != self.active_uid_prev {
            self.active_uid_prev = cur_active_uid;
            self.fwd_confirm_del = None;
            self.fwd_editing = None;
            self.fwd_error = None;
            self.proc_popup = None;
        }

        // 1) 排空所有会话的后台事件，并在连接成功后初始化文件树
        // 身份用会话 uid（稳定唯一），title 仅作显示——避免同名会话（默认 title=用户名）串台。
        let mut new_placeholders: Vec<(u64, String, String, u64, UnboundedSender<UiCommand>)> = Vec::new(); // id, path, title, uid, tx
        let mut filled: Vec<(u64, String, String, String, crate::proto::Eol, u32)> = Vec::new(); // id, path, content, encoding, eol, mtime
        let mut load_progress: Vec<(u64, u64, u64)> = Vec::new();
        let mut load_fail: Vec<u64> = Vec::new();
        let mut new_images: Vec<(String, Vec<u8>, String, u64)> = Vec::new(); // path, data, title, uid
        let mut saved: Vec<(u64, String, u32)> = Vec::new(); // uid, path, mtime
        let mut save_progress: Vec<(u64, String, u64, u64)> = Vec::new(); // uid, path, done, total
        let mut conflicts: Vec<(u64, String)> = Vec::new(); // uid, path
        let mut too_large: Vec<(u64, u64, String, u64)> = Vec::new(); // uid, id, path, size
        for s in &mut self.sessions {
            s.drain_events();
            if s.connected && !s.initialized {
                s.initialized = true;
                s.init_files();
            }
            for (id, path) in s.pending_placeholder.drain(..) {
                new_placeholders.push((id, path, s.title.clone(), s.uid, s.cmd_tx.clone()));
            }
            for (id, path, content, encoding, eol, mtime) in s.pending_open.drain(..) {
                filled.push((id, path, content, encoding, eol, mtime));
            }
            for (path, mtime) in s.pending_saved.drain(..) {
                saved.push((s.uid, path, mtime));
            }
            for (path, done, total) in s.pending_save_progress.drain(..) {
                save_progress.push((s.uid, path, done, total));
            }
            for path in s.pending_conflict.drain(..) {
                conflicts.push((s.uid, path));
            }
            for (id, path, size) in s.pending_too_large.drain(..) {
                too_large.push((s.uid, id, path, size));
            }
            for p in s.pending_load_progress.drain(..) {
                load_progress.push(p);
            }
            for (id, _msg) in s.pending_load_fail.drain(..) {
                load_fail.push(id);
            }
            for (path, data) in s.pending_image.drain(..) {
                new_images.push((path, data, s.title.clone(), s.uid));
            }
        }
        // 打开时发现文件实际超限：移除占位标签（复用 load_fail 移除逻辑），并在对应会话的文件面板
        // 弹「打开大文件」确认，确认后走 force=true 重新打开（列表里的旧大小已过时，双击前无法预判）。
        for (uid, id, path, size) in too_large {
            load_fail.push(id);
            if let Some(s) = self.sessions.iter_mut().find(|s| s.uid == uid) {
                s.files.dialog = Some(file_panel::Dialog::ConfirmOpenLarge { path, size });
            }
        }
        // 跨服务器中转任务推进（下载完→上传，上传完→剪切则删源）
        self.process_relays();
        // 跨服务器直传任务推进（完成则删源/刷新；失败则弹「转中转」）
        self.process_direct_jobs();
        for (path, data, server, uid) in new_images {
            self.image_focus = true; // 打开/切换后聚焦看图窗口
            // 同一会话同一图片已打开则切到该标签（身份用 uid，不用可能重名的 title）
            if let Some(i) = self.image_tabs.iter().position(|t| t.uid == uid && t.path == path) {
                self.active_image = i;
                continue;
            }
            match image::load_from_memory(&data) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let name = format!("img:{server}:{path}");
                    let tex = ui.ctx().load_texture(name, color, egui::TextureOptions::LINEAR);
                    self.image_tabs.push(ImageTab {
                        server,
                        uid,
                        path,
                        tex,
                        data,
                        size: egui::vec2(size[0] as f32, size[1] as f32),
                        zoom: 0.0,
                        offset: egui::Vec2::ZERO,
                    });
                    self.active_image = self.image_tabs.len() - 1;
                }
                Err(e) => {
                    let msg = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("图片解码失败：{e}"), crate::i18n::Lang::En => format!("Decode failed: {e}") };
                    if let Some(sess) = self.sessions.iter_mut().find(|s| s.uid == uid) {
                        sess.status = msg;
                    }
                }
            }
        }
        // 编辑器标签：立即建占位（loading）→ 进度更新 → 内容就位 → 失败移除。
        let opened_editor = !new_placeholders.is_empty() || !filled.is_empty() || !load_progress.is_empty() || !load_fail.is_empty();
        if opened_editor {
            // 占位标签的 text_id 先在锁外分配（alloc_editor_id 借用 self）
            let mut ph_ids: Vec<egui::Id> = Vec::with_capacity(new_placeholders.len());
            for _ in &new_placeholders {
                ph_ids.push(self.alloc_editor_id());
            }
            let mut ed = self.editor_state.lock().unwrap();
            // 1) 新建占位标签（同服务器同文件已打开则切过去）
            for ((id, path, server, uid, tx), tid) in new_placeholders.into_iter().zip(ph_ids) {
                ed.focus = true;
                if let Some(i) = ed.tabs.iter().position(|t| t.uid == uid && t.editor.path == path) {
                    ed.active = i;
                } else {
                    let mut editor = crate::ui::editor::Editor::new(path, String::new());
                    editor.set_loading(true);
                    ed.tabs.push(EditorTab { editor, server, uid, cmd_tx: tx, text_id: tid, load_id: Some(id), load_done: 0, load_total: 0, save_conflict: false, save_at: None, saving: false, save_done: 0, save_total: 0, save_done_at: None });
                    ed.active = ed.tabs.len() - 1;
                }
            }
            // 2) 下载进度 → 占位标签
            for (id, done, total) in load_progress {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    t.load_done = done;
                    t.load_total = total;
                }
            }
            // 3) 内容就位：占位标签变为可编辑、填入内容
            for (id, path, content, encoding, eol, mtime) in filled {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    let mut editor = crate::ui::editor::Editor::new(path, content);
                    editor.set_meta(encoding, eol, mtime);
                    t.editor = editor;
                    t.load_id = None;
                }
            }
            // 4) 失败：移除对应占位标签
            for id in load_fail {
                if let Some(i) = ed.tabs.iter().position(|t| t.load_id == Some(id)) {
                    ed.tabs.remove(i);
                    if ed.active >= ed.tabs.len() {
                        ed.active = ed.tabs.len().saturating_sub(1);
                    }
                }
            }
            // 编辑器是独立 deferred 子窗口：变化后必须显式唤醒它重绘（含进度条动画）。
            ui.ctx().request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        }
        // 保存成功 → 更新对应标签 mtime（避免下次保存误判）；外部改动冲突 → 置标志，编辑器弹横幅。
        if !saved.is_empty() || !conflicts.is_empty() || !save_progress.is_empty() {
            let mut ed = self.editor_state.lock().unwrap();
            for (uid, path, done, total) in save_progress {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.save_done = done;
                    t.save_total = total;
                }
            }
            for (uid, path, mtime) in saved {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.editor.set_mtime(mtime); // 回填服务器新 mtime，避免下次保存把「自己刚写入」误判为外部改动
                    t.save_conflict = false;
                    t.saving = false; // 保存完成，解锁再次保存
                }
            }
            for (uid, path) in conflicts {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.save_conflict = true;
                    t.saving = false; // 冲突也算本次保存结束，解锁（用户可在横幅选择覆盖）
                    t.save_at = None; // 冲突未写入：中止「已保存」动画
                    t.save_done_at = None;
                }
            }
            ui.ctx().request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        }

        // 断线自动重连：到点的执行重连，并安排下次唤醒（即使无交互也能触发）
        let now = std::time::Instant::now();
        let mut due: Vec<usize> = Vec::new();
        let mut next_wake: Option<std::time::Duration> = None;
        for (i, s) in self.sessions.iter().enumerate() {
            if let Some(at) = s.reconnect_at {
                if now >= at {
                    due.push(i);
                } else {
                    let d = at - now;
                    next_wake = Some(next_wake.map_or(d, |w: std::time::Duration| w.min(d)));
                }
            }
        }
        for i in due {
            if let Some(s) = self.sessions.get_mut(i) {
                s.reconnect_tries += 1;
            }
            self.reconnect_session(i);
        }
        if let Some(d) = next_wake {
            ui.ctx().request_repaint_after(d);
        }

        // 编辑器关闭标签后请求归还内存（deferred 回调里无法直接动 App，用共享标志传出）
        {
            let mut ed = self.editor_state.lock().unwrap();
            if ed.trim_request {
                ed.trim_request = false;
                self.trim_after = Some(4);
            }
        }
        // 关闭编辑器后延迟归还内存（等 galley 缓存淘汰）
        if let Some(n) = self.trim_after {
            if n == 0 {
                trim_memory();
                self.trim_after = None;
            } else {
                self.trim_after = Some(n - 1);
                ui.ctx().request_repaint();
            }
        }

        // 自检：注入网络曲线波形以核对点密度
        if self.demo_net {
            if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                s.net_hist.down.clear();
                s.net_hist.up.clear();
                // 仅 30 个点，便于核对「从右侧起、向左生长」与点密度
                for i in 0..30 {
                    let t = i as f64;
                    s.net_hist.down.push_back(((t * 0.4).sin() * 0.5 + 0.5) * 5.0e6);
                    s.net_hist.up.push_back(((t * 0.3).cos() * 0.5 + 0.5) * 2.0e6);
                }
            }
        }

        // 自检：注入假 GPU 数据并保持详情窗打开
        if self.demo_gpu {
            if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                if let Some(si) = s.sysinfo.as_mut() {
                    si.gpus = vec![
                        crate::proto::GpuInfo { index: 0, name: "RTX 4090".into(), util: 73.0, mem_used_mb: 18000, mem_total_mb: 24564 },
                        crate::proto::GpuInfo { index: 1, name: "RTX 4090".into(), util: 12.0, mem_used_mb: 2000, mem_total_mb: 24564 },
                    ];
                }
            }
            self.gpu_popup = Some(egui::pos2(130.0, 130.0));
            self.gpu_popup_just_opened = true;
        }

        // 进程详情返回 -> 填充小窗
        if let Some(idx) = self.active {
            let detail = self.sessions.get_mut(idx).and_then(|s| s.proc_detail.take());
            if let Some((pid, cmd, cwd, exe)) = detail {
                if let Some(pp) = &mut self.proc_popup {
                    if pp.pid == pid {
                        pp.cmd = cmd;
                        pp.cwd = cwd;
                        pp.exe = exe;
                    }
                }
            }
        }

        // 2) 连接对话框（浮动窗口）
        let ctx = ui.ctx().clone();
        if let Some(cfg) = self.connect_form.show(&ctx) {
            self.spawn_session(cfg);
        }

        // 3) 左侧操作栏：独立全高区域
        let mut proc_click: Option<(u32, egui::Pos2)> = None;
        let mut gpu_click: Option<egui::Pos2> = None;
        if !sidebar_collapsed() {
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=460.0)
            .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 8 }))
            .show_inside(ui, |ui| {
                // 背景层右键弹语言菜单：在子控件之前注册，置于最底层 z 序，
                // 这样不会抢走进程行/网卡/IP 等子控件的左键；空白处右键仍可触发。
                let bg = ui.interact(ui.max_rect(), ui.id().with("sidebar_bg"), egui::Sense::click());
                // 监控栏右键：语言 / 字体大小 / 折叠开关 / 强制 X11 的统一入口
                view_context_menu(&bg);
                match self.active {
                    Some(idx) if idx < self.sessions.len() => {
                        let s = &mut self.sessions[idx];
                        sidebar::show(ui, s.sysinfo.as_ref(), &s.net_hist, &mut s.selected_nic, &mut s.proc_sort_mem, &mut proc_click, &mut gpu_click);
                    }
                    _ => {
                        ui.add_space(16.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(egui_phosphor::regular::PLUGS).size(28.0).color(Palette::TEXT_DIM));
                            ui.label(RichText::new(crate::i18n::tr("未连接", "Not connected")).color(Palette::TEXT_DIM));
                        });
                    }
                }
            });
        } else {
            // 折叠态：保留一条细边，提供展开按钮 + 同样的右键菜单（否则收起后无处可点回来）
            egui::Panel::left("sidebar_strip")
                .resizable(false)
                .default_size(20.0)
                .size_range(20.0..=20.0)
                .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin::same(2)))
                .show_inside(ui, |ui| {
                    let bg = ui.interact(ui.max_rect(), ui.id().with("sidebar_strip_bg"), egui::Sense::click());
                    view_context_menu(&bg);
                    ui.add_space(4.0);
                    ui.vertical_centered(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new(egui_phosphor::regular::CARET_RIGHT).size(14.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(crate::i18n::tr("展开系统监控栏", "Expand monitor sidebar"))
                            .clicked()
                        {
                            set_sidebar_collapsed(false);
                        }
                    });
                });
        }
        // 进程行被点击：打开详情小窗并请求详情
        if let Some((pid, pos)) = proc_click {
            let mut popup = None;
            if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                if let Some(p) = s.sysinfo.as_ref().and_then(|si| si.procs.iter().find(|p| p.pid == pid)) {
                    popup = Some(ProcPopup {
                        pid, name: p.name.clone(), cpu: p.cpu, mem: p.mem, pos,
                        cmd: String::new(), cwd: String::new(), exe: String::new(),
                        copied_t: None,
                        confirm_kill: false,
                        uid: s.uid,
                    });
                }
                let _ = s.cmd_tx.send(UiCommand::ProcDetail(pid));
            }
            if let Some(pp) = popup {
                self.proc_popup = Some(pp);
                self.proc_popup_just_opened = true;
            }
        }
        if let Some(pos) = gpu_click {
            self.gpu_popup = Some(pos);
            self.gpu_popup_just_opened = true;
        }

        // Ctrl+Tab / Ctrl+Shift+Tab 切换会话标签（consume 以免终端把 Tab 发往远端）
        if !self.sessions.is_empty() {
            let ctx = ui.ctx();
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)) {
                self.switch_tab(-1);
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                self.switch_tab(1);
            }
        }

        // 4) 顶部选项卡（仅位于右侧区域之上）
        self.top_tabs(ui);

        // 4.5) 命令广播栏
        self.broadcast_bar(ui);

        // 5) 右侧主体
        match self.active {
            Some(idx) if idx < self.sessions.len() => self.right_body(ui, idx),
            _ => self.welcome(ui),
        }

        // 顶部浮层提示（撤销结果等醒目反馈）
        self.toast_overlay(&ctx);

        // 传输进度浮窗
        self.transfer_window(&ctx);

        // 端口转发管理浮窗
        self.forward_window(&ctx);

        // 进程详情小窗
        self.proc_popup_window(&ctx);

        // 「关于」弹框（右键菜单触发）
        about_window(&ctx);

        // GPU 详情小窗
        self.gpu_popup_window(&ctx);

        // 文本编辑器浮窗
        self.editor_window(&ctx);

        // 看图工具浮窗
        self.image_window(&ctx);

        // 未知主机指纹确认（TOFU）
        self.host_key_dialog(&ctx);

        // 键盘交互认证（OTP / 2FA）
        self.kbd_prompt_dialog(&ctx);

        // 关闭活动标签二次确认
        self.close_tab_dialog(&ctx);

        // 粘贴确认（跨服务器：含「直传/中转」互斥选择）
        self.paste_confirm_dialog(&ctx);
        // 直传失败后的「必须改用中转」提醒
        self.direct_fallback_dialog(&ctx);

        // 命令片段库
        self.snippets_window(&ctx);

        // 关闭确认：仍有会话连接时，先弹确认
        self.handle_close(&ctx);

        // 自检截图驱动
        self.drive_screenshot(&ctx);
    }
}

impl App {
    /// 未知主机首次连接：确认指纹（TOFU），同意则 worker 写入 known_hosts。
    fn host_key_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.sessions.iter().position(|s| s.pending_hostkey.is_some()) else {
            return;
        };
        let (host, fp, changed) = self.sessions[idx].pending_hostkey.clone().unwrap();
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("hostkey_modal"))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                if changed {
                    // 主机密钥变更：更醒目的红色警告 + 在 UI 内替换 known_hosts
                    ui.label(RichText::new(crate::i18n::tr("⚠ 主机密钥已变更", "⚠ Host key changed")).size(16.0).strong().color(Palette::DANGER));
                    ui.add_space(8.0);
                    ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("主机：{host}"), crate::i18n::Lang::En => format!("Host: {host}") });
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("新指纹 (SHA256)：", "New fingerprint (SHA256):")).color(Palette::TEXT_DIM).size(12.0));
                    ui.label(RichText::new(&fp).monospace());
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("known_hosts 中记录的密钥与服务器当前不符。若非你主动更换了服务器密钥，可能是中间人攻击！接受将删除旧密钥并写入新密钥。", "The recorded key differs from the server's. If you didn't rotate the key, this could be a MITM attack! Accepting removes the old key and stores the new one.")).color(Palette::DANGER).size(11.0));
                } else {
                    ui.label(RichText::new(crate::i18n::tr("未知主机", "Unknown host")).size(16.0).strong());
                    ui.add_space(8.0);
                    ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("首次连接主机：{host}"), crate::i18n::Lang::En => format!("First connect: {host}") });
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("指纹 (SHA256)：", "Fingerprint (SHA256):")).color(Palette::TEXT_DIM).size(12.0));
                    ui.label(RichText::new(&fp).monospace());
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("请确认该指纹与目标服务器一致；信任后将写入 ~/.ssh/known_hosts。", "Verify the fingerprint matches the server; trusting writes to ~/.ssh/known_hosts.")).color(Palette::TEXT_DIM).size(11.0));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 120.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    let (accept_label, accept_col) = if changed {
                        (crate::i18n::tr("删除旧密钥并信任", "Replace & trust"), Palette::DANGER)
                    } else {
                        (crate::i18n::tr("信任并连接", "Trust & connect"), Palette::ACCENT)
                    };
                    if dialog_button(ui, accept_label, Some(accept_col), bw) {
                        decision = Some(true);
                    }
                    if dialog_button(ui, crate::i18n::tr("拒绝", "Reject"), None, bw) {
                        decision = Some(false);
                    }
                });
            });
        if let Some(d) = decision {
            let s = &mut self.sessions[idx];
            let _ = s.hostkey_tx.send(d);
            s.pending_hostkey = None;
        }
    }

    /// 键盘交互认证：弹窗逐项收集回答，提交后经 cmd_tx 回 `KbdResponse`；取消则断开。
    fn kbd_prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.sessions.iter().position(|s| s.kbd_prompt.is_some()) else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("kbd_modal")).show(ctx, |ui| {
            ui.set_width(360.0);
            let s = &mut self.sessions[idx];
            let kp = s.kbd_prompt.as_mut().unwrap();
            let title = if kp.name.trim().is_empty() {
                crate::i18n::tr("二次验证", "Verification").to_string()
            } else {
                kp.name.clone()
            };
            ui.label(RichText::new(title).size(16.0).strong());
            if !kp.instructions.trim().is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&kp.instructions).color(Palette::TEXT_DIM).size(12.0));
            }
            ui.add_space(8.0);
            egui::Grid::new("kbd_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                for (i, (prompt, echo)) in kp.prompts.iter().enumerate() {
                    ui.label(prompt);
                    // echo=false 的提示（如密码/验证码）做遮蔽
                    let r = ui.add(egui::TextEdit::singleline(&mut kp.answers[i]).desired_width(200.0).password(!echo));
                    // 打开即聚焦第一个输入框，可直接输入验证码
                    if i == 0 && ui.memory(|m| m.focused().is_none()) {
                        r.request_focus();
                    }
                    ui.end_row();
                }
            });
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 96.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(ui, crate::i18n::tr("提交", "Submit"), Some(Palette::ACCENT), bw) {
                    submit = true;
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    cancel = true;
                }
            });
        });
        if submit {
            let s = &mut self.sessions[idx];
            if let Some(kp) = s.kbd_prompt.take() {
                let _ = s.cmd_tx.send(UiCommand::KbdResponse(kp.answers));
            }
        } else if cancel {
            let s = &mut self.sessions[idx];
            s.kbd_prompt = None;
            let _ = s.cmd_tx.send(UiCommand::Disconnect);
        }
    }

    /// 粘贴二次确认：剪切（移动/删源）或跨服务器（重操作）执行前弹窗确认。
    fn paste_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.pending_paste.as_ref() else { return };
        let mut go = false;
        let mut cancel = false;
        // 互斥选择的本地镜像（plan 已不可变借用 self，不能再借 self.confirm_direct）
        let mut direct = self.confirm_direct;
        let cross = plan.cross;
        egui::Modal::new(egui::Id::new("paste_confirm")).show(ctx, |ui| {
            dialog_body(ui, |ui| {
                let n = plan.items.len();
                let title = match (plan.is_cut, crate::i18n::current()) {
                    (true, crate::i18n::Lang::Zh) => format!("确认移动 {n} 项？"),
                    (false, crate::i18n::Lang::Zh) => format!("确认复制 {n} 项？"),
                    (true, crate::i18n::Lang::En) => format!("Move {n} item(s)?"),
                    (false, crate::i18n::Lang::En) => format!("Copy {n} item(s)?"),
                };
                ui.label(RichText::new(title).size(16.0).strong());
                ui.add_space(8.0);
                // 源主机 + 源目录
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::i18n::tr("源", "From")).size(11.0).color(Palette::TEXT_DIM));
                    ui.label(RichText::new(&plan.src_label).size(12.0).strong().color(Palette::TEXT));
                });
                ui.label(RichText::new(&plan.src_dir).monospace().size(11.0).color(Palette::TEXT_DIM));
                ui.add_space(4.0);
                // 目标主机 + 粘贴目录
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::i18n::tr("目标", "To")).size(11.0).color(Palette::TEXT_DIM));
                    ui.label(RichText::new(&plan.dest_label).size(12.0).strong().color(Palette::TEXT));
                });
                ui.label(RichText::new(&plan.dest_dir).monospace().size(11.0).color(Palette::TEXT_DIM));
                if plan.cross {
                    ui.add_space(8.0);
                    // 「直传 / 中转」互斥选择（默认直传）
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("方式", "Method")).size(11.0).color(Palette::TEXT_DIM));
                        ui.selectable_value(&mut direct, true, RichText::new(crate::i18n::tr("直传", "Direct")).size(12.0));
                        ui.selectable_value(&mut direct, false, RichText::new(crate::i18n::tr("中转", "Relay")).size(12.0));
                    });
                    let hint = if direct {
                        crate::i18n::tr("源主机直推目标，数据不经本地（需目标会话为「无口令密钥」认证）。", "Source pushes straight to target, bypassing local (target must use a passphrase-less key).")
                    } else {
                        crate::i18n::tr("经本地「下载→上传」中转，最通用，大文件较慢。", "Relayed via local download→upload; most compatible, slower for large files.")
                    };
                    ui.label(RichText::new(hint).color(Palette::TEXT_DIM).size(11.0));
                }
                if plan.is_cut {
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("剪切为移动：复制成功后会从源删除，不可恢复。", "Cut = move: source is deleted after a successful copy. Irreversible.")).color(Palette::DANGER).size(11.0));
                }
                // 列出名称（最多 8 个）
                ui.add_space(6.0);
                let shown: Vec<String> = plan.items.iter().take(8).map(|(p, _)| p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p).to_string()).collect();
                let more = if n > 8 { format!(" … (+{})", n - 8) } else { String::new() };
                ui.label(RichText::new(format!("{}{}", shown.join("、"), more)).color(Palette::TEXT_DIM).size(11.0));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 96.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    let confirm_label = if plan.is_cut { crate::i18n::tr("移动", "Move") } else { crate::i18n::tr("复制", "Copy") };
                    let confirm_col = if plan.is_cut { Palette::DANGER } else { Palette::ACCENT };
                    if dialog_button(ui, confirm_label, Some(confirm_col), bw) {
                        go = true;
                    }
                    if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                        cancel = true;
                    }
                });
            });
        });
        // 记住本帧的互斥选择（跨帧保持，直到下次打开确认复位为直传）
        if cross {
            self.confirm_direct = direct;
        }
        if go {
            if let Some(mut plan) = self.pending_paste.take() {
                if plan.cross {
                    plan.direct = direct; // 直传 / 中转 取自弹框里的互斥选择
                }
                self.execute_paste(plan);
            }
        } else if cancel {
            self.pending_paste = None;
        }
    }

    /// 命令片段库：列出片段（一键发送到活动会话终端）+ 新增/编辑/删除，落盘持久化。
    fn snippets_window(&mut self, ctx: &egui::Context) {
        if !self.show_snippets {
            return;
        }
        use egui_phosphor::regular as icon;
        let mut send_cmd: Option<(String, bool)> = None;
        let mut edit: Option<usize> = None;
        let mut delete: Option<usize> = None;
        let mut save_now = false;
        let mut close_win = false;
        let mut changed = false;
        // 与「传输 / 转发」面板同款：右上角锚定、无标题栏、PANEL 底色的紧凑浮窗
        let win = egui::Window::new("snippet_win")
            .title_bar(false)
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(340.0)
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::CODE, crate::i18n::tr("命令片段", "Snippets"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                    });
                });
                ui.separator();

                if self.snippets.is_empty() {
                    ui.add_space(4.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::CODE, crate::i18n::tr("暂无片段，在下方新增", "No snippets; add one below"), false);
                }
                // 列表：点名称即发送到当前会话终端；右侧编辑 / 删除（无边框图标，风格统一）
                egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (i, sn) in self.snippets.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon::PAPER_PLANE_TILT).color(Palette::ACCENT).size(13.0));
                            let label = if sn.name.trim().is_empty() { sn.command.clone() } else { sn.name.clone() };
                            if ui
                                .add(egui::Label::new(RichText::new(label).size(12.0).color(Palette::TEXT)).sense(Sense::click()))
                                .on_hover_text(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("发送：{}", sn.command), crate::i18n::Lang::En => format!("Send: {}", sn.command) })
                                .clicked()
                            {
                                send_cmd = Some((sn.command.clone(), sn.run));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除", "Delete")).clicked() {
                                    delete = Some(i);
                                }
                                if ui.add(egui::Button::new(RichText::new(icon::PENCIL_SIMPLE).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("编辑", "Edit")).clicked() {
                                    edit = Some(i);
                                }
                            });
                        });
                        // 有名称时在下方以等宽小字补充命令原文
                        if !sn.name.trim().is_empty() {
                            ui.label(RichText::new(&sn.command).monospace().size(10.5).color(Palette::TEXT_DIM));
                        }
                        ui.add_space(3.0);
                    }
                });

                ui.separator();
                let editing = self.snip_editing.is_some();
                ui.label(RichText::new(if editing { crate::i18n::tr("编辑片段", "Edit snippet") } else { crate::i18n::tr("新增片段", "New snippet") }).strong().size(12.0));
                ui.add_space(2.0);
                egui::Grid::new("snip_form").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label(crate::i18n::tr("名称", "Name"));
                    ui.add(egui::TextEdit::singleline(&mut self.snip_name).desired_width(210.0).hint_text(crate::i18n::tr("可选，便于识别", "Optional label")));
                    ui.end_row();
                    ui.label(crate::i18n::tr("命令", "Command"));
                    ui.add(egui::TextEdit::multiline(&mut self.snip_cmd).desired_width(210.0).desired_rows(2));
                    ui.end_row();
                });
                ui.checkbox(&mut self.snip_run, crate::i18n::tr("发送后自动回车执行", "Press Enter after sending"));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(if editing { crate::i18n::tr("保存", "Save") } else { crate::i18n::tr("添加", "Add") }).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                        save_now = true;
                    }
                    if editing && ui.button(crate::i18n::tr("取消编辑", "Cancel")).clicked() {
                        self.snip_editing = None;
                        self.snip_name.clear();
                        self.snip_cmd.clear();
                        self.snip_run = true;
                    }
                });
            });
        // 闭包外处理，避免与 self 的借用冲突
        if let Some(i) = edit {
            if let Some(sn) = self.snippets.get(i) {
                self.snip_editing = Some(i);
                self.snip_name = sn.name.clone();
                self.snip_cmd = sn.command.clone();
                self.snip_run = sn.run;
            }
        }
        if let Some(i) = delete {
            if i < self.snippets.len() {
                self.snippets.remove(i);
                changed = true;
                if self.snip_editing == Some(i) {
                    self.snip_editing = None;
                    self.snip_name.clear();
                    self.snip_cmd.clear();
                    self.snip_run = true;
                }
            }
        }
        if save_now {
            let cmd = self.snip_cmd.trim().to_string();
            if !cmd.is_empty() {
                let sn = crate::store::Snippet { name: self.snip_name.trim().to_string(), command: cmd, run: self.snip_run };
                match self.snip_editing.take() {
                    Some(i) if i < self.snippets.len() => self.snippets[i] = sn,
                    _ => self.snippets.push(sn),
                }
                self.snip_name.clear();
                self.snip_cmd.clear();
                self.snip_run = true;
                changed = true;
            }
        }
        if changed {
            crate::store::save_snippets(&self.snippets);
        }
        if let Some((cmd, run)) = send_cmd {
            if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                let mut bytes = cmd.into_bytes();
                if run {
                    bytes.push(b'\r');
                }
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(bytes));
                s.terminal.request_focus();
            }
        }
        // 点击窗口外部自动隐藏（打开当帧除外），或点 X 关闭
        let clicked_outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close_win || (clicked_outside && !self.snip_just_opened) {
            self.show_snippets = false;
        }
        self.snip_just_opened = false;
    }

    /// 关闭窗口前确认（仍有会话时）。
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && !self.sessions.is_empty()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_close_confirm = true;
        }
        if self.show_close_confirm {
            egui::Modal::new(egui::Id::new("close_modal"))
                .show(ctx, |ui| {
                    ui.set_width(320.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("确认退出", "Quit?")).size(16.0).strong());
                        ui.add_space(6.0);
                        ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("还有 {} 个会话处于连接中", self.sessions.len()), crate::i18n::Lang::En => format!("{} session(s) still connected", self.sessions.len()) });
                        ui.label(crate::i18n::tr("确定退出 iShell 吗？", "Quit iShell?"));
                    });
                    ui.add_space(12.0);
                    // 按钮行水平居中（固定按钮宽度 + 居中留白）
                    ui.horizontal(|ui| {
                        let bw = 72.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        let space = ((ui.available_width() - total) / 2.0).max(0.0);
                        ui.add_space(space);
                        if dialog_button(ui, crate::i18n::tr("退出", "Quit"), Some(Palette::DANGER), bw) {
                            self.allow_close = true;
                            self.show_close_confirm = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                            self.show_close_confirm = false;
                        }
                    });
                });
        }
    }
}

impl App {
    /// 截图自检：到达指定帧请求截图，收到后写 PNG 并退出。
    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint(); // 保持持续渲染

        // 收到截图事件 -> 保存退出
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let [w, h] = [img.size[0] as u32, img.size[1] as u32];
            let mut buf = Vec::with_capacity((w * h * 4) as usize);
            for p in img.pixels.iter() {
                buf.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
            }
            if let Some(im) = image::RgbaImage::from_raw(w, h, buf) {
                let _ = im.save(&shot.path);
            }
            std::process::exit(0);
        }

        if std::time::Instant::now() >= shot.deadline && !shot.requested {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
    }
}

impl App {
    /// 命令广播栏：输入命令回车，发送到所有已连接会话。
    fn broadcast_bar(&mut self, root: &mut egui::Ui) {
        use egui_phosphor::regular as icon;
        if !self.show_broadcast {
            return;
        }
        let targets = self.sessions.iter().filter(|s| s.connected).count();
        let mut send = false;
        egui::Panel::top("broadcast")
            .frame(egui::Frame::new().fill(Palette::ACCENT_SOFT).inner_margin(egui::Margin::symmetric(8, 5)))
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("{} 群发到 {} 个会话", icon::MEGAPHONE, targets), crate::i18n::Lang::En => format!("{} Broadcast to {} session(s)", icon::MEGAPHONE, targets) }).color(Palette::TEXT).size(12.0));
                    if ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                        self.show_broadcast = false;
                    }
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.broadcast_input)
                            .desired_width(ui.available_width() - 70.0)
                            .hint_text(crate::i18n::tr("输入命令，回车发送到所有已连接会话", "Type a command; Enter sends to all connected sessions")),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                        resp.request_focus();
                    }
                    if ui.add(egui::Button::new(RichText::new(format!("{} {}", icon::PAPER_PLANE_RIGHT, crate::i18n::tr("发送", "Send"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                        send = true;
                    }
                });
            });
        if send && !self.broadcast_input.trim().is_empty() {
            let mut bytes = self.broadcast_input.clone().into_bytes();
            bytes.push(b'\r'); // 用 CR（Enter）提交，与其它终端输入一致；\n 在多数行规程下不会执行命令
            for s in self.sessions.iter().filter(|s| s.connected) {
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(bytes.clone()));
            }
            self.broadcast_input.clear();
        }
    }

    fn top_tabs(&mut self, root: &mut egui::Ui) {
        use egui_phosphor::regular as icon;
        egui::Panel::top("tabs")
            .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin::symmetric(6, 4)))
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    // 固定行高，使右侧按钮与标签在同一水平线居中对齐
                    ui.set_min_height(28.0);
                    let mut to_close = None;
                    let mut to_activate = None;
                    let mut reorder: Option<(usize, usize)> = None;
                    // 右侧按钮固定占位（传输 / 转发 / 群发 / 新建），剩余空间给可滚动标签条
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let active_xfers = self
                            .active
                            .and_then(|i| self.sessions.get(i))
                            .map(|s| s.transfers.iter().filter(|t| t.ok.is_none()).count())
                            .unwrap_or(0);
                        let label = if active_xfers > 0 {
                            format!("{} {} {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("传输", "Xfer"), active_xfers)
                        } else {
                            format!("{} {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("传输", "Xfer"))
                        };
                        if flat_button(ui, &RichText::new(label), crate::i18n::tr("显示/隐藏传输进度", "Show/hide transfers")) {
                            self.show_transfers = !self.show_transfers;
                            if self.show_transfers {
                                self.xfer_just_opened = true;
                            }
                        }
                        let nfwd = self
                            .active
                            .and_then(|i| self.sessions.get(i))
                            .map(|s| s.forwards.len())
                            .unwrap_or(0);
                        let flabel = if nfwd > 0 {
                            format!("{} {} {}", icon::ARROWS_LEFT_RIGHT, crate::i18n::tr("转发", "Fwd"), nfwd)
                        } else {
                            format!("{} {}", icon::ARROWS_LEFT_RIGHT, crate::i18n::tr("转发", "Fwd"))
                        };
                        if flat_button(ui, &RichText::new(flabel), crate::i18n::tr("端口转发管理", "Port forwarding")) {
                            self.show_forwards = !self.show_forwards;
                            if self.show_forwards {
                                self.fwd_just_opened = true;
                            }
                        }
                        if flat_button(ui, &RichText::new(format!("{} {}", icon::MEGAPHONE, crate::i18n::tr("群发", "Bcast"))), crate::i18n::tr("向所有已连接会话广播命令", "Broadcast to all connected sessions")) {
                            self.show_broadcast = !self.show_broadcast;
                        }
                        if flat_button(ui, &RichText::new(format!("{} {}", icon::CODE, crate::i18n::tr("片段", "Snip"))), crate::i18n::tr("命令片段库：保存常用命令一键发送到终端", "Command snippets: save & send common commands")) {
                            self.show_snippets = !self.show_snippets;
                            if self.show_snippets {
                                self.snip_just_opened = true;
                            }
                        }
                        // 折叠监控栏/文件栏的开关已移到左侧监控栏右键菜单，避免右上角按钮过多
                        // 分隔竖线：把标签区（标签 + 新建）与右侧功能区分开（短、低调，和谐配色）
                        {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(11.0, ui.available_height()), egui::Sense::hover());
                            let cy = rect.center().y;
                            ui.painter().vline(rect.center().x.round(), (cy - 8.0)..=(cy + 8.0), egui::Stroke::new(1.0, Palette::BORDER));
                        }
                        // 新建：固定在标签条右侧，标签溢出也不会被滚走
                        if flat_button(ui, &RichText::new(icon::PLUS).size(15.0), crate::i18n::tr("新建连接", "New connection")) {
                            self.connect_form.open_dialog();
                            self.show_close_confirm = false;
                        }

                        // 剩余空间：标签条横向可滚动；标签按动画位置放置，重排时平滑滑动。
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let mut drag_start: Option<usize> = None;
                            let mut new_grab: Option<f32> = None;
                            let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
                            let mut drag_w = 0.0f32; // 被拖标签宽度，用于算其跟手中心
                            // 先取出标量字段，避免在借用 self.sessions 的循环里再借 self
                            let dragging_tab = self.dragging_tab;
                            let active = self.active;
                            let grab_dx = self.tab_grab_dx;
                            let total_w_cache = self.tab_total_w;
                            let want_scroll = self.scroll_to_active; // 本帧是否需把激活标签滚到可视区
                            let out = egui::ScrollArea::horizontal()
                                .auto_shrink([false, false])
                                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                                .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
                                .show(ui, |ui| {
                                    let tab_h = 26.0;
                                    let spacing = 4.0;
                                    // 预留区域撑出滚动内容宽度（用上一帧总宽）
                                    let (area, _) = ui.allocate_exact_size(egui::vec2(total_w_cache.max(1.0), tab_h), Sense::hover());
                                    let origin = area.min;
                                    let pointer = ui.input(|i| i.pointer.interact_pos());
                                    let drag_down = ui.input(|i| i.pointer.any_down());
                                    let ctx = ui.ctx().clone();
                                    let body_font = egui::TextStyle::Body.resolve(ui.style());
                                    let mut acc = 0.0f32; // 目标布局累计左边界
                                    for (i, s) in self.sessions.iter().enumerate() {
                                        let selected = active == Some(i);
                                        // 宽度 = 左margin(9)+圆点(10)+间隔(6)+标题+间隔(6)+关闭(18)+右margin(9)
                                        let title_w = ctx.fonts_mut(|f| f.layout_no_wrap(s.title.clone(), body_font.clone(), Palette::TEXT).rect.width());
                                        let w = 58.0 + title_w;
                                        let target = acc;
                                        // 激活标签若被请求滚动到可视区：按其「目标槽」位置请求滚动（带边距余量）
                                        if selected && want_scroll {
                                            let r = egui::Rect::from_min_size(egui::pos2(origin.x + target, origin.y), egui::vec2(w, tab_h));
                                            ui.scroll_to_rect(r.expand2(egui::vec2(12.0, 0.0)), None);
                                        }
                                        let id = egui::Id::new(("tabx", s.uid));
                                        let dragging_this = drag_down && dragging_tab == Some(i);
                                        if dragging_this {
                                            drag_w = w;
                                        }
                                        let x = if dragging_this {
                                            let want = pointer.map(|p| p.x - origin.x - grab_dx).unwrap_or(target);
                                            ctx.animate_value_with_time(id, want, 0.0) // 跟手
                                        } else {
                                            ctx.animate_value_with_time(id, target, 0.14) // 缓动到目标槽
                                        };
                                        let tab_rect = egui::Rect::from_min_size(egui::pos2(origin.x + x, origin.y), egui::vec2(w, tab_h));
                                        // 交互：整张标签可点击（激活）/拖动（排序）；关闭区在上层优先
                                        let resp = ui.interact(tab_rect, egui::Id::new(("tab", s.uid)), Sense::click_and_drag()).on_hover_text(s.tip.as_str());
                                        let close_rect = egui::Rect::from_center_size(egui::pos2(tab_rect.right() - 18.0, tab_rect.center().y), egui::vec2(18.0, 18.0));
                                        let close_resp = ui.interact(close_rect, egui::Id::new(("tabclose", s.uid)), Sense::click());
                                        // 绘制
                                        let fill = if dragging_this { Palette::ACCENT_SOFT } else if selected { Palette::PANEL } else { egui::Color32::TRANSPARENT };
                                        let p = ui.painter();
                                        p.rect_filled(tab_rect, egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, fill);
                                        // 激活标签底部 2px 珊瑚下划线（更清晰的激活指示）
                                        if selected && !dragging_this {
                                            let y = tab_rect.bottom() - 1.0;
                                            p.hline(tab_rect.left()..=tab_rect.right(), y, egui::Stroke::new(2.0, Palette::ACCENT));
                                        }
                                        p.circle_filled(egui::pos2(tab_rect.left() + 14.0, tab_rect.center().y), 4.0, if s.connected { Palette::OK } else { Palette::WARN });
                                        let tcolor = if selected { Palette::TEXT } else { Palette::TEXT_DIM };
                                        p.text(egui::pos2(tab_rect.left() + 25.0, tab_rect.center().y), egui::Align2::LEFT_CENTER, &s.title, body_font.clone(), tcolor);
                                        let xcolor = if close_resp.hovered() { Palette::DANGER } else { Palette::TEXT_DIM };
                                        p.text(close_rect.center(), egui::Align2::CENTER_CENTER, icon::X, egui::FontId::proportional(12.0), xcolor);
                                        // 事件：关闭优先于激活
                                        if close_resp.clicked() {
                                            to_close = Some(i);
                                        } else if resp.clicked() {
                                            to_activate = Some(i);
                                        } else if resp.middle_clicked() {
                                            to_close = Some(i);
                                        }
                                        if resp.drag_started() {
                                            drag_start = Some(i);
                                            if let Some(pp) = pointer {
                                                new_grab = Some(pp.x - (origin.x + x));
                                            }
                                        }
                                        // 命中用「目标槽」位置，稳定判断拖到哪个槽
                                        tab_rects.push((i, egui::Rect::from_min_size(egui::pos2(origin.x + target, origin.y), egui::vec2(w, tab_h))));
                                        acc += w + spacing;
                                    }
                                    acc // 返回总宽
                                });
                            let total_w = out.inner.max(1.0);
                            self.tab_total_w = total_w;
                            self.scroll_to_active = false; // 滚动请求一次性消费
                            // 溢出渐隐：提示左右还有被隐藏的标签
                            let off = out.state.offset.x;
                            let vw = out.inner_rect.width();
                            if off > 0.5 {
                                edge_fade(ui.painter(), out.inner_rect, true, Palette::PANEL_2);
                            }
                            if off + vw < total_w - 0.5 {
                                edge_fade(ui.painter(), out.inner_rect, false, Palette::PANEL_2);
                            }
                            // 应用拖拽状态（循环外，避免与 self.sessions 借用冲突）
                            if let Some(g) = new_grab {
                                self.tab_grab_dx = g;
                            }
                            if let Some(f) = drag_start {
                                self.dragging_tab = Some(f);
                            }
                            // 拖动过程中：用「被拖标签的跟手中心」与相邻标签「目标槽中心」比较，
                            // 越过相邻标签中点才换位（单帧只移一位）。换位后邻居中心移到另一侧，
                            // 判定条件自然失效，避免抓取点偏移（grab_dx）导致的来回抖动。
                            if let Some(from) = self.dragging_tab {
                                if ui.input(|i| i.pointer.any_down()) {
                                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                                        // 被拖标签跟手中心（屏幕横坐标），与绘制时的跟手位口径一致
                                        let drag_center = pos.x - self.tab_grab_dx + drag_w / 2.0;
                                        let mut to = from;
                                        // 向左：越过左邻目标槽中心
                                        if from > 0 {
                                            if let Some(&(_, lr)) = tab_rects.get(from - 1) {
                                                if drag_center < lr.center().x {
                                                    to = from - 1;
                                                }
                                            }
                                        }
                                        // 向右：越过右邻目标槽中心（与向左互斥）
                                        if to == from {
                                            if let Some(&(_, rr)) = tab_rects.get(from + 1) {
                                                if drag_center > rr.center().x {
                                                    to = from + 1;
                                                }
                                            }
                                        }
                                        if to != from {
                                            reorder = Some((from, to));
                                            self.dragging_tab = Some(to);
                                        }
                                    }
                                } else {
                                    self.dragging_tab = None; // 松手：被拖标签从跟手位缓动回目标槽
                                }
                            }
                        });
                    });
                    if let Some((from, to)) = reorder {
                        self.reorder_session(from, to);
                    }
                    if let Some(i) = to_activate {
                        self.active = Some(i);
                        self.scroll_to_active = true; // 点击的标签若被遮挡则滚到可视区
                        if let Some(s) = self.sessions.get_mut(i) {
                            s.terminal.request_focus(); // 点击标签后焦点切到终端
                        }
                    }
                    if let Some(i) = to_close {
                        // 会话仍连接（活动）则弹确认，避免误关；否则直接关闭
                        if self.sessions.get(i).map(|s| s.connected).unwrap_or(false) {
                            self.pending_close_tab = Some(i);
                        } else {
                            self.close_session(i);
                        }
                    }
                });
            });
    }

    /// 关闭活动标签前的二次确认（会话仍连接时）。
    fn close_tab_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_close_tab else { return };
        // 若该会话已不在或已断开，则无需确认
        let Some(title) = self.sessions.get(idx).filter(|s| s.connected).map(|s| s.title.clone()) else {
            self.pending_close_tab = None;
            return;
        };
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("close_tab_modal")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(crate::i18n::tr("关闭会话", "Close session")).size(16.0).strong());
                ui.add_space(6.0);
                ui.label(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("「{title}」仍在连接中，确定关闭吗？"),
                    crate::i18n::Lang::En => format!("\"{title}\" is still connected. Close it?"),
                });
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 72.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(ui, crate::i18n::tr("关闭", "Close"), Some(Palette::DANGER), bw) {
                    decision = Some(true);
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                self.close_session(idx);
                self.pending_close_tab = None;
            }
            Some(false) => self.pending_close_tab = None,
            None => {}
        }
    }

    /// 多标签文本编辑器：独立 OS 窗口（deferred viewport）。状态放在 self.editor_state（Arc<Mutex>），
    /// 回调与主 update() 共享。
    #[allow(deprecated)]
    fn editor_window(&mut self, ctx: &egui::Context) {
        // 标题随激活文件变化：锁内算好即释放，回调运行时再单独加锁（二者不同时持锁）。
        let title = {
            let mut ed = self.editor_state.lock().unwrap();
            if ed.tabs.is_empty() {
                return; // 无标签：不再注册 viewport → eframe 自动关闭该窗口
            }
            if ed.active >= ed.tabs.len() {
                ed.active = ed.tabs.len() - 1;
            }
            let active = ed.active;
            ed.tabs
                .get(active)
                .map(|t| match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("iShell 编辑器 — {}·{}", t.server, t.editor.filename()),
                    crate::i18n::Lang::En => format!("iShell Editor — {}·{}", t.server, t.editor.filename()),
                })
                .unwrap_or_else(|| crate::i18n::tr("iShell 编辑器", "iShell Editor").into())
        };
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([480.0, 320.0])
            .with_maximize_button(false);
        let vid = egui::ViewportId::from_hash_of("ishell_editor");
        let state = self.editor_state.clone();

        ctx.show_viewport_deferred(vid, builder, move |vctx, _class| {
            use egui_phosphor::regular as icon;
            let mut ed = state.lock().unwrap();
            if ed.tabs.is_empty() {
                return;
            }
            if ed.active >= ed.tabs.len() {
                ed.active = ed.tabs.len() - 1;
            }
            // 新开/切换文件后把本窗口置前并聚焦
            if ed.focus {
                vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ed.focus = false;
            }
            if vctx.egui_wants_keyboard_input() {
                vctx.request_repaint();
            }
            // Ctrl+Tab / Ctrl+Shift+Tab 切换编辑器标签（先 consume，免被文本框当作 Tab 字符）
            let n = ed.tabs.len();
            if n > 1 {
                if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)) {
                    ed.active = (ed.active + n - 1) % n;
                } else if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                    ed.active = (ed.active + 1) % n;
                }
            }
            let mut close_tab: Option<usize> = None;
            let mut activate: Option<usize> = None;
            let mut do_save = false;
            let mut toggle_find = false;
            // 标签栏：左侧可拖动重排的标签（仿主窗口，带跟手+缓动），右侧「保存 / 查找」
            egui::Panel::top("editor_tabs")
                .frame(egui::Frame::new().fill(Palette::BG).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(vctx, |ui| {
                    ui.style_mut().interaction.tooltip_delay = 0.5; // 悬停 0.5s 才弹出完整路径
                    let want_scroll = ed.active != ed.shown;
                    ui.horizontal(|ui| {
                        // 固定行高并整体垂直居中，保证保存/查找按钮与标签上下对齐
                        ui.set_min_height(28.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // 居中要点：不在子布局再 set_min_height（与主窗口 top_tabs 一致）。
                            if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::FLOPPY_DISK, crate::i18n::tr("保存", "Save"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                do_save = true;
                            }
                            // 查找：采用主窗口右侧按钮（flat_button）样式
                            if flat_button(ui, &RichText::new(format!("{} {}", icon::MAGNIFYING_GLASS, crate::i18n::tr("查找", "Find"))), crate::i18n::tr("查找 / 替换", "Find / replace")) {
                                toggle_find = true;
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                // 保存动画：珊瑚线上先「绿扫」(save 0→1) 跟随实际上传进度、再「珊瑚扫回」
                                // (save 1→2) 表示已保存。绿扫速度 = min(实际写入进度, 限速)，限速下每段
                                // 至少耗时 MIN_SWEEP（最快动画，避免小文件瞬移）。
                                const MIN_SWEEP: f64 = 0.32;
                                let now = ui.input(|i| i.time);
                                let mut any_saving = false;
                                // 先做可变推进：计算各标签的 save 值 [0,2] / -1，并驱动阶段切换与结束清理
                                let mut saves: Vec<f32> = Vec::with_capacity(ed.tabs.len());
                                for t in ed.tabs.iter_mut() {
                                    let save = match (t.save_at, t.save_done_at) {
                                        (Some(t0), None) => {
                                            // 绿扫阶段：实际进度（写完但无 total 时视为 1）与限速取小
                                            let actual = if t.save_total > 0 {
                                                (t.save_done as f64 / t.save_total as f64).clamp(0.0, 1.0)
                                            } else if !t.saving { 1.0 } else { 0.0 };
                                            let g = actual.min(((now - t0) / MIN_SWEEP).clamp(0.0, 1.0));
                                            if !t.saving && g >= 1.0 {
                                                t.save_done_at = Some(now); // 写完且绿扫满 → 转珊瑚扫回
                                            }
                                            any_saving = true;
                                            g as f32
                                        }
                                        (Some(_), Some(td)) => {
                                            let c = ((now - td) / MIN_SWEEP).clamp(0.0, 1.0);
                                            if c >= 1.0 {
                                                t.save_at = None; // 动画结束，清理
                                                t.save_done_at = None;
                                                t.save_done = 0;
                                                t.save_total = 0;
                                                -1.0
                                            } else {
                                                any_saving = true;
                                                (1.0 + c) as f32
                                            }
                                        }
                                        _ => -1.0,
                                    };
                                    saves.push(save);
                                }
                                let labels: Vec<(u64, String, String, f32, f32)> = ed
                                    .tabs
                                    .iter()
                                    .enumerate()
                                    .map(|(i, t)| {
                                        let dirty = if t.editor.dirty() { " ●" } else { "" };
                                        // 加载中 → 进度 [0,1]，驱动 tab 上珊瑚色进度条；否则 -1（不画）
                                        let prog = if t.load_id.is_some() { (t.load_done as f32 / t.load_total.max(1) as f32).clamp(0.0, 1.0) } else { -1.0 };
                                        (
                                            t.text_id.value(),
                                            format!("{} {}·{}{}", icon::FILE_CODE, t.server, t.editor.filename(), dirty),
                                            t.editor.path.clone(),
                                            prog,
                                            saves[i],
                                        )
                                    })
                                    .collect();
                                if any_saving {
                                    ui.ctx().request_repaint(); // 动画进行中：持续重绘推进
                                }
                                let active = ed.active;
                                // 解引用为 &mut EditorState，借用检查器才允许同时可变借用多个不相交字段
                                let edm: &mut EditorState = &mut ed;
                                let (act, cls, reord) = draggable_tabs(ui, &mut edm.tab_drag, &mut edm.tab_grab_dx, &mut edm.tab_total_w, active, want_scroll, &labels);
                                if let Some(a) = act {
                                    activate = Some(a);
                                }
                                if let Some(c) = cls {
                                    close_tab = Some(c);
                                }
                                if let Some((from, to)) = reord {
                                    if from < ed.tabs.len() && to < ed.tabs.len() {
                                        ed.tabs.swap(from, to);
                                        ed.active = if ed.active == from { to } else if ed.active == to { from } else { ed.active };
                                    }
                                }
                            });
                        });
                    });
                    ed.shown = ed.active;
                });
            if toggle_find {
                let active = ed.active;
                if let Some(t) = ed.tabs.get_mut(active) {
                    t.editor.toggle_find();
                }
            }

            // 当前标签内容（无内边距：底部状态栏、编辑区贴到窗口左右/底边，仿 VSCode）
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(0))
                .show(vctx, |ui| {
                    let active = ed.active;
                    if let Some(tab) = ed.tabs.get_mut(active) {
                        let tid = tab.text_id;
                        // 外部改动冲突横幅：保存被拒后提示，可覆盖或取消
                        if tab.save_conflict {
                            egui::Frame::new().fill(egui::Color32::from_rgb(255, 244, 220)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::WARNING, crate::i18n::tr("文件已被外部修改，未保存", "File changed externally; not saved"))).color(Palette::TEXT).size(12.0));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                                            tab.save_conflict = false;
                                        }
                                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("覆盖保存", "Overwrite")).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                                            let _ = tab.cmd_tx.send(UiCommand::WriteFile {
                                                path: tab.editor.path.clone(),
                                                content: tab.editor.content.clone(),
                                                encoding: tab.editor.encoding().to_string(),
                                                eol: tab.editor.eol(),
                                                expect_mtime: tab.editor.mtime(),
                                                force: true,
                                            });
                                            tab.save_conflict = false;
                                            tab.saving = true; // 覆盖保存进行中，屏蔽再次保存直至结果返回
                                        }
                                    });
                                });
                            });
                        }
                        if crate::ui::editor::content(ui, &mut tab.editor, tid) {
                            do_save = true;
                        }
                    }
                });

            if let Some(i) = activate {
                ed.active = i;
            }
            if do_save {
                let active = ed.active;
                // 仅在「有改动」且「上次保存已完成」时才真正保存：无改动不触发也不放动画；
                // 保存进行中（大文件耗时）屏蔽再次保存，避免用旧 mtime 重复写入被误判为外部改动。
                let should = ed.tabs.get(active).map_or(false, |t| t.editor.dirty() && !t.saving);
                if should {
                    if let Some(tab) = ed.tabs.get(active) {
                        let _ = tab.cmd_tx.send(UiCommand::WriteFile {
                            path: tab.editor.path.clone(),
                            content: tab.editor.content.clone(),
                            encoding: tab.editor.encoding().to_string(),
                            eol: tab.editor.eol(),
                            expect_mtime: tab.editor.mtime(),
                            force: false,
                        });
                    }
                    if let Some(tab) = ed.tabs.get_mut(active) {
                        tab.editor.mark_saved();
                        tab.saving = true; // 标记保存进行中，收到 FileSaved/Conflict 前屏蔽再次保存
                        // 触发标签底部珊瑚线的「绿扫→珊瑚扫」保存动画（重置进度，跟随本次写入）
                        tab.save_at = Some(vctx.input(|i| i.time));
                        tab.save_done_at = None;
                        tab.save_done = 0;
                        tab.save_total = 0;
                    }
                }
            }
            if let Some(i) = close_tab {
                // 脏标签：先弹确认（保存并关闭 / 不保存 / 取消）；干净标签直接关
                if ed.tabs.get(i).map(|t| t.editor.dirty()).unwrap_or(false) {
                    ed.close_tab_confirm = Some(i);
                } else {
                    if i < ed.tabs.len() {
                        let closed = ed.tabs.remove(i);
                        // 清除该编辑器在 egui 内存中的 TextEdit 状态（含撤销历史的文本快照）
                        vctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
                    }
                    if ed.active >= ed.tabs.len() && !ed.tabs.is_empty() {
                        ed.active = ed.tabs.len() - 1;
                    }
                    ed.trim_request = true;
                }
            }
            // 脏标签关闭确认
            if let Some(ti) = ed.close_tab_confirm {
                let name = ed.tabs.get(ti).map(|t| t.editor.filename()).unwrap_or_default();
                let mut decision = 0u8; // 1=保存并关闭 2=不保存关闭 3=取消
                egui::Modal::new(egui::Id::new("editor_tab_close_modal")).show(vctx, |ui| {
                    ui.set_width(330.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("关闭标签", "Close tab")).size(16.0).strong());
                        ui.add_space(6.0);
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{name} 有未保存的修改"),
                            crate::i18n::Lang::En => format!("{name} has unsaved changes"),
                        });
                    });
                    ui.add_space(12.0);
                    let bw = 100.0;
                    let total = bw * 3.0 + ui.spacing().item_spacing.x * 2.0;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("保存并关闭", "Save & close")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            decision = 1;
                        }
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("不保存", "Discard")).color(Palette::DANGER)).min_size(egui::vec2(bw, 0.0))).clicked() {
                            decision = 2;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            decision = 3;
                        }
                    });
                });
                if decision != 0 {
                    if decision == 1 {
                        if let Some(t) = ed.tabs.get(ti) {
                            let _ = t.cmd_tx.send(UiCommand::WriteFile { path: t.editor.path.clone(), content: t.editor.content.clone(), encoding: t.editor.encoding().to_string(), eol: t.editor.eol(), expect_mtime: t.editor.mtime(), force: false });
                        }
                        if let Some(t) = ed.tabs.get_mut(ti) {
                            t.editor.mark_saved();
                        }
                    }
                    if decision == 1 || decision == 2 {
                        if ti < ed.tabs.len() {
                            let closed = ed.tabs.remove(ti);
                            vctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
                        }
                        if ed.active >= ed.tabs.len() && !ed.tabs.is_empty() {
                            ed.active = ed.tabs.len() - 1;
                        }
                        ed.trim_request = true;
                    }
                    ed.close_tab_confirm = None;
                }
            }

            // 原生关闭按钮：若有未保存修改先拦截并确认，否则关闭全部标签
            if vctx.input(|i| i.viewport().close_requested()) {
                if ed.tabs.iter().any(|t| t.editor.dirty()) {
                    vctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ed.close_confirm = true;
                } else {
                    ed.close_all(vctx);
                }
            }
            if ed.close_confirm {
                let mut do_close = false;
                let mut cancel = false;
                egui::Modal::new(egui::Id::new("editor_close_modal")).show(vctx, |ui| {
                    ui.set_width(300.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("关闭编辑器", "Close editor")).size(16.0).strong());
                        ui.add_space(6.0);
                        ui.label(crate::i18n::tr("有未保存的修改，确定关闭吗？", "Some files have unsaved changes. Close anyway?"));
                    });
                    ui.add_space(12.0);
                    let bw = 80.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("关闭", "Close")).color(egui::Color32::WHITE)).fill(Palette::DANGER).min_size(egui::vec2(bw, 0.0))).clicked() {
                            do_close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            cancel = true;
                        }
                    });
                });
                if do_close {
                    ed.close_all(vctx);
                } else if cancel {
                    ed.close_confirm = false;
                }
            }
        });
    }

    /// 看图工具浮窗：独立可缩放窗口；滚轮以光标为锚点缩放，拖动平移。
    #[allow(deprecated)] // 同 editor_window：viewport 内用 Panel::show(ctx) 渲染根 UI
    fn image_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if self.image_tabs.is_empty() {
            return;
        }
        if self.active_image >= self.image_tabs.len() {
            self.active_image = self.image_tabs.len() - 1;
        }

        // 独立 OS 窗口（immediate viewport）：与主窗口分离，原生关闭按钮即可关闭。
        let vid = egui::ViewportId::from_hash_of("ishell_image");
        let title = self
            .image_tabs
            .get(self.active_image)
            .map(|t| {
                let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("iShell 看图 — {}·{}", t.server, fname),
                    crate::i18n::Lang::En => format!("iShell Image — {}·{}", t.server, fname),
                }
            })
            .unwrap_or_else(|| crate::i18n::tr("iShell 看图", "iShell Image").into());
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([760.0, 580.0])
            .with_min_inner_size([320.0, 240.0])
            // 同编辑器窗口：禁用最大化按钮，规避 macOS 最大化触发的幽灵窗口
            .with_maximize_button(false);

        ctx.show_viewport_immediate(vid, builder, |vctx, _class| {
            // 新开/切换图片后把本窗口置前并聚焦
            if self.image_focus {
                vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                self.image_focus = false;
            }
            // Ctrl+Tab / Ctrl+Shift+Tab 切换看图标签
            let n = self.image_tabs.len();
            if n > 1 {
                if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)) {
                    self.active_image = (self.active_image + n - 1) % n;
                } else if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                    self.active_image = (self.active_image + 1) % n;
                }
            }
            let mut close_tab: Option<usize> = None;
            let mut activate: Option<usize> = None;
            let mut save_msg: Option<String> = None;
            let mut do_fit = false;
            let mut do_one = false;
            let mut do_save_as = false;

            // 标签栏（仿编辑器/主窗口：左侧可拖动重排的标签，右侧操作按钮，整体垂直居中）
            egui::Panel::top("image_tabs")
                .frame(egui::Frame::new().fill(Palette::BG).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(vctx, |ui| {
                    ui.style_mut().interaction.tooltip_delay = 0.5; // 悬停 0.5s 显示完整路径
                    let want_scroll = self.active_image != self.image_shown;
                    ui.horizontal(|ui| {
                        ui.set_min_height(28.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // 另存为=主操作(珊瑚填充，对齐编辑器「保存」)；1:1/适应窗口=扁平按钮(对齐「查找」)
                            if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::FLOPPY_DISK, crate::i18n::tr("另存为", "Save as"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                do_save_as = true;
                            }
                            if flat_button(ui, &RichText::new("1:1"), crate::i18n::tr("原始大小", "Actual size")) {
                                do_one = true;
                            }
                            if flat_button(ui, &RichText::new(crate::i18n::tr("适应窗口", "Fit")), crate::i18n::tr("适应窗口", "Fit to window")) {
                                do_fit = true;
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                let labels: Vec<(u64, String, String, f32, f32)> = self
                                    .image_tabs
                                    .iter()
                                    .map(|t| {
                                        let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                                        (
                                            egui::Id::new((t.uid, &t.path)).value(),
                                            format!("{} {}·{}", icon::IMAGE, t.server, fname),
                                            t.path.clone(),
                                            -1.0, // 图片标签无下载进度条
                                            -1.0, // 图片标签无保存动画
                                        )
                                    })
                                    .collect();
                                let active = self.active_image;
                                let (act, cls, reord) = draggable_tabs(ui, &mut self.img_tab_drag, &mut self.img_grab_dx, &mut self.img_total_w, active, want_scroll, &labels);
                                if let Some(a) = act {
                                    activate = Some(a);
                                }
                                if let Some(c) = cls {
                                    close_tab = Some(c);
                                }
                                if let Some((from, to)) = reord {
                                    if from < self.image_tabs.len() && to < self.image_tabs.len() {
                                        self.image_tabs.swap(from, to);
                                        self.active_image = if self.active_image == from { to } else if self.active_image == to { from } else { self.active_image };
                                    }
                                }
                            });
                        });
                    });
                    self.image_shown = self.active_image;
                });

            // 底部状态栏（仿编辑器：贴窗口左右/底边；左侧尺寸/缩放，右侧文件名）
            egui::Panel::bottom("image_status")
                .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin { left: 8, right: 8, top: 2, bottom: 2 }))
                .show(vctx, |ui| {
                    if let Some(t) = self.image_tabs.get(self.active_image) {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}×{}", t.size.x as i32, t.size.y as i32)).color(Palette::TEXT_DIM).size(11.0));
                            if t.zoom > 0.0 {
                                ui.label(RichText::new("·").color(Palette::TEXT_DIM).size(11.0));
                                ui.label(RichText::new(format!("{}%", (t.zoom * 100.0).round() as i32)).color(Palette::TEXT_DIM).size(11.0));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                                ui.label(RichText::new(fname).color(Palette::TEXT_DIM).size(11.0)).on_hover_text(t.path.as_str());
                            });
                        });
                    }
                });

            // 画布（贴窗口边）：灰底 + 图片，滚轮以光标为锚缩放、拖动平移、双击适应窗口
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(0))
                .show(vctx, |ui| {
                    if let Some(t) = self.image_tabs.get_mut(self.active_image) {
                        let avail = ui.available_size();
                        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(rect, 0.0, Palette::PANEL_2);
                        if resp.double_clicked() {
                            t.zoom = 0.0;
                            t.offset = egui::Vec2::ZERO;
                        }
                        if t.zoom <= 0.0 {
                            let fit = (rect.width() / t.size.x).min(rect.height() / t.size.y).min(1.0);
                            t.zoom = fit.clamp(0.02, 32.0);
                            t.offset = egui::Vec2::ZERO;
                        }
                        if resp.hovered() {
                            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
                            if scroll_y != 0.0 {
                                let old = t.zoom;
                                let new = (old * (scroll_y * 0.0015).exp()).clamp(0.02, 32.0);
                                if let Some(ptr) = resp.hover_pos() {
                                    let d = ptr - rect.center();
                                    let k = new / old;
                                    t.offset = d * (1.0 - k) + t.offset * k;
                                }
                                t.zoom = new;
                            }
                        }
                        if resp.dragged() {
                            t.offset += resp.drag_delta();
                        }
                        let disp = t.size * t.zoom;
                        let center = rect.center() + t.offset;
                        let img_rect = egui::Rect::from_center_size(center, disp);
                        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        painter.image(t.tex.id(), img_rect, uv, egui::Color32::WHITE);
                    }
                });

            if let Some(i) = activate {
                self.active_image = i;
            }
            // 应用标签栏按钮（在标签栏闭包外执行，避免与遍历 image_tabs 的不可变借用冲突）
            let active = self.active_image;
            if do_fit {
                if let Some(t) = self.image_tabs.get_mut(active) {
                    t.zoom = 0.0;
                    t.offset = egui::Vec2::ZERO;
                }
            }
            if do_one {
                if let Some(t) = self.image_tabs.get_mut(active) {
                    t.zoom = 1.0;
                    t.offset = egui::Vec2::ZERO;
                }
            }
            if do_save_as {
                if let Some(t) = self.image_tabs.get(active) {
                    if !t.data.is_empty() {
                        let fname = t.path.rsplit('/').next().unwrap_or("image").to_string();
                        let data = t.data.clone();
                        if let Some(path) = rfd::FileDialog::new().set_file_name(&fname).save_file() {
                            save_msg = Some(match std::fs::write(&path, &data) {
                                Ok(_) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已保存到 {}", path.display()), crate::i18n::Lang::En => format!("Saved to {}", path.display()) },
                                Err(e) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("保存失败：{e}"), crate::i18n::Lang::En => format!("Save failed: {e}") },
                            });
                        }
                    }
                }
            }
            // 方向键切换上一张/下一张（本窗口聚焦时即可，独立窗口不会抢占主窗口按键）
            let nav_delta = vctx.input(|i| {
                i.key_pressed(egui::Key::ArrowRight) as i32 - i.key_pressed(egui::Key::ArrowLeft) as i32
            });
            if nav_delta != 0 && !self.image_tabs.is_empty() {
                let n = self.image_tabs.len() as i32;
                self.active_image = (self.active_image as i32 + nav_delta).rem_euclid(n) as usize;
            }
            if let Some(msg) = save_msg {
                if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                    s.status = msg;
                }
            }
            if let Some(i) = close_tab {
                if i < self.image_tabs.len() {
                    self.image_tabs.remove(i); // 丢弃 TextureHandle 即释放 GPU 纹理
                }
                if self.active_image >= self.image_tabs.len() && !self.image_tabs.is_empty() {
                    self.active_image = self.image_tabs.len() - 1;
                }
                self.trim_after = Some(4);
            }

            // 原生关闭按钮 → 关闭看图工具（清空全部图片）
            if vctx.input(|i| i.viewport().close_requested()) {
                self.image_tabs.clear();
                self.active_image = 0;
                self.trim_after = Some(4);
            }
        });
    }

    /// GPU 详情小窗：每块 GPU 使用率 + 显存；点击窗口外任意处或点 X 关闭（不随鼠标移开关闭）。
    fn gpu_popup_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        let Some(pos) = self.gpu_popup else { return };
        // 取活动会话的 GPU 列表（克隆，避免借用冲突）
        let gpus = self
            .active
            .and_then(|i| self.sessions.get(i))
            .and_then(|s| s.sysinfo.as_ref())
            .map(|si| si.gpus.clone())
            .unwrap_or_default();
        if gpus.is_empty() {
            self.gpu_popup = None;
            return;
        }
        let mut close = false;
        let win = egui::Window::new("gpu_popup")
            .title_bar(false)
            // 略微上移，让窗口左上角靠近点击点
            .fixed_pos(pos - egui::vec2(10.0, 10.0))
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 同进程详情窗：定宽使标题行/分割线与内容同宽对齐
                ui.set_width(300.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::CPU, crate::i18n::tr("GPU 详情", "GPU"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                // 自绘条（与侧栏 meter_row 同款）：暖灰轨道 + 近实色填充，文字浮于条上
                let bar_line = |ui: &mut egui::Ui, pct: f32, color: egui::Color32, text: String| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), Sense::hover());
                    let p = ui.painter_at(rect);
                    p.rect_filled(rect, 2.0, Palette::TRACK);
                    let mut fill = rect;
                    fill.set_width((rect.width() * (pct / 100.0).clamp(0.0, 1.0)).max(3.0));
                    p.rect_filled(fill, 2.0, egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 190));
                    p.text(rect.left_center() + egui::vec2(6.0, 0.0), egui::Align2::LEFT_CENTER, text,
                        egui::FontId::proportional(10.0), Palette::TEXT);
                };
                for g in &gpus {
                    ui.label(RichText::new(format!("GPU{} {}", g.index, g.name)).size(12.0).color(Palette::TEXT));
                    let mem_pct = if g.mem_total_mb > 0 { g.mem_used_mb as f32 / g.mem_total_mb as f32 * 100.0 } else { 0.0 };
                    bar_line(ui, g.util, crate::ui::usage_color(g.util),
                        match crate::i18n::current() { crate::i18n::Lang::Zh => format!("使用率 {:.0}%", g.util), crate::i18n::Lang::En => format!("Util {:.0}%", g.util) });
                    bar_line(ui, mem_pct, Palette::ACCENT,
                        match crate::i18n::current() { crate::i18n::Lang::Zh => format!("显存 {}/{} MB", g.mem_used_mb, g.mem_total_mb), crate::i18n::Lang::En => format!("VRAM {}/{} MB", g.mem_used_mb, g.mem_total_mb) });
                    ui.add_space(5.0);
                }
            });

        // 点击窗口外任意处或点 X 关闭（打开当帧除外）；不再因鼠标移开而关闭
        let outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close || (outside && !self.gpu_popup_just_opened) {
            self.gpu_popup = None;
        }
        self.gpu_popup_just_opened = false;
    }

    /// 进程详情小窗：显示资源/目录/命令 + 强制结束；点击外部关闭。
    fn proc_popup_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        let (pid, name, cpu, mem, pos, cmd, cwd, exe) = match &self.proc_popup {
            Some(p) => (p.pid, p.name.clone(), p.cpu, p.mem, p.pos, p.cmd.clone(), p.cwd.clone(), p.exe.clone()),
            None => return,
        };
        let mut close = false;
        let mut kill = false; // 真正下发 KillProc（仅二次确认后置 true）
        let mut arm_kill = false; // 点击「强制结束」按钮 → 进入确认态
        let mut cancel_kill = false; // 确认态里点「取消」→ 退回
        let mut copy_target: Option<String> = None;
        let copied_t = self.proc_popup.as_ref().and_then(|p| p.copied_t);
        let confirm_kill = self.proc_popup.as_ref().map(|p| p.confirm_kill).unwrap_or(false);
        let now = ctx.input(|i| i.time);
        let win = egui::Window::new("proc_popup")
            .title_bar(false)
            .fixed_pos(pos + egui::vec2(8.0, 8.0))
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 固定内容宽度：自适应收缩窗口里，先布局的标题行/分割线取的是「当时估计宽度」，
                // 会被后续更宽的行（长命令）撑开而不跟随；定宽让所有行按同一宽度对齐
                ui.set_width(320.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::CPU, name)).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(k).color(Palette::TEXT_DIM).size(12.0));
                        ui.label(RichText::new(v).color(Palette::TEXT).size(12.0).monospace());
                    });
                };
                let tip = crate::i18n::tr("双击复制", "Double-click to copy");
                // PID：值可双击复制
                ui.horizontal(|ui| {
                    ui.label(RichText::new("PID").color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(pid.to_string()).color(Palette::TEXT).size(12.0).monospace()).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                        copy_target = Some(pid.to_string());
                    }
                });
                kv(ui, "CPU", format!("{cpu:.1}%"));
                kv(ui, crate::i18n::tr("内存", "Mem"), format!("{mem:.1}%"));
                // 程序 / 目录：值可双击复制
                if !exe.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("程序", "Exe")).color(Palette::TEXT_DIM).size(12.0));
                        if ui.add(egui::Label::new(RichText::new(&exe).color(Palette::TEXT).size(12.0).monospace()).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                            copy_target = Some(exe.clone());
                        }
                    });
                }
                if !cwd.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("目录", "Dir")).color(Palette::TEXT_DIM).size(12.0));
                        if ui.add(egui::Label::new(RichText::new(&cwd).color(Palette::TEXT).size(12.0).monospace()).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                            copy_target = Some(cwd.clone());
                        }
                    });
                }
                // 命令：可双击复制
                if cmd.is_empty() {
                    ui.label(RichText::new(crate::i18n::tr("（正在获取命令…）", "(loading command…)")).color(Palette::TEXT_DIM).size(11.0));
                } else {
                    ui.add_space(2.0);
                    ui.label(RichText::new(crate::i18n::tr("命令", "Command")).color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(&cmd).size(11.5).monospace().color(Palette::TEXT)).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                        copy_target = Some(cmd.clone());
                    }
                }
                // 「已复制」短暂提示
                if let Some(t) = copied_t {
                    if now - t < 1.3 {
                        ui.add_space(2.0);
                        ui.label(RichText::new(format!("{}  {}", icon::CHECK_CIRCLE, crate::i18n::tr("已复制", "Copied"))).color(Palette::OK).size(11.0));
                    }
                }
                ui.separator();
                if !confirm_kill {
                    // 第一步：仅「武装」确认，不立即结束
                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::SKULL, crate::i18n::tr("强制结束 (kill -9)", "Kill (-9)"))).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                        arm_kill = true;
                    }
                } else {
                    // 第二步：二次确认（破坏性、不可撤销）——确认 / 取消
                    ui.label(RichText::new(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("确定强制结束 PID {pid}（{name}）？此操作不可撤销。"),
                        crate::i18n::Lang::En => format!("Kill PID {pid} ({name})? This cannot be undone."),
                    }).color(Palette::TEXT).size(12.0));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::SKULL, crate::i18n::tr("确认结束", "Confirm"))).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                            kill = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            cancel_kill = true;
                        }
                    });
                }
            });

        let outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        // 双击复制 -> 写剪贴板并记录时间（显示「已复制」）
        if let Some(v) = copy_target {
            ctx.copy_text(v);
            if let Some(p) = &mut self.proc_popup {
                p.copied_t = Some(now);
            }
            ctx.request_repaint();
        }
        // 让「已复制」提示到点自动消失
        if let Some(t) = copied_t {
            if now - t < 1.4 {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
        // 进入/退出「强制结束」确认态（不关窗）
        if arm_kill {
            if let Some(p) = &mut self.proc_popup {
                p.confirm_kill = true;
            }
        }
        if cancel_kill {
            if let Some(p) = &mut self.proc_popup {
                p.confirm_kill = false;
            }
        }
        if kill {
            // 发往「打开弹窗时所属会话」(uid)，而非当前 active——避免 Ctrl+Tab 切走后误 kill 别的主机
            let target = self.proc_popup.as_ref().and_then(|p| self.session_idx_by_uid(p.uid));
            if let Some(i) = target {
                let _ = self.sessions[i].cmd_tx.send(UiCommand::KillProc(pid));
            }
            self.proc_popup = None;
        } else if close || (outside && !self.proc_popup_just_opened && !arm_kill) {
            // 注：arm_kill 当帧不因「点到按钮算窗外」而误关（按钮在窗内，理论上 outside=false，这里再加一道保险）
            self.proc_popup = None;
        }
        self.proc_popup_just_opened = false;
    }

    /// 端口转发管理浮窗（右上角弹出，样式与传输浮窗一致）。
    fn forward_window(&mut self, ctx: &egui::Context) {
        use crate::proto::{ForwardKind, ForwardSpec};
        use egui_phosphor::regular as icon;
        if !self.show_forwards {
            return;
        }
        let idx = self.active.filter(|&i| i < self.sessions.len());
        let mut add_spec: Option<ForwardSpec> = None;
        let mut remove_id: Option<u64> = None; // 确认后真正删除
        let mut arm_del: Option<u64> = None; // 点垃圾桶 → 进入该行确认态
        let mut cancel_del = false; // 确认态点取消
        let mut edit_id: Option<u64> = None; // 点铅笔 → 把该条回填表单进入编辑
        let mut cancel_edit = false; // 编辑态点「取消编辑」
        let confirm_del = self.fwd_confirm_del; // 本帧处于确认态的转发 id（快照）
        let mut close_win = false;

        let win = egui::Window::new("forward_win")
            .title_bar(false)
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(340.0)
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::ARROWS_LEFT_RIGHT, crate::i18n::tr("端口转发", "Port forward"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                    });
                });
                ui.separator();

                let Some(idx) = idx else {
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("请先连接一个会话", "Connect a session first")).color(Palette::TEXT_DIM).size(12.0));
                    return;
                };

                // 新增/编辑表单（分段按钮代替下拉，避免点击下拉被判为窗口外而自动关闭）
                let editing = self.fwd_editing; // 快照：编辑态决定按钮文案与提交语义
                let fwd_error = self.fwd_error.clone(); // 快照：内联错误（避免与 f 的可变借用冲突）
                let f = &mut self.fwd_form;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut f.kind, 0usize, crate::i18n::tr("本地转发", "Local"));
                    ui.selectable_value(&mut f.kind, 1usize, crate::i18n::tr("动态 SOCKS5", "Dynamic SOCKS5"));
                });
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::tr("本地", "Local"));
                    ui.add(egui::TextEdit::singleline(&mut f.bind).desired_width(84.0).hint_text("127.0.0.1"));
                    ui.label(":");
                    ui.add(egui::TextEdit::singleline(&mut f.local_port).desired_width(48.0).hint_text(crate::i18n::tr("端口", "Port")));
                });
                if f.kind == 0 {
                    ui.horizontal(|ui| {
                        ui.label(crate::i18n::tr("目标", "Target"));
                        ui.add(egui::TextEdit::singleline(&mut f.target_host).desired_width(120.0).hint_text(crate::i18n::tr("主机/IP", "Host/IP")));
                        ui.label(":");
                        ui.add(egui::TextEdit::singleline(&mut f.target_port).desired_width(48.0).hint_text(crate::i18n::tr("端口", "Port")));
                    });
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let (btn_icon, btn_label) = if editing.is_some() {
                        (icon::CHECK, crate::i18n::tr("保存修改", "Save"))
                    } else {
                        (icon::PLUS, crate::i18n::tr("添加转发", "Add forward"))
                    };
                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", btn_icon, btn_label)).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                        if let Ok(lp) = f.local_port.trim().parse::<u16>() {
                            let kind = if f.kind == 0 {
                                match f.target_port.trim().parse::<u16>() {
                                    Ok(tp) if !f.target_host.trim().is_empty() => {
                                        Some(ForwardKind::Local { remote_host: f.target_host.trim().to_string(), remote_port: tp })
                                    }
                                    _ => None,
                                }
                            } else {
                                Some(ForwardKind::Dynamic)
                            };
                            if let Some(kind) = kind {
                                let bind = if f.bind.trim().is_empty() { "127.0.0.1".into() } else { f.bind.trim().to_string() };
                                add_spec = Some(ForwardSpec { id: 0, bind_host: bind, bind_port: lp, kind });
                            }
                        }
                    }
                    // 编辑态：提供「取消编辑」退回新增态
                    if editing.is_some() && ui.button(crate::i18n::tr("取消编辑", "Cancel")).clicked() {
                        cancel_edit = true;
                    }
                });
                // 内联错误（端口占用 / 参数无效）
                if let Some(err) = &fwd_error {
                    ui.label(RichText::new(err).color(Palette::DANGER).size(11.0));
                }

                ui.separator();
                if let Some(s) = self.sessions.get(idx) {
                    if s.forwards.is_empty() {
                        crate::ui::empty_state(ui, egui_phosphor::regular::ARROWS_LEFT_RIGHT, crate::i18n::tr("暂无转发任务", "No forwards"), false);
                    }
                    for fwd in &s.forwards {
                        ui.horizontal(|ui| {
                            let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, if fwd.ok { Palette::OK } else { Palette::DANGER });
                            ui.label(RichText::new(&fwd.label).size(12.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if confirm_del == Some(fwd.id) {
                                    // 行内二次确认：确认（红勾）/ 取消（X）
                                    if ui.add(egui::Button::new(RichText::new(icon::CHECK).size(12.0).color(Palette::DANGER)).frame(false)).on_hover_text(crate::i18n::tr("确认删除", "Confirm delete")).clicked() {
                                        remove_id = Some(fwd.id);
                                    }
                                    if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("取消", "Cancel")).clicked() {
                                        cancel_del = true;
                                    }
                                } else {
                                    if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除", "Delete")).clicked() {
                                        arm_del = Some(fwd.id);
                                    }
                                    // 编辑：把该条参数回填表单
                                    if ui.add(egui::Button::new(RichText::new(icon::PENCIL_SIMPLE).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("编辑", "Edit")).clicked() {
                                        edit_id = Some(fwd.id);
                                    }
                                }
                            });
                        });
                        ui.label(RichText::new(&fwd.status).color(if fwd.ok { Palette::TEXT_DIM } else { Palette::DANGER }).size(10.5));
                        ui.add_space(3.0);
                    }
                }
            });

        // 行内删除确认态的进入/退出
        if let Some(id) = arm_del {
            self.fwd_confirm_del = Some(id);
        }
        if cancel_del || remove_id.is_some() {
            self.fwd_confirm_del = None;
        }

        // 点击窗口外部自动隐藏（打开当帧除外）
        let clicked_outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close_win || (clicked_outside && !self.fwd_just_opened) {
            self.show_forwards = false;
            self.fwd_confirm_del = None; // 关窗时复位确认态，避免下次打开仍处于「确认删除」
            self.fwd_editing = None; // 复位编辑态与内联错误
            self.fwd_error = None;
        }
        self.fwd_just_opened = false;

        let idx = match idx {
            Some(i) => i,
            None => return,
        };
        // 取消编辑：复位编辑态与表单
        if cancel_edit {
            self.fwd_editing = None;
            self.fwd_error = None;
            self.fwd_form.local_port.clear();
            self.fwd_form.target_host.clear();
            self.fwd_form.target_port.clear();
            self.fwd_just_opened = true;
        }
        // 进入编辑：把选中转发的参数回填表单
        if let Some(id) = edit_id {
            if let Some(fwd) = self.sessions.get(idx).and_then(|s| s.forwards.iter().find(|f| f.id == id)) {
                let (bh, bp, kind) = (fwd.bind_host.clone(), fwd.bind_port, fwd.kind.clone());
                let form = &mut self.fwd_form;
                form.bind = bh;
                form.local_port = bp.to_string();
                match kind {
                    ForwardKind::Local { remote_host, remote_port } => {
                        form.kind = 0;
                        form.target_host = remote_host;
                        form.target_port = remote_port.to_string();
                    }
                    ForwardKind::Dynamic => {
                        form.kind = 1;
                        form.target_host.clear();
                        form.target_port.clear();
                    }
                }
                self.fwd_editing = Some(id);
                self.fwd_error = None;
            }
            self.fwd_just_opened = true; // 点编辑不算窗外点击
        }
        // 添加 / 保存：先做本地端口占用 + 重复校验，通过才发起（编辑则先删旧再加新）
        if let Some(mut spec) = add_spec {
            let editing = self.fwd_editing;
            // 与现有转发重复（排除正在编辑的那条），或本机端口已被占用
            let dup = self.sessions.get(idx).is_some_and(|s| {
                s.forwards
                    .iter()
                    .any(|f| f.bind_port == spec.bind_port && f.bind_host == spec.bind_host && Some(f.id) != editing)
            });
            // 编辑且端口与原值相同时跳过 OS 探测：那个端口正被「被编辑的转发」自身监听着，会误报占用
            let same_as_editing = editing
                .and_then(|id| self.sessions.get(idx).and_then(|s| s.forwards.iter().find(|f| f.id == id)))
                .is_some_and(|f| f.bind_port == spec.bind_port && f.bind_host == spec.bind_host);
            if dup || (!same_as_editing && local_port_in_use(&spec.bind_host, spec.bind_port)) {
                self.fwd_error = Some(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("本地端口 {} 已被占用", spec.bind_port),
                    crate::i18n::Lang::En => format!("Local port {} is already in use", spec.bind_port),
                });
                self.fwd_just_opened = true;
            } else {
                // 编辑模式：先删旧转发（移除记录 + 通知 worker 关闭监听）
                if let Some(old) = editing {
                    if let Some(s) = self.sessions.get_mut(idx) {
                        s.forwards.retain(|f| f.id != old);
                        let _ = s.cmd_tx.send(UiCommand::RemoveForward(old));
                    }
                }
                if let Some(s) = self.sessions.get_mut(idx) {
                    let id = s.next_forward;
                    s.next_forward += 1;
                    spec.id = id;
                    let label = match &spec.kind {
                        ForwardKind::Local { remote_host, remote_port } => {
                            format!("{}:{} → {}:{}", spec.bind_host, spec.bind_port, remote_host, remote_port)
                        }
                        ForwardKind::Dynamic => format!("SOCKS5 {}:{}", spec.bind_host, spec.bind_port),
                    };
                    s.forwards.push(ForwardEntry {
                        id,
                        label,
                        status: crate::i18n::tr("启动中 …", "Starting …").into(),
                        ok: true,
                        bind_host: spec.bind_host.clone(),
                        bind_port: spec.bind_port,
                        kind: spec.kind.clone(),
                    });
                    let _ = s.cmd_tx.send(UiCommand::AddForward(spec));
                }
                self.fwd_editing = None;
                self.fwd_error = None;
                self.fwd_form.local_port.clear();
                self.fwd_form.target_host.clear();
                self.fwd_form.target_port.clear();
                self.fwd_just_opened = true;
            }
        }
        if let Some(id) = remove_id {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.forwards.retain(|f| f.id != id);
                let _ = s.cmd_tx.send(UiCommand::RemoveForward(id));
            }
        }
    }

    /// 右上角传输进度浮窗（可弹出/隐藏）。
    /// 顶部居中浮层提示：数秒后淡出。用于撤销结果等需要醒目反馈的操作。
    fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some((msg, t0)) = self.toast.clone() else { return };
        const DUR: f64 = 3.5; // 显示时长（秒）
        const FADE: f64 = 0.6; // 末尾淡出时长
        let now = ctx.input(|i| i.time);
        let age = now - t0;
        if age >= DUR {
            self.toast = None;
            return;
        }
        let alpha = if age > DUR - FADE { ((DUR - age) / FADE) as f32 } else { 1.0 }.clamp(0.0, 1.0);
        egui::Area::new(egui::Id::new("undo_toast"))
            .anchor(egui::Align2::CENTER_TOP, [0.0, 54.0])
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                egui::Frame::new()
                    .fill(Palette::PANEL_2)
                    .stroke(egui::Stroke::new(1.0, Palette::ACCENT))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(egui_phosphor::regular::INFO).color(Palette::ACCENT).size(15.0));
                            ui.label(RichText::new(&msg).color(Palette::TEXT).size(13.0));
                        });
                    });
            });
        ctx.request_repaint(); // 维持淡出动画
    }

    fn transfer_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if !self.show_transfers {
            return;
        }
        let Some(idx) = self.active else { return };
        // 同一服务器（host:port）的所有会话：把它们的传输任务汇总到同一个列表里展示
        let server_idxs = self.same_server_idxs(idx);
        let mut close_win = false;
        let mut clear = false;
        let mut cancel_all = false;
        let mut retry_all = false;
        let mut pick_dir = false;
        // 动作均以 (会话 uid, 传输 id) 标识，确保多标签同服务器时路由到正确的会话 worker
        let mut cancel_id: Option<(u64, u64)> = None;
        let mut toggle_err: Option<(u64, u64)> = None;
        let mut remove_id: Option<(u64, u64)> = None;
        let mut delete_id: Option<(u64, u64, String)> = None;
        let mut resume_id: Option<(u64, u64)> = None;
        let mut cycle_policy = false;
        let dl_dir = self.download_dir.to_string_lossy().into_owned();
        // 冲突策略短标签（中/英）+ 按策略区分的图标，用于标题栏按钮显示
        let policy_label = match (self.conflict_policy, crate::i18n::current()) {
            (ConflictPolicy::Overwrite, crate::i18n::Lang::Zh) => "覆盖",
            (ConflictPolicy::Skip, crate::i18n::Lang::Zh) => "跳过",
            (ConflictPolicy::Rename, crate::i18n::Lang::Zh) => "重命名",
            (ConflictPolicy::Overwrite, crate::i18n::Lang::En) => "Overwrite",
            (ConflictPolicy::Skip, crate::i18n::Lang::En) => "Skip",
            (ConflictPolicy::Rename, crate::i18n::Lang::En) => "Rename",
        };
        let policy_icon = match self.conflict_policy {
            ConflictPolicy::Overwrite => icon::SWAP,       // 覆盖=替换
            ConflictPolicy::Skip => icon::SKIP_FORWARD,    // 跳过
            ConflictPolicy::Rename => icon::PENCIL_SIMPLE, // 重命名
        };
        let win = egui::Window::new("transfer_win")
            .title_bar(false) // 隐藏过大的默认标题，使用自定义紧凑标题
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(330.0)
            .resizable(false)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(10),
            )
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("文件传输", "Transfers"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                        // 冲突策略：目标已存在时的默认处理；点击循环切换（覆盖→跳过→重命名），持久化
                        if ui
                            .add(egui::Button::new(RichText::new(format!("{} {}", policy_icon, policy_label)).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(crate::i18n::tr("目标已存在时的默认处理（点击切换：覆盖 / 跳过 / 重命名）", "Default when target exists (click to cycle: Overwrite / Skip / Rename)"))
                            .clicked()
                        {
                            cycle_policy = true;
                        }
                        if ui
                            .add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(13.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("选择默认下载文件夹\n当前：{}", dl_dir), crate::i18n::Lang::En => format!("Set default download folder\nCurrent: {}", dl_dir) })
                            .clicked()
                        {
                            pick_dir = true;
                        }
                    });
                });
                ui.separator();

                // 状态筛选：紧凑的 frameless 文字 chips（带计数），仅在有任务时显示——
                // 避免空列表时占位、也避免大按钮 + 多余分隔线让顶部拥挤。
                // 先在借用 s 之前算各状态计数（借用随即结束），再据此渲染并允许改 self.xfer_filter。
                let counts = server_idxs.iter().filter_map(|&i| self.sessions.get(i)).fold(
                    (0usize, 0usize, 0usize, 0usize),
                    |(tot, act, dn, fl), s| {
                        (
                            tot + s.transfers.len(),
                            act + s.transfers.iter().filter(|t| t.ok.is_none()).count(),
                            dn + s.transfers.iter().filter(|t| t.ok == Some(true)).count(),
                            fl + s.transfers.iter().filter(|t| t.ok == Some(false)).count(),
                        )
                    },
                );
                if counts.0 > 0 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 9.0;
                        for (f, zh, en, n) in [
                            (XferFilter::All, "全部", "All", counts.0),
                            (XferFilter::Active, "进行中", "Active", counts.1),
                            (XferFilter::Done, "已完成", "Done", counts.2),
                            (XferFilter::Failed, "失败", "Failed", counts.3),
                        ] {
                            let on = self.xfer_filter == f;
                            // 激活=强调色加粗；有失败时「失败」用危险色；其余弱色
                            let col = if on {
                                Palette::ACCENT
                            } else if matches!(f, XferFilter::Failed) && n > 0 {
                                Palette::DANGER
                            } else {
                                Palette::TEXT_DIM
                            };
                            let mut rt = RichText::new(format!("{} {}", crate::i18n::tr(zh, en), n)).size(11.0).color(col);
                            if on {
                                rt = rt.strong();
                            }
                            if ui.add(egui::Button::new(rt).frame(false).small()).clicked() {
                                self.xfer_filter = f;
                            }
                        }
                    });
                    ui.add_space(2.0);
                }
                let filter = self.xfer_filter;

                // 汇总同服务器所有会话的传输（活动会话在前，各自新→旧），元素为 (会话 uid, &Transfer)
                let items: Vec<(u64, &Transfer)> = server_idxs
                    .iter()
                    .filter_map(|&i| self.sessions.get(i))
                    .flat_map(|s| s.transfers.iter().rev().map(move |t| (s.uid, t)))
                    .collect();
                let total_len = items.len();
                if total_len == 0 {
                    ui.add_space(6.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::DOWNLOAD_SIMPLE, crate::i18n::tr("暂无传输任务", "No transfers"), false);
                }
                let mut open_dir: Option<String> = None;
                let mut shown = 0usize; // 当前筛选下实际展示的条数（用于「无匹配」提示）
                // 列表过长时滚动：约 8 条高度封顶，其余可滚动查看
                egui::ScrollArea::vertical().max_height(400.0).auto_shrink([false, true]).show(ui, |ui| {
                for (uid, t) in items.iter().copied().filter(|(_, t)| match filter {
                    XferFilter::All => true,
                    XferFilter::Active => t.ok.is_none(),
                    XferFilter::Done => t.ok == Some(true),
                    XferFilter::Failed => t.ok == Some(false),
                }).take(50) {
                    shown += 1;
                    // 下载=绿色，上传=珊瑚橙，颜色区分方向
                    let (dir_icon, dir_col) = match t.dir {
                        crate::proto::TransferDir::Download => (icon::DOWNLOAD_SIMPLE, Palette::OK),
                        crate::proto::TransferDir::Upload => (icon::UPLOAD_SIMPLE, Palette::ACCENT),
                    };
                    // 整个传输项包进一个感知点击的 scope，便于整体右键（否则右键会穿透到下方终端）
                    let item = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(dir_icon).color(dir_col).size(13.0));
                            ui.label(RichText::new(&t.name).size(12.0).color(Palette::TEXT));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                match t.ok {
                                    Some(true) => {
                                        ui.label(RichText::new(icon::CHECK_CIRCLE).color(Palette::OK).size(13.0));
                                        // 下载完成：保留「打开所在文件夹」按钮
                                        if let Some(local) = &t.local {
                                            if ui.add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(12.0).color(Palette::TEXT_DIM)).frame(false))
                                                .on_hover_text(crate::i18n::tr("在文件管理器中显示", "Show in file manager"))
                                                .clicked()
                                            {
                                                open_dir = Some(local.clone());
                                            }
                                        }
                                    }
                                    Some(false) => {
                                        // 失败：可重试（有重发规格时）+ 状态按钮展开原因
                                        if ui.add(egui::Button::new(RichText::new(icon::WARNING_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("点击查看失败原因", "Click for reason"))
                                            .clicked()
                                        {
                                            toggle_err = Some((uid, t.id));
                                        }
                                        if t.spec.is_some()
                                            && ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                                .on_hover_text(crate::i18n::tr("重试", "Retry"))
                                                .clicked()
                                        {
                                            resume_id = Some((uid, t.id));
                                        }
                                    }
                                    None if t.paused => {
                                        // 已中断/暂停：续传按钮
                                        if ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("续传", "Resume"))
                                            .clicked()
                                        {
                                            resume_id = Some((uid, t.id));
                                        }
                                    }
                                    None if t.queued => {
                                        // 等待态（中转目标端，正等源端下载）：时钟图标 + 转圈，不提供取消（受中转任务管控）
                                        ui.label(RichText::new(icon::CLOCK).color(Palette::TEXT_DIM).size(13.0))
                                            .on_hover_text(crate::i18n::tr("等待中", "Waiting"));
                                        ui.spinner();
                                    }
                                    None => {
                                        // 进行中：取消按钮 + 转圈
                                        if ui.add(egui::Button::new(RichText::new(icon::X_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("取消", "Cancel"))
                                            .clicked()
                                        {
                                            cancel_id = Some((uid, t.id));
                                        }
                                        ui.spinner();
                                    }
                                }
                            });
                        });
                        let done = t.ok == Some(true);
                        let frac = if done { 1.0 } else if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
                        let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as i32;
                        // 进行中/失败：条上居中显示百分比；完成：条上分两端（大小靠左、100% 靠右）
                        let mut bar = egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                            .fill(dir_col)
                            .desired_height(10.0)
                            .corner_radius(2.0);
                        if !done {
                            bar = bar.text(RichText::new(format!("{pct}%")).size(10.0));
                        }
                        let bar_resp = ui.add(bar);
                        if done {
                            let rect = bar_resp.rect;
                            let p = ui.painter_at(rect);
                            let font = egui::FontId::proportional(10.0);
                            // 大小靠左；有模式徽标（如「直传」）时显示在文件大小之后
                            let left_label = if t.tag.is_empty() {
                                crate::ui::fmt_bytes(t.total as f64)
                            } else {
                                format!("{} · {}", crate::ui::fmt_bytes(t.total as f64), t.tag)
                            };
                            p.text(
                                egui::pos2(rect.left() + 6.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                left_label,
                                font.clone(),
                                egui::Color32::WHITE,
                            );
                            // 100% 靠右
                            p.text(
                                egui::pos2(rect.right() - 6.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                "100%",
                                font,
                                egui::Color32::WHITE,
                            );
                        }
                        // 进行中才显示详情行（已传/总量 + 实时速度）；完成后不再单独一行
                        if t.ok.is_none() {
                            // 有阶段提示（打包/解包/等待/直传）时优先显示提示，替代字节读数——
                            // 这些阶段没有逐字节进度，显示「0 B / 0 B」会误导。
                            if !t.note.is_empty() {
                                ui.label(RichText::new(&t.note).size(10.0).color(Palette::TEXT_DIM));
                            } else {
                                let mut detail = format!("{} / {}", crate::ui::fmt_bytes(t.done as f64), crate::ui::fmt_bytes(t.total as f64));
                                // 模式徽标（如「直传」）紧跟在文件大小之后
                                if !t.tag.is_empty() {
                                    detail.push_str(&format!("  ·  {}", t.tag));
                                }
                                if t.speed > 0.0 {
                                    detail.push_str(&format!("  ·  {}", crate::ui::fmt_rate(t.speed)));
                                    // ETA：剩余字节 / 当前速度（仅未暂停、有剩余、速度有效时）
                                    if !t.paused && t.total > t.done {
                                        let eta = ((t.total - t.done) as f64 / t.speed).round() as u64;
                                        detail.push_str(&format!("  ·  {} {}", crate::i18n::tr("剩余", "ETA"), fmt_dur(eta)));
                                    }
                                }
                                ui.label(RichText::new(detail).size(10.0).color(Palette::TEXT_DIM));
                            }
                        }
                        // 失败且已展开：显示失败原因
                        if t.ok == Some(false) && t.show_err && !t.message.is_empty() {
                            ui.label(RichText::new(&t.message).color(Palette::DANGER).size(11.0));
                        }
                    });
                    // 右键菜单：打开所在文件 / 删除记录 / 删文件并删记录
                    item.response.context_menu(|ui| {
                        if let Some(local) = &t.local {
                            if ui.button(crate::i18n::tr("打开所在文件", "Reveal file")).clicked() {
                                open_dir = Some(local.clone());
                                ui.close();
                            }
                        }
                        // 仅「已完成/失败」的行可移除；进行中/等待中的行移除会让其追踪任务
                        // （直传 DirectJob / 中转 Relay）永久卡住 poll 不存在的 id，故不提供
                        if t.ok.is_some() {
                            if ui.button(crate::i18n::tr("删除记录", "Remove from list")).clicked() {
                                remove_id = Some((uid, t.id));
                                ui.close();
                            }
                            if let Some(local) = &t.local {
                                if ui.button(RichText::new(crate::i18n::tr("删除文件并移除记录", "Delete file & remove")).color(Palette::DANGER)).clicked() {
                                    delete_id = Some((uid, t.id, local.clone()));
                                    ui.close();
                                }
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                // 有任务但当前筛选下一条都没有：给出「无匹配」提示，避免看着像空列表
                if shown == 0 && total_len > 0 {
                    ui.add_space(6.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::MAGNIFYING_GLASS, crate::i18n::tr("该筛选下暂无任务", "No transfers match this filter"), false);
                }
                });
                if let Some(p) = open_dir {
                    open_containing_folder(&p);
                }
                if total_len > 0 {
                    ui.separator();
                    // 批量操作：仅在对应状态存在时显示，避免无意义按钮
                    let any_active = items.iter().any(|(_, t)| t.ok.is_none());
                    let any_failed = items.iter().any(|(_, t)| t.ok == Some(false) && t.spec.is_some());
                    let any_done = items.iter().any(|(_, t)| t.ok.is_some());
                    ui.horizontal(|ui| {
                        if any_active && ui.button(crate::i18n::tr("全部取消", "Cancel all")).clicked() {
                            cancel_all = true;
                        }
                        if any_failed && ui.button(crate::i18n::tr("重试失败", "Retry failed")).clicked() {
                            retry_all = true;
                        }
                        if any_done && ui.button(crate::i18n::tr("清除已完成", "Clear done")).clicked() {
                            clear = true;
                        }
                    });
                }
            });
        if clear {
            for &i in &server_idxs {
                if let Some(s) = self.sessions.get_mut(i) {
                    s.transfers.retain(|t| t.ok.is_none());
                }
            }
        }
        // 全部取消：对同服务器所有会话里进行中的任务下发取消
        if cancel_all {
            // 跳过 queued 占位行（worker 未登记）；镜像行经 cancel_target 转到源端真实传输
            let raw: Vec<(u64, u64)> = server_idxs
                .iter()
                .filter_map(|&i| self.sessions.get(i))
                .flat_map(|s| s.transfers.iter().filter(|t| t.ok.is_none() && !t.queued).map(move |t| (s.uid, t.id)))
                .collect();
            for (uid, id) in raw {
                let (tu, ti) = self.cancel_target(uid, id);
                if let Some(s) = self.session_idx_by_uid(tu).and_then(|i| self.sessions.get(i)) {
                    let _ = s.cmd_tx.send(UiCommand::CancelTransfer(ti));
                }
            }
            self.xfer_just_opened = true;
        }
        // 重试全部失败：对同服务器各会话每个有重发规格的失败任务重新发起（续传语义，覆盖）
        if retry_all {
            for &i in &server_idxs {
                if let Some(s) = self.sessions.get_mut(i) {
                    let targets: Vec<(u64, XferSpec)> = s
                        .transfers
                        .iter()
                        .filter(|t| t.ok == Some(false))
                        .filter_map(|t| t.spec.clone().map(|sp| (t.id, sp)))
                        .collect();
                    for (id, spec) in targets {
                        match spec {
                            XferSpec::Download { remote, local } => {
                                let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy: ConflictPolicy::Overwrite });
                            }
                            XferSpec::Upload { local, remote_dir } => {
                                let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy: ConflictPolicy::Overwrite });
                            }
                        }
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None;
                            t.paused = false;
                            t.show_err = false;
                            t.message = crate::i18n::tr("重试中 …", "Retrying …").into();
                        }
                    }
                }
            }
            self.xfer_just_opened = true;
        }
        // 取消传输：镜像行/源行都经 cancel_target 路由到源端真实传输并标记 cancelled
        if let Some((uid, id)) = cancel_id {
            let (tu, ti) = self.cancel_target(uid, id);
            if let Some(s) = self.session_idx_by_uid(tu).and_then(|i| self.sessions.get(i)) {
                let _ = s.cmd_tx.send(UiCommand::CancelTransfer(ti));
            }
            self.xfer_just_opened = true; // 避免点击被当作窗外点击而关窗
        }
        // 续传/重试：按重发规格重新发起，底层据已有字节自动续传
        if let Some((uid, id)) = resume_id {
            if let Some(i) = self.session_idx_by_uid(uid) {
                let s = &mut self.sessions[i];
                if let Some(spec) = s.transfers.iter().find(|t| t.id == id).and_then(|t| t.spec.clone()) {
                    match spec {
                        XferSpec::Download { remote, local } => { let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy: ConflictPolicy::Overwrite }); }
                        XferSpec::Upload { local, remote_dir } => { let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy: ConflictPolicy::Overwrite }); }
                    }
                    if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                        t.ok = None;
                        t.paused = false;
                        t.show_err = false;
                        t.message = crate::i18n::tr("续传中 …", "Resuming …").into();
                    }
                }
            }
            self.xfer_just_opened = true;
        }
        // 切换失败原因展开
        if let Some((uid, id)) = toggle_err {
            if let Some(i) = self.session_idx_by_uid(uid) {
                if let Some(t) = self.sessions[i].transfers.iter_mut().find(|t| t.id == id) {
                    t.show_err = !t.show_err;
                }
            }
            self.xfer_just_opened = true;
        }
        // 删除记录（仅移除列表项）
        if let Some((uid, id)) = remove_id {
            if let Some(i) = self.session_idx_by_uid(uid) {
                self.sessions[i].transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 删除文件并移除记录
        if let Some((uid, id, path)) = delete_id {
            let _ = std::fs::remove_file(&path);
            if let Some(i) = self.session_idx_by_uid(uid) {
                self.sessions[i].transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 选择默认下载目录（原生文件夹选择器）
        if pick_dir {
            if let Some(dir) = rfd::FileDialog::new().set_title(crate::i18n::tr("选择默认下载文件夹", "Select default download folder")).pick_folder() {
                self.download_dir = dir.clone();
                crate::store::save_download_dir(&dir.to_string_lossy());
            }
            self.xfer_just_opened = true; // 选择期间点击不算"外部点击"，避免关窗
        }
        // 冲突策略循环切换：覆盖 → 跳过 → 重命名 → 覆盖，并持久化
        if cycle_policy {
            self.conflict_policy = match self.conflict_policy {
                ConflictPolicy::Overwrite => ConflictPolicy::Skip,
                ConflictPolicy::Skip => ConflictPolicy::Rename,
                ConflictPolicy::Rename => ConflictPolicy::Overwrite,
            };
            crate::store::save_conflict_policy(self.conflict_policy.as_str());
            self.xfer_just_opened = true; // 切换点击不算窗外点击
        }
        // 点击窗口外部任意位置自动隐藏（打开当帧除外，避免被开启动作立即关闭）
        let clicked_outside = win
            .as_ref()
            .map(|r| r.response.clicked_elsewhere())
            .unwrap_or(false);
        if close_win || (clicked_outside && !self.xfer_just_opened) {
            self.show_transfers = false;
        }
        self.xfer_just_opened = false;
    }

    fn welcome(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(root, |ui| {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("iShell").size(40.0).strong().color(Palette::ACCENT));
                ui.label(
                    RichText::new(crate::i18n::tr("现代化 Rust SSH 客户端", "A modern Rust SSH client"))
                        .size(16.0)
                        .color(Palette::TEXT_DIM),
                );
                ui.add_space(20.0);
                if ui
                    .add(egui::Button::new(RichText::new(format!("{}  {}", egui_phosphor::regular::PLUS, crate::i18n::tr("新建连接", "New connection"))).size(16.0).color(egui::Color32::WHITE)).fill(Palette::ACCENT))
                    .clicked()
                {
                    self.connect_form.open_dialog();
                }
            });
        });
    }

    fn right_body(&mut self, root: &mut egui::Ui, idx: usize) {
        // 右下文件操作区（可拖动调整高度）
        let mut file_actions: Vec<FileAction> = Vec::new();
        let has_clip = self.file_clip.is_some();
        if !files_collapsed() {
            egui::Panel::bottom("files")
                .resizable(true)
                .default_size(250.0)
                .size_range(120.0..=640.0)
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL)
                        .inner_margin(8)
                        .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 6 }),
                )
                .show_inside(root, |ui| {
                    file_actions = file_panel::show(ui, &mut self.sessions[idx].files, has_clip);
                });
            for a in file_actions {
                self.handle_file_action(idx, a);
            }
        }

        // 中间终端区（四周留空隙，与其他区域分开）。
        // 6px 内边距（边框）用「窗口暖米」与「当前终端主题底色」的中间色（固定色，非渐变），
        // 让窗口与 shell 之间过渡柔和、不再是生硬的一圈暖米。
        let mut reconnect_click = false;
        let tbg = crate::terminal::current_bg();
        // 浅色终端（经典浅/近白/暖米）边框直接用终端底色，与 shell 一致、无缝；
        // 深色终端用偏向终端的混合色，略留层次。
        let term_border = if tbg.r() as u32 + tbg.g() as u32 + tbg.b() as u32 > 450 {
            tbg
        } else {
            blend_color(Palette::TERM_BG, tbg)
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(term_border)
                    .inner_margin(6)
                    .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 0 }),
            )
            .show_inside(root, |ui| {
                let s = &mut self.sessions[idx];
                // 断线提示条 + 手动重连（初次"连接中"不显示）
                if !s.connected {
                    egui::Frame::new()
                        .fill(Palette::ACCENT_SOFT)
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::WARNING, s.status)).color(Palette::DANGER).size(12.0));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", egui_phosphor::regular::ARROW_CLOCKWISE, crate::i18n::tr("重连", "Reconnect"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                        reconnect_click = true;
                                    }
                                });
                            });
                        });
                    ui.add_space(4.0);
                }
                let input = s.terminal.ui(ui);
                if !input.is_empty() {
                    let _ = s.cmd_tx.send(UiCommand::TerminalInput(input));
                }
                // 右键菜单「在文件列表中显示当前目录」：把文件区导航到终端当前目录
                if let Some(cwd) = s.terminal.take_reveal_cwd() {
                    s.files.cwd = cwd;
                    s.files.selected.clear();
                }
                // 无 cwd 时点该菜单：已同意过则静默注入（吞掉命令回显）；否则弹确认框（同意后记住）
                if s.terminal.take_inject_request() {
                    if osc7_consent() {
                        let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("{OSC7_SNIPPET}\r").into_bytes()));
                        s.terminal.expect_echo(OSC7_SNIPPET);
                        s.osc7_pending_reveal = true;
                    } else {
                        s.osc7_confirm = true;
                    }
                }
                if s.osc7_confirm {
                    let mut decided: Option<bool> = None;
                    egui::Modal::new(egui::Id::new("osc7_confirm_modal")).show(ui.ctx(), |ui| {
                        ui.set_width(370.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(crate::i18n::tr("获取终端当前目录", "Track terminal directory")).size(16.0).strong());
                            ui.add_space(6.0);
                            ui.label(crate::i18n::tr(
                                "需向当前 shell 注入一行命令以上报工作目录（仅本会话、不写配置文件）。同意后将记住，后续自动静默注入。",
                                "Inject one line into the current shell to report its directory (this session only, not written to config). Remembered after you agree.",
                            ));
                        });
                        ui.add_space(12.0);
                        let bw = 110.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                            if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("同意并注入", "Agree & inject")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                                decided = Some(true);
                            }
                            if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                                decided = Some(false);
                            }
                        });
                    });
                    match decided {
                        Some(true) => {
                            set_osc7_consent(true);
                            let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("{OSC7_SNIPPET}\r").into_bytes()));
                            s.terminal.expect_echo(OSC7_SNIPPET);
                            s.osc7_pending_reveal = true;
                            s.osc7_confirm = false;
                        }
                        Some(false) => s.osc7_confirm = false,
                        None => {}
                    }
                }
                // 注入后：下个提示符上报 cwd 时把文件区跳过去
                if s.osc7_pending_reveal {
                    if let Some(cwd) = s.terminal.cwd() {
                        s.files.cwd = cwd.to_string();
                        s.files.selected.clear();
                        s.osc7_pending_reveal = false;
                    }
                }
                let size = s.terminal.size();
                if size != s.last_size && s.connected {
                    s.last_size = size;
                    let _ = s.cmd_tx.send(UiCommand::Resize { cols: size.0, rows: size.1 });
                }
            });
        if reconnect_click {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.reconnect_tries = 0;
            }
            self.reconnect_session(idx);
        }
    }
}

/// 弹框内容统一包裹：固定宽度 + 内边距，避免文字直接贴到窗口边缘不美观。
fn dialog_body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(420.0);
            add(ui)
        })
        .inner
}

/// 扁平按钮（无边框、悬停高亮），用于标签栏等处。
/// 对话框按钮：用 egui 原生按钮（自然高度，仅约束最小宽度），由 egui 居中文字，
/// 与全局其它按钮一致，避免硬编码像素偏移在不同字体下错位。
fn dialog_button(ui: &mut egui::Ui, label: &str, fill: Option<egui::Color32>, width: f32) -> bool {
    let text = match fill {
        Some(_) => RichText::new(label).color(egui::Color32::WHITE),
        None => RichText::new(label),
    };
    let mut btn = egui::Button::new(text).min_size(egui::vec2(width, 0.0));
    if let Some(f) = fill {
        btn = btn.fill(f);
    }
    ui.add(btn).clicked()
}

/// 可拖动重排 + 缓动动画的标签条（仿主窗口顶部标签）。在当前 `ui` 内画一行可横向滚动的标签。
/// `labels`：每项 (稳定 uid, 显示文本, 悬停提示)。`drag`/`grab_dx`/`total_w` 为调用方持有的状态。
/// 返回 (要激活索引, 要关闭索引, 重排 (from,to))。
fn draggable_tabs(
    ui: &mut egui::Ui,
    drag: &mut Option<usize>,
    grab_dx: &mut f32,
    total_w: &mut f32,
    active: usize,
    want_scroll: bool,
    labels: &[(u64, String, String, f32, f32)],
) -> (Option<usize>, Option<usize>, Option<(usize, usize)>) {
    let mut to_activate = None;
    let mut to_close = None;
    let mut reorder = None;
    let mut drag_start: Option<usize> = None;
    let mut new_grab: Option<f32> = None;
    let mut drag_w = 0.0f32;
    let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
    let dragging_tab = *drag;
    let total_w_in = (*total_w).max(1.0);
    let out = egui::ScrollArea::horizontal()
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
        .show(ui, |ui| {
            let tab_h = 24.0;
            let spacing = 4.0;
            let (area, _) = ui.allocate_exact_size(egui::vec2(total_w_in, tab_h), Sense::hover());
            let origin = area.min;
            let pointer = ui.input(|i| i.pointer.interact_pos());
            let drag_down = ui.input(|i| i.pointer.any_down());
            let ctx = ui.ctx().clone();
            let font = egui::FontId::proportional(12.0);
            let mut acc = 0.0f32;
            for (i, (uid, text, tip, prog, save)) in labels.iter().enumerate() {
                let selected = active == i;
                let title_w = ctx.fonts_mut(|f| f.layout_no_wrap(text.clone(), font.clone(), Palette::TEXT).rect.width());
                let w = title_w + 16.0 + 22.0; // 左内边距 + 文本 + 右侧关闭区
                let target = acc;
                if selected && want_scroll {
                    let r = egui::Rect::from_min_size(egui::pos2(origin.x + target, origin.y), egui::vec2(w, tab_h));
                    ui.scroll_to_rect(r.expand2(egui::vec2(12.0, 0.0)), None);
                }
                let id = egui::Id::new(("dtabx", *uid));
                let dragging_this = drag_down && dragging_tab == Some(i);
                if dragging_this {
                    drag_w = w;
                }
                let x = if dragging_this {
                    let want = pointer.map(|p| p.x - origin.x - *grab_dx).unwrap_or(target);
                    ctx.animate_value_with_time(id, want, 0.0) // 跟手
                } else {
                    ctx.animate_value_with_time(id, target, 0.14) // 缓动到目标槽
                };
                let tab_rect = egui::Rect::from_min_size(egui::pos2(origin.x + x, origin.y), egui::vec2(w, tab_h));
                let mut resp = ui.interact(tab_rect, egui::Id::new(("dtab", *uid)), Sense::click_and_drag());
                // 拖动期间不弹路径提示（避免拖着拖着冒出悬停 tooltip）
                if dragging_tab.is_none() && !tip.is_empty() {
                    resp = resp.on_hover_text(tip.as_str());
                }
                let close_rect = egui::Rect::from_center_size(egui::pos2(tab_rect.right() - 12.0, tab_rect.center().y), egui::vec2(16.0, 16.0));
                let close_resp = ui.interact(close_rect, egui::Id::new(("dtabclose", *uid)), Sense::click());
                let p = ui.painter();
                let fill = if dragging_this { Palette::ACCENT_SOFT } else if selected { Palette::PANEL_2 } else { egui::Color32::TRANSPARENT };
                p.rect_filled(tab_rect, 6, fill);
                if *save >= 0.0 {
                    // 保存动画：底部整条珊瑚线上，先绿色从左扫到右（save 0→1，「保存中」），
                    // 再珊瑚色从左扫回覆盖绿色（save 1→2，「已保存」）。
                    let y = tab_rect.bottom() - 1.0;
                    let coral = Palette::ACCENT;
                    let green = egui::Color32::from_rgb(46, 200, 120);
                    if *save <= 1.0 {
                        let x = tab_rect.left() + tab_rect.width() * *save;
                        p.hline(tab_rect.left()..=tab_rect.right(), y, egui::Stroke::new(2.0, coral));
                        p.hline(tab_rect.left()..=x, y, egui::Stroke::new(2.0, green));
                    } else {
                        let x = tab_rect.left() + tab_rect.width() * (*save - 1.0);
                        p.hline(tab_rect.left()..=tab_rect.right(), y, egui::Stroke::new(2.0, green));
                        p.hline(tab_rect.left()..=x, y, egui::Stroke::new(2.0, coral));
                    }
                } else if *prog >= 0.0 {
                    // 加载中：底部珊瑚线从左到右随下载进度增长（替代选中态整条下划线）
                    let w_done = (tab_rect.width() * prog.clamp(0.0, 1.0)).max(0.0);
                    p.hline(tab_rect.left()..=(tab_rect.left() + w_done), tab_rect.bottom() - 1.0, egui::Stroke::new(2.0, Palette::ACCENT));
                } else if selected && !dragging_this {
                    p.hline(tab_rect.left()..=tab_rect.right(), tab_rect.bottom() - 1.0, egui::Stroke::new(2.0, Palette::ACCENT));
                }
                let tcolor = if selected { Palette::TEXT } else { Palette::TEXT_DIM };
                p.text(egui::pos2(tab_rect.left() + 8.0, tab_rect.center().y), egui::Align2::LEFT_CENTER, text, font.clone(), tcolor);
                let xcolor = if close_resp.hovered() { Palette::DANGER } else { Palette::TEXT_DIM };
                p.text(close_rect.center(), egui::Align2::CENTER_CENTER, egui_phosphor::regular::X, egui::FontId::proportional(11.0), xcolor);
                if close_resp.clicked() {
                    to_close = Some(i);
                } else if resp.clicked() {
                    to_activate = Some(i);
                } else if resp.middle_clicked() {
                    to_close = Some(i);
                }
                if resp.drag_started() {
                    drag_start = Some(i);
                    if let Some(pp) = pointer {
                        new_grab = Some(pp.x - (origin.x + x));
                    }
                }
                tab_rects.push((i, egui::Rect::from_min_size(egui::pos2(origin.x + target, origin.y), egui::vec2(w, tab_h))));
                acc += w + spacing;
            }
            acc
        });
    *total_w = out.inner.max(1.0);
    // 溢出渐隐提示
    let off = out.state.offset.x;
    let vw = out.inner_rect.width();
    if off > 0.5 {
        edge_fade(ui.painter(), out.inner_rect, true, Palette::BG);
    }
    if off + vw < out.inner - 0.5 {
        edge_fade(ui.painter(), out.inner_rect, false, Palette::BG);
    }
    if let Some(g) = new_grab {
        *grab_dx = g;
    }
    if let Some(f) = drag_start {
        *drag = Some(f);
    }
    if let Some(from) = *drag {
        if ui.input(|i| i.pointer.any_down()) {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                let drag_center = pos.x - *grab_dx + drag_w / 2.0;
                let mut to = from;
                if from > 0 {
                    if let Some(&(_, lr)) = tab_rects.get(from - 1) {
                        if drag_center < lr.center().x {
                            to = from - 1;
                        }
                    }
                }
                if to == from {
                    if let Some(&(_, rr)) = tab_rects.get(from + 1) {
                        if drag_center > rr.center().x {
                            to = from + 1;
                        }
                    }
                }
                if to != from {
                    reorder = Some((from, to));
                    *drag = Some(to);
                }
            }
        } else {
            *drag = None;
        }
    }
    (to_activate, to_close, reorder)
}

fn flat_button(ui: &mut egui::Ui, text: &RichText, tip: &str) -> bool {
    let mut clicked = false;
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        clicked = ui
            .add(egui::Button::new(text.clone()).corner_radius(6.0))
            .on_hover_text(tip)
            .clicked();
    });
    clicked
}

/// 把已释放的堆内存归还给操作系统（glibc）。关闭大文件编辑器后调用。
fn trim_memory() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// 用系统文件管理器打开文件所在目录。
/// 在标签条某一侧绘制渐隐遮罩，提示该方向还有被滚动隐藏的标签。
/// `left=true` 左侧（实色在左、向右透明）；否则右侧（向右渐变为实色）。
fn edge_fade(painter: &egui::Painter, rect: egui::Rect, left: bool, bg: egui::Color32) {
    let w = 18.0_f32.min(rect.width());
    let transp = egui::Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), 0);
    let (x0, x1, c0, c1) = if left {
        (rect.left(), rect.left() + w, bg, transp)
    } else {
        (rect.right() - w, rect.right(), transp, bg)
    };
    let (t, b) = (rect.top(), rect.bottom());
    let mut mesh = egui::Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    mesh.vertices.push(egui::epaint::Vertex { pos: egui::pos2(x0, t), uv, color: c0 });
    mesh.vertices.push(egui::epaint::Vertex { pos: egui::pos2(x1, t), uv, color: c1 });
    mesh.vertices.push(egui::epaint::Vertex { pos: egui::pos2(x1, b), uv, color: c1 });
    mesh.vertices.push(egui::epaint::Vertex { pos: egui::pos2(x0, b), uv, color: c0 });
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// 在文件管理器中打开并**选中**该文件（而不仅是打开目录）。
fn open_containing_folder(file: &str) {
    #[cfg(target_os = "windows")]
    {
        // explorer /select, 选中文件
        let _ = std::process::Command::new("explorer").arg(format!("/select,{file}")).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        // Finder 中显示并选中
        let _ = std::process::Command::new("open").arg("-R").arg(file).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        // 优先 freedesktop D-Bus ShowItems（nautilus/dolphin/nemo 等都支持），可选中文件；
        // 不可用时退回 xdg-open 仅打开所在目录。
        let uri = file_uri(file);
        let dbus = std::process::Command::new("dbus-send")
            .args([
                "--type=method_call",
                "--dest=org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .spawn();
        if dbus.is_err() {
            let dir = std::path::Path::new(file)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}

/// 把本地绝对路径转成百分号编码的 file:// URI。
#[cfg(target_os = "linux")]
fn file_uri(p: &str) -> String {
    let mut s = String::from("file://");
    for b in p.bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

/// 本地下载目录：优先用户主目录下的 Downloads，否则当前目录。
fn downloads_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    if let Some(home) = home {
        let d = std::path::Path::new(&home).join("Downloads");
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    std::path::PathBuf::from(".")
}
