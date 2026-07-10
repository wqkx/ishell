//! VT 序列化与 UTF-8 拼包辅助。

pub(super) fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 一个单元格的可序列化属性（用于 resize 重排时还原颜色/字形）。
#[derive(Clone, Copy, PartialEq)]
struct CellAttrs {
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellAttrs {
    const DEFAULT: CellAttrs = CellAttrs {
        fg: vt100::Color::Default,
        bg: vt100::Color::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        inverse: false,
    };
    fn of(c: &vt100::Cell) -> Self {
        Self {
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            dim: c.dim(),
            italic: c.italic(),
            underline: c.underline(),
            inverse: c.inverse(),
        }
    }
    /// 生成「自包含」的 SGR：先 reset(0) 再追加非默认属性，无需跟踪上一状态的开关位。
    fn sgr(&self) -> Vec<u8> {
        let mut p: Vec<String> = vec!["0".into()];
        if self.bold {
            p.push("1".into());
        }
        if self.dim {
            p.push("2".into());
        }
        if self.italic {
            p.push("3".into());
        }
        if self.underline {
            p.push("4".into());
        }
        if self.inverse {
            p.push("7".into());
        }
        push_sgr_color(&mut p, self.fg, true);
        push_sgr_color(&mut p, self.bg, false);
        format!("\x1b[{}m", p.join(";")).into_bytes()
    }
}

/// 把一个颜色追加为 SGR 参数（前景 fg=true / 背景 fg=false）。
fn push_sgr_color(p: &mut Vec<String>, c: vt100::Color, fg: bool) {
    let (base, bright, ext) = if fg {
        (30u16, 90u16, 38u16)
    } else {
        (40, 100, 48)
    };
    match c {
        vt100::Color::Default => {}
        vt100::Color::Idx(i) => {
            if i < 8 {
                p.push((base + i as u16).to_string());
            } else if i < 16 {
                p.push((bright + (i as u16 - 8)).to_string());
            } else {
                p.push(format!("{ext};5;{i}"));
            }
        }
        vt100::Color::Rgb(r, g, b) => p.push(format!("{ext};2;{r};{g};{b}")),
    }
}

/// 把可见屏第 `row` 行序列化为带 SGR 的字节（裁掉行尾空白；空行返回空 Vec）。
/// 行内属性变化时插入自包含 SGR，行尾补 `\x1b[0m`，使各行互不影响。
pub(super) fn serialize_row(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<u8> {
    // 该行最后一个有内容的列(+1)，用于裁掉行尾空白
    let mut last = 0u16;
    for c in 0..cols {
        if screen.cell(row, c).is_some_and(|cell| cell.has_contents()) {
            last = c + 1;
        }
    }
    if last == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = CellAttrs::DEFAULT; // 行首解析器状态为默认（上一行尾已 reset）
    let mut col = 0u16;
    while col < last {
        let Some(cell) = screen.cell(row, col) else {
            out.push(b' ');
            col += 1;
            continue;
        };
        if cell.is_wide_continuation() {
            col += 1; // 宽字符占位列，跳过（宽字符本体已在前一列输出）
            continue;
        }
        let a = CellAttrs::of(cell);
        if a != cur {
            out.extend_from_slice(&a.sgr());
            cur = a;
        }
        let s = cell.contents();
        if s.is_empty() {
            out.push(b' ');
        } else {
            out.extend_from_slice(s.as_bytes());
        }
        col += 1;
    }
    out.extend_from_slice(b"\x1b[0m");
    out
}

/// 若字节流结尾是一个**不完整**的多字节 UTF-8 序列，返回需要暂存的尾字节数；否则 0。
/// 用于避免一个 UTF-8 字符被拆分在两次 `feed` 之间导致乱码。
pub(super) fn incomplete_utf8_tail(b: &[u8]) -> usize {
    let mut cont = 0usize; // 已统计的连续字节（0b10xxxxxx）数量
    let mut i = b.len();
    while i > 0 && cont < 3 {
        i -= 1;
        let byte = b[i];
        if byte & 0b1100_0000 == 0b1000_0000 {
            cont += 1; // 连续字节，继续往前找首字节
            continue;
        }
        // 找到序列首字节（或单字节）：按首字节判断该序列需要的总长度
        let need = if byte & 0b1000_0000 == 0 {
            0 // ASCII，单字节，完整
        } else if byte & 0b1110_0000 == 0b1100_0000 {
            1 // 2 字节
        } else if byte & 0b1111_0000 == 0b1110_0000 {
            2 // 3 字节（绝大多数中文）
        } else if byte & 0b1111_1000 == 0b1111_0000 {
            3 // 4 字节
        } else {
            0 // 非法首字节，按完整处理，交给解析器
        };
        // 还差连续字节 -> 把「首字节 + 已有连续字节」整体暂存
        return if need > cont { cont + 1 } else { 0 };
    }
    0
}

#[cfg(test)]
mod utf8_tail_tests {
    use super::incomplete_utf8_tail;
    #[test]
    fn detects_split_multibyte() {
        // "你"=E4 BD A0：完整应为 0；缺尾则需暂存
        assert_eq!(incomplete_utf8_tail(&[0xE4, 0xBD, 0xA0]), 0);
        assert_eq!(incomplete_utf8_tail(&[0xE4, 0xBD]), 2); // 缺 1 个连续字节
        assert_eq!(incomplete_utf8_tail(&[0xE4]), 1); // 只有首字节
        assert_eq!(incomplete_utf8_tail(b"abc"), 0); // 纯 ASCII
        assert_eq!(incomplete_utf8_tail(&[0x41, 0xE4, 0xBD, 0xA0]), 0); // A + 完整"你"
    }
}
