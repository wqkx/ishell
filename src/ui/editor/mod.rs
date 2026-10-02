//! 文本编辑器：状态与入口。虚拟渲染见 `virtual_`，查找见 `find`。
//! 多标签与窗口框架由 app 负责。

use egui::RichText;

use crate::theme::Palette;
use crate::ui::highlight::{self, Indent};
use crate::ui::markdown;

mod find;
mod virtual_;

use virtual_::{editable_virtual, v_cancel_preedit, v_line_of, v_recompute, v_sel_range};

pub struct Editor {
    pub path: String,
    pub content: String,
    pub language: String,
    orig: String,
    /// 「有改动」标记：由各内容/编码/行尾变更点维护（undo/redo 后全量重算），
    /// 避免 dirty() 每帧全文 memcmp（20MB 文件曾每帧比较一次）。
    dirty_flag: bool,
    /// 远端文件与 `orig` 已不一致（见 `note_remote_diverged`）。
    remote_diverged: bool,
    find: String,
    replace: String,
    show_find: bool,
    status: String,
    /// 自动探测到的缩进风格（Tab 键 / 回车续进据此）
    indent: Indent,
    /// 右键打开菜单时冻结的选区
    menu_sel: Option<(usize, usize)>,
    /// 打开查找栏时请求把焦点定位到查找输入框（一次性）
    find_focus: bool,
    /// —— VSCode 风格查找/替换选项 ——
    find_case: bool, // 区分大小写
    find_word: bool,    // 全字匹配
    find_regex: bool,   // 正则
    replace_open: bool, // 展开替换行
    /// 下载中占位：只显示文件名、不可编辑（内容到位后清除）
    loading: bool,
    /// 跳转到行（Ctrl+G）浮层
    goto_open: bool,
    goto_text: String,
    goto_focus: bool,
    /// 一次性：请求把该行号居中滚到可视区（Ctrl+G 等用，不受「已可见才不滚」条件限制）
    pending_scroll: Option<usize>,
    /// 多光标/多选（Ctrl+D 累加）：各选区字节范围(升序)；非空即多选模式，编辑作用于全部
    msel: Vec<(usize, usize)>,
    /// 虚拟编辑器自绘 IME：当前组字(预编辑)文本在 content 中的字节范围；无则 None
    vime_preedit: Option<(usize, usize)>,
    /// 组字开始时收起的多光标区间（组字期间它们对不上内容），提交/取消时放回 msel。
    ime_msel: Vec<(usize, usize)>,
    /// 自绘竖向滚动：当前首个可见「视觉行」号（我们自己维护，不经 egui 像素滚动条）。
    /// 这样竖向定位按行号，与内容像素高度彻底解耦——大文件拖到底不再有 egui 边界结算卡顿。
    vtop: usize,
    /// 滚轮/触控板的亚行像素累加器（凑够一行才移动 vtop，保持与旧版一致的整行滚动手感）
    vscroll_accum: f32,
    /// 上一帧滚动度量（横向仍用 egui；vlast_top/vlast_vis 供跟随光标判断）
    vlast_top: usize,
    vlast_vis: usize,
    /// 上一帧顶部「粘性作用域行」占掉的行数（它们盖在正文最上面几行上）
    vlast_sticky: usize,
    vlast_hoff: f32,
    vlast_vieww: f32,
    vlast_viewh: f32,
    /// 拖选到边缘时下一帧要施加的滚动增量 (水平像素, 垂直行数)
    vscroll_nudge: Option<(f32, f32)>,
    /// 原文件字符编码（保存时按此编码回写，避免破坏 GBK 等非 UTF-8 文件）
    encoding: String,
    /// 打开/上次保存时的编码（编码切换计入 dirty）
    orig_encoding: String,
    /// 原文件行尾风格（内部统一 LF，保存时还原）
    eol: crate::proto::Eol,
    /// 打开/上次保存时的行尾（行尾切换计入 dirty）
    orig_eol: crate::proto::Eol,
    /// 打开/上次保存时的远端 mtime（外部改动检测）
    mtime: u32,
    /// 所有匹配（字节范围）缓存 + 缓存签名（变化时重算）
    find_matches: Vec<(usize, usize)>,
    find_sig: u64,
    /// —— 虚拟化可编辑器（大文件）状态 ——
    /// 光标字节偏移
    vcaret: usize,
    /// 各行起始字节偏移（缓存，编辑后重算）
    vlines: Vec<usize>,
    /// 最长行字节数（缓存，随 vlines 一起算，避免每帧全行扫描）
    vmax: usize,
    /// 自动换行（word-wrap）开关：开启时长行折行显示、无横向滚动
    wrap: bool,
    /// 编辑器字号（pt）：None 表示沿用全局等宽字号；有值时覆盖。可在底部状态栏放大/缩小，持久化。
    font_pt: Option<f32>,
    /// 内容版本号（每次 v_recompute +1，用于失效换行行数缓存）
    vver: u64,
    /// 换行缓存：vrow_pre[i] = 第 i 逻辑行之前的累计视觉行数；末元素为总视觉行数
    vrow_pre: Vec<u32>,
    /// 换行缓存对应的列宽与版本（不匹配则重算 vrow_pre）
    vrow_cols: usize,
    vrow_ver: u64,
    /// 换行缓存对应的字宽度量版本（字体换了，同样的内容折出来的行数会变）
    vrow_wepoch: u64,
    /// 长行的折段位置缓存（见 `virtual_::wrap::SegCache`）
    vseg_cache: std::cell::RefCell<virtual_::SegCache>,
    /// 上下移动时保持的目标列（字符数；None 表示用当前列）
    vgoal_col: Option<usize>,
    /// 选区锚点（Some 时 [anchor, caret] 为选区）
    vsel: Option<usize>,
    /// 虚拟编辑器撤销/重做栈（操作式，省内存）
    vundo: Vec<EditOp>,
    vredo: Vec<EditOp>,
    /// 括号 lint：不匹配括号所在的 0 基逻辑行号集合（行号标红）。按 lint_ver 缓存。
    lint_lines: std::collections::HashSet<usize>,
    /// 括号 lint：不匹配括号的字节范围（全文偏移），用于在正文里逐字符红色下划线。
    lint_ranges: Vec<std::ops::Range<usize>>,
    /// 括号 lint 概述（状态栏红字显示）；无问题为 None。
    lint_msg: Option<String>,
    /// 上次计算 lint 时的内容版本号（vver）；不一致才重算，避免逐帧 tokenize。
    lint_ver: u64,
    /// 各行行首的跨行高亮状态（docstring/块注释延续）；随 hl_ver 缓存。
    /// 每逻辑行前导缩进列宽（Tab 按 unit 列计）；-1 = 空白行。按 vver+unit 缓存，
    /// 让缩进线/粘性作用域行/折叠判定的按行探测从「切片+扫描」降为 O(1) 数组查表
    ///（否则拖动大文件时每帧的反复缩进扫描会明显卡顿）。
    leads: Vec<i32>,
    leads_ver: u64,
    leads_unit: usize,
    hl_states: Vec<highlight::LineState>,
    /// 上次计算 hl_states 时的内容版本号；u64::MAX 表示未算过。
    hl_ver: u64,
    /// 已折叠区域（按 header 行号升序，互不重叠）：(header 行, 区域末行)，
    /// 隐藏 header+1..=末行。内容一旦编辑即整体清空（行号会漂移，v1 从简）。
    folds: Vec<(usize, usize)>,
    /// 折叠状态版本（切换/重映射折叠 +1，用于失效视觉行缓存）。
    fold_ver: u64,
    /// 视觉行缓存所对应的折叠版本。
    vrow_fver: u64,
    /// 缓冲词补全弹窗：(候选词, 选中项, 触发前缀的字节长)。None = 未打开。
    complete: Option<(Vec<String>, usize, usize)>,
    /// 缓冲词表（去重排序）+ 其内容版本（编辑后按需重建）。
    words: Vec<String>,
    words_ver: u64,
    /// 主光标屏幕坐标（渲染循环记录，供补全弹窗定位到光标下方）。
    caret_px: Option<egui::Pos2>,
    /// 光标闪烁相位起点（秒）：移动/输入时重置，使光标立即可见。
    caret_blink_at: f64,
    /// 跟随模式（tail -f）：追加远端新增内容并滚到底；开启期间常规修改输入被忽略。
    pub follow: bool,
    /// 状态栏「跟随」按钮被点击（app 层消费：切换跟随并发送初始化命令）。
    pub follow_req: bool,
    /// 大文件默认只读（整文件仍在内存；可点状态栏「改为可编辑」解除）。
    pub readonly: bool,
    /// 状态栏「改为可编辑」被点击（一次性，app/editor 层消费后清零）。
    pub unlock_req: bool,
    /// 状态栏「按编码重新打开」选了某个编码（一次性，app 层消费：重新读取文件）。
    pub reopen_req: Option<String>,
    /// 占位（loading）状态下的自定义文案（None = 「下载中 …」）。
    pub loading_note: Option<String>,
    /// Markdown 预览：开启时以渲染视图替代源码编辑区（仅 Markdown 文件可开）。
    preview: bool,
    /// 预览的解析结果与渲染缓存（按内容版本 vver 失效）。
    md: crate::ui::markdown::Preview,
    /// 一次性：从预览切回源码后把键盘焦点还给编辑区。
    refocus: bool,
}

/// 一次编辑操作：把 content[at..at+removed.len()] 由 removed 换成 inserted。
#[derive(Clone)]
pub(super) struct EditOp {
    at: usize,
    removed: String,
    inserted: String,
    /// 操作后光标位置（用于撤销/重做后定位）
    caret_after: usize,
    caret_before: usize,
}

impl Editor {
    pub fn new(path: String, content: String) -> Self {
        let language = path
            .rsplit_once('.')
            .map(|(_, e)| e.to_lowercase())
            .unwrap_or_else(|| "txt".into());
        let indent = highlight::detect_indent(&content);
        Self {
            orig: content.clone(),
            dirty_flag: false,
            remote_diverged: false,
            path,
            content,
            language,
            find: String::new(),
            replace: String::new(),
            show_find: false,
            status: String::new(),
            indent,
            menu_sel: None,
            find_focus: false,
            find_case: false,
            find_word: false,
            find_regex: false,
            replace_open: false,
            loading: false,
            goto_open: false,
            goto_text: String::new(),
            goto_focus: false,
            pending_scroll: None,
            msel: Vec::new(),
            vime_preedit: None,
            ime_msel: Vec::new(),
            vtop: 0,
            vscroll_accum: 0.0,
            vlast_top: 0,
            vlast_vis: 1,
            vlast_sticky: 0,
            vlast_hoff: 0.0,
            vlast_vieww: 0.0,
            vlast_viewh: 0.0,
            vscroll_nudge: None,
            encoding: "UTF-8".into(),
            orig_encoding: "UTF-8".into(),
            eol: crate::proto::Eol::Lf,
            orig_eol: crate::proto::Eol::Lf,
            mtime: 0,
            find_matches: Vec::new(),
            find_sig: 0,
            vcaret: 0,
            vlines: Vec::new(),
            vmax: 0,
            wrap: false,
            font_pt: crate::store::load_editor_font(),
            vver: 0,
            vrow_pre: Vec::new(),
            vrow_cols: 0,
            vrow_ver: u64::MAX,
            vrow_wepoch: u64::MAX,
            vseg_cache: Default::default(),
            vgoal_col: None,
            vsel: None,
            vundo: Vec::new(),
            vredo: Vec::new(),
            lint_lines: std::collections::HashSet::new(),
            lint_ranges: Vec::new(),
            lint_msg: None,
            lint_ver: u64::MAX,
            leads: Vec::new(),
            leads_ver: u64::MAX,
            leads_unit: 0,
            hl_states: Vec::new(),
            hl_ver: u64::MAX,
            folds: Vec::new(),
            fold_ver: 0,
            vrow_fver: u64::MAX,
            complete: None,
            words: Vec::new(),
            words_ver: u64::MAX,
            caret_px: None,
            caret_blink_at: 0.0,
            follow: false,
            follow_req: false,
            readonly: false,
            unlock_req: false,
            reopen_req: None,
            loading_note: None,
            preview: false,
            md: Default::default(),
            refocus: false,
        }
    }

    /// 是否 Markdown 文件（才提供渲染预览）。
    pub fn is_markdown(&self) -> bool {
        matches!(self.language.as_str(), "md" | "markdown" | "mdown" | "mkd")
    }
    /// 当前是否显示渲染预览。
    pub fn previewing(&self) -> bool {
        self.preview && self.is_markdown()
    }
    /// 源码 ⇄ 渲染预览切换（非 Markdown 文件无操作）。
    pub fn toggle_preview(&mut self) {
        if !self.is_markdown() {
            return;
        }
        self.preview = !self.preview;
        if self.preview {
            // 预览期间编辑区不绘制、也不处理输入：没提交的输入法组字先撤掉，
            // 否则那截拼音会留在正文里被渲染出来，Ctrl+S 还会把它存进文件。
            v_cancel_preedit(self);
        } else {
            self.refocus = true;
        }
    }

    /// 是否禁止修改内容（跟随或大文件只读）。
    pub fn is_readonly(&self) -> bool {
        self.follow || self.readonly
    }
    /// 打开查找栏（对齐 VSCode：Ctrl+F / 查找按钮只打开不关闭；Esc 才关）。
    /// 若有单行选区，将其填入查找框。
    pub fn open_find(&mut self) {
        if let Some((a, b)) = v_sel_range(self) {
            let sel = &self.content[a..b];
            if !sel.is_empty() && !sel.contains('\n') {
                self.find = sel.to_string();
                self.find_sig = 0; // 强制 rebuild_matches
            }
        }
        self.show_find = true;
        self.find_focus = true;
        // 查找栏属于源码视图：预览中发起查找即切回源码
        self.preview = false;
    }
    /// 兼容旧名：与 [`Self::open_find`] 相同（不再切换关闭）。
    pub fn toggle_find(&mut self) {
        self.open_find();
    }
    pub fn dirty(&self) -> bool {
        // 内容、编码、行尾任一与打开/上次保存时不同都算「有改动」——
        // 仅切换 GBK/UTF-8 或 LF/CRLF 也必须能保存、关闭时也要警告。
        // 标记由各变更点维护（v_apply 置位、mark_saved 复位、set_encoding/set_eol 与
        // undo/redo 全量重算），此处 O(1) 读取。
        self.dirty_flag
    }
    /// 全量重算 dirty 标记（undo/redo 可能精确回到保存点，必须重新比较）。
    pub(crate) fn recompute_dirty(&mut self) {
        self.dirty_flag = self.remote_diverged
            || self.content != self.orig
            || self.encoding != self.orig_encoding
            || self.eol != self.orig_eol;
    }
    /// 保存成功了，但落盘的不是当前内容（保存途中又有编辑/切了编码或行尾）。
    /// 此后「与打开时一致」不再等于「与远端一致」，直到下一次签名吻合的保存为止都算有改动。
    pub fn note_remote_diverged(&mut self) {
        self.remote_diverged = true;
        self.dirty_flag = true;
    }
    pub fn mark_saved(&mut self) {
        self.orig = self.content.clone();
        self.orig_encoding = self.encoding.clone();
        self.orig_eol = self.eol;
        self.remote_diverged = false;
        self.dirty_flag = false;
    }
    /// 保存修订签名 = (正文版本, 编码, 行尾)。保存确认据此判断「是否仍是当时发出去的那份」：
    /// 只有内容、编码、行尾都未变才算已保存——单独切换编码/行尾也不会被旧的成功事件误标干净。
    pub fn save_rev(&self) -> (u64, String, crate::proto::Eol) {
        (self.vver, self.encoding.clone(), self.eol)
    }
    pub fn filename(&self) -> String {
        self.path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.path)
            .to_string()
    }
    /// 当前光标所在逻辑行（0 基），供「记住光标位置」持久化。
    pub fn caret_line(&self) -> usize {
        v_line_of(self, self.vcaret)
    }
    /// 恢复上次的光标行：光标置行首并滚动到该行（行号越界则忽略）。
    pub fn restore_line(&mut self, line: usize) {
        // 行索引是懒建的（`new` 只存内容）。调用方紧跟在 `new` 之后恢复光标，不先建的话
        // 下面的越界判断恒真——这个功能就一次都不会生效。
        if self.vlines.is_empty() {
            v_recompute(self);
        }
        if line == 0 || line >= self.vlines.len() {
            return;
        }
        self.vcaret = self.vlines[line];
        self.pending_scroll = Some(line);
    }
    /// 跟随模式追加远端新增文本：不进撤销栈、orig 同步（保持“未修改”状态）。
    /// 仅当光标位于文末且无选区时才推进光标并滚到底（less +F 语义）——
    /// 用户正在拖选/查看历史时只追加不滚动，选区在尾部追加下字节偏移天然稳定。
    pub fn append_tail(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let at_end =
            self.vcaret >= self.content.len() && self.vsel.is_none() && self.msel.is_empty();
        self.content.push_str(text);
        self.orig.push_str(text);
        v_recompute(self);
        if at_end {
            self.vcaret = self.content.len();
            self.pending_scroll = Some(self.vlines.len().saturating_sub(1));
        }
    }
    /// 状态栏提示文字。
    pub fn set_status(&mut self, s: &str) {
        self.status = s.to_string();
    }
    /// 自动换行开关。
    pub fn set_wrap(&mut self, on: bool) {
        self.wrap = on;
        self.vgoal_col = None;
    }
    pub fn set_loading(&mut self, v: bool) {
        self.loading = v;
    }
    pub fn set_meta(&mut self, encoding: String, eol: crate::proto::Eol, mtime: u32) {
        self.orig_encoding = encoding.clone();
        self.encoding = encoding;
        self.orig_eol = eol;
        self.eol = eol;
        self.mtime = mtime;
    }
    pub fn encoding(&self) -> &str {
        &self.encoding
    }
    pub fn eol(&self) -> crate::proto::Eol {
        self.eol
    }
    pub fn mtime(&self) -> u32 {
        self.mtime
    }
    pub fn set_mtime(&mut self, m: u32) {
        self.mtime = m;
    }
    /// 改行尾/编码后一律走 `recompute_dirty`，不能直接置 `dirty_flag = true`。
    /// `dirty()` 比的是与**打开时**的差异，改回原值就该重新变干净；直接置位会让
    /// 「UTF-8 → GBK → 改回 UTF-8」这种来回切换留下一个抹不掉的已修改标记，
    /// 关标签时还要提示保存一个其实没动过的文件。
    pub fn set_eol(&mut self, e: crate::proto::Eol) {
        if self.eol != e {
            self.eol = e;
            self.recompute_dirty();
        }
    }
    pub fn set_encoding(&mut self, enc: String) {
        if self.encoding != enc {
            self.encoding = enc;
            self.recompute_dirty();
        }
    }
}

/// 渲染编辑器内容（工具栏 + 查找栏 + 代码区）。返回 true 表示请求保存。
/// `text_id` 为该编辑器固定的 TextEdit Id（用于关闭时清理其状态/撤销历史）。
pub fn content(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    // 下载中占位：只显示文件名、不可编辑（进度由标签栏的珊瑚色进度条体现）。
    // 文案可被覆盖（文档标签下载完转后台解析时显示「渲染中 …」）。
    if ed.loading {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(252, 252, 250)))
            .show_inside(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.label(RichText::new(ed.filename()).size(16.0).color(Palette::TEXT));
                    ui.add_space(6.0);
                    let note = ed
                        .loading_note
                        .clone()
                        .unwrap_or_else(|| crate::i18n::tr("下载中 …", "Downloading …").into());
                    ui.label(RichText::new(note).size(12.0).color(Palette::TEXT_DIM));
                });
            });
        return false;
    }

    if ed.previewing() {
        return preview(ui, ed, text_id);
    }
    if ed.refocus {
        ed.refocus = false;
        ui.memory_mut(|m| m.request_focus(text_id));
    }
    editable_virtual(ui, ed, text_id)
}

/// Markdown 渲染预览（替代源码编辑区）。返回 true 表示请求保存。
fn preview(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    // 编辑区此时不绘制，egui 会自行收回它的焦点（聚焦控件当帧没出现即失焦），无需手动让出。
    // 保存与查找快捷键原本由编辑区的输入处理负责，预览里要自己接
    //（与源码视图一致，Cmd 和 Ctrl 都认——macOS 上它们是两个键）
    let hotkey = |key| {
        ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, key) || i.consume_key(egui::Modifiers::CTRL, key)
        })
    };
    let save = hotkey(egui::Key::S);
    if hotkey(egui::Key::F) {
        ed.open_find(); // 顺带切回源码
        ui.ctx().request_repaint();
        return save;
    }
    if ed.content.len() > markdown::PREVIEW_LIMIT {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.4);
            ui.label(
                RichText::new(crate::i18n::tr(
                    "文件过大，不提供渲染预览",
                    "File too large to preview",
                ))
                .size(12.0)
                .color(Palette::TEXT_DIM),
            );
        });
        return save;
    }
    ed.md.sync(&ed.content, ed.vver);
    markdown::show(ui, &mut ed.md, text_id.with("md_preview"));
    save
}

#[cfg(test)]
mod dirty_tests {
    use super::*;

    #[test]
    fn encoding_and_eol_changes_are_dirty() {
        let mut ed = Editor::new("/tmp/a.txt".into(), "hello\n".into());
        ed.set_meta("UTF-8".into(), crate::proto::Eol::Lf, 1);
        assert!(!ed.dirty());
        // 仅切换编码 → dirty
        ed.set_encoding("GBK".into());
        assert!(ed.dirty());
        ed.mark_saved();
        assert!(!ed.dirty());
        // 仅切换行尾 → dirty
        ed.set_eol(crate::proto::Eol::Crlf);
        assert!(ed.dirty());
        ed.mark_saved();
        assert!(!ed.dirty());
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    /// 「记住光标行」：`Editor::new` 不建行索引（首帧才懒建），而恢复紧跟在 new 之后调用——
    /// 原先因此每次都被「行号越界」挡回去，这个功能一次都没生效过。
    #[test]
    fn restore_line_works_right_after_construction() {
        let mut ed = Editor::new("/tmp/a.txt".into(), "a\nb\nc\nd\n".into());
        ed.restore_line(2);
        assert_eq!(ed.caret_line(), 2);
        // 越界行号仍然忽略
        let mut ed = Editor::new("/tmp/a.txt".into(), "a\nb\n".into());
        ed.restore_line(99);
        assert_eq!(ed.caret_line(), 0);
    }

    /// 保存途中又改了内容：远端落盘的是**发出去那一刻**的内容，既不是打开时的，也不是现在的。
    /// 这时「与打开时一致」不再等于「与远端一致」——撤销回打开时的内容必须仍算有改动，
    /// 否则远端与所见不同、却没有脏标记，也存不回去。
    #[test]
    fn a_save_that_landed_stale_content_keeps_the_tab_dirty_until_resaved() {
        let mut ed = Editor::new("/tmp/a.txt".into(), "O".into());
        v_recompute(&mut ed);
        ed.set_meta("UTF-8".into(), crate::proto::Eol::Lf, 1);
        virtual_::test_insert(&mut ed, "1"); // S1：此刻发出保存
        virtual_::test_insert(&mut ed, "\n2"); // S2：保存途中继续编辑
        ed.note_remote_diverged(); // FileSaved 到达，但签名对不上
        virtual_::test_undo(&mut ed);
        virtual_::test_undo(&mut ed);
        assert_eq!(ed.content, "O");
        assert!(ed.dirty(), "远端是 S1、编辑器是 O，却被判成干净");
        ed.mark_saved();
        assert!(!ed.dirty());
    }
}

#[cfg(test)]
mod reveal_tests {
    use super::*;

    #[allow(deprecated)]
    fn frames(ed: &mut Editor, n: usize) {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        for _ in 0..n {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 300.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    content(ui, ed, egui::Id::new("reveal_probe"));
                });
            });
        }
    }

    /// 查找 / 跳转把光标放到某处后，那一处必须滚进视口——竖向和横向都要。
    /// 原先横向跟随只认「键盘移动了光标」，查找跳到长行右侧的命中时选区在屏幕外。
    #[test]
    fn a_jump_reveals_the_target_horizontally() {
        let mut ed = Editor::new("/tmp/a.txt".into(), format!("{}needle\n", "x".repeat(600)));
        frames(&mut ed, 2);
        assert_eq!(ed.vlast_hoff, 0.0);
        ed.vsel = Some(600);
        ed.vcaret = 606;
        ed.pending_scroll = Some(0); // 查找跳转就是这么设的
        frames(&mut ed, 3);
        assert!(ed.vlast_hoff > 0.0, "命中在第 600 列，视图却没有横向滚过去");
    }

    /// 非换行模式下横向跟随光标要按**真实字宽**算光标的位置。按「每个字符一列」算的话，
    /// 中文长行的行尾实际在更右边，视图滚不到那里，光标在屏幕外。
    #[test]
    fn horizontal_follow_reaches_the_end_of_a_cjk_line() {
        let line = "汉字".repeat(300);
        let mut ed = Editor::new("/tmp/a.txt".into(), format!("{line}\n"));
        frames(&mut ed, 2);
        ed.vcaret = line.len();
        ed.pending_scroll = Some(0);
        frames(&mut ed, 3);
        // 这一行真实的像素宽度（用同一套字体量）
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut true_w = 0.0;
        #[allow(deprecated)]
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let mono = egui::TextStyle::Monospace.resolve(&ctx.global_style());
            true_w = ctx
                .fonts_mut(|f| f.layout_no_wrap(line.clone(), mono, egui::Color32::BLACK))
                .size()
                .x;
        });
        assert!(
            ed.vlast_hoff + ed.vlast_vieww > true_w * 0.95,
            "行宽 {true_w:.0}px，视图只滚到 {:.0}..{:.0}",
            ed.vlast_hoff,
            ed.vlast_hoff + ed.vlast_vieww
        );
    }

    /// 换行模式下，用**字体实测的字宽**折出来的每一段都得放得进视口——这是用户看到的
    /// 那个问题本身：纯中文行每段都比窗口宽，右半截被裁掉。
    #[test]
    fn wrapped_cjk_segments_fit_inside_the_viewport() {
        let text = format!("{}\n{}\n", "汉字宽字符".repeat(60), "mixed 中英 text ".repeat(30));
        let mut ed = Editor::new("/tmp/a.txt".into(), text);
        ed.wrap = true;
        frames(&mut ed, 3);
        let cols = ed.vrow_cols;
        assert!(cols > 10 && cols < 200, "前提：换行列数来自视口宽度（{cols}）");
        for line in 0..2 {
            let widths = virtual_::test_seg_widths(&ed, line);
            assert!(widths.len() > 2, "前提：这一行折成了多段");
            for (seg, w) in widths.iter().enumerate() {
                assert!(*w <= cols as f32 + 1e-3, "第 {line} 行第 {seg} 段宽 {w} 列，视口只有 {cols} 列");
            }
        }
    }

    /// 换行模式下，跳转目标在一个折成很多段的长行深处：要滚到光标所在的**那一段**，
    /// 而不是这一行的第一段。
    #[test]
    fn a_jump_reveals_the_target_row_inside_a_wrapped_line() {
        let mut ed = Editor::new("/tmp/a.txt".into(), format!("{}needle\n", "x".repeat(20_000)));
        ed.wrap = true;
        frames(&mut ed, 2);
        ed.vcaret = 20_006;
        ed.pending_scroll = Some(0);
        frames(&mut ed, 3);
        let caret_row = virtual_::test_caret_vrow(&ed);
        assert!(
            caret_row >= ed.vtop && caret_row < ed.vtop + ed.vlast_vis.max(1),
            "光标在第 {caret_row} 视觉行，视口却停在 {}..{}",
            ed.vtop,
            ed.vtop + ed.vlast_vis
        );
    }

    /// 顶部的「粘性作用域行」盖在正文最上面几行上。键盘上移把光标带到视口顶端时，
    /// 光标所在行不能落在它们下面——那样光标和正在编辑的那行都看不见。
    #[test]
    fn the_caret_row_is_never_hidden_under_the_sticky_scope_rows() {
        let body: String = (0..300)
            .map(|i| format!("        if c{i} {{ work{i}(); }}\n"))
            .collect();
        let mut ed = Editor::new(
            "/tmp/a.rs".into(),
            format!("mod m {{\n    fn f() {{\n{body}    }}\n}}\n"),
        );
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let id = egui::Id::new("sticky_probe");
        let frame = |ed: &mut Editor, up: bool| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 300.0),
                )),
                events: if up {
                    vec![egui::Event::Key {
                        key: egui::Key::ArrowUp,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Default::default(),
                    }]
                } else {
                    vec![]
                },
                ..Default::default()
            };
            #[allow(deprecated)]
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    content(ui, ed, id);
                });
            });
        };
        frame(&mut ed, false);
        virtual_::test_goto_line(&mut ed, 150);
        frame(&mut ed, false);
        ctx.memory_mut(|m| m.request_focus(id));
        frame(&mut ed, false);
        let mut seen_sticky = false;
        for _ in 0..40 {
            frame(&mut ed, true);
            frame(&mut ed, false); // 让粘性行数按新的滚动位置结算
            let row = virtual_::test_caret_vrow(&ed);
            seen_sticky |= ed.vlast_sticky > 0;
            assert!(
                row >= ed.vtop + ed.vlast_sticky,
                "光标在视觉行 {row}，视口从 {} 开始，最上面 {} 行被粘性作用域行盖住",
                ed.vtop,
                ed.vlast_sticky
            );
        }
        assert!(seen_sticky, "前提：这段代码里确实出现了粘性作用域行");
    }

    /// 跳转目标落在折叠区里：必须先展开再定位，否则居中的是折叠着的那一行。
    ///
    /// **钉子不是门禁**：真实缺陷是帧内顺序（展开检查排在跳转之前），而这里只能在帧外
    /// 把光标放进折叠区，修复前同样通过。留着保证「光标进折叠区即展开并可见」这条不退化。
    #[test]
    fn a_jump_into_a_folded_region_unfolds_it() {
        let body: String = (0..200).map(|i| format!("    line{i};\n")).collect();
        let mut ed = Editor::new("/tmp/a.rs".into(), format!("fn f() {{\n{body}}}\n"));
        frames(&mut ed, 2);
        ed.folds = vec![(0, 200)];
        ed.fold_ver = ed.fold_ver.wrapping_add(1);
        frames(&mut ed, 2);
        // 模拟「跳转到行」把光标放进折叠区（跳转发生在状态栏/查找栏处理里）
        virtual_::test_goto_line(&mut ed, 149);
        frames(&mut ed, 1);
        assert!(ed.folds.is_empty(), "光标进了折叠区，折叠却没展开");
        let row = virtual_::test_caret_vrow(&ed);
        assert!(row >= ed.vtop && row < ed.vtop + ed.vlast_vis.max(1), "展开后目标行不在视口内");
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    const CTRL: egui::Modifiers = egui::Modifiers {
        alt: false,
        ctrl: true,
        shift: false,
        mac_cmd: false,
        command: true,
    };

    fn md_editor() -> Editor {
        let mut ed = Editor::new("/tmp/a.md".into(), "# 标题\n\n正文 **粗**\n".into());
        v_recompute(&mut ed);
        ed
    }

    /// 跑一帧真实的 `content()`，返回它的「请求保存」结果。
    #[allow(deprecated)]
    fn frame(ctx: &egui::Context, ed: &mut Editor, id: egui::Id, keys: &[egui::Key]) -> bool {
        frame_with(ctx, ed, id, keys, CTRL)
    }

    #[allow(deprecated)]
    fn frame_with(
        ctx: &egui::Context,
        ed: &mut Editor,
        id: egui::Id,
        keys: &[egui::Key],
        mods: egui::Modifiers,
    ) -> bool {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 400.0),
            )),
            modifiers: if keys.is_empty() { Default::default() } else { mods },
            events: keys
                .iter()
                .map(|&key| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: mods,
                })
                .collect(),
            ..Default::default()
        };
        let mut save = false;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                save = content(ui, ed, id);
            });
        });
        save
    }

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx
    }

    #[test]
    fn only_markdown_files_can_preview() {
        let mut ed = Editor::new("/tmp/a.rs".into(), "fn main() {}\n".into());
        ed.toggle_preview();
        assert!(!ed.previewing());
        let mut ed = md_editor();
        ed.toggle_preview();
        assert!(ed.previewing());
        ed.toggle_preview();
        assert!(!ed.previewing());
    }

    /// 保存快捷键原本由编辑区的输入处理负责；预览路径不经过它，必须自己接住，
    /// 否则「预览着按 Ctrl+S 没反应」。
    #[test]
    fn ctrl_s_in_preview_requests_save() {
        let (ctx, id) = (ctx(), egui::Id::new("md_probe"));
        let mut ed = md_editor();
        ed.toggle_preview();
        frame(&ctx, &mut ed, id, &[]); // 首帧：字体/样式生效
        assert!(!frame(&ctx, &mut ed, id, &[]), "没按键不该请求保存");
        assert!(frame(&ctx, &mut ed, id, &[egui::Key::S]));
        assert!(ed.previewing(), "保存不该退出预览");
    }

    /// 源码视图的快捷键认的是 `command || ctrl`，预览必须一致：macOS 上 Ctrl 与 Cmd 是
    /// 两个键（`command` 只跟 Cmd），只认 COMMAND 的话那里 Ctrl+S / Ctrl+F 在预览里没反应。
    #[test]
    fn plain_ctrl_without_command_works_in_preview_too() {
        let ctrl_only = egui::Modifiers {
            command: false,
            ..CTRL
        };
        let (ctx, id) = (ctx(), egui::Id::new("md_probe"));
        let mut ed = md_editor();
        ed.toggle_preview();
        frame(&ctx, &mut ed, id, &[]);
        assert!(frame_with(&ctx, &mut ed, id, &[egui::Key::S], ctrl_only));
        frame_with(&ctx, &mut ed, id, &[egui::Key::F], ctrl_only);
        assert!(!ed.previewing());
    }

    /// 查找栏属于源码视图：预览中按 Ctrl+F 要切回源码并打开查找，而不是毫无反应。
    #[test]
    fn ctrl_f_in_preview_returns_to_source_with_find_open() {
        let (ctx, id) = (ctx(), egui::Id::new("md_probe"));
        let mut ed = md_editor();
        ed.toggle_preview();
        frame(&ctx, &mut ed, id, &[]);
        frame(&ctx, &mut ed, id, &[egui::Key::F]);
        assert!(!ed.previewing());
        assert!(ed.show_find);
    }

    /// 切回源码后要把焦点还给编辑区，否则得先点一下才能继续打字——这是门禁（去掉
    /// `refocus` 实测会挂）。
    ///
    /// 中间那条「预览中焦点不在编辑区」是**钉子不是门禁**：这靠的是 egui 自己的行为
    /// （聚焦控件当帧没出现就失焦），我们没有为它写代码，所以它恒绿。留着是防将来
    /// egui 升级改了这个行为——那时按键/输入法会路由到看不见的编辑器。
    #[test]
    fn focus_leaves_the_hidden_editor_and_returns_with_it() {
        let (ctx, id) = (ctx(), egui::Id::new("md_probe"));
        let mut ed = md_editor();
        frame(&ctx, &mut ed, id, &[]);
        ctx.memory_mut(|m| m.request_focus(id));
        frame(&ctx, &mut ed, id, &[]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(id), "前提：编辑区已聚焦");

        ed.toggle_preview();
        frame(&ctx, &mut ed, id, &[]);
        assert_ne!(ctx.memory(|m| m.focused()), Some(id), "预览中焦点仍在编辑区");

        ed.toggle_preview();
        frame(&ctx, &mut ed, id, &[]);
        frame(&ctx, &mut ed, id, &[]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(id), "切回源码后焦点没还给编辑区");
    }

    /// 进预览时没提交的输入法组字必须撤掉：预览里没有输入处理去收尾它，
    /// 那截拼音会被渲染出来，Ctrl+S 还会把它写进文件。
    #[test]
    fn entering_preview_cancels_an_unfinished_composition() {
        let mut ed = md_editor();
        let orig = ed.content.clone();
        let (s, e) = crate::ui::ime_safe::replace_preedit(&mut ed.content, (0, 0), "zhong");
        ed.vime_preedit = Some((s, e));
        ed.vcaret = e;
        assert_ne!(ed.content, orig);
        ed.toggle_preview();
        assert_eq!(ed.content, orig);
        assert!(ed.vime_preedit.is_none());
    }
}
