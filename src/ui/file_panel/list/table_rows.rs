//! File table header, row rendering, and drag/drop feedback.

use egui::{RichText, Sense};
use egui_extras::{Column, TableBuilder};

use super::super::super::ime::ime_singleline;
use super::super::super::{
    fmt_mtime, join_path, open_intent, perm_string, FilePanelState, OpenIntent, SortKey,
    DEFAULT_COLS,
};
use super::super::helpers::{
    drag_source_paths, entry_context, file_icon, stem_char_len, valid_move_srcs, DragPaths, Menu,
};
use crate::proto::FileEntry;
use crate::theme::Palette;
use crate::ui::fmt_bytes;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_table_rows(
    ui: &mut egui::Ui,
    state: &mut FilePanelState,
    entries: &[FileEntry],
    cwd: &str,
    favs: &[String],
    has_clip: bool,
    scroll_to_row: Option<usize>,
    sort_click: &mut Option<SortKey>,
    menu: &mut Vec<Menu>,
    clicks: &mut Vec<usize>,
    rclick: &mut Option<usize>,
    navigate: &mut Option<String>,
    open_file: &mut Option<String>,
    open_image: &mut Option<String>,
    open_pdf: &mut Option<String>,
    open_docx: &mut Option<String>,
    confirm_open: &mut Option<(String, u64)>,
    confirm_text: &mut Option<(String, u64)>,
    rename_commit: &mut Option<(String, String)>,
    cancel_rename: &mut bool,
    drop_move: &mut Option<(Vec<String>, String)>,
    broken_link: &mut bool,
    spring_target: &mut Option<String>,
    focus_list: &mut bool,
    now: f64,
    mod_ctrl: bool,
    mod_shift: bool,
) {
    // 表头列宽拖拽产生的 (列号, 本帧位移)；在 Frame 闭包外声明，闭包结束后统一应用并落盘
    let mut col_drag: Option<(usize, f32)> = None;
    let mut col_drag_done = false;

    egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 6,
            right: 2,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            // 滚动条滑块用灰度（非浮动），尤其拖动(active)时用深灰——否则默认拖动时偏白看不见
            ui.spacing_mut().scroll.floating = false;
            ui.spacing_mut().scroll.foreground_color = false;
            ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_rgb(193, 188, 175);
            ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_rgb(154, 148, 134);
            ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_rgb(114, 109, 97);
            // 拖拽悬停高亮用独立图层绘制：TableBuilder 闭包内 ui 已被可变借用，不能再取 ui.painter()
            let dnd_painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("file_dnd_hl"),
            ));
            // 列宽自管（Column::exact + 表头自绘拖拽）：内建 resizable 的 TableState 私有且按
            // id_salt(cwd) 隔离，既不能跨目录共享也无法持久化；自管后列宽全局一致并写入配置。
            if state.col_w.iter().all(|w| *w <= 0.0) {
                state.col_w = crate::store::load_file_cols().unwrap_or(DEFAULT_COLS);
            }
            let colw = state.col_w;
            let mut tbl = TableBuilder::new(ui)
                // 按目录区分滚动状态：进入子目录/切换目录后从顶部开始，不沿用上个目录的滚动位置
                .id_salt(cwd)
                .striped(true)
                .resizable(false)
                .sense(Sense::click_and_drag())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(colw[0]).clip(true))
                .column(Column::exact(colw[1]).clip(true))
                .column(Column::exact(colw[2]).clip(true))
                .column(Column::exact(colw[3]).clip(true))
                // owner 列不用 remainder：列表填不满时右侧留白，便于右键空白处操作当前文件夹
                .column(Column::exact(colw[4]).clip(true));
            // 键盘移动选中行时滚动到该行
            if let Some(r) = scroll_to_row {
                tbl = tbl.scroll_to_row(r, Some(egui::Align::Center));
            }
            tbl.header(22.0, |mut h| {
                let (cur, desc) = (state.sort_key, state.sort_desc);
                // 列宽拖拽热区：贴在表头列右缘，悬停/拖动时显示竖线并给出双向箭头光标
                let mut resize_handle = |ui: &mut egui::Ui, idx: usize| {
                    let r = ui.max_rect();
                    let hr = egui::Rect::from_min_max(
                        egui::pos2(r.right() - 2.0, r.top() - 3.0),
                        egui::pos2(r.right() + 4.0, r.bottom() + 3.0),
                    );
                    let resp = ui.interact(hr, ui.id().with(("colw", idx)), Sense::drag());
                    if resp.hovered() || resp.dragged() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                        ui.painter().vline(
                            r.right() + 1.0,
                            hr.y_range(),
                            egui::Stroke::new(2.0, Palette::ACCENT.gamma_multiply(0.6)),
                        );
                    }
                    if resp.dragged() {
                        col_drag = Some((idx, resp.drag_delta().x));
                    }
                    if resp.drag_stopped() {
                        col_drag_done = true;
                    }
                };
                // 可排序表头：点击切换排序键/方向，激活列显示升降箭头
                for (idx, (k, label)) in [
                    (SortKey::Name, crate::i18n::tr("名称", "Name")),
                    (SortKey::Size, crate::i18n::tr("大小", "Size")),
                    (SortKey::Mtime, crate::i18n::tr("修改时间", "Modified")),
                ]
                .into_iter()
                .enumerate()
                {
                    h.col(|ui| {
                        let arrow = if cur == k {
                            if desc {
                                egui_phosphor::regular::CARET_DOWN
                            } else {
                                egui_phosphor::regular::CARET_UP
                            }
                        } else {
                            ""
                        };
                        let color = if cur == k {
                            Palette::ACCENT
                        } else {
                            Palette::TEXT_DIM
                        };
                        if ui
                            .add(
                                egui::Label::new(
                                    RichText::new(format!("{label} {arrow}"))
                                        .strong()
                                        .color(color),
                                )
                                .sense(Sense::click()),
                            )
                            .clicked()
                        {
                            *sort_click = Some(k);
                        }
                        resize_handle(ui, idx);
                    });
                }
                // 不可排序：权限 / 所有者
                for (idx, t) in [
                    crate::i18n::tr("权限", "Perm"),
                    crate::i18n::tr("所有者", "Owner"),
                ]
                .into_iter()
                .enumerate()
                {
                    h.col(|ui| {
                        ui.label(RichText::new(t).strong().color(Palette::TEXT_DIM));
                        resize_handle(ui, 3 + idx);
                    });
                }
            })
            .body(|mut body| {
                for (i, e) in entries.iter().enumerate() {
                    let full = join_path(cwd, &e.name);
                    body.row(21.0, |mut row| {
                        row.set_selected(state.selected.contains(&i));
                        row.col(|ui| {
                            // 重命名中：显示输入框（默认选中后缀前的主名）
                            // 必须走 ime_singleline：与新建文件夹相同，绕开 egui 0.34 fcitx Commit 门
                            // （否则中文只能输一次 / 第二次无法组字）。
                            let renaming_here = matches!(&state.renaming, Some(r) if r.idx == i);
                            if renaming_here {
                                if let Some(r) = &mut state.renaming {
                                    let te_id = "file_rename_inline";
                                    let (resp, submit) =
                                        ime_singleline(ui, te_id, &mut r.buf, &mut r.ime);
                                    if r.init {
                                        let stem = stem_char_len(&r.buf);
                                        let id = egui::Id::new(te_id);
                                        let mut st =
                                            egui::text_edit::TextEditState::load(ui.ctx(), id)
                                                .unwrap_or_default();
                                        st.cursor.set_char_range(Some(
                                            egui::text_selection::CCursorRange::two(
                                                egui::text::CCursor::new(0),
                                                egui::text::CCursor::new(stem),
                                            ),
                                        ));
                                        st.store(ui.ctx(), id);
                                        resp.request_focus();
                                        r.init = false;
                                    }
                                    if submit && !r.buf.trim().is_empty() {
                                        *rename_commit =
                                            Some((full.clone(), join_path(cwd, r.buf.trim())));
                                    }
                                    if resp.lost_focus() {
                                        *cancel_rename = true;
                                    }
                                }
                            } else {
                                // 图标着色：目录/指向目录的软链=强调色；断链=危险色；其它软链=警示色；普通文件=弱色
                                let icon_col = if e.is_dir || e.link_dir {
                                    Palette::ACCENT
                                } else if e.is_link && e.link_target.is_none() {
                                    Palette::DANGER
                                } else if e.is_link {
                                    Palette::WARN
                                } else {
                                    Palette::TEXT_DIM
                                };
                                ui.spacing_mut().item_spacing.x = 5.0;
                                ui.label(RichText::new(file_icon(e)).color(icon_col));
                                // 名称：单行显示，超出由列 clip(true) 裁剪；悬停给出完整名称——应对超长 / 特殊字符 / emoji
                                let name_resp =
                                    ui.label(RichText::new(&e.name).color(Palette::TEXT));
                                // 软链：紧跟暗色「→ 目标」，让指向一目了然；断链显示红色「断链」
                                if e.is_link {
                                    match &e.link_target {
                                        Some(t) => {
                                            ui.label(
                                                RichText::new(format!("→ {t}"))
                                                    .color(Palette::TEXT_DIM)
                                                    .size(11.0),
                                            );
                                        }
                                        None => {
                                            ui.label(
                                                RichText::new(crate::i18n::tr(
                                                    "→ 断链",
                                                    "→ broken",
                                                ))
                                                .color(Palette::DANGER)
                                                .size(11.0),
                                            );
                                        }
                                    }
                                }
                                // 悬停 tooltip：完整名称（+ 链接目标 / 断链说明）
                                let tip = match (e.is_link, &e.link_target) {
                                    (true, Some(t)) => format!("{}\n→ {t}", e.name),
                                    (true, None) => format!(
                                        "{}\n{}",
                                        e.name,
                                        crate::i18n::tr(
                                            "断链（目标不存在）",
                                            "broken link (target missing)"
                                        )
                                    ),
                                    _ => e.name.clone(),
                                };
                                name_resp.on_hover_text(tip);
                            }
                        });
                        row.col(|ui| {
                            // 目录与「指向目录的软链」不显示字节大小；文件型软链的 size 已由 worker 改为目标大小
                            let s = if e.is_dir || e.link_dir {
                                "-".to_string()
                            } else {
                                fmt_bytes(e.size as f64)
                            };
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
                            *focus_list = true; // 点击行 → 文件列表夺取键盘焦点（表外应用，避免借用冲突）
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
                            // 双击：目录进入、图片看图、文本编辑、大/非文本确认、指向目录的软链跟随进入、断链提示
                            match open_intent(e, &full) {
                                OpenIntent::Navigate(p) => *navigate = Some(p),
                                OpenIntent::Image(p) => *open_image = Some(p),
                                OpenIntent::Pdf(p) => *open_pdf = Some(p),
                                OpenIntent::Docx(p) => *open_docx = Some(p),
                                OpenIntent::ConfirmText(p, sz) => *confirm_text = Some((p, sz)),
                                OpenIntent::ConfirmLarge(p, sz) => *confirm_open = Some((p, sz)),
                                OpenIntent::Text(p) => *open_file = Some(p),
                                OpenIntent::Broken => *broken_link = true,
                            }
                        }
                        if r.secondary_clicked() {
                            *rclick = Some(i);
                        }
                        // 拖拽源：整个拖动过程持续写入载荷（多选则整组、否则单项），
                        // 避免只在 drag_started 一帧设置时偶发丢失/沿用上次的旧载荷。
                        if r.drag_started() || r.dragged() {
                            let paths = drag_source_paths(state, entries, cwd, i);
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
                                *spring_target = Some(full.clone());
                            }
                            if let Some(payload) = r.dnd_release_payload::<DragPaths>() {
                                let srcs = valid_move_srcs(&payload.0, &full);
                                if !srcs.is_empty() {
                                    *drop_move = Some((srcs, full.clone()));
                                }
                            }
                        } else if let Some(payload) = r.dnd_release_payload::<DragPaths>() {
                            let srcs = valid_move_srcs(&payload.0, cwd);
                            if !srcs.is_empty() {
                                *drop_move = Some((srcs, cwd.to_string()));
                            }
                        }
                        let is_fav = favs.iter().any(|f| f == &full);
                        entry_context(&r, e, i, &full, has_clip, is_fav, menu);
                    });
                }
            });
        });
    // 行被点击 → 让出终端焦点，使方向键导航文件列表（表格借用结束后再操作 ui，避免借用冲突）
    if *focus_list {
        ui.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
    }

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
            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("file_drag_preview"),
            ));
            let font = egui::FontId::proportional(12.0);
            let galley = painter.layout_no_wrap(text, font, egui::Color32::WHITE);
            let pad = egui::vec2(8.0, 4.0);
            let rect =
                egui::Rect::from_min_size(pos + egui::vec2(14.0, 8.0), galley.size() + pad * 2.0);
            painter.rect_filled(rect, 6.0, Palette::ACCENT.gamma_multiply(0.95));
            painter.galley(rect.min + pad, galley, egui::Color32::WHITE);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    }

    // 应用表头列宽拖拽；松手时写入配置（重启恢复）
    if let Some((i, dx)) = col_drag {
        state.col_w[i] = (state.col_w[i] + dx).clamp(40.0, 800.0);
    }
    if col_drag_done {
        crate::store::save_file_cols(&state.col_w);
    }
}
