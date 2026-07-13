use std::collections::HashSet;

use egui::Sense;

use super::list::{valid_move_srcs, DragPaths};
use super::{join_path, FileAction, FilePanelState, UP_DWELL};
use crate::theme::Palette;

/// 绘制目录树（从 root 递归）。
pub(super) fn tree(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    if state.root.is_empty() {
        return;
    }
    // 行间距与默认基本持平（先 -20%，再 +15%、+10%，回到舒适的疏密度）
    ui.spacing_mut().item_spacing.y *= 1.01;
    let mut toggles: Vec<String> = Vec::new();
    let mut select: Option<String> = None;
    let mut drop: Option<(Vec<String>, String)> = None; // 拖入某树节点 -> (srcs, dest_dir)
    let mut spring: Option<String> = None; // 拖拽悬停中的树节点（停留展开/折叠）

    // 根节点
    let root = state.root.clone();
    draw_node(
        ui,
        state,
        &root,
        &root,
        0,
        &mut toggles,
        &mut select,
        &mut drop,
        &mut spring,
    );

    // 弹簧式展开/折叠：在某树节点上持续悬停 UP_DWELL 秒则切换其展开态，并在指针处双闪；
    // 切换后把计时设为 +∞ 哨兵，避免同一节点反复翻转（移开再回来方可再次触发）。
    let now = ui.input(|i| i.time);
    if let Some(tp) = spring.clone() {
        let armed = matches!(&state.tree_spring_since, Some((k, _)) if *k == tp);
        if !armed {
            state.tree_spring_since = Some((tp.clone(), now));
        }
        let since = state
            .tree_spring_since
            .as_ref()
            .map(|(_, t)| *t)
            .unwrap_or(now);
        if since.is_finite() && now - since >= UP_DWELL {
            toggles.push(tp.clone());
            state.tree_spring_since = Some((tp.clone(), f64::INFINITY));
            state.spring_flash = Some(now); // 复用文件区的双闪动画（由 spring_navigate 在指针处绘制）
        }
        ui.ctx().request_repaint();
    } else {
        state.tree_spring_since = None;
    }

    // 应用展开/折叠
    for p in toggles {
        if state.expanded.contains(&p) {
            state.expanded.remove(&p);
        } else {
            state.expanded.insert(p.clone());
            if !state.listings.contains_key(&p) {
                state.loading.insert(p.clone());
                actions.push(FileAction::List(p));
            }
        }
    }
    // 应用导航（具体的加载/展开由 sync_tree 统一处理）
    if let Some(p) = select {
        state.cwd = p;
        state.selected.clear();
    }
    // 应用拖入树节点的移动：乐观地从当前目录移除被移动项（避免整目录刷新跳动），
    // 记录撤销，再发起远端 mv（目标目录由 worker 的 OpDone 刷新）。
    if let Some((srcs, dest_dir)) = drop {
        if dest_dir != state.cwd {
            let cwd = state.cwd.clone();
            if let Some(list) = state.listings.get_mut(&cwd) {
                let moved: HashSet<String> = srcs.iter().cloned().collect();
                list.retain(|e| !moved.contains(&join_path(&cwd, &e.name)));
            }
        }
        state.record_move(srcs.clone(), dest_dir.clone());
        actions.push(FileAction::Move { srcs, dest_dir });
        state.selected.clear();
        state.anchor = None;
    }
}

/// 自 root 起按 cwd 路径逐级展开树，并请求缺失目录的列表。
pub(super) fn sync_tree(state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    if state.cwd.is_empty() {
        return;
    }
    for anc in ancestors(&state.cwd) {
        state.expanded.insert(anc.clone());
        if !state.listings.contains_key(&anc) && !state.loading.contains(&anc) {
            state.loading.insert(anc.clone());
            actions.push(FileAction::List(anc));
        }
    }
}

/// 路径的所有前缀（含自身），如 "/a/b" -> ["/", "/a", "/a/b"]。
fn ancestors(path: &str) -> Vec<String> {
    let mut out = vec!["/".to_string()];
    let mut cur = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        cur.push('/');
        cur.push_str(seg);
        out.push(cur.clone());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn draw_node(
    ui: &mut egui::Ui,
    state: &FilePanelState,
    path: &str,
    label: &str,
    depth: usize,
    toggles: &mut Vec<String>,
    select: &mut Option<String>,
    drop: &mut Option<(Vec<String>, String)>,
    spring: &mut Option<String>,
) {
    let expanded = state.expanded.contains(path);
    let is_cwd = state.cwd == path;
    // 用 phosphor 图标，避免 ▸/▾ 在字体缺字形时显示成方块
    let tri = if expanded {
        egui_phosphor::regular::CARET_DOWN
    } else {
        egui_phosphor::regular::CARET_RIGHT
    };
    let folder = if expanded {
        egui_phosphor::regular::FOLDER_OPEN
    } else {
        egui_phosphor::regular::FOLDER
    };
    let color = if is_cwd {
        Palette::ACCENT
    } else {
        Palette::TEXT
    };
    // 整行可点：占满可用宽度的一块可点击区域；单击展开/折叠，双击在右侧列表打开。
    // 行高在文本高度基础上 +10%（更松快的疏密度，便于拖拽落点）。
    let font = egui::TextStyle::Body.resolve(ui.style());
    let row_h = (ui.text_style_height(&egui::TextStyle::Body) + 1.0) * 1.1;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
    // 拖拽目标：把文件列表里的项拖到树中的文件夹上。悬停高亮 + 登记弹簧目标（停留展开/折叠），
    // 在该节点上松手即移入该目录。
    let dragging_in = resp.dnd_hover_payload::<DragPaths>().is_some();
    if is_cwd || dragging_in {
        ui.painter().rect_filled(rect, 4.0, Palette::ACCENT_SOFT);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_black_alpha(8));
    }
    if dragging_in {
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.5, Palette::ACCENT),
            egui::StrokeKind::Inside,
        );
        *spring = Some(path.to_string());
    }
    if let Some(payload) = resp.dnd_release_payload::<DragPaths>() {
        let srcs = valid_move_srcs(&payload.0, path);
        if !srcs.is_empty() {
            *drop = Some((srcs, path.to_string()));
        }
    }
    ui.painter().text(
        egui::pos2(rect.left() + depth as f32 * 12.0 + 4.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{tri} {folder} {label}"),
        font,
        color,
    );
    // 双击的第二次点击同帧也报 clicked：若不排除，双击 = toggle 两次（展开又收起，
    // 还可能重复发起列目录）。排除后双击效果 = 首击的 toggle + 导航，与单击展开态一致。
    if resp.clicked() && !resp.double_clicked() {
        toggles.push(path.to_string());
    }
    if resp.double_clicked() {
        *select = Some(path.to_string());
    }

    if expanded {
        if let Some(entries) = state.listings.get(path) {
            for e in entries.iter().filter(|e| e.is_dir) {
                let child = join_path(path, &e.name);
                draw_node(
                    ui,
                    state,
                    &child,
                    &e.name,
                    depth + 1,
                    toggles,
                    select,
                    drop,
                    spring,
                );
            }
        } else if state.loading.contains(path) {
            ui.horizontal(|ui| {
                ui.add_space((depth as f32 + 1.0) * 12.0);
                ui.spinner();
            });
        }
    }
}
