//! 连接表单的网格行小组件。

use egui::RichText;

use crate::theme::Palette;

pub(super) fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) -> egui::Response {
    ui.label(label);
    let resp = ui.add(egui::TextEdit::singleline(value).desired_width(f32::INFINITY));
    ui.end_row();
    resp
}

pub(super) fn text_row_hint(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    hint: &str,
) -> egui::Response {
    ui.label(label);
    let resp = ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .hint_text(hint),
    );
    ui.end_row();
    resp
}

pub(super) fn password_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .password(true),
    );
    ui.end_row();
}

pub(super) fn note_row(ui: &mut egui::Ui, text: &str) {
    ui.label("");
    ui.label(RichText::new(text).color(Palette::TEXT_DIM).size(11.0));
    ui.end_row();
}

pub(super) fn key_file_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    width: f32,
    dialog_title: &str,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(width - 36.0));
        if ui
            .button(egui_phosphor::regular::FOLDER_OPEN)
            .on_hover_text(crate::i18n::tr("浏览选择私钥文件", "Browse for key file"))
            .clicked()
        {
            if let Some(path) = rfd::FileDialog::new().set_title(dialog_title).pick_file() {
                *value = path.to_string_lossy().into_owned();
            }
        }
    });
    ui.end_row();
}
