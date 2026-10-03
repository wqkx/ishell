use egui::text::CCursor;

use super::super::geom::{byte_to_char, v_line_range};

/// 超过这个字节数的行不做语法高亮（VSCode 也有同样的上限）。
const LONG_LINE_PLAIN: usize = 20_000;
use super::gutter;
use super::{RowPaintContext, TextRowResult};
use crate::theme::Palette;
use crate::ui::highlight;

pub(super) fn paint_text_row(
    ctx: &mut RowPaintContext<'_>,
    row: usize,
    row_offset: usize,
) -> TextRowResult {
    // 视觉行 → 逻辑行 i + 要画的那段文本（换行模式：一个折段；非换行模式：横向可视窗口）。
    // 两种模式都经 v_line_of_vrow：折叠行占 0 视觉行，映射自动跳过。
    // 窗口按**显示宽度**取，两端落在字符边界上——见 `wrap::v_row_window`。
    if ctx.total == 0 {
        return TextRowResult::stop();
    }
    let win = super::super::wrap::v_row_window(
        ctx.ed,
        row,
        ctx.wrap,
        ctx.eff_cols,
        ctx.first_col,
        ctx.cols_vis,
    );
    let i = win.line;
    let is_first = win.first;
    let (seg_a, seg_b) = (win.a, win.b);
    let gx = ctx.text_x + win.x0 * ctx.char_w;
    let ncols = if ctx.wrap { ctx.wrap_cols } else { ctx.cols_vis };
    let (ls, le) = v_line_range(ctx.ed, i);
    let line_full: &str = &ctx.ed.content[ls..le]; // 切片，不整行拷贝
    let y = ctx.clip.top() + row_offset as f32 * ctx.row_h;
    // 位置 b（全文字节偏移）是否落在本段里。段的右端：换行模式下属于**下一段开头**，只有
    // 行末才算本段（否则上一段末尾和下一段开头各画一个光标）；非换行模式窗口两端都算。
    let wrapping = ctx.wrap;
    let to_end = win.to_end;
    let caret_here = move |b: usize| {
        let lb = b.saturating_sub(ls);
        b >= ls && lb >= seg_a && (lb < seg_b || (lb == seg_b && (!wrapping || to_end)))
    };
    // 当前行高亮（极淡）：聚焦且无选区时，给光标所在行铺一层很淡的底
    if ctx.focused && ctx.sels.is_empty() && i == ctx.caret_line {
        ctx.painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(ctx.clip.left(), y),
                egui::pos2(ctx.clip.right(), y + ctx.row_h),
            ),
            0.0,
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 10),
        );
    }
    // 缩进参考线（仅首段画、仅代码文件）：在各缩进层级之间画淡竖线；
    // 空白行取上下最近非空行缩进的较小值，使缩进线跨空行连续（同 VSCode）
    if is_first && ctx.show_code_aids {
        let lead_of = |l: usize| -> Option<usize> {
            let (a, b) = v_line_range(ctx.ed, l);
            let mut lead = 0usize;
            for c in ctx.ed.content[a..b].chars() {
                match c {
                    ' ' => lead += 1,
                    '\t' => lead += ctx.unit_cols,
                    _ => return Some(lead), // 有内容的行
                }
            }
            None // 空白行
        };
        let lead = match lead_of(i) {
            Some(l) => l,
            None => {
                let up = (0..i).rev().take(400).find_map(lead_of);
                let down = ((i + 1)..ctx.total).take(400).find_map(lead_of);
                match (up, down) {
                    (Some(a), Some(b)) => a.min(b),
                    _ => 0,
                }
            }
        };
        let mut col = ctx.unit_cols;
        while col < lead {
            let gx = ctx.text_x + col as f32 * ctx.char_w;
            // 光标所在块的那条竖线用强调色高亮（活动缩进线）
            let active = ctx
                .active_guide
                .is_some_and(|(ac, lo, hi)| col == ac && i >= lo && i <= hi);
            let stroke = if active {
                egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(
                        ctx.accent.r(),
                        ctx.accent.g(),
                        ctx.accent.b(),
                        150,
                    ),
                )
            } else {
                egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 30))
            };
            ctx.painter.vline(gx, y..=(y + ctx.row_h), stroke);
            col += ctx.unit_cols;
        }
        // 本行画线上限（lead）不及活动列时（块内/块尾空行），单独补画活动线保证贯穿
        if let Some((ac, lo, hi)) = ctx.active_guide {
            if i >= lo && i <= hi && ac >= lead && ac > 0 {
                ctx.painter.vline(
                    ctx.text_x + ac as f32 * ctx.char_w,
                    y..=(y + ctx.row_h),
                    egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(
                            ctx.accent.r(),
                            ctx.accent.g(),
                            ctx.accent.b(),
                            150,
                        ),
                    ),
                );
            }
        }
    }
    let seg = &line_full[seg_a..seg_b];
    let seg_x = gx;
    let seg_right = gx + ncols as f32 * ctx.char_w;
    // 括号 lint 下划线：把全文偏移的错误范围裁剪/平移成「本段内相对范围」传给高亮器
    let (seg_start, seg_end_abs) = (ls + seg_a, ls + seg_b);
    let lint_errs: Vec<std::ops::Range<usize>> = if ctx.ed.lint_ranges.is_empty() {
        Vec::new()
    } else {
        ctx.ed
            .lint_ranges
            .iter()
            .filter_map(|r| {
                let a = r.start.max(seg_start);
                let b = r.end.min(seg_end_abs);
                (a < b).then(|| (a - seg_start)..(b - seg_start))
            })
            .collect()
    };
    let galley = {
        let mut job = if line_full.len() > LONG_LINE_PLAIN {
            // 超长行不上色（见 `plain_segment`）
            highlight::plain_segment(seg, ctx.fsize)
        } else {
            // 整行分词（行首带跨行状态，docstring/块注释正确延续）、仅窗口布局
            let state = ctx
                .ed
                .hl_states
                .get(i)
                .copied()
                .unwrap_or(highlight::LineState::Normal);
            highlight::highlight_segment(
                line_full,
                seg_a..seg_b,
                ctx.lang,
                ctx.fsize,
                &lint_errs,
                state,
            )
        };
        job.wrap.max_width = f32::INFINITY;
        ctx.ui.ctx().fonts_mut(|f| f.layout_job(job))
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
    if let Some(wd) = ctx.occ_word {
        if line_full.len() <= 10_000 {
            let isw = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
            let bytes = line_full.as_bytes();
            let mut from = 0usize;
            while let Some(p) = line_full[from..].find(wd) {
                let s0 = from + p;
                let e0 = s0 + wd.len();
                from = e0;
                // 全词匹配 + 跳过选区本体
                if (s0 > 0 && isw(bytes[s0 - 1])) || (e0 < bytes.len() && isw(bytes[e0])) {
                    continue;
                }
                if ctx
                    .sels
                    .iter()
                    .any(|&(sa, sb)| sa == ls + s0 && sb == ls + e0)
                {
                    continue;
                }
                let ax = x_of(s0);
                let bx = x_of(e0);
                if bx > ax {
                    ctx.painter.rect_filled(
                        egui::Rect::from_min_max(egui::pos2(ax, y), egui::pos2(bx, y + ctx.row_h)),
                        2.0,
                        egui::Color32::from_rgba_unmultiplied(0x6b, 0x63, 0x50, 34),
                    );
                }
            }
        }
    }
    // 选区/查找当前项高亮：半透明珊瑚色（多选时画全部）
    for &(sa, sb) in ctx.sels {
        if sb > sa && sb > ls && sa <= le {
            let ax = x_of(sa.clamp(ls, le) - ls);
            // 只有当选区确实越过「本逻辑行末」——含换行符(sb>le，跨到下一行)，或本行还有内容
            // 折到下一视觉行(本段没覆盖到行末) → 才把高亮填到本段右缘；若选区恰好止于行末文本
            // (sb==le，未含换行符)，就画到实际文本末尾，不把行末空白一起涂上。
            let seg_end = ls + seg_b;
            let bx = if sb > le || (sb >= seg_end && seg_end < le) {
                seg_right
            } else {
                x_of(sb.clamp(ls, le) - ls)
            };
            if bx > ax {
                ctx.painter.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(ax, y), egui::pos2(bx, y + ctx.row_h)),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(
                        ctx.accent.r(),
                        ctx.accent.g(),
                        ctx.accent.b(),
                        60,
                    ),
                );
            }
        }
    }
    // 正文
    ctx.painter
        .galley(egui::pos2(seg_x, y), galley.clone(), Palette::TEXT);
    // 行尾是 `\r\n`（混合行尾的文件里原样保留）：在行末画一个淡色 CR，让这个看不见的
    // 字节可见。不用 U+240D「␍」——字体里不一定有这个字形，缺了就是一个方框。
    if to_end && super::super::geom::v_line_next(ctx.ed, i) == le + 2 {
        ctx.painter.text(
            egui::pos2(seg_x + galley.size().x + ctx.char_w * 0.3, y + ctx.row_h * 0.5),
            egui::Align2::LEFT_CENTER,
            "CR",
            egui::FontId::monospace(ctx.fsize * 0.7),
            egui::Color32::from_rgba_unmultiplied(
                Palette::TEXT_DIM.r(),
                Palette::TEXT_DIM.g(),
                Palette::TEXT_DIM.b(),
                110,
            ),
        );
    }
    // 括号匹配：给光标相邻括号及其匹配括号描边
    if let Some((ba, bb)) = ctx.brackets {
        for &bp in &[ba, bb] {
            if bp >= ls && bp < le && (seg_a..seg_b).contains(&(bp - ls)) {
                let bx0 = x_of(bp - ls);
                let bx1 = x_of(bp + 1 - ls);
                ctx.painter.rect_stroke(
                    egui::Rect::from_min_max(egui::pos2(bx0, y), egui::pos2(bx1, y + ctx.row_h)),
                    2.0,
                    egui::Stroke::new(1.0, Palette::ACCENT),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
    // 查找命中高亮（半透明灰），跳过「当前项」(=选区)避免叠灰盖字。
    for &(ma, mb) in ctx.vis_matches {
        if ctx.sels.contains(&(ma, mb)) {
            continue;
        }
        if ma < ls + seg_b && mb > ls + seg_a {
            let hx0 = x_of(ma.clamp(ls, le) - ls);
            let hx1 = x_of(mb.clamp(ls, le) - ls);
            if hx1 > hx0 {
                ctx.painter.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(hx0, y), egui::pos2(hx1, y + ctx.row_h)),
                    2.0,
                    egui::Color32::from_rgba_unmultiplied(120, 120, 120, 56),
                );
            }
        }
    }
    let mut caret_px_frame = None;
    // 光标（多选时每个选区末尾各画一个；闪烁约 530ms 亮/灭）
    if ctx.focused {
        let now = ctx.ui.input(|i| i.time);
        let blink_on = ((now - ctx.ed.caret_blink_at).rem_euclid(1.06)) < 0.53;
        if blink_on {
            for &cp in ctx.carets {
                if cp >= ls && cp <= le && caret_here(cp) {
                    let cx = x_of(cp - ls);
                    ctx.painter.vline(
                        cx,
                        y..=(y + ctx.row_h),
                        egui::Stroke::new(1.5, Palette::ACCENT),
                    );
                }
            }
        }
        // 主光标的屏幕坐标：补全弹窗定位 + IME 候选框定位都用它。
        //
        // 注意这里**只**记坐标，不再顺手上报 `o.ime`——上报改到了 paint/mod.rs 的行循环之后。
        // 原因见那里的注释：在这里报等于「光标可见才报」，光标滚出视口的那一帧 `o.ime` 变成
        // None，X11 上就是一次 XDestroyIC，滚回来又是 XCreateIC，都是同步的 XIM 往返。
        if ctx.ed.vcaret >= ls && ctx.ed.vcaret <= le && caret_here(ctx.ed.vcaret) {
            let cx = x_of(ctx.ed.vcaret - ls);
            caret_px_frame = Some(egui::pos2(cx, y + ctx.row_h));
        }
    }
    let folded_end = ctx.ed.folds.iter().find(|&&(h, _)| h == i).map(|&(_, e)| e);
    let mut fold_click = None;
    // 折叠 header：行尾画「⋯ N」胶囊提示（点击展开）
    // 换行模式下 header 行可能折成几段：胶囊只画在最后一段的行尾
    let last_seg = !ctx.wrap || seg_b >= line_full.len();
    if let Some(fe) = folded_end.filter(|_| last_seg) {
        let bx = seg_x + galley.size().x + 10.0;
        let label = format!("⋯ {}", fe - i);
        let tr = ctx.painter.text(
            egui::pos2(bx, y + ctx.row_h / 2.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::monospace((ctx.fsize * 0.85).max(9.0)),
            Palette::TEXT_DIM,
        );
        let cap = tr.expand2(egui::vec2(5.0, 1.5));
        ctx.painter.rect_stroke(
            cap,
            4.0,
            egui::Stroke::new(1.0, Palette::BORDER),
            egui::StrokeKind::Outside,
        );
        if ctx
            .ui
            .interact(cap, ctx.text_id.with(("unfold", i)), egui::Sense::click())
            .clicked()
        {
            fold_click = Some(i);
        }
    }
    // 行号列固定在左侧：最后画（铺底盖住横向滚到下面的正文）+ 右对齐行号
    fold_click =
        fold_click.or_else(|| gutter::paint_gutter_row(ctx, i, y, is_first, folded_end.is_some()));

    TextRowResult {
        stop: false,
        fold_click,
        caret_px: caret_px_frame,
    }
}
