//! App 的模态对话框（主机密钥 / 键盘交互认证 / 粘贴确认 / 片段库 / 关闭确认）。
//! 均为 `impl App` 方法，签名与调用点不变；从 God Object 物理拆出。

use egui::{RichText, Sense};

use crate::proto::UiCommand;
use crate::theme::Palette;

use super::widgets::*;
use super::App;

impl App {
    /// 未知主机首次连接：确认指纹（TOFU），同意则 worker 写入 known_hosts。
    pub(super) fn host_key_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self
            .sessions
            .iter()
            .position(|s| s.pending_hostkey.is_some())
        else {
            return;
        };
        let (host, fp, changed) = self.sessions[idx].pending_hostkey.clone().unwrap();
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("hostkey_modal"))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                if changed {
                    // 主机密钥变更：更醒目的红色警告 + 在 UI 内替换 known_hosts
                    ui.label(RichText::new(crate::i18n::tr("⚠ 主机密钥已变更", "⚠ Host key changed")).size(16.0).strong().color(Palette::DANGER));
                    ui.add_space(8.0);
                    ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("主机：{host}"), crate::i18n::Lang::En => format!("Host: {host}") });
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("新指纹 (SHA256)：", "New fingerprint (SHA256):")).color(Palette::TEXT_DIM).size(12.0));
                    ui.label(RichText::new(&fp).monospace());
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("known_hosts 中记录的密钥与服务器当前不符。若非你主动更换了服务器密钥，可能是中间人攻击！接受将删除旧密钥并写入新密钥。", "The recorded key differs from the server's. If you didn't rotate the key, this could be a MITM attack! Accepting removes the old key and stores the new one.")).color(Palette::DANGER).size(11.0));
                } else {
                    ui.label(RichText::new(crate::i18n::tr("未知主机", "Unknown host")).size(16.0).strong());
                    ui.add_space(8.0);
                    ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("首次连接主机：{host}"), crate::i18n::Lang::En => format!("First connect: {host}") });
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("指纹 (SHA256)：", "Fingerprint (SHA256):")).color(Palette::TEXT_DIM).size(12.0));
                    ui.label(RichText::new(&fp).monospace());
                    ui.add_space(6.0);
                    ui.label(RichText::new(crate::i18n::tr("请确认该指纹与目标服务器一致；信任后将写入 ~/.ssh/known_hosts。", "Verify the fingerprint matches the server; trusting writes to ~/.ssh/known_hosts.")).color(Palette::TEXT_DIM).size(11.0));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 120.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    let (accept_label, accept_col) = if changed {
                        (crate::i18n::tr("删除旧密钥并信任", "Replace & trust"), Palette::DANGER)
                    } else {
                        (crate::i18n::tr("信任并连接", "Trust & connect"), Palette::ACCENT)
                    };
                    if dialog_button(ui, accept_label, Some(accept_col), bw) {
                        decision = Some(true);
                    }
                    if dialog_button(ui, crate::i18n::tr("拒绝", "Reject"), None, bw) {
                        decision = Some(false);
                    }
                });
            });
        if let Some(d) = decision {
            let s = &mut self.sessions[idx];
            let _ = s.hostkey_tx.send(d);
            s.pending_hostkey = None;
        }
        // 模态异常关闭时由 worker 侧 HOSTKEY_DECISION_TIMEOUT 兜底拒绝
    }

    /// 键盘交互认证：弹窗逐项收集回答，提交后经 cmd_tx 回 `KbdResponse`；取消则断开。
    pub(super) fn kbd_prompt_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.sessions.iter().position(|s| s.kbd_prompt.is_some()) else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("kbd_modal")).show(ctx, |ui| {
            ui.set_width(360.0);
            let s = &mut self.sessions[idx];
            let kp = s.kbd_prompt.as_mut().unwrap();
            let title = if kp.name.trim().is_empty() {
                crate::i18n::tr("二次验证", "Verification").to_string()
            } else {
                kp.name.clone()
            };
            ui.label(RichText::new(title).size(16.0).strong());
            if !kp.instructions.trim().is_empty() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&kp.instructions)
                        .color(Palette::TEXT_DIM)
                        .size(12.0),
                );
            }
            ui.add_space(8.0);
            egui::Grid::new("kbd_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    for (i, (prompt, echo)) in kp.prompts.iter().enumerate() {
                        ui.label(prompt);
                        // echo=false 的提示（如密码/验证码）做遮蔽
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut kp.answers[i])
                                .desired_width(200.0)
                                .password(!echo),
                        );
                        // 打开即聚焦第一个输入框，可直接输入验证码
                        if i == 0 && ui.memory(|m| m.focused().is_none()) {
                            r.request_focus();
                        }
                        ui.end_row();
                    }
                });
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 96.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(
                    ui,
                    crate::i18n::tr("提交", "Submit"),
                    Some(Palette::ACCENT),
                    bw,
                ) {
                    submit = true;
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    cancel = true;
                }
            });
        });
        if submit {
            let s = &mut self.sessions[idx];
            if let Some(kp) = s.kbd_prompt.take() {
                let _ = s.cmd_tx.send(UiCommand::KbdResponse(kp.answers));
            }
        } else if cancel {
            let s = &mut self.sessions[idx];
            s.kbd_prompt = None;
            let _ = s.cmd_tx.send(UiCommand::Disconnect);
        }
    }

    /// 粘贴二次确认：剪切（移动/删源）或跨服务器（重操作）执行前弹窗确认。
    pub(super) fn paste_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.xfer.pending_paste.as_ref() else {
            return;
        };
        let mut go = false;
        let mut cancel = false;
        // 互斥选择的本地镜像（plan 已不可变借用 self，不能再借 self.xfer.confirm_direct）
        let mut direct = self.xfer.confirm_direct;
        let cross = plan.cross;
        egui::Modal::new(egui::Id::new("paste_confirm")).show(ctx, |ui| {
            dialog_body(ui, |ui| {
                let n = plan.items.len();
                let title = match (plan.is_cut, crate::i18n::current()) {
                    (true, crate::i18n::Lang::Zh) => format!("确认移动 {n} 项？"),
                    (false, crate::i18n::Lang::Zh) => format!("确认复制 {n} 项？"),
                    (true, crate::i18n::Lang::En) => format!("Move {n} item(s)?"),
                    (false, crate::i18n::Lang::En) => format!("Copy {n} item(s)?"),
                };
                ui.label(RichText::new(title).size(16.0).strong());
                ui.add_space(8.0);
                // 源主机 + 源目录
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::i18n::tr("源", "From")).size(11.0).color(Palette::TEXT_DIM));
                    ui.label(RichText::new(&plan.src_label).size(12.0).strong().color(Palette::TEXT));
                });
                ui.label(RichText::new(&plan.src_dir).monospace().size(11.0).color(Palette::TEXT_DIM));
                ui.add_space(4.0);
                // 目标主机 + 粘贴目录
                ui.horizontal(|ui| {
                    ui.label(RichText::new(crate::i18n::tr("目标", "To")).size(11.0).color(Palette::TEXT_DIM));
                    ui.label(RichText::new(&plan.dest_label).size(12.0).strong().color(Palette::TEXT));
                });
                ui.label(RichText::new(&plan.dest_dir).monospace().size(11.0).color(Palette::TEXT_DIM));
                if plan.cross {
                    ui.add_space(8.0);
                    // 「直传 / 中转」互斥选择（默认直传）
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("方式", "Method")).size(11.0).color(Palette::TEXT_DIM));
                        ui.selectable_value(&mut direct, true, RichText::new(crate::i18n::tr("直传", "Direct")).size(12.0));
                        ui.selectable_value(&mut direct, false, RichText::new(crate::i18n::tr("中转", "Relay")).size(12.0));
                    });
                    if direct {
                        ui.label(RichText::new(crate::i18n::tr("源主机直推目标，数据不经本地（需目标会话为「无口令密钥」认证）。", "Source pushes straight to target, bypassing local (target must use a passphrase-less key).")).color(Palette::TEXT_DIM).size(11.0));
                        // 安全警示：直传需把目标服务器的私钥临时投放到源服务器，源服务器 root/
                        // 同用户进程/被入侵时都可能读取该私钥。默认走中转即为规避此风险。
                        ui.label(RichText::new(crate::i18n::tr(
                            "⚠ 安全：直传会把目标服务器私钥临时上传到源服务器，源服务器可读取该私钥。仅在完全信任源服务器时使用。",
                            "⚠ Security: direct mode uploads the target's private key to the source server, which can read it. Use only if you fully trust the source server.",
                        )).color(Palette::DANGER).size(11.0));
                    } else {
                        ui.label(RichText::new(crate::i18n::tr("经本地「下载→上传」中转，最通用、最安全，大文件较慢。", "Relayed via local download→upload; most compatible & safest, slower for large files.")).color(Palette::TEXT_DIM).size(11.0));
                    }
                }
                if plan.is_cut {
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("剪切为移动：复制成功后会从源删除，不可恢复。", "Cut = move: source is deleted after a successful copy. Irreversible.")).color(Palette::DANGER).size(11.0));
                }
                // 列出名称（最多 8 个）
                ui.add_space(6.0);
                let shown: Vec<String> = plan.items.iter().take(8).map(|(p, _)| p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p).to_string()).collect();
                let more = if n > 8 { format!(" … (+{})", n - 8) } else { String::new() };
                ui.label(RichText::new(format!("{}{}", shown.join("、"), more)).color(Palette::TEXT_DIM).size(11.0));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 96.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    let confirm_label = if plan.is_cut { crate::i18n::tr("移动", "Move") } else { crate::i18n::tr("复制", "Copy") };
                    let confirm_col = if plan.is_cut { Palette::DANGER } else { Palette::ACCENT };
                    if dialog_button(ui, confirm_label, Some(confirm_col), bw) {
                        go = true;
                    }
                    if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                        cancel = true;
                    }
                });
            });
        });
        // 记住本帧的互斥选择（跨帧保持，直到下次打开确认复位为直传）
        if cross {
            self.xfer.confirm_direct = direct;
        }
        if go {
            if let Some(mut plan) = self.xfer.pending_paste.take() {
                if plan.cross {
                    plan.direct = direct; // 直传 / 中转 取自弹框里的互斥选择
                }
                self.execute_paste(plan);
            }
        } else if cancel {
            self.xfer.pending_paste = None;
        }
    }

    /// 命令片段库：列出片段（一键发送到活动会话终端）+ 新增/编辑/删除，落盘持久化。
    pub(super) fn snippets_window(&mut self, ctx: &egui::Context) {
        if !self.snip.show {
            return;
        }
        use egui_phosphor::regular as icon;
        let mut send_cmd: Option<(String, bool)> = None;
        let mut edit: Option<usize> = None;
        let mut delete: Option<usize> = None;
        let mut save_now = false;
        let mut close_win = false;
        let mut changed = false;
        // 与「传输 / 转发」面板同款：右上角锚定、无标题栏、PANEL 底色的紧凑浮窗
        let win = egui::Window::new("snippet_win")
            .title_bar(false)
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(340.0)
            .resizable(false)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(10),
            )
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{}  {}",
                            icon::CODE,
                            crate::i18n::tr("命令片段", "Snippets")
                        ))
                        .strong()
                        .size(13.0)
                        .color(Palette::TEXT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            close_win = true;
                        }
                    });
                });
                ui.separator();

                if self.snip.list.is_empty() {
                    ui.add_space(4.0);
                    crate::ui::empty_state(
                        ui,
                        egui_phosphor::regular::CODE,
                        crate::i18n::tr("暂无片段，在下方新增", "No snippets; add one below"),
                        false,
                    );
                }
                // 列表：点名称即发送到当前会话终端；右侧编辑 / 删除（无边框图标，风格统一）
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, sn) in self.snip.list.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(icon::PAPER_PLANE_TILT)
                                        .color(Palette::ACCENT)
                                        .size(13.0),
                                );
                                let label = if sn.name.trim().is_empty() {
                                    sn.command.clone()
                                } else {
                                    sn.name.clone()
                                };
                                if ui
                                    .add(
                                        egui::Label::new(
                                            RichText::new(label).size(12.0).color(Palette::TEXT),
                                        )
                                        .sense(Sense::click()),
                                    )
                                    .on_hover_text(match crate::i18n::current() {
                                        crate::i18n::Lang::Zh => format!("发送：{}", sn.command),
                                        crate::i18n::Lang::En => format!("Send: {}", sn.command),
                                    })
                                    .clicked()
                                {
                                    send_cmd = Some((sn.command.clone(), sn.run));
                                }
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
                                            .on_hover_text(crate::i18n::tr("删除", "Delete"))
                                            .clicked()
                                        {
                                            delete = Some(i);
                                        }
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new(icon::PENCIL_SIMPLE)
                                                        .size(12.0)
                                                        .color(Palette::TEXT_DIM),
                                                )
                                                .frame(false),
                                            )
                                            .on_hover_text(crate::i18n::tr("编辑", "Edit"))
                                            .clicked()
                                        {
                                            edit = Some(i);
                                        }
                                    },
                                );
                            });
                            // 有名称时在下方以等宽小字补充命令原文
                            if !sn.name.trim().is_empty() {
                                ui.label(
                                    RichText::new(&sn.command)
                                        .monospace()
                                        .size(10.5)
                                        .color(Palette::TEXT_DIM),
                                );
                            }
                            ui.add_space(3.0);
                        }
                    });

                ui.separator();
                let editing = self.snip.editing.is_some();
                ui.label(
                    RichText::new(if editing {
                        crate::i18n::tr("编辑片段", "Edit snippet")
                    } else {
                        crate::i18n::tr("新增片段", "New snippet")
                    })
                    .strong()
                    .size(12.0),
                );
                ui.add_space(2.0);
                egui::Grid::new("snip_form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(crate::i18n::tr("名称", "Name"));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.snip.name)
                                .desired_width(210.0)
                                .hint_text(crate::i18n::tr("可选，便于识别", "Optional label")),
                        );
                        ui.end_row();
                        ui.label(crate::i18n::tr("命令", "Command"));
                        ui.add(
                            egui::TextEdit::multiline(&mut self.snip.cmd)
                                .desired_width(210.0)
                                .desired_rows(2),
                        );
                        ui.end_row();
                    });
                ui.checkbox(
                    &mut self.snip.run,
                    crate::i18n::tr("发送后自动回车执行", "Press Enter after sending"),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new(if editing {
                                    crate::i18n::tr("保存", "Save")
                                } else {
                                    crate::i18n::tr("添加", "Add")
                                })
                                .color(egui::Color32::WHITE),
                            )
                            .fill(Palette::ACCENT),
                        )
                        .clicked()
                    {
                        save_now = true;
                    }
                    if editing && ui.button(crate::i18n::tr("取消编辑", "Cancel")).clicked() {
                        self.snip.editing = None;
                        self.snip.name.clear();
                        self.snip.cmd.clear();
                        self.snip.run = true;
                    }
                });
            });
        // 闭包外处理，避免与 self 的借用冲突
        if let Some(i) = edit {
            if let Some(sn) = self.snip.list.get(i) {
                self.snip.editing = Some(i);
                self.snip.name = sn.name.clone();
                self.snip.cmd = sn.command.clone();
                self.snip.run = sn.run;
            }
        }
        if let Some(i) = delete {
            if i < self.snip.list.len() {
                self.snip.list.remove(i);
                changed = true;
                if self.snip.editing == Some(i) {
                    self.snip.editing = None;
                    self.snip.name.clear();
                    self.snip.cmd.clear();
                    self.snip.run = true;
                }
            }
        }
        if save_now {
            let cmd = self.snip.cmd.trim().to_string();
            if !cmd.is_empty() {
                let sn = crate::store::Snippet {
                    name: self.snip.name.trim().to_string(),
                    command: cmd,
                    run: self.snip.run,
                };
                match self.snip.editing.take() {
                    Some(i) if i < self.snip.list.len() => self.snip.list[i] = sn,
                    _ => self.snip.list.push(sn),
                }
                self.snip.name.clear();
                self.snip.cmd.clear();
                self.snip.run = true;
                changed = true;
            }
        }
        if changed {
            crate::store::save_snippets(&self.snip.list);
        }
        if let Some((cmd, run)) = send_cmd {
            if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                // ai_owned 会话是只读的（AI 专用），代码片段不能往里面插
                if !s.ai_owned {
                    let mut bytes = cmd.into_bytes();
                    if run {
                        bytes.push(b'\r');
                    }
                    let _ = s.cmd_tx.send(UiCommand::TerminalInput(bytes));
                    s.terminal.request_focus();
                }
            }
        }
        // 点击窗口外部自动隐藏（打开当帧除外），或点 X 关闭
        let clicked_outside = win
            .as_ref()
            .map(|r| r.response.clicked_elsewhere())
            .unwrap_or(false);
        if close_win || (clicked_outside && !self.snip.just_opened) {
            self.snip.show = false;
        }
        self.snip.just_opened = false;
    }

    /// 关闭窗口前确认（仍有会话，或编辑器有未保存修改时——后者即使会话已全部关闭
    /// 也必须拦截，否则未保存内容会随主窗口静默丢失）。
    pub(super) fn handle_close(&mut self, ctx: &egui::Context) {
        let dirty_tabs = {
            let ed = super::util::lock_mutex(&self.editor_state);
            ed.tabs.iter().filter(|t| t.editor.dirty()).count()
        };
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && (!self.sessions.is_empty() || dirty_tabs > 0)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_close_confirm = true;
        }
        if self.show_close_confirm {
            egui::Modal::new(egui::Id::new("close_modal"))
                .show(ctx, |ui| {
                    ui.set_width(320.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("确认退出", "Quit?")).size(16.0).strong());
                        ui.add_space(6.0);
                        if !self.sessions.is_empty() {
                            ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("还有 {} 个会话处于连接中", self.sessions.len()), crate::i18n::Lang::En => format!("{} session(s) still connected", self.sessions.len()) });
                        }
                        if dirty_tabs > 0 {
                            ui.label(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("编辑器有 {dirty_tabs} 个文件未保存，退出将丢失修改"), crate::i18n::Lang::En => format!("{dirty_tabs} file(s) have unsaved changes; quitting discards them") }).color(Palette::DANGER));
                        }
                        ui.label(crate::i18n::tr("确定退出 iShell 吗？", "Quit iShell?"));
                    });
                    ui.add_space(12.0);
                    // 按钮行水平居中（固定按钮宽度 + 居中留白）
                    ui.horizontal(|ui| {
                        let bw = 72.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        let space = ((ui.available_width() - total) / 2.0).max(0.0);
                        ui.add_space(space);
                        if dialog_button(ui, crate::i18n::tr("退出", "Quit"), Some(Palette::DANGER), bw) {
                            self.allow_close = true;
                            self.show_close_confirm = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                            self.show_close_confirm = false;
                        }
                    });
                });
        }
    }

    /// AI 首次通过 `open_session` 使用某条已保存连接时，弹窗等用户当面批准（而不是仅凭
    /// AI 传的名字字符串就信任）；批准后本次运行期间对同一条连接不再重复询问。
    pub(super) fn handle_ai_open_consent(&mut self, ctx: &egui::Context) {
        let Some(name) = self
            .pending_open_consent
            .as_ref()
            .map(|p| p.conn.name.clone())
        else {
            return;
        };
        egui::Modal::new(egui::Id::new("ai_open_consent_modal")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(crate::i18n::tr("AI 请求新开一个终端会话", "AI wants to open a new terminal session"))
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.label(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!(
                        "AI 想用已保存连接 “{name}” 新开一个终端会话。这个会话会由 AI 驱动执行命令，\
                         你可以实时看到，但键盘输入不会发给它。是否允许？"
                    ),
                    crate::i18n::Lang::En => format!(
                        "AI wants to open a new terminal session using the saved connection “{name}”. \
                         AI will drive it; you can watch in real time, but your keystrokes won't be \
                         sent to it. Allow?"
                    ),
                });
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 96.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                let space = ((ui.available_width() - total) / 2.0).max(0.0);
                ui.add_space(space);
                if dialog_button(ui, crate::i18n::tr("允许", "Allow"), Some(Palette::ACCENT), bw) {
                    self.resolve_open_consent(true);
                }
                if dialog_button(ui, crate::i18n::tr("拒绝", "Deny"), Some(Palette::DANGER), bw) {
                    self.resolve_open_consent(false);
                }
            });
        });
    }

    /// AI 想对**用户自己打开的**会话做写入类操作时的授权弹窗（见 `PendingUseConsent`）。
    ///
    /// 和上面「新开会话」那个确认框是两回事：那个批准的是"动用某条已保存连接的凭据"，
    /// 这个批准的是"往我正在用的这个 shell 里插手"——AI 开的会话是只读的、不会和用户抢
    /// 输入，用户自己的会话没这层保护，所以要单独确认，且授权只对这一个会话生效。
    pub(super) fn handle_ai_use_consent(&mut self, ctx: &egui::Context) {
        let Some((title, action)) = self
            .pending_use_consent
            .as_ref()
            .map(|p| (p.title.clone(), p.action.clone()))
        else {
            return;
        };
        egui::Modal::new(egui::Id::new("ai_use_consent_modal")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "AI 请求操作你的终端会话",
                        "AI wants to act in your terminal session",
                    ))
                    .size(16.0)
                    .strong(),
                );
                ui.add_space(6.0);
                ui.label(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!(
                        "“{title}” 是你自己打开的会话，AI 想在里面{action}。\n\n\
                         允许后你和 AI 会同时能往这个 shell 里输入，两边的按键可能互相打断。\
                         更稳妥的做法是拒绝，让 AI 用 open_session 开一个它专用的只读会话。"
                    ),
                    crate::i18n::Lang::En => format!(
                        "“{title}” is a session you opened yourself, and AI wants to {action} in it.\n\n\
                         If you allow this, you and AI can both type into the same shell and your \
                         keystrokes may interleave. Denying is usually safer: AI can call \
                         open_session to get a read-only session of its own."
                    ),
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "允许后这个会话在本次运行期间不再询问（重启 iShell 后失效）",
                        "Allowing stops the prompts for this session until iShell restarts",
                    ))
                    .size(11.0)
                    .color(Palette::TEXT_DIM),
                );
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 96.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                let space = ((ui.available_width() - total) / 2.0).max(0.0);
                ui.add_space(space);
                if dialog_button(ui, crate::i18n::tr("允许", "Allow"), Some(Palette::ACCENT), bw) {
                    self.resolve_use_consent(true);
                }
                if dialog_button(ui, crate::i18n::tr("拒绝", "Deny"), Some(Palette::DANGER), bw) {
                    self.resolve_use_consent(false);
                }
            });
        });
    }
}

impl App {}
