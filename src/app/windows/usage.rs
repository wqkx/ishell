//! 目录占用分析小窗。打开先看这一层的名字，点「分析」才算大小。
//! 算过的结果按路径留着，进子目录不会重算，除非再点分析。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::RichText;

use crate::i18n::tr;
use crate::proto::UiCommand;
use crate::theme::{Palette, FS_BODY, FS_NOTE, FS_STRONG, FS_TITLE};
use crate::usage::{
    LevelReport, MountPoint, NameReport, SearchReport, UsageBody, UsageKind, UsageRow,
};

/// `known_size` 每帧最多入库多少条。一次分析最多带回 [`crate::usage::WALK_CAP`] 条
/// 深层大小，一帧全插会让界面卡住几百毫秒；分批摊开，数字在约一秒内陆续到位。
const KNOWN_DRAIN_PER_FRAME: usize = 30_000;
/// 记下的大小总量上限。与一次遍历的上限对齐：装得下一整棵大树，又不至于无限增长。
const KNOWN_CAP: usize = crate::usage::WALK_CAP as usize;

use super::super::App;

enum Mode {
    Level,
    Search,
}

enum Page {
    /// 主目录和各个磁盘。
    Disks,
    /// 当前目录里的文件和文件夹。
    Dir,
}

pub(in crate::app) struct UsageWin {
    session_uid: u64,
    path: String,
    home: String,
    page: Page,
    /// 与文件区路径栏同一套状态：面包屑、双击编辑、单击延时、幽灵子路径。
    path_edit: Option<String>,
    path_edit_focus: bool,
    path_edit_rsel: Option<(usize, usize)>,
    path_edit_ime: Option<(usize, usize)>,
    pending_nav: Option<(String, f64)>,
    path_trail: Option<String>,
    needle: String,
    needle_ime: Option<(usize, usize)>,
    mode: Mode,
    /// 当前路径已经分析过的结果。没分析过是 None。
    level: Option<LevelReport>,
    /// 还没分析时的名字列表。
    names: Option<NameReport>,
    /// 分析过的目录，按路径留着。再进去直接用，不重新计算。
    analyzed: HashMap<String, LevelReport>,
    /// 父目录分析时记下的子目录大小。键是规范化路径。
    known_size: HashMap<String, u64>,
    /// 分析带回的深层大小，按「批」排队等分帧入库（一批 = 一次扫描的 nested +
    /// 它要不要覆盖已有值）。路径规范化和插入都摊到消化时做，结果回来那一帧零拷贝。
    pending_known: VecDeque<(Vec<(String, u64)>, bool)>,
    search: Option<SearchReport>,
    mounts: Vec<MountPoint>,
    error: String,
    level_id: Option<u64>,
    names_id: Option<u64>,
    search_id: Option<u64>,
    mount_id: Option<u64>,
    next_id: u64,
    /// 父目录还在统计时进了子目录，结果回来也要记下来。
    level_keep: HashSet<u64>,
    level_cancel: Option<Arc<AtomicBool>>,
    list_cancel: Option<Arc<AtomicBool>>,
    selected: HashSet<usize>,
    select_anchor: Option<usize>,
    /// 待确认删除的 (路径, 显示名)。
    confirm_delete: Option<Vec<(String, String)>>,
}

impl UsageWin {
    fn new(session_uid: u64, path: String, home: String) -> Self {
        Self {
            session_uid,
            path,
            home,
            page: Page::Dir,
            path_edit: None,
            path_edit_focus: false,
            path_edit_rsel: None,
            path_edit_ime: None,
            pending_nav: None,
            path_trail: None,
            needle: String::new(),
            needle_ime: None,
            mode: Mode::Level,
            level: None,
            names: None,
            analyzed: HashMap::new(),
            known_size: HashMap::new(),
            pending_known: VecDeque::new(),
            search: None,
            mounts: Vec::new(),
            error: String::new(),
            level_id: None,
            names_id: None,
            search_id: None,
            mount_id: None,
            next_id: 1,
            level_keep: HashSet::new(),
            level_cancel: None,
            list_cancel: None,
            selected: HashSet::new(),
            select_anchor: None,
            confirm_delete: None,
        }
    }

    fn stop(&self) {
        for c in [&self.level_cancel, &self.list_cancel] {
            if let Some(c) = c {
                c.store(true, Ordering::Relaxed);
            }
        }
    }

    /// 退出搜索，回到已经统计过的这一层。不重新扫描。
    fn leave_search(&mut self) {
        self.mode = Mode::Level;
        self.search_id = None;
        self.search = None;
        self.needle.clear();
        self.needle_ime = None;
        self.error.clear();
        if let Some(c) = self.list_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
    }

    fn busy(&self) -> bool {
        self.level_id.is_some() || self.search_id.is_some() || self.names_id.is_some()
    }

    /// 把排队的大小入库一批。返回 true 表示还有剩余，调用方要接着要重绘。
    fn drain_known_batch(&mut self) -> bool {
        let mut budget = KNOWN_DRAIN_PER_FRAME;
        while budget > 0 {
            if self.known_size.len() >= KNOWN_CAP {
                // 总量到顶：剩下的不再记。一棵树装得下；再分析别的大树，
                // 深层大小记不住的层点「分析」仍可按需统计。
                self.pending_known.clear();
                return false;
            }
            let Some((batch, overwrite)) = self.pending_known.front_mut() else {
                return false;
            };
            let Some((path, size)) = batch.pop() else {
                self.pending_known.pop_front();
                continue;
            };
            let path = norm_path(&path);
            if *overwrite {
                self.known_size.insert(path, size);
            } else {
                self.known_size.entry(path).or_insert(size);
            }
            budget -= 1;
        }
        !self.pending_known.is_empty()
    }

    #[cfg(test)]
    fn flush_known(&mut self) {
        while self.drain_known_batch() {}
    }
}

impl App {
    pub(in crate::app) fn open_usage(&mut self, idx: usize) {
        let Some(s) = self.sessions.get(idx) else {
            return;
        };
        let path = s.files.cwd.clone();
        let home = s.files.home.clone();
        let uid = s.uid;
        if path.is_empty() {
            let msg = tr("还没有当前目录", "No current directory").to_string();
            self.toast = Some((msg, self.ctx.input(|i| i.time)));
            return;
        }
        if let Some(w) = self.usage.take() {
            w.stop();
        }
        self.usage = Some(UsageWin::new(uid, path.clone(), home));
        self.usage_go(UsageKind::Mounts);
        self.usage_go(UsageKind::Names { path });
    }

    pub(in crate::app) fn usage_window(&mut self, ctx: &egui::Context) {
        self.drain_usage(ctx);
        if let Some(w) = self.usage.as_mut() {
            // 分析带回的深层大小分帧入库；还有剩余就要重绘接着倒。
            if w.drain_known_batch() {
                ctx.request_repaint();
            }
        }
        if self.usage.is_none() {
            return;
        }
        let mut close = false;
        let mut go: Option<UsageKind> = None;
        let mut clear_query = false;
        let mut delete: Option<Vec<String>> = None;
        egui::Window::new("usage_win")
            .title_bar(false)
            .default_size([540.0, 560.0])
            .resizable(true)
            .collapsible(false)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(12),
            )
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                if let Some(w) = self.usage.as_mut() {
                    paint_usage(ui, w, &mut close, &mut go, &mut clear_query, &mut delete);
                }
            });
        if close {
            if let Some(w) = self.usage.take() {
                w.stop();
            }
        } else if clear_query {
            if let Some(w) = self.usage.as_mut() {
                if matches!(w.mode, Mode::Search) {
                    w.leave_search();
                } else {
                    w.needle.clear();
                    w.needle_ime = None;
                }
            }
        } else if let Some(paths) = delete {
            self.usage_delete(paths);
        } else if let Some(kind) = go {
            self.usage_go(kind);
        }
    }

    fn usage_delete(&mut self, paths: Vec<String>) {
        let Some(w) = self.usage.as_mut() else {
            return;
        };
        let uid = w.session_uid;
        drop_deleted(w, &paths);
        let Some(s) = self.sessions.iter().find(|s| s.uid == uid) else {
            return;
        };
        let _ = s.cmd_tx.send(UiCommand::DeleteMany { paths });
    }

    fn drain_usage(&mut self, ctx: &egui::Context) {
        let Some(uid) = self.usage.as_ref().map(|w| w.session_uid) else {
            for s in &mut self.sessions {
                s.pending.usage.clear();
            }
            return;
        };
        if !self.sessions.iter().any(|s| s.uid == uid) {
            if let Some(w) = self.usage.take() {
                w.stop();
            }
            return;
        }
        let mut got = Vec::new();
        for s in &mut self.sessions {
            if s.uid == uid {
                got.append(&mut s.pending.usage);
            } else {
                s.pending.usage.clear();
            }
        }
        if got.is_empty() {
            return;
        }
        let editing = ctx.memory(|m| m.focused()) == Some(egui::Id::new("usage_path_edit"));
        let Some(w) = self.usage.as_mut() else {
            return;
        };
        for (id, result) in got {
            match result {
                Ok(UsageBody::Level(rep)) if w.level_keep.remove(&id) => {
                    let show = w.level_id == Some(id);
                    let rep = remember_level(w, rep);
                    if show && !editing && norm_path(&rep.path) == w.path {
                        w.level = Some(rep);
                        w.names = None;
                        w.mode = Mode::Level;
                        w.selected.clear();
                        w.select_anchor = None;
                    }
                    if show {
                        w.level_id = None;
                    }
                    w.error.clear();
                }
                Ok(UsageBody::Names(rep)) if w.names_id == Some(id) => {
                    let path = norm_path(&rep.path);
                    if path == w.path && !w.analyzed.contains_key(&path) {
                        w.names = Some(rep);
                        w.level = None;
                    }
                    w.names_id = None;
                    w.error.clear();
                }
                Ok(UsageBody::Search(rep)) if w.search_id == Some(id) => {
                    w.search = Some(rep);
                    w.search_id = None;
                    w.mode = Mode::Search;
                    w.error.clear();
                }
                Ok(UsageBody::Mounts(list)) if w.mount_id == Some(id) => {
                    w.mounts = list;
                    w.mount_id = None;
                }
                Err(e) if e.is_empty() => {}
                Err(e)
                    if w.level_id == Some(id)
                        || w.search_id == Some(id)
                        || w.names_id == Some(id) =>
                {
                    w.error = e;
                    if w.level_id == Some(id) {
                        w.level_id = None;
                    }
                    if w.search_id == Some(id) {
                        w.search_id = None;
                    }
                    if w.names_id == Some(id) {
                        w.names_id = None;
                    }
                }
                _ => {}
            }
        }
    }

    fn usage_go(&mut self, kind: UsageKind) {
        let home = self
            .usage
            .as_ref()
            .map(|w| w.home.clone())
            .unwrap_or_default();
        let tidy = |path: &str| {
            crate::ui::file_panel::normalize_path(&crate::ui::file_panel::expand_tilde(
                &home,
                path.trim(),
            ))
        };
        let kind = match kind {
            UsageKind::Level { path } => UsageKind::Level { path: tidy(&path) },
            UsageKind::Names { path } => UsageKind::Names { path: tidy(&path) },
            UsageKind::Search { path, needle } => UsageKind::Search {
                path: tidy(&path),
                needle,
            },
            UsageKind::Mounts => UsageKind::Mounts,
        };
        if let UsageKind::Level { path }
        | UsageKind::Names { path }
        | UsageKind::Search { path, .. } = &kind
        {
            if let Err(e) = crate::usage::path_ok(path) {
                if let Some(w) = self.usage.as_mut() {
                    w.error = e;
                }
                return;
            }
        }
        let kind = if let UsageKind::Search { path, needle } = &kind {
            match crate::usage::needle_ok(needle) {
                Ok(needle) => UsageKind::Search {
                    path: path.clone(),
                    needle,
                },
                Err(e) => {
                    if let Some(w) = self.usage.as_mut() {
                        w.error = e;
                    }
                    return;
                }
            }
        } else {
            kind
        };
        let heavy = !matches!(kind, UsageKind::Mounts);
        let (uid, id, cancel) = {
            let Some(w) = self.usage.as_mut() else {
                return;
            };
            let id = w.next_id;
            w.next_id = w.next_id.wrapping_add(1);
            let cancel = Arc::new(AtomicBool::new(false));
            if heavy {
                let measuring = matches!(kind, UsageKind::Level { .. });
                if measuring {
                    if let Some(prev) = w.level_cancel.replace(cancel.clone()) {
                        prev.store(true, Ordering::Relaxed);
                    }
                    w.level_keep.insert(id);
                } else if let Some(prev) = w.list_cancel.replace(cancel.clone()) {
                    prev.store(true, Ordering::Relaxed);
                }
                w.error.clear();
                match &kind {
                    UsageKind::Level { path } => {
                        set_usage_path(w, path);
                        w.page = Page::Dir;
                        w.mode = Mode::Level;
                        w.level_id = Some(id);
                        w.search_id = None;
                        w.names_id = None;
                    }
                    UsageKind::Names { path } => {
                        set_usage_path(w, path);
                        w.page = Page::Dir;
                        w.mode = Mode::Level;
                        w.names = None;
                        w.level = None;
                        w.names_id = Some(id);
                        w.level_id = None;
                        w.search_id = None;
                        w.selected.clear();
                        w.select_anchor = None;
                    }
                    UsageKind::Search { .. } => {
                        w.mode = Mode::Search;
                        w.search = None;
                        w.search_id = Some(id);
                        w.level_id = None;
                        w.names_id = None;
                    }
                    UsageKind::Mounts => {}
                }
            } else {
                w.mount_id = Some(id);
            }
            (w.session_uid, id, cancel)
        };
        let sent = self.sessions.iter().find(|s| s.uid == uid).map(|s| {
            s.cmd_tx
                .send(UiCommand::UsageQuery { id, kind, cancel })
                .is_ok()
        });
        if sent != Some(true) {
            if let Some(w) = self.usage.as_mut() {
                w.error = tr("会话已断开", "Session disconnected").into();
                w.level_id = None;
                w.search_id = None;
                w.names_id = None;
            }
        }
    }
}

fn norm_path(path: &str) -> String {
    crate::ui::file_panel::normalize_path(path)
}

/// 把这一层的分析结果留下。子目录的大小记在 `known_size` 里，进去时不再重算。
/// 深层大小可能上百万条，推进队列由界面分帧入库，不在这一帧卡住。
fn remember_level(w: &mut UsageWin, mut rep: LevelReport) -> LevelReport {
    let path = norm_path(&rep.path);
    rep.path = path.clone();
    // 半截结果不能盖掉已经算完的那一份。
    if rep.partial && w.analyzed.get(&path).is_some_and(|old| !old.partial) {
        return rep;
    }
    for row in &mut rep.rows {
        row.path = norm_path(&row.path);
        if row.is_dir {
            if let Some(disk) = disk_used(w, &row.path) {
                if disk > row.size {
                    row.size = disk;
                }
            }
        }
        if w.known_size.len() < KNOWN_CAP {
            w.known_size.insert(row.path.clone(), row.size);
        }
    }
    if w.known_size.len() < KNOWN_CAP {
        w.known_size.insert(path.clone(), rep.total);
    }
    let overwrite = !rep.partial;
    let nested = std::mem::take(&mut rep.nested);
    if !nested.is_empty() {
        w.pending_known.push_back((nested, overwrite));
    }
    w.analyzed.insert(path, rep.clone());
    rep
}

fn disk_of<'a>(w: &'a UsageWin, path: &str) -> Option<&'a MountPoint> {
    let path = norm_path(path);
    w.mounts.iter().find(|m| norm_path(&m.path) == path)
}

fn disk_used(w: &UsageWin, path: &str) -> Option<u64> {
    disk_of(w, path).map(|m| m.used)
}

fn drop_deleted(w: &mut UsageWin, paths: &[String]) {
    let set: HashSet<&str> = paths.iter().map(String::as_str).collect();
    if let Some(level) = w.level.as_mut() {
        let mut gone = 0u64;
        level.rows.retain(|row| {
            if set.contains(row.path.as_str()) {
                gone = gone.saturating_add(row.size);
                false
            } else {
                true
            }
        });
        level.total = level.total.saturating_sub(gone);
        if let Some(cached) = w.analyzed.get_mut(&level.path) {
            *cached = level.clone();
        }
    }
    if let Some(names) = w.names.as_mut() {
        names.rows.retain(|row| !set.contains(row.path.as_str()));
    }
    if let Some(search) = w.search.as_mut() {
        search.rows.retain(|row| !set.contains(row.path.as_str()));
    }
    // 每张表只过一遍：条目落在任何一个被删路径之下就摘掉。
    // 逐路径 retain 是 O（删除数 × 表大小），多选删除大树时会卡住界面。
    w.analyzed.retain(|k, _| !under_deleted(k, &set));
    w.known_size.retain(|k, _| !under_deleted(k, &set));
    for (batch, _) in w.pending_known.iter_mut() {
        batch.retain(|(p, _)| !under_deleted(p, &set));
    }
    w.pending_known.retain(|(batch, _)| !batch.is_empty());
    w.selected.clear();
    w.select_anchor = None;
}

/// `path` 是否等于某个被删路径、或在它之下。沿父链逐级查集合，深度有限。
fn under_deleted(path: &str, set: &HashSet<&str>) -> bool {
    let mut p = path.trim_end_matches(['/', '\\']);
    loop {
        if set.contains(p) {
            return true;
        }
        let Some(i) = p.rfind(['/', '\\']) else {
            return false;
        };
        if i == 0 {
            return false;
        }
        p = &p[..i];
    }
}

/// 进入一个目录。已经分析过就直接用缓存，否则只列名字。
fn enter_dir(w: &mut UsageWin, path: &str) -> Option<UsageKind> {
    set_usage_path(w, path);
    w.page = Page::Dir;
    w.mode = Mode::Level;
    w.error.clear();
    w.selected.clear();
    w.select_anchor = None;
    w.search = None;
    w.search_id = None;
    // 父目录可能还在统计。不要停掉它，结果回来后子目录大小才留得住。
    // 只是这一页不再等那一次：按钮不要停在「分析中」。
    w.level_id = None;
    w.names_id = None;
    if let Some(rep) = w.analyzed.get(&w.path) {
        w.level = Some(rep.clone());
        w.names = None;
        w.names_id = None;
        w.level_id = None;
        None
    } else {
        Some(UsageKind::Names {
            path: w.path.clone(),
        })
    }
}

fn paint_usage(
    ui: &mut egui::Ui,
    w: &mut UsageWin,
    close: &mut bool,
    go: &mut Option<UsageKind>,
    clear_query: &mut bool,
    delete: &mut Option<Vec<String>>,
) {
    title_row(ui, w, close);
    match w.page {
        Page::Disks => {
            ui.add_space(8.0);
            disk_page(ui, w, go);
        }
        Page::Dir => {
            ui.add_space(10.0);
            path_bar(ui, w, go);
            ui.add_space(10.0);
            summary_row(ui, w, go);
            if !w.error.is_empty() {
                ui.add_space(4.0);
                ui.label(RichText::new(&w.error).size(FS_BODY).color(Palette::DANGER));
            }
            ui.add_space(4.0);
            let avail = ui.available_height();
            let list_h = if avail.is_finite() {
                (avail - 48.0).max(96.0)
            } else {
                260.0
            };
            let (rows, analyzed, total, search) = shown_rows(w);
            let full_w = ui.available_width();
            ui.allocate_ui(egui::vec2(full_w, list_h), |ui| {
                if rows.is_empty() {
                    if let Some(msg) = list_message(w) {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new(msg).size(FS_BODY).color(Palette::TEXT_DIM));
                        });
                    }
                } else {
                    paint_rows(ui, w, &rows, total, analyzed, search, go);
                }
            });
            ui.add_space(8.0);
            search_bar(ui, w, go, clear_query);
        }
    }
    if let Some(paths) = delete_modal(ui, w) {
        *delete = Some(paths);
    }
}

fn title_row(ui: &mut egui::Ui, w: &mut UsageWin, close: &mut bool) {
    use egui_phosphor::regular as icon;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            RichText::new(tr("占用分析", "Disk usage"))
                .strong()
                .size(FS_TITLE)
                .color(Palette::TEXT),
        );
        let on_disks = matches!(w.page, Page::Disks);
        let label = format!("{} {}", icon::HARD_DRIVES, tr("选择磁盘", "Choose disk"));
        let mut button = egui::Button::new(RichText::new(label).size(FS_BODY).color(Palette::TEXT));
        button = if on_disks {
            button.fill(Palette::ACCENT_SOFT)
        } else {
            button.fill(Palette::PANEL_2)
        };
        if ui.add(button).clicked() {
            w.page = if on_disks { Page::Dir } else { Page::Disks };
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(icon::X)
                            .size(FS_STRONG)
                            .color(Palette::TEXT_DIM),
                    )
                    .frame(false),
                )
                .clicked()
            {
                *close = true;
            }
        });
    });
}

fn disk_page(ui: &mut egui::Ui, w: &mut UsageWin, go: &mut Option<UsageKind>) {
    use egui_phosphor::regular as icon;
    if !w.home.is_empty() {
        section_head(ui, icon::HOUSE, tr("主目录", "Home"), None);
        if home_row(ui, &w.home) {
            let home = w.home.clone();
            if let Some(kind) = enter_dir(w, &home) {
                *go = Some(kind);
            }
        }
        ui.add_space(8.0);
    }
    disk_section(ui, w, go);
}

fn home_row(ui: &mut egui::Ui, home: &str) -> bool {
    use egui_phosphor::regular as icon;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Palette::CARD);
    p.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, Palette::BORDER),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.left_center() + egui::vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        icon::HOUSE,
        egui::FontId::proportional(FS_STRONG),
        Palette::ACCENT,
    );
    p.text(
        rect.left_center() + egui::vec2(28.0, 0.0),
        egui::Align2::LEFT_CENTER,
        home,
        egui::FontId::proportional(FS_BODY),
        Palette::TEXT,
    );
    resp.on_hover_text(tr("打开主目录", "Open home")).clicked()
}

fn set_usage_path(w: &mut UsageWin, path: &str) {
    let path =
        crate::ui::file_panel::normalize_path(&crate::ui::file_panel::expand_tilde(&w.home, path));
    if path == w.path {
        return;
    }
    let prev = w.path.clone();
    crate::ui::file_panel::update_trail(&path, &prev, &mut w.path_trail);
    w.path = path;
    w.path_edit = None;
    w.path_edit_focus = false;
    w.pending_nav = None;
}

/// 文件区同款路径栏：分段按钮，单击进入，双击编辑，右键复制/粘贴。
fn path_bar(ui: &mut egui::Ui, w: &mut UsageWin, go: &mut Option<UsageKind>) {
    egui::Frame::new()
        .fill(Palette::PANEL_2)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            // 面包屑内部是「不收缩」的横向滚动区。文件区工具栏自己只有一行高，
            // 所以那里没问题；这里如果把剩余高度都交给它，路径栏会铺满整个窗口，
            // 磁盘、列表和搜索都被挤到窗口外面。
            let row_h = ui.spacing().interact_size.y.max(22.0);
            let width = ui.available_width();
            ui.allocate_ui_with_layout(
                egui::vec2(width, row_h),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if let Some(p) = crate::ui::file_panel::path_crumbs(
                        ui,
                        &mut crate::ui::file_panel::Crumbs {
                            cwd: &w.path,
                            edit: &mut w.path_edit,
                            focus: &mut w.path_edit_focus,
                            rsel: &mut w.path_edit_rsel,
                            ime: &mut w.path_edit_ime,
                            pending: &mut w.pending_nav,
                            trail: &mut w.path_trail,
                            id_salt: "usage_path_edit",
                        },
                    ) {
                        if let Some(kind) = enter_dir(w, &p) {
                            *go = Some(kind);
                        }
                    }
                },
            );
        });
}

/// 磁盘在路径之上：先选卷，再看这个卷里的目录。子挂载点缩进在父挂载点下面。
/// 右侧数字和侧栏磁盘行一样，是「可用 / 总量」。
fn disk_section(ui: &mut egui::Ui, w: &mut UsageWin, go: &mut Option<UsageKind>) {
    use egui_phosphor::regular as icon;
    let avails: Vec<String> = w
        .mounts
        .iter()
        .map(|m| crate::ui::fmt_bytes(m.avail as f64))
        .collect();
    let totals: Vec<String> = w
        .mounts
        .iter()
        .map(|m| crate::ui::fmt_bytes(m.size as f64))
        .collect();
    let aw = avails.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let tw = totals.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    section_head(
        ui,
        icon::HARD_DRIVES,
        tr("磁盘", "Disks"),
        Some(tr("可用 / 总量", "Free / total")),
    );
    if w.mounts.is_empty() {
        let msg = if w.mount_id.is_some() {
            tr("正在读取磁盘…", "Reading disks…")
        } else {
            tr("没有识别到磁盘", "No disks found")
        };
        ui.label(RichText::new(msg).size(FS_BODY).color(Palette::TEXT_DIM));
        return;
    }
    let current = current_mount(&w.path, &w.mounts);
    let mounts = w.mounts.clone();
    let max_h = if matches!(w.page, Page::Disks) {
        ui.available_height().max(120.0)
    } else {
        156.0
    };
    let scroll = egui::ScrollArea::vertical()
        .id_salt("usage_disks")
        .max_height(max_h)
        .auto_shrink([false, true]);
    scroll.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        for (i, m) in mounts.iter().enumerate() {
            let indent = nest_level(&mounts, i) as f32 * 14.0;
            let detail = format!("{:>aw$} / {:>tw$}", avails[i], totals[i]);
            if volume_row(ui, m, &detail, current == Some(i), indent) {
                if let Some(kind) = enter_dir(w, &m.path) {
                    *go = Some(kind);
                }
            }
        }
    });
}

fn section_head(ui: &mut egui::Ui, icon: &str, title: &str, aside: Option<&str>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ui.label(RichText::new(icon).color(Palette::ACCENT).size(FS_STRONG));
        ui.label(
            RichText::new(title)
                .strong()
                .size(FS_STRONG)
                .color(Palette::TEXT),
        );
        if let Some(aside) = aside {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(aside).size(FS_NOTE).color(Palette::TEXT_DIM));
            });
        }
    });
    hairline(ui);
}

fn hairline(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, Palette::BORDER),
    );
    ui.add_space(4.0);
}

fn volume_row(
    ui: &mut egui::Ui,
    m: &MountPoint,
    detail: &str,
    selected: bool,
    indent: f32,
) -> bool {
    ui.horizontal(|ui| {
        if indent > 0.0 {
            ui.add_space(indent.min(42.0));
        }
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 40.0), egui::Sense::click());
        let percent = if m.size == 0 {
            0.0
        } else {
            (m.used as f64 / m.size as f64).clamp(0.0, 1.0) as f32
        };
        let p = ui.painter();
        p.rect_filled(rect, 4.0, Palette::CARD);
        p.rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(
                1.0,
                if selected {
                    Palette::OK
                } else {
                    Palette::BORDER
                },
            ),
            egui::StrokeKind::Inside,
        );
        use egui_phosphor::regular as icon;
        let text_y = rect.top() + 14.0;
        p.text(
            egui::pos2(rect.left() + 8.0, text_y),
            egui::Align2::LEFT_CENTER,
            icon::HARD_DRIVES,
            egui::FontId::proportional(FS_STRONG),
            Palette::OK,
        );
        let detail_rect = p.text(
            egui::pos2(rect.right() - 8.0, text_y),
            egui::Align2::RIGHT_CENTER,
            detail,
            egui::FontId::monospace(FS_NOTE),
            Palette::TEXT_DIM,
        );
        let title = disk_title(m);
        let name_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 28.0, rect.top()),
            egui::pos2(detail_rect.left() - 8.0, rect.top() + 26.0),
        );
        if name_rect.width() > 8.0 {
            p.with_clip_rect(name_rect).text(
                egui::pos2(name_rect.left(), text_y),
                egui::Align2::LEFT_CENTER,
                title,
                egui::FontId::proportional(FS_BODY),
                Palette::TEXT,
            );
        }
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.bottom() - 8.0),
            egui::pos2(rect.right() - 8.0, rect.bottom() - 4.0),
        );
        green_bar(p, bar, percent);
        let tip = match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!(
                "{}  {}\n已用 {} / 共 {}，可用 {}",
                m.label,
                m.path,
                crate::ui::fmt_bytes(m.used as f64),
                crate::ui::fmt_bytes(m.size as f64),
                crate::ui::fmt_bytes(m.avail as f64)
            ),
            crate::i18n::Lang::En => format!(
                "{}  {}\n{} used / {} total, {} free",
                m.label,
                m.path,
                crate::ui::fmt_bytes(m.used as f64),
                crate::ui::fmt_bytes(m.size as f64),
                crate::ui::fmt_bytes(m.avail as f64)
            ),
        };
        resp.on_hover_text(tip).clicked()
    })
    .inner
}

fn disk_title(m: &MountPoint) -> String {
    let dev = m
        .source
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(m.source.as_str());
    if dev.is_empty() || dev == m.path {
        format!("{}  {}", m.path, m.label)
    } else {
        format!("{dev}   {}  {}", m.path, m.label)
    }
}

fn nest_level(mounts: &[MountPoint], idx: usize) -> usize {
    let path = &mounts[idx].path;
    mounts
        .iter()
        .enumerate()
        .filter(|(j, m)| {
            *j != idx && m.path != *path && !is_fs_root(&m.path) && path_on_mount(path, &m.path)
        })
        .count()
}

/// `/` 和 `C:` 是整机的根，不算某一块盘的父级，否则每块盘都会缩进一层。
fn is_fs_root(path: &str) -> bool {
    let t = path.trim_end_matches(['/', '\\']);
    t.is_empty() || (t.len() == 2 && t.as_bytes().get(1) == Some(&b':'))
}

fn summary_row(ui: &mut egui::Ui, w: &UsageWin, go: &mut Option<UsageKind>) {
    use egui_phosphor::regular as icon;
    let (headline, hint) = headline(w);
    let (note, note_tip, note_color) = summary_note(w);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let busy = w.level_id.is_some();
        let again = w.analyzed.contains_key(&w.path);
        let word = if busy {
            tr("分析中…", "Analyzing…")
        } else if again {
            tr("重新分析", "Analyze again")
        } else {
            tr("分析", "Analyze")
        };
        let color = if busy {
            Palette::TEXT_DIM
        } else {
            Palette::TEXT
        };
        let label = format!("{}  {word}", icon::CHART_PIE);
        if ui
            .add_enabled(
                !busy,
                egui::Button::new(RichText::new(label).size(FS_BODY).color(color)).frame(false),
            )
            .on_hover_text(tr(
                "计算这一层各项的大小，并按大小排序",
                "Measure this folder and sort by size",
            ))
            .clicked()
        {
            *go = Some(UsageKind::Level {
                path: w.path.clone(),
            });
        }
        if !note.is_empty() {
            ui.label(RichText::new(note).size(FS_NOTE).color(note_color))
                .on_hover_text(note_tip);
        }
        if !headline.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(headline)
                        .strong()
                        .size(FS_TITLE)
                        .color(Palette::TEXT),
                )
                .on_hover_text(hint);
            });
        }
    });
    hairline(ui);
}

fn headline(w: &UsageWin) -> (String, String) {
    let limits = tr(
        "分析会记下整棵树里每个文件和目录的大小。不跟随符号链接，也不进入别的磁盘。",
        "Analyze saves every file and folder in the tree. Symlinks and other disks are not followed.",
    );
    if w.level_id.is_some() {
        return (tr("正在分析…", "Analyzing…").into(), limits.into());
    }
    match w.mode {
        Mode::Search => {
            let n = w.search.as_ref().map(|s| s.matched).unwrap_or(0);
            let text = match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("{n} 个文件"),
                crate::i18n::Lang::En if n == 1 => "1 file".into(),
                crate::i18n::Lang::En => format!("{n} files"),
            };
            let hint = match (&w.search, crate::i18n::current()) {
                (Some(s), crate::i18n::Lang::Zh) => {
                    format!(
                        "在 {} 里搜索「{}」。点一条结果，打开它所在的目录。",
                        s.path, s.needle
                    )
                }
                (Some(s), crate::i18n::Lang::En) => format!(
                    "Search “{}” in {}. Click a result to open its folder.",
                    s.needle, s.path
                ),
                (None, _) => limits.into(),
            };
            (text, hint)
        }
        Mode::Level => {
            if let Some(level) = w.level.as_ref().filter(|l| norm_path(&l.path) == w.path) {
                (crate::ui::fmt_bytes(level.total as f64), limits.into())
            } else if let Some(size) = saved_size(w, &w.path) {
                (
                    crate::ui::fmt_bytes(size as f64),
                    tr(
                        "这是分析时留下的大小，下面每一层的文件和目录都有。",
                        "Saved by the scan, including every file and folder below.",
                    )
                    .into(),
                )
            } else {
                (String::new(), limits.into())
            }
        }
    }
}

fn summary_note(w: &UsageWin) -> (String, String, egui::Color32) {
    if w.busy() {
        return (String::new(), String::new(), Palette::TEXT_DIM);
    }
    let mut bits = Vec::new();
    let mut tips = Vec::new();
    let mut color = Palette::TEXT_DIM;
    if let Mode::Level = w.mode {
        if w.level.is_none() {
            if let Some(names) = w.names.as_ref().filter(|n| n.omitted > 0) {
                bits.push(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("还有 {} 项未列出", names.omitted),
                    crate::i18n::Lang::En => format!("{} more not listed", names.omitted),
                });
            }
        }
        if let Some(level) = &w.level {
            if level.partial {
                bits.push(tr("未算完", "Incomplete").to_string());
                tips.push(
                    tr(
                        "目录太大，统计没有走完，部分数字不完整",
                        "Too large to finish — some figures are incomplete",
                    )
                    .to_string(),
                );
                color = Palette::WARN;
            }
            if level.denied > 0 {
                bits.push(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("{} 个目录无权限", level.denied),
                    crate::i18n::Lang::En => format!("{} unreadable", level.denied),
                });
            }
            if level.omitted > 0 {
                bits.push(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("还有 {} 项", level.omitted),
                    crate::i18n::Lang::En => format!("{} more", level.omitted),
                });
                tips.push(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!(
                        "其余 {} 项合计 {}，已经算进总数",
                        level.omitted,
                        crate::ui::fmt_bytes(level.omitted_bytes as f64)
                    ),
                    crate::i18n::Lang::En => format!(
                        "{} more items, {}, already included in the total",
                        level.omitted,
                        crate::ui::fmt_bytes(level.omitted_bytes as f64)
                    ),
                });
            }
        }
    } else if let Some(search) = &w.search {
        if search.truncated {
            bits.push(tr("只显示最大的", "Largest only").to_string());
        }
    }
    (bits.join(" · "), tips.join("\n"), color)
}

fn list_message(w: &UsageWin) -> Option<String> {
    if !w.error.is_empty() {
        return None;
    }
    if w.names_id.is_some() || w.level_id.is_some() {
        return Some(if w.level_id.is_some() {
            tr("正在分析…", "Analyzing…").into()
        } else {
            tr("正在读取…", "Loading…").into()
        });
    }
    match w.mode {
        Mode::Level => Some(tr("这个目录是空的", "This folder is empty").into()),
        Mode::Search => Some(tr("没有匹配的文件", "No matching files").into()),
    }
}

fn shown_rows(w: &UsageWin) -> (Vec<UsageRow>, bool, u64, bool) {
    if matches!(w.mode, Mode::Search) {
        if let Some(search) = &w.search {
            let biggest = search.rows.iter().map(|r| r.size).max().unwrap_or(0);
            return (search.rows.clone(), true, biggest, true);
        }
    }
    if let Some(level) = w.level.as_ref().filter(|l| norm_path(&l.path) == w.path) {
        return (level.rows.clone(), true, level.total, false);
    }
    if let Some(names) = w.names.as_ref().filter(|n| norm_path(&n.path) == w.path) {
        let mut rows = names.rows.clone();
        for row in &mut rows {
            row.path = norm_path(&row.path);
            // 缓存只补目录的大小（名字列表里目录没有数字）。文件大小列目录时已经从
            // metadata 读到，不能用父目录扫描留下的旧值盖掉——文件可能刚变动过。
            if row.is_dir {
                if let Some(sz) = saved_size(w, &row.path) {
                    row.size = sz;
                }
            }
        }
        // 文件本来就有大小。只有目录也被记下之后，才按大小重排，免得没分析时文件夹沉到底下。
        let have_dir_sizes = rows.iter().any(|r| r.is_dir && r.size > 0);
        if have_dir_sizes {
            rows.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        }
        let total = w
            .known_size
            .get(&w.path)
            .copied()
            .unwrap_or_else(|| rows.iter().fold(0u64, |a, r| a.saturating_add(r.size)));
        return (rows, false, total, false);
    }
    (Vec::new(), false, 0, false)
}

fn saved_size(w: &UsageWin, path: &str) -> Option<u64> {
    let path = norm_path(path);
    let saved = w.known_size.get(&path).copied();
    match (saved, disk_used(w, &path)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) if b > 0 => Some(b),
        _ => None,
    }
}

fn green_bar(p: &egui::Painter, rect: egui::Rect, frac: f32) {
    p.rect_filled(rect, 2.0, Palette::TRACK);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(2.0));
        let c = Palette::OK;
        p.rect_filled(
            fill,
            2.0,
            egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 210),
        );
    }
}

fn paint_rows(
    ui: &mut egui::Ui,
    w: &mut UsageWin,
    rows: &[UsageRow],
    total: u64,
    analyzed: bool,
    search: bool,
    go: &mut Option<UsageKind>,
) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let salt = if search {
        "usage_search"
    } else {
        "usage_level"
    };
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .auto_shrink([false, false])
        .show_rows(ui, 26.0, rows.len(), |ui, range| {
            for i in range {
                row_hit(ui, w, rows, i, total, analyzed, search, go);
            }
        });
}

fn select_row(w: &mut UsageWin, idx: usize, ui: &egui::Ui) {
    let (ctrl, shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));
    if ctrl {
        if !w.selected.insert(idx) {
            w.selected.remove(&idx);
        }
        w.select_anchor = Some(idx);
    } else if shift {
        let anchor = w.select_anchor.unwrap_or(idx);
        let (lo, hi) = (anchor.min(idx), anchor.max(idx));
        w.selected = (lo..=hi).collect();
    } else {
        w.selected.clear();
        w.selected.insert(idx);
        w.select_anchor = Some(idx);
    }
}

fn selected_items(rows: &[UsageRow], selected: &HashSet<usize>) -> Vec<(String, String)> {
    let mut idxs: Vec<usize> = selected.iter().copied().collect();
    idxs.sort_unstable();
    idxs.into_iter()
        .filter_map(|i| {
            rows.get(i)
                .map(|row| (row.path.clone(), leaf(&row.path).to_string()))
        })
        .collect()
}

fn row_hit(
    ui: &mut egui::Ui,
    w: &mut UsageWin,
    rows: &[UsageRow],
    index: usize,
    total: u64,
    analyzed: bool,
    search: bool,
    go: &mut Option<UsageKind>,
) {
    use egui_phosphor::regular as icon;
    let row = &rows[index];
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::click());
    let selected = w.selected.contains(&index);
    let painter = ui.painter();
    // 和右下文件列表一样：奇数行一块浅底，选中整行铺上强调色，行与行之间不留缝。
    if selected {
        painter.rect_filled(rect, 0.0, Palette::ACCENT_SOFT);
    } else if index % 2 == 1 {
        painter.rect_filled(rect, 0.0, Palette::PANEL_2);
    }
    if resp.hovered() && !selected {
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(10));
    }
    let show_size = analyzed || !row.is_dir || row.size > 0;
    let text_y = rect.top() + 13.0;
    // 挂载点行的大小被回填成整盘已用，父目录总数又不含它——除以总数会冒出
    // 好几百的百分比。这类行的比例和百分比都按它自己那块盘算，和磁盘页一致。
    let disk = disk_of(w, &row.path);
    let (frac, pct_text) = if let Some(m) = disk {
        let frac = if m.size == 0 {
            0.0
        } else {
            (m.used as f64 / m.size as f64) as f32
        };
        (frac, pct(m.used, m.size))
    } else if !show_size || total == 0 {
        (0.0, String::new())
    } else {
        (
            (row.size as f64 / total as f64).clamp(0.0, 1.0) as f32,
            pct(row.size, total),
        )
    };
    if show_size && frac > 0.0 {
        let bar = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 8.0, rect.bottom() - 4.0),
            egui::pos2(rect.right() - 8.0, rect.bottom() - 2.0),
        );
        green_bar(painter, bar, frac);
    }
    let (mark, icon_color) = if disk.is_some() {
        (icon::HARD_DRIVES, Palette::OK)
    } else if row.is_dir {
        (icon::FOLDER, Palette::ACCENT)
    } else {
        (icon::FILE, Palette::TEXT_DIM)
    };
    painter.text(
        egui::pos2(rect.left() + 8.0, text_y),
        egui::Align2::LEFT_CENTER,
        mark,
        egui::FontId::proportional(FS_STRONG),
        icon_color,
    );
    let size_right = if show_size {
        let pct_rect = painter.text(
            egui::pos2(rect.right() - 8.0, text_y),
            egui::Align2::RIGHT_CENTER,
            &pct_text,
            egui::FontId::monospace(FS_NOTE),
            Palette::TEXT_DIM,
        );
        painter
            .text(
                egui::pos2(pct_rect.left() - 10.0, text_y),
                egui::Align2::RIGHT_CENTER,
                crate::ui::fmt_bytes(row.size as f64),
                egui::FontId::monospace(FS_BODY),
                Palette::TEXT,
            )
            .left()
    } else {
        rect.right()
    };
    let name_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 26.0, rect.top()),
        egui::pos2(size_right - 8.0, rect.bottom()),
    );
    if name_rect.width() > 8.0 {
        let name_g = painter.with_clip_rect(name_rect).text(
            egui::pos2(name_rect.left(), text_y),
            egui::Align2::LEFT_CENTER,
            leaf(&row.path),
            egui::FontId::proportional(FS_BODY),
            Palette::TEXT,
        );
        // 搜索结果只露文件名会分不清谁是谁，后面用暗色小字跟上所在目录。
        if search {
            if let Some(parent) = crate::usage::parent_path(&row.path) {
                let px = name_g.max.x + 6.0;
                let prect =
                    egui::Rect::from_min_max(egui::pos2(px, name_rect.top()), name_rect.max);
                if prect.width() > 24.0 {
                    painter.with_clip_rect(prect).text(
                        egui::pos2(px, text_y),
                        egui::Align2::LEFT_CENTER,
                        parent,
                        egui::FontId::proportional(FS_NOTE),
                        Palette::TEXT_DIM,
                    );
                }
            }
        }
    }
    let tip = if search {
        match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("{}\n打开所在目录", row.path),
            crate::i18n::Lang::En => format!("{}\nOpen the folder", row.path),
        }
    } else {
        row.path.clone()
    };
    resp.clone().on_hover_text(tip);
    if resp.double_clicked() {
        let path = if search {
            crate::usage::parent_path(&row.path)
        } else if row.is_dir {
            Some(row.path.clone())
        } else {
            None
        };
        if let Some(path) = path {
            if let Some(kind) = enter_dir(w, &path) {
                *go = Some(kind);
            }
        }
    } else if resp.clicked() {
        select_row(w, index, ui);
    }
    if resp.secondary_clicked() && !w.selected.contains(&index) {
        w.selected.clear();
        w.selected.insert(index);
        w.select_anchor = Some(index);
    }
    let menu_resp = resp;
    menu_resp.context_menu(|ui| {
        ui.set_min_width(200.0);
        let n = w.selected.len().max(1);
        let label = if n > 1 {
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("删除 {n} 项"),
                crate::i18n::Lang::En => format!("Delete {n} items"),
            }
        } else {
            tr("删除", "Delete").to_string()
        };
        if ui
            .add_sized(
                [200.0, 32.0],
                egui::Button::new(RichText::new(label).size(FS_BODY)),
            )
            .clicked()
        {
            let items = selected_items(rows, &w.selected);
            if !items.is_empty() {
                w.confirm_delete = Some(items);
            }
            ui.close();
        }
    });
}

fn delete_modal(ui: &mut egui::Ui, w: &mut UsageWin) -> Option<Vec<String>> {
    let items = w.confirm_delete.clone()?;
    let mut decided: Option<Vec<String>> = None;
    let mut cancel = false;
    egui::Modal::new(egui::Id::new("usage_delete_confirm")).show(ui.ctx(), |ui| {
        ui.set_min_width(300.0);
        ui.label(
            RichText::new(tr("确认删除", "Confirm delete"))
                .strong()
                .size(FS_STRONG)
                .color(Palette::TEXT),
        );
        ui.add_space(6.0);
        let body = if items.len() == 1 {
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("确定删除 {} 吗？此操作不可恢复。", items[0].1),
                crate::i18n::Lang::En => {
                    format!("Delete {}? This cannot be undone.", items[0].1)
                }
            }
        } else {
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => {
                    format!("确定删除选中的 {} 项吗？此操作不可恢复。", items.len())
                }
                crate::i18n::Lang::En => {
                    format!(
                        "Delete the {} selected items? This cannot be undone.",
                        items.len()
                    )
                }
            }
        };
        ui.label(RichText::new(body).size(FS_BODY).color(Palette::TEXT));
        for (_, name) in items.iter().take(4) {
            ui.label(RichText::new(name).size(FS_NOTE).color(Palette::TEXT_DIM));
        }
        if items.len() > 4 {
            ui.label(
                RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("…还有 {} 项", items.len() - 4),
                    crate::i18n::Lang::En => format!("…and {} more", items.len() - 4),
                })
                .size(FS_NOTE)
                .color(Palette::TEXT_DIM),
            );
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(tr("删除", "Delete"))
                            .size(FS_BODY)
                            .color(Palette::PANEL),
                    )
                    .fill(Palette::DANGER),
                )
                .clicked()
            {
                decided = Some(items.iter().map(|(path, _)| path.clone()).collect());
            }
            if ui
                .button(RichText::new(tr("取消", "Cancel")).size(FS_BODY))
                .clicked()
            {
                cancel = true;
            }
        });
    });
    if decided.is_some() || cancel {
        w.confirm_delete = None;
    }
    decided
}

fn search_bar(
    ui: &mut egui::Ui,
    w: &mut UsageWin,
    go: &mut Option<UsageKind>,
    clear_query: &mut bool,
) {
    use egui_phosphor::regular as icon;
    let mut submit = false;
    let show_clear = !w.needle.is_empty() || matches!(w.mode, Mode::Search);
    inset_bar(ui, 32.0, |ui| {
        ui.label(
            RichText::new(icon::MAGNIFYING_GLASS)
                .size(FS_STRONG)
                .color(Palette::TEXT_DIM),
        );
        let clear_w = if show_clear { 24.0 } else { 0.0 };
        let field_w = (ui.available_width() - clear_w).max(40.0);
        let (_, enter) = ui
            .allocate_ui_with_layout(
                egui::vec2(field_w, 24.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    crate::ui::file_panel::ime_line(
                        ui,
                        "usage_needle",
                        &mut w.needle,
                        &mut w.needle_ime,
                        false,
                        Some(tr("搜索文件名，回车", "Search names, then Enter")),
                    )
                },
            )
            .inner;
        submit = enter;
        if show_clear && icon_button(ui, icon::X, tr("清除搜索", "Clear search"), true) {
            *clear_query = true;
        }
    });
    if submit {
        *go = Some(UsageKind::Search {
            path: w.path.clone(),
            needle: w.needle.clone(),
        });
    }
}

fn inset_bar(ui: &mut egui::Ui, height: f32, add: impl FnOnce(&mut egui::Ui)) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, 8.0, Palette::PANEL_2);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(4.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        add,
    );
}

fn icon_button(ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool) -> bool {
    let color = if enabled {
        Palette::TEXT
    } else {
        Palette::TEXT_DIM
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(glyph).size(15.0).color(color)).frame(false),
    )
    .on_hover_text(tip)
    .clicked()
}

fn pct(size: u64, total: u64) -> String {
    if total == 0 {
        return "0%".into();
    }
    let p = size as f64 / total as f64 * 100.0;
    if p >= 10.0 {
        format!("{p:.0}%")
    } else if p >= 0.1 {
        format!("{p:.1}%")
    } else if p > 0.0 {
        "<0.1%".into()
    } else {
        "0%".into()
    }
}

fn leaf(path: &str) -> &str {
    let t = path.trim_end_matches(['/', '\\']);
    t.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

fn current_mount(path: &str, mounts: &[MountPoint]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, m) in mounts.iter().enumerate() {
        if path_on_mount(path, &m.path) {
            let n = m.path.len();
            if best.map(|(_, len)| n > len).unwrap_or(true) {
                best = Some((i, n));
            }
        }
    }
    best.map(|(i, _)| i)
}

fn path_on_mount(path: &str, mount: &str) -> bool {
    if mount == "/" {
        return path.starts_with('/');
    }
    let m = mount.trim_end_matches(['/', '\\']);
    if m.is_empty() {
        return path.starts_with('/') || path.starts_with('\\');
    }
    if path == mount || path == m {
        return true;
    }
    match path.strip_prefix(m) {
        Some(rest) => rest.starts_with('/') || rest.starts_with('\\'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(path: &str) -> MountPoint {
        MountPoint {
            path: path.into(),
            label: "ext4".into(),
            source: String::new(),
            size: 100,
            used: 40,
            avail: 60,
        }
    }

    #[test]
    fn root_does_not_indent_every_disk() {
        let mounts = vec![
            mount("/"),
            mount("/boot"),
            mount("/boot/efi"),
            mount("/mnt/sda1"),
        ];
        assert_eq!(nest_level(&mounts, 0), 0);
        assert_eq!(nest_level(&mounts, 1), 0);
        assert_eq!(nest_level(&mounts, 2), 1);
        assert_eq!(nest_level(&mounts, 3), 0);
    }

    /// 路径栏里的横向滚动区会占满「剩余高度」。钉成一行之后，下面的搜索框必须还在窗口里。
    #[test]
    fn path_bar_does_not_swallow_the_window() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut w = UsageWin::new(1, "/home/wangqi/snap/code".into(), "/home/wangqi".into());
        w.mounts = vec![mount("/"), mount("/boot/efi"), mount("/mnt/sda1")];
        w.level = Some(LevelReport {
            path: w.path.clone(),
            total: 1000,
            rows: vec![UsageRow {
                name: "src".into(),
                path: "/home/wangqi/snap/code/src".into(),
                size: 400,
                is_dir: true,
            }],
            omitted: 0,
            omitted_bytes: 0,
            partial: false,
            denied: 0,
            nested: Vec::new(),
        });
        let mut cursor_before_path = 0.0f32;
        let mut cursor_after_path = 0.0f32;
        let mut content_bottom = 0.0f32;
        let screen_h = 480.0f32;
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(520.0, screen_h),
                )),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::window(&ui.ctx().global_style())
                            .fill(Palette::PANEL)
                            .inner_margin(12),
                    )
                    .show_inside(ui, |ui| {
                        let mut go = None;
                        let mut close = false;
                        let mut clear = false;
                        title_row(ui, &mut w, &mut close);
                        ui.add_space(10.0);
                        cursor_before_path = ui.cursor().top();
                        path_bar(ui, &mut w, &mut go);
                        cursor_after_path = ui.cursor().top();
                        ui.add_space(10.0);
                        summary_row(ui, &w, &mut go);
                        let avail = ui.available_height();
                        let list_h = if avail.is_finite() {
                            (avail - 48.0).max(96.0)
                        } else {
                            260.0
                        };
                        ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), list_h),
                            egui::Sense::hover(),
                        );
                        ui.add_space(8.0);
                        search_bar(ui, &mut w, &mut go, &mut clear);
                        content_bottom = ui.cursor().top();
                    });
            });
        }
        let path_h = cursor_after_path - cursor_before_path;
        assert!(path_h < 48.0, "路径栏又把窗口撑满了，这一行高 {path_h}px");
        assert!(
            content_bottom <= screen_h + 4.0,
            "列表和搜索被挤出窗口：内容底边 {content_bottom}px，窗口 {screen_h}px"
        );
    }

    #[test]
    fn parent_scan_is_reused_until_analyze() {
        let mut w = UsageWin::new(1, "/data".into(), "/home/wangqi".into());
        remember_level(
            &mut w,
            LevelReport {
                path: "/data".into(),
                total: 100,
                rows: vec![UsageRow {
                    name: "src".into(),
                    path: "/data/src".into(),
                    size: 40,
                    is_dir: true,
                }],
                omitted: 0,
                omitted_bytes: 0,
                partial: false,
                denied: 0,
                nested: Vec::new(),
            },
        );
        w.flush_known();
        assert_eq!(w.known_size.get("/data/src"), Some(&40));
        let child = enter_dir(&mut w, "/data/src");
        assert!(matches!(child, Some(UsageKind::Names { .. })));
        assert!(w.level.is_none());
        assert_eq!(w.known_size.get("/data/src"), Some(&40));
        assert!(enter_dir(&mut w, "/data").is_none());
        assert_eq!(w.level.as_ref().map(|l| l.total), Some(100));
        assert_eq!(saved_size(&w, "/data/src"), Some(40));
    }

    #[test]
    fn parent_scan_keeps_the_next_level() {
        let mut w = UsageWin::new(1, "/data".into(), "/home/wangqi".into());
        remember_level(
            &mut w,
            LevelReport {
                path: "/data".into(),
                total: 100,
                rows: vec![UsageRow {
                    name: "src".into(),
                    path: "/data/src".into(),
                    size: 40,
                    is_dir: true,
                }],
                omitted: 0,
                omitted_bytes: 0,
                partial: false,
                denied: 0,
                nested: vec![
                    ("/data/src/lib".into(), 25),
                    ("/data/src/main.rs".into(), 15),
                ],
            },
        );
        w.flush_known();
        assert_eq!(saved_size(&w, "/data/src/lib"), Some(25));
        assert_eq!(saved_size(&w, "/data/src/main.rs"), Some(15));
        assert!(enter_dir(&mut w, "/data/src").is_some());
        assert_eq!(saved_size(&w, "/data/src"), Some(40));
        assert_eq!(saved_size(&w, "/data/src/lib"), Some(25));
        assert_eq!(saved_size(&w, "/data/src/main.rs"), Some(15));
    }

    #[test]
    fn partial_scan_does_not_wipe_a_finished_one() {
        let mut w = UsageWin::new(1, "/data".into(), "/home/wangqi".into());
        remember_level(
            &mut w,
            LevelReport {
                path: "/data".into(),
                total: 100,
                rows: vec![UsageRow {
                    name: "src".into(),
                    path: "/data/src".into(),
                    size: 40,
                    is_dir: true,
                }],
                omitted: 0,
                omitted_bytes: 0,
                partial: false,
                denied: 0,
                nested: Vec::new(),
            },
        );
        remember_level(
            &mut w,
            LevelReport {
                path: "/data".into(),
                total: 1,
                rows: vec![UsageRow {
                    name: "src".into(),
                    path: "/data/src".into(),
                    size: 1,
                    is_dir: true,
                }],
                omitted: 0,
                omitted_bytes: 0,
                partial: true,
                denied: 0,
                nested: vec![("/data/src/lib".into(), 1)],
            },
        );
        w.flush_known();
        assert_eq!(w.known_size.get("/data/src"), Some(&40));
        assert_eq!(w.analyzed.get("/data").map(|r| r.total), Some(100));
    }

    /// partial 扫描的深层大小不覆盖已有值（or_insert），完整扫描的覆盖。
    #[test]
    fn queued_nested_respects_overwrite_flag() {
        let mut w = UsageWin::new(1, "/data".into(), "/home/wangqi".into());
        let report = |path: &str, nested: Vec<(String, u64)>, partial: bool| LevelReport {
            path: path.into(),
            total: 100,
            rows: Vec::new(),
            omitted: 0,
            omitted_bytes: 0,
            partial,
            denied: 0,
            nested,
        };
        remember_level(
            &mut w,
            report("/data/other", vec![("/data/shared".into(), 10)], false),
        );
        w.flush_known();
        remember_level(
            &mut w,
            report("/data", vec![("/data/shared".into(), 99)], true),
        );
        w.flush_known();
        assert_eq!(w.known_size.get("/data/shared"), Some(&10));
        remember_level(
            &mut w,
            report("/data", vec![("/data/shared".into(), 42)], false),
        );
        w.flush_known();
        assert_eq!(w.known_size.get("/data/shared"), Some(&42));
    }

    /// 删除一棵子树：缓存大小、分析结果和排队待入库的条目一起清掉，且只过一遍表。
    #[test]
    fn drop_deleted_clears_caches_and_queue() {
        let mut w = UsageWin::new(1, "/data".into(), "/home/wangqi".into());
        remember_level(
            &mut w,
            LevelReport {
                path: "/data".into(),
                total: 100,
                rows: vec![UsageRow {
                    name: "src".into(),
                    path: "/data/src".into(),
                    size: 40,
                    is_dir: true,
                }],
                omitted: 0,
                omitted_bytes: 0,
                partial: false,
                denied: 0,
                nested: vec![("/data/src/lib".into(), 25), ("/data/other/x".into(), 7)],
            },
        );
        w.flush_known();
        assert!(w.known_size.contains_key("/data/src/lib"));
        drop_deleted(&mut w, &["/data/src".to_string()]);
        assert!(!w.known_size.contains_key("/data/src"));
        assert!(!w.known_size.contains_key("/data/src/lib"));
        assert!(w.known_size.contains_key("/data/other/x"));
        assert!(w.analyzed.contains_key("/data"));
        // 队列里还没入库的也摘干净。
        remember_level(
            &mut w,
            LevelReport {
                path: "/data".into(),
                total: 90,
                rows: Vec::new(),
                omitted: 0,
                omitted_bytes: 0,
                partial: false,
                denied: 0,
                nested: vec![("/data/src/new".into(), 3), ("/data/keep".into(), 1)],
            },
        );
        drop_deleted(&mut w, &["/data/src".to_string()]);
        w.flush_known();
        assert!(!w.known_size.contains_key("/data/src/new"));
        assert!(w.known_size.contains_key("/data/keep"));
        assert!(under_deleted(
            "/data/src/lib",
            &["/data/src"].into_iter().collect()
        ));
        assert!(!under_deleted(
            "/data/other",
            &["/data/src"].into_iter().collect()
        ));
    }
}
