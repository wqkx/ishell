//! IME 组字区间的「总函数」化工具：把可能已经陈旧的字节偏移收敛成一定不会 panic 的形状。
//!
//! # 为什么需要它
//!
//! 自绘 IME（见 `file_panel/ime.rs`、`editor/virtual_/input.rs`）必须**跨帧**保存组字区间
//! `(start, end)`，因为 fcitx 会连发多条 `Preedit`，后一条要替换掉前一条已上屏的内容。
//! 但这个区间指向的 `String` 完全可能在两条 `Preedit` 之间被别处整个换掉：
//!
//! - 编辑器撤销/重做、重新加载文件 → `Editor::content` 换内容；
//! - 切目录重置路径框、重命名行复用、对话框换类型 → 单行框的 `buf` 换内容；
//! - fcitx 自身异常（进程重启、切换输入法）时 `Disabled` 丢失，组字区间就一直挂着。
//!
//! 此时旧区间落在新串里可能：① 越界；② **落在多字节字符（中文/emoji）中间**。
//! 只写 `.min(s.len())` 只挡得住 ①；② 一样是 `byte index N is not a char boundary` panic，
//! 而且必须中文输入 + 特定时序才复现，正是「fcitx 一抽风 iShell 就崩」的形状。
//!
//! 所以这里统一提供向下吸附到字符边界的版本，所有 IME 路径只准用这些函数取偏移。

/// 把字节偏移收敛到 `s` 上合法的字符边界：先钳到串长，再向下吸附。
///
/// 向下（而不是向上）吸附是刻意的：组字区间的左端向下吸附最多多吃掉半个字符的前半段，
/// 右端向下吸附最多少吃半个字符——两者都只影响一次组字的显示，不会 panic，也不会越界。
pub fn floor_boundary(s: &str, b: usize) -> usize {
    let mut b = b.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// 把一段可能陈旧的组字区间收敛成 `s` 上一定可以安全 `replace_range` 的区间。
///
/// 保证：两端都是字符边界、都不越界、且 `start <= end`。
pub fn clamp_range(s: &str, (start, end): (usize, usize)) -> (usize, usize) {
    let start = floor_boundary(s, start);
    let end = floor_boundary(s, end).max(start);
    (start, end)
}

/// 用 `t` 替换掉 `buf` 上（可能已陈旧的）组字区间，返回替换后的新区间 `(start, end)`。
///
/// 这是 IME 路径**唯一**允许改 buffer 的入口。之所以不只导出 [`clamp_range`] 让各处自己
/// `replace_range`：那样每个调用点都得记得先 clamp，漏一处就是一个只有中文输入 + 特定
/// 时序才复现的崩溃。把「钳位 + 替换 + 算新区间」打包成一个不可能用错的函数，代价是一次
/// 函数调用，换来的是这条路上再也不会有裸 `replace_range`。
pub fn replace_preedit(buf: &mut String, range: (usize, usize), t: &str) -> (usize, usize) {
    let (s, e) = clamp_range(buf, range);
    buf.replace_range(s..e, t);
    (s, s + t.len())
}

/// 在 `at`（向下吸附到字符边界后）插入文本，返回插入后的光标字节位置。
pub fn insert_at(buf: &mut String, at: usize, t: &str) -> usize {
    let at = floor_boundary(buf, at);
    buf.insert_str(at, t);
    at + t.len()
}

/// 字节偏移 → 字符位（非字符边界时向下取整，避免切片 panic）。
pub fn char_of_byte(s: &str, b: usize) -> usize {
    s[..floor_boundary(s, b)].chars().count()
}

/// 字符位 → 字节偏移（越界回退到串尾）。
pub fn byte_of_char(s: &str, ch: usize) -> usize {
    s.char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(s.len()))
        .nth(ch)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这是本模块存在的理由：旧区间落在多字节字符中间时，`replace_range` 会 panic。
    /// 只 `.min(len)` 过不了这一条。
    #[test]
    fn stale_range_inside_a_multibyte_char_is_snapped() {
        let s = String::from("中文abc"); // "中"=0..3, "文"=3..6
        // 4、5 都在「文」的中间——直接拿去切片就是 panic
        assert!(!s.is_char_boundary(4) && !s.is_char_boundary(5));
        let (a, b) = clamp_range(&s, (4, 5));
        assert!(s.is_char_boundary(a) && s.is_char_boundary(b));
        let mut s2 = s.clone();
        s2.replace_range(a..b, "X"); // 不 panic 即达标
    }

    /// 缓冲区被换成更短的串（撤销/切目录）后，旧区间整体越界。
    #[test]
    fn out_of_range_is_clamped_to_the_tail() {
        let s = "ab";
        assert_eq!(clamp_range(s, (100, 200)), (2, 2));
        let mut s2 = String::from(s);
        s2.replace_range(2..2, "!");
        assert_eq!(s2, "ab!");
    }

    /// 两端分别吸附后可能出现 start > end（例如 (5,4) 同落在「文」里），必须归一，
    /// 否则 `replace_range` 报 "slice index starts at 5 but ends at 4"。
    #[test]
    fn inverted_range_is_normalized() {
        let s = "中文abc";
        let (a, b) = clamp_range(s, (5, 4));
        assert!(a <= b, "clamp 后必须 start <= end，实得 ({a}, {b})");
    }

    #[test]
    fn boundary_conversions_round_trip_on_multibyte() {
        let s = "中a文";
        assert_eq!(char_of_byte(s, 0), 0);
        assert_eq!(char_of_byte(s, 3), 1);
        assert_eq!(char_of_byte(s, 4), 2);
        assert_eq!(char_of_byte(s, 5), 2, "落在「文」中间应向下取整到 4→字符位 2");
        assert_eq!(char_of_byte(s, 999), 3);
        assert_eq!(byte_of_char(s, 0), 0);
        assert_eq!(byte_of_char(s, 2), 4);
        assert_eq!(byte_of_char(s, 3), 7);
        assert_eq!(byte_of_char(s, 999), 7);
    }

    /// 空串是最容易被漏掉的退化情形（对话框刚打开、路径框刚清空）。
    #[test]
    fn empty_string_never_panics() {
        assert_eq!(clamp_range("", (7, 9)), (0, 0));
        assert_eq!(char_of_byte("", 7), 0);
        assert_eq!(byte_of_char("", 7), 0);
        let mut s = String::new();
        assert_eq!(replace_preedit(&mut s, (7, 9), "拼"), (0, 3));
        assert_eq!(s, "拼");
        assert_eq!(insert_at(&mut s, 99, "音"), 6);
        assert_eq!(s, "拼音");
    }

    /// 正常组字：连续 Preedit 逐次替换掉上一次的候选，区间随之推进。
    #[test]
    fn consecutive_preedits_replace_the_previous_one() {
        let mut buf = String::from("前缀|后缀");
        let at = "前缀".len();
        let r = replace_preedit(&mut buf, (at, at), "z");
        assert_eq!(buf, "前缀z|后缀");
        let r = replace_preedit(&mut buf, r, "zh");
        assert_eq!(buf, "前缀zh|后缀");
        let r = replace_preedit(&mut buf, r, "中");
        assert_eq!(buf, "前缀中|后缀");
        // 提交：先清掉组字区间，再在原位插入最终文本
        let (s, _) = replace_preedit(&mut buf, r, "");
        assert_eq!(buf, "前缀|后缀");
        assert_eq!(insert_at(&mut buf, s, "中文"), s + "中文".len());
        assert_eq!(buf, "前缀中文|后缀");
    }

    /// 组字进行中缓冲区被整个换掉（编辑器撤销、切目录重置路径框），旧区间落在多字节
    /// 字符中间——修复前这一行就是 `byte index is not a char boundary` panic。
    #[test]
    fn preedit_across_a_buffer_swap_does_not_panic() {
        let mut buf = String::from("中文");
        let stale = (5, 20); // 上一个 buffer 留下的区间：5 在「文」中间，20 越界
        let (s, e) = replace_preedit(&mut buf, stale, "拼");
        assert!(buf.is_char_boundary(s) && buf.is_char_boundary(e));
        assert_eq!(buf, "中拼");
    }
}

#[cfg(test)]
mod boundary_sweep_tests {
    use super::*;

    /// 一段混合宽度的字符串：ASCII(1) + 中文(3) + 带变音符(2) + emoji(4)。
    /// 覆盖 UTF-8 全部四种长度，且相邻字符长度不同——最容易踩边界的形状。
    const MIXED: &str = "a中é😀b漢z";

    /// **全下标扫描**：对每一个字节位置（含 0 与 len，含所有字符内部的非法位置）调用
    /// `floor_boundary`，结果必须是合法边界、必须 ≤ 输入、且必须是 ≤ 输入的最大合法边界。
    ///
    /// 这个函数是「vcaret 必须落在字符边界」这条不变量的唯一收口点（见 geom.rs 的注释：
    /// 组字期间 vcaret 会被一段陈旧的 IME 区间推到多字节字符中间，此后第一次退格就崩）。
    /// 既有测试只点了几个位置，这里把整条串扫穿。
    #[test]
    fn floor_boundary_sweeps_every_byte_index() {
        let n = MIXED.len();
        for b in 0..=n + 5 {
            let got = floor_boundary(MIXED, b);
            assert!(
                MIXED.is_char_boundary(got),
                "floor_boundary({b}) = {got} 不是字符边界"
            );
            assert!(got <= n, "floor_boundary({b}) = {got} 越界（len={n}）");
            assert!(got <= b || b > n, "floor_boundary({b}) = {got} 比入参还大");
            // 必须是「≤ b 的最大合法边界」，不能保守地退太多
            let want = (0..=b.min(n)).rev().find(|&i| MIXED.is_char_boundary(i)).unwrap();
            assert_eq!(got, want, "floor_boundary({b}) 应当退到 {want}");
        }
    }

    /// `clamp_range` 对**任意**一对下标（含颠倒、越界、落在字符中间）都必须产出一个
    /// 可以直接拿去 `String::replace_range` 的合法区间：两端都是边界、start ≤ end ≤ len。
    /// 这是 `v_apply` 的最后一道防线（编辑器里存着用户还没保存的远端文件，为一个算错的
    /// 偏移崩掉整个应用不值得）。
    #[test]
    fn clamp_range_always_yields_a_usable_range() {
        let n = MIXED.len();
        for a in 0..=n + 3 {
            for b in 0..=n + 3 {
                let (s, e) = clamp_range(MIXED, (a, b));
                assert!(MIXED.is_char_boundary(s), "({a},{b}) → start {s} 不是边界");
                assert!(MIXED.is_char_boundary(e), "({a},{b}) → end {e} 不是边界");
                assert!(s <= e, "({a},{b}) → ({s},{e}) 顺序颠倒");
                assert!(e <= n, "({a},{b}) → end {e} 越界");
                // 真拿去用一次，确认不会 panic
                let mut buf = MIXED.to_string();
                buf.replace_range(s..e, "X");
            }
        }
    }

    /// `char_of_byte` / `byte_of_char` 在所有合法边界上必须互为逆；对落在字符中间的
    /// 字节位置也不能 panic。
    #[test]
    fn char_and_byte_indices_round_trip_on_every_boundary() {
        let n = MIXED.len();
        for b in 0..=n {
            let ch = char_of_byte(MIXED, b); // 非边界也不能崩
            if MIXED.is_char_boundary(b) {
                assert_eq!(byte_of_char(MIXED, ch), b, "字节 {b} → 字符 {ch} → 字节 往返不一致");
            }
        }
        // 字符下标越界也要有个合理答案（钳到末尾），不能 panic
        let total = MIXED.chars().count();
        assert_eq!(byte_of_char(MIXED, total), n);
        assert_eq!(byte_of_char(MIXED, total + 10), n);
    }

    /// `replace_preedit` / `insert_at` 用任意（可能非法的）位置调用都不能 panic，
    /// 且改完之后缓冲区仍是合法 UTF-8、返回的位置仍是合法边界。
    /// 组字过程中这两个函数是被 IME 事件以任意顺序驱动的，位置随时可能过时。
    #[test]
    fn preedit_helpers_never_corrupt_the_buffer() {
        let n = MIXED.len();
        for a in 0..=n + 2 {
            for b in 0..=n + 2 {
                let mut buf = MIXED.to_string();
                let (s, e) = replace_preedit(&mut buf, (a, b), "组字");
                assert!(buf.is_char_boundary(s) && buf.is_char_boundary(e));
                assert!(s <= e && e <= buf.len());
            }
            let mut buf = MIXED.to_string();
            let at = insert_at(&mut buf, a, "x");
            assert!(buf.is_char_boundary(at), "insert_at({a}) 返回了非边界 {at}");
            assert!(at <= buf.len());
        }
    }
}
