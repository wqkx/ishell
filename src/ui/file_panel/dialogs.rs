//! 文件面板对话框。从 file_panel 拆出，行为不变。

use egui::RichText;

use crate::theme::Palette;
use crate::ui::fmt_bytes;
use super::{
    basename, join_path, parent_of, Dialog, FileAction, FilePanelState,
};
use super::ime::ime_singleline;

pub(super) fn dialogs(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    let ctx = ui.ctx().clone();
    let mut close = false;
    let mut clear_sel = false; // 批量删除确认后清空选中，避免残留的陈旧行索引
    let mut undo_confirmed = false; // 撤销移动确认框点了「撤销」
    let mut new_item: Option<(String, bool)> = None; // 新建目录/文件：(名称, 是否目录)，延后到借用结束再乐观插入
    let cwd = state.cwd.clone();
    // 取出对话框 IME 组字状态到本地（避免与 &mut state.dialog 借用冲突），末尾写回
    let mut ime = state.dialog_ime.take();

    if let Some(dialog) = &mut state.dialog {
        match dialog {
            Dialog::NewDir { name } => {
                modal(&ctx, crate::i18n::tr("新建目录", "New folder"), |ui| {
                    let (resp, submit) = ime_singleline(ui, "new_dir_name", name, &mut ime);
                    // 打开即自动聚焦输入框（无其它控件占焦时抓取），可直接输入
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if (dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) || submit) && !name.trim().is_empty() {
                            actions.push(FileAction::Mkdir(join_path(&cwd, name.trim())));
                            new_item = Some((name.trim().to_string(), true));
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::NewFile { name } => {
                modal(&ctx, crate::i18n::tr("新建文件", "New file"), |ui| {
                    let (resp, submit) = ime_singleline(ui, "new_file_name", name, &mut ime);
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if (dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) || submit) && !name.trim().is_empty() {
                            actions.push(FileAction::CreateFile(join_path(&cwd, name.trim())));
                            new_item = Some((name.trim().to_string(), false));
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Upload { local } => {
                modal(&ctx, crate::i18n::tr("上传", "Upload"), |ui| {
                    ui.label(RichText::new(crate::i18n::tr("本地文件/文件夹路径（也可拖拽到文件区）", "Local file/folder path (or drag onto the panel)")).size(12.0).color(Palette::TEXT_DIM));
                    let (resp, submit) = ime_singleline(ui, "upload_local_path", local, &mut ime);
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    if submit && !local.trim().is_empty() {
                        actions.push(FileAction::Upload { local: local.trim().to_string(), remote_dir: cwd.clone() });
                        close = true;
                    }
                    ui.add_space(6.0);
                    // 原生选择器：选文件（可多选）/ 选文件夹（整个上传）
                    button_row(ui, 118.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("选择文件…", "Choose files…"), 118.0, 0) {
                            if let Some(paths) = rfd::FileDialog::new().pick_files() {
                                for p in paths {
                                    actions.push(FileAction::Upload { local: p.to_string_lossy().into_owned(), remote_dir: cwd.clone() });
                                }
                                close = true;
                            }
                        }
                        if dlg_btn(ui, crate::i18n::tr("选择文件夹…", "Choose folder…"), 118.0, 0) {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                actions.push(FileAction::Upload { local: p.to_string_lossy().into_owned(), remote_dir: cwd.clone() });
                                close = true;
                            }
                        }
                    });
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("上传", "Upload"), 72.0, 2) && !local.trim().is_empty() {
                            actions.push(FileAction::Upload { local: local.trim().to_string(), remote_dir: cwd.clone() });
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Chmod { path, mode, name } => {
                modal(&ctx, crate::i18n::tr("修改权限", "Chmod"), |ui| {
                    ui.vertical_centered(|ui| ui.label(RichText::new(name.as_str()).strong()));
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| chmod_grid(ui, mode));
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("八进制：{:03o}", *mode & 0o777), crate::i18n::Lang::En => format!("Octal: {:03o}", *mode & 0o777) }).monospace().color(Palette::TEXT_DIM)));
                    ui.add_space(10.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("应用", "Apply"), 72.0, 2) {
                            actions.push(FileAction::Chmod { path: path.clone(), mode: *mode & 0o777 });
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Rename { path, name } => {
                modal(&ctx, crate::i18n::tr("重命名", "Rename"), |ui| {
                    // 与新建文件夹相同：自绘 IME，避免 fcitx 下中文第二次无法输入
                    let (resp, submit) = ime_singleline(ui, "rename_name", name, &mut ime);
                    if ui.memory(|m| m.focused().is_none()) {
                        resp.request_focus();
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if (dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) || submit) && !name.trim().is_empty() {
                            let parent = parent_of(path);
                            actions.push(FileAction::Rename { from: path.clone(), to: join_path(&parent, name.trim()) });
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmDelete { items } => {
                modal(&ctx, crate::i18n::tr("确认删除", "Confirm delete"), |ui| {
                    let n = items.len();
                    if n == 1 {
                        let name = &items[0].2;
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("确定删除 {name} 吗？此操作不可恢复。"),
                            crate::i18n::Lang::En => format!("Delete {name}? This cannot be undone."),
                        });
                    } else {
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("确定删除选中的 {n} 项吗？此操作不可恢复。"),
                            crate::i18n::Lang::En => format!("Delete {n} selected items? This cannot be undone."),
                        });
                        // 列出待删名称（最多 4 个，多余用 … 概括），避免文件多时对话框过长
                        ui.add_space(4.0);
                        let shown: Vec<String> = items.iter().take(4).map(|(_, _, nm)| nm.clone()).collect();
                        let more = if n > 4 {
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!(" … 等 {n} 项"),
                                crate::i18n::Lang::En => format!(" … ({n} total)"),
                            }
                        } else {
                            String::new()
                        };
                        ui.label(RichText::new(format!("{}{}", shown.join("、"), more)).color(Palette::TEXT_DIM).size(11.0));
                    }
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("删除", "Delete"), 72.0, 1) {
                            // 一条批量删除（单 rm，单通道）——避免多文件时并发开过多 SSH 通道被拒
                            let paths: Vec<String> = items.iter().map(|(p, _, _)| p.clone()).collect();
                            actions.push(FileAction::DeleteMany(paths));
                            clear_sel = true;
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmOpenLarge { path, size } => {
                modal(&ctx, crate::i18n::tr("打开大文件", "Open large file"), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("文件较大（{}），仍要打开吗？", fmt_bytes(*size as f64)),
                            crate::i18n::Lang::En => format!("Large file ({}). Open anyway?", fmt_bytes(*size as f64)),
                        });
                        ui.label(RichText::new(crate::i18n::tr(
                            "将以只读模式打开（整文件载入内存）；可在状态栏改为可编辑。",
                            "Opens in read-only mode (full file loaded into memory). Unlock from the status bar to edit.",
                        )).color(Palette::TEXT_DIM).size(11.0));
                    });
                    ui.add_space(10.0);
                    // 按钮水平居中
                    ui.horizontal(|ui| {
                        let bw = 80.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("打开", "Open")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            actions.push(FileAction::OpenFile { path: path.clone(), force: true });
                            close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmOpenAsText { path, size } => {
                modal(&ctx, crate::i18n::tr("用文本编辑器打开？", "Open as text?"), |ui| {
                    ui.vertical_centered(|ui| {
                        let fname = path.rsplit('/').next().unwrap_or(path);
                        ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("「{}」不是常见文本/代码类型。", fname), crate::i18n::Lang::En => format!("\"{}\" is not a known text type.", fname) });
                        ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("仍用文本编辑器打开吗？（{}，二进制内容会显示为乱码）", fmt_bytes(*size as f64)), crate::i18n::Lang::En => format!("Open with the text editor anyway? ({}, binary will look garbled)", fmt_bytes(*size as f64)) }).color(Palette::TEXT_DIM).size(11.0));
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let bw = 100.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("文本打开", "Open as text")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            actions.push(FileAction::OpenFile { path: path.clone(), force: true });
                            close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmUndoMove { what, from, to } => {
                modal(&ctx, crate::i18n::tr("撤销移动", "Undo move"), |ui| {
                    ui.label(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("确定撤销移动 {what} 吗？"),
                        crate::i18n::Lang::En => format!("Undo moving {what}?"),
                    });
                    ui.add_space(4.0);
                    // 明确「从哪移回哪」，避免误撤销
                    ui.label(RichText::new(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("从 {from}\n移回 {to}"),
                        crate::i18n::Lang::En => format!("from {from}\nback to {to}"),
                    }).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("撤销", "Undo"), 72.0, 2) {
                            undo_confirmed = true;
                            close = true;
                        }
                        if dlg_btn(ui, crate::i18n::tr("取消", "Cancel"), 72.0, 0) {
                            close = true;
                        }
                    });
                });
            }
        }
    }
    // 统一：任意弹框按 Esc 取消
    if state.dialog.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }
    if close {
        state.dialog = None;
    }
    // 写回 IME 组字状态（对话框已关则清空，避免下次打开残留旧组字范围）
    state.dialog_ime = if close { None } else { ime };
    // 乐观插入新建的目录/文件（借用已结束）：即时出现在列表中并选中，无需等整目录刷新。
    if let Some((name, is_dir)) = new_item {
        state.insert_new(&cwd, &name, is_dir);
    }
    if clear_sel {
        state.selected.clear();
    }
    // 用户确认撤销：此刻才出栈并执行反向 mv（与确认框展示的栈顶记录一致）
    if undo_confirmed {
        if let Some(rec) = state.move_undo.pop() {
            let orig_parent = parent_of(&rec.original[0]);
            let new_srcs: Vec<String> = rec.original.iter().map(|o| join_path(&rec.dest_dir, basename(o))).collect();
            // 把这些项从「目标目录」缓存移除，否则该目录/树节点仍显示已移回的文件（看着像没删）
            {
                let moved: std::collections::HashSet<String> = new_srcs.iter().cloned().collect();
                if let Some(list) = state.listings.get_mut(&rec.dest_dir) {
                    list.retain(|e| !moved.contains(&join_path(&rec.dest_dir, &e.name)));
                }
            }
            let names: Vec<&str> = rec.original.iter().map(|o| basename(o)).collect();
            let what = if names.len() == 1 {
                names[0].to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("{} 项", names.len()),
                    crate::i18n::Lang::En => format!("{} items", names.len()),
                }
            };
            let msg = match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("已撤销移动：{what}（从 {} 移回 {}）", rec.dest_dir, orig_parent),
                crate::i18n::Lang::En => format!("Undid move: {what} (from {} back to {})", rec.dest_dir, orig_parent),
            };
            // 先发反向 mv，再设提示——确保撤销提示覆盖 Move 自身的「移动中」提示
            actions.push(FileAction::Move { srcs: new_srcs, dest_dir: orig_parent });
            actions.push(FileAction::Status(msg));
            state.selected.clear();
            state.anchor = None;
        }
    }
}

/// 一行 `count` 个定宽按钮，水平居中（前置留白）。按钮请用 `.min_size((btn_w, 0))`。
fn button_row(ui: &mut egui::Ui, btn_w: f32, count: usize, add: impl FnOnce(&mut egui::Ui)) {
    let total = count as f32 * btn_w + count.saturating_sub(1) as f32 * ui.spacing().item_spacing.x;
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
        add(ui);
    });
}

/// 定宽对话框按钮（普通/危险/主色）。
fn dlg_btn(ui: &mut egui::Ui, label: &str, w: f32, kind: u8) -> bool {
    let txt = match kind {
        2 => RichText::new(label).color(egui::Color32::WHITE), // 主色
        1 => RichText::new(label).color(Palette::DANGER),      // 危险
        _ => RichText::new(label),
    };
    let mut b = egui::Button::new(txt).min_size(egui::vec2(w, 0.0));
    if kind == 2 {
        b = b.fill(Palette::ACCENT);
    }
    ui.add(b).clicked()
}

fn modal(ctx: &egui::Context, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            // 固定内容宽度：让按钮/内容居中（add_space 用的 available_width 才稳定）
            ui.set_width(300.0);
            add(ui);
        });
    // 注：原此处有「开着对话框就低频轮询重绘」的输入法 workaround，已移除——
    // 它同样修不了 X11/XIM 的提交延迟，却让对话框打开期间持续重绘。egui 会在
    // 收到按键/IME 事件时反应式重绘，对话框输入正常。
}

fn chmod_grid(ui: &mut egui::Ui, mode: &mut u32) {
    // 与文件列表类似的表格风格：表头加粗弱化色 + 斑马纹行，列宽统一便于对齐
    egui::Grid::new("chmod_grid")
        .num_columns(4)
        .striped(true)
        .spacing([18.0, 7.0])
        .min_col_width(46.0)
        .show(ui, |ui| {
            ui.label("");
            for t in [crate::i18n::tr("读", "R"), crate::i18n::tr("写", "W"), crate::i18n::tr("执行", "X")] {
                ui.vertical_centered(|ui| ui.label(RichText::new(t).strong().color(Palette::TEXT_DIM).size(12.0)));
            }
            ui.end_row();
            for (label, base) in [(crate::i18n::tr("所有者", "Owner"), 6u32), (crate::i18n::tr("用户组", "Group"), 3), (crate::i18n::tr("其他", "Other"), 0)] {
                ui.label(RichText::new(label).size(12.0).color(Palette::TEXT));
                for bit in [2u32, 1, 0] {
                    let shift = base + bit;
                    let mut on = *mode & (1 << shift) != 0;
                    ui.vertical_centered(|ui| {
                        if ui.checkbox(&mut on, "").changed() {
                            if on {
                                *mode |= 1 << shift;
                            } else {
                                *mode &= !(1 << shift);
                            }
                        }
                    });
                }
                ui.end_row();
            }
        });
}

