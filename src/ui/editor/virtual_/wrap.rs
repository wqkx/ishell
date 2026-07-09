//! 自动换行视觉行映射与行缓存重算。

use super::super::Editor;
use super::fold::v_line_hidden;
use super::geom::{char_to_byte, compute_line_starts, v_line_of, v_line_range};

// ——— 自动换行（word-wrap）视觉行映射 ———
/// 某逻辑行按 cols 列折行后的视觉行数（按字符数近似，CJK 暂按 1 列计）。
pub(super) fn line_vrows(chars: usize, cols: usize) -> u32 {
    (chars / cols + if !chars.is_multiple_of(cols) { 1 } else { 0 }).max(1) as u32
}
/// 同步换行行数前缀和缓存（列宽/内容/折叠变化时重算）。折叠区域内的行占 0 视觉行。
pub(super) fn v_wrap_sync(ed: &mut Editor, cols: usize) {
    let cols = cols.max(1);
    if ed.vrow_cols == cols && ed.vrow_ver == ed.vver && ed.vrow_fver == ed.fold_ver && ed.vrow_pre.len() == ed.vlines.len() + 1 {
        return;
    }
    let n = ed.vlines.len();
    let mut pre = Vec::with_capacity(n + 1);
    let mut acc = 0u32;
    pre.push(0);
    for i in 0..n {
        if v_line_hidden(ed, i) {
            pre.push(acc);
            continue;
        }
        let (s, e) = v_line_range(ed, i);
        let chars = ed.content[s..e].chars().count();
        acc = acc.saturating_add(line_vrows(chars, cols));
        pre.push(acc);
    }
    ed.vrow_pre = pre;
    ed.vrow_cols = cols;
    ed.vrow_ver = ed.vver;
    ed.vrow_fver = ed.fold_ver;
}
pub(super) fn v_total_vrows(ed: &Editor) -> usize {
    ed.vrow_pre.last().copied().unwrap_or(0) as usize
}
/// 视觉行号 → (逻辑行, 段内序号)。
pub(super) fn v_line_of_vrow(ed: &Editor, vrow: usize) -> (usize, usize) {
    let v = vrow as u32;
    let line = ed.vrow_pre.partition_point(|&p| p <= v).saturating_sub(1).min(ed.vlines.len().saturating_sub(1));
    let seg = vrow - ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (line, seg)
}
/// 字节偏移 → (视觉行, 段内列)。
pub(super) fn v_vpos_of_byte(ed: &Editor, byte: usize, cols: usize) -> (usize, usize) {
    let cols = cols.max(1);
    let line = v_line_of(ed, byte);
    let (ls, _) = v_line_range(ed, line);
    let col = ed.content[ls..byte.max(ls)].chars().count();
    let base = ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (base + col / cols, col % cols)
}
/// (视觉行, 段内列) → 字节偏移（钳到行尾）。
pub(super) fn v_byte_of_vpos(ed: &Editor, vrow: usize, vcol: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    let (line, seg) = v_line_of_vrow(ed, vrow);
    let (ls, le) = v_line_range(ed, line);
    let line_chars = ed.content[ls..le].chars().count();
    let col = (seg * cols + vcol).min(line_chars);
    ls + char_to_byte(&ed.content[ls..le], col)
}
pub fn v_recompute(ed: &mut Editor) {
    ed.vver = ed.vver.wrapping_add(1); // 内容变更 → 换行行数缓存失效
    ed.vlines = compute_line_starts(&ed.content);
    // 最长行字节数（含尾行）——缓存，渲染时直接用，避免每帧扫全部行
    ed.vmax = ed
        .vlines
        .windows(2)
        .map(|w| w[1] - w[0])
        .chain(std::iter::once(ed.content.len() - ed.vlines.last().copied().unwrap_or(0)))
        .max()
        .unwrap_or(0);
}
