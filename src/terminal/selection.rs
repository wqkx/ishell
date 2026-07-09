//! Selection, word picking, and clipboard helpers.

use super::Terminal;

impl Terminal {
    pub(super) fn ordered_selection(&self) -> Option<((u16, u16), (u16, u16))> {
        let (a, b) = (self.sel_anchor?, self.sel_cursor?);
        if (a.0, a.1) <= (b.0, b.1) {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    pub(super) fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.ordered_selection()?;
        let screen = self.parser.screen();
        let mut out = String::new();
        for row in sr..=er {
            let c0 = if row == sr { sc } else { 0 };
            let c1 = if row == er {
                ec
            } else {
                self.cols.saturating_sub(1)
            };
            let mut line = String::new();
            for col in c0..=c1 {
                let Some(cell) = screen.cell(row, col) else {
                    line.push(' ');
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let ch = cell.contents();
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
