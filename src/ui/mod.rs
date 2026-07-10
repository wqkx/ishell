//! UI 子模块：左侧系统信息栏、右下文件面板、连接对话框，以及通用绘制辅助。

pub mod connect;
pub mod docx;
pub mod editor;
pub mod file_panel;
pub mod highlight;
pub mod sidebar;

use egui::{Color32, Rect, Vec2};

use crate::theme::Palette;

/// 人性化字节单位（KB 入参为千字节时用 `fmt_kb`）。
pub fn fmt_bytes(bytes: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// 入参单位为 KB。
pub fn fmt_kb(kb: u64) -> String {
    fmt_bytes(kb as f64 * 1024.0)
}

/// 速率（字节/秒）。
pub fn fmt_rate(bps: f64) -> String {
    format!("{}/s", fmt_bytes(bps))
}

/// 空态提示：图标 + 灰字，统一各面板「无内容」的样式。
/// `centered`：面板级空态（垂直留白 + 居中竖排）；否则为列表内的一行横排。
pub fn empty_state(ui: &mut egui::Ui, icon: &str, text: &str, centered: bool) {
    if centered {
        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new(icon)
                    .color(Palette::TEXT_DIM)
                    .size(20.0),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(text)
                    .color(Palette::TEXT_DIM)
                    .size(12.0),
            );
        });
    } else {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(icon)
                    .color(Palette::TEXT_DIM)
                    .size(13.0),
            );
            ui.label(
                egui::RichText::new(text)
                    .color(Palette::TEXT_DIM)
                    .size(12.0),
            );
        });
    }
}

/// 根据使用率取色：低=绿、中=黄、高=红。
pub fn usage_color(percent: f32) -> Color32 {
    if percent >= 85.0 {
        Palette::DANGER
    } else if percent >= 60.0 {
        Palette::WARN
    } else {
        Palette::OK
    }
}

/// 绘制网络速率折线图（含刻度值与水平虚线网格）。
/// `slots` 为横轴总点位数（决定点间距/密度）；数据从右侧（最新）向左排布。
pub fn net_sparkline(ui: &mut egui::Ui, down: &[f64], up: &[f64], height: f32, slots: usize) {
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    // 卡片式画布：圆角 + 细边框，与全局控件风格一致；底边框即 0 基线。
    // 底色用暖奶油 CARD，贴合全局暖色调
    painter.rect_filled(rect, 4.0, Palette::CARD);
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, Palette::BORDER),
        egui::StrokeKind::Inside,
    );

    // 1px 横线对齐物理像素中心，避免高 DPI 下发虚
    let ppp = painter.ctx().pixels_per_point();
    let snap = |y: f32| ((y * ppp).floor() + 0.5) / ppp;

    // 绘图区：左右留白与底部基线留白相当；顶线贴住画布上沿（仅让出 1px 边框）
    let plot = Rect::from_min_max(
        egui::pos2(rect.left() + 6.0, snap(rect.top() + 1.0)),
        egui::pos2(rect.right() - 6.0, snap(rect.bottom() - 4.0)),
    );

    let raw_max = down
        .iter()
        .chain(up.iter())
        .cloned()
        .fold(0.0_f64, f64::max);
    let max = nice_ceiling(raw_max.max(1024.0)); // 至少 1KB，向上取整到“整”刻度

    // 三条水平虚线（frac = 1, 2/3, 1/3，把绘图区分三块），刻度值贴在各自线下方
    let grid_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0xc6, 0xc0, 0xb0));
    for frac in [1.0_f32, 2.0 / 3.0, 1.0 / 3.0] {
        let y = snap(plot.bottom() - plot.height() * frac);
        dashed_hline(&painter, plot.left(), plot.right(), y, grid_stroke);
        let val = max * frac as f64;
        painter.text(
            egui::pos2(plot.left() + 2.0, y + 2.0),
            egui::Align2::LEFT_TOP,
            fmt_rate_compact(val),
            egui::FontId::monospace(9.0),
            Palette::TEXT_DIM,
        );
    }

    let draw = |series: &[f64], color: Color32| {
        if series.len() < 2 {
            return;
        }
        let n = series.len();
        // 固定点间距（按总点位数），数据右对齐：最新点在最右，旧点向左延伸。
        // 这样点的密度始终正确，不会在数据未填满时被拉伸成稀疏。
        let step = plot.width() / (slots.saturating_sub(1).max(1) as f32);
        let pts: Vec<egui::Pos2> = series
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = plot.right() - (n - 1 - i) as f32 * step;
                let y = plot.bottom() - plot.height() * (*v / max).min(1.0) as f32;
                egui::pos2(x, y)
            })
            .collect();
        // 线下半透明填充（逐段四边形，避免凹多边形填充问题）
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40);
        for w in pts.windows(2) {
            let quad = vec![
                w[0],
                w[1],
                egui::pos2(w[1].x, plot.bottom()),
                egui::pos2(w[0].x, plot.bottom()),
            ];
            painter.add(egui::Shape::convex_polygon(quad, fill, egui::Stroke::NONE));
        }
        painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, color)));
    };
    draw(down, Palette::NET_DOWN); // 下载
    draw(up, Palette::NET_UP); // 上传
}

/// 紧凑速率（刻度用）：如 "60K"、"1.2M"。
fn fmt_rate_compact(bps: f64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bps;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 || i == 0 {
        format!("{:.0}{}", v, U[i])
    } else {
        format!("{:.1}{}", v, U[i])
    }
}

/// 把数值向上取整到 1/2/5×10^n 的“整”刻度。
fn nice_ceiling(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let exp = v.log10().floor();
    let base = 10f64.powf(exp);
    let m = v / base;
    let nice = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

/// 绘制一条水平虚线。
fn dashed_hline(painter: &egui::Painter, x0: f32, x1: f32, y: f32, stroke: egui::Stroke) {
    let (dash, gap) = (3.0, 3.0);
    let mut x = x0;
    while x < x1 {
        let xe = (x + dash).min(x1);
        painter.line_segment([egui::pos2(x, y), egui::pos2(xe, y)], stroke);
        x = xe + gap;
    }
}
