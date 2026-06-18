//! 已保存连接的持久化（`~/.config/ishell/connections.json`）。
//!
//! 注意：为便于使用，密码以**明文**保存在用户配置目录。这是个人本地工具的取舍，
//! 若用于多人环境请改为系统密钥环或加密存储。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedConnection {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// "password" 或 "key"
    #[serde(default = "default_auth")]
    pub auth_kind: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub key_path: String,
    #[serde(default)]
    pub passphrase: String,
}

fn default_auth() -> String {
    "password".into()
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".config").join("ishell").join("connections.json"))
}

/// 读取已保存连接列表（文件不存在或损坏时返回空）。
pub fn load() -> Vec<SavedConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写回连接列表。
pub fn save(list: &[SavedConnection]) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(&path, json);
    }
}
