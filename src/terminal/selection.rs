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
}
