//! GPU / 进程详情弹出小窗。

use egui::{RichText, Sense};

use crate::proto::UiCommand;
use crate::theme::Palette;

use super::super::App;

impl App {
    /// GPU 详情小窗：每块 GPU 使用率 + 显存；点击窗口外任意处或点 X 关闭（不随鼠标移开关闭）。
    pub(in crate::app) fn gpu_popup_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        let Some(pos) = self.popups.gpu else { return };
        // 取活动会话的 GPU 列表（克隆，避免借用冲突）
        let gpus = self
            .active
            .and_then(|i| self.sessions.get(i))
            .and_then(|s| s.sysinfo.as_ref())
            .map(|si| si.gpus.clone())
            .unwrap_or_default();
        if gpus.is_empty() {
            self.popups.gpu = None;
            return;
        }
        let mut close = false;
        let win = egui::Window::new("gpu_popup")
            .title_bar(false)
            // 略微上移，让窗口左上角靠近点击点
            .fixed_pos(pos - egui::vec2(10.0, 10.0))
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 同进程详情窗：定宽使标题行/分割线与内容同宽对齐
                ui.set_width(300.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::CPU, crate::i18n::tr("GPU 详情", "GPU"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                // 自绘条（与侧栏 meter_row 同款）：暖灰轨道 + 近实色填充，文字浮于条上
                let bar_line = |ui: &mut egui::Ui, pct: f32, color: egui::Color32, text: String| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), Sense::hover());
                    let p = ui.painter_at(rect);
                    p.rect_filled(rect, 2.0, Palette::TRACK);
                    let mut fill = rect;
                    fill.set_width((rect.width() * (pct / 100.0).clamp(0.0, 1.0)).max(3.0));
                    p.rect_filled(fill, 2.0, egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 190));
                    p.text(rect.left_center() + egui::vec2(6.0, 0.0), egui::Align2::LEFT_CENTER, text,
                        egui::FontId::proportional(10.0), Palette::TEXT);
                };
                for g in &gpus {
                    ui.label(RichText::new(format!("GPU{} {}", g.index, g.name)).size(12.0).color(Palette::TEXT));
                    let mem_pct = if g.mem_total_mb > 0 { g.mem_used_mb as f32 / g.mem_total_mb as f32 * 100.0 } else { 0.0 };
                    bar_line(ui, g.util, crate::ui::usage_color(g.util),
                        match crate::i18n::current() { crate::i18n::Lang::Zh => format!("使用率 {:.0}%", g.util), crate::i18n::Lang::En => format!("Util {:.0}%", g.util) });
                    bar_line(ui, mem_pct, Palette::ACCENT,
                        match crate::i18n::current() { crate::i18n::Lang::Zh => format!("显存 {}/{} MB", g.mem_used_mb, g.mem_total_mb), crate::i18n::Lang::En => format!("VRAM {}/{} MB", g.mem_used_mb, g.mem_total_mb) });
                    ui.add_space(5.0);
                }
            });

        // 点击窗口外任意处或点 X 关闭（打开当帧除外）；不再因鼠标移开而关闭
        let outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close || (outside && !self.popups.gpu_just_opened) {
            self.popups.gpu = None;
        }
        self.popups.gpu_just_opened = false;
    }

    /// 进程详情小窗：显示资源/目录/命令 + 强制结束；点击外部关闭。
    pub(in crate::app) fn proc_popup_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        let (pid, name, cpu, mem, pos, cmd, cwd, exe) = match &self.popups.proc {
            Some(p) => (p.pid, p.name.clone(), p.cpu, p.mem, p.pos, p.cmd.clone(), p.cwd.clone(), p.exe.clone()),
            None => return,
        };
        let mut close = false;
        let mut kill = false; // 真正下发 KillProc（仅二次确认后置 true）
        let mut arm_kill = false; // 点击「强制结束」按钮 → 进入确认态
        let mut cancel_kill = false; // 确认态里点「取消」→ 退回
        let mut copy_target: Option<String> = None;
        let copied_t = self.popups.proc.as_ref().and_then(|p| p.copied_t);
        let confirm_kill = self.popups.proc.as_ref().map(|p| p.confirm_kill).unwrap_or(false);
        let now = ctx.input(|i| i.time);
        let win = egui::Window::new("proc_popup")
            .title_bar(false)
            .fixed_pos(pos + egui::vec2(8.0, 8.0))
            .resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).fill(Palette::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                // 固定内容宽度：自适应收缩窗口里，先布局的标题行/分割线取的是「当时估计宽度」，
                // 会被后续更宽的行（长命令）撑开而不跟随；定宽让所有行按同一宽度对齐
                ui.set_width(320.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::CPU, name)).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(k).color(Palette::TEXT_DIM).size(12.0));
                        ui.label(RichText::new(v).color(Palette::TEXT).size(12.0).monospace());
                    });
                };
                let tip = crate::i18n::tr("双击复制", "Double-click to copy");
                // PID：值可双击复制
                ui.horizontal(|ui| {
                    ui.label(RichText::new("PID").color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(pid.to_string()).color(Palette::TEXT).size(12.0).monospace()).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                        copy_target = Some(pid.to_string());
                    }
                });
                kv(ui, "CPU", format!("{cpu:.1}%"));
                kv(ui, crate::i18n::tr("内存", "Mem"), format!("{mem:.1}%"));
                // 程序 / 目录：值可双击复制
                // 长路径/命令用 .wrap() 折行到固定宽度，避免撑宽窗口（否则标题行的关闭键、
                // 分割线是按初始 320 宽布局的，不会跟随被撑宽的窗口）。
                if !exe.is_empty() {
                    ui.label(RichText::new(crate::i18n::tr("程序", "Exe")).color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(&exe).color(Palette::TEXT).size(12.0).monospace()).wrap().sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                        copy_target = Some(exe.clone());
                    }
                }
                if !cwd.is_empty() {
                    ui.label(RichText::new(crate::i18n::tr("目录", "Dir")).color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(&cwd).color(Palette::TEXT).size(12.0).monospace()).wrap().sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                        copy_target = Some(cwd.clone());
                    }
                }
                // 命令：可双击复制
                if cmd.is_empty() {
                    ui.label(RichText::new(crate::i18n::tr("（正在获取命令…）", "(loading command…)")).color(Palette::TEXT_DIM).size(11.0));
                } else {
                    ui.add_space(2.0);
                    ui.label(RichText::new(crate::i18n::tr("命令", "Command")).color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(&cmd).size(11.5).monospace().color(Palette::TEXT)).wrap().sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                        copy_target = Some(cmd.clone());
                    }
                }
                // 「已复制」短暂提示
                if let Some(t) = copied_t {
                    if now - t < 1.3 {
                        ui.add_space(2.0);
                        ui.label(RichText::new(format!("{}  {}", icon::CHECK_CIRCLE, crate::i18n::tr("已复制", "Copied"))).color(Palette::OK).size(11.0));
                    }
                }
                ui.separator();
                if !confirm_kill {
                    // 第一步：仅「武装」确认，不立即结束
                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::SKULL, crate::i18n::tr("强制结束 (kill -9)", "Kill (-9)"))).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                        arm_kill = true;
                    }
                } else {
                    // 第二步：二次确认（破坏性、不可撤销）——确认 / 取消
                    ui.label(RichText::new(match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("确定强制结束 PID {pid}（{name}）？此操作不可撤销。"),
                        crate::i18n::Lang::En => format!("Kill PID {pid} ({name})? This cannot be undone."),
                    }).color(Palette::TEXT).size(12.0));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::SKULL, crate::i18n::tr("确认结束", "Confirm"))).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                            kill = true;
                        }
                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                            cancel_kill = true;
                        }
                    });
                }
            });

        let outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        // 双击复制 -> 写剪贴板并记录时间（显示「已复制」）
        if let Some(v) = copy_target {
            ctx.copy_text(v);
            if let Some(p) = &mut self.popups.proc {
                p.copied_t = Some(now);
            }
            ctx.request_repaint();
        }
        // 让「已复制」提示到点自动消失
        if let Some(t) = copied_t {
            if now - t < 1.4 {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
        // 进入/退出「强制结束」确认态（不关窗）
        if arm_kill {
            if let Some(p) = &mut self.popups.proc {
                p.confirm_kill = true;
            }
        }
        if cancel_kill {
            if let Some(p) = &mut self.popups.proc {
                p.confirm_kill = false;
            }
        }
        if kill {
            // 发往「打开弹窗时所属会话」(uid)，而非当前 active——避免 Ctrl+Tab 切走后误 kill 别的主机
            let target = self.popups.proc.as_ref().and_then(|p| self.session_idx_by_uid(p.uid));
            if let Some(i) = target {
                if self.sessions[i].monitor_ok == Some(false) {
                    self.toast = Some((
                        crate::i18n::tr("远端不支持进程管理", "Remote does not support process control").into(),
                        ctx.input(|inp| inp.time),
                    ));
                } else {
                    let _ = self.sessions[i].cmd_tx.send(UiCommand::KillProc(pid));
                }
            }
            self.popups.proc = None;
        } else if close || (outside && !self.popups.proc_just_opened && !arm_kill) {
            // 注：arm_kill 当帧不因「点到按钮算窗外」而误关（按钮在窗内，理论上 outside=false，这里再加一道保险）
            self.popups.proc = None;
        }
        self.popups.proc_just_opened = false;
    }
}
