//! Main window session tab bar. Split from layout.rs; behavior unchanged.

use egui::{RichText, Sense};

use crate::theme::Palette;

use super::util::edge_fade;
use super::widgets::*;
use super::App;

impl App {
    pub(super) fn top_tabs(&mut self, root: &mut egui::Ui) {
        use egui_phosphor::regular as icon;
        egui::Panel::top("tabs")
            .frame(
                egui::Frame::new()
                    .fill(Palette::PANEL_2)
                    .inner_margin(egui::Margin::symmetric(6, 4)),
            )
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    // 固定行高，使右侧按钮与标签在同一水平线居中对齐
                    ui.set_min_height(28.0);
                    let mut to_close = None;
                    let mut to_activate = None;
                    let mut reorder: Option<(usize, usize)> = None;
                    // 右侧按钮固定占位（传输 / 转发 / 群发 / 新建），剩余空间给可滚动标签条
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let active_xfers = self
                            .active
                            .and_then(|i| self.sessions.get(i))
                            .map(|s| s.transfers.iter().filter(|t| t.ok.is_none()).count())
                            .unwrap_or(0);
                        let label = if active_xfers > 0 {
                            format!(
                                "{} {} {}",
                                icon::ARROWS_DOWN_UP,
                                crate::i18n::tr("传输", "Xfer"),
                                active_xfers
                            )
                        } else {
                            format!(
                                "{} {}",
                                icon::ARROWS_DOWN_UP,
                                crate::i18n::tr("传输", "Xfer")
                            )
                        };
                        if flat_button(
                            ui,
                            &RichText::new(label),
                            crate::i18n::tr("显示/隐藏传输进度", "Show/hide transfers"),
                        ) {
                            self.show_transfers = !self.show_transfers;
                            if self.show_transfers {
                                self.xfer_just_opened = true;
                            }
                        }
                        let nfwd = self
                            .active
                            .and_then(|i| self.sessions.get(i))
                            .map(|s| s.forwards.len())
                            .unwrap_or(0);
                        let flabel = if nfwd > 0 {
                            format!(
                                "{} {} {}",
                                icon::ARROWS_LEFT_RIGHT,
                                crate::i18n::tr("转发", "Fwd"),
                                nfwd
                            )
                        } else {
                            format!(
                                "{} {}",
                                icon::ARROWS_LEFT_RIGHT,
                                crate::i18n::tr("转发", "Fwd")
                            )
                        };
                        if flat_button(
                            ui,
                            &RichText::new(flabel),
                            crate::i18n::tr("端口转发管理", "Port forwarding"),
                        ) {
                            self.fwd.show = !self.fwd.show;
                            if self.fwd.show {
                                self.fwd.just_opened = true;
                            }
                        }
                        if flat_button(
                            ui,
                            &RichText::new(format!(
                                "{} {}",
                                icon::MEGAPHONE,
                                crate::i18n::tr("群发", "Bcast")
                            )),
                            crate::i18n::tr(
                                "向所有已连接会话广播命令",
                                "Broadcast to all connected sessions",
                            ),
                        ) {
                            self.show_broadcast = !self.show_broadcast;
                        }
                        if flat_button(
                            ui,
                            &RichText::new(format!(
                                "{} {}",
                                icon::CODE,
                                crate::i18n::tr("片段", "Snip")
                            )),
                            crate::i18n::tr(
                                "命令片段库：保存常用命令一键发送到终端",
                                "Command snippets: save & send common commands",
                            ),
                        ) {
                            self.snip.show = !self.snip.show;
                            if self.snip.show {
                                self.snip.just_opened = true;
                            }
                        }
                        // 折叠监控栏/文件栏的开关已移到左侧监控栏右键菜单，避免右上角按钮过多
                        // 分隔竖线：把标签区（标签 + 新建）与右侧功能区分开（短、低调，和谐配色）
                        {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(11.0, ui.available_height()),
                                egui::Sense::hover(),
                            );
                            let cy = rect.center().y;
                            ui.painter().vline(
                                rect.center().x.round(),
                                (cy - 8.0)..=(cy + 8.0),
                                egui::Stroke::new(1.0, Palette::BORDER),
                            );
                        }
                        // 新建：固定在标签条右侧，标签溢出也不会被滚走
                        if flat_button(
                            ui,
                            &RichText::new(icon::PLUS).size(15.0),
                            crate::i18n::tr("新建连接", "New connection"),
                        ) {
                            self.connect_form.open_dialog();
                            self.show_close_confirm = false;
                        }

                        // 剩余空间：标签条横向可滚动；标签按动画位置放置，重排时平滑滑动。
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let mut drag_start: Option<usize> = None;
                            let mut new_grab: Option<f32> = None;
                            let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
                            let mut drag_w = 0.0f32; // 被拖标签宽度，用于算其跟手中心
                            let mut any_group = false;
                            let mut open_menu: Option<(u64, egui::Pos2)> = None;
                            let mut color_pick: Option<(u64, Option<super::tab_color::TabColor>)> =
                                None;
                            // 先取出标量字段，避免在借用 self.sessions 的循环里再借 self
                            let dragging_tab = self.tabbar.drag;
                            let grab_dx = self.tabbar.grab_dx;
                            let total_w_cache = self.tabbar.total_w;
                            let want_scroll = self.tabbar.scroll_to_active; // 本帧是否需把激活标签滚到可视区
                            let out = egui::ScrollArea::horizontal()
                                .auto_shrink([false, false])
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                                )
                                .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
                                .show(ui, |ui| {
                                    let tab_h = 26.0;
                                    let spacing = 4.0;
                                    // 预留区域撑出滚动内容宽度（用上一帧总宽）
                                    let (area, _) = ui.allocate_exact_size(
                                        egui::vec2(total_w_cache.max(1.0), tab_h),
                                        Sense::hover(),
                                    );
                                    let origin = area.min;
                                    let pointer = ui.input(|i| i.pointer.interact_pos());
                                    let drag_down = ui.input(|i| i.pointer.any_down());
                                    let ctx = ui.ctx().clone();
                                    let body_font = egui::TextStyle::Body.resolve(ui.style());
                                    let mut acc = 0.0f32; // 目标布局累计左边界
                                                          // 同名会话消歧（规则见 `tab_labels`）：标签上只在同名会话
                                                          // 分属不同主机时追加「· 主机」；#uid 不上标签，只在 hover
                                                          // 提示里跟在 user@host 后面。
                                                          // 只给单独占标签的会话算标题。折进去的 AI 窗口不参与同名消歧。
                                    let roots = super::session::tab_root_indices(&self.sessions);
                                    let labels = tab_labels(roots.iter().map(|&i| {
                                        let s = &self.sessions[i];
                                        // 标签固定用连接名（`s.title`），不用 OSC 0/2 动态标题。
                                        // vim/shell 改窗口标题不应搅乱会话标签——那是用户认连接的锚点。
                                        let host = (!s.cfg.is_local() && !s.cfg.host.is_empty())
                                            .then_some(s.cfg.host.as_str());
                                        (s.title.as_str(), host)
                                    }));
                                    let active_root = self.active.and_then(|a| {
                                        self.sessions.get(a).map(|s| {
                                            super::session::tab_root_uid(&self.sessions, s.uid)
                                        })
                                    });
                                    let active_uid = self
                                        .active
                                        .and_then(|a| self.sessions.get(a).map(|s| s.uid));
                                    let now = ui.input(|i| i.time);
                                    for (vis, &i) in roots.iter().enumerate() {
                                        let uid = self.sessions[i].uid;
                                        let members = super::session::group_member_indices(
                                            &self.sessions,
                                            uid,
                                        );
                                        let has_kids = members.len() > 1;
                                        let showing = if has_kids && active_uid != Some(uid) {
                                            active_uid.and_then(|u| {
                                                self.sessions
                                                    .iter()
                                                    .find(|t| t.uid == u)
                                                    .map(|t| t.title.clone())
                                            })
                                        } else {
                                            None
                                        };
                                        let s = &self.sessions[i];
                                        let selected = active_root == Some(s.uid);
                                        if has_kids {
                                            any_group = true;
                                        }
                                        let viewing_child = selected && showing.is_some();
                                        // 标签只显示会话标题：机器人图标、开启者名这类
                                        // MCP 内部标识用户看着只是噪声（归属信息仍在
                                        // list_sessions/写入弹窗里发挥作用，不上屏）。
                                        let (display_title, title_dup) = labels[vis].clone();
                                        // 宽度 = 左margin(9)+圆点(10)+间隔(6)+标题+间隔(6)+关闭(18)+右margin(9)
                                        // 有 AI 窗口时圆点外侧多一圈，标题右移一截。
                                        let extra = if has_kids { 3.0 } else { 0.0 };
                                        let title_w = ctx.fonts_mut(|f| {
                                            f.layout_no_wrap(
                                                display_title.clone(),
                                                body_font.clone(),
                                                Palette::TEXT,
                                            )
                                            .rect
                                            .width()
                                        });
                                        let w = 58.0 + extra + title_w;
                                        let target = acc;
                                        // 激活标签若被请求滚动到可视区：按其「目标槽」位置请求滚动（带边距余量）
                                        if selected && want_scroll {
                                            let r = egui::Rect::from_min_size(
                                                egui::pos2(origin.x + target, origin.y),
                                                egui::vec2(w, tab_h),
                                            );
                                            ui.scroll_to_rect(
                                                r.expand2(egui::vec2(12.0, 0.0)),
                                                None,
                                            );
                                        }
                                        let id = egui::Id::new(("tabx", s.uid));
                                        let dragging_this = drag_down && dragging_tab == Some(vis);
                                        if dragging_this {
                                            drag_w = w;
                                        }
                                        let x = if dragging_this {
                                            let want = pointer
                                                .map(|p| p.x - origin.x - grab_dx)
                                                .unwrap_or(target);
                                            ctx.animate_value_with_time(id, want, 0.0)
                                        // 跟手
                                        } else {
                                            ctx.animate_value_with_time(id, target, 0.14)
                                            // 缓动到目标槽
                                        };
                                        let tab_rect = egui::Rect::from_min_size(
                                            egui::pos2(origin.x + x, origin.y),
                                            egui::vec2(w, tab_h),
                                        );
                                        // 交互：整张标签可点击（激活）/拖动（排序）；关闭区在上层优先
                                        let mut tip = tab_hover_text(
                                            &s.tip,
                                            title_dup.then_some(s.uid),
                                            s.ai_owner_label.as_deref(),
                                        );
                                        if let Some(c) = s.tab_color {
                                            let (zh, en) = c.label();
                                            tip.push_str(" · ");
                                            tip.push_str(crate::i18n::tr(zh, en));
                                        }
                                        if has_kids {
                                            tip.push_str(" · ");
                                            tip.push_str(&crate::i18n::tr(
                                                "点圆点或双击切换窗口",
                                                "Click the dot or double-click to switch",
                                            ));
                                            if viewing_child {
                                                if let Some(cur) = showing.as_deref() {
                                                    tip.push_str(&format!(
                                                        " · {} {cur}",
                                                        crate::i18n::tr("当前", "Showing")
                                                    ));
                                                }
                                            }
                                        }
                                        let resp = ui
                                            .interact(
                                                tab_rect,
                                                egui::Id::new(("tab", s.uid)),
                                                Sense::click_and_drag(),
                                            )
                                            .on_hover_text(tip);
                                        let close_rect = egui::Rect::from_center_size(
                                            egui::pos2(
                                                tab_rect.right() - 18.0,
                                                tab_rect.center().y,
                                            ),
                                            egui::vec2(18.0, 18.0),
                                        );
                                        let close_resp = ui.interact(
                                            close_rect,
                                            egui::Id::new(("tabclose", s.uid)),
                                            Sense::click(),
                                        );
                                        // 绘制。有颜色时铺一层浅色，没选中的标签也不再透明，颜色才看得出来。
                                        let fill = tab_fill(dragging_this, selected, s.tab_color);
                                        let p = ui.painter();
                                        p.rect_filled(
                                            tab_rect,
                                            egui::CornerRadius {
                                                nw: 6,
                                                ne: 6,
                                                sw: 0,
                                                se: 0,
                                            },
                                            fill,
                                        );
                                        // 激活标签底部 2px 下划线。设了颜色就用那颗实色，否则仍是珊瑚。
                                        if selected && !dragging_this {
                                            let y = tab_rect.bottom() - 1.0;
                                            let mark = s
                                                .tab_color
                                                .map(|c| c.rgb())
                                                .unwrap_or(Palette::ACCENT);
                                            p.hline(
                                                tab_rect.left()..=tab_rect.right(),
                                                y,
                                                egui::Stroke::new(2.0, mark),
                                            );
                                        }
                                        let dot_at = egui::pos2(
                                            tab_rect.left() + 14.0 + extra * 0.5,
                                            tab_rect.center().y,
                                        );
                                        paint_status_dot(
                                            p,
                                            dot_at,
                                            if s.ai_owned {
                                                Palette::ACCENT
                                            } else if s.connected {
                                                Palette::OK
                                            } else {
                                                Palette::WARN
                                            },
                                            has_kids.then_some((viewing_child, now)),
                                        );
                                        // 圆点单独接点击，盖在整张标签上面。没有 AI 窗口时不拦，单击仍是激活。
                                        let dot_clicked = if has_kids {
                                            let dot_resp = ui.interact(
                                                egui::Rect::from_center_size(
                                                    dot_at,
                                                    egui::vec2(18.0, 18.0),
                                                ),
                                                egui::Id::new(("tabdot", s.uid)),
                                                Sense::click(),
                                            );
                                            offer_tab_color(
                                                &dot_resp,
                                                s.uid,
                                                s.tab_color,
                                                &mut color_pick,
                                            );
                                            dot_resp.clicked()
                                        } else {
                                            false
                                        };
                                        offer_tab_color(&resp, s.uid, s.tab_color, &mut color_pick);
                                        offer_tab_color(
                                            &close_resp,
                                            s.uid,
                                            s.tab_color,
                                            &mut color_pick,
                                        );
                                        let tcolor = if selected {
                                            Palette::TEXT
                                        } else {
                                            Palette::TEXT_DIM
                                        };
                                        p.text(
                                            egui::pos2(
                                                tab_rect.left() + 25.0 + extra,
                                                tab_rect.center().y,
                                            ),
                                            egui::Align2::LEFT_CENTER,
                                            &display_title,
                                            body_font.clone(),
                                            tcolor,
                                        );
                                        let xcolor = if close_resp.hovered() {
                                            Palette::DANGER
                                        } else {
                                            Palette::TEXT_DIM
                                        };
                                        p.text(
                                            close_rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            icon::X,
                                            egui::FontId::proportional(12.0),
                                            xcolor,
                                        );
                                        // 事件：关闭优先。有 AI 窗口时，圆点或双击打开切换菜单，不把正在看的窗口拽回根会话。
                                        let menu_anchor = egui::pos2(
                                            tab_rect.left() + 4.0,
                                            tab_rect.bottom() + 2.0,
                                        );
                                        if close_resp.clicked() {
                                            to_close = Some(i);
                                        } else if has_kids && (dot_clicked || resp.double_clicked())
                                        {
                                            open_menu = Some((s.uid, menu_anchor));
                                            if !selected {
                                                to_activate = Some(i);
                                            }
                                        } else if resp.clicked() && !selected {
                                            to_activate = Some(i);
                                        } else if resp.middle_clicked() {
                                            to_close = Some(i);
                                        }
                                        if resp.drag_started() {
                                            drag_start = Some(vis);
                                            if let Some(pp) = pointer {
                                                // drag_started() 触发时指针已越过拖拽阈值离开真实按下点，
                                                // 抓取偏移须用按下位置计算，否则拖动起始瞬间标签会相对光标跳动
                                                let press = crate::ui::drag_press_pos(ui, pp);
                                                new_grab = Some(press.x - (origin.x + x));
                                            }
                                        }
                                        // 命中用「目标槽」位置，稳定判断拖到哪个槽
                                        tab_rects.push((
                                            vis,
                                            egui::Rect::from_min_size(
                                                egui::pos2(origin.x + target, origin.y),
                                                egui::vec2(w, tab_h),
                                            ),
                                        ));
                                        acc += w + spacing;
                                    }
                                    acc // 返回总宽
                                });
                            let total_w = out.inner.max(1.0);
                            if any_group {
                                ui.ctx()
                                    .request_repaint_after(std::time::Duration::from_millis(32));
                            }
                            if let Some((uid, color)) = color_pick {
                                if let Some(s) = self.sessions.iter_mut().find(|s| s.uid == uid) {
                                    s.tab_color = color;
                                }
                            }
                            if let Some((uid, anchor)) = open_menu {
                                self.tab_menu = Some(super::TabMenu {
                                    root_uid: uid,
                                    anchor,
                                    opened_pass: ui.ctx().cumulative_pass_nr(),
                                });
                            }
                            self.tabbar.total_w = total_w;
                            self.tabbar.scroll_to_active = false; // 滚动请求一次性消费
                                                                  // 溢出渐隐：提示左右还有被隐藏的标签
                            let off = out.state.offset.x;
                            let vw = out.inner_rect.width();
                            if off > 0.5 {
                                edge_fade(ui.painter(), out.inner_rect, true, Palette::PANEL_2);
                            }
                            if off + vw < total_w - 0.5 {
                                edge_fade(ui.painter(), out.inner_rect, false, Palette::PANEL_2);
                            }
                            // 应用拖拽状态（循环外，避免与 self.sessions 借用冲突）
                            if let Some(g) = new_grab {
                                self.tabbar.grab_dx = g;
                            }
                            if let Some(f) = drag_start {
                                self.tabbar.drag = Some(f);
                            }
                            // 拖动过程中：用「被拖标签的跟手中心」与相邻标签「目标槽中心」比较，
                            // 越过相邻标签中点才换位（单帧只移一位）。换位后邻居中心移到另一侧，
                            // 判定条件自然失效，避免抓取点偏移（grab_dx）导致的来回抖动。
                            if let Some(from) = self.tabbar.drag {
                                if ui.input(|i| i.pointer.any_down()) {
                                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                                        // 被拖标签跟手中心（屏幕横坐标），与绘制时的跟手位口径一致
                                        let drag_center =
                                            pos.x - self.tabbar.grab_dx + drag_w / 2.0;
                                        let mut to = from;
                                        // 向左：越过左邻目标槽中心
                                        if from > 0 {
                                            if let Some(&(_, lr)) = tab_rects.get(from - 1) {
                                                if drag_center < lr.center().x {
                                                    to = from - 1;
                                                }
                                            }
                                        }
                                        // 向右：越过右邻目标槽中心（与向左互斥）
                                        if to == from {
                                            if let Some(&(_, rr)) = tab_rects.get(from + 1) {
                                                if drag_center > rr.center().x {
                                                    to = from + 1;
                                                }
                                            }
                                        }
                                        if to != from {
                                            reorder = Some((from, to));
                                            self.tabbar.drag = Some(to);
                                        }
                                    }
                                } else {
                                    self.tabbar.drag = None; // 松手：被拖标签从跟手位缓动回目标槽
                                }
                            }
                        });
                    });
                    if let Some((from, to)) = reorder {
                        let roots = super::session::tab_root_indices(&self.sessions);
                        if let (Some(&from), Some(&to)) = (roots.get(from), roots.get(to)) {
                            self.reorder_session(from, to);
                        }
                    }
                    if let Some(i) = to_activate {
                        self.active = Some(i);
                        self.tabbar.scroll_to_active = true; // 点击的标签若被遮挡则滚到可视区
                        if let Some(s) = self.sessions.get_mut(i) {
                            s.terminal.request_focus(); // 点击标签后焦点切到终端
                        }
                    }
                    if let Some(i) = to_close {
                        // 关掉的是整个标签：根会话以及折在它里面的 AI 窗口。只要其中有一个
                        // 还连着、是 AI 在用、或有未保存的编辑，就先问一声。
                        let root_uid = self.sessions.get(i).map(|s| s.uid);
                        let need_confirm = root_uid.is_some_and(|uid| {
                            super::session::group_member_indices(&self.sessions, uid)
                                .iter()
                                .any(|&j| {
                                    let s = &self.sessions[j];
                                    let dirty = super::util::lock_mutex(&self.editor_state)
                                        .dirty_tabs_for_session(s.uid);
                                    super::session::session_close_needs_confirm(
                                        s.connected,
                                        s.ai_owned,
                                        dirty,
                                    )
                                })
                        });
                        if need_confirm {
                            self.pending_close_tab = root_uid;
                        } else if let Some(uid) = root_uid {
                            self.close_session_group(uid);
                        }
                    }
                });
            });
        self.paint_tab_menu(root.ctx());
    }

    /// 关掉标签里的单个 AI 窗口。需要确认（连着、AI 在用、或有未保存编辑）就挂起等确认框，
    /// 否则直接关。
    fn request_close_one(&mut self, uid: u64) {
        let Some(idx) = self.session_idx_by_uid(uid) else {
            return;
        };
        let dirty = super::util::lock_mutex(&self.editor_state).dirty_tabs_for_session(uid);
        let s = &self.sessions[idx];
        if super::session::session_close_needs_confirm(s.connected, s.ai_owned, dirty) {
            self.pending_close_tab = None;
            self.pending_close_one = Some(uid);
        } else {
            self.close_session(idx);
        }
    }

    /// 标签下拉里切换这个标签底下的 shell。打开它的那一帧不响应「点外面关掉」，
    /// 否则点圆点的同一下会把它立刻关掉。
    fn paint_tab_menu(&mut self, ctx: &egui::Context) {
        let Some(menu) = self.tab_menu else {
            return;
        };
        if self.session_idx_by_uid(menu.root_uid).is_none() {
            self.tab_menu = None;
            return;
        }
        let members = super::session::group_member_indices(&self.sessions, menu.root_uid);
        if members.len() <= 1 {
            self.tab_menu = None;
            return;
        }
        let mut picked = None;
        let mut close_one: Option<u64> = None;
        let popup = egui::Area::new(egui::Id::new(("tab_group_menu", menu.root_uid)))
            .order(egui::Order::Foreground)
            .fixed_pos(menu.anchor)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(Palette::PANEL)
                    .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        ui.set_min_width(188.0);
                        ui.set_max_width(360.0);
                        ui.label(
                            RichText::new(crate::i18n::tr("切换窗口", "Switch shell"))
                                .size(11.0)
                                .color(Palette::TEXT_DIM),
                        );
                        for &idx in &members {
                            let s = &self.sessions[idx];
                            let is_root = s.uid == menu.root_uid;
                            let mut label = s.title.clone();
                            if is_root {
                                label =
                                    format!("{label} · {}", crate::i18n::tr("本窗口", "this tab"));
                            } else if let Some(origin) = s.ai_owner_label.as_deref() {
                                label = format!("{label} · {origin}");
                            }
                            // 一行：左边切换，AI 窗口的右边在鼠标移上来时露出 ×（关这一个窗口）。
                            // 根窗口没有 ×：它的关闭在标签上，关它会带走整个标签。
                            let (row, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), 24.0),
                                Sense::hover(),
                            );
                            let hovered = ui.rect_contains_pointer(row);
                            let close_rect = egui::Rect::from_center_size(
                                egui::pos2(row.right() - 12.0, row.center().y),
                                egui::vec2(20.0, 20.0),
                            );
                            let label_rect = if is_root {
                                row
                            } else {
                                egui::Rect::from_min_max(
                                    row.min,
                                    egui::pos2(close_rect.left() - 2.0, row.max.y),
                                )
                            };
                            // 自己画这一行：内建按钮在 `put`/对齐布局里都会把标题居中，和原来左对齐的
                            // 菜单不一样；整格可点、选中与悬停各有底色。
                            let label_resp = ui.interact(
                                label_rect,
                                egui::Id::new(("tab_menu_row", s.uid)),
                                Sense::click(),
                            );
                            let fill = if self.active == Some(idx) {
                                Some(Palette::ACCENT_SOFT)
                            } else if label_resp.hovered() {
                                Some(Palette::PANEL_2)
                            } else {
                                None
                            };
                            if let Some(f) = fill {
                                ui.painter().rect_filled(label_rect, 4, f);
                            }
                            ui.painter().text(
                                egui::pos2(label_rect.left() + 8.0, label_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                &label,
                                egui::TextStyle::Button.resolve(ui.style()),
                                Palette::TEXT,
                            );
                            if label_resp.clicked() {
                                picked = Some(idx);
                            }
                            if !is_root && hovered {
                                let x = ui.interact(
                                    close_rect,
                                    egui::Id::new(("tab_menu_close", s.uid)),
                                    Sense::click(),
                                );
                                let color = if x.hovered() {
                                    Palette::DANGER
                                } else {
                                    Palette::TEXT_DIM
                                };
                                ui.painter().text(
                                    close_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    egui_phosphor::regular::X,
                                    egui::FontId::proportional(13.0),
                                    color,
                                );
                                if x.clicked() {
                                    close_one = Some(s.uid);
                                }
                                x.on_hover_text(crate::i18n::tr(
                                    "关闭这个窗口",
                                    "Close this window",
                                ));
                            }
                        }
                    });
            });
        let pass = ctx.cumulative_pass_nr();
        let dismiss = ctx.input(|i| i.key_pressed(egui::Key::Escape))
            || (pass != menu.opened_pass
                && ctx.input(|i| i.pointer.any_click())
                && !popup.response.hovered());
        if let Some(uid) = close_one {
            self.tab_menu = None;
            self.request_close_one(uid);
        } else if let Some(idx) = picked {
            self.active = Some(idx);
            self.tabbar.scroll_to_active = true;
            if let Some(s) = self.sessions.get_mut(idx) {
                s.terminal.request_focus();
            }
            self.tab_menu = None;
        } else if dismiss {
            self.tab_menu = None;
        }
    }
}

/// 标签底色。没设颜色时保持原来的样子：选中是白卡片，其余透明。
fn tab_fill(
    dragging: bool,
    selected: bool,
    color: Option<super::tab_color::TabColor>,
) -> egui::Color32 {
    match (dragging, color, selected) {
        (true, Some(c), _) => c.tint(Palette::ACCENT_SOFT, 0.35),
        (true, None, _) => Palette::ACCENT_SOFT,
        (false, Some(c), true) => c.tint(Palette::PANEL, 0.20),
        (false, Some(c), false) => c.tint(Palette::PANEL_2, 0.34),
        (false, None, true) => Palette::PANEL,
        (false, None, false) => egui::Color32::TRANSPARENT,
    }
}

/// 右键标签弹出颜色。圆点、关闭钮盖在标签上面，右键落在它们身上也要能打开同一份菜单。
fn offer_tab_color(
    resp: &egui::Response,
    uid: u64,
    current: Option<super::tab_color::TabColor>,
    pick: &mut Option<(u64, Option<super::tab_color::TabColor>)>,
) {
    use super::tab_color::TabColor;

    resp.context_menu(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.label(
            RichText::new(crate::i18n::tr("标签颜色", "Tab color"))
                .color(Palette::TEXT_DIM)
                .size(11.0),
        );
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if color_swatch(ui, None, current.is_none(), crate::i18n::tr("无", "None")).clicked() {
                *pick = Some((uid, None));
                ui.close();
            }
            for c in TabColor::ALL {
                let (zh, en) = c.label();
                if color_swatch(
                    ui,
                    Some(c.rgb()),
                    current == Some(c),
                    crate::i18n::tr(zh, en),
                )
                .clicked()
                {
                    *pick = Some((uid, Some(c)));
                    ui.close();
                }
            }
        });
    });
}

/// 一颗颜色点。`fill = None` 是「无」：空心加一道斜线。当前选中的外面再套一圈。
fn color_swatch(
    ui: &mut egui::Ui,
    fill: Option<egui::Color32>,
    on: bool,
    tip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(20.0, 22.0), Sense::click());
    let center = rect.center();
    let p = ui.painter();
    let r = if resp.hovered() { 7.2 } else { 6.4 };
    if let Some(c) = fill {
        p.circle_filled(center, r, c);
    } else {
        p.circle_stroke(center, r, egui::Stroke::new(1.2, Palette::TEXT_DIM));
        let d = r * 0.65;
        p.line_segment(
            [center + egui::vec2(-d, d), center + egui::vec2(d, -d)],
            egui::Stroke::new(1.2, Palette::TEXT_DIM),
        );
    }
    if on {
        p.circle_stroke(center, r + 2.6, egui::Stroke::new(1.4, Palette::TEXT));
    }
    resp.on_hover_text(tip)
}

/// 连接状态圆点。`grouped` 为 `(正在看 AI 开的窗口, 当前时间)`：
/// 看自己的窗口时圆心是绿的、外圈黄色呼吸；看 AI 窗口时反过来。
fn paint_status_dot(
    p: &egui::Painter,
    center: egui::Pos2,
    plain: egui::Color32,
    grouped: Option<(bool, f64)>,
) {
    let Some((viewing_child, time)) = grouped else {
        p.circle_filled(center, 4.0, plain);
        return;
    };
    let (core, ring) = if viewing_child {
        (Palette::WARN, Palette::OK)
    } else {
        (Palette::OK, Palette::WARN)
    };
    let x = ((time * std::f64::consts::TAU) / 1.4).sin() as f32;
    // 0..1。只让外圈的透明度呼吸，圆心大小与普通状态点一致，不再描一圈实边。
    let breath = 0.5 + 0.5 * x;
    let [r, g, b, _] = ring.to_array();
    // 两层实心圆叠出软边，外缘停在半径 6（普通圆点是 4），没有描边。
    for (radius, alpha) in [(6.0, 16.0 + 36.0 * breath), (5.1, 28.0 + 64.0 * breath)] {
        p.circle_filled(
            center,
            radius,
            egui::Color32::from_rgba_unmultiplied(r, g, b, alpha as u8),
        );
    }
    p.circle_filled(center, 4.0, core);
}

/// 标签页显示标题：`items` 为每个会话的 `(标题, 远端主机)`（本机/无主机信息为 `None`），
/// 返回 `(显示标题, 是否与其他会话同名)`。
///
/// 不撞车的标签保持原标题、零额外噪声。同名的一组里，只有主机真的不同（至少两种不同的
/// 远端主机）才给远端会话追加「· 主机」——同一连接开两个时主机后缀完全相同，加了也分不开，
/// 只是噪声。#uid 这类内部编号不上标签：同名会话靠 hover 提示区分（见 `tab_hover_text`）。
fn tab_labels<'a>(items: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> Vec<(String, bool)> {
    use std::collections::{HashMap, HashSet};
    let items: Vec<_> = items.collect();
    let mut groups: HashMap<&str, (usize, HashSet<&str>)> = HashMap::new();
    for &(title, host) in &items {
        let g = groups.entry(title).or_default();
        g.0 += 1;
        if let Some(h) = host {
            g.1.insert(h);
        }
    }
    items
        .iter()
        .map(|&(title, host)| {
            let (count, hosts) = &groups[title];
            let dup = *count > 1;
            let label = match host {
                Some(h) if dup && hosts.len() > 1 => format!("{title} · {h}"),
                _ => title.to_string(),
            };
            (label, dup)
        })
        .collect()
}

/// 标签 hover 提示：`user@host:port`（本机为「本机 · 用户」），同名会话在其后追加 `· #uid`
/// 以便区分，AI 开的会话再追加开启来源。
fn tab_hover_text(base: &str, dup_uid: Option<u64>, ai_owner_label: Option<&str>) -> String {
    let mut tip = base.to_string();
    if let Some(uid) = dup_uid {
        tip.push_str(&format!(" · #{uid}"));
    }
    if let Some(label) = ai_owner_label {
        tip.push_str(&format!(
            " · {} {label}",
            crate::i18n::tr("AI 开启，来源", "AI-opened by")
        ));
    }
    tip
}

#[cfg(test)]
mod tab_label_tests {
    use super::{tab_hover_text, tab_labels};

    fn labels(items: &[(&'static str, Option<&'static str>)]) -> Vec<(String, bool)> {
        tab_labels(items.iter().copied())
    }

    /// 同名会话的标签上绝不出现 #uid（本机、同主机都一样），且都标记 dup 让 hover 带上 #uid。
    /// 这是钉子不是门禁：`tab_labels` 的入参里根本没有 uid，现实现下它不可能失败；留着是防
    /// 将来有人把 uid 传进来、又拼回标签上。
    #[test]
    fn duplicate_titles_never_show_uid_on_tab() {
        for got in [
            labels(&[("web", Some("10.0.0.1")), ("web", Some("10.0.0.1"))]),
            labels(&[("本机", None), ("本机", None)]),
        ] {
            for (label, dup) in got {
                assert!(dup, "同名会话要标记 dup，hover 才会带 #uid");
                assert!(!label.contains('#'), "标签不该出现 #uid：{label}");
            }
        }
    }

    /// 反向对照：把 `hosts.len() > 1` 改成 `>= 1`（同主机也加后缀），第二个断言当场挂。
    #[test]
    fn host_suffix_only_when_hosts_differ() {
        assert_eq!(
            labels(&[
                ("web", Some("10.0.0.1")),
                ("web", Some("10.0.0.2")),
                ("db", Some("10.0.0.1"))
            ]),
            vec![
                ("web · 10.0.0.1".to_string(), true),
                ("web · 10.0.0.2".to_string(), true),
                ("db".to_string(), false),
            ]
        );
        assert_eq!(
            labels(&[("web", Some("10.0.0.1")), ("web", Some("10.0.0.1"))]),
            vec![("web".to_string(), true), ("web".to_string(), true)],
            "同一连接开两个：主机后缀分不开，不加"
        );
    }

    #[test]
    fn hover_puts_uid_right_after_user_at_host() {
        assert_eq!(
            tab_hover_text("root@10.0.0.1:22", Some(3), None),
            "root@10.0.0.1:22 · #3"
        );
        assert_eq!(
            tab_hover_text("root@10.0.0.1:22", None, None),
            "root@10.0.0.1:22"
        );
        assert!(tab_hover_text("root@10.0.0.1:22", Some(3), Some("e5-1"))
            .starts_with("root@10.0.0.1:22 · #3 · "));
    }
}
