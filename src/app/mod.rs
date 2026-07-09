//! 应用主体：会话管理 + 顶部标签 + 三区布局（系统信息 / 终端 / 文件）。

mod util;
mod view_state;
mod widgets;
#[allow(unused_imports)]
use util::*;
#[allow(unused_imports)]
use view_state::*;
#[allow(unused_imports)]
use widgets::*;
mod doc_view;
mod transfers;
mod windows;
mod dialogs;
mod editor_win;
mod session_events;
mod file_actions;
mod layout;
mod frame;
pub use widgets::view_context_menu;


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
    /// 跟随读取返回：(路径, 新增字节, 新 offset, 是否截断/轮转)
    pending_tail: Vec<(String, Vec<u8>, u64, bool)>,
    /// PDF 页数查询返回：(占位标签 id, 页数)
    pending_pdf_info: Vec<(u64, u32)>,
    /// PDF 单页 PNG 返回：(路径, 页码, PNG 字节)
    pending_pdf_page: Vec<(String, u32, Vec<u8>)>,
    /// PDF 查找返回：(路径, 命中列表, 失败消息)
    pending_pdf_search: Vec<(String, Vec<(u32, String)>, Option<String>)>,
    /// 文档原始字节返回：(占位标签 id, 字节)
    pending_doc: Vec<(u64, Vec<u8>)>,
    pending_conflict: Vec<String>,
    /// 保存失败（网络/权限等）：(路径, 原因)
    pending_save_failed: Vec<(String, String)>,
    /// 需要在 App 层弹 toast 的警告（如编码丢字）；Session 无 toast/ctx，经此转交
    pending_warn: Vec<String>,
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
    /// 远端是否支持 /proc 系统监控（None=尚未探测；false 时侧栏提示并跳过杀进程等）
    monitor_ok: Option<bool>,
}

/// 传输的重发规格（断线重连/手动重试时据此重新发起，底层自动续传）。
#[derive(Clone)]
pub(super) enum XferSpec {
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




/// UI 侧的一条传输记录。
pub(super) struct Transfer {
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

/// 文件传输子系统的聚合状态（剪贴板 / 待确认粘贴 / 跨服务器中转 / 直传）。
/// 从 App 抽出的内聚字段组，配套 transfers.rs 里的方法。
#[derive(Default)]
struct Transfers {
    /// 文件剪贴板（跨 tab 共享）：复制/剪切的源项
    file_clip: Option<FileClip>,
    /// 待确认的粘贴（剪切 或 跨服务器：执行前二次确认）
    pending_paste: Option<PendingPaste>,
    /// 跨服务器中转任务（下载→上传→可选删源）
    relays: Vec<Relay>,
    /// 中转临时目录去重计数
    relay_seq: u64,
    /// 粘贴确认弹框里「直传/中转」互斥选择的当前值（false=中转，默认更安全）
    confirm_direct: bool,
    /// 进行中的直传任务追踪（成功删源/刷新；失败弹回退）
    direct_jobs: Vec<DirectJob>,
    /// 直传失败、待确认「转中转」的计划 + 原因（队列：多个失败依次弹，避免同帧互相覆盖）
    pending_direct_fallback: Vec<DirectFallback>,
    /// 直传目标不在本机 known_hosts：待用户确认首次 TOFU（在源机 accept-new）后再执行
    pending_direct_hostkey: Option<PendingPaste>,
}

/// 命令片段库状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
struct Snippets {
    /// 片段浮窗是否显示
    show: bool,
    /// 浮窗刚打开（本帧跳过"点击外部关闭"判定）
    just_opened: bool,
    /// 片段数据
    list: Vec<crate::store::Snippet>,
    /// 正在编辑的片段索引（None = 新建）
    editing: Option<usize>,
    /// 编辑表单缓冲
    name: String,
    cmd: String,
    run: bool,
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







pub struct App {
    runtime: Arc<tokio::runtime::Runtime>,
    ctx: egui::Context,
    sessions: Vec<Session>,
    active: Option<usize>,
    /// 主窗口会话标签条的拖拽重排 + 滚动状态
    tabbar: TabBar,
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
    /// 看图工具状态（标签、激活项、聚焦请求、拖动重排）
    image: ImageView,
    /// docx 后台解析结果通道：(占位标签 id, 解析结果)
    doc_parse_tx: std::sync::mpsc::Sender<(u64, Result<(crate::ui::docx::Doc, std::collections::HashMap<String, egui::TextureHandle>), String>)>,
    doc_parse_rx: std::sync::mpsc::Receiver<(u64, Result<(crate::ui::docx::Doc, std::collections::HashMap<String, egui::TextureHandle>), String>)>,
    /// 下一个编辑器 TextEdit Id 序号
    next_editor_id: u64,
    /// 关闭大文件编辑器后延迟若干帧再 malloc_trim（等 galley 缓存被淘汰）
    trim_after: Option<u32>,
    /// 端口转发管理窗口 UI 状态（开关 / 表单 / 编辑 / 删除确认 / 校验错误）
    fwd: ForwardUi,
    /// 上一帧活动会话的 uid——切换会话时复位「跨会话易串台」的临时 UI 态（转发确认/编辑、进程弹窗）
    active_uid_prev: Option<u64>,
    /// 命令广播栏是否显示 + 输入内容
    show_broadcast: bool,
    broadcast_input: String,
    // 折叠监控栏/文件栏与界面缩放改为进程级全局状态（见本文件底部 view 状态），
    // 以便侧栏背景层与各子控件共用同一右键菜单。
    /// 传输冲突策略（目标已存在时；默认覆盖）
    conflict_policy: ConflictPolicy,
    /// 文件传输/复制粘贴/跨服务器中转与直传的聚合状态（从 App 抽出的内聚字段组）
    xfer: Transfers,
    /// 命令片段库（窗口开关 + 数据 + 编辑表单缓冲）
    snip: Snippets,
    /// 进程/GPU 详情小窗状态
    popups: Popups,
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

/// 端口转发管理窗口的 UI 状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
struct ForwardUi {
    /// 管理窗口是否显示
    show: bool,
    /// 浮窗刚打开（本帧跳过"点击外部关闭"判定）
    just_opened: bool,
    /// 待删除确认的转发 id（行内二次确认）
    confirm_del: Option<u64>,
    /// 新增/编辑转发表单
    form: ForwardForm,
    /// 正在编辑的转发 id（Some=编辑模式）
    editing: Option<u64>,
    /// 表单内联校验错误（端口占用/参数无效等）
    error: Option<String>,
    /// 非回环绑定待二次确认的转发规格（确认后才真正添加）
    pending_open_bind: Option<crate::proto::ForwardSpec>,
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
/// 文档标签内容（PDF / Word）：挂在 EditorTab 上，Some 时该标签渲染文档查看器
/// 而非文本编辑器；占位/进度/失败/关闭全部复用编辑器标签框架。
enum DocKind {
    /// PDF：远端 poppler 逐页渲染为 PNG，本地页缓存 + 前后预取
    Pdf {
        /// 总页数（就位时已知，恒 >0）
        pages: u32,
        /// 当前页（1 基）
        cur: u32,
        /// 缩放；0 = 适应窗口宽
        zoom: f32,
        /// 页缓存（小 LRU：按插入序淘汰最旧）
        cache: Vec<(u32, egui::TextureHandle, egui::Vec2)>,
        /// 在途渲染请求的页码
        pending: std::collections::HashSet<u32>,
        /// 上次滚动连续翻页时刻（冷却 0.3s，防滚轮惯性连环翻页）
        flip_at: f64,
        /// 全文查找：输入、开关、命中 (页码, 片段)、当前命中序号、在途标志、失败消息
        search: String,
        search_open: bool,
        hits: Vec<(u32, String)>,
        hit_sel: usize,
        searching: bool,
        search_msg: Option<String>,
    },
    /// Word(docx)：本地解析的重排阅读视图
    Docx {
        doc: crate::ui::docx::Doc,
        /// 内嵌图片纹理（media 名 → 纹理）
        images: std::collections::HashMap<String, egui::TextureHandle>,
        /// 各内容块上一帧实测高度（视口裁剪用：屏幕外块直接占位跳过渲染）
        heights: Vec<f32>,
        /// 本地查找：输入、开关、命中块索引、当前命中序号、待滚动目标块
        search: String,
        search_open: bool,
        hits: Vec<usize>,
        hit_sel: usize,
        scroll_to: Option<usize>,
    },
}

/// 编辑器保存流程的类型化状态机，取代旧的 saving/save_conflict/close_on_saved/save_rev
/// 布尔量组合——那些组合能表达非法状态（如「保存中且冲突」）。此枚举保证任一时刻
/// 至多处于一个合法状态。保存进度/动画字段（save_at 等）是展示层，另行保留。
enum SaveState {
    /// 空闲：无在途保存、无未决冲突。
    Idle,
    /// 已发出 WriteFile、等待结果。rev=发出时的修订签名 (vver, 编码, 行尾)，
    /// 收到 FileSaved 且签名一致才算已保存；close_after=完成后是否关闭标签。
    Saving { rev: (u64, String, crate::proto::Eol), close_after: bool },
    /// 检测到外部改动（未写入），显示冲突横幅，等用户选择覆盖/取消。
    Conflict,
}

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
    /// 保存流程状态机（见 SaveState）。
    save: SaveState,
    /// 保存动画起始时刻（ctx 时间，秒）：驱动标签底部珊瑚线「绿扫→珊瑚扫」表示已保存；None=无动画
    save_at: Option<f64>,
    /// 保存写入进度（done/total 字节）：驱动绿扫跟随实际上传速度
    save_done: u64,
    save_total: u64,
    /// 跟随模式（tail -f）状态：下次读取的字节偏移（u64::MAX=待初始化）、
    /// 是否有在途请求、上次轮询时刻。注意：跟随期间**不更新 mtime**——外部对文件
    /// 中间的修改无法检测，保留旧 mtime 让保存走冲突确认，避免静默覆盖他人修改。
    tail_offset: u64,
    tail_pending: bool,
    tail_last: f64,
    /// Some = 文档标签（PDF/Word 查看器）；None = 常规文本编辑器
    doc: Option<DocKind>,
    /// 跟随模式跨块解码缓冲：上一块末尾不完整的多字节字符原始字节，与下一块拼接
    ///（否则 UTF-8/GBK 字符跨 512KB 分块边界会变替换字符并永久丢失原始字节）
    tail_carry: Vec<u8>,
    /// 绿扫完成、进入「珊瑚扫回」阶段的起始时刻（ctx 时间）；None=仍在绿扫阶段
    save_done_at: Option<f64>,
}

impl EditorTab {
    /// 保存进行中（已发 WriteFile、未收到结果）——期间屏蔽再次保存。
    fn is_saving(&self) -> bool {
        matches!(self.save, SaveState::Saving { .. })
    }
    /// 存在未决的外部改动冲突（显示横幅）。
    fn is_conflict(&self) -> bool {
        matches!(self.save, SaveState::Conflict)
    }
    /// 用户是否要求「保存成功后关闭标签」（仅在保存中有意义）。FSM 不变式，测试覆盖。
    #[allow(dead_code)]
    fn wants_close(&self) -> bool {
        matches!(self.save, SaveState::Saving { close_after: true, .. })
    }
    /// 进入「保存中」：记录发出时的修订签名与是否保存后关闭。
    fn begin_save(&mut self, close_after: bool) {
        self.save = SaveState::Saving { rev: self.editor.save_rev(), close_after };
    }
    /// 若处于保存中，标记「完成后关闭」（用于保存进行时用户点『保存并关闭』）。
    fn request_close_on_saved(&mut self) {
        if let SaveState::Saving { close_after, .. } = &mut self.save {
            *close_after = true;
        }
    }
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
/// 主窗口会话标签条的拖拽重排 + 滚动状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
struct TabBar {
    /// 正在拖拽排序的标签源索引
    drag: Option<usize>,
    /// 拖拽起点在标签内的横向抓取偏移（让被拖标签跟手而不跳到光标处）
    grab_dx: f32,
    /// 标签条总宽缓存（用于撑出横向滚动内容宽度）
    total_w: f32,
    /// 请求把激活标签滚动到可视区（新建/点击/Ctrl+Tab 切换时置位）
    scroll_to_active: bool,
}

/// 进程/GPU 详情小窗状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
struct Popups {
    /// 进程详情小窗
    proc: Option<ProcPopup>,
    proc_just_opened: bool,
    /// GPU 详情小窗（仅记录弹出位置，数据每帧从活动会话取）
    gpu: Option<egui::Pos2>,
    gpu_just_opened: bool,
}

/// 看图工具窗口状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
struct ImageView {
    /// 已打开的图片标签
    tabs: Vec<ImageTab>,
    /// 当前激活标签下标
    active: usize,
    /// 一次性请求：新开/切换后把看图窗口置前并聚焦
    focus: bool,
    /// 上次渲染时的激活标签（用于侦测切换后滚到可视区）
    shown: usize,
    /// 标签拖动重排状态（仿主窗口）
    tab_drag: Option<usize>,
    grab_dx: f32,
    total_w: f32,
}

pub(super) struct ImageTab {
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
        // docx 后台解析结果通道（解析/纹理解码在工作线程，UI 不冻结）
        let (doc_parse_tx, doc_parse_rx) = std::sync::mpsc::channel();
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
            tabbar: TabBar { total_w: 1.0, ..Default::default() },
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
            image: ImageView::default(),
            doc_parse_tx,
            doc_parse_rx,
            next_editor_id: 0,
            trim_after: None,
            fwd: ForwardUi::default(),
            active_uid_prev: None,
            show_broadcast: false,
            broadcast_input: String::new(),
            conflict_policy: crate::store::load_conflict_policy().map(|s| ConflictPolicy::from_str(&s)).unwrap_or(ConflictPolicy::Overwrite),
            xfer: Transfers::default(),
            snip: Snippets { list: crate::store::load_snippets(), run: true, ..Default::default() },
            popups: Popups::default(),
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
                let mut ed = lock_mutex(&app.editor_state);
                ed.tabs.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/home/e5-1/demo.rs".into(), code),
                    server: server.clone(),
                    uid,
                    cmd_tx: tx.clone(),
                    text_id: t1,
                    load_id: None,
                    load_done: 0,
            load_total: 0,
            save: SaveState::Idle,
            save_at: None,
            save_done: 0,
            save_total: 0,
            save_done_at: None,
            tail_offset: u64::MAX,
            tail_pending: false,
            tail_last: 0.0,
            doc: None,
            tail_carry: Vec::new(),
                });
                let mut big_ed = crate::ui::editor::Editor::new("/var/log/huge.log".into(), big);
                big_ed.readonly = true; // 演示大文件默认只读
                ed.tabs.push(EditorTab {
                    editor: big_ed,
                    server: server.clone(),
                    uid,
                    cmd_tx: tx.clone(),
                    text_id: t2,
                    load_id: None,
                    load_done: 0,
            load_total: 0,
            save: SaveState::Idle,
            save_at: None,
            save_done: 0,
            save_total: 0,
            save_done_at: None,
            tail_offset: u64::MAX,
            tail_pending: false,
            tail_last: 0.0,
            doc: None,
            tail_carry: Vec::new(),
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
            save: SaveState::Idle,
            save_at: None,
            save_done: 0,
            save_total: 0,
            save_done_at: None,
            tail_offset: u64::MAX,
            tail_pending: false,
            tail_last: 0.0,
            doc: None,
            tail_carry: Vec::new(),
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
                app.image.tabs.push(ImageTab {
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
            app.fwd.show = true;
        }

        // 自检：进程详情小窗
        if std::env::var("ISHELL_DEMO_PROC").is_ok() {
            app.popups.proc = Some(ProcPopup {
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
            pending_tail: Vec::new(),
            pending_pdf_info: Vec::new(),
            pending_pdf_page: Vec::new(),
            pending_pdf_search: Vec::new(),
            pending_doc: Vec::new(),
            pending_conflict: Vec::new(),
            pending_save_failed: Vec::new(),
            pending_warn: Vec::new(),
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
            monitor_ok: None,
        });
        self.active = Some(self.sessions.len() - 1);
        self.tabbar.scroll_to_active = true; // 新建标签后滚动到可视区
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
        s.monitor_ok = None;
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
        {
            let mut es = lock_mutex(&self.editor_state);
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
        self.tabbar.scroll_to_active = true; // 切换后滚动到可视区
        if let Some(s) = self.sessions.get_mut(next) {
            s.terminal.request_focus();
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
            self.fwd.confirm_del = None;
            self.fwd.editing = None;
            self.fwd.error = None;
            self.popups.proc = None;
        }

        self.process_frame_events(ui);

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
                        let mon = s.monitor_ok;
                        sidebar::show(ui, s.sysinfo.as_ref(), &s.net_hist, &mut s.selected_nic, &mut s.proc_sort_mem, &mut proc_click, &mut gpu_click, mon);
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
                self.popups.proc = Some(pp);
                self.popups.proc_just_opened = true;
            }
        }
        if let Some(pos) = gpu_click {
            self.popups.gpu = Some(pos);
            self.popups.gpu_just_opened = true;
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
        // 直传目标主机密钥未记录时的 TOFU 确认
        self.direct_hostkey_dialog(&ctx);
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

#[cfg(test)]
mod save_fsm_tests {
    use super::*;

    fn tab() -> EditorTab {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        EditorTab {
            editor: crate::ui::editor::Editor::new("/t.txt".into(), "hi\n".into()),
            server: String::new(),
            uid: 1,
            cmd_tx: tx,
            text_id: egui::Id::new(0u8),
            load_id: None,
            load_done: 0,
            load_total: 0,
            save: SaveState::Idle,
            save_at: None,
            save_done: 0,
            save_total: 0,
            tail_offset: u64::MAX,
            tail_pending: false,
            tail_last: 0.0,
            doc: None,
            tail_carry: Vec::new(),
            save_done_at: None,
        }
    }

    #[test]
    fn save_state_transitions() {
        let mut t = tab();
        // 初始：空闲
        assert!(!t.is_saving() && !t.is_conflict() && !t.wants_close());
        // 进入保存中（无关闭意图）
        t.begin_save(false);
        assert!(t.is_saving() && !t.is_conflict() && !t.wants_close());
        // 保存中追加「完成后关闭」意图
        t.request_close_on_saved();
        assert!(t.wants_close());
        // 非保存中调用 request_close_on_saved 无效（不产生非法状态）
        t.save = SaveState::Idle;
        t.request_close_on_saved();
        assert!(!t.wants_close() && !t.is_saving());
        // 冲突：与 saving 互斥，且不携带关闭意图
        t.begin_save(true);
        t.save = SaveState::Conflict;
        assert!(t.is_conflict() && !t.is_saving() && !t.wants_close());
    }
}
