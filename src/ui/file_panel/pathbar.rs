//! 文件区路径栏：面包屑按钮、双击编辑、右键复制/粘贴。占用分析用同一套。

use egui::{RichText, Sense};

use super::ime::ime_apply_events;
use super::{path_is_prefix, read_clip_path, write_clip_path};
use crate::theme::Palette;

/// 路径栏跨帧状态。`cwd` 只读；跳转目标由 [`show`] 返回，调用方自己写入。
pub struct Crumbs<'a> {
    pub cwd: &'a str,
    pub edit: &'a mut Option<String>,
    pub focus: &'a mut bool,
    pub rsel: &'a mut Option<(usize, usize)>,
    pub ime: &'a mut Option<(usize, usize)>,
    pub pending: &'a mut Option<(String, f64)>,
    pub trail: &'a mut Option<String>,
    /// 文本框 id。文件区和占用分析必须不同，否则焦点和选区会串。
    pub id_salt: &'static str,
}

/// 画出面包屑或编辑框。返回本帧要跳转的路径（调用方再做 `~` 展开和规范化）。
pub fn show(ui: &mut egui::Ui, c: &mut Crumbs<'_>) -> Option<String> {
    use egui_phosphor::regular as icon;
    let mut nav: Option<String> = None;
    let te_id = egui::Id::new(c.id_salt);
    if c.edit.is_some() {
        let mut go: Option<String> = None;
        let mut done = false;
        let take_focus = *c.focus;
        let pre_range = egui::text_edit::TextEditState::load(ui.ctx(), te_id)
            .and_then(|s| s.cursor.char_range());
        let rsel_prev = *c.rsel;
        let mut new_rsel: Option<Option<(usize, usize)>> = None;
        if let Some(buf) = c.edit.as_mut() {
            ime_apply_events(ui, te_id, buf, c.ime);
            let out = egui::TextEdit::singleline(buf)
                .id(te_id)
                .desired_width(ui.available_width() - 4.0)
                .hint_text(crate::i18n::tr(
                    "输入路径后回车跳转，Esc 取消",
                    "Enter a path, Enter to go, Esc to cancel",
                ))
                .show(ui);
            if take_focus {
                let len = buf.chars().count();
                let mut st = out.state.clone();
                st.cursor
                    .set_char_range(Some(egui::text_selection::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(len),
                    )));
                st.store(ui.ctx(), te_id);
                out.response.request_focus();
                new_rsel = Some(None);
            }
            let resp = &out.response;
            let menu = resp.context_menu(|ui| {
                ui.set_min_width(140.0);
                if ui.button(crate::i18n::tr("复制", "Copy")).clicked() {
                    let text = rsel_prev
                        .and_then(|(a, b)| {
                            (a < b).then(|| buf.chars().skip(a).take(b - a).collect::<String>())
                        })
                        .unwrap_or_else(|| buf.clone());
                    write_clip_path(ui, text);
                    ui.close();
                }
                if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                    if let Some(t) = read_clip_path(ui) {
                        *buf = t;
                    }
                    ui.close();
                }
                if ui
                    .button(crate::i18n::tr("粘贴并转到", "Paste & go"))
                    .clicked()
                {
                    if let Some(t) = read_clip_path(ui) {
                        go = Some(t);
                    }
                    ui.close();
                }
            });
            let sec_pressed = ui.input(|i| {
                i.pointer.secondary_pressed()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| resp.rect.contains(p))
            });
            if sec_pressed {
                let sel = pre_range.and_then(|r| {
                    let a = r.primary.index.min(r.secondary.index);
                    let b = r.primary.index.max(r.secondary.index);
                    (a < b).then_some((a, b))
                });
                new_rsel = Some(sel);
            }
            let eff = new_rsel.unwrap_or(rsel_prev);
            let sec_down = ui.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
            if menu.is_some() {
                resp.request_focus();
                if let Some((a, b)) = eff {
                    if a < b {
                        if let Some(mut st) = egui::text_edit::TextEditState::load(ui.ctx(), te_id)
                        {
                            st.cursor.set_char_range(Some(
                                egui::text_selection::CCursorRange::two(
                                    egui::text::CCursor::new(a),
                                    egui::text::CCursor::new(b),
                                ),
                            ));
                            st.store(ui.ctx(), te_id);
                        }
                    }
                }
            }
            if sec_down {
                ui.ctx().request_repaint();
            }
            if menu.is_some() || sec_down {
                if let Some((a, b)) = eff {
                    if a < b {
                        let r0 = out.galley.pos_from_cursor(egui::text::CCursor::new(a));
                        let r1 = out.galley.pos_from_cursor(egui::text::CCursor::new(b));
                        let sel = egui::Rect::from_min_max(
                            out.galley_pos + r0.min.to_vec2(),
                            out.galley_pos + egui::vec2(r1.min.x, r0.max.y),
                        );
                        ui.painter().with_clip_rect(out.text_clip_rect).rect_filled(
                            sel,
                            2.0,
                            egui::Color32::from_rgba_unmultiplied(0xd9, 0x70, 0x49, 72),
                        );
                    }
                }
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                let t = buf.trim();
                if !t.is_empty() {
                    go = Some(t.to_string());
                }
                done = true;
            } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                done = true;
            } else if menu.is_none() && (resp.lost_focus() || resp.clicked_elsewhere()) {
                done = true;
            }
        }
        if let Some(v) = new_rsel {
            *c.rsel = v;
        }
        *c.focus = false;
        if let Some(p) = go {
            nav = Some(p);
        }
        if done {
            *c.edit = None;
            *c.rsel = None;
            *c.ime = None;
        }
    } else {
        let now_t = ui.input(|i| i.time);
        let cwd_s = c.cwd.to_string();
        let trail_s = c.trail.clone();
        let mut enter_edit = false;
        let mut nav_click: Option<String> = None;
        let ghost_col = egui::Color32::from_rgb(0xb8, 0xb3, 0xa6);
        // 目录一变就把「当前目录的最后一段」滚进可视区。`stick_to_right` 只在「本来就贴在最右边」
        // 时才跟随：用户滚过一次滚轮之后它就不跟了，切到很深的子目录时当前目录被挤出可视区。
        let seen_id = egui::Id::new((c.id_salt, "crumbs_cwd"));
        let cwd_changed = ui
            .data(|d| d.get_temp::<String>(seen_id))
            .as_deref()
            != Some(c.cwd);
        if cwd_changed {
            let cwd = c.cwd.to_string();
            ui.data_mut(|d| d.insert_temp(seen_id, cwd));
        }
        let bc_resp = egui::ScrollArea::horizontal()
            .id_salt((c.id_salt, "crumbs"))
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .stick_to_right(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let root = ui.add(
                        egui::Label::new(RichText::new(icon::HOUSE).color(Palette::ACCENT))
                            .sense(Sense::click()),
                    );
                    if root.double_clicked() {
                        enter_edit = true;
                    } else if root.clicked() {
                        nav_click = Some("/".into());
                    }
                    // 根目录没有段可滚：回到最左边。
                    if cwd_changed && cwd_s.split('/').all(str::is_empty) {
                        root.scroll_to_me(Some(egui::Align::LEFT));
                    }
                    let mut combined = root;
                    let mut acc = String::new();
                    for seg in cwd_s.split('/').filter(|s| !s.is_empty()) {
                        ui.label(RichText::new("›").color(Palette::TEXT_DIM));
                        acc.push('/');
                        acc.push_str(seg);
                        let here = acc.clone();
                        let is_last = here == cwd_s;
                        let color = if is_last {
                            Palette::TEXT
                        } else {
                            Palette::TEXT_DIM
                        };
                        let r = ui.add(
                            egui::Label::new(RichText::new(seg).color(color)).sense(Sense::click()),
                        );
                        if is_last && cwd_changed {
                            // 滚到这一段（不是灰色的「幽灵」尾巴）靠右可见。
                            r.scroll_to_me(Some(egui::Align::RIGHT));
                        }
                        if r.double_clicked() {
                            enter_edit = true;
                        } else if r.clicked() {
                            nav_click = Some(here);
                        }
                        combined |= r;
                    }
                    if let Some(trail) = trail_s.as_ref() {
                        if path_is_prefix(&cwd_s, trail) && trail != &cwd_s {
                            let suffix = if cwd_s == "/" {
                                trail.trim_start_matches('/')
                            } else {
                                trail[cwd_s.len()..].trim_start_matches('/')
                            };
                            for seg in suffix.split('/').filter(|s| !s.is_empty()) {
                                ui.label(RichText::new("›").color(ghost_col));
                                acc.push('/');
                                acc.push_str(seg);
                                let here = acc.clone();
                                let r =
                                    ui.add(
                                        egui::Label::new(RichText::new(seg).color(ghost_col))
                                            .sense(Sense::click()),
                                    )
                                    .on_hover_text(
                                        crate::i18n::tr("回到此目录", "Go to this folder"),
                                    );
                                if r.double_clicked() {
                                    enter_edit = true;
                                } else if r.clicked() {
                                    nav_click = Some(here);
                                }
                                combined |= r;
                            }
                        }
                    }
                    let rest = ui.available_size_before_wrap();
                    if rest.x > 8.0 {
                        let (_, resp) = ui.allocate_exact_size(rest, Sense::click());
                        if resp.double_clicked() {
                            enter_edit = true;
                        }
                        combined |= resp;
                    }
                    combined
                })
                .inner
            })
            .inner;
        bc_resp.context_menu(|ui| {
            ui.set_min_width(140.0);
            if ui.button(crate::i18n::tr("复制", "Copy")).clicked() {
                write_clip_path(ui, c.cwd.to_string());
                ui.close();
            }
            if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                if let Some(t) = read_clip_path(ui) {
                    *c.edit = Some(t);
                    *c.focus = true;
                }
                ui.close();
            }
            if ui
                .button(crate::i18n::tr("粘贴并转到", "Paste & go"))
                .clicked()
            {
                if let Some(t) = read_clip_path(ui) {
                    nav = Some(t);
                }
                ui.close();
            }
        });
        if enter_edit {
            *c.edit = Some(c.cwd.to_string());
            *c.focus = true;
            *c.pending = None;
        } else if let Some(p) = nav_click {
            *c.pending = Some((p, now_t));
        }
        if let Some((p, t)) = c.pending.clone() {
            if now_t - t >= 0.28 {
                nav = Some(p);
                *c.pending = None;
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(120));
            }
        }
    }
    nav
}
