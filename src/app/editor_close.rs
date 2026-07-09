//! Editor viewport close confirmation. Split from editor_window_view.rs; behavior unchanged.

use egui::RichText;

use crate::theme::Palette;

use super::EditorState;

pub(super) fn handle_editor_viewport_close(vctx: &egui::Context, ed: &mut EditorState) {
    // 原生关闭按钮：若有未保存修改先拦截并确认，否则关闭全部标签
    if vctx.input(|i| i.viewport().close_requested()) {
        if ed.tabs.iter().any(|t| t.editor.dirty()) {
            vctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ed.close_confirm = true;
        } else {
            ed.close_all(vctx);
        }
    }
    if ed.close_confirm {
        let mut do_close = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("editor_close_modal")).show(vctx, |ui| {
            ui.set_width(300.0);
            ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(crate::i18n::tr("关闭编辑器", "Close editor"))
                .size(16.0)
                .strong(),
        );
        ui.add_space(6.0);
        ui.label(crate::i18n::tr(
            "有未保存的修改，确定关闭吗？",
            "Some files have unsaved changes. Close anyway?",
        ));
            });
            ui.add_space(12.0);
            let bw = 80.0;
            let total = bw * 2.0 + ui.spacing().item_spacing.x;
            ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
        if ui
            .add(
                egui::Button::new(
            RichText::new(crate::i18n::tr("关闭", "Close"))
                .color(egui::Color32::WHITE),
                )
                .fill(Palette::DANGER)
                .min_size(egui::vec2(bw, 0.0)),
            )
            .clicked()
        {
            do_close = true;
        }
        if ui
            .add(
                egui::Button::new(crate::i18n::tr("取消", "Cancel"))
            .min_size(egui::vec2(bw, 0.0)),
            )
            .clicked()
        {
            cancel = true;
        }
            });
        });
        if do_close {
            ed.close_all(vctx);
        } else if cancel {
            ed.close_confirm = false;
        }
    }
}
