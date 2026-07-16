use tokio::sync::mpsc::UnboundedSender;

use crate::proto::UiCommand;

/// 传输列表的状态筛选。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum XferFilter {
    All,
    Active,
    Done,
    Failed,
}

/// UI 侧的一条传输记录。
pub(super) struct Transfer {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) dir: crate::proto::TransferDir,
    /// 重发规格（用于断线重连续传 / 手动重试）；演示记录为 None
    pub(super) spec: Option<super::XferSpec>,
    /// 因断线被中断、等待重连后自动续传
    pub(super) paused: bool,
    pub(super) done: u64,
    pub(super) total: u64,
    /// None=进行中，Some(true/false)=完成/失败
    pub(super) ok: Option<bool>,
    /// 下载到的本地路径（用于「打开所在文件夹」）
    pub(super) local: Option<String>,
    /// 完成/失败原因（点击状态可展开查看）
    pub(super) message: String,
    /// 是否展开显示失败原因
    pub(super) show_err: bool,
    /// 实时速度（字节/秒，指数平滑）
    pub(super) speed: f64,
    /// 上次采样的已传字节数与时刻（用于计算速度）
    pub(super) last_done: u64,
    pub(super) last_t: Option<std::time::Instant>,
    /// 进行中的阶段提示（如「打包中…」「解包中…」「等待源端…」「直传中…」）；非空时在详情行替代字节读数
    pub(super) note: String,
    /// 排队等待态（如跨服务器中转的目标端，正等源端下载完成）：显示「等待」而非进度数字
    pub(super) queued: bool,
    /// 模式徽标（如「直传」）：显示在文件大小之后，标注传输方式；空串不显示
    pub(super) tag: String,
}

impl Transfer {
    /// 新建一条「进行中」的传输记录（note/queued 默认空，speed 等计量字段归零）。
    pub(super) fn new(
        id: u64,
        name: String,
        dir: crate::proto::TransferDir,
        total: u64,
        local: Option<String>,
        spec: Option<super::XferSpec>,
    ) -> Self {
        Transfer {
            id,
            name,
            dir,
            spec,
            paused: false,
            done: 0,
            total,
            ok: None,
            local,
            message: String::new(),
            show_err: false,
            speed: 0.0,
            last_done: 0,
            last_t: None,
            note: String::new(),
            queued: false,
            tag: String::new(),
        }
    }
}

/// 文件传输子系统的聚合状态（剪贴板 / 待确认粘贴 / 跨服务器中转 / 直传）。
/// 从 App 抽出的内聚字段组，配套 transfers.rs 里的方法。
#[derive(Default)]
pub(super) struct Transfers {
    /// 文件剪贴板（跨 tab 共享）：复制/剪切的源项
    pub(super) file_clip: Option<FileClip>,
    /// 待确认的粘贴（剪切 或 跨服务器：执行前二次确认）
    pub(super) pending_paste: Option<PendingPaste>,
    /// 跨服务器中转任务（下载→上传→可选删源）
    pub(super) relays: Vec<Relay>,
    /// 中转临时目录去重计数
    pub(super) relay_seq: u64,
    /// 粘贴确认弹框里「直传/中转」互斥选择的当前值（false=中转，默认更安全）
    pub(super) confirm_direct: bool,
    /// 进行中的直传任务追踪（成功删源/刷新；失败弹回退）
    pub(super) direct_jobs: Vec<DirectJob>,
    /// 直传失败、待确认「转中转」的计划 + 原因（队列：多个失败依次弹，避免同帧互相覆盖）
    pub(super) pending_direct_fallback: Vec<DirectFallback>,
    /// 直传目标不在本机 known_hosts：待用户确认首次 TOFU（在源机 accept-new）后再执行
    pub(super) pending_direct_hostkey: Option<PendingPaste>,
}

/// 命令片段库状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
pub(super) struct Snippets {
    /// 片段浮窗是否显示
    pub(super) show: bool,
    /// 浮窗刚打开（本帧跳过"点击外部关闭"判定）
    pub(super) just_opened: bool,
    /// 片段数据
    pub(super) list: Vec<crate::store::Snippet>,
    /// 正在编辑的片段索引（None = 新建）
    pub(super) editing: Option<usize>,
    /// 编辑表单缓冲
    pub(super) name: String,
    pub(super) cmd: String,
    pub(super) run: bool,
}

/// App 级文件剪贴板（跨 tab 共享）。
pub(super) struct FileClip {
    /// (绝对路径, 是否目录)
    pub(super) items: Vec<(String, bool)>,
    /// true=剪切（粘贴时移动），false=复制
    pub(super) is_cut: bool,
    pub(super) src_uid: u64,
    pub(super) src_host: String,
    pub(super) src_port: u16,
    pub(super) src_label: String,
}

/// 待确认的粘贴计划（剪切，或跨服务器复制/剪切，执行前二次确认）。
#[derive(Clone)]
pub(super) struct PendingPaste {
    pub(super) items: Vec<(String, bool)>,
    pub(super) is_cut: bool,
    /// 源与目标是否不同服务器（需经本地中转或直传）
    pub(super) cross: bool,
    pub(super) src_uid: u64,
    pub(super) dest_uid: u64,
    /// 源目录（被复制/剪切项的所在目录，用于确认弹框展示）
    pub(super) src_dir: String,
    pub(super) dest_dir: String,
    pub(super) src_label: String,
    pub(super) dest_label: String,
    /// 跨服务器时是否走「直传」（true=源主机直推目标；false=经本地中转）。一级确认后才据传输方式设定。
    pub(super) direct: bool,
}

/// 直传任务追踪（App 侧）：源会话里一条直传传输（id）的归属与善后信息。
/// 成功 → 剪切则删源 + 刷新目标目录；失败 → 弹「转中转」提醒。
pub(super) struct DirectJob {
    /// 源会话里的传输 id（真实数据通路在此）
    pub(super) id: u64,
    /// 目标会话里的「镜像」进度行 id（直传数据不经 B，App 据源端进度同步显示）
    pub(super) mir_id: u64,
    pub(super) src_uid: u64,
    pub(super) dest_uid: u64,
    /// 源目录（回退中转确认弹框展示用）
    pub(super) src_dir: String,
    pub(super) dest_dir: String,
    pub(super) is_cut: bool,
    /// 原始条目（失败回退中转、或剪切删源时用）
    pub(super) items: Vec<(String, bool)>,
    pub(super) src_label: String,
    pub(super) dest_label: String,
    /// 用户主动取消（经取消按钮标记）：失败收尾时据此跳过「转中转」提醒，避免误弹
    pub(super) cancelled: bool,
}

/// 直传失败后，等待用户确认「转中转」的计划 + 失败原因。
pub(super) struct DirectFallback {
    pub(super) plan: PendingPaste,
    pub(super) reason: String,
}

/// 跨服务器中转任务：源会话下载到本地临时 → 目标会话上传 →（剪切则删源）。
pub(super) struct Relay {
    pub(super) src_path: String,
    pub(super) is_dir: bool,
    pub(super) src_uid: u64,
    pub(super) dest_uid: u64,
    pub(super) dest_dir: String,
    pub(super) is_cut: bool,
    pub(super) tmp: std::path::PathBuf,
    pub(super) phase: RelayPhase,
    /// 目标会话里预占的上传传输 id（粘贴时即登记「等待」占位行，源端下载完才真正发起上传）
    pub(super) up_id: u64,
}

/// 中转任务阶段：保存对应会话里的传输 id，用于轮询完成状态。
pub(super) enum RelayPhase {
    Down(u64),
    Up(u64),
}

/// 键盘交互认证的一组待回答提示（每项 (提示文本, 是否回显) + 用户输入缓冲）。
pub(super) struct KbdPrompt {
    pub(super) name: String,
    pub(super) instructions: String,
    /// (提示文本, 是否回显)
    pub(super) prompts: Vec<(String, bool)>,
    /// 与 prompts 等长的回答缓冲
    pub(super) answers: Vec<String>,
}

/// 进程详情小窗状态。
pub(super) struct ProcPopup {
    pub(super) pid: u32,
    pub(super) name: String,
    pub(super) cpu: f32,
    pub(super) mem: f32,
    pub(super) pos: egui::Pos2,
    pub(super) cmd: String,
    pub(super) cwd: String,
    pub(super) exe: String,
    /// 最近一次复制的时刻（ctx 时间，秒），用于短暂显示「已复制」
    pub(super) copied_t: Option<f64>,
    /// 是否处于「强制结束」二次确认态：kill 按钮先置此标志，确认后才真正下发 KillProc
    pub(super) confirm_kill: bool,
    /// 打开弹窗时所属会话的 uid——kill 据此定位会话，避免切会话后误 kill 别的主机
    pub(super) uid: u64,
}

/// UI 侧的一条端口转发记录。
pub(super) struct ForwardEntry {
    pub(super) id: u64,
    pub(super) label: String,
    pub(super) status: String,
    pub(super) ok: bool,
    /// 结构化参数：用于「编辑」时把该条回填到表单，以及本地端口占用检测。
    pub(super) bind_host: String,
    pub(super) bind_port: u16,
    pub(super) kind: crate::proto::ForwardKind,
}

/// 端口转发管理窗口的 UI 状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
pub(super) struct ForwardUi {
    /// 管理窗口是否显示
    pub(super) show: bool,
    /// 浮窗刚打开（本帧跳过"点击外部关闭"判定）
    pub(super) just_opened: bool,
    /// 待删除确认的转发 id（行内二次确认）
    pub(super) confirm_del: Option<u64>,
    /// 新增/编辑转发表单
    pub(super) form: ForwardForm,
    /// 正在编辑的转发 id（Some=编辑模式）
    pub(super) editing: Option<u64>,
    /// 表单内联校验错误（端口占用/参数无效等）
    pub(super) error: Option<String>,
    /// 非回环绑定待二次确认的转发规格（确认后才真正添加）
    pub(super) pending_open_bind: Option<crate::proto::ForwardSpec>,
}

/// "新增转发"表单状态。
pub(super) struct ForwardForm {
    /// 0 = 本地转发，1 = 动态 SOCKS5
    pub(super) kind: usize,
    pub(super) bind: String,
    pub(super) local_port: String,
    pub(super) target_host: String,
    pub(super) target_port: String,
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
pub(super) enum DocKind {
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
pub(super) enum SaveState {
    /// 空闲：无在途保存、无未决冲突。
    Idle,
    /// 已发出 WriteFile、等待结果。rev=发出时的修订签名 (vver, 编码, 行尾)，
    /// 收到 FileSaved 且签名一致才算已保存；close_after=完成后是否关闭标签。
    Saving {
        rev: (u64, String, crate::proto::Eol),
        close_after: bool,
    },
    /// 检测到外部改动（未写入），显示冲突横幅，等用户选择覆盖/取消。
    Conflict,
}

pub(super) struct EditorTab {
    pub(super) editor: crate::ui::editor::Editor,
    /// 所属会话标题（仅用于标签显示）
    pub(super) server: String,
    /// 所属会话稳定唯一 id（身份匹配用：保存/冲突/去重，避免同名会话串台）
    pub(super) uid: u64,
    pub(super) cmd_tx: UnboundedSender<UiCommand>,
    /// 该编辑器固定的 TextEdit Id（关闭时据此清理 egui 状态/撤销历史）
    pub(super) text_id: egui::Id,
    /// 稳定唯一的标签身份（创建时取当初那次 ReadFile 请求的 id，此后永久不变，
    /// 不像 load_id 那样加载完就清空）：保存请求（WriteFile）用它做 id，收到的
    /// FileSaved/FileSaveFailed/FileSaveConflict 按这个 id 匹配回这个标签，
    /// 不再仅靠 path 字符串——避免同一路径同时有编辑器手动保存和 AI write_file
    /// 两路请求时响应张冠李戴。
    pub(super) tid: u64,
    /// 下载中关联的 ReadFile id（Some=加载中占位，None=已就绪）；以及下载进度
    pub(super) load_id: Option<u64>,
    pub(super) load_done: u64,
    pub(super) load_total: u64,
    /// 保存流程状态机（见 SaveState）。
    pub(super) save: SaveState,
    /// 保存动画起始时刻（ctx 时间，秒）：驱动标签底部珊瑚线「绿扫→珊瑚扫」表示已保存；None=无动画
    pub(super) save_at: Option<f64>,
    /// 保存写入进度（done/total 字节）：驱动绿扫跟随实际上传速度
    pub(super) save_done: u64,
    pub(super) save_total: u64,
    /// 跟随模式（tail -f）状态：下次读取的字节偏移（u64::MAX=待初始化）、
    /// 是否有在途请求、上次轮询时刻。注意：跟随期间**不更新 mtime**——外部对文件
    /// 中间的修改无法检测，保留旧 mtime 让保存走冲突确认，避免静默覆盖他人修改。
    pub(super) tail_offset: u64,
    pub(super) tail_pending: bool,
    pub(super) tail_last: f64,
    /// Some = 文档标签（PDF/Word 查看器）；None = 常规文本编辑器
    pub(super) doc: Option<DocKind>,
    /// 跟随模式跨块解码缓冲：上一块末尾不完整的多字节字符原始字节，与下一块拼接
    ///（否则 UTF-8/GBK 字符跨 512KB 分块边界会变替换字符并永久丢失原始字节）
    pub(super) tail_carry: Vec<u8>,
    /// 绿扫完成、进入「珊瑚扫回」阶段的起始时刻（ctx 时间）；None=仍在绿扫阶段
    pub(super) save_done_at: Option<f64>,
    /// 本次在途保存的唯一操作 id（每次 begin_save 递增分配，作为 WriteFile.id 下发；
    /// worker 回报的 FileSaved/FileSaveFailed/FileSaveConflict 按它匹配回本标签）。
    /// 0 = 当前无在途保存。超时判定后置 0：迟到的旧事件因匹配不到任何标签而被安全丢弃，
    /// 不会误更新一个可能已关闭 / 已重试保存的标签（重试会分配一个新的 save_op）。
    pub(super) save_op: u64,
    /// 在途保存的截止时刻（stall 超时：每收到一次写入进度就顺延，见 process_editor_save_events）。
    /// None = 无在途保存。到点仍未收到结果 → 判定保存超时，转空闲失败态并收尾动画。
    pub(super) save_deadline: Option<std::time::Instant>,
}

/// 保存超时阈值（stall 超时：自最后一次写入进度起算）。略高于底层 SFTP 单请求 20s 超时
/// （见 auth.rs `sftp.set_timeout(20)`），让「SFTP 层超时 → FileSaveFailed」这条能给出精确
/// 失败原因的路径在常态下优先生效；此处仅兜底 TCP 静默失联（Windows 休眠唤醒 / Wi-Fi 切换 /
/// VPN 抖动）导致 spawn 出去的 handle_fs_op 长期不返回、标签永久卡在「保存中」的场景。
pub(super) const SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 全局单调递增的保存操作 id 分配器（见 EditorTab::save_op）。每次 begin_save 取一个新值，
/// 保证任一在途保存的 id 全局唯一——迟到事件据此被可靠地识别、丢弃。
static SAVE_OP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl EditorTab {
    /// 保存进行中（已发 WriteFile、未收到结果）——期间屏蔽再次保存。
    pub(super) fn is_saving(&self) -> bool {
        matches!(self.save, SaveState::Saving { .. })
    }
    /// 存在未决的外部改动冲突（显示横幅）。
    pub(super) fn is_conflict(&self) -> bool {
        matches!(self.save, SaveState::Conflict)
    }
    /// 用户是否要求「保存成功后关闭标签」（仅在保存中有意义）。FSM 不变式，测试覆盖。
    #[allow(dead_code)]
    pub(super) fn wants_close(&self) -> bool {
        matches!(
            self.save,
            SaveState::Saving {
                close_after: true,
                ..
            }
        )
    }
    /// 进入「保存中」：记录发出时的修订签名与是否保存后关闭，并分配本次保存的唯一
    /// `save_op`（供随后的 WriteFile.id 使用）+ 设定 stall 超时截止时刻。
    /// 注意：调用方必须在 begin_save **之后**再发送 WriteFile，且 `id` 取本方法赋好的 `self.save_op`。
    pub(super) fn begin_save(&mut self, close_after: bool) {
        self.save = SaveState::Saving {
            rev: self.editor.save_rev(),
            close_after,
        };
        self.save_op = SAVE_OP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.save_deadline = Some(std::time::Instant::now() + SAVE_TIMEOUT);
    }
    /// 若处于保存中，标记「完成后关闭」（用于保存进行时用户点『保存并关闭』）。
    pub(super) fn request_close_on_saved(&mut self) {
        if let SaveState::Saving { close_after, .. } = &mut self.save {
            *close_after = true;
        }
    }
}

/// 编辑器窗口的共享状态（主窗口与 deferred viewport 回调共用，见 App::editor_state）。
#[derive(Default)]
pub(super) struct EditorState {
    pub(super) tabs: Vec<EditorTab>,
    /// 当前激活标签
    pub(super) active: usize,
    /// 上次渲染的激活标签（用于切换后滚到可视区）
    pub(super) shown: usize,
    /// 一次性请求：新开/切换后把编辑器窗口置前并聚焦
    pub(super) focus: bool,
    /// 「关闭全部」时若有未保存修改，弹确认框
    pub(super) close_confirm: bool,
    /// 关闭单个「脏」标签前的确认（标签索引）
    pub(super) close_tab_confirm: Option<usize>,
    /// 关闭标签后请求主循环归还内存（trim）
    pub(super) trim_request: bool,
    /// 标签拖动重排状态（仿主窗口）：拖动索引 / 抓取偏移 / 内容总宽缓存
    pub(super) tab_drag: Option<usize>,
    pub(super) tab_grab_dx: f32,
    pub(super) tab_total_w: f32,
    /// 已判定「保存超时」的保存操作 id 墓碑（有界滚动窗口）。迟到的真实
    /// FileSaved/FileSaveFailed/FileSaveConflict 若命中此表即识别为「超时后的迟到事件」直接丢弃，
    /// 不再据其更新标签状态（标签可能已被用户关闭 / 已重试保存）。仿 mcp_bridge::file_op_tombstones。
    pub(super) save_tombstones: std::collections::VecDeque<u64>,
}

impl EditorState {
    /// 移除指定标签：修正 active、可选保存光标、清理 egui TextEditState，并请求 trim。
    pub(super) fn remove_tab_at(&mut self, ctx: &egui::Context, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        let closed = self.tabs.remove(index);
        if index < self.active {
            self.active -= 1;
        } else if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        }
        if closed.doc.is_none() && closed.load_id.is_none() {
            crate::store::save_cursor_line(
                &format!("{}|{}", closed.server, closed.editor.path),
                closed.editor.caret_line(),
            );
        }
        ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
        self.trim_request = true;
        true
    }

    /// 关闭全部标签并清理各自的 TextEdit 内存状态；请求 trim。
    pub(super) fn close_all(&mut self, ctx: &egui::Context) {
        for tab in self.tabs.drain(..) {
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(tab.text_id));
        }
        self.active = 0;
        self.close_confirm = false;
        self.trim_request = true;
    }
}

/// 主窗口会话标签条的拖拽重排 + 滚动状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
pub(super) struct TabBar {
    /// 正在拖拽排序的标签源索引
    pub(super) drag: Option<usize>,
    /// 拖拽起点在标签内的横向抓取偏移（让被拖标签跟手而不跳到光标处）
    pub(super) grab_dx: f32,
    /// 标签条总宽缓存（用于撑出横向滚动内容宽度）
    pub(super) total_w: f32,
    /// 请求把激活标签滚动到可视区（新建/点击/Ctrl+Tab 切换时置位）
    pub(super) scroll_to_active: bool,
}

/// 进程/GPU 详情小窗状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
pub(super) struct Popups {
    /// 进程详情小窗
    pub(super) proc: Option<ProcPopup>,
    pub(super) proc_just_opened: bool,
    /// GPU 详情小窗（仅记录弹出位置，数据每帧从活动会话取）
    pub(super) gpu: Option<egui::Pos2>,
    pub(super) gpu_just_opened: bool,
}

/// 看图工具窗口状态（从 App 抽出的内聚字段组）。
#[derive(Default)]
pub(super) struct ImageView {
    /// 已打开的图片标签
    pub(super) tabs: Vec<ImageTab>,
    /// 当前激活标签下标
    pub(super) active: usize,
    /// 一次性请求：新开/切换后把看图窗口置前并聚焦
    pub(super) focus: bool,
    /// 上次渲染时的激活标签（用于侦测切换后滚到可视区）
    pub(super) shown: usize,
    /// 标签拖动重排状态（仿主窗口）
    pub(super) tab_drag: Option<usize>,
    pub(super) grab_dx: f32,
    pub(super) total_w: f32,
}

impl ImageView {
    /// 移除指定图片标签并修正 active 下标。
    pub(super) fn remove_tab_at(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        self.tabs.remove(index);
        if index < self.active {
            self.active -= 1;
        } else if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        }
        true
    }
}

pub(super) struct ImageTab {
    /// 所属会话标题（仅显示）
    pub(super) server: String,
    /// 所属会话稳定唯一 id（身份匹配用）
    pub(super) uid: u64,
    pub(super) path: String,
    pub(super) tex: egui::TextureHandle,
    /// 原始字节（用于「另存为」，保留源格式/质量）
    pub(super) data: Vec<u8>,
    /// 原始像素尺寸
    pub(super) size: egui::Vec2,
    /// 缩放系数；0 表示「首帧自动适应窗口」
    pub(super) zoom: f32,
    /// 平移偏移（像素）
    pub(super) offset: egui::Vec2,
}

/// 自检截图状态。
pub(super) struct Shot {
    pub(super) path: String,
    pub(super) deadline: std::time::Instant,
    pub(super) requested: bool,
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
            tid: 1,
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
            save_op: 0,
            save_deadline: None,
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
