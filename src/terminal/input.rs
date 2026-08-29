//! Keyboard input collection and command history navigation.

use egui::Key;

use super::{keys::encode_key, Terminal};

pub(super) struct HistState {
    pub(super) prefix: String,
    pub(super) idx: usize,
}

impl Terminal {
    pub(super) fn collect_input(&mut self, ui: &egui::Ui) -> Vec<u8> {
        let mut out = Vec::new();
        let events: Vec<egui::Event> = ui.input(|i| i.events.clone());
        let shift = ui.input(|i| i.modifiers.shift);
        // 「Option 当 Meta 键」：按住 Alt 时丢掉 Text 事件，改由 encode_key 发 `ESC <char>`。
        // 不这样做的话，macOS 上 Option+B 会既产生文本 "∫"（Event::Text）又产生 Key 事件，
        // 两者都发出去就是双份输入；而终端用户要的是 Meta 语义（Alt+B = 按词后移）。
        // 排除 ctrl：Windows/Linux 的 AltGr 被报成 Ctrl+Alt，那是真的要输入字符（某些键盘
        // 布局靠它打 @、# 等），不能吞。
        let meta_held = ui.input(|i| i.modifiers.alt && !i.modifiers.ctrl);
        let alt = self.parser.screen().alternate_screen();
        if alt {
            self.input_line.clear();
            self.hist = None;
        }
        // 组字状态自愈的两个信号（判定放在循环后，见那里的说明）
        let ime_seen = events.iter().any(|e| matches!(e, egui::Event::Ime(_)));
        let plain_text_seen = events.iter().any(|e| matches!(e, egui::Event::Text(_)));
        for ev in events {
            // 记录用户输入时刻：自动注入（如 MCP 配对 export）必须等用户停笔的安全信号
            match &ev {
                egui::Event::Text(_)
                | egui::Event::Paste(_)
                | egui::Event::Ime(egui::ImeEvent::Commit(_))
                | egui::Event::Key { pressed: true, .. } => {
                    self.last_input_at = Some(std::time::Instant::now());
                }
                _ => {}
            }
            // 诊断（`RUST_LOG=ishell=trace` 可见）：把到达终端的按键/文本/IME 事件原样记下。
            // 输入法类问题（组字不显示、按 Shift 丢字等）光看现象必然靠猜——必须先知道
            // 「输入法到底发了什么、走的哪条通道」。只记这三类，鼠标移动等不记，避免刷屏。
            if log::log_enabled!(log::Level::Trace) {
                match &ev {
                    egui::Event::Text(t) => log::trace!("term ev: Text({t:?})"),
                    egui::Event::Ime(e) => log::trace!("term ev: Ime({e:?})"),
                    egui::Event::Key {
                        key,
                        pressed,
                        repeat,
                        modifiers,
                        ..
                    } => log::trace!(
                        "term ev: Key({key:?} pressed={pressed} repeat={repeat} mods={modifiers:?})"
                    ),
                    _ => {}
                }
            }
            match ev {
                egui::Event::Text(t) => {
                    if meta_held {
                        continue; // Alt 组合交给 encode_key 发 ESC 前缀形式，见上面 meta_held
                    }
                    self.clear_selection(); // 输入字符即取消选择（与按键分支一致）
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    out.extend_from_slice(t.as_bytes());
                }
                egui::Event::Ime(egui::ImeEvent::Preedit(s)) => {
                    log::debug!("IME Preedit: {s:?}");
                    self.ime_preedit = s;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(t)) => {
                    log::debug!("IME Commit: {t:?}");
                    self.ime_preedit.clear();
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    out.extend_from_slice(t.as_bytes());
                }
                egui::Event::Ime(egui::ImeEvent::Enabled)
                | egui::Event::Ime(egui::ImeEvent::Disabled) => {
                    log::debug!("IME enabled/disabled event");
                    self.ime_preedit.clear();
                }
                egui::Event::Paste(t) => {
                    // 记下「这一下 Ctrl+V 是有文本的」，供下面 V 松开时判断——见那段说明。
                    self.saw_text_paste = true;
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    // bracketed paste：远端开了就套括号，否则多行内容会被逐行当成回车敲进去。
                    let wrapped = self.wrap_paste(t.as_bytes());
                    out.extend_from_slice(&wrapped);
                }
                // Ctrl+V 且剪贴板里**没有文本**（典型：刚截的一张图）。
                //
                // 这里为什么要靠「松开」而不是「按下」：egui-winit 在**按下**时就把 Ctrl+V
                // 认成粘贴命令，读一次剪贴板文本，然后 `return`——读不到文本（图片/空剪贴板）
                // 时它连 `Event::Key` 都不再往下发，终端于是彻底收不到这一下按键，
                // Claude Code 的「Ctrl+V 贴图」按下去毫无反应。而那段拦截写在 `if pressed`
                // 里面，**松开**的 Key 事件照常送达，所以这一下只能在松开时补。
                //
                // 补什么：剪贴板里真有图 → 交给 App 落地成文件（远端会话则上传），再把路径
                // 打进终端，AI CLI 认路径；连图都没有（空剪贴板）→ 发 0x16，也就是 Ctrl+V
                // 的原字节，跟普通终端一致（readline 的 quoted-insert）。
                egui::Event::Key {
                    key: Key::V,
                    pressed: false,
                    modifiers,
                    ..
                } => {
                    // 判据是「有 V 的松开、却没有 V 的按下」：按下被吞掉说明 egui 把它当成了
                    // 粘贴命令（Ctrl+V 或 Ctrl+Shift+V，`is_paste_command` 只看 command 不看
                    // shift）。**不能**改看松开时的 `modifiers.ctrl`——先松 Ctrl 还是先松 V
                    // 完全取决于用户手指，先松 Ctrl 的话这里读到的 ctrl 已经是 false 了。
                    let was_swallowed_paste = !self.saw_v_press;
                    if was_swallowed_paste && !self.saw_text_paste {
                        self.grab_clipboard_image();
                        // 连图都没有（空剪贴板）：裸 Ctrl+V 按普通终端的样子发 0x16；
                        // Ctrl+Shift+V 是 iShell 自己的「粘贴」键，没内容时该什么都不做，
                        // 而不是往远端发一个控制字符。
                        if self.paste_image.is_none() && !modifiers.shift {
                            out.push(0x16);
                        }
                    }
                    self.saw_text_paste = false;
                    self.saw_v_press = false;
                }
                egui::Event::Copy => {
                    let copy_selection = cfg!(target_os = "macos") || shift;
                    if copy_selection {
                        if let Some(t) = self.selected_text() {
                            ui.ctx().copy_text(t);
                        }
                    } else {
                        out.push(0x03);
                        if !alt {
                            self.input_line.clear();
                            self.hist = None;
                        }
                    }
                }
                egui::Event::Cut =>
                {
                    #[cfg(not(target_os = "macos"))]
                    if !shift {
                        out.push(0x18);
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    // V 的按下到达了我们 = egui 没有把它当成粘贴命令吞掉（见下面的松开分支）。
                    if key == Key::V {
                        self.saw_v_press = true;
                    }
                    let plain =
                        !modifiers.ctrl && !modifiers.alt && !modifiers.command && !modifiers.shift;
                    // Shift+选择键：主屏且远端未接管键盘时做本地键盘选区（Shift+↑↓ 跨行选择
                    // 多行文本；远端程序开了应用光标/备用屏/bracketed paste 时透传——
                    // vim 里 Shift+↑ 是翻页，nano 里 Shift+方向是选区，不能拦）
                    if !alt
                        && modifiers.shift
                        && !modifiers.ctrl
                        && !modifiers.alt
                        && !modifiers.command
                        && !self.parser.screen().application_cursor()
                        && !self.parser.screen().bracketed_paste()
                        && matches!(
                            key,
                            Key::ArrowUp
                                | Key::ArrowDown
                                | Key::PageUp
                                | Key::PageDown
                                | Key::Home
                                | Key::End
                        )
                    {
                        self.shift_select(key);
                        continue;
                    }
                    // 其余按键取消选区（主流终端行为：键盘输入即撤销选择；鼠标点击处另有清除）
                    self.clear_selection();
                    // 裸上下键：默认走 iShell 的本地前缀历史（对普通 shell 提示符很好用）。但当前台
                    // 程序**自己在做行编辑**时绝不能拦，否则它的方向键功能（ipython 的补全菜单、
                    // vim 的光标移动、fzf 选择等）全废。两个信号任一置位即「让程序自己接管方向键」，
                    // 透传给 encode_key：
                    //   · application_cursor（DECCKM）——vim/fzf/less 等会开；
                    //   · bracketed_paste——**ipython/prompt_toolkit 与现代 readline 会开，但它们
                    //     并不开 DECCKM**（实测 ipython 8.x 甚至主动关 DECCKM、只开 bracketed
                    //     paste）。少了这条，ipython 补全菜单的上下键就只能被本地历史吞掉、失灵。
                    if !alt
                        && plain
                        && !self.parser.screen().application_cursor()
                        && !self.parser.screen().bracketed_paste()
                        && matches!(key, Key::ArrowUp | Key::ArrowDown)
                    {
                        out.extend_from_slice(&self.history_nav(key == Key::ArrowUp));
                        continue;
                    }
                    if !alt {
                        match key {
                            // 只有「裸回车」才是提交：Shift/Alt+Enter 现在发 `ESC CR`，语义是
                            // 换行继续输入（见 keys.rs），若也当成提交会把半截命令推进本地
                            // 历史、并清空正在跟踪的输入行。
                            Key::Enter if !modifiers.shift && !modifiers.alt => self.commit_line(),
                            Key::Backspace => {
                                self.input_line.pop();
                                self.hist = None;
                            }
                            Key::C | Key::U if modifiers.ctrl => {
                                self.input_line.clear();
                                self.hist = None;
                            }
                            _ => {}
                        }
                    }
                    encode_key(key, modifiers, self.parser.screen().application_cursor(), &mut out);
                }
                _ => {}
            }
        }
        // 组字状态自愈：本帧收到了普通文本输入，却一条 Ime 事件都没有。XIM 组字期间按键
        // 会被输入法过滤掉，能收到裸 `Text` 就说明组字已经不在了——输入法多半是半路没了
        // （fcitx 崩溃/重启、远程桌面会话切换），`Disabled` 永远不会来。不清的话，那截没
        // 提交的拼音会一直画在光标处，看起来像终端花了，而且用户重启输入法也擦不掉。
        if !ime_seen && plain_text_seen && !self.ime_preedit.is_empty() {
            self.ime_preedit.clear();
        }
        out
    }

    pub(super) fn history_nav(&mut self, up: bool) -> Vec<u8> {
        if self.input_line.is_empty() {
            self.hist = None;
            return if up {
                b"\x1b[A".to_vec()
            } else {
                b"\x1b[B".to_vec()
            };
        }
        let prefix = match &self.hist {
            Some(h) => h.prefix.clone(),
            None => self.input_line.clone(),
        };
        let start = self
            .hist
            .as_ref()
            .map(|h| h.idx as isize)
            .unwrap_or(self.history.len() as isize);
        if up {
            let mut i = start - 1;
            while i >= 0 {
                let cand = &self.history[i as usize];
                if cand.starts_with(&prefix) && cand != &self.input_line {
                    let m = cand.clone();
                    self.hist = Some(HistState {
                        prefix,
                        idx: i as usize,
                    });
                    return self.rewrite_line(&m);
                }
                i -= 1;
            }
            Vec::new()
        } else {
            if self.hist.is_none() {
                return Vec::new();
            }
            let mut i = start + 1;
            while (i as usize) < self.history.len() {
                let cand = &self.history[i as usize];
                if cand.starts_with(&prefix) {
                    let m = cand.clone();
                    self.hist = Some(HistState {
                        prefix,
                        idx: i as usize,
                    });
                    return self.rewrite_line(&m);
                }
                i += 1;
            }
            self.hist = None;
            self.rewrite_line(&prefix.clone())
        }
    }

    pub(super) fn rewrite_line(&mut self, text: &str) -> Vec<u8> {
        let mut out = vec![0x05, 0x15];
        out.extend_from_slice(text.as_bytes());
        self.input_line = text.to_string();
        out
    }

    pub(super) fn commit_line(&mut self) {
        // 本标签跑过 AI CLI → 此后允许裸 BEL 当通知（见 `Terminal::ai_cli_seen`）。
        if is_ai_cli_command(&self.input_line) {
            self.ai_cli_seen = true;
        }
        if !self.input_line.trim().is_empty()
            && self
                .history
                .last()
                .map(|s| s != &self.input_line)
                .unwrap_or(true)
        {
            self.history.push(self.input_line.clone());
            if self.history.len() > 500 {
                self.history.remove(0);
            }
        }
        self.input_line.clear();
        self.hist = None;
    }
}

/// 已知的 AI CLI 可执行名。**只做 basename 精确匹配,不做子串/模糊匹配**——`q`、`amp`
/// 这类短名一旦用子串匹配会命中半数普通命令,把"仅 AI 标签提醒"直接废掉。
///
/// claude 与 codex 是实测过通知行为的:codex 发 OSC 9,Claude Code 只发裸 BEL
/// (`preferredNotifChannel` 在 Linux 上只有 `terminal_bell`)——正是后者让"哪个标签算 AI"
/// 变成必须回答的问题。其余几个是同类工具,按同一形状收录。
const AI_CLI_NAMES: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "aider",
    "gemini",
    "cursor-agent",
];

/// 这条命令行是不是在启动一个 AI CLI。
///
/// 只看**解析出来的可执行名**:跳过前置的 `VAR=value` 环境赋值,跳过 `env`/`npx` 之类的
/// 包装器,再按最后一段路径名比对——`/usr/local/bin/claude`、`./claude`、`claude -c`、
/// `FOO=1 npx @anthropic-ai/claude-code` 都该算数。
pub(super) fn is_ai_cli_command(line: &str) -> bool {
    const WRAPPERS: &[&str] = &[
        "env", "command", "exec", "nohup", "npx", "bunx", "pnpx", "uvx", "time",
    ];
    for tok in line.split_whitespace() {
        // 前置环境赋值(`FOO=1 claude`):跳过继续看下一个词。要求 `=` 前非空且不含 `/`,
        // 免得把 `./a=b` 这种路径当成赋值。
        if let Some(eq) = tok.find('=') {
            if eq > 0 && !tok[..eq].contains('/') {
                continue;
            }
        }
        let name = tok.rsplit('/').next().unwrap_or(tok);
        if WRAPPERS.contains(&name) {
            continue;
        }
        // 第一个"既不是赋值也不是包装器"的词就是可执行名,判完即止——后面全是参数,
        // 再往下找会把 `git commit -m "ask claude"` 里的 claude 也算上。
        return AI_CLI_NAMES.contains(&name);
    }
    false
}

#[cfg(test)]
mod ai_cli_tests {
    use super::is_ai_cli_command;

    #[test]
    fn recognises_ai_cli_launches() {
        for line in [
            "claude",
            "claude -c",
            "codex",
            "/usr/local/bin/claude --resume",
            "./codex",
            "FOO=1 BAR=2 claude",
            "npx @anthropic-ai/claude-code",
            "env claude -c",
            "aider --model gpt-4",
        ] {
            assert!(is_ai_cli_command(line), "应识别为 AI CLI:{line:?}");
        }
    }

    /// 参数里出现 AI CLI 的名字不算——否则 `git commit -m "ask claude"` 会把普通标签
    /// 误标成 AI 标签,BEL 噪音又全回来了。
    #[test]
    fn does_not_fire_on_the_name_appearing_as_an_argument() {
        for line in [
            "",
            "ls",
            "git commit -m \"ask claude\"",
            "echo codex",
            "vim claude.py",
            "grep claude *.log",
            "which claude",
        ] {
            assert!(!is_ai_cli_command(line), "不该识别为 AI CLI:{line:?}");
        }
    }
}
