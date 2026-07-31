use crate::proto::ConnectConfig;

use super::{ConnectForm, Mode};

impl ConnectForm {
    /// 渲染对话框。返回 `Some(config)` 表示用户点击了「连接」且校验通过。
    pub fn show(&mut self, ctx: &egui::Context) -> Option<ConnectConfig> {
        if !self.open {
            return None;
        }
        let mut result = None;
        let mut open = self.open;

        let title = if self.mode == Mode::List {
            crate::i18n::tr("快速连接", "Quick Connect")
        } else {
            crate::i18n::tr("新建连接", "New Connection")
        };
        let win_width = if self.mode == Mode::List {
            520.0
        } else {
            380.0
        };
        // 自绘标题栏，不用 `Window::open` 的内置关闭按钮：那个 ✕ 的判定区就是图标本身那
        // 一小块，必须点正中心才关得掉。这里给它一块明确的 26×26 按钮区域（图标仍是小的，
        // 只是可点范围变大），并与「传输浮窗」的自定义紧凑标题栏保持同一做法。
        egui::Window::new(title)
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(win_width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .strong()
                            .size(crate::theme::FS_TITLE)
                            .color(crate::theme::Palette::TEXT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn = ui.add_sized(
                            egui::vec2(26.0, 26.0),
                            egui::Button::new(
                                egui::RichText::new(egui_phosphor::regular::X)
                                    .size(13.0)
                                    .color(crate::theme::Palette::TEXT_DIM),
                            )
                            .frame(false),
                        );
                        if btn.clicked() {
                            open = false;
                        }
                    });
                });
                ui.add_space(2.0);
                match self.mode {
                    Mode::List => self.list_view(ui, &mut result),
                    Mode::Form => self.form_view(ui, &mut result),
                }
            });

        self.delete_confirm_dialog(ctx);
        self.import_select_dialog(ctx);

        if !open {
            self.open = false;
        }
        if result.is_some() {
            self.open = false;
        }
        result
    }
}
