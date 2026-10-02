//! 终端单元格着色、关键字高亮与 URL 检测。

use egui::{Color32, FontId, Rect, Stroke, TextFormat, Vec2};

use super::theme::TermColors;

pub(super) fn vt_color(c: vt100::Color, default: Color32, tc: &TermColors) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => xterm256(i, tc),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

/// 关键字高亮规则（小写匹配子串 -> 颜色）。red=错误，orange=警告。
const HL_RULES: &[(&str, Color32)] = &[
    ("error", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("fatal", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("panic", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("fail", Color32::from_rgb(0xd0, 0x40, 0x40)),
    // "warning" 放在 "warn" 之前，确保整词都被着色（否则子串匹配只染前 4 个字符）
    ("warning", Color32::from_rgb(0xc8, 0x8a, 0x20)),
    ("warn", Color32::from_rgb(0xc8, 0x8a, 0x20)),
];

/// 行内容哈希：URL / 关键字高亮缓存的键。逐格喂入与 find_row_urls / highlight_colors
/// 完全一致的字符序列（续格跳过、宽字符取首字符、空 cell 记空格）+ 列宽——哈希相同
/// 必然对应相同的扫描输入，无需再构建 String 验证。64 位碰撞率对装饰性高亮可忽略。
pub(super) fn row_content_hash(screen: &vt100::Screen, row: u16, cols: u16) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut col = 0u16;
    while col < cols {
        let wide = screen.cell(row, col).is_some_and(|c| c.is_wide());
        match screen.cell(row, col) {
            Some(c) if c.is_wide_continuation() => {}
            Some(c) => c.contents().chars().next().unwrap_or(' ').hash(&mut h),
            None => ' '.hash(&mut h),
        }
        col += if wide { 2 } else { 1 };
    }
    cols.hash(&mut h);
    h.finish()
}

/// 计算一行各单元格的高亮覆盖色（None=不覆盖）。关键字为 ASCII，按 1 列/字符。
pub(super) fn highlight_colors(
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
) -> Vec<Option<Color32>> {
    let mut chars: Vec<(u16, char)> = Vec::new();
    let mut col = 0u16;
    while col < cols {
        let wide = screen.cell(row, col).is_some_and(|c| c.is_wide());
        match screen.cell(row, col) {
            Some(c) if c.is_wide_continuation() => {}
            Some(c) => chars.push((col, c.contents().chars().next().unwrap_or(' '))),
            None => chars.push((col, ' ')),
        }
        col += if wide { 2 } else { 1 };
    }
    let text: String = chars.iter().map(|(_, c)| *c).collect();
    // 用 ASCII 小写：保持字节长度 1:1，使 `lower` 的字节偏移在 `text` 上同样有效——
    // 避免 to_lowercase() 改变长度（如 İ→i̇、ẞ→ß）后 text[..start] 落到非字符边界而 panic。
    // 关键字（HL_RULES）均为 ASCII，ASCII 折叠已足够。
    let lower = text.to_ascii_lowercase();
    let mut out = vec![None; cols as usize];
    for (kw, color) in HL_RULES {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(kw) {
            let start = from + rel;
            let start_char = text[..start].chars().count();
            for k in 0..kw.chars().count() {
                if let Some(&(c, _)) = chars.get(start_char + k) {
                    out[c as usize] = Some(*color);
                }
            }
            from = start + kw.len();
        }
    }
    out
}

/// 可点击 URL 的匹配正则（一次性编译）：常见协议 + 裸 `www.`。
/// 末尾在 `find_row_urls` 里再统一裁掉句读符号（. , ; : ! ? ) ] }）。
fn url_regex() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // (?i) 协议不区分大小写；正文取到空白或明显分隔符为止。
        // 只识别可安全交给浏览器的 scheme（与 open_url 白名单一致）：
        // ssh/sftp/file 等不再高亮——点击它们会触发本地协议处理器，终端输出不可信。
        // \b 防子串误匹配（如 sftp:// 中间的 ftp://）
        //
        // 两处为中文环境做的调整：
        // - 开头用 ASCII 单词边界 `(?-u:\b)`。默认的 Unicode `\b` 把汉字也算作单词字符，
        //   「详见https://…」这种紧贴汉字的链接两侧都是单词字符、没有边界，整条认不出来。
        // - 正文遇到全角标点即止。「见 https://a.com，然后重试」不能把逗号和后面的汉字
        //   吞进链接里（路径里的汉字本身是允许的）。
        regex::Regex::new(
            r#"(?i)(?-u:\b)(?:(?:https?|ftps?)://|www\.)[^\s"'<>`|，。；：！？、（）【】《》「」『』“”‘’…]+"#,
        )
        .unwrap()
    })
}

/// 在一行里查找链接，返回 (起列, 止列(含), url)。列号按屏幕单元格计。
/// 支持 http(s)/ftp(s)/ssh/sftp/file 协议与裸 `www.`（后者自动补 https://）。
pub(super) fn find_row_urls(
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
) -> Vec<(u16, u16, String)> {
    // 链接可能被终端折到下一行：沿软换行把这一行所在的整条逻辑行拼起来再找，然后只取落在
    // 本行上的那一段——这样每一行上的格子都指向**完整**的链接（否则第一行点开的是半截，
    // 后面几行根本不可点）。上下各最多扩 8 行，够一条很长的链接，也给扫描量封了顶。
    const SPAN: u16 = 8;
    let rows = screen.size().0;
    let mut first = row;
    while first > 0 && row - first < SPAN && screen.row_wrapped(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows && last - row < SPAN && screen.row_wrapped(last) {
        last += 1;
    }
    // 逐字符记录 (行, 起始列, 字符)；宽字符续格跳过
    let mut chars: Vec<(u16, u16, char)> = Vec::new();
    for r in first..=last {
        let mut col = 0u16;
        while col < cols {
            let cell = screen.cell(r, col);
            let wide = cell.is_some_and(|c| c.is_wide());
            match cell {
                Some(c) if c.is_wide_continuation() => {}
                // 软换行行末尾空着的那一格是「宽字符放不下」留的，不是文字里的空格
                Some(c) if c.contents().is_empty() && r != last && col + 1 == cols => {}
                Some(c) => chars.push((r, col, c.contents().chars().next().unwrap_or(' '))),
                None => chars.push((r, col, ' ')),
            }
            col += if wide { 2 } else { 1 };
        }
    }
    if chars.is_empty() {
        return Vec::new();
    }
    let text: String = chars.iter().map(|(_, _, c)| *c).collect();
    urls_in_text(&text)
        .into_iter()
        .filter_map(|(start_char, ulen, url)| {
            let end = (start_char + ulen).min(chars.len());
            let mut on_row = chars[start_char.min(end)..end]
                .iter()
                .filter(|(r, _, _)| *r == row)
                .map(|(_, c, _)| *c);
            let sc = on_row.next()?;
            let ec = on_row.next_back().unwrap_or(sc);
            Some((sc, ec, url))
        })
        .collect()
}

/// 裁掉链接末尾不属于它的句读：`. , ; : ! ?` 一律裁；右括号只裁**落单**的——成对的括号
/// 是链接的一部分（`…/wiki/Rust_(programming_language)`），链接外面包着的那一个才不是
///（`(see https://a.com/x)`）。
fn trim_url_tail(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().next_back() else {
            return s;
        };
        let drop = match last {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' | ']' | '}' => {
                let open = match last {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                s.matches(last).count() > s.matches(open).count()
            }
            _ => false,
        };
        if !drop {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

/// 在一段文本里找链接，返回 (起始字符下标, 字符数, url)。
pub(super) fn urls_in_text(text: &str) -> Vec<(usize, usize, String)> {
    let mut urls = Vec::new();
    for m in url_regex().find_iter(text) {
        let trimmed = trim_url_tail(m.as_str());
        let ulen = trimmed.chars().count();
        if ulen == 0 {
            continue;
        }
        let start_char = text[..m.start()].chars().count();
        // 裸 www. 补全协议，便于浏览器直接打开
        let url = if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("www.") {
            format!("https://{trimmed}")
        } else {
            trimmed.to_string()
        };
        urls.push((start_char, ulen, url));
    }
    urls
}

pub(super) fn cell_format(c: &vt100::Cell, font: &FontId, tc: &TermColors) -> TextFormat {
    // 反显：文字改用背景色（实际背景块在 paint_row_backgrounds 中绘制）
    let base = if c.inverse() {
        c.bgcolor()
    } else {
        c.fgcolor()
    };
    let default = if c.inverse() { tc.bg } else { tc.fg };
    let mut fg = vt_color(base, default, tc);
    // Bold：ANSI 0–7 升到亮色 8–15（xterm 常见行为）；其余略提亮
    if c.bold() && !c.dim() {
        fg = match base {
            vt100::Color::Idx(i) if i < 8 => vt_color(vt100::Color::Idx(i + 8), default, tc),
            _ => brighten_rgb(fg, 1.18),
        };
    }
    if c.dim() {
        fg = brighten_rgb(fg, 0.55);
    }
    let mut f = TextFormat {
        font_id: font.clone(),
        color: fg,
        ..Default::default()
    };
    if c.underline() || c.double_underline() {
        // 宽度保持 1；双下划线由 ui_paint 再画一条，避免又粗又双。
        f.underline = Stroke::new(1.0, fg);
    }
    f
}

/// 闪烁相位：约 2Hz；用于 SGR 5/6。
pub(super) fn blink_phase_visible() -> bool {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        / 500)
        % 2
        == 0
}

/// 按比例调整 RGB（用于 bold 提亮 / dim 变暗），保持 alpha。
pub(super) fn brighten_rgb(c: Color32, factor: f32) -> Color32 {
    let scale = |v: u8| -> u8 { ((v as f32 * factor).round()).clamp(0.0, 255.0) as u8 };
    Color32::from_rgba_unmultiplied(scale(c.r()), scale(c.g()), scale(c.b()), c.a())
}

/// 逐格绘制非默认背景色（egui 文本布局不便携带逐段背景，单独画矩形）。
pub(super) fn paint_row_backgrounds(
    painter: &egui::Painter,
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
    origin: egui::Pos2,
    cell: Vec2,
    tc: &TermColors,
) {
    for col in 0..cols {
        if let Some(c) = screen.cell(row, col) {
            // 宽字符（中文等）的续格由其首格统一铺底，避免只盖住半个字
            if c.is_wide_continuation() {
                continue;
            }
            let mut bg = vt_color(c.bgcolor(), Color32::TRANSPARENT, tc);
            if c.inverse() {
                bg = vt_color(c.fgcolor(), tc.fg, tc);
            }
            if bg != Color32::TRANSPARENT {
                let w = if c.is_wide() { cell.x * 2.0 } else { cell.x };
                let pos = origin + Vec2::new(col as f32 * cell.x, row as f32 * cell.y);
                painter.rect_filled(Rect::from_min_size(pos, Vec2::new(w, cell.y)), 0.0, bg);
            }
        }
    }
}

/// xterm 256 色板（0..15 取当前终端配色的 ANSI 表）。
pub(super) fn xterm256(i: u8, tc: &TermColors) -> Color32 {
    match i {
        0..=15 => {
            let (r, g, b) = tc.ansi[i as usize];
            Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(conv(r), conv(g), conv(b))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}
