//! Terminal UI painting helpers used by `Terminal::ui`.

use egui::{Color32, FontId, Rect, Response, Stroke, Vec2};

use super::{
    osc::open_url,
    paint::{cell_format, find_row_urls, highlight_colors, paint_row_backgrounds},
    theme::TermColors,
    Terminal,
};

pub(super) struct PaintParams<'a> {
    pub(super) rect: Rect,
    pub(super) resp: &'a Response,
    pub(super) font: &'a FontId,
    pub(super) glyph_h: f32,
    pub(super) char_w: f32,
    pub(super) char_h: f32,
    pub(super) cell: Vec2,
    pub(super) ppp: f32,
    pub(super) focused: bool,
    pub(super) report_mouse: bool,
    pub(super) max_sb: usize,
    pub(super) sb_w: f32,
    pub(super) sb_track: Rect,
}

impl Terminal {
    pub(super) fn paint_terminal(&self, ui: &egui::Ui, params: PaintParams<'_>) {
        let PaintParams {
            rect,
            resp,
            font,
            glyph_h,
            char_w,
            char_h,
            cell,
            ppp,
            focused,
            report_mouse,
            max_sb,
            sb_w,
            sb_track,
        } = params;

        let tc = TermColors::by_index(self.theme);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, tc.bg);

        let sel = self.ordered_selection();
        let screen = self.parser.screen();
        // origin 也吸附到物理像素网格，使每个单元格都落在整数像素上（配合上面 char_w/char_h 吸附）
        let origin = egui::pos2(
            (rect.min.x * ppp).round() / ppp,
            (rect.min.y * ppp).round() / ppp,
        );

        // 可见行中的链接：用于悬停下划线 + 点击打开（鼠标上报模式下让位给 TUI）
        let mut link_rects: Vec<(Rect, String)> = Vec::new();
        if !report_mouse {
            for row in 0..self.rows {
                for (sc, ec, url) in find_row_urls(screen, row, self.cols) {
                    let x0 = origin.x + sc as f32 * char_w;
                    let x1 = origin.x + (ec as f32 + 1.0) * char_w;
                    let y = origin.y + row as f32 * char_h;
                    link_rects.push((
                        Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + char_h)),
                        url,
                    ));
                }
            }
        }
        let hover_pos = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| rect.contains(*p));
        let hover_link = hover_pos.and_then(|p| {
            link_rects
                .iter()
                .find(|(r, _)| r.contains(p))
                .map(|(_, u)| u.clone())
        });

        for row in 0..self.rows {
            let y = origin.y + row as f32 * char_h;
            // 先绘制该行的背景块（处理非默认底色）
            paint_row_backgrounds(&painter, screen, row, self.cols, origin, cell, &tc);
            // 搜索命中行高亮（整行淡黄底，取主题 WARN 色保持一致）
            if self.search_hl == Some(row) {
                let w = crate::theme::Palette::WARN;
                painter.rect_filled(
                    Rect::from_min_max(
                        egui::pos2(origin.x, y),
                        egui::pos2(rect.right(), y + char_h),
                    ),
                    0.0,
                    Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), 90),
                );
            }
            // 选区高亮（半透明，文字仍可见）
            if let Some(((sr, sc), (er, ec))) = sel {
                if row >= sr && row <= er {
                    let c0 = if row == sr { sc } else { 0 };
                    let c1 = if row == er {
                        ec
                    } else {
                        self.cols.saturating_sub(1)
                    };
                    let x0 = origin.x + c0 as f32 * char_w;
                    let x1 = origin.x + (c1 as f32 + 1.0) * char_w;
                    let a = crate::theme::Palette::ACCENT;
                    painter.rect_filled(
                        Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + char_h)),
                        0.0,
                        Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 80),
                    );
                }
            }
            // 逐格绘制字形：固定网格定位，避免 CJK / 宽字符的字形步进破坏对齐。
            // 空内容（含宽字符的续格）跳过；宽字符自身在本格绘制，自然跨两格。
            // 关键字高亮：覆盖匹配单元格的文字颜色
            let hl = if self.highlight {
                highlight_colors(screen, row, self.cols)
            } else {
                Vec::new()
            };
            for col in 0..self.cols {
                let Some(c) = screen.cell(row, col) else {
                    continue;
                };
                let s = c.contents();
                if s.is_empty() {
                    continue;
                }
                let fmt = cell_format(c, font, &tc);
                let color = hl.get(col as usize).copied().flatten().unwrap_or(fmt.color);
                let x = origin.x + col as f32 * char_w;
                if c.is_wide() {
                    // 全角字符（中文等）占 2 格：在两格内水平+纵向居中。
                    // 用反向放大的字号抵消 CJK 后备字体的全局缩小（CJK_SCALE），
                    // 让全角字以原始大小填满更多两格空间，减小字间距；行高 1.2× 留白足以容纳。
                    let wfont = FontId::monospace(self.font_size / crate::theme::CJK_SCALE);
                    painter.text(
                        egui::pos2(x + char_w, y + char_h / 2.0),
                        egui::Align2::CENTER_CENTER,
                        s,
                        wfont,
                        color,
                    );
                } else {
                    // 半角字符：纵向居中，使 1.2× 行高的额外留白上下均分
                    painter.text(
                        egui::pos2(x, y + char_h / 2.0),
                        egui::Align2::LEFT_CENTER,
                        s,
                        font.clone(),
                        color,
                    );
                }
                if fmt.underline.width > 0.0 {
                    // 下划线落在居中字形的底部附近
                    let uy = y + (char_h + glyph_h) / 2.0 - 1.0;
                    let w = if c.is_wide() { 2.0 * char_w } else { char_w };
                    painter.hline(x..=(x + w), uy, fmt.underline);
                }
            }
        }

        // 悬停的链接：手型光标 + 下划线；点击打开
        if let Some(p) = hover_pos {
            if let Some((r, _)) = link_rects.iter().find(|(r, _)| r.contains(p)) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                let uy = (origin.y + ((r.top() - origin.y) / char_h).round() * char_h)
                    + (char_h + glyph_h) / 2.0
                    - 1.0;
                painter.hline(
                    r.left()..=r.right(),
                    uy,
                    Stroke::new(1.0, crate::theme::Palette::ACCENT),
                );
            }
        }
        if resp.clicked() {
            if let Some(url) = &hover_link {
                open_url(url);
            }
        }

        // 光标
        if !screen.hide_cursor() && self.scrollback == 0 {
            let (cr, cc) = screen.cursor_position();
            let cpos = origin + Vec2::new(cc as f32 * char_w, cr as f32 * char_h);
            let crect = Rect::from_min_size(cpos, cell);
            // 失焦时仍用珊瑚色描边（而非低对比灰），避免点到文件栏/侧栏后光标看似「消失」
            if focused {
                painter.rect_filled(
                    crect,
                    1.0,
                    crate::theme::Palette::ACCENT.gamma_multiply(0.6),
                );
            } else {
                painter.rect_stroke(
                    crect,
                    1.0,
                    Stroke::new(1.2, crate::theme::Palette::ACCENT.gamma_multiply(0.8)),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // 启用 IME（中文 / fcitx 等输入法）：聚焦时上报输入区，并把候选框定位到光标处。
        // 否则平台不会在终端上激活输入法，导致无法输入中文。
        if focused {
            let (cr, cc) = screen.cursor_position();
            let ipos = origin + Vec2::new(cc as f32 * char_w, cr as f32 * char_h);
            let irect = Rect::from_min_size(ipos, cell);
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect: irect,
                    cursor_rect: irect,
                });
            });
            // 在光标处显示 IME 预编辑（组字中的拼音/候选），铺底 + 下划线以便辨识
            if !self.ime_preedit.is_empty() {
                let font = egui::FontId::monospace(self.font_size / crate::theme::CJK_SCALE);
                let galley = painter.layout_no_wrap(
                    self.ime_preedit.clone(),
                    font,
                    crate::theme::Palette::ACCENT,
                );
                let bg = Rect::from_min_size(ipos, galley.size());
                painter.rect_filled(bg, 0.0, crate::theme::Palette::PANEL);
                painter.galley(ipos, galley, crate::theme::Palette::ACCENT);
                painter.hline(
                    bg.x_range(),
                    bg.max.y - 1.0,
                    Stroke::new(1.0, crate::theme::Palette::ACCENT),
                );
            }
        }

        // 右侧滚动条（仅有可回滚历史时显示）：滑块高=视口/总量，位置由 scrollback 决定（0=底/最新）。
        if max_sb > 0 {
            let total = self.rows as f32 + max_sb as f32;
            let handle_h =
                (sb_track.height() * (self.rows as f32 / total)).clamp(24.0, sb_track.height());
            let pos_frac = 1.0 - (self.scrollback as f32 / max_sb as f32);
            let handle_top = sb_track.top() + (sb_track.height() - handle_h) * pos_frac;
            let handle = Rect::from_min_size(
                egui::pos2(sb_track.left() + 1.0, handle_top),
                Vec2::new(sb_w - 2.0, handle_h),
            );
            let hovered = hover_pos.is_some_and(|p| sb_track.contains(p));
            // 暖灰滑块，与全局暖色调一致
            let col = if self.sb_dragging {
                egui::Color32::from_rgb(114, 109, 97)
            } else if hovered {
                egui::Color32::from_rgb(144, 138, 124)
            } else {
                egui::Color32::from_rgb(179, 173, 159)
            };
            painter.rect_filled(handle, 3.0, col);
        }
    }
}
