//! DOCX egui rendering helpers.

use std::collections::HashMap;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText};

use crate::theme::Palette;

use super::{Align, Block, CellPiece, Doc};

/// 渲染单个内容块。
fn render_block(ui: &mut egui::Ui, block: &Block, images: &HashMap<String, egui::TextureHandle>) {
    match block {
        Block::Para {
            spans,
            heading,
            bullet,
            align,
        } => {
            let size = match heading {
                1 => 21.0,
                2 => 18.0,
                3 => 16.0,
                4..=6 => 15.0,
                _ => 14.0,
            };
            let mut job = LayoutJob::default();
            if *bullet {
                job.append(
                    "•  ",
                    0.0,
                    TextFormat::simple(FontId::proportional(size), Palette::TEXT_DIM),
                );
            }
            for s in spans {
                let mut fmt = TextFormat::simple(
                    FontId::proportional(size),
                    if *heading > 0 || s.bold {
                        Color32::from_rgb(0x17, 0x15, 0x12)
                    } else {
                        Palette::TEXT
                    },
                );
                fmt.italics = s.italic;
                if s.underline {
                    fmt.underline = egui::Stroke::new(1.0, Palette::TEXT_DIM);
                }
                job.append(&s.text, 0.0, fmt);
            }
            // 宽度取整：稳定 LayoutJob 哈希，命中 egui 的 galley 缓存（否则每帧微小的
            // 浮点宽度变化会让全部段落每帧重排——大文档卡顿与内存暴涨的主因）
            job.wrap.max_width = ui.available_width().floor().max(40.0);
            // 段落对齐（w:jc）：直接以 Label 为放置单元（不能再包 horizontal——
            // 行容器会占满整行宽，把 top_down 的水平对齐吞掉，全部变成居左）
            let layout = match align {
                Align::Center => egui::Layout::top_down(egui::Align::Center),
                Align::Right => egui::Layout::top_down(egui::Align::Max),
                Align::Left => egui::Layout::top_down(egui::Align::Min),
            };
            ui.with_layout(layout, |ui| {
                ui.label(job);
            });
            if *heading > 0 {
                ui.add_space(3.0);
            }
        }
        Block::Table(rows) => {
            let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            if cols == 0 {
                return;
            }
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    egui::Grid::new(rows.as_ptr())
                        .striped(true)
                        .min_col_width(40.0)
                        .spacing([16.0, 5.0])
                        .show(ui, |ui| {
                            for (ri, r) in rows.iter().enumerate() {
                                for c in 0..cols {
                                    ui.vertical(|ui| {
                                        let Some(cell) = r.get(c) else { return };
                                        for piece in cell {
                                            match piece {
                                                CellPiece::Text(t) => {
                                                    if !t.is_empty() {
                                                        // 折行限宽：长文本不再把表格撑出显示边界
                                                        let color = if ri == 0 {
                                                            Color32::from_rgb(0x17, 0x15, 0x12)
                                                        } else {
                                                            Palette::TEXT
                                                        };
                                                        let job = LayoutJob::simple(
                                                            t.clone(),
                                                            FontId::proportional(12.5),
                                                            color,
                                                            260.0,
                                                        );
                                                        ui.label(job);
                                                    }
                                                }
                                                CellPiece::Image(name) => {
                                                    if let Some(tex) = images.get(name) {
                                                        let size = tex.size_vec2();
                                                        let w = size.x.clamp(1.0, 220.0);
                                                        ui.add(
                                                            egui::Image::new((
                                                                tex.id(),
                                                                egui::vec2(w, w / size.x * size.y),
                                                            ))
                                                            .corner_radius(2.0),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }
                                ui.end_row();
                            }
                        });
                });
        }
        Block::Image { name, w, h } => {
            if let Some(tex) = images.get(name) {
                let orig = tex.size_vec2();
                // 文档指定的显示尺寸优先（HiDPI 文档常缩小放置大图）；未指定按原图
                let (mut dw, mut dh) = if *w > 1.0 && *h > 1.0 {
                    (*w, *h)
                } else {
                    (orig.x, orig.y)
                };
                let maxw = (ui.available_width() - 8.0).max(1.0);
                if dw > maxw {
                    dh *= maxw / dw;
                    dw = maxw;
                }
                ui.add(egui::Image::new((tex.id(), egui::vec2(dw, dh))).corner_radius(2.0));
            } else {
                ui.label(
                    RichText::new(crate::i18n::tr("[图片]", "[image]"))
                        .color(Palette::TEXT_DIM)
                        .size(12.0),
                );
            }
        }
    }
}

/// 提取某内容块的纯文本（查找用；图片块为空串）。
pub fn block_text(block: &Block) -> String {
    match block {
        Block::Para { spans, .. } => spans.iter().map(|s| s.text.as_str()).collect(),
        Block::Table(rows) => {
            let mut out = String::new();
            for r in rows {
                for cell in r {
                    for p in cell {
                        if let CellPiece::Text(t) = p {
                            out.push_str(t);
                            out.push(' ');
                        }
                    }
                }
            }
            out
        }
        Block::Image { .. } => String::new(),
    }
}

/// 视口裁剪渲染（长文档性能核心）：屏幕外且已知高度的块直接占位跳过；
/// 视口内正常渲染并更新高度缓存。首帧全量布局，其后滚动只付可见部分成本。
/// `hilite` = 当前查找命中块（淡染标识）；`scroll_to` = 待滚动目标块（消费后置 None）。
pub fn render_virtual(
    ui: &mut egui::Ui,
    doc: &Doc,
    images: &HashMap<String, egui::TextureHandle>,
    heights: &mut Vec<f32>,
    vp: egui::Rect,
    hilite: Option<usize>,
    scroll_to: &mut Option<usize>,
) {
    if heights.len() != doc.blocks.len() {
        heights.resize(doc.blocks.len(), 0.0);
    }
    ui.spacing_mut().item_spacing.y = 7.0;
    let origin = ui.min_rect().top();
    let full_w = ui.available_width().floor();
    for (bi, block) in doc.blocks.iter().enumerate() {
        let h = heights[bi];
        let top = ui.cursor().top() - origin; // 相对内容起点（与 vp 同坐标系）
        let skip = h > 0.0
            && (top + h < vp.min.y - 300.0 || top > vp.max.y + 300.0)
            && *scroll_to != Some(bi);
        let before = ui.cursor().top();
        if skip {
            // 屏幕外：按缓存高度占位（±300px 余量防边缘跳动）
            ui.allocate_space(egui::vec2(full_w, h));
        } else {
            render_block(ui, block, images);
            heights[bi] = (ui.cursor().top() - before - ui.spacing().item_spacing.y).max(1.0);
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(ui.min_rect().left(), before),
            egui::pos2(ui.min_rect().right(), ui.cursor().top()),
        );
        if *scroll_to == Some(bi) {
            ui.scroll_to_rect(rect, Some(egui::Align::Center));
            *scroll_to = None;
        }
        if hilite == Some(bi) {
            // 当前命中块：淡黄染 + 细边框（α 低不遮字）
            let w = Palette::WARN;
            ui.painter().rect_filled(
                rect.expand2(egui::vec2(4.0, 2.0)),
                4.0,
                egui::Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), 22),
            );
            ui.painter().rect_stroke(
                rect.expand2(egui::vec2(4.0, 2.0)),
                4.0,
                egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), 120),
                ),
                egui::StrokeKind::Outside,
            );
        }
    }
}
