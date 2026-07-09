//! File table tail actions: shortcuts, blank-drop moves, and undo.

use egui::Response;

use crate::proto::FileEntry;

use super::super::helpers::{valid_move_srcs, DragPaths};
use super::super::super::{basename, join_path, parent_of, Dialog, FileAction, FilePanelState};

pub(super) fn apply_table_tail_actions(
    ui: &egui::Ui,
    state: &mut FilePanelState,
    actions: &mut Vec<FileAction>,
    entries: &[FileEntry],
    cwd: &str,
    bg: &Response,
    mut drop_move: Option<(Vec<String>, String)>,
) {
    // Ctrl/Cmd+A 全选当前列表（含已过滤后的所有行）。文本框聚焦/重命名/对话框时不抢，
    // 让 Ctrl+A 仍作用于文本框自身的全选。
    let select_all =
        ui.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::A));
    if select_all
        && state.renaming.is_none()
        && state.path_edit.is_none()
        && state.dialog.is_none()
        && ui.ctx().memory(|m| m.focused().is_none())
        && !entries.is_empty()
    {
        state.selected = (0..entries.len()).collect();
        state.anchor = Some(0);
    }

    // 批量删除：工具栏删除按钮 或 Delete 键（重命名/路径编辑/已有对话框时不触发）
    let key_del = ui.input(|i| i.key_pressed(egui::Key::Delete))
        && state.renaming.is_none()
        && state.path_edit.is_none();
    if key_del && state.dialog.is_none() && !state.selected.is_empty() {
        let mut idxs: Vec<usize> = state.selected.iter().copied().collect();
        idxs.sort_unstable();
        let items: Vec<(String, bool, String)> = idxs
            .iter()
            .filter_map(|&k| {
                entries
                    .get(k)
                    .map(|e| (join_path(&cwd, &e.name), e.is_dir, e.name.clone()))
            })
            .collect();
        if !items.is_empty() {
            state.dialog = Some(Dialog::ConfirmDelete { items });
        }
    }

    // 兜底：拖放释放在列表空白处（无任何行接收）-> 移入当前目录。
    // 必须在表格渲染之后再取载荷，否则 bg 会抢在文件夹行之前把载荷取走。
    if drop_move.is_none() {
        if let Some(payload) = bg.dnd_release_payload::<DragPaths>() {
            let srcs = valid_move_srcs(&payload.0, &cwd);
            if !srcs.is_empty() {
                drop_move = Some((srcs, cwd.to_string()));
            }
        }
    }

    // 拖拽移动：释放到某文件夹后发起远端 mv，并记录撤销。
    if let Some((srcs, dest_dir)) = drop_move {
        // 移入「非当前目录」的文件夹：乐观地从当前列表移除被移动项，呈现「移走」效果，
        // 且不整目录刷新（刷新会跳一下）；目标目录由 worker 的 OpDone 后台刷新（不可见，无跳动）。
        if dest_dir != cwd {
            if let Some(list) = state.listings.get_mut(cwd) {
                let moved: std::collections::HashSet<String> = srcs.iter().cloned().collect();
                list.retain(|e| !moved.contains(&join_path(&cwd, &e.name)));
            }
        }
        state.record_move(srcs.clone(), dest_dir.clone());
        actions.push(FileAction::Move { srcs, dest_dir });
        state.selected.clear();
        state.anchor = None;
    }

    // Ctrl+Z 撤销最近一次拖拽移动：把目标目录里的项移回原父目录，并在状态栏提示撤销了什么。
    // 仅在无文本框聚焦 / 无对话框 / 非重命名时触发，避免抢占输入框自身的撤销。
    let undo_key = ui.input(|i| {
        i.key_pressed(egui::Key::Z)
            && (i.modifiers.command || i.modifiers.ctrl)
            && !i.modifiers.shift
    });
    if undo_key
        && state.renaming.is_none()
        && state.path_edit.is_none()
        && state.dialog.is_none()
        && ui.ctx().memory(|m| m.focused().is_none())
    {
        // 不直接执行：弹确认框，明确告知撤销的是「哪些文件、从哪移回哪」，用户确认后才执行。
        // 仅取栈顶预览（不出栈），真正出栈与反向 mv 在用户点「撤销」后于 dialogs() 中进行。
        if let Some(rec) = state.move_undo.last() {
            let orig_parent = parent_of(&rec.original[0]);
            let names: Vec<&str> = rec.original.iter().map(|o| basename(o)).collect();
            let what = if names.len() == 1 {
                names[0].to_string()
            } else {
                let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join("、");
                let more = if names.len() > 3 {
                    match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!(" 等 {} 项", names.len()),
                        crate::i18n::Lang::En => format!(" +{} more", names.len() - 3),
                    }
                } else {
                    String::new()
                };
                format!("{shown}{more}")
            };
            state.dialog = Some(Dialog::ConfirmUndoMove {
                what,
                from: rec.dest_dir.clone(),
                to: orig_parent,
            });
        } else {
            // 撤销栈为空：明确告知用户没有可撤销的移动
            actions.push(FileAction::Status(
                crate::i18n::tr("没有可撤销的移动", "Nothing to undo").into(),
            ));
        }
    }

}
