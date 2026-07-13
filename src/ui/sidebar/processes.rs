//! 进程表渲染。

use egui::{Color32, Rect, Vec2};

use crate::i18n::tr;
use crate::proto::SysInfo;
use crate::theme::Palette;
use crate::ui::usage_color;

/// 进程表：最多 5 行；可点击 CPU%/内存% 表头切换排序；单击行查看详情。
pub(super) fn proc_table(
    ui: &mut egui::Ui,
    info: &SysInfo,
    sort_mem: &mut bool,
    proc_click: &mut Option<(u32, egui::Pos2)>,
) {
    let pid_w = 60.0; // 容纳较长 PID（约 +2 个字符）
    let cpu_w = 44.0;
    let mem_w = 44.0;

    // 表头：PID | 名称 | CPU% | 内存%（CPU%/内存% 可点击排序）；轻底色带与卡片风格呼应
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 3.0, Palette::CARD);
        p.text(
            rect.left_center() + Vec2::new(4.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "PID",
            egui::FontId::proportional(10.5),
            Palette::TEXT_DIM,
        );
        p.text(
            rect.left_center() + Vec2::new(pid_w, 0.0),
            egui::Align2::LEFT_CENTER,
            tr("名称", "Name"),
            egui::FontId::proportional(10.5),
            Palette::TEXT_DIM,
        );
        let mem_rect = Rect::from_min_max(
            rect.right_top() - Vec2::new(mem_w, 0.0),
            rect.right_bottom(),
        );
        let cpu_rect = Rect::from_min_max(
            rect.right_top() - Vec2::new(mem_w + cpu_w, 0.0),
            egui::pos2(rect.right() - mem_w, rect.bottom()),
        );
        let cpu_col = if !*sort_mem {
            Palette::ACCENT
        } else {
            Palette::TEXT_DIM
        };
        let mem_col = if *sort_mem {
            Palette::ACCENT
        } else {
            Palette::TEXT_DIM
        };
        p.text(
            cpu_rect.right_center(),
            egui::Align2::RIGHT_CENTER,
            "CPU%",
            egui::FontId::proportional(10.5),
            cpu_col,
        );
        p.text(
            mem_rect.right_center() - Vec2::new(4.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            tr("内存%", "Mem%"),
            egui::FontId::proportional(10.5),
            mem_col,
        );
        if ui
            .interact(cpu_rect, ui.id().with("sort_cpu"), egui::Sense::click())
            .clicked()
        {
            *sort_mem = false;
        }
        if ui
            .interact(mem_rect, ui.id().with("sort_mem"), egui::Sense::click())
            .clicked()
        {
            *sort_mem = true;
        }
    });

    // 取所选排序键的前 5
    let mut procs: Vec<&crate::proto::ProcInfo> = info.procs.iter().collect();
    if *sort_mem {
        procs.sort_by(|a, b| {
            b.mem
                .partial_cmp(&a.mem)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        procs.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    for proc in procs.into_iter().take(5) {
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), egui::Sense::click());
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect.expand2(Vec2::new(2.0, 0.0)), 3.0, Palette::PANEL_2);
        }
        if resp.clicked() {
            *proc_click = Some((
                proc.pid,
                resp.interact_pointer_pos().unwrap_or(rect.right_center()),
            ));
        }
        crate::app::view_context_menu(&resp);
        let p = ui.painter_at(rect);
        p.text(
            rect.left_center() + Vec2::new(4.0, 0.0),
            egui::Align2::LEFT_CENTER,
            proc.pid.to_string(),
            egui::FontId::proportional(11.0),
            Palette::TEXT_DIM,
        );
        // 内存%（最右）：等宽防抖；正常值安静灰、偏高才用语义警示色
        let mem_rect = Rect::from_min_max(
            rect.right_top() - Vec2::new(mem_w, 0.0),
            rect.right_bottom(),
        );
        p.text(
            mem_rect.right_center() - Vec2::new(4.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            format!("{:.1}", proc.mem),
            egui::FontId::monospace(11.0),
            proc_num_color(proc.mem),
        );
        // CPU%（中右）：可超 100%（多核），大值去小数以适配列宽
        let cpu_rect = Rect::from_min_max(
            rect.right_top() - Vec2::new(mem_w + cpu_w, 0.0),
            egui::pos2(rect.right() - mem_w, rect.bottom()),
        );
        let cpu_txt = if proc.cpu >= 100.0 {
            format!("{:.0}", proc.cpu)
        } else {
            format!("{:.1}", proc.cpu)
        };
        p.text(
            cpu_rect.right_center(),
            egui::Align2::RIGHT_CENTER,
            cpu_txt,
            egui::FontId::monospace(11.0),
            proc_num_color(proc.cpu),
        );
        // 名称（中间，截断）
        let name_rect = Rect::from_min_max(
            rect.left_top() + Vec2::new(pid_w, 0.0),
            rect.right_bottom() - Vec2::new(cpu_w + mem_w, 0.0),
        );
        let name = truncate_to(proc.name.as_str(), name_rect.width());
        // 名称被截断时悬停显示完整命令名
        if name.ends_with('…') {
            resp.on_hover_text(&proc.name);
        }
        ui.painter_at(name_rect).text(
            name_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(11.0),
            Palette::TEXT,
        );
    }
}

/// 进程数值取色：正常时安静（暗灰），偏高（≥60%）才用语义警示色，避免满屏彩色小字。
fn proc_num_color(pct: f32) -> Color32 {
    if pct >= 60.0 {
        usage_color(pct)
    } else {
        Palette::TEXT_DIM
    }
}

/// 粗略按宽度截断名称（每字符约 6.2px）。
fn truncate_to(s: &str, width: f32) -> String {
    let max = (width / 6.2).floor() as usize;
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    }
}
