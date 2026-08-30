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
                // MCP 配对 token 自动注入：多台电脑共用同一台 AI 服务器时，让本会话里启动的
                // AI（及其 ishell-mcp 子进程）自动携带配对标识——MCP 请求经既有的 token 匹配
                // 精确路由回本电脑，不再弹窗打扰其他人（未注入时维持原有「多实例弹窗选择」）。
                //
                // **默认关，需在设置里显式勾选**（见 `store::load_mcp_auto_pair`）。这一步是
                // iShell 替用户在他自己的 shell 里敲一条命令并回车；以前挂在「AI 控制已开启」
                // 下面还说得过去（那时 AI 控制默认关，能走到这儿的人都是自己勾过的），0.19 起
                // AI 控制默认开启之后，同一个门就变成「每个新会话都被自动打进一条命令」——
                // 用户的现场反馈正是「iShell 往我当前会话里输东西」。
                //
                // 只在「提示符几乎确定闲置」时注入，信号缺一不可（等下一帧重试）：
                // 终端不忙（无全屏程序/1s 内无输出）、远端输出静止 2s+、用户停笔 2s+ 且
                // 本地输入行为空、**本次连接以来一个键都没敲过**；且无挂起的 AI 命令/哨兵
                // 捕获（expect_echo 会覆盖其吞除状态）。ai_owned 会话（AI 专用）不注入。
                //
                // 最后那条「没敲过键」是安全边界而非优化：前面几条信号分不清「shell 提示符」
                // 和「程序阻塞在 stdin」，而 sudo/ssh 的密码提示符恰好也是安静不动的。
                // 详见 Terminal::never_typed。
                //
                // 仍存在的残余风险：某个保存的连接其远端命令直接落进一个密码提示符——那种
                // 情况下用户确实一个键都没敲。这不在本次修复范围内。
                // 重连后恢复工作目录：等 shell 真的闲下来再 `cd` 回去。
                // 判据与下面的配对标识注入共用 `Session::shell_idle_for_injection`——两处干的
                // 是同一件事（程序替用户敲键盘），判据分成两份迟早会漂移。
                //
                // **`s.connected` 是这一段的前提。** 断线横幅那个分支只画一条提示就往下走，
                // 不挡渲染——也就是说重连期间本段照样每帧被求值，而那时 `do_reconnect` 已
                // 置下 `restore_cwd`、截止时刻却要到 `Connected` 才武装（`restore_cwd_until`
                // 还是 `None`）。少了这个门，判据全落在「未连接」那一侧：`idle` 恒假、
                // `never_typed` 恒真（终端刚重建），意图只会被白白丢掉。
                if s.connected && s.restore_cwd && !s.last_cwd.is_empty() && !s.ai_owned {
                    let expired = super::session::cwd_restore_expired(
                        s.restore_cwd_until,
                        std::time::Instant::now(),
                    );
                    match super::session::cwd_restore_decision(
                        // 这里再读一次 `never_typed` **不是**跟闸门重复：闸门问的是
                        // 「现在敲键盘安不安全」（不安全就等下一帧），这里问的是「这个
                        // 意图还该不该留着」——用户已经上手了就直接放弃，而不是干等到
                        // 15s 超时。同一个字段，两个不同的问题、两种不同的结果。
                        s.terminal.never_typed(),
                        s.shell_idle_for_injection(),
                        expired,
                    ) {
                        super::session::CwdRestore::Inject => {
                            let cmd = format!(
                                "cd '{}'",
                                s.last_cwd.replace('\'', "'\\''")
                            );
                            let _ = s.cmd_tx.send(UiCommand::TerminalInput(
                                format!("{cmd}\r").into_bytes(),
                            ));
                            // 吞掉回显——此前这条注入连这一步都没有，`cd '…'` 会原样留在屏幕上。
                            s.terminal.expect_echo(&cmd);
                            s.restore_cwd = false;
                            s.restore_cwd_until = None;
                        }
                        super::session::CwdRestore::GiveUp => {
                            s.restore_cwd = false;
                            s.restore_cwd_until = None;
                        }
                        super::session::CwdRestore::Wait => {}
                    }
                }
                if pair_inject_allowed(
                    crate::store::load_mcp_auto_pair(),
                    crate::store::load_mcp_consent(),
                    s.ai_owned,
                    s.mcp_token_injected,
                ) && s.shell_idle_for_injection()
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

/// 该不该往这个会话自动注入配对标识——只管**策略**那几个开关，终端是否闲置由调用处判断。
///
/// 单独抽出来是为了能测：这里最要命的错误是「打开 AI 控制就等于允许 iShell 替我敲键盘」，
/// 那是一条只有真机上开个新会话才看得见的回归，写成纯函数才守得住。
fn pair_inject_allowed(
    auto_pair_on: bool,
    mcp_on: bool,
    ai_owned: bool,
    already_injected: bool,
) -> bool {
    // `mcp_on` 仍是前置条件：AI 控制都没开，注入配对标识毫无意义。
    auto_pair_on && mcp_on && !ai_owned && !already_injected
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

#[cfg(test)]
mod pair_inject_tests {
    use super::pair_inject_allowed;

    /// **回归门禁**：仅仅打开「允许 AI 通过 MCP 控制终端」，不等于允许 iShell 替用户在他
    /// 自己的 shell 里敲一条命令并回车。
    ///
    /// 0.19 把 AI 控制的默认值翻成开启之后，自动注入的门还挂在那个开关上，于是每一个新连上
    /// 的会话都会被自动打进 ` export ISHELL_MCP_TOKEN=…` 并执行——用户报的「iShell 往我当前
    /// 会话里输东西」就是它。注入必须由**它自己那个默认关闭的开关**把门。
    #[test]
    fn enabling_ai_control_alone_never_authorises_typing_into_the_users_shell() {
        assert!(
            !pair_inject_allowed(false, true, false, false),
            "只开了 AI 控制就往用户 shell 里敲命令——这正是 0.19 的那条回归"
        );
    }

    /// 显式勾选之后才注入，且只注入一次、只注入用户自己的会话。
    #[test]
    fn opted_in_injects_once_into_user_sessions_only() {
        assert!(pair_inject_allowed(true, true, false, false), "勾选后应当注入");
        assert!(
            !pair_inject_allowed(true, true, true, false),
            "AI 专用会话不该注入：那里的 AI 是我们自己开的，本来就知道该回哪台电脑"
        );
        assert!(
            !pair_inject_allowed(true, true, false, true),
            "已经注入过就不该再来一次"
        );
        assert!(
            !pair_inject_allowed(true, false, false, false),
            "AI 控制没开时注入配对标识毫无意义"
        );
    }
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
