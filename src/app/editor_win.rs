//! App 的编辑器独立窗口渲染与标签关闭确认（`impl App` 方法，行为不变）。

use egui::RichText;

use crate::proto::UiCommand;
use crate::theme::Palette;

use super::util::lock_mutex;
use super::widgets::*;
use super::doc_view::doc_view;
use super::{App, DocKind, EditorState};

impl App {
    /// 关闭活动标签前的二次确认（会话仍连接时）。
    pub(super) fn close_tab_dialog(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.pending_close_tab else { return };
        // 若该会话已不在或已断开，则无需确认
        let Some(title) = self.sessions.get(idx).filter(|s| s.connected).map(|s| s.title.clone()) else {
            self.pending_close_tab = None;
            return;
        };
        let mut decision: Option<bool> = None;
        egui::Modal::new(egui::Id::new("close_tab_modal")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(crate::i18n::tr("关闭会话", "Close session")).size(16.0).strong());
                ui.add_space(6.0);
                ui.label(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("「{title}」仍在连接中，确定关闭吗？"),
                    crate::i18n::Lang::En => format!("\"{title}\" is still connected. Close it?"),
                });
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 72.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(ui, crate::i18n::tr("关闭", "Close"), Some(Palette::DANGER), bw) {
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

    /// 多标签文本编辑器：独立 OS 窗口（deferred viewport）。状态放在 self.editor_state（Arc<Mutex>），
    /// 回调与主 update() 共享。
    #[allow(deprecated)]
    pub(super) fn editor_window(&mut self, ctx: &egui::Context) {
        // 标题随激活文件变化：锁内算好即释放，回调运行时再单独加锁（二者不同时持锁）。
        let title = {
            let mut ed = lock_mutex(&self.editor_state);
            if ed.tabs.is_empty() {
                return; // 无标签：不再注册 viewport → eframe 自动关闭该窗口
            }
            if ed.active >= ed.tabs.len() {
                ed.active = ed.tabs.len() - 1;
            }
            let active = ed.active;
            ed.tabs
                .get(active)
                .map(|t| match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("iShell 编辑器 — {}·{}", t.server, t.editor.filename()),
                    crate::i18n::Lang::En => format!("iShell Editor — {}·{}", t.server, t.editor.filename()),
                })
                .unwrap_or_else(|| crate::i18n::tr("iShell 编辑器", "iShell Editor").into())
        };
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([480.0, 320.0])
            .with_maximize_button(false);
        let vid = egui::ViewportId::from_hash_of("ishell_editor");
        let state = self.editor_state.clone();

        ctx.show_viewport_deferred(vid, builder, move |vctx, _class| {
            use egui_phosphor::regular as icon;
            let mut ed = lock_mutex(&state);
            if ed.tabs.is_empty() {
                return;
            }
            if ed.active >= ed.tabs.len() {
                ed.active = ed.tabs.len() - 1;
            }
            // 新开/切换文件后把本窗口置前并聚焦
            if ed.focus {
                vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ed.focus = false;
            }
            if vctx.egui_wants_keyboard_input() {
                vctx.request_repaint();
            }
            // Ctrl+Tab / Ctrl+Shift+Tab 切换编辑器标签（先 consume，免被文本框当作 Tab 字符）
            let n = ed.tabs.len();
            if n > 1 {
                if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::Tab)) {
                    ed.active = (ed.active + n - 1) % n;
                } else if vctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                    ed.active = (ed.active + 1) % n;
                }
            }
            let mut close_tab: Option<usize> = None;
            let mut activate: Option<usize> = None;
            let mut do_save = false;
            let mut toggle_find = false;
            let mut toggle_follow = false;
            // 标签栏：左侧可拖动重排的标签（仿主窗口，带跟手+缓动），右侧「保存 / 查找」
            egui::Panel::top("editor_tabs")
                .frame(egui::Frame::new().fill(Palette::BG).inner_margin(egui::Margin::symmetric(8, 4)))
                .show(vctx, |ui| {
                    ui.style_mut().interaction.tooltip_delay = 0.5; // 悬停 0.5s 才弹出完整路径
                    let want_scroll = ed.active != ed.shown;
                    ui.horizontal(|ui| {
                        // 固定行高并整体垂直居中，保证保存/查找按钮与标签上下对齐
                        ui.set_min_height(28.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // 居中要点：不在子布局再 set_min_height（与主窗口 top_tabs 一致）。
                            // 文档标签（PDF/Word 查看器）只读，不显示保存按钮
                            let is_doc = ed.tabs.get(ed.active).map(|t| t.doc.is_some()).unwrap_or(false);
                            if !is_doc && ui.add(egui::Button::new(RichText::new(format!("{}  {}", icon::FLOPPY_DISK, crate::i18n::tr("保存", "Save"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                do_save = true;
                            }
                            // 查找：采用主窗口右侧按钮（flat_button）样式
                            if flat_button(ui, &RichText::new(format!("{} {}", icon::MAGNIFYING_GLASS, crate::i18n::tr("查找", "Find"))), crate::i18n::tr("查找 / 替换", "Find / replace")) {
                                toggle_find = true;
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                // 保存动画：珊瑚线上先「绿扫」(save 0→1) 跟随实际上传进度、再「珊瑚扫回」
                                // (save 1→2) 表示已保存。绿扫速度 = min(实际写入进度, 限速)，限速下每段
                                // 至少耗时 MIN_SWEEP（最快动画，避免小文件瞬移）。
                                const MIN_SWEEP: f64 = 0.32;
                                let now = ui.input(|i| i.time);
                                let mut any_saving = false;
                                // 先做可变推进：计算各标签的 save 值 [0,2] / -1，并驱动阶段切换与结束清理
                                let mut saves: Vec<f32> = Vec::with_capacity(ed.tabs.len());
                                for t in ed.tabs.iter_mut() {
                                    let save = match (t.save_at, t.save_done_at) {
                                        (Some(t0), None) => {
                                            // 绿扫阶段：实际进度（写完但无 total 时视为 1）与限速取小
                                            let actual = if t.save_total > 0 {
                                                (t.save_done as f64 / t.save_total as f64).clamp(0.0, 1.0)
                                            } else if !t.is_saving() { 1.0 } else { 0.0 };
                                            let g = actual.min(((now - t0) / MIN_SWEEP).clamp(0.0, 1.0));
                                            if !t.is_saving() && g >= 1.0 {
                                                t.save_done_at = Some(now); // 写完且绿扫满 → 转珊瑚扫回
                                            }
                                            any_saving = true;
                                            g as f32
                                        }
                                        (Some(_), Some(td)) => {
                                            let c = ((now - td) / MIN_SWEEP).clamp(0.0, 1.0);
                                            if c >= 1.0 {
                                                t.save_at = None; // 动画结束，清理
                                                t.save_done_at = None;
                                                t.save_done = 0;
                                                t.save_total = 0;
                                                -1.0
                                            } else {
                                                any_saving = true;
                                                (1.0 + c) as f32
                                            }
                                        }
                                        _ => -1.0,
                                    };
                                    saves.push(save);
                                }
                                let labels: Vec<(u64, String, String, f32, f32)> = ed
                                    .tabs
                                    .iter()
                                    .enumerate()
                                    .map(|(i, t)| {
                                        let dirty = if t.editor.dirty() { " ●" } else { "" };
                                        // 加载中 → 进度 [0,1]，驱动 tab 上珊瑚色进度条；否则 -1（不画）
                                        let prog = if t.load_id.is_some() { (t.load_done as f32 / t.load_total.max(1) as f32).clamp(0.0, 1.0) } else { -1.0 };
                                        // 图标按内容类型：PDF / Word 文档标签与文本编辑区分
                                        let ic = match &t.doc {
                                            Some(DocKind::Pdf { .. }) => icon::FILE_CODE,
                                            Some(DocKind::Docx { .. }) => icon::CLIPBOARD_TEXT,
                                            None => icon::FILE_CODE,
                                        };
                                        (
                                            t.text_id.value(),
                                            format!("{} {}·{}{}", ic, t.server, t.editor.filename(), dirty),
                                            t.editor.path.clone(),
                                            prog,
                                            saves[i],
                                        )
                                    })
                                    .collect();
                                if any_saving {
                                    ui.ctx().request_repaint(); // 动画进行中：持续重绘推进
                                }
                                let active = ed.active;
                                // 解引用为 &mut EditorState，借用检查器才允许同时可变借用多个不相交字段
                                let edm: &mut EditorState = &mut ed;
                                let (act, cls, reord) = draggable_tabs(ui, &mut edm.tab_drag, &mut edm.tab_grab_dx, &mut edm.tab_total_w, active, want_scroll, &labels);
                                if let Some(a) = act {
                                    activate = Some(a);
                                }
                                if let Some(c) = cls {
                                    close_tab = Some(c);
                                }
                                if let Some((from, to)) = reord {
                                    if from < ed.tabs.len() && to < ed.tabs.len() {
                                        ed.tabs.swap(from, to);
                                        ed.active = if ed.active == from { to } else if ed.active == to { from } else { ed.active };
                                    }
                                }
                            });
                        });
                    });
                    ed.shown = ed.active;
                });
            if toggle_find {
                let active = ed.active;
                if let Some(t) = ed.tabs.get_mut(active) {
                    match &mut t.doc {
                        // PDF 标签：查找按钮打开 PDF 全文查找条（远端 pdftotext）
                        Some(DocKind::Pdf { search_open, .. }) => *search_open = !*search_open,
                        // Word 标签：本地即时查找条
                        Some(DocKind::Docx { search_open, .. }) => *search_open = !*search_open,
                        None => t.editor.toggle_find(),
                    }
                }
            }

            // 当前标签内容（无内边距：底部状态栏、编辑区贴到窗口左右/底边，仿 VSCode）
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::PANEL).inner_margin(0))
                .show(vctx, |ui| {
                    let active = ed.active;
                    if let Some(tab) = ed.tabs.get_mut(active) {
                        let tid = tab.text_id;
                        // 外部改动冲突横幅：保存被拒后提示，可覆盖或取消
                        if tab.is_conflict() {
                            egui::Frame::new().fill(egui::Color32::from_rgb(255, 244, 220)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::WARNING, crate::i18n::tr("文件已被外部修改，未保存", "File changed externally; not saved"))).color(Palette::TEXT).size(12.0));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button(crate::i18n::tr("取消", "Cancel")).clicked() {
                                            tab.save = super::SaveState::Idle; // 取消冲突横幅，回到空闲（保留 dirty）
                                        }
                                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("覆盖保存", "Overwrite")).color(egui::Color32::WHITE)).fill(Palette::DANGER)).clicked() {
                                            let _ = tab.cmd_tx.send(UiCommand::WriteFile {
                                                path: tab.editor.path.clone(),
                                                content: tab.editor.content.clone(),
                                                encoding: tab.editor.encoding().to_string(),
                                                eol: tab.editor.eol(),
                                                expect_mtime: tab.editor.mtime(),
                                                force: true,
                                            });
                                            tab.begin_save(false); // 覆盖保存进行中，屏蔽再次保存直至结果返回
                                        }
                                    });
                                });
                            });
                        }
                        if tab.doc.is_some() {
                            doc_view(ui, tab); // PDF / Word 文档视图（复用编辑器标签框架）
                        } else if crate::ui::editor::content(ui, &mut tab.editor, tid) {
                            do_save = true;
                        }
                    }
                });

            if let Some(i) = activate {
                ed.active = i;
            }
            // 状态栏「跟随」/「只读→可编辑」按钮请求（editor.rs 置位，这里消费）
            {
                let active = ed.active;
                if let Some(t) = ed.tabs.get_mut(active) {
                    if t.editor.follow_req {
                        t.editor.follow_req = false;
                        toggle_follow = true;
                    }
                    if t.editor.unlock_req {
                        t.editor.unlock_req = false;
                        t.editor.readonly = false;
                    }
                }
            }
            // 跟随开关：开启要求文件无未保存修改（跟随=查看模式；退出后保存仍走 mtime 冲突确认）
            if toggle_follow {
                let now = vctx.input(|i| i.time);
                let active = ed.active;
                if let Some(t) = ed.tabs.get_mut(active) {
                    if t.editor.follow {
                        t.editor.follow = false;
                    } else if !t.editor.dirty() && t.load_id.is_none() {
                        t.editor.follow = true;
                        t.tail_offset = u64::MAX;
                        t.tail_pending = true;
                        t.tail_last = now;
                        // 初始化：只取当前文件大小（相当于 tail -f -n 0），此后每 ~1s 增量拉取
                        let _ = t.cmd_tx.send(UiCommand::TailFile { path: t.editor.path.clone(), offset: u64::MAX });
                    }
                }
            }
            if do_save {
                let active = ed.active;
                // 仅在「有改动」且「上次保存已完成」时才真正保存：无改动不触发也不放动画；
                // 保存进行中（大文件耗时）屏蔽再次保存，避免用旧 mtime 重复写入被误判为外部改动；
                // 跟随模式（tail -f）期间禁止保存——外部持续写入，本地内容无权威性。
                let should = ed.tabs.get(active).is_some_and(|t| {
                    t.editor.dirty() && !t.is_saving() && !t.editor.is_readonly()
                });
                if should {
                    if let Some(tab) = ed.tabs.get(active) {
                        let _ = tab.cmd_tx.send(UiCommand::WriteFile {
                            path: tab.editor.path.clone(),
                            content: tab.editor.content.clone(),
                            encoding: tab.editor.encoding().to_string(),
                            eol: tab.editor.eol(),
                            expect_mtime: tab.editor.mtime(),
                            force: false,
                        });
                    }
                    if let Some(tab) = ed.tabs.get_mut(active) {
                        // 不在此处 mark_saved：只有收到服务器 FileSaved 确认（且签名一致）
                        // 才清 dirty——发送失败/远端写失败时标签必须仍是「未保存」
                        tab.begin_save(false); // 保存进行中，收到 FileSaved/Conflict/Failed 前屏蔽再次保存
                        // 触发标签底部珊瑚线的「绿扫→珊瑚扫」保存动画（重置进度，跟随本次写入）
                        tab.save_at = Some(vctx.input(|i| i.time));
                        tab.save_done_at = None;
                        tab.save_done = 0;
                        tab.save_total = 0;
                    }
                }
            }
            if let Some(i) = close_tab {
                // 脏标签：先弹确认（保存并关闭 / 不保存 / 取消）；干净标签直接关
                if ed.tabs.get(i).map(|t| t.editor.dirty()).unwrap_or(false) {
                    ed.close_tab_confirm = Some(i);
                } else {
                    if i < ed.tabs.len() {
                        let closed = ed.tabs.remove(i);
                        // 记住光标位置（下次打开恢复）
                        if closed.doc.is_none() { crate::store::save_cursor_line(&format!("{}|{}", closed.server, closed.editor.path), closed.editor.caret_line()); }
                        // 清除该编辑器在 egui 内存中的 TextEdit 状态（含撤销历史的文本快照）
                        vctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
                    }
                    if ed.active >= ed.tabs.len() && !ed.tabs.is_empty() {
                        ed.active = ed.tabs.len() - 1;
                    }
                    ed.trim_request = true;
                }
            }
            // 脏标签关闭确认
            if let Some(ti) = ed.close_tab_confirm {
                let name = ed.tabs.get(ti).map(|t| t.editor.filename()).unwrap_or_default();
                let mut decision = 0u8; // 1=保存并关闭 2=不保存关闭 3=取消
                egui::Modal::new(egui::Id::new("editor_tab_close_modal")).show(vctx, |ui| {
                    ui.set_width(330.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("关闭标签", "Close tab")).size(16.0).strong());
                        ui.add_space(6.0);
                        ui.label(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{name} 有未保存的修改"),
                            crate::i18n::Lang::En => format!("{name} has unsaved changes"),
                        });
                    });
                    ui.add_space(12.0);
                    let bw = 100.0;
                    let total = bw * 3.0 + ui.spacing().item_spacing.x * 2.0;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("保存并关闭", "Save & close")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            decision = 1;
                        }
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("不保存", "Discard")).color(Palette::DANGER)).min_size(egui::vec2(bw, 0.0))).clicked() {
                            decision = 2;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            decision = 3;
                        }
                    });
                });
                if decision != 0 {
                    if decision == 1 {
                        // 保存并关闭：发出保存后**不立即关闭**——等 FileSaved 确认才移除标签；
                        // 失败/冲突则保留标签（否则本地修改的唯一副本会随标签一起消失）
                        if let Some(t) = ed.tabs.get_mut(ti) {
                            if !t.is_saving() {
                                let _ = t.cmd_tx.send(UiCommand::WriteFile { path: t.editor.path.clone(), content: t.editor.content.clone(), encoding: t.editor.encoding().to_string(), eol: t.editor.eol(), expect_mtime: t.editor.mtime(), force: false });
                                t.begin_save(true); // 保存中，且完成后关闭
                                t.save_at = Some(vctx.input(|i| i.time));
                                t.save_done_at = None;
                                t.save_done = 0;
                                t.save_total = 0;
                            } else {
                                t.request_close_on_saved(); // 已在保存中：标记完成后关闭（当前在途保存确认时处理）
                            }
                        }
                    }
                    if decision == 2 {
                        if ti < ed.tabs.len() {
                            let closed = ed.tabs.remove(ti);
                            if closed.doc.is_none() { crate::store::save_cursor_line(&format!("{}|{}", closed.server, closed.editor.path), closed.editor.caret_line()); }
                            vctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(closed.text_id));
                        }
                        if ed.active >= ed.tabs.len() && !ed.tabs.is_empty() {
                            ed.active = ed.tabs.len() - 1;
                        }
                        ed.trim_request = true;
                    }
                    ed.close_tab_confirm = None;
                }
            }

            // 原生关闭按钮：若有未保存修改先拦截并确认，否则关闭全部标签
            if vctx.input(|i| i.viewport().close_requested()) {
                if ed.tabs.iter().any(|t| t.editor.dirty()) {
                    vctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ed.close_confirm = true;
                } else {
                    ed.close_all(vctx);
                }
            }
            if ed.close_confirm {
                let mut do_close = false;
                let mut cancel = false;
                egui::Modal::new(egui::Id::new("editor_close_modal")).show(vctx, |ui| {
                    ui.set_width(300.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("关闭编辑器", "Close editor")).size(16.0).strong());
                        ui.add_space(6.0);
                        ui.label(crate::i18n::tr("有未保存的修改，确定关闭吗？", "Some files have unsaved changes. Close anyway?"));
                    });
                    ui.add_space(12.0);
                    let bw = 80.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("关闭", "Close")).color(egui::Color32::WHITE)).fill(Palette::DANGER).min_size(egui::vec2(bw, 0.0))).clicked() {
                            do_close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            cancel = true;
                        }
                    });
                });
                if do_close {
                    ed.close_all(vctx);
                } else if cancel {
                    ed.close_confirm = false;
                }
            }
        });
    }
}
