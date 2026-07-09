use egui::RichText;

use super::super::{join_path, parent_of, FileAction, FilePanelState, UP_DWELL, UP_FLASH};
use crate::proto::FileEntry;
use crate::theme::Palette;

/// 右键菜单产生的请求（在表格遍历后统一处理，避免在遍历中借用 state）。
pub(super) enum Menu {
    Download(usize),
    CopyPath(String),
    Chmod {
        path: String,
        mode: u32,
        name: String,
    },
    Rename {
        path: String,
        name: String,
    },
    Delete(usize),
    /// 复制 / 剪切右键项（含多选）到剪贴板
    Copy(usize),
    Cut(usize),
    /// 粘贴剪贴板内容到当前目录
    Paste,
    NewDir,
    NewFile,
    CdHere(String),
    /// 收藏该文件夹路径
    Favorite(String),
}

/// 条目右键菜单：把用户选择记录到 `menu`。
pub(super) fn entry_context(
    resp: &egui::Response,
    e: &FileEntry,
    idx: usize,
    full: &str,
    has_clip: bool,
    is_fav: bool,
    menu: &mut Vec<Menu>,
) {
    use egui_phosphor::regular as icon;
    resp.context_menu(|ui| {
        ui.set_min_width(178.0); // 菜单宽度（较前一版 210 收窄约 15%）
        if ui
            .button(format!(
                "{}  {}",
                icon::FOLDER_PLUS,
                crate::i18n::tr("新建文件夹", "New folder")
            ))
            .clicked()
        {
            menu.push(Menu::NewDir);
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::FILE_PLUS,
                crate::i18n::tr("新建文件", "New file")
            ))
            .clicked()
        {
            menu.push(Menu::NewFile);
            ui.close();
        }
        ui.separator();
        let dl_label = if e.is_dir {
            crate::i18n::tr("下载文件夹", "Download folder")
        } else {
            crate::i18n::tr("下载", "Download")
        };
        if ui
            .button(format!("{}  {}", icon::DOWNLOAD_SIMPLE, dl_label))
            .clicked()
        {
            menu.push(Menu::Download(idx));
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::COPY,
                crate::i18n::tr("复制路径", "Copy path")
            ))
            .clicked()
        {
            menu.push(Menu::CopyPath(full.to_string()));
            ui.close();
        }
        ui.separator();
        // 复制 / 剪切 到剪贴板（含多选）；粘贴到当前目录
        if ui
            .button(format!(
                "{}  {}",
                icon::COPY_SIMPLE,
                crate::i18n::tr("复制", "Copy")
            ))
            .clicked()
        {
            menu.push(Menu::Copy(idx));
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::SCISSORS,
                crate::i18n::tr("剪切", "Cut")
            ))
            .clicked()
        {
            menu.push(Menu::Cut(idx));
            ui.close();
        }
        if has_clip
            && ui
                .button(format!(
                    "{}  {}",
                    icon::CLIPBOARD_TEXT,
                    crate::i18n::tr("粘贴到此目录", "Paste here")
                ))
                .clicked()
        {
            menu.push(Menu::Paste);
            ui.close();
        }
        ui.separator();
        if e.is_dir {
            let lbl = if is_fav {
                format!(
                    "★  {}",
                    crate::i18n::tr("取消收藏该文件夹", "Remove bookmark")
                )
            } else {
                format!("☆  {}", crate::i18n::tr("收藏该文件夹", "Bookmark folder"))
            };
            if ui.button(lbl).clicked() {
                menu.push(Menu::Favorite(full.to_string()));
                ui.close();
            }
        }
        if e.is_dir
            && ui
                .button(format!(
                    "{}  {}",
                    icon::TERMINAL_WINDOW,
                    crate::i18n::tr("在终端打开此目录", "Open in terminal")
                ))
                .clicked()
        {
            menu.push(Menu::CdHere(full.to_string()));
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::LOCK_KEY,
                crate::i18n::tr("改权限", "Chmod")
            ))
            .clicked()
        {
            menu.push(Menu::Chmod {
                path: full.to_string(),
                mode: e.perm,
                name: e.name.clone(),
            });
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::PENCIL_SIMPLE,
                crate::i18n::tr("重命名", "Rename")
            ))
            .clicked()
        {
            menu.push(Menu::Rename {
                path: full.to_string(),
                name: e.name.clone(),
            });
            ui.close();
        }
        ui.separator();
        if ui
            .button(
                RichText::new(format!(
                    "{}  {}",
                    icon::TRASH,
                    crate::i18n::tr("删除", "Delete")
                ))
                .color(Palette::DANGER),
            )
            .clicked()
        {
            menu.push(Menu::Delete(idx));
            ui.close();
        }
    });
}

/// 拖拽载荷：被拖动的源绝对路径列表（egui 内部 Arc 持有，需 Send + Sync）。
#[derive(Clone)]
pub(in crate::ui::file_panel) struct DragPaths(pub(in crate::ui::file_panel) Vec<String>);

/// 计算拖拽源：与 clip_targets 同规则，仅取路径。
pub(super) fn drag_source_paths(
    state: &FilePanelState,
    entries: &[FileEntry],
    cwd: &str,
    idx: usize,
) -> Vec<String> {
    clip_targets(state, entries, cwd, idx)
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

/// 把拖拽到目标目录 dest 的源过滤为合法移动：排除目标自身、已直接位于 dest 内、
/// 以及把祖先目录拖进其子目录的非法情况。
pub(in crate::ui::file_panel) fn valid_move_srcs(srcs: &[String], dest: &str) -> Vec<String> {
    srcs.iter()
        .filter(|s| {
            let s = s.as_str();
            s != dest                          // 不能移动到自身
                && parent_of(s) != dest        // 已在目标目录内，无需移动
                && !dest.starts_with(&format!("{s}/")) // 不能把父目录拖进自己的子目录
        })
        .cloned()
        .collect()
}

/// 弹簧式拖拽导航的统一处理：在某目标目录上持续悬停 `UP_DWELL` 秒则进入它，
/// 并在指针处播放两次脉冲环动画；继续悬停可逐级连跳。无悬停目标时复位计时。
pub(super) fn spring_navigate(
    ui: &mut egui::Ui,
    state: &mut FilePanelState,
    spring_target: Option<String>,
    actions: &mut Vec<FileAction>,
) {
    let now = ui.input(|i| i.time);
    if let Some(target) = spring_target {
        let armed = matches!(&state.spring_since, Some((k, _)) if *k == target);
        if !armed {
            state.spring_since = Some((target.clone(), now));
        }
        let since = state.spring_since.as_ref().map(|(_, t)| *t).unwrap_or(now);
        if now - since >= UP_DWELL {
            state.cwd = target.clone();
            state.selected.clear();
            state.spring_since = None; // 重新计时，继续悬停可连跳
            state.spring_flash = Some(now);
            if !state.listings.contains_key(&target) && !state.loading.contains(&target) {
                state.loading.insert(target.clone());
                actions.push(FileAction::List(target));
            }
        }
        ui.ctx().request_repaint();
    } else {
        state.spring_since = None;
    }

    // 跳转动画：在指针处播放两次脉冲环
    if let Some(t) = state.spring_flash {
        let e = now - t;
        if e < UP_FLASH {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let phase = (e / UP_FLASH) as f32; // 0..1
                let f = (phase * 2.0).fract(); // 两个脉冲
                let k = 1.0 - (2.0 * f - 1.0).abs(); // 每个脉冲 0→1→0
                let r = 9.0 + 13.0 * (1.0 - k); // 环随脉冲收放
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("spring_flash"),
                ));
                painter.circle_stroke(
                    pos,
                    r,
                    egui::Stroke::new(2.0, Palette::ACCENT.gamma_multiply(k.max(0.1))),
                );
            }
            ui.ctx().request_repaint();
        } else {
            state.spring_flash = None;
        }
    }
}

/// 计算放入剪贴板的源项：右键项在多选内则取整组选中，否则只取该项。
/// 返回 (绝对路径, 是否目录) 列表。
pub(super) fn clip_targets(
    state: &FilePanelState,
    entries: &[FileEntry],
    cwd: &str,
    idx: usize,
) -> Vec<(String, bool)> {
    let targets: Vec<usize> = if state.selected.contains(&idx) && state.selected.len() > 1 {
        let mut v: Vec<usize> = state.selected.iter().copied().collect();
        v.sort_unstable();
        v
    } else {
        vec![idx]
    };
    targets
        .iter()
        .filter_map(|&k| entries.get(k).map(|e| (join_path(cwd, &e.name), e.is_dir)))
        .collect()
}

/// 工具栏图标按钮（扁平无边框，悬停高亮）。
pub(super) fn tool_btn(ui: &mut egui::Ui, icon: &str, tip: &str) -> bool {
    tool_btn_color(ui, icon, tip, Palette::TEXT)
}

pub(super) fn tool_btn_color(
    ui: &mut egui::Ui,
    icon: &str,
    tip: &str,
    color: egui::Color32,
) -> bool {
    tool_btn_resp(ui, icon, tip, color).clicked()
}

/// 路径过长时仅显示尾部字符（前缀省略号）；用于收藏列表显示。
pub(super) fn trailing_path(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        p.to_string()
    } else {
        let tail: String = p.chars().skip(n - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}

/// 收藏/取消收藏切换并持久化到该服务器的收藏表。
pub(super) fn toggle_favorite(state: &mut FilePanelState, path: String) {
    if path.is_empty() {
        return;
    }
    if let Some(i) = state.favorites.iter().position(|f| f == &path) {
        state.favorites.remove(i);
    } else {
        state.favorites.push(path);
    }
    crate::store::save_favorites(&state.server_key, &state.favorites);
}

/// 同 `tool_btn_color`，但返回 `Response`（用于需要命中检测/拖拽目标的按钮，如「上级目录」）。
fn tool_btn_resp(ui: &mut egui::Ui, icon: &str, tip: &str, color: egui::Color32) -> egui::Response {
    let mut resp = None;
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        resp = Some(
            ui.add(
                egui::Button::new(RichText::new(icon).size(16.0).color(color))
                    .min_size(egui::vec2(30.0, 26.0))
                    .corner_radius(6.0),
            )
            .on_hover_text(tip),
        );
    });
    resp.unwrap()
}

/// 主名长度（后缀前），用于重命名时默认选中主名。
pub(super) fn stem_char_len(name: &str) -> usize {
    match name.rsplit_once('.') {
        // 隐藏文件如 ".bashrc"（点在开头）不算后缀
        Some((stem, _)) if !stem.is_empty() => stem.chars().count(),
        _ => name.chars().count(),
    }
}

/// 根据扩展名选择文件类型图标。
pub(super) fn file_icon(e: &FileEntry) -> &'static str {
    use egui_phosphor::regular as i;
    if e.is_dir {
        return i::FOLDER;
    }
    if e.is_link {
        return i::LINK;
    }
    let ext = e
        .name
        .rsplit_once('.')
        .map(|(_, x)| x)
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp" | "ico" => i::IMAGE,
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar" => i::FILE_ZIP,
        "pdf" => i::FILE_PDF,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => i::MUSIC_NOTE,
        "mp4" | "mkv" | "avi" | "mov" | "webm" => i::FILM_STRIP,
        "sh" | "bash" | "zsh" | "fish" => i::TERMINAL_WINDOW,
        "rs" | "c" | "cpp" | "cc" | "h" | "hpp" | "py" | "js" | "ts" | "go" | "java" | "rb"
        | "php" | "json" | "toml" | "yaml" | "yml" | "xml" | "html" | "css" | "sql" => i::FILE_CODE,
        "txt" | "md" | "log" | "conf" | "cfg" | "ini" | "env" => i::FILE_TEXT,
        _ => i::FILE,
    }
}
