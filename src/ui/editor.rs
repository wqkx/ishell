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
    search_from: usize,
    status: String,
    /// 大文件只读模式
    read_only: bool,
    /// 只读模式下各行的字节范围（用于虚拟化渲染）
    line_ranges: Vec<(usize, usize)>,
    /// 只读模式下最长行的字节数（估算横向内容宽度，供横向滚动）
    max_line_bytes: usize,
    /// 自动探测到的缩进风格（Tab 键 / 回车续进据此）
    indent: Indent,
    /// 上一帧的非空选区（右键会折叠选区，菜单复制/剪切用这个冻结值）
    last_sel: Option<(usize, usize)>,
    /// 右键打开菜单时冻结的选区
    menu_sel: Option<(usize, usize)>,
}

impl Editor {
    pub fn new(path: String, content: String) -> Self {
        let language = path
            .rsplit_once('.')
            .map(|(_, e)| e.to_lowercase())
            .unwrap_or_else(|| "txt".into());
        let read_only = content.len() > EDIT_LIMIT;
        let line_ranges = if read_only { compute_line_ranges(&content) } else { Vec::new() };
        let max_line_bytes = line_ranges.iter().map(|(s, e)| e - s).max().unwrap_or(0);
        let indent = highlight::detect_indent(&content);
        Self {
            orig: content.clone(),
            path,
            content,
            language,
            find: String::new(),
            replace: String::new(),
            show_find: false,
            search_from: 0,
            status: String::new(),
            read_only,
            line_ranges,
            max_line_bytes,
            indent,
            last_sel: None,
            menu_sel: None,
        }
    }
    /// 切换查找栏（供窗口标签栏的「查找」按钮调用）。
    pub fn toggle_find(&mut self) {
        self.show_find = !self.show_find;
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
}

/// 渲染编辑器内容（工具栏 + 查找栏 + 代码区）。返回 true 表示请求保存。
/// `text_id` 为该编辑器固定的 TextEdit Id（用于关闭时清理其状态/撤销历史）。
pub fn content(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    use egui_phosphor::regular as icon;
    let mut save = false;

    // 大文件：只读、按行虚拟化渲染（仅渲染可见行，内存占用低）
    if ed.read_only {
        let mb = ed.content.len() as f64 / 1_048_576.0;
        ui.horizontal(|ui| {
            // 按钮 + 大文件提示先占右侧，路径在剩余宽度里横向滚动、默认贴右
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(RichText::new(format!("{}  {}", icon::PENCIL_SIMPLE, crate::i18n::tr("改为可编辑", "Make editable"))))
                    .on_hover_text(crate::i18n::tr("大文件编辑会占用较多内存（约文件大小的数倍），关闭后自动释放", "Editing large files uses several× the file size in RAM; freed on close"))
                    .clicked()
                {
                    ed.read_only = false;
                    // 释放只读行索引（编辑模式不再需要）
                    ed.line_ranges = Vec::new();
                }
                ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("（大文件 {mb:.1} MB · 只读）"), crate::i18n::Lang::En => format!("(large {mb:.1} MB · read-only)") }).color(Palette::WARN).size(11.0));
            });
        });
        ui.separator();
        // 手动虚拟化渲染（仅画可见行）。用 show_viewport 而非 show_rows：后者在本窗口里
        // 不会撑满可用高度（内容只占可见行高度、下方留大片空白）。
        let mono = egui::TextStyle::Monospace.resolve(ui.style());
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, ' ')).max(1.0);
        let total = ed.line_ranges.len();
        let digits = total.max(1).to_string().len();
        let gutter_w = (digits as f32 + 1.5) * char_w; // 行号区宽度
        let content_w = gutter_w + (ed.max_line_bytes as f32 + 2.0) * char_w;
        let content_h = total as f32 * row_h;
        let bg = egui::Color32::from_rgb(252, 252, 250); // 近白底，与可编辑模式一致
        egui::Frame::new().fill(bg).show(ui, |ui| {
        egui::ScrollArea::both().auto_shrink([false, false]).show_viewport(ui, |ui, vp| {
            ui.set_width(content_w);
            ui.set_height(content_h);
            let origin = ui.min_rect().min;
            let first = (vp.min.y / row_h).floor().max(0.0) as usize;
            let visible = (vp.height() / row_h).ceil() as usize + 2;
            let last = (first + visible).min(total);
            let painter = ui.painter();
            for i in first..last {
                let y = origin.y + i as f32 * row_h;
                // 行号（右对齐）
                painter.text(
                    egui::pos2(origin.x + gutter_w - char_w * 0.7, y),
                    egui::Align2::RIGHT_TOP,
                    (i + 1).to_string(),
                    mono.clone(),
                    Palette::TEXT_DIM,
                );
                // 正文
                let (s, e) = ed.line_ranges[i];
                painter.text(
                    egui::pos2(origin.x + gutter_w, y),
                    egui::Align2::LEFT_TOP,
                    &ed.content[s..e],
                    mono.clone(),
                    Palette::TEXT,
                );
            }
        });
        });
        return false;
    }

    // 保存/查找按钮已上移到窗口标签栏右侧（对齐主窗口风格）；此处仅保留快捷键。
    if ui.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::S)) {
        save = true;
    }
    if ui.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::F)) {
        ed.show_find = true;
    }

    // 查找/替换栏
    let mut pending_select: Option<(usize, usize)> = None;
    if ed.show_find {
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(Palette::TEXT_DIM));
            ui.add(egui::TextEdit::singleline(&mut ed.find).desired_width(150.0).hint_text(crate::i18n::tr("查找", "Find")));
            if ui.button(crate::i18n::tr("下一个", "Next")).clicked() {
                if let Some((c0, c1)) = find_from(&ed.content, &ed.find, ed.search_from) {
                    pending_select = Some((c0, c1));
                    ed.search_from = c1;
                    ed.status.clear();
                } else if !ed.find.is_empty() {
                    ed.status = crate::i18n::tr("未找到", "Not found").into();
                }
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut ed.replace).desired_width(150.0).hint_text(crate::i18n::tr("替换为", "Replace with")));
            if ui.button(crate::i18n::tr("替换", "Replace")).clicked() {
                let from = ed.search_from.saturating_sub(ed.find.chars().count());
                if let Some((c0, c1)) = find_from(&ed.content, &ed.find, from) {
                    replace_char_range(&mut ed.content, c0, c1, &ed.replace);
                    let nc1 = c0 + ed.replace.chars().count();
                    pending_select = Some((c0, nc1));
                    ed.search_from = nc1;
                }
            }
            if ui.button(crate::i18n::tr("全部替换", "Replace all")).clicked() && !ed.find.is_empty() {
                let n = ed.content.matches(ed.find.as_str()).count();
                ed.content = ed.content.replace(ed.find.as_str(), ed.replace.as_str());
                ed.status = match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已替换 {n} 处"), crate::i18n::Lang::En => format!("Replaced {n}") };
            }
            if !ed.status.is_empty() {
                ui.label(RichText::new(&ed.status).color(Palette::TEXT_DIM).size(11.0));
            }
        });
    }
    ui.separator();

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
            }
        }
    }

    // 代码编辑区（近白背景，显示更清晰；大文件不做高亮/lint）
    let bg = egui::Color32::from_rgb(252, 252, 250); // 近白底
    let do_highlight = ed.content.len() <= HIGHLIGHT_LIMIT;
    // 初级 lint：括号配对（仅对可高亮大小的文件，避免大文件每帧扫描）
    let (err_lines, err_ranges, lint_msg): (std::collections::HashSet<usize>, Vec<std::ops::Range<usize>>, Option<String>) =
        if do_highlight {
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

    // 状态行：缩进风格 + lint 概述
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        ui.label(RichText::new(format!("{} {}", crate::i18n::tr("缩进", "Indent"), ed.indent.label())).color(Palette::TEXT_DIM).size(11.0));
        if let Some(msg) = &lint_msg {
            ui.separator();
            ui.label(RichText::new(msg).color(Palette::DANGER).size(11.0));
        }
    });

    let mono = egui::FontId::monospace(13.0);
    let n_lines = ed.content.split('\n').count().max(1);
    let digits = n_lines.to_string().len();
    let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, '0')).max(1.0);
    let gutter_w = (digits as f32 + 1.5) * char_w; // 行号列宽
    let row_h = ui.ctx().fonts_mut(|f| f.row_height(&mono));
    // 编辑区至少撑满窗口高度（内容不足一屏也填满，便于点击空白定位光标）。
    let fill_rows = (((ui.available_height() - 6.0) / row_h).ceil() as usize).max(8);

    egui::Frame::new().fill(bg).show(ui, |ui| {
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            ui.visuals_mut().extreme_bg_color = bg; // TextEdit 自身背景也用近白，无接缝
            ui.horizontal_top(|ui| {
                ui.add_space(gutter_w + 4.0); // 预留行号列宽度（行号随后按 galley 位置绘制）
                let out = egui::TextEdit::multiline(&mut ed.content)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(fill_rows)
                    .id(text_id)
                    .layouter(&mut layouter)
                    .show(ui);
                let id = out.response.id;

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
                if cur_sel.is_some() {
                    ed.last_sel = cur_sel;
                } else if !out.response.secondary_clicked() {
                    ed.last_sel = None;
                }
                if out.response.secondary_clicked() {
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

/// 计算每行的字节范围（不含换行符），用于只读虚拟化渲染。
fn compute_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            let end = if i > start && text.as_bytes()[i - 1] == b'\r' { i - 1 } else { i };
            out.push((start, end));
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    if out.is_empty() {
        out.push((0, 0));
    }
    out
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

fn find_from(text: &str, query: &str, from_char: usize) -> Option<(usize, usize)> {
    if query.is_empty() {
        return None;
    }
    let from_byte = text.char_indices().nth(from_char).map(|(b, _)| b).unwrap_or(text.len());
    let byte = match text[from_byte..].find(query) {
        Some(r) => from_byte + r,
        None => text.find(query)?,
    };
    let c0 = text[..byte].chars().count();
    let c1 = c0 + query.chars().count();
    Some((c0, c1))
}

fn replace_char_range(text: &mut String, c0: usize, c1: usize, rep: &str) {
    let b0 = text.char_indices().nth(c0).map(|(b, _)| b).unwrap_or(text.len());
    let b1 = text.char_indices().nth(c1).map(|(b, _)| b).unwrap_or(text.len());
    text.replace_range(b0..b1, rep);
}
