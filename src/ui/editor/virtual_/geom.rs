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
    // 混合行尾文件里留在内容中的 `\r\n` 是**一个**行尾：左移 / 退格一步跨过整个，
    // 光标永远不停在 `\r` 与 `\n` 之间（见 `v_line_range`）
    if s.as_bytes()[..b].ends_with(b"\r\n") {
        return b - 2;
    }
    s[..b]
        .chars()
        .next_back()
        .map(|c| b - c.len_utf8())
        .unwrap_or(0)
}
pub(super) fn next_char_boundary(s: &str, b: usize) -> usize {
    let b = crate::ui::ime_safe::floor_boundary(s, b);
    if s.as_bytes()[b..].starts_with(b"\r\n") {
        return b + 2;
    }
    s[b..]
        .chars()
        .next()
        .map(|c| b + c.len_utf8())
        .unwrap_or_else(|| s.len())
}
pub fn v_line_of(ed: &Editor, b: usize) -> usize {
    ed.vlines.partition_point(|&s| s <= b).saturating_sub(1)
}
/// 第 i 行文字的字节范围 [起, 止)：不含行尾。
///
/// 行尾可能是 `\n`，也可能是 `\r\n`：全文都是 CRLF 的文件打开时统一成 LF，混合行尾的
/// 文件则原样保留（保存时没改过的行一个字节都不变），`\r` 就留在内容里。它算行尾的一部分：
/// End、点击行末、上下移动都停在它前面，绘制也不把它当正文（行末另画一个淡色 CR 标记）。
pub(super) fn v_line_range(ed: &Editor, i: usize) -> (usize, usize) {
    let s = ed.vlines[i];
    let e = if i + 1 < ed.vlines.len() {
        let nl = ed.vlines[i + 1] - 1;
        if nl > s && ed.content.as_bytes()[nl - 1] == b'\r' {
            nl - 1
        } else {
            nl
        }
    } else {
        ed.content.len()
    };
    (s, e)
}
/// 第 i 行连同行尾的结束位置（= 下一行的起点；最后一行为全文末尾）。整行操作（删除、
/// 复制、三击选中）用它，别用「行尾 + 1」——CRLF 的行尾是两个字节。
pub(super) fn v_line_next(ed: &Editor, i: usize) -> usize {
    ed.vlines.get(i + 1).copied().unwrap_or(ed.content.len())
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

// ——— 显示宽度 ———
//
// 编辑器里的「列」= 等宽字体下一个 ASCII 字符的宽度。但不是每个字符都占一列：实测这套字体
// 里汉字约 1.67 列、韩文约 1.53 列、emoji 约 2.1 列、Tab 是 4 列——既不是 1，也不是终端
// 那种整数 2。折行、横向滚动窗口、光标的横向位置都得按**字体实测的字宽**算，否则算出来
// 的位置与实际画出来的（galley 排版）对不上：中文行折出来比视口宽、横向滚动时文字跳动、
// 光标跟随滚不到行尾。
//
// 字宽要问字体，而编辑操作（上下移动光标）手里没有 `egui::Context`，所以度量放在线程局部：
// 每帧绘制前登记当前字体，之后各处查表。比例与字号无关（实测 12–20pt 完全一致），所以按
// 字符缓存「相对空格的宽度」即可。没登记过字体时（单元测试、首帧之前）退回东亚宽度惯例。

/// 字符显示宽度的查表缓存（单位：列）。
pub(in crate::ui::editor) struct Metric {
    ctx: Option<egui::Context>,
    font: Option<egui::FontId>,
    /// 空格的像素宽度（= 一列）
    space: f32,
    tab: f32,
    /// 上次登记字体时量到的「汉字 / 空格」比例：变了说明字体换了，缓存作废
    probe: f32,
    /// BMP 内各字符的宽度（NaN = 还没量过）；首次遇到非 ASCII 字符时才分配
    bmp: Vec<f32>,
    astral: std::collections::HashMap<char, f32>,
    /// 缓存每作废一次 +1：按宽度算出来的东西（折行行数）据此失效
    epoch: u64,
}

thread_local! {
    static METRIC: std::cell::RefCell<Metric> = std::cell::RefCell::new(Metric {
        ctx: None,
        font: None,
        space: 0.0,
        tab: 4.0,
        probe: 0.0,
        bmp: Vec::new(),
        astral: std::collections::HashMap::new(),
        epoch: 0,
    });
}

/// 没有字体可问时的宽度：东亚宽字符 / emoji 按 2 列，其余 1 列。
fn fallback_cols(c: char) -> f32 {
    let wide = matches!(
        c as u32,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    );
    if wide {
        2.0
    } else {
        1.0
    }
}

/// 用**排版结果**量一个字符的像素宽度：排 8 个取平均。
///
/// 不用 `Fonts::glyph_width`：它报的是字形自己的步进，而回退字体（中文走的就是回退字体）
/// 在排版时还要乘上字体的缩放微调——实测汉字 `glyph_width` 报 13px，排出来是 12.15px。
/// 正文是 galley 排出来的，量宽度也得用同一把尺子，否则每个汉字差 7%，一行下来位置就漂了。
fn laid_out_width(ctx: &egui::Context, font: &egui::FontId, c: char) -> f32 {
    const N: usize = 8;
    let text: String = std::iter::repeat_n(c, N).collect();
    ctx.fonts_mut(|f| f.layout_no_wrap(text, font.clone(), egui::Color32::WHITE))
        .size()
        .x
        / N as f32
}

impl Metric {
    fn measure(&self, c: char) -> f32 {
        match (&self.ctx, &self.font) {
            (Some(ctx), Some(font)) if self.space > 0.0 => {
                laid_out_width(ctx, font, c) / self.space
            }
            _ => fallback_cols(c),
        }
    }

    /// 字符 `c` 占几列。
    pub(in crate::ui::editor) fn w(&mut self, c: char) -> f32 {
        if c == '\t' {
            return self.tab;
        }
        if c.is_ascii() {
            return 1.0;
        }
        let cp = c as usize;
        if cp < 0x10000 {
            if self.bmp.is_empty() {
                self.bmp = vec![f32::NAN; 0x10000];
            }
            if self.bmp[cp].is_nan() {
                self.bmp[cp] = self.measure(c);
            }
            self.bmp[cp]
        } else {
            if let Some(w) = self.astral.get(&c) {
                return *w;
            }
            let w = self.measure(c);
            self.astral.insert(c, w);
            w
        }
    }
}

/// 登记本帧用的等宽字体与列宽（每帧绘制前调用）。字体换了才作废缓存。
/// `char_w` 是绘制用的「一列」的像素宽度——宽度都折算成它的倍数，位置才能和绘制对上。
pub(in crate::ui::editor) fn set_metric_font(ctx: &egui::Context, font: &egui::FontId, char_w: f32) {
    let space = char_w.max(1.0);
    let tab = laid_out_width(ctx, font, '\t') / space;
    let probe = laid_out_width(ctx, font, '中') / space;
    METRIC.with(|m| {
        let mut m = m.borrow_mut();
        let changed = m.ctx.is_none() || (m.probe - probe).abs() > 1e-3 || (m.tab - tab).abs() > 1e-3;
        if changed {
            m.bmp.clear();
            m.astral.clear();
            m.epoch += 1;
        }
        m.ctx = Some(ctx.clone());
        m.font = Some(font.clone());
        m.space = space;
        m.tab = if tab > 0.0 { tab } else { 4.0 };
        m.probe = probe;
    });
}

/// 借出度量做一批查询（整行/整个文件的扫描用这个，免得每个字符借一次）。
pub(in crate::ui::editor) fn with_metric<R>(f: impl FnOnce(&mut Metric) -> R) -> R {
    METRIC.with(|m| f(&mut m.borrow_mut()))
}

pub(in crate::ui::editor) fn metric_epoch() -> u64 {
    METRIC.with(|m| m.borrow().epoch)
}

/// 字符串的显示宽度（列）。
pub(in crate::ui::editor) fn str_cols(s: &str) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    with_metric(|m| s.chars().map(|c| m.w(c)).sum())
}

/// 显示位置 `x`（列）落在哪个字符上：返回该字符的起始字节与起始位置。
/// 不劈开字符——`x` 落在一个宽字符中间时取这个字符的起点。超出末尾返回 (len, 总宽)。
pub(in crate::ui::editor) fn byte_at_cols_floor(s: &str, x: f32) -> (usize, f32) {
    with_metric(|m| {
        let mut acc = 0.0f32;
        for (i, c) in s.char_indices() {
            let w = m.w(c);
            if acc + w > x + 1e-3 {
                return (i, acc);
            }
            acc += w;
        }
        (s.len(), acc)
    })
}

/// 第一个「起始位置 ≥ x」的字符边界（横向窗口的右端：把跨在 x 上的那个字符整个包进来）。
pub(in crate::ui::editor) fn byte_at_cols_ceil(s: &str, x: f32) -> usize {
    with_metric(|m| {
        let mut acc = 0.0f32;
        for (i, c) in s.char_indices() {
            if acc >= x - 1e-3 {
                return i;
            }
            acc += m.w(c);
        }
        s.len()
    })
}

/// 离显示位置 `x` 最近的字符边界（上下移动光标时保持视觉列用）。
pub(in crate::ui::editor) fn byte_near_cols(s: &str, x: f32) -> usize {
    with_metric(|m| {
        let mut acc = 0.0f32;
        for (i, c) in s.char_indices() {
            let w = m.w(c);
            if acc + w / 2.0 > x {
                return i;
            }
            acc += w;
        }
        s.len()
    })
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
