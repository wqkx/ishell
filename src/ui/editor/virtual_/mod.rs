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
pub(super) use view::editable_virtual;
pub(super) use wrap::v_recompute;
