//! 轻量代码高亮 + 缩进探测 + 初级 lint（括号配对），无外部重依赖。
//!
//! 高亮：按扩展名取语言规格（注释/字符串风格 + 关键字集），单遍扫描分词后逐段着色。
//! 覆盖常见语言的注释、字符串、数字、关键字；不做完整语法分析，足够清晰且轻量。

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

/// 缩进风格（自动探测）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Indent {
    Spaces(usize),
    Tab,
}

impl Indent {
    /// 一个缩进层级对应的字符串。
    pub fn unit(&self) -> String {
        match self {
            Indent::Spaces(n) => " ".repeat(*n),
            Indent::Tab => "\t".into(),
        }
    }
    /// 人类可读标签（状态栏显示）。
    pub fn label(&self) -> String {
        match self {
            Indent::Spaces(n) => format!("{n} 空格"),
            Indent::Tab => "Tab".into(),
        }
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// 自动探测文件缩进：Tab 占多数→Tab；否则取各行前导空格数的最大公约数作为缩进宽度。
pub fn detect_indent(text: &str) -> Indent {
    let mut tabs = 0usize;
    let mut space_lines = 0usize;
    let mut g = 0usize;
    for line in text.lines() {
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let lead = line.bytes().take_while(|b| *b == b' ').count();
        if lead > 0 {
            space_lines += 1;
            if lead % 2 == 0 {
                g = gcd(g, lead);
            }
        }
    }
    if tabs > 0 && tabs >= space_lines {
        return Indent::Tab;
    }
    match g {
        0 => Indent::Spaces(4), // 没有可判定的缩进，默认 4
        2 => Indent::Spaces(2),
        n if n % 4 == 0 => Indent::Spaces(4),
        n => Indent::Spaces(n.max(2)),
    }
}

/// 语言规格：注释/字符串风格 + 关键字集。
struct Lang {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    strings: &'static [char],
    keywords: &'static [&'static str],
}

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// 近白底上的配色（仿 VSCode Light）。
fn color(t: Tok) -> Color32 {
    match t {
        Tok::Plain => Color32::from_rgb(0x24, 0x29, 0x2e),   // 近黑
        Tok::Comment => Color32::from_rgb(0x00, 0x80, 0x00), // 绿
        Tok::Str => Color32::from_rgb(0xa3, 0x15, 0x15),     // 暗红
        Tok::Num => Color32::from_rgb(0x09, 0x86, 0x58),     // 青绿
        Tok::Keyword => Color32::from_rgb(0x00, 0x00, 0xd0), // 蓝
    }
}

// —— 关键字集（常见子集，够用即可）——
const KW_RUST: &[&str] = &["as","async","await","break","const","continue","crate","dyn","else","enum","extern","false","fn","for","if","impl","in","let","loop","match","mod","move","mut","pub","ref","return","self","Self","static","struct","super","trait","true","type","unsafe","use","where","while","union"];
const KW_PY: &[&str] = &["and","as","assert","async","await","break","class","continue","def","del","elif","else","except","False","finally","for","from","global","if","import","in","is","lambda","None","nonlocal","not","or","pass","raise","return","True","try","while","with","yield","match","case","self"];
const KW_JS: &[&str] = &["async","await","break","case","catch","class","const","continue","debugger","default","delete","do","else","export","extends","false","finally","for","function","if","import","in","instanceof","let","new","null","of","return","static","super","switch","this","throw","true","try","typeof","undefined","var","void","while","yield","interface","type","enum","public","private","readonly"];
const KW_C: &[&str] = &["auto","bool","break","case","char","class","const","constexpr","continue","default","delete","do","double","else","enum","extern","false","float","for","goto","if","inline","int","long","namespace","new","nullptr","operator","private","protected","public","register","return","short","signed","sizeof","static","struct","switch","template","this","true","typedef","typename","union","unsigned","using","virtual","void","volatile","while"];
const KW_GO: &[&str] = &["break","case","chan","const","continue","default","defer","else","fallthrough","for","func","go","goto","if","import","interface","map","package","range","return","select","struct","switch","type","var","nil","true","false"];
const KW_JAVA: &[&str] = &["abstract","boolean","break","byte","case","catch","char","class","const","continue","default","do","double","else","enum","extends","final","finally","float","for","if","implements","import","instanceof","int","interface","long","native","new","null","package","private","protected","public","return","short","static","super","switch","synchronized","this","throw","throws","true","false","try","void","volatile","while","var"];
const KW_SH: &[&str] = &["if","then","else","elif","fi","case","esac","for","while","until","do","done","in","function","select","return","local","export","readonly","declare","echo","exit","break","continue","set","unset"];
const KW_RUBY: &[&str] = &["alias","and","begin","break","case","class","def","defined?","do","else","elsif","end","ensure","false","for","if","in","module","next","nil","not","or","redo","rescue","retry","return","self","super","then","true","undef","unless","until","when","while","yield"];
const KW_SQL: &[&str] = &["select","from","where","insert","into","values","update","set","delete","create","table","drop","alter","add","primary","key","foreign","references","join","left","right","inner","outer","on","group","by","order","having","limit","offset","and","or","not","null","as","distinct","count","sum","avg","min","max","index","view","union","all","like","between","in","exists","case","when","then","else","end"];
const KW_LUA: &[&str] = &["and","break","do","else","elseif","end","false","for","function","goto","if","in","local","nil","not","or","repeat","return","then","true","until","while"];

fn lang_for(ext: &str) -> Lang {
    let cl: &[char] = &['"', '\'', '`'];
    match ext {
        "rs" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: &['"'], keywords: KW_RUST },
        "py" | "pyw" => Lang { line: &["#"], block: None, strings: cl, keywords: KW_PY },
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: cl, keywords: KW_JS },
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cu" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_C },
        "go" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: cl, keywords: KW_GO },
        "java" | "kt" | "kts" | "swift" | "scala" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_JAVA },
        "sh" | "bash" | "zsh" | "fish" => Lang { line: &["#"], block: None, strings: &['"', '\''], keywords: KW_SH },
        "rb" => Lang { line: &["#"], block: None, strings: cl, keywords: KW_RUBY },
        "php" => Lang { line: &["//", "#"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_JS },
        "sql" => Lang { line: &["--"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_SQL },
        "lua" => Lang { line: &["--"], block: Some(("--[[", "]]")), strings: &['"', '\''], keywords: KW_LUA },
        "toml" | "ini" | "cfg" | "conf" | "yaml" | "yml" => Lang { line: &["#"], block: None, strings: &['"', '\''], keywords: &[] },
        "html" | "xml" | "svg" | "vue" => Lang { line: &[], block: Some(("<!--", "-->")), strings: &['"', '\''], keywords: &[] },
        "css" | "scss" | "less" => Lang { line: &[], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: &[] },
        "json" => Lang { line: &[], block: None, strings: &['"'], keywords: &[] },
        // 未知：C 风格注释 + 常见字符串，无关键字（仍高亮注释/字符串/数字）
        _ => Lang { line: &["//", "#"], block: Some(("/*", "*/")), strings: cl, keywords: &[] },
    }
}

/// 单遍分词，返回 (字节范围, 类别) 列表（连续 Plain 已合并）。
fn tokenize(text: &str, lang: &Lang) -> Vec<(usize, usize, Tok)> {
    let mut segs: Vec<(usize, usize, Tok)> = Vec::new();
    let n = text.len();
    let mut i = 0usize;
    while i < n {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();
        // 行注释
        if let Some(lc) = lang.line.iter().find(|p| rest.starts_with(**p)) {
            let _ = lc;
            let end = rest.find('\n').map(|e| i + e).unwrap_or(n);
            segs.push((i, end, Tok::Comment));
            i = end;
            continue;
        }
        // 块注释
        if let Some((bs, be)) = lang.block {
            if rest.starts_with(bs) {
                let end = rest[bs.len()..].find(be).map(|e| i + bs.len() + e + be.len()).unwrap_or(n);
                segs.push((i, end, Tok::Comment));
                i = end;
                continue;
            }
        }
        // 字符串（处理 \ 转义；非反引号串遇换行即结束，避免漏闭合时染色整篇）
        if lang.strings.contains(&c) {
            let quote = c;
            let mut j = i + quote.len_utf8();
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj == '\\' {
                    j += cj.len_utf8();
                    if j < n {
                        j += text[j..].chars().next().unwrap().len_utf8();
                    }
                    continue;
                }
                if cj == quote {
                    j += cj.len_utf8();
                    break;
                }
                if cj == '\n' && quote != '`' {
                    break;
                }
                j += cj.len_utf8();
            }
            segs.push((i, j, Tok::Str));
            i = j;
            continue;
        }
        // 数字
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj.is_ascii_alphanumeric() || cj == '.' || cj == '_' {
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            segs.push((i, j, Tok::Num));
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
            let t = if lang.keywords.contains(&word) { Tok::Keyword } else { Tok::Plain };
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

/// 生成高亮布局；`errors` 中的字节范围加红色下划线（初级错误标记）。
pub fn highlight(text: &str, ext: &str, font_size: f32, errors: &[Range<usize>]) -> LayoutJob {
    let lang = lang_for(ext);
    let segs = tokenize(text, &lang);
    let font = FontId::monospace(font_size);
    let mut job = LayoutJob::default();
    for (s, e, tok) in segs {
        // 段内若与某错误范围相交，则进一步按边界切分以便只给错误处加下划线
        let mut p = s;
        while p < e {
            let err = errors.iter().find(|r| r.start < e && r.end > p && r.start <= p && r.end > p);
            let (seg_end, underline) = if let Some(r) = err {
                (r.end.min(e), true)
            } else {
                // 下一个错误起点（若在本段内）作为切点
                let next = errors.iter().filter(|r| r.start > p && r.start < e).map(|r| r.start).min().unwrap_or(e);
                (next, false)
            };
            let mut fmt = TextFormat::simple(font.clone(), color(tok));
            if underline {
                fmt.underline = egui::Stroke::new(1.0, Color32::from_rgb(0xd0, 0x20, 0x20));
            }
            job.append(&text[p..seg_end], 0.0, fmt);
            p = seg_end;
        }
    }
    job
}

/// 初级 lint：括号 () [] {} 配对检查（跳过注释/字符串内的括号）。
/// 返回 (出问题的 0 基行号集合, 字节范围集合用于下划线, 概述文案)。
pub fn lint_brackets(text: &str, ext: &str) -> (Vec<usize>, Vec<Range<usize>>, Option<String>) {
    let lang = lang_for(ext);
    let segs = tokenize(text, &lang);
    let mut stack: Vec<(char, usize)> = Vec::new(); // (开括号, 字节位置)
    let mut bad: Vec<usize> = Vec::new(); // 字节位置
    for (s, e, tok) in &segs {
        if *tok != Tok::Plain {
            continue; // 只看代码区的括号
        }
        let mut p = *s;
        while p < *e {
            let c = text[p..].chars().next().unwrap();
            match c {
                '(' | '[' | '{' => stack.push((c, p)),
                ')' | ']' | '}' => {
                    let open = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    match stack.last() {
                        Some((o, _)) if *o == open => {
                            stack.pop();
                        }
                        _ => bad.push(p), // 多余/不匹配的闭括号
                    }
                }
                _ => {}
            }
            p += c.len_utf8();
        }
    }
    // 未闭合的开括号
    for (_, pos) in &stack {
        bad.push(*pos);
    }
    bad.sort_unstable();
    let ranges: Vec<Range<usize>> = bad.iter().map(|&b| b..(b + 1)).collect();
    let lines: Vec<usize> = bad.iter().map(|&b| text[..b].bytes().filter(|x| *x == b'\n').count()).collect();
    let msg = if bad.is_empty() {
        None
    } else {
        Some(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("⚠ {} 处括号不匹配", bad.len()),
            crate::i18n::Lang::En => format!("⚠ {} unmatched bracket(s)", bad.len()),
        })
    };
    (lines, ranges, msg)
}
