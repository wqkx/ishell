use egui::RichText;

use crate::proto::ConnectConfig;
use crate::store;
use crate::theme::Palette;

use super::form_widgets::{key_file_row, note_row, password_row, text_row, text_row_hint};
use super::{AuthKind, ConnectForm, Mode};

impl ConnectForm {
    pub(super) fn form_view(
        &mut self,
        ui: &mut egui::Ui,
        result: &mut Option<ConnectConfig>,
    ) {
        ui.set_min_width(340.0);
        let w = 250.0;
        egui::Grid::new("conn_form")
            .num_columns(2)
            .spacing([12.0, 12.0])
            .min_col_width(64.0)
            .show(ui, |ui| {
                text_row_hint(
                    ui,
                    crate::i18n::tr("名称", "Name"),
                    &mut self.name,
                    crate::i18n::tr("便于识别，可留空", "For display, optional"),
                );

                let host_resp = text_row(ui, crate::i18n::tr("主机", "Host"), &mut self.host);
                if self.focus_host {
                    host_resp.request_focus();
                    self.focus_host = false;
                }

                text_row(ui, crate::i18n::tr("端口", "Port"), &mut self.port);
                text_row(ui, crate::i18n::tr("用户名", "User"), &mut self.username);
                text_row_hint(
                    ui,
                    crate::i18n::tr("分组", "Group"),
                    &mut self.group,
                    crate::i18n::tr("可留空，用于归类", "Optional folder"),
                );
                text_row_hint(
                    ui,
                    crate::i18n::tr("标签", "Tags"),
                    &mut self.tags,
                    crate::i18n::tr("逗号分隔，参与搜索", "Comma-separated, searchable"),
                );

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
                note_row(
                    ui,
                    crate::i18n::tr(
                        "使用本机 ssh-agent 中的私钥（先 ssh-add）",
                        "Use keys from local ssh-agent (ssh-add first)",
                    ),
                );
            }
            AuthKind::Interactive => {
                note_row(
                    ui,
                    crate::i18n::tr(
                        "登录时按服务器提示逐项输入（支持验证码 / 二次验证）",
                        "Answer server prompts at login (OTP / 2FA)",
                    ),
                );
            }
            AuthKind::Password => {
                password_row(ui, crate::i18n::tr("密码", "Password"), &mut self.password);
            }
            AuthKind::Key => {
                key_file_row(
                    ui,
                    crate::i18n::tr("私钥路径", "Key file"),
                    &mut self.key_path,
                    w,
                    crate::i18n::tr("选择私钥文件", "Select key file"),
                );
                password_row(
                    ui,
                    crate::i18n::tr("私钥口令", "Passphrase"),
                    &mut self.passphrase,
                );
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
                    text_row(
                        ui,
                        crate::i18n::tr("跳板主机", "Jump host"),
                        &mut self.j_host,
                    );
                    text_row(
                        ui,
                        crate::i18n::tr("跳板端口", "Jump port"),
                        &mut self.j_port,
                    );
                    text_row(
                        ui,
                        crate::i18n::tr("跳板用户", "Jump user"),
                        &mut self.j_username,
                    );
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
                note_row(
                    ui,
                    crate::i18n::tr("使用本机 ssh-agent", "Use local ssh-agent"),
                );
            }
            AuthKind::Password => {
                password_row(
                    ui,
                    crate::i18n::tr("跳板密码", "Jump pwd"),
                    &mut self.j_password,
                );
            }
            AuthKind::Key => {
                key_file_row(
                    ui,
                    crate::i18n::tr("跳板私钥", "Jump key"),
                    &mut self.j_key_path,
                    w,
                    crate::i18n::tr("选择跳板机私钥", "Select jump key file"),
                );
                password_row(
                    ui,
                    crate::i18n::tr("私钥口令", "Passphrase"),
                    &mut self.j_passphrase,
                );
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
