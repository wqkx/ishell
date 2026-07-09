//! 右下文件操作区：状态、树与入口。列表见 `list`，对话框见 `dialogs`，IME 见 `ime`。
//! 支持：进入/刷新目录、拖拽上传、右键下载/删除/重命名/改权限、复制路径、新建文件/目录。

use std::collections::{HashMap, HashSet};

use egui::{RichText, Sense};

use crate::proto::FileEntry;
use crate::theme::Palette;

mod dialogs;
mod ime;
mod list;

use dialogs::dialogs;
use list::{file_list, DragPaths, valid_move_srcs};

/// 文件面板状态（每个会话一份）。
#[derive(Default)]
pub struct FilePanelState {
    /// 树根（默认 "/"）
    pub root: String,
    /// 右栏当前目录（绝对路径）
    pub cwd: String,
    /// 路径 -> 该目录的条目（树与右栏共用）
    pub listings: HashMap<String, Vec<FileEntry>>,
    /// 树中已展开的目录
    pub expanded: HashSet<String>,
    /// 正在加载的目录
    pub loading: HashSet<String>,
    /// 各加载中目录的 (首次请求时刻, 已发请求次数)，用于弱网下超时重试。
    /// 惰性登记：任何进入 loading 的路径首帧被打上时间戳，随 loading 移除而清理。
    pub load_at: HashMap<String, (f64, u32)>,
    /// 列目录失败（无效/无权限）的路径集合：路径栏据此在最右侧显示「路径无效」标识
    pub nav_error: HashSet<String>,
    /// 右栏当前选中的行索引（支持多选）
    pub selected: HashSet<usize>,
    /// 区间选择的锚点行
    pub anchor: Option<usize>,
    /// 正在原地重命名的行（含输入缓冲与是否首帧）
    pub renaming: Option<Renaming>,
    /// 待触发重命名（行索引, 单击时刻）——延时以避开双击打开
    pub pending_rename: Option<(usize, f64)>,
    /// 路径编辑模式（双击路径栏进入），Some 时显示输入框
    pub path_edit: Option<String>,
    /// 路径编辑框是否需要请求焦点（仅进入时一次）
    pub path_edit_focus: bool,
    /// 右键路径编辑框那一刻的选区（字符下标 a<b）。egui 右键会把选区塌成光标、失焦又不画高亮，
    /// 故在右键当帧把选区存下来：菜单「复制」用它取子串，菜单打开期间也据它把高亮还原回去。
    pub path_edit_rsel: Option<(usize, usize)>,
    /// 上次已同步到树的 cwd（仅在 cwd 变化时同步，允许手动折叠）
    pub synced_cwd: String,
    /// 当前弹出的对话框
    pub dialog: Option<Dialog>,
    /// 对话框输入框的 IME 组字范围（字节位，绕开 egui Commit 门自绘 IME 用），跨帧维护
    pub dialog_ime: Option<(usize, usize)>,
    /// 路径栏编辑框的 IME 组字范围（与 dialog_ime 同理）
    pub path_edit_ime: Option<(usize, usize)>,
    /// 列表排序键 + 是否降序
    pub sort_key: SortKey,
    pub sort_desc: bool,
    /// 列表五列宽度（名称/大小/修改时间/权限/所有者）。全 0 表示未初始化，
    /// 首帧惰性从配置载入（egui_extras 内建 TableState 私有不可读，故自管 + 持久化）。
    pub col_w: [f32; 5],
    /// 按名称过滤当前目录列表（不区分大小写子串匹配）
    pub filter: String,
    /// 「复制路径」按钮的成功反馈时刻（短暂显示对勾）
    pub copy_flash: Option<f64>,
    /// 面包屑单击导航的延后执行（路径, 单击时刻）——给双击编辑留判定窗口
    pub pending_nav: Option<(String, f64)>,
    /// 弹簧式拖拽导航：当前悬停目标目录与起始时刻 (目标路径, 悬停起点)。
    /// 目标可为「上级目录」(parent) 或某个文件夹的绝对路径。
    pub spring_since: Option<(String, f64)>,
    /// 弹簧式跳转动画的触发时刻（在指针处播放两次脉冲环）
    pub spring_flash: Option<f64>,
    /// 拖到「文件树」节点上的悬停计时 (节点路径, 悬停起点)：停留够久则展开/折叠该节点。
    /// 完成一次切换后存 +∞ 作为哨兵，避免在同一节点上反复翻转（移开再回来才会再次触发）。
    pub tree_spring_since: Option<(String, f64)>,
    /// 移动操作撤销栈（Ctrl+Z 逐步回退最近的拖拽移动）。
    pub move_undo: Vec<MoveRecord>,
    /// 上传成功后待选中的项：(目录, 文件名集合)。该目录刷新渲染时按名选中并清空。
    pub pending_select: Option<(String, std::collections::HashSet<String>)>,
    /// 收藏的文件夹路径（按服务器区分，持久化）。
    pub favorites: Vec<String>,
    /// 该服务器的持久化键（host），收藏读写用。
    pub server_key: String,
    /// 「返回上一个目录」历史栈（浏览器式后退）。
    pub nav_history: Vec<String>,
    /// 上一帧末的 cwd，用于检测目录切换并把旧目录压入历史。
    pub nav_prev: String,
    /// 本次切换由「后退」触发（不再压栈，避免来回循环）。
    pub nav_pending_back: bool,
    /// 面包屑「幽灵」子路径：从当前 cwd 点到某父级后，仍以淡色显示的原完整路径。
    /// 仅当 `cwd` 仍是其前缀时保留；点淡色段可回到子目录；cwd 切到旁支则清空。
    /// 双击进入编辑时仍编辑 `cwd`，不受此字段影响。
    pub path_trail: Option<String>,
}

/// 一次「移动」的撤销记录：被移动项的原始绝对路径 + 落入的目标目录。
/// 撤销时把 `dest_dir/<basename>` 移回各自原父目录。
pub struct MoveRecord {
    /// 移动前各项的绝对路径（同一次拖拽必同源，故共享父目录）
    pub original: Vec<String>,
    /// 移动落入的目标目录
    pub dest_dir: String,
}

impl FilePanelState {
    /// 记录一次移动以供撤销；限制栈深度避免无限增长。
    fn record_move(&mut self, original: Vec<String>, dest_dir: String) {
        if original.is_empty() {
            return;
        }
        self.move_undo.push(MoveRecord { original, dest_dir });
        if self.move_undo.len() > 50 {
            self.move_undo.remove(0);
        }
    }
}

/// 列目录请求多久无响应即判定超时并重发（秒）。弱网下每次重发给 SFTP 会话重连留出恢复时间。
const LIST_TIMEOUT: f64 = 6.0;
/// 单个目录最多请求次数（含首次）；超过仍无响应则放弃并让用户手动刷新。弱网需较大预算，
/// 让后台 SFTP 重连有足够时间恢复（≈ LIST_TIMEOUT × LIST_MAX_TRIES 秒）。
const LIST_MAX_TRIES: u32 = 10;

/// 文件列表默认列宽（名称/大小/修改时间/权限/所有者）
pub(super) const DEFAULT_COLS: [f32; 5] = [220.0, 80.0, 140.0, 96.0, 120.0];

/// 拖拽悬停多久后自动进入目标目录（秒）
pub(super) const UP_DWELL: f64 = 0.8;
/// 跳转动画时长（秒，期间播放两次脉冲）
pub(super) const UP_FLASH: f64 = 0.5;

/// 文件列表排序键。
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Mtime,
}

/// 原地重命名状态。
pub struct Renaming {
    pub idx: usize,
    pub buf: String,
    pub init: bool,
    /// 行内重命名输入框的 IME 组字范围（与 dialog_ime 同理，绕开 egui Commit 门）
    pub ime: Option<(usize, usize)>,
}

/// 模态小对话框。
pub enum Dialog {
    NewDir { name: String },
    NewFile { name: String },
    Upload { local: String },
    Chmod { path: String, mode: u32, name: String },
    Rename { path: String, name: String },
    ConfirmDelete { items: Vec<(String, bool, String)> }, // (path, is_dir, name)，支持多选批量删除
    ConfirmOpenLarge { path: String, size: u64 },
    /// 非文本后缀，确认是否仍用文本编辑器打开
    ConfirmOpenAsText { path: String, size: u64 },
    /// 撤销移动确认：what=文件名概述，from=当前所在目录，to=移回的原目录（栈顶记录的预览）
    ConfirmUndoMove { what: String, from: String, to: String },
}

/// 面板交互产生的动作，由 App 翻译为 SFTP 指令或剪贴板操作。
pub enum FileAction {
    /// 请求列出目录（展开树 / 刷新 / 进入）
    List(String),
    Download(String),
    Upload { local: String, remote_dir: String },
    Mkdir(String),
    CreateFile(String),
    Chmod { path: String, mode: u32 },
    /// 批量删除：一条 rm 处理所有路径（单通道），避免多文件时并发开太多 SSH 通道被服务端拒绝
    DeleteMany(Vec<String>),
    Rename { from: String, to: String },
    /// 复制选中项到 App 级剪贴板（跨 tab 共享）；每项 (绝对路径, 是否目录)
    ClipCopy { items: Vec<(String, bool)> },
    /// 剪切选中项到剪贴板（粘贴时为移动，会在删除前二次确认）
    ClipCut { items: Vec<(String, bool)> },
    /// 把剪贴板内容粘贴到当前目录（同机用 cp/mv，跨机走下载→上传中转，由 App 决定）
    Paste { dest_dir: String },
    /// 在同一会话内拖拽移动：把 srcs 移动到 dest_dir（远端 mv）
    Move { srcs: Vec<String>, dest_dir: String },
    CopyPath(String),
    /// 双击文本文件 -> 打开编辑器（force=true 放宽大小限制）
    OpenFile { path: String, force: bool },
    /// 双击图片文件 -> 打开看图工具
    OpenImage { path: String },
    /// 双击 PDF -> 打开 PDF 查看器（远端 poppler 渲染）
    OpenPdf { path: String },
    /// 双击 Word(docx) -> 打开文档查看器（本地解析）
    OpenDocx { path: String },
    /// 在终端 cd 到该目录并聚焦终端
    CdTerminal(String),
    /// 直接设置状态栏文案（用于撤销等即时提示）
    Status(String),
}

/// 是否为可用看图工具打开的常见图片扩展名。
pub fn is_image_path(name: &str) -> bool {
    let lower = name.rsplit('.').next().map(|e| e.to_ascii_lowercase());
    matches!(lower.as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "bmp"))
}

/// 按后缀粗判是否「可直接以文本打开」。采用「黑名单」策略：除了**已知的二进制后缀**，其余一律
/// 当文本打开（无扩展名、各类科学计算/配置/日志等纯文本格式都直接开，不再误弹确认框）。
/// 真正的二进制若漏网，后台读取时的 NUL 检查会兜底拒绝并报错。
pub fn is_text_path(name: &str) -> bool {
    let fname = name.rsplit('/').next().unwrap_or(name);
    let ext = match fname.rsplit_once('.') {
        Some((base, e)) if !base.is_empty() => e.to_ascii_lowercase(),
        _ => return true, // 无扩展名 / dotfile → 文本
    };
    // 已知二进制后缀 → 需要确认（图片在调用处单独处理，这里不含）
    let binary = matches!(
        ext.as_str(),
        // 压缩 / 打包
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "tbz" | "xz" | "txz" | "zst" | "7z" | "rar" | "lz" | "lz4" | "jar" | "war" | "whl" | "deb" | "rpm" | "apk" | "dmg" | "iso" | "cab"
        // 可执行 / 目标 / 库
        | "exe" | "dll" | "so" | "dylib" | "a" | "o" | "obj" | "lib" | "bin" | "elf" | "class" | "pyc" | "pyo" | "wasm" | "msi" | "ko"
        // 媒体（音视频）
        | "mp3" | "mp4" | "m4a" | "m4v" | "aac" | "flac" | "wav" | "ogg" | "opus" | "avi" | "mkv" | "mov" | "wmv" | "webm" | "flv" | "mpg" | "mpeg" | "3gp"
        // 图片（其它入口防呆）/ 字体
        | "ico" | "icns" | "tif" | "tiff" | "webp" | "heic" | "psd" | "ttf" | "otf" | "ttc" | "woff" | "woff2" | "eot"
        // 文档（私有二进制容器）
        | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        // 数据库 / 序列化 / 数值
        | "db" | "sqlite" | "sqlite3" | "mdb" | "pkl" | "pickle" | "npy" | "npz" | "mat" | "h5" | "hdf5" | "parquet" | "feather" | "arrow" | "onnx" | "bson"
        // 科学计算二进制轨迹/态（GROMACS/AMBER/NAMD 等；其文本格式 gro/top/itp/mdp/ndx/xvg/pdb 仍走文本）
        | "trr" | "xtc" | "tpr" | "edr" | "cpt" | "dcd" | "binpos" | "ncdf" | "nc" | "gbw" | "wfn"
    );
    !binary
}

/// 「打开 / 进入」一个条目的意图——把目录、图片、文本、大文件确认、断链等分支统一，
/// 供双击与回车两处共用，避免逻辑分叉。
pub(super) enum OpenIntent {
    /// 进入目录（普通目录或指向目录的软链；后者用「规范目标路径」以便正确加载与显示面包屑）
    Navigate(String),
    /// 看图工具打开
    Image(String),
    /// PDF 查看器打开（远端 poppler 渲染）
    Pdf(String),
    /// Word(docx) 查看器打开（本地解析）
    Docx(String),
    /// 非文本后缀：弹确认框（路径, 大小）
    ConfirmText(String, u64),
    /// 大文本文件：弹确认框（路径, 大小）
    ConfirmLarge(String, u64),
    /// 直接以文本编辑器打开
    Text(String),
    /// 断链：目标不存在，提示用户
    Broken,
}

/// 据条目类型决定双击/回车的行为。软链已由 worker 跟随解析（link_dir / link_target）：
/// - 指向目录 → 进入其规范目标；
/// - 指向文件 → 按链接名后缀走图片/文本/大文件分支（worker 读取时自动跟随到目标）；
/// - 断链 → Broken。
pub(super) fn open_intent(e: &FileEntry, full: &str) -> OpenIntent {
    // 目录，或指向目录的软链 → 进入。软链优先用解析出的规范目标路径，
    // 否则（理论上 link_dir=true 必有 target）回退到链接自身路径。
    if e.is_dir || e.link_dir {
        let dest = if e.is_link {
            e.link_target.clone().unwrap_or_else(|| full.to_string())
        } else {
            full.to_string()
        };
        return OpenIntent::Navigate(dest);
    }
    // 断链：是链接却没解析出目标
    if e.is_link && e.link_target.is_none() {
        return OpenIntent::Broken;
    }
    // 文件（含指向文件的软链）：按名称后缀分流
    let lower = e.name.to_lowercase();
    if is_image_path(&e.name) {
        OpenIntent::Image(full.to_string())
    } else if lower.ends_with(".pdf") {
        OpenIntent::Pdf(full.to_string())
    } else if lower.ends_with(".docx") {
        OpenIntent::Docx(full.to_string())
    } else if !is_text_path(&e.name) {
        OpenIntent::ConfirmText(full.to_string(), e.size)
    } else if e.size > crate::limits::FILE_SOFT_LIMIT {
        OpenIntent::ConfirmLarge(full.to_string(), e.size)
    } else {
        OpenIntent::Text(full.to_string())
    }
}

impl FilePanelState {
    /// 收到目录列表后由 App 调用：写入缓存并清除 loading；首次自动设为 cwd。
    pub fn on_listing(&mut self, path: String, entries: Vec<FileEntry>) {
        self.loading.remove(&path);
        self.nav_error.remove(&path); // 列出成功 → 清除该路径的「无效」标记
        // 选择防错位：行选择存的是「排序后行索引」，刷新后条目集或排序键字段一旦变化，
        // 旧索引可能指向另一个文件——随后的删除/复制会作用于错误目标。
        // 条目完全一致才保留选择，否则一律清空（保守正确）。
        if path == self.cwd {
            let same = self.listings.get(&path).is_some_and(|old| {
                old.len() == entries.len()
                    && old
                        .iter()
                        .zip(entries.iter())
                        .all(|(a, b)| a.name == b.name && a.size == b.size && a.mtime == b.mtime && a.is_dir == b.is_dir)
            });
            if !same {
                self.selected.clear();
                self.anchor = None;
            }
        }
        self.listings.insert(path.clone(), entries);
        if self.cwd.is_empty() {
            self.cwd = path;
        }
    }

    /// 列目录失败（无效/无权限路径）由 App 调用：标记该路径无效，并写入空列表占位
    /// 以避免每帧重复发起 List 请求（与空目录区分仅靠 nav_error）。
    pub fn on_list_failed(&mut self, path: String, retryable: bool) {
        // 首次列举（cwd 尚空）即失败时也要落 cwd，否则文件区一片空白且无法显示状态
        if self.cwd.is_empty() {
            self.cwd = path.clone();
        }
        if retryable {
            // 会话级错误（弱网/SFTP 通道重连中）：保留 loading，交给重试循环稍后自动重发；
            // 不落「无效」、不动已有列表——待重连恢复后某次重试即可成功，界面持续转圈+重试提示。
            self.loading.insert(path);
            return;
        }
        // 路径级错误（不存在/无权限）：清 loading、落「无效」占位（不覆盖已有非空列表）
        self.loading.remove(&path);
        if self.listings.get(&path).map_or(true, |v| v.is_empty()) {
            self.nav_error.insert(path.clone());
            self.listings.insert(path, Vec::new());
        }
    }

    /// 手动刷新指定目录：清缓存/无效标记/重试计时并重新发起 List（重试次数从头计）。
    /// 返回是否已发起（空路径不发）。调用方负责把返回的 List 动作推入队列。
    ///
    /// 一并清掉「直接子目录」的缓存：列目录时渲染会顺带预取各直接子目录（命中缓存则跳过），
    /// 若不清子目录缓存，刷新后子目录仍是旧数据，随后跳转进去（有缓存即不再刷新）会看到陈旧内容。
    /// 故刷新时连带失效子目录，让预取重新一次性拉取，令「有缓存即不刷新」的跳转始终基于最新数据。
    fn refresh_dir(&mut self, path: &str) -> bool {
        if path.is_empty() {
            return false;
        }
        // 移除该目录与其所有直接子目录的缓存/无效标记（孙级不动：进入子目录时其渲染会再预取）
        self.listings.retain(|k, _| k != path && parent_of(k) != path);
        self.nav_error.retain(|k| k != path && parent_of(k) != path);
        self.load_at.remove(path); // 重置超时计时，让手动刷新获得完整重试预算
        self.loading.insert(path.to_string());
        true
    }

    /// 新建目录/文件后，乐观地把新条目插入当前目录列表并标记选中——避免整目录刷新造成闪动。
    /// owner/精确权限/mtime 等元数据留待随后的静默刷新回填；若远端创建失败，刷新会把它移除。
    fn insert_new(&mut self, dir: &str, name: &str, is_dir: bool) {
        let list = self.listings.entry(dir.to_string()).or_default();
        if list.iter().any(|e| e.name == name) {
            return; // 同名已存在：不重复插入，交由远端报错
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        list.push(FileEntry {
            name: name.to_string(),
            is_dir,
            is_link: false,
            size: 0,
            mtime: now,
            perm: if is_dir { 0o755 } else { 0o644 },
            owner: String::new(),
            link_target: None,
            link_dir: false,
        });
        // 刷新渲染时按名选中新条目（与上传后高亮一致）
        self.pending_select = Some((dir.to_string(), std::iter::once(name.to_string()).collect()));
    }
}

/// 路径栏「复制」：写系统剪贴板（供外部应用粘贴）+ 存入 egui 进程内暂存
///（egui 的 copy_text 走 winit 剪贴板，同进程内 arboard 常读不到，故另存一份供内部粘贴回退）。
pub(super) fn write_clip_path(ui: &egui::Ui, path: String) {
    ui.ctx().copy_text(path.clone()); // egui 剪贴板：供跨应用粘贴
    // 同时用 arboard 写系统剪贴板：否则 Linux 上 egui/winit 的同进程 copy_text 读不回来，
    // read_clip_path(arboard 优先) 会拿到旧的外部剪贴板、把刚复制的路径「盖掉」→ 粘贴错值。
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_text(path.clone());
    }
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new("file_panel_copied_path"), path));
}

/// 路径栏「粘贴」：优先系统剪贴板（能拿到外部复制的路径），读不到再退回进程内暂存。
/// 返回已 trim 且非空的路径。
pub(super) fn read_clip_path(ui: &egui::Ui) -> Option<String> {
    if let Some(t) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    ui.ctx()
        .data(|d| d.get_temp::<String>(egui::Id::new("file_panel_copied_path")))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[allow(deprecated)] // egui::popup_below_widget/toggle_popup 在 0.34 仍稳定可用（收藏夹弹窗）
pub fn show(ui: &mut egui::Ui, state: &mut FilePanelState, has_clip: bool) -> Vec<FileAction> {
    let mut actions = Vec::new();

    // 导航历史 + 面包屑幽灵子路径：检测上一帧的目录切换。
    if state.cwd != state.nav_prev {
        // 幽灵路径：沿原路径上/下钻时保留；切到旁支或回到幽灵末端则清除。
        update_path_trail(state);
        if state.nav_pending_back {
            state.nav_pending_back = false;
        } else if !state.nav_prev.is_empty() {
            state.nav_history.push(state.nav_prev.clone());
            if state.nav_history.len() > 100 {
                state.nav_history.remove(0);
            }
        }
        state.nav_prev = state.cwd.clone();
    }

    // 拖入文件 -> 上传到当前目录
    let dropped: Vec<String> = ui.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy().into_owned()))
            .collect()
    });
    if !dropped.is_empty() && !state.cwd.is_empty() {
        for local in dropped {
            actions.push(FileAction::Upload { local, remote_dir: state.cwd.clone() });
        }
    }

    // 仅在右栏目录“变化”时同步树（展开到 cwd）；其余时间允许用户自由折叠
    if state.cwd != state.synced_cwd {
        sync_tree(state, &mut actions);
        state.synced_cwd = state.cwd.clone();
        state.filter.clear(); // 切目录后清空过滤
    }

    // 弱网超时重试：List 请求可能长时间无响应（既不成功也不报错），loading 会一直卡住、
    // 转圈不出列表。此处对仍在 loading 的目录做超时判定：超时且未达上限则重发请求；
    // 达上限仍无响应则放弃——可见目录标记「无效」让用户手动刷新，隐藏目录静默丢弃待重访再拉。
    {
        let now = ui.input(|i| i.time);
        // 清理已完成（不在 loading）的登记；随后为新进入 loading 的目录打首帧时间戳。
        state.load_at.retain(|k, _| state.loading.contains(k));
        let loading_now: Vec<String> = state.loading.iter().cloned().collect();
        if !loading_now.is_empty() {
            // 持续请求重绘，保证即便无输入事件也能推进超时判定。
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
        let mut retry: Vec<String> = Vec::new();
        let mut give_up: Vec<String> = Vec::new();
        for p in &loading_now {
            let (t0, att) = *state.load_at.entry(p.clone()).or_insert((now, 1));
            if state.listings.contains_key(p) {
                continue; // 已有数据（预取叠加等），不必超时
            }
            if now - t0 >= LIST_TIMEOUT {
                if att < LIST_MAX_TRIES {
                    retry.push(p.clone());
                } else {
                    give_up.push(p.clone());
                }
            }
        }
        for p in retry {
            let att = state.load_at.get(&p).map(|v| v.1).unwrap_or(1);
            state.load_at.insert(p.clone(), (now, att + 1)); // 重置计时、累加次数
            actions.push(FileAction::List(p));
        }
        for p in give_up {
            state.loading.remove(&p);
            state.load_at.remove(&p);
            // 仅当前可见目录落「无效」占位；隐藏目录不污染缓存，重访时再拉。
            if p == state.cwd && !state.listings.contains_key(&p) {
                state.nav_error.insert(p.clone());
                state.listings.insert(p, Vec::new());
            }
        }
    }

    // 左侧目录树（自带浅色卡片，与右侧留出空隙）
    egui::Panel::left("file_tree")
        .resizable(true)
        .default_size(232.0)
        .size_range(150.0..=460.0)
        .frame(
            egui::Frame::new()
                .fill(Palette::PANEL_2)
                .inner_margin(egui::Margin { left: 8, right: 6, top: 6, bottom: 6 })
                .outer_margin(egui::Margin { left: 0, right: 8, top: 0, bottom: 0 }),
        )
        .show_inside(ui, |ui| {
            ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::TREE_VIEW, crate::i18n::tr("目录树", "Files"))).strong().color(Palette::ACCENT));
            ui.separator();
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                tree(ui, state, &mut actions);
            });
        });

    // 右侧：工具栏 + 文件列表（左侧留白，避免文件名贴到目录树）
    file_list(ui, state, has_clip, &mut actions);

    // 对话框
    dialogs(ui, state, &mut actions);

    actions
}

/// 绘制目录树（从 root 递归）。
fn tree(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    if state.root.is_empty() {
        return;
    }
    // 行间距与默认基本持平（先 -20%，再 +15%、+10%，回到舒适的疏密度）
    ui.spacing_mut().item_spacing.y *= 1.01;
    let mut toggles: Vec<String> = Vec::new();
    let mut select: Option<String> = None;
    let mut drop: Option<(Vec<String>, String)> = None; // 拖入某树节点 -> (srcs, dest_dir)
    let mut spring: Option<String> = None; // 拖拽悬停中的树节点（停留展开/折叠）

    // 根节点
    let root = state.root.clone();
    draw_node(ui, state, &root, &root, 0, &mut toggles, &mut select, &mut drop, &mut spring);

    // 弹簧式展开/折叠：在某树节点上持续悬停 UP_DWELL 秒则切换其展开态，并在指针处双闪；
    // 切换后把计时设为 +∞ 哨兵，避免同一节点反复翻转（移开再回来方可再次触发）。
    let now = ui.input(|i| i.time);
    if let Some(tp) = spring.clone() {
        let armed = matches!(&state.tree_spring_since, Some((k, _)) if *k == tp);
        if !armed {
            state.tree_spring_since = Some((tp.clone(), now));
        }
        let since = state.tree_spring_since.as_ref().map(|(_, t)| *t).unwrap_or(now);
        if since.is_finite() && now - since >= UP_DWELL {
            toggles.push(tp.clone());
            state.tree_spring_since = Some((tp.clone(), f64::INFINITY));
            state.spring_flash = Some(now); // 复用文件区的双闪动画（由 spring_navigate 在指针处绘制）
        }
        ui.ctx().request_repaint();
    } else {
        state.tree_spring_since = None;
    }

    // 应用展开/折叠
    for p in toggles {
        if state.expanded.contains(&p) {
            state.expanded.remove(&p);
        } else {
            state.expanded.insert(p.clone());
            if !state.listings.contains_key(&p) {
                state.loading.insert(p.clone());
                actions.push(FileAction::List(p));
            }
        }
    }
    // 应用导航（具体的加载/展开由 sync_tree 统一处理）
    if let Some(p) = select {
        state.cwd = p;
        state.selected.clear();
    }
    // 应用拖入树节点的移动：乐观地从当前目录移除被移动项（避免整目录刷新跳动），
    // 记录撤销，再发起远端 mv（目标目录由 worker 的 OpDone 刷新）。
    if let Some((srcs, dest_dir)) = drop {
        if dest_dir != state.cwd {
            let cwd = state.cwd.clone();
            if let Some(list) = state.listings.get_mut(&cwd) {
                let moved: HashSet<String> = srcs.iter().cloned().collect();
                list.retain(|e| !moved.contains(&join_path(&cwd, &e.name)));
            }
        }
        state.record_move(srcs.clone(), dest_dir.clone());
        actions.push(FileAction::Move { srcs, dest_dir });
        state.selected.clear();
        state.anchor = None;
    }
}

/// 自 root 起按 cwd 路径逐级展开树，并请求缺失目录的列表。
fn sync_tree(state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    if state.cwd.is_empty() {
        return;
    }
    for anc in ancestors(&state.cwd) {
        state.expanded.insert(anc.clone());
        if !state.listings.contains_key(&anc) && !state.loading.contains(&anc) {
            state.loading.insert(anc.clone());
            actions.push(FileAction::List(anc));
        }
    }
}

/// 路径的所有前缀（含自身），如 "/a/b" -> ["/", "/a", "/a/b"]。
fn ancestors(path: &str) -> Vec<String> {
    let mut out = vec!["/".to_string()];
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(seg);
        out.push(cur.clone());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn draw_node(
    ui: &mut egui::Ui,
    state: &FilePanelState,
    path: &str,
    label: &str,
    depth: usize,
    toggles: &mut Vec<String>,
    select: &mut Option<String>,
    drop: &mut Option<(Vec<String>, String)>,
    spring: &mut Option<String>,
) {
    let expanded = state.expanded.contains(path);
    let is_cwd = state.cwd == path;
    // 用 phosphor 图标，避免 ▸/▾ 在字体缺字形时显示成方块
    let tri = if expanded { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_RIGHT };
    let folder = if expanded { egui_phosphor::regular::FOLDER_OPEN } else { egui_phosphor::regular::FOLDER };
    let color = if is_cwd { Palette::ACCENT } else { Palette::TEXT };
    // 整行可点：占满可用宽度的一块可点击区域；单击展开/折叠，双击在右侧列表打开。
    // 行高在文本高度基础上 +10%（更松快的疏密度，便于拖拽落点）。
    let font = egui::TextStyle::Body.resolve(ui.style());
    let row_h = (ui.text_style_height(&egui::TextStyle::Body) + 1.0) * 1.1;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
    // 拖拽目标：把文件列表里的项拖到树中的文件夹上。悬停高亮 + 登记弹簧目标（停留展开/折叠），
    // 在该节点上松手即移入该目录。
    let dragging_in = resp.dnd_hover_payload::<DragPaths>().is_some();
    if is_cwd {
        ui.painter().rect_filled(rect, 4.0, Palette::ACCENT_SOFT);
    } else if dragging_in {
        ui.painter().rect_filled(rect, 4.0, Palette::ACCENT_SOFT);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_black_alpha(8));
    }
    if dragging_in {
        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.5, Palette::ACCENT), egui::StrokeKind::Inside);
        *spring = Some(path.to_string());
    }
    if let Some(payload) = resp.dnd_release_payload::<DragPaths>() {
        let srcs = valid_move_srcs(&payload.0, path);
        if !srcs.is_empty() {
            *drop = Some((srcs, path.to_string()));
        }
    }
    ui.painter().text(
        egui::pos2(rect.left() + depth as f32 * 12.0 + 4.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{tri} {folder} {label}"),
        font,
        color,
    );
    // 双击的第二次点击同帧也报 clicked：若不排除，双击 = toggle 两次（展开又收起，
    // 还可能重复发起列目录）。排除后双击效果 = 首击的 toggle + 导航，与单击展开态一致。
    if resp.clicked() && !resp.double_clicked() {
        toggles.push(path.to_string());
    }
    if resp.double_clicked() {
        *select = Some(path.to_string());
    }

    if expanded {
        if let Some(entries) = state.listings.get(path) {
            for e in entries.iter().filter(|e| e.is_dir) {
                let child = join_path(path, &e.name);
                draw_node(ui, state, &child, &e.name, depth + 1, toggles, select, drop, spring);
            }
        } else if state.loading.contains(path) {
            ui.horizontal(|ui| {
                ui.add_space((depth as f32 + 1.0) * 12.0);
                ui.spinner();
            });
        }
    }
}

/// 右侧：工具栏 + 文件表格。
/// 取路径最后一段（文件/目录名）。
pub(super) fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

pub(super) fn parent_of(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return "/".into();
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// `prefix` 是否为 `path` 的目录前缀（`/` 或 `prefix/`…）。
pub(super) fn path_is_prefix(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return path.starts_with('/');
    }
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// 目录切换后维护面包屑幽灵子路径：
/// - 从更深路径点到其祖先 → 保留原完整路径为 trail（淡色显示后续段）
/// - 沿 trail 下钻 / 上移但仍是前缀 → 保留 trail
/// - 切到旁支、或 cwd 已等于 trail 末端 → 清除
pub(super) fn update_path_trail(state: &mut FilePanelState) {
    let cwd = normalize_path(&state.cwd);
    let prev = normalize_path(&state.nav_prev);
    if let Some(trail) = state.path_trail.clone() {
        let trail = normalize_path(&trail);
        if cwd == trail || !path_is_prefix(&cwd, &trail) {
            state.path_trail = None;
        } else {
            state.path_trail = Some(trail);
        }
        return;
    }
    // 无 trail：若从子路径上移到祖先，把离开前的路径记为幽灵
    if !prev.is_empty() && path_is_prefix(&cwd, &prev) && cwd != prev {
        state.path_trail = Some(prev);
    }
}

pub(super) fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// 规范化目录路径：去掉末尾多余 "/"；空或全为 "/" 视为根。
pub(super) fn normalize_path(p: &str) -> String {
    let t = p.trim();
    if t == "/" || t.is_empty() {
        return "/".into();
    }
    let trimmed = t.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// 把权限位转为 `drwxr-xr-x` 形式。
pub(super) fn perm_string(perm: u32, is_dir: bool, is_link: bool) -> String {
    let t = if is_link { 'l' } else if is_dir { 'd' } else { '-' };
    let bit = |shift: u32, c: char| if perm & (1 << shift) != 0 { c } else { '-' };
    format!(
        "{t}{}{}{}{}{}{}{}{}{}",
        bit(8, 'r'), bit(7, 'w'), bit(6, 'x'),
        bit(5, 'r'), bit(4, 'w'), bit(3, 'x'),
        bit(2, 'r'), bit(1, 'w'), bit(0, 'x'),
    )
}

/// 简单的 unix 时间格式化：按本地时区偏移换算后展示（SFTP 的 mtime 为 UTC 纪元秒）。
pub(super) fn fmt_mtime(secs: u64) -> String {
    if secs == 0 {
        return "-".into();
    }
    // 加上本地 UTC 偏移（东区为正）得到本地墙钟秒；偏移取负仍越界则视为无效
    let local = secs as i64 + local_offset_seconds();
    if local < 0 {
        return "-".into();
    }
    let local = local as u64;
    let days = local / 86400;
    let rem = local % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// 本地时区相对 UTC 的偏移（秒，东区为正）。首次查询后缓存，避免逐行重复系统调用。
/// DST 仅按「当前」状态取一次，跨夏令时边界的历史文件可能差 1 小时，于文件列表足够。
fn local_offset_seconds() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(query_local_offset_seconds)
}

#[cfg(unix)]
fn query_local_offset_seconds() -> i64 {
    // libc::localtime_r 填充的 tm_gmtoff 即「本地相对 UTC 的秒数偏移」
    unsafe {
        let t: libc::time_t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(windows)]
fn query_local_offset_seconds() -> i64 {
    // 直接声明 Win32 FFI，免引入额外 crate。TIME_ZONE_INFORMATION 布局固定且稳定。
    #[repr(C)]
    struct WinSystemTime {
        w_year: u16, w_month: u16, w_day_of_week: u16, w_day: u16,
        w_hour: u16, w_minute: u16, w_second: u16, w_milliseconds: u16,
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: WinSystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: WinSystemTime,
        daylight_bias: i32,
    }
    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    unsafe {
        let mut tzi: TimeZoneInformation = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut tzi);
        // UTC = local + bias(分钟)；夏令时生效时再叠加 daylight_bias，否则用 standard_bias
        let extra = if r == TIME_ZONE_ID_DAYLIGHT { tzi.daylight_bias } else { tzi.standard_bias };
        -((tzi.bias + extra) as i64) * 60
    }
}

#[cfg(not(any(unix, windows)))]
fn query_local_offset_seconds() -> i64 {
    0
}

/// 自 1970-01-01 起的天数 -> (年,月,日)。算法源自 Howard Hinnant。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::{normalize_path, path_is_prefix, update_path_trail, FilePanelState};

    #[test]
    fn normalize_trailing_slash() {
        assert_eq!(normalize_path("/home/e5-1/"), "/home/e5-1");
        assert_eq!(normalize_path("/home/e5-1"), "/home/e5-1");
        assert_eq!(normalize_path("/home/e5-1///"), "/home/e5-1");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("///"), "/");
        assert_eq!(normalize_path("  /tmp/  "), "/tmp");
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn path_prefix_and_trail() {
        assert!(path_is_prefix("/", "/a/b"));
        assert!(path_is_prefix("/a", "/a/b/c"));
        assert!(path_is_prefix("/a/b", "/a/b"));
        assert!(!path_is_prefix("/a/b", "/a"));
        assert!(!path_is_prefix("/a", "/ab"));

        let mut s = FilePanelState {
            cwd: "/a".into(),
            nav_prev: "/a/b/c".into(),
            ..Default::default()
        };
        update_path_trail(&mut s);
        assert_eq!(s.path_trail.as_deref(), Some("/a/b/c"));

        // 沿幽灵下钻：保留
        s.cwd = "/a/b".into();
        s.nav_prev = "/a".into();
        update_path_trail(&mut s);
        assert_eq!(s.path_trail.as_deref(), Some("/a/b/c"));

        // 回到幽灵末端：清除
        s.cwd = "/a/b/c".into();
        s.nav_prev = "/a/b".into();
        update_path_trail(&mut s);
        assert!(s.path_trail.is_none());

        // 旁支：清除
        s.path_trail = Some("/a/b/c".into());
        s.cwd = "/x".into();
        s.nav_prev = "/a".into();
        update_path_trail(&mut s);
        assert!(s.path_trail.is_none());
    }
}
