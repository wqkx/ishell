//! 文本编辑器内容渲染：语法高亮（大文件自动关闭高亮以省内存）、查找/替换。
//! 多标签与窗口框架由 app 负责。

use egui::text::CCursor;
use egui::text_selection::CCursorRange;
use egui::RichText;

use crate::theme::Palette;
use crate::ui::highlight::{self, Indent};

/// 超过该大小则不做语法高亮（省内存/CPU）。
const HIGHLIGHT_LIMIT: usize = 256 * 1024;
/// 超过该大小改为只读、按行虚拟化渲染（避免 egui 整文件布局占用巨量内存）。
const EDIT_LIMIT: usize = 1024 * 1024;

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
    /// 上一帧的非空选区（右键会折叠选区，菜单复制/剪切用这个冻结值）
    last_sel: Option<(usize, usize)>,
    /// 右键打开菜单时冻结的选区
    menu_sel: Option<(usize, usize)>,
    /// 自绘输入法：当前预编辑(组字)文本在 content 中的字符范围 [start,end)；无组字为 None。
    ime_preedit: Option<(usize, usize)>,
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
    /// 上下移动时保持的目标列（字符数；None 表示用当前列）
    vgoal_col: Option<usize>,
    /// 选区锚点（Some 时 [anchor, caret] 为选区）
    vsel: Option<usize>,
    /// 虚拟编辑器撤销/重做栈（操作式，省内存）
    vundo: Vec<EditOp>,
    vredo: Vec<EditOp>,
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
            last_sel: None,
            menu_sel: None,
            ime_preedit: None,
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
            encoding: "UTF-8".into(),
            eol: crate::proto::Eol::Lf,
            mtime: 0,
            find_matches: Vec::new(),
            find_sig: 0,
            vcaret: 0,
            vlines: Vec::new(),
            vmax: 0,
            vgoal_col: None,
            vsel: None,
            vundo: Vec::new(),
            vredo: Vec::new(),
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
    let mut save = false;

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

    // 大文件直接用虚拟化可编辑器（仅渲染可见行，避免 egui TextEdit 给整文件建 galley 的内存与每帧
    // 重排开销）；已去掉只读模式。小/中文件仍用功能完整的 egui TextEdit。
    if ed.content.len() > EDIT_LIMIT {
        return editable_virtual(ui, ed, text_id);
    }

    // 保存/查找按钮已上移到窗口标签栏右侧（对齐主窗口风格）；此处仅保留快捷键。
    if ui.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::S)) {
        save = true;
    }
    // Ctrl/Cmd+F：切换查找栏（再次按下关闭）；打开时聚焦查找框。
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F))
        || ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F))
    {
        ed.show_find = !ed.show_find;
        if ed.show_find {
            ed.find_focus = true;
        }
    }
    // Ctrl/Cmd+G：跳转到行
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::G)) || ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::G)) {
        ed.goto_open = !ed.goto_open;
        if ed.goto_open {
            ed.goto_focus = true;
        }
    }

    // 查找/替换：VSCode 风格浮层（共用 find_widget）。
    let mut pending_select: Option<(usize, usize)> = None;
    if ed.goto_open {
        if let Some(n) = goto_widget(ui, ed, text_id) {
            // 第 n 行行首的字节偏移 → 字符位置
            let byte = ed.content.split_inclusive('\n').take(n - 1).map(|s| s.len()).sum::<usize>().min(ed.content.len());
            let c = byte_to_char(&ed.content, byte);
            pending_select = Some((c, c));
        }
    }
    if ed.show_find {
        let caret_char = egui::text_edit::TextEditState::load(ui.ctx(), text_id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| r.primary.index)
            .unwrap_or(0);
        let caret_byte = char_to_byte(&ed.content, caret_char);
        match find_widget(ui, ed, text_id, caret_byte) {
            FindOut::Goto(a, b) => {
                pending_select = Some((byte_to_char(&ed.content, a), byte_to_char(&ed.content, b)));
            }
            FindOut::ReplaceOne(a, b) => {
                let c0 = byte_to_char(&ed.content, a);
                let c1 = byte_to_char(&ed.content, b);
                let rep = ed.replace.clone();
                replace_char_range(&mut ed.content, c0, c1, &rep);
                pending_select = Some((c0, c0 + rep.chars().count()));
            }
            FindOut::ReplaceAll(newc) => {
                ed.content = newc;
                pending_select = Some((0, 0));
            }
            FindOut::None => {}
        }
    }

    // —— 自绘输入法 ——
    // egui 0.34 的 Commit 处理有个门：`cursor_range.secondary.index == state.ime_cursor_range.secondary.index`
    // 才插入提交文本，而 `ime_cursor_range` 只在 Enabled/Preedit 事件里更新。fcitx(X11) 这类「只发
    // Commit、不发 Enabled/Preedit」的输入法下，该值永远停在 0：第一次光标在 0 能插入，插入后光标
    // 移动，第二次起门永远不通过 → 中文只能输一次。这里在 TextEdit 之前自行处理 Preedit/Commit
    // 直接写入缓冲、再移除全部 Ime 事件，绕开该 bug；独立窗口保留、各平台通用。
    let ime_events: Vec<egui::ImeEvent> = ui.input(|i| {
        i.events
            .iter()
            .filter_map(|e| if let egui::Event::Ime(ev) = e { Some(ev.clone()) } else { None })
            .collect()
    });
    if !ime_events.is_empty() && ui.memory(|m| m.focused() == Some(text_id)) {
        // 当前选区（字符索引）；无则取末尾
        let (mut sel0, mut sel1) = egui::text_edit::TextEditState::load(ui.ctx(), text_id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| (r.primary.index.min(r.secondary.index), r.primary.index.max(r.secondary.index)))
            .unwrap_or_else(|| {
                let n = ed.content.chars().count();
                (n, n)
            });
        for ev in ime_events {
            match ev {
                egui::ImeEvent::Enabled => {}
                egui::ImeEvent::Preedit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    // 有进行中的预编辑则替换它，否则替换当前选区
                    let (s, e) = ed.ime_preedit.take().unwrap_or((sel0, sel1));
                    replace_char_range(&mut ed.content, s, e, &t);
                    let end = s + t.chars().count();
                    if !t.is_empty() {
                        ed.ime_preedit = Some((s, end));
                    }
                    sel0 = end;
                    sel1 = end;
                    set_cursor(ui.ctx(), text_id, end, end);
                }
                egui::ImeEvent::Commit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    let (s, e) = ed.ime_preedit.take().unwrap_or((sel0, sel1));
                    replace_char_range(&mut ed.content, s, e, &t);
                    let end = s + t.chars().count();
                    sel0 = end;
                    sel1 = end;
                    set_cursor(ui.ctx(), text_id, end, end);
                }
                egui::ImeEvent::Disabled => {
                    // 取消未提交的组字
                    if let Some((s, e)) = ed.ime_preedit.take() {
                        replace_char_range(&mut ed.content, s, e, "");
                        sel0 = s;
                        sel1 = s;
                        set_cursor(ui.ctx(), text_id, s, s);
                    }
                }
            }
        }
        // 已自行处理，移除全部 Ime 事件，避免 egui 用坏掉的 Commit 门重复处理
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Ime(_))));
    }

    // —— 自动缩进：本编辑器聚焦时拦截 Tab / Shift+Tab / 回车，手动处理后阻止 TextEdit 默认行为 ——
    // 用上一帧存储的光标位置（consume 必须在 TextEdit.show() 之前）。
    // 关键：输入法组字/提交（有 Ime 事件）时不拦截 Enter，否则会吃掉中文提交导致无法输入中文。
    let ime_active = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
    if !ime_active && ui.memory(|m| m.focused() == Some(text_id)) {
        if let Some(r) = egui::text_edit::TextEditState::load(ui.ctx(), text_id).and_then(|s| s.cursor.char_range()) {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                apply_enter(&mut ed.content, r, ed.indent, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
                apply_indent(&mut ed.content, r, ed.indent, false, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)) {
                apply_indent(&mut ed.content, r, ed.indent, true, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Slash) || i.consume_key(egui::Modifiers::CTRL, egui::Key::Slash)) {
                apply_comment(&mut ed.content, r, &ed.language, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::K) || i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::K)) {
                apply_delete_line(&mut ed.content, r, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::ArrowUp)) {
                apply_dup_line(&mut ed.content, r, false, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT | egui::Modifiers::SHIFT, egui::Key::ArrowDown)) {
                apply_dup_line(&mut ed.content, r, true, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowUp)) {
                apply_move_line(&mut ed.content, r, true, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowDown)) {
                apply_move_line(&mut ed.content, r, false, ui.ctx(), text_id);
            } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D) || i.consume_key(egui::Modifiers::CTRL, egui::Key::D)) {
                // 无选区→选中光标处的词；有选区→跳选下一处相同文本（单选）
                let range = r.as_sorted_char_range();
                if range.start == range.end {
                    let b = char_to_byte(&ed.content, range.start);
                    if let Some((wa, wb)) = v_word_range(&ed.content, b) {
                        set_cursor(ui.ctx(), text_id, byte_to_char(&ed.content, wa), byte_to_char(&ed.content, wb));
                    }
                } else {
                    let bs = char_to_byte(&ed.content, range.start);
                    let be = char_to_byte(&ed.content, range.end);
                    let needle = ed.content[bs..be].to_string();
                    if !needle.is_empty() {
                        if let Some(p) = ed.content[be..].find(&needle).map(|o| be + o).or_else(|| ed.content.find(&needle)) {
                            let c0 = byte_to_char(&ed.content, p);
                            set_cursor(ui.ctx(), text_id, c0, c0 + needle.chars().count());
                        }
                    }
                }
            }
        }
    }

    // 代码编辑区（近白背景，显示更清晰；大文件不做高亮/lint）
    let bg = egui::Color32::from_rgb(252, 252, 250); // 近白底
    let do_highlight = ed.content.len() <= HIGHLIGHT_LIMIT;
    // 初级 lint：仅对「可高亮大小 + 已知编程语言」做括号配对（不认识的文本类型不判，避免误报）
    let (err_lines, err_ranges, lint_msg): (std::collections::HashSet<usize>, Vec<std::ops::Range<usize>>, Option<String>) =
        if do_highlight && highlight::lint_enabled(&ed.language) {
            let (lines, ranges, msg) = highlight::lint_brackets(&ed.content, &ed.language);
            (lines.into_iter().collect(), ranges, msg)
        } else {
            (Default::default(), Vec::new(), None)
        };
    let lang = ed.language.clone();
    let err_for_layout = err_ranges.clone();
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = if do_highlight {
            highlight::highlight(buf.as_str(), &lang, 13.0, &err_for_layout)
        } else {
            let mut j = egui::text::LayoutJob::default();
            j.append(
                buf.as_str(),
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::monospace(13.0),
                    color: Palette::TEXT,
                    ..Default::default()
                },
            );
            j
        };
        job.wrap.max_width = wrap_width;
        ui.ctx().fonts_mut(|f| f.layout_job(job))
    };

    let mono = egui::FontId::monospace(13.0);
    let n_lines = ed.content.split('\n').count().max(1);
    let digits = n_lines.to_string().len();
    let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, '0')).max(1.0);
    let gutter_w = (digits as f32 + 1.5) * char_w; // 行号列宽
    let row_h = ui.ctx().fonts_mut(|f| f.row_height(&mono));
    // 底部固定状态栏（仿 VSCode）：缩进规则可点切换 + lint 概述 + 语言。先占住底部，编辑区填其余。
    // 无内边距 → 内容贴到窗口左右/底边；缩进按钮做成「与栏等高的矩形」(无圆角、贴左)。
    egui::Panel::bottom("editor_status")
        // 栏底色贴窗口左右/底边；左右留 8px 内边距，文字/按钮不顶边、不被遮挡；上下 0 让按钮与栏等高。
        .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // 缩进按钮：矩形（无圆角）、与状态栏等高、贴左
                ui.scope(|ui| {
                    let v = ui.visuals_mut();
                    v.widgets.inactive.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.hovered.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.active.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.open.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0); // 决定按钮（即状态栏）高度与左右内边距
                    ui.menu_button(format!("{} {}", crate::i18n::tr("缩进", "Indent"), ed.indent.label()), |ui| {
                        ui.set_min_width(120.0);
                        for ind in [Indent::Spaces(2), Indent::Spaces(4), Indent::Tab] {
                            if ui.selectable_label(ed.indent == ind, ind.label()).clicked() {
                                ed.indent = ind;
                                ui.close();
                            }
                        }
                    });
                });
                if let Some(msg) = &lint_msg {
                    ui.add_space(8.0);
                    ui.label(RichText::new(msg).color(Palette::DANGER).size(11.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(ed.language.as_str()).color(Palette::TEXT_DIM).size(11.0));
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

    // 编辑区填满状态栏以上的剩余高度（内容不足一屏也填满，便于点击空白定位光标）。
    let fill_rows = (((ui.available_height() - 6.0) / row_h).ceil() as usize).max(8);
    egui::Frame::new().fill(bg).show(ui, |ui| {
        // 滚动条独立成右侧一列（不浮在内容上）——必须在创建 ScrollArea 之前设置
        ui.spacing_mut().scroll.floating = false;
        // 滑块颜色取 widgets.*.bg_fill（默认 floating 样式用 fg_stroke，会忽略 bg_fill 而显白）。
        ui.spacing_mut().scroll.foreground_color = false;
        // 滑块在近白底上保持可见灰度（默认拖动时偏白看不清）；槽底用近白与内容一致。
        ui.visuals_mut().extreme_bg_color = bg;
        ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_gray(202);
        ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_gray(168);
        ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_gray(140);
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .id_salt(text_id) // 按标签区分滚动状态：否则各标签共用同一滚动位置、互相带动
            .show(ui, |ui| {
            ui.visuals_mut().extreme_bg_color = bg; // TextEdit 自身背景也用近白，无接缝
            // 去掉聚焦/悬停/未聚焦时的边框描边（橙色聚焦框）
            ui.visuals_mut().widgets.active.bg_stroke = egui::Stroke::NONE;
            ui.visuals_mut().widgets.hovered.bg_stroke = egui::Stroke::NONE;
            ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::NONE;
            // egui 0.34 用 selection.stroke.color 给「选中的文字」重新着色：设为 NONE(透明) 会让选中
            // 的文字完全消失（只剩底色）。这里设为正常文字色，保证选中后文字仍清晰可见。
            ui.visuals_mut().selection.stroke = egui::Stroke::new(1.0, Palette::TEXT);
            // 选区/查找当前项底色用半透明珊瑚，盖在字上仍能看清字符（默认不透明会完全遮住）
            ui.visuals_mut().selection.bg_fill = {
                let a = Palette::ACCENT;
                egui::Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 60) // 当前项/选区：半透明珊瑚色，比未选中灰更醒目、仍透字
            };
            ui.horizontal_top(|ui| {
                ui.add_space(gutter_w + 4.0); // 预留行号列宽度（行号随后按 galley 位置绘制）
                // 宽度比「行号列之后的剩余可视宽度」再小几像素：保证短行时正文总宽严格小于视口，
                // 横向滚动条不出现、也不会因恰好等于视口而反复出现/消失地闪动；仅当某行确实更长
                // （不换行）时正文才超出视口、出现横向滚动条。
                let avail = (ui.available_width() - 6.0).max(50.0);
                let out = egui::TextEdit::multiline(&mut ed.content)
                    .code_editor()
                    .frame(egui::Frame::new()) // 空 Frame：去掉 TextEdit 自带边框（聚焦/编辑态的深色框）；背景由外层 Frame 提供
                    .desired_width(avail)
                    .desired_rows(fill_rows)
                    .id(text_id)
                    .layouter(&mut layouter)
                    .show(ui);
                let id = out.response.id;
                // 行号与正文之间一条浅浅的竖向分割线
                {
                    let x = out.galley_pos.x - 3.0;
                    let top = out.galley_pos.y;
                    let bot = (top + out.galley.rows.last().map(|r| r.pos.y + row_h).unwrap_or(row_h)).max(top + ui.clip_rect().height());
                    ui.painter().vline(x, top..=bot, egui::Stroke::new(1.0, Palette::BORDER));
                }

                // 行号：按 galley 每行的实际像素位置逐行绘制，与正文严格对齐——彻底解决长文本
                // 底部缺行号（旧实现用单个多行 Label，高亮字体行高与 Label 不一致会累积错位）。
                // 仅绘制可见行，避免长文件逐行造 galley 拖慢。
                {
                    let clip = ui.clip_rect();
                    let num_x = out.galley_pos.x - 6.0;
                    let painter = ui.painter();
                    for (i, prow) in out.galley.rows.iter().enumerate() {
                        let y = out.galley_pos.y + prow.pos.y;
                        if y + row_h < clip.top() || y > clip.bottom() {
                            continue;
                        }
                        // 该行有 lint 错误（括号不匹配）则行号标红
                        let col = if err_lines.contains(&i) { Palette::DANGER } else { Palette::TEXT_DIM };
                        painter.text(egui::pos2(num_x, y), egui::Align2::RIGHT_TOP, (i + 1).to_string(), mono.clone(), col);
                    }
                }

                // 查找：半透明灰高亮匹配项（用全局匹配缓存，含大小写/全字/正则；仅画可视区内的）。
                if ed.show_find && !ed.find.is_empty() {
                    let clip = ui.clip_rect();
                    let gp = out.galley_pos;
                    let top = (clip.top() - gp.y).max(0.0);
                    let bot = (clip.bottom() - gp.y).max(0.0);
                    let first_c = out.galley.cursor_from_pos(egui::vec2(0.0, top)).index;
                    let last_c = out.galley.cursor_from_pos(egui::vec2(f32::INFINITY, bot)).index;
                    if last_c > first_c {
                        let fb = char_to_byte(&ed.content, first_c);
                        let lb = char_to_byte(&ed.content, last_c).min(ed.content.len());
                        let cur = out.cursor_range.map(|r| r.as_sorted_char_range()).map(|r| (r.start, r.end));
                        let painter = ui.painter();
                        let hl = egui::Color32::from_rgba_unmultiplied(120, 120, 120, 56);
                        let mlo = ed.find_matches.partition_point(|&(s, _)| s < fb);
                        let mhi = ed.find_matches.partition_point(|&(s, _)| s < lb);
                        for &(ma, mb) in &ed.find_matches[mlo..mhi] {
                            let c0 = byte_to_char(&ed.content, ma);
                            let c1 = byte_to_char(&ed.content, mb);
                            // 跳过「当前项」：当前选区，或本帧刚 next 定位到的项（避免 1 帧灰盖字）
                            if cur == Some((c0, c1)) || pending_select == Some((c0, c1)) {
                                continue;
                            }
                            let a = out.galley.pos_from_cursor(CCursor::new(c0));
                            let z = out.galley.pos_from_cursor(CCursor::new(c1));
                            if (z.top() - a.top()).abs() < 1.0 {
                                let r = egui::Rect::from_min_max(egui::pos2(gp.x + a.left(), gp.y + a.top()), egui::pos2(gp.x + z.left(), gp.y + a.bottom()));
                                painter.rect_filled(r, 2.0, hl);
                            }
                        }
                    }
                }

                // 括号匹配：给光标相邻括号及其匹配括号描边
                if ui.memory(|m| m.focused() == Some(text_id)) {
                    if let Some(cr) = out.cursor_range {
                        let caret_byte = char_to_byte(&ed.content, cr.primary.index);
                        if let Some((ba, bb)) = bracket_match(&ed.content, caret_byte) {
                            let gp = out.galley_pos;
                            let painter = ui.painter();
                            for bp in [ba, bb] {
                                let c0 = byte_to_char(&ed.content, bp);
                                let a = out.galley.pos_from_cursor(CCursor::new(c0));
                                let z = out.galley.pos_from_cursor(CCursor::new(c0 + 1));
                                if (z.top() - a.top()).abs() < 1.0 {
                                    painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(gp.x + a.left(), gp.y + a.top()), egui::pos2(gp.x + z.left(), gp.y + a.bottom())), 2.0, egui::Stroke::new(1.0, Palette::ACCENT), egui::StrokeKind::Inside);
                                }
                            }
                        }
                    }
                }

                // 查找定位
                if let Some((c0, c1)) = pending_select {
                    let mut st = out.state.clone();
                    st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(c0), CCursor::new(c1))));
                    st.store(ui.ctx(), id);
                    out.response.request_focus();
                }

                // 右键菜单：复制 / 剪切 / 粘贴 / 全选。
                // 难点：右键会被 TextEdit 当成点击而折叠选区，导致「全选后右键复制」复制不到。
                // 解法：每帧记录非空选区到 last_sel；右键当帧不清它，并冻结成 menu_sel 供菜单使用。
                let cur_sel = out
                    .cursor_range
                    .map(|r| r.as_sorted_char_range())
                    .filter(|r| r.start != r.end)
                    .map(|r| (r.start, r.end));
                // 右键的「按下」就会折叠选区（且早于 secondary_clicked 的「释放」一帧），
                // 因此必须在 secondary_pressed 这一帧、用上一帧仍在的 last_sel 冻结。
                let sec_pressed = ui.input(|i| i.pointer.secondary_pressed());
                if let Some(s) = cur_sel {
                    ed.last_sel = Some(s);
                } else if !sec_pressed {
                    ed.last_sel = None; // 非右键造成的折叠（左键/打字）才清，保证右键当帧 last_sel 仍在
                }
                if sec_pressed {
                    ed.menu_sel = cur_sel.or(ed.last_sel);
                }
                let menu_sel = ed.menu_sel;
                // 当前光标折叠位（粘贴无选区时的插入点）
                let caret = out.cursor_range.map(|r| r.as_sorted_char_range().start).unwrap_or(0);
                let mut act = 0u8; // 1=复制 2=剪切 3=粘贴 4=全选
                out.response.context_menu(|ui| {
                    ui.set_min_width(150.0); // 菜单不至于太窄
                    let has_sel = menu_sel.is_some();
                    if ui.add_enabled(has_sel, egui::Button::new(crate::i18n::tr("复制", "Copy"))).clicked() {
                        act = 1;
                        ui.close();
                    }
                    if ui.add_enabled(has_sel, egui::Button::new(crate::i18n::tr("剪切", "Cut"))).clicked() {
                        act = 2;
                        ui.close();
                    }
                    if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                        act = 3;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(crate::i18n::tr("全选", "Select all")).clicked() {
                        act = 4;
                        ui.close();
                    }
                });
                if act != 0 {
                    let ctx = ui.ctx().clone();
                    let mut new_cursor: Option<(usize, usize)> = None;
                    match act {
                        1 => {
                            if let Some((c0, c1)) = menu_sel {
                                let (b0, b1) = (char_to_byte(&ed.content, c0), char_to_byte(&ed.content, c1));
                                if b1 > b0 {
                                    ctx.copy_text(ed.content[b0..b1].to_string());
                                }
                            }
                        }
                        2 => {
                            if let Some((c0, c1)) = menu_sel {
                                let (b0, b1) = (char_to_byte(&ed.content, c0), char_to_byte(&ed.content, c1));
                                if b1 > b0 {
                                    ctx.copy_text(ed.content[b0..b1].to_string());
                                    ed.content.replace_range(b0..b1, "");
                                    new_cursor = Some((c0, c0));
                                    ed.last_sel = None;
                                }
                            }
                        }
                        3 => {
                            if let Some(t) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
                                // 有冻结选区则替换它，否则插入到当前光标
                                let (c0, c1) = menu_sel.unwrap_or((caret, caret));
                                let (b0, b1) = (char_to_byte(&ed.content, c0), char_to_byte(&ed.content, c1));
                                ed.content.replace_range(b0..b1, &t);
                                let nc = c0 + t.chars().count();
                                new_cursor = Some((nc, nc));
                                ed.last_sel = None;
                            }
                        }
                        4 => {
                            let n = ed.content.chars().count();
                            new_cursor = Some((0, n));
                            ed.last_sel = if n > 0 { Some((0, n)) } else { None };
                        }
                        _ => {}
                    }
                    ed.menu_sel = None;
                    if let Some((c0, c1)) = new_cursor {
                        let mut st = out.state.clone();
                        st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(c0), CCursor::new(c1))));
                        st.store(&ctx, id);
                    }
                    out.response.request_focus();
                }
            });
        });
    });

    save
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
fn v_recompute(ed: &mut Editor) {
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

    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, ' ')).max(1.0);
    let bg = egui::Color32::from_rgb(252, 252, 250);
    let focused = ui.memory(|m| m.focused() == Some(text_id));
    let page = ((ui.available_height() / row_h).floor() as isize - 2).max(1);
    let lang = ed.language.clone();
    let fsize = mono.size;
    // 右键菜单/查找动作（在闭包外应用，避免借用冲突）
    let mut do_copy = false;
    let mut do_cut = false;
    let mut do_paste = false;
    let mut do_selall = false;

    // ——— 输入（聚焦时）———
    let mut moved = false; // 本帧光标可能移动 → 渲染后滚到可视区
    if focused {
        let events = ui.input(|i| i.events.clone());
        moved = events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Text(_)
                    | egui::Event::Paste(_)
                    | egui::Event::Ime(egui::ImeEvent::Commit(_))
                    | egui::Event::Key { pressed: true, .. }
            )
        });
        for ev in events {
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
                        egui::Key::D if cmd => {
                            // 无选区→选中光标处的词；有选区→跳选下一处相同文本（单选，非多光标）
                            if let Some((a, b)) = v_sel_range(ed) {
                                let needle = ed.content[a..b].to_string();
                                if !needle.is_empty() {
                                    let from = b;
                                    let found = ed.content[from..].find(&needle).map(|o| from + o).or_else(|| ed.content.find(&needle));
                                    if let Some(p) = found {
                                        ed.vsel = Some(p);
                                        ed.vcaret = p + needle.len();
                                        ed.pending_scroll = Some(v_line_of(ed, p));
                                        moved = true;
                                    }
                                }
                            } else if let Some((a, b)) = v_word_range(&ed.content, ed.vcaret) {
                                ed.vsel = Some(a);
                                ed.vcaret = b;
                            }
                        }
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
                });
                if !ed.status.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&ed.status).color(Palette::TEXT_DIM).size(11.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(ed.language.as_str()).color(Palette::TEXT_DIM).size(11.0));
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
                moved = true;
            }
            FindOut::ReplaceOne(a, b) => {
                let rep = ed.replace.clone();
                v_apply(ed, a, b - a, &rep);
                moved = true;
            }
            FindOut::ReplaceAll(newc) => {
                let old = ed.content.len();
                v_apply(ed, 0, old, &newc);
                moved = true;
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
            moved = true;
        }
    }

    // ——— 渲染（仅可见行）———
    let total = ed.vlines.len();
    let digits = total.max(1).to_string().len();
    let gutter_w = (digits as f32 + 1.5) * char_w;
    let content_w = gutter_w + (ed.vmax as f32 + 2.0) * char_w;
    // 内容高度封顶在 f32 安全区：大文件行数巨大时 (总行数×行高) 可达数百万像素，拖到最底部时
    // vp 偏移的 f32 精度只剩约 0.5px，导致抖动/卡顿。封顶后改按「行号」虚拟化、用视口相对坐标绘制，
    // 纵向坐标始终很小、不再丢精度。
    // 末尾额外留 3 行空白：可滚到最后一行之下，避免底部横向滚动条遮住最后一行、也便于操作。
    let pad_rows = 3usize;
    let content_h = ((total + pad_rows) as f32 * row_h).min(2_000_000.0);

    egui::Frame::new().fill(bg).show(ui, |ui| {
        ui.spacing_mut().scroll.floating = false;
        ui.spacing_mut().scroll.foreground_color = false;
        ui.visuals_mut().extreme_bg_color = bg;
        ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_gray(202);
        ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_gray(168);
        ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_gray(140);
        egui::ScrollArea::both().auto_shrink([false, false]).id_salt(text_id).show_viewport(ui, |ui, vp| {
            ui.set_width(content_w);
            ui.set_height(content_h);
            let origin = ui.min_rect().min; // 横向滚动用其 x；纵向改用 clip + 行号映射避免大坐标丢精度
            let clip = ui.clip_rect();
            let view_h = clip.height();
            let max_off = (content_h - view_h).max(1.0);
            let frac = (vp.min.y / max_off).clamp(0.0, 1.0);
            let visible = (view_h / row_h).ceil() as usize + 2;
            let max_top = (total + pad_rows).saturating_sub(visible.saturating_sub(2)); // 最大首行号（含末尾留白）
            let top_line = ((frac * max_top as f32).round() as usize).min(max_top);
            let text_x = origin.x + gutter_w;

            let area = egui::Rect::from_min_size(egui::pos2(origin.x, clip.top()), egui::vec2(content_w.max(ui.available_width()), view_h));
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
            let sel = v_sel_range(ed);
            let caret_line = v_line_of(ed, ed.vcaret); // 当前行高亮
            let unit_cols = match ed.indent { Indent::Spaces(n) => (n as usize).max(1), Indent::Tab => 4 }; // 缩进参考线步长
            let brackets = if focused { bracket_match(&ed.content, ed.vcaret) } else { None }; // 括号匹配高亮
            // 可视区内的查找匹配（克隆出来，避免后续可变借用 ed 冲突）
            let vis_matches: Vec<(usize, usize)> = if ed.show_find && !ed.find.is_empty() {
                let vis_a = ed.vlines.get(top_line).copied().unwrap_or(0);
                let vis_b = ed.vlines.get((top_line + visible).min(total)).copied().unwrap_or(ed.content.len());
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
                let i = top_line + k;
                if i >= total {
                    break;
                }
                let (ls, le) = v_line_range(ed, i);
                let line_full: &str = &ed.content[ls..le]; // 切片，不整行拷贝（超长行整行 to_string 也很贵）
                let y = clip.top() + k as f32 * row_h;
                // 当前行高亮（极淡）：聚焦且无选区时，给光标所在行铺一层很淡的底
                if focused && sel.is_none() && i == caret_line {
                    painter.rect_filled(egui::Rect::from_min_max(egui::pos2(clip.left(), y), egui::pos2(clip.right(), y + row_h)), 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10));
                }
                // 缩进参考线：在各缩进层级之间画很淡的竖线
                {
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
                let seg_a = char_to_byte(line_full, first_col);
                let seg_b = char_to_byte(line_full, first_col + cols_vis);
                let seg = &line_full[seg_a..seg_b];
                let seg_x = text_x + first_col as f32 * char_w;
                let galley = {
                    let mut job = highlight::highlight(seg, &lang, fsize, &[]);
                    job.wrap.max_width = f32::INFINITY;
                    ui.ctx().fonts_mut(|f| f.layout_job(job))
                };
                // 行内字节偏移 → 屏幕 x（窗口外钳制到窗口边缘，超出部分本就不可见）
                let x_of = |lb: usize| -> f32 { seg_x + galley.pos_from_cursor(CCursor::new(byte_to_char(seg, lb.clamp(seg_a, seg_b) - seg_a))).left() };
                // 选区/查找当前项高亮：半透明珊瑚色
                if let Some((sa, sb)) = sel {
                    if sb > ls && sa <= le {
                        let ax = x_of(sa.clamp(ls, le) - ls);
                        let bx = if sb > le { clip.right() } else { x_of(sb.clamp(ls, le) - ls) };
                        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(ax, y), egui::pos2(bx, y + row_h)), 0.0, egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 60));
                    }
                }
                // 行号
                painter.text(egui::pos2(origin.x + gutter_w - char_w * 0.7, y), egui::Align2::RIGHT_TOP, (i + 1).to_string(), mono.clone(), Palette::TEXT_DIM);
                // 正文
                painter.galley(egui::pos2(seg_x, y), galley.clone(), Palette::TEXT);
                // 括号匹配：给光标相邻括号及其匹配括号描边
                if let Some((ba, bb)) = brackets {
                    for &bp in &[ba, bb] {
                        if bp >= ls && bp < le {
                            let bx0 = x_of(bp - ls);
                            let bx1 = x_of(bp + 1 - ls);
                            painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(bx0, y), egui::pos2(bx1, y + row_h)), 2.0, egui::Stroke::new(1.0, Palette::ACCENT), egui::StrokeKind::Inside);
                        }
                    }
                }
                // 查找命中高亮（半透明灰），跳过「当前项」(=选区)避免叠灰盖字。
                for &(ma, mb) in &vis_matches {
                    if sel == Some((ma, mb)) {
                        continue;
                    }
                    if ma < le && mb > ls {
                        let hx0 = x_of(ma.clamp(ls, le) - ls);
                        let hx1 = x_of(mb.clamp(ls, le) - ls);
                        painter.rect_filled(egui::Rect::from_min_max(egui::pos2(hx0, y), egui::pos2(hx1, y + row_h)), 2.0, egui::Color32::from_rgba_unmultiplied(120, 120, 120, 56));
                    }
                }
                // 光标
                if focused && ed.vcaret >= ls && ed.vcaret <= le {
                    let cx = x_of(ed.vcaret - ls);
                    painter.vline(cx, y..=(y + row_h), egui::Stroke::new(1.5, Palette::ACCENT));
                }
            }
            // 行号分割线
            painter.vline(text_x - 3.0, clip.top()..=clip.bottom(), egui::Stroke::new(1.0, Palette::BORDER));
            // 点击 / 拖拽定位光标与选区（行号 = top_line + 视口内行偏移）
            if resp.clicked() || resp.drag_started() || resp.dragged() {
                if resp.clicked() || resp.drag_started() {
                    ui.memory_mut(|m| m.request_focus(text_id));
                }
                if let Some(pos) = resp.interact_pointer_pos() {
                    let k = ((pos.y - clip.top()) / row_h).floor().max(0.0) as usize;
                    let li = (top_line + k).min(total.saturating_sub(1));
                    let (ls, le) = v_line_range(ed, li);
                    // 同样只布局窗口片段（避免在超长行上拖拽选择时每帧整行 layout）
                    let line_full: &str = &ed.content[ls..le];
                    let seg_a = char_to_byte(line_full, first_col);
                    let seg_b = char_to_byte(line_full, first_col + cols_vis);
                    let seg = line_full[seg_a..seg_b].to_string();
                    let seg_x = text_x + first_col as f32 * char_w;
                    let g = ui.ctx().fonts_mut(|f| f.layout_no_wrap(seg.clone(), mono.clone(), Palette::TEXT));
                    let cc = g.cursor_from_pos(egui::vec2(pos.x - seg_x, 0.0)).index;
                    let b = ls + seg_a + char_to_byte(&seg, cc);
                    if resp.drag_started() {
                        ed.vsel = Some(b);
                    } else if resp.dragged() {
                        if ed.vsel.is_none() {
                            ed.vsel = Some(ed.vcaret);
                        }
                    } else {
                        ed.vsel = None;
                    }
                    ed.vcaret = b;
                    ed.vgoal_col = None;
                }
            }
            // 跳转到行等：把目标行居中滚入可视区（无条件，优先于下面的「仅越界才滚」）
            if let Some(tl) = ed.pending_scroll.take() {
                let target_top = tl.saturating_sub(visible / 2);
                let target_off = (target_top as f32 / max_top.max(1) as f32) * max_off;
                let screen_y = origin.y + target_off;
                ui.scroll_to_rect(egui::Rect::from_min_size(egui::pos2(text_x, screen_y), egui::vec2(2.0, view_h)), Some(egui::Align::Min));
            } else if moved {
                // 仅键盘移动光标时滚到可视区（点击/拖拽不滚）。光标行不在可视范围才滚。
                let cl = v_line_of(ed, ed.vcaret);
                if cl < top_line || cl + 2 >= top_line + visible {
                    let target_top = cl.saturating_sub(visible / 4);
                    let target_off = (target_top as f32 / max_top.max(1) as f32) * max_off;
                    let screen_y = origin.y + target_off;
                    ui.scroll_to_rect(egui::Rect::from_min_size(egui::pos2(text_x, screen_y), egui::vec2(2.0, view_h)), Some(egui::Align::Min));
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

/// 写回 TextEdit 光标（c0=c1 即折叠光标）。
fn set_cursor(ctx: &egui::Context, id: egui::Id, c0: usize, c1: usize) {
    if let Some(mut st) = egui::text_edit::TextEditState::load(ctx, id) {
        st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(c0), CCursor::new(c1))));
        st.store(ctx, id);
    }
}

/// 回车自动续进：删掉选区（若有）后插入「换行 + 当前行前导空白」；若行尾是 `{([:` 再加一级缩进。
fn apply_enter(content: &mut String, r: CCursorRange, indent: Indent, ctx: &egui::Context, id: egui::Id) {
    let range = r.as_sorted_char_range();
    let cs = range.start;
    let (bs, be) = (char_to_byte(content, cs), char_to_byte(content, range.end));
    if be > bs {
        content.replace_range(bs..be, "");
    }
    let b = char_to_byte(content, cs); // == bs（删除后）
    let line_start = content[..b].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let lead: String = content[line_start..b].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let before = content[line_start..b].trim_end();
    let mut ins = String::from("\n");
    ins.push_str(&lead);
    if before.ends_with(|c| matches!(c, '{' | '(' | '[' | ':')) {
        ins.push_str(&indent.unit());
    }
    content.insert_str(b, &ins);
    let new_c = cs + ins.chars().count();
    set_cursor(ctx, id, new_c, new_c);
}

/// 行首去掉最多一个缩进单位（一个 tab，或最多 unit_width 个空格），返回删除的字节数。
fn dedent_line(content: &mut String, ls: usize, indent: Indent) -> usize {
    if ls < content.len() && content.as_bytes()[ls] == b'\t' {
        content.remove(ls);
        return 1;
    }
    let w = match indent {
        Indent::Spaces(n) => n,
        Indent::Tab => 4,
    };
    let mut k = 0;
    while k < w && ls + k < content.len() && content.as_bytes()[ls + k] == b' ' {
        k += 1;
    }
    if k > 0 {
        content.replace_range(ls..ls + k, "");
    }
    k
}

/// Tab / Shift+Tab：无选区时插入一级缩进 / 反缩进当前行；有选区时整体缩进 / 反缩进涉及的各行。
fn apply_indent(content: &mut String, r: CCursorRange, indent: Indent, dedent: bool, ctx: &egui::Context, id: egui::Id) {
    let range = r.as_sorted_char_range();
    let (cs, ce) = (range.start, range.end);
    let unit = indent.unit();

    if !dedent && cs == ce {
        // 单光标：原位插入一级缩进
        let b = char_to_byte(content, cs);
        content.insert_str(b, &unit);
        set_cursor(ctx, id, cs + unit.chars().count(), cs + unit.chars().count());
        return;
    }

    // 收集选区涉及的各行行首（无选区时即当前行）
    let bs = char_to_byte(content, cs);
    let be = char_to_byte(content, ce);
    let first_line_start = content[..bs].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let mut line_starts: Vec<usize> = vec![first_line_start];
    let mut search = first_line_start;
    while search < be {
        match content[search..be].find('\n') {
            Some(off) => {
                let ls = search + off + 1;
                if ls < be {
                    line_starts.push(ls);
                    search = ls;
                } else {
                    break;
                }
            }
            None => break,
        }
    }

    // 从后往前改，避免前面行的字节偏移被后续插入/删除影响
    let mut total_delta: i64 = 0;
    for &ls in line_starts.iter().rev() {
        if dedent {
            total_delta -= dedent_line(content, ls, indent) as i64;
        } else {
            content.insert_str(ls, &unit);
            total_delta += unit.len() as i64;
        }
    }
    // 选区调整为「首行行首 → 原选区末尾按字节增量平移」，使整块保持选中
    let new_cs = byte_to_char(content, first_line_start);
    let new_ce_byte = ((be as i64) + total_delta).max(first_line_start as i64) as usize;
    let new_ce = byte_to_char(content, new_ce_byte.min(content.len()));
    set_cursor(ctx, id, new_cs, new_ce);
}

// ——— 小文件(egui)编辑器的注释切换 / 行操作（直接改 content + 写回光标，与 apply_indent 同风格）———
fn apply_comment(content: &mut String, r: CCursorRange, lang: &str, ctx: &egui::Context, id: egui::Id) {
    let prefix = match line_comment(lang) {
        Some(p) => p,
        None => return,
    };
    let range = r.as_sorted_char_range();
    let bs = char_to_byte(content, range.start);
    let be = char_to_byte(content, range.end).max(bs);
    let first = content[..bs].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let mut starts = vec![first];
    let mut s = first;
    loop {
        match content[s..].find('\n') {
            Some(off) => {
                let ls = s + off + 1;
                if ls > be || ls >= content.len() {
                    break;
                }
                starts.push(ls);
                s = ls;
            }
            None => break,
        }
    }
    let line_end = |content: &str, ls: usize| content[ls..].find('\n').map(|o| ls + o).unwrap_or(content.len());
    let all = starts.iter().all(|&ls| {
        let t = content[ls..line_end(content, ls)].trim_start();
        t.is_empty() || t.starts_with(prefix)
    });
    let pfx = format!("{prefix} ");
    let mut delta: i64 = 0;
    for &ls in starts.iter().rev() {
        let le = line_end(content, ls);
        let line = &content[ls..le];
        let indent = line.len() - line.trim_start().len();
        if all {
            let after = &line[indent..];
            if after.starts_with(prefix) {
                let mut rm = prefix.len();
                if after[prefix.len()..].starts_with(' ') {
                    rm += 1;
                }
                content.replace_range(ls + indent..ls + indent + rm, "");
                delta -= rm as i64;
            }
        } else if !line[indent..].is_empty() {
            content.insert_str(ls + indent, &pfx);
            delta += pfx.len() as i64;
        }
    }
    let nc0 = byte_to_char(content, first);
    let nc1 = byte_to_char(content, (((be as i64) + delta).max(first as i64) as usize).min(content.len()));
    set_cursor(ctx, id, nc0, nc1);
}
fn cur_line_bounds(content: &str, r: CCursorRange) -> (usize, usize, usize) {
    let b = char_to_byte(content, r.as_sorted_char_range().start);
    let ls = content[..b].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let le = content[ls..].find('\n').map(|o| ls + o).unwrap_or(content.len());
    (ls, le, b - ls) // 行首字节、行尾字节(不含\n)、列内字节偏移
}
fn apply_delete_line(content: &mut String, r: CCursorRange, ctx: &egui::Context, id: egui::Id) {
    let (ls, le, _) = cur_line_bounds(content, r);
    if le < content.len() {
        content.replace_range(ls..le + 1, "");
    } else if ls > 0 {
        content.replace_range(ls - 1..le, "");
    } else {
        content.replace_range(ls..le, "");
    }
    let nc = byte_to_char(content, ls.min(content.len()));
    set_cursor(ctx, id, nc, nc);
}
fn apply_dup_line(content: &mut String, r: CCursorRange, down: bool, ctx: &egui::Context, id: egui::Id) {
    let (ls, le, col) = cur_line_bounds(content, r);
    let line = content[ls..le].to_string();
    if down {
        content.insert_str(le, &format!("\n{line}"));
        let nc = byte_to_char(content, le + 1 + col);
        set_cursor(ctx, id, nc, nc);
    } else {
        content.insert_str(ls, &format!("{line}\n"));
        let nc = byte_to_char(content, ls + col);
        set_cursor(ctx, id, nc, nc);
    }
}
fn apply_move_line(content: &mut String, r: CCursorRange, up: bool, ctx: &egui::Context, id: egui::Id) {
    let (ls, le, col) = cur_line_bounds(content, r);
    if up {
        if ls == 0 {
            return;
        }
        let prev_ls = content[..ls - 1].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prev = content[prev_ls..ls - 1].to_string();
        let cur = content[ls..le].to_string();
        content.replace_range(prev_ls..le, &format!("{cur}\n{prev}"));
        let nc = byte_to_char(content, (prev_ls + col).min(content.len()));
        set_cursor(ctx, id, nc, nc);
    } else {
        if le >= content.len() {
            return;
        }
        let next_le = content[le + 1..].find('\n').map(|o| le + 1 + o).unwrap_or(content.len());
        let cur = content[ls..le].to_string();
        let next = content[le + 1..next_le].to_string();
        content.replace_range(ls..next_le, &format!("{next}\n{cur}"));
        let nc = byte_to_char(content, (ls + next.len() + 1 + col).min(content.len()));
        set_cursor(ctx, id, nc, nc);
    }
}

fn replace_char_range(text: &mut String, c0: usize, c1: usize, rep: &str) {
    let b0 = text.char_indices().nth(c0).map(|(b, _)| b).unwrap_or(text.len());
    let b1 = text.char_indices().nth(c1).map(|(b, _)| b).unwrap_or(text.len());
    text.replace_range(b0..b1, rep);
}
