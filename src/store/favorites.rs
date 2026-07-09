use std::collections::HashMap;
use std::path::PathBuf;

use super::paths::{config_dir, write_atomic};

fn favorites_path() -> Option<PathBuf> {
    Some(config_dir()?.join("favorites.json"))
}

fn load_favorites_map() -> HashMap<String, Vec<String>> {
    favorites_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// 读取某服务器的收藏路径列表。
pub fn load_favorites(server: &str) -> Vec<String> {
    load_favorites_map().remove(server).unwrap_or_default()
}

/// 写回某服务器的收藏路径列表（合并进总表后落盘）。
pub fn save_favorites(server: &str, list: &[String]) {
    let Some(path) = favorites_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut map = load_favorites_map();
    if list.is_empty() {
        map.remove(server);
    } else {
        map.insert(server.to_string(), list.to_vec());
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        let _ = write_atomic(&path, &json);
    }
}
