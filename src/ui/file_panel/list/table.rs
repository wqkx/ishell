use std::collections::HashSet;

use egui::{RichText, Sense};

use super::super::{
    basename, join_path, open_intent, parent_of, Dialog, FileAction, FilePanelState, OpenIntent,
    Renaming, SortKey,
};
use super::helpers::{
    clip_targets, spring_navigate, toggle_favorite, valid_move_srcs, DragPaths, Menu,
};
use crate::theme::Palette;

#[path = "table_rows.rs"]
mod table_rows;
#[path = "table_selection.rs"]
mod table_selection;

pub(super) fn file_table(
    ui: &mut egui::Ui,
    state: &mut FilePanelState,
    has_clip: bool,
    actions: &mut Vec<FileAction>,
) {
    use egui_phosphor::regular as icon;
    let mut paste_here = false;
    let mut spring_target: Option<String> = None;

    if state.loading.contains(&state.cwd) && !state.listings.contains_key(&state.cwd) {
        // 已重试过（att>1）即视为弱网，给出「网络较慢，正在重试」提示，避免看似卡死无反馈
        let slow = state
            .load_at
            .get(&state.cwd)
            .map(|v| v.1 > 1)
            .unwrap_or(false);
        ui.horizontal(|ui| {
            ui.spinner();
            let msg = if slow {
                crate::i18n::tr("网络较慢，正在重试 …", "Slow network, retrying …")
            } else {
                crate::i18n::tr("加载中 …", "Loading …")
            };
            ui.label(RichText::new(msg).color(Palette::TEXT_DIM));
        });
        return;
    }

    let cwd = state.cwd.clone();
    let favs = state.favorites.clone(); // 供表格内右键菜单判断「已收藏」（state 在表格闭包里已被可变借用）
    let total_count = state.listings.get(&cwd).map(|e| e.len()).unwrap_or(0);

    // 名称过滤行：左侧留边（与操作栏一致）、右侧顶到边
    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 12,
            right: 0,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(icon::MAGNIFYING_GLASS)
                        .color(Palette::TEXT_DIM)
                        .size(12.0),
                );
                let clear_w = if state.filter.is_empty() { 0.0 } else { 22.0 };
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .desired_width(ui.available_width() - clear_w - 2.0)
                        .hint_text(crate::i18n::tr("按名称过滤", "Filter by name")),
                );
                if resp.changed() {
                    // 过滤变化会改变行索引，清空选择避免错位
                    state.selected.clear();
                    state.anchor = None;
                }
                if !state.filter.is_empty()
                    && ui
                        .add(
                            egui::Button::new(
                                RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM),
                            )
                            .frame(false),
                        )
                        .on_hover_text(crate::i18n::tr("清除过滤", "Clear"))
                        .clicked()
                {
                    state.filter.clear();
                }
            });
        });
    ui.add_space(2.0);

    let mut entries = match state.listings.get(&cwd) {
        Some(e) => e.clone(),
        None => return,
    };
    // 预取：进入目录后顺带请求各「直接子文件夹」的列表，点进下一级时即时显示（命中缓存）。
    // 每个子目录只在未缓存且未加载时请求一次；上限避免极端目录发起过多请求。
    {
        let mut prefetched = 0;
        for e in entries.iter().filter(|e| e.is_dir && !e.is_link) {
            if prefetched >= 64 {
                break;
            }
            let sub = join_path(&cwd, &e.name);
            if !state.listings.contains_key(&sub) && !state.loading.contains(&sub) {
                state.loading.insert(sub.clone());
                actions.push(FileAction::List(sub));
                prefetched += 1;
            }
        }
    }
    // 排序：目录始终在前，组内按所选键升/降序
    {
        let key = state.sort_key;
        let desc = state.sort_desc;
        entries.sort_by(|a, b| {
            (!a.is_dir).cmp(&!b.is_dir).then_with(|| {
                let ord = match key {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Size => a.size.cmp(&b.size),
                    SortKey::Mtime => a.mtime.cmp(&b.mtime),
                };
                if desc {
                    ord.reverse()
                } else {
                    ord
                }
            })
        });
    }
    // 应用名称过滤
    if !state.filter.trim().is_empty() {
        let f = state.filter.to_lowercase();
        entries.retain(|e| e.name.to_lowercase().contains(&f));
        if entries.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("无匹配（共 {total_count} 项）"),
                        crate::i18n::Lang::En => format!("No match ({total_count} items)"),
                    })
                    .color(Palette::TEXT_DIM)
                    .size(12.0),
                );
            });
        }
    }

    // 上传成功后选中所传文件：当前目录刷新出新列表时，按文件名定位行号并选中（命中后才清空，
    // 以兼容「刷新尚未到达、文件暂未出现」的情形）。
    if let Some(names) = state
        .pending_select
        .as_ref()
        .filter(|(d, _)| *d == cwd)
        .map(|(_, n)| n.clone())
    {
        let sel: HashSet<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| names.contains(&e.name))
            .map(|(i, _)| i)
            .collect();
        if !sel.is_empty() {
            state.anchor = sel.iter().min().copied();
            state.selected = sel;
            state.pending_select = None;
        }
    }

    let mut navigate: Option<String> = None;
    let mut sort_click: Option<SortKey> = None;
    let mut menu: Vec<Menu> = Vec::new();
    let mut clicks: Vec<usize> = Vec::new(); // 本帧被点击的行
    let mut rclick: Option<usize> = None; // 本帧被右键的行
    let mut open_file: Option<String> = None; // 双击文本文件
    let mut open_image: Option<String> = None; // 双击图片文件
    let mut open_pdf: Option<String> = None; // 双击 PDF 文件
    let mut open_docx: Option<String> = None; // 双击 Word(docx) 文件
    let mut confirm_open: Option<(String, u64)> = None; // 大文本文件待确认
    let mut confirm_text: Option<(String, u64)> = None; // 非文本后缀，待确认是否强开
    let mut rename_commit: Option<(String, String)> = None;
    let mut cancel_rename = false;
    let mut drop_move: Option<(Vec<String>, String)> = None; // 拖拽释放到某文件夹 -> (srcs, dest_dir)
    let mut broken_link = false; // 双击断链 -> 表后统一提示
    let now = ui.input(|i| i.time);
    let (mod_ctrl, mod_shift) =
        ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

    // 空白区域右键 -> 新建文件/目录（仅覆盖列表区域，避免遮挡上方路径栏）
    let bg = ui.interact(
        ui.available_rect_before_wrap(),
        ui.id().with("filelist_bg"),
        Sense::click(),
    );
    // 点击列表空白 → 让出当前(终端)焦点：终端仅在被点击时夺焦，让焦后 focused()=None，
    // ↑/↓/Enter 即作用于文件列表而非被终端的焦点锁吞掉。
    if bg.clicked() {
        ui.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
    }
    // 注意：bg 几何上覆盖整个列表，dnd_release_payload 用 contains_pointer 判定并「取走」载荷，
    // 若在此处先检查会把本应落到文件夹行的拖放抢走。故放到表格之后、仅当无行接收时再兜底。
    let mut bg_new_dir = false;
    let mut bg_new_file = false;
    let mut bg_upload = false;
    let mut bg_cd = false;
    let mut bg_fav = false;
    let mut bg_refresh = false;
    bg.context_menu(|ui| {
        if !state.cwd.is_empty()
            && ui
                .button(format!(
                    "{}  {}",
                    icon::ARROW_CLOCKWISE,
                    crate::i18n::tr("刷新", "Refresh")
                ))
                .clicked()
        {
            bg_refresh = true;
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::FOLDER_PLUS,
                crate::i18n::tr("新建文件夹", "New folder")
            ))
            .clicked()
        {
            bg_new_dir = true;
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
            bg_new_file = true;
            ui.close();
        }
        if ui
            .button(format!(
                "{}  {}",
                icon::UPLOAD_SIMPLE,
                crate::i18n::tr("上传文件", "Upload")
            ))
            .clicked()
        {
            bg_upload = true;
            ui.close();
        }
        if has_clip {
            ui.separator();
            if ui
                .button(format!(
                    "{}  {}",
                    icon::CLIPBOARD_TEXT,
                    crate::i18n::tr("粘贴到此目录", "Paste here")
                ))
                .clicked()
            {
                paste_here = true;
                ui.close();
            }
        }
        if !state.cwd.is_empty() {
            ui.separator();
            let faved = state.favorites.iter().any(|f| f == &state.cwd);
            let lbl = if faved {
                format!(
                    "★  {}",
                    crate::i18n::tr("取消收藏当前目录", "Remove bookmark")
                )
            } else {
                format!(
                    "☆  {}",
                    crate::i18n::tr("收藏当前目录", "Bookmark current dir")
                )
            };
            if ui.button(lbl).clicked() {
                bg_fav = true;
                ui.close();
            }
            if ui
                .button(format!(
                    "{}  {}",
                    icon::TERMINAL_WINDOW,
                    crate::i18n::tr("在终端打开当前目录", "Open current dir in terminal")
                ))
                .clicked()
            {
                bg_cd = true;
                ui.close();
            }
        }
    });
    if bg_fav {
        toggle_favorite(state, state.cwd.clone());
    }
    if bg_refresh && state.refresh_dir(&cwd) {
        actions.push(FileAction::List(cwd.clone()));
    }

    // —— 键盘导航：↑/↓ 移动选中行、Enter 打开/进入目录（与 Ctrl+A/Delete 同一聚焦门：无文本框聚焦时生效）——
    let mut focus_list = false; // 本帧是否有行被点击 → 表外为文件列表夺取键盘焦点
    let mut scroll_to_row: Option<usize> = None;
    let kbd_nav = state.renaming.is_none()
        && state.path_edit.is_none()
        && state.dialog.is_none()
        && ui.ctx().memory(|m| m.focused().is_none());
    if kbd_nav && !entries.is_empty() {
        let n = entries.len();
        let cur = state
            .anchor
            .filter(|&a| a < n)
            .or_else(|| state.selected.iter().min().copied())
            .unwrap_or(0)
            .min(n - 1);
        let (down, up, enter) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
            )
        });
        if down || up {
            let nc = if down {
                (cur + 1).min(n - 1)
            } else {
                cur.saturating_sub(1)
            };
            state.selected.clear();
            state.selected.insert(nc);
            state.anchor = Some(nc);
            scroll_to_row = Some(nc);
        } else if enter {
            if let Some(e) = entries.get(cur) {
                let full = join_path(&cwd, &e.name);
                match open_intent(e, &full) {
                    OpenIntent::Navigate(p) => navigate = Some(p),
                    OpenIntent::Image(p) => open_image = Some(p),
                    OpenIntent::Pdf(p) => open_pdf = Some(p),
                    OpenIntent::Docx(p) => open_docx = Some(p),
                    OpenIntent::ConfirmText(p, sz) => confirm_text = Some((p, sz)),
                    OpenIntent::ConfirmLarge(p, sz) => confirm_open = Some((p, sz)),
                    OpenIntent::Text(p) => open_file = Some(p),
                    OpenIntent::Broken => actions.push(FileAction::Status(
                        crate::i18n::tr("断链：目标不存在", "Broken link: target missing").into(),
                    )),
                }
            }
        }
    }

    table_rows::render_table_rows(
        ui,
        state,
        &entries,
        &cwd,
        &favs,
        has_clip,
        scroll_to_row,
        &mut sort_click,
        &mut menu,
        &mut clicks,
        &mut rclick,
        &mut navigate,
        &mut open_file,
        &mut open_image,
        &mut open_pdf,
        &mut open_docx,
        &mut confirm_open,
        &mut confirm_text,
        &mut rename_commit,
        &mut cancel_rename,
        &mut drop_move,
        &mut broken_link,
        &mut spring_target,
        &mut focus_list,
        now,
        mod_ctrl,
        mod_shift,
    );

    // 表头点击排序：同列切升/降；换列时按列性质选默认方向——
    // 大小/修改时间首次点击用降序（先看最大/最新），名称用升序（A→Z）。
    if let Some(k) = sort_click {
        if state.sort_key == k {
            state.sort_desc = !state.sort_desc;
        } else {
            state.sort_key = k;
            state.sort_desc = matches!(k, SortKey::Size | SortKey::Mtime);
        }
        state.selected.clear();
        state.anchor = None;
    }

    // 背景右键新建
    if bg_new_dir {
        state.dialog = Some(Dialog::NewDir {
            name: String::new(),
        });
    }
    if bg_new_file {
        state.dialog = Some(Dialog::NewFile {
            name: String::new(),
        });
    }
    if bg_upload {
        state.dialog = Some(Dialog::Upload {
            local: String::new(),
        });
    }
    if bg_cd {
        actions.push(FileAction::CdTerminal(state.cwd.clone()));
    }
    // 点击列表空白处（非任何行）：若有选中则全部取消选中
    if bg.clicked() && !state.selected.is_empty() {
        state.selected.clear();
        state.anchor = None;
        state.pending_rename = None;
    }

    // 延时重命名触发：单击后 0.4s 内无双击则进入重命名
    if let Some((i, t)) = state.pending_rename {
        if now - t > 0.40 {
            if let Some(e) = entries.get(i) {
                state.renaming = Some(Renaming {
                    idx: i,
                    buf: e.name.clone(),
                    init: true,
                    ime: None,
                });
            }
            state.pending_rename = None;
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(450));
        }
    }
    // 提交 / 取消重命名
    if let Some((from, to)) = rename_commit.take() {
        if from != to {
            actions.push(FileAction::Rename { from, to });
        }
        state.renaming = None;
    } else if cancel_rename {
        state.renaming = None;
    }
    // 双击断链：目标不存在，提示用户（不进入、不打开）
    if broken_link {
        actions.push(FileAction::Status(
            crate::i18n::tr("断链：目标不存在", "Broken link: target missing").into(),
        ));
    }
    // 双击打开文本文件
    if let Some(p) = open_file {
        actions.push(FileAction::OpenFile {
            path: p,
            force: false,
        });
    }
    // 双击打开图片 / PDF / Word
    if let Some(p) = open_image {
        actions.push(FileAction::OpenImage { path: p });
    }
    if let Some(p) = open_pdf {
        actions.push(FileAction::OpenPdf { path: p });
    }
    if let Some(p) = open_docx {
        actions.push(FileAction::OpenDocx { path: p });
    }
    if let Some((p, size)) = confirm_open {
        state.dialog = Some(Dialog::ConfirmOpenLarge { path: p, size });
    }
    if let Some((p, size)) = confirm_text {
        state.dialog = Some(Dialog::ConfirmOpenAsText { path: p, size });
    }

    table_selection::apply_click_selection(state, clicks, rclick, mod_ctrl, mod_shift);

    // 处理右键菜单结果：下载/复制立即成动作，改权限/重命名/删除打开对话框
    for m in menu {
        match m {
            Menu::Download(idx) => {
                // 多选时批量下载（文件与文件夹均可，文件夹递归下载）
                let targets: Vec<usize> =
                    if state.selected.contains(&idx) && state.selected.len() > 1 {
                        let mut v: Vec<usize> = state.selected.iter().copied().collect();
                        v.sort_unstable();
                        v
                    } else {
                        vec![idx]
                    };
                for k in targets {
                    if let Some(e) = entries.get(k) {
                        actions.push(FileAction::Download(join_path(&cwd, &e.name)));
                    }
                }
            }
            Menu::CopyPath(p) => actions.push(FileAction::CopyPath(p)),
            Menu::Chmod { path, mode, name } => {
                state.dialog = Some(Dialog::Chmod { path, mode, name });
            }
            Menu::Rename { path, name } => {
                state.dialog = Some(Dialog::Rename { path, name });
            }
            Menu::Delete(idx) => {
                // 多选时批量删除（含文件夹，递归删除）；否则只删右键的那一项
                let targets: Vec<usize> =
                    if state.selected.contains(&idx) && state.selected.len() > 1 {
                        let mut v: Vec<usize> = state.selected.iter().copied().collect();
                        v.sort_unstable();
                        v
                    } else {
                        vec![idx]
                    };
                let items: Vec<(String, bool, String)> = targets
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
            Menu::Copy(idx) => {
                let items = clip_targets(state, &entries, &cwd, idx);
                if !items.is_empty() {
                    actions.push(FileAction::ClipCopy { items });
                }
            }
            Menu::Cut(idx) => {
                let items = clip_targets(state, &entries, &cwd, idx);
                if !items.is_empty() {
                    actions.push(FileAction::ClipCut { items });
                }
            }
            Menu::Paste => paste_here = true,
            Menu::NewDir => {
                state.dialog = Some(Dialog::NewDir {
                    name: String::new(),
                })
            }
            Menu::NewFile => {
                state.dialog = Some(Dialog::NewFile {
                    name: String::new(),
                })
            }
            Menu::CdHere(p) => actions.push(FileAction::CdTerminal(p)),
            Menu::Favorite(p) => toggle_favorite(state, p),
        }
    }

    // 粘贴：工具栏按钮 / 右键菜单触发；具体同机或跨机、是否确认由 App 决定
    if paste_here && has_clip {
        actions.push(FileAction::Paste {
            dest_dir: cwd.clone(),
        });
    }

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
                drop_move = Some((srcs, cwd.clone()));
            }
        }
    }

    // 拖拽移动：释放到某文件夹后发起远端 mv，并记录撤销。
    if let Some((srcs, dest_dir)) = drop_move {
        // 移入「非当前目录」的文件夹：乐观地从当前列表移除被移动项，呈现「移走」效果，
        // 且不整目录刷新（刷新会跳一下）；目标目录由 worker 的 OpDone 后台刷新（不可见，无跳动）。
        if dest_dir != cwd {
            if let Some(list) = state.listings.get_mut(&cwd) {
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

    // 弹簧式拖拽导航：在 Up / 文件夹上持续悬停则进入目标目录，并在指针处双闪
    spring_navigate(ui, state, spring_target, actions);

    if let Some(p) = navigate {
        state.cwd = p;
        state.selected.clear();
    }
}
