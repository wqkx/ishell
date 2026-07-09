//! 按缩进的代码折叠。

use super::super::Editor;
use super::geom::{v_line_of, v_line_range};

// ——— 按缩进的代码折叠 ———
/// 该行是否被某个折叠区域隐藏（header 行本身可见）。
pub(super) fn v_line_hidden(ed: &Editor, line: usize) -> bool {
    // folds 按 header 升序且不重叠：找最后一个 header < line 的区域
    let idx = ed.folds.partition_point(|&(h, _)| h < line);
    idx > 0 && ed.folds[idx - 1].1 >= line
}
/// 重建每行缩进列宽缓存（内容版本或缩进单位变化时）。O(总字符数)，仅编辑/切换缩进时付出。
pub(super) fn v_sync_leads(ed: &mut Editor, unit: usize) {
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
pub(super) fn v_lead(ed: &Editor, line: usize, unit: usize) -> Option<usize> {
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
pub(super) fn v_foldable(ed: &Editor, line: usize, unit: usize) -> bool {
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
pub(super) fn v_fold_region(ed: &Editor, line: usize, unit: usize) -> Option<(usize, usize)> {
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
pub(super) fn v_remap_folds(ed: &mut Editor, at: usize, removed_len: usize, inserted: &str) {
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
pub(super) fn v_toggle_fold(ed: &mut Editor, line: usize, unit: usize) {
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
