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
                    self.status = "已连接".into();
                }
                WorkerEvent::Disconnected(reason) => {
                    self.connected = false;
                    self.status = reason;
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
                WorkerEvent::ForwardStatus { id, ok, message } => {
                    if let Some(f) = self.forwards.iter_mut().find(|f| f.id == id) {
                        f.ok = ok;
                        f.status = message;
                    }
                }
                WorkerEvent::HostKeyPrompt { host, fingerprint } => {
                    self.pending_hostkey = Some((host, fingerprint));
                    self.status = "等待确认主机指纹 …".into();
                }
                WorkerEvent::FileOpened { path, content } => {
                    self.pending_open.push((path, content));
                    self.status = "已打开文件".into();
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
    connect_form: ConnectForm,
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
    /// 端口转发管理窗口是否显示
    show_forwards: bool,
    /// 转发浮窗刚打开（本帧跳过"点击外部关闭"判定）
    fwd_just_opened: bool,
    /// 新增转发表单
    fwd_form: ForwardForm,
    /// 自检截图模式（由环境变量触发，正常使用时为 None）
    shot: Option<Shot>,
}

/// 拖拽会话标签时携带的源索引。
#[derive(Clone)]
struct TabDrag(usize);

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
            connect_form: form,
            show_transfers: false,
            xfer_just_opened: false,
            show_close_confirm: false,
            allow_close: false,
            editors: Vec::new(),
            active_editor: 0,
            show_forwards: false,
            fwd_just_opened: false,
            fwd_form: ForwardForm::default(),
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
                app.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/home/e5-1/demo.rs".into(), code),
                    server: server.clone(),
                    cmd_tx: tx.clone(),
                });
                app.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/etc/hosts".into(), "127.0.0.1 localhost\n::1 localhost\n".into()),
                    server,
                    cmd_tx: tx,
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
                    status: "启动中 …".into(),
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
    fn spawn_session(&mut self, cfg: ConnectConfig) {
        self.show_close_confirm = false; // 新建会话则取消退出提示
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let (hostkey_tx, hostkey_rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = UiSink::new(evt_tx, self.ctx.clone());

        self.runtime.spawn(ssh::run(cfg.clone(), cmd_rx, sink, hostkey_rx));

        self.sessions.push(Session {
            title: if cfg.label.trim().is_empty() { cfg.username.clone() } else { cfg.label.trim().to_string() },
            tip: format!("{}@{}:{}", cfg.username, cfg.host, cfg.port),
            cmd_tx,
            evt_rx,
            connected: false,
            status: "连接中 …".into(),
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
        });
        self.active = Some(self.sessions.len() - 1);
    }

    /// 拖动排序：把会话从 `from` 移动到放置目标 `to` 处。
    fn reorder_session(&mut self, from: usize, to: usize) {
        let len = self.sessions.len();
        if from >= len || to >= len || from == to {
            return;
        }
        let moved = self.sessions.remove(from);
        let dest = if from < to { to - 1 } else { to };
        let dest = dest.min(self.sessions.len());
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
                let local = downloads_dir().join(&name).to_string_lossy().into_owned();
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
                s.status = format!("已复制路径：{p}");
            }
            FileAction::OpenFile { path, force } => {
                s.status = format!("打开中：{path} …");
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
                self.editors.push(EditorTab {
                    editor: crate::ui::editor::Editor::new(path, content),
                    server,
                    cmd_tx: tx,
                });
                self.active_editor = self.editors.len() - 1;
            }
        }

        // 2) 连接对话框（浮动窗口）
        let ctx = ui.ctx().clone();
        if let Some(cfg) = self.connect_form.show(&ctx) {
            self.spawn_session(cfg);
        }

        // 3) 左侧操作栏：独立全高区域
        egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=460.0)
            .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 8 }))
            .show_inside(ui, |ui| match self.active {
                Some(idx) if idx < self.sessions.len() => {
                    let s = &mut self.sessions[idx];
                    sidebar::show(ui, s.sysinfo.as_ref(), &s.net_hist, &mut s.selected_nic, &mut s.proc_sort_mem);
                }
                _ => {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(egui_phosphor::regular::PLUGS).size(28.0).color(Palette::TEXT_DIM));
                        ui.label(RichText::new("未连接").color(Palette::TEXT_DIM));
                    });
                }
            });

        // 4) 顶部选项卡（仅位于右侧区域之上）
        self.top_tabs(ui);

        // 5) 右侧主体
        match self.active {
            Some(idx) if idx < self.sessions.len() => self.right_body(ui, idx),
            _ => self.welcome(ui),
        }

        // 传输进度浮窗
        self.transfer_window(&ctx);

        // 端口转发管理浮窗
        self.forward_window(&ctx);

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
                ui.label(RichText::new("未知主机").size(16.0).strong());
                ui.add_space(8.0);
                ui.label(format!("首次连接主机：{host}"));
                ui.add_space(4.0);
                ui.label(RichText::new("指纹 (SHA256)：").color(Palette::TEXT_DIM).size(12.0));
                ui.label(RichText::new(&fp).monospace());
                ui.add_space(6.0);
                ui.label(RichText::new("请确认该指纹与目标服务器一致；信任后将写入 ~/.ssh/known_hosts。").color(Palette::TEXT_DIM).size(11.0));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 96.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    if ui.add(egui::Button::new(RichText::new("信任并连接").color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 28.0))).clicked() {
                        decision = Some(true);
                    }
                    if ui.add(egui::Button::new("拒绝").min_size(egui::vec2(bw, 28.0))).clicked() {
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
                        ui.label(RichText::new("确认退出").size(16.0).strong());
                        ui.add_space(6.0);
                        ui.label(format!("还有 {} 个会话处于连接中", self.sessions.len()));
                        ui.label("确定退出 iShell 吗？");
                    });
                    ui.add_space(12.0);
                    // 按钮行水平居中（固定按钮宽度 + 居中留白）
                    ui.horizontal(|ui| {
                        let bw = 72.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        let space = ((ui.available_width() - total) / 2.0).max(0.0);
                        ui.add_space(space);
                        if ui
                            .add(egui::Button::new(RichText::new("退出").color(egui::Color32::WHITE)).fill(Palette::DANGER).min_size(egui::vec2(bw, 28.0)))
                            .clicked()
                        {
                            self.allow_close = true;
                            self.show_close_confirm = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if ui.add(egui::Button::new("取消").min_size(egui::vec2(bw, 28.0))).clicked() {
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
                    // 右侧按钮先占位（传输 / 转发 / 状态），剩余空间留给可滚动的标签条
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let active_xfers = self
                            .active
                            .and_then(|i| self.sessions.get(i))
                            .map(|s| s.transfers.iter().filter(|t| t.ok.is_none()).count())
                            .unwrap_or(0);
                        let label = if active_xfers > 0 {
                            format!("{} 传输 {}", icon::ARROWS_DOWN_UP, active_xfers)
                        } else {
                            format!("{} 传输", icon::ARROWS_DOWN_UP)
                        };
                        if flat_button(ui, &RichText::new(label), "显示/隐藏传输进度") {
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
                            format!("{} 转发 {}", icon::ARROWS_LEFT_RIGHT, nfwd)
                        } else {
                            format!("{} 转发", icon::ARROWS_LEFT_RIGHT)
                        };
                        if flat_button(ui, &RichText::new(flabel), "端口转发管理") {
                            self.show_forwards = !self.show_forwards;
                            if self.show_forwards {
                                self.fwd_just_opened = true;
                            }
                        }
                        if let Some(idx) = self.active {
                            if let Some(s) = self.sessions.get(idx) {
                                let c = if s.connected { Palette::OK } else { Palette::WARN };
                                ui.label(RichText::new(&s.status).color(c).size(12.0));
                            }
                        }

                        // 剩余空间：标签条横向可滚动（标签多时不再与右侧按钮重叠）
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            egui::ScrollArea::horizontal()
                                .auto_shrink([false, false])
                                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                                .drag_to_scroll(false) // 让标签的拖拽排序直接生效，无需先点一下
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        for (i, s) in self.sessions.iter().enumerate() {
                                            let selected = self.active == Some(i);
                                            let fill = if selected { Palette::PANEL } else { egui::Color32::TRANSPARENT };
                                            let id = egui::Id::new(("shtab", i));
                                            let resp = ui
                                                .dnd_drag_source(id, TabDrag(i), |ui| {
                                                    egui::Frame::new()
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
                                                                ui.add(egui::Label::new(title).selectable(false)).on_hover_text(s.tip.as_str());
                                                                if ui
                                                                    .add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                                                                    .on_hover_text("关闭会话")
                                                                    .clicked()
                                                                {
                                                                    to_close = Some(i);
                                                                }
                                                            });
                                                        });
                                                })
                                                .response;
                                            if resp.clicked() {
                                                to_activate = Some(i);
                                            }
                                            if resp.middle_clicked() {
                                                to_close = Some(i);
                                            }
                                            if let Some(p) = resp.dnd_release_payload::<TabDrag>() {
                                                reorder = Some((p.0, i));
                                            }
                                        }
                                        if flat_button(ui, &RichText::new(icon::PLUS).size(15.0), "新建连接") {
                                            self.connect_form.open_dialog();
                                            self.show_close_confirm = false;
                                        }
                                    });
                                });
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
                    if crate::ui::editor::content(ui, &mut tab.editor) {
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
                self.editors.remove(i);
            }
            if self.active_editor >= self.editors.len() && !self.editors.is_empty() {
                self.active_editor = self.editors.len() - 1;
            }
            // 关闭大文件后把空闲内存归还 OS
            trim_memory();
        }
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
                    ui.label(RichText::new(format!("{}  端口转发", icon::ARROWS_LEFT_RIGHT)).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                    });
                });
                ui.separator();

                let Some(idx) = idx else {
                    ui.add_space(4.0);
                    ui.label(RichText::new("请先连接一个会话").color(Palette::TEXT_DIM).size(12.0));
                    return;
                };

                // 新增表单（分段按钮代替下拉，避免点击下拉被判为窗口外而自动关闭）
                let f = &mut self.fwd_form;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut f.kind, 0usize, "本地转发");
                    ui.selectable_value(&mut f.kind, 1usize, "动态 SOCKS5");
                });
                ui.horizontal(|ui| {
                    ui.label("本地");
                    ui.add(egui::TextEdit::singleline(&mut f.bind).desired_width(84.0).hint_text("127.0.0.1"));
                    ui.label(":");
                    ui.add(egui::TextEdit::singleline(&mut f.local_port).desired_width(48.0).hint_text("端口"));
                });
                if f.kind == 0 {
                    ui.horizontal(|ui| {
                        ui.label("目标");
                        ui.add(egui::TextEdit::singleline(&mut f.target_host).desired_width(120.0).hint_text("主机/IP"));
                        ui.label(":");
                        ui.add(egui::TextEdit::singleline(&mut f.target_port).desired_width(48.0).hint_text("端口"));
                    });
                }
                ui.add_space(4.0);
                if ui.add(egui::Button::new(RichText::new(format!("{}  添加转发", icon::PLUS)).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
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
                        ui.label(RichText::new("暂无转发任务").color(Palette::TEXT_DIM).size(12.0));
                    }
                    for fwd in &s.forwards {
                        ui.horizontal(|ui| {
                            let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, if fwd.ok { Palette::OK } else { Palette::DANGER });
                            ui.label(RichText::new(&fwd.label).size(12.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text("删除").clicked() {
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
                s.forwards.push(ForwardEntry { id, label, status: "启动中 …".into(), ok: true });
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
                    ui.label(RichText::new(format!("{}  文件传输", icon::ARROWS_DOWN_UP)).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                    });
                });
                ui.separator();

                let Some(s) = self.sessions.get(idx) else { return };
                if s.transfers.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new("暂无传输任务").color(Palette::TEXT_DIM).size(12.0));
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
                                        .on_hover_text("打开所在文件夹")
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
                    if ui.button("清除已完成").clicked() {
                        clear = true;
                    }
                }
            });
        if clear {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.transfers.retain(|t| t.ok.is_none());
            }
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
                    RichText::new("现代化 Rust SSH 客户端")
                        .size(16.0)
                        .color(Palette::TEXT_DIM),
                );
                ui.add_space(20.0);
                if ui
                    .add(egui::Button::new(RichText::new(format!("{}  新建连接", egui_phosphor::regular::PLUS)).size(16.0).color(egui::Color32::WHITE)).fill(Palette::ACCENT))
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
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Palette::TERM_BG)
                    .inner_margin(6)
                    .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 0 }),
            )
            .show_inside(root, |ui| {
                let s = &mut self.sessions[idx];
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
    }
}

/// 扁平按钮（无边框、悬停高亮），用于标签栏等处，贴近 FinalShell 风格。
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

/// 本地下载目录：优先 `$HOME/Downloads`，否则当前目录。
fn downloads_dir() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let d = std::path::Path::new(&home).join("Downloads");
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    std::path::PathBuf::from(".")
}
