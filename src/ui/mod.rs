//! UI 子模块：左侧系统信息栏、右下文件面板、连接对话框，以及通用绘制辅助。

pub mod connect;
pub mod editor;
pub mod file_panel;
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

/// 绘制网络速率折线图（含左侧刻度值与水平虚线网格，仿 FinalShell）。
pub fn net_sparkline(ui: &mut egui::Ui, down: &[f64], up: &[f64], height: f32) {
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Palette::TRACK);

    // 左侧预留刻度标签区
    let label_w = 46.0;
    let plot = Rect::from_min_max(
        egui::pos2(rect.left() + label_w, rect.top() + 9.0),
        egui::pos2(rect.right() - 6.0, rect.bottom() - 6.0),
    );

    let raw_max = down.iter().chain(up.iter()).cloned().fold(0.0_f64, f64::max);
    let max = nice_ceiling(raw_max.max(1024.0)); // 至少 1KB，向上取整到“整”刻度

    // 水平虚线网格 + 左侧刻度值（顶/中/底，仿 FinalShell）
    let grid_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0xb6, 0xb0, 0xa0));
    for frac in [1.0_f32, 0.5, 0.0] {
        let y = plot.bottom() - plot.height() * frac;
        dashed_hline(&painter, plot.left(), plot.right(), y, grid_stroke);
        let val = max * frac as f64;
        // 刻度文字：紧凑、右对齐，离绘图区左边界留 8px（整体远离窗口边界）
        painter.text(
            egui::pos2(plot.left() - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            fmt_rate_compact(val),
            egui::FontId::proportional(9.0),
            Palette::TEXT_DIM,
        );
    }

    let draw = |series: &[f64], color: Color32| {
        if series.len() < 2 {
            return;
        }
        let n = series.len();
        let pts: Vec<egui::Pos2> = series
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = plot.left() + plot.width() * (i as f32 / (n - 1) as f32);
                let y = plot.bottom() - plot.height() * (*v / max).min(1.0) as f32;
                egui::pos2(x, y)
            })
            .collect();
        // 线下半透明填充（逐段四边形，避免凹多边形填充问题）
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 48);
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
    draw(down, Palette::OK); // 下载
    draw(up, Palette::ACCENT); // 上传
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
    let nice = if m <= 1.0 { 1.0 } else if m <= 2.0 { 2.0 } else if m <= 5.0 { 5.0 } else { 10.0 };
    nice * base
}

/// 绘制一条水平虚线。
fn dashed_hline(painter: &egui::Painter, x0: f32, x1: f32, y: f32, stroke: egui::Stroke) {
    let (dash, gap) = (4.0, 3.0);
    let mut x = x0;
    while x < x1 {
        let xe = (x + dash).min(x1);
        painter.line_segment([egui::pos2(x, y), egui::pos2(xe, y)], stroke);
        x = xe + gap;
    }
}
