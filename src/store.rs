//! 已保存连接的持久化（`~/.config/ishell/connections.json`）。
//!
//! 注意：为便于使用，密码以**明文**保存在用户配置目录。这是个人本地工具的取舍，
//! 若用于多人环境请改为系统密钥环或加密存储。
//!
//! 兼容性：所有字段都带 `#[serde(default)]`，新增字段不会导致旧文件解析失败；
//! 升级后仍能读出旧的账号/密码。解析失败时会把原文件备份为 `.bak`，避免被覆盖丢失。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SavedConnection {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
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
fn default_port() -> u16 {
    22
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".config").join("ishell").join("connections.json"))
}

/// 读取已保存连接列表。
///
/// - 文件不存在 → 空列表
/// - 解析失败 → **先把原文件备份为 `connections.json.bak`**，再返回空（避免后续 `save` 覆盖丢数据）
pub fn load() -> Vec<SavedConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new(); // 文件不存在
    };
    match serde_json::from_str::<Vec<SavedConnection>>(&text) {
        Ok(list) => list,
        Err(e) => {
            log::warn!("connections.json 解析失败：{e}，已备份为 .bak");
            let _ = std::fs::write(path.with_extension("json.bak"), &text);
            Vec::new()
        }
    }
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
