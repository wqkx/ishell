use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::paths::{config_dir, write_atomic};

fn default_true() -> bool {
    true
}

/// 一条命令片段：名称 + 命令文本；`run` 决定发送后是否自动回车执行。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Snippet {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default = "default_true")]
    pub run: bool,
}

fn snippets_path() -> Option<PathBuf> {
    Some(config_dir()?.join("snippets.json"))
}

/// 读取命令片段列表（文件不存在或解析失败均返回空）。
pub fn load_snippets() -> Vec<Snippet> {
    let Some(path) = snippets_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 写回命令片段列表。
pub fn save_snippets(list: &[Snippet]) {
    let Some(path) = snippets_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = write_atomic(&path, &json);
    }
}
