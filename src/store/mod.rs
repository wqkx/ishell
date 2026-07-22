//! 已保存连接的持久化（`~/.config/ishell/connections.json`）。
//!
//! 密码/口令在磁盘上以 **ChaCha20-Poly1305** 加密存储（前缀 `enc:v1:`），密钥随机生成
//! 保存在 `~/.config/ishell/key`（0600）。**内存中始终为明文**，其余代码无需感知加密。
//!
//! 说明：这是「at-rest」加密——能挡住直接读文件偷密码；但密钥就在同机，无法防御
//! 拿到本机完整权限的攻击者（个人工具取舍）。换机时需同时带上 `key` 才能解密。
//!
//! 兼容性：所有字段带 `#[serde(default)]`，新增字段不致旧文件解析失败；读到旧明文密码
//! 会自动改写为密文（迁移）。解析失败时把原文件备份为 `.bak`，避免被覆盖丢失。

mod connections;
mod crypto;
mod favorites;
mod paths;
mod settings;
mod snippets;

pub use connections::{import_ssh_config, load, save, SavedConnection};
#[allow(unused_imports)]
pub use crypto::{decrypt_secret, encrypt_secret, key_perms_were_loose, key_storage, KeyStorage};
pub use favorites::{load_favorites, save_favorites};
pub use settings::{
    load_conflict_policy, load_cursor_line, load_download_dir, load_file_cols, load_force_x11,
    load_lang, load_mcp_consent, load_osc7_consent, load_term_theme, load_zoom, mcp_instance_id,
    mcp_pairing_token, mcp_socket_path,
    save_conflict_policy, save_cursor_line, save_download_dir, save_file_cols, save_force_x11,
    save_lang, save_mcp_consent, save_osc7_consent, save_term_theme, save_zoom,
    take_setting_write_errors,
};
pub use snippets::{load_snippets, save_snippets, Snippet};
