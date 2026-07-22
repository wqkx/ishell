use egui::RichText;

use crate::proto::ConnectConfig;
use crate::theme::Palette;

use super::{ConnectForm, Mode};

impl ConnectForm {
    pub(super) fn list_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        use egui_phosphor::regular as icon;
        ui.horizontal(|ui| {
            ui.heading(
                RichText::new(crate::i18n::tr("快速连接", "Quick Connect"))
                    .size(18.0)
                    .color(Palette::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!(
                                "{}  {}",
                                icon::PLUS,
                                crate::i18n::tr("新建", "New")
                            ))
                            .color(egui::Color32::WHITE),
                        )
                        .fill(Palette::ACCENT)
                        .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .clicked()
                {
                    self.reset_form();
                    self.mode = Mode::Form;
                    self.focus_host = true;
                }
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!(
                                "{}  {}",
                                icon::DOWNLOAD_SIMPLE,
                                crate::i18n::tr("导入 ssh/config", "Import ssh/config")
                            ))
                            .color(Palette::TEXT),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .on_hover_text(crate::i18n::tr(
                        "从 ~/.ssh/config 导入主机（无密钥默认用 agent）",
                        "Import hosts from ~/.ssh/config",
                    ))
                    .clicked()
                {
                    self.open_import_dialog();
                }
            });
        });
        ui.label(
            RichText::new(crate::i18n::tr(
                "单击选择，双击连接",
                "Click to select, double-click to connect",
            ))
            .color(Palette::TEXT_DIM)
            .size(11.0),
        );
        if let Some(msg) = &self.notice {
            ui.label(RichText::new(msg).color(Palette::OK).size(11.0));
        }
        ui.separator();
        ui.set_min_width(500.0);

        // 固定「本机」入口：始终在最上方，单击直接开本地终端。它不写入 connections.json，
        // 不占用户保存的连接，也不会被误删；点一下即产出一个本机 ConnectConfig。
        {
            let row_h = 36.0;
            let full_w = ui.available_width();
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, 6.0, Palette::PANEL_2);
            }
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |ui| {
                    ui.label(
                        RichText::new(icon::HOUSE)
                            .color(Palette::ACCENT)
                            .size(18.0),
                    );
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::tr("本机", "Local machine"))
                                .color(Palette::TEXT)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(crate::i18n::tr(
                                "打开本机终端（本地 shell，无需 SSH）",
                                "Open a local terminal (local shell, no SSH)",
                            ))
                            .color(Palette::TEXT_DIM)
                            .size(11.0),
                        );
                    });
                },
            );
            if resp
                .on_hover_text(crate::i18n::tr(
                    "在本机直接打开一个终端",
                    "Open a terminal on this computer",
                ))
                .clicked()
            {
                *result = Some(crate::proto::ConnectConfig::local());
            }
            ui.add_space(4.0);
            ui.separator();
        }

        if !self.saved.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(Palette::TEXT_DIM));
                let clear_w = if self.search.is_empty() { 0.0 } else { 22.0 };
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .desired_width(ui.available_width() - clear_w - 4.0)
                        .hint_text(crate::i18n::tr(
                            "搜索名称/主机/用户/分组/标签",
                            "Search name/host/user/group/tags",
                        )),
                );
                if !self.search.is_empty()
                    && ui
                        .add(
                            egui::Button::new(
                                RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM),
                            )
                            .frame(false),
                        )
                        .clicked()
                {
                    self.search.clear();
                }
            });
            ui.add_space(2.0);
        }

        egui::ScrollArea::vertical()
            .max_height(420.0)
            .show(ui, |ui| {
                if self.saved.is_empty() {
                    crate::ui::empty_state(
                        ui,
                        egui_phosphor::regular::PLUGS,
                        crate::i18n::tr(
                            "还没有保存的连接，点击右上角「新建」",
                            "No saved connections. Click \"New\" top-right.",
                        ),
                        true,
                    );
                    return;
                }
                let q = self.search.trim().to_lowercase();
                let mut filtered: Vec<usize> = self
                    .saved
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| {
                        q.is_empty()
                            || c.name.to_lowercase().contains(&q)
                            || c.host.to_lowercase().contains(&q)
                            || c.username.to_lowercase().contains(&q)
                            || c.group.to_lowercase().contains(&q)
                            || c.tags.to_lowercase().contains(&q)
                    })
                    .map(|(i, _)| i)
                    .collect();
                if filtered.is_empty() {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::tr("无匹配", "No match"))
                                .color(Palette::TEXT_DIM),
                        );
                    });
                    return;
                }
                filtered.sort_by_key(|&i| {
                    let c = &self.saved[i];
                    (
                        c.group.is_empty(),
                        c.group.to_lowercase(),
                        c.name.to_lowercase(),
                    )
                });

                let mut connect_idx = None;
                let mut edit_idx = None;
                let mut del_idx = None;
                let mut sel_idx = None;
                let mut cur_group: Option<String> = None;
                for &i in &filtered {
                    let c = &self.saved[i];
                    if cur_group.as_deref() != Some(c.group.as_str()) {
                        cur_group = Some(c.group.clone());
                        let label = if c.group.is_empty() {
                            crate::i18n::tr("未分组", "Ungrouped").to_string()
                        } else {
                            c.group.clone()
                        };
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!("{}  {}", icon::FOLDER, label))
                                .strong()
                                .color(Palette::TEXT_DIM)
                                .size(12.0),
                        );
                    }
                    let selected = self.sel == Some(i);
                    let row_h = 30.0;
                    let full_w = ui.available_width();
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::click());
                    if selected {
                        ui.painter().rect_filled(rect, 6.0, Palette::ACCENT_SOFT);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 6.0, Palette::PANEL_2);
                    }
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| {
                            ui.label(RichText::new(icon::DESKTOP_TOWER).color(Palette::ACCENT));
                            ui.add_space(2.0);
                            ui.label(RichText::new(&c.name).color(Palette::TEXT));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(icon::TRASH).color(Palette::TEXT_DIM),
                                            )
                                            .frame(false),
                                        )
                                        .on_hover_text(crate::i18n::tr("删除", "Delete"))
                                        .clicked()
                                    {
                                        del_idx = Some(i);
                                    }
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(icon::PENCIL_SIMPLE)
                                                    .color(Palette::TEXT_DIM),
                                            )
                                            .frame(false),
                                        )
                                        .on_hover_text(crate::i18n::tr("编辑", "Edit"))
                                        .clicked()
                                    {
                                        edit_idx = Some(i);
                                    }
                                    ui.add_space(10.0);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(80.0, row_h),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(&c.username)
                                                    .color(Palette::TEXT_DIM)
                                                    .size(12.0),
                                            );
                                        },
                                    );
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(150.0, row_h),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(format!("{}:{}", c.host, c.port))
                                                    .color(Palette::TEXT_DIM)
                                                    .size(12.0),
                                            );
                                        },
                                    );
                                },
                            );
                        },
                    );
                    if resp.clicked() {
                        sel_idx = Some(i);
                    }
                    if resp.double_clicked() {
                        connect_idx = Some(i);
                    }
                }
                if let Some(i) = sel_idx {
                    self.sel = Some(i);
                }
                if let Some(i) = connect_idx {
                    self.load_saved(i);
                    if let Ok(cfg) = self.build() {
                        *result = Some(cfg);
                    }
                }
                if let Some(i) = edit_idx {
                    self.load_saved(i);
                    self.mode = Mode::Form;
                    self.focus_host = true;
                }
                if let Some(i) = del_idx {
                    self.confirm_delete = Some(i);
                }
            });
    }
}
