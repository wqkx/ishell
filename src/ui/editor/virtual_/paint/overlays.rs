use super::super::super::Editor;
use super::super::edit::v_complete_accept;
use super::super::fold::v_lead;
use super::super::geom::{char_to_byte, v_line_range};
use crate::theme::Palette;
use crate::ui::highlight;

pub(super) struct ScrollbarState {
    pub show: bool,
    pub hit: bool,
    pub thumb: Option<(egui::Rect, egui::Color32)>,
}

pub(super) fn vertical_scrollbar(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    text_id: egui::Id,
    clip: egui::Rect,
    nrows: usize,
    pad_rows: usize,
    visible: usize,
    max_top: usize,
) -> ScrollbarState {
    let sb_w = 12.0f32; // 轨道稍宽，便于拖动命中
    let sb_track = egui::Rect::from_min_max(
        egui::pos2(clip.right() - sb_w, clip.top()),
        clip.right_bottom(),
    );
    let total_rows = (nrows + pad_rows).max(1);
    let show = max_top > 0;
    let mut hit = false;
    // 滑块几何/配色留到正文绘制之后再画（否则被后绘的字形盖住）。
    let mut thumb: Option<(egui::Rect, egui::Color32)> = None;
    if show {
        // 滚动条交互必须先于正文 resp 注册、且正文交互区要避开这条右缘（见下 area），
        // 否则同层里后注册、覆盖更广的正文会「盖」在滚动条上，把拖动事件抢走 → 点不到。
        let sb_resp = ui.interact(sb_track, text_id.with("vsb"), egui::Sense::click_and_drag());
        let thumb_h = (sb_track.height() * (visible as f32 / total_rows as f32))
            .clamp(24.0, sb_track.height());
        if sb_resp.dragged() || sb_resp.clicked() {
            if let Some(p) = sb_resp.interact_pointer_pos() {
                // 让指针对准滑块中心：行号 = (指针 - 半个滑块) / 可移动轨道 × max_top
                let f = ((p.y - sb_track.top() - thumb_h / 2.0)
                    / (sb_track.height() - thumb_h).max(1.0))
                .clamp(0.0, 1.0);
                ed.vtop = (f * max_top as f32).round() as usize;
                ui.ctx().request_repaint();
            }
        }
        hit = sb_resp.hovered() || sb_resp.dragged();
        let top_now = ed.vtop.min(max_top);
        let frac = top_now as f32 / max_top as f32;
        let thumb_y = sb_track.top() + (sb_track.height() - thumb_h) * frac;
        let rect = egui::Rect::from_min_size(
            egui::pos2(sb_track.left() + 2.5, thumb_y),
            egui::vec2(sb_w - 5.0, thumb_h),
        );
        let col = if hit {
            egui::Color32::from_rgb(144, 138, 124)
        } else {
            egui::Color32::from_rgb(179, 173, 159)
        };
        thumb = Some((rect, col));
    }

    ScrollbarState { show, hit, thumb }
}

pub(super) fn paint_scrollbar_thumb(
    painter: &egui::Painter,
    thumb: Option<(egui::Rect, egui::Color32)>,
) {
    if let Some((thumb, col)) = thumb {
        painter.rect_filled(thumb, 3.0, col);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_completion_popup(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    text_id: egui::Id,
    clip: egui::Rect,
    gutter_w: f32,
    char_w: f32,
    row_h: f32,
) {
    // 补全弹窗（VSCode 风格）：对齐前缀起点、紧贴编辑行正下方；下方空间不足时
    // 翻到行上方——永不遮挡正在编辑的那一行。扁平卡片：细边框、无浮动大阴影。
    let mut accept_click: Option<usize> = None;
    if let (Some((items, sel, plen)), Some(cpos)) = (&ed.complete, ed.caret_px) {
        let item_h = 20.0f32;
        let pop_h = items.len() as f32 * item_h + 10.0;
        // x 对齐前缀起点（前缀为 ASCII，字节数=字符数），并夹在视口内
        let mut pos = egui::pos2(cpos.x - *plen as f32 * char_w - 6.0, cpos.y + 1.0);
        pos.x = pos.x.clamp(
            clip.left() + gutter_w,
            (clip.right() - 268.0).max(clip.left()),
        );
        if pos.y + pop_h > clip.bottom() {
            pos.y = cpos.y - row_h - pop_h - 1.0; // 放到编辑行上方
        }
        egui::Area::new(text_id.with("complete"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain(false)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(Palette::PANEL)
                    .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                    .corner_radius(4.0)
                    .inner_margin(egui::Margin::symmetric(4, 4))
                    .show(ui, |ui| {
                        ui.set_min_width(260.0);
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);
                        for (idx, w) in items.iter().enumerate() {
                            if ui
                                .selectable_label(
                                    idx == *sel,
                                    egui::RichText::new(w).monospace().size(12.5),
                                )
                                .clicked()
                            {
                                accept_click = Some(idx);
                            }
                        }
                    });
            });
    }
    if let Some(idx) = accept_click {
        v_complete_accept(ed, idx);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_sticky_scope(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    painter: &egui::Painter,
    text_id: egui::Id,
    clip: egui::Rect,
    top_row: usize,
    first_line: usize,
    row_h: f32,
    char_w: f32,
    mono: &egui::FontId,
    gutter_w: f32,
    unit_cols: usize,
    cols_vis: usize,
    lang: &str,
    fsize: f32,
    show_code_aids: bool,
) {
    // 顶部固定显示首个可见行的外层作用域链（按缩进推导，至多 3 行），点击跳转
    let sticky: Vec<usize> = if show_code_aids && top_row > 0 && first_line > 0 {
        let mut chain: Vec<usize> = Vec::new();
        let mut min_lead = v_lead(ed, first_line, unit_cols).unwrap_or(usize::MAX);
        let lo = first_line.saturating_sub(3000);
        let mut l = first_line;
        while l > lo && min_lead > 0 && chain.len() < 3 {
            l -= 1;
            if let Some(d) = v_lead(ed, l, unit_cols) {
                if d < min_lead {
                    chain.push(l);
                    min_lead = d;
                }
            }
        }
        chain.reverse();
        chain
    } else {
        Vec::new()
    };
    for (si, &l) in sticky.iter().enumerate() {
        let y = clip.top() + si as f32 * row_h;
        let row_rect = egui::Rect::from_min_max(
            egui::pos2(clip.left(), y),
            egui::pos2(clip.right(), y + row_h),
        );
        if ui
            .interact(row_rect, text_id.with(("sticky", si)), egui::Sense::click())
            .clicked()
        {
            ed.pending_scroll = Some(l);
            ui.ctx().request_repaint();
        }
        painter.rect_filled(row_rect, 0.0, Palette::PANEL_2);
        let (ls2, le2) = v_line_range(ed, l);
        let line_full = &ed.content[ls2..le2];
        let seg_b2 = char_to_byte(line_full, cols_vis);
        let state = ed
            .hl_states
            .get(l)
            .copied()
            .unwrap_or(highlight::LineState::Normal);
        let mut job = highlight::highlight_segment(line_full, 0..seg_b2, lang, fsize, &[], state);
        job.wrap.max_width = f32::INFINITY;
        let g = ui.ctx().fonts_mut(|f| f.layout_job(job));
        painter.galley(egui::pos2(clip.left() + gutter_w, y), g, Palette::TEXT);
        painter.text(
            egui::pos2(clip.left() + gutter_w - char_w * 2.0, y),
            egui::Align2::RIGHT_TOP,
            (l + 1).to_string(),
            mono.clone(),
            Palette::TEXT_DIM,
        );
    }
    if !sticky.is_empty() {
        let by = clip.top() + sticky.len() as f32 * row_h;
        painter.hline(
            clip.left()..=clip.right(),
            by + 0.5,
            egui::Stroke::new(1.0, Palette::BORDER),
        );
    }
}
