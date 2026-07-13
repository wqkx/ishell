//! 主窗口布局：标签栏、欢迎页、右侧主体与截图驱动。从 App 拆出，行为不变。

use egui::RichText;

use crate::proto::UiCommand;
use crate::theme::Palette;

use super::App;

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
            .frame(
                egui::Frame::new()
                    .fill(Palette::ACCENT_SOFT)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => {
                                format!("{} 群发到 {} 个会话", icon::MEGAPHONE, targets)
                            }
                            crate::i18n::Lang::En => {
                                format!("{} Broadcast to {} session(s)", icon::MEGAPHONE, targets)
                            }
                        })
                        .color(Palette::TEXT)
                        .size(12.0),
                    );
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        self.show_broadcast = false;
                    }
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.broadcast_input)
                            .desired_width(ui.available_width() - 70.0)
                            .hint_text(crate::i18n::tr(
                                "输入命令，回车发送到所有已连接会话",
                                "Type a command; Enter sends to all connected sessions",
                            )),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                        resp.request_focus();
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(format!(
                                    "{} {}",
                                    icon::PAPER_PLANE_RIGHT,
                                    crate::i18n::tr("发送", "Send")
                                ))
                                .color(egui::Color32::WHITE),
                            )
                            .fill(Palette::ACCENT),
                        )
                        .clicked()
                    {
                        send = true;
                    }
                });
            });
        if send && !self.broadcast_input.trim().is_empty() {
            let mut bytes = self.broadcast_input.clone().into_bytes();
            bytes.push(b'\r'); // 用 CR（Enter）提交，与其它终端输入一致；\n 在多数行规程下不会执行命令
            // ai_owned 会话是只读的（AI 专用），广播不应该往里面灌用户输入
            for s in self.sessions.iter().filter(|s| s.connected && !s.ai_owned) {
                let _ = s.cmd_tx.send(UiCommand::TerminalInput(bytes.clone()));
            }
            self.broadcast_input.clear();
        }
    }

    pub(super) fn welcome(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(root, |ui| {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("iShell")
                        .size(40.0)
                        .strong()
                        .color(Palette::ACCENT),
                );
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "现代化 Rust SSH 客户端",
                        "A modern Rust SSH client",
                    ))
                    .size(16.0)
                    .color(Palette::TEXT_DIM),
                );
                ui.add_space(20.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!(
                                "{}  {}",
                                egui_phosphor::regular::PLUS,
                                crate::i18n::tr("新建连接", "New connection")
                            ))
                            .size(16.0)
                            .color(egui::Color32::WHITE),
                        )
                        .fill(Palette::ACCENT),
                    )
                    .clicked()
                {
                    self.connect_form.open_dialog();
                }
            });
        });
    }
}
