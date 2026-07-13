//! 小型指标、标签与卡片绘制辅助。

use egui::{Color32, Rect, RichText, Vec2};

use crate::i18n::tr;
use crate::theme::Palette;
use crate::ui::usage_color;

pub(super) fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        used as f32 / total as f32 * 100.0
    }
}

pub(super) fn section(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ui.label(RichText::new(icon).color(Palette::ACCENT).size(13.0));
        ui.label(
            RichText::new(title)
                .color(Palette::TEXT)
                .strong()
                .size(12.5),
        );
    });
    // 细分隔线（低调）
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, Palette::BORDER),
    );
    ui.add_space(3.0);
}

pub(super) fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(
            RichText::new(k.to_string())
                .color(Palette::TEXT_DIM)
                .size(12.0),
        );
        ui.label(RichText::new(v).color(Palette::TEXT).size(12.0));
    });
}

/// 同 kv，但值可双击复制到剪贴板（用于 IP 等），复制后短暂显示「已复制」。
pub(super) fn kv_copy(ui: &mut egui::Ui, k: &str, v: &str) {
    let id = ui.id().with(("kv_copy", k));
    let now = ui.input(|i| i.time);
    let flash: Option<f64> = ui.ctx().data(|d| d.get_temp(id));
    let copied = flash.is_some_and(|t| now - t < 1.2);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(k).color(Palette::TEXT_DIM).size(12.0));
        let resp = ui
            .add(
                egui::Label::new(RichText::new(v).color(Palette::TEXT).size(12.0))
                    .sense(egui::Sense::click()),
            )
            .on_hover_text(tr("双击复制", "Double-click to copy"));
        crate::app::view_context_menu(&resp);
        if resp.double_clicked() {
            ui.ctx().copy_text(v.to_string());
            ui.ctx().data_mut(|d| d.insert_temp(id, now));
        }
        if copied {
            ui.label(
                RichText::new(format!(
                    "{}  {}",
                    egui_phosphor::regular::CHECK,
                    tr("已复制", "Copied")
                ))
                .color(Palette::OK)
                .size(11.0),
            );
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(150));
        }
    });
}

/// 单行指标：固定宽标签 + 进度条（条上右对齐显示数值）。
pub(super) fn meter_row(ui: &mut egui::Ui, label: &str, percent: f32, detail: &str) {
    let percent = percent.clamp(0.0, 100.0);
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 19.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    // 标签很短（CPU / 内存 / 交换），进度条紧贴文字右侧
    let label_w = 34.0;

    p.text(
        rect.left_center() + Vec2::new(1.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.0),
        Palette::TEXT,
    );

    let bar = Rect::from_min_max(
        rect.left_top() + Vec2::new(label_w, 2.0),
        rect.right_bottom() - Vec2::new(0.0, 2.0),
    );
    // 关键指标要醒目：暖灰轨道 + 近实色语义填充（磁盘那种淡染只适合背景信息）
    p.rect_filled(bar, 2.0, Palette::TRACK);
    let mut fill = bar;
    fill.set_width((bar.width() * percent / 100.0).max(3.0));
    let c = usage_color(percent);
    p.rect_filled(
        fill,
        2.0,
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 190),
    );
    p.text(
        bar.right_center() - Vec2::new(6.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        detail,
        egui::FontId::monospace(11.0),
        Palette::TEXT,
    );
}

/// GPU 单行：与 meter_row 同款，但可单击（查看每块详情）。
pub(super) fn gpu_meter(ui: &mut egui::Ui, percent: f32, count: usize) -> egui::Response {
    let percent = percent.clamp(0.0, 100.0);
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 19.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect.expand2(Vec2::new(2.0, 0.0)), 3.0, Palette::PANEL_2);
    }
    let p = ui.painter_at(rect);
    let label_w = 34.0;
    p.text(
        rect.left_center() + Vec2::new(1.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "GPU",
        egui::FontId::proportional(12.0),
        Palette::TEXT,
    );
    let bar = Rect::from_min_max(
        rect.left_top() + Vec2::new(label_w, 2.0),
        rect.right_bottom() - Vec2::new(0.0, 2.0),
    );
    // 与 meter_row 同款：暖灰轨道 + 近实色填充
    p.rect_filled(bar, 2.0, Palette::TRACK);
    let mut fill = bar;
    fill.set_width((bar.width() * percent / 100.0).max(3.0));
    let c = usage_color(percent);
    p.rect_filled(
        fill,
        2.0,
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 190),
    );
    let detail = if count > 1 {
        match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("{count} 卡  {percent:>3.0}%"),
            crate::i18n::Lang::En => format!("x{count}  {percent:>3.0}%"),
        }
    } else {
        format!("{percent:>3.0}%")
    };
    p.text(
        bar.right_center() - Vec2::new(5.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        detail,
        egui::FontId::monospace(11.0),
        Palette::TEXT,
    );
    crate::app::view_context_menu(&resp);
    resp
}

/// 上/下行速率小卡：与折线图画布同风格（圆角 + 细边框），左图标（曲线同色）、右等宽速率值。
pub(super) fn rate_chip(p: &egui::Painter, rect: Rect, icon: &str, color: Color32, rate: &str) {
    p.rect_filled(rect, 4.0, Palette::CARD);
    p.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, Palette::BORDER),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.left_center() + Vec2::new(7.0, 0.0),
        egui::Align2::LEFT_CENTER,
        icon,
        egui::FontId::proportional(12.0),
        color,
    );
    p.text(
        rect.right_center() - Vec2::new(7.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        rate,
        egui::FontId::monospace(11.0),
        Palette::TEXT,
    );
}

/// 磁盘单行：与速率小卡/折线图同一卡片语言——骨白底 + 细边框，
/// 使用量为从左铺入的极淡色染，文字浮于其上（磁盘多时依然轻盈统一）。
pub(super) fn disk_row(ui: &mut egui::Ui, mount: &str, percent: f32, detail: &str) {
    let percent = percent.clamp(0.0, 100.0);
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), egui::Sense::hover());
    // 悬停显示完整挂载点与使用率（长路径被截断时仍可读全）
    resp.on_hover_text(format!("{mount}\n{} {percent:.0}%", tr("已用", "Used")));
    let p = ui.painter_at(rect);

    p.rect_filled(rect, 4.0, Palette::CARD);
    // 使用量：从右侧淡染（与原版方向一致；低透明度，仅作暗示，不压文字）
    let c = usage_color(percent);
    let w = (rect.width() * percent / 100.0).max(3.0);
    let fill = Rect::from_min_max(
        egui::pos2(rect.right() - w, rect.top()),
        rect.right_bottom(),
    );
    p.rect_filled(
        fill,
        4.0,
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 36),
    );
    p.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, Palette::BORDER),
        egui::StrokeKind::Inside,
    );

    p.text(
        rect.left_center() + Vec2::new(7.0, 0.0),
        egui::Align2::LEFT_CENTER,
        mount,
        egui::FontId::proportional(12.0),
        Palette::TEXT,
    );
    // 用整数字号（非整数字号行高取整会让文字视觉偏上），与挂载点同基线居中
    p.text(
        rect.right_center() - Vec2::new(7.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        detail,
        egui::FontId::monospace(11.0),
        Palette::TEXT_DIM,
    );
}
