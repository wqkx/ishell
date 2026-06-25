//! 右下文件操作区：左侧树形目录 + 右侧文件列表。
//! 支持：进入/刷新目录、拖拽上传、右键下载/删除/重命名/改权限、复制路径、新建文件/目录。

use std::collections::{HashMap, HashSet};

use egui::{RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::proto::FileEntry;
use crate::theme::Palette;
use crate::ui::fmt_bytes;

/// 文件面板状态（每个会话一份）。
#[derive(Default)]
pub struct FilePanelState {
    /// 树根（默认 "/"）
    pub root: String,
    /// 右栏当前目录（绝对路径）
    pub cwd: String,
    /// 路径 -> 该目录的条目（树与右栏共用）
    pub listings: HashMap<String, Vec<FileEntry>>,
    /// 树中已展开的目录
    pub expanded: HashSet<String>,
    /// 正在加载的目录
    pub loading: HashSet<String>,
    /// 右栏当前选中的行索引（支持多选）
    pub selected: HashSet<usize>,
    /// 区间选择的锚点行
    pub anchor: Option<usize>,
    /// 正在原地重命名的行（含输入缓冲与是否首帧）
    pub renaming: Option<Renaming>,
    /// 待触发重命名（行索引, 单击时刻）——延时以避开双击打开
    pub pending_rename: Option<(usize, f64)>,
    /// 路径编辑模式（双击路径栏进入），Some 时显示输入框
    pub path_edit: Option<String>,
    /// 路径编辑框是否需要请求焦点（仅进入时一次）
    pub path_edit_focus: bool,
    /// 上次已同步到树的 cwd（仅在 cwd 变化时同步，允许手动折叠）
    pub synced_cwd: String,
    /// 当前弹出的对话框
    pub dialog: Option<Dialog>,
    /// 列表排序键 + 是否降序
    pub sort_key: SortKey,
    pub sort_desc: bool,
    /// 按名称过滤当前目录列表（不区分大小写子串匹配）
    pub filter: String,
    /// 「复制路径」按钮的成功反馈时刻（短暂显示对勾）
    pub copy_flash: Option<f64>,
    /// 面包屑单击导航的延后执行（路径, 单击时刻）——给双击编辑留判定窗口
    pub pending_nav: Option<(String, f64)>,
    /// 弹簧式拖拽导航：当前悬停目标目录与起始时刻 (目标路径, 悬停起点)。
    /// 目标可为「上级目录」(parent) 或某个文件夹的绝对路径。
    pub spring_since: Option<(String, f64)>,
    /// 弹簧式跳转动画的触发时刻（在指针处播放两次脉冲环）
    pub spring_flash: Option<f64>,
    /// 拖到「文件树」节点上的悬停计时 (节点路径, 悬停起点)：停留够久则展开/折叠该节点。
    /// 完成一次切换后存 +∞ 作为哨兵，避免在同一节点上反复翻转（移开再回来才会再次触发）。
    pub tree_spring_since: Option<(String, f64)>,
    /// 移动操作撤销栈（Ctrl+Z 逐步回退最近的拖拽移动）。
    pub move_undo: Vec<MoveRecord>,
    /// 上传成功后待选中的项：(目录, 文件名集合)。该目录刷新渲染时按名选中并清空。
    pub pending_select: Option<(String, std::collections::HashSet<String>)>,
    /// 「返回上一个目录」历史栈（浏览器式后退；区别于「上级目录」=parent）。
    pub nav_history: Vec<String>,
    /// 上一帧末的 cwd，用于检测目录切换并把旧目录压入历史。
    pub nav_prev: String,
    /// 本次切换由「后退」触发（不再压栈，避免来回循环）。
    pub nav_pending_back: bool,
}

/// 一次「移动」的撤销记录：被移动项的原始绝对路径 + 落入的目标目录。
/// 撤销时把 `dest_dir/<basename>` 移回各自原父目录。
pub struct MoveRecord {
    /// 移动前各项的绝对路径（同一次拖拽必同源，故共享父目录）
    pub original: Vec<String>,
    /// 移动落入的目标目录
    pub dest_dir: String,
}

impl FilePanelState {
    /// 记录一次移动以供撤销；限制栈深度避免无限增长。
    fn record_move(&mut self, original: Vec<String>, dest_dir: String) {
        if original.is_empty() {
            return;
        }
        self.move_undo.push(MoveRecord { original, dest_dir });
        if self.move_undo.len() > 50 {
            self.move_undo.remove(0);
        }
    }
}

/// 拖拽悬停多久后自动进入目标目录（秒）
const UP_DWELL: f64 = 0.8;
/// 跳转动画时长（秒，期间播放两次脉冲）
const UP_FLASH: f64 = 0.5;

/// 文件列表排序键。
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Mtime,
}

/// 原地重命名状态。
pub struct Renaming {
    pub idx: usize,
    pub buf: String,
    pub init: bool,
}

/// 模态小对话框。
pub enum Dialog {
    NewDir { name: String },
    NewFile { name: String },
    Upload { local: String },
    Chmod { path: String, mode: u32, name: String },
    Rename { path: String, name: String },
    ConfirmDelete { items: Vec<(String, bool, String)> }, // (path, is_dir, name)，支持多选批量删除
    ConfirmOpenLarge { path: String, size: u64 },
    /// 撤销移动确认：what=文件名概述，from=当前所在目录，to=移回的原目录（栈顶记录的预览）
    ConfirmUndoMove { what: String, from: String, to: String },
}

/// 面板交互产生的动作，由 App 翻译为 SFTP 指令或剪贴板操作。
pub enum FileAction {
    /// 请求列出目录（展开树 / 刷新 / 进入）
    List(String),
    Download(String),
    Upload { local: String, remote_dir: String },
    Mkdir(String),
    CreateFile(String),
    Chmod { path: String, mode: u32 },
    /// 批量删除：一条 rm 处理所有路径（单通道），避免多文件时并发开太多 SSH 通道被服务端拒绝
    DeleteMany(Vec<String>),
    Rename { from: String, to: String },
    /// 复制选中项到 App 级剪贴板（跨 tab 共享）；每项 (绝对路径, 是否目录)
    ClipCopy { items: Vec<(String, bool)> },
    /// 剪切选中项到剪贴板（粘贴时为移动，会在删除前二次确认）
    ClipCut { items: Vec<(String, bool)> },
    /// 把剪贴板内容粘贴到当前目录（同机用 cp/mv，跨机走下载→上传中转，由 App 决定）
    Paste { dest_dir: String },
    /// 在同一会话内拖拽移动：把 srcs 移动到 dest_dir（远端 mv）
    Move { srcs: Vec<String>, dest_dir: String },
    CopyPath(String),
    /// 双击文本文件 -> 打开编辑器（force=true 放宽大小限制）
    OpenFile { path: String, force: bool },
    /// 双击图片文件 -> 打开看图工具
    OpenImage { path: String },
    /// 在终端 cd 到该目录并聚焦终端
    CdTerminal(String),
    /// 直接设置状态栏文案（用于撤销等即时提示）
    Status(String),
}

/// 是否为可用看图工具打开的常见图片扩展名。
pub fn is_image_path(name: &str) -> bool {
    let lower = name.rsplit('.').next().map(|e| e.to_ascii_lowercase());
    matches!(lower.as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "bmp"))
}

impl FilePanelState {
    /// 收到目录列表后由 App 调用：写入缓存并清除 loading；首次自动设为 cwd。
    pub fn on_listing(&mut self, path: String, entries: Vec<FileEntry>) {
        self.loading.remove(&path);
        self.listings.insert(path.clone(), entries);
        if self.cwd.is_empty() {
            self.cwd = path;
        }
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut FilePanelState, has_clip: bool) -> Vec<FileAction> {
    let mut actions = Vec::new();

    // 导航历史：检测上一帧的目录切换；非「后退」触发时，把旧目录压入历史，供「返回上一个目录」用。
    if state.cwd != state.nav_prev {
        if state.nav_pending_back {
            state.nav_pending_back = false;
        } else if !state.nav_prev.is_empty() {
            state.nav_history.push(state.nav_prev.clone());
            if state.nav_history.len() > 100 {
                state.nav_history.remove(0);
            }
        }
        state.nav_prev = state.cwd.clone();
    }

    // 拖入文件 -> 上传到当前目录
    let dropped: Vec<String> = ui.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy().into_owned()))
            .collect()
    });
    if !dropped.is_empty() && !state.cwd.is_empty() {
        for local in dropped {
            actions.push(FileAction::Upload { local, remote_dir: state.cwd.clone() });
        }
    }

    // 仅在右栏目录“变化”时同步树（展开到 cwd）；其余时间允许用户自由折叠
    if state.cwd != state.synced_cwd {
        sync_tree(state, &mut actions);
        state.synced_cwd = state.cwd.clone();
        state.filter.clear(); // 切目录后清空过滤
    }

    // 左侧目录树（自带浅色卡片，与右侧留出空隙）
    egui::Panel::left("file_tree")
        .resizable(true)
        .default_size(232.0)
        .size_range(150.0..=460.0)
        .frame(
            egui::Frame::new()
                .fill(Palette::PANEL_2)
                .inner_margin(egui::Margin { left: 8, right: 6, top: 6, bottom: 6 })
                .outer_margin(egui::Margin { left: 0, right: 8, top: 0, bottom: 0 }),
        )
        .show_inside(ui, |ui| {
            ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::TREE_VIEW, crate::i18n::tr("目录树", "Files"))).strong().color(Palette::ACCENT));
            ui.separator();
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                tree(ui, state, &mut actions);
            });
        });

    // 右侧：工具栏 + 文件列表（左侧留白，避免文件名贴到目录树）
    file_list(ui, state, has_clip, &mut actions);

    // 对话框
    dialogs(ui, state, &mut actions);

    actions
}

/// 绘制目录树（从 root 递归）。
fn tree(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
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
    draw_node(ui, state, &root, &root, 0, &mut toggles, &mut select, &mut drop, &mut spring);

    // 弹簧式展开/折叠：在某树节点上持续悬停 UP_DWELL 秒则切换其展开态，并在指针处双闪；
    // 切换后把计时设为 +∞ 哨兵，避免同一节点反复翻转（移开再回来方可再次触发）。
    let now = ui.input(|i| i.time);
    if let Some(tp) = spring.clone() {
        let armed = matches!(&state.tree_spring_since, Some((k, _)) if *k == tp);
        if !armed {
            state.tree_spring_since = Some((tp.clone(), now));
        }
        let since = state.tree_spring_since.as_ref().map(|(_, t)| *t).unwrap_or(now);
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
fn sync_tree(state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
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
    let tri = if expanded { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_RIGHT };
    let folder = if expanded { egui_phosphor::regular::FOLDER_OPEN } else { egui_phosphor::regular::FOLDER };
    let color = if is_cwd { Palette::ACCENT } else { Palette::TEXT };
    // 整行可点：占满可用宽度的一块可点击区域；单击展开/折叠，双击在右侧列表打开。
    // 行高在文本高度基础上 +10%（更松快的疏密度，便于拖拽落点）。
    let font = egui::TextStyle::Body.resolve(ui.style());
    let row_h = (ui.text_style_height(&egui::TextStyle::Body) + 1.0) * 1.1;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::click());
    // 拖拽目标：把文件列表里的项拖到树中的文件夹上。悬停高亮 + 登记弹簧目标（停留展开/折叠），
    // 在该节点上松手即移入该目录。
    let dragging_in = resp.dnd_hover_payload::<DragPaths>().is_some();
    if is_cwd {
        ui.painter().rect_filled(rect, 4.0, Palette::ACCENT_SOFT);
    } else if dragging_in {
        ui.painter().rect_filled(rect, 4.0, Palette::ACCENT_SOFT);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_black_alpha(8));
    }
    if dragging_in {
        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.5, Palette::ACCENT), egui::StrokeKind::Inside);
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
    if resp.clicked() {
        toggles.push(path.to_string());
    }
    if resp.double_clicked() {
        *select = Some(path.to_string());
    }

    if expanded {
        if let Some(entries) = state.listings.get(path) {
            for e in entries.iter().filter(|e| e.is_dir) {
                let child = join_path(path, &e.name);
                draw_node(ui, state, &child, &e.name, depth + 1, toggles, select, drop, spring);
            }
        } else if state.loading.contains(path) {
            ui.horizontal(|ui| {
                ui.add_space((depth as f32 + 1.0) * 12.0);
                ui.spinner();
            });
        }
    }
}

/// 右侧：工具栏 + 文件表格。
fn file_list(ui: &mut egui::Ui, state: &mut FilePanelState, has_clip: bool, actions: &mut Vec<FileAction>) {
    // 工具栏：扁平图标条（带浅色背景）
    use egui_phosphor::regular as icon;
    let mut bc_nav: Option<String> = None;
    let mut paste_here = false; // 工具栏/菜单粘贴触发（has_clip 由 App 传入）
    // 弹簧式拖拽导航：本帧拖拽悬停的目标目录（Up 按钮的上一层 / 某文件夹），统一计时跳转
    let mut spring_target: Option<String> = None;
    // 拖到「上级目录」按钮上释放的移动（与文件夹落点统一在表格后处理，便于记录撤销）
    let mut up_move: Option<(Vec<String>, String)> = None;
    egui::Frame::new()
        .fill(Palette::PANEL_2)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if tool_btn(ui, icon::ARROW_CLOCKWISE, crate::i18n::tr("刷新", "Refresh")) && !state.cwd.is_empty() {
                    state.listings.remove(&state.cwd);
                    state.loading.insert(state.cwd.clone());
                    actions.push(FileAction::List(state.cwd.clone()));
                }
                // 返回上一个目录（浏览器式后退，可连续返回；区别于「上级目录」=parent）
                let back_enabled = !state.nav_history.is_empty();
                let back_col = if back_enabled { Palette::TEXT } else { Palette::TEXT_DIM };
                if tool_btn_color(ui, icon::ARROW_LEFT, crate::i18n::tr("返回上一个目录", "Back"), back_col) && back_enabled {
                    if let Some(prev) = state.nav_history.pop() {
                        state.nav_pending_back = true;
                        state.cwd = prev;
                        state.selected.clear();
                    }
                }
                let up_resp = tool_btn_resp(ui, icon::ARROW_UP, crate::i18n::tr("上级目录", "Up"), Palette::TEXT);
                if up_resp.clicked() && !state.cwd.is_empty() {
                    state.cwd = parent_of(&state.cwd);
                    state.selected.clear();
                }
                handle_up_drag(ui, state, &up_resp, &mut up_move, &mut spring_target);
                // 上传 / 删除已从工具栏移除：上传走拖拽或空白处右键菜单，删除走右键菜单或 Delete 键
                // 粘贴：剪贴板有内容时显示（内容/数量由 App 持有）
                if has_clip
                    && tool_btn_color(ui, icon::CLIPBOARD_TEXT, crate::i18n::tr("粘贴到此目录", "Paste here"), Palette::ACCENT)
                {
                    paste_here = true;
                }
                // 复制路径：点击后短暂显示绿色对勾，再恢复
                let now = ui.input(|i| i.time);
                let copied = state.copy_flash.map_or(false, |t| now - t < 1.1);
                let (ci, ctip, ccol) = if copied {
                    (icon::CHECK, crate::i18n::tr("已复制", "Copied"), Palette::OK)
                } else {
                    (icon::COPY, crate::i18n::tr("复制当前路径", "Copy path"), Palette::TEXT)
                };
                if tool_btn_color(ui, ci, ctip, ccol) && !state.cwd.is_empty() {
                    actions.push(FileAction::CopyPath(state.cwd.clone()));
                    state.copy_flash = Some(now);
                }
                if copied {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                }

                ui.add_space(4.0);
                ui.separator();
                if state.path_edit.is_some() {
                    // 路径编辑模式：回车跳转、Esc 或点击别处退出
                    let mut go: Option<String> = None;
                    let mut done = false;
                    let take_focus = state.path_edit_focus;
                    if let Some(buf) = &mut state.path_edit {
                        let out = egui::TextEdit::singleline(buf)
                            .desired_width(ui.available_width() - 4.0)
                            .hint_text(crate::i18n::tr("输入路径后回车跳转，Esc 取消", "Enter path, Enter to go, Esc to cancel"))
                            .show(ui);
                        if take_focus {
                            // 首帧进入编辑：聚焦并全选路径，便于直接覆盖输入
                            let len = buf.chars().count();
                            let id = out.response.id;
                            let mut st = out.state;
                            st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(len),
                            )));
                            st.store(ui.ctx(), id);
                            out.response.request_focus();
                        }
                        let resp = &out.response;
                        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            let t = buf.trim();
                            if !t.is_empty() {
                                go = Some(t.to_string());
                            }
                            done = true;
                        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            done = true;
                        } else if resp.lost_focus() || resp.clicked_elsewhere() {
                            // 点击别处 -> 退出编辑模式
                            done = true;
                        }
                    }
                    state.path_edit_focus = false;
                    if let Some(p) = go {
                        bc_nav = Some(p);
                    }
                    if done {
                        state.path_edit = None;
                    }
                } else {
                    // 面包屑：路径超长时横向滚动（隐藏滚动条，滚轮滚动）；
                    // 单击逐级跳转，双击路径任意处（含分段、空白）进入编辑模式。
                    // 单击导航延后 ~0.28s 执行，期间若发生双击则取消，避免双击误触发跳转。
                    let now_t = ui.input(|i| i.time);
                    let cwd_s = state.cwd.clone();
                    let mut enter_edit = false;
                    let mut nav_click: Option<String> = None;
                    egui::ScrollArea::horizontal()
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .auto_shrink([false, false])
                        .stick_to_right(true) // 路径过长时默认展示末尾（当前目录）
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 3.0;
                                let root = ui.add(egui::Label::new(RichText::new(icon::HOUSE).color(Palette::ACCENT)).sense(Sense::click()));
                                if root.double_clicked() {
                                    enter_edit = true;
                                } else if root.clicked() {
                                    nav_click = Some("/".into());
                                }
                                let mut acc = String::new();
                                for seg in cwd_s.split('/').filter(|s| !s.is_empty()) {
                                    ui.label(RichText::new("›").color(Palette::TEXT_DIM));
                                    acc.push('/');
                                    acc.push_str(seg);
                                    let here = acc.clone();
                                    let is_last = here == cwd_s;
                                    let color = if is_last { Palette::TEXT } else { Palette::TEXT_DIM };
                                    let r = ui.add(egui::Label::new(RichText::new(seg).color(color)).sense(Sense::click()));
                                    if r.double_clicked() {
                                        enter_edit = true;
                                    } else if r.clicked() {
                                        nav_click = Some(here);
                                    }
                                }
                                // 末尾空白：双击也进入编辑
                                let rest = ui.available_size_before_wrap();
                                if rest.x > 8.0 {
                                    let (_, resp) = ui.allocate_exact_size(rest, Sense::click());
                                    if resp.double_clicked() {
                                        enter_edit = true;
                                    }
                                }
                            });
                        });
                    if enter_edit {
                        state.path_edit = Some(state.cwd.clone());
                        state.path_edit_focus = true;
                        state.pending_nav = None;
                    } else if let Some(p) = nav_click {
                        state.pending_nav = Some((p, now_t));
                    }
                    // 延时执行单击导航（其间未发生双击）
                    if let Some((p, t)) = state.pending_nav.clone() {
                        if now_t - t >= 0.28 {
                            bc_nav = Some(p);
                            state.pending_nav = None;
                        } else {
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                        }
                    }
                }
            });
        });
    ui.add_space(2.0);
    if let Some(p) = bc_nav {
        // 规范化：去掉末尾多余的 "/"（否则与 worker 返回的规范路径不匹配，无法进入）
        state.cwd = normalize_path(&p);
        state.selected.clear();
    }
    ui.separator();

    if state.loading.contains(&state.cwd) && !state.listings.contains_key(&state.cwd) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(crate::i18n::tr("加载中 …", "Loading …")).color(Palette::TEXT_DIM));
        });
        return;
    }

    let cwd = state.cwd.clone();
    let total_count = state.listings.get(&cwd).map(|e| e.len()).unwrap_or(0);

    // 名称过滤行
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(Palette::TEXT_DIM).size(12.0));
        let clear_w = if state.filter.is_empty() { 0.0 } else { 22.0 };
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .desired_width(ui.available_width() - clear_w - 4.0)
                .hint_text(crate::i18n::tr("按名称过滤", "Filter by name")),
        );
        if resp.changed() {
            // 过滤变化会改变行索引，清空选择避免错位
            state.selected.clear();
            state.anchor = None;
        }
        if !state.filter.is_empty()
            && ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                .on_hover_text(crate::i18n::tr("清除过滤", "Clear"))
                .clicked()
        {
            state.filter.clear();
        }
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
                if desc { ord.reverse() } else { ord }
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
                ui.label(RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("无匹配（共 {total_count} 项）"),
                    crate::i18n::Lang::En => format!("No match ({total_count} items)"),
                }).color(Palette::TEXT_DIM).size(12.0));
            });
        }
    }

    // 上传成功后选中所传文件：当前目录刷新出新列表时，按文件名定位行号并选中（命中后才清空，
    // 以兼容「刷新尚未到达、文件暂未出现」的情形）。
    if let Some(names) = state.pending_select.as_ref().filter(|(d, _)| *d == cwd).map(|(_, n)| n.clone()) {
        let sel: HashSet<usize> = entries.iter().enumerate().filter(|(_, e)| names.contains(&e.name)).map(|(i, _)| i).collect();
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
    let mut confirm_open: Option<(String, u64)> = None; // 大文件待确认
    let mut rename_commit: Option<(String, String)> = None;
    let mut cancel_rename = false;
    let mut drop_move: Option<(Vec<String>, String)> = None; // 拖拽释放到某文件夹 -> (srcs, dest_dir)
    let now = ui.input(|i| i.time);
    let (mod_ctrl, mod_shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

    // 空白区域右键 -> 新建文件/目录（仅覆盖列表区域，避免遮挡上方路径栏）
    let bg = ui.interact(ui.available_rect_before_wrap(), ui.id().with("filelist_bg"), Sense::click());
    // 注意：bg 几何上覆盖整个列表，dnd_release_payload 用 contains_pointer 判定并「取走」载荷，
    // 若在此处先检查会把本应落到文件夹行的拖放抢走。故放到表格之后、仅当无行接收时再兜底。
    let mut bg_new_dir = false;
    let mut bg_new_file = false;
    let mut bg_upload = false;
    let mut bg_cd = false;
    bg.context_menu(|ui| {
        if ui.button(format!("{}  {}", icon::FOLDER_PLUS, crate::i18n::tr("新建文件夹", "New folder"))).clicked() {
            bg_new_dir = true;
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::FILE_PLUS, crate::i18n::tr("新建文件", "New file"))).clicked() {
            bg_new_file = true;
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::UPLOAD_SIMPLE, crate::i18n::tr("上传文件", "Upload"))).clicked() {
            bg_upload = true;
            ui.close();
        }
        if has_clip {
            ui.separator();
            if ui.button(format!("{}  {}", icon::CLIPBOARD_TEXT, crate::i18n::tr("粘贴到此目录", "Paste here"))).clicked() {
                paste_here = true;
                ui.close();
            }
        }
        if !state.cwd.is_empty() {
            ui.separator();
            if ui.button(format!("{}  {}", icon::TERMINAL_WINDOW, crate::i18n::tr("在终端打开当前目录", "Open current dir in terminal"))).clicked() {
                bg_cd = true;
                ui.close();
            }
        }
    });

    egui::Frame::new()
        .inner_margin(egui::Margin { left: 6, right: 2, top: 0, bottom: 0 })
        .show(ui, |ui| {
    // 拖拽悬停高亮用独立图层绘制：TableBuilder 闭包内 ui 已被可变借用，不能再取 ui.painter()
    let dnd_painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("file_dnd_hl")));
    TableBuilder::new(ui)
        // 按目录区分滚动状态：进入子目录/切换目录后从顶部开始，不沿用上个目录的滚动位置
        .id_salt(&cwd)
        .striped(true)
        .resizable(true)
        .sense(Sense::click_and_drag())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::auto().at_least(190.0).clip(true))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(140.0))
        .column(Column::auto().at_least(96.0))
        // owner 列用 auto 而非 remainder：列表填不满时右侧留白，便于右键空白处操作当前文件夹
        .column(Column::auto().at_least(120.0))
        .header(22.0, |mut h| {
            let (cur, desc) = (state.sort_key, state.sort_desc);
            // 可排序表头：点击切换排序键/方向，激活列显示升降箭头
            for (k, label) in [
                (SortKey::Name, crate::i18n::tr("名称", "Name")),
                (SortKey::Size, crate::i18n::tr("大小", "Size")),
                (SortKey::Mtime, crate::i18n::tr("修改时间", "Modified")),
            ] {
                h.col(|ui| {
                    let arrow = if cur == k {
                        if desc { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_UP }
                    } else {
                        ""
                    };
                    let color = if cur == k { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(format!("{label} {arrow}")).strong().color(color)).sense(Sense::click()))
                        .clicked()
                    {
                        sort_click = Some(k);
                    }
                });
            }
            // 不可排序：权限 / 所有者
            for t in [crate::i18n::tr("权限", "Perm"), crate::i18n::tr("所有者", "Owner")] {
                h.col(|ui| {
                    ui.label(RichText::new(t).strong().color(Palette::TEXT_DIM));
                });
            }
        })
        .body(|mut body| {
            for (i, e) in entries.iter().enumerate() {
                let full = join_path(&cwd, &e.name);
                body.row(21.0, |mut row| {
                    row.set_selected(state.selected.contains(&i));
                    row.col(|ui| {
                        // 重命名中：显示输入框（默认选中后缀前的主名）
                        let renaming_here = matches!(&state.renaming, Some(r) if r.idx == i);
                        if renaming_here {
                            if let Some(r) = &mut state.renaming {
                                let out = egui::TextEdit::singleline(&mut r.buf)
                                    .desired_width(f32::INFINITY)
                                    .show(ui);
                                if r.init {
                                    let stem = stem_char_len(&r.buf);
                                    let id = out.response.id;
                                    let mut st = out.state;
                                    st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                        egui::text::CCursor::new(0),
                                        egui::text::CCursor::new(stem),
                                    )));
                                    st.store(ui.ctx(), id);
                                    out.response.request_focus();
                                    r.init = false;
                                }
                                if out.response.lost_focus() {
                                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !r.buf.trim().is_empty() {
                                        rename_commit = Some((full.clone(), join_path(&cwd, r.buf.trim())));
                                    }
                                    cancel_rename = true;
                                }
                            }
                        } else {
                            let icon_col = if e.is_dir {
                                Palette::ACCENT
                            } else if e.is_link {
                                Palette::WARN
                            } else {
                                Palette::TEXT_DIM
                            };
                            ui.spacing_mut().item_spacing.x = 5.0;
                            ui.label(RichText::new(file_icon(e)).color(icon_col));
                            ui.label(RichText::new(&e.name).color(Palette::TEXT));
                        }
                    });
                    row.col(|ui| {
                        let s = if e.is_dir { "-".to_string() } else { fmt_bytes(e.size as f64) };
                        ui.label(RichText::new(s).color(Palette::TEXT_DIM));
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(fmt_mtime(e.mtime)).color(Palette::TEXT_DIM));
                    });
                    row.col(|ui| {
                        ui.label(
                            RichText::new(perm_string(e.perm, e.is_dir, e.is_link))
                                .monospace()
                                .color(Palette::TEXT_DIM),
                        );
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(&e.owner).color(Palette::TEXT_DIM));
                    });
                    // 整行交互：点击选择 / 延时重命名、双击进目录或打开、右键菜单
                    let r = row.response();
                    if r.clicked() {
                        let was_sole = state.selected.len() == 1
                            && state.selected.contains(&i)
                            && state.renaming.is_none();
                        if was_sole && !mod_ctrl && !mod_shift {
                            // 已选中的行再次单击 -> 计划重命名（延时以避开双击）
                            state.pending_rename = Some((i, now));
                        } else {
                            clicks.push(i);
                        }
                    }
                    if r.double_clicked() {
                        state.pending_rename = None;
                        if e.is_dir {
                            navigate = Some(full.clone());
                        } else if is_image_path(&e.name) {
                            // 图片文件 -> 看图工具
                            open_image = Some(full.clone());
                        } else if e.size > 4 * 1024 * 1024 {
                            // 大文件先确认
                            confirm_open = Some((full.clone(), e.size));
                        } else {
                            // 任意文件都尝试打开，由后台智能判断是否文本（拒绝二进制）
                            open_file = Some(full.clone());
                        }
                    }
                    if r.secondary_clicked() {
                        rclick = Some(i);
                    }
                    // 拖拽源：整个拖动过程持续写入载荷（多选则整组、否则单项），
                    // 避免只在 drag_started 一帧设置时偶发丢失/沿用上次的旧载荷。
                    if r.drag_started() || r.dragged() {
                        let paths = drag_source_paths(state, &entries, &cwd, i);
                        if !paths.is_empty() {
                            r.dnd_set_drag_payload(DragPaths(paths));
                        }
                    }
                    // 拖拽目标：
                    // - 文件夹行：悬停高亮 + 登记弹簧目标（停留进入），释放→移入该文件夹；
                    // - 文件行：释放→移入当前目录（便于弹簧进入文件夹后在其内任意处松手）。
                    if e.is_dir {
                        if r.dnd_hover_payload::<DragPaths>().is_some() {
                            dnd_painter.rect_stroke(
                                r.rect,
                                4.0,
                                egui::Stroke::new(1.5, Palette::ACCENT),
                                egui::StrokeKind::Inside,
                            );
                            spring_target = Some(full.clone());
                        }
                        if let Some(payload) = r.dnd_release_payload::<DragPaths>() {
                            let srcs = valid_move_srcs(&payload.0, &full);
                            if !srcs.is_empty() {
                                drop_move = Some((srcs, full.clone()));
                            }
                        }
                    } else if let Some(payload) = r.dnd_release_payload::<DragPaths>() {
                        let srcs = valid_move_srcs(&payload.0, &cwd);
                        if !srcs.is_empty() {
                            drop_move = Some((srcs, cwd.clone()));
                        }
                    }
                    entry_context(&r, e, i, &full, has_clip, &mut menu);
                });
            }
        });
    });

    // 拖拽预览：拖动中在指针旁画一个跟手的小标签（单项显示名称，多项显示数量），
    // 给「文件正被拖动」一个明确的视觉反馈。
    if let Some(payload) = egui::DragAndDrop::payload::<DragPaths>(ui.ctx()) {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let n = payload.0.len();
            let text = if n == 1 {
                payload.0[0].rsplit('/').next().unwrap_or("").to_string()
            } else {
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("移动 {n} 项"),
                    crate::i18n::Lang::En => format!("Move {n} items"),
                }
            };
            let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("file_drag_preview")));
            let font = egui::FontId::proportional(12.0);
            let galley = painter.layout_no_wrap(text, font, egui::Color32::WHITE);
            let pad = egui::vec2(8.0, 4.0);
            let rect = egui::Rect::from_min_size(pos + egui::vec2(14.0, 8.0), galley.size() + pad * 2.0);
            painter.rect_filled(rect, 6.0, Palette::ACCENT.gamma_multiply(0.95));
            painter.galley(rect.min + pad, galley, egui::Color32::WHITE);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    }

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
        state.dialog = Some(Dialog::NewDir { name: String::new() });
    }
    if bg_new_file {
        state.dialog = Some(Dialog::NewFile { name: String::new() });
    }
    if bg_upload {
        state.dialog = Some(Dialog::Upload { local: String::new() });
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
                state.renaming = Some(Renaming { idx: i, buf: e.name.clone(), init: true });
            }
            state.pending_rename = None;
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(450));
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
    // 双击打开文本文件
    if let Some(p) = open_file {
        actions.push(FileAction::OpenFile { path: p, force: false });
    }
    // 双击打开图片
    if let Some(p) = open_image {
        actions.push(FileAction::OpenImage { path: p });
    }
    if let Some((p, size)) = confirm_open {
        state.dialog = Some(Dialog::ConfirmOpenLarge { path: p, size });
    }

    // 处理选择（单选 / Ctrl 多选 / Shift 区间）
    for i in clicks {
        if mod_shift {
            if let Some(a) = state.anchor {
                let (lo, hi) = (a.min(i), a.max(i));
                if !mod_ctrl {
                    state.selected.clear();
                }
                for k in lo..=hi {
                    state.selected.insert(k);
                }
            } else {
                state.selected.insert(i);
                state.anchor = Some(i);
            }
        } else if mod_ctrl {
            if !state.selected.remove(&i) {
                state.selected.insert(i);
            }
            state.anchor = Some(i);
        } else {
            state.selected.clear();
            state.selected.insert(i);
            state.anchor = Some(i);
        }
    }

    // 右键命中不在选区的行 -> 改为只选它（让批量操作对象明确）
    if let Some(i) = rclick {
        if !state.selected.contains(&i) {
            state.selected.clear();
            state.selected.insert(i);
            state.anchor = Some(i);
        }
    }

    // 处理右键菜单结果：下载/复制立即成动作，改权限/重命名/删除打开对话框
    for m in menu {
        match m {
            Menu::Download(idx) => {
                // 多选时批量下载（文件与文件夹均可，文件夹递归下载）
                let targets: Vec<usize> = if state.selected.contains(&idx) && state.selected.len() > 1 {
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
                let targets: Vec<usize> = if state.selected.contains(&idx) && state.selected.len() > 1 {
                    let mut v: Vec<usize> = state.selected.iter().copied().collect();
                    v.sort_unstable();
                    v
                } else {
                    vec![idx]
                };
                let items: Vec<(String, bool, String)> = targets
                    .iter()
                    .filter_map(|&k| entries.get(k).map(|e| (join_path(&cwd, &e.name), e.is_dir, e.name.clone())))
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
            Menu::NewDir => state.dialog = Some(Dialog::NewDir { name: String::new() }),
            Menu::NewFile => state.dialog = Some(Dialog::NewFile { name: String::new() }),
            Menu::CdHere(p) => actions.push(FileAction::CdTerminal(p)),
        }
    }

    // 粘贴：工具栏按钮 / 右键菜单触发；具体同机或跨机、是否确认由 App 决定
    if paste_here && has_clip {
        actions.push(FileAction::Paste { dest_dir: cwd.clone() });
    }

    // Ctrl/Cmd+A 全选当前列表（含已过滤后的所有行）。文本框聚焦/重命名/对话框时不抢，
    // 让 Ctrl+A 仍作用于文本框自身的全选。
    let select_all = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl) && i.key_pressed(egui::Key::A));
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
            .filter_map(|&k| entries.get(k).map(|e| (join_path(&cwd, &e.name), e.is_dir, e.name.clone())))
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
    // 「上级目录」按钮上的释放（几何上独立，已在工具栏阶段取走载荷）兜底并入
    if drop_move.is_none() {
        drop_move = up_move.take();
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
        i.key_pressed(egui::Key::Z) && (i.modifiers.command || i.modifiers.ctrl) && !i.modifiers.shift
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
            state.dialog = Some(Dialog::ConfirmUndoMove { what, from: rec.dest_dir.clone(), to: orig_parent });
        } else {
            // 撤销栈为空：明确告知用户没有可撤销的移动
            actions.push(FileAction::Status(crate::i18n::tr("没有可撤销的移动", "Nothing to undo").into()));
        }
    }

    // 弹簧式拖拽导航：在 Up / 文件夹上持续悬停则进入目标目录，并在指针处双闪
    spring_navigate(ui, state, spring_target, actions);

    if let Some(p) = navigate {
        state.cwd = p;
        state.selected.clear();
    }
}

/// 右键菜单产生的请求（在表格遍历后统一处理，避免在遍历中借用 state）。
enum Menu {
    Download(usize),
    CopyPath(String),
    Chmod { path: String, mode: u32, name: String },
    Rename { path: String, name: String },
    Delete(usize),
    /// 复制 / 剪切右键项（含多选）到剪贴板
    Copy(usize),
    Cut(usize),
    /// 粘贴剪贴板内容到当前目录
    Paste,
    NewDir,
    NewFile,
    CdHere(String),
}

/// 条目右键菜单：把用户选择记录到 `menu`。
fn entry_context(resp: &egui::Response, e: &FileEntry, idx: usize, full: &str, has_clip: bool, menu: &mut Vec<Menu>) {
    use egui_phosphor::regular as icon;
    resp.context_menu(|ui| {
        if ui.button(format!("{}  {}", icon::FOLDER_PLUS, crate::i18n::tr("新建文件夹", "New folder"))).clicked() {
            menu.push(Menu::NewDir);
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::FILE_PLUS, crate::i18n::tr("新建文件", "New file"))).clicked() {
            menu.push(Menu::NewFile);
            ui.close();
        }
        ui.separator();
        let dl_label = if e.is_dir { crate::i18n::tr("下载文件夹", "Download folder") } else { crate::i18n::tr("下载", "Download") };
        if ui.button(format!("{}  {}", icon::DOWNLOAD_SIMPLE, dl_label)).clicked() {
            menu.push(Menu::Download(idx));
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::COPY, crate::i18n::tr("复制路径", "Copy path"))).clicked() {
            menu.push(Menu::CopyPath(full.to_string()));
            ui.close();
        }
        ui.separator();
        // 复制 / 剪切 到剪贴板（含多选）；粘贴到当前目录
        if ui.button(format!("{}  {}", icon::COPY_SIMPLE, crate::i18n::tr("复制", "Copy"))).clicked() {
            menu.push(Menu::Copy(idx));
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::SCISSORS, crate::i18n::tr("剪切", "Cut"))).clicked() {
            menu.push(Menu::Cut(idx));
            ui.close();
        }
        if has_clip && ui.button(format!("{}  {}", icon::CLIPBOARD_TEXT, crate::i18n::tr("粘贴到此目录", "Paste here"))).clicked() {
            menu.push(Menu::Paste);
            ui.close();
        }
        ui.separator();
        if e.is_dir && ui.button(format!("{}  {}", icon::TERMINAL_WINDOW, crate::i18n::tr("在终端打开此目录", "Open in terminal"))).clicked() {
            menu.push(Menu::CdHere(full.to_string()));
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::LOCK_KEY, crate::i18n::tr("改权限", "Chmod"))).clicked() {
            menu.push(Menu::Chmod { path: full.to_string(), mode: e.perm, name: e.name.clone() });
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::PENCIL_SIMPLE, crate::i18n::tr("重命名", "Rename"))).clicked() {
            menu.push(Menu::Rename { path: full.to_string(), name: e.name.clone() });
            ui.close();
        }
        ui.separator();
        if ui.button(RichText::new(format!("{}  {}", icon::TRASH, crate::i18n::tr("删除", "Delete"))).color(Palette::DANGER)).clicked() {
            menu.push(Menu::Delete(idx));
            ui.close();
        }
    });
}

/// 拖拽载荷：被拖动的源绝对路径列表（egui 内部 Arc 持有，需 Send + Sync）。
#[derive(Clone)]
struct DragPaths(Vec<String>);

/// 计算拖拽源：与 clip_targets 同规则，仅取路径。
fn drag_source_paths(state: &FilePanelState, entries: &[FileEntry], cwd: &str, idx: usize) -> Vec<String> {
    clip_targets(state, entries, cwd, idx).into_iter().map(|(p, _)| p).collect()
}

/// 把拖拽到目标目录 dest 的源过滤为合法移动：排除目标自身、已直接位于 dest 内、
/// 以及把祖先目录拖进其子目录的非法情况。
fn valid_move_srcs(srcs: &[String], dest: &str) -> Vec<String> {
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

/// 拖拽到「上级目录」按钮：悬停时高亮并把上一层目录登记为弹簧目标（计时与跳转由
/// `spring_navigate` 统一处理）；在按钮上释放则把文件移动到上一层目录。
fn handle_up_drag(ui: &mut egui::Ui, state: &FilePanelState, up: &egui::Response, up_move: &mut Option<(Vec<String>, String)>, spring_target: &mut Option<String>) {
    if state.cwd.is_empty() || state.cwd == "/" {
        return;
    }
    let parent = parent_of(&state.cwd);
    if let Some(payload) = up.dnd_release_payload::<DragPaths>() {
        let srcs = valid_move_srcs(&payload.0, &parent);
        if !srcs.is_empty() {
            // 统一交由 file_list 的落点处理（乐观移除 + 记录撤销 + 发起 mv）
            *up_move = Some((srcs, parent.clone()));
        }
    } else if up.dnd_hover_payload::<DragPaths>().is_some() {
        // 悬停高亮（无进度条；是否跳转由停留时长决定）
        let rect = up.rect;
        let p = ui.painter();
        p.rect_filled(rect, 6.0, Palette::ACCENT_SOFT);
        p.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, Palette::ACCENT.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        *spring_target = Some(parent);
    }
}

/// 弹簧式拖拽导航的统一处理：在某目标目录上持续悬停 `UP_DWELL` 秒则进入它，
/// 并在指针处播放两次脉冲环动画；继续悬停可逐级连跳。无悬停目标时复位计时。
fn spring_navigate(ui: &mut egui::Ui, state: &mut FilePanelState, spring_target: Option<String>, actions: &mut Vec<FileAction>) {
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
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("spring_flash")));
                painter.circle_stroke(pos, r, egui::Stroke::new(2.0, Palette::ACCENT.gamma_multiply(k.max(0.1))));
            }
            ui.ctx().request_repaint();
        } else {
            state.spring_flash = None;
        }
    }
}

/// 计算放入剪贴板的源项：右键项在多选内则取整组选中，否则只取该项。
/// 返回 (绝对路径, 是否目录) 列表。
fn clip_targets(state: &FilePanelState, entries: &[FileEntry], cwd: &str, idx: usize) -> Vec<(String, bool)> {
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
fn tool_btn(ui: &mut egui::Ui, icon: &str, tip: &str) -> bool {
    tool_btn_color(ui, icon, tip, Palette::TEXT)
}

fn tool_btn_color(ui: &mut egui::Ui, icon: &str, tip: &str, color: egui::Color32) -> bool {
    tool_btn_resp(ui, icon, tip, color).clicked()
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
fn stem_char_len(name: &str) -> usize {
    match name.rsplit_once('.') {
        // 隐藏文件如 ".bashrc"（点在开头）不算后缀
        Some((stem, _)) if !stem.is_empty() => stem.chars().count(),
        _ => name.chars().count(),
    }
}

/// 根据扩展名选择文件类型图标。
fn file_icon(e: &FileEntry) -> &'static str {
    use egui_phosphor::regular as i;
    if e.is_dir {
        return i::FOLDER;
    }
    if e.is_link {
        return i::LINK;
    }
    let ext = e.name.rsplit_once('.').map(|(_, x)| x).unwrap_or("").to_lowercase();
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

/// 渲染所有对话框，并把确认结果转成动作。
fn dialogs(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    let ctx = ui.ctx().clone();
    let mut close = false;
    let mut clear_sel = false; // 批量删除确认后清空选中，避免残留的陈旧行索引
    let mut undo_confirmed = false; // 撤销移动确认框点了「撤销」
    let cwd = state.cwd.clone();

    if let Some(dialog) = &mut state.dialog {
        match dialog {
            Dialog::NewDir { name } => {
                modal(&ctx, crate::i18n::tr("新建目录", "New folder"), |ui| {
                    ui.text_edit_singleline(name);
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) && !name.trim().is_empty() {
                            actions.push(FileAction::Mkdir(join_path(&cwd, name.trim())));
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
                    ui.text_edit_singleline(name);
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) && !name.trim().is_empty() {
                            actions.push(FileAction::CreateFile(join_path(&cwd, name.trim())));
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
                    ui.text_edit_singleline(local);
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
                    ui.text_edit_singleline(name);
                    ui.add_space(8.0);
                    button_row(ui, 72.0, 2, |ui| {
                        if dlg_btn(ui, crate::i18n::tr("确定", "OK"), 72.0, 0) && !name.trim().is_empty() {
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
                        ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("文件较大（{}），仍要打开吗？", fmt_bytes(*size as f64)), crate::i18n::Lang::En => format!("Large file ({}). Open anyway?", fmt_bytes(*size as f64)) });
                        ui.label(RichText::new(crate::i18n::tr("将以只读方式打开，可在编辑器内切换为可编辑", "Opens read-only; switch to editable inside the editor")).color(Palette::TEXT_DIM).size(11.0));
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
    if close {
        state.dialog = None;
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

/// rwx 九宫格复选框，直接修改 mode。
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

/// 取路径最后一段（文件/目录名）。
fn basename(p: &str) -> &str {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or(p)
}

fn parent_of(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return "/".into();
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// 规范化目录路径：去掉末尾多余 "/"；空或全为 "/" 视为根。
fn normalize_path(p: &str) -> String {
    let t = p.trim();
    if t == "/" || t.is_empty() {
        return "/".into();
    }
    let trimmed = t.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// 把权限位转为 `drwxr-xr-x` 形式。
fn perm_string(perm: u32, is_dir: bool, is_link: bool) -> String {
    let t = if is_link { 'l' } else if is_dir { 'd' } else { '-' };
    let bit = |shift: u32, c: char| if perm & (1 << shift) != 0 { c } else { '-' };
    format!(
        "{t}{}{}{}{}{}{}{}{}{}",
        bit(8, 'r'), bit(7, 'w'), bit(6, 'x'),
        bit(5, 'r'), bit(4, 'w'), bit(3, 'x'),
        bit(2, 'r'), bit(1, 'w'), bit(0, 'x'),
    )
}

/// 简单的 unix 时间格式化：按本地时区偏移换算后展示（SFTP 的 mtime 为 UTC 纪元秒）。
fn fmt_mtime(secs: u64) -> String {
    if secs == 0 {
        return "-".into();
    }
    // 加上本地 UTC 偏移（东区为正）得到本地墙钟秒；偏移取负仍越界则视为无效
    let local = secs as i64 + local_offset_seconds();
    if local < 0 {
        return "-".into();
    }
    let local = local as u64;
    let days = local / 86400;
    let rem = local % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// 本地时区相对 UTC 的偏移（秒，东区为正）。首次查询后缓存，避免逐行重复系统调用。
/// DST 仅按「当前」状态取一次，跨夏令时边界的历史文件可能差 1 小时，于文件列表足够。
fn local_offset_seconds() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(query_local_offset_seconds)
}

#[cfg(unix)]
fn query_local_offset_seconds() -> i64 {
    // libc::localtime_r 填充的 tm_gmtoff 即「本地相对 UTC 的秒数偏移」
    unsafe {
        let t: libc::time_t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

#[cfg(windows)]
fn query_local_offset_seconds() -> i64 {
    // 直接声明 Win32 FFI，免引入额外 crate。TIME_ZONE_INFORMATION 布局固定且稳定。
    #[repr(C)]
    struct WinSystemTime {
        w_year: u16, w_month: u16, w_day_of_week: u16, w_day: u16,
        w_hour: u16, w_minute: u16, w_second: u16, w_milliseconds: u16,
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: WinSystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: WinSystemTime,
        daylight_bias: i32,
    }
    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    unsafe {
        let mut tzi: TimeZoneInformation = std::mem::zeroed();
        let r = GetTimeZoneInformation(&mut tzi);
        // UTC = local + bias(分钟)；夏令时生效时再叠加 daylight_bias，否则用 standard_bias
        let extra = if r == TIME_ZONE_ID_DAYLIGHT { tzi.daylight_bias } else { tzi.standard_bias };
        -((tzi.bias + extra) as i64) * 60
    }
}

#[cfg(not(any(unix, windows)))]
fn query_local_offset_seconds() -> i64 {
    0
}

/// 自 1970-01-01 起的天数 -> (年,月,日)。算法源自 Howard Hinnant。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::normalize_path;

    #[test]
    fn normalize_trailing_slash() {
        assert_eq!(normalize_path("/home/e5-1/"), "/home/e5-1");
        assert_eq!(normalize_path("/home/e5-1"), "/home/e5-1");
        assert_eq!(normalize_path("/home/e5-1///"), "/home/e5-1");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("///"), "/");
        assert_eq!(normalize_path("  /tmp/  "), "/tmp");
        assert_eq!(normalize_path(""), "/");
    }
}
