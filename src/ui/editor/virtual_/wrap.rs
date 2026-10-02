//! 自动换行视觉行映射与行缓存重算。
//!
//! 一条逻辑行按视口宽度折成若干「段」，每段是一个视觉行。段的边界按**显示宽度**贪心切：
//! 一个字符放不下就整个进下一段（不劈开宽字符），每段至少一个字符。宽度来自
//! `geom::Metric`（字体实测）。非换行模式是容量无穷大的特例：每行恰好一段。

use std::collections::HashMap;

use super::super::Editor;
use super::fold::v_line_hidden;
use super::geom::{
    byte_at_cols_ceil, byte_at_cols_floor, byte_near_cols, compute_line_starts, metric_epoch,
    str_cols, v_line_of, v_line_range, with_metric,
};

/// 超过这个字节数的行算「长行」：折段位置进缓存，免得每个可见段都从行首重扫一遍。
const LONG_LINE: usize = 4096;

/// 长行的折段位置缓存：行号 → 各段起始字节（行内偏移）。
///
/// 只缓存被问到的长行（可见的那几条、光标所在的那条），不给整个文件建表。键里的三样
/// 任何一个变了（内容、列数、字宽度量）就整体作废。
#[derive(Default)]
pub struct SegCache {
    key: (u64, usize, u64),
    map: HashMap<usize, Vec<u32>>,
}

/// 每段可容纳的列数。非换行模式传进来的是个超大值，当作无穷大（每行一段）。
fn cap_of(cols: usize) -> f32 {
    if cols >= usize::MAX / 8 {
        f32::INFINITY
    } else {
        cols.max(1) as f32
    }
}

/// 一行各段的起始字节偏移（首段恒为 0）。
pub(super) fn seg_starts(line: &str, cap: f32) -> Vec<u32> {
    let mut starts = vec![0u32];
    if !cap.is_finite() {
        return starts;
    }
    with_metric(|m| {
        let mut x = 0.0f32;
        for (i, c) in line.char_indices() {
            let w = m.w(c);
            // 放不下：这个字符整个进下一段（x > 0 保证每段至少一个字符，哪怕它比一整行还宽）
            if x > 0.0 && x + w > cap + 1e-3 {
                starts.push(i as u32);
                x = 0.0;
            }
            x += w;
        }
    });
    starts
}

/// 一行折成几段（≥1）。纯 ASCII 且无 Tab 的行每个字符正好一列，直接按长度除。
fn seg_count(line: &str, cap: f32) -> u32 {
    if !cap.is_finite() {
        return 1;
    }
    if line.is_ascii() && !line.as_bytes().contains(&b'\t') {
        let per = cap.floor().max(1.0) as usize;
        return line.len().div_ceil(per).max(1) as u32;
    }
    with_metric(|m| {
        let (mut n, mut x) = (1u32, 0.0f32);
        for c in line.chars() {
            let w = m.w(c);
            if x > 0.0 && x + w > cap + 1e-3 {
                n += 1;
                x = 0.0;
            }
            x += w;
        }
        n
    })
}

/// 取某逻辑行的折段位置给 `f` 用。短行现算，长行走缓存。
fn with_segs<R>(ed: &Editor, line: usize, cols: usize, f: impl FnOnce(&[u32]) -> R) -> R {
    let (ls, le) = v_line_range(ed, line);
    let text = &ed.content[ls..le];
    let cap = cap_of(cols);
    if !cap.is_finite() {
        return f(&[0]);
    }
    if text.len() <= LONG_LINE {
        return f(&seg_starts(text, cap));
    }
    let key = (ed.vver, cols, metric_epoch());
    let mut cache = ed.vseg_cache.borrow_mut();
    if cache.key != key || cache.map.len() > 256 {
        cache.key = key;
        cache.map.clear();
    }
    f(cache
        .map
        .entry(line)
        .or_insert_with(|| seg_starts(text, cap)))
}

/// 某逻辑行第 `seg` 段的字节范围（行内偏移）。
pub(super) fn v_seg_range(ed: &Editor, line: usize, seg: usize, cols: usize) -> (usize, usize) {
    let (ls, le) = v_line_range(ed, line);
    let len = le - ls;
    with_segs(ed, line, cols, |starts| {
        let seg = seg.min(starts.len() - 1);
        let a = starts[seg] as usize;
        let b = starts.get(seg + 1).map_or(len, |&s| s as usize);
        (a, b)
    })
}

/// 同步换行行数前缀和缓存（列宽/内容/折叠/字宽度量变化时重算）。折叠区域内的行占 0 视觉行。
pub(super) fn v_wrap_sync(ed: &mut Editor, cols: usize) {
    let cols = cols.max(1);
    let epoch = metric_epoch();
    if ed.vrow_cols == cols
        && ed.vrow_ver == ed.vver
        && ed.vrow_fver == ed.fold_ver
        && ed.vrow_wepoch == epoch
        && ed.vrow_pre.len() == ed.vlines.len() + 1
    {
        return;
    }
    let cap = cap_of(cols);
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
        acc = acc.saturating_add(seg_count(&ed.content[s..e], cap));
        pre.push(acc);
    }
    ed.vrow_pre = pre;
    ed.vrow_cols = cols;
    ed.vrow_ver = ed.vver;
    ed.vrow_fver = ed.fold_ver;
    ed.vrow_wepoch = epoch;
}
pub(super) fn v_total_vrows(ed: &Editor) -> usize {
    ed.vrow_pre.last().copied().unwrap_or(0) as usize
}
/// 视觉行号 → (逻辑行, 段内序号)。
pub(super) fn v_line_of_vrow(ed: &Editor, vrow: usize) -> (usize, usize) {
    let v = vrow as u32;
    let line = ed
        .vrow_pre
        .partition_point(|&p| p <= v)
        .saturating_sub(1)
        .min(ed.vlines.len().saturating_sub(1));
    let seg = vrow - ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (line, seg)
}
/// 字节偏移 → (视觉行, 段内显示列)。
///
/// 段边界上的位置属于**后一段**的开头；行末属于最后一段的末尾——两条都由「找最后一个起点
/// 不超过它的段」自然得出，不需要特判。
pub(super) fn v_vpos_of_byte(ed: &Editor, byte: usize, cols: usize) -> (usize, usize) {
    let line = v_line_of(ed, byte);
    let (ls, le) = v_line_range(ed, line);
    let text = &ed.content[ls..le];
    let lb = crate::ui::ime_safe::floor_boundary(text, byte.saturating_sub(ls));
    let (seg, a) = with_segs(ed, line, cols, |starts| {
        let seg = starts
            .partition_point(|&s| s as usize <= lb)
            .saturating_sub(1);
        (seg, starts[seg] as usize)
    });
    let base = ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (base + seg, str_cols(&text[a..lb]).round() as usize)
}
/// (视觉行, 段内显示列) → 字节偏移（落在离该列最近的字符边界上）。
pub(super) fn v_byte_of_vpos(ed: &Editor, vrow: usize, vcol: usize, cols: usize) -> usize {
    let (line, seg) = v_line_of_vrow(ed, vrow);
    let (ls, le) = v_line_range(ed, line);
    let text = &ed.content[ls..le];
    let (a, b) = v_seg_range(ed, line, seg, cols);
    let mut off = a + byte_near_cols(&text[a..b], vcol as f32);
    // 落到段尾、而这一行后面还有段：段尾 = 下一段开头，光标会画到下一行去。留在本段里。
    if off >= b && b < text.len() && b > a {
        off = super::geom::prev_char_boundary(text, b);
    }
    ls + off
}

/// 一个视觉行要绘制 / 命中测试的那段文本。
pub(super) struct RowWin {
    pub(super) line: usize,
    /// 文本片段的字节范围（行内偏移）
    pub(super) a: usize,
    pub(super) b: usize,
    /// 片段起点的显示位置（列）：换行模式恒为 0；非换行模式是横向窗口左端那个字符的位置
    pub(super) x0: f32,
    /// 是否该逻辑行的第一段
    pub(super) first: bool,
    /// 片段右端是否就是行末（换行模式下：最后一段）
    pub(super) to_end: bool,
}

/// 视觉行 `row` 的文本窗口。换行模式取对应的折段；非换行模式取横向可视窗口
///（从 `first_col` 列起、`cols_vis` 列宽）——窗口两端都落在字符边界上，不劈开宽字符。
pub(super) fn v_row_window(
    ed: &Editor,
    row: usize,
    wrap: bool,
    cols: usize,
    first_col: usize,
    cols_vis: usize,
) -> RowWin {
    let (line, seg) = v_line_of_vrow(ed, row);
    let (ls, le) = v_line_range(ed, line);
    let text = &ed.content[ls..le];
    if wrap {
        let (a, b) = v_seg_range(ed, line, seg, cols);
        return RowWin {
            line,
            a,
            b,
            x0: 0.0,
            first: seg == 0,
            to_end: b >= text.len(),
        };
    }
    let (a, x0) = byte_at_cols_floor(text, first_col as f32);
    // 起点为了不劈开宽字符可能比 first_col 靠左一点：右端要补上这段，窗口才盖得满视口
    let b = a + byte_at_cols_ceil(&text[a..], cols_vis as f32 + (first_col as f32 - x0));
    RowWin {
        line,
        a,
        b,
        x0,
        first: true,
        to_end: b >= text.len(),
    }
}

pub fn v_recompute(ed: &mut Editor) {
    ed.vver = ed.vver.wrapping_add(1); // 内容变更 → 换行行数缓存失效
    ed.vlines = compute_line_starts(&ed.content);
    // 最宽行的宽度估计（列）——缓存，渲染时直接用来定横向滚动范围，避免每帧扫全部行。
    // 用「字节数 + 每个 Tab 再加 3」：多字节字符的字节数总不小于它占的列数（汉字 3 字节
    // 约 1.7 列），Tab 是 1 字节占 4 列，单看字节数会低估。
    let (mut max, mut cur) = (0usize, 0usize);
    for &b in ed.content.as_bytes() {
        match b {
            b'\n' => {
                max = max.max(cur);
                cur = 0;
            }
            b'\t' => cur += 4,
            _ => cur += 1,
        }
    }
    ed.vmax = max.max(cur);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 折行按**显示宽度**算，不是按字符数：汉字比字母宽，一行能放 4 个字母的宽度放不下
    /// 4 个汉字。按字符数折的话，纯中文行每段都比视口宽，右半截被裁掉、看不到也点不到。
    ///（没有字体可量时宽字符按 2 列计，见 `geom::char_cols`。）
    #[test]
    fn wrapping_counts_display_width_not_characters() {
        let rows = |text: &str, cols: usize| {
            let mut ed = Editor::new("/tmp/a.txt".into(), text.into());
            v_recompute(&mut ed);
            v_wrap_sync(&mut ed, cols);
            v_total_vrows(&ed)
        };
        assert_eq!(rows("中文中文中", 4), 3, "5 个汉字、每行 4 列：中文|中文|中");
        assert_eq!(rows("ab中c", 4), 2, "ab中 正好 4 列，c 进下一段");
        assert_eq!(rows("abc中", 4), 2, "中 放不下：整个字进下一段，不劈开");
        assert_eq!(rows("\tab", 4), 2, "Tab 占 4 列");
        assert_eq!(rows("abcd", 4), 1);
        assert_eq!(rows("", 4), 1);
    }

    const SAMPLES: &[&str] = &[
        "纯中文的一行没有任何半角字符而且相当长需要折好几次才放得下",
        "mixed 中英 mixed 文字 with ASCII，全角标点。and more text here",
        "\tindented\twith\ttabs\tand 汉字\t混排",
        "😀 emoji 😀😀 and 한국어 and ｆｕｌｌｗｉｄｔｈ",
        "plain ascii only, long enough to wrap a few times over the limit",
        "中",
        "",
    ];

    /// 折段的三条不变量，对各种混排都要成立：
    /// 1. 每段的显示宽度不超过列数（唯一的例外是「单个字符就比一整行宽」，那也只能独占一段）；
    /// 2. 各段首尾相接、正好覆盖整行，切点都在字符边界上；
    /// 3. 行数缓存里记的段数与实际切出来的一致。
    #[test]
    fn segments_fit_the_width_and_cover_the_line() {
        for text in SAMPLES {
            for cols in [1usize, 3, 4, 7, 10, 23] {
                let mut ed = Editor::new("/tmp/a.txt".into(), text.to_string());
                v_recompute(&mut ed);
                v_wrap_sync(&mut ed, cols);
                let n = v_total_vrows(&ed);
                let mut at = 0;
                for seg in 0..n {
                    let (a, b) = v_seg_range(&ed, 0, seg, cols);
                    assert_eq!(a, at, "{text:?} cols={cols} 第 {seg} 段没接上");
                    assert!(text.is_char_boundary(a) && text.is_char_boundary(b));
                    let piece = &text[a..b];
                    assert!(b > a || text.is_empty(), "{text:?} cols={cols} 出现空段");
                    assert!(
                        str_cols(piece) <= cols as f32 + 1e-3 || piece.chars().count() == 1,
                        "{text:?} cols={cols} 第 {seg} 段 {piece:?} 宽 {} 列",
                        str_cols(piece)
                    );
                    at = b;
                }
                assert_eq!(at, text.len(), "{text:?} cols={cols} 没覆盖到行末");
            }
        }
    }

    /// 位置 ↔ (视觉行, 列) 互相还原：每个字符边界映射过去再映射回来，必须是它自己。
    /// 光标上下移动、点击定位、滚动跟随都建立在这对映射上。
    #[test]
    fn byte_and_visual_position_round_trip() {
        for text in SAMPLES {
            for cols in [3usize, 4, 7, 10, usize::MAX / 4] {
                let mut ed = Editor::new("/tmp/a.txt".into(), format!("{text}\nnext line\n"));
                v_recompute(&mut ed);
                v_wrap_sync(&mut ed, cols);
                let mut bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
                bounds.push(text.len());
                for b in bounds {
                    let (vrow, vcol) = v_vpos_of_byte(&ed, b, cols);
                    let back = v_byte_of_vpos(&ed, vrow, vcol, cols);
                    // 段尾 = 下一段开头是同一个位置的两种画法；v_vpos 一律报成后者，
                    // 所以还原回来一定是它自己
                    assert_eq!(back, b, "{text:?} cols={cols} 字节 {b} → ({vrow},{vcol}) → {back}");
                }
            }
        }
    }

    /// 非换行模式的横向窗口：两端落在字符边界上，起点位置 = 窗口之前那段文字的真实宽度
    ///（这样窗口从哪开始取，文字画出来的位置都一样，横向滚动时不会跳）。
    #[test]
    fn horizontal_window_never_splits_a_character() {
        for text in SAMPLES {
            let mut ed = Editor::new("/tmp/a.txt".into(), text.to_string());
            v_recompute(&mut ed);
            let cols = usize::MAX / 4;
            v_wrap_sync(&mut ed, cols);
            for first_col in 0..40 {
                let win = v_row_window(&ed, 0, false, cols, first_col, 12);
                assert!(text.is_char_boundary(win.a) && text.is_char_boundary(win.b));
                assert!(win.a <= win.b);
                assert!((str_cols(&text[..win.a]) - win.x0).abs() < 1e-3);
                assert!(win.x0 <= first_col as f32 + 1e-3, "窗口起点跑到了视口左缘右边");
                // 窗口覆盖了视口：右端要么到行末，要么已经超出视口右缘
                let right = win.x0 + str_cols(&text[win.a..win.b]);
                assert!(win.to_end || right >= (first_col + 12) as f32 - 1e-3);
            }
        }
    }

    /// 一行的字符数恰好是折行列数的整数倍时，行末光标属于**最后一段的末尾**，而不是
    /// 「下一段第 0 列」——那一段并不存在（行数是 chars/cols，没有多出来的一行）。
    /// 算错的话，在这种行的行末按 ↓ 会跳过一整行，按 ↑ 会留在本行回到行首。
    #[test]
    fn caret_at_the_end_of_an_exactly_full_line_stays_on_its_last_row() {
        let mut ed = Editor::new("/tmp/a.txt".into(), "abcdefgh\nxy\n".into());
        v_recompute(&mut ed);
        v_wrap_sync(&mut ed, 4); // 第 0 行 8 字符 → 2 段
        assert_eq!(v_total_vrows(&ed), 4);
        assert_eq!(v_vpos_of_byte(&ed, 8, 4), (1, 4), "行末应在第 2 段末尾");
        assert_eq!(v_vpos_of_byte(&ed, 4, 4), (1, 0), "行中的段边界属于下一段开头");
        assert_eq!(v_vpos_of_byte(&ed, 0, 4), (0, 0));
        assert_eq!(v_vpos_of_byte(&ed, 9, 4), (2, 0)); // 下一逻辑行
        // 往返：行末位置能映射回同一个字节
        assert_eq!(v_byte_of_vpos(&ed, 1, 4, 4), 8);
        // 空行不受影响
        assert_eq!(v_vpos_of_byte(&ed, 12, 4), (3, 0));
    }
}
