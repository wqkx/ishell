//! PDF / Word 文档标签的内容区渲染（编辑器窗口内，复用编辑器标签框架）。

use egui::RichText;

use super::{DocKind, EditorTab};
use crate::proto::UiCommand;
use crate::theme::Palette;

/// PDF / Word 文档视图（编辑器窗口的文档标签内容区）。
/// PDF：底部工具栏（与编辑器状态栏同款）+ 单页视图；滚动到页底继续滚自动翻下页。
/// Word：重排阅读视图，块高度缓存 + 视口裁剪（长文档只渲染可见部分）。
pub(crate) fn doc_view(ui: &mut egui::Ui, tab: &mut EditorTab) {
    use egui_phosphor::regular as icon;
    let path = tab.editor.path.clone();
    let fname = tab.editor.filename();
    let tab_cmd = tab.cmd_tx.clone();
    let Some(doc) = tab.doc.as_mut() else { return };
    match doc {
        DocKind::Pdf {
            pages,
            cur,
            zoom,
            cache,
            pending,
            flip_at,
            search,
            search_open,
            hits,
            hit_sel,
            searching,
            search_msg,
        } => {
            // Ctrl+F 打开/关闭查找；Esc 关闭
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F)) {
                *search_open = !*search_open;
            }
            if *search_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                *search_open = false;
            }
            // 查找条（顶部，VSCode 风格小浮条）
            if *search_open {
                egui::Panel::top(ui.id().with("pdf_find"))
                    .frame(
                        egui::Frame::new()
                            .fill(Palette::PANEL_2)
                            .inner_margin(egui::Margin::symmetric(8, 4)),
                    )
                    .show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(search)
                                    .desired_width(220.0)
                                    .hint_text(crate::i18n::tr(
                                        "查找（回车搜索全文）",
                                        "Find (Enter to search)",
                                    )),
                            );
                            let go =
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            // searching 期间不重复发送（大 PDF 提取全文需数秒，连按回车会排队多个任务）
                            if go && !search.trim().is_empty() && !*searching {
                                *searching = true;
                                *search_msg = None;
                                hits.clear();
                                let _ = tab_cmd.send(UiCommand::PdfSearch {
                                    path: path.clone(),
                                    query: search.trim().to_string(),
                                });
                            }
                            if *searching {
                                ui.spinner();
                            } else if let Some(msg) = search_msg {
                                ui.label(
                                    RichText::new(msg.as_str())
                                        .color(Palette::DANGER)
                                        .size(11.5),
                                );
                            } else if !hits.is_empty() {
                                // 上一个/下一个命中（跳页）
                                if ui
                                    .button(
                                        RichText::new(egui_phosphor::regular::CARET_UP).size(12.0),
                                    )
                                    .clicked()
                                {
                                    *hit_sel = (*hit_sel + hits.len() - 1) % hits.len();
                                    *cur = hits[*hit_sel].0.clamp(1, *pages);
                                }
                                if ui
                                    .button(
                                        RichText::new(egui_phosphor::regular::CARET_DOWN)
                                            .size(12.0),
                                    )
                                    .clicked()
                                {
                                    *hit_sel = (*hit_sel + 1) % hits.len();
                                    *cur = hits[*hit_sel].0.clamp(1, *pages);
                                }
                                let (pg, snippet) = &hits[*hit_sel];
                                ui.label(
                                    RichText::new(format!(
                                        "{}/{} · P{}",
                                        *hit_sel + 1,
                                        hits.len(),
                                        pg
                                    ))
                                    .monospace()
                                    .size(11.5)
                                    .color(Palette::TEXT),
                                );
                                ui.label(
                                    RichText::new(snippet.as_str())
                                        .color(Palette::TEXT_DIM)
                                        .size(11.5),
                                );
                            } else if !search.trim().is_empty() {
                                ui.label(
                                    RichText::new(crate::i18n::tr("无结果", "No results"))
                                        .color(Palette::TEXT_DIM)
                                        .size(11.5),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(egui_phosphor::regular::X).size(11.0),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        *search_open = false;
                                    }
                                },
                            );
                        });
                    });
            }
            // 底部工具栏（样式/位置对齐编辑器状态栏）
            egui::Panel::bottom(ui.id().with("pdf_status"))
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL_2)
                        .inner_margin(egui::Margin {
                            left: 8,
                            right: 8,
                            top: 2,
                            bottom: 2,
                        }),
                )
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        let dim = Palette::TEXT_DIM;
                        let click_lbl = |ui: &mut egui::Ui, s: String, tip: &str| {
                            ui.add(
                                egui::Label::new(RichText::new(s).size(11.5).color(dim))
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text(tip.to_string())
                            .clicked()
                        };
                        if click_lbl(
                            ui,
                            icon::CARET_UP.to_string(),
                            crate::i18n::tr("上一页 (PageUp/←)", "Prev (PageUp/←)"),
                        ) {
                            *cur = cur.saturating_sub(1).max(1);
                        }
                        ui.label(
                            RichText::new(format!("{cur} / {pages}"))
                                .monospace()
                                .size(11.0)
                                .color(Palette::TEXT),
                        );
                        if click_lbl(
                            ui,
                            icon::CARET_DOWN.to_string(),
                            crate::i18n::tr("下一页 (PageDown/→)", "Next (PageDown/→)"),
                        ) {
                            *cur = (*cur + 1).min(*pages);
                        }
                        ui.add_space(12.0);
                        if click_lbl(ui, "−".into(), crate::i18n::tr("缩小", "Zoom out")) {
                            let z = if *zoom <= 0.0 { 1.0 } else { *zoom };
                            *zoom = (z / 1.2).max(0.25);
                        }
                        if click_lbl(ui, "+".into(), crate::i18n::tr("放大", "Zoom in")) {
                            let z = if *zoom <= 0.0 { 1.0 } else { *zoom };
                            *zoom = (z * 1.2).min(4.0);
                        }
                        if click_lbl(
                            ui,
                            crate::i18n::tr("适宽", "Fit").into(),
                            crate::i18n::tr("适应窗口宽度", "Fit width"),
                        ) {
                            *zoom = 0.0;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(&fname).color(dim).size(11.0))
                                .on_hover_text(&path);
                        });
                    });
                });
            // 键盘翻页
            if ui.input(|i| i.key_pressed(egui::Key::PageUp) || i.key_pressed(egui::Key::ArrowLeft))
            {
                *cur = cur.saturating_sub(1).max(1);
            }
            if ui.input(|i| {
                i.key_pressed(egui::Key::PageDown) || i.key_pressed(egui::Key::ArrowRight)
            }) {
                *cur = (*cur + 1).min(*pages);
            }
            // 请求当前页 ± 1（缺页且不在途才发）
            for p in [*cur, cur.saturating_sub(1), *cur + 1] {
                if p >= 1
                    && p <= *pages
                    && !cache.iter().any(|(cp, _, _)| *cp == p)
                    && !pending.contains(&p)
                {
                    pending.insert(p);
                    let _ = tab.cmd_tx.send(UiCommand::PdfPage {
                        path: path.clone(),
                        page: p,
                        dpi: 120,
                    });
                }
            }
            // 页面内容：每页独立滚动状态（id_salt 页码 → 翻页自动回顶）
            let out = egui::ScrollArea::both()
                .id_salt(*cur)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some((_, tex, size)) = cache.iter().find(|(p, _, _)| *p == *cur) {
                        let avail_w = ui.available_width();
                        let w = if *zoom <= 0.0 {
                            (avail_w - 18.0).min(size.x * 2.0).max(64.0)
                        } else {
                            size.x * *zoom
                        };
                        let h = w / size.x * size.y;
                        ui.vertical_centered(|ui| {
                            ui.add_space(8.0);
                            ui.add(egui::Image::new((tex.id(), egui::vec2(w, h))));
                            ui.add_space(8.0);
                        });
                    } else {
                        ui.add_space(40.0);
                        ui.vertical_centered(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(crate::i18n::tr("渲染中 …", "Rendering …"))
                                    .color(Palette::TEXT_DIM)
                                    .size(12.0),
                            );
                        });
                    }
                });
            // 滚动连续翻页：页底继续下滚 → 下页；页顶继续上滚 → 上页（0.3s 冷却防惯性连翻）
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            let now = ui.input(|i| i.time);
            if now - *flip_at > 0.3 && scroll_y.abs() > 0.5 {
                let at_bottom =
                    out.state.offset.y + out.inner_rect.height() >= out.content_size.y - 2.0;
                let at_top = out.state.offset.y <= 0.5;
                if scroll_y < 0.0 && at_bottom && *cur < *pages {
                    *cur += 1;
                    *flip_at = now;
                } else if scroll_y > 0.0
                    && at_top
                    && *cur > 1
                    && out.content_size.y > out.inner_rect.height() + 2.0
                {
                    // 仅当本页确实滚过（非单屏页）才向上翻，避免与「页底下滚」对称触发时抖动
                    *cur -= 1;
                    *flip_at = now;
                } else if scroll_y > 0.0
                    && at_top
                    && *cur > 1
                    && out.state.offset.y <= 0.0
                    && out.content_size.y <= out.inner_rect.height()
                {
                    *cur -= 1;
                    *flip_at = now;
                }
            }
        }
        DocKind::Docx {
            doc,
            images,
            heights,
            search,
            search_open,
            hits,
            hit_sel,
            scroll_to,
        } => {
            // Ctrl+F 打开/关闭查找；Esc 关闭
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F)) {
                *search_open = !*search_open;
            }
            if *search_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                *search_open = false;
            }
            if *search_open {
                egui::Panel::top(ui.id().with("docx_find"))
                    .frame(
                        egui::Frame::new()
                            .fill(Palette::PANEL_2)
                            .inner_margin(egui::Margin::symmetric(8, 4)),
                    )
                    .show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(search)
                                    .desired_width(220.0)
                                    .hint_text(crate::i18n::tr("查找", "Find")),
                            );
                            // 本地即时搜索：输入变化即重算命中块
                            if resp.changed() {
                                hits.clear();
                                *hit_sel = 0;
                                let q = search.trim().to_lowercase();
                                if !q.is_empty() {
                                    for (bi, b) in doc.blocks.iter().enumerate() {
                                        if crate::ui::docx::block_text(b)
                                            .to_lowercase()
                                            .contains(&q)
                                        {
                                            hits.push(bi);
                                            if hits.len() >= 500 {
                                                break;
                                            }
                                        }
                                    }
                                }
                                if let Some(&b0) = hits.first() {
                                    *scroll_to = Some(b0);
                                }
                            }
                            let enter =
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if !hits.is_empty() {
                                let mut jump = false;
                                if ui
                                    .button(
                                        RichText::new(egui_phosphor::regular::CARET_UP).size(12.0),
                                    )
                                    .clicked()
                                {
                                    *hit_sel = (*hit_sel + hits.len() - 1) % hits.len();
                                    jump = true;
                                }
                                if ui
                                    .button(
                                        RichText::new(egui_phosphor::regular::CARET_DOWN)
                                            .size(12.0),
                                    )
                                    .clicked()
                                    || enter
                                {
                                    *hit_sel = (*hit_sel + 1) % hits.len();
                                    jump = true;
                                }
                                if jump {
                                    *scroll_to = Some(hits[*hit_sel]);
                                }
                                ui.label(
                                    RichText::new(format!("{}/{}", *hit_sel + 1, hits.len()))
                                        .monospace()
                                        .size(11.5)
                                        .color(Palette::TEXT),
                                );
                            } else if !search.trim().is_empty() {
                                ui.label(
                                    RichText::new(crate::i18n::tr("无结果", "No results"))
                                        .color(Palette::TEXT_DIM)
                                        .size(11.5),
                                );
                            }
                            if enter {
                                resp.request_focus(); // 回车跳转后焦点留在查找框，可连续回车
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(egui_phosphor::regular::X).size(11.0),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        *search_open = false;
                                    }
                                },
                            );
                        });
                    });
            }
            egui::Panel::bottom(ui.id().with("docx_status"))
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL_2)
                        .inner_margin(egui::Margin {
                            left: 8,
                            right: 8,
                            top: 2,
                            bottom: 2,
                        }),
                )
                .show_inside(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::tr("阅读视图", "Reading view"))
                                .color(Palette::TEXT_DIM)
                                .size(11.0),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(&fname).color(Palette::TEXT_DIM).size(11.0))
                                .on_hover_text(&path);
                        });
                    });
                });
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_viewport(ui, |ui, vp| {
                    // 左对齐内容列 + 两侧留白居中（不能用 vertical_centered——它会把每个
                    // 控件水平居中，段落对齐全部错乱）。宽度取整以稳定布局缓存。
                    let avail = ui.available_width();
                    let maxw = avail.min(820.0).floor();
                    let pad = ((avail - maxw) / 2.0).max(0.0).floor();
                    let hilite = if *search_open {
                        hits.get(*hit_sel).copied()
                    } else {
                        None
                    };
                    ui.horizontal_top(|ui| {
                        ui.add_space(pad);
                        ui.vertical(|ui| {
                            ui.set_width(maxw);
                            ui.add_space(16.0);
                            crate::ui::docx::render_virtual(
                                ui, doc, images, heights, vp, hilite, scroll_to,
                            );
                            ui.add_space(28.0);
                        });
                    });
                });
        }
    }
}
