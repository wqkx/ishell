mod gutter;
mod overlays;
mod text;

use super::super::Editor;
use super::chrome::ChromeActions;
use super::commands::bracket_match;
use super::edit::v_word_range;
use super::fold::v_lead;
use super::geom::{char_to_byte, v_line_of, v_line_range, v_sel_range};
use super::wrap::{v_line_of_vrow, v_total_vrows, v_vpos_of_byte, v_wrap_sync};
use crate::theme::Palette;
use crate::ui::highlight::{self, Indent};

/// 把 `r` 平移进 `bounds` 里（尺寸不变）。用于 IME 候选框定位：光标滚出视口时报一个视口外
/// 的位置，输入法的候选窗会飘到屏幕别处；钳住之后这个值在继续滚动时还是常量，顺带避免了
/// 「每帧一次 XSetICValues」。`r` 比 `bounds` 还大时以 `bounds` 左上角为准。
fn clamp_rect_into(r: egui::Rect, bounds: egui::Rect) -> egui::Rect {
    let x = r
        .min
        .x
        .clamp(bounds.min.x, (bounds.max.x - r.width()).max(bounds.min.x));
    let y = r
        .min
        .y
        .clamp(bounds.min.y, (bounds.max.y - r.height()).max(bounds.min.y));
    egui::Rect::from_min_size(egui::pos2(x, y), r.size())
}

pub(super) struct RowPaintContext<'a> {
    ui: &'a mut egui::Ui,
    ed: &'a mut Editor,
    painter: egui::Painter,
    text_id: egui::Id,
    clip: egui::Rect,
    text_x: f32,
    row_h: f32,
    char_w: f32,
    mono: &'a egui::FontId,
    bg: egui::Color32,
    focused: bool,
    lang: &'a str,
    fsize: f32,
    wrap: bool,
    wrap_cols: usize,
    first_col: usize,
    cols_vis: usize,
    total: usize,
    unit_cols: usize,
    show_code_aids: bool,
    caret_line: usize,
    active_guide: Option<(usize, usize, usize)>,
    brackets: Option<(usize, usize)>,
    vis_matches: &'a [(usize, usize)],
    occ_word: Option<&'a str>,
    sels: &'a [(usize, usize)],
    carets: &'a [usize],
    gutter_hover: bool,
    gutter_w: f32,
    accent: egui::Color32,
}

pub(super) struct TextRowResult {
    stop: bool,
    fold_click: Option<usize>,
    caret_px: Option<egui::Pos2>,
}

impl TextRowResult {
    fn stop() -> Self {
        Self {
            stop: true,
            fold_click: None,
            caret_px: None,
        }
    }
}

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
                let scrollbar = overlays::vertical_scrollbar(
                    ui, ed, text_id, clip, nrows, pad_rows, visible, max_top,
                );

                // 交互区取「可视视口」(clip)，但避开右缘滚动条条带（否则正文交互覆盖滚动条、抢走其拖动事件）。
                // 内层 ui 被 set_width(content_w) 限成内容宽度，若按 content_w 取交互区，短行右侧空白会落在区外、
                // 点击不到；用 clip（减去滚动条宽）覆盖视口，短行右侧空白也能点击定位到行末。
                let area = if scrollbar.show {
                    egui::Rect::from_min_max(
                        clip.min,
                        egui::pos2(clip.right() - 12.0, clip.bottom()),
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
                    let mut ctx = RowPaintContext {
                        ui,
                        ed,
                        painter: painter.clone(),
                        text_id,
                        clip,
                        text_x,
                        row_h,
                        char_w,
                        mono: &mono,
                        bg,
                        focused,
                        lang: &lang,
                        fsize,
                        wrap,
                        wrap_cols,
                        first_col,
                        cols_vis,
                        total,
                        unit_cols,
                        show_code_aids,
                        caret_line,
                        active_guide,
                        brackets,
                        vis_matches: &vis_matches,
                        occ_word: occ_word.as_deref(),
                        sels: &sels,
                        carets: &carets,
                        gutter_hover,
                        gutter_w,
                        accent,
                    };
                    let result = text::paint_text_row(&mut ctx, row, k);
                    if result.stop {
                        break;
                    }
                    fold_click = fold_click.or(result.fold_click);
                    caret_px_frame = caret_px_frame.or(result.caret_px);
                }
                // 应用折叠切换（下一帧重算视觉行映射）
                if let Some(l) = fold_click {
                    gutter::apply_fold_click(ui, ed, l, unit_cols);
                }
                // 聚焦时驱动光标闪烁（约 30fps 即可，不必每帧满速）
                if focused {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(33));
                }
                ed.caret_px = caret_px_frame;
                // ——— 上报 IME 输入区（激活输入法 + 定位候选框）———
                //
                // **只要编辑器聚焦就报**，光标滚出视口时把矩形钳进可见区，而不是像以前那样
                // 「光标可见才报」。差别不是美观问题，是会把整个程序冻住的：
                //
                // `o.ime` 为 None 的那一帧，egui-winit 会调 `set_ime_allowed(false)`；X11 上
                // winit 的实现是 **XDestroyIC**，再次为 Some 时是 **XCreateIC**。这两个都是
                // 同步的 XIM 往返——Xlib 发出请求后阻塞等输入法（fcitx）回复，**没有超时**。
                // 光标停在视口边缘上下滚动时，旧实现会每帧销毁+重建一次输入上下文；只要
                // fcitx 在某一次往返中途没了（重启、崩溃、远程桌面会话切换），事件循环线程
                // 就永远卡在那一次调用里，整个界面冻住——正是用户报的「用 fcitx 打字/删除时
                // 程序卡死」。聚焦期间恒定上报，XIC 只建一次，往返次数降到「光标真的移动了」
                // 那几次。
                //
                // 钳进 `clip`：光标滚出视口时不能把候选框定位到视口外（会飞到屏幕别处），
                // 而且钳住之后这个值在继续滚动时是**不变**的，连带把「滚动时每帧一次
                // XSetICValues」也消掉了。
                if focused {
                    let caret_rect = caret_px_frame
                        .map(|p| {
                            egui::Rect::from_min_size(
                                egui::pos2(p.x, p.y - row_h),
                                egui::vec2(1.0, row_h),
                            )
                        })
                        .unwrap_or_else(|| {
                            egui::Rect::from_min_size(clip.min, egui::vec2(1.0, row_h))
                        });
                    let irect = clamp_rect_into(caret_rect, clip);
                    ui.ctx().output_mut(|o| {
                        o.ime = Some(egui::output::IMEOutput {
                            rect: irect,
                            cursor_rect: irect,
                        })
                    });
                }
                overlays::paint_completion_popup(ui, ed, text_id, clip, gutter_w, char_w, row_h);
                // 行号分割线（固定在左侧行号列右缘）
                gutter::paint_gutter_separator(&painter, clip, gutter_w);
                // 自绘竖向滚动条滑块：正文之后再画，确保浮在字形之上、不被盖住
                overlays::paint_scrollbar_thumb(&painter, scrollbar.thumb);
                // ——— 粘性作用域行（sticky scroll）———
                overlays::paint_sticky_scope(
                    ui,
                    ed,
                    &painter,
                    text_id,
                    clip,
                    top_row,
                    first_line,
                    row_h,
                    char_w,
                    &mono,
                    gutter_w,
                    unit_cols,
                    cols_vis,
                    &lang,
                    fsize,
                    show_code_aids,
                );

                // 点击 / 双击 / 三击 / 拖拽定位光标与选区（行号 = top_line + 视口内行偏移）
                if !scrollbar.hit
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
                        // include_nl：仅拖拽选择时为真——指针超出行末文本时把换行符也纳入（= 选中整行）。
                        // 普通点击必须为假，否则点击行末空白会落到「下一行首字符」（le+1 = 下一行起点）。
                        let byte_at = |p: egui::Pos2, include_nl: bool| -> usize {
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
                            // 行末选择：指针超出「行末文本右缘」1 个字符宽以上时，才把换行符也选进去
                            // （= 选中整行）；否则止于行末文本，不把换行符/行末空白卷进来。仅当本段
                            // 覆盖到行末、且不是最后一行（有换行符可选）时适用。
                            if include_nl
                                && seg_b >= line_full.len()
                                && li + 1 < ed.vlines.len()
                                && p.x - gx > g.size().x + char_w
                            {
                                le + 1
                            } else {
                                ls + seg_a + char_to_byte(&seg, cc)
                            }
                        };
                        // 拖拽（含拖拽起始）时才允许把换行符选进来；普通/双击/三击点击不含换行符
                        let allow_nl = resp.drag_started() || resp.dragged();
                        let b = byte_at(pos, allow_nl);
                        // 拖拽需移动超过阈值才激活，此刻指针已离开按下点——锚点必须用「按下位置」，
                        // 否则起始字符会被漏选（从左往右拖丢第一个字，从右往左拖丢按下处字符）
                        let ob = if resp.drag_started() {
                            ui.input(|i| i.pointer.press_origin())
                                .map(|p| byte_at(p, true))
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

#[cfg(test)]
mod ime_report_tests {
    use super::*;

    /// 编辑器聚焦时 `o.ime` 必须**恒定上报**，与光标是否滚出视口无关。
    ///
    /// 这不是美观问题，是会把整个程序冻住的那类：`o.ime` 为 `None` 的那一帧，egui-winit 会
    /// 调 `set_ime_allowed(false)`，而 winit 在 X11 上的实现是 **XDestroyIC**；下一帧又变回
    /// `Some` 就是 **XCreateIC**。两者都是同步的 XIM 往返——Xlib 把请求发给输入法（fcitx）
    /// 之后阻塞等回复，**没有超时**。旧实现是「光标可见才报」，于是光标停在视口边缘上下
    /// 滚动时，每一帧都销毁并重建一次输入上下文；只要 fcitx 在其中某一次往返中途没了
    /// （崩溃、重启、远程桌面会话切换），事件循环线程就永远停在那一次调用里，整个界面冻住。
    /// 这正是用户报的「远程桌面下用 fcitx 打字/删除时 iShell 卡死」。
    ///
    /// 反向对照：把 `paint_visible_rows` 里那段上报改回「光标可见才报」，第三帧就会是 false。
    #[test]
    fn ime_is_reported_whenever_focused_even_with_the_caret_scrolled_away() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let content: String = (0..400).map(|i| format!("line {i}\n")).collect();
        let mut ed = Editor::new("/tmp/ime_probe.txt".into(), content);
        // 行索引是懒建的（`Editor::new` 只存内容），不先算一次会在取行时下标越界。
        super::super::wrap::v_recompute(&mut ed);

        let mut reported = Vec::new();
        // 光标始终在第 0 行；视口滚到很远再滚回来。第一帧顺带让字体/样式生效。
        for top in [0usize, 0, 300, 0] {
            ed.vtop = top;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 400.0),
                )),
                ..Default::default()
            };
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut actions = ChromeActions::default();
                    paint_visible_rows(
                        ui,
                        &mut ed,
                        egui::Id::new("ime_probe"),
                        16.0,
                        8.0,
                        egui::FontId::monospace(14.0),
                        Palette::BG,
                        true, // focused
                        false,
                        "txt".into(),
                        14.0,
                        &mut actions,
                    );
                });
            });
            reported.push(out.platform_output.ime.is_some());
        }
        assert_eq!(
            reported,
            vec![true; 4],
            "聚焦期间有某一帧没有上报 o.ime（第 3 帧是光标滚出视口）：\
             那一帧 egui-winit 会 set_ime_allowed(false) → X11 上 XDestroyIC，\
             滚回来又 XCreateIC，都是没有超时的同步 XIM 往返，fcitx 一抽风就整个界面冻住"
        );
    }

    /// 上报的矩形必须落在可见区内：光标滚出视口时报一个视口外的位置，输入法候选窗会飘到
    /// 屏幕别处；而且那个值在继续滚动时每帧都变，每次变化都是一次 `XSetICValues`（同样是
    /// 同步 XIM 往返）。钳住之后它在光标出视口后是常量，滚动期间不再产生任何 XIM 往返。
    #[test]
    fn clamped_rect_stays_inside_and_stops_moving_once_out_of_view() {
        let bounds = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(100.0, 50.0));
        // 远在上方 / 远在下方 / 远在右侧，都必须落回 bounds 内
        for r in [
            egui::Rect::from_min_size(egui::pos2(-500.0, -900.0), egui::vec2(1.0, 16.0)),
            egui::Rect::from_min_size(egui::pos2(5000.0, 9000.0), egui::vec2(1.0, 16.0)),
            egui::Rect::from_min_size(egui::pos2(400.0, 30.0), egui::vec2(1.0, 16.0)),
        ] {
            let c = clamp_rect_into(r, bounds);
            assert!(bounds.contains_rect(c), "{c:?} 没落在 {bounds:?} 里");
            assert_eq!(c.size(), r.size(), "钳位不该改变尺寸");
        }
        // 关键性质：光标越滚越远时，钳位结果不再变化——滚动期间零 XIM 往返。
        let far1 = clamp_rect_into(
            egui::Rect::from_min_size(egui::pos2(0.0, -1000.0), egui::vec2(1.0, 16.0)),
            bounds,
        );
        let far2 = clamp_rect_into(
            egui::Rect::from_min_size(egui::pos2(0.0, -9999.0), egui::vec2(1.0, 16.0)),
            bounds,
        );
        assert_eq!(far1, far2, "越滚越远时钳位结果还在变，等于每帧一次 XSetICValues");
    }

    /// 矩形比可见区还大时也不能产生 NaN / 反向区间（窗口被拖到极小的退化情形）。
    #[test]
    fn degenerate_bounds_do_not_produce_garbage() {
        let tiny = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1.0, 1.0));
        let big = egui::Rect::from_min_size(egui::pos2(50.0, 50.0), egui::vec2(200.0, 200.0));
        let c = clamp_rect_into(big, tiny);
        assert!(c.min.x.is_finite() && c.min.y.is_finite());
        assert_eq!(c.min, tiny.min, "放不下时以可见区左上角为准");
    }
}
