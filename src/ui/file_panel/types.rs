use std::collections::{HashMap, HashSet};

use crate::proto::FileEntry;

/// 文件面板状态（每个会话一份）。
#[derive(Default)]
pub struct FilePanelState {
    /// 树根（默认 "/"）
    pub root: String,
    /// 右栏当前目录（绝对路径）
    pub cwd: String,
    /// 路径 -> 该目录的条目（树与右栏共用）
    pub listings: HashMap<String, Vec<FileEntry>>,
    /// canon 路径 -> 已应用的最大目录列举序号（gen）。同一目录可能有多个 List 请求在飞
    /// （删除后自动刷新 + 手动刷新 + 弱网超时重发），乱序返回时用它丢弃「后到的旧结果」，
    /// 避免陈旧列表覆盖较新列表（曾致刷新后新建的同名目录不显示、只能靠过滤框才看到）。
    /// 全局单调，故只增不删也安全——新请求 gen 必然更大，陈旧结果永远比不过。
    pub applied_list_gen: HashMap<String, u64>,
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
    pub pending_select: Option<(String, HashSet<String>)>,
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
    NewDir {
        name: String,
    },
    NewFile {
        name: String,
    },
    Upload {
        local: String,
    },
    Chmod {
        path: String,
        mode: u32,
        name: String,
    },
    Rename {
        path: String,
        name: String,
    },
    ConfirmDelete {
        items: Vec<(String, bool, String)>,
    }, // (path, is_dir, name)，支持多选批量删除
    ConfirmOpenLarge {
        path: String,
        size: u64,
    },
    /// 非文本后缀，确认是否仍用文本编辑器打开
    ConfirmOpenAsText {
        path: String,
        size: u64,
    },
    /// 撤销移动确认：what=文件名概述，from=当前所在目录，to=移回的原目录（栈顶记录的预览）
    ConfirmUndoMove {
        what: String,
        from: String,
        to: String,
    },
}

/// 面板交互产生的动作，由 App 翻译为 SFTP 指令或剪贴板操作。
pub enum FileAction {
    /// 请求列出目录（展开树 / 刷新 / 进入）
    List(String),
    Download(String),
    Upload {
        local: String,
        remote_dir: String,
    },
    Mkdir(String),
    CreateFile(String),
    Chmod {
        path: String,
        mode: u32,
    },
    /// 批量删除：一条 rm 处理所有路径（单通道），避免多文件时并发开太多 SSH 通道被服务端拒绝
    DeleteMany(Vec<String>),
    Rename {
        from: String,
        to: String,
    },
    /// 复制选中项到 App 级剪贴板（跨 tab 共享）；每项 (绝对路径, 是否目录)
    ClipCopy {
        items: Vec<(String, bool)>,
    },
    /// 剪切选中项到剪贴板（粘贴时为移动，会在删除前二次确认）
    ClipCut {
        items: Vec<(String, bool)>,
    },
    /// 把剪贴板内容粘贴到当前目录（同机用 cp/mv，跨机走下载→上传中转，由 App 决定）
    Paste {
        dest_dir: String,
    },
    /// 在同一会话内拖拽移动：把 srcs 移动到 dest_dir（远端 mv）
    Move {
        srcs: Vec<String>,
        dest_dir: String,
    },
    CopyPath(String),
    /// 双击文本文件 -> 打开编辑器（force=true 放宽大小限制）
    OpenFile {
        path: String,
        force: bool,
    },
    /// 双击图片文件 -> 打开看图工具
    OpenImage {
        path: String,
    },
    /// 双击 PDF -> 打开 PDF 查看器（远端 poppler 渲染）
    OpenPdf {
        path: String,
    },
    /// 双击 Word(docx) -> 打开文档查看器（本地解析）
    OpenDocx {
        path: String,
    },
    /// 在终端 cd 到该目录并聚焦终端
    CdTerminal(String),
    /// 直接设置状态栏文案（用于撤销等即时提示）
    Status(String),
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
