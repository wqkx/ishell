//! 右侧文件列表：工具栏、面包屑、表格与拖拽。从 file_panel 拆出，行为不变。

use super::{FileAction, FilePanelState};

mod helpers;
mod table;
mod toolbar;

pub(super) use helpers::{valid_move_srcs, DragPaths};

pub(super) fn file_list(
    ui: &mut egui::Ui,
    state: &mut FilePanelState,
    has_clip: bool,
    actions: &mut Vec<FileAction>,
) {
    toolbar::toolbar(ui, state, actions);
    ui.separator();
    table::file_table(ui, state, has_clip, actions);
}
