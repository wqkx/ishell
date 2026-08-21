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

/// 在总表上切换一条收藏，返回该服务器**切换后的新列表**。纯函数，便于测试。
pub(super) fn toggle_in_map(
    map: &mut HashMap<String, Vec<String>>,
    server: &str,
    path: &str,
) -> Vec<String> {
    let list = map.entry(server.to_string()).or_default();
    match list.iter().position(|f| f == path) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(path.to_string()),
    }
    let out = list.clone();
    if out.is_empty() {
        map.remove(server);
    }
    out
}

/// 在总表上按**路径**删除一条收藏，返回该服务器删除后的新列表。
///
/// 按路径而不是按下标：下标是相对**调用方那份可能已经过时的快照**算的，别的标签页动过收藏
/// 之后，同一个下标指向的就是另一条了——那会删掉用户没点的那条。
pub(super) fn remove_in_map(
    map: &mut HashMap<String, Vec<String>>,
    server: &str,
    path: &str,
) -> Vec<String> {
    let Some(list) = map.get_mut(server) else {
        return Vec::new();
    };
    list.retain(|f| f != path);
    let out = list.clone();
    if out.is_empty() {
        map.remove(server);
    }
    out
}

/// 切换一条收藏：**读盘 → 改 → 写回**，返回最新列表。
///
/// 为什么不能沿用「改内存里的 Vec 再整个写回去」：同一台服务器可以同时开好几个标签页，
/// 每个标签页在**自己打开的那一刻**各读了一份收藏快照，此后互不知情。用快照整体覆盖磁盘
/// 就是典型的「基于陈旧数据的最后写入者获胜」——A 标签加了四条，B 标签（还拿着两条的旧快照）
/// 再加一条，就把 A 那四条全抹了。用户看到的现象是「明明收藏了五六个，点开只剩两行」。
/// 每次都从盘上读最新的再改，这一类丢失就不可能发生。
pub fn toggle_favorite(server: &str, path: &str) -> Vec<String> {
    let mut map = load_favorites_map();
    let out = toggle_in_map(&mut map, server, path);
    write_map(&map);
    out
}

/// 按路径删除一条收藏：同样走读盘→改→写回，返回最新列表。
pub fn remove_favorite(server: &str, path: &str) -> Vec<String> {
    let mut map = load_favorites_map();
    let out = remove_in_map(&mut map, server, path);
    write_map(&map);
    out
}

fn write_map(map: &HashMap<String, Vec<String>>) {
    let Some(path) = favorites_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = write_atomic(&path, &json);
    }
}

/// 写回某服务器的收藏路径列表（合并进总表后落盘）。
///
/// ⚠ 这是「用调用方的整份列表覆盖该服务器条目」的语义，只适合调用方**确信自己拿的是最新
/// 数据**的场合。UI 里的收藏增删请一律走 [`toggle_favorite`] / [`remove_favorite`]，
/// 它们是读盘→改→写回，不会拿陈旧快照覆盖别的标签页刚加的收藏。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    /// **数据丢失守门人**：同一台服务器开多个标签页时，各自拿着打开那一刻的收藏快照。
    /// 增删必须以**磁盘上的最新总表**为基准做读-改-写，绝不能用自己的旧快照整体覆盖。
    ///
    /// 复现的就是用户报的那个现象：A 标签加了四条，B 标签（还拿着两条的旧快照）再加一条，
    /// 若走「整体覆盖」，A 那四条当场消失——用户点开收藏夹只看到两三行，以为是显示 bug。
    #[test]
    fn a_stale_tab_cannot_wipe_another_tabs_favorites() {
        let srv = "u@h:22";
        // 磁盘初始：两条。两个标签页都在这一刻读了快照。
        let mut disk = map_of(&[(srv, &["/a", "/b"])]);
        let _stale_snapshot_of_tab_b = vec!["/a".to_string(), "/b".to_string()];

        // A 标签陆续加了四条（每次都读-改-写）
        for p in ["/c", "/d", "/e", "/f"] {
            toggle_in_map(&mut disk, srv, p);
        }
        assert_eq!(disk[srv].len(), 6);

        // B 标签此刻才动手加一条。它手里还是那份两条的旧快照，但操作走的是总表。
        let after_b = toggle_in_map(&mut disk, srv, "/g");

        assert_eq!(
            after_b,
            vec!["/a", "/b", "/c", "/d", "/e", "/f", "/g"],
            "陈旧标签页的一次收藏操作把别人加的条目抹掉了"
        );
    }

    /// 按**路径**删除，而不是按下标：下标是相对调用方那份可能过时的快照算的，
    /// 别的标签页动过之后，同一个下标指向的是另一条——那会删掉用户没点的那条。
    #[test]
    fn removal_targets_the_path_not_a_stale_index() {
        let srv = "u@h:22";
        let mut disk = map_of(&[(srv, &["/a", "/b", "/c"])]);
        // 用户在一个旧快照里看到 ["/b", "/c"]，点了第 0 行想删 "/b"
        let left = remove_in_map(&mut disk, srv, "/b");
        assert_eq!(left, vec!["/a", "/c"], "删错了条目");
    }

    /// 只动被点的那台服务器，别的服务器的收藏一条都不能少。
    #[test]
    fn other_servers_are_never_touched() {
        let mut disk = map_of(&[("s1", &["/x", "/y"]), ("s2", &["/p"])]);
        toggle_in_map(&mut disk, "s1", "/z");
        remove_in_map(&mut disk, "s1", "/x");
        assert_eq!(disk["s2"], vec!["/p"], "动 s1 把 s2 的收藏改了");
    }

    /// 取消最后一条收藏后，该服务器的条目整体移除（不留空数组占位），
    /// 且不影响别人。
    #[test]
    fn emptying_a_server_drops_its_entry_only() {
        let mut disk = map_of(&[("s1", &["/only"]), ("s2", &["/p"])]);
        let left = toggle_in_map(&mut disk, "s1", "/only");
        assert!(left.is_empty());
        assert!(!disk.contains_key("s1"));
        assert_eq!(disk["s2"], vec!["/p"]);
    }

    /// 删一条不存在的路径是无操作，不能把别的条目带走。
    #[test]
    fn removing_an_absent_path_is_a_no_op() {
        let srv = "s";
        let mut disk = map_of(&[(srv, &["/a", "/b"])]);
        let left = remove_in_map(&mut disk, srv, "/nope");
        assert_eq!(left, vec!["/a", "/b"]);
    }
}
