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
pub(super) fn prev_char_boundary(s: &str, b: usize) -> usize {
    s[..b]
        .chars()
        .next_back()
        .map(|c| b - c.len_utf8())
        .unwrap_or(0)
}
pub(super) fn next_char_boundary(s: &str, b: usize) -> usize {
    s[b.min(s.len())..]
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
pub(super) fn byte_to_char(s: &str, b: usize) -> usize {
    s[..b.min(s.len())].chars().count()
}
