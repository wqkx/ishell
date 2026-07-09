use egui::RichText;

use crate::proto::ConnectConfig;
use crate::store;
use crate::theme::Palette;

use super::{AuthKind, ConnectForm, Mode};

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

    fn delete_confirm_dialog(&mut self, ctx: &egui::Context) {
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

    fn import_select_dialog(&mut self, ctx: &egui::Context) {
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

    fn list_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
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

    fn form_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        ui.set_min_width(340.0);
        let w = 250.0;
        egui::Grid::new("conn_form")
            .num_columns(2)
            .spacing([12.0, 12.0])
            .min_col_width(64.0)
            .show(ui, |ui| {
                ui.label(crate::i18n::tr("名称", "Name"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .desired_width(f32::INFINITY)
                        .hint_text(crate::i18n::tr("便于识别，可留空", "For display, optional")),
                );
                ui.end_row();

                ui.label(crate::i18n::tr("主机", "Host"));
                let host_resp =
                    ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(f32::INFINITY));
                if self.focus_host {
                    host_resp.request_focus();
                    self.focus_host = false;
                }
                ui.end_row();

                ui.label(crate::i18n::tr("端口", "Port"));
                ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label(crate::i18n::tr("用户名", "User"));
                ui.add(egui::TextEdit::singleline(&mut self.username).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label(crate::i18n::tr("分组", "Group"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.group)
                        .desired_width(f32::INFINITY)
                        .hint_text(crate::i18n::tr("可留空，用于归类", "Optional folder")),
                );
                ui.end_row();

                ui.label(crate::i18n::tr("标签", "Tags"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.tags)
                        .desired_width(f32::INFINITY)
                        .hint_text(crate::i18n::tr(
                            "逗号分隔，参与搜索",
                            "Comma-separated, searchable",
                        )),
                );
                ui.end_row();

                ui.label(crate::i18n::tr("认证方式", "Auth"));
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.auth,
                        AuthKind::Password,
                        crate::i18n::tr("密码", "Password"),
                    );
                    ui.selectable_value(
                        &mut self.auth,
                        AuthKind::Key,
                        crate::i18n::tr("私钥", "Key"),
                    );
                    ui.selectable_value(
                        &mut self.auth,
                        AuthKind::Agent,
                        crate::i18n::tr("Agent", "Agent"),
                    );
                    ui.selectable_value(
                        &mut self.auth,
                        AuthKind::Interactive,
                        crate::i18n::tr("交互/2FA", "2FA"),
                    );
                });
                ui.end_row();

                self.auth_fields(ui, w);
            });

        self.forward_agent_section(ui);
        self.jump_host_section(ui, w);
        self.error_section(ui);
        self.credential_notice(ui);
        self.action_buttons(ui, result);
    }

    fn auth_fields(&mut self, ui: &mut egui::Ui, w: f32) {
        match self.auth {
            AuthKind::Agent => {
                ui.label("");
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "使用本机 ssh-agent 中的私钥（先 ssh-add）",
                        "Use keys from local ssh-agent (ssh-add first)",
                    ))
                    .color(Palette::TEXT_DIM)
                    .size(11.0),
                );
                ui.end_row();
            }
            AuthKind::Interactive => {
                ui.label("");
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "登录时按服务器提示逐项输入（支持验证码 / 二次验证）",
                        "Answer server prompts at login (OTP / 2FA)",
                    ))
                    .color(Palette::TEXT_DIM)
                    .size(11.0),
                );
                ui.end_row();
            }
            AuthKind::Password => {
                ui.label(crate::i18n::tr("密码", "Password"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.password)
                        .desired_width(f32::INFINITY)
                        .password(true),
                );
                ui.end_row();
            }
            AuthKind::Key => {
                ui.label(crate::i18n::tr("私钥路径", "Key file"));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.key_path).desired_width(w - 36.0));
                    if ui
                        .button(egui_phosphor::regular::FOLDER_OPEN)
                        .on_hover_text(crate::i18n::tr("浏览选择私钥文件", "Browse for key file"))
                        .clicked()
                    {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_title(crate::i18n::tr("选择私钥文件", "Select key file"))
                            .pick_file()
                        {
                            self.key_path = path.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
                ui.label(crate::i18n::tr("私钥口令", "Passphrase"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.passphrase)
                        .desired_width(f32::INFINITY)
                        .password(true),
                );
                ui.end_row();
            }
        }
    }

    fn forward_agent_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.checkbox(
            &mut self.forward_agent,
            crate::i18n::tr("转发本机 ssh-agent（-A）", "Forward ssh-agent (-A)"),
        )
        .on_hover_text(crate::i18n::tr(
            "让远端任意进程复用本机 agent 的全部私钥，无密钥级限制；仅在完全信任的主机上开启",
            "Lets any remote process use all keys in your local agent with no per-key restriction; enable only on fully trusted hosts",
        ));
        if self.forward_agent {
            ui.label(
                RichText::new(crate::i18n::tr(
                    "⚠ 风险：远端任意进程可使用本机 agent 中的全部私钥，等同于把本地身份交给该主机。",
                    "⚠ Risk: any remote process can use every key in your local agent — equivalent to handing your local identity to that host.",
                ))
                .color(Palette::DANGER)
                .size(11.0),
            );
        }
    }

    fn jump_host_section(&mut self, ui: &mut egui::Ui, w: f32) {
        ui.add_space(8.0);
        ui.checkbox(
            &mut self.use_jump,
            crate::i18n::tr("通过跳板机连接（ProxyJump）", "Via jump host (ProxyJump)"),
        );
        if self.use_jump {
            egui::Grid::new("jump_form")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .min_col_width(64.0)
                .show(ui, |ui| {
                    ui.label(crate::i18n::tr("跳板主机", "Jump host"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.j_host).desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label(crate::i18n::tr("跳板端口", "Jump port"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.j_port).desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label(crate::i18n::tr("跳板用户", "Jump user"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.j_username)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                    ui.label(crate::i18n::tr("跳板认证", "Jump auth"));
                    ui.horizontal(|ui| {
                        ui.selectable_value(
                            &mut self.j_auth,
                            AuthKind::Password,
                            crate::i18n::tr("密码", "Password"),
                        );
                        ui.selectable_value(
                            &mut self.j_auth,
                            AuthKind::Key,
                            crate::i18n::tr("私钥", "Key"),
                        );
                        ui.selectable_value(
                            &mut self.j_auth,
                            AuthKind::Agent,
                            crate::i18n::tr("Agent", "Agent"),
                        );
                    });
                    ui.end_row();
                    self.jump_auth_fields(ui, w);
                });
        }
    }

    fn jump_auth_fields(&mut self, ui: &mut egui::Ui, w: f32) {
        match self.j_auth {
            AuthKind::Interactive => {}
            AuthKind::Agent => {
                ui.label("");
                ui.label(
                    RichText::new(crate::i18n::tr("使用本机 ssh-agent", "Use local ssh-agent"))
                        .color(Palette::TEXT_DIM)
                        .size(11.0),
                );
                ui.end_row();
            }
            AuthKind::Password => {
                ui.label(crate::i18n::tr("跳板密码", "Jump pwd"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.j_password)
                        .desired_width(f32::INFINITY)
                        .password(true),
                );
                ui.end_row();
            }
            AuthKind::Key => {
                ui.label(crate::i18n::tr("跳板私钥", "Jump key"));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.j_key_path).desired_width(w - 36.0),
                    );
                    if ui
                        .button(egui_phosphor::regular::FOLDER_OPEN)
                        .on_hover_text(crate::i18n::tr("浏览选择私钥文件", "Browse for key file"))
                        .clicked()
                    {
                        if let Some(path) = rfd::FileDialog::new()
                            .set_title(crate::i18n::tr("选择跳板机私钥", "Select jump key file"))
                            .pick_file()
                        {
                            self.j_key_path = path.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
                ui.label(crate::i18n::tr("私钥口令", "Passphrase"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.j_passphrase)
                        .desired_width(f32::INFINITY)
                        .password(true),
                );
                ui.end_row();
            }
        }
    }

    fn error_section(&self, ui: &mut egui::Ui) {
        if let Some(err) = &self.error {
            ui.add_space(4.0);
            ui.label(RichText::new(err).color(Palette::DANGER));
        }
    }

    fn credential_notice(&self, ui: &mut egui::Ui) {
        use egui_phosphor::regular as eicon;
        let (ic, txt, col) = if store::key_perms_were_loose() {
            (
                eicon::WARNING_CIRCLE,
                crate::i18n::tr(
                    "本地 key 文件权限曾过宽，已自动收紧为 0600",
                    "Local key file perms were too open; tightened to 0600",
                ),
                Palette::DANGER,
            )
        } else {
            match store::key_storage() {
                store::KeyStorage::Keychain => (
                    eicon::LOCK_KEY,
                    crate::i18n::tr(
                        "密码已加密，主密钥存于系统钥匙串",
                        "Passwords encrypted; master key in system keychain",
                    ),
                    Palette::TEXT_DIM,
                ),
                store::KeyStorage::LocalFile => (
                    eicon::LOCK_KEY,
                    crate::i18n::tr(
                        "密码已加密；系统钥匙串不可用，主密钥存于本地文件 (0600)",
                        "Encrypted; keychain unavailable — master key in local file (0600)",
                    ),
                    Palette::WARN,
                ),
                store::KeyStorage::None => (
                    eicon::WARNING_CIRCLE,
                    crate::i18n::tr(
                        "加密不可用，密码将以明文保存",
                        "Encryption unavailable — passwords stored in plaintext",
                    ),
                    Palette::DANGER,
                ),
            }
        };
        ui.add_space(6.0);
        ui.label(RichText::new(format!("{ic}  {txt}")).color(col).size(11.0));
    }

    fn action_buttons(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        ui.add_space(10.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(format!(
                            "{}  {}",
                            egui_phosphor::regular::PLUGS_CONNECTED,
                            crate::i18n::tr("连接", "Connect")
                        ))
                        .color(egui::Color32::WHITE),
                    )
                    .fill(Palette::ACCENT)
                    .wrap_mode(egui::TextWrapMode::Extend),
                )
                .clicked()
            {
                match self.build() {
                    Ok(cfg) => {
                        self.save_current();
                        *result = Some(cfg);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if ui
                .button(crate::i18n::tr("保存", "Save"))
                .on_hover_text(crate::i18n::tr("保存到快速连接", "Save to quick connect"))
                .clicked()
            {
                match self.build() {
                    Ok(_) => {
                        self.save_current();
                        self.mode = Mode::List;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button(crate::i18n::tr("返回", "Back")).clicked() {
                self.error = None;
                self.mode = Mode::List;
            }
        });
    }
}
