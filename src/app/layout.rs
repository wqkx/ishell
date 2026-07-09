//! 主窗口布局：标签栏、欢迎页、右侧主体与截图驱动。从 App 拆出，行为不变。

use egui::{RichText, Sense};

use crate::proto::UiCommand;
use crate::theme::Palette;
use crate::ui::file_panel::{self, FileAction};

use super::util::*;
use super::view_state::{files_collapsed, osc7_consent, set_osc7_consent, OSC7_SNIPPET};
use super::widgets::*;
use super::App;

impl App {
    /// 截图自检：到达指定帧请求截图，收到后写 PNG 并退出。
    pub(super) fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint(); // 保持持续渲染

        // 收到截图事件 -> 保存退出
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let [w, h] = [img.size[0] as u32, img.size[1] as u32];
            let mut buf = Vec::with_capacity((w * h * 4) as usize);
            for p in img.pixels.iter() {
                buf.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
            }
            if let Some(im) = image::RgbaImage::from_raw(w, h, buf) {
                let _ = im.save(&shot.path);
            }
            std::process::exit(0);
        }

        if std::time::Instant::now() >= shot.deadline && !shot.requested {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
    }
}

impl App {
    /// 命令广播栏：输入命令回车，发送到所有已连接会话。
    pub(super) fn broadcast_bar(&mut self, root: &mut egui::Ui) {
        use egui_phosphor::regular as icon;
        if !self.show_broadcast {
            return;
        }
        let targets = self.sessions.iter().filter(|s| s.connected).count();
        let mut send = false;
        egui::Panel::top("broadcast")
            .frame(egui::Frame::new().fill(Palette::ACCENT_SOFT).inner_margin(egui::Margin::symmetric(8, 5)))
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("{} 群发到 {} 个会话", icon::MEGAPHONE, targets), crate::i18n::Lang::En => format!("{} Broadcast to {} session(s)", icon::MEGAPHONE, targets) }).color(Palette::TEXT).size(12.0));
                    if ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                        self.show_broadcast = false;
                    }
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.broadcast_input)
                            .desired_width(ui.available_width() - 70.0)
                            .hint_text(crate::i18n::tr("输入命令，回车发送到所有已连接会话", "Type a command; Enter sends to all connected sessions")),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                        resp.request_focus();
                    }
                    if ui.add(egui::Button::new(RichText::new(format!("{} {}", icon::PAPER_PLANE_RIGHT, crate::i18n::tr("发送", "Send"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                        send = true;
                    }
                });
            });
        if send && !self.broadcast_input.trim().is_empty() {
            let mut bytes = self.broadcast_input.clone().into_bytes();
            bytes.push(b'\r'); // 用 CR（Enter）提交，与其它终端输入一致；\n 在多数行规程下不会执行命令
            for s in self.sessions.iter().filter(|s| s.connected) {
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(bytes.clone()));
            }
            self.broadcast_input.clear();
        }
    }

    pub(super) fn top_tabs(&mut self, root: &mut egui::Ui) {
        use egui_phosphor::regular as icon;
        egui::Panel::top("tabs")
            .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin::symmetric(6, 4)))
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
                            format!("{} {} {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("传输", "Xfer"), active_xfers)
                        } else {
                            format!("{} {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("传输", "Xfer"))
                        };
                        if flat_button(ui, &RichText::new(label), crate::i18n::tr("显示/隐藏传输进度", "Show/hide transfers")) {
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
                            format!("{} {} {}", icon::ARROWS_LEFT_RIGHT, crate::i18n::tr("转发", "Fwd"), nfwd)
                        } else {
                            format!("{} {}", icon::ARROWS_LEFT_RIGHT, crate::i18n::tr("转发", "Fwd"))
                        };
                        if flat_button(ui, &RichText::new(flabel), crate::i18n::tr("端口转发管理", "Port forwarding")) {
                            self.fwd.show = !self.fwd.show;
                            if self.fwd.show {
                                self.fwd.just_opened = true;
                            }
                        }
                        if flat_button(ui, &RichText::new(format!("{} {}", icon::MEGAPHONE, crate::i18n::tr("群发", "Bcast"))), crate::i18n::tr("向所有已连接会话广播命令", "Broadcast to all connected sessions")) {
                            self.show_broadcast = !self.show_broadcast;
                        }
                        if flat_button(ui, &RichText::new(format!("{} {}", icon::CODE, crate::i18n::tr("片段", "Snip"))), crate::i18n::tr("命令片段库：保存常用命令一键发送到终端", "Command snippets: save & send common commands")) {
                            self.snip.show = !self.snip.show;
                            if self.snip.show {
                                self.snip.just_opened = true;
                            }
                        }
                        // 折叠监控栏/文件栏的开关已移到左侧监控栏右键菜单，避免右上角按钮过多
                        // 分隔竖线：把标签区（标签 + 新建）与右侧功能区分开（短、低调，和谐配色）
                        {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(11.0, ui.available_height()), egui::Sense::hover());
                            let cy = rect.center().y;
                            ui.painter().vline(rect.center().x.round(), (cy - 8.0)..=(cy + 8.0), egui::Stroke::new(1.0, Palette::BORDER));
                        }
                        // 新建：固定在标签条右侧，标签溢出也不会被滚走
                        if flat_button(ui, &RichText::new(icon::PLUS).size(15.0), crate::i18n::tr("新建连接", "New connection")) {
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
                                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                                .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
                                .show(ui, |ui| {
                                    let tab_h = 26.0;
                                    let spacing = 4.0;
                                    // 预留区域撑出滚动内容宽度（用上一帧总宽）
                                    let (area, _) = ui.allocate_exact_size(egui::vec2(total_w_cache.max(1.0), tab_h), Sense::hover());
                                    let origin = area.min;
                                    let pointer = ui.input(|i| i.pointer.interact_pos());
                                    let drag_down = ui.input(|i| i.pointer.any_down());
                                    let ctx = ui.ctx().clone();
                                    let body_font = egui::TextStyle::Body.resolve(ui.style());
                                    let mut acc = 0.0f32; // 目标布局累计左边界
                                    for (i, s) in self.sessions.iter().enumerate() {
                                        let selected = active == Some(i);
                                        // 宽度 = 左margin(9)+圆点(10)+间隔(6)+标题+间隔(6)+关闭(18)+右margin(9)
                                        let title_w = ctx.fonts_mut(|f| f.layout_no_wrap(s.title.clone(), body_font.clone(), Palette::TEXT).rect.width());
                                        let w = 58.0 + title_w;
                                        let target = acc;
                                        // 激活标签若被请求滚动到可视区：按其「目标槽」位置请求滚动（带边距余量）
                                        if selected && want_scroll {
                                            let r = egui::Rect::from_min_size(egui::pos2(origin.x + target, origin.y), egui::vec2(w, tab_h));
                                            ui.scroll_to_rect(r.expand2(egui::vec2(12.0, 0.0)), None);
                                        }
                                        let id = egui::Id::new(("tabx", s.uid));
                                        let dragging_this = drag_down && dragging_tab == Some(i);
                                        if dragging_this {
                                            drag_w = w;
                                        }
                                        let x = if dragging_this {
                                            let want = pointer.map(|p| p.x - origin.x - grab_dx).unwrap_or(target);
                                            ctx.animate_value_with_time(id, want, 0.0) // 跟手
                                        } else {
                                            ctx.animate_value_with_time(id, target, 0.14) // 缓动到目标槽
                                        };
                                        let tab_rect = egui::Rect::from_min_size(egui::pos2(origin.x + x, origin.y), egui::vec2(w, tab_h));
                                        // 交互：整张标签可点击（激活）/拖动（排序）；关闭区在上层优先
                                        let resp = ui.interact(tab_rect, egui::Id::new(("tab", s.uid)), Sense::click_and_drag()).on_hover_text(s.tip.as_str());
                                        let close_rect = egui::Rect::from_center_size(egui::pos2(tab_rect.right() - 18.0, tab_rect.center().y), egui::vec2(18.0, 18.0));
                                        let close_resp = ui.interact(close_rect, egui::Id::new(("tabclose", s.uid)), Sense::click());
                                        // 绘制
                                        let fill = if dragging_this { Palette::ACCENT_SOFT } else if selected { Palette::PANEL } else { egui::Color32::TRANSPARENT };
                                        let p = ui.painter();
                                        p.rect_filled(tab_rect, egui::CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, fill);
                                        // 激活标签底部 2px 珊瑚下划线（更清晰的激活指示）
                                        if selected && !dragging_this {
                                            let y = tab_rect.bottom() - 1.0;
                                            p.hline(tab_rect.left()..=tab_rect.right(), y, egui::Stroke::new(2.0, Palette::ACCENT));
                                        }
                                        p.circle_filled(egui::pos2(tab_rect.left() + 14.0, tab_rect.center().y), 4.0, if s.connected { Palette::OK } else { Palette::WARN });
                                        let tcolor = if selected { Palette::TEXT } else { Palette::TEXT_DIM };
                                        p.text(egui::pos2(tab_rect.left() + 25.0, tab_rect.center().y), egui::Align2::LEFT_CENTER, &s.title, body_font.clone(), tcolor);
                                        let xcolor = if close_resp.hovered() { Palette::DANGER } else { Palette::TEXT_DIM };
                                        p.text(close_rect.center(), egui::Align2::CENTER_CENTER, icon::X, egui::FontId::proportional(12.0), xcolor);
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
                                        tab_rects.push((i, egui::Rect::from_min_size(egui::pos2(origin.x + target, origin.y), egui::vec2(w, tab_h))));
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
                                        let drag_center = pos.x - self.tabbar.grab_dx + drag_w / 2.0;
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

    pub(super) fn welcome(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(root, |ui| {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("iShell").size(40.0).strong().color(Palette::ACCENT));
                ui.label(
                    RichText::new(crate::i18n::tr("现代化 Rust SSH 客户端", "A modern Rust SSH client"))
                        .size(16.0)
                        .color(Palette::TEXT_DIM),
                );
                ui.add_space(20.0);
                if ui
                    .add(egui::Button::new(RichText::new(format!("{}  {}", egui_phosphor::regular::PLUS, crate::i18n::tr("新建连接", "New connection"))).size(16.0).color(egui::Color32::WHITE)).fill(Palette::ACCENT))
                    .clicked()
                {
                    self.connect_form.open_dialog();
                }
            });
        });
    }

    pub(super) fn right_body(&mut self, root: &mut egui::Ui, idx: usize) {
        // 右下文件操作区（可拖动调整高度）
        let mut file_actions: Vec<FileAction> = Vec::new();
        let has_clip = self.xfer.file_clip.is_some();
        if !files_collapsed() {
            egui::Panel::bottom("files")
                .resizable(true)
                .default_size(250.0)
                .size_range(120.0..=640.0)
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL)
                        .inner_margin(8)
                        .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 6 }),
                )
                .show_inside(root, |ui| {
                    file_actions = file_panel::show(ui, &mut self.sessions[idx].files, has_clip);
                });
            for a in file_actions {
                self.handle_file_action(idx, a);
            }
        }

        // 中间终端区（四周留空隙，与其他区域分开）。
        // 6px 内边距（边框）用「窗口暖米」与「当前终端主题底色」的中间色（固定色，非渐变），
        // 让窗口与 shell 之间过渡柔和、不再是生硬的一圈暖米。
        let mut reconnect_click = false;
        let tbg = crate::terminal::current_bg();
        // 浅色终端（经典浅/近白/暖米）边框直接用终端底色，与 shell 一致、无缝；
        // 深色终端用偏向终端的混合色，略留层次。
        let term_border = if tbg.r() as u32 + tbg.g() as u32 + tbg.b() as u32 > 450 {
            tbg
        } else {
            blend_color(Palette::TERM_BG, tbg)
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(term_border)
                    .inner_margin(6)
                    .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 0 }),
            )
            .show_inside(root, |ui| {
                let s = &mut self.sessions[idx];
                // 断线提示条 + 手动重连（初次"连接中"不显示）
                if !s.connected {
                    egui::Frame::new()
                        .fill(Palette::ACCENT_SOFT)
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::WARNING, s.status)).color(Palette::DANGER).size(12.0));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", egui_phosphor::regular::ARROW_CLOCKWISE, crate::i18n::tr("重连", "Reconnect"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                        reconnect_click = true;
                                    }
                                });
                            });
                        });
                    ui.add_space(4.0);
                }
                let input = s.terminal.ui(ui);
                if !input.is_empty() {
                    let _ = s.cmd_tx.send(UiCommand::TerminalInput(input));
                }
                // 右键菜单「在文件列表中显示当前目录」：把文件区导航到终端当前目录
                if let Some(cwd) = s.terminal.take_reveal_cwd() {
                    s.files.cwd = cwd;
                    s.files.selected.clear();
                }
                // 无 cwd 时点该菜单：已同意过则静默注入（吞掉命令回显）；否则弹确认框（同意后记住）
                if s.terminal.take_inject_request() {
                    if osc7_consent() {
                        let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("{OSC7_SNIPPET}\r").into_bytes()));
                        s.terminal.expect_echo(OSC7_SNIPPET);
                        s.osc7_pending_reveal = true;
                    } else {
                        s.osc7_confirm = true;
                    }
                }
                if s.osc7_confirm {
                    let mut decided: Option<bool> = None;
                    egui::Modal::new(egui::Id::new("osc7_confirm_modal")).show(ui.ctx(), |ui| {
                        ui.set_width(370.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(crate::i18n::tr("获取终端当前目录", "Track terminal directory")).size(16.0).strong());
                            ui.add_space(6.0);
                            ui.label(crate::i18n::tr(
                                "需向当前 shell 注入一行命令以上报工作目录（仅本会话、不写配置文件）。同意后将记住，后续自动静默注入。",
                                "Inject one line into the current shell to report its directory (this session only, not written to config). Remembered after you agree.",
                            ));
                        });
                        ui.add_space(12.0);
                        let bw = 110.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                            if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("同意并注入", "Agree & inject")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                                decided = Some(true);
                            }
                            if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                                decided = Some(false);
                            }
                        });
                    });
                    match decided {
                        Some(true) => {
                            set_osc7_consent(true);
                            let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("{OSC7_SNIPPET}\r").into_bytes()));
                            s.terminal.expect_echo(OSC7_SNIPPET);
                            s.osc7_pending_reveal = true;
                            s.osc7_confirm = false;
                        }
                        Some(false) => s.osc7_confirm = false,
                        None => {}
                    }
                }
                // 注入后：下个提示符上报 cwd 时把文件区跳过去
                if s.osc7_pending_reveal {
                    if let Some(cwd) = s.terminal.cwd() {
                        s.files.cwd = cwd.to_string();
                        s.files.selected.clear();
                        s.osc7_pending_reveal = false;
                    }
                }
                let size = s.terminal.size();
                if size != s.last_size && s.connected {
                    s.last_size = size;
                    let _ = s.cmd_tx.send(UiCommand::Resize { cols: size.0, rows: size.1 });
                }
            });
        if reconnect_click {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.reconnect_tries = 0;
            }
            self.reconnect_session(idx);
        }
    }
}










