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
        regex::Regex::new(r#"(?i)\b(?:(?:https?|ftps?)://|www\.)[^\s"'<>`|]+"#).unwrap()
    })
}

/// 在一行里查找链接，返回 (起列, 止列(含), url)。列号按屏幕单元格计。
/// 支持 http(s)/ftp(s)/ssh/sftp/file 协议与裸 `www.`（后者自动补 https://）。
pub(super) fn find_row_urls(
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
) -> Vec<(u16, u16, String)> {
    // 逐字符记录 (起始列, 字符)；宽字符续格跳过
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
    if chars.is_empty() {
        return Vec::new();
    }
    let mut urls = Vec::new();
    for m in url_regex().find_iter(&text) {
        // 裁掉常见的尾随句读（URL 紧跟逗号/句号/右括号等时不应纳入）
        let trimmed = m
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '!', '?']);
        let ulen = trimmed.chars().count();
        if ulen == 0 {
            continue;
        }
        let start_char = text[..m.start()].chars().count();
        let sc = chars[start_char.min(chars.len() - 1)].0;
        let ec = chars[(start_char + ulen - 1).min(chars.len() - 1)].0;
        // 裸 www. 补全协议，便于浏览器直接打开
        let url = if trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case("www.") {
            format!("https://{trimmed}")
        } else {
            trimmed.to_string()
        };
        urls.push((sc, ec, url));
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
    if c.underline() {
        f.underline = Stroke::new(1.0, fg);
    }
    f
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
