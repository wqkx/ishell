use egui::RichText;

use crate::store;
use crate::theme::Palette;

use super::ConnectForm;

impl ConnectForm {
    pub(super) fn delete_confirm_dialog(&mut self, ctx: &egui::Context) {
        if let Some(i) = self.confirm_delete {
            let name = self
                .saved
                .get(i)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let mut close = false;
            egui::Window::new(crate::i18n::tr("确认删除", "Confirm delete"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(300.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::tr("确认删除", "Confirm delete"))
                                .size(15.0)
                                .strong(),
                        );
                        ui.add_space(6.0);
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("确定删除连接「{name}」吗？"),
                            crate::i18n::Lang::En => format!("Delete \"{name}\"?"),
                        });
                    });
                    ui.add_space(12.0);
                    let bw = 80.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(crate::i18n::tr("删除", "Delete"))
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(Palette::DANGER)
                                .min_size(egui::vec2(bw, 0.0)),
                            )
                            .clicked()
                        {
                            if i < self.saved.len() {
                                self.saved.remove(i);
                                if let Err(e) = store::save(&self.saved) {
                                    self.error = Some(e);
                                }
                            }
                            self.sel = None;
                            close = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(crate::i18n::tr("取消", "Cancel"))
                                    .min_size(egui::vec2(bw, 0.0)),
                            )
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            if close {
                self.confirm_delete = None;
            }
        }
    }

    pub(super) fn import_select_dialog(&mut self, ctx: &egui::Context) {
        if self.import_candidates.is_some() {
            let mut do_import = false;
            let mut cancel = false;
            egui::Window::new(crate::i18n::tr("导入 ssh/config", "Import ssh/config"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(400.0);
                    let cands = self.import_candidates.as_mut().unwrap();
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(match crate::i18n::current() {
                                crate::i18n::Lang::Zh => {
                                    format!("发现 {} 台主机，选择要导入的：", cands.len())
                                }
                                crate::i18n::Lang::En => {
                                    format!("{} hosts found — choose to import:", cands.len())
                                }
                            })
                            .color(Palette::TEXT_DIM)
                            .size(12.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let all = cands.iter().all(|(_, s)| *s);
                            if ui
                                .button(if all {
                                    crate::i18n::tr("全不选", "None")
                                } else {
                                    crate::i18n::tr("全选", "All")
                                })
                                .clicked()
                            {
                                let v = !all;
                                for c in cands.iter_mut() {
                                    c.1 = v;
                                }
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (c, sel) in cands.iter_mut() {
                                let auth = match c.auth_kind.as_str() {
                                    "key" => "key",
                                    "agent" => "agent",
                                    "interactive" => "2fa",
                                    _ => "pwd",
                                };
                                ui.checkbox(
                                    sel,
                                    format!(
                                        "{}   {}@{}:{}  · {auth}",
                                        c.name, c.username, c.host, c.port
                                    ),
                                );
                            }
                        });
                    ui.separator();
                    ui.add_space(8.0);
                    let bw = 90.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        let n = cands.iter().filter(|(_, s)| *s).count();
                        let enabled = n > 0;
                        if ui
                            .add_enabled(
                                enabled,
                                egui::Button::new(
                                    RichText::new(match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("导入选中（{n}）"),
                                        crate::i18n::Lang::En => format!("Import ({n})"),
                                    })
                                    .color(egui::Color32::WHITE),
                                )
                                .fill(Palette::ACCENT)
                                .min_size(egui::vec2(bw, 0.0)),
                            )
                            .clicked()
                        {
                            do_import = true;
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
            if do_import {
                self.apply_import();
            } else if cancel {
                self.import_candidates = None;
            }
        }
    }
}
