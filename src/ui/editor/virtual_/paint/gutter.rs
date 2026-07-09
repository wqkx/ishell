use super::super::fold::{v_foldable, v_toggle_fold};
use super::RowPaintContext;
use crate::theme::Palette;

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_gutter_row(
    ctx: &mut RowPaintContext<'_>,
    line_idx: usize,
    y: f32,
    is_first: bool,
    folded: bool,
) -> Option<usize> {
    ctx.painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(ctx.clip.left(), y),
            egui::pos2(ctx.clip.left() + ctx.gutter_w, y + ctx.row_h),
        ),
        0.0,
        ctx.bg,
    );

    if !is_first {
        return None;
    }

    // 括号不匹配的行：行号标红（lint）
    let num_col = if ctx.ed.lint_lines.contains(&line_idx) {
        Palette::DANGER
    } else {
        Palette::TEXT_DIM
    };
    ctx.painter.text(
        egui::pos2(ctx.clip.left() + ctx.gutter_w - ctx.char_w * 2.0, y),
        egui::Align2::RIGHT_TOP,
        (line_idx + 1).to_string(),
        ctx.mono.clone(),
        num_col,
    );

    // 折叠箭头（仅代码文件）：已折叠恒显 ▸（强调色）；可折叠仅悬停行号列时显 ▾（弱色）
    if ctx.show_code_aids
        && (folded || (ctx.gutter_hover && v_foldable(ctx.ed, line_idx, ctx.unit_cols)))
    {
        let arect = egui::Rect::from_min_size(
            egui::pos2(ctx.clip.left() + ctx.gutter_w - ctx.char_w * 1.8, y),
            egui::vec2(ctx.char_w * 1.5, ctx.row_h),
        );
        let clicked = ctx
            .ui
            .interact(
                arect,
                ctx.text_id.with(("fold", line_idx)),
                egui::Sense::click(),
            )
            .clicked();
        let (glyph, colr) = if folded {
            (egui_phosphor::regular::CARET_RIGHT, Palette::ACCENT)
        } else {
            (egui_phosphor::regular::CARET_DOWN, Palette::TEXT_DIM)
        };
        ctx.painter.text(
            egui::pos2(arect.center().x, y + ctx.row_h / 2.0),
            egui::Align2::CENTER_CENTER,
            glyph,
            egui::FontId::proportional((ctx.fsize * 0.8).max(9.0)),
            colr,
        );
        if clicked {
            return Some(line_idx);
        }
    }

    None
}

pub(super) fn paint_gutter_separator(painter: &egui::Painter, clip: egui::Rect, gutter_w: f32) {
    painter.vline(
        clip.left() + gutter_w - 3.0,
        clip.top()..=clip.bottom(),
        egui::Stroke::new(1.0, Palette::BORDER),
    );
}

pub(super) fn apply_fold_click(
    ui: &mut egui::Ui,
    ed: &mut super::super::super::Editor,
    line_idx: usize,
    unit_cols: usize,
) {
    v_toggle_fold(ed, line_idx, unit_cols);
    ui.ctx().request_repaint();
}
