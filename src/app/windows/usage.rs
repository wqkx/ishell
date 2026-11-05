//! 目录占用分析小窗。一次只看当前目录的直接子项，点进去再统计下一层。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use egui::RichText;

use crate::i18n::tr;
use crate::proto::UiCommand;
use crate::theme::Palette;
use crate::usage::{LevelReport, MountPoint, SearchReport, UsageBody, UsageKind, UsageRow};

use super::super::App;

enum Mode {
    Level,
    Search,
}

pub(in crate::app) struct UsageWin {
    session_uid: u64,
    path: String,
    home: String,
    path_ime: Option<(usize, usize)>,
    needle: String,
    needle_ime: Option<(usize, usize)>,
    mode: Mode,
    level: Option<LevelReport>,
    search: Option<SearchReport>,
    mounts: Vec<MountPoint>,
    error: String,
    level_id: Option<u64>,
    search_id: Option<u64>,
    mount_id: Option<u64>,
    next_id: u64,
    level_cancel: Option<Arc<AtomicBool>>,
}

impl UsageWin {
    fn new(session_uid: u64, path: String, home: String) -> Self {
        Self {
            session_uid,
            path,
            home,
            path_ime: None,
            needle: String::new(),
            needle_ime: None,
            mode: Mode::Level,
            level: None,
            search: None,
            mounts: Vec::new(),
            error: String::new(),
            level_id: None,
            search_id: None,
            mount_id: None,
            next_id: 1,
            level_cancel: None,
        }
    }

    fn stop(&self) {
        if let Some(c) = &self.level_cancel {
            c.store(true, Ordering::Relaxed);
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
        if let Some(c) = self.level_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
    }

    fn busy(&self) -> bool {
        self.level_id.is_some() || self.search_id.is_some()
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
        self.usage_go(UsageKind::Level { path });
        self.usage_go(UsageKind::Mounts);
    }

    pub(in crate::app) fn usage_window(&mut self, ctx: &egui::Context) {
        self.drain_usage(ctx);
        if self.usage.is_none() {
            return;
        }
        let mut close = false;
        let mut go: Option<UsageKind> = None;
        let mut clear_query = false;
        egui::Window::new("usage_win")
            .title_bar(false)
            .default_size([520.0, 480.0])
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
                    paint_usage(ui, w, &mut close, &mut go, &mut clear_query);
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
        } else if let Some(kind) = go {
            self.usage_go(kind);
        }
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
        let editing = ctx.memory(|m| m.focused()) == Some(egui::Id::new("usage_path"));
        let Some(w) = self.usage.as_mut() else {
            return;
        };
        for (id, result) in got {
            match result {
                Ok(UsageBody::Level(rep)) if w.level_id == Some(id) => {
                    if !editing {
                        w.path.clone_from(&rep.path);
                    }
                    w.level = Some(rep);
                    w.level_id = None;
                    w.mode = Mode::Level;
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
                Err(e) if w.level_id == Some(id) || w.search_id == Some(id) => {
                    w.error = e;
                    if w.level_id == Some(id) {
                        w.level_id = None;
                    }
                    if w.search_id == Some(id) {
                        w.search_id = None;
                    }
                }
                _ => {}
            }
        }
    }

    fn usage_go(&mut self, kind: UsageKind) {
        let kind = match kind {
            UsageKind::Level { path } => UsageKind::Level {
                path: path.trim().to_string(),
            },
            UsageKind::Search { path, needle } => UsageKind::Search {
                path: path.trim().to_string(),
                needle,
            },
            UsageKind::Mounts => UsageKind::Mounts,
        };
        if let UsageKind::Level { path } | UsageKind::Search { path, .. } = &kind {
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
                if let Some(prev) = w.level_cancel.replace(cancel.clone()) {
                    prev.store(true, Ordering::Relaxed);
                }
                w.error.clear();
                match &kind {
                    UsageKind::Level { path } => {
                        w.path.clone_from(path);
                        w.mode = Mode::Level;
                        w.level = None;
                        w.level_id = Some(id);
                        w.search_id = None;
                    }
                    UsageKind::Search { .. } => {
                        w.mode = Mode::Search;
                        w.search = None;
                        w.search_id = Some(id);
                        w.level_id = None;
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
            }
        }
    }
}

fn paint_usage(
    ui: &mut egui::Ui,
    w: &mut UsageWin,
    close: &mut bool,
    go: &mut Option<UsageKind>,
    clear_query: &mut bool,
) {
    title_row(ui, close);
    ui.add_space(8.0);
    location_bar(ui, w, go);
    if !w.mounts.is_empty() {
        ui.add_space(6.0);
        disk_row(ui, w, go);
    }
    ui.add_space(10.0);
    summary_row(ui, w);
    if !w.error.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new(&w.error).size(12.0).color(Palette::DANGER));
    }
    ui.add_space(6.0);
    let avail = ui.available_height();
    let list_h = if avail.is_finite() {
        (avail - 48.0).max(96.0)
    } else {
        260.0
    };
    let full_w = ui.available_width();
    ui.allocate_ui(egui::vec2(full_w, list_h), |ui| {
        if let Some(msg) = list_message(w) {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(msg).size(12.0).color(Palette::TEXT_DIM));
            });
        } else {
            match w.mode {
                Mode::Level => {
                    if let Some(level) = w.level.as_ref() {
                        paint_rows(ui, &level.rows, level.total, false, go);
                    }
                }
                Mode::Search => {
                    if let Some(search) = w.search.as_ref() {
                        let biggest = search.rows.iter().map(|r| r.size).max().unwrap_or(0);
                        paint_rows(ui, &search.rows, biggest, true, go);
                    }
                }
            }
        }
    });
    ui.add_space(8.0);
    search_bar(ui, w, go, clear_query);
}

fn title_row(ui: &mut egui::Ui, close: &mut bool) {
    use egui_phosphor::regular as icon;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tr("占用分析", "Disk usage"))
                .strong()
                .size(13.0)
                .color(Palette::TEXT),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM))
                        .frame(false),
                )
                .clicked()
            {
                *close = true;
            }
        });
    });
}

fn location_bar(ui: &mut egui::Ui, w: &mut UsageWin, go: &mut Option<UsageKind>) {
    use egui_phosphor::regular as icon;
    let mut entered = false;
    inset_bar(ui, 32.0, |ui| {
        let parent = crate::usage::parent_path(&w.path);
        if icon_button(ui, icon::ARROW_UP, tr("上级目录", "Up"), parent.is_some()) {
            if let Some(path) = parent {
                *go = Some(UsageKind::Level { path });
            }
        }
        let home_w = if w.home.is_empty() { 0.0 } else { 28.0 };
        let field_w = (ui.available_width() - home_w).max(40.0);
        let (_, enter) = ui
            .allocate_ui_with_layout(
                egui::vec2(field_w, 24.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    crate::ui::file_panel::ime_line(
                        ui,
                        "usage_path",
                        &mut w.path,
                        &mut w.path_ime,
                        false,
                        None,
                    )
                },
            )
            .inner;
        entered = enter;
        if !w.home.is_empty() && icon_button(ui, icon::HOUSE, tr("主目录", "Home"), true) {
            *go = Some(UsageKind::Level {
                path: w.home.clone(),
            });
        }
    });
    if entered {
        *go = Some(UsageKind::Level {
            path: w.path.clone(),
        });
    }
}

fn disk_row(ui: &mut egui::Ui, w: &UsageWin, go: &mut Option<UsageKind>) {
    let current = current_mount(&w.path, &w.mounts);
    egui::ScrollArea::horizontal()
        .id_salt("usage_mounts")
        .max_height(28.0)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.horizontal(|ui| {
                for (i, m) in w.mounts.iter().enumerate() {
                    if disk_chip(ui, m, current == Some(i)) {
                        *go = Some(UsageKind::Level {
                            path: m.path.clone(),
                        });
                    }
                }
            });
        });
}

fn disk_chip(ui: &mut egui::Ui, m: &MountPoint, selected: bool) -> bool {
    let label = short_label(&mount_label(&m.path));
    let color = if selected {
        Palette::ACCENT
    } else {
        Palette::TEXT_DIM
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label, egui::FontId::proportional(12.0), color);
    let width = galley.size().x + 16.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::click());
    let bg = if selected {
        Palette::ACCENT_SOFT
    } else if resp.hovered() {
        Palette::CARD
    } else {
        egui::Color32::TRANSPARENT
    };
    if bg != egui::Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, 11.0, bg);
    }
    let pos = egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley, color);
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
}

fn summary_row(ui: &mut egui::Ui, w: &UsageWin) {
    let (headline, hint) = headline(w);
    let (note, note_tip, note_color) = summary_note(w);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(headline)
                .strong()
                .size(18.0)
                .color(Palette::TEXT),
        )
        .on_hover_text(hint);
        if !note.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(note).size(11.0).color(note_color))
                    .on_hover_text(note_tip);
            });
        }
    });
}

fn headline(w: &UsageWin) -> (String, String) {
    let limits = tr(
        "只统计当前这一层，点目录再往下。不跟随符号链接，也不进入别的磁盘。",
        "One level at a time. Click a folder to go deeper. Symlinks and other disks are not followed.",
    );
    if w.busy() {
        return (tr("正在统计…", "Measuring…").into(), limits.into());
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
            let text = w
                .level
                .as_ref()
                .map(|l| crate::ui::fmt_bytes(l.total as f64))
                .unwrap_or_else(|| "—".into());
            (text, limits.into())
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
        if let Some(level) = &w.level {
            if level.partial {
                bits.push(tr("未算完", "Incomplete").to_string());
                tips.push(
                    tr(
                        "目录太大，统计提前停下，数字不完整",
                        "Stopped early — this folder is too large, so the total is incomplete",
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
    if w.busy() {
        return Some(tr("正在统计…", "Measuring…").into());
    }
    if !w.error.is_empty() {
        return None;
    }
    match w.mode {
        Mode::Level => w
            .level
            .as_ref()
            .filter(|l| l.rows.is_empty())
            .map(|_| tr("这个目录是空的", "This folder is empty").to_string()),
        Mode::Search => w
            .search
            .as_ref()
            .filter(|s| s.rows.is_empty())
            .map(|_| tr("没有匹配的文件", "No matching files").to_string()),
    }
}

fn paint_rows(
    ui: &mut egui::Ui,
    rows: &[UsageRow],
    total: u64,
    search: bool,
    go: &mut Option<UsageKind>,
) {
    ui.spacing_mut().item_spacing.y = 2.0;
    let salt = if search {
        "usage_search"
    } else {
        "usage_level"
    };
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .auto_shrink([false, false])
        .show_rows(ui, 28.0, rows.len(), |ui, range| {
            for i in range {
                let row = &rows[i];
                if !row_hit(ui, row, total, search) {
                    continue;
                }
                let path = if search {
                    crate::usage::parent_path(&row.path)
                } else if row.is_dir {
                    Some(row.path.clone())
                } else {
                    None
                };
                if let Some(path) = path {
                    *go = Some(UsageKind::Level { path });
                }
            }
        });
}

fn row_hit(ui: &mut egui::Ui, row: &UsageRow, total: u64, search: bool) -> bool {
    use egui_phosphor::regular as icon;
    let clickable = search || row.is_dir;
    let sense = if clickable {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), sense);
    if resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, Palette::PANEL_2);
    }
    let frac = if total == 0 {
        0.0
    } else {
        (row.size as f64 / total as f64) as f32
    };
    if row.size > 0 && frac > 0.0 {
        let mut fill = rect.shrink2(egui::vec2(0.0, 4.0));
        fill.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(4.0));
        ui.painter().rect_filled(
            fill,
            5.0,
            egui::Color32::from_rgba_unmultiplied(0xd9, 0x70, 0x49, 72),
        );
    }
    let painter = ui.painter();
    let icon_color = if row.is_dir {
        Palette::ACCENT
    } else {
        Palette::TEXT_DIM
    };
    let mark = if row.is_dir { icon::FOLDER } else { icon::FILE };
    painter.text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        egui::Align2::LEFT_CENTER,
        mark,
        egui::FontId::proportional(14.0),
        icon_color,
    );
    let name_rect = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(30.0, 0.0),
        egui::pos2(rect.right() - 128.0, rect.bottom()),
    );
    if name_rect.width() > 8.0 {
        painter.with_clip_rect(name_rect).text(
            name_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            leaf(&row.path),
            egui::FontId::proportional(12.0),
            Palette::TEXT,
        );
    }
    painter.text(
        rect.right_center() - egui::vec2(54.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        crate::ui::fmt_bytes(row.size as f64),
        egui::FontId::monospace(12.0),
        Palette::TEXT,
    );
    painter.text(
        rect.right_center() - egui::vec2(8.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        pct(row.size, total),
        egui::FontId::monospace(11.0),
        Palette::TEXT_DIM,
    );
    let tip = if search {
        match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("{}\n打开所在目录", row.path),
            crate::i18n::Lang::En => format!("{}\nOpen the folder", row.path),
        }
    } else {
        row.path.clone()
    };
    resp.on_hover_text(tip).clicked() && clickable
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
                .size(14.0)
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

fn mount_label(path: &str) -> String {
    let t = path.trim_end_matches(['/', '\\']);
    if t.is_empty() {
        return "/".into();
    }
    if t.len() == 2 && t.as_bytes().get(1) == Some(&b':') {
        return t.to_string();
    }
    t.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(t)
        .to_string()
}

fn short_label(s: &str) -> String {
    let mut n = 0;
    for (i, _) in s.char_indices() {
        if n == 16 {
            return format!("{}…", &s[..i]);
        }
        n += 1;
    }
    s.to_string()
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
