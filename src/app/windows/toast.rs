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
                // 与其它弹出框（对话框、传输浮窗）同一套窗口外观：阴影 + 细边框 + 圆角全来自
                // 全局 style，靠阴影表达层次；底色用 PANEL，比页面底色亮，浮在上面。
                // 不再用橙色描边和前导图标——那种「抢眼」的样式放在转瞬即逝的提示上显得突兀。
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 10))
                    .show(ui, |ui| {
                        ui.label(RichText::new(&msg).color(Palette::TEXT).size(13.0));
                    });
            });
        ctx.request_repaint(); // 维持淡出动画
    }
}
