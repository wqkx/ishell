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
        for ev in events {
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
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    out.extend_from_slice(t.as_bytes());
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
                    let plain =
                        !modifiers.ctrl && !modifiers.alt && !modifiers.command && !modifiers.shift;
                    if !alt && plain && matches!(key, Key::ArrowUp | Key::ArrowDown) {
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
