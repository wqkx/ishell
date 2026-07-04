//! 右下文件操作区：左侧树形目录 + 右侧文件列表。
//! 支持：进入/刷新目录、拖拽上传、右键下载/删除/重命名/改权限、复制路径、新建文件/目录。

use std::collections::{HashMap, HashSet};

use egui::{RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::proto::FileEntry;
use crate::theme::Palette;
use crate::ui::fmt_bytes;

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
    /// 上次已同步到树的 cwd（仅在 cwd 变化时同步，允许手动折叠）
    pub synced_cwd: String,
    /// 当前弹出的对话框
    pub dialog: Option<Dialog>,
    /// 对话框输入框的 IME 组字范围（字节位，绕开 egui Commit 门自绘 IME 用），跨帧维护
    pub dialog_ime: Option<(usize, usize)>,
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
    /// 「返回上一个目录」历史栈（浏览器式后退；区别于「上级目录」=parent）。
    pub nav_history: Vec<String>,
    /// 上一帧末的 cwd，用于检测目录切换并把旧目录压入历史。
    pub nav_prev: String,
    /// 本次切换由「后退」触发（不再压栈，避免来回循环）。
    pub nav_pending_back: bool,
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
const DEFAULT_COLS: [f32; 5] = [220.0, 80.0, 140.0, 96.0, 120.0];

/// 拖拽悬停多久后自动进入目标目录（秒）
const UP_DWELL: f64 = 0.8;
/// 跳转动画时长（秒，期间播放两次脉冲）
const UP_FLASH: f64 = 0.5;

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
enum OpenIntent {
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
fn open_intent(e: &FileEntry, full: &str) -> OpenIntent {
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
    } else if e.size > 4 * 1024 * 1024 {
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

#[allow(deprecated)] // egui::popup_below_widget/toggle_popup 在 0.34 仍稳定可用（收藏夹弹窗）
pub fn show(ui: &mut egui::Ui, state: &mut FilePanelState, has_clip: bool) -> Vec<FileAction> {
    let mut actions = Vec::new();

    // 导航历史：检测上一帧的目录切换；非「后退」触发时，把旧目录压入历史，供「返回上一个目录」用。
    if state.cwd != state.nav_prev {
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
fn file_list(ui: &mut egui::Ui, state: &mut FilePanelState, has_clip: bool, actions: &mut Vec<FileAction>) {
    // 工具栏：扁平图标条（带浅色背景）
    use egui_phosphor::regular as icon;
    let mut bc_nav: Option<String> = None;
    let mut paste_here = false; // 右键菜单「粘贴到此目录」触发（has_clip 由 App 传入）
    // 弹簧式拖拽导航：本帧拖拽悬停的目标目录（Up 按钮的上一层 / 某文件夹），统一计时跳转
    let mut spring_target: Option<String> = None;
    // 拖到「上级目录」按钮上释放的移动（与文件夹落点统一在表格后处理，便于记录撤销）
    let mut up_move: Option<(Vec<String>, String)> = None;
    egui::Frame::new()
        .fill(Palette::PANEL_2)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 4))
        // 左侧外边距、不给右侧留边（右侧顶到边）
        .outer_margin(egui::Margin { left: 8, right: 0, top: 2, bottom: 2 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if tool_btn(ui, icon::ARROW_CLOCKWISE, crate::i18n::tr("刷新", "Refresh")) && state.refresh_dir(&state.cwd.clone()) {
                    actions.push(FileAction::List(state.cwd.clone()));
                }
                // 返回上一个目录（浏览器式后退，可连续返回；区别于「上级目录」=parent）
                let back_enabled = !state.nav_history.is_empty();
                let back_col = if back_enabled { Palette::TEXT } else { Palette::TEXT_DIM };
                // 后退用「弧形返回箭头」、上级用「上到横线箭头」——两者形态迥异，一眼可分，
                // 不再是相近的左箭头/上箭头。
                if tool_btn_color(ui, icon::ARROW_ARC_LEFT, crate::i18n::tr("返回上一个目录", "Back"), back_col) && back_enabled {
                    if let Some(prev) = state.nav_history.pop() {
                        state.nav_pending_back = true;
                        state.cwd = prev;
                        state.selected.clear();
                    }
                }
                let up_resp = tool_btn_resp(ui, icon::ARROW_LINE_UP, crate::i18n::tr("上级目录", "Up"), Palette::TEXT);
                if up_resp.clicked() && !state.cwd.is_empty() {
                    state.cwd = parent_of(&state.cwd);
                    state.selected.clear();
                }
                handle_up_drag(ui, state, &up_resp, &mut up_move, &mut spring_target);
                // 上传 / 删除 / 粘贴均不放工具栏：上传走拖拽或空白处右键菜单，删除走右键菜单或 Delete 键，
                // 粘贴走空白处右键菜单「粘贴到此目录」（保持工具栏精简）
                // 复制路径：点击后短暂显示绿色对勾，再恢复
                let now = ui.input(|i| i.time);
                let copied = state.copy_flash.is_some_and(|t| now - t < 1.1);
                let (ci, ctip, ccol) = if copied {
                    (icon::CHECK, crate::i18n::tr("已复制", "Copied"), Palette::OK)
                } else {
                    (icon::COPY, crate::i18n::tr("复制当前路径", "Copy path"), Palette::TEXT)
                };
                if tool_btn_color(ui, ci, ctip, ccol) && !state.cwd.is_empty() {
                    actions.push(FileAction::CopyPath(state.cwd.clone()));
                    state.copy_flash = Some(now);
                }
                if copied {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                }

                // 收藏夹：弹出可滚动路径列表，点路径进入、右侧删除，点其它处关闭。
                // 图标：当前目录已收藏=实心灰 ★，未收藏=空心 ☆；弹窗打开时按钮显示按下态（灰底）。
                let cwd_fav = !state.cwd.is_empty() && state.favorites.iter().any(|f| f == &state.cwd);
                let pop_id = ui.make_persistent_id("fav_popup");
                let pop_open = ui.memory(|m| m.is_popup_open(pop_id));
                let star_glyph = if cwd_fav { "★" } else { "☆" };
                let star_col = if cwd_fav { Palette::TEXT_DIM } else { Palette::TEXT };
                let star = ui
                    .scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.weak_bg_fill = if pop_open { Palette::TRACK } else { egui::Color32::TRANSPARENT };
                        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
                        v.widgets.active.bg_stroke = egui::Stroke::NONE;
                        ui.add(
                            egui::Button::new(RichText::new(star_glyph).size(16.0).color(star_col))
                                .min_size(egui::vec2(30.0, 26.0))
                                .corner_radius(6.0),
                        )
                        .on_hover_text(crate::i18n::tr("收藏夹", "Favorites"))
                    })
                    .inner;
                if star.clicked() {
                    ui.memory_mut(|m| m.toggle_popup(pop_id));
                }
                let mut fav_nav: Option<String> = None;
                let mut fav_remove: Option<usize> = None;
                egui::popup_below_widget(ui, pop_id, &star, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                    ui.set_min_width(280.0);
                    if state.favorites.is_empty() {
                        crate::ui::empty_state(ui, egui_phosphor::regular::STAR, crate::i18n::tr("暂无收藏，右键文件夹/空白处可加入", "No favorites — right-click to add"), false);
                    } else {
                        egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                            for (i, p) in state.favorites.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.set_min_width(266.0);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除收藏", "Remove")).clicked() {
                                            fav_remove = Some(i);
                                        }
                                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                            let disp = trailing_path(p, 40);
                                            if ui.add(egui::Label::new(RichText::new(format!("{} {}", icon::FOLDER, disp)).size(12.0).color(Palette::TEXT)).selectable(false).sense(Sense::click()))
                                                .on_hover_text(p.as_str())
                                                .clicked()
                                            {
                                                fav_nav = Some(p.clone());
                                            }
                                        });
                                    });
                                });
                            }
                        });
                    }
                });
                if let Some(i) = fav_remove {
                    if i < state.favorites.len() {
                        state.favorites.remove(i);
                        crate::store::save_favorites(&state.server_key, &state.favorites);
                    }
                }
                if let Some(p) = fav_nav {
                    state.cwd = p;
                    state.selected.clear();
                    ui.memory_mut(|m| m.close_popup(pop_id));
                }

                ui.add_space(4.0);
                ui.separator();
                // 路径栏最右侧：当前目录列举失败（无效/无权限路径）时显示「路径无效」标识
                let path_err = state.nav_error.contains(&state.cwd) && !state.loading.contains(&state.cwd);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                  if path_err {
                      ui.add(egui::Label::new(RichText::new(icon::WARNING_CIRCLE).color(Palette::DANGER).size(15.0)).sense(Sense::hover()))
                          .on_hover_text(crate::i18n::tr("路径无效或无法访问", "Invalid or inaccessible path"));
                      ui.add_space(3.0);
                  }
                  ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if state.path_edit.is_some() {
                    // 路径编辑模式：回车跳转、Esc 或点击别处退出
                    let mut go: Option<String> = None;
                    let mut done = false;
                    let take_focus = state.path_edit_focus;
                    if let Some(buf) = &mut state.path_edit {
                        let out = egui::TextEdit::singleline(buf)
                            .desired_width(ui.available_width() - 4.0)
                            .hint_text(crate::i18n::tr("输入路径后回车跳转，Esc 取消", "Enter path, Enter to go, Esc to cancel"))
                            .show(ui);
                        if take_focus {
                            // 首帧进入编辑：聚焦并全选路径，便于直接覆盖输入
                            let len = buf.chars().count();
                            let id = out.response.id;
                            let mut st = out.state;
                            st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(len),
                            )));
                            st.store(ui.ctx(), id);
                            out.response.request_focus();
                        }
                        let resp = &out.response;
                        // 用 consume_key「吃掉」回车事件：否则同一帧内文件列表的键盘处理器
                        // 会再次响应这次回车，误打开当前选中行（进入子目录 / 用编辑器打开文件）。
                        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                            let t = buf.trim();
                            if !t.is_empty() {
                                go = Some(t.to_string());
                            }
                            done = true;
                        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            done = true;
                        } else if resp.lost_focus() || resp.clicked_elsewhere() {
                            // 点击别处 -> 退出编辑模式
                            done = true;
                        }
                    }
                    state.path_edit_focus = false;
                    if let Some(p) = go {
                        bc_nav = Some(p);
                    }
                    if done {
                        state.path_edit = None;
                    }
                } else {
                    // 面包屑：路径超长时横向滚动（隐藏滚动条，滚轮滚动）；
                    // 单击逐级跳转，双击路径任意处（含分段、空白）进入编辑模式。
                    // 单击导航延后 ~0.28s 执行，期间若发生双击则取消，避免双击误触发跳转。
                    let now_t = ui.input(|i| i.time);
                    let cwd_s = state.cwd.clone();
                    let mut enter_edit = false;
                    let mut nav_click: Option<String> = None;
                    egui::ScrollArea::horizontal()
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .auto_shrink([false, false])
                        .stick_to_right(true) // 路径过长时默认展示末尾（当前目录）
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 3.0;
                                let root = ui.add(egui::Label::new(RichText::new(icon::HOUSE).color(Palette::ACCENT)).sense(Sense::click()));
                                if root.double_clicked() {
                                    enter_edit = true;
                                } else if root.clicked() {
                                    nav_click = Some("/".into());
                                }
                                let mut acc = String::new();
                                for seg in cwd_s.split('/').filter(|s| !s.is_empty()) {
                                    ui.label(RichText::new("›").color(Palette::TEXT_DIM));
                                    acc.push('/');
                                    acc.push_str(seg);
                                    let here = acc.clone();
                                    let is_last = here == cwd_s;
                                    let color = if is_last { Palette::TEXT } else { Palette::TEXT_DIM };
                                    let r = ui.add(egui::Label::new(RichText::new(seg).color(color)).sense(Sense::click()));
                                    if r.double_clicked() {
                                        enter_edit = true;
                                    } else if r.clicked() {
                                        nav_click = Some(here);
                                    }
                                }
                                // 末尾空白：双击也进入编辑
                                let rest = ui.available_size_before_wrap();
                                if rest.x > 8.0 {
                                    let (_, resp) = ui.allocate_exact_size(rest, Sense::click());
                                    if resp.double_clicked() {
                                        enter_edit = true;
                                    }
                                }
                            });
                        });
                    if enter_edit {
                        state.path_edit = Some(state.cwd.clone());
                        state.path_edit_focus = true;
                        state.pending_nav = None;
                    } else if let Some(p) = nav_click {
                        state.pending_nav = Some((p, now_t));
                    }
                    // 延时执行单击导航（其间未发生双击）
                    if let Some((p, t)) = state.pending_nav.clone() {
                        if now_t - t >= 0.28 {
                            bc_nav = Some(p);
                            state.pending_nav = None;
                        } else {
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                        }
                    }
                }
                  }); // 内层 left_to_right（面包屑/编辑框）
                }); // 外层 right_to_left（右侧无效标识）
            });
        });
    ui.add_space(2.0);
    if let Some(p) = bc_nav {
        // 规范化：去掉末尾多余的 "/"（否则与 worker 返回的规范路径不匹配，无法进入）
        state.cwd = normalize_path(&p);
        state.selected.clear();
    }
    ui.separator();

    if state.loading.contains(&state.cwd) && !state.listings.contains_key(&state.cwd) {
        // 已重试过（att>1）即视为弱网，给出「网络较慢，正在重试」提示，避免看似卡死无反馈
        let slow = state.load_at.get(&state.cwd).map(|v| v.1 > 1).unwrap_or(false);
        ui.horizontal(|ui| {
            ui.spinner();
            let msg = if slow {
                crate::i18n::tr("网络较慢，正在重试 …", "Slow network, retrying …")
            } else {
                crate::i18n::tr("加载中 …", "Loading …")
            };
            ui.label(RichText::new(msg).color(Palette::TEXT_DIM));
        });
        return;
    }

    let cwd = state.cwd.clone();
    let favs = state.favorites.clone(); // 供表格内右键菜单判断「已收藏」（state 在表格闭包里已被可变借用）
    let total_count = state.listings.get(&cwd).map(|e| e.len()).unwrap_or(0);

    // 名称过滤行：左侧留边（与操作栏一致）、右侧顶到边
    egui::Frame::new()
        .inner_margin(egui::Margin { left: 12, right: 0, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(Palette::TEXT_DIM).size(12.0));
                let clear_w = if state.filter.is_empty() { 0.0 } else { 22.0 };
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .desired_width(ui.available_width() - clear_w - 2.0)
                        .hint_text(crate::i18n::tr("按名称过滤", "Filter by name")),
                );
                if resp.changed() {
                    // 过滤变化会改变行索引，清空选择避免错位
                    state.selected.clear();
                    state.anchor = None;
                }
                if !state.filter.is_empty()
                    && ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                        .on_hover_text(crate::i18n::tr("清除过滤", "Clear"))
                        .clicked()
                {
                    state.filter.clear();
                }
            });
        });
    ui.add_space(2.0);

    let mut entries = match state.listings.get(&cwd) {
        Some(e) => e.clone(),
        None => return,
    };
    // 预取：进入目录后顺带请求各「直接子文件夹」的列表，点进下一级时即时显示（命中缓存）。
    // 每个子目录只在未缓存且未加载时请求一次；上限避免极端目录发起过多请求。
    {
        let mut prefetched = 0;
        for e in entries.iter().filter(|e| e.is_dir && !e.is_link) {
            if prefetched >= 64 {
                break;
            }
            let sub = join_path(&cwd, &e.name);
            if !state.listings.contains_key(&sub) && !state.loading.contains(&sub) {
                state.loading.insert(sub.clone());
                actions.push(FileAction::List(sub));
                prefetched += 1;
            }
        }
    }
    // 排序：目录始终在前，组内按所选键升/降序
    {
        let key = state.sort_key;
        let desc = state.sort_desc;
        entries.sort_by(|a, b| {
            (!a.is_dir).cmp(&!b.is_dir).then_with(|| {
                let ord = match key {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Size => a.size.cmp(&b.size),
                    SortKey::Mtime => a.mtime.cmp(&b.mtime),
                };
                if desc { ord.reverse() } else { ord }
            })
        });
    }
    // 应用名称过滤
    if !state.filter.trim().is_empty() {
        let f = state.filter.to_lowercase();
        entries.retain(|e| e.name.to_lowercase().contains(&f));
        if entries.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("无匹配（共 {total_count} 项）"),
                    crate::i18n::Lang::En => format!("No match ({total_count} items)"),
                }).color(Palette::TEXT_DIM).size(12.0));
            });
        }
    }

    // 上传成功后选中所传文件：当前目录刷新出新列表时，按文件名定位行号并选中（命中后才清空，
    // 以兼容「刷新尚未到达、文件暂未出现」的情形）。
    if let Some(names) = state.pending_select.as_ref().filter(|(d, _)| *d == cwd).map(|(_, n)| n.clone()) {
        let sel: HashSet<usize> = entries.iter().enumerate().filter(|(_, e)| names.contains(&e.name)).map(|(i, _)| i).collect();
        if !sel.is_empty() {
            state.anchor = sel.iter().min().copied();
            state.selected = sel;
            state.pending_select = None;
        }
    }

    let mut navigate: Option<String> = None;
    let mut sort_click: Option<SortKey> = None;
    let mut menu: Vec<Menu> = Vec::new();
    let mut clicks: Vec<usize> = Vec::new(); // 本帧被点击的行
    let mut rclick: Option<usize> = None; // 本帧被右键的行
    let mut open_file: Option<String> = None; // 双击文本文件
    let mut open_image: Option<String> = None; // 双击图片文件
    let mut open_pdf: Option<String> = None; // 双击 PDF 文件
    let mut open_docx: Option<String> = None; // 双击 Word(docx) 文件
    let mut confirm_open: Option<(String, u64)> = None; // 大文本文件待确认
    let mut confirm_text: Option<(String, u64)> = None; // 非文本后缀，待确认是否强开
    let mut rename_commit: Option<(String, String)> = None;
    let mut cancel_rename = false;
    let mut drop_move: Option<(Vec<String>, String)> = None; // 拖拽释放到某文件夹 -> (srcs, dest_dir)
    let mut broken_link = false; // 双击断链 -> 表后统一提示
    let now = ui.input(|i| i.time);
    let (mod_ctrl, mod_shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

    // 空白区域右键 -> 新建文件/目录（仅覆盖列表区域，避免遮挡上方路径栏）
    let bg = ui.interact(ui.available_rect_before_wrap(), ui.id().with("filelist_bg"), Sense::click());
    // 点击列表空白 → 让出当前(终端)焦点：终端仅在被点击时夺焦，让焦后 focused()=None，
    // ↑/↓/Enter 即作用于文件列表而非被终端的焦点锁吞掉。
    if bg.clicked() {
        ui.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
    }
    // 注意：bg 几何上覆盖整个列表，dnd_release_payload 用 contains_pointer 判定并「取走」载荷，
    // 若在此处先检查会把本应落到文件夹行的拖放抢走。故放到表格之后、仅当无行接收时再兜底。
    let mut bg_new_dir = false;
    let mut bg_new_file = false;
    let mut bg_upload = false;
    let mut bg_cd = false;
    let mut bg_fav = false;
    let mut bg_refresh = false;
    bg.context_menu(|ui| {
        if !state.cwd.is_empty()
            && ui.button(format!("{}  {}", icon::ARROW_CLOCKWISE, crate::i18n::tr("刷新", "Refresh"))).clicked()
        {
            bg_refresh = true;
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::FOLDER_PLUS, crate::i18n::tr("新建文件夹", "New folder"))).clicked() {
            bg_new_dir = true;
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::FILE_PLUS, crate::i18n::tr("新建文件", "New file"))).clicked() {
            bg_new_file = true;
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::UPLOAD_SIMPLE, crate::i18n::tr("上传文件", "Upload"))).clicked() {
            bg_upload = true;
            ui.close();
        }
        if has_clip {
            ui.separator();
            if ui.button(format!("{}  {}", icon::CLIPBOARD_TEXT, crate::i18n::tr("粘贴到此目录", "Paste here"))).clicked() {
                paste_here = true;
                ui.close();
            }
        }
        if !state.cwd.is_empty() {
            ui.separator();
            let faved = state.favorites.iter().any(|f| f == &state.cwd);
            let lbl = if faved {
                format!("★  {}", crate::i18n::tr("取消收藏当前目录", "Remove bookmark"))
            } else {
                format!("☆  {}", crate::i18n::tr("收藏当前目录", "Bookmark current dir"))
            };
            if ui.button(lbl).clicked() {
                bg_fav = true;
                ui.close();
            }
            if ui.button(format!("{}  {}", icon::TERMINAL_WINDOW, crate::i18n::tr("在终端打开当前目录", "Open current dir in terminal"))).clicked() {
                bg_cd = true;
                ui.close();
            }
        }
    });
    if bg_fav {
        toggle_favorite(state, state.cwd.clone());
    }
    if bg_refresh && state.refresh_dir(&cwd) {
        actions.push(FileAction::List(cwd.clone()));
    }

    // —— 键盘导航：↑/↓ 移动选中行、Enter 打开/进入目录（与 Ctrl+A/Delete 同一聚焦门：无文本框聚焦时生效）——
    let mut focus_list = false; // 本帧是否有行被点击 → 表外为文件列表夺取键盘焦点
    let mut scroll_to_row: Option<usize> = None;
    let kbd_nav = state.renaming.is_none()
        && state.path_edit.is_none()
        && state.dialog.is_none()
        && ui.ctx().memory(|m| m.focused().is_none());
    if kbd_nav && !entries.is_empty() {
        let n = entries.len();
        let cur = state
            .anchor
            .filter(|&a| a < n)
            .or_else(|| state.selected.iter().min().copied())
            .unwrap_or(0)
            .min(n - 1);
        let (down, up, enter) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
            )
        });
        if down || up {
            let nc = if down { (cur + 1).min(n - 1) } else { cur.saturating_sub(1) };
            state.selected.clear();
            state.selected.insert(nc);
            state.anchor = Some(nc);
            scroll_to_row = Some(nc);
        } else if enter {
            if let Some(e) = entries.get(cur) {
                let full = join_path(&cwd, &e.name);
                match open_intent(e, &full) {
                    OpenIntent::Navigate(p) => navigate = Some(p),
                    OpenIntent::Image(p) => open_image = Some(p),
                    OpenIntent::Pdf(p) => open_pdf = Some(p),
                    OpenIntent::Docx(p) => open_docx = Some(p),
                    OpenIntent::ConfirmText(p, sz) => confirm_text = Some((p, sz)),
                    OpenIntent::ConfirmLarge(p, sz) => confirm_open = Some((p, sz)),
                    OpenIntent::Text(p) => open_file = Some(p),
                    OpenIntent::Broken => actions.push(FileAction::Status(
                        crate::i18n::tr("断链：目标不存在", "Broken link: target missing").into(),
                    )),
                }
            }
        }
    }

    // 表头列宽拖拽产生的 (列号, 本帧位移)；在 Frame 闭包外声明，闭包结束后统一应用并落盘
    let mut col_drag: Option<(usize, f32)> = None;
    let mut col_drag_done = false;

    egui::Frame::new()
        .inner_margin(egui::Margin { left: 6, right: 2, top: 0, bottom: 0 })
        .show(ui, |ui| {
    // 滚动条滑块用灰度（非浮动），尤其拖动(active)时用深灰——否则默认拖动时偏白看不见
    ui.spacing_mut().scroll.floating = false;
    ui.spacing_mut().scroll.foreground_color = false;
    ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_rgb(193, 188, 175);
    ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_rgb(154, 148, 134);
    ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_rgb(114, 109, 97);
    // 拖拽悬停高亮用独立图层绘制：TableBuilder 闭包内 ui 已被可变借用，不能再取 ui.painter()
    let dnd_painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("file_dnd_hl")));
    // 列宽自管（Column::exact + 表头自绘拖拽）：内建 resizable 的 TableState 私有且按
    // id_salt(cwd) 隔离，既不能跨目录共享也无法持久化；自管后列宽全局一致并写入配置。
    if state.col_w.iter().all(|w| *w <= 0.0) {
        state.col_w = crate::store::load_file_cols().unwrap_or(DEFAULT_COLS);
    }
    let colw = state.col_w;
    let mut tbl = TableBuilder::new(ui)
        // 按目录区分滚动状态：进入子目录/切换目录后从顶部开始，不沿用上个目录的滚动位置
        .id_salt(&cwd)
        .striped(true)
        .resizable(false)
        .sense(Sense::click_and_drag())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::exact(colw[0]).clip(true))
        .column(Column::exact(colw[1]).clip(true))
        .column(Column::exact(colw[2]).clip(true))
        .column(Column::exact(colw[3]).clip(true))
        // owner 列不用 remainder：列表填不满时右侧留白，便于右键空白处操作当前文件夹
        .column(Column::exact(colw[4]).clip(true));
    // 键盘移动选中行时滚动到该行
    if let Some(r) = scroll_to_row {
        tbl = tbl.scroll_to_row(r, Some(egui::Align::Center));
    }
    tbl.header(22.0, |mut h| {
            let (cur, desc) = (state.sort_key, state.sort_desc);
            // 列宽拖拽热区：贴在表头列右缘，悬停/拖动时显示竖线并给出双向箭头光标
            let mut resize_handle = |ui: &mut egui::Ui, idx: usize| {
                let r = ui.max_rect();
                let hr = egui::Rect::from_min_max(
                    egui::pos2(r.right() - 2.0, r.top() - 3.0),
                    egui::pos2(r.right() + 4.0, r.bottom() + 3.0),
                );
                let resp = ui.interact(hr, ui.id().with(("colw", idx)), Sense::drag());
                if resp.hovered() || resp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    ui.painter().vline(r.right() + 1.0, hr.y_range(), egui::Stroke::new(2.0, Palette::ACCENT.gamma_multiply(0.6)));
                }
                if resp.dragged() {
                    col_drag = Some((idx, resp.drag_delta().x));
                }
                if resp.drag_stopped() {
                    col_drag_done = true;
                }
            };
            // 可排序表头：点击切换排序键/方向，激活列显示升降箭头
            for (idx, (k, label)) in [
                (SortKey::Name, crate::i18n::tr("名称", "Name")),
                (SortKey::Size, crate::i18n::tr("大小", "Size")),
                (SortKey::Mtime, crate::i18n::tr("修改时间", "Modified")),
            ]
            .into_iter()
            .enumerate()
            {
                h.col(|ui| {
                    let arrow = if cur == k {
                        if desc { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_UP }
                    } else {
                        ""
                    };
                    let color = if cur == k { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(format!("{label} {arrow}")).strong().color(color)).sense(Sense::click()))
                        .clicked()
                    {
                        sort_click = Some(k);
                    }
                    resize_handle(ui, idx);
                });
            }
            // 不可排序：权限 / 所有者
            for (idx, t) in [crate::i18n::tr("权限", "Perm"), crate::i18n::tr("所有者", "Owner")].into_iter().enumerate() {
                h.col(|ui| {
                    ui.label(RichText::new(t).strong().color(Palette::TEXT_DIM));
                    resize_handle(ui, 3 + idx);
                });
            }
        })
        .body(|mut body| {
            for (i, e) in entries.iter().enumerate() {
                let full = join_path(&cwd, &e.name);
                body.row(21.0, |mut row| {
                    row.set_selected(state.selected.contains(&i));
                    row.col(|ui| {
                        // 重命名中：显示输入框（默认选中后缀前的主名）
                        let renaming_here = matches!(&state.renaming, Some(r) if r.idx == i);
                        if renaming_here {
                            if let Some(r) = &mut state.renaming {
                                let out = egui::TextEdit::singleline(&mut r.buf)
                                    .desired_width(f32::INFINITY)
                                    .show(ui);
                                if r.init {
                                    let stem = stem_char_len(&r.buf);
                                    let id = out.response.id;
                                    let mut st = out.state;
                                    st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                        egui::text::CCursor::new(0),
                                        egui::text::CCursor::new(stem),
                                    )));
                                    st.store(ui.ctx(), id);
                                    out.response.request_focus();
                                    r.init = false;
                                }
                                if out.response.lost_focus() {
                                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !r.buf.trim().is_empty() {
                                        rename_commit = Some((full.clone(), join_path(&cwd, r.buf.trim())));
                                    }
                                    cancel_rename = true;
                                }
                            }
                        } else {
                            // 图标着色：目录/指向目录的软链=强调色；断链=危险色；其它软链=警示色；普通文件=弱色
                            let icon_col = if e.is_dir || e.link_dir {
                                Palette::ACCENT
                            } else if e.is_link && e.link_target.is_none() {
                                Palette::DANGER
                            } else if e.is_link {
                                Palette::WARN
                            } else {
                                Palette::TEXT_DIM
                            };
                            ui.spacing_mut().item_spacing.x = 5.0;
                            ui.label(RichText::new(file_icon(e)).color(icon_col));
                            // 名称：单行显示，超出由列 clip(true) 裁剪；悬停给出完整名称——应对超长 / 特殊字符 / emoji
                            let name_resp = ui.label(RichText::new(&e.name).color(Palette::TEXT));
                            // 软链：紧跟暗色「→ 目标」，让指向一目了然；断链显示红色「断链」
                            if e.is_link {
                                match &e.link_target {
                                    Some(t) => {
                                        ui.label(RichText::new(format!("→ {t}")).color(Palette::TEXT_DIM).size(11.0));
                                    }
                                    None => {
                                        ui.label(RichText::new(crate::i18n::tr("→ 断链", "→ broken")).color(Palette::DANGER).size(11.0));
                                    }
                                }
                            }
                            // 悬停 tooltip：完整名称（+ 链接目标 / 断链说明）
                            let tip = match (e.is_link, &e.link_target) {
                                (true, Some(t)) => format!("{}\n→ {t}", e.name),
                                (true, None) => format!("{}\n{}", e.name, crate::i18n::tr("断链（目标不存在）", "broken link (target missing)")),
                                _ => e.name.clone(),
                            };
                            name_resp.on_hover_text(tip);
                        }
                    });
                    row.col(|ui| {
                        // 目录与「指向目录的软链」不显示字节大小；文件型软链的 size 已由 worker 改为目标大小
                        let s = if e.is_dir || e.link_dir { "-".to_string() } else { fmt_bytes(e.size as f64) };
                        ui.label(RichText::new(s).color(Palette::TEXT_DIM));
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(fmt_mtime(e.mtime)).color(Palette::TEXT_DIM));
                    });
                    row.col(|ui| {
                        ui.label(
                            RichText::new(perm_string(e.perm, e.is_dir, e.is_link))
                                .monospace()
                                .color(Palette::TEXT_DIM),
                        );
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(&e.owner).color(Palette::TEXT_DIM));
                    });
                    // 整行交互：点击选择 / 延时重命名、双击进目录或打开、右键菜单
                    let r = row.response();
                    if r.clicked() {
                        focus_list = true; // 点击行 → 文件列表夺取键盘焦点（表外应用，避免借用冲突）
                        let was_sole = state.selected.len() == 1
                            && state.selected.contains(&i)
                            && state.renaming.is_none();
                        if was_sole && !mod_ctrl && !mod_shift {
                            // 已选中的行再次单击 -> 计划重命名（延时以避开双击）
                            state.pending_rename = Some((i, now));
                        } else {
                            clicks.push(i);
                        }
                    }
                    if r.double_clicked() {
                        state.pending_rename = None;
                        // 双击：目录进入、图片看图、文本编辑、大/非文本确认、指向目录的软链跟随进入、断链提示
                        match open_intent(e, &full) {
                            OpenIntent::Navigate(p) => navigate = Some(p),
                            OpenIntent::Image(p) => open_image = Some(p),
                            OpenIntent::Pdf(p) => open_pdf = Some(p),
                            OpenIntent::Docx(p) => open_docx = Some(p),
                            OpenIntent::ConfirmText(p, sz) => confirm_text = Some((p, sz)),
                            OpenIntent::ConfirmLarge(p, sz) => confirm_open = Some((p, sz)),
                            OpenIntent::Text(p) => open_file = Some(p),
                            OpenIntent::Broken => broken_link = true,
                        }
                    }
                    if r.secondary_clicked() {
                        rclick = Some(i);
                    }
                    // 拖拽源：整个拖动过程持续写入载荷（多选则整组、否则单项），
                    // 避免只在 drag_started 一帧设置时偶发丢失/沿用上次的旧载荷。
                    if r.drag_started() || r.dragged() {
                        let paths = drag_source_paths(state, &entries, &cwd, i);
                        if !paths.is_empty() {
                            r.dnd_set_drag_payload(DragPaths(paths));
                        }
                    }
                    // 拖拽目标：
                    // - 文件夹行：悬停高亮 + 登记弹簧目标（停留进入），释放→移入该文件夹；
                    // - 文件行：释放→移入当前目录（便于弹簧进入文件夹后在其内任意处松手）。
                    if e.is_dir {
                        if r.dnd_hover_payload::<DragPaths>().is_some() {
                            dnd_painter.rect_stroke(
                                r.rect,
                                4.0,
                                egui::Stroke::new(1.5, Palette::ACCENT),
                                egui::StrokeKind::Inside,
                            );
                            spring_target = Some(full.clone());
                        }
                        if let Some(payload) = r.dnd_release_payload::<DragPaths>() {
                            let srcs = valid_move_srcs(&payload.0, &full);
                            if !srcs.is_empty() {
                                drop_move = Some((srcs, full.clone()));
                            }
                        }
                    } else if let Some(payload) = r.dnd_release_payload::<DragPaths>() {
                        let srcs = valid_move_srcs(&payload.0, &cwd);
                        if !srcs.is_empty() {
                            drop_move = Some((srcs, cwd.clone()));
                        }
                    }
                    let is_fav = favs.iter().any(|f| f == &full);
                    entry_context(&r, e, i, &full, has_clip, is_fav, &mut menu);
                });
            }
        });
    });
    // 行被点击 → 让出终端焦点，使方向键导航文件列表（表格借用结束后再操作 ui，避免借用冲突）
    if focus_list {
        ui.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
    }

    // 拖拽预览：拖动中在指针旁画一个跟手的小标签（单项显示名称，多项显示数量），
    // 给「文件正被拖动」一个明确的视觉反馈。
    if let Some(payload) = egui::DragAndDrop::payload::<DragPaths>(ui.ctx()) {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let n = payload.0.len();
            let text = if n == 1 {
                payload.0[0].rsplit('/').next().unwrap_or("").to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("移动 {n} 项"),
                    crate::i18n::Lang::En => format!("Move {n} items"),
                }
            };
            let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("file_drag_preview")));
            let font = egui::FontId::proportional(12.0);
            let galley = painter.layout_no_wrap(text, font, egui::Color32::WHITE);
            let pad = egui::vec2(8.0, 4.0);
            let rect = egui::Rect::from_min_size(pos + egui::vec2(14.0, 8.0), galley.size() + pad * 2.0);
            painter.rect_filled(rect, 6.0, Palette::ACCENT.gamma_multiply(0.95));
            painter.galley(rect.min + pad, galley, egui::Color32::WHITE);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    }

    // 应用表头列宽拖拽；松手时写入配置（重启恢复）
    if let Some((i, dx)) = col_drag {
        state.col_w[i] = (state.col_w[i] + dx).clamp(40.0, 800.0);
    }
    if col_drag_done {
        crate::store::save_file_cols(&state.col_w);
    }

    // 表头点击排序：同列切升/降；换列时按列性质选默认方向——
    // 大小/修改时间首次点击用降序（先看最大/最新），名称用升序（A→Z）。
    if let Some(k) = sort_click {
        if state.sort_key == k {
            state.sort_desc = !state.sort_desc;
        } else {
            state.sort_key = k;
            state.sort_desc = matches!(k, SortKey::Size | SortKey::Mtime);
        }
        state.selected.clear();
        state.anchor = None;
    }

    // 背景右键新建
    if bg_new_dir {
        state.dialog = Some(Dialog::NewDir { name: String::new() });
    }
    if bg_new_file {
        state.dialog = Some(Dialog::NewFile { name: String::new() });
    }
    if bg_upload {
        state.dialog = Some(Dialog::Upload { local: String::new() });
    }
    if bg_cd {
        actions.push(FileAction::CdTerminal(state.cwd.clone()));
    }
    // 点击列表空白处（非任何行）：若有选中则全部取消选中
    if bg.clicked() && !state.selected.is_empty() {
        state.selected.clear();
        state.anchor = None;
        state.pending_rename = None;
    }

    // 延时重命名触发：单击后 0.4s 内无双击则进入重命名
    if let Some((i, t)) = state.pending_rename {
        if now - t > 0.40 {
            if let Some(e) = entries.get(i) {
                state.renaming = Some(Renaming { idx: i, buf: e.name.clone(), init: true });
            }
            state.pending_rename = None;
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(450));
        }
    }
    // 提交 / 取消重命名
    if let Some((from, to)) = rename_commit.take() {
        if from != to {
            actions.push(FileAction::Rename { from, to });
        }
        state.renaming = None;
    } else if cancel_rename {
        state.renaming = None;
    }
    // 双击断链：目标不存在，提示用户（不进入、不打开）
    if broken_link {
        actions.push(FileAction::Status(
            crate::i18n::tr("断链：目标不存在", "Broken link: target missing").into(),
        ));
    }
    // 双击打开文本文件
    if let Some(p) = open_file {
        actions.push(FileAction::OpenFile { path: p, force: false });
    }
    // 双击打开图片 / PDF / Word
    if let Some(p) = open_image {
        actions.push(FileAction::OpenImage { path: p });
    }
    if let Some(p) = open_pdf {
        actions.push(FileAction::OpenPdf { path: p });
    }
    if let Some(p) = open_docx {
        actions.push(FileAction::OpenDocx { path: p });
    }
    if let Some((p, size)) = confirm_open {
        state.dialog = Some(Dialog::ConfirmOpenLarge { path: p, size });
    }
    if let Some((p, size)) = confirm_text {
        state.dialog = Some(Dialog::ConfirmOpenAsText { path: p, size });
    }

    // 处理选择（单选 / Ctrl 多选 / Shift 区间）
    for i in clicks {
        if mod_shift {
            if let Some(a) = state.anchor {
                let (lo, hi) = (a.min(i), a.max(i));
                if !mod_ctrl {
                    state.selected.clear();
                }
                for k in lo..=hi {
                    state.selected.insert(k);
                }
            } else {
                state.selected.insert(i);
                state.anchor = Some(i);
            }
        } else if mod_ctrl {
            if !state.selected.remove(&i) {
                state.selected.insert(i);
            }
            state.anchor = Some(i);
        } else {
            state.selected.clear();
            state.selected.insert(i);
            state.anchor = Some(i);
        }
    }

    // 右键命中不在选区的行 -> 改为只选它（让批量操作对象明确）
    if let Some(i) = rclick {
        if !state.selected.contains(&i) {
            state.selected.clear();
            state.selected.insert(i);
            state.anchor = Some(i);
        }
    }

    // 处理右键菜单结果：下载/复制立即成动作，改权限/重命名/删除打开对话框
    for m in menu {
        match m {
            Menu::Download(idx) => {
                // 多选时批量下载（文件与文件夹均可，文件夹递归下载）
                let targets: Vec<usize> = if state.selected.contains(&idx) && state.selected.len() > 1 {
                    let mut v: Vec<usize> = state.selected.iter().copied().collect();
                    v.sort_unstable();
                    v
                } else {
                    vec![idx]
                };
                for k in targets {
                    if let Some(e) = entries.get(k) {
                        actions.push(FileAction::Download(join_path(&cwd, &e.name)));
                    }
                }
            }
            Menu::CopyPath(p) => actions.push(FileAction::CopyPath(p)),
            Menu::Chmod { path, mode, name } => {
                state.dialog = Some(Dialog::Chmod { path, mode, name });
            }
            Menu::Rename { path, name } => {
                state.dialog = Some(Dialog::Rename { path, name });
            }
            Menu::Delete(idx) => {
                // 多选时批量删除（含文件夹，递归删除）；否则只删右键的那一项
                let targets: Vec<usize> = if state.selected.contains(&idx) && state.selected.len() > 1 {
                    let mut v: Vec<usize> = state.selected.iter().copied().collect();
                    v.sort_unstable();
                    v
                } else {
                    vec![idx]
                };
                let items: Vec<(String, bool, String)> = targets
                    .iter()
                    .filter_map(|&k| entries.get(k).map(|e| (join_path(&cwd, &e.name), e.is_dir, e.name.clone())))
                    .collect();
                if !items.is_empty() {
                    state.dialog = Some(Dialog::ConfirmDelete { items });
                }
            }
            Menu::Copy(idx) => {
                let items = clip_targets(state, &entries, &cwd, idx);
                if !items.is_empty() {
                    actions.push(FileAction::ClipCopy { items });
                }
            }
            Menu::Cut(idx) => {
                let items = clip_targets(state, &entries, &cwd, idx);
                if !items.is_empty() {
                    actions.push(FileAction::ClipCut { items });
                }
            }
            Menu::Paste => paste_here = true,
            Menu::NewDir => state.dialog = Some(Dialog::NewDir { name: String::new() }),
            Menu::NewFile => state.dialog = Some(Dialog::NewFile { name: String::new() }),
            Menu::CdHere(p) => actions.push(FileAction::CdTerminal(p)),
            Menu::Favorite(p) => toggle_favorite(state, p),
        }
    }

    // 粘贴：工具栏按钮 / 右键菜单触发；具体同机或跨机、是否确认由 App 决定
    if paste_here && has_clip {
        actions.push(FileAction::Paste { dest_dir: cwd.clone() });
    }

    // Ctrl/Cmd+A 全选当前列表（含已过滤后的所有行）。文本框聚焦/重命名/对话框时不抢，
    // 让 Ctrl+A 仍作用于文本框自身的全选。
    let select_all = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::A));
    if select_all
        && state.renaming.is_none()
        && state.path_edit.is_none()
        && state.dialog.is_none()
        && ui.ctx().memory(|m| m.focused().is_none())
        && !entries.is_empty()
    {
        state.selected = (0..entries.len()).collect();
        state.anchor = Some(0);
    }

    // 批量删除：工具栏删除按钮 或 Delete 键（重命名/路径编辑/已有对话框时不触发）
    let key_del = ui.input(|i| i.key_pressed(egui::Key::Delete))
        && state.renaming.is_none()
        && state.path_edit.is_none();
    if key_del && state.dialog.is_none() && !state.selected.is_empty() {
        let mut idxs: Vec<usize> = state.selected.iter().copied().collect();
        idxs.sort_unstable();
        let items: Vec<(String, bool, String)> = idxs
            .iter()
            .filter_map(|&k| entries.get(k).map(|e| (join_path(&cwd, &e.name), e.is_dir, e.name.clone())))
            .collect();
        if !items.is_empty() {
            state.dialog = Some(Dialog::ConfirmDelete { items });
        }
    }

    // 兜底：拖放释放在列表空白处（无任何行接收）-> 移入当前目录。
    // 必须在表格渲染之后再取载荷，否则 bg 会抢在文件夹行之前把载荷取走。
    if drop_move.is_none() {
        if let Some(payload) = bg.dnd_release_payload::<DragPaths>() {
            let srcs = valid_move_srcs(&payload.0, &cwd);
            if !srcs.is_empty() {
                drop_move = Some((srcs, cwd.clone()));
            }
        }
    }
    // 「上级目录」按钮上的释放（几何上独立，已在工具栏阶段取走载荷）兜底并入
    if drop_move.is_none() {
        drop_move = up_move.take();
    }

    // 拖拽移动：释放到某文件夹后发起远端 mv，并记录撤销。
    if let Some((srcs, dest_dir)) = drop_move {
        // 移入「非当前目录」的文件夹：乐观地从当前列表移除被移动项，呈现「移走」效果，
        // 且不整目录刷新（刷新会跳一下）；目标目录由 worker 的 OpDone 后台刷新（不可见，无跳动）。
        if dest_dir != cwd {
            if let Some(list) = state.listings.get_mut(&cwd) {
                let moved: std::collections::HashSet<String> = srcs.iter().cloned().collect();
                list.retain(|e| !moved.contains(&join_path(&cwd, &e.name)));
            }
        }
        state.record_move(srcs.clone(), dest_dir.clone());
        actions.push(FileAction::Move { srcs, dest_dir });
        state.selected.clear();
        state.anchor = None;
    }

    // Ctrl+Z 撤销最近一次拖拽移动：把目标目录里的项移回原父目录，并在状态栏提示撤销了什么。
    // 仅在无文本框聚焦 / 无对话框 / 非重命名时触发，避免抢占输入框自身的撤销。
    let undo_key = ui.input(|i| {
        i.key_pressed(egui::Key::Z) && (i.modifiers.command || i.modifiers.ctrl) && !i.modifiers.shift
    });
    if undo_key
        && state.renaming.is_none()
        && state.path_edit.is_none()
        && state.dialog.is_none()
        && ui.ctx().memory(|m| m.focused().is_none())
    {
        // 不直接执行：弹确认框，明确告知撤销的是「哪些文件、从哪移回哪」，用户确认后才执行。
        // 仅取栈顶预览（不出栈），真正出栈与反向 mv 在用户点「撤销」后于 dialogs() 中进行。
        if let Some(rec) = state.move_undo.last() {
            let orig_parent = parent_of(&rec.original[0]);
            let names: Vec<&str> = rec.original.iter().map(|o| basename(o)).collect();
            let what = if names.len() == 1 {
                names[0].to_string()
            } else {
                let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join("、");
                let more = if names.len() > 3 {
                    match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!(" 等 {} 项", names.len()),
                        crate::i18n::Lang::En => format!(" +{} more", names.len() - 3),
                    }
                } else {
                    String::new()
                };
                format!("{shown}{more}")
            };
            state.dialog = Some(Dialog::ConfirmUndoMove { what, from: rec.dest_dir.clone(), to: orig_parent });
        } else {
            // 撤销栈为空：明确告知用户没有可撤销的移动
            actions.push(FileAction::Status(crate::i18n::tr("没有可撤销的移动", "Nothing to undo").into()));
        }
    }

    // 弹簧式拖拽导航：在 Up / 文件夹上持续悬停则进入目标目录，并在指针处双闪
    spring_navigate(ui, state, spring_target, actions);

    if let Some(p) = navigate {
        state.cwd = p;
        state.selected.clear();
    }
}

/// 右键菜单产生的请求（在表格遍历后统一处理，避免在遍历中借用 state）。
enum Menu {
    Download(usize),
    CopyPath(String),
    Chmod { path: String, mode: u32, name: String },
    Rename { path: String, name: String },
    Delete(usize),
    /// 复制 / 剪切右键项（含多选）到剪贴板
    Copy(usize),
    Cut(usize),
    /// 粘贴剪贴板内容到当前目录
    Paste,
    NewDir,
    NewFile,
    CdHere(String),
    /// 收藏该文件夹路径
    Favorite(String),
}

/// 条目右键菜单：把用户选择记录到 `menu`。
fn entry_context(resp: &egui::Response, e: &FileEntry, idx: usize, full: &str, has_clip: bool, is_fav: bool, menu: &mut Vec<Menu>) {
    use egui_phosphor::regular as icon;
    resp.context_menu(|ui| {
        ui.set_min_width(178.0); // 菜单宽度（较前一版 210 收窄约 15%）
        if ui.button(format!("{}  {}", icon::FOLDER_PLUS, crate::i18n::tr("新建文件夹", "New folder"))).clicked() {
            menu.push(Menu::NewDir);
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::FILE_PLUS, crate::i18n::tr("新建文件", "New file"))).clicked() {
            menu.push(Menu::NewFile);
            ui.close();
        }
        ui.separator();
        let dl_label = if e.is_dir { crate::i18n::tr("下载文件夹", "Download folder") } else { crate::i18n::tr("下载", "Download") };
        if ui.button(format!("{}  {}", icon::DOWNLOAD_SIMPLE, dl_label)).clicked() {
            menu.push(Menu::Download(idx));
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::COPY, crate::i18n::tr("复制路径", "Copy path"))).clicked() {
            menu.push(Menu::CopyPath(full.to_string()));
            ui.close();
        }
        ui.separator();
        // 复制 / 剪切 到剪贴板（含多选）；粘贴到当前目录
        if ui.button(format!("{}  {}", icon::COPY_SIMPLE, crate::i18n::tr("复制", "Copy"))).clicked() {
            menu.push(Menu::Copy(idx));
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::SCISSORS, crate::i18n::tr("剪切", "Cut"))).clicked() {
            menu.push(Menu::Cut(idx));
            ui.close();
        }
        if has_clip && ui.button(format!("{}  {}", icon::CLIPBOARD_TEXT, crate::i18n::tr("粘贴到此目录", "Paste here"))).clicked() {
            menu.push(Menu::Paste);
            ui.close();
        }
        ui.separator();
        if e.is_dir {
            let lbl = if is_fav {
                format!("★  {}", crate::i18n::tr("取消收藏该文件夹", "Remove bookmark"))
            } else {
                format!("☆  {}", crate::i18n::tr("收藏该文件夹", "Bookmark folder"))
            };
            if ui.button(lbl).clicked() {
                menu.push(Menu::Favorite(full.to_string()));
                ui.close();
            }
        }
        if e.is_dir && ui.button(format!("{}  {}", icon::TERMINAL_WINDOW, crate::i18n::tr("在终端打开此目录", "Open in terminal"))).clicked() {
            menu.push(Menu::CdHere(full.to_string()));
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::LOCK_KEY, crate::i18n::tr("改权限", "Chmod"))).clicked() {
            menu.push(Menu::Chmod { path: full.to_string(), mode: e.perm, name: e.name.clone() });
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::PENCIL_SIMPLE, crate::i18n::tr("重命名", "Rename"))).clicked() {
            menu.push(Menu::Rename { path: full.to_string(), name: e.name.clone() });
            ui.close();
        }
        ui.separator();
        if ui.button(RichText::new(format!("{}  {}", icon::TRASH, crate::i18n::tr("删除", "Delete"))).color(Palette::DANGER)).clicked() {
            menu.push(Menu::Delete(idx));
            ui.close();
        }
    });
}

/// 拖拽载荷：被拖动的源绝对路径列表（egui 内部 Arc 持有，需 Send + Sync）。
#[derive(Clone)]
struct DragPaths(Vec<String>);

/// 计算拖拽源：与 clip_targets 同规则，仅取路径。
fn drag_source_paths(state: &FilePanelState, entries: &[FileEntry], cwd: &str, idx: usize) -> Vec<String> {
    clip_targets(state, entries, cwd, idx).into_iter().map(|(p, _)| p).collect()
}

/// 把拖拽到目标目录 dest 的源过滤为合法移动：排除目标自身、已直接位于 dest 内、
/// 以及把祖先目录拖进其子目录的非法情况。
fn valid_move_srcs(srcs: &[String], dest: &str) -> Vec<String> {
    srcs.iter()
        .filter(|s| {
            let s = s.as_str();
            s != dest                          // 不能移动到自身
                && parent_of(s) != dest        // 已在目标目录内，无需移动
                && !dest.starts_with(&format!("{s}/")) // 不能把父目录拖进自己的子目录
        })
        .cloned()
        .collect()
}

/// 拖拽到「上级目录」按钮：悬停时高亮并把上一层目录登记为弹簧目标（计时与跳转由
/// `spring_navigate` 统一处理）；在按钮上释放则把文件移动到上一层目录。
fn handle_up_drag(ui: &mut egui::Ui, state: &FilePanelState, up: &egui::Response, up_move: &mut Option<(Vec<String>, String)>, spring_target: &mut Option<String>) {
    if state.cwd.is_empty() || state.cwd == "/" {
        return;
    }
    let parent = parent_of(&state.cwd);
    if let Some(payload) = up.dnd_release_payload::<DragPaths>() {
        let srcs = valid_move_srcs(&payload.0, &parent);
        if !srcs.is_empty() {
            // 统一交由 file_list 的落点处理（乐观移除 + 记录撤销 + 发起 mv）
            *up_move = Some((srcs, parent.clone()));
        }
    } else if up.dnd_hover_payload::<DragPaths>().is_some() {
        // 悬停高亮（无进度条；是否跳转由停留时长决定）
        let rect = up.rect;
        let p = ui.painter();
        p.rect_filled(rect, 6.0, Palette::ACCENT_SOFT);
        p.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, Palette::ACCENT.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        *spring_target = Some(parent);
    }
}

/// 弹簧式拖拽导航的统一处理：在某目标目录上持续悬停 `UP_DWELL` 秒则进入它，
/// 并在指针处播放两次脉冲环动画；继续悬停可逐级连跳。无悬停目标时复位计时。
fn spring_navigate(ui: &mut egui::Ui, state: &mut FilePanelState, spring_target: Option<String>, actions: &mut Vec<FileAction>) {
    let now = ui.input(|i| i.time);
    if let Some(target) = spring_target {
        let armed = matches!(&state.spring_since, Some((k, _)) if *k == target);
        if !armed {
            state.spring_since = Some((target.clone(), now));
        }
        let since = state.spring_since.as_ref().map(|(_, t)| *t).unwrap_or(now);
        if now - since >= UP_DWELL {
            state.cwd = target.clone();
            state.selected.clear();
            state.spring_since = None; // 重新计时，继续悬停可连跳
            state.spring_flash = Some(now);
            if !state.listings.contains_key(&target) && !state.loading.contains(&target) {
                state.loading.insert(target.clone());
                actions.push(FileAction::List(target));
            }
        }
        ui.ctx().request_repaint();
    } else {
        state.spring_since = None;
    }

    // 跳转动画：在指针处播放两次脉冲环
    if let Some(t) = state.spring_flash {
        let e = now - t;
        if e < UP_FLASH {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let phase = (e / UP_FLASH) as f32; // 0..1
                let f = (phase * 2.0).fract(); // 两个脉冲
                let k = 1.0 - (2.0 * f - 1.0).abs(); // 每个脉冲 0→1→0
                let r = 9.0 + 13.0 * (1.0 - k); // 环随脉冲收放
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("spring_flash")));
                painter.circle_stroke(pos, r, egui::Stroke::new(2.0, Palette::ACCENT.gamma_multiply(k.max(0.1))));
            }
            ui.ctx().request_repaint();
        } else {
            state.spring_flash = None;
        }
    }
}

/// 计算放入剪贴板的源项：右键项在多选内则取整组选中，否则只取该项。
/// 返回 (绝对路径, 是否目录) 列表。
fn clip_targets(state: &FilePanelState, entries: &[FileEntry], cwd: &str, idx: usize) -> Vec<(String, bool)> {
    let targets: Vec<usize> = if state.selected.contains(&idx) && state.selected.len() > 1 {
        let mut v: Vec<usize> = state.selected.iter().copied().collect();
        v.sort_unstable();
        v
    } else {
        vec![idx]
    };
    targets
        .iter()
        .filter_map(|&k| entries.get(k).map(|e| (join_path(cwd, &e.name), e.is_dir)))
        .collect()
}

/// 工具栏图标按钮（扁平无边框，悬停高亮）。
fn tool_btn(ui: &mut egui::Ui, icon: &str, tip: &str) -> bool {
    tool_btn_color(ui, icon, tip, Palette::TEXT)
}

fn tool_btn_color(ui: &mut egui::Ui, icon: &str, tip: &str, color: egui::Color32) -> bool {
    tool_btn_resp(ui, icon, tip, color).clicked()
}

/// 路径过长时仅显示尾部字符（前缀省略号）；用于收藏列表显示。
fn trailing_path(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        p.to_string()
    } else {
        let tail: String = p.chars().skip(n - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}

/// 收藏/取消收藏切换并持久化到该服务器的收藏表。
fn toggle_favorite(state: &mut FilePanelState, path: String) {
    if path.is_empty() {
        return;
    }
    if let Some(i) = state.favorites.iter().position(|f| f == &path) {
        state.favorites.remove(i);
    } else {
        state.favorites.push(path);
    }
    crate::store::save_favorites(&state.server_key, &state.favorites);
}

/// 同 `tool_btn_color`，但返回 `Response`（用于需要命中检测/拖拽目标的按钮，如「上级目录」）。
fn tool_btn_resp(ui: &mut egui::Ui, icon: &str, tip: &str, color: egui::Color32) -> egui::Response {
    let mut resp = None;
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        resp = Some(
            ui.add(
                egui::Button::new(RichText::new(icon).size(16.0).color(color))
                    .min_size(egui::vec2(30.0, 26.0))
                    .corner_radius(6.0),
            )
            .on_hover_text(tip),
        );
    });
    resp.unwrap()
}

/// 主名长度（后缀前），用于重命名时默认选中主名。
fn stem_char_len(name: &str) -> usize {
    match name.rsplit_once('.') {
        // 隐藏文件如 ".bashrc"（点在开头）不算后缀
        Some((stem, _)) if !stem.is_empty() => stem.chars().count(),
        _ => name.chars().count(),
    }
}

/// 根据扩展名选择文件类型图标。
fn file_icon(e: &FileEntry) -> &'static str {
    use egui_phosphor::regular as i;
    if e.is_dir {
        return i::FOLDER;
    }
    if e.is_link {
        return i::LINK;
    }
    let ext = e.name.rsplit_once('.').map(|(_, x)| x).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp" | "ico" => i::IMAGE,
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar" => i::FILE_ZIP,
        "pdf" => i::FILE_PDF,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => i::MUSIC_NOTE,
        "mp4" | "mkv" | "avi" | "mov" | "webm" => i::FILM_STRIP,
        "sh" | "bash" | "zsh" | "fish" => i::TERMINAL_WINDOW,
        "rs" | "c" | "cpp" | "cc" | "h" | "hpp" | "py" | "js" | "ts" | "go" | "java" | "rb"
        | "php" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "css" | "sql" => i::FILE_CODE,
        "txt" | "md" | "log" | "conf" | "cfg" | "ini" | "env" => i::FILE_TEXT,
        _ => i::FILE,
    }
}

/// 渲染所有对话框，并把确认结果转成动作。
fn dialogs(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    let ctx = ui.ctx().clone();
    let mut close = false;
    let mut clear_sel = false; // 批量删除确认后清空选中，避免残留的陈旧行索引
    let mut undo_confirmed = false; // 撤销移动确认框点了「撤销」
    let mut new_item: Option<(String, bool)> = None; // 新建目录/文件：(名称, 是否目录)，延后到借用结束再乐观插入
    let cwd = state.cwd.clone();
    // 取出对话框 IME 组字状态到本地（避免与 &mut state.dialog 借用冲突），末尾写回
    let mut ime = state.dialog_ime.take();

    if let Some(dialog) = &mut state.dialog {
        match dialog {
            Dialog::NewDir { name } => {
                modal(&ctx, crate::i18n::tr("新建目录", "New folder"), |ui| {
                    let (resp, submit) = ime_singleline(ui, "new_dir_name", name, &mut ime);
                    // 打开即自动聚焦输入框（无其它控件占焦时抓取），可直接输入
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if (dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) || submit) && !name.trim().is_empty() {
                            actions.push(FileAction::Mkdir(join_path(&cwd, name.trim())));
                            new_item = Some((name.trim().to_string(), true));
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::NewFile { name } => {
                modal(&ctx, crate::i18n::tr("新建文件", "New file"), |ui| {
                    let (resp, submit) = ime_singleline(ui, "new_file_name", name, &mut ime);
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if (dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) || submit) && !name.trim().is_empty() {
                            actions.push(FileAction::CreateFile(join_path(&cwd, name.trim())));
                            new_item = Some((name.trim().to_string(), false));
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Upload { local } => {
                modal(&ctx, crate::i18n::tr("上传", "Upload"), |ui| {
                    ui.label(RichText::new(crate::i18n::tr("本地文件/文件夹路径（也可拖拽到文件区）", "Local file/folder path (or drag onto the panel)")).size(12.0).color(Palette::TEXT_DIM));
                    let (resp, submit) = ime_singleline(ui, "upload_local_path", local, &mut ime);
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    if submit && !local.trim().is_empty() {
                        actions.push(FileAction::Upload { local: local.trim().to_string(), remote_dir: cwd.clone() });
                        close = true;
                    }
                    ui.add_space(6.0);
                    // 原生选择器：选文件（可多选）/ 选文件夹（整个上传）
                    button_row(ui, 118.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("选择文件…", "Choose files…"), 118.0, 0) {
                            if let Some(paths) = rfd::FileDialog::new().pick_files() {
                                for p in paths {
                                    actions.push(FileAction::Upload { local: p.to_string_lossy().into_owned(), remote_dir: cwd.clone() });
                                }
                                close = true;
                            }
                        }
                        if dlg_btn(ui, crate::i18n::tr("选择文件夹…", "Choose folder…"), 118.0, 0) {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                actions.push(FileAction::Upload { local: p.to_string_lossy().into_owned(), remote_dir: cwd.clone() });
                                close = true;
                            }
                        }
                    });
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("上传", "Upload"), 72.0, 2) && !local.trim().is_empty() {
                            actions.push(FileAction::Upload { local: local.trim().to_string(), remote_dir: cwd.clone() });
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Chmod { path, mode, name } => {
                modal(&ctx, crate::i18n::tr("修改权限", "Chmod"), |ui| {
                    ui.vertical_centered(|ui| ui.label(RichText::new(name.as_str()).strong()));
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| chmod_grid(ui, mode));
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("八进制：{:03o}", *mode & 0o777), crate::i18n::Lang::En => format!("Octal: {:03o}", *mode & 0o777) }).monospace().color(Palette::TEXT_DIM)));
                    ui.add_space(10.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("应用", "Apply"), 72.0, 2) {
                            actions.push(FileAction::Chmod { path: path.clone(), mode: *mode & 0o777 });
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Rename { path, name } => {
                modal(&ctx, crate::i18n::tr("重命名", "Rename"), |ui| {
                    let resp = ui.text_edit_singleline(name);
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if (dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) || submit) && !name.trim().is_empty() {
                            let parent = parent_of(path);
                            actions.push(FileAction::Rename { from: path.clone(), to: join_path(&parent, name.trim()) });
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmDelete { items } => {
                modal(&ctx, crate::i18n::tr("确认删除", "Confirm delete"), |ui| {
                    let n = items.len();
                    if n == 1 {
                        let name = &items[0].2;
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("确定删除 {name} 吗？此操作不可恢复。"),
                            crate::i18n::Lang::En => format!("Delete {name}? This cannot be undone."),
                        });
                    } else {
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("确定删除选中的 {n} 项吗？此操作不可恢复。"),
                            crate::i18n::Lang::En => format!("Delete {n} selected items? This cannot be undone."),
                        });
                        // 列出待删名称（最多 4 个，多余用 … 概括），避免文件多时对话框过长
                        ui.add_space(4.0);
                        let shown: Vec<String> = items.iter().take(4).map(|(_, _, nm)| nm.clone()).collect();
                        let more = if n > 4 {
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!(" … 等 {n} 项"),
                                crate::i18n::Lang::En => format!(" … ({n} total)"),
                            }
                        } else {
                            String::new()
                        };
                        ui.label(RichText::new(format!("{}{}", shown.join("、"), more)).color(Palette::TEXT_DIM).size(11.0));
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("删除", "Delete"), 72.0, 1) {
                            // 一条批量删除（单 rm，单通道）——避免多文件时并发开过多 SSH 通道被拒
                            let paths: Vec<String> = items.iter().map(|(p, _, _)| p.clone()).collect();
                            actions.push(FileAction::DeleteMany(paths));
                            clear_sel = true;
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmOpenLarge { path, size } => {
                modal(&ctx, crate::i18n::tr("打开大文件", "Open large file"), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("文件较大（{}），仍要打开吗？", fmt_bytes(*size as f64)), crate::i18n::Lang::En => format!("Large file ({}). Open anyway?", fmt_bytes(*size as f64)) });
                        ui.label(RichText::new(crate::i18n::tr("将以虚拟化编辑器打开（仅渲染可见行，内存占用低）", "Opens in the virtualized editor (renders only visible lines)")).color(Palette::TEXT_DIM).size(11.0));
                    });
                    ui.add_space(10.0);
                    // 按钮水平居中
                    ui.horizontal(|ui| {
                        let bw = 80.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("打开", "Open")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            actions.push(FileAction::OpenFile { path: path.clone(), force: true });
                            close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmOpenAsText { path, size } => {
                modal(&ctx, crate::i18n::tr("用文本编辑器打开？", "Open as text?"), |ui| {
                    ui.vertical_centered(|ui| {
                        let fname = path.rsplit('/').next().unwrap_or(path);
                        ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("「{}」不是常见文本/代码类型。", fname), crate::i18n::Lang::En => format!("\"{}\" is not a known text type.", fname) });
                        ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("仍用文本编辑器打开吗？（{}，二进制内容会显示为乱码）", fmt_bytes(*size as f64)), crate::i18n::Lang::En => format!("Open with the text editor anyway? ({}, binary will look garbled)", fmt_bytes(*size as f64)) }).color(Palette::TEXT_DIM).size(11.0));
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let bw = 100.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("文本打开", "Open as text")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            actions.push(FileAction::OpenFile { path: path.clone(), force: true });
                            close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmUndoMove { what, from, to } => {
                modal(&ctx, crate::i18n::tr("撤销移动", "Undo move"), |ui| {
                    ui.label(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("确定撤销移动 {what} 吗？"),
                        crate::i18n::Lang::En => format!("Undo moving {what}?"),
                    });
                    ui.add_space(4.0);
                    // 明确「从哪移回哪」，避免误撤销
                    ui.label(RichText::new(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("从 {from}\n移回 {to}"),
                        crate::i18n::Lang::En => format!("from {from}\nback to {to}"),
                    }).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("撤销", "Undo"), 72.0, 2) {
                            undo_confirmed = true;
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
        }
    }
    // 统一：任意弹框按 Esc 取消
    if state.dialog.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }
    if close {
        state.dialog = None;
    }
    // 写回 IME 组字状态（对话框已关则清空，避免下次打开残留旧组字范围）
    state.dialog_ime = if close { None } else { ime };
    // 乐观插入新建的目录/文件（借用已结束）：即时出现在列表中并选中，无需等整目录刷新。
    if let Some((name, is_dir)) = new_item {
        state.insert_new(&cwd, &name, is_dir);
    }
    if clear_sel {
        state.selected.clear();
    }
    // 用户确认撤销：此刻才出栈并执行反向 mv（与确认框展示的栈顶记录一致）
    if undo_confirmed {
        if let Some(rec) = state.move_undo.pop() {
            let orig_parent = parent_of(&rec.original[0]);
            let new_srcs: Vec<String> = rec.original.iter().map(|o| join_path(&rec.dest_dir, basename(o))).collect();
            // 把这些项从「目标目录」缓存移除，否则该目录/树节点仍显示已移回的文件（看着像没删）
            {
                let moved: std::collections::HashSet<String> = new_srcs.iter().cloned().collect();
                if let Some(list) = state.listings.get_mut(&rec.dest_dir) {
                    list.retain(|e| !moved.contains(&join_path(&rec.dest_dir, &e.name)));
                }
            }
            let names: Vec<&str> = rec.original.iter().map(|o| basename(o)).collect();
            let what = if names.len() == 1 {
                names[0].to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("{} 项", names.len()),
                    crate::i18n::Lang::En => format!("{} items", names.len()),
                }
            };
            let msg = match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已撤销移动：{what}（从 {} 移回 {}）", rec.dest_dir, orig_parent),
                crate::i18n::Lang::En => format!("Undid move: {what} (from {} back to {})", rec.dest_dir, orig_parent),
            };
            // 先发反向 mv，再设提示——确保撤销提示覆盖 Move 自身的「移动中」提示
            actions.push(FileAction::Move { srcs: new_srcs, dest_dir: orig_parent });
            actions.push(FileAction::Status(msg));
            state.selected.clear();
            state.anchor = None;
        }
    }
}

/// 一行 `count` 个定宽按钮，水平居中（前置留白）。按钮请用 `.min_size((btn_w, 0))`。
fn button_row(ui: &mut egui::Ui, btn_w: f32, count: usize, add: impl FnOnce(&mut egui::Ui)) {
    let total = count as f32 * btn_w + count.saturating_sub(1) as f32 * ui.spacing().item_spacing.x;
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
        add(ui);
    });
}

/// 定宽对话框按钮（普通/危险/主色）。
fn dlg_btn(ui: &mut egui::Ui, label: &str, w: f32, kind: u8) -> bool {
    let txt = match kind {
        2 => RichText::new(label).color(egui::Color32::WHITE), // 主色
        1 => RichText::new(label).color(Palette::DANGER),      // 危险
        _ => RichText::new(label),
    };
    let mut b = egui::Button::new(txt).min_size(egui::vec2(w, 0.0));
    if kind == 2 {
        b = b.fill(Palette::ACCENT);
    }
    ui.add(b).clicked()
}

fn modal(ctx: &egui::Context, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            // 固定内容宽度：让按钮/内容居中（add_space 用的 available_width 才稳定）
            ui.set_width(300.0);
            add(ui);
        });
    // 注：原此处有「开着对话框就低频轮询重绘」的输入法 workaround，已移除——
    // 它同样修不了 X11/XIM 的提交延迟，却让对话框打开期间持续重绘。egui 会在
    // 收到按键/IME 事件时反应式重绘，对话框输入正常。
}

/// 单行输入框 + 自绘 IME：绕开 egui 0.34 `TextEdit` 的 Commit 门——fcitx(X11) 只发
/// `Ime(Commit)`、不发 `Enabled`/`Preedit`，egui 的 `ime_cursor_range` 门永假导致「中文只能
/// 输一次」（同 editor.rs 的修法，见 memory `ime-secondary-window-fix`）。本函数在 TextEdit
/// 渲染前抽走并自行落地 Ime 事件，绕开坏门；同时用键盘事件可靠检测回车提交。
/// 返回 (response, 本帧是否回车提交)。`preedit` 为跨帧维护的组字字节范围。
fn ime_singleline(
    ui: &mut egui::Ui,
    id_src: &str,
    buf: &mut String,
    preedit: &mut Option<(usize, usize)>,
) -> (egui::Response, bool) {
    let id = egui::Id::new(id_src);
    let focused = ui.ctx().memory(|m| m.focused() == Some(id));
    if focused {
        // 抽取并移除本帧 Ime 事件，改由本函数写入 buf，egui TextEdit 便看不到、坏门不触发
        let ime: Vec<egui::ImeEvent> = ui.input_mut(|i| {
            let evs: Vec<egui::ImeEvent> = i
                .events
                .iter()
                .filter_map(|e| if let egui::Event::Ime(ev) = e { Some(ev.clone()) } else { None })
                .collect();
            i.events.retain(|e| !matches!(e, egui::Event::Ime(_)));
            evs
        });
        if !ime.is_empty() {
            // 载入 TextEdit 光标（字符位）→ 字节位；缺省落到末尾
            let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
            let caret_char = st
                .cursor
                .char_range()
                .map(|r| r.primary.index)
                .unwrap_or_else(|| buf.chars().count());
            let mut caret = byte_of_char(buf, caret_char);
            for ev in ime {
                match ev {
                    egui::ImeEvent::Preedit(t) => {
                        if t == "\n" || t == "\r" {
                            continue;
                        }
                        // 组字为临时预览：替换上一段 preedit 范围
                        let (s, e) = preedit.take().unwrap_or((caret, caret));
                        let (s, e) = (s.min(buf.len()), e.min(buf.len()));
                        buf.replace_range(s..e, &t);
                        caret = s + t.len();
                        *preedit = if t.is_empty() { None } else { Some((s, caret)) };
                    }
                    egui::ImeEvent::Commit(t) => {
                        if t == "\n" || t == "\r" {
                            continue;
                        }
                        if let Some((s, e)) = preedit.take() {
                            let (s, e) = (s.min(buf.len()), e.min(buf.len()));
                            buf.replace_range(s..e, "");
                            caret = s;
                        }
                        let at = caret.min(buf.len());
                        buf.insert_str(at, &t);
                        caret = at + t.len();
                    }
                    egui::ImeEvent::Enabled => {}
                    egui::ImeEvent::Disabled => {
                        if let Some((s, e)) = preedit.take() {
                            let (s, e) = (s.min(buf.len()), e.min(buf.len()));
                            buf.replace_range(s..e, "");
                            caret = s;
                        }
                    }
                }
            }
            // 字节位 → 字符位写回光标，使 TextEdit 本帧按新内容/新光标渲染
            let cc = egui::text::CCursor::new(char_of_byte(buf, caret));
            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cc)));
            st.store(ui.ctx(), id);
        }
    }
    let out = egui::TextEdit::singleline(buf).id(id).desired_width(f32::INFINITY).show(ui);
    let resp = out.response.response; // TextEditOutput.response 是 AtomLayoutResponse，取其内层 Response
    // 回车提交：egui 单行不消费回车事件（`lost_focus()+key_pressed(Enter)` 官方惯用法），
    // 聚焦或本帧刚失焦时读到回车即视为提交，比单看 lost_focus 更可靠。
    let enter = (resp.has_focus() || resp.lost_focus())
        && ui.input(|i| i.key_pressed(egui::Key::Enter));
    (resp, enter)
}

/// 字符位 → 字节偏移（越界回退到串尾）。
fn byte_of_char(s: &str, ch: usize) -> usize {
    s.char_indices().map(|(b, _)| b).chain(std::iter::once(s.len())).nth(ch).unwrap_or(s.len())
}

/// 字节偏移 → 字符位（非字符边界时向下取整，避免切片 panic）。
fn char_of_byte(s: &str, b: usize) -> usize {
    let mut b = b.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    s[..b].chars().count()
}

/// rwx 九宫格复选框，直接修改 mode。
fn chmod_grid(ui: &mut egui::Ui, mode: &mut u32) {
    // 与文件列表类似的表格风格：表头加粗弱化色 + 斑马纹行，列宽统一便于对齐
    egui::Grid::new("chmod_grid")
        .num_columns(4)
        .striped(true)
        .spacing([18.0, 7.0])
        .min_col_width(46.0)
        .show(ui, |ui| {
            ui.label("");
            for t in [crate::i18n::tr("读", "R"), crate::i18n::tr("写", "W"), crate::i18n::tr("执行", "X")] {
                ui.vertical_centered(|ui| ui.label(RichText::new(t).strong().color(Palette::TEXT_DIM).size(12.0)));
            }
            ui.end_row();
            for (label, base) in [(crate::i18n::tr("所有者", "Owner"), 6u32), (crate::i18n::tr("用户组", "Group"), 3), (crate::i18n::tr("其他", "Other"), 0)] {
                ui.label(RichText::new(label).size(12.0).color(Palette::TEXT));
                for bit in [2u32, 1, 0] {
                    let shift = base + bit;
                    let mut on = *mode & (1 << shift) != 0;
                    ui.vertical_centered(|ui| {
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                *mode |= 1 << shift;
                            } else {
                                *mode &= !(1 << shift);
                            }
                        }
                    });
                }
                ui.end_row();
            }
        });
}

/// 取路径最后一段（文件/目录名）。
fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

fn parent_of(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return "/".into();
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// 规范化目录路径：去掉末尾多余 "/"；空或全为 "/" 视为根。
fn normalize_path(p: &str) -> String {
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
fn perm_string(perm: u32, is_dir: bool, is_link: bool) -> String {
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
fn fmt_mtime(secs: u64) -> String {
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
    use super::normalize_path;

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
}
