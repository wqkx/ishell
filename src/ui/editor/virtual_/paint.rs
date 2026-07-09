use egui::text::CCursor;

use super::super::Editor;
use super::chrome::ChromeActions;
use super::commands::bracket_match;
use super::edit::{v_complete_accept, v_word_range};
use super::fold::{v_foldable, v_lead, v_toggle_fold};
use super::geom::{byte_to_char, char_to_byte, v_line_of, v_line_range, v_sel_range};
use super::wrap::{v_line_of_vrow, v_total_vrows, v_vpos_of_byte, v_wrap_sync};
use crate::theme::Palette;
use crate::ui::highlight::{self, Indent};

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_visible_rows(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    text_id: egui::Id,
    row_h: f32,
    char_w: f32,
    mono: egui::FontId,
    bg: egui::Color32,
    focused: bool,
    moved: bool,
    lang: String,
    fsize: f32,
    actions: &mut ChromeActions,
) {
    // ——— 渲染（仅可见行）———
    let total = ed.vlines.len();
    let digits = total.max(1).to_string().len();
    // 行号 + 折叠箭头列（箭头在行号右侧，占约 1.5 字符宽）
    let gutter_w = (digits as f32 + 3.0) * char_w;
    // 自动换行：按视口宽度算每行可容纳列数，并同步「视觉行前缀和」缓存（列宽/内容变化才重算）
    let view_w_pre = if ed.vlast_vieww > 0.0 {
        ed.vlast_vieww
    } else {
        ui.available_width()
    };
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
        let view_h = if ed.vlast_viewh > 0.0 {
            ed.vlast_viewh
        } else {
            ui.available_height()
        };
        let view_w = if ed.vlast_vieww > 0.0 {
            ed.vlast_vieww
        } else {
            ui.available_width()
        };
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
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(bg))
        .show_inside(ui, |ui| {
            ui.spacing_mut().scroll.floating = false;
            ui.spacing_mut().scroll.foreground_color = false;
            ui.visuals_mut().extreme_bg_color = bg;
            ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_rgb(205, 200, 188);
            ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_rgb(172, 166, 152);
            ui.visuals_mut().widgets.active.bg_fill = egui::Color32::from_rgb(144, 138, 124);
            // 横向交给 egui；竖向自绘（下面按 ed.vtop 渲染 + 自画滚动条）。
            let mut sa = egui::ScrollArea::horizontal()
                .auto_shrink([false, false])
                .id_salt(text_id);
            if let Some(h) = force_h {
                sa = sa.horizontal_scroll_offset(h);
            }
            sa.show_viewport(ui, |ui, vp| {
                ui.set_width(content_w);
                let origin = ui.min_rect().min;
                // horizontal 模式不竖向裁剪，手动把 clip 夹到底部状态栏之上（否则正文/滚动条画到状态栏上、且抢其点击）。
                let clip_full = ui.clip_rect();
                let clip = egui::Rect::from_min_max(
                    clip_full.min,
                    egui::pos2(clip_full.max.x, clip_full.max.y.min(content_bottom)),
                );
                ui.set_clip_rect(clip);
                ui.set_height((clip.bottom() - origin.y).max(row_h)); // 内容高度限到视口，横向滚动条落在状态栏之上
                let view_h = clip.height();
                let visible = (view_h / row_h).ceil() as usize + 2;
                let max_top = (nrows + pad_rows).saturating_sub(visible.saturating_sub(2)); // 最大首行号
                let top_row = ed.vtop.min(max_top);
                ed.vtop = top_row;
                // 首/末可见逻辑行（由视觉行换算；用于查找命中的可视范围）
                let first_line = v_line_of_vrow(ed, top_row).0;
                let last_line =
                    v_line_of_vrow(ed, (top_row + visible).min(nrows.saturating_sub(1))).0 + 1;
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
                let sb_track = egui::Rect::from_min_max(
                    egui::pos2(clip.right() - sb_w, clip.top()),
                    clip.right_bottom(),
                );
                let total_rows = (nrows + pad_rows).max(1);
                let show_vsb = max_top > 0;
                let mut vsb_hit = false;
                // 滑块几何/配色留到正文绘制之后再画（否则被后绘的字形盖住）。
                let mut vsb_thumb: Option<(egui::Rect, egui::Color32)> = None;
                if show_vsb {
                    // 滚动条交互必须先于正文 resp 注册、且正文交互区要避开这条右缘（见下 area），
                    // 否则同层里后注册、覆盖更广的正文会「盖」在滚动条上，把拖动事件抢走 → 点不到。
                    let sb_resp =
                        ui.interact(sb_track, text_id.with("vsb"), egui::Sense::click_and_drag());
                    let thumb_h = (sb_track.height() * (visible as f32 / total_rows as f32))
                        .clamp(24.0, sb_track.height());
                    if sb_resp.dragged() || sb_resp.clicked() {
                        if let Some(p) = sb_resp.interact_pointer_pos() {
                            // 让指针对准滑块中心：行号 = (指针 - 半个滑块) / 可移动轨道 × max_top
                            let f = ((p.y - sb_track.top() - thumb_h / 2.0)
                                / (sb_track.height() - thumb_h).max(1.0))
                            .clamp(0.0, 1.0);
                            ed.vtop = (f * max_top as f32).round() as usize;
                            ui.ctx().request_repaint();
                        }
                    }
                    vsb_hit = sb_resp.hovered() || sb_resp.dragged();
                    let top_now = ed.vtop.min(max_top);
                    let frac = top_now as f32 / max_top as f32;
                    let thumb_y = sb_track.top() + (sb_track.height() - thumb_h) * frac;
                    let thumb = egui::Rect::from_min_size(
                        egui::pos2(sb_track.left() + 2.5, thumb_y),
                        egui::vec2(sb_w - 5.0, thumb_h),
                    );
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
                    egui::Rect::from_min_max(
                        clip.min,
                        egui::pos2(clip.right() - sb_w, clip.bottom()),
                    )
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
                    if ui
                        .add_enabled(has_sel, egui::Button::new(crate::i18n::tr("复制", "Copy")))
                        .clicked()
                    {
                        actions.do_copy = true;
                        ui.close();
                    }
                    if ui
                        .add_enabled(has_sel, egui::Button::new(crate::i18n::tr("剪切", "Cut")))
                        .clicked()
                    {
                        actions.do_cut = true;
                        ui.close();
                    }
                    if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                        actions.do_paste = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(crate::i18n::tr("全选", "Select all")).clicked() {
                        actions.do_selall = true;
                        ui.close();
                    }
                });
                let painter = ui.painter().clone();
                // 多选时用 msel 全部选区/光标；否则用单选区 + 单光标
                let sels: Vec<(usize, usize)> = if !ed.msel.is_empty() {
                    ed.msel.clone()
                } else {
                    v_sel_range(ed).into_iter().collect()
                };
                let carets: Vec<usize> = if !ed.msel.is_empty() {
                    ed.msel.iter().map(|&(_, e)| e).collect()
                } else {
                    vec![ed.vcaret]
                };
                let caret_line = v_line_of(ed, ed.vcaret); // 当前行高亮
                let unit_cols = match ed.indent {
                    Indent::Spaces(n) => n.max(1),
                    Indent::Tab => 4,
                }; // 缩进参考线步长
                   // 纯文本（未识别的扩展名）不显示缩进对齐线 / 折叠 / 粘性作用域等依赖缩进结构的代码辅助
                let show_code_aids = highlight::is_code(&ed.language);
                // 活动缩进线（VSCode 风格）：光标所在代码块对应的那条竖线高亮。
                // (列, 起始行, 结束行)：列 = 光标行缩进的上一级；范围 = 向上下延伸「更深缩进或空白」的行
                let active_guide: Option<(usize, usize, usize)> = if !show_code_aids {
                    None
                } else {
                    let resolve = |l: usize| -> Option<usize> {
                        v_lead(ed, l, unit_cols).or_else(|| {
                            let up = (0..l)
                                .rev()
                                .take(400)
                                .find_map(|x| v_lead(ed, x, unit_cols));
                            let down = ((l + 1)..total)
                                .take(400)
                                .find_map(|x| v_lead(ed, x, unit_cols));
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
                        let deeper =
                            |l: usize| v_lead(ed, l, unit_cols).map(|d| d > col).unwrap_or(true);
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
                let brackets = if focused {
                    bracket_match(&ed.content, ed.vcaret)
                } else {
                    None
                }; // 括号匹配高亮
                   // 可视区内的查找匹配（克隆出来，避免后续可变借用 ed 冲突）
                let vis_matches: Vec<(usize, usize)> = if ed.show_find && !ed.find.is_empty() {
                    let vis_a = ed.vlines.get(first_line).copied().unwrap_or(0);
                    let vis_b = ed
                        .vlines
                        .get(last_line.min(total))
                        .copied()
                        .unwrap_or(ed.content.len());
                    let mlo = ed.find_matches.partition_point(|&(s, _)| s < vis_a);
                    let mhi = ed.find_matches.partition_point(|&(s, _)| s < vis_b);
                    ed.find_matches[mlo..mhi].to_vec()
                } else {
                    Vec::new()
                };
                // 双击选词后的「相同词」淡高亮（仅常见代码类型）：当前选区恰为一个完整词时，
                // 可见行内该词的其它出现处铺一层比查找更淡的底色（VSCode occurrence 风格）
                let occ_word: Option<String> =
                    if highlight::lint_enabled(&ed.language) && ed.msel.is_empty() {
                        v_sel_range(ed).and_then(|(a, b)| {
                            let w = &ed.content[a..b];
                            let is_w = |c: char| c.is_ascii_alphanumeric() || c == '_';
                            let ok = (2..=64).contains(&(b - a))
                                && w.chars().all(is_w)
                                && (a == 0 || !is_w(ed.content[..a].chars().next_back().unwrap()))
                                && (b >= ed.content.len()
                                    || !is_w(ed.content[b..].chars().next().unwrap()));
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
                let gutter_hover = ui
                    .input(|inp| inp.pointer.hover_pos())
                    .is_some_and(|p| clip.contains(p) && p.x < clip.left() + gutter_w);
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
                        (
                            li,
                            first_col,
                            cols_vis,
                            text_x + first_col as f32 * char_w,
                            true,
                        )
                    };
                    if i >= total {
                        break;
                    }
                    let (ls, le) = v_line_range(ed, i);
                    let line_full: &str = &ed.content[ls..le]; // 切片，不整行拷贝
                    let y = clip.top() + k as f32 * row_h;
                    let col_of = |b: usize| -> usize {
                        byte_to_char(line_full, b.saturating_sub(ls).min(line_full.len()))
                    };
                    let in_win = |c: usize| c >= col0 && c <= col0 + ncols;
                    // 当前行高亮（极淡）：聚焦且无选区时，给光标所在行铺一层很淡的底
                    if focused && sels.is_empty() && i == caret_line {
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(clip.left(), y),
                                egui::pos2(clip.right(), y + row_h),
                            ),
                            0.0,
                            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10),
                        );
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
                            let active = active_guide
                                .is_some_and(|(ac, lo, hi)| col == ac && i >= lo && i <= hi);
                            let stroke = if active {
                                egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgba_unmultiplied(
                                        accent.r(),
                                        accent.g(),
                                        accent.b(),
                                        150,
                                    ),
                                )
                            } else {
                                egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 30),
                                )
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
                                    egui::Stroke::new(
                                        1.0,
                                        egui::Color32::from_rgba_unmultiplied(
                                            accent.r(),
                                            accent.g(),
                                            accent.b(),
                                            150,
                                        ),
                                    ),
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
                        let state = ed
                            .hl_states
                            .get(i)
                            .copied()
                            .unwrap_or(highlight::LineState::Normal);
                        let mut job = highlight::highlight_segment(
                            line_full,
                            seg_a..seg_b,
                            &lang,
                            fsize,
                            &lint_errs,
                            state,
                        );
                        job.wrap.max_width = f32::INFINITY;
                        ui.ctx().fonts_mut(|f| f.layout_job(job))
                    };
                    // 行内字节偏移 → 屏幕 x（窗口外钳制到窗口边缘，超出部分本就不可见）
                    let x_of = |lb: usize| -> f32 {
                        seg_x
                            + galley
                                .pos_from_cursor(CCursor::new(byte_to_char(
                                    seg,
                                    lb.clamp(seg_a, seg_b) - seg_a,
                                )))
                                .left()
                    };
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
                                if (s0 > 0 && isw(bytes[s0 - 1]))
                                    || (e0 < bytes.len() && isw(bytes[e0]))
                                {
                                    continue;
                                }
                                if sels.iter().any(|&(sa, sb)| sa == ls + s0 && sb == ls + e0) {
                                    continue;
                                }
                                let ax = x_of(s0);
                                let bx = x_of(e0);
                                if bx > ax {
                                    painter.rect_filled(
                                        egui::Rect::from_min_max(
                                            egui::pos2(ax, y),
                                            egui::pos2(bx, y + row_h),
                                        ),
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
                            let bx = if sb >= ls + seg_b {
                                seg_right
                            } else {
                                x_of(sb.clamp(ls, le) - ls)
                            };
                            if bx > ax {
                                painter.rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(ax, y),
                                        egui::pos2(bx, y + row_h),
                                    ),
                                    0.0,
                                    egui::Color32::from_rgba_unmultiplied(
                                        accent.r(),
                                        accent.g(),
                                        accent.b(),
                                        60,
                                    ),
                                );
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
                                painter.rect_stroke(
                                    egui::Rect::from_min_max(
                                        egui::pos2(bx0, y),
                                        egui::pos2(bx1, y + row_h),
                                    ),
                                    2.0,
                                    egui::Stroke::new(1.0, Palette::ACCENT),
                                    egui::StrokeKind::Inside,
                                );
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
                                painter.rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(hx0, y),
                                        egui::pos2(hx1, y + row_h),
                                    ),
                                    2.0,
                                    egui::Color32::from_rgba_unmultiplied(120, 120, 120, 56),
                                );
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
                                    painter.vline(
                                        cx,
                                        y..=(y + row_h),
                                        egui::Stroke::new(1.5, Palette::ACCENT),
                                    );
                                }
                            }
                        }
                        // 在主光标处上报 IME 输入区：激活输入法 + 定位候选框（否则虚拟编辑器无法输入中文）
                        if ed.vcaret >= ls && ed.vcaret <= le && in_win(col_of(ed.vcaret)) {
                            let cx = x_of(ed.vcaret - ls);
                            let irect = egui::Rect::from_min_size(
                                egui::pos2(cx, y),
                                egui::vec2(1.0, row_h),
                            );
                            ui.ctx().output_mut(|o| {
                                o.ime = Some(egui::output::IMEOutput {
                                    rect: irect,
                                    cursor_rect: irect,
                                })
                            });
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
                        painter.rect_stroke(
                            cap,
                            4.0,
                            egui::Stroke::new(1.0, Palette::BORDER),
                            egui::StrokeKind::Outside,
                        );
                        if ui
                            .interact(cap, text_id.with(("unfold", i)), egui::Sense::click())
                            .clicked()
                        {
                            fold_click = Some(i);
                        }
                    }
                    // 行号列固定在左侧：最后画（铺底盖住横向滚到下面的正文）+ 右对齐行号
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(clip.left(), y),
                            egui::pos2(clip.left() + gutter_w, y + row_h),
                        ),
                        0.0,
                        bg,
                    );
                    if is_first {
                        // 括号不匹配的行：行号标红（lint）
                        let num_col = if ed.lint_lines.contains(&i) {
                            Palette::DANGER
                        } else {
                            Palette::TEXT_DIM
                        };
                        painter.text(
                            egui::pos2(clip.left() + gutter_w - char_w * 2.0, y),
                            egui::Align2::RIGHT_TOP,
                            (i + 1).to_string(),
                            mono.clone(),
                            num_col,
                        );
                        // 折叠箭头（仅代码文件）：已折叠恒显 ▸（强调色）；可折叠仅悬停行号列时显 ▾（弱色）
                        let folded = folded_end.is_some();
                        if show_code_aids
                            && (folded || (gutter_hover && v_foldable(ed, i, unit_cols)))
                        {
                            let arect = egui::Rect::from_min_size(
                                egui::pos2(clip.left() + gutter_w - char_w * 1.8, y),
                                egui::vec2(char_w * 1.5, row_h),
                            );
                            if ui
                                .interact(arect, text_id.with(("fold", i)), egui::Sense::click())
                                .clicked()
                            {
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
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(33));
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
                    pos.x = pos.x.clamp(
                        clip.left() + gutter_w,
                        (clip.right() - 268.0).max(clip.left()),
                    );
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
                                        if ui
                                            .selectable_label(
                                                idx == *sel,
                                                egui::RichText::new(w).monospace().size(12.5),
                                            )
                                            .clicked()
                                        {
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
                painter.vline(
                    clip.left() + gutter_w - 3.0,
                    clip.top()..=clip.bottom(),
                    egui::Stroke::new(1.0, Palette::BORDER),
                );
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
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(clip.left(), y),
                        egui::pos2(clip.right(), y + row_h),
                    );
                    if ui
                        .interact(row_rect, text_id.with(("sticky", si)), egui::Sense::click())
                        .clicked()
                    {
                        ed.pending_scroll = Some(l);
                        ui.ctx().request_repaint();
                    }
                    painter.rect_filled(row_rect, 0.0, Palette::PANEL_2);
                    let (ls2, le2) = v_line_range(ed, l);
                    let line_full = &ed.content[ls2..le2];
                    let seg_b2 = char_to_byte(line_full, cols_vis);
                    let state = ed
                        .hl_states
                        .get(l)
                        .copied()
                        .unwrap_or(highlight::LineState::Normal);
                    let mut job = highlight::highlight_segment(
                        line_full,
                        0..seg_b2,
                        &lang,
                        fsize,
                        &[],
                        state,
                    );
                    job.wrap.max_width = f32::INFINITY;
                    let g = ui.ctx().fonts_mut(|f| f.layout_job(job));
                    painter.galley(egui::pos2(clip.left() + gutter_w, y), g, Palette::TEXT);
                    painter.text(
                        egui::pos2(clip.left() + gutter_w - char_w * 2.0, y),
                        egui::Align2::RIGHT_TOP,
                        (l + 1).to_string(),
                        mono.clone(),
                        Palette::TEXT_DIM,
                    );
                }
                if !sticky.is_empty() {
                    let by = clip.top() + sticky.len() as f32 * row_h;
                    painter.hline(
                        clip.left()..=clip.right(),
                        by + 0.5,
                        egui::Stroke::new(1.0, Palette::BORDER),
                    );
                }

                // 点击 / 双击 / 三击 / 拖拽定位光标与选区（行号 = top_line + 视口内行偏移）
                if !vsb_hit
                    && (resp.clicked()
                        || resp.drag_started()
                        || resp.dragged()
                        || resp.double_clicked()
                        || resp.triple_clicked())
                {
                    if resp.clicked()
                        || resp.drag_started()
                        || resp.double_clicked()
                        || resp.triple_clicked()
                    {
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
                            let g = ctx.fonts_mut(|f| {
                                f.layout_no_wrap(seg.clone(), mono.clone(), Palette::TEXT)
                            });
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
}
