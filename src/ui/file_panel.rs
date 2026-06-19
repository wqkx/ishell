//! 右下文件操作区（仿 FinalShell）：左侧树形目录 + 右侧文件列表。
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
}

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
    ConfirmDelete { path: String, is_dir: bool, name: String },
    ConfirmOpenLarge { path: String, size: u64 },
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
    Delete { path: String, is_dir: bool },
    Rename { from: String, to: String },
    CopyPath(String),
    /// 双击文本文件 -> 打开编辑器（force=true 放宽大小限制）
    OpenFile { path: String, force: bool },
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

pub fn show(ui: &mut egui::Ui, state: &mut FilePanelState) -> Vec<FileAction> {
    let mut actions = Vec::new();

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
    file_list(ui, state, &mut actions);

    // 对话框
    dialogs(ui, state, &mut actions);

    actions
}

/// 绘制目录树（从 root 递归）。
fn tree(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    if state.root.is_empty() {
        return;
    }
    let mut toggles: Vec<String> = Vec::new();
    let mut select: Option<String> = None;

    // 根节点
    let root = state.root.clone();
    draw_node(ui, state, &root, &root, 0, &mut toggles, &mut select);

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

fn draw_node(
    ui: &mut egui::Ui,
    state: &FilePanelState,
    path: &str,
    label: &str,
    depth: usize,
    toggles: &mut Vec<String>,
    select: &mut Option<String>,
) {
    let expanded = state.expanded.contains(path);
    let is_cwd = state.cwd == path;
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 12.0);
        // 用 phosphor 图标，避免 ▸/▾ 在字体缺字形时显示成方块
        let tri = if expanded { egui_phosphor::regular::CARET_DOWN } else { egui_phosphor::regular::CARET_RIGHT };
        let folder = if expanded { egui_phosphor::regular::FOLDER_OPEN } else { egui_phosphor::regular::FOLDER };
        let color = if is_cwd { Palette::ACCENT } else { Palette::TEXT };
        // 整个节点一个可点击响应：单击展开/折叠，双击在右侧列表打开
        let resp = ui.add(
            egui::Label::new(RichText::new(format!("{tri} {folder} {label}")).color(color)).sense(Sense::click()),
        );
        if resp.clicked() {
            toggles.push(path.to_string());
        }
        if resp.double_clicked() {
            *select = Some(path.to_string());
        }
    });

    if expanded {
        if let Some(entries) = state.listings.get(path) {
            for e in entries.iter().filter(|e| e.is_dir) {
                let child = join_path(path, &e.name);
                draw_node(ui, state, &child, &e.name, depth + 1, toggles, select);
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
fn file_list(ui: &mut egui::Ui, state: &mut FilePanelState, actions: &mut Vec<FileAction>) {
    // FinalShell 风格工具栏：扁平图标条（带浅色背景）
    use egui_phosphor::regular as icon;
    let mut bc_nav: Option<String> = None;
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
                if tool_btn(ui, icon::ARROW_UP, crate::i18n::tr("上级目录", "Up")) && !state.cwd.is_empty() {
                    state.cwd = parent_of(&state.cwd);
                    state.selected.clear();
                }
                if tool_btn(ui, icon::FOLDER_PLUS, crate::i18n::tr("新建目录", "New folder")) {
                    state.dialog = Some(Dialog::NewDir { name: String::new() });
                }
                if tool_btn(ui, icon::FILE_PLUS, crate::i18n::tr("新建文件", "New file")) {
                    state.dialog = Some(Dialog::NewFile { name: String::new() });
                }
                if tool_btn(ui, icon::UPLOAD_SIMPLE, crate::i18n::tr("上传文件", "Upload")) {
                    state.dialog = Some(Dialog::Upload { local: String::new() });
                }
                if tool_btn(ui, icon::COPY, crate::i18n::tr("复制当前路径", "Copy path")) && !state.cwd.is_empty() {
                    actions.push(FileAction::CopyPath(state.cwd.clone()));
                }

                ui.add_space(4.0);
                ui.separator();
                if state.path_edit.is_some() {
                    // 路径编辑模式：回车跳转、Esc 或点击别处退出
                    let mut go: Option<String> = None;
                    let mut done = false;
                    let take_focus = state.path_edit_focus;
                    if let Some(buf) = &mut state.path_edit {
                        let resp = ui.add(
                            egui::TextEdit::singleline(buf)
                                .desired_width(ui.available_width() - 4.0)
                                .hint_text(crate::i18n::tr("输入路径后回车跳转，Esc 取消", "Enter path, Enter to go, Esc to cancel")),
                        );
                        if take_focus {
                            resp.request_focus();
                        }
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
                    // 面包屑（可点击逐级跳转；双击空白处进入编辑模式）
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let root = ui.add(egui::Label::new(RichText::new(icon::HOUSE).color(Palette::ACCENT)).sense(Sense::click()));
                    if root.clicked() {
                        bc_nav = Some("/".into());
                    }
                    let mut acc = String::new();
                    for seg in state.cwd.split('/').filter(|s| !s.is_empty()) {
                        ui.label(RichText::new("›").color(Palette::TEXT_DIM));
                        acc.push('/');
                        acc.push_str(seg);
                        let here = acc.clone();
                        let is_last = here == state.cwd;
                        let color = if is_last { Palette::TEXT } else { Palette::TEXT_DIM };
                        let r = ui.add(egui::Label::new(RichText::new(seg).color(color)).sense(Sense::click()));
                        if r.clicked() {
                            bc_nav = Some(here);
                        }
                    }
                    // 剩余空白区域：双击进入路径编辑模式
                    let rest = ui.available_size_before_wrap();
                    if rest.x > 8.0 {
                        let (rr, resp) = ui.allocate_exact_size(rest, Sense::click());
                        let _ = rr;
                        if resp.double_clicked() {
                            state.path_edit = Some(state.cwd.clone());
                            state.path_edit_focus = true;
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
    let mut entries = match state.listings.get(&cwd) {
        Some(e) => e.clone(),
        None => return,
    };
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

    let mut navigate: Option<String> = None;
    let mut sort_click: Option<SortKey> = None;
    let mut menu: Vec<Menu> = Vec::new();
    let mut clicks: Vec<usize> = Vec::new(); // 本帧被点击的行
    let mut rclick: Option<usize> = None; // 本帧被右键的行
    let mut open_file: Option<String> = None; // 双击文本文件
    let mut confirm_open: Option<(String, u64)> = None; // 大文件待确认
    let mut rename_commit: Option<(String, String)> = None;
    let mut cancel_rename = false;
    let now = ui.input(|i| i.time);
    let (mod_ctrl, mod_shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

    // 空白区域右键 -> 新建文件/目录（仅覆盖列表区域，避免遮挡上方路径栏）
    let bg = ui.interact(ui.available_rect_before_wrap(), ui.id().with("filelist_bg"), Sense::click());
    let mut bg_new_dir = false;
    let mut bg_new_file = false;
    bg.context_menu(|ui| {
        if ui.button(format!("{}  {}", icon::FOLDER_PLUS, crate::i18n::tr("新建文件夹", "New folder"))).clicked() {
            bg_new_dir = true;
            ui.close();
        }
        if ui.button(format!("{}  {}", icon::FILE_PLUS, crate::i18n::tr("新建文件", "New file"))).clicked() {
            bg_new_file = true;
            ui.close();
        }
    });

    egui::Frame::new()
        .inner_margin(egui::Margin { left: 6, right: 2, top: 0, bottom: 0 })
        .show(ui, |ui| {
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::auto().at_least(190.0).clip(true))
        .column(Column::auto().at_least(80.0))
        .column(Column::auto().at_least(140.0))
        .column(Column::auto().at_least(96.0))
        .column(Column::remainder())
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
                    entry_context(&r, e, i, &full, &mut menu);
                });
            }
        });
    });

    // 表头点击排序：同列切升/降，换列则置升序
    if let Some(k) = sort_click {
        if state.sort_key == k {
            state.sort_desc = !state.sort_desc;
        } else {
            state.sort_key = k;
            state.sort_desc = false;
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
            Menu::Delete { path, is_dir, name } => {
                state.dialog = Some(Dialog::ConfirmDelete { path, is_dir, name });
            }
            Menu::NewDir => state.dialog = Some(Dialog::NewDir { name: String::new() }),
            Menu::NewFile => state.dialog = Some(Dialog::NewFile { name: String::new() }),
        }
    }

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
    Delete { path: String, is_dir: bool, name: String },
    NewDir,
    NewFile,
}

/// 条目右键菜单：把用户选择记录到 `menu`。
fn entry_context(resp: &egui::Response, e: &FileEntry, idx: usize, full: &str, menu: &mut Vec<Menu>) {
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
            menu.push(Menu::Delete { path: full.to_string(), is_dir: e.is_dir, name: e.name.clone() });
            ui.close();
        }
    });
}

/// 工具栏图标按钮（FinalShell 风格：扁平无边框，悬停高亮）。
fn tool_btn(ui: &mut egui::Ui, icon: &str, tip: &str) -> bool {
    let mut clicked = false;
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        clicked = ui
            .add(
                egui::Button::new(RichText::new(icon).size(16.0).color(Palette::TEXT))
                    .min_size(egui::vec2(30.0, 26.0))
                    .corner_radius(5.0),
            )
            .on_hover_text(tip)
            .clicked();
    });
    clicked
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
    let cwd = state.cwd.clone();

    if let Some(dialog) = &mut state.dialog {
        match dialog {
            Dialog::NewDir { name } => {
                modal(&ctx, crate::i18n::tr("新建目录", "New folder"), |ui| {
                    ui.text_edit_singleline(name);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(crate::i18n::tr("确定", "OK")).clicked() && !name.trim().is_empty() {
                            actions.push(FileAction::Mkdir(join_path(&cwd, name.trim())));
                            close = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::NewFile { name } => {
                modal(&ctx, crate::i18n::tr("新建文件", "New file"), |ui| {
                    ui.text_edit_singleline(name);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(crate::i18n::tr("确定", "OK")).clicked() && !name.trim().is_empty() {
                            actions.push(FileAction::CreateFile(join_path(&cwd, name.trim())));
                            close = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Upload { local } => {
                modal(&ctx, crate::i18n::tr("上传文件", "Upload"), |ui| {
                    ui.label(RichText::new(crate::i18n::tr("本地文件路径（也可直接把文件拖入文件区）", "Local file path (or drag files into the panel)")).size(12.0).color(Palette::TEXT_DIM));
                    ui.text_edit_singleline(local);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(crate::i18n::tr("上传", "Upload")).clicked() && !local.trim().is_empty() {
                            actions.push(FileAction::Upload { local: local.trim().to_string(), remote_dir: cwd.clone() });
                            close = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Chmod { path, mode, name } => {
                modal(&ctx, crate::i18n::tr("修改权限", "Chmod"), |ui| {
                    ui.label(RichText::new(name.as_str()).strong());
                    ui.add_space(4.0);
                    chmod_grid(ui, mode);
                    ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("八进制：{:03o}", *mode & 0o777), crate::i18n::Lang::En => format!("Octal: {:03o}", *mode & 0o777) }).monospace().color(Palette::TEXT_DIM));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(crate::i18n::tr("应用", "Apply")).clicked() {
                            actions.push(FileAction::Chmod { path: path.clone(), mode: *mode & 0o777 });
                            close = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Rename { path, name } => {
                modal(&ctx, crate::i18n::tr("重命名", "Rename"), |ui| {
                    ui.text_edit_singleline(name);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(crate::i18n::tr("确定", "OK")).clicked() && !name.trim().is_empty() {
                            let parent = parent_of(path);
                            actions.push(FileAction::Rename { from: path.clone(), to: join_path(&parent, name.trim()) });
                            close = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::ConfirmDelete { path, is_dir, name } => {
                modal(&ctx, crate::i18n::tr("确认删除", "Confirm delete"), |ui| {
                    ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("确定删除 {} 吗？此操作不可恢复。", name), crate::i18n::Lang::En => format!("Delete {}? This cannot be undone.", name) });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(crate::i18n::tr("删除", "Delete")).color(Palette::DANGER)).clicked() {
                            actions.push(FileAction::Delete { path: path.clone(), is_dir: *is_dir });
                            close = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
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
        }
    }
    if close {
        state.dialog = None;
    }
}

fn modal(ctx: &egui::Context, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(280.0);
            add(ui);
        });
}

/// rwx 九宫格复选框，直接修改 mode。
fn chmod_grid(ui: &mut egui::Ui, mode: &mut u32) {
    egui::Grid::new("chmod_grid").num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
        ui.label("");
        ui.label(RichText::new(crate::i18n::tr("读", "R")).size(12.0));
        ui.label(RichText::new(crate::i18n::tr("写", "W")).size(12.0));
        ui.label(RichText::new(crate::i18n::tr("执行", "X")).size(12.0));
        ui.end_row();
        for (row, (label, base)) in [(crate::i18n::tr("所有者","Owner"), 6u32), (crate::i18n::tr("用户组","Group"), 3), (crate::i18n::tr("其他","Other"), 0)].iter().enumerate() {
            let _ = row;
            ui.label(RichText::new(*label).size(12.0));
            for bit in [2u32, 1, 0] {
                let shift = base + bit;
                let mut on = *mode & (1 << shift) != 0;
                if ui.checkbox(&mut on, "").changed() {
                    if on {
                        *mode |= 1 << shift;
                    } else {
                        *mode &= !(1 << shift);
                    }
                }
            }
            ui.end_row();
        }
    });
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

/// 简单的 unix 时间格式化（UTC，无外部依赖）。
fn fmt_mtime(secs: u64) -> String {
    if secs == 0 {
        return "-".into();
    }
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
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
