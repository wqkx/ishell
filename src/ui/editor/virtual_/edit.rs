//! 编辑核心：插入/删除/撤销、光标移动与多选、词补全。

use crate::ui::highlight::{self, Indent};
use super::super::{EditOp, Editor};
use super::fold::v_remap_folds;
use super::geom::{
    char_to_byte, next_char_boundary, prev_char_boundary, v_line_of, v_line_range, v_sel_range,
};
use super::wrap::{v_byte_of_vpos, v_recompute, v_total_vrows, v_vpos_of_byte};

// ——— 缓冲词补全 ———
/// 重建词表（内容版本变化时）：提取长度 3..=48、以字母/下划线开头的标识符，去重排序。
/// 超大文件跳过重建（沿用旧表），避免每次按键付全文扫描成本。
pub(super) fn v_build_words(ed: &mut Editor) {
    if ed.words_ver == ed.vver {
        return;
    }
    ed.words_ver = ed.vver;
    if ed.content.len() > 2 * 1024 * 1024 {
        return;
    }
    let words: Vec<String> = {
        let mut set: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for w in ed.content.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if (3..=48).contains(&w.len()) && (w.as_bytes()[0].is_ascii_alphabetic() || w.starts_with('_')) {
                set.insert(w);
            }
        }
        let mut v: Vec<String> = set.into_iter().map(str::to_string).collect();
        v.sort_unstable();
        v
    };
    ed.words = words;
}
/// 光标前的词前缀（ASCII 标识符字符），返回 (字节长, 前缀)；不足 2 字符返回 None。
pub(super) fn v_word_prefix(ed: &Editor) -> Option<(usize, String)> {
    let b = ed.vcaret.min(ed.content.len());
    let bytes = ed.content.as_bytes();
    let mut start = b;
    while start > 0 {
        let c = bytes[start - 1];
        if c.is_ascii_alphanumeric() || c == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    let prefix = &ed.content[start..b];
    (prefix.len() >= 2 && (bytes[start].is_ascii_alphabetic() || prefix.starts_with('_'))).then(|| (prefix.len(), prefix.to_string()))
}
/// 按光标前缀打开/刷新补全弹窗；无候选则关闭。
/// 候选 = 缓冲区单词（优先）+ 该语言关键字/常见内置名（补足），去重、至多 8 条。
pub(super) fn v_complete_refresh(ed: &mut Editor) {
    let Some((plen, prefix)) = v_word_prefix(ed) else {
        ed.complete = None;
        return;
    };
    v_build_words(ed);
    let mut items: Vec<String> = ed.words.iter().filter(|w| w.starts_with(&prefix) && w.as_str() != prefix).take(8).cloned().collect();
    if items.len() < 8 {
        for w in highlight::completion_words(&ed.language) {
            if w.starts_with(prefix.as_str()) && w != prefix && !items.iter().any(|x| x == w) {
                items.push(w.to_string());
                if items.len() >= 8 {
                    break;
                }
            }
        }
    }
    ed.complete = if items.is_empty() { None } else { Some((items, 0, plen)) };
}
/// 接受补全候选：把候选词剩余部分插入光标处。
pub(super) fn v_complete_accept(ed: &mut Editor, idx: usize) {
    if let Some((items, _, plen)) = ed.complete.take() {
        if let Some(w) = items.get(idx) {
            let suffix = w[plen..].to_string();
            if !suffix.is_empty() {
                v_insert(ed, &suffix);
            }
        }
    }
}

/// 内容替换（content[at..at+removed_len] → inserted）**之前**调用：按行数增量平移
/// 折叠区间；行结构未变（无换行增删）时折叠原样保留；与编辑行重叠的折叠保守展开。
pub(super) fn v_apply(ed: &mut Editor, at: usize, removed_len: usize, inserted: &str) {
    v_remap_folds(ed, at, removed_len, inserted);
    let caret_before = ed.vcaret;
    let removed = ed.content[at..at + removed_len].to_string();
    ed.content.replace_range(at..at + removed_len, inserted);
    ed.vcaret = at + inserted.len();
    ed.vsel = None;
    // 连续单段输入（非换行）合并到上一条，避免每个字符一条撤销记录
    let mergeable = removed.is_empty() && !inserted.is_empty() && !inserted.contains('\n');
    if mergeable {
        if let Some(last) = ed.vundo.last_mut() {
            if last.removed.is_empty() && !last.inserted.ends_with('\n') && last.at + last.inserted.len() == at {
                last.inserted.push_str(inserted);
                last.caret_after = ed.vcaret;
                ed.vredo.clear();
                v_recompute(ed);
                return;
            }
        }
    }
    ed.vundo.push(EditOp { at, removed, inserted: inserted.to_string(), caret_before, caret_after: ed.vcaret });
    if ed.vundo.len() > 5000 {
        ed.vundo.remove(0);
    }
    ed.vredo.clear();
    v_recompute(ed);
}
pub(super) fn v_delete_selection(ed: &mut Editor) -> bool {
    if let Some((a, b)) = v_sel_range(ed) {
        v_apply(ed, a, b - a, "");
        ed.vgoal_col = None;
        true
    } else {
        ed.vsel = None;
        false
    }
}
pub(super) fn v_insert(ed: &mut Editor, t: &str) {
    let (at, rl) = if let Some((a, b)) = v_sel_range(ed) { (a, b - a) } else { (ed.vcaret, 0) };
    v_apply(ed, at, rl, t);
    ed.vgoal_col = None;
}
/// 回车自动缩进：沿用当前行前导空白；行尾是 : { ( [ 时再加一级。
pub(super) fn v_newline_indent(ed: &mut Editor) {
    let at = v_sel_range(ed).map(|(a, _)| a).unwrap_or(ed.vcaret);
    let ls = v_line_range(ed, v_line_of(ed, at)).0;
    let before = &ed.content[ls..at.max(ls)];
    let lead: String = before.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let mut t = String::from("\n");
    t.push_str(&lead);
    if matches!(before.trim_end().chars().last(), Some(':' | '{' | '(' | '[')) {
        t.push_str(&ed.indent.unit());
    }
    v_insert(ed, &t);
}
/// 多行块缩进 / 反缩进（Tab / Shift+Tab）：对选区覆盖的每一行增删一个缩进单位，
/// 单次可撤销；完成后选中受影响的整块，便于连续调整。
pub(super) fn v_block_indent(ed: &mut Editor, add: bool) {
    let (a, b) = v_sel_range(ed).unwrap_or((ed.vcaret, ed.vcaret));
    let la = v_line_of(ed, a);
    // 选区末端恰在行首时不包含该行（主流编辑器惯例）
    let lb = v_line_of(ed, if b > a && ed.vlines.get(v_line_of(ed, b)).copied() == Some(b) { b - 1 } else { b });
    let start = v_line_range(ed, la).0;
    let end = v_line_range(ed, lb).1;
    let unit = ed.indent.unit();
    let mut out = String::with_capacity(end - start + (lb - la + 1) * unit.len());
    for (idx, line) in ed.content[start..end].split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if add {
            if !line.trim().is_empty() {
                out.push_str(&unit);
            }
            out.push_str(line);
        } else {
            // 反缩进：删一个 Tab，或至多一个缩进单位宽度的空格
            let mut rest = line;
            if let Some(r) = rest.strip_prefix('\t') {
                rest = r;
            } else {
                let w = match ed.indent {
                    Indent::Spaces(n) => n.max(1),
                    Indent::Tab => 4,
                };
                let strip = rest.len() - rest.trim_start_matches(' ').len();
                rest = &rest[strip.min(w)..];
            }
            out.push_str(rest);
        }
    }
    if out != ed.content[start..end] {
        v_apply(ed, start, end - start, &out);
        // 选中整块，支持连续 Tab/Shift+Tab
        ed.vsel = Some(start);
        ed.vcaret = start + out.len();
    }
    ed.vgoal_col = None;
}
pub(super) fn v_backspace(ed: &mut Editor) {
    if v_delete_selection(ed) {
        return;
    }
    if ed.vcaret == 0 {
        return;
    }
    let prev = prev_char_boundary(&ed.content, ed.vcaret);
    v_apply(ed, prev, ed.vcaret - prev, "");
    ed.vgoal_col = None;
}
pub(super) fn v_delete_fwd(ed: &mut Editor) {
    if v_delete_selection(ed) {
        return;
    }
    if ed.vcaret >= ed.content.len() {
        return;
    }
    let next = next_char_boundary(&ed.content, ed.vcaret);
    v_apply(ed, ed.vcaret, next - ed.vcaret, "");
    ed.vgoal_col = None;
}
pub(super) fn v_undo(ed: &mut Editor) {
    if let Some(op) = ed.vundo.pop() {
        let end = op.at + op.inserted.len();
        v_remap_folds(ed, op.at, op.inserted.len(), &op.removed);
        ed.content.replace_range(op.at..end, &op.removed);
        ed.vcaret = op.caret_before.min(ed.content.len());
        ed.vsel = None;
        ed.vgoal_col = None;
        v_recompute(ed);
        ed.vredo.push(op);
    }
}
pub(super) fn v_redo(ed: &mut Editor) {
    if let Some(op) = ed.vredo.pop() {
        let end = op.at + op.removed.len();
        v_remap_folds(ed, op.at, op.removed.len(), &op.inserted);
        ed.content.replace_range(op.at..end, &op.inserted);
        ed.vcaret = op.caret_after.min(ed.content.len());
        ed.vsel = None;
        ed.vgoal_col = None;
        v_recompute(ed);
        ed.vundo.push(op);
    }
}
pub(super) fn v_move_h(ed: &mut Editor, fwd: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        if let Some((a, b)) = v_sel_range(ed) {
            ed.vcaret = if fwd { b } else { a };
            ed.vsel = None;
            return;
        }
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = if fwd { next_char_boundary(&ed.content, ed.vcaret) } else { prev_char_boundary(&ed.content, ed.vcaret) };
}
pub(super) fn v_move_v(ed: &mut Editor, delta: isize, shift: bool) {
    if shift && ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    if !shift {
        ed.vsel = None;
    }
    // 按「视觉行」上下移动（保持视觉列）：换行/非换行都维护该映射，且自动跳过折叠行
    if ed.vrow_cols > 0 && !ed.vrow_pre.is_empty() {
        let cols = ed.vrow_cols;
        let (vrow, vcol) = v_vpos_of_byte(ed, ed.vcaret, cols);
        let goal = ed.vgoal_col.unwrap_or(vcol);
        ed.vgoal_col = Some(goal);
        let total = v_total_vrows(ed);
        let target = (vrow as isize + delta).clamp(0, total.saturating_sub(1) as isize) as usize;
        ed.vcaret = v_byte_of_vpos(ed, target, goal, cols);
        return;
    }
    let line = v_line_of(ed, ed.vcaret);
    let (ls, _) = v_line_range(ed, line);
    let col = ed.vgoal_col.unwrap_or_else(|| ed.content[ls..ed.vcaret].chars().count());
    ed.vgoal_col = Some(col);
    let target = (line as isize + delta).clamp(0, ed.vlines.len() as isize - 1) as usize;
    let (ts, te) = v_line_range(ed, target);
    let line_chars = ed.content[ts..te].chars().count();
    let c = col.min(line_chars);
    ed.vcaret = ts + char_to_byte(&ed.content[ts..te], c);
}
pub(super) fn v_move_edge(ed: &mut Editor, end: bool, shift: bool) {
    ed.vgoal_col = None;
    if shift && ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    if !shift {
        ed.vsel = None;
    }
    let line = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, line);
    ed.vcaret = if end { le } else { ls };
}

/// 词边界：从字节 b 向前/后找下一个词边界（跳过空白，再跳过一段同类字符；换行单独成界）。
pub(super) fn v_word_boundary(s: &str, b: usize, fwd: bool) -> usize {
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = b.min(s.len());
    if fwd {
        loop {
            match s[i..].chars().next() {
                Some(c) if c.is_whitespace() && c != '\n' => i += c.len_utf8(),
                _ => break,
            }
        }
        if let Some('\n') = s[i..].chars().next() {
            return i + 1;
        }
        let word = s[i..].chars().next().map(is_w).unwrap_or(false);
        loop {
            match s[i..].chars().next() {
                Some(c) if c != '\n' && !c.is_whitespace() && is_w(c) == word => i += c.len_utf8(),
                _ => break,
            }
        }
    } else {
        loop {
            match s[..i].chars().next_back() {
                Some(c) if c.is_whitespace() && c != '\n' => i -= c.len_utf8(),
                _ => break,
            }
        }
        if let Some('\n') = s[..i].chars().next_back() {
            return i - 1;
        }
        let word = s[..i].chars().next_back().map(is_w).unwrap_or(false);
        loop {
            match s[..i].chars().next_back() {
                Some(c) if c != '\n' && !c.is_whitespace() && is_w(c) == word => i -= c.len_utf8(),
                _ => break,
            }
        }
    }
    i
}
/// 光标处的「词」字节范围（前后扩展词字符）；无词则 None。
pub(super) fn v_word_range(s: &str, pos: usize) -> Option<(usize, usize)> {
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = pos.min(s.len());
    let mut end = start;
    while start > 0 {
        let c = s[..start].chars().next_back().unwrap();
        if is_w(c) {
            start -= c.len_utf8();
        } else {
            break;
        }
    }
    while end < s.len() {
        let c = s[end..].chars().next().unwrap();
        if is_w(c) {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    (end > start).then_some((start, end))
}
// ——— 多光标（Ctrl+D 累加选区）———
/// 把最后一个选区的文本的「下一处」加入 msel（向后找、到尾环绕；跳过已在集合中的）。
pub(super) fn v_multi_add_next(ed: &mut Editor) {
    let &(ls, le) = match ed.msel.last() {
        Some(r) => r,
        None => return,
    };
    let needle = ed.content[ls..le].to_string();
    if needle.is_empty() {
        return;
    }
    let n = needle.len();
    let mut pos = le;
    for _ in 0..(ed.msel.len() + 2) {
        let p = match ed.content[pos.min(ed.content.len())..].find(&needle).map(|o| pos + o).or_else(|| ed.content.find(&needle)) {
            Some(p) => p,
            None => return,
        };
        let r = (p, p + n);
        if !ed.msel.contains(&r) {
            ed.msel.push(r);
            ed.msel.sort_by_key(|x| x.0);
            ed.vsel = Some(r.0);
            ed.vcaret = r.1;
            ed.pending_scroll = Some(v_line_of(ed, r.0));
            return;
        }
        pos = if p + n >= ed.content.len() { 0 } else { p + n };
    }
}
/// Ctrl+D：首次→选中当前选区/光标处的词并入集合；其后→加入下一处相同文本。
pub(super) fn v_ctrl_d(ed: &mut Editor) {
    if ed.msel.is_empty() {
        if let Some((a, b)) = v_sel_range(ed) {
            ed.msel.push((a, b));
            v_multi_add_next(ed);
        } else if let Some((a, b)) = v_word_range(&ed.content, ed.vcaret) {
            ed.msel.push((a, b));
            ed.vsel = Some(a);
            ed.vcaret = b;
        }
    } else {
        v_multi_add_next(ed);
    }
}
/// 把全部选区替换为 text（一次撤销记录），并把 msel 收为各插入点后的裸光标。
pub(super) fn v_multi_replace(ed: &mut Editor, text: &str) {
    let mut ranges = ed.msel.clone();
    ranges.sort_by_key(|r| r.0);
    let mut clean: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if clean.last().is_some_and(|l| s < l.1) {
            continue; // 跳过重叠
        }
        clean.push((s, e));
    }
    if clean.is_empty() {
        return;
    }
    let lo = clean.first().unwrap().0;
    let hi = clean.last().unwrap().1;
    let mut seg = String::new();
    let mut cursor = lo;
    let mut carets = Vec::new();
    for &(s, e) in &clean {
        seg.push_str(&ed.content[cursor..s]);
        seg.push_str(text);
        carets.push(lo + seg.len());
        cursor = e;
    }
    v_apply(ed, lo, hi - lo, &seg);
    ed.msel = carets.into_iter().map(|p| (p, p)).collect();
    ed.vcaret = ed.msel.last().map(|r| r.1).unwrap_or(ed.vcaret);
    ed.vsel = None;
    ed.vgoal_col = None;
}
pub(super) fn v_multi_backspace(ed: &mut Editor) {
    let del: Vec<(usize, usize)> = ed.msel.iter().map(|&(s, e)| if e > s { (s, e) } else { (prev_char_boundary(&ed.content, s), s) }).collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
pub(super) fn v_multi_delete(ed: &mut Editor) {
    let del: Vec<(usize, usize)> = ed.msel.iter().map(|&(s, e)| if e > s { (s, e) } else { (s, next_char_boundary(&ed.content, s)) }).collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
/// 多选模式下移动所有光标（左/右）：选区折叠到一侧，裸光标按字符移动；保持多选。
pub(super) fn v_multi_move(ed: &mut Editor, fwd: bool) {
    let mut carets: Vec<usize> = ed
        .msel
        .iter()
        .map(|&(s, e)| {
            if e > s {
                if fwd {
                    e
                } else {
                    s
                }
            } else if fwd {
                next_char_boundary(&ed.content, e)
            } else {
                prev_char_boundary(&ed.content, s)
            }
        })
        .collect();
    carets.sort_unstable();
    carets.dedup();
    ed.msel = carets.into_iter().map(|p| (p, p)).collect();
    ed.vcaret = ed.msel.last().map(|r| r.1).unwrap_or(ed.vcaret);
    ed.vsel = None;
    ed.vgoal_col = None;
}
pub(super) fn v_multi_copy(ed: &Editor) -> String {
    let parts: Vec<String> = ed.msel.iter().filter(|&&(s, e)| e > s).map(|&(s, e)| ed.content[s..e].to_string()).collect();
    parts.join("\n")
}
pub(super) fn v_move_word(ed: &mut Editor, fwd: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = v_word_boundary(&ed.content, ed.vcaret, fwd);
}
pub(super) fn v_delete_word(ed: &mut Editor, fwd: bool) {
    if v_delete_selection(ed) {
        return;
    }
    let to = v_word_boundary(&ed.content, ed.vcaret, fwd);
    let (a, b) = if fwd { (ed.vcaret, to) } else { (to, ed.vcaret) };
    if b > a {
        v_apply(ed, a, b - a, "");
    }
    ed.vgoal_col = None;
}
pub(super) fn v_move_doc(ed: &mut Editor, end: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = if end { ed.content.len() } else { 0 };
}
