//! 行级命令与括号配对。

use super::super::Editor;
use super::edit::v_apply;
use super::geom::{prev_char_boundary, v_line_of, v_line_range, v_sel_range};

pub(super) fn line_comment(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "rs" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "js" | "mjs" | "cjs" | "ts" | "tsx"
        | "jsx" | "go" | "java" | "kt" | "kts" | "swift" | "dart" | "cs" | "scala" | "php"
        | "rust" | "json5" | "proto" | "groovy" | "v" | "zig" | "vue" | "svelte" => "//",
        "py" | "pyw" | "rb" | "sh" | "bash" | "zsh" | "fish" | "pl" | "pm" | "r" | "jl"
        | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "config" | "properties"
        | "dockerfile" | "makefile" | "mk" | "cmake" | "gitignore" | "env" | "tcl" | "nim"
        | "awk" | "sed" | "gro" | "top" | "itp" | "mdp" | "ndx" => "#",
        "sql" | "lua" | "hs" | "ml" | "elm" | "adoc" => "--",
        "clj" | "cljs" | "lisp" | "el" | "asm" | "s" => ";",
        "vim" => "\"",
        _ => return None,
    })
}
pub(super) fn v_toggle_comment(ed: &mut Editor, prefix: &str) {
    let (sa, sb) = v_sel_range(ed).unwrap_or((ed.vcaret, ed.vcaret));
    let first = v_line_of(ed, sa);
    let last = v_line_of(ed, sb.max(sa).saturating_sub(if sb > sa { 1 } else { 0 }));
    // 判定是否「全部已注释」：非空行都以前缀开头 → 反注释，否则加注释
    let mut all = true;
    for li in first..=last {
        let (ls, le) = v_line_range(ed, li);
        let t = ed.content[ls..le].trim_start();
        if !t.is_empty() && !t.starts_with(prefix) {
            all = false;
            break;
        }
    }
    let pfx = format!("{prefix} ");
    // 整块重写后**一次** `v_apply`：逐行调用的话选了几行就是几条撤销记录（要按几次 Ctrl+Z
    // 才能还原），每行还各触发一次全文行索引重算。
    let start = v_line_range(ed, first).0;
    let end = v_line_range(ed, last).1;
    let mut out = String::with_capacity(end - start + (last - first + 1) * pfx.len());
    for (i, line) in ed.content[start..end].split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let indent = line.len() - line.trim_start().len();
        let (lead, body) = line.split_at(indent);
        out.push_str(lead);
        if all {
            match body.strip_prefix(prefix) {
                Some(rest) => out.push_str(rest.strip_prefix(' ').unwrap_or(rest)),
                None => out.push_str(body),
            }
        } else {
            if !body.is_empty() {
                out.push_str(&pfx);
            }
            out.push_str(body);
        }
    }
    if out != ed.content[start..end] {
        let had_sel = sb > sa;
        let old_caret = ed.vcaret;
        let grew = out.len() as isize - (end - start) as isize;
        v_apply(ed, start, end - start, &out);
        if had_sel {
            // 保持选中整块，便于再按一次切回去
            ed.vsel = Some(start);
            ed.vcaret = start + out.len();
        } else {
            let moved = (old_caret as isize + grew).max(start as isize) as usize;
            ed.vcaret = crate::ui::ime_safe::floor_boundary(&ed.content, moved);
        }
    }
    ed.vgoal_col = None;
}
pub(super) fn v_duplicate_line(ed: &mut Editor, down: bool) {
    let li = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, li);
    let next = super::geom::v_line_next(ed, li);
    let col = ed.vcaret - ls;
    if next > le {
        // 有行尾：整行连同它自己的行尾（`\n` 或 `\r\n`）复制一份，插在本行之前——
        // 原行一个字节不动，副本行尾与原行一致
        let whole = ed.content[ls..next].to_string();
        v_apply(ed, ls, 0, &whole);
        ed.vcaret = if down { next + col } else { ls + col };
        ed.vsel = None;
        ed.vgoal_col = None;
        return;
    }
    let line = ed.content[ls..le].to_string();
    if down {
        v_apply(ed, le, 0, &format!("\n{line}"));
        ed.vcaret = le + 1 + col;
    } else {
        v_apply(ed, ls, 0, &format!("{line}\n"));
        ed.vcaret = ls + col;
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
pub(super) fn v_move_line(ed: &mut Editor, up: bool) {
    let li = v_line_of(ed, ed.vcaret);
    let total = ed.vlines.len();
    if (up && li == 0) || (!up && li + 1 >= total) {
        return;
    }
    let col = ed.vcaret - v_line_range(ed, li).0;
    let (a, b) = if up { (li - 1, li) } else { (li, li + 1) };
    let (as_, _) = v_line_range(ed, a);
    let (bs, be) = v_line_range(ed, b);
    let ae = v_line_range(ed, a).1;
    let la = ed.content[as_..ae].to_string();
    let lb = ed.content[bs..be].to_string();
    // 两行之间原来的行尾（`\n` 或 `\r\n`）原样放回中间，不写死成 `\n`
    let mid = ed.content[ae..bs].to_string();
    v_apply(ed, as_, be - as_, &format!("{lb}{mid}{la}"));
    let target = if up { li - 1 } else { li + 1 };
    let (ts, te) = v_line_range(ed, target);
    ed.vcaret = ts + col.min(te - ts);
    ed.vsel = None;
    ed.vgoal_col = None;
}
pub(super) fn v_delete_line(ed: &mut Editor) {
    let li = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, li);
    let total = ed.vlines.len();
    if li + 1 < total {
        // 连同行尾一起删（CRLF 是两个字节，用「行尾 + 1」会把 `\n` 留下成一个空行）
        v_apply(ed, ls, super::geom::v_line_next(ed, li) - ls, "");
    } else if ls > 0 {
        // 最后一行：连同上一行的行尾一起删。从上一行的行末文字处起删——`ls - 1` 只删掉 `\n`，
        // CRLF 的 `\r` 会留下来成为正文
        let prev_end = v_line_range(ed, li - 1).1;
        v_apply(ed, prev_end, le - prev_end, "");
    } else {
        v_apply(ed, ls, le - ls, "");
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
/// 输入开括号/引号时自动补全的闭合符。
pub(super) fn auto_close_for(t: &str) -> Option<&'static str> {
    match t {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        "\"" => Some("\""),
        "'" => Some("'"),
        "`" => Some("`"),
        _ => None,
    }
}

/// 光标处已是配对闭合符时，再输入同一闭合符则跳过（对齐 VSCode / 常见编辑器）。
pub(super) fn skip_closing_pair(ed: &Editor, t: &str) -> bool {
    let close = match t {
        ")" | "]" | "}" | "\"" | "'" | "`" => t,
        _ => return false,
    };
    let i = ed.vcaret;
    if i >= ed.content.len() || !ed.content.is_char_boundary(i) {
        return false;
    }
    ed.content[i..].starts_with(close)
}

/// 若字节 bp 处是括号，返回 (该括号位置, 匹配括号位置)；否则 None。扫描有上限、忽略字符串/注释。
pub(super) fn bracket_at(s: &str, bp: usize) -> Option<(usize, usize)> {
    const OPENS: [char; 3] = ['(', '[', '{'];
    const CLOSES: [char; 3] = [')', ']', '}'];
    const CAP: usize = 200_000;
    if bp >= s.len() || !s.is_char_boundary(bp) {
        return None;
    }
    let c = s[bp..].chars().next()?;
    if let Some(oi) = OPENS.iter().position(|&o| o == c) {
        let close = CLOSES[oi];
        let mut depth = 1i32;
        for (off, ch) in s[bp + c.len_utf8()..].char_indices().take(CAP) {
            if ch == c {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some((bp, bp + c.len_utf8() + off));
                }
            }
        }
        None
    } else if let Some(ci) = CLOSES.iter().position(|&o| o == c) {
        let open = OPENS[ci];
        let mut depth = 1i32;
        let mut i = bp;
        let mut n = 0usize;
        while i > 0 && n < CAP {
            let ch = s[..i].chars().next_back().unwrap();
            i -= ch.len_utf8();
            n += 1;
            if ch == c {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    return Some((bp, i));
                }
            }
        }
        None
    } else {
        None
    }
}
/// 找到与 caret 相邻（左/右）的括号及其匹配位置。
pub(super) fn bracket_match(s: &str, caret: usize) -> Option<(usize, usize)> {
    if caret > 0 {
        let before = prev_char_boundary(s, caret);
        if let Some(r) = bracket_at(s, before) {
            return Some(r);
        }
    }
    bracket_at(s, caret)
}
