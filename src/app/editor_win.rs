//! App 的编辑器独立窗口渲染与标签关闭确认（`impl App` 方法，行为不变）。

use egui::RichText;

use crate::theme::Palette;

use super::widgets::*;
use super::App;

impl App {
    /// 关闭活动标签前的二次确认（会话仍连接时）。
    pub(super) fn close_tab_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_close_tab else {
            return;
        };
        // 若该会话已不在或已断开，则无需确认
        let Some(title) = self
            .sessions
            .get(idx)
            .filter(|s| s.connected)
            .map(|s| s.title.clone())
        else {
            self.pending_close_tab = None;
            return;
        };
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("close_tab_modal")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(crate::i18n::tr("关闭会话", "Close session"))
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.label(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("「{title}」仍在连接中，确定关闭吗？"),
                    crate::i18n::Lang::En => format!("\"{title}\" is still connected. Close it?"),
                });
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 72.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(
                    ui,
                    crate::i18n::tr("关闭", "Close"),
                    Some(Palette::DANGER),
                    bw,
                ) {
                    decision = Some(true);
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                self.close_session(idx);
                self.pending_close_tab = None;
            }
            Some(false) => self.pending_close_tab = None,
            None => {}
        }
    }

}
