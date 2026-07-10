//! 端口转发管理浮窗。

use egui::{RichText, Sense};

use crate::proto::UiCommand;
use crate::theme::Palette;

use super::super::util::*;
use super::super::widgets::*;
use super::super::{App, ForwardEntry};

impl App {
    /// 端口转发管理浮窗（右上角弹出，样式与传输浮窗一致）。
    pub(in crate::app) fn forward_window(&mut self, ctx: &egui::Context) {
        use crate::proto::{ForwardKind, ForwardSpec};
        use egui_phosphor::regular as icon;
        if !self.fwd.show {
            return;
        }
        let idx = self.active.filter(|&i| i < self.sessions.len());
        let mut add_spec: Option<ForwardSpec> = None;
        let mut remove_id: Option<u64> = None; // 确认后真正删除
        let mut arm_del: Option<u64> = None; // 点垃圾桶 → 进入该行确认态
        let mut cancel_del = false; // 确认态点取消
        let mut edit_id: Option<u64> = None; // 点铅笔 → 把该条回填表单进入编辑
        let mut cancel_edit = false; // 编辑态点「取消编辑」
        let confirm_del = self.fwd.confirm_del; // 本帧处于确认态的转发 id（快照）
        let mut close_win = false;

        let win = egui::Window::new("forward_win")
            .title_bar(false)
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(340.0)
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::ARROWS_LEFT_RIGHT, crate::i18n::tr("端口转发", "Port forward"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                    });
                });
                ui.separator();

                let Some(idx) = idx else {
                    ui.add_space(4.0);
                    ui.label(RichText::new(crate::i18n::tr("请先连接一个会话", "Connect a session first")).color(Palette::TEXT_DIM).size(12.0));
                    return;
                };

                // 新增/编辑表单（分段按钮代替下拉，避免点击下拉被判为窗口外而自动关闭）
                let editing = self.fwd.editing; // 快照：编辑态决定按钮文案与提交语义
                let fwd_error = self.fwd.error.clone(); // 快照：内联错误（避免与 f 的可变借用冲突）
                let f = &mut self.fwd.form;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut f.kind, 0usize, crate::i18n::tr("本地转发", "Local"));
                    ui.selectable_value(&mut f.kind, 1usize, crate::i18n::tr("动态 SOCKS5", "Dynamic SOCKS5"));
                });
                ui.horizontal(|ui| {
                    ui.label(crate::i18n::tr("本地", "Local"));
                    ui.add(egui::TextEdit::singleline(&mut f.bind).desired_width(84.0).hint_text("127.0.0.1"));
                    ui.label(":");
                    ui.add(egui::TextEdit::singleline(&mut f.local_port).desired_width(48.0).hint_text(crate::i18n::tr("端口", "Port")));
                });
                if !is_loopback_bind(&f.bind) {
                    ui.label(RichText::new(crate::i18n::tr(
                        "⚠ 非回环地址：局域网内他人可使用此转发（SOCKS5 无认证）",
                        "⚠ Non-loopback: others on the LAN can use this forward (SOCKS5 has no auth)",
                    )).color(Palette::DANGER).size(11.0));
                }
                if f.kind == 0 {
                    ui.horizontal(|ui| {
                        ui.label(crate::i18n::tr("目标", "Target"));
                        ui.add(egui::TextEdit::singleline(&mut f.target_host).desired_width(120.0).hint_text(crate::i18n::tr("主机/IP", "Host/IP")));
                        ui.label(":");
                        ui.add(egui::TextEdit::singleline(&mut f.target_port).desired_width(48.0).hint_text(crate::i18n::tr("端口", "Port")));
                    });
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let (btn_icon, btn_label) = if editing.is_some() {
                        (icon::CHECK, crate::i18n::tr("保存修改", "Save"))
                    } else {
                        (icon::PLUS, crate::i18n::tr("添加转发", "Add forward"))
                    };
                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", btn_icon, btn_label)).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                        if let Ok(lp) = f.local_port.trim().parse::<u16>() {
                            let kind = if f.kind == 0 {
                                match f.target_port.trim().parse::<u16>() {
                                    Ok(tp) if !f.target_host.trim().is_empty() => {
                                        Some(ForwardKind::Local { remote_host: f.target_host.trim().to_string(), remote_port: tp })
                                    }
                                    _ => None,
                                }
                            } else {
                                Some(ForwardKind::Dynamic)
                            };
                            if let Some(kind) = kind {
                                let bind = if f.bind.trim().is_empty() { "127.0.0.1".into() } else { f.bind.trim().to_string() };
                                add_spec = Some(ForwardSpec { id: 0, bind_host: bind, bind_port: lp, kind });
                            }
                        }
                    }
                    // 编辑态：提供「取消编辑」退回新增态
                    if editing.is_some() && ui.button(crate::i18n::tr("取消编辑", "Cancel")).clicked() {
                        cancel_edit = true;
                    }
                });
                // 内联错误（端口占用 / 参数无效）
                if let Some(err) = &fwd_error {
                    ui.label(RichText::new(err).color(Palette::DANGER).size(11.0));
                }

                ui.separator();
                if let Some(s) = self.sessions.get(idx) {
                    if s.forwards.is_empty() {
                        crate::ui::empty_state(ui, egui_phosphor::regular::ARROWS_LEFT_RIGHT, crate::i18n::tr("暂无转发任务", "No forwards"), false);
                    }
                    for fwd in &s.forwards {
                        ui.horizontal(|ui| {
                            let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, if fwd.ok { Palette::OK } else { Palette::DANGER });
                            ui.label(RichText::new(&fwd.label).size(12.0));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if confirm_del == Some(fwd.id) {
                                    // 行内二次确认：确认（红勾）/ 取消（X）
                                    if ui.add(egui::Button::new(RichText::new(icon::CHECK).size(12.0).color(Palette::DANGER)).frame(false)).on_hover_text(crate::i18n::tr("确认删除", "Confirm delete")).clicked() {
                                        remove_id = Some(fwd.id);
                                    }
                                    if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("取消", "Cancel")).clicked() {
                                        cancel_del = true;
                                    }
                                } else {
                                    if ui.add(egui::Button::new(RichText::new(icon::TRASH).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除", "Delete")).clicked() {
                                        arm_del = Some(fwd.id);
                                    }
                                    // 编辑：把该条参数回填表单
                                    if ui.add(egui::Button::new(RichText::new(icon::PENCIL_SIMPLE).size(12.0).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("编辑", "Edit")).clicked() {
                                        edit_id = Some(fwd.id);
                                    }
                                }
                            });
                        });
                        ui.label(RichText::new(&fwd.status).color(if fwd.ok { Palette::TEXT_DIM } else { Palette::DANGER }).size(10.5));
                        ui.add_space(3.0);
                    }
                }
            });

        // 行内删除确认态的进入/退出
        if let Some(id) = arm_del {
            self.fwd.confirm_del = Some(id);
        }
        if cancel_del || remove_id.is_some() {
            self.fwd.confirm_del = None;
        }

        // 点击窗口外部自动隐藏（打开当帧除外）
        let clicked_outside = win
            .as_ref()
            .map(|r| r.response.clicked_elsewhere())
            .unwrap_or(false);
        if close_win || (clicked_outside && !self.fwd.just_opened) {
            self.fwd.show = false;
            self.fwd.confirm_del = None; // 关窗时复位确认态，避免下次打开仍处于「确认删除」
            self.fwd.editing = None; // 复位编辑态与内联错误
            self.fwd.error = None;
        }
        self.fwd.just_opened = false;

        let idx = match idx {
            Some(i) => i,
            None => return,
        };
        // 取消编辑：复位编辑态与表单
        if cancel_edit {
            self.fwd.editing = None;
            self.fwd.error = None;
            self.fwd.form.local_port.clear();
            self.fwd.form.target_host.clear();
            self.fwd.form.target_port.clear();
            self.fwd.just_opened = true;
        }
        // 进入编辑：把选中转发的参数回填表单
        if let Some(id) = edit_id {
            if let Some(fwd) = self
                .sessions
                .get(idx)
                .and_then(|s| s.forwards.iter().find(|f| f.id == id))
            {
                let (bh, bp, kind) = (fwd.bind_host.clone(), fwd.bind_port, fwd.kind.clone());
                let form = &mut self.fwd.form;
                form.bind = bh;
                form.local_port = bp.to_string();
                match kind {
                    ForwardKind::Local {
                        remote_host,
                        remote_port,
                    } => {
                        form.kind = 0;
                        form.target_host = remote_host;
                        form.target_port = remote_port.to_string();
                    }
                    ForwardKind::Dynamic => {
                        form.kind = 1;
                        form.target_host.clear();
                        form.target_port.clear();
                    }
                }
                self.fwd.editing = Some(id);
                self.fwd.error = None;
            }
            self.fwd.just_opened = true; // 点编辑不算窗外点击
        }
        // 添加 / 保存：先做本地端口占用 + 重复校验，通过才发起（编辑则先删旧再加新）
        if let Some(spec) = add_spec {
            let editing = self.fwd.editing;
            // 与现有转发重复（排除正在编辑的那条），或本机端口已被占用
            let dup = self.sessions.get(idx).is_some_and(|s| {
                s.forwards.iter().any(|f| {
                    f.bind_port == spec.bind_port
                        && f.bind_host == spec.bind_host
                        && Some(f.id) != editing
                })
            });
            // 编辑且端口与原值相同时跳过 OS 探测：那个端口正被「被编辑的转发」自身监听着，会误报占用
            let same_as_editing = editing
                .and_then(|id| {
                    self.sessions
                        .get(idx)
                        .and_then(|s| s.forwards.iter().find(|f| f.id == id))
                })
                .is_some_and(|f| f.bind_port == spec.bind_port && f.bind_host == spec.bind_host);
            if dup || (!same_as_editing && local_port_in_use(&spec.bind_host, spec.bind_port)) {
                self.fwd.error = Some(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("本地端口 {} 已被占用", spec.bind_port),
                    crate::i18n::Lang::En => {
                        format!("Local port {} is already in use", spec.bind_port)
                    }
                });
                self.fwd.just_opened = true;
            } else if !is_loopback_bind(&spec.bind_host) {
                // 非回环：二次确认后再真正添加（SOCKS5 无认证时即开放代理）
                self.fwd.pending_open_bind = Some(spec);
                self.fwd.just_opened = true;
            } else {
                self.commit_forward(idx, spec, editing);
            }
        }
        // 非回环绑定二次确认
        if self.fwd.pending_open_bind.is_some() {
            let mut accept = false;
            let mut reject = false;
            let bind_desc = self
                .fwd
                .pending_open_bind
                .as_ref()
                .map(|s| format!("{}:{}", s.bind_host, s.bind_port))
                .unwrap_or_default();
            egui::Modal::new(egui::Id::new("fwd_open_bind")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(crate::i18n::tr("确认对外开放转发？", "Expose forward on the network?")).size(16.0).strong().color(Palette::DANGER));
                ui.add_space(8.0);
                ui.label(RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("将绑定 {bind_desc}（非回环）。局域网内他人可使用此转发；动态 SOCKS5 无认证，等同开放代理。"),
                    crate::i18n::Lang::En => format!("Will bind {bind_desc} (non-loopback). Others on the LAN can use it; dynamic SOCKS5 has no auth (open proxy)."),
                }).size(12.0));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 120.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    if dialog_button(ui, crate::i18n::tr("仍然开放", "Expose anyway"), Some(Palette::DANGER), bw) {
                        accept = true;
                    }
                    if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                        reject = true;
                    }
                });
            });
            if accept {
                if let Some(spec) = self.fwd.pending_open_bind.take() {
                    let editing = self.fwd.editing;
                    self.commit_forward(idx, spec, editing);
                }
            } else if reject {
                self.fwd.pending_open_bind = None;
            }
        }
        if let Some(id) = remove_id {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.forwards.retain(|f| f.id != id);
                let _ = s.cmd_tx.send(UiCommand::RemoveForward(id));
            }
        }
    }

    /// 真正添加/更新一条端口转发（编辑则先删旧再加新），并复位表单。
    fn commit_forward(
        &mut self,
        idx: usize,
        mut spec: crate::proto::ForwardSpec,
        editing: Option<u64>,
    ) {
        use crate::proto::ForwardKind;
        if let Some(old) = editing {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.forwards.retain(|f| f.id != old);
                let _ = s.cmd_tx.send(UiCommand::RemoveForward(old));
            }
        }
        if let Some(s) = self.sessions.get_mut(idx) {
            let id = s.next_forward;
            s.next_forward += 1;
            spec.id = id;
            let label = match &spec.kind {
                ForwardKind::Local {
                    remote_host,
                    remote_port,
                } => {
                    format!(
                        "{}:{} → {}:{}",
                        spec.bind_host, spec.bind_port, remote_host, remote_port
                    )
                }
                ForwardKind::Dynamic => format!("SOCKS5 {}:{}", spec.bind_host, spec.bind_port),
            };
            s.forwards.push(ForwardEntry {
                id,
                label,
                status: crate::i18n::tr("启动中 …", "Starting …").into(),
                ok: true,
                bind_host: spec.bind_host.clone(),
                bind_port: spec.bind_port,
                kind: spec.kind.clone(),
            });
            let _ = s.cmd_tx.send(UiCommand::AddForward(spec));
        }
        self.fwd.editing = None;
        self.fwd.error = None;
        self.fwd.pending_open_bind = None;
        self.fwd.form.local_port.clear();
        self.fwd.form.target_host.clear();
        self.fwd.form.target_port.clear();
        self.fwd.just_opened = true;
    }
}
