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
                                                     // 先取出标量字段，避免在借用 self.sessions 的循环里再借 self
                            let dragging_tab = self.tabbar.drag;
                            let active = self.active;
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
                                    for (i, s) in self.sessions.iter().enumerate() {
                                        let selected = active == Some(i);
                                        // 宽度 = 左margin(9)+圆点(10)+间隔(6)+标题+间隔(6)+关闭(18)+右margin(9)
                                        let title_w = ctx.fonts_mut(|f| {
                                            f.layout_no_wrap(
                                                s.title.clone(),
                                                body_font.clone(),
                                                Palette::TEXT,
                                            )
                                            .rect
                                            .width()
                                        });
                                        let w = 58.0 + title_w;
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
                                        let dragging_this = drag_down && dragging_tab == Some(i);
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
                                        let resp = ui
                                            .interact(
                                                tab_rect,
                                                egui::Id::new(("tab", s.uid)),
                                                Sense::click_and_drag(),
                                            )
                                            .on_hover_text(s.tip.as_str());
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
                                        // 绘制
                                        let fill = if dragging_this {
                                            Palette::ACCENT_SOFT
                                        } else if selected {
                                            Palette::PANEL
                                        } else {
                                            egui::Color32::TRANSPARENT
                                        };
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
                                        // 激活标签底部 2px 珊瑚下划线（更清晰的激活指示）
                                        if selected && !dragging_this {
                                            let y = tab_rect.bottom() - 1.0;
                                            p.hline(
                                                tab_rect.left()..=tab_rect.right(),
                                                y,
                                                egui::Stroke::new(2.0, Palette::ACCENT),
                                            );
                                        }
                                        p.circle_filled(
                                            egui::pos2(tab_rect.left() + 14.0, tab_rect.center().y),
                                            4.0,
                                            if s.connected {
                                                Palette::OK
                                            } else {
                                                Palette::WARN
                                            },
                                        );
                                        let tcolor = if selected {
                                            Palette::TEXT
                                        } else {
                                            Palette::TEXT_DIM
                                        };
                                        p.text(
                                            egui::pos2(tab_rect.left() + 25.0, tab_rect.center().y),
                                            egui::Align2::LEFT_CENTER,
                                            &s.title,
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
                                        // 事件：关闭优先于激活
                                        if close_resp.clicked() {
                                            to_close = Some(i);
                                        } else if resp.clicked() {
                                            to_activate = Some(i);
                                        } else if resp.middle_clicked() {
                                            to_close = Some(i);
                                        }
                                        if resp.drag_started() {
                                            drag_start = Some(i);
                                            if let Some(pp) = pointer {
                                                new_grab = Some(pp.x - (origin.x + x));
                                            }
                                        }
                                        // 命中用「目标槽」位置，稳定判断拖到哪个槽
                                        tab_rects.push((
                                            i,
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
                        self.reorder_session(from, to);
                    }
                    if let Some(i) = to_activate {
                        self.active = Some(i);
                        self.tabbar.scroll_to_active = true; // 点击的标签若被遮挡则滚到可视区
                        if let Some(s) = self.sessions.get_mut(i) {
                            s.terminal.request_focus(); // 点击标签后焦点切到终端
                        }
                    }
                    if let Some(i) = to_close {
                        // 会话仍连接（活动）则弹确认，避免误关；否则直接关闭
                        if self.sessions.get(i).map(|s| s.connected).unwrap_or(false) {
                            self.pending_close_tab = Some(i);
                        } else {
                            self.close_session(i);
                        }
                    }
                });
            });
    }
}
