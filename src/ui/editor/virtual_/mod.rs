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
pub(super) use wrap::SegCache;

#[cfg(test)]
pub(super) fn test_insert(ed: &mut super::Editor, t: &str) {
    edit::v_insert(ed, t);
}
/// 当前登记的字体下，一段文字的显示宽度（列）。测试里按它推期望值，别硬编码某台机器上
/// 的字宽——中日韩字体读的是系统字体，CI 镜像和开发机装的不一样。
#[cfg(test)]
pub(super) fn test_str_cols(s: &str) -> f32 {
    geom::str_cols(s)
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
/// 测试用：某逻辑行各折段的显示宽度（列）。
#[cfg(test)]
pub(super) fn test_seg_widths(ed: &super::Editor, line: usize) -> Vec<f32> {
    let (ls, le) = geom::v_line_range(ed, line);
    let text = &ed.content[ls..le];
    let n = (ed.vrow_pre[line + 1] - ed.vrow_pre[line]) as usize;
    (0..n)
        .map(|seg| {
            let (a, b) = wrap::v_seg_range(ed, line, seg, ed.vrow_cols);
            geom::str_cols(&text[a..b])
        })
        .collect()
}
#[cfg(test)]
pub(super) fn test_undo(ed: &mut super::Editor) {
    edit::v_undo(ed);
}
