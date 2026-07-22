//! 通用 UI 小部件：可拖拽标签条、扁平按钮、对话框骨架、监控栏右键菜单、关于窗。

use egui::{RichText, Sense};

use crate::theme::Palette;

use super::util::edge_fade;
#[allow(unused_imports)]
use super::view_state::*;

pub fn view_context_menu(resp: &egui::Response) {
    resp.context_menu(|ui| {
        // 菜单项不换行，避免较长英文项折行
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);

        // —— 语言 ——
        ui.label(
            RichText::new(crate::i18n::tr("语言", "Language"))
                .color(Palette::TEXT_DIM)
                .size(11.0),
        );
        crate::i18n::language_menu(ui);
        ui.separator();

        // —— 字体大小（全局界面缩放）——
        ui.label(
            RichText::new(format!(
                "{}  {:.0}%",
                crate::i18n::tr("字体大小", "Font size"),
                ui_zoom() * 100.0
            ))
            .color(Palette::TEXT_DIM)
            .size(11.0),
        );
        ui.horizontal(|ui| {
            // +/- 不关闭菜单，便于连续调整；百分比实时更新
            if ui
                .button(RichText::new(egui_phosphor::regular::MINUS).size(13.0))
                .clicked()
            {
                set_ui_zoom(ui_zoom() - 0.1);
            }
            if ui
                .button(RichText::new(egui_phosphor::regular::PLUS).size(13.0))
                .clicked()
            {
                set_ui_zoom(ui_zoom() + 0.1);
            }
            if ui.button(crate::i18n::tr("复位", "Reset")).clicked() {
                set_ui_zoom(1.0);
            }
        });
        ui.separator();

        // —— 视图折叠 ——
        let s_label = if sidebar_collapsed() {
            format!(
                "{}  {}",
                egui_phosphor::regular::SIDEBAR_SIMPLE,
                crate::i18n::tr("显示系统监控栏", "Show monitor sidebar")
            )
        } else {
            format!(
                "{}  {}",
                egui_phosphor::regular::SIDEBAR_SIMPLE,
                crate::i18n::tr("隐藏系统监控栏", "Hide monitor sidebar")
            )
        };
        if ui.button(s_label).clicked() {
            set_sidebar_collapsed(!sidebar_collapsed());
            ui.close();
        }
        let f_label = if files_collapsed() {
            format!(
                "{}  {}",
                egui_phosphor::regular::TREE_VIEW,
                crate::i18n::tr("显示文件栏", "Show file panel")
            )
        } else {
            format!(
                "{}  {}",
                egui_phosphor::regular::TREE_VIEW,
                crate::i18n::tr("隐藏文件栏", "Hide file panel")
            )
        };
        if ui.button(f_label).clicked() {
            set_files_collapsed(!files_collapsed());
            ui.close();
        }

        // —— 强制 X11（仅 Linux；修复 Wayland 下输入法）——
        #[cfg(target_os = "linux")]
        {
            ui.separator();
            let mut fx = crate::store::load_force_x11();
            if ui
                .checkbox(
                    &mut fx,
                    crate::i18n::tr(
                        "强制 X11（修复输入法·重启生效）",
                        "Force X11 (fix IME · restart)",
                    ),
                )
                .on_hover_text(crate::i18n::tr(
                    "Wayland 下输入法常失效；开启后下次启动改走 X11",
                    "IME often fails on Wayland; enabling switches to X11 on next launch",
                ))
                .clicked()
            {
                crate::store::save_force_x11(fx);
                ui.close();
            }
        }

        // —— AI/MCP 控制（本地 socket，供 AI 助手驱动终端·重启生效）——
        ui.separator();
        let mut mcp_on = crate::store::load_mcp_consent();
        if ui
            .checkbox(
                &mut mcp_on,
                crate::i18n::tr(
                    "允许 AI 通过 MCP 控制终端（重启生效）",
                    "Allow AI to control terminal via MCP (restart)",
                ),
            )
            .on_hover_text(crate::i18n::tr(
                "开启后本机可通过本地 MCP server 驱动已打开的终端会话（发命令、读输出）；\
                 本身不监听网络端口，但会把这个控制通道反向转发到你连接的每一台服务器\
                 （复用该 SSH 连接）——谁能 SSH 到那台服务器，谁就能借此控制本机 iShell，\
                 只对可信的服务器开启",
                "Lets an AI client drive your open terminal sessions via a local MCP server \
                 (run commands, read output); doesn't open a network port itself, but reverse-\
                 forwards this control channel to every server you connect to (over that SSH \
                 session) — anyone who can SSH into that server can control this iShell through \
                 it, so only enable this for servers you trust",
            ))
            .clicked()
        {
            crate::store::save_mcp_consent(mcp_on);
            ui.close();
        }

        // 多机配对 token：多台电脑共用同一台 AI 服务器账号时，各家 iShell 反向转发的 socket
        // 会堆在一起、代理无从区分谁是谁。把这个 token 填进各自 Claude Code 的 MCP server
        // `env`（ISHELL_MCP_TOKEN），代理便只绑定 token 匹配的这台 iShell，请求不会串到别人
        // 电脑上（见 store::mcp_pairing_token）。只在开启 MCP 后才有意义，故随开关一起显示。
        if mcp_on {
            let token = crate::store::mcp_pairing_token();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(crate::i18n::tr("配对 token：", "Pairing token: "))
                        .color(Palette::TEXT_DIM)
                        .size(12.0),
                );
                ui.label(RichText::new(&token).monospace().size(12.0));
            });
            if ui
                .button(format!(
                    "{}  {}",
                    egui_phosphor::regular::COPY,
                    crate::i18n::tr("复制多机配对配置", "Copy pairing config")
                ))
                .on_hover_text(crate::i18n::tr(
                    "多台电脑共用同一台 AI 服务器账号时，把复制出来的这行填进你自己那份 \
                     Claude Code 的 MCP server env，代理就只会连到你这台 iShell、请求不会\
                     串到别人电脑上。每台电脑各配一次即可。",
                    "When several computers share one AI-server account, paste the copied line \
                     into your own Claude Code MCP server env so the proxy binds only to your \
                     iShell and requests never land on someone else's computer. Configure once \
                     per computer.",
                ))
                .clicked()
            {
                ui.ctx().copy_text(format!("ISHELL_MCP_TOKEN={token}"));
                ui.close();
            }
        }

        // —— 关于 ——
        ui.separator();
        if ui
            .button(format!(
                "{}  {}",
                egui_phosphor::regular::INFO,
                crate::i18n::tr("关于 iShell", "About iShell")
            ))
            .clicked()
        {
            set_about_open(true);
            ui.close();
        }
    });
}

/// 「关于」弹框：软件名 / 版本 / 主页 / 发布 / 许可 / 技术栈。版本号取自 Cargo.toml（编译期内嵌）。
pub(crate) fn about_window(ctx: &egui::Context) {
    if !about_open() {
        return;
    }
    let ver = crate::version::VERSION;
    let mut open = true;
    // 版本号同时放进标题，确保一定可见
    let title = match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("关于 iShell  ·  v{ver}"),
        crate::i18n::Lang::En => format!("About iShell  ·  v{ver}"),
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open) // 标题栏 X 关闭
        .show(ctx, |ui| {
            ui.set_width(330.0);
            ui.vertical_centered(|ui| {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("iShell")
                        .size(28.0)
                        .strong()
                        .color(Palette::ACCENT),
                );
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "现代化 Rust SSH 客户端",
                        "A modern Rust SSH client",
                    ))
                    .size(12.0)
                    .color(Palette::TEXT_DIM),
                );
                ui.add_space(8.0);
                // 版本行：强调色加大，显式着色，确保醒目可见
                ui.label(
                    RichText::new(format!("{} {ver}", crate::i18n::tr("版本", "Version")))
                        .size(16.0)
                        .strong()
                        .color(Palette::ACCENT),
                );
                ui.add_space(6.0);
            });
            ui.separator();
            ui.add_space(6.0);
            egui::Grid::new("about_grid")
                .num_columns(2)
                .spacing([12.0, 7.0])
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(crate::i18n::tr("项目主页", "Repository"))
                            .color(Palette::TEXT_DIM),
                    );
                    ui.hyperlink_to("github.com/wqkx/ishell", "https://github.com/wqkx/ishell");
                    ui.end_row();
                    ui.label(
                        RichText::new(crate::i18n::tr("下载发布", "Releases"))
                            .color(Palette::TEXT_DIM),
                    );
                    ui.hyperlink_to(
                        crate::i18n::tr("最新版本与各平台包", "Latest & platform builds"),
                        "https://github.com/wqkx/ishell/releases",
                    );
                    ui.end_row();
                    ui.label(
                        RichText::new(crate::i18n::tr("许可", "License")).color(Palette::TEXT_DIM),
                    );
                    ui.label("MIT");
                    ui.end_row();
                    ui.label(
                        RichText::new(crate::i18n::tr("技术栈", "Built with"))
                            .color(Palette::TEXT_DIM),
                    );
                    ui.label("Rust · egui/eframe · russh");
                    ui.end_row();
                });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                if ui
                    .add(
                        egui::Button::new(crate::i18n::tr("关闭", "Close"))
                            .min_size(egui::vec2(80.0, 0.0)),
                    )
                    .clicked()
                {
                    set_about_open(false);
                }
            });
        });
    if !open {
        set_about_open(false);
    }
}

/// 弹框内容统一包裹：固定宽度 + 内边距，避免文字直接贴到窗口边缘不美观。
pub(crate) fn dialog_body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(420.0);
            add(ui)
        })
        .inner
}

/// 扁平按钮（无边框、悬停高亮），用于标签栏等处。
/// 对话框按钮：用 egui 原生按钮（自然高度，仅约束最小宽度），由 egui 居中文字，
/// 与全局其它按钮一致，避免硬编码像素偏移在不同字体下错位。
pub(crate) fn dialog_button(
    ui: &mut egui::Ui,
    label: &str,
    fill: Option<egui::Color32>,
    width: f32,
) -> bool {
    let text = match fill {
        Some(_) => RichText::new(label).color(egui::Color32::WHITE),
        None => RichText::new(label),
    };
    let mut btn = egui::Button::new(text).min_size(egui::vec2(width, 0.0));
    if let Some(f) = fill {
        btn = btn.fill(f);
    }
    ui.add(btn).clicked()
}

/// 可拖动重排 + 缓动动画的标签条（仿主窗口顶部标签）。在当前 `ui` 内画一行可横向滚动的标签。
/// `labels`：每项 (稳定 uid, 显示文本, 悬停提示)。`drag`/`grab_dx`/`total_w` 为调用方持有的状态。
/// 返回 (要激活索引, 要关闭索引, 重排 (from,to))。
pub(crate) fn draggable_tabs(
    ui: &mut egui::Ui,
    drag: &mut Option<usize>,
    grab_dx: &mut f32,
    total_w: &mut f32,
    active: usize,
    want_scroll: bool,
    labels: &[(u64, String, String, f32, f32)],
) -> (Option<usize>, Option<usize>, Option<(usize, usize)>) {
    let mut to_activate = None;
    let mut to_close = None;
    let mut reorder = None;
    let mut drag_start: Option<usize> = None;
    let mut new_grab: Option<f32> = None;
    let mut drag_w = 0.0f32;
    let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
    let dragging_tab = *drag;
    let total_w_in = (*total_w).max(1.0);
    let out = egui::ScrollArea::horizontal()
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .scroll_source(egui::scroll_area::ScrollSource::MOUSE_WHEEL)
        .show(ui, |ui| {
            let tab_h = 24.0;
            let spacing = 4.0;
            let (area, _) = ui.allocate_exact_size(egui::vec2(total_w_in, tab_h), Sense::hover());
            let origin = area.min;
            let pointer = ui.input(|i| i.pointer.interact_pos());
            let drag_down = ui.input(|i| i.pointer.any_down());
            let ctx = ui.ctx().clone();
            let font = egui::FontId::proportional(12.0);
            let mut acc = 0.0f32;
            for (i, (uid, text, tip, prog, save)) in labels.iter().enumerate() {
                let selected = active == i;
                // 标签文本最大宽度约束：超长文件名截断加 …（完整名走悬停提示），
                // 否则一个长名字会把整个标签栏/窗口撑得很长
                const TAB_TEXT_MAX: f32 = 190.0;
                let mut text_show = text.clone();
                let mut title_w = ctx.fonts_mut(|f| {
                    f.layout_no_wrap(text_show.clone(), font.clone(), Palette::TEXT)
                        .rect
                        .width()
                });
                if title_w > TAB_TEXT_MAX {
                    // 先按比例粗截，再逐步收敛（至多几次布局，成本可忽略）
                    let chars: Vec<char> = text.chars().collect();
                    let mut keep = ((chars.len() as f32) * TAB_TEXT_MAX / title_w) as usize;
                    loop {
                        keep = keep.clamp(4, chars.len());
                        text_show = chars.iter().take(keep).collect::<String>() + "…";
                        title_w = ctx.fonts_mut(|f| {
                            f.layout_no_wrap(text_show.clone(), font.clone(), Palette::TEXT)
                                .rect
                                .width()
                        });
                        if title_w <= TAB_TEXT_MAX || keep <= 4 {
                            break;
                        }
                        keep -= 2;
                    }
                }
                let truncated = text_show.len() != text.len();
                let w = title_w + 16.0 + 22.0; // 左内边距 + 文本 + 右侧关闭区
                let target = acc;
                if selected && want_scroll {
                    let r = egui::Rect::from_min_size(
                        egui::pos2(origin.x + target, origin.y),
                        egui::vec2(w, tab_h),
                    );
                    ui.scroll_to_rect(r.expand2(egui::vec2(12.0, 0.0)), None);
                }
                let id = egui::Id::new(("dtabx", *uid));
                let dragging_this = drag_down && dragging_tab == Some(i);
                if dragging_this {
                    drag_w = w;
                }
                let x = if dragging_this {
                    let want = pointer.map(|p| p.x - origin.x - *grab_dx).unwrap_or(target);
                    ctx.animate_value_with_time(id, want, 0.0) // 跟手
                } else {
                    ctx.animate_value_with_time(id, target, 0.14) // 缓动到目标槽
                };
                let tab_rect = egui::Rect::from_min_size(
                    egui::pos2(origin.x + x, origin.y),
                    egui::vec2(w, tab_h),
                );
                let mut resp = ui.interact(
                    tab_rect,
                    egui::Id::new(("dtab", *uid)),
                    Sense::click_and_drag(),
                );
                // 拖动期间不弹路径提示（避免拖着拖着冒出悬停 tooltip）
                if dragging_tab.is_none() && !tip.is_empty() {
                    resp = resp.on_hover_text(tip.as_str());
                } else if dragging_tab.is_none() && truncated {
                    resp = resp.on_hover_text(text.as_str()); // 被截断且无路径提示 → 悬停显示完整名
                }
                let close_rect = egui::Rect::from_center_size(
                    egui::pos2(tab_rect.right() - 12.0, tab_rect.center().y),
                    egui::vec2(16.0, 16.0),
                );
                let close_resp = ui.interact(
                    close_rect,
                    egui::Id::new(("dtabclose", *uid)),
                    Sense::click(),
                );
                let p = ui.painter();
                let fill = if dragging_this {
                    Palette::ACCENT_SOFT
                } else if selected {
                    Palette::PANEL_2
                } else {
                    egui::Color32::TRANSPARENT
                };
                p.rect_filled(tab_rect, 6, fill);
                if *save >= 0.0 {
                    // 保存动画：底部整条珊瑚线上，先绿色从左扫到右（save 0→1，「保存中」），
                    // 再珊瑚色从左扫回覆盖绿色（save 1→2，「已保存」）。
                    let y = tab_rect.bottom() - 1.0;
                    let coral = Palette::ACCENT;
                    let green = egui::Color32::from_rgb(46, 200, 120);
                    if *save <= 1.0 {
                        let x = tab_rect.left() + tab_rect.width() * *save;
                        p.hline(
                            tab_rect.left()..=tab_rect.right(),
                            y,
                            egui::Stroke::new(2.0, coral),
                        );
                        p.hline(tab_rect.left()..=x, y, egui::Stroke::new(2.0, green));
                    } else {
                        let x = tab_rect.left() + tab_rect.width() * (*save - 1.0);
                        p.hline(
                            tab_rect.left()..=tab_rect.right(),
                            y,
                            egui::Stroke::new(2.0, green),
                        );
                        p.hline(tab_rect.left()..=x, y, egui::Stroke::new(2.0, coral));
                    }
                } else if *prog >= 0.0 {
                    // 加载中：底部珊瑚线从左到右随下载进度增长（替代选中态整条下划线）
                    let w_done = (tab_rect.width() * prog.clamp(0.0, 1.0)).max(0.0);
                    p.hline(
                        tab_rect.left()..=(tab_rect.left() + w_done),
                        tab_rect.bottom() - 1.0,
                        egui::Stroke::new(2.0, Palette::ACCENT),
                    );
                } else if selected && !dragging_this {
                    p.hline(
                        tab_rect.left()..=tab_rect.right(),
                        tab_rect.bottom() - 1.0,
                        egui::Stroke::new(2.0, Palette::ACCENT),
                    );
                }
                let tcolor = if selected {
                    Palette::TEXT
                } else {
                    Palette::TEXT_DIM
                };
                p.text(
                    egui::pos2(tab_rect.left() + 8.0, tab_rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &text_show,
                    font.clone(),
                    tcolor,
                );
                let xcolor = if close_resp.hovered() {
                    Palette::DANGER
                } else {
                    Palette::TEXT_DIM
                };
                p.text(
                    close_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    egui_phosphor::regular::X,
                    egui::FontId::proportional(11.0),
                    xcolor,
                );
                if close_resp.clicked() {
                    to_close = Some(i);
                } else if resp.clicked() {
                    to_activate = Some(i);
                } else if resp.middle_clicked() {
                    to_close = Some(i);
                }
                if resp.drag_started() {
                    drag_start = Some(i);
                    if let Some(pp) = pointer {
                        // drag_started() 触发时指针已越过拖拽阈值离开真实按下点，
                        // 抓取偏移须用按下位置计算，否则拖动起始瞬间标签会相对光标跳动
                        let press = crate::ui::drag_press_pos(ui, pp);
                        new_grab = Some(press.x - (origin.x + x));
                    }
                }
                tab_rects.push((
                    i,
                    egui::Rect::from_min_size(
                        egui::pos2(origin.x + target, origin.y),
                        egui::vec2(w, tab_h),
                    ),
                ));
                acc += w + spacing;
            }
            acc
        });
    *total_w = out.inner.max(1.0);
    // 溢出渐隐提示
    let off = out.state.offset.x;
    let vw = out.inner_rect.width();
    if off > 0.5 {
        edge_fade(ui.painter(), out.inner_rect, true, Palette::BG);
    }
    if off + vw < out.inner - 0.5 {
        edge_fade(ui.painter(), out.inner_rect, false, Palette::BG);
    }
    if let Some(g) = new_grab {
        *grab_dx = g;
    }
    if let Some(f) = drag_start {
        *drag = Some(f);
    }
    if let Some(from) = *drag {
        if ui.input(|i| i.pointer.any_down()) {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                let drag_center = pos.x - *grab_dx + drag_w / 2.0;
                let mut to = from;
                if from > 0 {
                    if let Some(&(_, lr)) = tab_rects.get(from - 1) {
                        if drag_center < lr.center().x {
                            to = from - 1;
                        }
                    }
                }
                if to == from {
                    if let Some(&(_, rr)) = tab_rects.get(from + 1) {
                        if drag_center > rr.center().x {
                            to = from + 1;
                        }
                    }
                }
                if to != from {
                    reorder = Some((from, to));
                    *drag = Some(to);
                }
            }
        } else {
            *drag = None;
        }
    }
    (to_activate, to_close, reorder)
}

pub(crate) fn flat_button(ui: &mut egui::Ui, text: &RichText, tip: &str) -> bool {
    let mut clicked = false;
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        v.widgets.active.bg_stroke = egui::Stroke::NONE;
        clicked = ui
            .add(egui::Button::new(text.clone()).corner_radius(6.0))
            .on_hover_text(tip)
            .clicked();
    });
    clicked
}
