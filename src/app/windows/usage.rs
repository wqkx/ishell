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
        egui::Window::new("usage_win")
            .title_bar(false)
            .default_size([640.0, 520.0])
            .resizable(true)
            .collapsible(false)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(10),
            )
            .show(ctx, |ui| {
                ui.set_min_width(460.0);
                if let Some(w) = self.usage.as_mut() {
                    paint_usage(ui, w, &mut close, &mut go);
                }
            });
        if close {
            if let Some(w) = self.usage.take() {
                w.stop();
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

fn paint_usage(ui: &mut egui::Ui, w: &mut UsageWin, close: &mut bool, go: &mut Option<UsageKind>) {
    use egui_phosphor::regular as icon;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{}  {}",
                icon::CHART_PIE,
                tr("占用分析", "Disk usage")
            ))
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
    ui.add_space(4.0);
    let mut analyze = false;
    ui.horizontal(|ui| {
        let btn = tr("分析", "Analyze").to_string();
        let width = (ui.available_width() - 76.0).max(80.0);
        let (_, enter) = ui
            .allocate_ui_with_layout(
                egui::vec2(width, 22.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    crate::ui::file_panel::ime_singleline(
                        ui,
                        "usage_path",
                        &mut w.path,
                        &mut w.path_ime,
                    )
                },
            )
            .inner;
        if ui.button(btn).clicked() || enter {
            analyze = true;
        }
    });
    if analyze {
        *go = Some(UsageKind::Level {
            path: w.path.clone(),
        });
    }
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        if !w.home.is_empty() && ui.small_button(tr("主目录", "Home")).clicked() {
            *go = Some(UsageKind::Level {
                path: w.home.clone(),
            });
        }
        if let Some(parent) = crate::usage::parent_path(&w.path) {
            if ui.small_button(tr("上级", "Up")).clicked() {
                *go = Some(UsageKind::Level { path: parent });
            }
        }
    });
    if !w.mounts.is_empty() {
        egui::ScrollArea::horizontal()
            .id_salt("usage_mounts")
            .max_height(26.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for m in &w.mounts {
                        let label = mount_label(&m.path);
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
                        if ui.small_button(label).on_hover_text(tip).clicked() {
                            *go = Some(UsageKind::Level {
                                path: m.path.clone(),
                            });
                        }
                    }
                });
            });
    }
    ui.add_space(4.0);
    let mut search = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 150.0).max(80.0);
        let (_, enter) = ui
            .allocate_ui_with_layout(
                egui::vec2(width, 22.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    crate::ui::file_panel::ime_singleline(
                        ui,
                        "usage_needle",
                        &mut w.needle,
                        &mut w.needle_ime,
                    )
                },
            )
            .inner;
        if ui.button(tr("搜索", "Search")).clicked() || enter {
            search = true;
        }
        if matches!(w.mode, Mode::Search) && ui.button(tr("返回目录", "Back")).clicked() {
            *go = Some(UsageKind::Level {
                path: w.path.clone(),
            });
        }
    });
    if search {
        *go = Some(UsageKind::Search {
            path: w.path.clone(),
            needle: w.needle.clone(),
        });
    }
    if w.busy() {
        ui.label(
            RichText::new(tr("正在统计…", "Measuring…"))
                .size(12.0)
                .color(Palette::TEXT_DIM),
        );
    }
    if !w.error.is_empty() {
        ui.label(RichText::new(&w.error).size(12.0).color(Palette::DANGER));
    }
    ui.add_space(2.0);
    match w.mode {
        Mode::Level => paint_level(ui, w.level.as_ref(), go),
        Mode::Search => paint_search(ui, w.search.as_ref(), go),
    }
    ui.label(
        RichText::new(tr(
            "只统计这一层，点目录再往下。不跟随符号链接，也不进入别的磁盘。",
            "One level at a time. Click a folder to go deeper. Symlinks and other disks are not followed.",
        ))
        .size(11.0)
        .color(Palette::TEXT_DIM),
    );
}

fn paint_level(ui: &mut egui::Ui, level: Option<&LevelReport>, go: &mut Option<UsageKind>) {
    let Some(level) = level else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{} {}",
                tr("合计", "Total"),
                crate::ui::fmt_bytes(level.total as f64)
            ))
            .size(12.0)
            .strong()
            .color(Palette::TEXT),
        );
        if level.partial {
            ui.label(
                RichText::new(tr(
                    "目录太大，统计提前停下，数字不完整",
                    "Stopped early — this folder is too large, totals are incomplete",
                ))
                .size(11.0)
                .color(Palette::WARN),
            );
        }
        if level.denied > 0 {
            ui.label(
                RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("有 {} 个目录没有权限", level.denied),
                    crate::i18n::Lang::En => format!("{} folder(s) not readable", level.denied),
                })
                .size(11.0)
                .color(Palette::TEXT_DIM),
            );
        }
    });
    if level.omitted > 0 {
        ui.label(
            RichText::new(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!(
                    "其余 {} 项，合计 {}（已计入总数）",
                    level.omitted,
                    crate::ui::fmt_bytes(level.omitted_bytes as f64)
                ),
                crate::i18n::Lang::En => format!(
                    "{} more, {} (included in the total)",
                    level.omitted,
                    crate::ui::fmt_bytes(level.omitted_bytes as f64)
                ),
            })
            .size(11.0)
            .color(Palette::TEXT_DIM),
        );
    }
    let rows = &level.rows;
    let total = level.total;
    let h = (ui.available_height() - 36.0).clamp(120.0, 640.0);
    ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("usage_level")
            .auto_shrink([false, false])
            .show_rows(ui, 22.0, rows.len(), |ui, range| {
                for i in range {
                    let drill = row_line(ui, &rows[i], total, true);
                    if drill {
                        *go = Some(UsageKind::Level {
                            path: rows[i].path.clone(),
                        });
                    }
                }
            });
    });
}

fn paint_search(ui: &mut egui::Ui, search: Option<&SearchReport>, go: &mut Option<UsageKind>) {
    let Some(search) = search else {
        return;
    };
    ui.label(
        RichText::new(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("「{}」找到 {} 个文件", search.needle, search.matched),
            crate::i18n::Lang::En => format!("“{}”: {} file(s)", search.needle, search.matched),
        })
        .size(12.0)
        .color(Palette::TEXT),
    )
    .on_hover_text(&search.path);
    if search.truncated {
        ui.label(
            RichText::new(tr("只列出最大的一部分", "Showing the largest matches only"))
                .size(11.0)
                .color(Palette::TEXT_DIM),
        );
    }
    if search.rows.is_empty() {
        return;
    }
    let biggest = search.rows.iter().map(|r| r.size).max().unwrap_or(0);
    let rows = &search.rows;
    let h = (ui.available_height() - 36.0).clamp(120.0, 640.0);
    ui.allocate_ui(egui::vec2(ui.available_width(), h), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("usage_search")
            .auto_shrink([false, false])
            .show_rows(ui, 22.0, rows.len(), |ui, range| {
                for i in range {
                    if row_line(ui, &rows[i], biggest, false) {
                        if let Some(parent) = crate::usage::parent_path(&rows[i].path) {
                            *go = Some(UsageKind::Level { path: parent });
                        }
                    }
                }
            });
    });
}

/// 画一行。目录在分层视图里可点，返回 true 表示要点进去。
fn row_line(ui: &mut egui::Ui, row: &UsageRow, total: u64, drill_dirs: bool) -> bool {
    use egui_phosphor::regular as icon;
    let frac = if total == 0 {
        0.0
    } else {
        (row.size as f64 / total as f64) as f32
    };
    let mut drill = false;
    ui.horizontal(|ui| {
        bar(ui, frac);
        let mark = if row.is_dir { icon::FOLDER } else { icon::FILE };
        let name = format!("{mark}  {}", row.name);
        let reserve = 132.0;
        let width = (ui.available_width() - reserve).max(40.0);
        let resp = ui
            .add_sized(
                [width, 18.0],
                egui::Label::new(RichText::new(name).size(12.0).color(Palette::TEXT))
                    .truncate()
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(&row.path);
        if resp.clicked() && (!drill_dirs || row.is_dir) {
            drill = true;
        }
        ui.label(
            RichText::new(pct(row.size, total))
                .size(11.0)
                .color(Palette::TEXT_DIM),
        );
        ui.label(
            RichText::new(crate::ui::fmt_bytes(row.size as f64))
                .size(12.0)
                .color(Palette::TEXT),
        );
    });
    drill
}

fn bar(ui: &mut egui::Ui, frac: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 8.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Palette::TRACK);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    if fill.width() > 0.5 {
        painter.rect_filled(fill, 2.0, Palette::ACCENT);
    }
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
