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
        let alt = self.parser.screen().alternate_screen();
        if alt {
            self.input_line.clear();
            self.hist = None;
        }
        for ev in events {
            match ev {
                egui::Event::Text(t) => {
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
                            Key::Enter => self.commit_line(),
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
