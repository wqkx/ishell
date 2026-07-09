//! 左侧系统信息栏：主机概览、CPU/内存/交换、进程、网络、磁盘。紧凑布局。

mod history;
mod processes;
mod widgets;

use egui::{Rect, RichText, Vec2};

use crate::i18n::tr;
use crate::proto::SysInfo;
use crate::theme::Palette;
use crate::ui::{fmt_kb, fmt_rate, net_sparkline};

pub use history::NetHistory;
use processes::proc_table;
use widgets::{disk_row, gpu_meter, kv, kv_copy, meter_row, pct, rate_chip, section};

pub fn show(
    ui: &mut egui::Ui,
    info: Option<&SysInfo>,
    hist: &NetHistory,
    selected_nic: &mut String,
    sort_mem: &mut bool,
    proc_click: &mut Option<(u32, egui::Pos2)>,
    gpu_click: &mut Option<egui::Pos2>,
    // None=探测中；Some(false)=非 Linux / 无 /proc，监控不可用
    monitor_ok: Option<bool>,
) {
    use egui_phosphor::regular as icon;
    egui::ScrollArea::vertical()
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        // 仅用滚轮滚动：禁用「拖拽滚动」，否则内容溢出（窗口缩小）时滚动区会抢走
        // 背景层的右键按下事件，导致右键菜单时灵时不灵。
        .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
        .show(ui, |ui| {
            // 适度行距（与快速连接列表一致的舒展感）
            ui.spacing_mut().item_spacing.y = 3.5;
            if monitor_ok == Some(false) {
                crate::ui::empty_state(
                    ui,
                    icon::WARNING,
                    tr(
                        "系统监控仅支持 Linux 远端（需 /proc）",
                        "Monitor requires a Linux remote with /proc",
                    ),
                    false,
                );
                return;
            }
            let Some(info) = info else {
                crate::ui::empty_state(
                    ui,
                    icon::CLOCK,
                    tr("等待系统信息 …", "Waiting for system info …"),
                    true,
                );
                return;
            };

            // —— 主机概览 ——
            section(ui, icon::DESKTOP, tr("主机信息", "Host"));
            kv(ui, tr("主机名", "Name"), &info.hostname);
            if !info.ip.is_empty() {
                kv_copy(ui, "IP", &info.ip);
            }
            if !info.os.is_empty() {
                kv(ui, tr("系统", "OS"), &info.os);
            }
            kv(ui, tr("运行", "Up"), &info.uptime);
            ui.add_space(6.0);

            // —— CPU / 内存 / 交换：单行对齐，无小标题 ——
            // 百分比按 3 位补齐：等宽字体下数值跳动不引起宽度抖动
            meter_row(
                ui,
                "CPU",
                info.cpu_percent,
                &format!("{:>3.0}%", info.cpu_percent),
            );
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
                    format!(
                        "{}/{}",
                        fmt_kb(info.swap_used_kb),
                        fmt_kb(info.swap_total_kb)
                    )
                },
            );
            // GPU（如有）：总（平均）使用率，单击查看每块详情
            if !info.gpus.is_empty() {
                let avg = info.gpus.iter().map(|g| g.util).sum::<f32>() / info.gpus.len() as f32;
                let resp = gpu_meter(ui, avg, info.gpus.len());
                if resp.clicked() {
                    *gpu_click = resp
                        .interact_pointer_pos()
                        .or(Some(resp.rect.left_bottom()));
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
            // 网卡选择（单独一行，右对齐；压缩内边距，视觉更轻盈）
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tr("网卡", "NIC"))
                        .color(Palette::TEXT_DIM)
                        .size(12.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().button_padding = egui::vec2(8.0, 2.0);
                    let cur = if selected_nic.is_empty() {
                        tr("全部", "All").to_string()
                    } else {
                        selected_nic.clone()
                    };
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
            // 上下行速率：左右两枚等宽小卡，与折线图同风格；图标色即曲线颜色（兼作图例）
            {
                let (row, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 22.0),
                    egui::Sense::hover(),
                );
                let p = ui.painter_at(row);
                let gap = 4.0;
                let half = (row.width() - gap) / 2.0;
                let left = Rect::from_min_size(row.min, Vec2::new(half, row.height()));
                let right = Rect::from_min_size(
                    egui::pos2(row.min.x + half + gap, row.top()),
                    Vec2::new(half, row.height()),
                );
                rate_chip(&p, left, icon::ARROW_DOWN, Palette::NET_DOWN, &fmt_rate(rx));
                rate_chip(&p, right, icon::ARROW_UP, Palette::NET_UP, &fmt_rate(tx));
            }
            net_sparkline(
                ui,
                &hist.down_slice(),
                &hist.up_slice(),
                76.0,
                NetHistory::CAP,
            );
            ui.add_space(6.0);

            // —— 磁盘：每行一张轻卡片 ——
            section(ui, icon::HARD_DRIVES, tr("磁盘", "Disk"));
            // 显示 df 的真实可用空间（已扣除 root 预留块），而非 total-used。
            // 「可用/总量」按本机磁盘的最大字符宽补齐（等宽字体）：各行的 "/" 与数值右缘
            // 上下对齐，列宽随实际数据自适应，不为极端长度付出固定大留白。
            let avails: Vec<String> = info.disks.iter().map(|d| fmt_kb(d.avail_kb)).collect();
            let totals: Vec<String> = info.disks.iter().map(|d| fmt_kb(d.total_kb)).collect();
            let aw = avails.iter().map(|s| s.len()).max().unwrap_or(0);
            let tw = totals.iter().map(|s| s.len()).max().unwrap_or(0);
            for (i, d) in info.disks.iter().enumerate() {
                disk_row(
                    ui,
                    &d.mount,
                    d.percent,
                    &format!("{:>aw$}/{:>tw$}", avails[i], totals[i]),
                );
            }
        });
}
