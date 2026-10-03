//! 编辑核心：插入/删除/撤销、光标移动与多选、词补全。

use super::super::{EditOp, Editor};
use super::fold::v_remap_folds;
use super::geom::{
    char_to_byte, next_char_boundary, prev_char_boundary, v_line_of, v_line_range, v_sel_range,
};
use super::wrap::{v_byte_of_vpos, v_recompute, v_total_vrows, v_vpos_of_byte};
use crate::ui::highlight::{self, Indent};

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
        for w in ed
            .content
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        {
            if (3..=48).contains(&w.len())
                && (w.as_bytes()[0].is_ascii_alphabetic() || w.starts_with('_'))
            {
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
    (prefix.len() >= 2 && (bytes[start].is_ascii_alphabetic() || prefix.starts_with('_')))
        .then(|| (prefix.len(), prefix.to_string()))
}
/// 按光标前缀打开/刷新补全弹窗；无候选则关闭。
/// 候选 = 缓冲区单词（优先）+ 该语言关键字/常见内置名（补足），去重、至多 8 条。
pub(super) fn v_complete_refresh(ed: &mut Editor) {
    let Some((plen, prefix)) = v_word_prefix(ed) else {
        ed.complete = None;
        return;
    };
    v_build_words(ed);
    let mut items: Vec<String> = ed
        .words
        .iter()
        .filter(|w| w.starts_with(&prefix) && w.as_str() != prefix)
        .take(8)
        .cloned()
        .collect();
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
    ed.complete = if items.is_empty() {
        None
    } else {
        Some((items, 0, plen))
    };
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
    // 这里是编辑器**唯一**改 content 的地方，所以也是最后一道防线：把区间收敛到合法的
    // 字符边界上，让 `v_apply` 成为一个总函数（同 `ime_safe::replace_preedit` 的思路）。
    //
    // 上游各处算出来的 (at, len) 只要有一端落在多字节字符中间就是 panic，而编辑器里存着
    // 用户还没保存的远端文件——为一个算错的偏移崩掉整个应用不值得。这里刻意**不加**
    // debug_assert：越界/非边界正是它要兜住的输入，加了断言等于让 dev 构建在我们明确
    // 决定要容忍的场景上崩掉。真正的不变量守在源头（见 v_undo 里的 floor_boundary）。
    //
    // 也因为是唯一入口，只读（大文件只读 / 跟随）的门设在这里：键盘、右键菜单、查找替换、
    // 各种行命令最终都走到这儿，在上游按事件类型一个个拦是拦不全的。
    if ed.is_readonly() {
        return;
    }
    let (at, end) = crate::ui::ime_safe::clamp_range(&ed.content, (at, at + removed_len));
    let removed_len = end - at;
    // 什么都没改的不算编辑：不置 dirty、不占撤销记录、不清重做栈
    if removed_len == 0 && inserted.is_empty() {
        return;
    }
    // 多光标区间是字节偏移，内容一变就陈旧了。多光标自己的编辑（v_multi_replace）会在
    // 这之后重新设好；其它任何编辑（右键菜单、查找替换、补全……）都意味着退出多选。
    ed.msel.clear();
    v_remap_folds(ed, at, removed_len, inserted);
    let caret_before = ed.vcaret;
    let removed = ed.content[at..end].to_string();
    ed.content.replace_range(at..end, inserted);
    ed.vcaret = at + inserted.len();
    ed.vsel = None;
    // 连续单段输入（非换行）合并到上一条，避免每个字符一条撤销记录
    let mergeable = removed.is_empty() && !inserted.is_empty() && !inserted.contains('\n');
    if mergeable {
        if let Some(last) = ed.vundo.last_mut() {
            if last.removed.is_empty()
                && !last.inserted.ends_with('\n')
                && last.at + last.inserted.len() == at
            {
                last.inserted.push_str(inserted);
                last.caret_after = ed.vcaret;
                ed.vredo.clear();
                mark_edited(ed);
                v_recompute(ed);
                return;
            }
        }
    }
    ed.vundo.push(EditOp {
        at,
        removed,
        inserted: inserted.to_string(),
        caret_before,
        caret_after: ed.vcaret,
    });
    if ed.vundo.len() > 5000 {
        ed.vundo.remove(0);
    }
    ed.vredo.clear();
    mark_edited(ed);
    v_recompute(ed);
}

/// 一次编辑之后更新「有改动」标记。
///
/// 绝大多数编辑都离开了保存点，直接置位即可（O(1)，不比较全文）。但编辑也可能恰好**回到**
/// 保存点——敲一个字再退格删掉——那时不该还显示「已修改」、关标签时还提示保存一个其实
/// 没动过的文件。长度不同就一定不同；只有长度碰巧相等时才值得比一次内容。
fn mark_edited(ed: &mut Editor) {
    if ed.content.len() == ed.orig.len() {
        ed.recompute_dirty();
    } else {
        ed.dirty_flag = true;
    }
}
/// 粘贴文本归一成 LF（内部统一用 LF，保存时按文件行尾还原）。
pub(super) fn normalize_paste(t: &str) -> String {
    if t.contains('\r') {
        t.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        t.to_string()
    }
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
    let (at, rl) = if let Some((a, b)) = v_sel_range(ed) {
        (a, b - a)
    } else {
        (ed.vcaret, 0)
    };
    v_apply(ed, at, rl, t);
    ed.vgoal_col = None;
}
/// 回车自动缩进：沿用当前行前导空白；行尾是 : { ( [ 时再加一级。
/// 内容里还有 `\r\n` 吗（混合行尾：全文都是 CRLF 的文件打开时已统一成 LF，留下的 `\r\n`
/// 只来自混合行尾的文件）。按内容版本缓存。
pub(super) fn v_mixed_eol(ed: &mut Editor) -> bool {
    if ed.eol_mixed.0 != ed.vver {
        ed.eol_mixed = (ed.vver, ed.content.contains("\r\n"));
    }
    ed.eol_mixed.1
}
/// 把混合行尾统一成一种：内容里的 `\r\n` 全部归一成 `\n`（一次可撤销的编辑），保存时
/// 按 `eol` 写出。光标留在原来那个字符上。只读时不动。
pub(super) fn v_unify_eol(ed: &mut Editor, eol: crate::proto::Eol) {
    if ed.is_readonly() {
        return;
    }
    if ed.content.contains("\r\n") {
        let caret = crate::ui::ime_safe::floor_boundary(&ed.content, ed.vcaret);
        let shift = ed.content[..caret].matches("\r\n").count();
        let norm = ed.content.replace("\r\n", "\n");
        let len = ed.content.len();
        v_apply(ed, 0, len, &norm);
        ed.vcaret = caret - shift;
        ed.vsel = None;
        ed.vgoal_col = None;
    }
    ed.set_eol(eol);
}
pub(super) fn v_newline_indent(ed: &mut Editor) {
    let at = v_sel_range(ed).map(|(a, _)| a).unwrap_or(ed.vcaret);
    let li = v_line_of(ed, at);
    let (ls, le) = v_line_range(ed, li);
    let before = &ed.content[ls..at.max(ls)];
    let lead: String = before
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    // 混合行尾的文件里，在一个 CRLF 行里回车也断出 CRLF：两半都保持这一行原来的行尾
    let crlf = super::geom::v_line_next(ed, li) == le + 2;
    let mut t = String::from(if crlf { "\r\n" } else { "\n" });
    t.push_str(&lead);
    if matches!(
        before.trim_end().chars().last(),
        Some(':' | '{' | '(' | '[')
    ) {
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
    let lb = v_line_of(
        ed,
        if b > a && ed.vlines.get(v_line_of(ed, b)).copied() == Some(b) {
            b - 1
        } else {
            b
        },
    );
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
        let had_sel = b > a;
        let old_caret = ed.vcaret;
        let grew = out.len() as isize - (end - start) as isize;
        v_apply(ed, start, end - start, &out);
        if had_sel {
            // 选中整块，支持连续 Tab/Shift+Tab
            ed.vsel = Some(start);
            ed.vcaret = start + out.len();
        } else {
            // 无选区：只是调整当前行的缩进，光标跟着文字走，不留选区
            //（留下整行选区的话，接着敲的字会把整行替换掉）
            let moved = (old_caret as isize + grew).max(start as isize) as usize;
            ed.vcaret = crate::ui::ime_safe::floor_boundary(&ed.content, moved);
        }
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
/// 撤销/重做前的共同检查：只读不许改；组字中的临时文本不在撤销栈的账上，先撤掉。
fn history_ready(ed: &mut Editor) -> bool {
    if ed.is_readonly() {
        return false;
    }
    super::input::v_cancel_preedit(ed);
    true
}

/// 撤销栈记的是字节偏移，只在「内容就是这条操作留下的样子」时才有效。对不上说明有修改
/// 绕过了栈——继续按旧偏移改写，轻则改错位置、重则越界 panic 带走全部未保存内容。
/// 宁可作废历史（两个栈此时都不可信）。
fn history_matches(ed: &mut Editor, at: usize, expect: &str) -> bool {
    let ok = ed.content.get(at..at + expect.len()) == Some(expect);
    if !ok {
        ed.vundo.clear();
        ed.vredo.clear();
    }
    ok
}

pub(super) fn v_undo(ed: &mut Editor) {
    if !history_ready(ed) {
        return;
    }
    if let Some(op) = ed.vundo.pop() {
        if !history_matches(ed, op.at, &op.inserted) {
            return;
        }
        ed.msel.clear();
        let end = op.at + op.inserted.len();
        v_remap_folds(ed, op.at, op.inserted.len(), &op.removed);
        ed.content.replace_range(op.at..end, &op.removed);
        // floor_boundary 而不是 `.min(len)`：`caret_before` 是**另一个形状**的缓冲区留下的
        // 偏移，撤销一段含组字文本的编辑后，它完全可能落在某个多字节字符中间。`.min` 只挡
        // 越界，落在字符中间照样 panic——而且不是崩在这里，是崩在下一次退格/删除/绘制上，
        // 现场看不出跟撤销有关。vcaret 是「必须是字符边界」这条不变量唯一的漏口，堵在这里。
        ed.vcaret = crate::ui::ime_safe::floor_boundary(&ed.content, op.caret_before);
        ed.vsel = None;
        ed.vgoal_col = None;
        // 撤销可能精确回到保存点（或离开它）——必须全量重算而非简单置位
        ed.recompute_dirty();
        v_recompute(ed);
        ed.vredo.push(op);
    }
}
pub(super) fn v_redo(ed: &mut Editor) {
    if !history_ready(ed) {
        return;
    }
    if let Some(op) = ed.vredo.pop() {
        if !history_matches(ed, op.at, &op.removed) {
            return;
        }
        ed.msel.clear();
        let end = op.at + op.removed.len();
        v_remap_folds(ed, op.at, op.removed.len(), &op.inserted);
        ed.content.replace_range(op.at..end, &op.inserted);
        ed.vcaret = crate::ui::ime_safe::floor_boundary(&ed.content, op.caret_after); // 同 v_undo
        ed.vsel = None;
        ed.vgoal_col = None;
        ed.recompute_dirty();
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
    ed.vcaret = if fwd {
        next_char_boundary(&ed.content, ed.vcaret)
    } else {
        prev_char_boundary(&ed.content, ed.vcaret)
    };
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
        // 行数表平时在绘制时同步；同一帧里先有编辑再有移动的话它还是旧的。这里先同步
        //（没变化时是空操作）。
        super::wrap::v_wrap_sync(ed, cols);
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
    let col = ed
        .vgoal_col
        .unwrap_or_else(|| ed.content[ls..ed.vcaret].chars().count());
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
            // 停在上一行的行末文字之后：`\r\n` 整个算行尾（见 `v_line_range`）
            return if s.as_bytes()[..i].ends_with(b"\r\n") {
                i - 2
            } else {
                i - 1
            };
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
/// 把多光标区间收敛到当前内容的合法字符边界上。正常情况下它们本来就合法（`v_apply`
/// 会让非多光标的编辑退出多选）；这是给漏网的陈旧区间兜底——下面几处是裸切片，
/// 一个落在汉字中间的偏移就是 panic。
fn v_multi_sanitize(ed: &mut Editor) {
    let content = &ed.content;
    for r in ed.msel.iter_mut() {
        *r = crate::ui::ime_safe::clamp_range(content, *r);
    }
}
pub(super) fn v_multi_add_next(ed: &mut Editor) {
    v_multi_sanitize(ed);
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
        let p = match ed.content[pos.min(ed.content.len())..]
            .find(&needle)
            .map(|o| pos + o)
            .or_else(|| ed.content.find(&needle))
        {
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
    if ed.is_readonly() {
        return;
    }
    v_multi_sanitize(ed);
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
    v_multi_sanitize(ed);
    let del: Vec<(usize, usize)> = ed
        .msel
        .iter()
        .map(|&(s, e)| {
            if e > s {
                (s, e)
            } else {
                (prev_char_boundary(&ed.content, s), s)
            }
        })
        .collect();
    ed.msel = del;
    v_multi_replace(ed, "");
}
pub(super) fn v_multi_delete(ed: &mut Editor) {
    v_multi_sanitize(ed);
    let del: Vec<(usize, usize)> = ed
        .msel
        .iter()
        .map(|&(s, e)| {
            if e > s {
                (s, e)
            } else {
                (s, next_char_boundary(&ed.content, s))
            }
        })
        .collect();
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
    let parts: Vec<String> = ed
        .msel
        .iter()
        .filter(|&&(s, e)| e > s)
        .map(|&(s, e)| ed.content[s..e].to_string())
        .collect();
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
    let (a, b) = if fwd {
        (ed.vcaret, to)
    } else {
        (to, ed.vcaret)
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// dirty 标记（O(1) 读取，替代每帧全文 memcmp）的关键正确性：
    /// 编辑置位、撤销精确回到保存点时必须复位、重做再次置位。
    #[test]
    fn undo_back_to_saved_point_clears_dirty() {
        let mut ed = Editor::new("/tmp/a.txt".into(), "hello\n".into());
        ed.set_meta("UTF-8".into(), crate::proto::Eol::Lf, 1);
        assert!(!ed.dirty());
        v_insert(&mut ed, "x");
        assert!(ed.dirty(), "编辑后应有改动");
        v_undo(&mut ed);
        assert!(!ed.dirty(), "撤销回到保存点应恢复干净");
        v_redo(&mut ed);
        assert!(ed.dirty(), "重做应再次有改动");
    }

    /// 撤销把光标放回**另一个形状**的缓冲区留下的偏移，它可能落在多字节字符中间。
    /// 此后第一次退格就崩在 `v_apply` 的 `content[at..at+len]` 上——注意光标吸附必须做在
    /// **撤销那一步**：只让 `prev_char_boundary` 内部吸附是不够的，调用方仍拿未吸附的
    /// `vcaret` 去算删除长度，panic 只是从一处挪到另一处。
    /// 混合行尾的文件（`a` 行是 CRLF，其余 LF）：`\r` 原样留在内容里，但它属于行尾——
    /// 光标停不进 `\r` 与 `\n` 之间，打字不会插到 `\r` 后面，整行操作不丢不留它。
    fn mixed() -> Editor {
        let mut ed = ed_with("a\r\nbc\nd");
        super::super::wrap::v_recompute(&mut ed);
        ed
    }

    #[test]
    fn end_stops_before_the_cr_of_a_crlf_line() {
        let mut ed = mixed();
        ed.vcaret = 0;
        v_move_edge(&mut ed, true, false);
        assert_eq!(ed.vcaret, 1, "End 停在了 \\r 之后");
        v_insert(&mut ed, "X");
        assert_eq!(ed.content, "aX\r\nbc\nd");
    }

    #[test]
    fn arrows_backspace_and_delete_treat_crlf_as_one_line_end() {
        let mut ed = mixed();
        ed.vcaret = 1;
        v_move_h(&mut ed, true, false);
        assert_eq!(ed.vcaret, 3, "右移应直接到下一行行首");
        v_move_h(&mut ed, false, false);
        assert_eq!(ed.vcaret, 1, "左移应回到上一行行末文字之后");
        ed.vcaret = 3;
        v_move_word(&mut ed, false, false);
        assert_eq!(ed.vcaret, 1, "按词左移停在了 \\r 与 \\n 之间");

        let mut ed = mixed();
        ed.vcaret = 3;
        v_backspace(&mut ed);
        assert_eq!(ed.content, "abc\nd", "退格应删掉整个 \\r\\n");
        let mut ed = mixed();
        ed.vcaret = 1;
        v_delete_fwd(&mut ed);
        assert_eq!(ed.content, "abc\nd", "Delete 应删掉整个 \\r\\n");
    }

    #[test]
    fn vertical_moves_and_enter_respect_the_crlf_line_end() {
        let mut ed = mixed();
        ed.wrap = false;
        ed.vcaret = 5; // bc 行末
        v_move_v(&mut ed, -1, false);
        assert_eq!(ed.vcaret, 1, "上移落进了 \\r 与 \\n 之间");

        let mut ed = mixed();
        ed.vcaret = 1;
        v_newline_indent(&mut ed);
        assert_eq!(ed.content, "a\r\n\r\nbc\nd", "CRLF 行里回车应断出 CRLF");
    }

    #[test]
    fn line_commands_keep_each_line_end_intact() {
        use super::super::commands::{v_delete_line, v_duplicate_line, v_move_line};
        let mut ed = mixed();
        ed.vcaret = 0;
        v_delete_line(&mut ed);
        assert_eq!(ed.content, "bc\nd", "删行留下了半个行尾");

        let mut ed = mixed();
        ed.vcaret = 0;
        v_duplicate_line(&mut ed, true);
        assert_eq!(ed.content, "a\r\na\r\nbc\nd");

        let mut ed = mixed();
        ed.vcaret = 0;
        v_move_line(&mut ed, false);
        assert_eq!(ed.content, "bc\r\na\nd", "两行之间的行尾被改写了");
    }

    /// 状态栏的「混合 → 统一为 LF / CRLF」：一次可撤销的编辑，光标留在原来那个字符上。
    #[test]
    fn unifying_mixed_line_ends_is_one_undoable_edit() {
        let mut ed = mixed();
        assert!(v_mixed_eol(&mut ed));
        ed.vcaret = 4; // bc 的 c
        v_unify_eol(&mut ed, crate::proto::Eol::Lf);
        assert_eq!(ed.content, "a\nbc\nd");
        assert_eq!(&ed.content[ed.vcaret..ed.vcaret + 1], "c", "光标没跟着字符走");
        assert!(!v_mixed_eol(&mut ed), "缓存没随内容失效");
        assert!(ed.dirty());
        v_undo(&mut ed);
        assert_eq!(ed.content, "a\r\nbc\nd");
        assert!(v_mixed_eol(&mut ed));

        let mut ed = mixed();
        v_unify_eol(&mut ed, crate::proto::Eol::Crlf);
        assert_eq!(ed.content, "a\nbc\nd");
        assert_eq!(ed.eol(), crate::proto::Eol::Crlf, "保存时按 CRLF 写出");

        let mut ed = mixed();
        ed.readonly = true;
        v_unify_eol(&mut ed, crate::proto::Eol::Lf);
        assert_eq!(ed.content, "a\r\nbc\nd", "只读时不该改");
    }

    fn ed_with(content: &str) -> Editor {
        let mut ed = Editor::new("/tmp/a.txt".into(), content.into());
        ed.set_meta("UTF-8".into(), crate::proto::Eol::Lf, 1);
        ed
    }

    /// 光标停在多字节字符中间时退格。
    ///
    /// 注意断言的对象是**生产代码自己吸附**：光标直接置成 5（「文」的中间），一步都不替它
    /// 修正就调 `v_backspace`。修复前 `prev_char_boundary` 内部吸附出 prev=0，调用方却拿
    /// 未吸附的 vcaret 算长度，`v_apply` 执行 `content[0..5]` 当场 panic——光在 helper 里
    /// 吸附只是把 panic 从一处挪到另一处。
    #[test]
    fn backspace_with_a_mid_character_caret_does_not_panic() {
        let mut ed = ed_with("中文abc");
        assert!(!ed.content.is_char_boundary(5));
        ed.vcaret = 5;
        v_backspace(&mut ed);
        assert!(ed.content.is_char_boundary(ed.vcaret));
        assert_eq!(ed.content, "文abc", "应当只删掉「中」这一个字符");
    }

    /// 前向删除同理：`v_apply(ed, vcaret, next - vcaret, "")` 的**左端**就是未吸附的 vcaret。
    #[test]
    fn delete_forward_with_a_mid_character_caret_does_not_panic() {
        let mut ed = ed_with("中文abc");
        ed.vcaret = 5;
        v_delete_fwd(&mut ed);
        assert!(ed.content.is_char_boundary(ed.vcaret));
        assert_eq!(ed.content, "中abc");
    }

    /// 走完整的一条真实路径：光标落在多字节字符中间时做了一次编辑（组字直接改 content 就是
    /// 这个形状），撤销会把那个坏偏移原样放回 `vcaret`——`.min(len)` 拦不住它。
    /// 此后第一次退格就崩，而且崩在删除代码里、看不出跟撤销有关。
    #[test]
    fn undo_does_not_restore_a_mid_character_caret() {
        let mut ed = ed_with("中文abc");
        ed.vcaret = 5; // 「文」的中间
        v_insert(&mut ed, "X"); // 这次编辑把 caret_before=5 记进了撤销栈
        v_undo(&mut ed);
        assert!(
            ed.content.is_char_boundary(ed.vcaret),
            "撤销把光标放回了字符中间：{}",
            ed.vcaret
        );
        v_backspace(&mut ed); // 修复前在这里 panic
    }

    fn ed_rs(text: &str) -> Editor {
        let mut ed = Editor::new("/tmp/a.rs".into(), text.into());
        ed.set_meta("UTF-8".into(), crate::proto::Eol::Lf, 1);
        v_recompute(&mut ed);
        ed
    }

    /// 编辑恰好回到保存时的内容（敲一个字又删掉）：不该还算「已修改」。
    #[test]
    fn editing_back_to_the_saved_content_is_clean() {
        let mut ed = ed_rs("abc");
        v_insert(&mut ed, "x");
        assert!(ed.dirty());
        v_backspace(&mut ed);
        assert_eq!(ed.content, "abc");
        assert!(!ed.dirty(), "内容已回到保存点，仍显示已修改");
        // 长度相等但内容不同：仍是已修改
        ed.vcaret = 0;
        ed.vsel = Some(1);
        v_insert(&mut ed, "z");
        assert_eq!(ed.content, "zbc");
        assert!(ed.dirty());
    }

    /// 同一帧里「先编辑、再上下移动」：移动用的折行行数表必须是编辑之后的。表只在绘制时
    /// 同步的话，这一帧里它还是旧的——行数变了，上移就落到错误的那一段上。
    #[test]
    fn vertical_moves_after_an_edit_in_the_same_frame_use_fresh_rows() {
        let mut ed = ed_rs("abcdefgh\nxy");
        super::super::wrap::v_wrap_sync(&mut ed, 4); // 第 0 行 2 段
        ed.vcaret = 8;
        v_insert(&mut ed, "ijkl"); // 第 0 行变成 3 段；这一帧还没绘制
        ed.vcaret = 13; // 第 1 行行首
        v_move_v(&mut ed, -1, false);
        assert_eq!(ed.vcaret, 8, "应落在第 0 行最后一段（ijkl）的行首");
    }

    /// 什么都没改的「编辑」不是编辑：不该置 dirty、不该占一条撤销记录、不该清空重做栈。
    /// 空文件删行、光标在文首的多光标退格、空的输入法提交都会走到这里。
    #[test]
    fn a_no_op_edit_leaves_no_trace() {
        let mut ed = ed_rs("");
        super::super::commands::v_delete_line(&mut ed);
        v_apply(&mut ed, 0, 0, "");
        ed.msel = vec![(0, 0)];
        v_multi_backspace(&mut ed);
        assert!(!ed.dirty(), "没改任何内容却显示已修改");
        assert!(ed.vundo.is_empty());
        // 重做栈不被空操作清掉
        let mut ed = ed_rs("a");
        v_insert(&mut ed, "b");
        v_undo(&mut ed);
        v_apply(&mut ed, 0, 0, "");
        assert_eq!(ed.vredo.len(), 1);
    }

    /// 无选区按 Shift+Tab 只是反缩进当前行，不该留下一个整行选区——
    /// 留下的话，接着敲的那个字会把整行替换掉。
    #[test]
    fn outdent_without_a_selection_does_not_select_the_line() {
        let mut ed = ed_rs("    let x = 1;\n");
        ed.vcaret = 8; // 在 `let` 之后
        v_block_indent(&mut ed, false);
        assert_eq!(ed.content, "let x = 1;\n");
        assert_eq!(v_sel_range(&ed), None);
        assert_eq!(ed.vcaret, 4, "光标应跟着文字左移");
        // 有选区时照旧选中整块，便于连续调整
        let mut ed = ed_rs("    a\n    b\n");
        ed.vsel = Some(0);
        ed.vcaret = 11;
        v_block_indent(&mut ed, false);
        assert_eq!(ed.content, "a\nb\n");
        assert!(v_sel_range(&ed).is_some());
    }

    /// 注释切换是一次操作：一次撤销就该还原，而不是选了几行就要按几次 Ctrl+Z。
    #[test]
    fn toggling_comments_is_a_single_undo_step() {
        let src = "fn a() {\n    x();\n\n    y();\n}\n";
        let mut ed = ed_rs(src);
        ed.vsel = Some(0);
        ed.vcaret = src.len() - 1;
        super::super::commands::v_toggle_comment(&mut ed, "//");
        assert_eq!(ed.content, "// fn a() {\n    // x();\n\n    // y();\n// }\n");
        v_undo(&mut ed);
        assert_eq!(ed.content, src);
        // 切换后整块保持选中，紧接着再按一次就是反注释
        ed.vsel = Some(0);
        ed.vcaret = src.len() - 1;
        super::super::commands::v_toggle_comment(&mut ed, "//");
        super::super::commands::v_toggle_comment(&mut ed, "//");
        assert_eq!(ed.content, src);
    }

    /// 多光标区间是字节偏移，内容被别的路径改过之后就是陈旧的。普通编辑必须退出多选；
    /// 即便真有陈旧区间漏进来（落在汉字中间、越界），多光标替换也不能 panic。
    #[test]
    fn stale_multi_cursor_ranges_never_panic() {
        let mut ed = ed_rs("中文 中文 中文");
        ed.msel = vec![(0, 6), (7, 13), (14, 20)];
        v_apply(&mut ed, 0, 6, ""); // 右键剪切 / 查找替换走的就是这条
        assert!(ed.msel.is_empty(), "普通编辑后仍留着旧的多选区间");
        ed.msel = vec![(1, 2), (5, 99), (200, 300)]; // 人为塞进陈旧区间
        v_multi_replace(&mut ed, "x");
        v_multi_backspace(&mut ed);
        v_multi_add_next(&mut ed);
    }

    /// 多光标下用输入法：组字期间只在主光标显示，提交时要作用到全部光标。
    #[test]
    fn ime_commit_applies_to_every_cursor() {
        use super::super::input::{v_ime_commit, v_preedit};
        let mut ed = ed_rs("foo foo");
        ed.msel = vec![(0, 3), (4, 7)];
        ed.vsel = Some(4);
        ed.vcaret = 7;
        v_preedit(&mut ed, "ni");
        v_preedit(&mut ed, "nihao");
        v_ime_commit(&mut ed, "你好");
        assert_eq!(ed.content, "你好 你好");
        // 取消组字：多选原样回来
        let mut ed = ed_rs("foo foo");
        ed.msel = vec![(0, 3), (4, 7)];
        ed.vsel = Some(4);
        ed.vcaret = 7;
        v_preedit(&mut ed, "ni");
        super::super::input::v_cancel_preedit(&mut ed);
        assert_eq!(ed.content, "foo foo");
        assert_eq!(ed.msel, vec![(0, 3), (4, 7)]);
    }

    /// 组字被退格删空（`Preedit("")`）后内容已回到原样，不该留着脏标记。
    #[test]
    fn emptied_composition_restores_clean_state() {
        use super::super::input::v_preedit;
        let mut ed = ed_rs("abc");
        v_preedit(&mut ed, "n");
        assert!(ed.dirty());
        v_preedit(&mut ed, "");
        assert_eq!(ed.content, "abc");
        assert!(!ed.dirty());
    }

    /// 粘贴源（Windows 记事本 / 浏览器）几乎总是 CRLF；编辑器内部统一 LF。
    #[test]
    fn pasted_text_is_normalized_to_lf() {
        assert_eq!(normalize_paste("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize_paste("plain"), "plain");
    }
}
