//! 终端模拟：用 `vt100` 维护屏幕模型，并在 egui 中以等宽字体逐格渲染；
//! 同时把键盘事件编码为终端字节流。

use egui::{Color32, FontId, Key, Modifiers, Rect, Sense, Stroke, TextFormat, Vec2};
use egui::text::LayoutJob;

/// 默认字号（pt）。
const FONT_SIZE: f32 = 14.0;

pub struct Terminal {
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    scrollback: usize,
    /// 可调字号（Ctrl+滚轮）
    font_size: f32,
    /// 选区两端（屏幕字符坐标 row,col）；None 表示无选区
    sel_anchor: Option<(u16, u16)>,
    sel_cursor: Option<(u16, u16)>,
    /// 系统剪贴板（懒初始化，用于右键粘贴）
    clipboard: Option<arboard::Clipboard>,
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 5000),
            cols: 80,
            rows: 24,
            scrollback: 0,
            font_size: FONT_SIZE,
            sel_anchor: None,
            sel_cursor: None,
            clipboard: None,
        }
    }

    /// 选区按阅读顺序的 (起, 止)（含两端）。
    fn ordered_selection(&self) -> Option<((u16, u16), (u16, u16))> {
        let (a, b) = (self.sel_anchor?, self.sel_cursor?);
        if (a.0, a.1) <= (b.0, b.1) {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    /// 提取选中文本（按行拼接，行尾去除多余空格）。
    fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.ordered_selection()?;
        let screen = self.parser.screen();
        let mut out = String::new();
        for row in sr..=er {
            let c0 = if row == sr { sc } else { 0 };
            let c1 = if row == er { ec } else { self.cols.saturating_sub(1) };
            let mut line = String::new();
            for col in c0..=c1 {
                let ch = screen.cell(row, col).map(|c| c.contents()).unwrap_or_default();
                line.push_str(if ch.is_empty() { " " } else { ch });
            }
            out.push_str(line.trim_end());
            if row != er {
                out.push('\n');
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn has_selection(&self) -> bool {
        matches!((self.sel_anchor, self.sel_cursor), (Some(a), Some(b)) if a != b)
    }

    fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_cursor = None;
    }

    /// 读系统剪贴板（懒初始化）。
    fn read_clipboard(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()?.get_text().ok()
    }

    /// 喂入来自远程的原始字节。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// 调整逻辑尺寸（字符行列）。返回是否真的变化。
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
        true
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// 渲染终端内容。返回本帧用户键盘输入产生的字节流（交给 worker 发送）。
    ///
    /// `focused` 表示终端区域是否持有焦点，决定是否采集键盘事件。
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<u8> {
        let font = FontId::monospace(self.font_size);
        // 以字符 'M' 的宽高度量单元格尺寸
        let (char_w, char_h) = ui.ctx().fonts_mut(|f| {
            let w = f.glyph_width(&font, 'M');
            let h = f.row_height(&font);
            (w, h)
        });
        let cell = Vec2::new(char_w, char_h);

        let avail = ui.available_size();
        // 申请整块区域并捕获键盘/鼠标焦点
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        if resp.clicked() {
            resp.request_focus();
        }
        let focused = resp.has_focus();

        // 关键：终端聚焦时锁定 Tab / 方向键 / Esc，使其传给 shell（修复 Tab 补全），
        // 而不是被 egui 用于控件间焦点切换。
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }

        // 根据可用区域换算行列，必要时上报 resize（由调用方读 size 比较）
        let new_cols = (avail.x / char_w).floor().max(2.0) as u16;
        let new_rows = (avail.y / char_h).floor().max(1.0) as u16;
        self.resize(new_cols, new_rows);

        // 滚轮：Ctrl+滚轮调字号，否则回滚 scrollback
        if resp.hovered() {
            let (scroll, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl || i.modifiers.command));
            if scroll != 0.0 {
                if ctrl {
                    self.font_size = (self.font_size + scroll.signum() * 1.0).clamp(8.0, 32.0);
                } else {
                    let lines = (scroll / char_h).round() as i64;
                    let nb = (self.scrollback as i64 + lines).clamp(0, 5000) as usize;
                    self.scrollback = nb;
                    self.parser.screen_mut().set_scrollback(nb);
                }
            }
        }

        // 鼠标拖拽选择文本（屏幕字符坐标）
        let cell_at = |pos: egui::Pos2| -> (u16, u16) {
            let c = (((pos.x - rect.left()) / char_w).floor() as i32).clamp(0, self.cols as i32 - 1) as u16;
            let r = (((pos.y - rect.top()) / char_h).floor() as i32).clamp(0, self.rows as i32 - 1) as u16;
            (r, c)
        };
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                let c = cell_at(p);
                self.sel_anchor = Some(c);
                self.sel_cursor = Some(c);
            }
        } else if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.sel_cursor = Some(cell_at(p));
            }
        }
        if resp.clicked() {
            self.clear_selection();
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, crate::theme::Palette::TERM_BG);

        let sel = self.ordered_selection();
        let screen = self.parser.screen();
        let origin = rect.min;
        for row in 0..self.rows {
            let mut job = LayoutJob::default();
            let mut run = String::new();
            let mut run_fmt: Option<TextFormat> = None;

            let flush =
                |job: &mut LayoutJob, run: &mut String, fmt: &mut Option<TextFormat>| {
                    if let Some(f) = fmt.take() {
                        if !run.is_empty() {
                            job.append(run, 0.0, f);
                        }
                    }
                    run.clear();
                };

            for col in 0..self.cols {
                // vt100::Cell::contents() 返回 &str（可能为空，代表空格）
                let (ch, fmt): (&str, TextFormat) = match screen.cell(row, col) {
                    Some(c) => {
                        let s = c.contents();
                        (if s.is_empty() { " " } else { s }, cell_format(c, &font))
                    }
                    None => (" ", default_format(&font)),
                };
                match &run_fmt {
                    Some(prev) if same_format(prev, &fmt) => run.push_str(ch),
                    _ => {
                        flush(&mut job, &mut run, &mut run_fmt);
                        run.push_str(ch);
                        run_fmt = Some(fmt);
                    }
                }
            }
            flush(&mut job, &mut run, &mut run_fmt);

            let pos = origin + Vec2::new(0.0, row as f32 * char_h);
            // 先绘制该行的背景块（处理非默认底色）
            paint_row_backgrounds(&painter, screen, row, self.cols, origin, cell);
            // 选区高亮（半透明，文字仍可见）
            if let Some(((sr, sc), (er, ec))) = sel {
                if row >= sr && row <= er {
                    let c0 = if row == sr { sc } else { 0 };
                    let c1 = if row == er { ec } else { self.cols.saturating_sub(1) };
                    let x0 = origin.x + c0 as f32 * char_w;
                    let x1 = origin.x + (c1 as f32 + 1.0) * char_w;
                    let y = origin.y + row as f32 * char_h;
                    let a = crate::theme::Palette::ACCENT;
                    painter.rect_filled(
                        Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + char_h)),
                        0.0,
                        Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 80),
                    );
                }
            }
            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
            painter.galley(pos, galley, Color32::WHITE);
        }

        // 光标
        if !screen.hide_cursor() && self.scrollback == 0 {
            let (cr, cc) = screen.cursor_position();
            let cpos = origin + Vec2::new(cc as f32 * char_w, cr as f32 * char_h);
            let crect = Rect::from_min_size(cpos, cell);
            let color = if focused {
                crate::theme::Palette::ACCENT
            } else {
                Color32::from_gray(150)
            };
            if focused {
                painter.rect_filled(crect, 1.0, color.gamma_multiply(0.6));
            } else {
                painter.rect_stroke(crect, 1.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
            }
        }

        // 键盘输入
        let mut out = if focused { self.collect_input(ui) } else { Vec::new() };

        // 复制/粘贴：Ctrl+Shift+C / Ctrl+Shift+V，以及右键菜单
        let (copy_sc, paste_sc) = ui.input(|i| {
            let m = i.modifiers.command || i.modifiers.ctrl;
            (
                m && i.modifiers.shift && i.key_pressed(Key::C),
                m && i.modifiers.shift && i.key_pressed(Key::V),
            )
        });
        let mut do_copy = copy_sc;
        let mut do_paste = paste_sc;
        resp.context_menu(|ui| {
            let sel = self.has_selection();
            if ui.add_enabled(sel, egui::Button::new("复制")).clicked() {
                do_copy = true;
                ui.close();
            }
            if ui.button("粘贴").clicked() {
                do_paste = true;
                ui.close();
            }
        });
        if do_copy {
            if let Some(t) = self.selected_text() {
                ui.ctx().copy_text(t);
            }
        }
        if do_paste {
            if let Some(t) = self.read_clipboard() {
                out.extend_from_slice(t.as_bytes());
            }
        }
        out
    }

    /// 把本帧键盘事件编码为终端字节。
    fn collect_input(&self, ui: &egui::Ui) -> Vec<u8> {
        let mut out = Vec::new();
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Text(t) => out.extend_from_slice(t.as_bytes()),
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        encode_key(*key, *modifiers, &mut out);
                    }
                    egui::Event::Paste(t) => out.extend_from_slice(t.as_bytes()),
                    _ => {}
                }
            }
        });
        out
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

/// 把特殊按键编码为 ANSI 转义序列；Ctrl 组合键编码为控制字符。
fn encode_key(key: Key, mods: Modifiers, out: &mut Vec<u8>) {
    // Ctrl+Shift+C/V 保留给复制/粘贴，不作为终端输入
    if (mods.ctrl || mods.command) && mods.shift && matches!(key, Key::C | Key::V) {
        return;
    }
    // Ctrl + 字母 -> 0x01..0x1a
    if mods.ctrl {
        if let Some(c) = key_to_ascii_letter(key) {
            out.push((c as u8 - b'a') + 1);
            return;
        }
    }
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => out.push(0x7f),
        Key::Tab => out.push(b'\t'),
        Key::Escape => out.push(0x1b),
        Key::ArrowUp => out.extend_from_slice(b"\x1b[A"),
        Key::ArrowDown => out.extend_from_slice(b"\x1b[B"),
        Key::ArrowRight => out.extend_from_slice(b"\x1b[C"),
        Key::ArrowLeft => out.extend_from_slice(b"\x1b[D"),
        Key::Home => out.extend_from_slice(b"\x1b[H"),
        Key::End => out.extend_from_slice(b"\x1b[F"),
        Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
        Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
        Key::Insert => out.extend_from_slice(b"\x1b[2~"),
        Key::Delete => out.extend_from_slice(b"\x1b[3~"),
        _ => {}
    }
}

fn key_to_ascii_letter(key: Key) -> Option<char> {
    use Key::*;
    let c = match key {
        A => 'a', B => 'b', C => 'c', D => 'd', E => 'e', F => 'f', G => 'g',
        H => 'h', I => 'i', J => 'j', K => 'k', L => 'l', M => 'm', N => 'n',
        O => 'o', P => 'p', Q => 'q', R => 'r', S => 's', T => 't', U => 'u',
        V => 'v', W => 'w', X => 'x', Y => 'y', Z => 'z',
        _ => return None,
    };
    Some(c)
}

/// 把 vt100 颜色映射到 egui 颜色（含 256 色板）。
fn vt_color(c: vt100::Color, default: Color32) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => xterm256(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn cell_format(c: &vt100::Cell, font: &FontId) -> TextFormat {
    let fg_default = crate::theme::Palette::TERM_FG;
    let mut fg = vt_color(c.fgcolor(), fg_default);
    // 反显：文字改用背景色（实际背景块在 paint_row_backgrounds 中绘制）
    if c.inverse() {
        fg = vt_color(c.bgcolor(), crate::theme::Palette::TERM_BG);
    }
    let mut f = TextFormat {
        font_id: font.clone(),
        color: fg,
        ..Default::default()
    };
    if c.underline() {
        f.underline = Stroke::new(1.0, fg);
    }
    f
}

fn default_format(font: &FontId) -> TextFormat {
    TextFormat {
        font_id: font.clone(),
        color: crate::theme::Palette::TERM_FG,
        ..Default::default()
    }
}

fn same_format(a: &TextFormat, b: &TextFormat) -> bool {
    a.color == b.color && a.underline == b.underline
}

/// 逐格绘制非默认背景色（egui 文本布局不便携带逐段背景，单独画矩形）。
fn paint_row_backgrounds(
    painter: &egui::Painter,
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
    origin: egui::Pos2,
    cell: Vec2,
) {
    for col in 0..cols {
        if let Some(c) = screen.cell(row, col) {
            let mut bg = vt_color(c.bgcolor(), Color32::TRANSPARENT);
            if c.inverse() {
                bg = vt_color(c.fgcolor(), crate::theme::Palette::TERM_FG);
            }
            if bg != Color32::TRANSPARENT {
                let pos = origin + Vec2::new(col as f32 * cell.x, row as f32 * cell.y);
                painter.rect_filled(Rect::from_min_size(pos, cell), 0.0, bg);
            }
        }
    }
}

/// xterm 256 色板。
fn xterm256(i: u8) -> Color32 {
    // 适配暖色浅背景的 ANSI 16 色（灰阶偏暖，彩色保持清晰）
    const BASE: [(u8, u8, u8); 16] = [
        (0x3a, 0x38, 0x33), (0xc0, 0x4b, 0x3f), (0x4f, 0x86, 0x4a), (0xb5, 0x82, 0x2e),
        (0x2f, 0x6f, 0xb0), (0xa6, 0x55, 0x9d), (0x2b, 0x8a, 0x8f), (0xb8, 0xb2, 0xa3),
        (0x6f, 0x6b, 0x61), (0xc0, 0x56, 0x4b), (0x5b, 0x8a, 0x56), (0xc2, 0x8e, 0x3c),
        (0x35, 0x78, 0xbb), (0xb0, 0x60, 0xa6), (0x30, 0x95, 0x9a), (0x55, 0x52, 0x4a),
    ];
    match i {
        0..=15 => {
            let (r, g, b) = BASE[i as usize];
            Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(conv(r), conv(g), conv(b))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}
