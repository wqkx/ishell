//! 看图工具独立视口窗口。

use egui::RichText;

use crate::theme::Palette;

use super::super::widgets::*;
use super::super::App;

impl App {
    /// 看图工具浮窗：独立可缩放窗口；滚轮以光标为锚点缩放，拖动平移。
    #[allow(deprecated)] // 同 editor_window：viewport 内用 Panel::show(ctx) 渲染根 UI
    pub(in crate::app) fn image_window(&mut self, ctx: &egui::Context) {
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
}
