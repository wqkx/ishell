//! 语言规格、分词与高亮布局。

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

use super::lang::{lang_for, Lang};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Tok {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// 近白底上的配色（仿 VSCode Light）。
pub(super) fn color(t: Tok) -> Color32 {
    match t {
        Tok::Plain => Color32::from_rgb(0x24, 0x29, 0x2e), // 近黑
        Tok::Comment => Color32::from_rgb(0x00, 0x80, 0x00), // 绿
        Tok::Str => Color32::from_rgb(0xa3, 0x15, 0x15),   // 暗红
        Tok::Num => Color32::from_rgb(0x09, 0x86, 0x58),   // 青绿
        Tok::Keyword => Color32::from_rgb(0x00, 0x00, 0xd0), // 蓝
    }
}

/// 扫描普通引号字符串，返回结束字节位（含收尾引号）。
/// `raw` 时不处理 `\` 转义（Python r"…"）；非反引号串遇换行即结束，避免漏闭合时染色整篇。
fn scan_str(text: &str, start: usize, quote: char, raw: bool) -> usize {
    let n = text.len();
    let mut j = start + quote.len_utf8();
    while j < n {
        let cj = text[j..].chars().next().unwrap();
        if cj == '\\' && !raw {
            j += cj.len_utf8();
            if j < n {
                j += text[j..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        if cj == quote {
            return j + cj.len_utf8();
        }
        if cj == '\n' && quote != '`' {
            return j;
        }
        j += cj.len_utf8();
    }
    n
}

/// 扫描多行字符串（open 已在 start 处匹配），返回结束字节位（含收尾定界符）；未闭合染到文末。
fn scan_pair(text: &str, start: usize, open: &str, close: &str) -> usize {
    text[start + open.len()..]
        .find(close)
        .map(|e| start + open.len() + e + close.len())
        .unwrap_or(text.len())
}

/// 单遍分词，返回 (字节范围, 类别) 列表（连续 Plain 已合并）。
pub(super) fn tokenize(text: &str, lang: &Lang) -> Vec<(usize, usize, Tok)> {
    let mut segs: Vec<(usize, usize, Tok)> = Vec::new();
    let n = text.len();
    let mut i = 0usize;
    while i < n {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();
        // 多行字符串（Python 三引号 / Rust r#"…"# / Lua [[…]] 等；先于单字符引号匹配）
        if let Some((o, cl2)) = lang.multi.iter().find(|(o, _)| rest.starts_with(o)) {
            let end = scan_pair(text, i, o, cl2);
            segs.push((i, end, Tok::Str));
            i = end;
            continue;
        }
        // 块注释（先于行注释：Lua 的 --[[ 以 -- 开头，反序会被误判为行注释）
        if let Some((bs, be)) = lang.block {
            if let Some(after) = rest.strip_prefix(bs) {
                let end = after
                    .find(be)
                    .map(|e| i + bs.len() + e + be.len())
                    .unwrap_or(n);
                segs.push((i, end, Tok::Comment));
                i = end;
                continue;
            }
        }
        // 行注释
        if lang.line.iter().any(|p| rest.starts_with(*p)) {
            let end = rest.find('\n').map(|e| i + e).unwrap_or(n);
            segs.push((i, end, Tok::Comment));
            i = end;
            continue;
        }
        // 字符串
        if lang.strings.contains(&c) {
            let j = scan_str(text, i, c, false);
            segs.push((i, j, Tok::Str));
            i = j;
            continue;
        }
        // 前缀字符串（Python 的 f"…" / r'…' / rb"…" 等）：前缀与字符串一体染色；
        // raw（含 r/R）时 \ 不作转义
        if lang.str_prefix && matches!(c, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U') {
            let pfx = rest
                .chars()
                .take_while(|ch| matches!(ch, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U'))
                .count();
            if pfx <= 2 {
                let after = &rest[pfx..];
                if let Some((o, cl2)) = lang.multi.iter().find(|(o, _)| after.starts_with(o)) {
                    let end = scan_pair(text, i + pfx, o, cl2);
                    segs.push((i, end, Tok::Str));
                    i = end;
                    continue;
                }
                if let Some(q) = after.chars().next().filter(|q| lang.strings.contains(q)) {
                    let raw = rest[..pfx].chars().any(|ch| ch == 'r' || ch == 'R');
                    let end = scan_str(text, i + pfx, q, raw);
                    segs.push((i, end, Tok::Str));
                    i = end;
                    continue;
                }
            }
            // 非字符串前缀 → 落入下方标识符分支
        }
        // 数字（含 0x/0b 前缀、下划线分隔、1e-5 科学计数）
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj.is_ascii_alphanumeric() || cj == '.' || cj == '_' {
                    j += cj.len_utf8();
                    // e/E 后允许一个正负号（科学计数）
                    if (cj == 'e' || cj == 'E') && j < n {
                        let cs = text[j..].chars().next().unwrap();
                        if (cs == '+' || cs == '-')
                            && text[j + 1..]
                                .chars()
                                .next()
                                .is_some_and(|d| d.is_ascii_digit())
                        {
                            j += 1;
                        }
                    }
                } else {
                    break;
                }
            }
            segs.push((i, j, Tok::Num));
            i = j;
            continue;
        }
        // 装饰器（@ident.ident）。注意：无论后面是否跟标识符都必须消费 @ 前进——
        // Plain 合并循环遇 @ 会 break，若此处不前进（如 `a @ b` 矩阵乘、行尾 @）会死循环
        if lang.deco && c == '@' {
            let mut j = i + 1;
            let mut any = false;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj == '_' || cj == '.' || cj.is_alphanumeric() {
                    any = true;
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            segs.push((i, j, if any { Tok::Keyword } else { Tok::Plain }));
            i = j;
            continue;
        }
        // 标识符 / 关键字
        if c == '_' || c.is_alphabetic() {
            let mut j = i;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj == '_' || cj.is_alphanumeric() {
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            let word = &text[i..j];
            let t = if lang.keywords.contains(&word) {
                Tok::Keyword
            } else {
                Tok::Plain
            };
            segs.push((i, j, t));
            i = j;
            continue;
        }
        // 其余（标点/空白）：合并为一段 Plain，直到下一个可能的特殊起点
        let start = i;
        loop {
            if i >= n {
                break;
            }
            let rest = &text[i..];
            let c = rest.chars().next().unwrap();
            if lang.line.iter().any(|p| rest.starts_with(*p)) {
                break;
            }
            if let Some((bs, _)) = lang.block {
                if rest.starts_with(bs) {
                    break;
                }
            }
            // 多行定界符可能不以引号/字母开头（如 Lua 的 [[），需显式让位
            if lang.multi.iter().any(|(o, _)| rest.starts_with(o)) {
                break;
            }
            if lang.deco && c == '@' {
                break;
            }
            if lang.strings.contains(&c) || c.is_ascii_digit() || c == '_' || c.is_alphabetic() {
                break;
            }
            i += c.len_utf8();
        }
        if i > start {
            segs.push((start, i, Tok::Plain));
        }
    }
    segs
}

/// 行首状态（跨行结构）：常规 / 在块注释内 / 在多行字符串内（记录收尾定界符）。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineState {
    Normal,
    InComment,
    InStr(&'static str),
}

/// 全文单遍扫描，得出每一行行首所处的状态（供逐行高亮时正确延续
/// docstring / 块注释的着色）。行数 = 换行符数 + 1。
pub fn line_states(text: &str, ext: &str) -> Vec<LineState> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let mut states = vec![LineState::Normal; starts.len()];
    let lang = lang_for(ext);
    if lang.multi.is_empty() && lang.block.is_none() && !lang.strings.contains(&'`') {
        return states; // 该语言没有跨行结构
    }
    for (s, e, tok) in tokenize(text, &lang) {
        let state = match tok {
            Tok::Comment => LineState::InComment, // 行注释不跨行，下方循环自然不命中
            Tok::Str => {
                // 该串的收尾定界符：多行定界符（可能带 f/r/b 前缀字母，检查 s 与跳过前缀后的 k
                // 两个位置）或反引号（JS 模板串 / Go raw 串，可跨行）；单行串不会命中下方循环
                let mut k = s;
                while k < e && text.as_bytes()[k].is_ascii_alphabetic() {
                    k += 1;
                }
                if let Some((_, cl2)) = lang
                    .multi
                    .iter()
                    .find(|(o, _)| text[s..].starts_with(o) || text[k..].starts_with(o))
                {
                    LineState::InStr(cl2)
                } else if text.as_bytes().get(k) == Some(&b'`') {
                    LineState::InStr("`")
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        // 标记落在该 token 内部的行首（第一个 > s 的行起点起，到 e 之前）
        let mut li = starts.partition_point(|&p| p <= s);
        while li < starts.len() && starts[li] < e {
            states[li] = state;
            li += 1;
        }
    }
    states
}

/// 按行首状态起始分词：先把「延续中的多行结构」收尾，再对剩余部分常规分词。
fn tokenize_with_state(text: &str, lang: &Lang, state: LineState) -> Vec<(usize, usize, Tok)> {
    let mut segs: Vec<(usize, usize, Tok)> = Vec::new();
    let mut i = 0usize;
    match state {
        LineState::InComment => {
            if let Some((_, be)) = lang.block {
                let end = text.find(be).map(|e| e + be.len()).unwrap_or(text.len());
                segs.push((0, end, Tok::Comment));
                i = end;
            }
        }
        LineState::InStr(delim) => {
            let end = text
                .find(delim)
                .map(|e| e + delim.len())
                .unwrap_or(text.len());
            segs.push((0, end, Tok::Str));
            i = end;
        }
        LineState::Normal => {}
    }
    if i < text.len() {
        segs.extend(
            tokenize(&text[i..], lang)
                .into_iter()
                .map(|(s, e, t)| (s + i, e + i, t)),
        );
    }
    segs
}

/// 对整行 `line` 按 `state` 分词，仅对窗口 `win`（字节范围）生成布局；
/// `errors` 的字节范围以窗口起点为 0（调用方已裁剪平移）。
/// 分词整行是为了跨行/行内状态正确；布局只做窗口，超长行不付整行 layout 成本。
pub fn highlight_segment(
    line: &str,
    win: Range<usize>,
    ext: &str,
    font_size: f32,
    errors: &[Range<usize>],
    state: LineState,
) -> LayoutJob {
    let lang = lang_for(ext);
    // 只分词到窗口右界即可：窗口之后的 token 在下方 `s.min(win.end)` 全被裁掉、纯属浪费。
    // 超长行（日志/JSON/minified）只在左侧可见时，此举把每帧整行分词降为「仅可见前缀」，
    // 根治「拖到底部有超长行时卡顿一下」。win.end 已是字符边界（调用方 char_to_byte 得到）。
    let scan_end = win.end.min(line.len());
    let toks = tokenize_with_state(&line[..scan_end], &lang, state);
    let font = FontId::monospace(font_size);
    let mut job = LayoutJob::default();
    for (s, e, tok) in toks {
        // 裁剪到窗口并转为窗口相对偏移
        let (s, e) = (s.max(win.start), e.min(win.end));
        if s >= e {
            continue;
        }
        let (rs, re) = (s - win.start, e - win.start);
        // 段内若与某错误范围相交，则进一步按边界切分以便只给错误处加下划线
        let mut p = rs;
        while p < re {
            let err = errors
                .iter()
                .find(|r| r.start <= p && p < r.end && r.start < re);
            let (seg_end, underline) = if let Some(r) = err {
                (r.end.min(re), true)
            } else {
                // 下一个错误起点（若在本段内）作为切点
                let next = errors
                    .iter()
                    .filter(|r| r.start > p && r.start < re)
                    .map(|r| r.start)
                    .min()
                    .unwrap_or(re);
                (next, false)
            };
            let mut fmt = TextFormat::simple(font.clone(), color(tok));
            if underline {
                fmt.underline = egui::Stroke::new(1.0, Color32::from_rgb(0xd0, 0x20, 0x20));
            }
            job.append(&line[win.start + p..win.start + seg_end], 0.0, fmt);
            p = seg_end;
        }
    }
    job
}
