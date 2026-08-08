//! 行/字节/字符几何辅助。

use super::super::Editor;

pub(super) fn compute_line_starts(s: &str) -> Vec<usize> {
    let mut v = Vec::with_capacity(s.len() / 40 + 1);
    v.push(0);
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}
/// 前一个字符边界。入参先吸附到合法边界——同 `byte_to_char`：组字期间 `vcaret` 可能被
/// 一段陈旧的 IME 区间推到多字节字符中间（典型路径：组字直接改 content 不入撤销栈，
/// 用户随即 Ctrl+Z，`v_undo` 把 `vcaret` 设成另一个形状的缓冲区留下的偏移），此后第一次
/// 退格/左移就会崩在 `s[..b]` 上。这里和下面 `next_char_boundary` 一并收口。
pub(super) fn prev_char_boundary(s: &str, b: usize) -> usize {
    let b = crate::ui::ime_safe::floor_boundary(s, b);
    s[..b]
        .chars()
        .next_back()
        .map(|c| b - c.len_utf8())
        .unwrap_or(0)
}
pub(super) fn next_char_boundary(s: &str, b: usize) -> usize {
    let b = crate::ui::ime_safe::floor_boundary(s, b);
    s[b..]
        .chars()
        .next()
        .map(|c| b + c.len_utf8())
        .unwrap_or_else(|| s.len())
}
pub fn v_line_of(ed: &Editor, b: usize) -> usize {
    ed.vlines.partition_point(|&s| s <= b).saturating_sub(1)
}
/// 第 i 行的字节范围 [起, 止)（止不含行尾换行符）。
pub(super) fn v_line_range(ed: &Editor, i: usize) -> (usize, usize) {
    let s = ed.vlines[i];
    let e = if i + 1 < ed.vlines.len() {
        ed.vlines[i + 1] - 1
    } else {
        ed.content.len()
    };
    (s, e)
}
pub fn v_sel_range(ed: &Editor) -> Option<(usize, usize)> {
    ed.vsel
        .map(|a| (a.min(ed.vcaret), a.max(ed.vcaret)))
        .filter(|(a, b)| a < b)
}
pub(super) fn char_to_byte(s: &str, c: usize) -> usize {
    s.char_indices().nth(c).map(|(b, _)| b).unwrap_or(s.len())
}

/// 字节偏移 → 字符下标。
///
/// 非字符边界向下取整：绘制路径上的 `x_of`/`col_of` 会拿 `vcaret` 来算屏幕坐标，而组字
/// 期间 `vcaret` 可能被一段陈旧的 IME 区间推到多字节字符中间——直接 `s[..b]` 就是 panic，
/// 而且崩在绘制里、看不出跟输入法有关。这里兜住，不允许一个坏偏移把整个应用带走。
pub(super) fn byte_to_char(s: &str, b: usize) -> usize {
    crate::ui::ime_safe::char_of_byte(s, b)
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    /// 组字期间光标可能被一段陈旧的 IME 区间推到多字节字符中间（组字直接改 content 不入
    /// 撤销栈，随后 Ctrl+Z 把 vcaret 设成另一个形状缓冲区留下的偏移）。此后第一次退格/
    /// 左移会打到 `prev_char_boundary`、右移打到 `next_char_boundary`——修复前两者都是
    /// `s[..b]` / `s[b..]` 直接切片，非边界即 panic。
    #[test]
    fn char_boundary_helpers_survive_a_mid_character_offset() {
        let s = "中文abc"; // 「中」=0..3，「文」=3..6
        assert!(!s.is_char_boundary(4) && !s.is_char_boundary(5));
        assert_eq!(prev_char_boundary(s, 5), 0, "5 吸附到 3，前一个边界是 0");
        assert_eq!(next_char_boundary(s, 5), 6, "5 吸附到 3，下一个边界是 6");
        assert_eq!(byte_to_char(s, 5), 1);
    }

    /// 缓冲区被换短后旧偏移整体越界：`prev_char_boundary` 原先连 `.min(len)` 都没有。
    #[test]
    fn char_boundary_helpers_survive_an_out_of_range_offset() {
        let s = "ab";
        assert_eq!(prev_char_boundary(s, 999), 1);
        assert_eq!(next_char_boundary(s, 999), 2);
    }

    /// 正常情形不得被上面的吸附改变行为。
    #[test]
    fn char_boundary_helpers_are_unchanged_on_valid_offsets() {
        let s = "a中b";
        assert_eq!(prev_char_boundary(s, 4), 1);
        assert_eq!(next_char_boundary(s, 1), 4);
        assert_eq!(prev_char_boundary(s, 0), 0);
        assert_eq!(next_char_boundary(s, 5), 5);
    }
}
