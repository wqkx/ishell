//! Main window content panes. Split from layout.rs; behavior unchanged.

use egui::RichText;

use crate::proto::UiCommand;
use crate::theme::Palette;
use crate::ui::file_panel::{self, FileAction};

use super::util::*;
use super::view_state::{files_collapsed, osc7_consent, set_osc7_consent, OSC7_SNIPPET};
use super::App;

impl App {
    pub(super) fn right_body(&mut self, root: &mut egui::Ui, idx: usize) {
        // 右下文件操作区（可拖动调整高度）
        let mut file_actions: Vec<FileAction> = Vec::new();
        let has_clip = self.xfer.file_clip.is_some();
        if !files_collapsed() {
            egui::Panel::bottom("files")
                .resizable(true)
                .default_size(250.0)
                .size_range(120.0..=640.0)
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL)
                        .inner_margin(8)
                        .outer_margin(egui::Margin {
                            left: 6,
                            right: 6,
                            top: 6,
                            bottom: 6,
                        }),
                )
                .show_inside(root, |ui| {
                    file_actions = file_panel::show(ui, &mut self.sessions[idx].files, has_clip);
                });
            for a in file_actions {
                self.handle_file_action(idx, a);
            }
        }

        // 中间终端区（四周留空隙，与其他区域分开）。
        // 6px 内边距（边框）用「窗口暖米」与「当前终端主题底色」的中间色（固定色，非渐变），
        // 让窗口与 shell 之间过渡柔和、不再是生硬的一圈暖米。
        let mut reconnect_click = false;
        let tbg = crate::terminal::current_bg();
        // 浅色终端（经典浅/近白/暖米）边框直接用终端底色，与 shell 一致、无缝；
        // 深色终端用偏向终端的混合色，略留层次。
        let term_border = if tbg.r() as u32 + tbg.g() as u32 + tbg.b() as u32 > 450 {
            tbg
        } else {
            blend_color(Palette::TERM_BG, tbg)
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(term_border)
                    .inner_margin(6)
                    .outer_margin(egui::Margin { left: 6, right: 6, top: 6, bottom: 0 }),
            )
            .show_inside(root, |ui| {
                // 记下终端内容区的矩形，供右上角通知浮层贴着终端摆（而不是贴窗口边）。
                // `max_rect` 已经是 frame 的内外边距之内，正好是 shell 可见区域本身。
                // 不用 `ctx.available_rect()`：那个在 egui 0.34 已弃用，官方建议换成
                // `content_rect()`——但后者是整个窗口内容区、不扣面板，语义对不上。
                self.term_rect = Some(ui.max_rect());
                // 当前所有 AI（open_session）打开的会话 uid，AI 提示条里要报全，方便 AI
                // 自己核对哪些会话还在。
                let ai_uids: Vec<u64> = self
                    .sessions
                    .iter()
                    .filter(|s| s.ai_owned)
                    .map(|s| s.uid)
                    .collect();
                let s = &mut self.sessions[idx];
                // 断线提示条 + 手动重连（初次"连接中"不显示）
                if !s.connected {
                    egui::Frame::new()
                        .fill(Palette::ACCENT_SOFT)
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{}  {}", egui_phosphor::regular::WARNING, s.status)).color(Palette::DANGER).size(12.0));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.add(egui::Button::new(RichText::new(format!("{}  {}", egui_phosphor::regular::ARROW_CLOCKWISE, crate::i18n::tr("重连", "Reconnect"))).color(egui::Color32::WHITE)).fill(Palette::ACCENT)).clicked() {
                                        reconnect_click = true;
                                    }
                                });
                            });
                        });
                    ui.add_space(4.0);
                }
                // ai_owned 会话是 AI 自己新开的只读会话：报出这个终端自己的 uid + 当前全部
                // AI 终端的 uid（方便 AI 核对），用高对比度的实心底色 + 加粗白字，确保不管
                // 当前终端主题深浅都清楚可辨。非 AI 会话不显示任何 MCP 相关提示。
                if s.ai_owned {
                    let uid = s.uid;
                    let ai_list = ai_uids
                        .iter()
                        .map(|u| u.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    egui::Frame::new()
                        .fill(Palette::ACCENT)
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(match crate::i18n::current() {
                                    crate::i18n::Lang::Zh => format!(
                                        "{}  AI 正在驱动此终端（只读，uid={uid}）· 当前全部 AI 终端 uid：{ai_list}",
                                        egui_phosphor::regular::ROBOT,
                                    ),
                                    crate::i18n::Lang::En => format!(
                                        "{}  AI is driving this terminal (read-only, uid={uid}) · All AI terminals: uid {ai_list}",
                                        egui_phosphor::regular::ROBOT,
                                    ),
                                })
                                .color(egui::Color32::WHITE)
                                .strong()
                                .size(12.0),
                            );
                        });
                    ui.add_space(4.0);
                }
                let input = s.terminal.ui(ui);
                // ai_owned 会话由 AI 驱动：用户仍可看（渲染/滚动/查找都照常），但键盘输入
                // 不转发给远端，避免用户误敲打断 AI 正在等待的哨兵检测。
                if !input.is_empty() && !s.ai_owned {
                    let _ = s.cmd_tx.send(UiCommand::TerminalInput(input));
                }
                // 右键菜单「在文件列表中显示当前目录」：把文件区导航到终端当前目录
                if let Some(cwd) = s.terminal.take_reveal_cwd() {
                    s.files.cwd = cwd;
                    s.files.selected.clear();
                }
                // 无 cwd 时点该菜单：已同意过则静默注入（吞掉命令回显）；否则弹确认框（同意后记住）
                // ai_owned 会话是只读的（AI 专用），不接受这类注入——也避免和 MCP 自己的
                // expect_echo（哨兵回显吞除）互相覆盖，打断正在进行的 run_command。
                if s.terminal.take_inject_request() && !s.ai_owned {
                    if osc7_consent() {
                        let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("{OSC7_SNIPPET}\r").into_bytes()));
                        s.terminal.expect_echo(OSC7_SNIPPET);
                        s.osc7_pending_reveal = true;
                    } else {
                        s.osc7_confirm = true;
                    }
                }
                // MCP 配对 token 自动注入：多台电脑共用同一台 AI 服务器时，让本会话里启动的
                // AI（及其 ishell-mcp 子进程）自动携带配对标识——MCP 请求经既有的 token 匹配
                // 精确路由回本电脑，不再弹窗打扰其他人（未注入时维持原有「多实例弹窗选择」）。
                //
                // 只在「提示符几乎确定闲置」时注入，信号缺一不可（等下一帧重试）：
                // 终端不忙（无全屏程序/1s 内无输出）、远端输出静止 2s+、用户停笔 2s+ 且
                // 本地输入行为空；且无挂起的 AI 命令/哨兵捕获（expect_echo 会覆盖其吞除状态）。
                // ai_owned 会话（AI 专用）不注入。
                if crate::store::load_mcp_consent()
                    && s.connected
                    && !s.ai_owned
                    && !s.mcp_token_injected
                    && s.pending_ai_run.is_none()
                    && !s.terminal.ai_capture_pending()
                    && !s.terminal.appears_busy()
                    && s.terminal.output_idle_for(std::time::Duration::from_secs(2))
                    && s.terminal.input_idle_for(std::time::Duration::from_secs(2))
                {
                    inject_mcp_token(s);
                    s.mcp_token_injected = true;
                }
                if s.osc7_confirm {
                    let mut decided: Option<bool> = None;
                    egui::Modal::new(egui::Id::new("osc7_confirm_modal")).show(ui.ctx(), |ui| {
                        ui.set_width(370.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(crate::i18n::tr("获取终端当前目录", "Track terminal directory")).size(16.0).strong());
                            ui.add_space(6.0);
                            ui.label(crate::i18n::tr(
                                "需向当前 shell 注入一行命令以上报工作目录（仅本会话、不写配置文件）。同意后将记住，后续自动静默注入。",
                                "Inject one line into the current shell to report its directory (this session only, not written to config). Remembered after you agree.",
                            ));
                        });
                        ui.add_space(12.0);
                        let bw = 110.0;
                        let total = bw * 2.0 + ui.spacing().item_spacing.x;
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                            if ui.add(egui::Button::new(RichText::new(crate::i18n::tr("同意并注入", "Agree & inject")).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                                decided = Some(true);
                            }
                            if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                                decided = Some(false);
                            }
                        });
                    });
                    match decided {
                        Some(true) => {
                            set_osc7_consent(true);
                            let _ = s.cmd_tx.send(UiCommand::TerminalInput(format!("{OSC7_SNIPPET}\r").into_bytes()));
                            s.terminal.expect_echo(OSC7_SNIPPET);
                            s.osc7_pending_reveal = true;
                            s.osc7_confirm = false;
                        }
                        Some(false) => s.osc7_confirm = false,
                        None => {}
                    }
                }
                // 注入后：下个提示符上报 cwd 时把文件区跳过去
                if s.osc7_pending_reveal {
                    if let Some(cwd) = s.terminal.cwd() {
                        s.files.cwd = cwd.to_string();
                        s.files.selected.clear();
                        s.osc7_pending_reveal = false;
                    }
                }
                let size = s.terminal.size();
                if size != s.last_size && s.connected {
                    s.last_size = size;
                    let _ = s.cmd_tx.send(UiCommand::Resize { cols: size.0, rows: size.1 });
                }
            });
        if reconnect_click {
            if let Some(s) = self.sessions.get_mut(idx) {
                s.reconnect_tries = 0;
            }
            self.reconnect_session(idx);
        }
    }
}

/// 往会话终端注入 `export ISHELL_MCP_TOKEN=<本机配对 token>`（回显吞除）。
/// 此后该 shell 里启动的 AI / ishell-mcp 子进程自动继承这个环境变量，MCP 绑定走
/// ishell-mcp 既有的 token 匹配路径（`bind_instance`），请求精确路由回这台电脑。
/// 前导空格：配合 bash/zsh 常见的 HISTCONTROL=ignorespace，不进 shell 历史。
/// token 是 16 位 hex（无 shell 特殊字符），无需引号。
fn inject_mcp_token(s: &mut super::Session) {
    let cmd = format!(
        " export ISHELL_MCP_TOKEN={}",
        crate::store::mcp_pairing_token()
    );
    let _ = s
        .cmd_tx
        .send(UiCommand::TerminalInput(format!("{cmd}\r").into_bytes()));
    s.terminal.expect_echo(&cmd);
}
