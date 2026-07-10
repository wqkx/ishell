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
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(win_width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match self.mode {
                Mode::List => self.list_view(ui, &mut result),
                Mode::Form => self.form_view(ui, &mut result),
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
