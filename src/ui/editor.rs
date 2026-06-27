//! 文本编辑器内容渲染：语法高亮（大文件自动关闭高亮以省内存）、查找/替换。
//! 多标签与窗口框架由 app 负责。

use egui::text::CCursor;
use egui::RichText;

use crate::theme::Palette;
use crate::ui::highlight::{self, Indent};

/// 超过该大小则跳过括号 lint（避免大文件每次编辑都整体 tokenize）。
const LINT_LIMIT: usize = 256 * 1024;

pub struct Editor {
    pub path: String,
    pub content: String,
    pub language: String,
    orig: String,
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
    find_case: bool,    // 区分大小写
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
    /// 上一帧滚动度量（用于本帧用「设置滚动偏移」方式可靠跟随光标；scroll_to_rect 在本容器不生效）
    vlast_top: usize,
    vlast_vis: usize,
    vlast_voff: f32,
    vlast_hoff: f32,
    vlast_vieww: f32,
    vlast_viewh: f32,
    /// 拖选到边缘时下一帧要施加的滚动增量 (水平, 垂直)
    vscroll_nudge: Option<(f32, f32)>,
    /// 原文件字符编码（保存时按此编码回写，避免破坏 GBK 等非 UTF-8 文件）
    encoding: String,
    /// 原文件行尾风格（内部统一 LF，保存时还原）
    eol: crate::proto::Eol,
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
    /// 内容版本号（每次 v_recompute +1，用于失效换行行数缓存）
    vver: u64,
    /// 换行缓存：vrow_pre[i] = 第 i 逻辑行之前的累计视觉行数；末元素为总视觉行数
    vrow_pre: Vec<u32>,
    /// 换行缓存对应的列宽与版本（不匹配则重算 vrow_pre）
    vrow_cols: usize,
    vrow_ver: u64,
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
}

/// 一次编辑操作：把 content[at..at+removed.len()] 由 removed 换成 inserted。
#[derive(Clone)]
struct EditOp {
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
            vlast_top: 0,
            vlast_vis: 1,
            vlast_voff: 0.0,
            vlast_hoff: 0.0,
            vlast_vieww: 0.0,
            vlast_viewh: 0.0,
            vscroll_nudge: None,
            encoding: "UTF-8".into(),
            eol: crate::proto::Eol::Lf,
            mtime: 0,
            find_matches: Vec::new(),
            find_sig: 0,
            vcaret: 0,
            vlines: Vec::new(),
            vmax: 0,
            wrap: false,
            vver: 0,
            vrow_pre: Vec::new(),
            vrow_cols: 0,
            vrow_ver: u64::MAX,
            vgoal_col: None,
            vsel: None,
            vundo: Vec::new(),
            vredo: Vec::new(),
            lint_lines: std::collections::HashSet::new(),
            lint_ranges: Vec::new(),
            lint_msg: None,
            lint_ver: u64::MAX,
        }
    }
    /// 切换查找栏（供窗口标签栏的「查找」按钮调用）；打开时请求聚焦查找框。
    pub fn toggle_find(&mut self) {
        self.show_find = !self.show_find;
        if self.show_find {
            self.find_focus = true;
        }
    }
    pub fn dirty(&self) -> bool {
        self.content != self.orig
    }
    pub fn mark_saved(&mut self) {
        self.orig = self.content.clone();
    }
    pub fn filename(&self) -> String {
        self.path.trim_end_matches('/').rsplit('/').next().unwrap_or(&self.path).to_string()
    }
    pub fn set_loading(&mut self, v: bool) {
        self.loading = v;
    }
    pub fn set_meta(&mut self, encoding: String, eol: crate::proto::Eol, mtime: u32) {
        self.encoding = encoding;
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
    pub fn set_eol(&mut self, e: crate::proto::Eol) {
        self.eol = e;
    }
    pub fn set_encoding(&mut self, enc: String) {
        self.encoding = enc;
    }
}

/// 渲染编辑器内容（工具栏 + 查找栏 + 代码区）。返回 true 表示请求保存。
/// `text_id` 为该编辑器固定的 TextEdit Id（用于关闭时清理其状态/撤销历史）。
pub fn content(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {

    // 下载中占位：只显示文件名、不可编辑（进度由标签栏的珊瑚色进度条体现）。
    if ed.loading {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(252, 252, 250)))
            .show_inside(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.4);
                    ui.label(RichText::new(ed.filename()).size(16.0).color(Palette::TEXT));
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("下载中 …", "Downloading …")).size(12.0).color(Palette::TEXT_DIM));
                });
            });
        return false;
    }

    editable_virtual(ui, ed, text_id)
}

// ———————————————————————— 虚拟化可编辑器（大文件，Phase 1） ————————————————————————

fn compute_line_starts(s: &str) -> Vec<usize> {
    let mut v = Vec::with_capacity(s.len() / 40 + 1);
    v.push(0);
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}
fn prev_char_boundary(s: &str, b: usize) -> usize {
    s[..b].chars().next_back().map(|c| b - c.len_utf8()).unwrap_or(0)
}
fn next_char_boundary(s: &str, b: usize) -> usize {
    s[b.min(s.len())..].chars().next().map(|c| b + c.len_utf8()).unwrap_or_else(|| s.len())
}
fn v_line_of(ed: &Editor, b: usize) -> usize {
    ed.vlines.partition_point(|&s| s <= b).saturating_sub(1)
}
/// 第 i 行的字节范围 [起, 止)（止不含行尾换行符）。
fn v_line_range(ed: &Editor, i: usize) -> (usize, usize) {
    let s = ed.vlines[i];
    let e = if i + 1 < ed.vlines.len() { ed.vlines[i + 1] - 1 } else { ed.content.len() };
    (s, e)
}
fn v_sel_range(ed: &Editor) -> Option<(usize, usize)> {
    ed.vsel.map(|a| (a.min(ed.vcaret), a.max(ed.vcaret))).filter(|(a, b)| a < b)
}
// ——— 自动换行（word-wrap）视觉行映射 ———
/// 某逻辑行按 cols 列折行后的视觉行数（按字符数近似，CJK 暂按 1 列计）。
fn line_vrows(chars: usize, cols: usize) -> u32 {
    (chars / cols + if chars % cols != 0 { 1 } else { 0 }).max(1) as u32
}
/// 同步换行行数前缀和缓存（列宽或内容变化时重算）。
fn v_wrap_sync(ed: &mut Editor, cols: usize) {
    let cols = cols.max(1);
    if ed.vrow_cols == cols && ed.vrow_ver == ed.vver && ed.vrow_pre.len() == ed.vlines.len() + 1 {
        return;
    }
    let n = ed.vlines.len();
    let mut pre = Vec::with_capacity(n + 1);
    let mut acc = 0u32;
    pre.push(0);
    for i in 0..n {
        let (s, e) = v_line_range(ed, i);
        let chars = ed.content[s..e].chars().count();
        acc = acc.saturating_add(line_vrows(chars, cols));
        pre.push(acc);
    }
    ed.vrow_pre = pre;
    ed.vrow_cols = cols;
    ed.vrow_ver = ed.vver;
}
/// 总视觉行数（换行模式）。
fn v_total_vrows(ed: &Editor) -> usize {
    ed.vrow_pre.last().copied().unwrap_or(0) as usize
}
/// 视觉行号 → (逻辑行, 段内序号)。
fn v_line_of_vrow(ed: &Editor, vrow: usize) -> (usize, usize) {
    let v = vrow as u32;
    let line = ed.vrow_pre.partition_point(|&p| p <= v).saturating_sub(1).min(ed.vlines.len().saturating_sub(1));
    let seg = vrow - ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (line, seg)
}
/// 字节偏移 → (视觉行, 段内列)。
fn v_vpos_of_byte(ed: &Editor, byte: usize, cols: usize) -> (usize, usize) {
    let cols = cols.max(1);
    let line = v_line_of(ed, byte);
    let (ls, _) = v_line_range(ed, line);
    let col = ed.content[ls..byte.max(ls)].chars().count();
    let base = ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (base + col / cols, col % cols)
}
/// (视觉行, 段内列) → 字节偏移（钳到行尾）。
fn v_byte_of_vpos(ed: &Editor, vrow: usize, vcol: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    let (line, seg) = v_line_of_vrow(ed, vrow);
    let (ls, le) = v_line_range(ed, line);
    let line_chars = ed.content[ls..le].chars().count();
    let col = (seg * cols + vcol).min(line_chars);
    ls + char_to_byte(&ed.content[ls..le], col)
}
fn v_recompute(ed: &mut Editor) {
    ed.vver = ed.vver.wrapping_add(1); // 内容变更 → 换行行数缓存失效
    ed.vlines = compute_line_starts(&ed.content);
    // 最长行字节数（含尾行）——缓存，渲染时直接用，避免每帧扫全部行
    ed.vmax = ed
        .vlines
        .windows(2)
        .map(|w| w[1] - w[0])
        .chain(std::iter::once(ed.content.len() - ed.vlines.last().copied().unwrap_or(0)))
        .max()
        .unwrap_or(0);
}
/// 把 content[at..at+removed_len] 替换为 inserted，并记录一条可撤销操作（连续输入会合并）。
fn v_apply(ed: &mut Editor, at: usize, removed_len: usize, inserted: &str) {
    let caret_before = ed.vcaret;
    let removed = ed.content[at..at + removed_len].to_string();
    ed.content.replace_range(at..at + removed_len, inserted);
    ed.vcaret = at + inserted.len();
    ed.vsel = None;
    // 连续单段输入（非换行）合并到上一条，避免每个字符一条撤销记录
    let mergeable = removed.is_empty() && !inserted.is_empty() && !inserted.contains('\n');
    if mergeable {
        if let Some(last) = ed.vundo.last_mut() {
            if last.removed.is_empty() && !last.inserted.ends_with('\n') && last.at + last.inserted.len() == at {
                last.inserted.push_str(inserted);
                last.caret_after = ed.vcaret;
                ed.vredo.clear();
                v_recompute(ed);
                return;
            }
        }
    }
    ed.vundo.push(EditOp { at, removed, inserted: inserted.to_string(), caret_before, caret_after: ed.vcaret });
    if ed.vundo.len() > 5000 {
        ed.vundo.remove(0);
    }
    ed.vredo.clear();
    v_recompute(ed);
}
fn v_delete_selection(ed: &mut Editor) -> bool {
    if let Some((a, b)) = v_sel_range(ed) {
        v_apply(ed, a, b - a, "");
        ed.vgoal_col = None;
        true
    } else {
        ed.vsel = None;
        false
    }
}
fn v_insert(ed: &mut Editor, t: &str) {
    let (at, rl) = if let Some((a, b)) = v_sel_range(ed) { (a, b - a) } else { (ed.vcaret, 0) };
    v_apply(ed, at, rl, t);
    ed.vgoal_col = None;
}
fn v_backspace(ed: &mut Editor) {
    if v_delete_selection(ed) {
        return;
    }
    if ed.vcaret == 0 {
        return;
    }
    let prev = prev_char_boundary(&ed.content, ed.vcaret);
    v_apply(ed, prev, ed.vcaret - prev, "");
    ed.vgoal_col = None;
}
fn v_delete_fwd(ed: &mut Editor) {
    if v_delete_selection(ed) {
        return;
    }
    if ed.vcaret >= ed.content.len() {
        return;
    }
    let next = next_char_boundary(&ed.content, ed.vcaret);
    v_apply(ed, ed.vcaret, next - ed.vcaret, "");
    ed.vgoal_col = None;
}
fn v_undo(ed: &mut Editor) {
    if let Some(op) = ed.vundo.pop() {
        let end = op.at + op.inserted.len();
        ed.content.replace_range(op.at..end, &op.removed);
        ed.vcaret = op.caret_before.min(ed.content.len());
        ed.vsel = None;
        ed.vgoal_col = None;
        v_recompute(ed);
        ed.vredo.push(op);
    }
}
fn v_redo(ed: &mut Editor) {
    if let Some(op) = ed.vredo.pop() {
        let end = op.at + op.removed.len();
        ed.content.replace_range(op.at..end, &op.inserted);
        ed.vcaret = op.caret_after.min(ed.content.len());
        ed.vsel = None;
        ed.vgoal_col = None;
        v_recompute(ed);
        ed.vundo.push(op);
    }
}
fn v_move_h(ed: &mut Editor, fwd: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        if let Some((a, b)) = v_sel_range(ed) {
            ed.vcaret = if fwd { b } else { a };
            ed.vsel = None;
            return;
        }
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = if fwd { next_char_boundary(&ed.content, ed.vcaret) } else { prev_char_boundary(&ed.content, ed.vcaret) };
}
fn v_move_v(ed: &mut Editor, delta: isize, shift: bool) {
    if shift && ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    if !shift {
        ed.vsel = None;
    }
    // 换行模式：按「视觉行」上下移动（保持视觉列）
    if ed.wrap && ed.vrow_cols > 0 && !ed.vrow_pre.is_empty() {
        let cols = ed.vrow_cols;
        let (vrow, vcol) = v_vpos_of_byte(ed, ed.vcaret, cols);
        let goal = ed.vgoal_col.unwrap_or(vcol);
        ed.vgoal_col = Some(goal);
        let total = v_total_vrows(ed);
        let target = (vrow as isize + delta).clamp(0, total.saturating_sub(1) as isize) as usize;
        ed.vcaret = v_byte_of_vpos(ed, target, goal, cols);
        return;
    }
    let line = v_line_of(ed, ed.vcaret);
    let (ls, _) = v_line_range(ed, line);
    let col = ed.vgoal_col.unwrap_or_else(|| ed.content[ls..ed.vcaret].chars().count());
    ed.vgoal_col = Some(col);
    let target = (line as isize + delta).clamp(0, ed.vlines.len() as isize - 1) as usize;
    let (ts, te) = v_line_range(ed, target);
    let line_chars = ed.content[ts..te].chars().count();
    let c = col.min(line_chars);
    ed.vcaret = ts + char_to_byte(&ed.content[ts..te], c);
}
fn v_move_edge(ed: &mut Editor, end: bool, shift: bool) {
    ed.vgoal_col = None;
    if shift && ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    if !shift {
        ed.vsel = None;
    }
    let line = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, line);
    ed.vcaret = if end { le } else { ls };
}

/// 词边界：从字节 b 向前/后找下一个词边界（跳过空白，再跳过一段同类字符；换行单独成界）。
fn v_word_boundary(s: &str, b: usize, fwd: bool) -> usize {
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = b.min(s.len());
    if fwd {
        loop {
            match s[i..].chars().next() {
                Some(c) if c.is_whitespace() && c != '\n' => i += c.len_utf8(),
                _ => break,
            }
        }
        if let Some('\n') = s[i..].chars().next() {
            return i + 1;
        }
        let word = s[i..].chars().next().map(is_w).unwrap_or(false);
        loop {
            match s[i..].chars().next() {
                Some(c) if c != '\n' && !c.is_whitespace() && is_w(c) == word => i += c.len_utf8(),
                _ => break,
            }
        }
    } else {
        loop {
            match s[..i].chars().next_back() {
                Some(c) if c.is_whitespace() && c != '\n' => i -= c.len_utf8(),
                _ => break,
            }
        }
        if let Some('\n') = s[..i].chars().next_back() {
            return i - 1;
        }
        let word = s[..i].chars().next_back().map(is_w).unwrap_or(false);
        loop {
            match s[..i].chars().next_back() {
                Some(c) if c != '\n' && !c.is_whitespace() && is_w(c) == word => i -= c.len_utf8(),
                _ => break,
            }
        }
    }
    i
}
/// 光标处的「词」字节范围（前后扩展词字符）；无词则 None。
fn v_word_range(s: &str, pos: usize) -> Option<(usize, usize)> {
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = pos.min(s.len());
    let mut end = start;
    while start > 0 {
        let c = s[..start].chars().next_back().unwrap();
        if is_w(c) {
            start -= c.len_utf8();
        } else {
            break;
        }
    }
    while end < s.len() {
        let c = s[end..].chars().next().unwrap();
        if is_w(c) {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    (end > start).then_some((start, end))
}
// ——— 多光标（Ctrl+D 累加选区）———
/// 把最后一个选区的文本的「下一处」加入 msel（向后找、到尾环绕；跳过已在集合中的）。
fn v_multi_add_next(ed: &mut Editor) {
    let &(ls, le) = match ed.msel.last() {
        Some(r) => r,
        None => return,
    };
    let needle = ed.content[ls..le].to_string();
    if needle.is_empty() {
        return;
    }
    let n = needle.len();
    let mut pos = le;
    for _ in 0..(ed.msel.len() + 2) {
        let p = match ed.content[pos.min(ed.content.len())..].find(&needle).map(|o| pos + o).or_else(|| ed.content.find(&needle)) {
            Some(p) => p,
            None => return,
        };
        let r = (p, p + n);
        if !ed.msel.contains(&r) {
            ed.msel.push(r);
            ed.msel.sort_by_key(|x| x.0);
            ed.vsel = Some(r.0);
            ed.vcaret = r.1;
            ed.pending_scroll = Some(v_line_of(ed, r.0));
            return;
        }
        pos = if p + n >= ed.content.len() { 0 } else { p + n };
    }
}
/// Ctrl+D：首次→选中当前选区/光标处的词并入集合；其后→加入下一处相同文本。
fn v_ctrl_d(ed: &mut Editor) {
    if ed.msel.is_empty() {
        if let Some((a, b)) = v_sel_range(ed) {
            ed.msel.push((a, b));
            v_multi_add_next(ed);
        } else if let Some((a, b)) = v_word_range(&ed.content, ed.vcaret) {
            ed.msel.push((a, b));
            ed.vsel = Some(a);
            ed.vcaret = b;
        }
    } else {
        v_multi_add_next(ed);
    }
}
/// 把全部选区替换为 text（一次撤销记录），并把 msel 收为各插入点后的裸光标。
fn v_multi_replace(ed: &mut Editor, text: &str) {
    let mut ranges = ed.msel.clone();
    ranges.sort_by_key(|r| r.0);
    let mut clean: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if clean.last().map_or(false, |l| s < l.1) {
            continue; // 跳过重叠
        }
        clean.push((s, e));
    }
    if clean.is_empty() {
        return;
    }
    let lo = clean.first().unwrap().0;
    let hi = clean.last().unwrap().1;
    let mut seg = String::new();
    let mut cursor = lo;
    let mut carets = Vec::new();
    for &(s, e) in &clean {
        seg.push_str(&ed.content[cursor..s]);
        seg.push_str(text);
        carets.push(lo + seg.len());
        cursor = e;
    }
    v_apply(ed, lo, hi - lo, &seg);
    ed.msel = carets.into_iter().map(|p| (p, p)).collect();
    ed.vcaret = ed.msel.last().map(|r| r.1).unwrap_or(ed.vcaret);
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_multi_backspace(ed: &mut Editor) {
    let del: Vec<(usize, usize)> = ed.msel.iter().map(|&(s, e)| if e > s { (s, e) } else { (prev_char_boundary(&ed.content, s), s) }).collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
fn v_multi_delete(ed: &mut Editor) {
    let del: Vec<(usize, usize)> = ed.msel.iter().map(|&(s, e)| if e > s { (s, e) } else { (s, next_char_boundary(&ed.content, s)) }).collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
/// 多选模式下移动所有光标（左/右）：选区折叠到一侧，裸光标按字符移动；保持多选。
fn v_multi_move(ed: &mut Editor, fwd: bool) {
    let mut carets: Vec<usize> = ed
        .msel
        .iter()
        .map(|&(s, e)| {
            if e > s {
                if fwd {
                    e
                } else {
                    s
                }
            } else if fwd {
                next_char_boundary(&ed.content, e)
            } else {
                prev_char_boundary(&ed.content, s)
            }
        })
        .collect();
    carets.sort_unstable();
    carets.dedup();
    ed.msel = carets.into_iter().map(|p| (p, p)).collect();
    ed.vcaret = ed.msel.last().map(|r| r.1).unwrap_or(ed.vcaret);
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_multi_copy(ed: &Editor) -> String {
    let parts: Vec<String> = ed.msel.iter().filter(|&&(s, e)| e > s).map(|&(s, e)| ed.content[s..e].to_string()).collect();
    parts.join("\n")
}
fn v_move_word(ed: &mut Editor, fwd: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = v_word_boundary(&ed.content, ed.vcaret, fwd);
}
fn v_delete_word(ed: &mut Editor, fwd: bool) {
    if v_delete_selection(ed) {
        return;
    }
    let to = v_word_boundary(&ed.content, ed.vcaret, fwd);
    let (a, b) = if fwd { (ed.vcaret, to) } else { (to, ed.vcaret) };
    if b > a {
        v_apply(ed, a, b - a, "");
    }
    ed.vgoal_col = None;
}
fn v_move_doc(ed: &mut Editor, end: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = if end { ed.content.len() } else { 0 };
}
/// 该语言的行注释前缀（无则 None）。
fn line_comment(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "rs" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "java" | "kt" | "kts" | "swift" | "dart" | "cs" | "scala" | "php" | "rust" | "json5" | "proto" | "groovy" | "v" | "zig" | "vue" | "svelte" => "//",
        "py" | "pyw" | "rb" | "sh" | "bash" | "zsh" | "fish" | "pl" | "pm" | "r" | "jl" | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "config" | "properties" | "dockerfile" | "makefile" | "mk" | "cmake" | "gitignore" | "env" | "tcl" | "nim" | "awk" | "sed" | "gro" | "top" | "itp" | "mdp" | "ndx" => "#",
        "sql" | "lua" | "hs" | "ml" | "elm" | "adoc" => "--",
        "clj" | "cljs" | "lisp" | "el" | "asm" | "s" => ";",
        "vim" => "\"",
        _ => return None,
    })
}
fn v_toggle_comment(ed: &mut Editor, prefix: &str) {
    let (sa, sb) = v_sel_range(ed).unwrap_or((ed.vcaret, ed.vcaret));
    let first = v_line_of(ed, sa);
    let last = v_line_of(ed, sb.max(sa).saturating_sub(if sb > sa { 1 } else { 0 }));
    // 判定是否「全部已注释」：非空行都以前缀开头 → 反注释，否则加注释
    let mut all = true;
    for li in first..=last {
        let (ls, le) = v_line_range(ed, li);
        let t = ed.content[ls..le].trim_start();
        if !t.is_empty() && !t.starts_with(prefix) {
            all = false;
            break;
        }
    }
    let pfx = format!("{prefix} ");
    // 从后往前改，前面行的偏移不受影响
    for li in (first..=last).rev() {
        let (ls, le) = v_line_range(ed, li);
        let line = &ed.content[ls..le];
        let indent = line.len() - line.trim_start().len();
        if all {
            let after = &line[indent..];
            if after.starts_with(prefix) {
                let mut rm = prefix.len();
                if after[prefix.len()..].starts_with(' ') {
                    rm += 1;
                }
                v_apply(ed, ls + indent, rm, "");
            }
        } else if !line[indent..].is_empty() {
            v_apply(ed, ls + indent, 0, &pfx);
        }
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_duplicate_line(ed: &mut Editor, down: bool) {
    let li = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, li);
    let line = ed.content[ls..le].to_string();
    let col = ed.vcaret - ls;
    if down {
        v_apply(ed, le, 0, &format!("\n{line}"));
        ed.vcaret = le + 1 + col;
    } else {
        v_apply(ed, ls, 0, &format!("{line}\n"));
        ed.vcaret = ls + col;
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_move_line(ed: &mut Editor, up: bool) {
    let li = v_line_of(ed, ed.vcaret);
    let total = ed.vlines.len();
    if (up && li == 0) || (!up && li + 1 >= total) {
        return;
    }
    let col = ed.vcaret - v_line_range(ed, li).0;
    let (a, b) = if up { (li - 1, li) } else { (li, li + 1) };
    let (as_, _) = v_line_range(ed, a);
    let (bs, be) = v_line_range(ed, b);
    let la = ed.content[as_..v_line_range(ed, a).1].to_string();
    let lb = ed.content[bs..be].to_string();
    v_apply(ed, as_, be - as_, &format!("{lb}\n{la}"));
    let target = if up { li - 1 } else { li + 1 };
    let (ts, te) = v_line_range(ed, target);
    ed.vcaret = ts + col.min(te - ts);
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_delete_line(ed: &mut Editor) {
    let li = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, li);
    let total = ed.vlines.len();
    if li + 1 < total {
        v_apply(ed, ls, (le + 1) - ls, "");
    } else if ls > 0 {
        v_apply(ed, ls - 1, le - (ls - 1), "");
    } else {
        v_apply(ed, ls, le - ls, "");
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
/// 输入开括号/引号时自动补全的闭合符。
fn auto_close_for(t: &str) -> Option<&'static str> {
    match t {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        "\"" => Some("\""),
        "'" => Some("'"),
        "`" => Some("`"),
        _ => None,
    }
}

/// 若字节 bp 处是括号，返回 (该括号位置, 匹配括号位置)；否则 None。扫描有上限、忽略字符串/注释。
fn bracket_at(s: &str, bp: usize) -> Option<(usize, usize)> {
    const OPENS: [char; 3] = ['(', '[', '{'];
    const CLOSES: [char; 3] = [')', ']', '}'];
    const CAP: usize = 200_000;
    if bp >= s.len() || !s.is_char_boundary(bp) {
        return None;
    }
    let c = s[bp..].chars().next()?;
    if let Some(oi) = OPENS.iter().position(|&o| o == c) {
        let close = CLOSES[oi];
        let mut depth = 1i32;
        for (off, ch) in s[bp + c.len_utf8()..].char_indices().take(CAP) {
            if ch == c {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some((bp, bp + c.len_utf8() + off));
                }
            }
        }
        None
    } else if let Some(ci) = CLOSES.iter().position(|&o| o == c) {
        let open = OPENS[ci];
        let mut depth = 1i32;
        let mut i = bp;
        let mut n = 0usize;
        while i > 0 && n < CAP {
            let ch = s[..i].chars().next_back().unwrap();
            i -= ch.len_utf8();
            n += 1;
            if ch == c {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    return Some((bp, i));
                }
            }
        }
        None
    } else {
        None
    }
}
/// 找到与 caret 相邻（左/右）的括号及其匹配位置。
fn bracket_match(s: &str, caret: usize) -> Option<(usize, usize)> {
    if caret > 0 {
        let before = prev_char_boundary(s, caret);
        if let Some(r) = bracket_at(s, before) {
            return Some(r);
        }
    }
    bracket_at(s, caret)
}

// ———————————————————————— VSCode 风格查找/替换控件（两套编辑器共用） ————————————————————————

enum FindOut {
    None,
    Goto(usize, usize),        // 选中并滚到该字节范围
    ReplaceOne(usize, usize),  // 把该字节范围替换为 ed.replace（字面）
    ReplaceAll(String),        // 用新全文替换
}

/// 由查找选项构造正则（字面查找也走正则：escape + 可选 \b）。
fn build_find_regex(pat: &str, case: bool, word: bool, regex_mode: bool) -> Option<regex::Regex> {
    let p = if regex_mode {
        pat.to_string()
    } else {
        let esc = regex::escape(pat);
        if word {
            format!(r"\b{esc}\b")
        } else {
            esc
        }
    };
    regex::RegexBuilder::new(&p).case_insensitive(!case).size_limit(1 << 24).build().ok()
}

/// 按需重算全部匹配（字节范围）；缓存签名（查找词+选项+内容长度）不变则跳过。
fn rebuild_matches(ed: &mut Editor) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ed.find.hash(&mut h);
    ed.find_case.hash(&mut h);
    ed.find_word.hash(&mut h);
    ed.find_regex.hash(&mut h);
    ed.content.len().hash(&mut h);
    let sig = h.finish();
    if sig == ed.find_sig {
        return;
    }
    ed.find_sig = sig;
    ed.find_matches.clear();
    if ed.find.is_empty() {
        return;
    }
    if let Some(re) = build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex) {
        for m in re.find_iter(&ed.content).take(200_000) {
            if m.end() > m.start() {
                ed.find_matches.push((m.start(), m.end()));
            }
        }
    }
}

fn nav_match(matches: &[(usize, usize)], caret: usize, forward: bool) -> Option<(usize, usize)> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        matches.iter().find(|&&(a, _)| a > caret).copied().or_else(|| matches.first().copied())
    } else {
        matches.iter().rev().find(|&&(a, _)| a < caret).copied().or_else(|| matches.last().copied())
    }
}

fn replace_all_content(ed: &Editor) -> Option<String> {
    let re = build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex)?;
    Some(if ed.find_regex {
        re.replace_all(&ed.content, ed.replace.as_str()).into_owned()
    } else {
        re.replace_all(&ed.content, regex::NoExpand(ed.replace.as_str())).into_owned()
    })
}

fn find_toggle(ui: &mut egui::Ui, label: &str, on: bool, tip: &str) -> bool {
    let fill = if on { Palette::ACCENT_SOFT } else { egui::Color32::TRANSPARENT };
    let col = if on { Palette::ACCENT } else { Palette::TEXT_DIM };
    ui.add(egui::Button::new(RichText::new(label).size(12.0).color(col)).fill(fill).corner_radius(4.0).min_size(egui::vec2(24.0, 20.0)))
        .on_hover_text(tip)
        .clicked()
}

/// 跳转到行浮层（顶部居中）；返回 Some(1 基行号) 表示跳转。
fn goto_widget(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> Option<usize> {
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ed.goto_open = false;
        return None;
    }
    let mut out = None;
    egui::Area::new(text_id.with("goto"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 44.0))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(Palette::PANEL_2)
                .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.visuals_mut().extreme_bg_color = egui::Color32::from_rgb(252, 252, 250);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("跳转到行", "Go to line")).color(Palette::TEXT_DIM).size(12.0));
                        let r = ui.add(egui::TextEdit::singleline(&mut ed.goto_text).desired_width(80.0).hint_text("1.."));
                        if ed.goto_focus {
                            r.request_focus();
                            ed.goto_focus = false;
                        }
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if enter || ui.button(crate::i18n::tr("跳转", "Go")).clicked() {
                            if let Ok(n) = ed.goto_text.trim().parse::<usize>() {
                                out = Some(n.max(1));
                            }
                            ed.goto_open = false;
                            ed.goto_text.clear();
                        }
                    });
                });
        });
    out
}

/// VSCode 风格查找/替换浮层（右上角）；`caret_byte` 为当前光标字节位置；返回要应用的动作。
fn find_widget(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id, caret_byte: usize) -> FindOut {
    use egui_phosphor::regular as icon;
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ed.show_find = false;
        return FindOut::None;
    }
    rebuild_matches(ed);
    let total = ed.find_matches.len();
    let cur_idx = ed.find_matches.iter().position(|&(a, b)| caret_byte >= a && caret_byte <= b);
    let mut out = FindOut::None;
    egui::Area::new(text_id.with("find_widget"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 44.0)) // 标签栏之下，避免遮住保存/查找
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(Palette::PANEL_2)
                .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.spacing_mut().interact_size.y = 24.0;
                    ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                    // 输入框用近白底，和卡片/边框区分开（默认会和 PANEL_2 同色看不清）
                    ui.visuals_mut().extreme_bg_color = egui::Color32::from_rgb(252, 252, 250);
                    ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::new(1.0, Palette::BORDER);
                    ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::new(1.0, Palette::TEXT_DIM);
                    ui.horizontal(|ui| {
                        let exp = if ed.replace_open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
                        if ui.add(egui::Button::new(RichText::new(exp).size(12.0).color(Palette::TEXT_DIM)).frame(false).min_size(egui::vec2(20.0, 20.0))).on_hover_text(crate::i18n::tr("展开/收起替换", "Toggle replace")).clicked() {
                            ed.replace_open = !ed.replace_open;
                        }
                        let fr = ui.add(egui::TextEdit::singleline(&mut ed.find).desired_width(150.0).hint_text(crate::i18n::tr("查找", "Find")));
                        if ed.find_focus {
                            fr.request_focus();
                            ed.find_focus = false;
                        }
                        if find_toggle(ui, "Aa", ed.find_case, crate::i18n::tr("区分大小写", "Match case")) {
                            ed.find_case = !ed.find_case;
                        }
                        if find_toggle(ui, "ab", ed.find_word, crate::i18n::tr("全字匹配", "Whole word")) {
                            ed.find_word = !ed.find_word;
                        }
                        if find_toggle(ui, ".*", ed.find_regex, crate::i18n::tr("正则表达式", "Regex")) {
                            ed.find_regex = !ed.find_regex;
                        }
                        let count = if ed.find.is_empty() {
                            String::new()
                        } else if total == 0 {
                            crate::i18n::tr("无结果", "No results").into()
                        } else if let Some(i) = cur_idx {
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("第 {} 项，共 {} 项", i + 1, total),
                                crate::i18n::Lang::En => format!("{} of {}", i + 1, total),
                            }
                        } else {
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("共 {} 项", total),
                                crate::i18n::Lang::En => format!("{} results", total),
                            }
                        };
                        ui.label(RichText::new(count).color(Palette::TEXT_DIM).size(11.0));
                        if ui.add(egui::Button::new(RichText::new(icon::ARROW_UP).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("上一个", "Previous")).clicked() {
                            if let Some((a, b)) = nav_match(&ed.find_matches, caret_byte, false) {
                                out = FindOut::Goto(a, b);
                            }
                        }
                        if ui.add(egui::Button::new(RichText::new(icon::ARROW_DOWN).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("下一个", "Next")).clicked() {
                            if let Some((a, b)) = nav_match(&ed.find_matches, caret_byte, true) {
                                out = FindOut::Goto(a, b);
                            }
                        }
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("关闭 (Esc)", "Close (Esc)")).clicked() {
                            ed.show_find = false;
                        }
                    });
                    if ed.replace_open {
                        ui.horizontal(|ui| {
                            // 与查找行的折叠箭头同宽的占位（同为首项 → 与查找输入框左对齐）
                            ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                            ui.add(egui::TextEdit::singleline(&mut ed.replace).desired_width(150.0).hint_text(crate::i18n::tr("替换", "Replace")));
                            if ui.add(egui::Button::new(RichText::new(icon::ARROW_BEND_DOWN_LEFT).size(13.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("替换", "Replace")).clicked() {
                                if let Some(i) = cur_idx {
                                    let (a, b) = ed.find_matches[i];
                                    out = FindOut::ReplaceOne(a, b);
                                } else if let Some((a, b)) = nav_match(&ed.find_matches, caret_byte, true) {
                                    out = FindOut::Goto(a, b);
                                }
                            }
                            if ui.add(egui::Button::new(RichText::new(icon::ARROWS_DOWN_UP).size(13.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("全部替换", "Replace all")).clicked() && total > 0 {
                                if let Some(newc) = replace_all_content(ed) {
                                    out = FindOut::ReplaceAll(newc);
                                }
                            }
                        });
                    }
                });
        });
    out
}

/// 虚拟化可编辑器：仅渲染可见行 + 自绘光标/选区。返回 true 表示请求保存（Ctrl+S）。
fn editable_virtual(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    let mut save = false;
    if ed.vlines.is_empty() {
        v_recompute(ed);
    }
    ed.vcaret = ed.vcaret.min(ed.content.len());

    // 括号 lint：仅对配平规则明确的语言、且文件不过大；按内容版本缓存（编辑时才重算，不逐帧 tokenize）。
    if ed.lint_ver != ed.vver {
        ed.lint_ver = ed.vver;
        if ed.content.len() <= LINT_LIMIT && highlight::lint_enabled(&ed.language) {
            let (lines, ranges, msg) = highlight::lint_brackets(&ed.content, &ed.language);
            ed.lint_lines = lines.into_iter().collect();
            ed.lint_ranges = ranges;
            ed.lint_msg = msg;
        } else {
            ed.lint_lines.clear();
            ed.lint_ranges.clear();
            ed.lint_msg = None;
        }
    }

    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, ' ')).max(1.0);
    let bg = egui::Color32::from_rgb(252, 252, 250);
    let focused = ui.memory(|m| m.focused() == Some(text_id));
    // 聚焦时尽早锁定 Tab/方向键/Esc 到编辑器：必须在底部状态栏菜单按钮等可聚焦控件渲染之前设置，
    // 否则 egui 在渲染那些控件时已用方向键把焦点切走（之前放在 ScrollArea 内、太晚，导致上下键跳到菜单）。
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(text_id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true });
        });
    }
    let page = ((ui.available_height() / row_h).floor() as isize - 2).max(1);
    let lang = ed.language.clone();
    let fsize = mono.size;
    // 右键菜单/查找动作（在闭包外应用，避免借用冲突）
    let mut do_copy = false;
    let mut do_cut = false;
    let mut do_paste = false;
    let mut do_selall = false;

    // ——— 输入（聚焦时）———
    // 视角跟随只看「光标是否真的移动」：按下任意键/输入法每帧都发事件会让旧的 moved 一直为真、
    // 导致不停把视角拉回光标、无法自由滚动。这里记下输入前的光标，处理完输入后用差异判断。
    let prev_caret = ed.vcaret;
    // 自绘 IME（同 egui 路径，绕开 egui Commit 门）：处理组字/提交，并在下方上报 o.ime 激活+定位候选框。
    if focused {
        let ime_events: Vec<egui::ImeEvent> = ui.input(|i| i.events.iter().filter_map(|e| if let egui::Event::Ime(ev) = e { Some(ev.clone()) } else { None }).collect());
        for ev in ime_events {
            match ev {
                egui::ImeEvent::Enabled => {}
                egui::ImeEvent::Preedit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    // 组字是临时的：直接改 content、不入撤销栈
                    let (s, e) = ed.vime_preedit.take().or_else(|| v_sel_range(ed)).unwrap_or((ed.vcaret, ed.vcaret));
                    let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                    ed.content.replace_range(s..e, &t);
                    let end = s + t.len();
                    ed.vcaret = end;
                    ed.vsel = None;
                    ed.msel.clear();
                    ed.vime_preedit = if t.is_empty() { None } else { Some((s, end)) };
                    v_recompute(ed);
                }
                egui::ImeEvent::Commit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    if let Some((s, e)) = ed.vime_preedit.take() {
                        let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                        ed.content.replace_range(s..e, "");
                        ed.vcaret = s;
                        ed.vsel = None;
                        v_recompute(ed);
                    }
                    // 多光标模式：英文/输入法提交也要作用到全部光标（系统输入法激活后字母走 Commit 而非 Text）
                    if ed.msel.is_empty() {
                        v_insert(ed, &t);
                    } else {
                        v_multi_replace(ed, &t);
                    }
                }
                egui::ImeEvent::Disabled => {
                    if let Some((s, e)) = ed.vime_preedit.take() {
                        let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                        ed.content.replace_range(s..e, "");
                        ed.vcaret = s;
                        ed.vsel = None;
                        v_recompute(ed);
                    }
                }
            }
        }
        // 已自绘处理，移除 Ime 事件，避免主循环重复处理
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Ime(_))));
    }
    if focused {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            // 多光标模式（msel 非空）：编辑/复制作用于全部选区；移动等其它键退出多选、走常规
            if !ed.msel.is_empty() {
                let mut handled = true;
                match &ev {
                    egui::Event::Text(t) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Paste(t) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Copy => {
                        let s = v_multi_copy(ed);
                        if !s.is_empty() {
                            ui.ctx().copy_text(s);
                        }
                    }
                    egui::Event::Cut => {
                        let s = v_multi_copy(ed);
                        if !s.is_empty() {
                            ui.ctx().copy_text(s);
                            v_multi_replace(ed, "");
                        }
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        let cmd = modifiers.command || modifiers.ctrl;
                        match key {
                            egui::Key::Escape => ed.msel.clear(),
                            egui::Key::Backspace => v_multi_backspace(ed),
                            egui::Key::Delete => v_multi_delete(ed),
                            egui::Key::Enter => v_multi_replace(ed, "\n"),
                            egui::Key::Tab => {
                                let u = ed.indent.unit();
                                v_multi_replace(ed, &u);
                            }
                            egui::Key::D if cmd => v_ctrl_d(ed),
                            egui::Key::ArrowLeft if !cmd => v_multi_move(ed, false),
                            egui::Key::ArrowRight if !cmd => v_multi_move(ed, true),
                            // 纵向导航 / Ctrl+组合键 → 退出多选、走常规处理
                            egui::Key::ArrowUp | egui::Key::ArrowDown | egui::Key::Home | egui::Key::End | egui::Key::PageUp | egui::Key::PageDown if !cmd => {
                                ed.msel.clear();
                                handled = false;
                            }
                            _ if cmd => {
                                ed.msel.clear();
                                handled = false;
                            }
                            // 普通字母/符号键：会另发 Text 事件做多光标插入，这里不清 msel
                            _ => handled = false,
                        }
                    }
                    _ => handled = false,
                }
                if handled {
                    continue; // 视角跟随由「光标是否移动」统一判断
                }
            }
            match ev {
                egui::Event::Text(t) if !t.is_empty() => {
                    // 自动补全括号/引号：无选区→插入成对并把光标放中间；有选区→用括号包裹并保留选中
                    if let Some(close) = auto_close_for(&t) {
                        if let Some((a, b)) = v_sel_range(ed) {
                            let inner = ed.content[a..b].to_string();
                            v_apply(ed, a, b - a, &format!("{t}{inner}{close}"));
                            ed.vsel = Some(a + t.len());
                            ed.vcaret = a + t.len() + inner.len();
                            ed.vgoal_col = None;
                        } else {
                            v_insert(ed, &format!("{t}{close}"));
                            ed.vcaret -= close.len();
                        }
                    } else {
                        v_insert(ed, &t);
                    }
                }
                egui::Event::Paste(t) if !t.is_empty() => v_insert(ed, &t),
                egui::Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => v_insert(ed, &t),
                egui::Event::Copy => {
                    if let Some(s) = v_sel_range(ed).map(|(a, b)| ed.content[a..b].to_string()) {
                        ui.ctx().copy_text(s);
                    }
                }
                egui::Event::Cut => {
                    if let Some(s) = v_sel_range(ed).map(|(a, b)| ed.content[a..b].to_string()) {
                        ui.ctx().copy_text(s);
                        v_delete_selection(ed);
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    let cmd = modifiers.command || modifiers.ctrl;
                    match key {
                        egui::Key::S if cmd => save = true,
                        egui::Key::F if cmd => {
                            ed.show_find = !ed.show_find;
                            if ed.show_find {
                                ed.find_focus = true;
                            }
                        }
                        egui::Key::G if cmd => {
                            ed.goto_open = !ed.goto_open;
                            if ed.goto_open {
                                ed.goto_focus = true;
                            }
                        }
                        egui::Key::A if cmd => {
                            ed.vsel = Some(0);
                            ed.vcaret = ed.content.len();
                        }
                        egui::Key::D if cmd => v_ctrl_d(ed),
                        egui::Key::Z if cmd && modifiers.shift => v_redo(ed),
                        egui::Key::Z if cmd => v_undo(ed),
                        egui::Key::Y if cmd => v_redo(ed),
                        egui::Key::Slash if cmd => {
                            if let Some(p) = line_comment(&ed.language) {
                                v_toggle_comment(ed, p);
                            }
                        }
                        egui::Key::K if cmd && modifiers.shift => v_delete_line(ed),
                        egui::Key::Backspace if cmd => v_delete_word(ed, false),
                        egui::Key::Delete if cmd => v_delete_word(ed, true),
                        egui::Key::Backspace => v_backspace(ed),
                        egui::Key::Delete => v_delete_fwd(ed),
                        egui::Key::Enter => v_insert(ed, "\n"),
                        egui::Key::Tab => {
                            let u = ed.indent.unit();
                            v_insert(ed, &u);
                        }
                        egui::Key::ArrowUp if modifiers.alt && modifiers.shift => v_duplicate_line(ed, false),
                        egui::Key::ArrowDown if modifiers.alt && modifiers.shift => v_duplicate_line(ed, true),
                        egui::Key::ArrowUp if modifiers.alt => v_move_line(ed, true),
                        egui::Key::ArrowDown if modifiers.alt => v_move_line(ed, false),
                        egui::Key::ArrowLeft if cmd => v_move_word(ed, false, modifiers.shift),
                        egui::Key::ArrowRight if cmd => v_move_word(ed, true, modifiers.shift),
                        egui::Key::ArrowLeft => v_move_h(ed, false, modifiers.shift),
                        egui::Key::ArrowRight => v_move_h(ed, true, modifiers.shift),
                        egui::Key::ArrowUp => v_move_v(ed, -1, modifiers.shift),
                        egui::Key::ArrowDown => v_move_v(ed, 1, modifiers.shift),
                        egui::Key::Home if cmd => v_move_doc(ed, false, modifiers.shift),
                        egui::Key::End if cmd => v_move_doc(ed, true, modifiers.shift),
                        egui::Key::Home => v_move_edge(ed, false, modifiers.shift),
                        egui::Key::End => v_move_edge(ed, true, modifiers.shift),
                        egui::Key::PageUp => v_move_v(ed, -page, modifiers.shift),
                        egui::Key::PageDown => v_move_v(ed, page, modifiers.shift),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        ed.vcaret = ed.vcaret.min(ed.content.len());
    }
    // 真正判断光标是否移动（仅此情况才让视角跟随；无移动时自由滚动、绝不拉回）
    let moved = ed.vcaret != prev_caret;

    // 底部状态栏（仿小文件编辑器）：缩进可切换（矩形按钮、贴左）+ 语言贴右。
    egui::Panel::bottom("editor_status_v")
        .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.scope(|ui| {
                    let v = ui.visuals_mut();
                    v.widgets.inactive.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.hovered.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.active.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.open.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0);
                    ui.menu_button(format!("{} {}", crate::i18n::tr("缩进", "Indent"), ed.indent.label()), |ui| {
                        ui.set_min_width(120.0);
                        for ind in [Indent::Spaces(2), Indent::Spaces(4), Indent::Tab] {
                            if ui.selectable_label(ed.indent == ind, ind.label()).clicked() {
                                ed.indent = ind;
                                ui.close();
                            }
                        }
                    });
                    // 自动换行开关：开启时长行折行、无横向滚动
                    ui.add_space(6.0);
                    let wrap_col = if ed.wrap { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(crate::i18n::tr("换行", "Wrap")).color(wrap_col).size(11.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr("点击切换自动换行", "Toggle word wrap"))
                        .clicked()
                    {
                        ed.wrap = !ed.wrap;
                        ed.vgoal_col = None; // 列语义改变，重置目标列
                    }
                });
                if !ed.status.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&ed.status).color(Palette::TEXT_DIM).size(11.0));
                }
                if ed.msel.len() > 1 {
                    ui.add_space(8.0);
                    let n = ed.msel.len();
                    let label = match crate::i18n::current() {
                        crate::i18n::Lang::En => format!("{n} cursors"),
                        _ => format!("{n} 光标"),
                    };
                    ui.label(RichText::new(label).color(Palette::ACCENT).size(11.0));
                }
                // 括号 lint 概述（不匹配时红字）
                if let Some(msg) = &ed.lint_msg {
                    ui.add_space(8.0);
                    ui.label(RichText::new(msg).color(Palette::DANGER).size(11.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(ed.language.as_str()).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 光标位置 Ln:Col（主光标，1 基；列按字符计）
                    let cl = v_line_of(ed, ed.vcaret);
                    let (lsx, _) = v_line_range(ed, cl);
                    let col = ed.content[lsx..ed.vcaret.min(ed.content.len())].chars().count() + 1;
                    ui.label(RichText::new(format!("Ln {}, Col {}", cl + 1, col)).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 行尾：点击切换 LF/CRLF
                    let eol_txt = match ed.eol() { crate::proto::Eol::Crlf => "CRLF", crate::proto::Eol::Lf => "LF" };
                    if ui.add(egui::Label::new(RichText::new(eol_txt).color(Palette::TEXT_DIM).size(11.0)).sense(egui::Sense::click())).on_hover_text(crate::i18n::tr("点击切换行尾 LF/CRLF", "Click to toggle LF/CRLF")).clicked() {
                        let n = match ed.eol() { crate::proto::Eol::Crlf => crate::proto::Eol::Lf, crate::proto::Eol::Lf => crate::proto::Eol::Crlf };
                        ed.set_eol(n);
                    }
                    ui.add_space(10.0);
                    // 编码：点击从菜单选择（保存时按所选编码写回）
                    ui.menu_button(RichText::new(ed.encoding()).color(Palette::TEXT_DIM).size(11.0), |ui| {
                        ui.set_min_width(120.0);
                        for enc in ["UTF-8", "GBK", "GB18030", "Big5", "Shift_JIS", "EUC-KR", "windows-1252", "ISO-8859-1"] {
                            if ui.selectable_label(ed.encoding() == enc, enc).clicked() {
                                ed.set_encoding(enc.to_string());
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text(crate::i18n::tr("点击选择保存编码", "Click to choose save encoding"));
                });
            });
        });

    // 查找/替换：VSCode 风格浮层（共用 find_widget），按字节定位/替换、可撤销。
    if ed.show_find {
        match find_widget(ui, ed, text_id, ed.vcaret) {
            FindOut::Goto(a, b) => {
                ed.vsel = Some(a);
                ed.vcaret = b;
                ed.pending_scroll = Some(v_line_of(ed, b));
            }
            FindOut::ReplaceOne(a, b) => {
                let rep = ed.replace.clone();
                v_apply(ed, a, b - a, &rep);
                ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
            }
            FindOut::ReplaceAll(newc) => {
                let old = ed.content.len();
                v_apply(ed, 0, old, &newc);
                ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
            }
            FindOut::None => {}
        }
    }
    // 跳转到行
    if ed.goto_open {
        if let Some(n) = goto_widget(ui, ed, text_id) {
            let line = (n - 1).min(ed.vlines.len().saturating_sub(1));
            ed.vcaret = v_line_range(ed, line).0;
            ed.vsel = None;
            ed.vgoal_col = None;
            ed.pending_scroll = Some(line);
        }
    }

    // ——— 渲染（仅可见行）———
    let total = ed.vlines.len();
    let digits = total.max(1).to_string().len();
    let gutter_w = (digits as f32 + 1.5) * char_w;
    // 自动换行：按视口宽度算每行可容纳列数，并同步「视觉行前缀和」缓存（列宽/内容变化才重算）
    let view_w_pre = if ed.vlast_vieww > 0.0 { ed.vlast_vieww } else { ui.available_width() };
    let wrap_cols = (((view_w_pre - gutter_w) / char_w) as i64).max(1) as usize;
    if ed.wrap {
        v_wrap_sync(ed, wrap_cols);
    }
    let wrap = ed.wrap;
    // 「滚动行」数：换行模式按视觉行总数虚拟化，否则按逻辑行数
    let nrows = if wrap { v_total_vrows(ed) } else { total };
    // 内容高度封顶在 f32 安全区：行数巨大时坐标会丢精度 → 封顶后按「行号」虚拟化、用视口相对坐标绘制。
    // 末尾额外留 3 行空白：可滚到最后一行之下，避免底部横向滚动条遮住最后一行。
    let pad_rows = 3usize;
    let content_w = if wrap {
        gutter_w + (wrap_cols as f32 + 1.0) * char_w // 换行模式无横向滚动
    } else {
        gutter_w + (ed.vmax as f32 + 2.0) * char_w
    };
    let content_h = ((nrows + pad_rows) as f32 * row_h).min(2_000_000.0);

    // —— 用「设置滚动偏移」可靠地让视角跟随光标（scroll_to_rect 在本虚拟容器里不生效）——
    // 用上一帧度量判断光标是否已在可视区，越界才强制滚动；普通滚动不受影响。
    let mut force_v: Option<f32> = None;
    let mut force_h: Option<f32> = None;
    {
        let view_h = if ed.vlast_viewh > 0.0 { ed.vlast_viewh } else { ui.available_height() };
        let view_w = if ed.vlast_vieww > 0.0 { ed.vlast_vieww } else { ui.available_width() };
        let visible = (view_h / row_h).ceil() as usize + 2;
        let max_off = (content_h - view_h).max(1.0);
        let max_top = (nrows + pad_rows).saturating_sub(visible.saturating_sub(2));
        let caret_row = if wrap { v_vpos_of_byte(ed, ed.vcaret, wrap_cols).0 } else { v_line_of(ed, ed.vcaret) };
        if let Some(tl) = ed.pending_scroll.take() {
            // 跳转/定位：居中（换行模式把逻辑行转成其首个视觉行）
            let tl_row = if wrap { ed.vrow_pre.get(tl).copied().unwrap_or(0) as usize } else { tl };
            let tt = tl_row.saturating_sub(visible / 2);
            force_v = Some((tt as f32 / max_top.max(1) as f32) * max_off);
        } else if moved {
            // 键盘移动：只在越界时「一行」地滚（不要整屏跳）
            let top = ed.vlast_top;
            let vis = ed.vlast_vis.max(3);
            let tt = if caret_row < top {
                caret_row // 光标在视口上方 → 滚到刚好露出该行（一行）
            } else if caret_row + 2 >= top + vis {
                (caret_row + 3).saturating_sub(vis) // 光标在视口下方 → 滚到该行刚好在底部附近（一行）
            } else {
                top // 已在可视区 → 不滚
            };
            if tt != top {
                force_v = Some((tt as f32 / max_top.max(1) as f32) * max_off);
            }
        }
        if moved && !wrap {
            let (ls2, _) = v_line_range(ed, caret_row);
            let cx = gutter_w + ed.content[ls2..ed.vcaret].chars().count() as f32 * char_w; // 光标在内容坐标里的 x
            if cx < ed.vlast_hoff + gutter_w + char_w {
                force_h = Some((cx - gutter_w - char_w * 2.0).max(0.0));
            } else if cx > ed.vlast_hoff + view_w - char_w * 2.0 {
                force_h = Some((cx - view_w + char_w * 3.0).max(0.0));
            }
        }
        if let Some((dh, dv)) = ed.vscroll_nudge.take() {
            force_v = Some((force_v.unwrap_or(ed.vlast_voff) + dv).clamp(0.0, max_off));
            force_h = Some((force_h.unwrap_or(ed.vlast_hoff) + dh).max(0.0));
        }
    }

    egui::Frame::new().fill(bg).show(ui, |ui| {
        ui.spacing_mut().scroll.floating = false;
        ui.spacing_mut().scroll.foreground_color = false;
        ui.visuals_mut().extreme_bg_color = bg;
        ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_gray(202);
        ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_gray(168);
        ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_gray(140);
        let mut sa = egui::ScrollArea::both().auto_shrink([false, false]).id_salt(text_id);
        if let Some(v) = force_v {
            sa = sa.vertical_scroll_offset(v);
        }
        if let Some(h) = force_h {
            sa = sa.horizontal_scroll_offset(h);
        }
        sa.show_viewport(ui, |ui, vp| {
            ui.set_width(content_w);
            ui.set_height(content_h);
            let origin = ui.min_rect().min; // 横向滚动用其 x；纵向改用 clip + 行号映射避免大坐标丢精度
            let clip = ui.clip_rect();
            let view_h = clip.height();
            let max_off = (content_h - view_h).max(1.0);
            let frac = (vp.min.y / max_off).clamp(0.0, 1.0);
            let visible = (view_h / row_h).ceil() as usize + 2;
            let max_top = (nrows + pad_rows).saturating_sub(visible.saturating_sub(2)); // 最大首「行」号（视觉行/逻辑行）
            let top_row = ((frac * max_top as f32).round() as usize).min(max_top);
            // 首/末可见逻辑行（换行模式由视觉行换算；用于查找命中的可视范围）
            let first_line = if wrap { v_line_of_vrow(ed, top_row).0 } else { top_row };
            let last_line = if wrap { v_line_of_vrow(ed, (top_row + visible).min(nrows.saturating_sub(1))).0 + 1 } else { (top_row + visible).min(total) };
            let text_x = origin.x + gutter_w;
            // 记录本帧滚动度量，供下一帧「跟随光标」判断与施加偏移
            ed.vlast_top = top_row;
            ed.vlast_vis = visible;
            ed.vlast_voff = vp.min.y;
            ed.vlast_hoff = vp.min.x;
            ed.vlast_vieww = clip.width();
            ed.vlast_viewh = view_h;

            // 交互区取「可视视口」(clip)：内层 ui 被 set_width(content_w) 限成内容宽度，若按 content_w 取交互区，
            // 短行右侧的空白会落在区外、点击不到（光标不动）。用 clip 覆盖整个视口，短行右侧空白也能点击定位到行末。
            let area = clip;
            let resp = ui.interact(area, text_id, egui::Sense::click_and_drag());
            // 右键弹菜单时选区可能被折叠/失焦：在右键按下这一帧冻结当前选区，供菜单复制/剪切/粘贴使用
            if ui.input(|i| i.pointer.secondary_pressed()) {
                ed.menu_sel = v_sel_range(ed);
            }
            resp.context_menu(|ui| {
                ui.set_min_width(160.0);
                let has_sel = ed.menu_sel.is_some();
                if ui.add_enabled(has_sel, egui::Button::new(crate::i18n::tr("复制", "Copy"))).clicked() {
                    do_copy = true;
                    ui.close();
                }
                if ui.add_enabled(has_sel, egui::Button::new(crate::i18n::tr("剪切", "Cut"))).clicked() {
                    do_cut = true;
                    ui.close();
                }
                if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                    do_paste = true;
                    ui.close();
                }
                ui.separator();
                if ui.button(crate::i18n::tr("全选", "Select all")).clicked() {
                    do_selall = true;
                    ui.close();
                }
            });
            let painter = ui.painter().clone();
            // 多选时用 msel 全部选区/光标；否则用单选区 + 单光标
            let sels: Vec<(usize, usize)> = if !ed.msel.is_empty() { ed.msel.clone() } else { v_sel_range(ed).into_iter().collect() };
            let carets: Vec<usize> = if !ed.msel.is_empty() { ed.msel.iter().map(|&(_, e)| e).collect() } else { vec![ed.vcaret] };
            let caret_line = v_line_of(ed, ed.vcaret); // 当前行高亮
            let unit_cols = match ed.indent { Indent::Spaces(n) => (n as usize).max(1), Indent::Tab => 4 }; // 缩进参考线步长
            let brackets = if focused { bracket_match(&ed.content, ed.vcaret) } else { None }; // 括号匹配高亮
            // 可视区内的查找匹配（克隆出来，避免后续可变借用 ed 冲突）
            let vis_matches: Vec<(usize, usize)> = if ed.show_find && !ed.find.is_empty() {
                let vis_a = ed.vlines.get(first_line).copied().unwrap_or(0);
                let vis_b = ed.vlines.get(last_line.min(total)).copied().unwrap_or(ed.content.len());
                let mlo = ed.find_matches.partition_point(|&(s, _)| s < vis_a);
                let mhi = ed.find_matches.partition_point(|&(s, _)| s < vis_b);
                ed.find_matches[mlo..mhi].to_vec()
            } else {
                Vec::new()
            };
            // 水平可视列窗口：每行只对窗口内片段做高亮 + 布局（开销 O(可视列)，与行长无关）。
            // 这样超长行（日志/JSON/CSV 等）不再每帧整行 tokenize + layout，根治「某些大文件拖到底卡顿」。
            let first_col = ((clip.left() - text_x).max(0.0) / char_w) as usize;
            let cols_vis = (clip.width() / char_w).ceil() as usize + 8; // 视口列数 + 余量（CJK 偏宽，余量足够）
            let accent = Palette::ACCENT;
            for k in 0..visible {
                let row = top_row + k;
                if row >= nrows {
                    break;
                }
                // 视觉行 → 逻辑行 i / 起始列 col0 / 本行列数 ncols / 绘制起点 gx / 是否首段
                let (i, col0, ncols, gx, is_first) = if wrap {
                    let (li, seg) = v_line_of_vrow(ed, row);
                    (li, seg * wrap_cols, wrap_cols, text_x, seg == 0)
                } else {
                    (row, first_col, cols_vis, text_x + first_col as f32 * char_w, true)
                };
                if i >= total {
                    break;
                }
                let (ls, le) = v_line_range(ed, i);
                let line_full: &str = &ed.content[ls..le]; // 切片，不整行拷贝
                let y = clip.top() + k as f32 * row_h;
                let col_of = |b: usize| -> usize { byte_to_char(line_full, b.saturating_sub(ls).min(line_full.len())) };
                let in_win = |c: usize| c >= col0 && c <= col0 + ncols;
                // 当前行高亮（极淡）：聚焦且无选区时，给光标所在行铺一层很淡的底
                if focused && sels.is_empty() && i == caret_line {
                    painter.rect_filled(egui::Rect::from_min_max(egui::pos2(clip.left(), y), egui::pos2(clip.right(), y + row_h)), 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10));
                }
                // 缩进参考线（仅首段画）：在各缩进层级之间画很淡的竖线
                if is_first {
                    let mut lead = 0usize;
                    for c in line_full.chars() {
                        match c {
                            ' ' => lead += 1,
                            '\t' => lead += unit_cols,
                            _ => break,
                        }
                    }
                    let mut col = unit_cols;
                    while col < lead {
                        let gx = text_x + col as f32 * char_w;
                        painter.vline(gx, y..=(y + row_h), egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 20)));
                        col += unit_cols;
                    }
                }
                // 仅取窗口片段（char_to_byte 至多遍历到 last_col 个字符）
                let seg_a = char_to_byte(line_full, col0);
                let seg_b = char_to_byte(line_full, col0 + ncols);
                let seg = &line_full[seg_a..seg_b];
                let seg_x = gx;
                let seg_right = gx + ncols as f32 * char_w;
                // 括号 lint 下划线：把全文偏移的错误范围裁剪/平移成「本段内相对范围」传给高亮器
                let (seg_start, seg_end_abs) = (ls + seg_a, ls + seg_b);
                let lint_errs: Vec<std::ops::Range<usize>> = if ed.lint_ranges.is_empty() {
                    Vec::new()
                } else {
                    ed.lint_ranges
                        .iter()
                        .filter_map(|r| {
                            let a = r.start.max(seg_start);
                            let b = r.end.min(seg_end_abs);
                            (a < b).then(|| (a - seg_start)..(b - seg_start))
                        })
                        .collect()
                };
                let galley = {
                    let mut job = highlight::highlight(seg, &lang, fsize, &lint_errs);
                    job.wrap.max_width = f32::INFINITY;
                    ui.ctx().fonts_mut(|f| f.layout_job(job))
                };
                // 行内字节偏移 → 屏幕 x（窗口外钳制到窗口边缘，超出部分本就不可见）
                let x_of = |lb: usize| -> f32 { seg_x + galley.pos_from_cursor(CCursor::new(byte_to_char(seg, lb.clamp(seg_a, seg_b) - seg_a))).left() };
                // 选区/查找当前项高亮：半透明珊瑚色（多选时画全部）
                for &(sa, sb) in &sels {
                    if sb > sa && sb > ls && sa <= le {
                        let ax = x_of(sa.clamp(ls, le) - ls);
                        // 选区越过本段末尾(含跨到下一视觉行/下一逻辑行) → 填到本段右缘
                        let bx = if sb >= ls + seg_b { seg_right } else { x_of(sb.clamp(ls, le) - ls) };
                        if bx > ax {
                            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(ax, y), egui::pos2(bx, y + row_h)), 0.0, egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 60));
                        }
                    }
                }
                // 正文
                painter.galley(egui::pos2(seg_x, y), galley.clone(), Palette::TEXT);
                // 括号匹配：给光标相邻括号及其匹配括号描边
                if let Some((ba, bb)) = brackets {
                    for &bp in &[ba, bb] {
                        if bp >= ls && bp < le && in_win(col_of(bp)) {
                            let bx0 = x_of(bp - ls);
                            let bx1 = x_of(bp + 1 - ls);
                            painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(bx0, y), egui::pos2(bx1, y + row_h)), 2.0, egui::Stroke::new(1.0, Palette::ACCENT), egui::StrokeKind::Inside);
                        }
                    }
                }
                // 查找命中高亮（半透明灰），跳过「当前项」(=选区)避免叠灰盖字。
                for &(ma, mb) in &vis_matches {
                    if sels.contains(&(ma, mb)) {
                        continue;
                    }
                    if ma < ls + seg_b && mb > ls + seg_a {
                        let hx0 = x_of(ma.clamp(ls, le) - ls);
                        let hx1 = x_of(mb.clamp(ls, le) - ls);
                        if hx1 > hx0 {
                            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(hx0, y), egui::pos2(hx1, y + row_h)), 2.0, egui::Color32::from_rgba_unmultiplied(120, 120, 120, 56));
                        }
                    }
                }
                // 光标（多选时每个选区末尾各画一个）
                if focused {
                    for &cp in &carets {
                        if cp >= ls && cp <= le && in_win(col_of(cp)) {
                            let cx = x_of(cp - ls);
                            painter.vline(cx, y..=(y + row_h), egui::Stroke::new(1.5, Palette::ACCENT));
                        }
                    }
                    // 在主光标处上报 IME 输入区：激活输入法 + 定位候选框（否则虚拟编辑器无法输入中文）
                    if ed.vcaret >= ls && ed.vcaret <= le && in_win(col_of(ed.vcaret)) {
                        let cx = x_of(ed.vcaret - ls);
                        let irect = egui::Rect::from_min_size(egui::pos2(cx, y), egui::vec2(1.0, row_h));
                        ui.ctx().output_mut(|o| o.ime = Some(egui::output::IMEOutput { rect: irect, cursor_rect: irect }));
                    }
                }
                // 行号列固定在左侧：最后画（铺底盖住横向滚到下面的正文）+ 右对齐行号
                painter.rect_filled(egui::Rect::from_min_max(egui::pos2(clip.left(), y), egui::pos2(clip.left() + gutter_w, y + row_h)), 0.0, bg);
                if is_first {
                    // 括号不匹配的行：行号标红（lint）
                    let num_col = if ed.lint_lines.contains(&i) { Palette::DANGER } else { Palette::TEXT_DIM };
                    painter.text(egui::pos2(clip.left() + gutter_w - char_w * 0.7, y), egui::Align2::RIGHT_TOP, (i + 1).to_string(), mono.clone(), num_col);
                }
            }
            // 行号分割线（固定在左侧行号列右缘）
            painter.vline(clip.left() + gutter_w - 3.0, clip.top()..=clip.bottom(), egui::Stroke::new(1.0, Palette::BORDER));
            // 点击 / 双击 / 拖拽定位光标与选区（行号 = top_line + 视口内行偏移）
            if resp.clicked() || resp.drag_started() || resp.dragged() || resp.double_clicked() {
                if resp.clicked() || resp.drag_started() || resp.double_clicked() {
                    ui.memory_mut(|m| m.request_focus(text_id));
                }
                if let Some(pos) = resp.interact_pointer_pos() {
                    ed.msel.clear(); // 点击退出多选
                    let k = ((pos.y - clip.top()) / row_h).floor().max(0.0) as usize;
                    let row = (top_row + k).min(nrows.saturating_sub(1));
                    let (li, c0, nc, gx) = if wrap {
                        let (l, seg) = v_line_of_vrow(ed, row);
                        (l, seg * wrap_cols, wrap_cols, text_x)
                    } else {
                        (row.min(total.saturating_sub(1)), first_col, cols_vis, text_x + first_col as f32 * char_w)
                    };
                    let (ls, le) = v_line_range(ed, li);
                    // 同样只布局窗口片段（避免在超长行上拖拽选择时每帧整行 layout）
                    let line_full: &str = &ed.content[ls..le];
                    let seg_a = char_to_byte(line_full, c0);
                    let seg_b = char_to_byte(line_full, c0 + nc);
                    let seg = line_full[seg_a..seg_b].to_string();
                    let seg_x = gx;
                    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(seg.clone(), mono.clone(), Palette::TEXT));
                    let cc = g.cursor_from_pos(egui::vec2(pos.x - seg_x, 0.0)).index;
                    let b = ls + seg_a + char_to_byte(&seg, cc);
                    if resp.double_clicked() {
                        // 双击选中光标处的词
                        if let Some((wa, wb)) = v_word_range(&ed.content, b) {
                            ed.vsel = Some(wa);
                            ed.vcaret = wb;
                        } else {
                            ed.vsel = None;
                            ed.vcaret = b;
                        }
                    } else if resp.drag_started() {
                        ed.vsel = Some(b);
                        ed.vcaret = b;
                    } else if resp.dragged() {
                        if ed.vsel.is_none() {
                            ed.vsel = Some(ed.vcaret);
                        }
                        ed.vcaret = b;
                    } else {
                        ed.vsel = None;
                        ed.vcaret = b;
                    }
                    ed.vgoal_col = None;
                }
            }
            // 键盘移动的「跟随光标」已在 ScrollArea 创建前用 vertical/horizontal_scroll_offset 施加（可靠）。
            // 这里只处理拖选到边缘：记录滚动增量，下一帧施加（持续自动滚动）。
            if resp.dragged() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let dv = if pos.y < clip.top() + row_h {
                        -row_h * 2.0
                    } else if pos.y > clip.bottom() - row_h {
                        row_h * 2.0
                    } else {
                        0.0
                    };
                    let dh = if pos.x < clip.left() + gutter_w + char_w {
                        -char_w * 3.0
                    } else if pos.x > clip.right() - char_w {
                        char_w * 3.0
                    } else {
                        0.0
                    };
                    if dv != 0.0 || dh != 0.0 {
                        ed.vscroll_nudge = Some((dh, dv));
                        ui.ctx().request_repaint();
                    }
                }
            }
        });
    });
    // 右键菜单动作（闭包外应用）
    if do_selall {
        ed.vsel = Some(0);
        ed.vcaret = ed.content.len();
    }
    // 复制/剪切用「冻结的右键选区」(menu_sel)，避免右键折叠选区后复制不到
    if do_copy || do_cut {
        if let Some((a, b)) = ed.menu_sel {
            let (a, b) = (a.min(ed.content.len()), b.min(ed.content.len()));
            if b > a {
                ui.ctx().copy_text(ed.content[a..b].to_string());
                if do_cut {
                    v_apply(ed, a, b - a, "");
                    ed.vgoal_col = None;
                }
            }
        }
    }
    if do_paste {
        if let Some(t) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
            if !t.is_empty() {
                // 有冻结选区则替换它，否则插入到光标
                if let Some((a, b)) = ed.menu_sel.filter(|&(a, b)| b > a) {
                    let (a, b) = (a.min(ed.content.len()), b.min(ed.content.len()));
                    v_apply(ed, a, b - a, &t);
                } else {
                    v_insert(ed, &t);
                }
                ed.vgoal_col = None;
            }
        }
    }
    if do_copy || do_cut || do_paste {
        ed.menu_sel = None;
    }
    save
}

/// 字符下标 → 字节偏移（用于右键复制/剪切/粘贴按选区操作 UTF-8 内容）。
fn char_to_byte(s: &str, c: usize) -> usize {
    s.char_indices().nth(c).map(|(b, _)| b).unwrap_or(s.len())
}

/// 字节偏移 → 字符下标。
fn byte_to_char(s: &str, b: usize) -> usize {
    s[..b.min(s.len())].chars().count()
}
