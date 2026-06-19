//! 应用主体：会话管理 + 顶部标签 + 三区布局（系统信息 / 终端 / 文件）。

use std::sync::Arc;

use egui::{RichText, Sense};
use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{AuthMethod, ConnectConfig, UiCommand, WorkerEvent};
use crate::ssh::{self, UiSink};
use crate::terminal::Terminal;
use crate::theme::Palette;
use crate::ui::connect::ConnectForm;
use crate::ui::file_panel::{self, FileAction, FilePanelState};
use crate::ui::sidebar::{self, NetHistory};

/// 单个 SSH 会话的前台状态。
struct Session {
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
    /// 已读取待打开到编辑器的文件（path, content）
    pending_open: Vec<(String, String)>,
    /// 向 worker 回复"是否信任未知主机"
    hostkey_tx: UnboundedSender<bool>,
    /// 待确认的未知主机（host, 指纹）
    pending_hostkey: Option<(String, String)>,
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
}

/// UI 侧的一条传输记录。
struct Transfer {
    id: u64,
    name: String,
    dir: crate::proto::TransferDir,
    done: u64,
    total: u64,
    /// None=进行中，Some(true/false)=完成/失败
    ok: Option<bool>,
    /// 下载到的本地路径（用于「打开所在文件夹」）
    local: Option<String>,
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
                }
                WorkerEvent::Disconnected(reason) => {
                    self.connected = false;
                    self.status = reason;
                    // 仅对"曾连上又掉线"的会话自动重连，最多 5 次，指数退避
                    const MAX_TRIES: u32 = 5;
                    if self.was_connected && self.reconnect_tries < MAX_TRIES {
                        let secs = (2u64 << self.reconnect_tries.min(4)).min(30); // 2,4,8,16,30
                        self.reconnect_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(secs));
                        let tail = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("{secs}s 后重连"), crate::i18n::Lang::En => format!("reconnect in {secs}s") };
                        self.status = format!("{} · {}", self.status, tail);
                    }
                }
                WorkerEvent::TerminalData(bytes) => self.terminal.feed(&bytes),
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
                WorkerEvent::ProcDetail { pid, cmd, cwd, exe } => {
                    self.proc_detail = Some((pid, cmd, cwd, exe));
                }
                WorkerEvent::ForwardStatus { id, ok, message } => {
                    if let Some(f) = self.forwards.iter_mut().find(|f| f.id == id) {
                        f.ok = ok;
                        f.status = message;
                    }
                }
                WorkerEvent::HostKeyPrompt { host, fingerprint } => {
                    self.pending_hostkey = Some((host, fingerprint));
                    self.status = crate::i18n::tr("等待确认主机指纹 …", "Awaiting host key …").into();
                }
                WorkerEvent::FileOpened { path, content } => {
                    self.pending_open.push((path, content));
                    self.status = crate::i18n::tr("已打开文件", "File opened").into();
                }
                WorkerEvent::OpDone { message, refresh_dir } => {
                    self.status = message;
                    self.refresh_dir(refresh_dir);
                }
                WorkerEvent::TransferStart { id, name, total, dir } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.name = name;
                        t.total = total;
                        t.dir = dir;
                    } else {
                        self.transfers.push(Transfer { id, name, dir, done: 0, total, ok: None, local: None });
                    }
                }
                WorkerEvent::TransferProgress { id, done } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.done = done;
                    }
                }
                WorkerEvent::TransferDone { id, ok, message, refresh_dir } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.ok = Some(ok);
                        if ok && t.total == 0 {
                            t.total = t.done;
                        }
                    }
                    self.status = message;
                    self.refresh_dir(refresh_dir);
                }
                WorkerEvent::Error(e) => self.status = e,
            }
        }
    }

    /// 刷新指定目录的列表（操作/传输完成后调用）。
    fn refresh_dir(&mut self, dir: Option<String>) {
        if let Some(dir) = dir {
            self.files.listings.remove(&dir);
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

pub struct App {
    runtime: Arc<tokio::runtime::Runtime>,
    ctx: egui::Context,
    sessions: Vec<Session>,
    active: Option<usize>,
    /// 正在拖拽排序的标签源索引
    dragging_tab: Option<usize>,
    connect_form: ConnectForm,
    /// 默认下载目录（可在传输窗中修改，持久化）
    download_dir: std::path::PathBuf,
    /// 传输进度浮窗是否显示
    show_transfers: bool,
    /// 传输浮窗刚打开（本帧跳过"点击外部关闭"判定）
    xfer_just_opened: bool,
    /// 显示"确认退出"对话框
    show_close_confirm: bool,
    /// 已确认可以关闭
    allow_close: bool,
    /// 编辑器标签页
    editors: Vec<EditorTab>,
    active_editor: usize,
    /// 下一个编辑器 TextEdit Id 序号
    next_editor_id: u64,
    /// 关闭大文件编辑器后延迟若干帧再 malloc_trim（等 galley 缓存被淘汰）
    trim_after: Option<u32>,
    /// 端口转发管理窗口是否显示
    show_forwards: bool,
    /// 转发浮窗刚打开（本帧跳过"点击外部关闭"判定）
    fwd_just_opened: bool,
    /// 新增转发表单
    fwd_form: ForwardForm,
    /// 命令广播栏是否显示 + 输入内容
    show_broadcast: bool,
    broadcast_input: String,
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
}

/// UI 侧的一条端口转发记录。
struct ForwardEntry {
    id: u64,
    label: String,
    status: String,
    ok: bool,
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
    server: String,
    cmd_tx: UnboundedSender<UiCommand>,
    /// 该编辑器固定的 TextEdit Id（关闭时据此清理 egui 状态/撤销历史）
    text_id: egui::Id,
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
            connect_form: form,
            download_dir: crate::store::load_download_dir().map(std::path::PathBuf::from).unwrap_or_else(downloads_dir),
            show_transfers: false,
            xfer_just_opened: false,
            show_close_confirm: false,
            allow_close: false,
            editors: Vec::new(),
            active_editor: 0,
            next_editor_id: 0,
            trim_after: None,
            show_forwards: false,
            fwd_just_opened: false,
            fwd_form: ForwardForm::default(),
            show_broadcast: false,
            broadcast_input: String::new(),
            proc_popup: None,
            proc_popup_just_opened: false,
            gpu_popup: None,
            gpu_popup_just_opened: false,
            demo_gpu: std::env::var("ISHELL_DEMO_GPU").is_ok(),
            demo_net: std::env::var("ISHELL_DEMO_NET").is_ok(),
            shot,
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
                        auth: AuthMethod::KeyFile { path: parts[3].into(), passphrase: None },
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
                    });
                }
            }
        }

        // 自检：直接打开新建表单（截图核对输入框样式）
        if std::env::var("ISHELL_DEMO_FORM").is_ok() {
            app.connect_form.open_form_for_demo();
        }
        // 自检：注入演示编辑器内容（截图核对代码高亮 + 多标签）
        if std::env::var("ISHELL_DEMO_EDIT").is_ok() {
            if let Some((server, tx)) = app.sessions.first().map(|s| (s.title.clone(), s.cmd_tx.clone())) {
                let code = "use std::io;\n\n// 示例：读取并打印\nfn main() {\n    let mut s = String::new();\n    io::stdin().read_line(&mut s).unwrap();\n    let n: i32 = s.trim().parse().unwrap_or(0);\n    for i in 0..n {\n        println!(\"line {}\", i);\n    }\n}\n".to_string();
                let t1 = app.alloc_editor_id();
                app.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/home/e5-1/demo.rs".into(), code),
                    server: server.clone(),
                    cmd_tx: tx.clone(),
                    text_id: t1,
                });
                // 大文件（>1MB）→ 只读模式，核对「改为可编辑」按钮
                let big: String = (0..40000).map(|i| format!("{i}: the quick brown fox jumps over the lazy dog\n")).collect();
                let t2 = app.alloc_editor_id();
                app.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/var/log/huge.log".into(), big),
                    server: server.clone(),
                    cmd_tx: tx.clone(),
                    text_id: t2,
                });
                let t3 = app.alloc_editor_id();
                app.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/etc/hosts".into(), "127.0.0.1 localhost\n::1 localhost\n".into()),
                    server,
                    cmd_tx: tx,
                    text_id: t3,
                });
                app.active_editor = 1; // 默认显示大文件标签
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
                s.transfers.push(Transfer { id: 1, name: "backup.tar.gz".into(), dir: Download, done: 73_400_320, total: 104_857_600, ok: None, local: None });
                s.transfers.push(Transfer { id: 2, name: "deploy.sh".into(), dir: Upload, done: 2048, total: 2048, ok: Some(true), local: None });
                s.transfers.push(Transfer { id: 3, name: "huge.bin".into(), dir: Download, done: 1024, total: 2048, ok: Some(true), local: Some("/root/Downloads/huge.bin".into()) });
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

        self.sessions.push(Session {
            title: if cfg.label.trim().is_empty() { cfg.username.clone() } else { cfg.label.trim().to_string() },
            tip: format!("{}@{}:{}", cfg.username, cfg.host, cfg.port),
            cmd_tx,
            evt_rx,
            connected: false,
            status: crate::i18n::tr("连接中 …", "Connecting …").into(),
            terminal: Terminal::new(),
            sysinfo: None,
            net_hist: NetHistory::default(),
            files: FilePanelState::default(),
            last_size: (0, 0),
            initialized: false,
            transfers: Vec::new(),
            next_xfer: 1,
            selected_nic: String::new(),
            proc_sort_mem: false,
            pending_open: Vec::new(),
            hostkey_tx,
            pending_hostkey: None,
            forwards: Vec::new(),
            next_forward: 1,
            proc_detail: None,
            cfg,
            was_connected: false,
            reconnect_at: None,
            reconnect_tries: 0,
        });
        self.active = Some(self.sessions.len() - 1);
    }

    /// 重连指定会话：用原配置重启 worker，重置连接相关状态，保留标签/目录等。
    fn reconnect_session(&mut self, idx: usize) {
        let Some(s) = self.sessions.get(idx) else { return };
        let cfg = s.cfg.clone();
        let (cmd_tx, evt_rx, hostkey_tx) = self.spawn_worker(cfg);
        let Some(s) = self.sessions.get_mut(idx) else { return };
        s.cmd_tx = cmd_tx;
        s.evt_rx = evt_rx;
        s.hostkey_tx = hostkey_tx;
        s.connected = false;
        s.initialized = false;
        s.terminal = Terminal::new();
        s.sysinfo = None;
        s.forwards.clear();
        s.pending_hostkey = None;
        s.reconnect_at = None;
        s.status = crate::i18n::tr("重连中 …", "Reconnecting …").into();
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
            self.active = Some(idx.min(self.sessions.len() - 1));
        }
    }

    /// 翻译文件面板动作为 SFTP 指令或剪贴板操作。
    fn handle_file_action(&mut self, idx: usize, action: FileAction) {
        let Some(s) = self.sessions.get_mut(idx) else { return };
        match action {
            FileAction::List(path) => {
                let p = if path == "~" || path.is_empty() { ".".into() } else { path };
                let _ = s.cmd_tx.send(UiCommand::ListDir(p));
            }
            FileAction::Download(remote) => {
                let name = remote.rsplit('/').next().unwrap_or("download").to_string();
                let local = self.download_dir.join(&name).to_string_lossy().into_owned();
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.transfers.push(Transfer {
                    id, name, dir: crate::proto::TransferDir::Download, done: 0, total: 0, ok: None,
                    local: Some(local.clone()),
                });
                let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local });
                self.show_transfers = true;
                self.xfer_just_opened = true;
            }
            FileAction::Upload { local, remote_dir } => {
                let name = local.rsplit('/').next().unwrap_or("upload").to_string();
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.transfers.push(Transfer {
                    id, name, dir: crate::proto::TransferDir::Upload, done: 0, total: 0, ok: None,
                    local: None,
                });
                let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir });
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
            FileAction::Delete { path, is_dir } => {
                let _ = s.cmd_tx.send(UiCommand::Delete { path, is_dir });
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
                let _ = s.cmd_tx.send(UiCommand::ReadFile { path, force });
            }
        }
    }
}

impl eframe::App for App {
    // 窗口清屏色用主题背景，避免各区域间隙露出黑色
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Palette::BG.to_normalized_gamma_f32()
    }

    // eframe 0.34 的现代入口：所有面板通过 `show_inside` 嵌入根 `ui`。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 1) 排空所有会话的后台事件，并在连接成功后初始化文件树
        let mut new_tabs: Vec<(String, String, String, UnboundedSender<UiCommand>)> = Vec::new();
        for s in &mut self.sessions {
            s.drain_events();
            if s.connected && !s.initialized {
                s.initialized = true;
                s.init_files();
            }
            for (path, content) in s.pending_open.drain(..) {
                new_tabs.push((path, content, s.title.clone(), s.cmd_tx.clone()));
            }
        }
        for (path, content, server, tx) in new_tabs {
            // 同一服务器同一文件已打开则切到该标签
            if let Some(i) = self.editors.iter().position(|t| t.server == server && t.editor.path == path) {
                self.active_editor = i;
            } else {
                let tid = self.alloc_editor_id();
                self.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new(path, content),
                    server,
                    cmd_tx: tx,
                    text_id: tid,
                });
                self.active_editor = self.editors.len() - 1;
            }
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
        let side = egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=460.0)
            .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 8 }))
            .show_inside(ui, |ui| match self.active {
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
            });
        // 右键左侧栏：语言设置
        side.response.context_menu(|ui| {
            ui.label(RichText::new(crate::i18n::tr("语言", "Language")).color(Palette::TEXT_DIM).size(11.0));
            crate::i18n::language_menu(ui);
        });
        // 进程行被点击：打开详情小窗并请求详情
        if let Some((pid, pos)) = proc_click {
            let mut popup = None;
            if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                if let Some(p) = s.sysinfo.as_ref().and_then(|si| si.procs.iter().find(|p| p.pid == pid)) {
                    popup = Some(ProcPopup {
                        pid, name: p.name.clone(), cpu: p.cpu, mem: p.mem, pos,
                        cmd: String::new(), cwd: String::new(), exe: String::new(),
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

        // 4) 顶部选项卡（仅位于右侧区域之上）
        self.top_tabs(ui);

        // 4.5) 命令广播栏
        self.broadcast_bar(ui);

        // 5) 右侧主体
        match self.active {
            Some(idx) if idx < self.sessions.len() => self.right_body(ui, idx),
            _ => self.welcome(ui),
        }

        // 传输进度浮窗
        self.transfer_window(&ctx);

        // 端口转发管理浮窗
        self.forward_window(&ctx);

        // 进程详情小窗
        self.proc_popup_window(&ctx);

        // GPU 详情小窗
        self.gpu_popup_window(&ctx);

        // 文本编辑器浮窗
        self.editor_window(&ctx);

        // 未知主机指纹确认（TOFU）
        self.host_key_dialog(&ctx);

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
        let (host, fp) = self.sessions[idx].pending_hostkey.clone().unwrap();
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("hostkey_modal"))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(crate::i18n::tr("未知主机", "Unknown host")).size(16.0).strong());
                ui.add_space(8.0);
                ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("首次连接主机：{host}"), crate::i18n::Lang::En => format!("First connect: {host}") });
                ui.add_space(4.0);
                ui.label(RichText::new(crate::i18n::tr("指纹 (SHA256)：", "Fingerprint (SHA256):")).color(Palette::TEXT_DIM).size(12.0));
                ui.label(RichText::new(&fp).monospace());
                ui.add_space(6.0);
                ui.label(RichText::new(crate::i18n::tr("请确认该指纹与目标服务器一致；信任后将写入 ~/.ssh/known_hosts。", "Verify the fingerprint matches the server; trusting writes to ~/.ssh/known_hosts.")).color(Palette::TEXT_DIM).size(11.0));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 96.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    if dialog_button(ui, crate::i18n::tr("信任并连接", "Trust & connect"), Some(Palette::ACCENT), bw) {
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
            bytes.push(b'\n');
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
                        // 新建：固定在标签条右侧，标签溢出也不会被滚走
                        if flat_button(ui, &RichText::new(icon::PLUS).size(15.0), crate::i18n::tr("新建连接", "New connection")) {
                            self.connect_form.open_dialog();
                            self.show_close_confirm = false;
                        }

                        // 剩余空间：标签条横向可滚动（标签多时不再与右侧按钮重叠）
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let mut drag_start: Option<usize> = None;
                            let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
                            let out = egui::ScrollArea::horizontal()
                                .auto_shrink([false, false])
                                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                                .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        for (i, s) in self.sessions.iter().enumerate() {
                                            let selected = self.active == Some(i);
                                            let fill = if selected { Palette::PANEL } else { egui::Color32::TRANSPARENT };
                                            let inner = egui::Frame::new()
                                                .fill(fill)
                                                .corner_radius(egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 })
                                                .inner_margin(egui::Margin::symmetric(9, 4))
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), Sense::hover());
                                                        let color = if s.connected { Palette::OK } else { Palette::WARN };
                                                        ui.painter().circle_filled(dot.center(), 4.0, color);
                                                        let title = RichText::new(&s.title)
                                                            .color(if selected { Palette::TEXT } else { Palette::TEXT_DIM });
                                                        // 标签标题：可点击（激活）+ 可拖拽（排序）
                                                        let tr = ui
                                                            .add(egui::Label::new(title).selectable(false).sense(Sense::click_and_drag()))
                                                            .on_hover_text(s.tip.as_str());
                                                        if tr.clicked() {
                                                            to_activate = Some(i);
                                                        }
                                                        if tr.middle_clicked() {
                                                            to_close = Some(i);
                                                        }
                                                        if tr.drag_started() {
                                                            drag_start = Some(i);
                                                        }
                                                        // 关闭按钮：独立响应，确保可点
                                                        if ui
                                                            .add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                                                            .on_hover_text(crate::i18n::tr("关闭会话", "Close session"))
                                                            .clicked()
                                                        {
                                                            to_close = Some(i);
                                                        }
                                                    });
                                                });
                                            tab_rects.push((i, inner.response.rect));
                                        }
                                    });
                                });
                            // 溢出渐隐：提示左右还有被隐藏的标签
                            let off = out.state.offset.x;
                            let vw = out.inner_rect.width();
                            let cw = out.content_size.x;
                            if off > 0.5 {
                                edge_fade(ui.painter(), out.inner_rect, true, Palette::PANEL_2);
                            }
                            if off + vw < cw - 0.5 {
                                edge_fade(ui.painter(), out.inner_rect, false, Palette::PANEL_2);
                            }
                            // 拖拽排序：记录起点，松开时按指针所在标签确定目标位置
                            if let Some(f) = drag_start {
                                self.dragging_tab = Some(f);
                            }
                            if ui.input(|i| i.pointer.any_released()) {
                                if let Some(from) = self.dragging_tab.take() {
                                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                                        if let Some(&(to, _)) = tab_rects.iter().find(|(_, r)| r.contains(pos)) {
                                            if to != from {
                                                reorder = Some((from, to));
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    });
                    if let Some((from, to)) = reorder {
                        self.reorder_session(from, to);
                    }
                    if let Some(i) = to_activate {
                        self.active = Some(i);
                    }
                    if let Some(i) = to_close {
                        self.close_session(i);
                    }
                });
            });
    }

    /// 多标签文本编辑器浮窗。
    fn editor_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if self.editors.is_empty() {
            return;
        }
        if self.active_editor >= self.editors.len() {
            self.active_editor = self.editors.len() - 1;
        }

        let mut close_tab: Option<usize> = None;
        let mut activate: Option<usize> = None;
        let mut do_save = false;

        egui::Window::new("editor_win")
            .title_bar(false)
            .default_size([840.0, 600.0])
            .resizable(true)
            .collapsible(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(8))
            .show(ctx, |ui| {
                // 标签栏：服务器 · 文件名 + 关闭
                ui.horizontal_wrapped(|ui| {
                    for (i, t) in self.editors.iter().enumerate() {
                        let selected = i == self.active_editor;
                        let fill = if selected { Palette::PANEL_2 } else { egui::Color32::TRANSPARENT };
                        egui::Frame::new()
                            .fill(fill)
                            .corner_radius(5)
                            .inner_margin(egui::Margin::symmetric(8, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let dirty = if t.editor.dirty() { " ●" } else { "" };
                                    let label = format!("{} {}·{}{}", icon::FILE_CODE, t.server, t.editor.filename(), dirty);
                                    let color = if selected { Palette::TEXT } else { Palette::TEXT_DIM };
                                    if ui.add(egui::Label::new(RichText::new(label).color(color).size(12.0)).selectable(false).sense(Sense::click())).clicked() {
                                        activate = Some(i);
                                    }
                                    if ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                                        close_tab = Some(i);
                                    }
                                });
                            });
                    }
                });
                ui.separator();

                // 当前标签内容
                if let Some(tab) = self.editors.get_mut(self.active_editor) {
                    let tid = tab.text_id;
                    if crate::ui::editor::content(ui, &mut tab.editor, tid) {
                        do_save = true;
                    }
                }
            });

        if let Some(i) = activate {
            self.active_editor = i;
        }
        if do_save {
            if let Some(tab) = self.editors.get(self.active_editor) {
                let _ = tab.cmd_tx.send(UiCommand::WriteFile {
                    path: tab.editor.path.clone(),
                    content: tab.editor.content.clone(),
                });
            }
            if let Some(tab) = self.editors.get_mut(self.active_editor) {
                tab.editor.mark_saved();
            }
        }
        if let Some(i) = close_tab {
            if i < self.editors.len() {
                let closed = self.editors.remove(i);
                // 清除该编辑器在 egui 内存中的 TextEdit 状态（含撤销历史的文本快照），
                // 否则大文件编辑后即使关闭也会残留占用
                ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
            }
            if self.active_editor >= self.editors.len() && !self.editors.is_empty() {
                self.active_editor = self.editors.len() - 1;
            }
            // 延迟几帧再 malloc_trim：等 galley 缓存被淘汰后再归还 OS，效果更明显
            self.trim_after = Some(4);
            ctx.request_repaint();
        }
    }

    /// GPU 详情小窗：每块 GPU 使用率 + 显存；鼠标移开或点击外部关闭。
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
            // 窗口套在光标上（光标落在窗口内），这样「鼠标移开即关闭」不会一打开就触发
            .fixed_pos(pos - egui::vec2(10.0, 10.0))
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                ui.set_max_width(300.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::CPU, crate::i18n::tr("GPU 详情", "GPU"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                for g in &gpus {
                    ui.label(RichText::new(format!("GPU{} {}", g.index, g.name)).size(12.0).color(Palette::TEXT));
                    let mem_pct = if g.mem_total_mb > 0 { g.mem_used_mb as f32 / g.mem_total_mb as f32 * 100.0 } else { 0.0 };
                    ui.add(
                        egui::ProgressBar::new((g.util / 100.0).clamp(0.0, 1.0))
                            .fill(crate::ui::usage_color(g.util))
                            .text(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("使用率 {:.0}%", g.util), crate::i18n::Lang::En => format!("Util {:.0}%", g.util) }).size(10.0))
                            .desired_height(12.0)
                            .corner_radius(2.0),
                    );
                    ui.add(
                        egui::ProgressBar::new((mem_pct / 100.0).clamp(0.0, 1.0))
                            .fill(Palette::ACCENT)
                            .text(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("显存 {}/{} MB", g.mem_used_mb, g.mem_total_mb), crate::i18n::Lang::En => format!("VRAM {}/{} MB", g.mem_used_mb, g.mem_total_mb) }).size(10.0))
                            .desired_height(12.0)
                            .corner_radius(2.0),
                    );
                    ui.add_space(5.0);
                }
            });

        // 鼠标移开（不在窗口上）或点击外部 -> 关闭
        let hovered = win.as_ref().map(|r| r.response.hovered()).unwrap_or(false);
        let outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close || ((outside || !hovered) && !self.gpu_popup_just_opened) {
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
        let mut kill = false;
        let win = egui::Window::new("proc_popup")
            .title_bar(false)
            .fixed_pos(pos + egui::vec2(8.0, 8.0))
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                ui.set_max_width(320.0);
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
                kv(ui, "PID", pid.to_string());
                kv(ui, "CPU", format!("{cpu:.1}%"));
                kv(ui, crate::i18n::tr("内存", "Mem"), format!("{mem:.1}%"));
                if !exe.is_empty() {
                    kv(ui, crate::i18n::tr("程序", "Exe"), exe.clone());
                }
                if !cwd.is_empty() {
                    kv(ui, crate::i18n::tr("目录", "Dir"), cwd.clone());
                }
                if cmd.is_empty() {
                    ui.label(RichText::new(crate::i18n::tr("（正在获取命令…）", "(loading command…)")).color(Palette::TEXT_DIM).size(11.0));
                } else {
                    ui.add_space(2.0);
                    ui.label(RichText::new(crate::i18n::tr("命令", "Command")).color(Palette::TEXT_DIM).size(12.0));
                    ui.label(RichText::new(&cmd).size(11.5).monospace().color(Palette::TEXT));
                }
                ui.separator();
                if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::SKULL, crate::i18n::tr("强制结束 (kill -9)", "Kill (-9)"))).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                    kill = true;
                }
            });

        let outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if kill {
            if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                let _ = s.cmd_tx.send(UiCommand::KillProc(pid));
            }
            self.proc_popup = None;
        } else if close || (outside && !self.proc_popup_just_opened) {
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
        let mut remove_id: Option<u64> = None;
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

                // 新增表单（分段按钮代替下拉，避免点击下拉被判为窗口外而自动关闭）
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
                if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::PLUS, crate::i18n::tr("添加转发", "Add forward"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
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

                ui.separator();
                if let Some(s) = self.sessions.get(idx) {
                    if s.forwards.is_empty() {
                        ui.label(RichText::new(crate::i18n::tr("暂无转发任务", "No forwards")).color(Palette::TEXT_DIM).size(12.0));
                    }
                    for fwd in &s.forwards {
                        ui.horizontal(|ui| {
                            let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, if fwd.ok { Palette::OK } else { Palette::DANGER });
                            ui.label(RichText::new(&fwd.label).size(12.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除", "Delete")).clicked() {
                                    remove_id = Some(fwd.id);
                                }
                            });
                        });
                        ui.label(RichText::new(&fwd.status).color(if fwd.ok { Palette::TEXT_DIM } else { Palette::DANGER }).size(10.5));
                        ui.add_space(3.0);
                    }
                }
            });

        // 点击窗口外部自动隐藏（打开当帧除外）
        let clicked_outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close_win || (clicked_outside && !self.fwd_just_opened) {
            self.show_forwards = false;
        }
        self.fwd_just_opened = false;

        let idx = match idx {
            Some(i) => i,
            None => return,
        };
        if let Some(mut spec) = add_spec {
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
                s.forwards.push(ForwardEntry { id, label, status: crate::i18n::tr("启动中 …", "Starting …").into(), ok: true });
                let _ = s.cmd_tx.send(UiCommand::AddForward(spec));
            }
            self.fwd_form.local_port.clear();
            self.fwd_form.target_host.clear();
            self.fwd_form.target_port.clear();
        }
        if let Some(id) = remove_id {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.forwards.retain(|f| f.id != id);
                let _ = s.cmd_tx.send(UiCommand::RemoveForward(id));
            }
        }
    }

    /// 右上角传输进度浮窗（可弹出/隐藏）。
    fn transfer_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if !self.show_transfers {
            return;
        }
        let Some(idx) = self.active else { return };
        let mut close_win = false;
        let mut clear = false;
        let mut pick_dir = false;
        let dl_dir = self.download_dir.to_string_lossy().into_owned();
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

                let Some(s) = self.sessions.get(idx) else { return };
                if s.transfers.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("暂无传输任务", "No transfers")).color(Palette::TEXT_DIM).size(12.0));
                }
                let mut open_dir: Option<String> = None;
                for t in s.transfers.iter().rev().take(20) {
                    // 下载=绿色，上传=珊瑚橙，颜色区分方向
                    let (dir_icon, dir_col) = match t.dir {
                        crate::proto::TransferDir::Download => (icon::DOWNLOAD_SIMPLE, Palette::OK),
                        crate::proto::TransferDir::Upload => (icon::UPLOAD_SIMPLE, Palette::ACCENT),
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(dir_icon).color(dir_col).size(13.0));
                        ui.label(RichText::new(&t.name).size(12.0).color(Palette::TEXT));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match t.ok {
                                Some(true) => { ui.label(RichText::new(icon::CHECK_CIRCLE).color(Palette::OK).size(13.0)); }
                                Some(false) => { ui.label(RichText::new(icon::WARNING_CIRCLE).color(Palette::DANGER).size(13.0)); }
                                None => { ui.spinner(); }
                            }
                            // 下载完成：打开所在文件夹
                            if t.ok == Some(true) {
                                if let Some(local) = &t.local {
                                    if ui.add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(12.0).color(Palette::TEXT_DIM)).frame(false))
                                        .on_hover_text(crate::i18n::tr("打开所在文件夹", "Open folder"))
                                        .clicked()
                                    {
                                        open_dir = Some(local.clone());
                                    }
                                }
                            }
                        });
                    });
                    let frac = if t.total > 0 { t.done as f32 / t.total as f32 } else if t.ok == Some(true) { 1.0 } else { 0.0 };
                    let text = format!("{} / {}", crate::ui::fmt_bytes(t.done as f64), crate::ui::fmt_bytes(t.total as f64));
                    ui.add(
                        egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                            .fill(dir_col)
                            .text(RichText::new(text).size(10.0))
                            .desired_height(10.0)
                            .corner_radius(2.0),
                    );
                    ui.add_space(4.0);
                }
                if let Some(p) = open_dir {
                    open_containing_folder(&p);
                }
                if !s.transfers.is_empty() {
                    ui.separator();
                    if ui.button(crate::i18n::tr("清除已完成", "Clear done")).clicked() {
                        clear = true;
                    }
                }
            });
        if clear {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.transfers.retain(|t| t.ok.is_none());
            }
        }
        // 选择默认下载目录（原生文件夹选择器）
        if pick_dir {
            if let Some(dir) = rfd::FileDialog::new().set_title(crate::i18n::tr("选择默认下载文件夹", "Select default download folder")).pick_folder() {
                self.download_dir = dir.clone();
                crate::store::save_download_dir(&dir.to_string_lossy());
            }
            self.xfer_just_opened = true; // 选择期间点击不算"外部点击"，避免关窗
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
                file_actions = file_panel::show(ui, &mut self.sessions[idx].files);
            });
        for a in file_actions {
            self.handle_file_action(idx, a);
        }

        // 中间终端区（四周留空隙，与其他区域分开）
        let mut reconnect_click = false;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Palette::TERM_BG)
                    .inner_margin(6)
                    .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 0 }),
            )
            .show_inside(root, |ui| {
                let s = &mut self.sessions[idx];
                // 断线提示条 + 手动重连（初次"连接中"不显示）
                if !s.connected {
                    egui::Frame::new()
                        .fill(Palette::ACCENT_SOFT)
                        .corner_radius(4)
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

/// 扁平按钮（无边框、悬停高亮），用于标签栏等处，贴近 FinalShell 风格。
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

fn flat_button(ui: &mut egui::Ui, text: &RichText, tip: &str) -> bool {
    let mut clicked = false;
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        clicked = ui
            .add(egui::Button::new(text.clone()).corner_radius(5.0))
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

fn open_containing_folder(file: &str) {
    let dir = std::path::Path::new(file)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    let _ = std::process::Command::new(cmd).arg(dir).spawn();
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
