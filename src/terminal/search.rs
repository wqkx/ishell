//! Terminal content search state and UI.

use egui::Key;

use super::Terminal;

#[derive(Default)]
pub(super) struct Find {
    pub(super) query: String,
    pub(super) hits: Vec<usize>, // 命中行的绝对行号（顶部为 0）
    pub(super) cur: usize,
    pub(super) focus: bool,
    pub(super) case: bool,
    pub(super) regex: bool,
    pub(super) word: bool,
    pub(super) bad_re: bool,
}

pub(super) enum FindAction {
    None,
    Search,
    Step(i32),
    Close,
}

pub(super) fn build_search_regex(f: &Find) -> Option<regex::Regex> {
    if f.query.is_empty() {
        return None;
    }
    let pat = if f.regex {
        f.query.clone()
    } else {
        regex::escape(&f.query)
    };
    let pat = if f.word {
        format!(r"\b(?:{pat})\b")
    } else {
        pat
    };
    regex::RegexBuilder::new(&pat)
        .case_insensitive(!f.case)
        .build()
        .ok()
}

impl Terminal {
    pub(super) fn run_search(&mut self) {
        let empty = self.find.as_ref().is_none_or(|f| f.query.is_empty());
        if empty {
            if let Some(f) = &mut self.find {
                f.hits.clear();
                f.bad_re = false;
            }
            self.search_hl = None;
            return;
        }

        let re = self.find.as_ref().and_then(build_search_regex);
        if let Some(f) = &mut self.find {
            f.bad_re = re.is_none();
        }
        let Some(re) = re else {
            return;
        };

        let lines = self.collect_lines();
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| re.is_match(l))
            .map(|(i, _)| i)
            .collect();
        if let Some(f) = &mut self.find {
            f.hits = hits;
            f.cur = 0;
        }
        self.jump_to_current();
    }

    pub(super) fn search_step(&mut self, dir: i32) {
        if let Some(f) = &mut self.find {
            let n = f.hits.len();
            if n == 0 {
                return;
            }
            f.cur = ((f.cur as i32 + dir).rem_euclid(n as i32)) as usize;
        }
        self.jump_to_current();
    }

    pub(super) fn jump_to_current(&mut self) {
        let line_idx = match &self.find {
            Some(f) if !f.hits.is_empty() => f.hits[f.cur.min(f.hits.len() - 1)],
            _ => {
                self.search_hl = None;
                return;
            }
        };
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let sb = self.parser.screen().scrollback();
        let rows = self.rows as usize;
        let r = rows / 3;
        let start_idx = line_idx.saturating_sub(r);
        let off = sb.saturating_sub(start_idx);
        self.parser.screen_mut().set_scrollback(off);
        self.scrollback = off.min(sb);
        let win_start = sb.saturating_sub(self.scrollback);
        self.search_hl = line_idx.checked_sub(win_start).map(|r| r as u16);
    }

    pub(super) fn recompute_search_hl(&mut self) {
        let line_idx = match &self.find {
            Some(f) if !f.hits.is_empty() => f.hits[f.cur.min(f.hits.len() - 1)],
            _ => {
                self.search_hl = None;
                return;
            }
        };
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let sb = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(self.scrollback);
        let win_start = sb.saturating_sub(self.scrollback);
        self.search_hl = match line_idx.checked_sub(win_start) {
            Some(r) if (r as u16) < self.rows => Some(r as u16),
            _ => None,
        };
    }

    pub(super) fn draw_find_bar(&mut self, ui: &mut egui::Ui) -> FindAction {
        use egui_phosphor::regular as icon;
        let mut action = FindAction::None;
        egui::Frame::new()
            .fill(crate::theme::Palette::PANEL_2)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let f = self.find.as_mut().unwrap();
                    ui.label(
                        egui::RichText::new(icon::MAGNIFYING_GLASS)
                            .color(crate::theme::Palette::TEXT_DIM),
                    );
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut f.query)
                            .desired_width(180.0)
                            .hint_text(crate::i18n::tr("查找终端内容", "Find in terminal")),
                    );
                    if f.focus {
                        resp.request_focus();
                        f.focus = false;
                    }
                    if resp.changed() {
                        action = FindAction::Search;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        action = FindAction::Step(1);
                        resp.request_focus();
                    }
                    let tgl = |ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str| -> bool {
                        let col = if *on {
                            crate::theme::Palette::ACCENT
                        } else {
                            crate::theme::Palette::TEXT_DIM
                        };
                        let clicked = ui
                            .add(
                                egui::Button::new(egui::RichText::new(label).size(12.0).color(col))
                                    .frame(false)
                                    .min_size(egui::vec2(20.0, 18.0)),
                            )
                            .on_hover_text(tip)
                            .clicked();
                        if clicked {
                            *on = !*on;
                        }
                        clicked
                    };
                    if tgl(
                        ui,
                        &mut f.case,
                        "Aa",
                        crate::i18n::tr("区分大小写", "Match case"),
                    ) {
                        action = FindAction::Search;
                    }
                    if tgl(
                        ui,
                        &mut f.regex,
                        ".*",
                        crate::i18n::tr("正则表达式", "Regex"),
                    ) {
                        action = FindAction::Search;
                    }
                    if tgl(
                        ui,
                        &mut f.word,
                        "\\b",
                        crate::i18n::tr("全字匹配", "Whole word"),
                    ) {
                        action = FindAction::Search;
                    }
                    let (cnt, cnt_col) = if f.bad_re {
                        (
                            crate::i18n::tr("正则错误", "bad regex").to_string(),
                            crate::theme::Palette::DANGER,
                        )
                    } else if f.hits.is_empty() {
                        ("0/0".to_string(), crate::theme::Palette::TEXT_DIM)
                    } else {
                        (
                            format!("{}/{}", f.cur + 1, f.hits.len()),
                            crate::theme::Palette::TEXT_DIM,
                        )
                    };
                    ui.label(egui::RichText::new(cnt).color(cnt_col).size(11.0));
                    if ui
                        .button(icon::CARET_UP)
                        .on_hover_text(crate::i18n::tr("上一个", "Prev"))
                        .clicked()
                    {
                        action = FindAction::Step(-1);
                    }
                    if ui
                        .button(icon::CARET_DOWN)
                        .on_hover_text(crate::i18n::tr("下一个", "Next"))
                        .clicked()
                    {
                        action = FindAction::Step(1);
                    }
                    if ui.button(icon::X).clicked() {
                        action = FindAction::Close;
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    action = FindAction::Close;
                }
            });
        action
    }
}
