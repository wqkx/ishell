//! 语法 lint：括号/字符串/Python/JSON 粗检。

use std::ops::Range;

use super::lang::{lang_for, Lang};
use super::token::{tokenize, Tok};

/// 是否对该语言做括号 lint。仅常见、括号配平规则明确的编程语言才判断；
/// 文本/标记/配置/shell 等不判（避免对不认识的文本误报）。
pub fn lint_enabled(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cxx"
            | "hpp"
            | "hh"
            | "cu"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "mjs"
            | "cjs"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "scala"
            | "py"
            | "pyw"
            | "rb"
            | "php"
            | "lua"
            | "json"
            | "css"
            | "scss"
            | "less"
    )
}

/// 常规语法 lint（不依赖外部库/模块解析）：
/// - 括号 () [] {} 配对
/// - 未闭合字符串 / 多行字符串
/// - Python：列表/元组/调用参数中缺逗号、`:` 后空块、混用 Tab/空格缩进
/// - JSON：尾逗号、非法结构的粗检
///
/// 返回：出问题的 0 基行号、字节范围下划线、概述文案。
pub fn lint_syntax(text: &str, ext: &str) -> (Vec<usize>, Vec<Range<usize>>, Option<String>) {
    let lang = lang_for(ext);
    let segs = tokenize(text, &lang);
    let mut bad: Vec<usize> = Vec::new();

    // —— 1) 括号配对（仅 Plain 区）——
    let mut stack: Vec<(char, usize)> = Vec::new();
    for (s, e, tok) in &segs {
        if *tok != Tok::Plain {
            continue;
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
                        _ => bad.push(p),
                    }
                }
                _ => {}
            }
            p += c.len_utf8();
        }
    }
    for (_, pos) in &stack {
        bad.push(*pos);
    }

    // —— 2) 未闭合字符串（tokenize 遇换行/文末截断且无收尾定界符）——
    for (s, e, tok) in &segs {
        if *tok != Tok::Str || *e <= *s {
            continue;
        }
        if str_segment_unclosed(text, *s, *e, &lang) {
            bad.push(e.saturating_sub(1).max(*s));
        }
    }

    // —— 3) 语言特化 ——
    match ext {
        "py" | "pyw" => lint_python(text, &segs, &mut bad),
        "json" => lint_json(text, &segs, &mut bad),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "rs" | "go" | "c" | "h" | "cpp" | "cc"
        | "cxx" | "java" => {
            lint_comma_in_brackets(text, &segs, &mut bad);
        }
        _ => {}
    }

    bad.sort_unstable();
    bad.dedup();
    let ranges: Vec<Range<usize>> = bad
        .iter()
        .map(|&b| {
            let end = next_utf8_end(text, b);
            b..end
        })
        .collect();
    let lines: Vec<usize> = bad
        .iter()
        .map(|&b| {
            text[..b.min(text.len())]
                .bytes()
                .filter(|x| *x == b'\n')
                .count()
        })
        .collect();
    let msg = if bad.is_empty() {
        None
    } else {
        Some(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("⚠ {} 处语法问题", bad.len()),
            crate::i18n::Lang::En => format!("⚠ {} syntax issue(s)", bad.len()),
        })
    };
    (lines, ranges, msg)
}

fn next_utf8_end(text: &str, b: usize) -> usize {
    if b >= text.len() {
        return b;
    }
    text[b..]
        .chars()
        .next()
        .map(|c| b + c.len_utf8())
        .unwrap_or(b + 1)
}

/// 判断字符串 token 是否未正确闭合。
fn str_segment_unclosed(text: &str, s: usize, e: usize, lang: &Lang) -> bool {
    let slice = &text[s..e];
    let body = if lang.str_prefix {
        let pfx = slice
            .chars()
            .take_while(|c| matches!(c, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U'))
            .map(|c| c.len_utf8())
            .sum::<usize>()
            .min(slice.len());
        // 前缀后必须是引号/多行定界符，否则不是前缀串
        if pfx > 0 && pfx < slice.len() {
            &slice[pfx..]
        } else {
            slice
        }
    } else {
        slice
    };
    for (o, cl) in lang.multi {
        if body.starts_with(o) {
            // 未闭合：整段不以收尾定界符结束（scan_pair 染到文末时亦如此）
            return body.len() == o.len() || !body.ends_with(cl);
        }
    }
    let Some(q) = body.chars().next().filter(|c| lang.strings.contains(c)) else {
        return false;
    };
    if body.len() == q.len_utf8() {
        return true; // 只有开引号
    }
    if !body.ends_with(q) {
        return true;
    }
    // 以引号结束，但可能是转义的假收尾：…\"
    if body.len() > q.len_utf8() {
        let before = &body[..body.len() - q.len_utf8()];
        // 奇数个连续反斜杠 → 引号被转义 → 未真正闭合
        let mut bs = 0usize;
        for c in before.chars().rev() {
            if c == '\\' {
                bs += 1;
            } else {
                break;
            }
        }
        if bs % 2 == 1 {
            return true;
        }
    }
    false
}

/// 在 () [] {} 内的相邻「值」之间缺逗号时标出（跳过注释/字符串）。
/// 启发式：值结束后仅空白再接另一值，且无逗号 → 缺逗号（如 `[1, 2 3]`）。
fn lint_comma_in_brackets(text: &str, segs: &[(usize, usize, Tok)], bad: &mut Vec<usize>) {
    let mut depth = 0i32;
    // 上一「值」token（跳过纯空白 Plain，避免 `2` 与 `3` 被空格段隔开）
    let mut prev_val: Option<(usize, usize, Tok)> = None;
    for &(s, e, tok) in segs {
        if tok == Tok::Comment {
            continue;
        }
        // 纯空白 Plain：不打断值邻接，但若含括号仍更新深度
        let plain_ws = tok == Tok::Plain && text[s..e].chars().all(|c| c.is_whitespace());
        if tok == Tok::Plain {
            let mut p = s;
            while p < e {
                let c = text[p..].chars().next().unwrap();
                match c {
                    '(' | '[' | '{' => {
                        depth += 1;
                        prev_val = None; // 新括号层重新开始
                    }
                    ')' | ']' | '}' => {
                        depth -= 1;
                        prev_val = None;
                    }
                    ',' => prev_val = None, // 已有逗号，重置
                    _ => {}
                }
                p += c.len_utf8();
            }
        }
        if plain_ws {
            continue;
        }
        if depth <= 0 {
            prev_val = None;
            continue;
        }
        // Plain 里可能混有标点+标识；按「值起点/终点」判断
        if let Some((ps, pe, ptok)) = prev_val {
            if looks_like_value_end(text, ps, pe, ptok) && looks_like_value_start(text, s, e, tok) {
                let between = &text[pe..s];
                if !between.contains(',') && between.chars().all(|c| c.is_whitespace()) {
                    bad.push(s);
                }
            }
        }
        // 更新 prev：仅当本段可作为值终点时记住；纯标点 Plain（如 `, `）清空
        if looks_like_value_end(text, s, e, tok) {
            prev_val = Some((s, e, tok));
        } else if tok == Tok::Plain {
            // 含逗号等分隔符的 Plain 已在上面清空；其它标点也断开
            if text[s..e].contains(',') {
                prev_val = None;
            }
        }
    }
}

fn looks_like_value_end(text: &str, s: usize, e: usize, tok: Tok) -> bool {
    match tok {
        Tok::Num | Tok::Str | Tok::Keyword => true,
        Tok::Plain => {
            let t = text[s..e].trim_end();
            t.ends_with(')')
                || t.ends_with(']')
                || t.ends_with('}')
                || t.chars()
                    .next_back()
                    .is_some_and(|c| c == '_' || c.is_alphanumeric())
        }
        Tok::Comment => false,
    }
}

fn looks_like_value_start(text: &str, s: usize, e: usize, tok: Tok) -> bool {
    match tok {
        Tok::Num | Tok::Str => true,
        Tok::Keyword => {
            // True/False/None/null 等可作值；控制关键字不当值起点
            matches!(
                &text[s..e],
                "True" | "False" | "None" | "true" | "false" | "null" | "undefined" | "nil"
            )
        }
        Tok::Plain => {
            let t = text[s..e].trim_start();
            t.starts_with('(')
                || t.starts_with('[')
                || t.starts_with('{')
                || t.chars().next().is_some_and(|c| {
                    c == '_' || c.is_alphanumeric() || c == '"' || c == '\'' || c == '`'
                })
        }
        Tok::Comment => false,
    }
}

fn lint_python(text: &str, segs: &[(usize, usize, Tok)], bad: &mut Vec<usize>) {
    lint_comma_in_brackets(text, segs, bad);

    // 混用 Tab / 空格缩进
    let mut saw_space = false;
    let mut saw_tab = false;
    for (i, line) in text.split('\n').enumerate() {
        let mut sp = false;
        let mut tb = false;
        for b in line.bytes() {
            match b {
                b' ' => sp = true,
                b'\t' => tb = true,
                _ => break,
            }
        }
        if sp {
            saw_space = true;
        }
        if tb {
            saw_tab = true;
        }
        if sp && tb {
            // 同行混用：标在行首
            let off = text.split('\n').take(i).map(|l| l.len() + 1).sum::<usize>();
            bad.push(off);
        }
    }
    if saw_space && saw_tab {
        // 文件级混用：标在第一个 Tab 缩进行
        let mut off = 0usize;
        for line in text.split('\n') {
            if line.starts_with('\t') {
                bad.push(off);
                break;
            }
            off += line.len() + 1;
        }
    }

    // `:` 行尾后下一非空行缩进未加深（空块 / 缺 body）——仅启发式，跳过已是 pass/... 的情况
    let lines: Vec<&str> = text.split('\n').collect();
    let mut off = 0usize;
    for i in 0..lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        let code = strip_py_line_comment(trimmed);
        if code.ends_with(':') && !code.ends_with("::") {
            let lead = line
                .bytes()
                .take_while(|b| *b == b' ' || *b == b'\t')
                .count();
            // 找下一非空、非注释行
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.is_empty() || t.starts_with('#') {
                    j += 1;
                    continue;
                }
                break;
            }
            if j >= lines.len() {
                // 文件以 `:` 结尾且无 body
                bad.push(off + lead.max(1) - 1);
            } else {
                let next = lines[j];
                let nlead = next
                    .bytes()
                    .take_while(|b| *b == b' ' || *b == b'\t')
                    .count();
                if nlead <= lead && !next.trim_start().starts_with('#') {
                    // 同级或更浅 → 可能缺缩进块（except/elif/else/finally/case 同级合法）
                    let nt = next.trim_start();
                    let ok_peer = nt.starts_with("elif ")
                        || nt.starts_with("else:")
                        || nt.starts_with("except")
                        || nt.starts_with("finally:")
                        || nt.starts_with("case ")
                        || nt == "else:"
                        || nt == "finally:";
                    if !ok_peer {
                        bad.push(off + trimmed.len().saturating_sub(1));
                    }
                }
            }
        }
        off += line.len() + 1;
    }
}

fn strip_py_line_comment(line: &str) -> &str {
    // 粗略：不在字符串内的 #（足够用于 `:` 行尾判断）
    let mut in_s: Option<char> = None;
    let mut chars = line.char_indices();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = in_s {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == q {
                in_s = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            in_s = Some(c);
            continue;
        }
        if c == '#' {
            return line[..i].trim_end();
        }
    }
    line.trim_end()
}

fn lint_json(text: &str, segs: &[(usize, usize, Tok)], bad: &mut Vec<usize>) {
    // 尾逗号：`,` 后仅空白再遇 `]`/`}`
    let mut i = 0usize;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        // 跳过字符串段
        if let Some(&(s, e, Tok::Str)) = segs.iter().find(|&&(s, e, _)| s <= i && i < e) {
            i = e;
            let _ = s;
            continue;
        }
        if bytes[i] == b',' {
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b']' | b'}') {
                bad.push(i);
            }
        }
        i += 1;
    }
    lint_comma_in_brackets(text, segs, bad);
}
