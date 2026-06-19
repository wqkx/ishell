//! 文本编辑器内容渲染：语法高亮（大文件自动关闭高亮以省内存）、查找/替换。
//! 多标签与窗口框架由 app 负责。

use egui::text::CCursor;
use egui::text_selection::CCursorRange;
use egui::RichText;
use egui_extras::syntax_highlighting::{highlight, CodeTheme};

use crate::theme::Palette;

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
}

impl Editor {
    pub fn new(path: String, content: String) -> Self {
        let language = path
            .rsplit_once('.')
            .map(|(_, e)| e.to_lowercase())
            .unwrap_or_else(|| "txt".into());
        let read_only = content.len() > EDIT_LIMIT;
        let line_ranges = if read_only { compute_line_ranges(&content) } else { Vec::new() };
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
}

/// 渲染编辑器内容（工具栏 + 查找栏 + 代码区）。返回 true 表示请求保存。
/// `text_id` 为该编辑器固定的 TextEdit Id（用于关闭时清理其状态/撤销历史）。
pub fn content(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    use egui_phosphor::regular as icon;
    let mut save = false;

    // 大文件：只读、按行虚拟化渲染（仅渲染可见行，内存占用低）
    if ed.read_only {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&ed.path).color(Palette::TEXT_DIM).size(11.0));
            let mb = ed.content.len() as f64 / 1_048_576.0;
            ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("（大文件 {mb:.1} MB · 只读）"), crate::i18n::Lang::En => format!("(large {mb:.1} MB · read-only)") }).color(Palette::WARN).size(11.0));
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
            });
        });
        ui.separator();
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        egui::ScrollArea::both().auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            ed.line_ranges.len(),
            |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for i in range {
                    let (s, e) = ed.line_ranges[i];
                    ui.add(
                        egui::Label::new(RichText::new(&ed.content[s..e]).monospace().color(Palette::TEXT))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                }
            },
        );
        return false;
    }

    // 工具栏：保存 / 查找
    ui.horizontal(|ui| {
        ui.label(RichText::new(&ed.path).color(Palette::TEXT_DIM).size(11.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::FLOPPY_DISK, crate::i18n::tr("保存", "Save"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                save = true;
            }
            if ui.button(RichText::new(format!("{}  {}", icon::MAGNIFYING_GLASS, crate::i18n::tr("查找", "Find")))).clicked() {
                ed.show_find = !ed.show_find;
            }
        });
    });

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

    // 代码编辑区（大文件不做高亮）
    let do_highlight = ed.content.len() <= HIGHLIGHT_LIMIT;
    let theme = CodeTheme::from_style(ui.style());
    let lang = ed.language.clone();
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = if do_highlight {
            highlight(ui.ctx(), ui.style(), &theme, buf.as_str(), &lang)
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

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let out = egui::TextEdit::multiline(&mut ed.content)
            .code_editor()
            .desired_width(f32::INFINITY)
            .desired_rows(24)
            .id(text_id)
            .layouter(&mut layouter)
            .show(ui);
        if let Some((c0, c1)) = pending_select {
            let id = out.response.id;
            let mut st = out.state;
            st.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(c0), CCursor::new(c1))));
            st.store(ui.ctx(), id);
            out.response.request_focus();
        }
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
