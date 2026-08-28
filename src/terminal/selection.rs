//! Selection, word picking, and clipboard helpers.

use super::Terminal;

impl Terminal {
    /// 视图行 → 绝对历史行（锚定内容：滚动/新输出/修剪均不漂移）。
    pub(super) fn abs_of_view(&self, view_row: u16) -> usize {
        let screen = self.parser.screen();
        screen.scrollback_total() - screen.scrollback() + view_row as usize
    }

    pub(super) fn ordered_selection(&self) -> Option<((usize, u16), (usize, u16))> {
        let (a, b) = (self.sel_anchor?, self.sel_cursor?);
        if (a.0, a.1) <= (b.0, b.1) {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    pub(super) fn selected_text(&mut self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.ordered_selection()?;
        let (total, kept, saved) = {
            let s = self.parser.screen();
            (s.scrollback_total(), s.scrollback_rows(), s.scrollback())
        };
        let mut out = String::new();
        for abs in sr..=er {
            // 已被修剪丢弃的历史行（abs < total - kept）读不到——跳过
            if abs + kept < total {
                continue;
            }
            // 把目标行切到视图第 0 行再读：abs > total 时它在当前屏幕区（偏移 0，
            // 视图行 = abs - total）；否则偏移 total - abs 使其成为视图第 0 行。
            // set_scrollback 是 O(1)（仅改偏移），逐行调整代价可忽略。
            let s_prime = total.saturating_sub(abs);
            self.parser.screen_mut().set_scrollback(s_prime);
            let view_row = (abs - (total - s_prime)) as u16;
            let c0 = if abs == sr { sc } else { 0 };
            let c1 = if abs == er {
                ec
            } else {
                self.cols.saturating_sub(1)
            };
            let mut line = String::new();
            let screen = self.parser.screen();
            for col in c0..=c1 {
                let Some(cell) = screen.cell(view_row, col) else {
                    line.push(' ');
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let ch = cell.contents();
                line.push_str(if ch.is_empty() { " " } else { ch });
            }
            // 「软换行」（一条长逻辑行被终端折到下一屏幕行）不能当成换行符复制出去：
            // 它在原文里根本没有 \n，粘贴时凭空多出的换行会把一条命令/一个 URL 拆断。
            // vt100 给每行记了 wrapped 标志（行满后自动折行时置位，真正收到 \n 则清零），
            // 据此区分：软换行只把两行首尾相接，真实换行才补 \n。
            let soft_wrap = screen.row_wrapped(view_row);
            // 软换行行是被字符填满才折的，行尾没有真实空白可言；trim_end 会把「刚好在行尾
            // 的空格」这种有意义的内容吃掉，导致接起来的两段粘连（如 `ls -la` 变 `ls-la`）。
            if soft_wrap {
                out.push_str(&line);
            } else {
                out.push_str(line.trim_end());
            }
            if abs != er && !soft_wrap {
                out.push('\n');
            }
        }
        // 恢复用户当前的回看位置（上面的逐行偏移调整只是临时探测）
        self.parser.screen_mut().set_scrollback(saved);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub(super) fn has_selection(&self) -> bool {
        matches!((self.sel_anchor, self.sel_cursor), (Some(a), Some(b)) if a != b)
    }

    /// Shift+方向/Home/End/PageUp/PageDown 的本地键盘选区（Windows Terminal 式）。
    /// 首次按下以终端光标位置为锚，之后垂直/翻页扩展；游标移出视图时滚动跟随。
    /// 仅在「主屏且远端未接管键盘」时由 input.rs 调用（vim 等程序下该组合键需透传）。
    pub(super) fn shift_select(&mut self, key: egui::Key) {
        use egui::Key;
        // 键盘交互先回底（与「输入即回底」的既有行为一致）——回看中光标位置的
        // 绝对行换算会错（光标属于当前屏幕区而非历史区）
        if self.scrollback != 0 {
            self.parser.screen_mut().set_scrollback(0);
            self.scrollback = 0;
        }
        let (rows, cols) = (self.rows as usize, self.cols);
        // 首次按下：锚定终端光标处（无选区时 Shift+点击/拖动已建的选区上继续扩展亦可）
        if self.sel_anchor.is_none() {
            let (cr, cc) = self.parser.screen().cursor_position();
            let a = self.abs_of_view(cr);
            self.sel_anchor = Some((a, cc));
            self.sel_cursor = Some((a, cc));
        }
        let Some((mut ar, mut ac)) = self.sel_cursor else {
            return;
        };
        let (total, kept) = {
            let s = self.parser.screen();
            (s.scrollback_total(), s.scrollback_rows())
        };
        let abs_min = total - kept; // 最早可读历史行（再早已被修剪）
        let abs_max = total + rows - 1; // 当前屏幕最后一行
        match key {
            Key::ArrowUp => ar = ar.saturating_sub(1).max(abs_min),
            Key::ArrowDown => ar = (ar + 1).min(abs_max),
            Key::PageUp => ar = ar.saturating_sub(rows).max(abs_min),
            Key::PageDown => ar = (ar + rows).min(abs_max),
            Key::Home => ac = 0,
            Key::End => ac = cols.saturating_sub(1),
            _ => {}
        }
        self.sel_cursor = Some((ar, ac));
        // 游标越出视图时滚动跟随：上越顶则上滚，下越底则下滚
        let s = self.parser.screen().scrollback();
        let view_top = total - s;
        if ar < view_top {
            let ns = total - ar;
            self.parser.screen_mut().set_scrollback(ns);
            self.scrollback = ns;
        } else if ar >= view_top + rows {
            // view_top_new = ar - rows + 1 → ns = total - view_top_new
            let ns = (total + rows - 1).saturating_sub(ar);
            self.parser.screen_mut().set_scrollback(ns);
            self.scrollback = ns;
        }
    }

    pub(super) fn word_range_at(&self, row: u16, col: u16) -> Option<(u16, u16)> {
        let screen = self.parser.screen();
        let is_word = |c: u16| -> bool {
            match screen.cell(row, c) {
                Some(cell) => {
                    if cell.is_wide_continuation() {
                        return true;
                    }
                    let s = cell.contents();
                    if s.is_empty() {
                        return false;
                    }
                    s.chars().any(|ch| {
                        ch.is_alphanumeric()
                            || "_-./~:@+#%".contains(ch)
                            || (!ch.is_ascii() && !ch.is_whitespace() && !ch.is_control())
                    })
                }
                None => false,
            }
        };
        if !is_word(col) {
            return None;
        }
        let mut c0 = col;
        while c0 > 0 && is_word(c0 - 1) {
            c0 -= 1;
        }
        let mut c1 = col;
        while c1 + 1 < self.cols && is_word(c1 + 1) {
            c1 += 1;
        }
        Some((c0, c1))
    }

    pub(super) fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_cursor = None;
    }

    pub(super) fn read_clipboard(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()?.get_text().ok()
    }

    /// 剪贴板里若是图片，编码成 PNG 存进 `paste_image`，等 App 取走。
    ///
    /// 编码放在这里（而不是交给 App 拿原始 RGBA）是因为 arboard 的 `ImageData` 借的是
    /// 剪贴板的生命周期，跨帧传出去很别扭；PNG 字节自包含，还顺手压掉了一大截体积
    /// （一张 4K 截图的裸 RGBA 有 30MB+）。
    pub(super) fn grab_clipboard_image(&mut self) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let Some(img) = self.clipboard.as_mut().and_then(|c| c.get_image().ok()) else {
            return;
        };
        let (w, h) = (img.width as u32, img.height as u32);
        let Some(buf) = image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) else {
            log::warn!("剪贴板图片尺寸与字节数不匹配（{w}x{h}），忽略");
            return;
        };
        let mut png = std::io::Cursor::new(Vec::new());
        match buf.write_to(&mut png, image::ImageFormat::Png) {
            Ok(()) => self.paste_image = Some(png.into_inner()),
            Err(e) => log::warn!("剪贴板图片编码 PNG 失败：{e}"),
        }
    }

    /// 给粘贴内容套上 bracketed paste 括号（远端开了 `CSI ?2004h` 时）。
    ///
    /// 不套的话，多行粘贴会被 shell / TUI 当成一行行**敲进去**：每个换行都是一次回车，
    /// 粘一段脚本进去就等于逐行执行了它，粘一段多行提示词给 Claude Code 则会被拆成好几次
    /// 提交。开了 bracketed paste 的程序（bash 5、zsh、ipython、几乎所有 Ink/TUI 应用）
    /// 靠这对括号把「粘贴」和「键入」区分开。
    ///
    /// 内容里出现的结束标记必须剔除，否则粘贴内容可以自己「关掉」括号、让后半段重新
    /// 变成键入——这是 bracketed paste 众所周知的注入面。
    pub(super) fn wrap_paste(&self, text: &[u8]) -> Vec<u8> {
        if !self.parser.screen().bracketed_paste() {
            return text.to_vec();
        }
        const END: &[u8] = b"\x1b[201~";
        let mut body = text.to_vec();
        while let Some(i) = body
            .windows(END.len())
            .position(|w| w == END)
        {
            body.drain(i..i + END.len());
        }
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(&body);
        out.extend_from_slice(END);
        out
    }
}
