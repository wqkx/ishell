//! 查找/替换与跳转行浮层：从 editor 拆出，行为不变。
//! 供虚拟编辑器调用；仅物理迁移以缩小 mod.rs。

use egui::RichText;

use super::Editor;
use crate::theme::Palette;

// ———————————————————————— VSCode 风格查找/替换控件（两套编辑器共用） ————————————————————————

pub(super) enum FindOut {
    None,
    Goto(usize, usize),       // 选中并滚到该字节范围
    ReplaceOne(usize, usize), // 把该字节范围替换为 ed.replace（字面）
    ReplaceAll(String),       // 用新全文替换
}

/// 由查找选项构造正则（字面查找也走正则：escape + 可选 \b）。
pub(super) fn build_find_regex(
    pat: &str,
    case: bool,
    word: bool,
    regex_mode: bool,
) -> Option<regex::Regex> {
    let p = if regex_mode {
        pat.to_string()
    } else {
        let esc = regex::escape(pat);
        if word {
            format!(r"\b{esc}\b")
        } else {
            esc
        }
    };
    regex::RegexBuilder::new(&p)
        .case_insensitive(!case)
        .size_limit(1 << 24)
        .build()
        .ok()
}

/// 按需重算全部匹配（字节范围）；缓存签名（查找词+选项+内容长度）不变则跳过。
fn rebuild_matches(ed: &mut Editor) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ed.find.hash(&mut h);
    ed.find_case.hash(&mut h);
    ed.find_word.hash(&mut h);
    ed.find_regex.hash(&mut h);
    // 用内容版本号 vver（每次编辑 +1）而非 content.len()：否则「等长编辑/替换」后 sig 不变、
    // find_matches 不重算却已失效，导致「替换」改到错误字节范围、可损坏内容。
    ed.vver.hash(&mut h);
    let sig = h.finish();
    if sig == ed.find_sig {
        return;
    }
    ed.find_sig = sig;
    ed.find_matches.clear();
    if ed.find.is_empty() {
        return;
    }
    if let Some(re) = build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex) {
        for m in re.find_iter(&ed.content).take(200_000) {
            if m.end() > m.start() {
                ed.find_matches.push((m.start(), m.end()));
            }
        }
    }
}

fn nav_match(matches: &[(usize, usize)], caret: usize, forward: bool) -> Option<(usize, usize)> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        matches
            .iter()
            .find(|&&(a, _)| a > caret)
            .copied()
            .or_else(|| matches.first().copied())
    } else {
        matches
            .iter()
            .rev()
            .find(|&&(a, _)| a < caret)
            .copied()
            .or_else(|| matches.last().copied())
    }
}

fn replace_all_content(ed: &Editor) -> Option<String> {
    let re = build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex)?;
    Some(if ed.find_regex {
        re.replace_all(&ed.content, ed.replace.as_str())
            .into_owned()
    } else {
        re.replace_all(&ed.content, regex::NoExpand(ed.replace.as_str()))
            .into_owned()
    })
}

fn find_toggle(ui: &mut egui::Ui, label: &str, on: bool, tip: &str) -> bool {
    let fill = if on {
        Palette::ACCENT_SOFT
    } else {
        egui::Color32::TRANSPARENT
    };
    let col = if on {
        Palette::ACCENT
    } else {
        Palette::TEXT_DIM
    };
    ui.add(
        egui::Button::new(RichText::new(label).size(12.0).color(col))
            .fill(fill)
            .corner_radius(4.0)
            .min_size(egui::vec2(24.0, 20.0)),
    )
    .on_hover_text(tip)
    .clicked()
}

/// 跳转到行浮层（顶部居中）；返回 Some(1 基行号) 表示跳转。
pub(super) fn goto_widget(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) -> Option<usize> {
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ed.goto_open = false;
        return None;
    }
    let mut out = None;
    egui::Area::new(text_id.with("goto"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 44.0))
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(Palette::PANEL_2)
                .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.visuals_mut().extreme_bg_color = egui::Color32::from_rgb(252, 252, 250);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(crate::i18n::tr("跳转到行", "Go to line"))
                                .color(Palette::TEXT_DIM)
                                .size(12.0),
                        );
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut ed.goto_text)
                                .desired_width(80.0)
                                .hint_text("1.."),
                        );
                        if ed.goto_focus {
                            r.request_focus();
                            ed.goto_focus = false;
                        }
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if enter || ui.button(crate::i18n::tr("跳转", "Go")).clicked() {
                            if let Ok(n) = ed.goto_text.trim().parse::<usize>() {
                                out = Some(n.max(1));
                            }
                            ed.goto_open = false;
                            ed.goto_text.clear();
                        }
                    });
                });
        });
    out
}

/// VSCode 风格查找/替换浮层（右上角）；`caret_byte` 为当前光标字节位置；返回要应用的动作。
pub(super) fn find_widget(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    text_id: egui::Id,
    caret_byte: usize,
) -> FindOut {
    use egui_phosphor::regular as icon;
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ed.show_find = false;
        return FindOut::None;
    }
    rebuild_matches(ed);
    let total = ed.find_matches.len();
    let cur_idx = ed
        .find_matches
        .iter()
        .position(|&(a, b)| caret_byte >= a && caret_byte <= b);
    let mut out = FindOut::None;
    egui::Area::new(text_id.with("find_widget"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 44.0)) // 标签栏之下，避免遮住保存/查找
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(Palette::PANEL_2)
                .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    // 行高较矮不易操作：加高（24 → 28，比初版 31 略收 ~10%）。
                    ui.spacing_mut().interact_size.y = 28.0;
                    ui.spacing_mut().item_spacing = egui::vec2(5.0, 5.0);
                    // 输入框用近白底，和卡片/边框区分开（默认会和 PANEL_2 同色看不清）
                    ui.visuals_mut().extreme_bg_color = egui::Color32::from_rgb(252, 252, 250);
                    ui.visuals_mut().widgets.inactive.bg_stroke =
                        egui::Stroke::new(1.0, Palette::BORDER);
                    ui.visuals_mut().widgets.hovered.bg_stroke =
                        egui::Stroke::new(1.0, Palette::TEXT_DIM);
                    ui.horizontal(|ui| {
                        let exp = if ed.replace_open {
                            icon::CARET_DOWN
                        } else {
                            icon::CARET_RIGHT
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(exp).size(12.0).color(Palette::TEXT_DIM),
                                )
                                .frame(false)
                                .min_size(egui::vec2(20.0, 20.0)),
                            )
                            .on_hover_text(crate::i18n::tr("展开/收起替换", "Toggle replace"))
                            .clicked()
                        {
                            ed.replace_open = !ed.replace_open;
                        }
                        let fr = ui.add(
                            egui::TextEdit::singleline(&mut ed.find)
                                .desired_width(150.0)
                                // 单行 TextEdit 的高度 = 字体行高 + 2×margin.y（不看 interact_size）。
                                // 字体较初版小 2 号（15→13），margin 略收，行高随之下降 ~10%。
                                .font(egui::FontId::proportional(13.0))
                                .margin(egui::Margin::symmetric(6, 6))
                                .hint_text(crate::i18n::tr("查找", "Find")),
                        );
                        if ed.find_focus {
                            fr.request_focus();
                            ed.find_focus = false;
                        }
                        if find_toggle(
                            ui,
                            "Aa",
                            ed.find_case,
                            crate::i18n::tr("区分大小写", "Match case"),
                        ) {
                            ed.find_case = !ed.find_case;
                        }
                        if find_toggle(
                            ui,
                            "ab",
                            ed.find_word,
                            crate::i18n::tr("全字匹配", "Whole word"),
                        ) {
                            ed.find_word = !ed.find_word;
                        }
                        if find_toggle(
                            ui,
                            ".*",
                            ed.find_regex,
                            crate::i18n::tr("正则表达式", "Regex"),
                        ) {
                            ed.find_regex = !ed.find_regex;
                        }
                        let count = if ed.find.is_empty() {
                            String::new()
                        } else if total == 0 {
                            crate::i18n::tr("无结果", "No results").into()
                        } else if let Some(i) = cur_idx {
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => {
                                    format!("第 {} 项，共 {} 项", i + 1, total)
                                }
                                crate::i18n::Lang::En => format!("{} of {}", i + 1, total),
                            }
                        } else {
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("共 {} 项", total),
                                crate::i18n::Lang::En => format!("{} results", total),
                            }
                        };
                        ui.label(RichText::new(count).color(Palette::TEXT_DIM).size(11.0));
                        // 上一个/下一个/关闭：原来 frame(false) 且无 min_size，点击区只有 ~12px
                        // 的字形本身，两个箭头又仅隔 5px，极难点中（尤其「上一个」）。给足
                        // min_size 点击区（约 +30%）并放大图标，既解决点不中、也符合放大诉求。
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(icon::ARROW_UP)
                                        .size(16.0)
                                        .color(Palette::TEXT_DIM),
                                )
                                .frame(false)
                                .min_size(egui::vec2(28.0, 28.0)),
                            )
                            .on_hover_text(crate::i18n::tr("上一个", "Previous"))
                            .clicked()
                        {
                            if let Some((a, b)) = nav_match(&ed.find_matches, caret_byte, false) {
                                out = FindOut::Goto(a, b);
                            }
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(icon::ARROW_DOWN)
                                        .size(16.0)
                                        .color(Palette::TEXT_DIM),
                                )
                                .frame(false)
                                .min_size(egui::vec2(28.0, 28.0)),
                            )
                            .on_hover_text(crate::i18n::tr("下一个", "Next"))
                            .clicked()
                        {
                            if let Some((a, b)) = nav_match(&ed.find_matches, caret_byte, true) {
                                out = FindOut::Goto(a, b);
                            }
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(icon::X).size(16.0).color(Palette::TEXT_DIM),
                                )
                                .frame(false)
                                .min_size(egui::vec2(28.0, 28.0)),
                            )
                            .on_hover_text(crate::i18n::tr("关闭 (Esc)", "Close (Esc)"))
                            .clicked()
                        {
                            ed.show_find = false;
                        }
                    });
                    if ed.replace_open {
                        ui.horizontal(|ui| {
                            // 与查找行的折叠箭头同宽的占位（同为首项 → 与查找输入框左对齐）
                            ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.replace)
                                    .desired_width(150.0)
                                    .font(egui::FontId::proportional(13.0))
                                    .margin(egui::Margin::symmetric(6, 6))
                                    .hint_text(crate::i18n::tr("替换", "Replace")),
                            );
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(icon::ARROW_BEND_DOWN_LEFT)
                                            .size(17.0)
                                            .color(Palette::TEXT_DIM),
                                    )
                                    .frame(false)
                                    .min_size(egui::vec2(28.0, 28.0)),
                                )
                                .on_hover_text(crate::i18n::tr("替换", "Replace"))
                                .clicked()
                            {
                                if let Some(i) = cur_idx {
                                    let (a, b) = ed.find_matches[i];
                                    out = FindOut::ReplaceOne(a, b);
                                } else if let Some((a, b)) =
                                    nav_match(&ed.find_matches, caret_byte, true)
                                {
                                    out = FindOut::Goto(a, b);
                                }
                            }
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(icon::ARROWS_DOWN_UP)
                                            .size(17.0)
                                            .color(Palette::TEXT_DIM),
                                    )
                                    .frame(false)
                                    .min_size(egui::vec2(28.0, 28.0)),
                                )
                                .on_hover_text(crate::i18n::tr("全部替换", "Replace all"))
                                .clicked()
                                && total > 0
                            {
                                if let Some(newc) = replace_all_content(ed) {
                                    out = FindOut::ReplaceAll(newc);
                                }
                            }
                        });
                    }
                });
        });
    out
}
