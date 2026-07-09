//! 右侧文件列表：工具栏、面包屑、表格与拖拽。从 file_panel 拆出，行为不变。

use std::collections::HashSet;

use egui::{RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::proto::FileEntry;
use crate::theme::Palette;
use crate::ui::fmt_bytes;
use super::{
    basename, fmt_mtime, join_path, normalize_path, parent_of, path_is_prefix, perm_string,
    read_clip_path, write_clip_path, Dialog, FileAction, FilePanelState, MoveRecord, OpenIntent,
    Renaming, SortKey, DEFAULT_COLS, UP_DWELL, UP_FLASH, open_intent,
};
use super::ime::{ime_apply_events, ime_singleline};

pub(super) fn file_list(ui: &mut egui::Ui, state: &mut FilePanelState, has_clip: bool, actions: &mut Vec<FileAction>) {
    // 工具栏：扁平图标条（带浅色背景）
    use egui_phosphor::regular as icon;
    let mut bc_nav: Option<String> = None;
    let mut paste_here = false; // 右键菜单「粘贴到此目录」触发（has_clip 由 App 传入）
    // 弹簧式拖拽导航：本帧拖拽悬停的目标目录（某文件夹），统一计时跳转
    let mut spring_target: Option<String> = None;
    egui::Frame::new()
        .fill(Palette::PANEL_2)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 4))
        // 左侧外边距、不给右侧留边（右侧顶到边）
        .outer_margin(egui::Margin { left: 8, right: 0, top: 2, bottom: 2 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if tool_btn(ui, icon::ARROW_CLOCKWISE, crate::i18n::tr("刷新", "Refresh")) && state.refresh_dir(&state.cwd.clone()) {
                    actions.push(FileAction::List(state.cwd.clone()));
                }
                // 返回上一个目录（浏览器式后退，可连续返回；上级目录改由面包屑点击）
                let back_enabled = !state.nav_history.is_empty();
                let back_col = if back_enabled { Palette::TEXT } else { Palette::TEXT_DIM };
                if tool_btn_color(ui, icon::ARROW_LINE_LEFT, crate::i18n::tr("返回上一个目录", "Back"), back_col) && back_enabled {
                    if let Some(prev) = state.nav_history.pop() {
                        state.nav_pending_back = true;
                        state.cwd = prev;
                        state.selected.clear();
                    }
                }
                // 上传 / 删除 / 粘贴均不放工具栏：上传走拖拽或空白处右键菜单，删除走右键菜单或 Delete 键，
                // 粘贴走空白处右键菜单「粘贴到此目录」（保持工具栏精简）
                // 复制路径：点击后短暂显示绿色对勾，再恢复
                let now = ui.input(|i| i.time);
                let copied = state.copy_flash.is_some_and(|t| now - t < 1.1);
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

                // 收藏夹：弹出可滚动路径列表，点路径进入、右侧删除，点其它处关闭。
                // 图标：当前目录已收藏=实心灰 ★，未收藏=空心 ☆；弹窗打开时按钮显示按下态（灰底）。
                let cwd_fav = !state.cwd.is_empty() && state.favorites.iter().any(|f| f == &state.cwd);
                let pop_id = ui.make_persistent_id("fav_popup");
                let pop_open = ui.memory(|m| m.is_popup_open(pop_id));
                let star_glyph = if cwd_fav { "★" } else { "☆" };
                let star_col = if cwd_fav { Palette::TEXT_DIM } else { Palette::TEXT };
                let star = ui
                    .scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.weak_bg_fill = if pop_open { Palette::TRACK } else { egui::Color32::TRANSPARENT };
                        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
                        v.widgets.active.bg_stroke = egui::Stroke::NONE;
                        ui.add(
                            egui::Button::new(RichText::new(star_glyph).size(16.0).color(star_col))
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
                egui::popup_below_widget(ui, pop_id, &star, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                    ui.set_min_width(280.0);
                    if state.favorites.is_empty() {
                        crate::ui::empty_state(ui, egui_phosphor::regular::STAR, crate::i18n::tr("暂无收藏，右键文件夹/空白处可加入", "No favorites — right-click to add"), false);
                    } else {
                        egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                            for (i, p) in state.favorites.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.set_min_width(266.0);
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除收藏", "Remove")).clicked() {
                                            fav_remove = Some(i);
                                        }
                                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                            let disp = trailing_path(p, 40);
                                            if ui.add(egui::Label::new(RichText::new(format!("{} {}", icon::FOLDER, disp)).size(12.0).color(Palette::TEXT)).selectable(false).sense(Sense::click()))
                                                .on_hover_text(p.as_str())
                                                .clicked()
                                            {
                                                fav_nav = Some(p.clone());
                                            }
                                        });
                                    });
                                });
                            }
                        });
                    }
                });
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
                let path_err = state.nav_error.contains(&state.cwd) && !state.loading.contains(&state.cwd);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                  if path_err {
                      ui.add(egui::Label::new(RichText::new(icon::WARNING_CIRCLE).color(Palette::DANGER).size(15.0)).sense(Sense::hover()))
                          .on_hover_text(crate::i18n::tr("路径无效或无法访问", "Invalid or inaccessible path"));
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
                            .hint_text(crate::i18n::tr("输入路径后回车跳转，Esc 取消", "Enter path, Enter to go, Esc to cancel"))
                            .show(ui);
                        if take_focus {
                            // 首帧进入编辑：聚焦并全选路径，便于直接覆盖输入
                            let len = buf.chars().count();
                            let mut st = out.state.clone();
                            st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(len),
                            )));
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
                                    .and_then(|(a, b)| (a < b).then(|| buf.chars().skip(a).take(b - a).collect::<String>()))
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
                            if ui.button(crate::i18n::tr("粘贴并转到", "Paste & go")).clicked() {
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
                                && i.pointer.interact_pos().is_some_and(|p| resp.rect.contains(p))
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
                        // 菜单打开期间：保持焦点并还原 egui 内部选区（供重新获焦后继续正常操作）。
                        if menu.is_some() {
                            resp.request_focus();
                            if let Some((a, b)) = eff {
                                if a < b {
                                    if let Some(mut st) = egui::text_edit::TextEditState::load(ui.ctx(), te_id) {
                                        st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                            egui::text::CCursor::new(a),
                                            egui::text::CCursor::new(b),
                                        )));
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
                        // 用 consume_key「吃掉」回车事件：否则同一帧内文件列表的键盘处理器
                        // 会再次响应这次回车，误打开当前选中行（进入子目录 / 用编辑器打开文件）。
                        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                            let t = buf.trim();
                            if !t.is_empty() {
                                go = Some(t.to_string());
                            }
                            done = true;
                        } else if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            done = true;
                        } else if menu.is_none() && (resp.lost_focus() || resp.clicked_elsewhere()) {
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
                                    let color = if is_last { Palette::TEXT } else { Palette::TEXT_DIM };
                                    let r = ui.add(egui::Label::new(RichText::new(seg).color(color)).sense(Sense::click()));
                                    if r.double_clicked() {
                                        enter_edit = true;
                                    } else if r.clicked() {
                                        nav_click = Some(here);
                                    }
                                    combined = combined | r;
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
                                            let r = ui
                                                .add(egui::Label::new(RichText::new(seg).color(ghost_col)).sense(Sense::click()))
                                                .on_hover_text(crate::i18n::tr("回到此目录", "Go to this folder"));
                                            if r.double_clicked() {
                                                // 双击仍进入编辑当前 cwd（不是幽灵路径）
                                                enter_edit = true;
                                            } else if r.clicked() {
                                                nav_click = Some(here);
                                            }
                                            combined = combined | r;
                                        }
                                    }
                                }
                                // 末尾空白：双击进入编辑；也并入右键菜单区
                                let rest = ui.available_size_before_wrap();
                                if rest.x > 8.0 {
                                    let (_, resp) = ui.allocate_exact_size(rest, Sense::click());
                                    if resp.double_clicked() {
                                        enter_edit = true;
                                    }
                                    combined = combined | resp;
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
                        if ui.button(crate::i18n::tr("粘贴并转到", "Paste & go")).clicked() {
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
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
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
        // 跳到未缓存的路径（如「粘贴并转到」到一个此前没进过的目录）：本帧 sync_tree 已在改 cwd 前
        // 跑过、不会再列举它，若不在此显式发起 List，会卡在空白/不加载。命中缓存则无需重复请求。
        if !state.listings.contains_key(&state.cwd) && !state.loading.contains(&state.cwd) {
            state.loading.insert(state.cwd.clone());
            actions.push(FileAction::List(state.cwd.clone()));
        }
    }
    ui.separator();

    if state.loading.contains(&state.cwd) && !state.listings.contains_key(&state.cwd) {
        // 已重试过（att>1）即视为弱网，给出「网络较慢，正在重试」提示，避免看似卡死无反馈
        let slow = state.load_at.get(&state.cwd).map(|v| v.1 > 1).unwrap_or(false);
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
        .inner_margin(egui::Margin { left: 12, right: 0, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(Palette::TEXT_DIM).size(12.0));
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
                    && ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false))
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
    let mut open_pdf: Option<String> = None; // 双击 PDF 文件
    let mut open_docx: Option<String> = None; // 双击 Word(docx) 文件
    let mut confirm_open: Option<(String, u64)> = None; // 大文本文件待确认
    let mut confirm_text: Option<(String, u64)> = None; // 非文本后缀，待确认是否强开
    let mut rename_commit: Option<(String, String)> = None;
    let mut cancel_rename = false;
    let mut drop_move: Option<(Vec<String>, String)> = None; // 拖拽释放到某文件夹 -> (srcs, dest_dir)
    let mut broken_link = false; // 双击断链 -> 表后统一提示
    let now = ui.input(|i| i.time);
    let (mod_ctrl, mod_shift) = ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));

    // 空白区域右键 -> 新建文件/目录（仅覆盖列表区域，避免遮挡上方路径栏）
    let bg = ui.interact(ui.available_rect_before_wrap(), ui.id().with("filelist_bg"), Sense::click());
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
            && ui.button(format!("{}  {}", icon::ARROW_CLOCKWISE, crate::i18n::tr("刷新", "Refresh"))).clicked()
        {
            bg_refresh = true;
            ui.close();
        }
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
            let faved = state.favorites.iter().any(|f| f == &state.cwd);
            let lbl = if faved {
                format!("★  {}", crate::i18n::tr("取消收藏当前目录", "Remove bookmark"))
            } else {
                format!("☆  {}", crate::i18n::tr("收藏当前目录", "Bookmark current dir"))
            };
            if ui.button(lbl).clicked() {
                bg_fav = true;
                ui.close();
            }
            if ui.button(format!("{}  {}", icon::TERMINAL_WINDOW, crate::i18n::tr("在终端打开当前目录", "Open current dir in terminal"))).clicked() {
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
            let nc = if down { (cur + 1).min(n - 1) } else { cur.saturating_sub(1) };
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

    // 表头列宽拖拽产生的 (列号, 本帧位移)；在 Frame 闭包外声明，闭包结束后统一应用并落盘
    let mut col_drag: Option<(usize, f32)> = None;
    let mut col_drag_done = false;

    egui::Frame::new()
        .inner_margin(egui::Margin { left: 6, right: 2, top: 0, bottom: 0 })
        .show(ui, |ui| {
    // 滚动条滑块用灰度（非浮动），尤其拖动(active)时用深灰——否则默认拖动时偏白看不见
    ui.spacing_mut().scroll.floating = false;
    ui.spacing_mut().scroll.foreground_color = false;
    ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_rgb(193, 188, 175);
    ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_rgb(154, 148, 134);
    ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_rgb(114, 109, 97);
    // 拖拽悬停高亮用独立图层绘制：TableBuilder 闭包内 ui 已被可变借用，不能再取 ui.painter()
    let dnd_painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("file_dnd_hl")));
    // 列宽自管（Column::exact + 表头自绘拖拽）：内建 resizable 的 TableState 私有且按
    // id_salt(cwd) 隔离，既不能跨目录共享也无法持久化；自管后列宽全局一致并写入配置。
    if state.col_w.iter().all(|w| *w <= 0.0) {
        state.col_w = crate::store::load_file_cols().unwrap_or(DEFAULT_COLS);
    }
    let colw = state.col_w;
    let mut tbl = TableBuilder::new(ui)
        // 按目录区分滚动状态：进入子目录/切换目录后从顶部开始，不沿用上个目录的滚动位置
        .id_salt(&cwd)
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
                    ui.painter().vline(r.right() + 1.0, hr.y_range(), egui::Stroke::new(2.0, Palette::ACCENT.gamma_multiply(0.6)));
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
                    resize_handle(ui, idx);
                });
            }
            // 不可排序：权限 / 所有者
            for (idx, t) in [crate::i18n::tr("权限", "Perm"), crate::i18n::tr("所有者", "Owner")].into_iter().enumerate() {
                h.col(|ui| {
                    ui.label(RichText::new(t).strong().color(Palette::TEXT_DIM));
                    resize_handle(ui, 3 + idx);
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
                        // 必须走 ime_singleline：与新建文件夹相同，绕开 egui 0.34 fcitx Commit 门
                        // （否则中文只能输一次 / 第二次无法组字）。
                        let renaming_here = matches!(&state.renaming, Some(r) if r.idx == i);
                        if renaming_here {
                            if let Some(r) = &mut state.renaming {
                                let te_id = "file_rename_inline";
                                let (resp, submit) = ime_singleline(ui, te_id, &mut r.buf, &mut r.ime);
                                if r.init {
                                    let stem = stem_char_len(&r.buf);
                                    let id = egui::Id::new(te_id);
                                    let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
                                    st.cursor.set_char_range(Some(egui::text_selection::CCursorRange::two(
                                        egui::text::CCursor::new(0),
                                        egui::text::CCursor::new(stem),
                                    )));
                                    st.store(ui.ctx(), id);
                                    resp.request_focus();
                                    r.init = false;
                                }
                                if submit && !r.buf.trim().is_empty() {
                                    rename_commit = Some((full.clone(), join_path(&cwd, r.buf.trim())));
                                }
                                if resp.lost_focus() {
                                    cancel_rename = true;
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
                            let name_resp = ui.label(RichText::new(&e.name).color(Palette::TEXT));
                            // 软链：紧跟暗色「→ 目标」，让指向一目了然；断链显示红色「断链」
                            if e.is_link {
                                match &e.link_target {
                                    Some(t) => {
                                        ui.label(RichText::new(format!("→ {t}")).color(Palette::TEXT_DIM).size(11.0));
                                    }
                                    None => {
                                        ui.label(RichText::new(crate::i18n::tr("→ 断链", "→ broken")).color(Palette::DANGER).size(11.0));
                                    }
                                }
                            }
                            // 悬停 tooltip：完整名称（+ 链接目标 / 断链说明）
                            let tip = match (e.is_link, &e.link_target) {
                                (true, Some(t)) => format!("{}\n→ {t}", e.name),
                                (true, None) => format!("{}\n{}", e.name, crate::i18n::tr("断链（目标不存在）", "broken link (target missing)")),
                                _ => e.name.clone(),
                            };
                            name_resp.on_hover_text(tip);
                        }
                    });
                    row.col(|ui| {
                        // 目录与「指向目录的软链」不显示字节大小；文件型软链的 size 已由 worker 改为目标大小
                        let s = if e.is_dir || e.link_dir { "-".to_string() } else { fmt_bytes(e.size as f64) };
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
                        focus_list = true; // 点击行 → 文件列表夺取键盘焦点（表外应用，避免借用冲突）
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
                            OpenIntent::Navigate(p) => navigate = Some(p),
                            OpenIntent::Image(p) => open_image = Some(p),
                            OpenIntent::Pdf(p) => open_pdf = Some(p),
                            OpenIntent::Docx(p) => open_docx = Some(p),
                            OpenIntent::ConfirmText(p, sz) => confirm_text = Some((p, sz)),
                            OpenIntent::ConfirmLarge(p, sz) => confirm_open = Some((p, sz)),
                            OpenIntent::Text(p) => open_file = Some(p),
                            OpenIntent::Broken => broken_link = true,
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
                    let is_fav = favs.iter().any(|f| f == &full);
                    entry_context(&r, e, i, &full, has_clip, is_fav, &mut menu);
                });
            }
        });
    });
    // 行被点击 → 让出终端焦点，使方向键导航文件列表（表格借用结束后再操作 ui，避免借用冲突）
    if focus_list {
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

    // 应用表头列宽拖拽；松手时写入配置（重启恢复）
    if let Some((i, dx)) = col_drag {
        state.col_w[i] = (state.col_w[i] + dx).clamp(40.0, 800.0);
    }
    if col_drag_done {
        crate::store::save_file_cols(&state.col_w);
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
                state.renaming = Some(Renaming { idx: i, buf: e.name.clone(), init: true, ime: None });
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
    // 双击断链：目标不存在，提示用户（不进入、不打开）
    if broken_link {
        actions.push(FileAction::Status(
            crate::i18n::tr("断链：目标不存在", "Broken link: target missing").into(),
        ));
    }
    // 双击打开文本文件
    if let Some(p) = open_file {
        actions.push(FileAction::OpenFile { path: p, force: false });
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
            Menu::Favorite(p) => toggle_favorite(state, p),
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
    /// 收藏该文件夹路径
    Favorite(String),
}

/// 条目右键菜单：把用户选择记录到 `menu`。
fn entry_context(resp: &egui::Response, e: &FileEntry, idx: usize, full: &str, has_clip: bool, is_fav: bool, menu: &mut Vec<Menu>) {
    use egui_phosphor::regular as icon;
    resp.context_menu(|ui| {
        ui.set_min_width(178.0); // 菜单宽度（较前一版 210 收窄约 15%）
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
        if e.is_dir {
            let lbl = if is_fav {
                format!("★  {}", crate::i18n::tr("取消收藏该文件夹", "Remove bookmark"))
            } else {
                format!("☆  {}", crate::i18n::tr("收藏该文件夹", "Bookmark folder"))
            };
            if ui.button(lbl).clicked() {
                menu.push(Menu::Favorite(full.to_string()));
                ui.close();
            }
        }
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
pub(super) struct DragPaths(pub(super) Vec<String>);

/// 计算拖拽源：与 clip_targets 同规则，仅取路径。
fn drag_source_paths(state: &FilePanelState, entries: &[FileEntry], cwd: &str, idx: usize) -> Vec<String> {
    clip_targets(state, entries, cwd, idx).into_iter().map(|(p, _)| p).collect()
}

/// 把拖拽到目标目录 dest 的源过滤为合法移动：排除目标自身、已直接位于 dest 内、
/// 以及把祖先目录拖进其子目录的非法情况。
pub(super) fn valid_move_srcs(srcs: &[String], dest: &str) -> Vec<String> {
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

/// 路径过长时仅显示尾部字符（前缀省略号）；用于收藏列表显示。
fn trailing_path(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        p.to_string()
    } else {
        let tail: String = p.chars().skip(n - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}

/// 收藏/取消收藏切换并持久化到该服务器的收藏表。
fn toggle_favorite(state: &mut FilePanelState, path: String) {
    if path.is_empty() {
        return;
    }
    if let Some(i) = state.favorites.iter().position(|f| f == &path) {
        state.favorites.remove(i);
    } else {
        state.favorites.push(path);
    }
    crate::store::save_favorites(&state.server_key, &state.favorites);
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
