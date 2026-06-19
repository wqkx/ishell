//! 左侧系统信息栏：主机概览、CPU/内存/交换、进程、网络、磁盘。紧凑布局。

use egui::{Color32, Rect, RichText, Vec2};

use crate::i18n::tr;
use crate::proto::SysInfo;
use crate::theme::Palette;
use crate::ui::{fmt_kb, fmt_rate, net_sparkline, usage_color};

/// 网络速率历史（用于折线图）。
#[derive(Default)]
pub struct NetHistory {
    pub down: std::collections::VecDeque<f64>,
    pub up: std::collections::VecDeque<f64>,
}

impl NetHistory {
    pub const CAP: usize = 120;
    pub fn push(&mut self, down: f64, up: f64) {
        self.down.push_back(down);
        self.up.push_back(up);
        while self.down.len() > Self::CAP {
            self.down.pop_front();
        }
        while self.up.len() > Self::CAP {
            self.up.pop_front();
        }
    }
    pub fn down_slice(&self) -> Vec<f64> {
        self.down.iter().cloned().collect()
    }
    pub fn up_slice(&self) -> Vec<f64> {
        self.up.iter().cloned().collect()
    }
}

pub fn show(
    ui: &mut egui::Ui,
    info: Option<&SysInfo>,
    hist: &NetHistory,
    selected_nic: &mut String,
    sort_mem: &mut bool,
    proc_click: &mut Option<(u32, egui::Pos2)>,
    gpu_click: &mut Option<egui::Pos2>,
) {
    use egui_phosphor::regular as icon;
    egui::ScrollArea::vertical()
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
        // 适度行距（与快速连接列表一致的舒展感）
        ui.spacing_mut().item_spacing.y = 3.5;
        let Some(info) = info else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(tr("等待系统信息 …", "Waiting for system info …")).color(Palette::TEXT_DIM));
            });
            return;
        };

        // —— 主机概览 ——
        section(ui, icon::DESKTOP, tr("主机信息", "Host"));
        kv(ui, tr("主机名", "Name"), &info.hostname);
        if !info.ip.is_empty() {
            kv(ui, "IP", &info.ip);
        }
        if !info.os.is_empty() {
            kv(ui, tr("系统", "OS"), &info.os);
        }
        kv(ui, tr("运行", "Up"), &info.uptime);
        ui.add_space(6.0);

        // —— CPU / 内存 / 交换：单行对齐，无小标题 ——
        meter_row(ui, "CPU", info.cpu_percent, &format!("{:.0}%", info.cpu_percent));
        meter_row(
            ui,
            tr("内存", "Mem"),
            pct(info.mem_used_kb, info.mem_total_kb),
            &format!("{}/{}", fmt_kb(info.mem_used_kb), fmt_kb(info.mem_total_kb)),
        );
        meter_row(
            ui,
            tr("交换", "Swap"),
            pct(info.swap_used_kb, info.swap_total_kb),
            &if info.swap_total_kb == 0 {
                tr("无", "N/A").to_string()
            } else {
                format!("{}/{}", fmt_kb(info.swap_used_kb), fmt_kb(info.swap_total_kb))
            },
        );
        // GPU（如有）：总（平均）使用率，单击查看每块详情
        if !info.gpus.is_empty() {
            let avg = info.gpus.iter().map(|g| g.util).sum::<f32>() / info.gpus.len() as f32;
            let resp = gpu_meter(ui, avg, info.gpus.len());
            if resp.clicked() {
                *gpu_click = resp.interact_pointer_pos().or(Some(resp.rect.left_bottom()));
            }
        }
        ui.add_space(6.0);

        // —— 进程（≤5，可按 CPU/内存 排序） ——
        section(ui, icon::LIST_BULLETS, tr("进程 Top", "Processes"));
        proc_table(ui, info, sort_mem, proc_click);
        ui.add_space(6.0);

        // —— 网络（可选网卡，显示实时上下行） ——
        section(ui, icon::WIFI_HIGH, tr("网络", "Network"));
        // 选中网卡的速率（空=全部之和）
        let (rx, tx) = if selected_nic.is_empty() {
            (info.net_rx_bps, info.net_tx_bps)
        } else {
            info.nets
                .iter()
                .find(|n| &n.name == selected_nic)
                .map(|n| (n.rx_bps, n.tx_bps))
                .unwrap_or((info.net_rx_bps, info.net_tx_bps))
        };
        // 网卡选择（单独一行，右对齐，按钮大小合理）
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("网卡", "NIC")).color(Palette::TEXT_DIM).size(12.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let cur = if selected_nic.is_empty() { tr("全部", "All").to_string() } else { selected_nic.clone() };
                egui::ComboBox::from_id_salt("nic_sel")
                    .selected_text(RichText::new(cur).size(12.0))
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(selected_nic, String::new(), tr("全部", "All"));
                        for n in &info.nets {
                            ui.selectable_value(selected_nic, n.name.clone(), &n.name);
                        }
                    });
            });
        });
        // 上下行速率（图标 + 文字，对齐）
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::ARROW_DOWN).color(Palette::OK).size(13.0));
            ui.label(RichText::new(fmt_rate(rx)).color(Palette::TEXT).size(12.0).monospace());
            ui.add_space(12.0);
            ui.label(RichText::new(icon::ARROW_UP).color(Palette::ACCENT).size(13.0));
            ui.label(RichText::new(fmt_rate(tx)).color(Palette::TEXT).size(12.0).monospace());
        });
        net_sparkline(ui, &hist.down_slice(), &hist.up_slice(), 76.0, NetHistory::CAP);
        ui.add_space(6.0);

        // —— 磁盘：每行一个，右起进度条 + 文字同行 ——
        section(ui, icon::HARD_DRIVES, tr("磁盘", "Disk"));
        for d in &info.disks {
            let avail = d.total_kb.saturating_sub(d.used_kb);
            disk_row(ui, &d.mount, d.percent, &format!("{}/{}", fmt_kb(avail), fmt_kb(d.total_kb)));
        }
    });
}

fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        used as f32 / total as f32 * 100.0
    }
}

fn section(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.add_space(5.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ui.label(RichText::new(icon).color(Palette::ACCENT).size(13.0));
        ui.label(RichText::new(title).color(Palette::TEXT).strong().size(12.5));
    });
    // 细分隔线（低调）
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, egui::Stroke::new(1.0, Palette::BORDER));
    ui.add_space(3.0);
}

fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(format!("{k}")).color(Palette::TEXT_DIM).size(12.0));
        ui.label(RichText::new(v).color(Palette::TEXT).size(12.0));
    });
}

/// 单行指标：固定宽标签 + 进度条（条上右对齐显示数值）。
fn meter_row(ui: &mut egui::Ui, label: &str, percent: f32, detail: &str) {
    let percent = percent.clamp(0.0, 100.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 19.0), egui::Sense::hover());
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
    p.rect_filled(bar, 0.0, Palette::TRACK);
    let mut fill = bar;
    fill.set_width((bar.width() * percent / 100.0).max(2.0));
    p.rect_filled(fill, 0.0, usage_color(percent));
    p.text(
        bar.right_center() - Vec2::new(5.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        detail,
        egui::FontId::monospace(11.0),
        Palette::TEXT,
    );
}

/// GPU 单行：与 meter_row 同款，但可单击（查看每块详情）。
fn gpu_meter(ui: &mut egui::Ui, percent: f32, count: usize) -> egui::Response {
    let percent = percent.clamp(0.0, 100.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 19.0), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect.expand2(Vec2::new(2.0, 0.0)), 3.0, Palette::PANEL_2);
    }
    let p = ui.painter_at(rect);
    let label_w = 34.0;
    p.text(rect.left_center() + Vec2::new(1.0, 0.0), egui::Align2::LEFT_CENTER, "GPU",
        egui::FontId::proportional(12.0), Palette::TEXT);
    let bar = Rect::from_min_max(rect.left_top() + Vec2::new(label_w, 2.0), rect.right_bottom() - Vec2::new(0.0, 2.0));
    p.rect_filled(bar, 0.0, Palette::TRACK);
    let mut fill = bar;
    fill.set_width((bar.width() * percent / 100.0).max(2.0));
    p.rect_filled(fill, 0.0, usage_color(percent));
    let detail = if count > 1 {
        match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("{count} 卡  {percent:.0}%"),
            crate::i18n::Lang::En => format!("x{count}  {percent:.0}%"),
        }
    } else {
        format!("{percent:.0}%")
    };
    p.text(bar.right_center() - Vec2::new(5.0, 0.0), egui::Align2::RIGHT_CENTER, detail,
        egui::FontId::monospace(11.0), Palette::TEXT);
    resp
}

/// 磁盘单行：浅色底，使用量从右侧填充；左挂载点、右「可用/总量」。
fn disk_row(ui: &mut egui::Ui, mount: &str, percent: f32, detail: &str) {
    let percent = percent.clamp(0.0, 100.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), egui::Sense::hover());
    let p = ui.painter_at(rect);

    p.rect_filled(rect, 0.0, Palette::TRACK);
    // 使用量：从右侧起填充（半透明，柔和不刺眼）
    let w = rect.width() * percent / 100.0;
    let used = Rect::from_min_max(egui::pos2(rect.right() - w, rect.top()), rect.right_bottom());
    let c = usage_color(percent);
    p.rect_filled(used, 0.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 90));

    p.text(
        rect.left_center() + Vec2::new(6.0, 0.0),
        egui::Align2::LEFT_CENTER,
        mount,
        egui::FontId::proportional(12.0),
        Palette::TEXT,
    );
    p.text(
        rect.right_center() - Vec2::new(6.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        detail,
        egui::FontId::monospace(11.0),
        Palette::TEXT_DIM,
    );
}

/// 进程表：最多 5 行；可点击 CPU%/内存% 表头切换排序；单击行查看详情。
fn proc_table(ui: &mut egui::Ui, info: &SysInfo, sort_mem: &mut bool, proc_click: &mut Option<(u32, egui::Pos2)>) {
    let pid_w = 46.0;
    let cpu_w = 44.0;
    let mem_w = 44.0;

    // 表头：PID | 名称 | CPU% | 内存%（CPU%/内存% 可点击排序）
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 15.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.text(rect.left_center(), egui::Align2::LEFT_CENTER, "PID",
            egui::FontId::proportional(10.5), Palette::TEXT_DIM);
        p.text(rect.left_center() + Vec2::new(pid_w, 0.0), egui::Align2::LEFT_CENTER, tr("名称", "Name"),
            egui::FontId::proportional(10.5), Palette::TEXT_DIM);
        let mem_rect = Rect::from_min_max(rect.right_top() - Vec2::new(mem_w, 0.0), rect.right_bottom());
        let cpu_rect = Rect::from_min_max(rect.right_top() - Vec2::new(mem_w + cpu_w, 0.0), egui::pos2(rect.right() - mem_w, rect.bottom()));
        let cpu_col = if !*sort_mem { Palette::ACCENT } else { Palette::TEXT_DIM };
        let mem_col = if *sort_mem { Palette::ACCENT } else { Palette::TEXT_DIM };
        p.text(cpu_rect.right_center(), egui::Align2::RIGHT_CENTER, "CPU%", egui::FontId::proportional(10.5), cpu_col);
        p.text(mem_rect.right_center(), egui::Align2::RIGHT_CENTER, tr("内存%", "Mem%"), egui::FontId::proportional(10.5), mem_col);
        if ui.interact(cpu_rect, ui.id().with("sort_cpu"), egui::Sense::click()).clicked() {
            *sort_mem = false;
        }
        if ui.interact(mem_rect, ui.id().with("sort_mem"), egui::Sense::click()).clicked() {
            *sort_mem = true;
        }
    });

    // 取所选排序键的前 5
    let mut procs: Vec<&crate::proto::ProcInfo> = info.procs.iter().collect();
    if *sort_mem {
        procs.sort_by(|a, b| b.mem.partial_cmp(&a.mem).unwrap_or(std::cmp::Ordering::Equal));
    } else {
        procs.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
    }

    for proc in procs.into_iter().take(5) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), egui::Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect.expand2(Vec2::new(2.0, 0.0)), 3.0, Palette::PANEL_2);
        }
        if resp.clicked() {
            *proc_click = Some((proc.pid, resp.interact_pointer_pos().unwrap_or(rect.right_center())));
        }
        let p = ui.painter_at(rect);
        p.text(rect.left_center(), egui::Align2::LEFT_CENTER, proc.pid.to_string(),
            egui::FontId::monospace(11.0), Palette::TEXT_DIM);
        // 内存%（最右）
        let mem_rect = Rect::from_min_max(rect.right_top() - Vec2::new(mem_w, 0.0), rect.right_bottom());
        p.text(mem_rect.right_center(), egui::Align2::RIGHT_CENTER, format!("{:.1}", proc.mem),
            egui::FontId::monospace(11.0), usage_color(proc.mem));
        // CPU%（中右）
        let cpu_rect = Rect::from_min_max(rect.right_top() - Vec2::new(mem_w + cpu_w, 0.0), egui::pos2(rect.right() - mem_w, rect.bottom()));
        p.text(cpu_rect.right_center(), egui::Align2::RIGHT_CENTER, format!("{:.1}", proc.cpu),
            egui::FontId::monospace(11.0), usage_color(proc.cpu));
        // 名称（中间，截断）
        let name_rect = Rect::from_min_max(
            rect.left_top() + Vec2::new(pid_w, 0.0),
            rect.right_bottom() - Vec2::new(cpu_w + mem_w, 0.0),
        );
        let name = truncate_to(proc.name.as_str(), name_rect.width());
        ui.painter_at(name_rect).text(
            name_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(11.0),
            Palette::TEXT,
        );
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
