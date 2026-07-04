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
    /// 排空后台事件，更新本地状态。
    /// 排空 worker 事件，带每帧预算：终端数据 ≤2MB、事件 ≤512 条/帧。
    /// 超出预算的事件留在队列、下一帧继续（返回 true 表示还有积压需要重绘）——
    /// 远端持续大量输出时 UI 仍按帧渲染，不会被「全量排空循环」饿死。
    fn drain_events(&mut self) -> bool {
        let mut term_budget: usize = 2 * 1024 * 1024;
        let mut evt_budget: usize = 512;
        loop {
            if evt_budget == 0 || term_budget == 0 {
                return true; // 预算耗尽且可能仍有积压
            }
            let Ok(ev) = self.evt_rx.try_recv() else {
                return false;
            };
            evt_budget -= 1;
            match ev {
                WorkerEvent::Status(s) => {
                    // ⚠ 前缀的警告（如编码丢字）转交 App 层弹顶部 toast，避免只写状态栏被后续消息滚走
                    if s.starts_with('⚠') {
                        self.pending_warn.push(s.clone());
                    }
                    self.status = s;
                }
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
                    term_budget = term_budget.saturating_sub(bytes.len());
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
                WorkerEvent::FileTail { path, data, offset, truncated } => {
                    self.pending_tail.push((path, data, offset, truncated));
                }
                WorkerEvent::PdfInfo { id, path: _, pages } => {
                    self.pending_pdf_info.push((id, pages));
                }
                WorkerEvent::PdfPage { path, page, data } => {
                    self.pending_pdf_page.push((path, page, data));
                }
                WorkerEvent::PdfSearch { path, query: _, hits, message } => {
                    self.pending_pdf_search.push((path, hits, message));
                }
                WorkerEvent::DocOpened { id, path: _, data } => {
                    self.pending_doc.push((id, data));
                }
                WorkerEvent::FileTooLarge { id, path, size } => {
                    self.pending_too_large.push((id, path, size));
                }
                WorkerEvent::FileSaveFailed { path, message } => {
                    self.pending_save_failed.push((path, message));
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
    /// 看图工具状态（标签、激活项、聚焦请求、拖动重排）
    image: ImageView,
    /// docx 后台解析结果通道：(占位标签 id, 解析结果)
    doc_parse_tx: std::sync::mpsc::Sender<(u64, Result<(crate::ui::docx::Doc, std::collections::HashMap<String, egui::TextureHandle>), String>)>,
    doc_parse_rx: std::sync::mpsc::Receiver<(u64, Result<(crate::ui::docx::Doc, std::collections::HashMap<String, egui::TextureHandle>), String>)>,
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
    /// 文件传输/复制粘贴/跨服务器中转与直传的聚合状态（从 App 抽出的内聚字段组）
    xfer: Transfers,
    /// 命令片段库（窗口开关 + 数据 + 编辑表单缓冲）
    snip: Snippets,
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
            image: ImageView::default(),
            doc_parse_tx,
            doc_parse_rx,
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
            xfer: Transfers::default(),
            snip: Snippets { list: crate::store::load_snippets(), run: true, ..Default::default() },
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
                    editor: crate::ui::editor::Editor::new("/var/log/huge.log".into(), big),
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
            FileAction::OpenPdf { path } => {
                // 与文本打开同构：先建占位标签（珊瑚线进度条），PdfInfo 就位后填充 PDF 视图
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.pending_placeholder.push((id, path.clone()));
                let _ = s.cmd_tx.send(UiCommand::PdfInfo { id, path });
            }
            FileAction::OpenDocx { path } => {
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.pending_placeholder.push((id, path.clone()));
                let _ = s.cmd_tx.send(UiCommand::ReadDoc { id, path });
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
        let mut save_failed: Vec<(u64, String, String)> = Vec::new(); // uid, path, message
        let mut warns: Vec<String> = Vec::new(); // 需弹 toast 的警告
        let mut too_large: Vec<(u64, u64, String, u64)> = Vec::new(); // uid, id, path, size
        let mut tails: Vec<(u64, String, Vec<u8>, u64, bool)> = Vec::new(); // uid, path, data, offset, truncated
        let mut pdf_infos: Vec<(u64, u32)> = Vec::new(); // 占位 id, 页数
        let mut pdf_pages: Vec<(u64, String, u32, Vec<u8>)> = Vec::new(); // uid, path, page, png
        let mut pdf_searches: Vec<(u64, String, Vec<(u32, String)>, Option<String>)> = Vec::new();
        let mut new_docs: Vec<(u64, Vec<u8>)> = Vec::new(); // 占位 id, docx 字节
        let mut evt_backlog = false;
        for s in &mut self.sessions {
            // 事件积压未排空（每帧预算保护渲染）时安排下一帧继续消化
            evt_backlog |= s.drain_events();
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
            for (path, data, offset, truncated) in s.pending_tail.drain(..) {
                tails.push((s.uid, path, data, offset, truncated));
            }
            for path in s.pending_conflict.drain(..) {
                conflicts.push((s.uid, path));
            }
            for w in s.pending_warn.drain(..) {
                warns.push(w);
            }
            for (path, msg) in s.pending_save_failed.drain(..) {
                save_failed.push((s.uid, path, msg));
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
            for x in s.pending_pdf_info.drain(..) {
                pdf_infos.push(x);
            }
            for (path, page, data) in s.pending_pdf_page.drain(..) {
                pdf_pages.push((s.uid, path, page, data));
            }
            for (path, hits, message) in s.pending_pdf_search.drain(..) {
                pdf_searches.push((s.uid, path, hits, message));
            }
            for x in s.pending_doc.drain(..) {
                new_docs.push(x);
            }
        }
        if evt_backlog {
            self.ctx.request_repaint();
        }
        // 警告（如编码丢字）弹顶部 toast
        if let Some(w) = warns.into_iter().next_back() {
            self.toast = Some((w, self.ctx.input(|i| i.time)));
        }
        // 打开时发现文件实际超限：移除占位标签（复用 load_fail 移除逻辑），并在对应会话的文件面板
        // 弹「打开大文件」确认，确认后走 force=true 重新打开（列表里的旧大小已过时，双击前无法预判）。
        for (uid, id, path, size) in too_large {
            load_fail.push(id);
            if let Some(s) = self.sessions.iter_mut().find(|s| s.uid == uid) {
                s.files.dialog = Some(file_panel::Dialog::ConfirmOpenLarge { path, size });
            }
        }
        // 跟随模式（tail -f）：应用增量 + 定时轮询下一次读取。
        // 注意：跟随期间不更新 tab 的 mtime——外部对文件「中间」的修改无法检测，
        // 保留旧 mtime 让保存必走冲突确认流程，避免静默覆盖他人修改。
        {
            let now = self.ctx.input(|i| i.time);
            let mut edst = self.editor_state.lock().unwrap();
            let mut any_follow = false;
            for (uid, path, data, offset, truncated) in tails {
                if let Some(t) = edst.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.tail_pending = false;
                    t.tail_offset = offset;
                    if !t.editor.follow {
                        continue; // 已关闭跟随：丢弃迟到的数据
                    }
                    if truncated {
                        t.editor.append_tail(&crate::i18n::tr("\n--- 文件被截断/轮转，以下为新内容 ---\n", "\n--- file truncated/rotated, new content follows ---\n"));
                    }
                    if !data.is_empty() {
                        // 跨块解码：与上一块留下的不完整尾字节拼接；UTF-8 时把本块末尾
                        // 不完整的多字节序列留到下一块（跨块字符不再变 �）
                        let mut bytes = std::mem::take(&mut t.tail_carry);
                        bytes.extend_from_slice(&data);
                        let enc = encoding_rs::Encoding::for_label(t.editor.encoding().as_bytes()).unwrap_or(encoding_rs::UTF_8);
                        if enc == encoding_rs::UTF_8 {
                            let valid = match std::str::from_utf8(&bytes) {
                                Ok(_) => bytes.len(),
                                Err(e) => e.valid_up_to(),
                            };
                            // 仅当截断发生在末尾 ≤3 字节内才视为「不完整序列」暂存；
                            // 中间的真实坏字节照常替换输出，避免 carry 死循环
                            if bytes.len() - valid <= 3 && valid < bytes.len() {
                                t.tail_carry = bytes.split_off(valid);
                            }
                        }
                        if !bytes.is_empty() {
                            let (cow, _, _) = enc.decode(&bytes);
                            let txt = cow.replace("\r\n", "\n");
                            t.editor.append_tail(&txt);
                        }
                    }
                }
            }
            for t in edst.tabs.iter_mut() {
                if t.editor.follow {
                    any_follow = true;
                    if !t.tail_pending && t.tail_offset != u64::MAX && now - t.tail_last > 1.0 {
                        t.tail_pending = true;
                        t.tail_last = now;
                        let _ = t.cmd_tx.send(UiCommand::TailFile { path: t.editor.path.clone(), offset: t.tail_offset });
                    }
                }
            }
            if any_follow {
                // 维持轮询节奏 + 唤醒编辑器窗口显示新内容
                self.ctx.request_repaint_after(std::time::Duration::from_millis(500));
                self.ctx.request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
            }
        }
        // 跨服务器中转任务推进（下载完→上传，上传完→剪切则删源）
        self.process_relays();
        // 跨服务器直传任务推进（完成则删源/刷新；失败则弹「转中转」）
        self.process_direct_jobs();
        for (path, data, server, uid) in new_images {
            self.image.focus = true; // 打开/切换后聚焦看图窗口
            // 同一会话同一图片已打开则切到该标签（身份用 uid，不用可能重名的 title）
            if let Some(i) = self.image.tabs.iter().position(|t| t.uid == uid && t.path == path) {
                self.image.active = i;
                continue;
            }
            match image::load_from_memory(&data) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let name = format!("img:{server}:{path}");
                    let tex = ui.ctx().load_texture(name, color, egui::TextureOptions::LINEAR);
                    self.image.tabs.push(ImageTab {
                        server,
                        uid,
                        path,
                        tex,
                        data,
                        size: egui::vec2(size[0] as f32, size[1] as f32),
                        zoom: 0.0,
                        offset: egui::Vec2::ZERO,
                    });
                    self.image.active = self.image.tabs.len() - 1;
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
        // PDF / Word 文档标签完整复用该框架（占位/进度/失败路径相同，就位时填充 doc 内容）。
        // docx 后台解析结果先收集（mpsc 无 peek；必须与其它事件一起纳入触发条件，
        // 否则「解析完成」那帧若无其它编辑器事件，下方块不执行 → 永远停在「渲染中」）
        let parsed: Vec<(u64, Result<(crate::ui::docx::Doc, std::collections::HashMap<String, egui::TextureHandle>), String>)> =
            self.doc_parse_rx.try_iter().collect();
        let opened_editor = !new_placeholders.is_empty()
            || !filled.is_empty()
            || !load_progress.is_empty()
            || !load_fail.is_empty()
            || !pdf_infos.is_empty()
            || !pdf_pages.is_empty()
            || !new_docs.is_empty()
            || !pdf_searches.is_empty()
            || !parsed.is_empty();
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
                    ed.tabs.push(EditorTab { editor, server, uid, cmd_tx: tx, text_id: tid, load_id: Some(id), load_done: 0, load_total: 0, save: SaveState::Idle, save_at: None, save_done: 0, save_total: 0, save_done_at: None, tail_offset: u64::MAX, tail_pending: false, tail_last: 0.0, doc: None, tail_carry: Vec::new() });
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
            // 3) 内容就位：占位标签变为可编辑、填入内容；恢复上次光标位置
            for (id, path, content, encoding, eol, mtime) in filled {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    let key = format!("{}|{}", t.server, path);
                    let mut editor = crate::ui::editor::Editor::new(path, content);
                    editor.set_meta(encoding, eol, mtime);
                    if let Some(line) = crate::store::load_cursor_line(&key) {
                        editor.restore_line(line);
                    }
                    t.editor = editor;
                    t.load_id = None;
                }
            }
            // 3.5) 文档就位：占位标签变为 PDF / Word 查看器
            for (id, pages) in pdf_infos {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    t.doc = Some(DocKind::Pdf {
                        pages,
                        cur: 1,
                        zoom: 0.0,
                        cache: Vec::new(),
                        pending: std::collections::HashSet::new(),
                        flip_at: 0.0,
                        search: String::new(),
                        search_open: false,
                        hits: Vec::new(),
                        hit_sel: 0,
                        searching: false,
                        search_msg: None,
                    });
                    t.load_id = None;
                }
            }
            // docx 下载完成 → 后台线程解析 + 解码纹理（ctx.load_texture 线程安全），
            // UI 不冻结；占位文案切换为「渲染中 …」。结果经 doc_parse 通道回来装配。
            for (id, data) in new_docs {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    t.editor.loading_note = Some(crate::i18n::tr("渲染中 …", "Rendering …").into());
                    // 进度条置满（下载已完成）
                    t.load_done = t.load_total.max(1);
                    t.load_total = t.load_total.max(1);
                    let ctx2 = ui.ctx().clone();
                    let tx = self.doc_parse_tx.clone();
                    let uid = t.uid;
                    let tpath = t.editor.path.clone();
                    std::thread::spawn(move || {
                        let res = match crate::ui::docx::parse(&data) {
                            Ok(mut doc) => {
                                let mut images = std::collections::HashMap::new();
                                // 图片纹理上限 100 张：图海文档防内存/显存失控
                                for (name, bytes) in doc.media.iter().take(100) {
                                    if let Ok(mut img) = image::load_from_memory(bytes) {
                                        // 大图降采样到 ≤1600px：相机原图直接建纹理动辄几十 MB，
                                        // 阅读视图用不到原始分辨率（这是 docx 内存高的大头）
                                        if img.width() > 1600 || img.height() > 1600 {
                                            img = img.thumbnail(1600, 1600);
                                        }
                                        let rgba = img.to_rgba8();
                                        let size = [rgba.width() as usize, rgba.height() as usize];
                                        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                                        images.insert(name.clone(), ctx2.load_texture(format!("docx:{uid}:{tpath}:{name}"), color, egui::TextureOptions::LINEAR));
                                    }
                                }
                                doc.media = Vec::new(); // 原始字节释放（内存大头）
                                Ok((doc, images))
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        let _ = tx.send((id, res));
                        ctx2.request_repaint();
                        ctx2.request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
                    });
                }
            }
            // 后台解析完成 → 装配文档标签
            for (id, res) in parsed {
                match res {
                    Ok((doc, images)) => {
                        if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                            let n = doc.blocks.len();
                            t.doc = Some(DocKind::Docx {
                                doc,
                                images,
                                heights: vec![0.0; n],
                                search: String::new(),
                                search_open: false,
                                hits: Vec::new(),
                                hit_sel: 0,
                                scroll_to: None,
                            });
                            t.load_id = None;
                            t.editor.loading_note = None;
                        }
                    }
                    Err(e) => {
                        self.toast = Some((match crate::i18n::current() { crate::i18n::Lang::Zh => format!("文档解析失败：{e}"), crate::i18n::Lang::En => format!("Doc parse failed: {e}") }, ui.input(|i| i.time)));
                        load_fail.push(id);
                    }
                }
            }
            // PDF 查找结果 → 命中列表（跳到首个命中页）
            for (uid, path, hits_in, message) in pdf_searches {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    if let Some(DocKind::Pdf { hits, hit_sel, searching, search_msg, cur, pages, .. }) = &mut t.doc {
                        *searching = false;
                        *search_msg = message;
                        *hits = hits_in;
                        *hit_sel = 0;
                        if let Some((p, _)) = hits.first() {
                            *cur = (*p).clamp(1, *pages);
                        }
                    }
                }
            }
            // PDF 页渲染结果 → 页缓存
            for (uid, path, page, data) in pdf_pages {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    if let Some(DocKind::Pdf { cache, pending, .. }) = &mut t.doc {
                        pending.remove(&page);
                        if data.is_empty() {
                            continue;
                        }
                        if let Ok(img) = image::load_from_memory(&data) {
                            let rgba = img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                            let tex = ui.ctx().load_texture(format!("pdf:{uid}:{path}:{page}"), color, egui::TextureOptions::LINEAR);
                            cache.retain(|(p, _, _)| *p != page);
                            cache.push((page, tex, egui::vec2(size[0] as f32, size[1] as f32)));
                            // 小 LRU：只留最近 6 页（控制内存）
                            while cache.len() > 6 {
                                let _ = cache.remove(0);
                            }
                        }
                    }
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
        if !saved.is_empty() || !conflicts.is_empty() || !save_progress.is_empty() || !save_failed.is_empty() {
            let mut ed = self.editor_state.lock().unwrap();
            for (uid, path, done, total) in save_progress {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.save_done = done;
                    t.save_total = total;
                }
            }
            let mut close_after_save: Vec<(u64, String)> = Vec::new();
            for (uid, path, mtime) in saved {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.editor.set_mtime(mtime); // 回填服务器新 mtime，避免下次保存把「自己刚写入」误判为外部改动
                    // 取出本次保存发出时的签名与关闭意图（Saving 状态里）；非 Saving 则忽略这条确认。
                    let (sent_rev, close_after) = match &t.save {
                        SaveState::Saving { rev, close_after } => (rev.clone(), *close_after),
                        _ => continue,
                    };
                    // 仅当修订签名（正文+编码+行尾）与发出保存时完全一致才算已保存
                    //（保存期间用户又编辑、或切了编码/行尾 → 远端并非该状态，不能清 dirty，也不能关闭）
                    if t.editor.save_rev() == sent_rev {
                        t.save = SaveState::Idle;
                        t.editor.mark_saved();
                        if close_after {
                            close_after_save.push((uid, t.editor.path.clone()));
                        }
                    } else if close_after {
                        // 保存期间内容又变了但用户要「保存并关闭」：用最新内容再存一次，
                        // 存完（届时签名一致）再关闭；否则「保存并关闭」会静默不生效。
                        let _ = t.cmd_tx.send(UiCommand::WriteFile {
                            path: t.editor.path.clone(),
                            content: t.editor.content.clone(),
                            encoding: t.editor.encoding().to_string(),
                            eol: t.editor.eol(),
                            expect_mtime: t.editor.mtime(),
                            force: false,
                        });
                        t.begin_save(true); // 重新进入保存中，保持关闭意图
                    } else {
                        // 保存成功但内容已变、无关闭意图：解锁，保留 dirty 交用户再存
                        t.save = SaveState::Idle;
                    }
                }
            }
            for (uid, path) in conflicts {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    // 冲突：进入 Conflict（未写入，保留 dirty）；「保存并关闭」意图自然丢弃，交用户处理
                    t.save = SaveState::Conflict;
                    t.save_at = None; // 冲突未写入：中止「已保存」动画
                    t.save_done_at = None;
                }
            }
            for (uid, path, message) in save_failed {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.editor.path == path) {
                    t.save = SaveState::Idle; // 失败：解锁重试；dirty 未被清，标签仍显示未保存；关闭意图丢弃
                    t.save_at = None; // 中止保存动画
                    t.save_done_at = None;
                }
                self.toast = Some((match crate::i18n::current() { crate::i18n::Lang::Zh => format!("保存失败：{message}"), crate::i18n::Lang::En => format!("Save failed: {message}") }, ui.input(|i| i.time)));
            }
            // 「保存并关闭」：确认成功后移除标签
            for (uid, path) in close_after_save {
                if let Some(i) = ed.tabs.iter().position(|t| t.uid == uid && t.editor.path == path) {
                    let closed = ed.tabs.remove(i);
                    if closed.doc.is_none() {
                        crate::store::save_cursor_line(&format!("{}|{}", closed.server, closed.editor.path), closed.editor.caret_line());
                    }
                    if ed.active >= ed.tabs.len() && !ed.tabs.is_empty() {
                        ed.active = ed.tabs.len() - 1;
                    }
                    ed.trim_request = true;
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
                            self.snip.show = !self.snip.show;
                            if self.snip.show {
                                self.snip.just_opened = true;
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
        let has_clip = self.xfer.file_clip.is_some();
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
