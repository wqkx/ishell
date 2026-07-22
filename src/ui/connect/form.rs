use crate::proto::{AuthMethod, ConnectConfig, JumpHost};
use crate::store::{self, SavedConnection};

use super::{AuthKind, ConnectForm};

impl ConnectForm {
    pub(super) fn open_import_dialog(&mut self) {
        let imported = store::import_ssh_config();
        if imported.is_empty() {
            self.notice = Some(
                crate::i18n::tr(
                    "未找到 ~/.ssh/config 或无可导入主机",
                    "No ~/.ssh/config hosts found",
                )
                .into(),
            );
        } else {
            self.import_candidates = Some(imported.into_iter().map(|c| (c, true)).collect());
        }
    }

    pub(super) fn apply_import(&mut self) {
        let Some(cands) = self.import_candidates.take() else {
            return;
        };
        let mut added = 0;
        let mut updated = 0;
        for (c, sel) in cands {
            if !sel {
                continue;
            }
            if let Some(slot) = self
                .saved
                .iter_mut()
                .find(|s| s.name == c.name && s.host == c.host)
            {
                *slot = c;
                updated += 1;
            } else {
                self.saved.push(c);
                added += 1;
            }
        }
        match store::save(&self.saved) {
            Ok(()) => {
                self.notice = Some(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("已导入：新增 {added}，更新 {updated}"),
                    crate::i18n::Lang::En => format!("Imported: {added} new, {updated} updated"),
                });
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub(super) fn reset_form(&mut self) {
        self.name.clear();
        self.host.clear();
        self.port = "22".into();
        self.username = "root".into();
        self.password.clear();
        self.key_path.clear();
        self.passphrase.clear();
        self.auth = AuthKind::Password;
        self.forward_agent = false;
        self.group.clear();
        self.tags.clear();
        self.use_jump = false;
        self.j_host.clear();
        self.j_port = "22".into();
        self.j_username = "root".into();
        self.j_password.clear();
        self.j_key_path.clear();
        self.j_passphrase.clear();
        self.j_auth = AuthKind::Password;
        self.error = None;
        self.editing = None;
    }

    pub(super) fn load_saved(&mut self, i: usize) {
        let c = self.saved[i].clone();
        self.editing = Some((c.name.clone(), c.host.clone()));
        self.name = c.name;
        self.host = c.host;
        self.port = c.port.to_string();
        self.username = c.username;
        self.password = c.password;
        self.key_path = c.key_path;
        self.passphrase = c.passphrase;
        self.auth = match c.auth_kind.as_str() {
            "key" => AuthKind::Key,
            "agent" => AuthKind::Agent,
            "interactive" => AuthKind::Interactive,
            _ => AuthKind::Password,
        };
        self.forward_agent = c.forward_agent;
        self.group = c.group;
        self.tags = c.tags;
        self.use_jump = c.use_jump;
        self.j_host = c.jump_host;
        self.j_port = c.jump_port.to_string();
        self.j_username = c.jump_username;
        self.j_password = c.jump_password;
        self.j_key_path = c.jump_key_path;
        self.j_passphrase = c.jump_passphrase;
        self.j_auth = match c.jump_auth_kind.as_str() {
            "key" => AuthKind::Key,
            "agent" => AuthKind::Agent,
            _ => AuthKind::Password,
        };
        self.error = None;
    }

    pub(super) fn save_current(&mut self) {
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
            auth_kind: match self.auth {
                AuthKind::Key => "key".into(),
                AuthKind::Agent => "agent".into(),
                AuthKind::Interactive => "interactive".into(),
                AuthKind::Password => "password".into(),
            },
            forward_agent: self.forward_agent,
            password: self.password.clone(),
            key_path: self.key_path.trim().to_string(),
            passphrase: self.passphrase.clone(),
            use_jump: self.use_jump,
            jump_host: self.j_host.trim().to_string(),
            jump_port: self.j_port.trim().parse().unwrap_or(22),
            jump_username: self.j_username.trim().to_string(),
            jump_auth_kind: match self.j_auth {
                AuthKind::Key => "key".into(),
                AuthKind::Agent => "agent".into(),
                AuthKind::Interactive => "interactive".into(),
                AuthKind::Password => "password".into(),
            },
            jump_password: self.j_password.clone(),
            jump_key_path: self.j_key_path.trim().to_string(),
            jump_passphrase: self.j_passphrase.clone(),
            group: self.group.trim().to_string(),
            tags: self.tags.trim().to_string(),
        };
        let slot = match &self.editing {
            Some((on, oh)) => self
                .saved
                .iter_mut()
                .find(|c| &c.name == on && &c.host == oh),
            None => self
                .saved
                .iter_mut()
                .find(|c| c.name == name && c.host == entry.host),
        };
        if let Some(slot) = slot {
            *slot = entry.clone();
        } else {
            self.saved.push(entry.clone());
        }
        self.editing = Some((entry.name, entry.host));
        match store::save(&self.saved) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    pub(super) fn build(&self) -> Result<ConnectConfig, String> {
        if self.host.trim().is_empty() {
            return Err(crate::i18n::tr("请填写主机地址", "Enter host").into());
        }
        let port: u16 = self
            .port
            .trim()
            .parse()
            .map_err(|_| crate::i18n::tr("端口非法", "Invalid port").to_string())?;
        if self.username.trim().is_empty() {
            return Err(crate::i18n::tr("请填写用户名", "Enter user").into());
        }
        let auth = match self.auth {
            AuthKind::Password => AuthMethod::Password(self.password.clone()),
            AuthKind::Agent => AuthMethod::Agent,
            AuthKind::Interactive => AuthMethod::Interactive,
            AuthKind::Key => {
                if self.key_path.trim().is_empty() {
                    return Err(crate::i18n::tr("请填写私钥路径", "Enter key file").into());
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
                return Err(crate::i18n::tr("请填写跳板主机地址", "Enter jump host").into());
            }
            let jport: u16 =
                self.j_port.trim().parse().map_err(|_| {
                    crate::i18n::tr("跳板端口非法", "Invalid jump port").to_string()
                })?;
            if self.j_username.trim().is_empty() {
                return Err(crate::i18n::tr("请填写跳板用户名", "Enter jump user").into());
            }
            let jauth = match self.j_auth {
                AuthKind::Password => AuthMethod::Password(self.j_password.clone()),
                AuthKind::Agent => AuthMethod::Agent,
                AuthKind::Interactive => AuthMethod::Interactive,
                AuthKind::Key => {
                    if self.j_key_path.trim().is_empty() {
                        return Err(
                            crate::i18n::tr("请填写跳板私钥路径", "Enter jump key file").into()
                        );
                    }
                    AuthMethod::KeyFile {
                        path: self.j_key_path.trim().to_string(),
                        passphrase: if self.j_passphrase.is_empty() {
                            None
                        } else {
                            Some(self.j_passphrase.clone())
                        },
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
            label: self.name.trim().to_string(),
            jump,
            forward_agent: self.forward_agent,
            transport: crate::proto::Transport::Ssh,
        })
    }
}
