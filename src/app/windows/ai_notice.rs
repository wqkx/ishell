//! 终端通知浮层（右上角）：AI CLI 响铃 / OSC 9/777 通知。
//!
//! 多条通知自右上角向下纵排（层级见 `Order::Middle` 处的说明）。
//! 左键点击：跳转到来源会话并移除该条；右键点击：浮层菜单（删除此条 / 删除全部）。

use egui::RichText;

use crate::theme::Palette;

use super::super::App;

/// 可见条目的下标区间：新通知追加在**尾部**，所以超出容量时要丢掉最旧的、留最新的
/// `max` 条。写成 `0..n.min(max)` 会正好取反——一旦攒满 max 条，后来的每一条都永远
/// 画不出来，只有「+N」计数在涨（0.17.0 就是这么错的）。
fn visible_range(n: usize, max: usize) -> std::ops::Range<usize> {
    n.saturating_sub(max)..n
}

impl App {
    pub(in crate::app) fn ai_notice_overlay(&mut self, ctx: &egui::Context) {
        if self.ai_notices.is_empty() {
            return;
        }
        const MAX_VISIBLE: usize = 6;
        /// 卡片宽度：够放「会话名 + 两行提示」即可，不占掉右上角一大块。
        const CARD_W: f32 = 300.0;
        /// 常态不透明度：通知是被动提示，压在终端上方长期发亮会碍眼；鼠标移上去才转不透明。
        /// 调得很淡是刻意的——它只需要用余光能察觉「有事」，真要看内容时鼠标移上去即可。
        const IDLE_ALPHA: f32 = 0.40;
        let mut jump_to: Option<u64> = None;
        let mut remove_one: Option<usize> = None;
        let mut clear_all = false;

        // 摆在**终端内容区之内**，而不是贴着窗口边。矩形由 layout_body 每帧记下
        // （CentralPanel 的 `ui.max_rect()`，已扣掉它自己的内外边距）。没画终端时
        // （欢迎页）退回窗口内容区。
        //
        // 纵向留得比横向多：上方紧挨着的是标签栏和它右端那排按钮（新建/传输/设置等），
        // 卡片贴太高就会压在它们身上——那些按钮在终端区之外，本来就不该被浮层盖住。
        const GAP_X: f32 = 12.0;
        const GAP_Y: f32 = 24.0;
        let content = ctx.content_rect();
        let area = self.term_rect.unwrap_or(content);
        // Area::anchor 的偏移是相对窗口内容区的，把目标位置换算成相对其右上角的偏移。
        let dx = (area.right() - GAP_X) - content.right();
        let dy = (area.top() + GAP_Y) - content.top();
        egui::Area::new(egui::Id::new("ai_notice_overlay"))
            .anchor(egui::Align2::RIGHT_TOP, [dx, dy])
            // 用 Middle（窗口的默认层）而不是 Foreground：右上角的传输浮窗等窗口也在这一层，
            // 但它们在本浮层**之后**创建、且点击会被提到最前，于是永远压在通知之上——
            // 那正是想要的：通知是被动提示，不该盖住用户正在操作的窗口。
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                ui.set_max_width(CARD_W);
                let n = self.ai_notices.len();
                // 最新在上：取**末尾** MAX_VISIBLE 条后倒序画（新的追加在尾部）；
                // 被丢掉的是最旧的那些，正好对应下方的「+N」。
                for i in visible_range(n, MAX_VISIBLE).rev() {
                    let no = &self.ai_notices[i];
                    // 半透明常态 + 悬停迅速转不透明。悬停状态取自**上一帧**（本帧的
                    // response 要等画完才有），配上 0.12s 的过渡，这一帧的滞后看不出来。
                    let anim_id = egui::Id::new(("ai_notice_hover", no.session_uid));
                    let was_hovered = ui.data(|d| d.get_temp::<bool>(anim_id).unwrap_or(false));
                    let t = ui.ctx().animate_bool_with_time(anim_id, was_hovered, 0.12);
                    ui.set_opacity(IDLE_ALPHA + (1.0 - IDLE_ALPHA) * t);
                    // 用与「传输浮窗」同一套窗口外观（阴影 + 描边 + 圆角来自全局 style），
                    // 只把底色提到 PANEL——它比终端底色亮，卡片因此"浮"在终端之上。
                    // 原先是 PANEL_2 + 一整圈 ACCENT 描边：那是 toast 的用法（转瞬即逝、
                    // 需要抢眼），常驻卡片套上就显得吵，橙色留给铃铛图标做点缀即可。
                    let frame = egui::Frame::window(&ctx.global_style())
                        .fill(Palette::PANEL)
                        .corner_radius(crate::theme::R_SM)
                        .inner_margin(egui::Margin::symmetric(9, 7))
                        .show(ui, |ui| {
                            ui.set_width(CARD_W - 18.0);
                            // 行距收紧：两行文字之间不需要控件之间那么宽的默认间距，
                            // 但外面的 inner_margin 要留着，否则字贴着边框很难看。
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new(egui_phosphor::regular::BELL)
                                        .color(Palette::ACCENT)
                                        .size(13.0),
                                );
                                ui.label(
                                    RichText::new(&no.session_title)
                                        .color(Palette::TEXT_DIM)
                                        .size(11.0),
                                );
                                // 关闭按钮压到最右
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
                                    },
                                );
                            });
                            // 正文另起一行、允许换行，但最多两行——通知里那句提示常常一行
                            // 放不下（"是否允许执行 xxx？1. Yes 2. No"这种），一行截断等于
                            // 把关键信息切掉了。超过两行才截断。
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&no.text).color(Palette::TEXT).size(12.0),
                                )
                                .wrap()
                                .truncate(),
                            );
                        });
                    // 记下本帧悬停状态，供下一帧的动画使用。
                    let hov = frame.response.hovered();
                    ui.data_mut(|d| d.insert_temp(anim_id, hov));
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

#[cfg(test)]
mod tests {
    use super::visible_range;

    /// 新通知追加在尾部，超出容量时必须丢最旧的、留最新的。
    #[test]
    fn keeps_newest_when_over_capacity() {
        assert_eq!(visible_range(0, 6), 0..0);
        assert_eq!(visible_range(3, 6), 0..3);
        assert_eq!(visible_range(6, 6), 0..6);
        // 攒满后又来两条：显示的必须是 2..8（最新六条），而不是 0..6（最旧六条）
        assert_eq!(visible_range(8, 6), 2..8);
        assert_eq!(visible_range(100, 6), 94..100);
    }
}
