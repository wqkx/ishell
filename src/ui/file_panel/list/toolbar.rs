use egui::{RichText, Sense};

use super::super::ime::ime_apply_events;
use super::super::{
    normalize_path, path_is_prefix, read_clip_path, write_clip_path, FileAction, FilePanelState,
};
use super::helpers::{tool_btn, tool_btn_color, trailing_path};
use crate::theme::Palette;

pub(super) fn toolbar(
    ui: &mut egui::Ui,
    state: &mut FilePanelState,
    actions: &mut Vec<FileAction>,
) {
    // 工具栏：扁平图标条（带浅色背景）
    use egui_phosphor::regular as icon;
    let mut bc_nav: Option<String> = None;
    egui::Frame::new()
        .fill(Palette::PANEL_2)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 4))
        // 左侧外边距、不给右侧留边（右侧顶到边）
        .outer_margin(egui::Margin {
            left: 8,
            right: 0,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // 工具栏按钮：后退 / 刷新 / 复制路径 / 收藏夹（收藏放最后）。按钮之间不加
                // 分割线，保持一条扁平图标条。上传 / 删除 / 粘贴均不放工具栏：上传走拖拽或
                // 空白处右键菜单，删除走右键菜单或 Delete 键，粘贴走空白处右键菜单「粘贴到
                // 此目录」（保持工具栏精简）。上级目录改由面包屑点击。
                // 后退（浏览器式后退，可连续返回）
                let back_enabled = !state.nav_history.is_empty();
                let back_col = if back_enabled {
                    Palette::TEXT
                } else {
                    Palette::TEXT_DIM
                };
                if tool_btn_color(
                    ui,
                    icon::CARET_LEFT,
                    crate::i18n::tr("返回上一个目录", "Back"),
                    back_col,
                ) && back_enabled
                {
                    if let Some(prev) = state.nav_history.pop() {
                        state.nav_pending_back = true;
                        state.cwd = prev;
                        state.selected.clear();
                    }
                }

                // 刷新
                if tool_btn(
                    ui,
                    icon::ARROW_CLOCKWISE,
                    crate::i18n::tr("刷新", "Refresh"),
                ) && state.refresh_dir(&state.cwd.clone())
                {
                    actions.push(FileAction::List(state.cwd.clone()));
                }

                // 复制路径：点击后短暂显示绿色对勾，再恢复
                let now = ui.input(|i| i.time);
                let copied = state.copy_flash.is_some_and(|t| now - t < 1.1);
                let (ci, ctip, ccol) = if copied {
                    (
                        icon::CHECK,
                        crate::i18n::tr("已复制", "Copied"),
                        Palette::OK,
                    )
                } else {
                    (
                        icon::COPY,
                        crate::i18n::tr("复制当前路径", "Copy path"),
                        Palette::TEXT,
                    )
                };
                if tool_btn_color(ui, ci, ctip, ccol) && !state.cwd.is_empty() {
                    actions.push(FileAction::CopyPath(state.cwd.clone()));
                    state.copy_flash = Some(now);
                }
                if copied {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(150));
                }

                // 收藏夹（最后一个按钮）：弹出可滚动路径列表，点路径进入、右侧删除，点其它处关闭。
                // 用 phosphor STAR 图标（与刷新/复制等同一字体，笔画粗细一致）——已收藏用暖色
                // 实心感的强调色标识，未收藏用普通文字色；弹窗打开时按钮显示按下态（灰底）。
                let cwd_fav =
                    !state.cwd.is_empty() && state.favorites.iter().any(|f| f == &state.cwd);
                let pop_id = ui.make_persistent_id("fav_popup");
                let pop_open = ui.memory(|m| m.is_popup_open(pop_id));
                let star_col = if cwd_fav {
                    Palette::ACCENT
                } else {
                    Palette::TEXT
                };
                let star = ui
                    .scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.weak_bg_fill = if pop_open {
                            Palette::TRACK
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
                        v.widgets.active.bg_stroke = egui::Stroke::NONE;
                        ui.add(
                            egui::Button::new(RichText::new(icon::STAR).size(16.0).color(star_col))
                                .min_size(egui::vec2(30.0, 26.0))
                                .corner_radius(6.0),
                        )
                        .on_hover_text(crate::i18n::tr("收藏夹", "Favorites"))
                    })
                    .inner;
                if star.clicked() {
                    ui.memory_mut(|m| m.toggle_popup(pop_id));
                }
                let mut fav_nav: Option<String> = None;
                let mut fav_remove: Option<usize> = None;
                egui::popup_below_widget(
                    ui,
                    pop_id,
                    &star,
                    egui::PopupCloseBehavior::CloseOnClickOutside,
                    |ui| {
                        ui.set_min_width(280.0);
                        if state.favorites.is_empty() {
                            crate::ui::empty_state(
                                ui,
                                egui_phosphor::regular::STAR,
                                crate::i18n::tr(
                                    "暂无收藏，右键文件夹/空白处可加入",
                                    "No favorites — right-click to add",
                                ),
                                false,
                            );
                        } else {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for (i, p) in state.favorites.iter().enumerate() {
                                        ui.horizontal(|ui| {
                                            ui.set_min_width(266.0);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui
                                                        .add(
                                                            egui::Button::new(
                                                                RichText::new(icon::TRASH)
                                                                    .size(12.0)
                                                                    .color(Palette::TEXT_DIM),
                                                            )
                                                            .frame(false),
                                                        )
                                                        .on_hover_text(crate::i18n::tr(
                                                            "删除收藏",
                                                            "Remove",
                                                        ))
                                                        .clicked()
                                                    {
                                                        fav_remove = Some(i);
                                                    }
                                                    ui.with_layout(
                                                        egui::Layout::left_to_right(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            let disp = trailing_path(p, 40);
                                                            if ui
                                                                .add(
                                                                    egui::Label::new(
                                                                        RichText::new(format!(
                                                                            "{} {}",
                                                                            icon::FOLDER,
                                                                            disp
                                                                        ))
                                                                        .size(12.0)
                                                                        .color(Palette::TEXT),
                                                                    )
                                                                    .selectable(false)
                                                                    .sense(Sense::click()),
                                                                )
                                                                .on_hover_text(p.as_str())
                                                                .clicked()
                                                            {
                                                                fav_nav = Some(p.clone());
                                                            }
                                                        },
                                                    );
                                                },
                                            );
                                        });
                                    }
                                });
                        }
                    },
                );
                if let Some(i) = fav_remove {
                    if i < state.favorites.len() {
                        state.favorites.remove(i);
                        crate::store::save_favorites(&state.server_key, &state.favorites);
                    }
                }
                if let Some(p) = fav_nav {
                    state.cwd = p;
                    state.selected.clear();
                    ui.memory_mut(|m| m.close_popup(pop_id));
                }

                ui.add_space(4.0);
                ui.separator();
                // 路径栏最右侧：当前目录列举失败（无效/无权限路径）时显示「路径无效」标识
                let path_err =
                    state.nav_error.contains(&state.cwd) && !state.loading.contains(&state.cwd);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if path_err {
                        ui.add(
                            egui::Label::new(
                                RichText::new(icon::WARNING_CIRCLE)
                                    .color(Palette::DANGER)
                                    .size(15.0),
                            )
                            .sense(Sense::hover()),
                        )
                        .on_hover_text(crate::i18n::tr(
                            "路径无效或无法访问",
                            "Invalid or inaccessible path",
                        ));
                        ui.add_space(3.0);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if state.path_edit.is_some() {
                            // 路径编辑模式：回车跳转、Esc 或点击别处退出
                            let mut go: Option<String> = None;
                            let mut done = false;
                            let take_focus = state.path_edit_focus;
                            // 固定 id，便于在 show 之前读到「上一帧的选区」（右键当帧 egui 会把它塌成光标）。
                            let te_id = ui.make_persistent_id("file_path_edit_box");
                            let pre_range = egui::text_edit::TextEditState::load(ui.ctx(), te_id)
                                .and_then(|s| s.cursor.char_range());
                            // 上一帧存下的右键选区（Copy 类型，先于可变借用读出）；本帧若右键会更新它。
                            let rsel_prev = state.path_edit_rsel;
                            let mut new_rsel: Option<Option<(usize, usize)>> = None;
                            if let Some(buf) = &mut state.path_edit {
                                // 与对话框相同：自绘 IME，避免路径栏中文第二次无法输入
                                ime_apply_events(ui, te_id, buf, &mut state.path_edit_ime);
                                let out = egui::TextEdit::singleline(buf)
                                    .id(te_id)
                                    .desired_width(ui.available_width() - 4.0)
                                    .hint_text(crate::i18n::tr(
                                        "输入路径后回车跳转，Esc 取消",
                                        "Enter path, Enter to go, Esc to cancel",
                                    ))
                                    .show(ui);
                                if take_focus {
                                    // 首帧进入编辑：聚焦并全选路径，便于直接覆盖输入
                                    let len = buf.chars().count();
                                    let mut st = out.state.clone();
                                    st.cursor.set_char_range(Some(
                                        egui::text_selection::CCursorRange::two(
                                            egui::text::CCursor::new(0),
                                            egui::text::CCursor::new(len),
                                        ),
                                    ));
                                    st.store(ui.ctx(), te_id);
                                    out.response.request_focus();
                                    new_rsel = Some(None); // 进入编辑：清除旧的右键选区记忆
                                }
                                let resp = &out.response;
                                // 右键菜单：复制**右键那刻选中的**文本（无选区则整条路径）/ 粘贴 / 粘贴并转到。
                                // 关键：Copy 用的是「右键当帧存下的选区」rsel_prev，而非点菜单时的 live 选区
                                //（后者早被 egui 塌缩/失焦清掉，会导致永远复制整条路径）。
                                let menu = resp.context_menu(|ui| {
                                    ui.set_min_width(140.0);
                                    if ui.button(crate::i18n::tr("复制", "Copy")).clicked() {
                                        let text = rsel_prev
                                            .and_then(|(a, b)| {
                                                (a < b).then(|| {
                                                    buf.chars()
                                                        .skip(a)
                                                        .take(b - a)
                                                        .collect::<String>()
                                                })
                                            })
                                            .unwrap_or_else(|| buf.clone());
                                        write_clip_path(ui, text);
                                        ui.close();
                                    }
                                    // 粘贴：替换编辑框内容（停留在编辑态，由用户回车确认）
                                    if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                                        if let Some(t) = read_clip_path(ui) {
                                            *buf = t;
                                        }
                                        ui.close();
                                    }
                                    // 粘贴并转到：直接跳转
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
                                // 右键当帧：把「塌缩前」的选区（pre_range）记下来，供本轮菜单 Copy 与高亮还原使用。
                                // 在「右键**按下**」这一帧捕获选区：egui 会在本帧 show 内因 any_pressed 把选区
                                // 塌成光标，而 pre_range 是 show **之前**读的，正好是塌缩前的完整选区。
                                // （之前用 secondary_clicked=抬起帧太晚，那时选区早在按下帧被塌掉了 → 永远 None。）
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
                                let sec_down = ui.input(|i| {
                                    i.pointer.button_down(egui::PointerButton::Secondary)
                                });
                                // 菜单打开期间：保持焦点并还原 egui 内部选区（供重新获焦后继续正常操作）。
                                if menu.is_some() {
                                    resp.request_focus();
                                    if let Some((a, b)) = eff {
                                        if a < b {
                                            if let Some(mut st) =
                                                egui::text_edit::TextEditState::load(
                                                    ui.ctx(),
                                                    te_id,
                                                )
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
                                // 仅在右键**按住**的短暂期间持续刷新，跟上「按下→抬起弹菜单」的过渡；菜单静止打开后
                                // 无需再刷（自绘高亮已画在最后一帧、会一直留在屏上），避免菜单久开时满帧空转。
                                if sec_down {
                                    ui.ctx().request_repaint();
                                }
                                // 自绘选区高亮：egui 只在有焦点时画、且右键会塌缩选区，都靠不住。
                                // 画的时机 = 菜单打开期间 **或** 右键按键仍按住时——后者覆盖「按下→抬起弹菜单」
                                // 整个窗口（含按住多帧），消除右键时高亮闪一下。按 galley 几何自绘、text_clip_rect 裁剪。
                                if menu.is_some() || sec_down {
                                    if let Some((a, b)) = eff {
                                        if a < b {
                                            let r0 = out
                                                .galley
                                                .pos_from_cursor(egui::text::CCursor::new(a));
                                            let r1 = out
                                                .galley
                                                .pos_from_cursor(egui::text::CCursor::new(b));
                                            let sel = egui::Rect::from_min_max(
                                                out.galley_pos + r0.min.to_vec2(),
                                                out.galley_pos + egui::vec2(r1.min.x, r0.max.y),
                                            );
                                            ui.painter()
                                                .with_clip_rect(out.text_clip_rect)
                                                .rect_filled(
                                                    sel,
                                                    2.0,
                                                    egui::Color32::from_rgba_unmultiplied(
                                                        0xd9, 0x70, 0x49, 72,
                                                    ),
                                                );
                                        }
                                    }
                                }
                                // 用 consume_key「吃掉」回车事件：否则同一帧内文件列表的键盘处理器
                                // 会再次响应这次回车，误打开当前选中行（进入子目录 / 用编辑器打开文件）。
                                if ui.input_mut(|i| {
                                    i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                                }) {
                                    let t = buf.trim();
                                    if !t.is_empty() {
                                        go = Some(t.to_string());
                                    }
                                    done = true;
                                } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    done = true;
                                } else if menu.is_none()
                                    && (resp.lost_focus() || resp.clicked_elsewhere())
                                {
                                    // 点击别处 -> 退出编辑模式（右键菜单打开期间不退出，否则菜单一开就消失）
                                    done = true;
                                }
                            }
                            if let Some(v) = new_rsel {
                                state.path_edit_rsel = v; // 写回本帧更新的右键选区记忆
                            }
                            state.path_edit_focus = false;
                            if let Some(p) = go {
                                bc_nav = Some(p);
                            }
                            if done {
                                state.path_edit = None;
                                state.path_edit_rsel = None;
                                state.path_edit_ime = None;
                            }
                        } else {
                            // 面包屑：路径超长时横向滚动（隐藏滚动条，滚轮滚动）；
                            // 单击逐级跳转，双击路径任意处（含分段、空白）进入编辑模式。
                            // 单击导航延后 ~0.28s 执行，期间若发生双击则取消，避免双击误触发跳转。
                            // 点到父级后，原路径的后续段以极淡色「幽灵」显示，仍可点击回到子目录。
                            let now_t = ui.input(|i| i.time);
                            let cwd_s = state.cwd.clone();
                            let trail_s = state.path_trail.clone();
                            let mut enter_edit = false;
                            let mut nav_click: Option<String> = None;
                            // 幽灵段颜色：比 TEXT_DIM 更淡，一眼能区分「当前路径」与「可回跳的子路径」
                            let ghost_col = egui::Color32::from_rgb(0xb8, 0xb3, 0xa6);
                            let bc_resp = egui::ScrollArea::horizontal()
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .auto_shrink([false, false])
                                .stick_to_right(true) // 路径过长时默认展示末尾（当前目录）
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 3.0;
                                        let root = ui.add(
                                            egui::Label::new(
                                                RichText::new(icon::HOUSE).color(Palette::ACCENT),
                                            )
                                            .sense(Sense::click()),
                                        );
                                        if root.double_clicked() {
                                            enter_edit = true;
                                        } else if root.clicked() {
                                            nav_click = Some("/".into());
                                        }
                                        // 合并所有面包屑元素的响应，供整条路径栏统一挂右键菜单：
                                        // egui 中子控件会「吃掉」次级（右键）点击，父容器响应收不到，
                                        // 故必须把菜单挂在各元素响应的并集上，右键才能在整条路径上都生效。
                                        let mut combined = root;
                                        let mut acc = String::new();
                                        // 活跃段：当前 cwd；幽灵段：trail 中 cwd 之后的子路径
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
                                                egui::Label::new(RichText::new(seg).color(color))
                                                    .sense(Sense::click()),
                                            );
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
                                                for seg in
                                                    suffix.split('/').filter(|s| !s.is_empty())
                                                {
                                                    ui.label(RichText::new("›").color(ghost_col));
                                                    acc.push('/');
                                                    acc.push_str(seg);
                                                    let here = acc.clone();
                                                    let r = ui
                                                        .add(
                                                            egui::Label::new(
                                                                RichText::new(seg).color(ghost_col),
                                                            )
                                                            .sense(Sense::click()),
                                                        )
                                                        .on_hover_text(crate::i18n::tr(
                                                            "回到此目录",
                                                            "Go to this folder",
                                                        ));
                                                    if r.double_clicked() {
                                                        // 双击仍进入编辑当前 cwd（不是幽灵路径）
                                                        enter_edit = true;
                                                    } else if r.clicked() {
                                                        nav_click = Some(here);
                                                    }
                                                    combined |= r;
                                                }
                                            }
                                        }
                                        // 末尾空白：双击进入编辑；也并入右键菜单区
                                        let rest = ui.available_size_before_wrap();
                                        if rest.x > 8.0 {
                                            let (_, resp) =
                                                ui.allocate_exact_size(rest, Sense::click());
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
                            // 面包屑右键菜单：复制 / 粘贴（填入编辑框） / 粘贴并转到（直接跳转）。
                            bc_resp.context_menu(|ui| {
                                ui.set_min_width(140.0);
                                if ui.button(crate::i18n::tr("复制", "Copy")).clicked() {
                                    write_clip_path(ui, state.cwd.clone());
                                    ui.close();
                                }
                                // 粘贴：把路径填入编辑框（进入编辑态、全选），由用户回车确认
                                if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                                    if let Some(t) = read_clip_path(ui) {
                                        state.path_edit = Some(t);
                                        state.path_edit_focus = true;
                                    }
                                    ui.close();
                                }
                                // 粘贴并转到：直接跳转
                                if ui
                                    .button(crate::i18n::tr("粘贴并转到", "Paste & go"))
                                    .clicked()
                                {
                                    if let Some(t) = read_clip_path(ui) {
                                        bc_nav = Some(t);
                                    }
                                    ui.close();
                                }
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
                                    ui.ctx().request_repaint_after(
                                        std::time::Duration::from_millis(120),
                                    );
                                }
                            }
                        }
                    }); // 内层 left_to_right（面包屑/编辑框）
                }); // 外层 right_to_left（右侧无效标识）
            });
        });
    ui.add_space(2.0);
    if let Some(p) = bc_nav {
        // 规范化：去掉末尾多余的 "/"（否则与 worker 返回的规范路径不匹配，无法进入）
        state.cwd = normalize_path(&p);
        state.selected.clear();
        // 「粘贴并转到」等跳转必须退出路径编辑态：否则若此前处于编辑态，路径栏会继续显示
        // 旧的编辑框内容，造成「列表变了、面包屑没变」的错觉。
        state.path_edit = None;
        state.path_edit_focus = false;
        state.pending_nav = None;
        // 显式「跳转到路径」是用户主动动作，可信度高于被动预取：若这个路径之前被判定过
        // 「无效」（比如粘贴的文件夹是刚在另一个终端里创建的，上次探测时还不存在），不能让
        // 那次陈旧判定继续拦着——清掉无效标记与占位空列表，强制重新查一次，不然只能靠用户
        // 先跳父级目录刷新（连带清子目录缓存）才能间接清掉这个陈旧状态。
        if state.nav_error.remove(&state.cwd) {
            state.listings.remove(&state.cwd);
        }
        // 跳到未缓存的路径（如「粘贴并转到」到一个此前没进过的目录）：本帧 sync_tree 已在改 cwd 前
        // 跑过、不会再列举它，若不在此显式发起 List，会卡在空白/不加载。命中缓存则无需重复请求。
        if !state.listings.contains_key(&state.cwd) && !state.loading.contains(&state.cwd) {
            state.loading.insert(state.cwd.clone());
            actions.push(FileAction::List(state.cwd.clone()));
        }
    }
}
