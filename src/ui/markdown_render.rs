//! Markdown egui 渲染：视口裁剪 + 高度缓存（做法同 `docx_render`）。

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, Sense, Stroke};

use crate::theme::Palette;
use crate::ui::highlight::{self, LineState};

use super::{Block, Kind, Marker, Preview, Span};

/// 正文字号
const BODY: f32 = 14.0;
/// 代码块字号
const CODE: f32 = 13.0;
/// 每级列表缩进
const INDENT: f32 = 22.0;
/// 每级引用缩进（左侧竖线占位）
const QUOTE_PAD: f32 = 14.0;
/// 内容列最大宽度（过宽的行不利于阅读）
const MAX_COL: f32 = 860.0;
/// 超过该大小的代码块不做高亮（纯等宽文本）
const HL_LIMIT: usize = 64 * 1024;
/// 标题 / 加粗的深色（本应用字体无粗体字重，沿用 docx 视图的「加深」表示强调）
const STRONG: Color32 = Color32::from_rgb(0x17, 0x15, 0x12);
const LINK: Color32 = Color32::from_rgb(0x2f, 0x6f, 0xb3);

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 24.0,
        2 => 20.0,
        3 => 17.0,
        4 => 15.5,
        5 | 6 => 14.5,
        _ => BODY,
    }
}

/// 围栏标注的语言名 → 编辑器高亮用的扩展名。
fn fence_ext(lang: &str) -> &str {
    match lang {
        "rust" => "rs",
        "python" | "python3" => "py",
        "javascript" | "node" => "js",
        "typescript" => "ts",
        "shell" | "console" | "shellsession" => "sh",
        "c++" => "cpp",
        "golang" => "go",
        "kotlin" => "kt",
        "ruby" => "rb",
        "yml" => "yaml",
        other => other,
    }
}

/// 一组行内片段 → LayoutJob，同时给出各链接覆盖的**字符**区间（命中测试用）。
fn spans_job(
    spans: &[Span],
    size: f32,
    strong: bool,
    wrap_w: f32,
) -> (LayoutJob, Vec<(Range<usize>, String)>) {
    let mut job = LayoutJob::default();
    let mut links = Vec::new();
    let mut nchars = 0usize;
    for s in spans {
        let placeholder;
        let text: &str = if s.image {
            placeholder = format!("[{}: {}]", crate::i18n::tr("图片", "image"), s.text);
            &placeholder
        } else {
            &s.text
        };
        let color = if s.link.is_some() {
            LINK
        } else if s.image {
            Palette::TEXT_DIM
        } else if strong || s.bold {
            STRONG
        } else {
            Palette::TEXT
        };
        let mut fmt = if s.code {
            let mut f = TextFormat::simple(FontId::monospace(size * 0.93), color);
            f.background = Palette::PANEL_2;
            f
        } else {
            TextFormat::simple(FontId::proportional(size), color)
        };
        fmt.italics = s.italic;
        if s.strike {
            fmt.strikethrough = Stroke::new(1.0, Palette::TEXT_DIM);
        }
        if s.link.is_some() {
            fmt.underline = Stroke::new(1.0, LINK);
        }
        let n = text.chars().count();
        if let Some(url) = &s.link {
            links.push((nchars..nchars + n, url.clone()));
        }
        nchars += n;
        job.append(text, 0.0, fmt);
    }
    job.wrap.max_width = wrap_w;
    (job, links)
}

/// 代码块 → 带高亮的 LayoutJob（逐行复用编辑器的轻量分词，跨行状态一并延续）。
fn code_job(lang: &str, text: &str) -> LayoutJob {
    let plain = TextFormat::simple(FontId::monospace(CODE), Palette::TEXT);
    let ext = fence_ext(lang);
    let mut job = LayoutJob::default();
    if ext.is_empty() || text.len() > HL_LIMIT {
        job.append(text, 0.0, plain);
    } else {
        let states = highlight::line_states(text, ext);
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                job.append("\n", 0.0, plain.clone());
            }
            let state = states.get(i).copied().unwrap_or(LineState::Normal);
            let lj = highlight::highlight_segment(line, 0..line.len(), ext, CODE, &[], state);
            // 高亮结果必须逐字节覆盖整行；对不上就退回纯文本——宁可没颜色也不能丢字
            let covered: usize = lj.sections.iter().map(|s| s.byte_range.len()).sum();
            if lj.text == line && covered == line.len() {
                for sec in &lj.sections {
                    job.append(&lj.text[sec.byte_range.clone()], 0.0, sec.format.clone());
                }
            } else {
                job.append(line, 0.0, plain.clone());
            }
        }
    }
    job.wrap.max_width = f32::INFINITY;
    job
}

/// 段落：可选中复制；点击链接用系统浏览器打开（仅白名单 scheme）。
fn paragraph(ui: &mut egui::Ui, spans: &[Span], size: f32, strong: bool) {
    // 宽度取整：稳定 LayoutJob 哈希，命中 egui 的 galley 缓存（见 docx_render 同处注释）
    let wrap_w = ui.available_width().floor().max(40.0);
    let (job, links) = spans_job(spans, size, strong, wrap_w);
    // 自己排版后把 galley 交给 Label：命中测试与实际绘制用的是同一份排版结果
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let resp = ui.add(egui::Label::new(galley.clone()).sense(Sense::click()));
    if links.is_empty() {
        return;
    }
    let Some(pos) = resp.hover_pos() else {
        return;
    };
    let local = pos - resp.rect.min;
    // cursor_from_pos 给的是「最近的字符间隙」，不是指针下的字符：指针在间隙右侧时压着的是
    // 间隙后面那个字符，在左侧时是前面那个。直接拿间隙下标去比区间，会把链接两侧相邻
    // 字符的半个身位也算进来（紧挨的两个链接还会点到另一个）。
    let cur = galley.cursor_from_pos(local);
    let gap_x = galley.pos_from_cursor(cur).center().x;
    let under = if local.x >= gap_x {
        Some(cur.index)
    } else {
        cur.index.checked_sub(1)
    };
    // 行尾右侧的空白处也会落到行末那个间隙上：要求指针确实贴着它，免得点空白也算点中
    let near = (gap_x - local.x).abs() <= size;
    let hit = links
        .iter()
        .find(|(r, _)| near && under.is_some_and(|c| r.contains(&c)));
    if let Some((_, url)) = hit {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if resp.clicked() {
            crate::terminal::open_url(url);
        }
        resp.on_hover_text_at_pointer(url.as_str());
    }
}

fn table(ui: &mut egui::Ui, bi: usize, head: &[Vec<Span>], rows: &[Vec<Vec<Span>>]) {
    let cols = rows
        .iter()
        .map(|r| r.len())
        .chain(std::iter::once(head.len()))
        .max()
        .unwrap_or(0);
    if cols == 0 {
        return;
    }
    // 宽表格横向滚动，不把内容列撑出窗口
    egui::ScrollArea::horizontal()
        .id_salt(("md_table_scroll", bi))
        .show(ui, |ui| {
            egui::Frame::new()
                .stroke(Stroke::new(1.0, Palette::BORDER))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    egui::Grid::new(("md_table", bi))
                        .striped(true)
                        .min_col_width(40.0)
                        .spacing([16.0, 5.0])
                        .show(ui, |ui| {
                            let all = std::iter::once(head).chain(rows.iter().map(|r| &r[..]));
                            for (ri, row) in all.enumerate() {
                                for c in 0..cols {
                                    // 折行限宽：长文本不把表格撑出显示边界
                                    let cell = row.get(c).map(|c| &c[..]).unwrap_or(&[]);
                                    let (job, _) = spans_job(cell, 12.5, ri == 0, 320.0);
                                    // 交给 Label 的必须是排好版的 galley：传 LayoutJob 的话
                                    // Label 会按自己的折行模式覆盖掉这里的限宽
                                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                                    ui.label(galley);
                                }
                                ui.end_row();
                            }
                        });
                });
        });
}

/// 渲染单个内容块。
fn render_block(ui: &mut egui::Ui, bi: usize, block: &Block, code_cache: &mut Option<LayoutJob>) {
    let heading = match &block.kind {
        Kind::Para { heading, .. } => *heading,
        _ => 0,
    };
    // 标题上方多留一点空（文首除外），与上一节拉开
    if heading > 0 && bi > 0 {
        ui.add_space(if heading <= 2 { 10.0 } else { 5.0 });
    }
    let left = ui.cursor().left();
    let pad = block.quote as f32 * QUOTE_PAD + block.indent as f32 * INDENT;
    let row = ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(pad);
        let content_top = ui.cursor().top();
        ui.vertical(|ui| match &block.kind {
            Kind::Para {
                spans,
                heading,
                marker,
            } => {
                let size = heading_size(*heading);
                // 列表标记画在缩进留出的那一列里（右对齐贴着正文）
                if let Some(m) = marker {
                    let (text, color) = match m {
                        Marker::Bullet => ("•".to_string(), Palette::TEXT_DIM),
                        Marker::Number(n) => (format!("{n}."), Palette::TEXT_DIM),
                        Marker::Task(true) => {
                            (egui_phosphor::regular::CHECK_SQUARE.to_string(), Palette::OK)
                        }
                        Marker::Task(false) => {
                            (egui_phosphor::regular::SQUARE.to_string(), Palette::TEXT_DIM)
                        }
                    };
                    ui.painter().text(
                        egui::pos2(ui.cursor().left() - 6.0, content_top),
                        egui::Align2::RIGHT_TOP,
                        text,
                        FontId::proportional(size),
                        color,
                    );
                }
                if spans.is_empty() {
                    // 空列表项：占一行高度，标记才有地方落
                    let h = ui.fonts_mut(|f| f.row_height(&FontId::proportional(size)));
                    ui.allocate_space(egui::vec2(1.0, h));
                } else {
                    paragraph(ui, spans, size, *heading > 0);
                }
                // 一、二级标题下加细线（GitHub 风格）
                if matches!(*heading, 1 | 2) {
                    ui.add_space(1.0);
                    let (r, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width().floor(), 1.0),
                        Sense::hover(),
                    );
                    ui.painter()
                        .hline(r.x_range(), r.center().y, Stroke::new(1.0, Palette::BORDER));
                }
            }
            Kind::Code { lang, text } => {
                let job = code_cache.get_or_insert_with(|| code_job(lang, text));
                egui::Frame::new()
                    .fill(Palette::PANEL_2)
                    .stroke(Stroke::new(1.0, Palette::BORDER))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        // 代码不折行：超宽时块内横向滚动
                        ui.set_width(ui.available_width().floor());
                        egui::ScrollArea::horizontal()
                            .id_salt(("md_code_scroll", bi))
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(job.clone()).extend());
                            });
                    });
            }
            Kind::Table { head, rows } => table(ui, bi, head, rows),
            Kind::Rule => {
                let (r, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width().floor(), 9.0),
                    Sense::hover(),
                );
                ui.painter()
                    .hline(r.x_range(), r.center().y, Stroke::new(1.0, Palette::BORDER));
            }
        });
    });
    // 引用：左侧竖线，每层一条；上下各伸出半个块间距，相邻引用块的竖线连成一条
    if block.quote > 0 {
        let half_gap = ui.spacing().item_spacing.y / 2.0;
        let r = row.response.rect;
        for q in 0..block.quote {
            let x = left + q as f32 * QUOTE_PAD + 2.0;
            ui.painter().vline(
                x,
                (r.top() - half_gap)..=(r.bottom() + half_gap),
                Stroke::new(3.0, Palette::BORDER),
            );
        }
    }
}

/// 视口裁剪渲染：屏幕外且已知高度的块直接占位跳过；视口内正常渲染并更新高度缓存。
/// 首帧全量布局，其后滚动只付可见部分成本。
fn render_virtual(ui: &mut egui::Ui, pv: &mut Preview, vp: egui::Rect) {
    let n = pv.blocks.len();
    if pv.heights.len() != n {
        pv.heights.resize(n, 0.0);
    }
    if pv.code_jobs.len() != n {
        pv.code_jobs.resize(n, None);
    }
    ui.spacing_mut().item_spacing.y = 8.0;
    let origin = ui.min_rect().top();
    let full_w = ui.available_width().floor();
    for (bi, block) in pv.blocks.iter().enumerate() {
        let h = pv.heights[bi];
        let top = ui.cursor().top() - origin; // 相对内容起点（与 vp 同坐标系）
        let skip = h > 0.0 && (top + h < vp.min.y - 300.0 || top > vp.max.y + 300.0);
        let before = ui.cursor().top();
        if skip {
            // 屏幕外：按缓存高度占位（±300px 余量防边缘跳动）
            ui.allocate_space(egui::vec2(full_w, h));
        } else {
            render_block(ui, bi, block, &mut pv.code_jobs[bi]);
            pv.heights[bi] = (ui.cursor().top() - before - ui.spacing().item_spacing.y).max(1.0);
        }
    }
}

/// 预览视图入口：竖向滚动 + 居中的限宽内容列。`id_salt` 区分各标签的滚动位置。
pub fn show(ui: &mut egui::Ui, pv: &mut Preview, id_salt: egui::Id) {
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, vp| {
            // 全局关掉了标签文本选中（文件列表里要普通指针）；阅读视图里要能选中复制
            ui.style_mut().interaction.selectable_labels = true;
            // 左对齐内容列 + 两侧留白居中（不能用 vertical_centered，见 doc_view 同处注释）
            let avail = ui.available_width();
            let maxw = (avail - 32.0).clamp(40.0, MAX_COL).floor();
            let pad = ((avail - maxw) / 2.0).max(0.0).floor();
            ui.horizontal_top(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(maxw);
                    ui.add_space(16.0);
                    if pv.blocks.is_empty() {
                        ui.label(
                            RichText::new(crate::i18n::tr("（空文档）", "(empty document)"))
                                .color(Palette::TEXT_DIM)
                                .size(12.0),
                        );
                    } else {
                        render_virtual(ui, pv, vp);
                    }
                    ui.add_space(28.0);
                });
            });
        });
}
