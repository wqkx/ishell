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
                    // 冲突策略同步给面板：拖拽移动要据它决定还能不能记撤销（见 FilePanelState 字段说明）
                    let policy = self.conflict_policy;
                    let files = &mut self.sessions[idx].files;
                    files.conflict_policy = policy;
                    file_actions = file_panel::show(ui, files, has_clip);
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
                // 粘贴了一张图片（Ctrl+V / 右键粘贴，剪贴板里是图不是文本）：落地成文件、
                // 远端会话则上传，传完把路径打进终端（见 Session::paste_image）。
                // ai_owned 会话不接受用户键入，跳过。
                if let Some(png) = s.terminal.take_paste_image() {
                    if !s.ai_owned {
                        s.paste_image(png);
                    }
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
                // 右键菜单「配置 AI 完成通知」：把安装命令打进终端（不自动执行）。
                if s.terminal.take_notify_setup_request() {
                    inject_notify_setup(s);
                }
                // 右键菜单「安装 AI 控制代理到这台服务器」：走 SFTP 传内嵌的 ishell-mcp。
                if s.terminal.take_deploy_agent_request() {
                    if s.cfg.is_local() {
                        s.status = crate::i18n::tr(
                            "「本机」会话不需要部署代理：代理本来就跑在这台电脑上。",
                            "The local session needs no agent deployment — the agent already runs on this computer.",
                        )
                        .into();
                    } else {
                        let _ = s.cmd_tx.send(UiCommand::DeployMcpAgent);
                        s.status = crate::i18n::tr(
                            "正在安装 AI 控制代理 …",
                            "Installing the AI control agent …",
                        )
                        .into();
                    }
                }
                // 两处「程序替用户敲键盘」的自动注入——重连后恢复工作目录、MCP 配对标识
                // ——都**不在这里**：它们挂在 `App::advance_auto_injections` 上
                // （`pump_background`，与标签页无关的每帧路径）。本函数只对当前活动标签
                // 调用，而「该不该替用户敲这一行」跟用户正在看哪个标签毫无关系。
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

/// hook 本体。`MSG` 是占位符，合并脚本按事件替换成具体文案。
///
/// **只用双引号，绝不含单引号**：整段要塞进外层的单引号里传给 python，而 sh 没有"在单引号
/// 内转义单引号"的写法——出现一个就会把命令从那里截断。有测试守着这条约束。
pub(super) const NOTIFY_HOOK: &str = concat!(
        r#"p=$$; i=0; while [ $i -lt 6 ]; do p=$(ps -o ppid= -p $p 2>/dev/null | tr -d " "); "#,
        r#"[ -z "$p" ] && break; t=$(ps -o tty= -p $p 2>/dev/null | tr -d " "); "#,
        r#"case $t in ""|"?") ;; *) [ -w /dev/$t ] && "#,
        r#"printf "\033]777;notify;%s;%s\007" "TAG" "MSG" > /dev/$t; break;; esac; "#,
        r#"i=$((i+1)); done; exit 0 # ishell-osc9"#,
);

/// 合并脚本。同样只用双引号；hook 本体经环境变量 `ISHW` 传入，避免两层引号互相打架。
pub(super) const NOTIFY_MERGE: &str = concat!(
        r#"import json,os,shutil; p=os.path.expanduser("~/.claude/settings.json"); "#,
        r#"os.makedirs(os.path.dirname(p),exist_ok=True); "#,
        r#"d=json.load(open(p,encoding="utf-8")) if os.path.exists(p) else {}; "#,
        r#"os.path.exists(p) and shutil.copy2(p,p+".bak"); w=os.environ["ISHW"]; "#,
        r#"h=d.setdefault("hooks",{}); "#,
        r#"h["Notification"]=[x for x in h.get("Notification",[]) if "ishell-osc9" not in json.dumps(x)]"#,
        r#"+[{"hooks":[{"type":"command","command":w.replace("TAG","ishell:need").replace("MSG","Claude Code 需要你确认")}]}]; "#,
        r#"h["Stop"]=[x for x in h.get("Stop",[]) if "ishell-osc9" not in json.dumps(x)]"#,
        r#"+[{"hooks":[{"type":"command","command":w.replace("TAG","ishell:done").replace("MSG","Claude Code 任务完成")}]}]; "#,
        r#"json.dump(d,open(p,"w",encoding="utf-8"),ensure_ascii=False,indent=2); "#,
        r#"print("iShell 通知 hook 已写入 "+p+"（原配置备份为 settings.json.bak）")"#,
);

/// 把「配置 AI 完成通知」的安装命令打进终端。
///
/// 刻意**不吞回显**（对比 `inject_mcp_token`）：token 是密钥，看见反而不好；这条命令要改的是
/// 对方机器上的 `~/.claude/settings.json`，用户有权在按回车之前看清它到底做了什么。
///
/// 也刻意**不自动执行**（结尾不发 `\r`）：改别人 AI 配置这种事，最后那一下必须由人来按。
///
/// 通知用 OSC 777（而不是 OSC 9）：它的标题位可以带一个类别标记（`ishell:need` /
/// `ishell:done`），iShell 据此把「需要你确认」和「任务完成」分开，好让设置里的
/// 「仅需要我处理时」这一档真的能滤掉每轮都来的完成提醒。标记只是内部用，显示时会被剥掉。
///
/// 命令做三件事：备份原配置 → 合并两个 hook（Notification / Stop）→ 写回。合并按注释标记
/// `# ishell-osc9` 去重，重复执行不会堆出多条。hook 本体是一段 sh：沿父进程链向上找到第一个
/// 有 tty 的祖先（通常就是 claude 进程本身）再发 OSC 9——不能用常见的 `> /dev/tty`，实测
/// Claude Code 起的子进程没有控制终端，那条路会静默失败。
fn inject_notify_setup(s: &mut super::Session) {
    let cmd = format!("ISHW='{NOTIFY_HOOK}' python3 -c '{NOTIFY_MERGE}'");
    let _ = s
        .cmd_tx
        .send(UiCommand::TerminalInput(cmd.into_bytes()));
    s.status = crate::i18n::tr(
        "安装命令已打进终端：看一眼没问题再按回车执行",
        "Install command typed into the terminal — review it, then press Enter",
    )
    .into();
}

#[cfg(test)]
mod notify_setup_tests {
    /// 安装命令是「外层单引号包住两大段」的形状：
    /// `ISHW='<hook>' python3 -c '<merge>'`。
    /// 只要这两段里出现**任何一个单引号**，shell 的引号就会在那里断开，后半段被当成别的
    /// 参数甚至别的命令——注入到用户终端里的东西必须不可能这样炸。sh 也没有"在单引号里
    /// 转义单引号"的写法，所以这不是"注意点"，是硬约束。
    #[test]
    fn installer_segments_contain_no_single_quotes() {
        for (name, seg) in [
            ("HOOK", super::NOTIFY_HOOK),
            ("MERGE", super::NOTIFY_MERGE),
        ] {
            assert!(
                !seg.contains('\''),
                "{name} 里出现了单引号，会把外层引号截断：{seg}"
            );
        }
    }

    /// 去重标记必须在 hook 命令里：合并脚本靠它认出"这条是 iShell 装的"，
    /// 丢了就会每点一次菜单堆一条新 hook。
    #[test]
    fn hook_carries_the_dedup_marker() {
        assert!(super::NOTIFY_HOOK.contains("ishell-osc9"));
        assert!(super::NOTIFY_MERGE.contains("ishell-osc9"));
    }

    /// hook 里必须有 MSG 占位符供合并脚本替换成具体文案；替换后不该再剩下占位符。
    #[test]
    fn message_placeholder_is_present_and_replaceable() {
        assert!(super::NOTIFY_HOOK.contains("MSG"));
        assert!(!super::NOTIFY_HOOK.replace("MSG", "x").contains("MSG"));
    }
}
