//! 虚拟化编辑器：行映射/编辑操作/渲染循环。从 editor 拆出，行为不变。
//! 均为私有函数；`editable_virtual` 由 `content()` 调用。

use egui::RichText;

use egui::text::CCursor;

use crate::theme::Palette;
use crate::ui::highlight::{self, Indent};
use super::{EditOp, Editor};
use super::find::{FindOut, build_find_regex, find_widget, goto_widget};

/// 超过该大小则跳过括号 lint（避免大文件每次编辑都整体 tokenize）。
const LINT_LIMIT: usize = 256 * 1024;
/// 跨行高亮状态的全文扫描上限（超过则逐行独立高亮，不付每次编辑的全文扫描成本）
const HL_STATE_LIMIT: usize = 2 * 1024 * 1024;

// ———————————————————————— 虚拟化可编辑器（大文件，Phase 1） ————————————————————————

fn compute_line_starts(s: &str) -> Vec<usize> {
    let mut v = Vec::with_capacity(s.len() / 40 + 1);
    v.push(0);
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}
fn prev_char_boundary(s: &str, b: usize) -> usize {
    s[..b].chars().next_back().map(|c| b - c.len_utf8()).unwrap_or(0)
}
fn next_char_boundary(s: &str, b: usize) -> usize {
    s[b.min(s.len())..].chars().next().map(|c| b + c.len_utf8()).unwrap_or_else(|| s.len())
}
pub(super) fn v_line_of(ed: &Editor, b: usize) -> usize {
    ed.vlines.partition_point(|&s| s <= b).saturating_sub(1)
}
/// 第 i 行的字节范围 [起, 止)（止不含行尾换行符）。
fn v_line_range(ed: &Editor, i: usize) -> (usize, usize) {
    let s = ed.vlines[i];
    let e = if i + 1 < ed.vlines.len() { ed.vlines[i + 1] - 1 } else { ed.content.len() };
    (s, e)
}
pub(super) fn v_sel_range(ed: &Editor) -> Option<(usize, usize)> {
    ed.vsel.map(|a| (a.min(ed.vcaret), a.max(ed.vcaret))).filter(|(a, b)| a < b)
}
// ——— 自动换行（word-wrap）视觉行映射 ———
/// 某逻辑行按 cols 列折行后的视觉行数（按字符数近似，CJK 暂按 1 列计）。
fn line_vrows(chars: usize, cols: usize) -> u32 {
    (chars / cols + if !chars.is_multiple_of(cols) { 1 } else { 0 }).max(1) as u32
}
/// 同步换行行数前缀和缓存（列宽/内容/折叠变化时重算）。折叠区域内的行占 0 视觉行。
fn v_wrap_sync(ed: &mut Editor, cols: usize) {
    let cols = cols.max(1);
    if ed.vrow_cols == cols && ed.vrow_ver == ed.vver && ed.vrow_fver == ed.fold_ver && ed.vrow_pre.len() == ed.vlines.len() + 1 {
        return;
    }
    let n = ed.vlines.len();
    let mut pre = Vec::with_capacity(n + 1);
    let mut acc = 0u32;
    pre.push(0);
    for i in 0..n {
        if v_line_hidden(ed, i) {
            pre.push(acc);
            continue;
        }
        let (s, e) = v_line_range(ed, i);
        let chars = ed.content[s..e].chars().count();
        acc = acc.saturating_add(line_vrows(chars, cols));
        pre.push(acc);
    }
    ed.vrow_pre = pre;
    ed.vrow_cols = cols;
    ed.vrow_ver = ed.vver;
    ed.vrow_fver = ed.fold_ver;
}
// ——— 按缩进的代码折叠 ———
/// 该行是否被某个折叠区域隐藏（header 行本身可见）。
fn v_line_hidden(ed: &Editor, line: usize) -> bool {
    // folds 按 header 升序且不重叠：找最后一个 header < line 的区域
    let idx = ed.folds.partition_point(|&(h, _)| h < line);
    idx > 0 && ed.folds[idx - 1].1 >= line
}
/// 重建每行缩进列宽缓存（内容版本或缩进单位变化时）。O(总字符数)，仅编辑/切换缩进时付出。
fn v_sync_leads(ed: &mut Editor, unit: usize) {
    if ed.leads_ver == ed.vver && ed.leads_unit == unit && ed.leads.len() == ed.vlines.len() {
        return;
    }
    let n = ed.vlines.len();
    let mut leads = Vec::with_capacity(n);
    for i in 0..n {
        let (a, b) = v_line_range(ed, i);
        let mut lead = 0i32;
        let mut blank = true;
        for c in ed.content[a..b].chars() {
            match c {
                ' ' => lead += 1,
                '\t' => lead += unit as i32,
                _ => {
                    blank = false;
                    break;
                }
            }
        }
        leads.push(if blank { -1 } else { lead });
    }
    ed.leads = leads;
    ed.leads_ver = ed.vver;
    ed.leads_unit = unit;
}
/// 行的前导缩进列宽（Tab 按 unit 列计）；空白行返回 None。O(1) 查缓存
///（缓存由 v_sync_leads 在每帧渲染前用当前 unit 同步；未命中则回退实时扫描）。
fn v_lead(ed: &Editor, line: usize, unit: usize) -> Option<usize> {
    if ed.leads_ver == ed.vver && ed.leads_unit == unit {
        return match ed.leads.get(line) {
            Some(&-1) | None => None,
            Some(&v) => Some(v as usize),
        };
    }
    // 回退：缓存未同步（理论上不发生，v_sync_leads 每帧先跑）
    let (a, b) = v_line_range(ed, line);
    let mut lead = 0usize;
    for c in ed.content[a..b].chars() {
        match c {
            ' ' => lead += 1,
            '\t' => lead += unit,
            _ => return Some(lead),
        }
    }
    None
}
/// 是否可折叠（快速判定，供画箭头）：下一个非空行缩进更深即可折。只向下看少量行。
fn v_foldable(ed: &Editor, line: usize, unit: usize) -> bool {
    let Some(head) = v_lead(ed, line, unit) else { return false };
    let total = ed.vlines.len();
    let mut l = line + 1;
    while l < total && l <= line + 50 {
        match v_lead(ed, l, unit) {
            Some(d) => return d > head,
            None => l += 1, // 空白行跨过
        }
    }
    false
}
/// 以 line 为 header 的可折叠区域（按缩进推导）：其后连续「更深缩进或空白」的行；
/// 尾部空白行不计入。无可折叠内容返回 None。
fn v_fold_region(ed: &Editor, line: usize, unit: usize) -> Option<(usize, usize)> {
    let head = v_lead(ed, line, unit)?;
    let total = ed.vlines.len();
    let mut end = line;
    let mut l = line + 1;
    while l < total {
        match v_lead(ed, l, unit) {
            Some(d) if d > head => end = l,
            None => {} // 空白行：跨过，但不更新 end（尾部空行不折进去）
            _ => break,
        }
        l += 1;
    }
    (end > line).then_some((line, end))
}
// ——— 缓冲词补全 ———
/// 重建词表（内容版本变化时）：提取长度 3..=48、以字母/下划线开头的标识符，去重排序。
/// 超大文件跳过重建（沿用旧表），避免每次按键付全文扫描成本。
fn v_build_words(ed: &mut Editor) {
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
fn v_word_prefix(ed: &Editor) -> Option<(usize, String)> {
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
fn v_complete_refresh(ed: &mut Editor) {
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
fn v_complete_accept(ed: &mut Editor, idx: usize) {
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
fn v_remap_folds(ed: &mut Editor, at: usize, removed_len: usize, inserted: &str) {
    if ed.folds.is_empty() {
        return;
    }
    let added = inserted.matches('\n').count() as isize;
    let l0 = v_line_of(ed, at.min(ed.content.len()));
    let l1 = v_line_of(ed, (at + removed_len).min(ed.content.len()));
    let removed = (l1 - l0) as isize;
    if added == 0 && removed == 0 {
        return; // 行结构未变（行内编辑），折叠不动
    }
    let delta = added - removed;
    let mut changed = false;
    ed.folds.retain_mut(|f| {
        if l1 < f.0 {
            // 编辑完全在折叠之前 → 区间平移
            if delta != 0 {
                f.0 = (f.0 as isize + delta).max(0) as usize;
                f.1 = (f.1 as isize + delta).max(0) as usize;
                changed = true;
            }
            true
        } else if l0 > f.1 {
            true // 完全在折叠之后，不受影响
        } else {
            changed = true;
            false // 与折叠区间（含 header 行）重叠 → 展开
        }
    });
    if changed {
        ed.fold_ver = ed.fold_ver.wrapping_add(1);
    }
}
/// 切换某 header 行的折叠状态。
fn v_toggle_fold(ed: &mut Editor, line: usize, unit: usize) {
    if let Some(idx) = ed.folds.iter().position(|&(h, _)| h == line) {
        ed.folds.remove(idx);
        ed.fold_ver = ed.fold_ver.wrapping_add(1);
        return;
    }
    let Some((h, e)) = v_fold_region(ed, line, unit) else { return };
    // 移除与新区域重叠的旧折叠（含嵌套），保持互不重叠
    ed.folds.retain(|&(h2, e2)| e2 < h || h2 > e);
    let at = ed.folds.partition_point(|&(h2, _)| h2 < h);
    ed.folds.insert(at, (h, e));
    ed.fold_ver = ed.fold_ver.wrapping_add(1);
    // 光标在被折叠区域内 → 移到 header 行首，避免「看不见的光标」
    let cl = v_line_of(ed, ed.vcaret);
    if cl > h && cl <= e {
        ed.vcaret = v_line_range(ed, h).0;
        ed.vsel = None;
    }
}
/// 总视觉行数（换行模式）。
fn v_total_vrows(ed: &Editor) -> usize {
    ed.vrow_pre.last().copied().unwrap_or(0) as usize
}
/// 视觉行号 → (逻辑行, 段内序号)。
fn v_line_of_vrow(ed: &Editor, vrow: usize) -> (usize, usize) {
    let v = vrow as u32;
    let line = ed.vrow_pre.partition_point(|&p| p <= v).saturating_sub(1).min(ed.vlines.len().saturating_sub(1));
    let seg = vrow - ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (line, seg)
}
/// 字节偏移 → (视觉行, 段内列)。
fn v_vpos_of_byte(ed: &Editor, byte: usize, cols: usize) -> (usize, usize) {
    let cols = cols.max(1);
    let line = v_line_of(ed, byte);
    let (ls, _) = v_line_range(ed, line);
    let col = ed.content[ls..byte.max(ls)].chars().count();
    let base = ed.vrow_pre.get(line).copied().unwrap_or(0) as usize;
    (base + col / cols, col % cols)
}
/// (视觉行, 段内列) → 字节偏移（钳到行尾）。
fn v_byte_of_vpos(ed: &Editor, vrow: usize, vcol: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    let (line, seg) = v_line_of_vrow(ed, vrow);
    let (ls, le) = v_line_range(ed, line);
    let line_chars = ed.content[ls..le].chars().count();
    let col = (seg * cols + vcol).min(line_chars);
    ls + char_to_byte(&ed.content[ls..le], col)
}
pub(super) fn v_recompute(ed: &mut Editor) {
    ed.vver = ed.vver.wrapping_add(1); // 内容变更 → 换行行数缓存失效
    ed.vlines = compute_line_starts(&ed.content);
    // 最长行字节数（含尾行）——缓存，渲染时直接用，避免每帧扫全部行
    ed.vmax = ed
        .vlines
        .windows(2)
        .map(|w| w[1] - w[0])
        .chain(std::iter::once(ed.content.len() - ed.vlines.last().copied().unwrap_or(0)))
        .max()
        .unwrap_or(0);
}
/// 把 content[at..at+removed_len] 替换为 inserted，并记录一条可撤销操作（连续输入会合并）。
fn v_apply(ed: &mut Editor, at: usize, removed_len: usize, inserted: &str) {
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
fn v_delete_selection(ed: &mut Editor) -> bool {
    if let Some((a, b)) = v_sel_range(ed) {
        v_apply(ed, a, b - a, "");
        ed.vgoal_col = None;
        true
    } else {
        ed.vsel = None;
        false
    }
}
fn v_insert(ed: &mut Editor, t: &str) {
    let (at, rl) = if let Some((a, b)) = v_sel_range(ed) { (a, b - a) } else { (ed.vcaret, 0) };
    v_apply(ed, at, rl, t);
    ed.vgoal_col = None;
}
/// 回车自动缩进：沿用当前行前导空白；行尾是 : { ( [ 时再加一级。
fn v_newline_indent(ed: &mut Editor) {
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
fn v_block_indent(ed: &mut Editor, add: bool) {
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
fn v_backspace(ed: &mut Editor) {
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
fn v_delete_fwd(ed: &mut Editor) {
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
fn v_undo(ed: &mut Editor) {
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
fn v_redo(ed: &mut Editor) {
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
fn v_move_h(ed: &mut Editor, fwd: bool, shift: bool) {
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
fn v_move_v(ed: &mut Editor, delta: isize, shift: bool) {
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
fn v_move_edge(ed: &mut Editor, end: bool, shift: bool) {
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
fn v_word_boundary(s: &str, b: usize, fwd: bool) -> usize {
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
fn v_word_range(s: &str, pos: usize) -> Option<(usize, usize)> {
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
fn v_multi_add_next(ed: &mut Editor) {
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
fn v_ctrl_d(ed: &mut Editor) {
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
fn v_multi_replace(ed: &mut Editor, text: &str) {
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
fn v_multi_backspace(ed: &mut Editor) {
    let del: Vec<(usize, usize)> = ed.msel.iter().map(|&(s, e)| if e > s { (s, e) } else { (prev_char_boundary(&ed.content, s), s) }).collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
fn v_multi_delete(ed: &mut Editor) {
    let del: Vec<(usize, usize)> = ed.msel.iter().map(|&(s, e)| if e > s { (s, e) } else { (s, next_char_boundary(&ed.content, s)) }).collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
/// 多选模式下移动所有光标（左/右）：选区折叠到一侧，裸光标按字符移动；保持多选。
fn v_multi_move(ed: &mut Editor, fwd: bool) {
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
fn v_multi_copy(ed: &Editor) -> String {
    let parts: Vec<String> = ed.msel.iter().filter(|&&(s, e)| e > s).map(|&(s, e)| ed.content[s..e].to_string()).collect();
    parts.join("\n")
}
fn v_move_word(ed: &mut Editor, fwd: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = v_word_boundary(&ed.content, ed.vcaret, fwd);
}
fn v_delete_word(ed: &mut Editor, fwd: bool) {
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
fn v_move_doc(ed: &mut Editor, end: bool, shift: bool) {
    ed.vgoal_col = None;
    if !shift {
        ed.vsel = None;
    } else if ed.vsel.is_none() {
        ed.vsel = Some(ed.vcaret);
    }
    ed.vcaret = if end { ed.content.len() } else { 0 };
}
/// 该语言的行注释前缀（无则 None）。
fn line_comment(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "rs" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "java" | "kt" | "kts" | "swift" | "dart" | "cs" | "scala" | "php" | "rust" | "json5" | "proto" | "groovy" | "v" | "zig" | "vue" | "svelte" => "//",
        "py" | "pyw" | "rb" | "sh" | "bash" | "zsh" | "fish" | "pl" | "pm" | "r" | "jl" | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "config" | "properties" | "dockerfile" | "makefile" | "mk" | "cmake" | "gitignore" | "env" | "tcl" | "nim" | "awk" | "sed" | "gro" | "top" | "itp" | "mdp" | "ndx" => "#",
        "sql" | "lua" | "hs" | "ml" | "elm" | "adoc" => "--",
        "clj" | "cljs" | "lisp" | "el" | "asm" | "s" => ";",
        "vim" => "\"",
        _ => return None,
    })
}
fn v_toggle_comment(ed: &mut Editor, prefix: &str) {
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
    // 从后往前改，前面行的偏移不受影响
    for li in (first..=last).rev() {
        let (ls, le) = v_line_range(ed, li);
        let line = &ed.content[ls..le];
        let indent = line.len() - line.trim_start().len();
        if all {
            let after = &line[indent..];
            if after.starts_with(prefix) {
                let mut rm = prefix.len();
                if after[prefix.len()..].starts_with(' ') {
                    rm += 1;
                }
                v_apply(ed, ls + indent, rm, "");
            }
        } else if !line[indent..].is_empty() {
            v_apply(ed, ls + indent, 0, &pfx);
        }
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_duplicate_line(ed: &mut Editor, down: bool) {
    let li = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, li);
    let line = ed.content[ls..le].to_string();
    let col = ed.vcaret - ls;
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
fn v_move_line(ed: &mut Editor, up: bool) {
    let li = v_line_of(ed, ed.vcaret);
    let total = ed.vlines.len();
    if (up && li == 0) || (!up && li + 1 >= total) {
        return;
    }
    let col = ed.vcaret - v_line_range(ed, li).0;
    let (a, b) = if up { (li - 1, li) } else { (li, li + 1) };
    let (as_, _) = v_line_range(ed, a);
    let (bs, be) = v_line_range(ed, b);
    let la = ed.content[as_..v_line_range(ed, a).1].to_string();
    let lb = ed.content[bs..be].to_string();
    v_apply(ed, as_, be - as_, &format!("{lb}\n{la}"));
    let target = if up { li - 1 } else { li + 1 };
    let (ts, te) = v_line_range(ed, target);
    ed.vcaret = ts + col.min(te - ts);
    ed.vsel = None;
    ed.vgoal_col = None;
}
fn v_delete_line(ed: &mut Editor) {
    let li = v_line_of(ed, ed.vcaret);
    let (ls, le) = v_line_range(ed, li);
    let total = ed.vlines.len();
    if li + 1 < total {
        v_apply(ed, ls, (le + 1) - ls, "");
    } else if ls > 0 {
        v_apply(ed, ls - 1, le - (ls - 1), "");
    } else {
        v_apply(ed, ls, le - ls, "");
    }
    ed.vsel = None;
    ed.vgoal_col = None;
}
/// 输入开括号/引号时自动补全的闭合符。
fn auto_close_for(t: &str) -> Option<&'static str> {
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
fn skip_closing_pair(ed: &Editor, t: &str) -> bool {
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
fn bracket_at(s: &str, bp: usize) -> Option<(usize, usize)> {
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
fn bracket_match(s: &str, caret: usize) -> Option<(usize, usize)> {
    if caret > 0 {
        let before = prev_char_boundary(s, caret);
        if let Some(r) = bracket_at(s, before) {
            return Some(r);
        }
    }
    bracket_at(s, caret)
}

/// 虚拟化可编辑器：仅渲染可见行 + 自绘光标/选区。返回 true 表示请求保存（Ctrl+S）。
pub(super) fn editable_virtual(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    let mut save = false;
    if ed.vlines.is_empty() {
        v_recompute(ed);
    }
    ed.vcaret = ed.vcaret.min(ed.content.len());

    // 括号 lint：仅对配平规则明确的语言、且文件不过大；按内容版本缓存（编辑时才重算，不逐帧 tokenize）。
    if ed.lint_ver != ed.vver {
        ed.lint_ver = ed.vver;
        if ed.content.len() <= LINT_LIMIT && highlight::lint_enabled(&ed.language) {
            let (lines, ranges, msg) = highlight::lint_syntax(&ed.content, &ed.language);
            ed.lint_lines = lines.into_iter().collect();
            ed.lint_ranges = ranges;
            ed.lint_msg = msg;
        } else {
            ed.lint_lines.clear();
            ed.lint_ranges.clear();
            ed.lint_msg = None;
        }
    }

    // 跨行高亮状态（docstring / 块注释延续）：全文单遍扫描，按内容版本缓存。
    // 超大文件跳过（退化为逐行独立高亮），避免每次编辑付全文扫描成本。
    if ed.hl_ver != ed.vver {
        ed.hl_ver = ed.vver;
        if ed.content.len() <= HL_STATE_LIMIT {
            ed.hl_states = highlight::line_states(&ed.content, &ed.language);
        } else {
            ed.hl_states.clear();
        }
    }

    // 每行缩进列宽缓存：缩进线/粘性作用域行/折叠判定的按行探测据此做 O(1) 查表，
    // 避免拖动大文件时每帧反复切片扫描缩进造成的卡顿。
    let unit_cols_now = match ed.indent {
        Indent::Spaces(n) => n.max(1),
        Indent::Tab => 4,
    };
    v_sync_leads(ed, unit_cols_now);

    // 折叠维护：编辑时区间已由 v_remap_folds 平移/展开；
    // 这里只处理跳转/查找把光标放进隐藏行的情况——自动展开所在折叠
    if !ed.folds.is_empty() {
        let cl = v_line_of(ed, ed.vcaret);
        if v_line_hidden(ed, cl) {
            ed.folds.retain(|&(h, e)| !(cl > h && cl <= e));
            ed.fold_ver = ed.fold_ver.wrapping_add(1);
        }
    }

    let mut mono = egui::TextStyle::Monospace.resolve(ui.style());
    // 字号对齐到整数物理像素：让 hinting 网格与像素对齐，分数缩放（zoom/HiDPI）下笔画更锐
    let ppp = ui.ctx().pixels_per_point().max(0.5);
    mono.size = ((mono.size * ppp).round().max(1.0)) / ppp;
    let row_h = ui.ctx().fonts_mut(|f| f.row_height(&mono));
    let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, ' ')).max(1.0);
    let bg = egui::Color32::from_rgb(252, 252, 250);
    let focused = ui.memory(|m| m.focused() == Some(text_id));
    // 聚焦时尽早锁定 Tab/方向键/Esc 到编辑器：必须在底部状态栏菜单按钮等可聚焦控件渲染之前设置，
    // 否则 egui 在渲染那些控件时已用方向键把焦点切走（之前放在 ScrollArea 内、太晚，导致上下键跳到菜单）。
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(text_id, egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true });
        });
    }
    let page = ((ui.available_height() / row_h).floor() as isize - 2).max(1);
    let lang = ed.language.clone();
    let fsize = mono.size;
    // 右键菜单/查找动作（在闭包外应用，避免借用冲突）
    let mut do_copy = false;
    let mut do_cut = false;
    let mut do_paste = false;
    let mut do_selall = false;

    // ——— 输入（聚焦时）———
    // 视角跟随只看「光标是否真的移动」：按下任意键/输入法每帧都发事件会让旧的 moved 一直为真、
    // 导致不停把视角拉回光标、无法自由滚动。这里记下输入前的光标，处理完输入后用差异判断。
    let prev_caret = ed.vcaret;
    // 自绘 IME（同 egui 路径，绕开 egui Commit 门）：处理组字/提交，并在下方上报 o.ime 激活+定位候选框。
    if focused {
        let ime_events: Vec<egui::ImeEvent> = ui.input(|i| i.events.iter().filter_map(|e| if let egui::Event::Ime(ev) = e { Some(ev.clone()) } else { None }).collect());
        for ev in ime_events {
            match ev {
                egui::ImeEvent::Enabled => {}
                egui::ImeEvent::Preedit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    // 组字是临时的：直接改 content、不入撤销栈
                    let (s, e) = ed.vime_preedit.take().or_else(|| v_sel_range(ed)).unwrap_or((ed.vcaret, ed.vcaret));
                    let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                    ed.content.replace_range(s..e, &t);
                    let end = s + t.len();
                    ed.vcaret = end;
                    ed.vsel = None;
                    ed.msel.clear();
                    ed.vime_preedit = if t.is_empty() { None } else { Some((s, end)) };
                    v_recompute(ed);
                }
                egui::ImeEvent::Commit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    if let Some((s, e)) = ed.vime_preedit.take() {
                        let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                        ed.content.replace_range(s..e, "");
                        ed.vcaret = s;
                        ed.vsel = None;
                        v_recompute(ed);
                    }
                    // 多光标模式：英文/输入法提交也要作用到全部光标（系统输入法激活后字母走 Commit 而非 Text）
                    if ed.msel.is_empty() {
                        v_insert(ed, &t);
                    } else {
                        v_multi_replace(ed, &t);
                    }
                }
                egui::ImeEvent::Disabled => {
                    if let Some((s, e)) = ed.vime_preedit.take() {
                        let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                        ed.content.replace_range(s..e, "");
                        ed.vcaret = s;
                        ed.vsel = None;
                        v_recompute(ed);
                    }
                }
            }
        }
        // 已自绘处理，移除 Ime 事件，避免主循环重复处理
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Ime(_))));
    }
    if !focused {
        ed.complete = None; // 失焦关闭补全弹窗
    }
    let mut typed = false; // 本帧是否有字符输入（补全触发 + 光标闪烁重置）
    if focused {
        let vver0 = ed.vver;
        let caret0 = ed.vcaret;
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            // 补全弹窗打开时优先消费导航键：↑↓ 选择、Enter/Tab 接受、Esc 关闭
            if ed.complete.is_some() {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = &ev {
                    let n = ed.complete.as_ref().map(|(v, _, _)| v.len()).unwrap_or(0).max(1);
                    match key {
                        egui::Key::ArrowDown if !modifiers.any() => {
                            if let Some((_, sel, _)) = &mut ed.complete {
                                *sel = (*sel + 1) % n;
                            }
                            continue;
                        }
                        egui::Key::ArrowUp if !modifiers.any() => {
                            if let Some((_, sel, _)) = &mut ed.complete {
                                *sel = (*sel + n - 1) % n;
                            }
                            continue;
                        }
                        egui::Key::Enter | egui::Key::Tab if !modifiers.any() => {
                            let sel = ed.complete.as_ref().map(|(_, s, _)| *s).unwrap_or(0);
                            v_complete_accept(ed, sel);
                            continue;
                        }
                        egui::Key::Escape => {
                            ed.complete = None;
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            // 跟随 / 大文件只读：吞掉常规修改输入（导航/复制/查找仍可用）
            if ed.is_readonly() {
                match &ev {
                    egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(_) | egui::Event::Cut => continue,
                    egui::Event::Key { key: egui::Key::Backspace | egui::Key::Delete | egui::Key::Enter | egui::Key::Tab, pressed: true, .. } => continue,
                    _ => {}
                }
            }
            if matches!(&ev, egui::Event::Text(t) if !t.is_empty()) {
                typed = true;
            }
            // 多光标模式（msel 非空）：编辑/复制作用于全部选区；移动等其它键退出多选、走常规
            if !ed.msel.is_empty() {
                let mut handled = true;
                match &ev {
                    egui::Event::Text(t) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Paste(t) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Copy => {
                        let s = v_multi_copy(ed);
                        if !s.is_empty() {
                            ui.ctx().copy_text(s);
                        }
                    }
                    egui::Event::Cut => {
                        let s = v_multi_copy(ed);
                        if !s.is_empty() {
                            ui.ctx().copy_text(s);
                            v_multi_replace(ed, "");
                        }
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        let cmd = modifiers.command || modifiers.ctrl;
                        match key {
                            egui::Key::Escape => ed.msel.clear(),
                            egui::Key::Backspace => v_multi_backspace(ed),
                            egui::Key::Delete => v_multi_delete(ed),
                            egui::Key::Enter => v_multi_replace(ed, "\n"),
                            egui::Key::Tab => {
                                let u = ed.indent.unit();
                                v_multi_replace(ed, &u);
                            }
                            egui::Key::D if cmd => v_ctrl_d(ed),
                            egui::Key::ArrowLeft if !cmd => v_multi_move(ed, false),
                            egui::Key::ArrowRight if !cmd => v_multi_move(ed, true),
                            // 纵向导航 / Ctrl+组合键 → 退出多选、走常规处理
                            egui::Key::ArrowUp | egui::Key::ArrowDown | egui::Key::Home | egui::Key::End | egui::Key::PageUp | egui::Key::PageDown if !cmd => {
                                ed.msel.clear();
                                handled = false;
                            }
                            _ if cmd => {
                                ed.msel.clear();
                                handled = false;
                            }
                            // 普通字母/符号键：会另发 Text 事件做多光标插入，这里不清 msel
                            _ => handled = false,
                        }
                    }
                    _ => handled = false,
                }
                if handled {
                    continue; // 视角跟随由「光标是否移动」统一判断
                }
            }
            match ev {
                egui::Event::Text(t) if !t.is_empty() => {
                    // 已在闭合符前再敲同一闭合符 → 跳过（如 "" 中间再敲 " 移到右侧）
                    if v_sel_range(ed).is_none() && skip_closing_pair(ed, &t) {
                        ed.vcaret = next_char_boundary(&ed.content, ed.vcaret);
                        ed.vgoal_col = None;
                    // 自动补全括号/引号：无选区→插入成对并把光标放中间；有选区→用括号包裹并保留选中
                    } else if let Some(close) = auto_close_for(&t) {
                        if let Some((a, b)) = v_sel_range(ed) {
                            let inner = ed.content[a..b].to_string();
                            v_apply(ed, a, b - a, &format!("{t}{inner}{close}"));
                            ed.vsel = Some(a + t.len());
                            ed.vcaret = a + t.len() + inner.len();
                            ed.vgoal_col = None;
                        } else {
                            v_insert(ed, &format!("{t}{close}"));
                            ed.vcaret -= close.len();
                        }
                    } else {
                        v_insert(ed, &t);
                    }
                }
                egui::Event::Paste(t) if !t.is_empty() => v_insert(ed, &t),
                egui::Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => v_insert(ed, &t),
                egui::Event::Copy => {
                    if let Some(s) = v_sel_range(ed).map(|(a, b)| ed.content[a..b].to_string()) {
                        ui.ctx().copy_text(s);
                    }
                }
                egui::Event::Cut => {
                    if let Some(s) = v_sel_range(ed).map(|(a, b)| ed.content[a..b].to_string()) {
                        ui.ctx().copy_text(s);
                        v_delete_selection(ed);
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    let cmd = modifiers.command || modifiers.ctrl;
                    match key {
                        egui::Key::S if cmd => save = true,
                        egui::Key::F if cmd => ed.open_find(),
                        egui::Key::G if cmd => {
                            ed.goto_open = !ed.goto_open;
                            if ed.goto_open {
                                ed.goto_focus = true;
                            }
                        }
                        egui::Key::A if cmd => {
                            ed.vsel = Some(0);
                            ed.vcaret = ed.content.len();
                        }
                        egui::Key::D if cmd => v_ctrl_d(ed),
                        egui::Key::Z if cmd && modifiers.shift => v_redo(ed),
                        egui::Key::Z if cmd => v_undo(ed),
                        egui::Key::Y if cmd => v_redo(ed),
                        egui::Key::Slash if cmd => {
                            if let Some(p) = line_comment(&ed.language) {
                                v_toggle_comment(ed, p);
                            }
                        }
                        egui::Key::K if cmd && modifiers.shift => v_delete_line(ed),
                        egui::Key::Backspace if cmd => v_delete_word(ed, false),
                        egui::Key::Delete if cmd => v_delete_word(ed, true),
                        egui::Key::Backspace => v_backspace(ed),
                        egui::Key::Delete => v_delete_fwd(ed),
                        egui::Key::Enter => v_newline_indent(ed),
                        egui::Key::Tab if modifiers.shift => v_block_indent(ed, false),
                        egui::Key::Tab => {
                            // 选区跨行 → 块缩进；否则插入一个缩进单位
                            if v_sel_range(ed).is_some_and(|(a, b)| ed.content[a..b].contains('\n')) {
                                v_block_indent(ed, true);
                            } else {
                                let u = ed.indent.unit();
                                v_insert(ed, &u);
                            }
                        }
                        egui::Key::ArrowUp if modifiers.alt && modifiers.shift => v_duplicate_line(ed, false),
                        egui::Key::ArrowDown if modifiers.alt && modifiers.shift => v_duplicate_line(ed, true),
                        egui::Key::ArrowUp if modifiers.alt => v_move_line(ed, true),
                        egui::Key::ArrowDown if modifiers.alt => v_move_line(ed, false),
                        egui::Key::ArrowLeft if cmd => v_move_word(ed, false, modifiers.shift),
                        egui::Key::ArrowRight if cmd => v_move_word(ed, true, modifiers.shift),
                        egui::Key::ArrowLeft => v_move_h(ed, false, modifiers.shift),
                        egui::Key::ArrowRight => v_move_h(ed, true, modifiers.shift),
                        egui::Key::ArrowUp => v_move_v(ed, -1, modifiers.shift),
                        egui::Key::ArrowDown => v_move_v(ed, 1, modifiers.shift),
                        egui::Key::Home if cmd => v_move_doc(ed, false, modifiers.shift),
                        egui::Key::End if cmd => v_move_doc(ed, true, modifiers.shift),
                        egui::Key::Home => v_move_edge(ed, false, modifiers.shift),
                        egui::Key::End => v_move_edge(ed, true, modifiers.shift),
                        egui::Key::PageUp => v_move_v(ed, -page, modifiers.shift),
                        egui::Key::PageDown => v_move_v(ed, page, modifiers.shift),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        ed.vcaret = ed.vcaret.min(ed.content.len());
        // 补全触发/维护：有字符输入 →（重新）打开；其它编辑（如退格）→ 按新前缀刷新；
        // 纯光标移动 → 关闭（避免弹窗脱离输入上下文）
        if typed || (ed.complete.is_some() && ed.vver != vver0) {
            v_complete_refresh(ed);
        } else if ed.complete.is_some() && ed.vcaret != caret0 {
            ed.complete = None;
        }
    }
    // 真正判断光标是否移动（仅此情况才让视角跟随；无移动时自由滚动、绝不拉回）
    let moved = ed.vcaret != prev_caret;
    // 光标移动或输入时重置闪烁相位，使光标立即显示
    if focused && (moved || typed) {
        ed.caret_blink_at = ui.input(|i| i.time);
    }

    // 底部状态栏（仿小文件编辑器）：缩进可切换（矩形按钮、贴左）+ 语言贴右。
    egui::Panel::bottom("editor_status_v")
        .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.scope(|ui| {
                    let v = ui.visuals_mut();
                    v.widgets.inactive.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.hovered.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.active.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.open.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0);
                    // 字号与状态栏其它项一致（11），否则默认按钮字号显得突兀地大
                    ui.menu_button(RichText::new(format!("{} {}", crate::i18n::tr("缩进", "Indent"), ed.indent.label())).size(11.0).color(Palette::TEXT_DIM), |ui| {
                        ui.set_min_width(120.0);
                        for ind in [Indent::Spaces(2), Indent::Spaces(4), Indent::Tab] {
                            if ui.selectable_label(ed.indent == ind, RichText::new(ind.label()).size(12.0)).clicked() {
                                ed.indent = ind;
                                ui.close();
                            }
                        }
                    });
                    // 自动换行开关：开启时长行折行、无横向滚动
                    ui.add_space(6.0);
                    let wrap_col = if ed.wrap { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(crate::i18n::tr("换行", "Wrap")).color(wrap_col).size(11.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr("点击切换自动换行", "Toggle word wrap"))
                        .clicked()
                    {
                        ed.wrap = !ed.wrap;
                        ed.vgoal_col = None; // 列语义改变，重置目标列
                    }
                });
                if !ed.status.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&ed.status).color(Palette::TEXT_DIM).size(11.0));
                }
                if ed.msel.len() > 1 {
                    ui.add_space(8.0);
                    let n = ed.msel.len();
                    let label = match crate::i18n::current() {
                        crate::i18n::Lang::En => format!("{n} cursors"),
                        _ => format!("{n} 光标"),
                    };
                    ui.label(RichText::new(label).color(Palette::ACCENT).size(11.0));
                }
                // 括号 lint 概述（不匹配时红字）
                if let Some(msg) = &ed.lint_msg {
                    ui.add_space(8.0);
                    ui.label(RichText::new(msg).color(Palette::DANGER).size(11.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(ed.language.as_str()).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 大文件只读徽标：点击可解除（整文件已在内存，编辑仍可能占较多 RAM）
                    if ed.readonly && !ed.follow {
                        if ui
                            .add(egui::Label::new(RichText::new(crate::i18n::tr("只读", "Read-only")).color(Palette::WARN).size(11.0)).sense(egui::Sense::click()))
                            .on_hover_text(crate::i18n::tr(
                                "大文件默认只读（整文件已载入内存）。点击改为可编辑。",
                                "Large files open read-only (fully loaded). Click to enable editing.",
                            ))
                            .clicked()
                        {
                            ed.unlock_req = true;
                        }
                        ui.add_space(10.0);
                    }
                    // 跟随（tail -f）：↧ 图标，开启时珊瑚色；点击由 app 层切换（需发起 SFTP 命令）
                    let f_col = if ed.follow { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(format!("{} {}", egui_phosphor::regular::ARROW_LINE_DOWN, crate::i18n::tr("跟随", "Follow"))).color(f_col).size(11.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr(
                            "跟随文件末尾（tail -f）：自动追加新内容并滚到底，开启期间只读。\n拖选/查看历史时暂停滚动，Ctrl+End 回到底部恢复跟随。",
                            "Follow file tail (tail -f): auto-append & scroll, read-only while on.\nScrolling pauses while selecting/browsing; Ctrl+End resumes.",
                        ))
                        .clicked()
                    {
                        ed.follow_req = true;
                    }
                    ui.add_space(10.0);
                    // 光标位置 Ln:Col（主光标，1 基；列按字符计）
                    let cl = v_line_of(ed, ed.vcaret);
                    let (lsx, _) = v_line_range(ed, cl);
                    let col = ed.content[lsx..ed.vcaret.min(ed.content.len())].chars().count() + 1;
                    ui.label(RichText::new(format!("Ln {}, Col {}", cl + 1, col)).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 行尾：点击切换 LF/CRLF
                    let eol_txt = match ed.eol() { crate::proto::Eol::Crlf => "CRLF", crate::proto::Eol::Lf => "LF" };
                    if ui.add(egui::Label::new(RichText::new(eol_txt).color(Palette::TEXT_DIM).size(11.0)).sense(egui::Sense::click())).on_hover_text(crate::i18n::tr("点击切换行尾 LF/CRLF", "Click to toggle LF/CRLF")).clicked() {
                        let n = match ed.eol() { crate::proto::Eol::Crlf => crate::proto::Eol::Lf, crate::proto::Eol::Lf => crate::proto::Eol::Crlf };
                        ed.set_eol(n);
                    }
                    ui.add_space(10.0);
                    // 编码：点击从菜单选择（保存时按所选编码写回）
                    ui.menu_button(RichText::new(ed.encoding()).color(Palette::TEXT_DIM).size(11.0), |ui| {
                        ui.set_min_width(120.0);
                        for enc in ["UTF-8", "GBK", "GB18030", "Big5", "Shift_JIS", "EUC-KR", "windows-1252", "ISO-8859-1"] {
                            if ui.selectable_label(ed.encoding() == enc, enc).clicked() {
                                ed.set_encoding(enc.to_string());
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text(crate::i18n::tr("点击选择保存编码", "Click to choose save encoding"));
                });
            });
        });

    // 查找/替换：VSCode 风格浮层（共用 find_widget），按字节定位/替换、可撤销。
    if ed.show_find {
        match find_widget(ui, ed, text_id, ed.vcaret) {
            FindOut::Goto(a, b) => {
                ed.vsel = Some(a);
                ed.vcaret = b;
                ed.pending_scroll = Some(v_line_of(ed, b));
            }
            FindOut::ReplaceOne(a, b) => {
                // 与「全部替换」保持一致：正则模式下展开捕获组（$1 等），字面模式直接用替换串。
                let rep: String = if ed.find_regex {
                    match build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex) {
                        Some(re) => re.replace(&ed.content[a..b], ed.replace.as_str()).into_owned(),
                        None => ed.replace.clone(),
                    }
                } else {
                    ed.replace.clone()
                };
                v_apply(ed, a, b - a, &rep);
                ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
            }
            FindOut::ReplaceAll(newc) => {
                let old = ed.content.len();
                v_apply(ed, 0, old, &newc);
                ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
            }
            FindOut::None => {}
        }
    }
    // 跳转到行
    if ed.goto_open {
        if let Some(n) = goto_widget(ui, ed, text_id) {
            let line = (n - 1).min(ed.vlines.len().saturating_sub(1));
            ed.vcaret = v_line_range(ed, line).0;
            ed.vsel = None;
            ed.vgoal_col = None;
            ed.pending_scroll = Some(line);
        }
    }

    // ——— 渲染（仅可见行）———
    let total = ed.vlines.len();
    let digits = total.max(1).to_string().len();
    // 行号 + 折叠箭头列（箭头在行号右侧，占约 1.5 字符宽）
    let gutter_w = (digits as f32 + 3.0) * char_w;
    // 自动换行：按视口宽度算每行可容纳列数，并同步「视觉行前缀和」缓存（列宽/内容变化才重算）
    let view_w_pre = if ed.vlast_vieww > 0.0 { ed.vlast_vieww } else { ui.available_width() };
    let wrap_cols = (((view_w_pre - gutter_w) / char_w) as i64).max(1) as usize;
    // 两种模式都维护「视觉行」映射：换行模式按折行数，非换行模式每行 1 视觉行
    //（列数取超大值），折叠行占 0 视觉行——行映射/滚动/折叠由同一套机制处理
    let eff_cols = if ed.wrap { wrap_cols } else { usize::MAX / 4 };
    v_wrap_sync(ed, eff_cols);
    let wrap = ed.wrap;
    // 「滚动行」数 = 视觉行总数（已扣除折叠隐藏的行）
    let nrows = v_total_vrows(ed);
    // 内容高度封顶在 f32 安全区：行数巨大时坐标会丢精度 → 封顶后按「行号」虚拟化。
    // 注意：字形按 clip 相对坐标绘制（不用绝对 content 坐标），故此上限只影响滚动条映射，
    // 不影响字形精度。上限越高，拖动滚动条时「每像素跨的行数」越少、越接近逐行平滑，
    // 大文件拖到底不再一下跳过整屏而卡顿。取 12M（约 66 万行处才开始压缩），且 12M×2(HiDPI)
    // =24M 仅用于滚动条位置（非字形），远在可接受范围。
    // 末尾额外留 3 行空白：可滚到最后一行之下，避免底部横向滚动条遮住最后一行。
    let pad_rows = 3usize;
    let content_w = if wrap {
        gutter_w + (wrap_cols as f32 + 1.0) * char_w // 换行模式无横向滚动
    } else {
        gutter_w + (ed.vmax as f32 + 2.0) * char_w
    };
    // —— 竖向滚动完全自绘：位置是「首个可见视觉行」ed.vtop（行号），与内容像素高度解耦。
    // 横向仍交给 egui ScrollArea（用 force_h 施加跟随光标的横向偏移）。
    // 用上一帧度量判断光标是否已在可视区，越界才滚一行；普通滚动不受影响。
    let mut force_h: Option<f32> = None;
    {
        let view_h = if ed.vlast_viewh > 0.0 { ed.vlast_viewh } else { ui.available_height() };
        let view_w = if ed.vlast_vieww > 0.0 { ed.vlast_vieww } else { ui.available_width() };
        let visible = (view_h / row_h).ceil() as usize + 2;
        let max_top = (nrows + pad_rows).saturating_sub(visible.saturating_sub(2));
        let caret_row = v_vpos_of_byte(ed, ed.vcaret, eff_cols).0;
        if let Some(tl) = ed.pending_scroll.take() {
            // 跳转/定位：居中（逻辑行 → 其首个视觉行）
            let tl_row = ed.vrow_pre.get(tl).copied().unwrap_or(0) as usize;
            ed.vtop = tl_row.saturating_sub(visible / 2).min(max_top);
        } else if moved {
            // 键盘移动：只在越界时「一行」地滚（不要整屏跳）
            let top = ed.vtop;
            let vis = ed.vlast_vis.max(3);
            let tt = if caret_row < top {
                caret_row // 光标在视口上方 → 滚到刚好露出该行（一行）
            } else if caret_row + 2 >= top + vis {
                (caret_row + 3).saturating_sub(vis) // 光标在视口下方 → 滚到该行刚好在底部附近（一行）
            } else {
                top // 已在可视区 → 不滚
            };
            ed.vtop = tt.min(max_top);
        }
        if moved && !wrap {
            let (ls2, _) = v_line_range(ed, v_line_of(ed, ed.vcaret));
            let cx = gutter_w + ed.content[ls2..ed.vcaret].chars().count() as f32 * char_w; // 光标在内容坐标里的 x
            if cx < ed.vlast_hoff + gutter_w + char_w {
                force_h = Some((cx - gutter_w - char_w * 2.0).max(0.0));
            } else if cx > ed.vlast_hoff + view_w - char_w * 2.0 {
                force_h = Some((cx - view_w + char_w * 3.0).max(0.0));
            }
        }
        // 拖选到边缘的自动滚动：dv 为行数增量；dh 仍为横向像素
        if let Some((dh, dv)) = ed.vscroll_nudge.take() {
            let nv = (ed.vtop as f32 + dv).clamp(0.0, max_top as f32);
            ed.vtop = nv as usize;
            force_h = Some((force_h.unwrap_or(ed.vlast_hoff) + dh).max(0.0));
        }
        // 竖向滚轮/触控板：pointer 在编辑区上时按行推进 ed.vtop，并「消费」掉竖向滚动量——
        // 必须在进入 ScrollArea 之前吃掉，否则 horizontal ScrollArea 会把竖向滚轮转译成横向滚动（左右抖动）。
        if ui.rect_contains_pointer(ui.available_rect_before_wrap()) {
            let sy = ui.input(|i| i.smooth_scroll_delta.y);
            if sy != 0.0 {
                ed.vscroll_accum -= sy; // 滚轮上(sy>0)→内容上移→vtop 减小
                let steps = (ed.vscroll_accum / row_h).trunc();
                if steps != 0.0 {
                    ed.vscroll_accum -= steps * row_h;
                    ed.vtop = (ed.vtop as f32 + steps).clamp(0.0, max_top as f32) as usize;
                }
                // 吃掉竖向分量（横向 .x 保留给 egui 做横向滚动）；ScrollArea 读的就是 smooth_scroll_delta
                ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
            }
        }
        ed.vtop = ed.vtop.min(max_top); // 内容变短后钳制
    }

    // horizontal ScrollArea 不做竖向裁剪 → 会继承父 ui 的 clip（含底部状态栏区域）。
    // 记录「可用区底部」（Panel::bottom 已把它抬到状态栏之上），进 closure 后据此把 clip 夹到状态栏之上。
    let content_bottom = ui.available_rect_before_wrap().bottom();
    // 用 CentralPanel（而非裸 Frame）承载正文：它会把 ScrollArea 视口（含 egui 自绘的横向滚动条）
    // 限定在「底部状态栏之上」的剩余区域内，否则 horizontal ScrollArea 会把视口铺到状态栏上、遮挡之。
    egui::CentralPanel::default().frame(egui::Frame::new().fill(bg)).show_inside(ui, |ui| {
        ui.spacing_mut().scroll.floating = false;
        ui.spacing_mut().scroll.foreground_color = false;
        ui.visuals_mut().extreme_bg_color = bg;
        ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_rgb(205, 200, 188);
        ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_rgb(172, 166, 152);
        ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_rgb(144, 138, 124);
        // 横向交给 egui；竖向自绘（下面按 ed.vtop 渲染 + 自画滚动条）。
        let mut sa = egui::ScrollArea::horizontal().auto_shrink([false, false]).id_salt(text_id);
        if let Some(h) = force_h {
            sa = sa.horizontal_scroll_offset(h);
        }
        sa.show_viewport(ui, |ui, vp| {
            ui.set_width(content_w);
            let origin = ui.min_rect().min;
            // horizontal 模式不竖向裁剪，手动把 clip 夹到底部状态栏之上（否则正文/滚动条画到状态栏上、且抢其点击）。
            let clip_full = ui.clip_rect();
            let clip = egui::Rect::from_min_max(clip_full.min, egui::pos2(clip_full.max.x, clip_full.max.y.min(content_bottom)));
            ui.set_clip_rect(clip);
            ui.set_height((clip.bottom() - origin.y).max(row_h)); // 内容高度限到视口，横向滚动条落在状态栏之上
            let view_h = clip.height();
            let visible = (view_h / row_h).ceil() as usize + 2;
            let max_top = (nrows + pad_rows).saturating_sub(visible.saturating_sub(2)); // 最大首行号
            let top_row = ed.vtop.min(max_top);
            ed.vtop = top_row;
            // 首/末可见逻辑行（由视觉行换算；用于查找命中的可视范围）
            let first_line = v_line_of_vrow(ed, top_row).0;
            let last_line = v_line_of_vrow(ed, (top_row + visible).min(nrows.saturating_sub(1))).0 + 1;
            let text_x = origin.x + gutter_w;
            // 记录本帧滚动度量，供下一帧「跟随光标」判断与施加偏移
            ed.vlast_top = top_row;
            ed.vlast_vis = visible;
            ed.vlast_hoff = vp.min.x;
            ed.vlast_vieww = clip.width();
            ed.vlast_viewh = view_h;

            // —— 自绘竖向滚动条（右缘细条）：拖动/点击按行号定位 ed.vtop ——
            // 先于正文交互注册并处理，命中滚动条时不把点击透传成「定位光标」。
            let sb_w = 12.0f32; // 轨道稍宽，便于拖动命中
            let sb_track = egui::Rect::from_min_max(egui::pos2(clip.right() - sb_w, clip.top()), clip.right_bottom());
            let total_rows = (nrows + pad_rows).max(1);
            let show_vsb = max_top > 0;
            let mut vsb_hit = false;
            // 滑块几何/配色留到正文绘制之后再画（否则被后绘的字形盖住）。
            let mut vsb_thumb: Option<(egui::Rect, egui::Color32)> = None;
            if show_vsb {
                // 滚动条交互必须先于正文 resp 注册、且正文交互区要避开这条右缘（见下 area），
                // 否则同层里后注册、覆盖更广的正文会「盖」在滚动条上，把拖动事件抢走 → 点不到。
                let sb_resp = ui.interact(sb_track, text_id.with("vsb"), egui::Sense::click_and_drag());
                let thumb_h = (sb_track.height() * (visible as f32 / total_rows as f32)).clamp(24.0, sb_track.height());
                if sb_resp.dragged() || sb_resp.clicked() {
                    if let Some(p) = sb_resp.interact_pointer_pos() {
                        // 让指针对准滑块中心：行号 = (指针 - 半个滑块) / 可移动轨道 × max_top
                        let f = ((p.y - sb_track.top() - thumb_h / 2.0) / (sb_track.height() - thumb_h).max(1.0)).clamp(0.0, 1.0);
                        ed.vtop = (f * max_top as f32).round() as usize;
                        ui.ctx().request_repaint();
                    }
                }
                vsb_hit = sb_resp.hovered() || sb_resp.dragged();
                let top_now = ed.vtop.min(max_top);
                let frac = top_now as f32 / max_top as f32;
                let thumb_y = sb_track.top() + (sb_track.height() - thumb_h) * frac;
                let thumb = egui::Rect::from_min_size(egui::pos2(sb_track.left() + 2.5, thumb_y), egui::vec2(sb_w - 5.0, thumb_h));
                let col = if vsb_hit {
                    egui::Color32::from_rgb(144, 138, 124)
                } else {
                    egui::Color32::from_rgb(179, 173, 159)
                };
                vsb_thumb = Some((thumb, col));
            }

            // 交互区取「可视视口」(clip)，但避开右缘滚动条条带（否则正文交互覆盖滚动条、抢走其拖动事件）。
            // 内层 ui 被 set_width(content_w) 限成内容宽度，若按 content_w 取交互区，短行右侧空白会落在区外、
            // 点击不到；用 clip（减去滚动条宽）覆盖视口，短行右侧空白也能点击定位到行末。
            let area = if show_vsb {
                egui::Rect::from_min_max(clip.min, egui::pos2(clip.right() - sb_w, clip.bottom()))
            } else {
                clip
            };
            let resp = ui.interact(area, text_id, egui::Sense::click_and_drag());
            // 编辑区悬停：I-beam（文本选择指针），与 VSCode / 系统文本控件一致
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
            }
            // 右键弹菜单时选区可能被折叠/失焦：在右键按下这一帧冻结当前选区，供菜单复制/剪切/粘贴使用
            if ui.input(|i| i.pointer.secondary_pressed()) {
                ed.menu_sel = v_sel_range(ed);
            }
            resp.context_menu(|ui| {
                ui.set_min_width(160.0);
                let has_sel = ed.menu_sel.is_some();
                if ui.add_enabled(has_sel, egui::Button::new(crate::i18n::tr("复制", "Copy"))).clicked() {
                    do_copy = true;
                    ui.close();
                }
                if ui.add_enabled(has_sel, egui::Button::new(crate::i18n::tr("剪切", "Cut"))).clicked() {
                    do_cut = true;
                    ui.close();
                }
                if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                    do_paste = true;
                    ui.close();
                }
                ui.separator();
                if ui.button(crate::i18n::tr("全选", "Select all")).clicked() {
                    do_selall = true;
                    ui.close();
                }
            });
            let painter = ui.painter().clone();
            // 多选时用 msel 全部选区/光标；否则用单选区 + 单光标
            let sels: Vec<(usize, usize)> = if !ed.msel.is_empty() { ed.msel.clone() } else { v_sel_range(ed).into_iter().collect() };
            let carets: Vec<usize> = if !ed.msel.is_empty() { ed.msel.iter().map(|&(_, e)| e).collect() } else { vec![ed.vcaret] };
            let caret_line = v_line_of(ed, ed.vcaret); // 当前行高亮
            let unit_cols = match ed.indent { Indent::Spaces(n) => n.max(1), Indent::Tab => 4 }; // 缩进参考线步长
            // 纯文本（未识别的扩展名）不显示缩进对齐线 / 折叠 / 粘性作用域等依赖缩进结构的代码辅助
            let show_code_aids = highlight::is_code(&ed.language);
            // 活动缩进线（VSCode 风格）：光标所在代码块对应的那条竖线高亮。
            // (列, 起始行, 结束行)：列 = 光标行缩进的上一级；范围 = 向上下延伸「更深缩进或空白」的行
            let active_guide: Option<(usize, usize, usize)> = if !show_code_aids {
                None
            } else {
                let resolve = |l: usize| -> Option<usize> {
                    v_lead(ed, l, unit_cols).or_else(|| {
                        let up = (0..l).rev().take(400).find_map(|x| v_lead(ed, x, unit_cols));
                        let down = ((l + 1)..total).take(400).find_map(|x| v_lead(ed, x, unit_cols));
                        match (up, down) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            _ => None,
                        }
                    })
                };
                resolve(caret_line).and_then(|lead| {
                    let col = (lead.saturating_sub(1) / unit_cols) * unit_cols;
                    if col == 0 {
                        return None; // 顶层代码没有外层块
                    }
                    let deeper = |l: usize| v_lead(ed, l, unit_cols).map(|d| d > col).unwrap_or(true);
                    let mut lo = caret_line;
                    while lo > 0 && lo > caret_line.saturating_sub(2000) && deeper(lo - 1) {
                        lo -= 1;
                    }
                    let mut hi = caret_line;
                    while hi + 1 < total && hi < caret_line + 2000 && deeper(hi + 1) {
                        hi += 1;
                    }
                    Some((col, lo, hi))
                })
            };
            let brackets = if focused { bracket_match(&ed.content, ed.vcaret) } else { None }; // 括号匹配高亮
            // 可视区内的查找匹配（克隆出来，避免后续可变借用 ed 冲突）
            let vis_matches: Vec<(usize, usize)> = if ed.show_find && !ed.find.is_empty() {
                let vis_a = ed.vlines.get(first_line).copied().unwrap_or(0);
                let vis_b = ed.vlines.get(last_line.min(total)).copied().unwrap_or(ed.content.len());
                let mlo = ed.find_matches.partition_point(|&(s, _)| s < vis_a);
                let mhi = ed.find_matches.partition_point(|&(s, _)| s < vis_b);
                ed.find_matches[mlo..mhi].to_vec()
            } else {
                Vec::new()
            };
            // 双击选词后的「相同词」淡高亮（仅常见代码类型）：当前选区恰为一个完整词时，
            // 可见行内该词的其它出现处铺一层比查找更淡的底色（VSCode occurrence 风格）
            let occ_word: Option<String> = if highlight::lint_enabled(&ed.language) && ed.msel.is_empty() {
                v_sel_range(ed).and_then(|(a, b)| {
                    let w = &ed.content[a..b];
                    let is_w = |c: char| c.is_ascii_alphanumeric() || c == '_';
                    let ok = (2..=64).contains(&(b - a))
                        && w.chars().all(is_w)
                        && (a == 0 || !is_w(ed.content[..a].chars().next_back().unwrap()))
                        && (b >= ed.content.len() || !is_w(ed.content[b..].chars().next().unwrap()));
                    ok.then(|| w.to_string())
                })
            } else {
                None
            };
            // 水平可视列窗口：每行只对窗口内片段做高亮 + 布局（开销 O(可视列)，与行长无关）。
            // 这样超长行（日志/JSON/CSV 等）不再每帧整行 tokenize + layout，根治「某些大文件拖到底卡顿」。
            let first_col = ((clip.left() - text_x).max(0.0) / char_w) as usize;
            let cols_vis = (clip.width() / char_w).ceil() as usize + 8; // 视口列数 + 余量（CJK 偏宽，余量足够）
            let accent = Palette::ACCENT;
            // 折叠箭头：悬停行号列时显示可折叠箭头；点击在循环后统一应用（避免借用冲突）
            let gutter_hover = ui.input(|inp| inp.pointer.hover_pos()).is_some_and(|p| clip.contains(p) && p.x < clip.left() + gutter_w);
            let mut fold_click: Option<usize> = None;
            let mut caret_px_frame: Option<egui::Pos2> = None; // 主光标屏幕坐标（补全弹窗定位）
            for k in 0..visible {
                let row = top_row + k;
                if row >= nrows {
                    break;
                }
                // 视觉行 → 逻辑行 i / 起始列 col0 / 本行列数 ncols / 绘制起点 gx / 是否首段
                //（两种模式都经 v_line_of_vrow：折叠行占 0 视觉行，映射自动跳过）
                let (li, seg) = v_line_of_vrow(ed, row);
                let (i, col0, ncols, gx, is_first) = if wrap {
                    (li, seg * wrap_cols, wrap_cols, text_x, seg == 0)
                } else {
                    (li, first_col, cols_vis, text_x + first_col as f32 * char_w, true)
                };
                if i >= total {
                    break;
                }
                let (ls, le) = v_line_range(ed, i);
                let line_full: &str = &ed.content[ls..le]; // 切片，不整行拷贝
                let y = clip.top() + k as f32 * row_h;
                let col_of = |b: usize| -> usize { byte_to_char(line_full, b.saturating_sub(ls).min(line_full.len())) };
                let in_win = |c: usize| c >= col0 && c <= col0 + ncols;
                // 当前行高亮（极淡）：聚焦且无选区时，给光标所在行铺一层很淡的底
                if focused && sels.is_empty() && i == caret_line {
                    painter.rect_filled(egui::Rect::from_min_max(egui::pos2(clip.left(), y), egui::pos2(clip.right(), y + row_h)), 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10));
                }
                // 缩进参考线（仅首段画、仅代码文件）：在各缩进层级之间画淡竖线；
                // 空白行取上下最近非空行缩进的较小值，使缩进线跨空行连续（同 VSCode）
                if is_first && show_code_aids {
                    let lead_of = |l: usize| -> Option<usize> {
                        let (a, b) = v_line_range(ed, l);
                        let mut lead = 0usize;
                        for c in ed.content[a..b].chars() {
                            match c {
                                ' ' => lead += 1,
                                '\t' => lead += unit_cols,
                                _ => return Some(lead), // 有内容的行
                            }
                        }
                        None // 空白行
                    };
                    let lead = match lead_of(i) {
                        Some(l) => l,
                        None => {
                            let up = (0..i).rev().take(400).find_map(lead_of);
                            let down = ((i + 1)..total).take(400).find_map(lead_of);
                            match (up, down) {
                                (Some(a), Some(b)) => a.min(b),
                                _ => 0,
                            }
                        }
                    };
                    let mut col = unit_cols;
                    while col < lead {
                        let gx = text_x + col as f32 * char_w;
                        // 光标所在块的那条竖线用强调色高亮（活动缩进线）
                        let active = active_guide.is_some_and(|(ac, lo, hi)| col == ac && i >= lo && i <= hi);
                        let stroke = if active {
                            egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 150))
                        } else {
                            egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 30))
                        };
                        painter.vline(gx, y..=(y + row_h), stroke);
                        col += unit_cols;
                    }
                    // 本行画线上限（lead）不及活动列时（块内/块尾空行），单独补画活动线保证贯穿
                    if let Some((ac, lo, hi)) = active_guide {
                        if i >= lo && i <= hi && ac >= lead && ac > 0 {
                            painter.vline(
                                text_x + ac as f32 * char_w,
                                y..=(y + row_h),
                                egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 150)),
                            );
                        }
                    }
                }
                // 仅取窗口片段（char_to_byte 至多遍历到 last_col 个字符）
                let seg_a = char_to_byte(line_full, col0);
                let seg_b = char_to_byte(line_full, col0 + ncols);
                let seg = &line_full[seg_a..seg_b];
                let seg_x = gx;
                let seg_right = gx + ncols as f32 * char_w;
                // 括号 lint 下划线：把全文偏移的错误范围裁剪/平移成「本段内相对范围」传给高亮器
                let (seg_start, seg_end_abs) = (ls + seg_a, ls + seg_b);
                let lint_errs: Vec<std::ops::Range<usize>> = if ed.lint_ranges.is_empty() {
                    Vec::new()
                } else {
                    ed.lint_ranges
                        .iter()
                        .filter_map(|r| {
                            let a = r.start.max(seg_start);
                            let b = r.end.min(seg_end_abs);
                            (a < b).then(|| (a - seg_start)..(b - seg_start))
                        })
                        .collect()
                };
                let galley = {
                    // 整行分词（行首带跨行状态，docstring/块注释正确延续）、仅窗口布局
                    let state = ed.hl_states.get(i).copied().unwrap_or(highlight::LineState::Normal);
                    let mut job = highlight::highlight_segment(line_full, seg_a..seg_b, &lang, fsize, &lint_errs, state);
                    job.wrap.max_width = f32::INFINITY;
                    ui.ctx().fonts_mut(|f| f.layout_job(job))
                };
                // 行内字节偏移 → 屏幕 x（窗口外钳制到窗口边缘，超出部分本就不可见）
                let x_of = |lb: usize| -> f32 { seg_x + galley.pos_from_cursor(CCursor::new(byte_to_char(seg, lb.clamp(seg_a, seg_b) - seg_a))).left() };
                // 相同词淡高亮（先画，衬在选区/查找高亮之下）：淡暖灰，比查找命中更轻
                if let Some(wd) = &occ_word {
                    if line_full.len() <= 10_000 {
                        let isw = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
                        let bytes = line_full.as_bytes();
                        let mut from = 0usize;
                        while let Some(p) = line_full[from..].find(wd.as_str()) {
                            let s0 = from + p;
                            let e0 = s0 + wd.len();
                            from = e0;
                            // 全词匹配 + 跳过选区本体
                            if (s0 > 0 && isw(bytes[s0 - 1])) || (e0 < bytes.len() && isw(bytes[e0])) {
                                continue;
                            }
                            if sels.iter().any(|&(sa, sb)| sa == ls + s0 && sb == ls + e0) {
                                continue;
                            }
                            let ax = x_of(s0);
                            let bx = x_of(e0);
                            if bx > ax {
                                painter.rect_filled(
                                    egui::Rect::from_min_max(egui::pos2(ax, y), egui::pos2(bx, y + row_h)),
                                    2.0,
                                    egui::Color32::from_rgba_unmultiplied(0x6b, 0x63, 0x50, 34),
                                );
                            }
                        }
                    }
                }
                // 选区/查找当前项高亮：半透明珊瑚色（多选时画全部）
                for &(sa, sb) in &sels {
                    if sb > sa && sb > ls && sa <= le {
                        let ax = x_of(sa.clamp(ls, le) - ls);
                        // 选区越过本段末尾(含跨到下一视觉行/下一逻辑行) → 填到本段右缘
                        let bx = if sb >= ls + seg_b { seg_right } else { x_of(sb.clamp(ls, le) - ls) };
                        if bx > ax {
                            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(ax, y), egui::pos2(bx, y + row_h)), 0.0, egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 60));
                        }
                    }
                }
                // 正文
                painter.galley(egui::pos2(seg_x, y), galley.clone(), Palette::TEXT);
                // 括号匹配：给光标相邻括号及其匹配括号描边
                if let Some((ba, bb)) = brackets {
                    for &bp in &[ba, bb] {
                        if bp >= ls && bp < le && in_win(col_of(bp)) {
                            let bx0 = x_of(bp - ls);
                            let bx1 = x_of(bp + 1 - ls);
                            painter.rect_stroke(egui::Rect::from_min_max(egui::pos2(bx0, y), egui::pos2(bx1, y + row_h)), 2.0, egui::Stroke::new(1.0, Palette::ACCENT), egui::StrokeKind::Inside);
                        }
                    }
                }
                // 查找命中高亮（半透明灰），跳过「当前项」(=选区)避免叠灰盖字。
                for &(ma, mb) in &vis_matches {
                    if sels.contains(&(ma, mb)) {
                        continue;
                    }
                    if ma < ls + seg_b && mb > ls + seg_a {
                        let hx0 = x_of(ma.clamp(ls, le) - ls);
                        let hx1 = x_of(mb.clamp(ls, le) - ls);
                        if hx1 > hx0 {
                            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(hx0, y), egui::pos2(hx1, y + row_h)), 2.0, egui::Color32::from_rgba_unmultiplied(120, 120, 120, 56));
                        }
                    }
                }
                // 光标（多选时每个选区末尾各画一个；闪烁约 530ms 亮/灭）
                if focused {
                    let now = ui.input(|i| i.time);
                    let blink_on = ((now - ed.caret_blink_at).rem_euclid(1.06)) < 0.53;
                    if blink_on {
                        for &cp in &carets {
                            if cp >= ls && cp <= le && in_win(col_of(cp)) {
                                let cx = x_of(cp - ls);
                                painter.vline(cx, y..=(y + row_h), egui::Stroke::new(1.5, Palette::ACCENT));
                            }
                        }
                    }
                    // 在主光标处上报 IME 输入区：激活输入法 + 定位候选框（否则虚拟编辑器无法输入中文）
                    if ed.vcaret >= ls && ed.vcaret <= le && in_win(col_of(ed.vcaret)) {
                        let cx = x_of(ed.vcaret - ls);
                        let irect = egui::Rect::from_min_size(egui::pos2(cx, y), egui::vec2(1.0, row_h));
                        ui.ctx().output_mut(|o| o.ime = Some(egui::output::IMEOutput { rect: irect, cursor_rect: irect }));
                        caret_px_frame = Some(egui::pos2(cx, y + row_h));
                    }
                }
                // 折叠 header：行尾画「⋯ N」胶囊提示（点击展开）
                let folded_end = ed.folds.iter().find(|&&(h, _)| h == i).map(|&(_, e)| e);
                if let Some(fe) = folded_end {
                    let bx = seg_x + galley.size().x + 10.0;
                    let label = format!("⋯ {}", fe - i);
                    let tr = painter.text(
                        egui::pos2(bx, y + row_h / 2.0),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::monospace((fsize * 0.85).max(9.0)),
                        Palette::TEXT_DIM,
                    );
                    let cap = tr.expand2(egui::vec2(5.0, 1.5));
                    painter.rect_stroke(cap, 4.0, egui::Stroke::new(1.0, Palette::BORDER), egui::StrokeKind::Outside);
                    if ui.interact(cap, text_id.with(("unfold", i)), egui::Sense::click()).clicked() {
                        fold_click = Some(i);
                    }
                }
                // 行号列固定在左侧：最后画（铺底盖住横向滚到下面的正文）+ 右对齐行号
                painter.rect_filled(egui::Rect::from_min_max(egui::pos2(clip.left(), y), egui::pos2(clip.left() + gutter_w, y + row_h)), 0.0, bg);
                if is_first {
                    // 括号不匹配的行：行号标红（lint）
                    let num_col = if ed.lint_lines.contains(&i) { Palette::DANGER } else { Palette::TEXT_DIM };
                    painter.text(egui::pos2(clip.left() + gutter_w - char_w * 2.0, y), egui::Align2::RIGHT_TOP, (i + 1).to_string(), mono.clone(), num_col);
                    // 折叠箭头（仅代码文件）：已折叠恒显 ▸（强调色）；可折叠仅悬停行号列时显 ▾（弱色）
                    let folded = folded_end.is_some();
                    if show_code_aids && (folded || (gutter_hover && v_foldable(ed, i, unit_cols))) {
                        let arect = egui::Rect::from_min_size(
                            egui::pos2(clip.left() + gutter_w - char_w * 1.8, y),
                            egui::vec2(char_w * 1.5, row_h),
                        );
                        if ui.interact(arect, text_id.with(("fold", i)), egui::Sense::click()).clicked() {
                            fold_click = Some(i);
                        }
                        let (glyph, colr) = if folded {
                            (egui_phosphor::regular::CARET_RIGHT, Palette::ACCENT)
                        } else {
                            (egui_phosphor::regular::CARET_DOWN, Palette::TEXT_DIM)
                        };
                        painter.text(
                            egui::pos2(arect.center().x, y + row_h / 2.0),
                            egui::Align2::CENTER_CENTER,
                            glyph,
                            egui::FontId::proportional((fsize * 0.8).max(9.0)),
                            colr,
                        );
                    }
                }
            }
            // 应用折叠切换（下一帧重算视觉行映射）
            if let Some(l) = fold_click {
                v_toggle_fold(ed, l, unit_cols);
                ui.ctx().request_repaint();
            }
            // 聚焦时驱动光标闪烁（约 30fps 即可，不必每帧满速）
            if focused {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            }
            // 补全弹窗（VSCode 风格）：对齐前缀起点、紧贴编辑行正下方；下方空间不足时
            // 翻到行上方——永不遮挡正在编辑的那一行。扁平卡片：细边框、无浮动大阴影。
            ed.caret_px = caret_px_frame;
            let mut accept_click: Option<usize> = None;
            if let (Some((items, sel, plen)), Some(cpos)) = (&ed.complete, ed.caret_px) {
                let item_h = 20.0f32;
                let pop_h = items.len() as f32 * item_h + 10.0;
                // x 对齐前缀起点（前缀为 ASCII，字节数=字符数），并夹在视口内
                let mut pos = egui::pos2(cpos.x - *plen as f32 * char_w - 6.0, cpos.y + 1.0);
                pos.x = pos.x.clamp(clip.left() + gutter_w, (clip.right() - 268.0).max(clip.left()));
                if pos.y + pop_h > clip.bottom() {
                    pos.y = cpos.y - row_h - pop_h - 1.0; // 放到编辑行上方
                }
                egui::Area::new(text_id.with("complete"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .constrain(false)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::new()
                            .fill(Palette::PANEL)
                            .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(4, 4))
                            .show(ui, |ui| {
                                ui.set_min_width(260.0);
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);
                                for (idx, w) in items.iter().enumerate() {
                                    if ui.selectable_label(idx == *sel, egui::RichText::new(w).monospace().size(12.5)).clicked() {
                                        accept_click = Some(idx);
                                    }
                                }
                            });
                    });
            }
            if let Some(idx) = accept_click {
                v_complete_accept(ed, idx);
            }
            // 行号分割线（固定在左侧行号列右缘）
            painter.vline(clip.left() + gutter_w - 3.0, clip.top()..=clip.bottom(), egui::Stroke::new(1.0, Palette::BORDER));
            // 自绘竖向滚动条滑块：正文之后再画，确保浮在字形之上、不被盖住
            if let Some((thumb, col)) = vsb_thumb {
                painter.rect_filled(thumb, 3.0, col);
            }

            // ——— 粘性作用域行（sticky scroll）———
            // 顶部固定显示首个可见行的外层作用域链（按缩进推导，至多 3 行），点击跳转
            let sticky: Vec<usize> = if show_code_aids && top_row > 0 && first_line > 0 {
                let mut chain: Vec<usize> = Vec::new();
                let mut min_lead = v_lead(ed, first_line, unit_cols).unwrap_or(usize::MAX);
                let lo = first_line.saturating_sub(3000);
                let mut l = first_line;
                while l > lo && min_lead > 0 && chain.len() < 3 {
                    l -= 1;
                    if let Some(d) = v_lead(ed, l, unit_cols) {
                        if d < min_lead {
                            chain.push(l);
                            min_lead = d;
                        }
                    }
                }
                chain.reverse();
                chain
            } else {
                Vec::new()
            };
            for (si, &l) in sticky.iter().enumerate() {
                let y = clip.top() + si as f32 * row_h;
                let row_rect = egui::Rect::from_min_max(egui::pos2(clip.left(), y), egui::pos2(clip.right(), y + row_h));
                if ui.interact(row_rect, text_id.with(("sticky", si)), egui::Sense::click()).clicked() {
                    ed.pending_scroll = Some(l);
                    ui.ctx().request_repaint();
                }
                painter.rect_filled(row_rect, 0.0, Palette::PANEL_2);
                let (ls2, le2) = v_line_range(ed, l);
                let line_full = &ed.content[ls2..le2];
                let seg_b2 = char_to_byte(line_full, cols_vis);
                let state = ed.hl_states.get(l).copied().unwrap_or(highlight::LineState::Normal);
                let mut job = highlight::highlight_segment(line_full, 0..seg_b2, &lang, fsize, &[], state);
                job.wrap.max_width = f32::INFINITY;
                let g = ui.ctx().fonts_mut(|f| f.layout_job(job));
                painter.galley(egui::pos2(clip.left() + gutter_w, y), g, Palette::TEXT);
                painter.text(egui::pos2(clip.left() + gutter_w - char_w * 2.0, y), egui::Align2::RIGHT_TOP, (l + 1).to_string(), mono.clone(), Palette::TEXT_DIM);
            }
            if !sticky.is_empty() {
                let by = clip.top() + sticky.len() as f32 * row_h;
                painter.hline(clip.left()..=clip.right(), by + 0.5, egui::Stroke::new(1.0, Palette::BORDER));
            }

            // 点击 / 双击 / 三击 / 拖拽定位光标与选区（行号 = top_line + 视口内行偏移）
            if !vsb_hit && (resp.clicked() || resp.drag_started() || resp.dragged() || resp.double_clicked() || resp.triple_clicked()) {
                if resp.clicked() || resp.drag_started() || resp.double_clicked() || resp.triple_clicked() {
                    ui.memory_mut(|m| m.request_focus(text_id));
                }
                if let Some(pos) = resp.interact_pointer_pos() {
                    ed.complete = None; // 任何正文点击/拖拽都关闭补全弹窗
                    // 坐标 → 内容字节位（行号 = top_line + 视口内行偏移；
                    // 只布局窗口片段，避免在超长行上拖拽选择时每帧整行 layout）
                    let ctx = ui.ctx().clone();
                    let byte_at = |p: egui::Pos2| -> usize {
                        let k = ((p.y - clip.top()) / row_h).floor().max(0.0) as usize;
                        let row = (top_row + k).min(nrows.saturating_sub(1));
                        let (l, seg2) = v_line_of_vrow(ed, row);
                        let (li, c0, nc, gx) = if wrap {
                            (l, seg2 * wrap_cols, wrap_cols, text_x)
                        } else {
                            (l, first_col, cols_vis, text_x + first_col as f32 * char_w)
                        };
                        let (ls, le) = v_line_range(ed, li);
                        let line_full: &str = &ed.content[ls..le];
                        let seg_a = char_to_byte(line_full, c0);
                        let seg_b = char_to_byte(line_full, c0 + nc);
                        let seg = line_full[seg_a..seg_b].to_string();
                        let g = ctx.fonts_mut(|f| f.layout_no_wrap(seg.clone(), mono.clone(), Palette::TEXT));
                        let cc = g.cursor_from_pos(egui::vec2(p.x - gx, 0.0)).index;
                        ls + seg_a + char_to_byte(&seg, cc)
                    };
                    let b = byte_at(pos);
                    // 拖拽需移动超过阈值才激活，此刻指针已离开按下点——锚点必须用「按下位置」，
                    // 否则起始字符会被漏选（从左往右拖丢第一个字，从右往左拖丢按下处字符）
                    let ob = if resp.drag_started() {
                        ui.input(|i| i.pointer.press_origin()).map(byte_at)
                    } else {
                        None
                    };
                    let alt_click = resp.clicked() && ui.input(|inp| inp.modifiers.alt);
                    if !alt_click {
                        ed.msel.clear(); // 普通点击退出多选
                    }
                    if alt_click {
                        // Alt+单击：在点击处添加一个光标（并入多选集合）
                        if ed.msel.is_empty() {
                            ed.msel.push((ed.vcaret, ed.vcaret));
                        }
                        if !ed.msel.iter().any(|&(_, e)| e == b) {
                            ed.msel.push((b, b));
                            ed.msel.sort_by_key(|&(s, _)| s);
                        }
                        ed.vcaret = b;
                        ed.vsel = None;
                    } else if resp.triple_clicked() {
                        // 三击选中当前逻辑行（含行尾换行符，与主流编辑器一致）
                        let li = ed.vlines.partition_point(|&p| p <= b).saturating_sub(1);
                        let (ls, le) = v_line_range(ed, li);
                        ed.vsel = Some(ls);
                        ed.vcaret = (le + 1).min(ed.content.len());
                    } else if resp.double_clicked() {
                        // 双击选中光标处的词
                        if let Some((wa, wb)) = v_word_range(&ed.content, b) {
                            ed.vsel = Some(wa);
                            ed.vcaret = wb;
                        } else {
                            ed.vsel = None;
                            ed.vcaret = b;
                        }
                    } else if resp.drag_started() {
                        ed.vsel = Some(ob.unwrap_or(b));
                        ed.vcaret = b;
                    } else if resp.dragged() {
                        if ed.vsel.is_none() {
                            ed.vsel = Some(ed.vcaret);
                        }
                        ed.vcaret = b;
                    } else {
                        ed.vsel = None;
                        ed.vcaret = b;
                    }
                    ed.vgoal_col = None;
                }
            }
            // 键盘移动的「跟随光标」已在 ScrollArea 创建前用 vertical/horizontal_scroll_offset 施加（可靠）。
            // 这里只处理拖选到边缘：记录滚动增量，下一帧施加（持续自动滚动）。
            if resp.dragged() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    // dv 为「行数」增量（自绘竖向滚动按行号推进）
                    let dv = if pos.y < clip.top() + row_h {
                        -2.0
                    } else if pos.y > clip.bottom() - row_h {
                        2.0
                    } else {
                        0.0
                    };
                    let dh = if pos.x < clip.left() + gutter_w + char_w {
                        -char_w * 3.0
                    } else if pos.x > clip.right() - char_w {
                        char_w * 3.0
                    } else {
                        0.0
                    };
                    if dv != 0.0 || dh != 0.0 {
                        ed.vscroll_nudge = Some((dh, dv));
                        ui.ctx().request_repaint();
                    }
                }
            }
        });
    });
    // 右键菜单动作（闭包外应用）
    if do_selall {
        ed.vsel = Some(0);
        ed.vcaret = ed.content.len();
    }
    // 复制/剪切用「冻结的右键选区」(menu_sel)，避免右键折叠选区后复制不到
    if do_copy || do_cut {
        if let Some((a, b)) = ed.menu_sel {
            let (a, b) = (a.min(ed.content.len()), b.min(ed.content.len()));
            if b > a {
                ui.ctx().copy_text(ed.content[a..b].to_string());
                if do_cut {
                    v_apply(ed, a, b - a, "");
                    ed.vgoal_col = None;
                }
            }
        }
    }
    if do_paste {
        if let Some(t) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
            if !t.is_empty() {
                // 有冻结选区则替换它，否则插入到光标
                if let Some((a, b)) = ed.menu_sel.filter(|&(a, b)| b > a) {
                    let (a, b) = (a.min(ed.content.len()), b.min(ed.content.len()));
                    v_apply(ed, a, b - a, &t);
                } else {
                    v_insert(ed, &t);
                }
                ed.vgoal_col = None;
            }
        }
    }
    if do_copy || do_cut || do_paste {
        ed.menu_sel = None;
    }
    save
}

/// 字符下标 → 字节偏移（用于右键复制/剪切/粘贴按选区操作 UTF-8 内容）。
fn char_to_byte(s: &str, c: usize) -> usize {
    s.char_indices().nth(c).map(|(b, _)| b).unwrap_or(s.len())
}

/// 字节偏移 → 字符下标。
fn byte_to_char(s: &str, b: usize) -> usize {
    s[..b.min(s.len())].chars().count()
}
