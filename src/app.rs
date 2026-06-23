//! 应用主体：会话管理 + 顶部标签 + 三区布局（系统信息 / 终端 / 文件）。

use std::sync::Arc;

use egui::{RichText, Sense};
use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{AuthMethod, ConflictPolicy, ConnectConfig, UiCommand, WorkerEvent};
use crate::ssh::{self, UiSink};
use crate::terminal::Terminal;
use crate::theme::Palette;
use crate::ui::connect::ConnectForm;
use crate::ui::file_panel::{self, FileAction, FilePanelState};
use crate::ui::sidebar::{self, NetHistory};

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
    /// 已读取待打开到编辑器的文件（path, content）
    pending_open: Vec<(String, String)>,
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
}

/// 传输的重发规格（断线重连/手动重试时据此重新发起，底层自动续传）。
#[derive(Clone)]
enum XferSpec {
    Download { remote: String, local: String },
    Upload { local: String, remote_dir: String },
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
struct PendingPaste {
    items: Vec<(String, bool)>,
    is_cut: bool,
    /// 源与目标是否不同服务器（需经本地中转）
    cross: bool,
    src_uid: u64,
    dest_uid: u64,
    dest_dir: String,
    src_label: String,
    dest_label: String,
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
                WorkerEvent::FileOpened { path, content } => {
                    self.pending_open.push((path, content));
                    self.status = crate::i18n::tr("已打开文件", "File opened").into();
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
                WorkerEvent::TransferStart { id, name, total, dir } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.name = name;
                        t.total = total;
                        t.dir = dir;
                    } else {
                        self.transfers.push(Transfer { id, name, dir, spec: None, paused: false, done: 0, total, ok: None, local: None, message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None });
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
                WorkerEvent::TransferDone { id, ok, message, refresh_dir } => {
                    let connected = self.connected;
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
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

// ===== 全局视图状态（折叠监控栏/文件栏、界面缩放）=====
// 设为进程级全局，便于侧栏背景层与各子控件（进程行/网卡/IP 等）共用同一右键菜单，
// 避免「右键到子控件上弹不出完整菜单」的不一致。
static SIDEBAR_COLLAPSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FILES_COLLAPSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ZOOM_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0); // 0 哨兵=未初始化(按 1.0)

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
}

/// 监控栏（左侧菜单栏）统一右键菜单：语言 / 字体大小 / 折叠视图 / 强制 X11。
/// 背景层与各子控件、以及折叠后的细条都调用它，保证右键处处一致。
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
    });
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
    /// 传输进度浮窗是否显示
    show_transfers: bool,
    /// 传输浮窗刚打开（本帧跳过"点击外部关闭"判定）
    xfer_just_opened: bool,
    /// 显示"确认退出"对话框
    show_close_confirm: bool,
    /// 待确认关闭的标签（仅当该会话仍连接中时弹确认）
    pending_close_tab: Option<usize>,
    /// 已确认可以关闭
    allow_close: bool,
    /// 编辑器标签页
    editors: Vec<EditorTab>,
    active_editor: usize,
    /// 「关闭全部」时若有未保存修改，弹确认框
    editor_close_confirm: bool,
    /// 看图工具：已打开的图片标签
    image_tabs: Vec<ImageTab>,
    active_image: usize,
    /// 一次性请求：新开/切换后把对应独立窗口置前并聚焦
    editor_focus: bool,
    image_focus: bool,
    /// 上次渲染时的激活编辑器/看图标签（用于侦测切换后滚到可视区）
    editor_shown: usize,
    image_shown: usize,
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

/// 看图工具的一个标签页（一张已加载的图片）。
struct ImageTab {
    server: String,
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
            show_transfers: false,
            xfer_just_opened: false,
            show_close_confirm: false,
            pending_close_tab: None,
            allow_close: false,
            editors: Vec::new(),
            active_editor: 0,
            editor_close_confirm: false,
            image_tabs: Vec::new(),
            active_image: 0,
            editor_focus: false,
            image_focus: false,
            editor_shown: 0,
            image_shown: 0,
            next_editor_id: 0,
            trim_after: None,
            show_forwards: false,
            fwd_just_opened: false,
            fwd_form: ForwardForm::default(),
            show_broadcast: false,
            broadcast_input: String::new(),
            conflict_policy: crate::store::load_conflict_policy().map(|s| ConflictPolicy::from_str(&s)).unwrap_or(ConflictPolicy::Overwrite),
            file_clip: None,
            pending_paste: None,
            relays: Vec::new(),
            relay_seq: 0,
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

        // 自检：看图工具——合成一张彩色渐变图打开
        if std::env::var("ISHELL_DEMO_IMG").is_ok() {
            if let Some(server) = app.sessions.first().map(|s| s.title.clone()) {
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
                s.transfers.push(Transfer { id: 1, name: "backup.tar.gz".into(), dir: Download, spec: None, paused: false, done: 73_400_320, total: 104_857_600, ok: None, local: None, message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None });
                s.transfers.push(Transfer { id: 2, name: "deploy.sh".into(), dir: Upload, spec: None, paused: false, done: 2048, total: 2048, ok: Some(true), local: None, message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None });
                s.transfers.push(Transfer { id: 3, name: "huge.bin".into(), dir: Download, spec: None, paused: false, done: 1024, total: 2048, ok: Some(true), local: Some("/root/Downloads/huge.bin".into()), message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None });
                // 自检：再塞一批，验证滚动
                for i in 4..16u64 {
                    s.transfers.push(Transfer { id: i, name: format!("file_{i}.dat"), dir: Download, spec: None, paused: false, done: i * 1000, total: 20000, ok: if i % 3 == 0 { Some(true) } else { None }, local: None, message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None });
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
            files: FilePanelState::default(),
            last_size: (0, 0),
            initialized: false,
            transfers: Vec::new(),
            next_xfer: 1,
            selected_nic: String::new(),
            proc_sort_mem: false,
            pending_open: Vec::new(),
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
        s.cmd_tx = cmd_tx;
        s.evt_rx = evt_rx;
        s.hostkey_tx = hostkey_tx;
        s.connected = false;
        s.initialized = false;
        s.terminal = Terminal::new();
        s.sysinfo = None;
        s.forwards.clear();
        s.pending_hostkey = None;
        s.kbd_prompt = None;
        s.reconnect_at = None;
        s.restore_cwd = true; // 重连成功后尝试 cd 回 last_cwd（保留不清空）
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
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.transfers.push(Transfer {
                    id, name, dir: crate::proto::TransferDir::Download,
                    spec: Some(XferSpec::Download { remote: remote.clone(), local: local.clone() }), paused: false,
                    done: 0, total: 0, ok: None,
                    local: Some(local.clone()), message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None,
                });
                let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy });
                self.show_transfers = true;
                self.xfer_just_opened = true;
            }
            FileAction::Upload { local, remote_dir } => {
                let name = local.rsplit('/').next().unwrap_or("upload").to_string();
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.transfers.push(Transfer {
                    id, name, dir: crate::proto::TransferDir::Upload,
                    spec: Some(XferSpec::Upload { local: local.clone(), remote_dir: remote_dir.clone() }), paused: false,
                    done: 0, total: 0, ok: None,
                    local: None, message: String::new(), show_err: false, speed: 0.0, last_done: 0, last_t: None,
                });
                let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy });
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
        let plan = PendingPaste {
            items: clip.items.clone(),
            is_cut: clip.is_cut,
            cross,
            src_uid: clip.src_uid,
            dest_uid: dest.uid,
            dest_dir,
            src_label: clip.src_label.clone(),
            dest_label: dest.title.clone(),
        };
        // 仅「跨服务器」需执行前确认（重操作、经本地中转）；同机无论复制还是移动都直接执行——
        // 同机移动是原子 mv，源在目标写成功前不会丢，无需二次确认。
        if plan.cross {
            self.pending_paste = Some(plan);
        } else {
            self.execute_paste(plan);
        }
    }

    /// 真正执行粘贴：同机服务器端 cp/mv；跨机建中转任务（下载→上传）。
    fn execute_paste(&mut self, plan: PendingPaste) {
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
        } else {
            // 跨服务器：源会话与目标会话都须在线
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
                self.relays.push(Relay {
                    src_path: src_path.clone(),
                    is_dir: *is_dir,
                    src_uid: plan.src_uid,
                    dest_uid: plan.dest_uid,
                    dest_dir: plan.dest_dir.clone(),
                    is_cut: plan.is_cut,
                    tmp,
                    phase: RelayPhase::Down(dlid),
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
        if plan.is_cut {
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
                Step::Wait => i += 1,
                Step::ToUpload => {
                    let dest_uid = self.relays[i].dest_uid;
                    let tmp = self.relays[i].tmp.to_string_lossy().into_owned();
                    let dest_dir = self.relays[i].dest_dir.clone();
                    if let Some(di) = self.session_idx_by_uid(dest_uid) {
                        let upid = {
                            let s = &mut self.sessions[di];
                            let id = s.next_xfer;
                            s.next_xfer += 1;
                            let _ = s.cmd_tx.send(UiCommand::Upload { id, local: tmp, remote_dir: dest_dir, policy: ConflictPolicy::Overwrite });
                            id
                        };
                        self.relays[i].phase = RelayPhase::Up(upid);
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
                            let _ = self.sessions[sidx].cmd_tx.send(UiCommand::Delete { path: src_path, is_dir: d });
                        }
                    }
                    Self::cleanup_relay_tmp(&t, d);
                    self.relays.remove(i);
                }
                Step::Failed => {
                    let (t, d) = (self.relays[i].tmp.clone(), self.relays[i].is_dir);
                    Self::cleanup_relay_tmp(&t, d);
                    self.relays.remove(i);
                }
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

        // 全局界面缩放（左侧栏可调）：仅在变化时设置，避免每帧触发重排
        if (ui.ctx().zoom_factor() - ui_zoom()).abs() > f32::EPSILON {
            ui.ctx().set_zoom_factor(ui_zoom());
        }

        // 1) 排空所有会话的后台事件，并在连接成功后初始化文件树
        let mut new_tabs: Vec<(String, String, String, UnboundedSender<UiCommand>)> = Vec::new();
        let mut new_images: Vec<(String, Vec<u8>, String)> = Vec::new();
        for s in &mut self.sessions {
            s.drain_events();
            if s.connected && !s.initialized {
                s.initialized = true;
                s.init_files();
            }
            for (path, content) in s.pending_open.drain(..) {
                new_tabs.push((path, content, s.title.clone(), s.cmd_tx.clone()));
            }
            for (path, data) in s.pending_image.drain(..) {
                new_images.push((path, data, s.title.clone()));
            }
        }
        // 跨服务器中转任务推进（下载完→上传，上传完→剪切则删源）
        self.process_relays();
        for (path, data, server) in new_images {
            self.image_focus = true; // 打开/切换后聚焦看图窗口
            // 同一服务器同一图片已打开则切到该标签
            if let Some(i) = self.image_tabs.iter().position(|t| t.server == server && t.path == path) {
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
                    if let Some(sess) = self.sessions.iter_mut().find(|s| s.title == server) {
                        sess.status = msg;
                    }
                }
            }
        }
        for (path, content, server, tx) in new_tabs {
            self.editor_focus = true; // 打开/切换后聚焦编辑器窗口
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

        // 看图工具浮窗
        self.image_window(&ctx);

        // 未知主机指纹确认（TOFU）
        self.host_key_dialog(&ctx);

        // 键盘交互认证（OTP / 2FA）
        self.kbd_prompt_dialog(&ctx);

        // 关闭活动标签二次确认
        self.close_tab_dialog(&ctx);

        // 粘贴二次确认（剪切 / 跨服务器）
        self.paste_confirm_dialog(&ctx);

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
                    ui.add(egui::TextEdit::singleline(&mut kp.answers[i]).desired_width(200.0).password(!echo));
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
        egui::Modal::new(egui::Id::new("paste_confirm")).show(ctx, |ui| {
            ui.set_width(420.0);
            let n = plan.items.len();
            let title = match (plan.is_cut, crate::i18n::current()) {
                (true, crate::i18n::Lang::Zh) => format!("确认移动 {n} 项？"),
                (false, crate::i18n::Lang::Zh) => format!("确认复制 {n} 项？"),
                (true, crate::i18n::Lang::En) => format!("Move {n} item(s)?"),
                (false, crate::i18n::Lang::En) => format!("Copy {n} item(s)?"),
            };
            ui.label(RichText::new(title).size(16.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{}  →  {}", plan.src_label, plan.dest_label)).color(Palette::TEXT_DIM).size(12.0));
            ui.label(RichText::new(&plan.dest_dir).monospace().size(11.0).color(Palette::TEXT_DIM));
            if plan.cross {
                ui.add_space(4.0);
                ui.label(RichText::new(crate::i18n::tr("跨服务器：经本地中转「下载→上传」，大文件较慢。", "Cross-server: relayed via local download→upload; large files are slower.")).color(Palette::WARN).size(11.0));
            }
            if plan.is_cut {
                ui.add_space(4.0);
                ui.label(RichText::new(crate::i18n::tr("剪切为移动：复制成功后会从源删除，不可恢复。", "Cut = move: source is deleted after a successful copy. Irreversible.")).color(Palette::DANGER).size(11.0));
            }
            // 列出名称（最多 8 个）
            ui.add_space(4.0);
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
        if go {
            if let Some(plan) = self.pending_paste.take() {
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
                    ui.label(RichText::new(crate::i18n::tr("暂无片段，在下方新增", "No snippets; add one below")).color(Palette::TEXT_DIM).size(12.0));
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

    /// 多标签文本编辑器浮窗。
    // 在独立 OS 窗口（viewport）里用 Panel::show(ctx) 渲染根 UI —— 这是 viewport 的惯用法；
    // egui 0.34 将其标记为 deprecated（建议 show_inside），但 viewport 场景下并无根 ui 可用。
    #[allow(deprecated)]
    fn editor_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if self.editors.is_empty() {
            return;
        }
        if self.active_editor >= self.editors.len() {
            self.active_editor = self.editors.len() - 1;
        }

        // 独立 OS 窗口（immediate viewport）：与主窗口分离，可自由移动/缩放，关闭走原生按钮。
        let vid = egui::ViewportId::from_hash_of("ishell_editor");
        let title = self
            .editors
            .get(self.active_editor)
            .map(|t| match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("iShell 编辑器 — {}·{}", t.server, t.editor.filename()),
                crate::i18n::Lang::En => format!("iShell Editor — {}·{}", t.server, t.editor.filename()),
            })
            .unwrap_or_else(|| crate::i18n::tr("iShell 编辑器", "iShell Editor").into());
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([480.0, 320.0]);

        ctx.show_viewport_immediate(vid, builder, |vctx, _class| {
            // 新开/切换文件后把本窗口置前并聚焦
            if self.editor_focus {
                vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                self.editor_focus = false;
            }
            // Ctrl+Tab / Ctrl+Shift+Tab 切换编辑器标签（先 consume，免被文本框当作 Tab 字符）
            let n = self.editors.len();
            if n > 1 {
                if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)) {
                    self.active_editor = (self.active_editor + n - 1) % n;
                } else if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                    self.active_editor = (self.active_editor + 1) % n;
                }
            }
            let mut close_tab: Option<usize> = None;
            let mut activate: Option<usize> = None;
            let mut do_save = false;

            // 标签栏
            egui::Panel::top("editor_tabs")
                .frame(egui::Frame::new().fill(Palette::BG).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(vctx, |ui| {
                    // 与 shell 标签一致：横向滚动（溢出可滚）+ 激活标签珊瑚下划线 + 切换后滚到可视区
                    let want_scroll = self.active_editor != self.editor_shown;
                    egui::ScrollArea::horizontal()
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (i, t) in self.editors.iter().enumerate() {
                                    let selected = i == self.active_editor;
                                    let fill = if selected { Palette::PANEL_2 } else { egui::Color32::TRANSPARENT };
                                    let r = egui::Frame::new()
                                        .fill(fill)
                                        .corner_radius(6)
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
                                    if selected {
                                        let rect = r.response.rect;
                                        ui.painter().hline(rect.left()..=rect.right(), rect.bottom() - 1.0, egui::Stroke::new(2.0, Palette::ACCENT));
                                        if want_scroll {
                                            ui.scroll_to_rect(rect.expand2(egui::vec2(12.0, 0.0)), None);
                                        }
                                    }
                                }
                            });
                        });
                    self.editor_shown = self.active_editor;
                });

            // 当前标签内容
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(8))
                .show(vctx, |ui| {
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
                    // 清除该编辑器在 egui 内存中的 TextEdit 状态（含撤销历史的文本快照）
                    vctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
                }
                if self.active_editor >= self.editors.len() && !self.editors.is_empty() {
                    self.active_editor = self.editors.len() - 1;
                }
                self.trim_after = Some(4);
            }

            // 原生关闭按钮：若有未保存修改先拦截并确认，否则关闭全部标签
            if vctx.input(|i| i.viewport().close_requested()) {
                if self.editors.iter().any(|t| t.editor.dirty()) {
                    vctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.editor_close_confirm = true;
                } else {
                    self.close_all_editors(vctx);
                }
            }
            if self.editor_close_confirm {
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
                    self.close_all_editors(vctx);
                } else if cancel {
                    self.editor_close_confirm = false;
                }
            }
        });
    }

    /// 关闭全部编辑器标签，并清理各自的 TextEdit 内存状态。
    fn close_all_editors(&mut self, ctx: &egui::Context) {
        for tab in self.editors.drain(..) {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(tab.text_id));
        }
        self.active_editor = 0;
        self.editor_close_confirm = false;
        self.trim_after = Some(4);
        ctx.request_repaint();
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
            .with_min_inner_size([320.0, 240.0]);

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

            // 标签栏
            egui::Panel::top("image_tabs")
                .frame(egui::Frame::new().fill(Palette::BG).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(vctx, |ui| {
                    // 与 shell 标签一致：横向滚动 + 激活珊瑚下划线 + 切换后滚到可视区
                    let want_scroll = self.active_image != self.image_shown;
                    egui::ScrollArea::horizontal()
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                for (i, t) in self.image_tabs.iter().enumerate() {
                                    let selected = i == self.active_image;
                                    let fill = if selected { Palette::PANEL_2 } else { egui::Color32::TRANSPARENT };
                                    let r = egui::Frame::new()
                                        .fill(fill)
                                        .corner_radius(6)
                                        .inner_margin(egui::Margin::symmetric(8, 3))
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                                                let label = format!("{} {}·{}", icon::IMAGE, t.server, fname);
                                                let color = if selected { Palette::TEXT } else { Palette::TEXT_DIM };
                                                if ui.add(egui::Label::new(RichText::new(label).color(color).size(12.0)).selectable(false).sense(Sense::click())).clicked() {
                                                    activate = Some(i);
                                                }
                                                if ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                                                    close_tab = Some(i);
                                                }
                                            });
                                        });
                                    if selected {
                                        let rect = r.response.rect;
                                        ui.painter().hline(rect.left()..=rect.right(), rect.bottom() - 1.0, egui::Stroke::new(2.0, Palette::ACCENT));
                                        if want_scroll {
                                            ui.scroll_to_rect(rect.expand2(egui::vec2(12.0, 0.0)), None);
                                        }
                                    }
                                }
                            });
                        });
                    self.image_shown = self.active_image;
                });

            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(8))
                .show(vctx, |ui| {
                    if let Some(t) = self.image_tabs.get_mut(self.active_image) {
                        // 工具栏：路径 + 尺寸/缩放 + 另存为/1:1/适应窗口
                        // 按钮先占右侧，路径在剩余宽度里横向滚动、默认贴右
                        ui.horizontal(|ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button(crate::i18n::tr("适应窗口", "Fit")).clicked() {
                                    t.zoom = 0.0;
                                    t.offset = egui::Vec2::ZERO;
                                }
                                if ui.button("1:1").clicked() {
                                    t.zoom = 1.0;
                                    t.offset = egui::Vec2::ZERO;
                                }
                                // 另存为本地文件（写回原始字节，保留源格式）
                                if ui.button(crate::i18n::tr("另存为…", "Save as…")).clicked() && !t.data.is_empty() {
                                    let fname = t.path.rsplit('/').next().unwrap_or("image");
                                    if let Some(path) = rfd::FileDialog::new().set_file_name(fname).save_file() {
                                        save_msg = Some(match std::fs::write(&path, &t.data) {
                                            Ok(_) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已保存到 {}", path.display()), crate::i18n::Lang::En => format!("Saved to {}", path.display()) },
                                            Err(e) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("保存失败：{e}"), crate::i18n::Lang::En => format!("Save failed: {e}") },
                                        });
                                    }
                                }
                                if t.zoom > 0.0 {
                                    ui.label(RichText::new(format!("{}%", (t.zoom * 100.0).round() as i32)).color(Palette::TEXT_DIM).size(11.0));
                                }
                                ui.label(RichText::new(format!("{}×{}", t.size.x as i32, t.size.y as i32)).color(Palette::TEXT_DIM).size(11.0));
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    crate::ui::path_scroll(ui, &t.path);
                                });
                            });
                        });
                        ui.separator();

                        // 画布：灰底 + 图片，支持滚轮缩放与拖动平移
                        let avail = ui.available_size();
                        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(rect, 0.0, Palette::PANEL_2);

                        // 双击复位为「适应窗口」
                        if resp.double_clicked() {
                            t.zoom = 0.0;
                            t.offset = egui::Vec2::ZERO;
                        }
                        // 首帧/复位后自动适应窗口（不放大超过 1:1）
                        if t.zoom <= 0.0 {
                            let fit = (rect.width() / t.size.x).min(rect.height() / t.size.y).min(1.0);
                            t.zoom = fit.clamp(0.02, 32.0);
                            t.offset = egui::Vec2::ZERO;
                        }
                        // 滚轮缩放：以光标为锚点，保持光标下的像素不动
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
                        // 拖动平移
                        if resp.dragged() {
                            t.offset += resp.drag_delta();
                        }

                        // 绘制图片
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
        let mut kill = false;
        let mut copy_target: Option<String> = None;
        let copied_t = self.proc_popup.as_ref().and_then(|p| p.copied_t);
        let now = ctx.input(|i| i.time);
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
                if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::SKULL, crate::i18n::tr("强制结束 (kill -9)", "Kill (-9)"))).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                    kill = true;
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
        let mut cancel_id: Option<u64> = None;
        let mut toggle_err: Option<u64> = None;
        let mut remove_id: Option<u64> = None;
        let mut delete_id: Option<(u64, String)> = None;
        let mut resume_id: Option<u64> = None;
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

                let Some(s) = self.sessions.get(idx) else { return };
                if s.transfers.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("暂无传输任务", "No transfers")).color(Palette::TEXT_DIM).size(12.0));
                }
                let mut open_dir: Option<String> = None;
                // 列表过长时滚动：约 8 条高度封顶，其余可滚动查看
                egui::ScrollArea::vertical().max_height(400.0).auto_shrink([false, true]).show(ui, |ui| {
                for t in s.transfers.iter().rev().take(50) {
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
                                            toggle_err = Some(t.id);
                                        }
                                        if t.spec.is_some()
                                            && ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                                .on_hover_text(crate::i18n::tr("重试", "Retry"))
                                                .clicked()
                                        {
                                            resume_id = Some(t.id);
                                        }
                                    }
                                    None if t.paused => {
                                        // 已中断/暂停：续传按钮
                                        if ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("续传", "Resume"))
                                            .clicked()
                                        {
                                            resume_id = Some(t.id);
                                        }
                                    }
                                    None => {
                                        // 进行中：取消按钮 + 转圈
                                        if ui.add(egui::Button::new(RichText::new(icon::X_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("取消", "Cancel"))
                                            .clicked()
                                        {
                                            cancel_id = Some(t.id);
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
                            // 大小靠左
                            p.text(
                                egui::pos2(rect.left() + 6.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                crate::ui::fmt_bytes(t.total as f64),
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
                            let mut detail = format!("{} / {}", crate::ui::fmt_bytes(t.done as f64), crate::ui::fmt_bytes(t.total as f64));
                            if t.speed > 0.0 {
                                detail.push_str(&format!("  ·  {}", crate::ui::fmt_rate(t.speed)));
                            }
                            ui.label(RichText::new(detail).size(10.0).color(Palette::TEXT_DIM));
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
                        if ui.button(crate::i18n::tr("删除记录", "Remove from list")).clicked() {
                            remove_id = Some(t.id);
                            ui.close();
                        }
                        if let Some(local) = &t.local {
                            if ui.button(RichText::new(crate::i18n::tr("删除文件并移除记录", "Delete file & remove")).color(Palette::DANGER)).clicked() {
                                delete_id = Some((t.id, local.clone()));
                                ui.close();
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                });
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
        // 取消传输：向 worker 发送取消指令
        if let Some(id) = cancel_id {
            if let Some(s) = self.sessions.get(idx) {
                let _ = s.cmd_tx.send(UiCommand::CancelTransfer(id));
            }
            self.xfer_just_opened = true; // 避免点击被当作窗外点击而关窗
        }
        // 续传/重试：按重发规格重新发起，底层据已有字节自动续传
        if let Some(id) = resume_id {
            if let Some(s) = self.sessions.get_mut(idx) {
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
        if let Some(id) = toggle_err {
            if let Some(s) = self.sessions.get_mut(idx) {
                if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                    t.show_err = !t.show_err;
                }
            }
            self.xfer_just_opened = true;
        }
        // 删除记录（仅移除列表项）
        if let Some(id) = remove_id {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 删除文件并移除记录
        if let Some((id, path)) = delete_id {
            let _ = std::fs::remove_file(&path);
            if let Some(s) = self.sessions.get_mut(idx) {
                s.transfers.retain(|t| t.id != id);
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
