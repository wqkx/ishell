//! 终端通知浮层（右上角）：AI CLI 响铃 / OSC 9/777 通知。
//!
//! 多条通知自右上角向下纵排，浮于全部内容之上（Order::Foreground）。
//! 左键点击：跳转到来源会话并移除该条；右键点击：浮层菜单（删除此条 / 删除全部）。

use egui::RichText;

use crate::theme::Palette;

use super::super::App;

impl App {
    pub(in crate::app) fn ai_notice_overlay(&mut self, ctx: &egui::Context) {
        if self.ai_notices.is_empty() {
            return;
        }
        const MAX_VISIBLE: usize = 6;
        let mut jump_to: Option<u64> = None;
        let mut remove_one: Option<usize> = None;
        let mut clear_all = false;

        egui::Area::new(egui::Id::new("ai_notice_overlay"))
            .anchor(egui::Align2::RIGHT_TOP, [-12.0, 40.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(340.0);
                let n = self.ai_notices.len();
                // 最新在上：倒序展示前 MAX_VISIBLE 条
                for i in (0..n.min(MAX_VISIBLE)).rev() {
                    let no = &self.ai_notices[i];
                    let frame = egui::Frame::new()
                        .fill(Palette::PANEL_2)
                        .stroke(egui::Stroke::new(1.0, Palette::ACCENT))
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_width(316.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(egui_phosphor::regular::BELL)
                                        .color(Palette::WARN)
                                        .size(15.0),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "{} · {}",
                                        no.session_title, no.title
                                    ))
                                    .color(Palette::TEXT)
                                    .size(13.0)
                                    .strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new(egui_phosphor::regular::X)
                                                        .size(12.0)
                                                        .color(Palette::TEXT_DIM),
                                                )
                                                .frame(false),
                                            )
                                            .clicked()
                                        {
                                            remove_one = Some(i);
                                        }
                                    },
                                );
                            });
                            if !no.body.is_empty() {
                                let preview: String = no.body.chars().take(80).collect();
                                ui.label(
                                    RichText::new(preview)
                                        .color(Palette::TEXT_DIM)
                                        .size(12.0),
                                );
                            }
                        });
                    let resp = frame.response.interact(egui::Sense::click());
                    if resp.clicked() {
                        jump_to = Some(no.session_uid);
                        remove_one = Some(i);
                    }
                    resp.context_menu(|ui| {
                        ui.set_min_width(140.0);
                        if ui
                            .button(crate::i18n::tr("删除此通知", "Dismiss"))
                            .clicked()
                        {
                            remove_one = Some(i);
                            ui.close();
                        }
                        if ui
                            .button(crate::i18n::tr("删除全部通知", "Dismiss all"))
                            .clicked()
                        {
                            clear_all = true;
                            ui.close();
                        }
                    });
                    ui.add_space(6.0);
                }
                if n > MAX_VISIBLE {
                    ui.label(
                        RichText::new(format!("+{}", n - MAX_VISIBLE))
                            .color(Palette::TEXT_DIM)
                            .size(12.0),
                    );
                }
            });

        if clear_all {
            self.ai_notices.clear();
        }
        if let Some(i) = remove_one {
            if i < self.ai_notices.len() {
                self.ai_notices.remove(i);
            }
        }
        if let Some(uid) = jump_to {
            if let Some(idx) = self.session_idx_by_uid(uid) {
                self.active = Some(idx);
                self.sessions[idx].terminal.request_focus();
            }
        }
    }
}
