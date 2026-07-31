//! 终端通知浮层（右上角）：AI CLI 响铃 / OSC 9/777 通知。
//!
//! 多条通知自右上角向下纵排，浮于全部内容之上（Order::Foreground）。
//! 左键点击：跳转到来源会话并移除该条；右键点击：浮层菜单（删除此条 / 删除全部）。

use egui::RichText;

use crate::theme::Palette;

use super::super::App;

impl App {
    pub(in crate::app) fn ai_notice_overlay(&mut self, ctx: &egui::Context) {
        if self.ai_notices.is_empty() {
            return;
        }
        const MAX_VISIBLE: usize = 6;
        /// 卡片宽度：单行显示，够放「会话名·一句提示」即可，不再占掉右上角一大块。
        const CARD_W: f32 = 268.0;
        let mut jump_to: Option<u64> = None;
        let mut remove_one: Option<usize> = None;
        let mut clear_all = false;

        // 贴着**终端区**摆，而不是贴着窗口边。`available_rect` 是去掉各侧栏/标签栏之后
        // 剩给 CentralPanel 的那块，也就是终端所在的区域；终端内容还要再往里 12px
        // （CentralPanel 的 outer_margin 6 + inner_margin 6，见 layout_body），在此基础上
        // 再留一道 GAP，卡片就不会压在终端边框上。
        const TERM_INSET: f32 = 12.0;
        const GAP: f32 = 12.0;
        let area = ctx.available_rect();
        let screen = ctx.screen_rect();
        // Area::anchor 的偏移是相对**屏幕**的，所以把目标位置换算成相对屏幕右上角的偏移。
        let dx = (area.right() - TERM_INSET - GAP) - screen.right();
        let dy = (area.top() + TERM_INSET + GAP) - screen.top();
        egui::Area::new(egui::Id::new("ai_notice_overlay"))
            .anchor(egui::Align2::RIGHT_TOP, [dx, dy])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(CARD_W);
                let n = self.ai_notices.len();
                // 最新在上：倒序展示前 MAX_VISIBLE 条
                for i in (0..n.min(MAX_VISIBLE)).rev() {
                    let no = &self.ai_notices[i];
                    // 用与「传输浮窗」同一套窗口外观（阴影 + 描边 + 圆角来自全局 style），
                    // 只把底色提到 PANEL——它比终端底色亮，卡片因此"浮"在终端之上。
                    // 原先是 PANEL_2 + 一整圈 ACCENT 描边：那是 toast 的用法（转瞬即逝、
                    // 需要抢眼），常驻卡片套上就显得吵，橙色留给铃铛图标做点缀即可。
                    let frame = egui::Frame::window(&ctx.global_style())
                        .fill(Palette::PANEL)
                        .corner_radius(crate::theme::R_SM)
                        .inner_margin(egui::Margin::symmetric(9, 6))
                        .show(ui, |ui| {
                            ui.set_width(CARD_W - 18.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new(egui_phosphor::regular::BELL)
                                        .color(Palette::ACCENT)
                                        .size(13.0),
                                );
                                // 关闭按钮先摆到最右，剩下的宽度全留给正文——反过来的话
                                // 正文会把按钮挤出卡片（一行布局里先排的先占宽）。
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new(egui_phosphor::regular::X)
                                                        .size(11.0)
                                                        .color(Palette::TEXT_DIM),
                                                )
                                                .frame(false),
                                            )
                                            .clicked()
                                        {
                                            remove_one = Some(i);
                                        }
                                        ui.with_layout(
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(format!("{}·", no.session_title))
                                                        .color(Palette::TEXT_DIM)
                                                        .size(12.0),
                                                );
                                                // 超长正文省略成一行，不换行撑高卡片
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&no.text)
                                                            .color(Palette::TEXT)
                                                            .size(12.0),
                                                    )
                                                    .truncate(),
                                                );
                                            },
                                        );
                                    },
                                );
                            });
                        });
                    let resp = frame.response.interact(egui::Sense::click());
                    if resp.clicked() {
                        jump_to = Some(no.session_uid);
                        remove_one = Some(i);
                    }
                    resp.context_menu(|ui| {
                        ui.set_min_width(140.0);
                        if ui
                            .button(crate::i18n::tr("删除此通知", "Dismiss"))
                            .clicked()
                        {
                            remove_one = Some(i);
                            ui.close();
                        }
                        if ui
                            .button(crate::i18n::tr("删除全部通知", "Dismiss all"))
                            .clicked()
                        {
                            clear_all = true;
                            ui.close();
                        }
                    });
                    // 卡片自带阴影，间距给足一点才不显得糊在一起
                    ui.add_space(6.0);
                }
                if n > MAX_VISIBLE {
                    ui.label(
                        RichText::new(format!("+{}", n - MAX_VISIBLE))
                            .color(Palette::TEXT_DIM)
                            .size(12.0),
                    );
                }
            });

        if clear_all {
            self.ai_notices.clear();
        }
        if let Some(i) = remove_one {
            if i < self.ai_notices.len() {
                self.ai_notices.remove(i);
            }
        }
        if let Some(uid) = jump_to {
            if let Some(idx) = self.session_idx_by_uid(uid) {
                self.active = Some(idx);
                self.sessions[idx].terminal.request_focus();
            }
        }
    }
}
