//! 顶部居中 toast 浮层。

use egui::RichText;

use crate::theme::Palette;

use super::super::App;

impl App {
    /// 顶部居中浮层提示：数秒后淡出。用于撤销结果等需要醒目反馈的操作。
    pub(in crate::app) fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some((msg, t0)) = self.toast.clone() else {
            return;
        };
        const DUR: f64 = 3.5; // 显示时长（秒）
        const FADE: f64 = 0.6; // 末尾淡出时长
        let now = ctx.input(|i| i.time);
        let age = now - t0;
        if age >= DUR {
            self.toast = None;
            return;
        }
        let alpha = if age > DUR - FADE {
            ((DUR - age) / FADE) as f32
        } else {
            1.0
        }
        .clamp(0.0, 1.0);
        egui::Area::new(egui::Id::new("undo_toast"))
            .anchor(egui::Align2::CENTER_TOP, [0.0, 54.0])
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                egui::Frame::new()
                    .fill(Palette::PANEL_2)
                    .stroke(egui::Stroke::new(1.0, Palette::ACCENT))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(egui_phosphor::regular::INFO)
                                    .color(Palette::ACCENT)
                                    .size(15.0),
                            );
                            ui.label(RichText::new(&msg).color(Palette::TEXT).size(13.0));
                        });
                    });
            });
        ctx.request_repaint(); // 维持淡出动画
    }
}
