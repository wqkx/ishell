//! 虚拟化编辑器：行映射/编辑操作/渲染循环。从 editor 拆出，行为不变。

mod chrome;
mod commands;
mod edit;
mod fold;
mod geom;
mod input;
mod paint;
mod view;
mod wrap;

pub(super) use geom::{v_line_of, v_sel_range};
pub(super) use input::v_cancel_preedit;
pub(super) use view::editable_virtual;
pub(super) use wrap::v_recompute;

#[cfg(test)]
pub(super) fn test_insert(ed: &mut super::Editor, t: &str) {
    edit::v_insert(ed, t);
}
#[cfg(test)]
pub(super) fn test_caret_vrow(ed: &super::Editor) -> usize {
    wrap::v_vpos_of_byte(ed, ed.vcaret, ed.vrow_cols.max(1)).0
}
/// 测试用：像「跳转到行」那样只设光标与待滚动目标（它发生在折叠自动展开的检查之后）。
#[cfg(test)]
pub(super) fn test_goto_line(ed: &mut super::Editor, line: usize) {
    ed.goto_open = false;
    ed.vcaret = geom::v_line_range(ed, line).0;
    ed.vsel = None;
    ed.pending_scroll = Some(line);
}
#[cfg(test)]
pub(super) fn test_undo(ed: &mut super::Editor) {
    edit::v_undo(ed);
}
