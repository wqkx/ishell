//! App 的辅助窗口渲染方法（看图 / GPU 详情 / 进程详情 / 端口转发 / toast / 传输列表）。
//! 均为 `impl App` 方法，签名与调用点不变；从 God Object 物理拆出以缩小 mod.rs。

use egui::{RichText, Sense};

use crate::proto::{ConflictPolicy, UiCommand};
use crate::theme::Palette;

use super::util::*;
use super::widgets::*;
use super::{App, ForwardEntry, Transfer, XferFilter, XferSpec};

impl App {
    /// 看图工具浮窗：独立可缩放窗口；滚轮以光标为锚点缩放，拖动平移。
    #[allow(deprecated)] // 同 editor_window：viewport 内用 Panel::show(ctx) 渲染根 UI
    pub(super) fn image_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if self.image.tabs.is_empty() {
            return;
        }
        if self.image.active >= self.image.tabs.len() {
            self.image.active = self.image.tabs.len() - 1;
        }

        // 独立 OS 窗口（immediate viewport）：与主窗口分离，原生关闭按钮即可关闭。
        let vid = egui::ViewportId::from_hash_of("ishell_image");
        let title = self.image.tabs
            .get(self.image.active)
            .map(|t| {
                let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("iShell 看图 — {}·{}", t.server, fname),
                    crate::i18n::Lang::En => format!("iShell Image — {}·{}", t.server, fname),
                }
            })
            .unwrap_or_else(|| crate::i18n::tr("iShell 看图", "iShell Image").into());
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([760.0, 580.0])
            .with_min_inner_size([320.0, 240.0])
            // 同编辑器窗口：禁用最大化按钮，规避 macOS 最大化触发的幽灵窗口
            .with_maximize_button(false);

        ctx.show_viewport_immediate(vid, builder, |vctx, _class| {
            // 新开/切换图片后把本窗口置前并聚焦
            if self.image.focus {
                vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                self.image.focus = false;
            }
            // Ctrl+Tab / Ctrl+Shift+Tab 切换看图标签
            let n = self.image.tabs.len();
            if n > 1 {
                if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)) {
                    self.image.active = (self.image.active + n - 1) % n;
                } else if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                    self.image.active = (self.image.active + 1) % n;
                }
            }
            let mut close_tab: Option<usize> = None;
            let mut activate: Option<usize> = None;
            let mut save_msg: Option<String> = None;
            let mut do_fit = false;
            let mut do_one = false;
            let mut do_save_as = false;

            // 标签栏（仿编辑器/主窗口：左侧可拖动重排的标签，右侧操作按钮，整体垂直居中）
            egui::Panel::top("image_tabs")
                .frame(egui::Frame::new().fill(Palette::BG).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(vctx, |ui| {
                    ui.style_mut().interaction.tooltip_delay = 0.5; // 悬停 0.5s 显示完整路径
                    let want_scroll = self.image.active != self.image.shown;
                    ui.horizontal(|ui| {
                        ui.set_min_height(28.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // 另存为=主操作(珊瑚填充，对齐编辑器「保存」)；1:1/适应窗口=扁平按钮(对齐「查找」)
                            if ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::FLOPPY_DISK, crate::i18n::tr("另存为", "Save as"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                do_save_as = true;
                            }
                            if flat_button(ui, &RichText::new("1:1"), crate::i18n::tr("原始大小", "Actual size")) {
                                do_one = true;
                            }
                            if flat_button(ui, &RichText::new(crate::i18n::tr("适应窗口", "Fit")), crate::i18n::tr("适应窗口", "Fit to window")) {
                                do_fit = true;
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                let labels: Vec<(u64, String, String, f32, f32)> = self.image.tabs
                                    .iter()
                                    .map(|t| {
                                        let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                                        (
                                            egui::Id::new((t.uid, &t.path)).value(),
                                            format!("{} {}·{}", icon::IMAGE, t.server, fname),
                                            t.path.clone(),
                                            -1.0, // 图片标签无下载进度条
                                            -1.0, // 图片标签无保存动画
                                        )
                                    })
                                    .collect();
                                let active = self.image.active;
                                let (act, cls, reord) = draggable_tabs(ui, &mut self.image.tab_drag, &mut self.image.grab_dx, &mut self.image.total_w, active, want_scroll, &labels);
                                if let Some(a) = act {
                                    activate = Some(a);
                                }
                                if let Some(c) = cls {
                                    close_tab = Some(c);
                                }
                                if let Some((from, to)) = reord {
                                    if from < self.image.tabs.len() && to < self.image.tabs.len() {
                                        self.image.tabs.swap(from, to);
                                        self.image.active = if self.image.active == from { to } else if self.image.active == to { from } else { self.image.active };
                                    }
                                }
                            });
                        });
                    });
                    self.image.shown = self.image.active;
                });

            // 底部状态栏（仿编辑器：贴窗口左右/底边；左侧尺寸/缩放，右侧文件名）
            egui::Panel::bottom("image_status")
                .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin { left: 8, right: 8, top: 2, bottom: 2 }))
                .show(vctx, |ui| {
                    if let Some(t) = self.image.tabs.get(self.image.active) {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{}×{}", t.size.x as i32, t.size.y as i32)).color(Palette::TEXT_DIM).size(11.0));
                            if t.zoom > 0.0 {
                                ui.label(RichText::new("·").color(Palette::TEXT_DIM).size(11.0));
                                ui.label(RichText::new(format!("{}%", (t.zoom * 100.0).round() as i32)).color(Palette::TEXT_DIM).size(11.0));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let fname = t.path.rsplit('/').next().unwrap_or(t.path.as_str());
                                ui.label(RichText::new(fname).color(Palette::TEXT_DIM).size(11.0)).on_hover_text(t.path.as_str());
                            });
                        });
                    }
                });

            // 画布（贴窗口边）：灰底 + 图片，滚轮以光标为锚缩放、拖动平移、双击适应窗口
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(0))
                .show(vctx, |ui| {
                    if let Some(t) = self.image.tabs.get_mut(self.image.active) {
                        let avail = ui.available_size();
                        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(rect, 0.0, Palette::PANEL_2);
                        if resp.double_clicked() {
                            t.zoom = 0.0;
                            t.offset = egui::Vec2::ZERO;
                        }
                        if t.zoom <= 0.0 {
                            let fit = (rect.width() / t.size.x).min(rect.height() / t.size.y).min(1.0);
                            t.zoom = fit.clamp(0.02, 32.0);
                            t.offset = egui::Vec2::ZERO;
                        }
                        if resp.hovered() {
                            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
                            if scroll_y != 0.0 {
                                let old = t.zoom;
                                let new = (old * (scroll_y * 0.0015).exp()).clamp(0.02, 32.0);
                                if let Some(ptr) = resp.hover_pos() {
                                    let d = ptr - rect.center();
                                    let k = new / old;
                                    t.offset = d * (1.0 - k) + t.offset * k;
                                }
                                t.zoom = new;
                            }
                        }
                        if resp.dragged() {
                            t.offset += resp.drag_delta();
                        }
                        let disp = t.size * t.zoom;
                        let center = rect.center() + t.offset;
                        let img_rect = egui::Rect::from_center_size(center, disp);
                        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        painter.image(t.tex.id(), img_rect, uv, egui::Color32::WHITE);
                    }
                });

            if let Some(i) = activate {
                self.image.active = i;
            }
            // 应用标签栏按钮（在标签栏闭包外执行，避免与遍历 image_tabs 的不可变借用冲突）
            let active = self.image.active;
            if do_fit {
                if let Some(t) = self.image.tabs.get_mut(active) {
                    t.zoom = 0.0;
                    t.offset = egui::Vec2::ZERO;
                }
            }
            if do_one {
                if let Some(t) = self.image.tabs.get_mut(active) {
                    t.zoom = 1.0;
                    t.offset = egui::Vec2::ZERO;
                }
            }
            if do_save_as {
                if let Some(t) = self.image.tabs.get(active) {
                    if !t.data.is_empty() {
                        let fname = t.path.rsplit('/').next().unwrap_or("image").to_string();
                        let data = t.data.clone();
                        if let Some(path) = rfd::FileDialog::new().set_file_name(&fname).save_file() {
                            save_msg = Some(match std::fs::write(&path, &data) {
                                Ok(_) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已保存到 {}", path.display()), crate::i18n::Lang::En => format!("Saved to {}", path.display()) },
                                Err(e) => match crate::i18n::current() { crate::i18n::Lang::Zh => format!("保存失败：{e}"), crate::i18n::Lang::En => format!("Save failed: {e}") },
                            });
                        }
                    }
                }
            }
            // 方向键切换上一张/下一张（本窗口聚焦时即可，独立窗口不会抢占主窗口按键）
            let nav_delta = vctx.input(|i| {
                i.key_pressed(egui::Key::ArrowRight) as i32 - i.key_pressed(egui::Key::ArrowLeft) as i32
            });
            if nav_delta != 0 && !self.image.tabs.is_empty() {
                let n = self.image.tabs.len() as i32;
                self.image.active = (self.image.active as i32 + nav_delta).rem_euclid(n) as usize;
            }
            if let Some(msg) = save_msg {
                if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                    s.status = msg;
                }
            }
            if let Some(i) = close_tab {
                if i < self.image.tabs.len() {
                    self.image.tabs.remove(i); // 丢弃 TextureHandle 即释放 GPU 纹理
                }
                if self.image.active >= self.image.tabs.len() && !self.image.tabs.is_empty() {
                    self.image.active = self.image.tabs.len() - 1;
                }
                self.trim_after = Some(4);
            }

            // 原生关闭按钮 → 关闭看图工具（清空全部图片）
            if vctx.input(|i| i.viewport().close_requested()) {
                self.image.tabs.clear();
                self.image.active = 0;
                self.trim_after = Some(4);
            }
        });
    }

    /// GPU 详情小窗：每块 GPU 使用率 + 显存；点击窗口外任意处或点 X 关闭（不随鼠标移开关闭）。
    pub(super) fn gpu_popup_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        let Some(pos) = self.gpu_popup else { return };
        // 取活动会话的 GPU 列表（克隆，避免借用冲突）
        let gpus = self
            .active
            .and_then(|i| self.sessions.get(i))
            .and_then(|s| s.sysinfo.as_ref())
            .map(|si| si.gpus.clone())
            .unwrap_or_default();
        if gpus.is_empty() {
            self.gpu_popup = None;
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
        if close || (outside && !self.gpu_popup_just_opened) {
            self.gpu_popup = None;
        }
        self.gpu_popup_just_opened = false;
    }

    /// 进程详情小窗：显示资源/目录/命令 + 强制结束；点击外部关闭。
    pub(super) fn proc_popup_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        let (pid, name, cpu, mem, pos, cmd, cwd, exe) = match &self.proc_popup {
            Some(p) => (p.pid, p.name.clone(), p.cpu, p.mem, p.pos, p.cmd.clone(), p.cwd.clone(), p.exe.clone()),
            None => return,
        };
        let mut close = false;
        let mut kill = false; // 真正下发 KillProc（仅二次确认后置 true）
        let mut arm_kill = false; // 点击「强制结束」按钮 → 进入确认态
        let mut cancel_kill = false; // 确认态里点「取消」→ 退回
        let mut copy_target: Option<String> = None;
        let copied_t = self.proc_popup.as_ref().and_then(|p| p.copied_t);
        let confirm_kill = self.proc_popup.as_ref().map(|p| p.confirm_kill).unwrap_or(false);
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
                if !exe.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("程序", "Exe")).color(Palette::TEXT_DIM).size(12.0));
                        if ui.add(egui::Label::new(RichText::new(&exe).color(Palette::TEXT).size(12.0).monospace()).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                            copy_target = Some(exe.clone());
                        }
                    });
                }
                if !cwd.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("目录", "Dir")).color(Palette::TEXT_DIM).size(12.0));
                        if ui.add(egui::Label::new(RichText::new(&cwd).color(Palette::TEXT).size(12.0).monospace()).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
                            copy_target = Some(cwd.clone());
                        }
                    });
                }
                // 命令：可双击复制
                if cmd.is_empty() {
                    ui.label(RichText::new(crate::i18n::tr("（正在获取命令…）", "(loading command…)")).color(Palette::TEXT_DIM).size(11.0));
                } else {
                    ui.add_space(2.0);
                    ui.label(RichText::new(crate::i18n::tr("命令", "Command")).color(Palette::TEXT_DIM).size(12.0));
                    if ui.add(egui::Label::new(RichText::new(&cmd).size(11.5).monospace().color(Palette::TEXT)).sense(egui::Sense::click())).on_hover_text(tip).double_clicked() {
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
            if let Some(p) = &mut self.proc_popup {
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
            if let Some(p) = &mut self.proc_popup {
                p.confirm_kill = true;
            }
        }
        if cancel_kill {
            if let Some(p) = &mut self.proc_popup {
                p.confirm_kill = false;
            }
        }
        if kill {
            // 发往「打开弹窗时所属会话」(uid)，而非当前 active——避免 Ctrl+Tab 切走后误 kill 别的主机
            let target = self.proc_popup.as_ref().and_then(|p| self.session_idx_by_uid(p.uid));
            if let Some(i) = target {
                let _ = self.sessions[i].cmd_tx.send(UiCommand::KillProc(pid));
            }
            self.proc_popup = None;
        } else if close || (outside && !self.proc_popup_just_opened && !arm_kill) {
            // 注：arm_kill 当帧不因「点到按钮算窗外」而误关（按钮在窗内，理论上 outside=false，这里再加一道保险）
            self.proc_popup = None;
        }
        self.proc_popup_just_opened = false;
    }

    /// 端口转发管理浮窗（右上角弹出，样式与传输浮窗一致）。
    pub(super) fn forward_window(&mut self, ctx: &egui::Context) {
        use crate::proto::{ForwardKind, ForwardSpec};
        use egui_phosphor::regular as icon;
        if !self.show_forwards {
            return;
        }
        let idx = self.active.filter(|&i| i < self.sessions.len());
        let mut add_spec: Option<ForwardSpec> = None;
        let mut remove_id: Option<u64> = None; // 确认后真正删除
        let mut arm_del: Option<u64> = None; // 点垃圾桶 → 进入该行确认态
        let mut cancel_del = false; // 确认态点取消
        let mut edit_id: Option<u64> = None; // 点铅笔 → 把该条回填表单进入编辑
        let mut cancel_edit = false; // 编辑态点「取消编辑」
        let confirm_del = self.fwd_confirm_del; // 本帧处于确认态的转发 id（快照）
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
                let editing = self.fwd_editing; // 快照：编辑态决定按钮文案与提交语义
                let fwd_error = self.fwd_error.clone(); // 快照：内联错误（避免与 f 的可变借用冲突）
                let f = &mut self.fwd_form;
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
            self.fwd_confirm_del = Some(id);
        }
        if cancel_del || remove_id.is_some() {
            self.fwd_confirm_del = None;
        }

        // 点击窗口外部自动隐藏（打开当帧除外）
        let clicked_outside = win.as_ref().map(|r| r.response.clicked_elsewhere()).unwrap_or(false);
        if close_win || (clicked_outside && !self.fwd_just_opened) {
            self.show_forwards = false;
            self.fwd_confirm_del = None; // 关窗时复位确认态，避免下次打开仍处于「确认删除」
            self.fwd_editing = None; // 复位编辑态与内联错误
            self.fwd_error = None;
        }
        self.fwd_just_opened = false;

        let idx = match idx {
            Some(i) => i,
            None => return,
        };
        // 取消编辑：复位编辑态与表单
        if cancel_edit {
            self.fwd_editing = None;
            self.fwd_error = None;
            self.fwd_form.local_port.clear();
            self.fwd_form.target_host.clear();
            self.fwd_form.target_port.clear();
            self.fwd_just_opened = true;
        }
        // 进入编辑：把选中转发的参数回填表单
        if let Some(id) = edit_id {
            if let Some(fwd) = self.sessions.get(idx).and_then(|s| s.forwards.iter().find(|f| f.id == id)) {
                let (bh, bp, kind) = (fwd.bind_host.clone(), fwd.bind_port, fwd.kind.clone());
                let form = &mut self.fwd_form;
                form.bind = bh;
                form.local_port = bp.to_string();
                match kind {
                    ForwardKind::Local { remote_host, remote_port } => {
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
                self.fwd_editing = Some(id);
                self.fwd_error = None;
            }
            self.fwd_just_opened = true; // 点编辑不算窗外点击
        }
        // 添加 / 保存：先做本地端口占用 + 重复校验，通过才发起（编辑则先删旧再加新）
        if let Some(mut spec) = add_spec {
            let editing = self.fwd_editing;
            // 与现有转发重复（排除正在编辑的那条），或本机端口已被占用
            let dup = self.sessions.get(idx).is_some_and(|s| {
                s.forwards
                    .iter()
                    .any(|f| f.bind_port == spec.bind_port && f.bind_host == spec.bind_host && Some(f.id) != editing)
            });
            // 编辑且端口与原值相同时跳过 OS 探测：那个端口正被「被编辑的转发」自身监听着，会误报占用
            let same_as_editing = editing
                .and_then(|id| self.sessions.get(idx).and_then(|s| s.forwards.iter().find(|f| f.id == id)))
                .is_some_and(|f| f.bind_port == spec.bind_port && f.bind_host == spec.bind_host);
            if dup || (!same_as_editing && local_port_in_use(&spec.bind_host, spec.bind_port)) {
                self.fwd_error = Some(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("本地端口 {} 已被占用", spec.bind_port),
                    crate::i18n::Lang::En => format!("Local port {} is already in use", spec.bind_port),
                });
                self.fwd_just_opened = true;
            } else {
                // 编辑模式：先删旧转发（移除记录 + 通知 worker 关闭监听）
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
                        ForwardKind::Local { remote_host, remote_port } => {
                            format!("{}:{} → {}:{}", spec.bind_host, spec.bind_port, remote_host, remote_port)
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
                self.fwd_editing = None;
                self.fwd_error = None;
                self.fwd_form.local_port.clear();
                self.fwd_form.target_host.clear();
                self.fwd_form.target_port.clear();
                self.fwd_just_opened = true;
            }
        }
        if let Some(id) = remove_id {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.forwards.retain(|f| f.id != id);
                let _ = s.cmd_tx.send(UiCommand::RemoveForward(id));
            }
        }
    }

    /// 右上角传输进度浮窗（可弹出/隐藏）。
    /// 顶部居中浮层提示：数秒后淡出。用于撤销结果等需要醒目反馈的操作。
    pub(super) fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some((msg, t0)) = self.toast.clone() else { return };
        const DUR: f64 = 3.5; // 显示时长（秒）
        const FADE: f64 = 0.6; // 末尾淡出时长
        let now = ctx.input(|i| i.time);
        let age = now - t0;
        if age >= DUR {
            self.toast = None;
            return;
        }
        let alpha = if age > DUR - FADE { ((DUR - age) / FADE) as f32 } else { 1.0 }.clamp(0.0, 1.0);
        egui::Area::new(egui::Id::new("undo_toast"))
            .anchor(egui::Align2::CENTER_TOP, [0.0, 54.0])
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                egui::Frame::new()
                    .fill(Palette::PANEL_2)
                    .stroke(egui::Stroke::new(1.0, Palette::ACCENT))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(egui_phosphor::regular::INFO).color(Palette::ACCENT).size(15.0));
                            ui.label(RichText::new(&msg).color(Palette::TEXT).size(13.0));
                        });
                    });
            });
        ctx.request_repaint(); // 维持淡出动画
    }

    pub(super) fn transfer_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if !self.show_transfers {
            return;
        }
        let Some(idx) = self.active else { return };
        // 同一服务器（host:port）的所有会话：把它们的传输任务汇总到同一个列表里展示
        let server_idxs = self.same_server_idxs(idx);
        let mut close_win = false;
        let mut clear = false;
        let mut cancel_all = false;
        let mut retry_all = false;
        let mut pick_dir = false;
        // 动作均以 (会话 uid, 传输 id) 标识，确保多标签同服务器时路由到正确的会话 worker
        let mut cancel_id: Option<(u64, u64)> = None;
        let mut toggle_err: Option<(u64, u64)> = None;
        let mut remove_id: Option<(u64, u64)> = None;
        let mut delete_id: Option<(u64, u64, String)> = None;
        let mut resume_id: Option<(u64, u64)> = None;
        let mut cycle_policy = false;
        let dl_dir = self.download_dir.to_string_lossy().into_owned();
        // 冲突策略短标签（中/英）+ 按策略区分的图标，用于标题栏按钮显示
        let policy_label = match (self.conflict_policy, crate::i18n::current()) {
            (ConflictPolicy::Overwrite, crate::i18n::Lang::Zh) => "覆盖",
            (ConflictPolicy::Skip, crate::i18n::Lang::Zh) => "跳过",
            (ConflictPolicy::Rename, crate::i18n::Lang::Zh) => "重命名",
            (ConflictPolicy::Overwrite, crate::i18n::Lang::En) => "Overwrite",
            (ConflictPolicy::Skip, crate::i18n::Lang::En) => "Skip",
            (ConflictPolicy::Rename, crate::i18n::Lang::En) => "Rename",
        };
        let policy_icon = match self.conflict_policy {
            ConflictPolicy::Overwrite => icon::SWAP,       // 覆盖=替换
            ConflictPolicy::Skip => icon::SKIP_FORWARD,    // 跳过
            ConflictPolicy::Rename => icon::PENCIL_SIMPLE, // 重命名
        };
        let win = egui::Window::new("transfer_win")
            .title_bar(false) // 隐藏过大的默认标题，使用自定义紧凑标题
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(330.0)
            .resizable(false)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(10),
            )
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("文件传输", "Transfers"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                        // 冲突策略：目标已存在时的默认处理；点击循环切换（覆盖→跳过→重命名），持久化
                        if ui
                            .add(egui::Button::new(RichText::new(format!("{} {}", policy_icon, policy_label)).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(crate::i18n::tr("目标已存在时的默认处理（点击切换：覆盖 / 跳过 / 重命名）", "Default when target exists (click to cycle: Overwrite / Skip / Rename)"))
                            .clicked()
                        {
                            cycle_policy = true;
                        }
                        if ui
                            .add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(13.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("选择默认下载文件夹\n当前：{}", dl_dir), crate::i18n::Lang::En => format!("Set default download folder\nCurrent: {}", dl_dir) })
                            .clicked()
                        {
                            pick_dir = true;
                        }
                    });
                });
                ui.separator();

                // 状态筛选：紧凑的 frameless 文字 chips（带计数），仅在有任务时显示——
                // 避免空列表时占位、也避免大按钮 + 多余分隔线让顶部拥挤。
                // 先在借用 s 之前算各状态计数（借用随即结束），再据此渲染并允许改 self.xfer_filter。
                let counts = server_idxs.iter().filter_map(|&i| self.sessions.get(i)).fold(
                    (0usize, 0usize, 0usize, 0usize),
                    |(tot, act, dn, fl), s| {
                        (
                            tot + s.transfers.len(),
                            act + s.transfers.iter().filter(|t| t.ok.is_none()).count(),
                            dn + s.transfers.iter().filter(|t| t.ok == Some(true)).count(),
                            fl + s.transfers.iter().filter(|t| t.ok == Some(false)).count(),
                        )
                    },
                );
                if counts.0 > 0 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 9.0;
                        for (f, zh, en, n) in [
                            (XferFilter::All, "全部", "All", counts.0),
                            (XferFilter::Active, "进行中", "Active", counts.1),
                            (XferFilter::Done, "已完成", "Done", counts.2),
                            (XferFilter::Failed, "失败", "Failed", counts.3),
                        ] {
                            let on = self.xfer_filter == f;
                            // 激活=强调色加粗；有失败时「失败」用危险色；其余弱色
                            let col = if on {
                                Palette::ACCENT
                            } else if matches!(f, XferFilter::Failed) && n > 0 {
                                Palette::DANGER
                            } else {
                                Palette::TEXT_DIM
                            };
                            let mut rt = RichText::new(format!("{} {}", crate::i18n::tr(zh, en), n)).size(11.0).color(col);
                            if on {
                                rt = rt.strong();
                            }
                            if ui.add(egui::Button::new(rt).frame(false).small()).clicked() {
                                self.xfer_filter = f;
                            }
                        }
                    });
                    ui.add_space(2.0);
                }
                let filter = self.xfer_filter;

                // 汇总同服务器所有会话的传输（活动会话在前，各自新→旧），元素为 (会话 uid, &Transfer)
                let items: Vec<(u64, &Transfer)> = server_idxs
                    .iter()
                    .filter_map(|&i| self.sessions.get(i))
                    .flat_map(|s| s.transfers.iter().rev().map(move |t| (s.uid, t)))
                    .collect();
                let total_len = items.len();
                if total_len == 0 {
                    ui.add_space(6.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::DOWNLOAD_SIMPLE, crate::i18n::tr("暂无传输任务", "No transfers"), false);
                }
                let mut open_dir: Option<String> = None;
                let mut shown = 0usize; // 当前筛选下实际展示的条数（用于「无匹配」提示）
                // 列表过长时滚动：约 8 条高度封顶，其余可滚动查看
                egui::ScrollArea::vertical().max_height(400.0).auto_shrink([false, true]).show(ui, |ui| {
                for (uid, t) in items.iter().copied().filter(|(_, t)| match filter {
                    XferFilter::All => true,
                    XferFilter::Active => t.ok.is_none(),
                    XferFilter::Done => t.ok == Some(true),
                    XferFilter::Failed => t.ok == Some(false),
                }).take(50) {
                    shown += 1;
                    // 下载=绿色，上传=珊瑚橙，颜色区分方向
                    let (dir_icon, dir_col) = match t.dir {
                        crate::proto::TransferDir::Download => (icon::DOWNLOAD_SIMPLE, Palette::OK),
                        crate::proto::TransferDir::Upload => (icon::UPLOAD_SIMPLE, Palette::ACCENT),
                    };
                    // 整个传输项包进一个感知点击的 scope，便于整体右键（否则右键会穿透到下方终端）
                    let item = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(dir_icon).color(dir_col).size(13.0));
                            ui.label(RichText::new(&t.name).size(12.0).color(Palette::TEXT));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                match t.ok {
                                    Some(true) => {
                                        ui.label(RichText::new(icon::CHECK_CIRCLE).color(Palette::OK).size(13.0));
                                        // 下载完成：保留「打开所在文件夹」按钮
                                        if let Some(local) = &t.local {
                                            if ui.add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(12.0).color(Palette::TEXT_DIM)).frame(false))
                                                .on_hover_text(crate::i18n::tr("在文件管理器中显示", "Show in file manager"))
                                                .clicked()
                                            {
                                                open_dir = Some(local.clone());
                                            }
                                        }
                                    }
                                    Some(false) => {
                                        // 失败：可重试（有重发规格时）+ 状态按钮展开原因
                                        if ui.add(egui::Button::new(RichText::new(icon::WARNING_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("点击查看失败原因", "Click for reason"))
                                            .clicked()
                                        {
                                            toggle_err = Some((uid, t.id));
                                        }
                                        if t.spec.is_some()
                                            && ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                                .on_hover_text(crate::i18n::tr("重试", "Retry"))
                                                .clicked()
                                        {
                                            resume_id = Some((uid, t.id));
                                        }
                                    }
                                    None if t.paused => {
                                        // 已中断/暂停：续传按钮
                                        if ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("续传", "Resume"))
                                            .clicked()
                                        {
                                            resume_id = Some((uid, t.id));
                                        }
                                    }
                                    None if t.queued => {
                                        // 等待态（中转目标端，正等源端下载）：时钟图标 + 转圈，不提供取消（受中转任务管控）
                                        ui.label(RichText::new(icon::CLOCK).color(Palette::TEXT_DIM).size(13.0))
                                            .on_hover_text(crate::i18n::tr("等待中", "Waiting"));
                                        ui.spinner();
                                    }
                                    None => {
                                        // 进行中：取消按钮 + 转圈
                                        if ui.add(egui::Button::new(RichText::new(icon::X_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("取消", "Cancel"))
                                            .clicked()
                                        {
                                            cancel_id = Some((uid, t.id));
                                        }
                                        ui.spinner();
                                    }
                                }
                            });
                        });
                        let done = t.ok == Some(true);
                        let frac = if done { 1.0 } else if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
                        let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as i32;
                        // 进行中/失败：条上居中显示百分比；完成：条上分两端（大小靠左、100% 靠右）
                        let mut bar = egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                            .fill(dir_col)
                            .desired_height(10.0)
                            .corner_radius(2.0);
                        if !done {
                            bar = bar.text(RichText::new(format!("{pct}%")).size(10.0));
                        }
                        let bar_resp = ui.add(bar);
                        if done {
                            let rect = bar_resp.rect;
                            let p = ui.painter_at(rect);
                            let font = egui::FontId::proportional(10.0);
                            // 大小靠左；有模式徽标（如「直传」）时显示在文件大小之后
                            let left_label = if t.tag.is_empty() {
                                crate::ui::fmt_bytes(t.total as f64)
                            } else {
                                format!("{} · {}", crate::ui::fmt_bytes(t.total as f64), t.tag)
                            };
                            p.text(
                                egui::pos2(rect.left() + 6.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                left_label,
                                font.clone(),
                                egui::Color32::WHITE,
                            );
                            // 100% 靠右
                            p.text(
                                egui::pos2(rect.right() - 6.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                "100%",
                                font,
                                egui::Color32::WHITE,
                            );
                        }
                        // 进行中才显示详情行（已传/总量 + 实时速度）；完成后不再单独一行
                        if t.ok.is_none() {
                            // 有阶段提示（打包/解包/等待/直传）时优先显示提示，替代字节读数——
                            // 这些阶段没有逐字节进度，显示「0 B / 0 B」会误导。
                            if !t.note.is_empty() {
                                ui.label(RichText::new(&t.note).size(10.0).color(Palette::TEXT_DIM));
                            } else {
                                let mut detail = format!("{} / {}", crate::ui::fmt_bytes(t.done as f64), crate::ui::fmt_bytes(t.total as f64));
                                // 模式徽标（如「直传」）紧跟在文件大小之后
                                if !t.tag.is_empty() {
                                    detail.push_str(&format!("  ·  {}", t.tag));
                                }
                                if t.speed > 0.0 {
                                    detail.push_str(&format!("  ·  {}", crate::ui::fmt_rate(t.speed)));
                                    // ETA：剩余字节 / 当前速度（仅未暂停、有剩余、速度有效时）
                                    if !t.paused && t.total > t.done {
                                        let eta = ((t.total - t.done) as f64 / t.speed).round() as u64;
                                        detail.push_str(&format!("  ·  {} {}", crate::i18n::tr("剩余", "ETA"), fmt_dur(eta)));
                                    }
                                }
                                ui.label(RichText::new(detail).size(10.0).color(Palette::TEXT_DIM));
                            }
                        }
                        // 失败且已展开：显示失败原因
                        if t.ok == Some(false) && t.show_err && !t.message.is_empty() {
                            ui.label(RichText::new(&t.message).color(Palette::DANGER).size(11.0));
                        }
                    });
                    // 右键菜单：打开所在文件 / 删除记录 / 删文件并删记录
                    item.response.context_menu(|ui| {
                        if let Some(local) = &t.local {
                            if ui.button(crate::i18n::tr("打开所在文件", "Reveal file")).clicked() {
                                open_dir = Some(local.clone());
                                ui.close();
                            }
                        }
                        // 仅「已完成/失败」的行可移除；进行中/等待中的行移除会让其追踪任务
                        // （直传 DirectJob / 中转 Relay）永久卡住 poll 不存在的 id，故不提供
                        if t.ok.is_some() {
                            if ui.button(crate::i18n::tr("删除记录", "Remove from list")).clicked() {
                                remove_id = Some((uid, t.id));
                                ui.close();
                            }
                            if let Some(local) = &t.local {
                                if ui.button(RichText::new(crate::i18n::tr("删除文件并移除记录", "Delete file & remove")).color(Palette::DANGER)).clicked() {
                                    delete_id = Some((uid, t.id, local.clone()));
                                    ui.close();
                                }
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                // 有任务但当前筛选下一条都没有：给出「无匹配」提示，避免看着像空列表
                if shown == 0 && total_len > 0 {
                    ui.add_space(6.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::MAGNIFYING_GLASS, crate::i18n::tr("该筛选下暂无任务", "No transfers match this filter"), false);
                }
                });
                if let Some(p) = open_dir {
                    open_containing_folder(&p);
                }
                if total_len > 0 {
                    ui.separator();
                    // 批量操作：仅在对应状态存在时显示，避免无意义按钮
                    let any_active = items.iter().any(|(_, t)| t.ok.is_none());
                    let any_failed = items.iter().any(|(_, t)| t.ok == Some(false) && t.spec.is_some());
                    let any_done = items.iter().any(|(_, t)| t.ok.is_some());
                    ui.horizontal(|ui| {
                        if any_active && ui.button(crate::i18n::tr("全部取消", "Cancel all")).clicked() {
                            cancel_all = true;
                        }
                        if any_failed && ui.button(crate::i18n::tr("重试失败", "Retry failed")).clicked() {
                            retry_all = true;
                        }
                        if any_done && ui.button(crate::i18n::tr("清除已完成", "Clear done")).clicked() {
                            clear = true;
                        }
                    });
                }
            });
        if clear {
            for &i in &server_idxs {
                if let Some(s) = self.sessions.get_mut(i) {
                    s.transfers.retain(|t| t.ok.is_none());
                }
            }
        }
        // 全部取消：对同服务器所有会话里进行中的任务下发取消
        if cancel_all {
            // 跳过 queued 占位行（worker 未登记）；镜像行经 cancel_target 转到源端真实传输
            let raw: Vec<(u64, u64)> = server_idxs
                .iter()
                .filter_map(|&i| self.sessions.get(i))
                .flat_map(|s| s.transfers.iter().filter(|t| t.ok.is_none() && !t.queued).map(move |t| (s.uid, t.id)))
                .collect();
            for (uid, id) in raw {
                let (tu, ti) = self.cancel_target(uid, id);
                if let Some(s) = self.session_idx_by_uid(tu).and_then(|i| self.sessions.get(i)) {
                    let _ = s.cmd_tx.send(UiCommand::CancelTransfer(ti));
                }
            }
            self.xfer_just_opened = true;
        }
        // 重试全部失败：对同服务器各会话每个有重发规格的失败任务重新发起（续传语义，覆盖）
        if retry_all {
            for &i in &server_idxs {
                if let Some(s) = self.sessions.get_mut(i) {
                    let targets: Vec<(u64, XferSpec)> = s
                        .transfers
                        .iter()
                        .filter(|t| t.ok == Some(false))
                        .filter_map(|t| t.spec.clone().map(|sp| (t.id, sp)))
                        .collect();
                    for (id, spec) in targets {
                        match spec {
                            XferSpec::Download { remote, local } => {
                                let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy: ConflictPolicy::Overwrite });
                            }
                            XferSpec::Upload { local, remote_dir } => {
                                let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy: ConflictPolicy::Overwrite });
                            }
                        }
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None;
                            t.paused = false;
                            t.show_err = false;
                            t.message = crate::i18n::tr("重试中 …", "Retrying …").into();
                        }
                    }
                }
            }
            self.xfer_just_opened = true;
        }
        // 取消传输：镜像行/源行都经 cancel_target 路由到源端真实传输并标记 cancelled
        if let Some((uid, id)) = cancel_id {
            let (tu, ti) = self.cancel_target(uid, id);
            if let Some(s) = self.session_idx_by_uid(tu).and_then(|i| self.sessions.get(i)) {
                let _ = s.cmd_tx.send(UiCommand::CancelTransfer(ti));
            }
            self.xfer_just_opened = true; // 避免点击被当作窗外点击而关窗
        }
        // 续传/重试：按重发规格重新发起，底层据已有字节自动续传
        if let Some((uid, id)) = resume_id {
            if let Some(i) = self.session_idx_by_uid(uid) {
                let s = &mut self.sessions[i];
                if let Some(spec) = s.transfers.iter().find(|t| t.id == id).and_then(|t| t.spec.clone()) {
                    match spec {
                        XferSpec::Download { remote, local } => { let _ = s.cmd_tx.send(UiCommand::Download { id, remote, local, policy: ConflictPolicy::Overwrite }); }
                        XferSpec::Upload { local, remote_dir } => { let _ = s.cmd_tx.send(UiCommand::Upload { id, local, remote_dir, policy: ConflictPolicy::Overwrite }); }
                    }
                    if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                        t.ok = None;
                        t.paused = false;
                        t.show_err = false;
                        t.message = crate::i18n::tr("续传中 …", "Resuming …").into();
                    }
                }
            }
            self.xfer_just_opened = true;
        }
        // 切换失败原因展开
        if let Some((uid, id)) = toggle_err {
            if let Some(i) = self.session_idx_by_uid(uid) {
                if let Some(t) = self.sessions[i].transfers.iter_mut().find(|t| t.id == id) {
                    t.show_err = !t.show_err;
                }
            }
            self.xfer_just_opened = true;
        }
        // 删除记录（仅移除列表项）
        if let Some((uid, id)) = remove_id {
            if let Some(i) = self.session_idx_by_uid(uid) {
                self.sessions[i].transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 删除文件并移除记录
        if let Some((uid, id, path)) = delete_id {
            let _ = std::fs::remove_file(&path);
            if let Some(i) = self.session_idx_by_uid(uid) {
                self.sessions[i].transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 选择默认下载目录（原生文件夹选择器）
        if pick_dir {
            if let Some(dir) = rfd::FileDialog::new().set_title(crate::i18n::tr("选择默认下载文件夹", "Select default download folder")).pick_folder() {
                self.download_dir = dir.clone();
                crate::store::save_download_dir(&dir.to_string_lossy());
            }
            self.xfer_just_opened = true; // 选择期间点击不算"外部点击"，避免关窗
        }
        // 冲突策略循环切换：覆盖 → 跳过 → 重命名 → 覆盖，并持久化
        if cycle_policy {
            self.conflict_policy = match self.conflict_policy {
                ConflictPolicy::Overwrite => ConflictPolicy::Skip,
                ConflictPolicy::Skip => ConflictPolicy::Rename,
                ConflictPolicy::Rename => ConflictPolicy::Overwrite,
            };
            crate::store::save_conflict_policy(self.conflict_policy.as_str());
            self.xfer_just_opened = true; // 切换点击不算窗外点击
        }
        // 点击窗口外部任意位置自动隐藏（打开当帧除外，避免被开启动作立即关闭）
        let clicked_outside = win
            .as_ref()
            .map(|r| r.response.clicked_elsewhere())
            .unwrap_or(false);
        if close_win || (clicked_outside && !self.xfer_just_opened) {
            self.show_transfers = false;
        }
        self.xfer_just_opened = false;
    }
}
