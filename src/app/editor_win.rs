//! App 的编辑器独立窗口渲染与标签关闭确认（`impl App` 方法，行为不变）。

use egui::RichText;

use crate::theme::Palette;

use super::widgets::*;
use super::App;

impl App {
    /// 关闭活动标签前的二次确认（会话仍连接时）。
    pub(super) fn close_tab_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_close_tab else {
            return;
        };
        // 若该会话已不在，或已断开、不是 AI 会话、名下也没有未保存的编辑器标签，则无需确认
        let Some((title, ai_owned, connected, dirty)) = self.sessions.get(idx).map(|s| {
            let dirty = super::util::lock_mutex(&self.editor_state).dirty_tabs_for_session(s.uid);
            (s.title.clone(), s.ai_owned, s.connected, dirty)
        }) else {
            self.pending_close_tab = None;
            return;
        };
        if !super::session::session_close_needs_confirm(connected, ai_owned, dirty) {
            self.pending_close_tab = None;
            return;
        }
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("close_tab_modal")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(crate::i18n::tr("关闭会话", "Close session"))
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.label(if ai_owned {
                    match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!(
                            "「{title}」是 AI 正在使用的终端，关闭会立即终止这条连接——\
                             AI 之后对它的操作都会失败。确定关闭吗？"
                        ),
                        crate::i18n::Lang::En => format!(
                            "\"{title}\" is a terminal AI is currently using. Closing it \
                             immediately terminates that connection — any further AI action \
                             on it will fail. Close it?"
                        ),
                    }
                } else if connected {
                    match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("「{title}」仍在连接中，确定关闭吗？"),
                        crate::i18n::Lang::En => {
                            format!("\"{title}\" is still connected. Close it?")
                        }
                    }
                } else {
                    match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("确定关闭「{title}」吗？"),
                        crate::i18n::Lang::En => format!("Close \"{title}\"?"),
                    }
                });
                // 关会话会连同它打开的编辑器标签一起关掉：有没保存的修改必须说出来
                if dirty > 0 {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!(
                                "编辑器里有 {dirty} 个文件的修改尚未保存，关闭后这些修改会丢失。"
                            ),
                            crate::i18n::Lang::En => format!(
                                "{dirty} file(s) in the editor have unsaved changes that will be lost."
                            ),
                        })
                        .color(Palette::DANGER),
                    );
                }
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 72.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(
                    ui,
                    crate::i18n::tr("关闭", "Close"),
                    Some(Palette::DANGER),
                    bw,
                ) {
                    decision = Some(true);
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                self.close_session(idx);
                self.pending_close_tab = None;
            }
            Some(false) => self.pending_close_tab = None,
            None => {}
        }
    }
}
