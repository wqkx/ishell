use super::super::Editor;
use super::chrome::{apply_context_menu_actions, show_status_and_find, ChromeActions};
use super::fold::{v_line_hidden, v_sync_leads};
use super::geom::v_line_of;
use super::input::handle_input;
use super::paint::paint_visible_rows;
use super::wrap::v_recompute;
use crate::ui::highlight::{self, Indent};

/// 超过该大小则跳过括号 lint（避免大文件每次编辑都整体 tokenize）。
const LINT_LIMIT: usize = 256 * 1024;
/// 跨行高亮状态的全文扫描上限（超过则逐行独立高亮，不付每次编辑的全文扫描成本）
const HL_STATE_LIMIT: usize = 2 * 1024 * 1024;

pub fn editable_virtual(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> bool {
    if ed.vlines.is_empty() {
        v_recompute(ed);
    }
    ed.vcaret = ed.vcaret.min(ed.content.len());

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
            m.set_focus_lock_filter(
                text_id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
    }
    let page = ((ui.available_height() / row_h).floor() as isize - 2).max(1);
    let lang = ed.language.clone();
    let fsize = mono.size;
    let mut chrome_actions = ChromeActions::default();

    let (save, moved, _) = handle_input(ui, ed, focused, page);

    // ⚠ 这些「按内容版本缓存」的重算**必须**放在 handle_input 之后、绘制之前。
    // 它们此前放在函数开头，于是同一帧里顺序是「算缓存 → handle_input 改内容 → 用旧缓存
    // 绘制」：粘贴/输入后 content 变了、vver 也自增了，但本帧绘制用的仍是改动前算出的
    // lint_ranges——那是一组**旧内容**的字节偏移。纯 ASCII 时旧偏移顶多让下划线画错一帧
    //（每个字节都是字符边界）；内容含中文等多字节字符时，旧偏移会落进某个字符中间，
    // highlight_segment 按错误范围边界切片就 panic 整个应用闪退
    //（`end byte index N is not a char boundary`）。放到输入之后即可根治：缓存与被绘制的
    // 内容永远同一个版本。
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
    // 避免拖动大文件时每帧反复切片扫描缩进造成的卡顿。同样要在 handle_input 之后，
    // 否则本帧绘制拿到的是改动前的行缩进。
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

    show_status_and_find(ui, ed, text_id);
    paint_visible_rows(
        ui,
        ed,
        text_id,
        row_h,
        char_w,
        mono,
        bg,
        focused,
        moved,
        lang,
        fsize,
        &mut chrome_actions,
    );
    apply_context_menu_actions(ui, ed, chrome_actions);
    save
}
