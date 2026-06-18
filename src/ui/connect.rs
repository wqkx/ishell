//! 连接对话框：左侧已保存连接列表，右侧录入主机/端口/用户名 + 密码或私钥。

use egui::RichText;

use crate::proto::{AuthMethod, ConnectConfig, JumpHost};
use crate::store::{self, SavedConnection};
use crate::theme::Palette;

#[derive(Clone, PartialEq)]
enum AuthKind {
    Password,
    Key,
}

#[derive(PartialEq)]
enum Mode {
    /// 快速连接列表（默认）
    List,
    /// 新建 / 编辑表单
    Form,
}

/// 对话框表单状态（在 App 中长期持有）。
pub struct ConnectForm {
    pub open: bool,
    mode: Mode,
    name: String,
    host: String,
    port: String,
    username: String,
    password: String,
    key_path: String,
    passphrase: String,
    auth: AuthKind,
    // —— 跳板机 ——
    use_jump: bool,
    j_host: String,
    j_port: String,
    j_username: String,
    j_password: String,
    j_key_path: String,
    j_passphrase: String,
    j_auth: AuthKind,
    error: Option<String>,
    saved: Vec<SavedConnection>,
    /// 列表中选中的连接（高亮）
    sel: Option<usize>,
    /// 待确认删除的连接索引
    confirm_delete: Option<usize>,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::List,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            username: "root".into(),
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            auth: AuthKind::Password,
            use_jump: false,
            j_host: String::new(),
            j_port: "22".into(),
            j_username: "root".into(),
            j_password: String::new(),
            j_key_path: String::new(),
            j_passphrase: String::new(),
            j_auth: AuthKind::Password,
            error: None,
            saved: store::load(),
            sel: None,
            confirm_delete: None,
        }
    }
}

impl ConnectForm {
    /// 打开对话框：始终回到「快速连接」列表并从磁盘重新加载，确保已保存的连接可见。
    pub fn open_dialog(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        self.saved = store::load();
        self.error = None;
    }

    /// 自检：直接打开到新建表单（仅供截图）。
    pub fn open_form_for_demo(&mut self) {
        self.open = true;
        self.mode = Mode::Form;
        self.reset_form();
    }

    /// 渲染对话框。返回 `Some(config)` 表示用户点击了「连接」且校验通过。
    pub fn show(&mut self, ctx: &egui::Context) -> Option<ConnectConfig> {
        if !self.open {
            return None;
        }
        let mut result = None;
        let mut open = self.open;

        let title = if self.mode == Mode::List { "快速连接" } else { "新建连接" };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match self.mode {
                Mode::List => self.list_view(ui, &mut result),
                Mode::Form => self.form_view(ui, &mut result),
            });

        // 删除确认弹窗
        if let Some(i) = self.confirm_delete {
            let name = self.saved.get(i).map(|c| c.name.clone()).unwrap_or_default();
            let mut close = false;
            egui::Window::new("确认删除")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(260.0);
                    ui.label(format!("确定删除连接「{name}」吗？"));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new("删除").color(egui::Color32::WHITE)).fill(Palette::DANGER))
                            .clicked()
                        {
                            if i < self.saved.len() {
                                self.saved.remove(i);
                                store::save(&self.saved);
                            }
                            self.sel = None;
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.confirm_delete = None;
            }
        }

        if !open {
            self.open = false;
        }
        if result.is_some() {
            self.open = false;
        }
        result
    }

    /// 快速连接列表视图（仿 FinalShell）。
    fn list_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        use egui_phosphor::regular as icon;
        ui.horizontal(|ui| {
            ui.heading(RichText::new("快速连接").size(18.0).color(Palette::TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new(format!("{}  新建", icon::PLUS)).color(egui::Color32::WHITE))
                            .fill(Palette::ACCENT)
                            .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .clicked()
                {
                    self.reset_form();
                    self.mode = Mode::Form;
                }
            });
        });
        ui.label(RichText::new("单击选择，双击连接").color(Palette::TEXT_DIM).size(11.0));
        ui.separator();
        ui.set_min_width(500.0);

        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            if self.saved.is_empty() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("还没有保存的连接，点击右上角「新建」").color(Palette::TEXT_DIM));
                });
                return;
            }
            let mut connect_idx = None;
            let mut edit_idx = None;
            let mut del_idx = None;
            let mut sel_idx = None;
            for (i, c) in self.saved.iter().enumerate() {
                let selected = self.sel == Some(i);
                // 预分配固定行高，整行可点击；高亮仅覆盖本行
                let row_h = 30.0;
                let full_w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::click());
                if selected {
                    ui.painter().rect_filled(rect, 4.0, Palette::ACCENT_SOFT);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 4.0, Palette::PANEL_2);
                }
                // 在该行 rect 内绘制内容（编辑/删除按钮在上层，单独响应点击）
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.label(RichText::new(icon::DESKTOP_TOWER).color(Palette::ACCENT));
                        ui.add_space(2.0);
                        ui.label(RichText::new(&c.name).color(Palette::TEXT));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(egui::Button::new(RichText::new(icon::TRASH).color(Palette::TEXT_DIM)).frame(false)).on_hover_text("删除").clicked() {
                                del_idx = Some(i);
                            }
                            if ui.add(egui::Button::new(RichText::new(icon::PENCIL_SIMPLE).color(Palette::TEXT_DIM)).frame(false)).on_hover_text("编辑").clicked() {
                                edit_idx = Some(i);
                            }
                            ui.add_space(12.0);
                            ui.label(RichText::new(&c.username).color(Palette::TEXT_DIM).size(12.0));
                            ui.add_space(20.0);
                            ui.label(RichText::new(format!("{}:{}", c.host, c.port)).color(Palette::TEXT_DIM).size(12.0));
                        });
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
            }
            if let Some(i) = del_idx {
                self.confirm_delete = Some(i);
            }
        });
    }

    fn reset_form(&mut self) {
        self.name.clear();
        self.host.clear();
        self.port = "22".into();
        self.username = "root".into();
        self.password.clear();
        self.key_path.clear();
        self.passphrase.clear();
        self.auth = AuthKind::Password;
        self.use_jump = false;
        self.j_host.clear();
        self.j_port = "22".into();
        self.j_username = "root".into();
        self.j_password.clear();
        self.j_key_path.clear();
        self.j_passphrase.clear();
        self.j_auth = AuthKind::Password;
        self.error = None;
    }

    /// 新建 / 编辑表单视图。
    fn form_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        ui.set_min_width(380.0);
        let w = 250.0; // 输入框宽度
        egui::Grid::new("conn_form")
            .num_columns(2)
            .spacing([12.0, 12.0])
            .min_col_width(64.0)
            .show(ui, |ui| {
                ui.label("名称");
                ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(w).hint_text("便于识别，可留空"));
                ui.end_row();

                ui.label("主机");
                ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(w));
                ui.end_row();

                ui.label("端口");
                ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(w));
                ui.end_row();

                ui.label("用户名");
                ui.add(egui::TextEdit::singleline(&mut self.username).desired_width(w));
                ui.end_row();

                ui.label("认证方式");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.auth, AuthKind::Password, "密码");
                    ui.selectable_value(&mut self.auth, AuthKind::Key, "私钥");
                });
                ui.end_row();

                match self.auth {
                    AuthKind::Password => {
                        ui.label("密码");
                        ui.add(egui::TextEdit::singleline(&mut self.password).desired_width(w).password(true));
                        ui.end_row();
                    }
                    AuthKind::Key => {
                        ui.label("私钥路径");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.key_path).desired_width(w - 36.0));
                            if ui
                                .button(egui_phosphor::regular::FOLDER_OPEN)
                                .on_hover_text("浏览选择私钥文件")
                                .clicked()
                            {
                                if let Some(path) = rfd::FileDialog::new().set_title("选择私钥文件").pick_file() {
                                    self.key_path = path.to_string_lossy().into_owned();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("私钥口令");
                        ui.add(egui::TextEdit::singleline(&mut self.passphrase).desired_width(w).password(true));
                        ui.end_row();
                    }
                }
            });

        ui.add_space(8.0);
        ui.checkbox(&mut self.use_jump, "通过跳板机连接（ProxyJump）");
        if self.use_jump {
            egui::Grid::new("jump_form")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .min_col_width(64.0)
                .show(ui, |ui| {
                    ui.label("跳板主机");
                    ui.add(egui::TextEdit::singleline(&mut self.j_host).desired_width(w));
                    ui.end_row();
                    ui.label("跳板端口");
                    ui.add(egui::TextEdit::singleline(&mut self.j_port).desired_width(w));
                    ui.end_row();
                    ui.label("跳板用户");
                    ui.add(egui::TextEdit::singleline(&mut self.j_username).desired_width(w));
                    ui.end_row();
                    ui.label("跳板认证");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.j_auth, AuthKind::Password, "密码");
                        ui.selectable_value(&mut self.j_auth, AuthKind::Key, "私钥");
                    });
                    ui.end_row();
                    match self.j_auth {
                        AuthKind::Password => {
                            ui.label("跳板密码");
                            ui.add(egui::TextEdit::singleline(&mut self.j_password).desired_width(w).password(true));
                            ui.end_row();
                        }
                        AuthKind::Key => {
                            ui.label("跳板私钥");
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.j_key_path).desired_width(w - 36.0));
                                if ui.button(egui_phosphor::regular::FOLDER_OPEN).on_hover_text("浏览选择私钥文件").clicked() {
                                    if let Some(path) = rfd::FileDialog::new().set_title("选择跳板机私钥").pick_file() {
                                        self.j_key_path = path.to_string_lossy().into_owned();
                                    }
                                }
                            });
                            ui.end_row();
                            ui.label("私钥口令");
                            ui.add(egui::TextEdit::singleline(&mut self.j_passphrase).desired_width(w).password(true));
                            ui.end_row();
                        }
                    }
                });
        }

        if let Some(err) = &self.error {
            ui.add_space(4.0);
            ui.label(RichText::new(err).color(Palette::DANGER));
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.add(
                egui::Button::new(RichText::new(format!("{}  连接", egui_phosphor::regular::PLUGS_CONNECTED)).color(egui::Color32::WHITE))
                    .fill(Palette::ACCENT)
                    .wrap_mode(egui::TextWrapMode::Extend),
            ).clicked() {
                match self.build() {
                    Ok(cfg) => *result = Some(cfg),
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button("保存").on_hover_text("保存到快速连接").clicked() {
                match self.build() {
                    Ok(_) => {
                        self.save_current();
                        self.mode = Mode::List;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button("返回").clicked() {
                self.error = None;
                self.mode = Mode::List;
            }
        });
    }

    fn load_saved(&mut self, i: usize) {
        let c = self.saved[i].clone();
        self.name = c.name;
        self.host = c.host;
        self.port = c.port.to_string();
        self.username = c.username;
        self.password = c.password;
        self.key_path = c.key_path;
        self.passphrase = c.passphrase;
        self.auth = if c.auth_kind == "key" { AuthKind::Key } else { AuthKind::Password };
        self.use_jump = c.use_jump;
        self.j_host = c.jump_host;
        self.j_port = c.jump_port.to_string();
        self.j_username = c.jump_username;
        self.j_password = c.jump_password;
        self.j_key_path = c.jump_key_path;
        self.j_passphrase = c.jump_passphrase;
        self.j_auth = if c.jump_auth_kind == "key" { AuthKind::Key } else { AuthKind::Password };
        self.error = None;
    }

    /// 把当前表单保存/更新到列表（按 名称+host 去重）。
    fn save_current(&mut self) {
        let name = if self.name.trim().is_empty() {
            format!("{}@{}", self.username.trim(), self.host.trim())
        } else {
            self.name.trim().to_string()
        };
        let entry = SavedConnection {
            name: name.clone(),
            host: self.host.trim().to_string(),
            port: self.port.trim().parse().unwrap_or(22),
            username: self.username.trim().to_string(),
            auth_kind: if self.auth == AuthKind::Key { "key".into() } else { "password".into() },
            password: self.password.clone(),
            key_path: self.key_path.trim().to_string(),
            passphrase: self.passphrase.clone(),
            use_jump: self.use_jump,
            jump_host: self.j_host.trim().to_string(),
            jump_port: self.j_port.trim().parse().unwrap_or(22),
            jump_username: self.j_username.trim().to_string(),
            jump_auth_kind: if self.j_auth == AuthKind::Key { "key".into() } else { "password".into() },
            jump_password: self.j_password.clone(),
            jump_key_path: self.j_key_path.trim().to_string(),
            jump_passphrase: self.j_passphrase.clone(),
        };
        if let Some(slot) = self.saved.iter_mut().find(|c| c.name == name && c.host == entry.host) {
            *slot = entry;
        } else {
            self.saved.push(entry);
        }
        store::save(&self.saved);
        self.error = None;
    }

    fn build(&self) -> Result<ConnectConfig, String> {
        if self.host.trim().is_empty() {
            return Err("请填写主机地址".into());
        }
        let port: u16 = self.port.trim().parse().map_err(|_| "端口非法".to_string())?;
        if self.username.trim().is_empty() {
            return Err("请填写用户名".into());
        }
        let auth = match self.auth {
            AuthKind::Password => AuthMethod::Password(self.password.clone()),
            AuthKind::Key => {
                if self.key_path.trim().is_empty() {
                    return Err("请填写私钥路径".into());
                }
                AuthMethod::KeyFile {
                    path: self.key_path.trim().to_string(),
                    passphrase: if self.passphrase.is_empty() {
                        None
                    } else {
                        Some(self.passphrase.clone())
                    },
                }
            }
        };
        let jump = if self.use_jump {
            if self.j_host.trim().is_empty() {
                return Err("请填写跳板主机地址".into());
            }
            let jport: u16 = self.j_port.trim().parse().map_err(|_| "跳板端口非法".to_string())?;
            if self.j_username.trim().is_empty() {
                return Err("请填写跳板用户名".into());
            }
            let jauth = match self.j_auth {
                AuthKind::Password => AuthMethod::Password(self.j_password.clone()),
                AuthKind::Key => {
                    if self.j_key_path.trim().is_empty() {
                        return Err("请填写跳板私钥路径".into());
                    }
                    AuthMethod::KeyFile {
                        path: self.j_key_path.trim().to_string(),
                        passphrase: if self.j_passphrase.is_empty() { None } else { Some(self.j_passphrase.clone()) },
                    }
                }
            };
            Some(JumpHost {
                host: self.j_host.trim().to_string(),
                port: jport,
                username: self.j_username.trim().to_string(),
                auth: jauth,
            })
        } else {
            None
        };
        Ok(ConnectConfig {
            host: self.host.trim().to_string(),
            port,
            username: self.username.trim().to_string(),
            auth,
            jump,
        })
    }
}
