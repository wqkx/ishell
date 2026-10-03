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
pub(super) fn rebuild_matches(ed: &mut Editor) {
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
        let bytes = ed.content.as_bytes();
        // 混合行尾文件里的 `\r\n` 是一个行尾，选区 / 替换不能把它拆开：正则的 `.`、`\s` 会
        // 吃进 `\r`。终点落在两者之间就退到 `\r` 之前，起点落在两者之间就把 `\r` 带上。
        let splits = |p: usize| p > 0 && bytes[p - 1] == b'\r' && bytes.get(p) == Some(&b'\n');
        for m in re.find_iter(&ed.content).take(200_000) {
            let (mut a, mut b) = (m.start(), m.end());
            if splits(b) {
                b -= 1;
            }
            if splits(a) {
                a -= 1;
            }
            if b > a {
                ed.find_matches.push((a, b));
            }
        }
    }
}

pub(super) fn nav_match(
    matches: &[(usize, usize)],
    caret: usize,
    forward: bool,
) -> Option<(usize, usize)> {
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
        // 反向必须以匹配**终点**判定：跳转后光标停在所选匹配的末尾（vcaret = b），
        // 若按起点 `a < caret` 判定，当前匹配的起点永远小于光标——每按「上一个」
        // 都命中自己、原地不动。`b < caret` 跳过光标所在的匹配，落到真正的上一处。
        matches
            .iter()
            .rev()
            .find(|&&(_, b)| b < caret)
            .copied()
            .or_else(|| matches.last().copied())
    }
}

/// 把一处匹配的替换文本追加到 `out`：正则模式展开捕获组（`$1` 等），字面模式原样。
fn push_replacement(ed: &Editor, caps: &regex::Captures, out: &mut String) {
    if ed.find_regex {
        caps.expand(&ed.replace, out);
    } else {
        out.push_str(&ed.replace);
    }
}

/// 「替换」单处：`content[a..b]` 这处匹配替换后的文本。
///
/// 在**原文的那个位置**上重新匹配来取捕获组，不能把命中的子串抠出来单独匹配——依赖上下文
/// 的断言（`\B`、`^`、`\b`）在孤立子串上不成立，结果就是点了「替换」什么都没换。
pub(super) fn replace_one_text(ed: &Editor, a: usize, b: usize) -> String {
    let mut out = String::new();
    let caps = build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex)
        .and_then(|re| re.captures_at(&ed.content, a))
        .filter(|c| c.get(0).is_some_and(|m| m.start() == a && m.end() == b));
    match caps {
        Some(caps) => push_replacement(ed, &caps, &mut out),
        // 对不上（匹配列表已陈旧）：退回字面替换串，至少不 panic、不乱展开
        None => out.push_str(&ed.replace),
    }
    out
}

/// 替换完一处后要选中的下一处：起点不早于 `pos`（替换文本的末尾）的第一处，没有则回绕。
/// 不能写成「起点 > pos-1」：pos 为 0（在文首替换成空串）时会把正在 0 的那一处跳过去。
pub(super) fn match_after_replace(matches: &[(usize, usize)], pos: usize) -> Option<(usize, usize)> {
    matches
        .iter()
        .find(|&&(a, _)| a >= pos)
        .or(matches.first())
        .copied()
}

/// 「全部替换」后的全文。只替换**非空**匹配——与 `rebuild_matches` 的计数口径一致：
/// 界面上显示几项，就只动那几项（`a*` 这类能空匹配的模式否则会把替换串塞满全文）。
fn replace_all_content(ed: &Editor) -> Option<String> {
    let re = build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex)?;
    let mut out = String::with_capacity(ed.content.len());
    let mut last = 0;
    for caps in re.captures_iter(&ed.content) {
        let Some(m) = caps.get(0).filter(|m| m.end() > m.start()) else {
            continue;
        };
        out.push_str(&ed.content[last..m.start()]);
        push_replacement(ed, &caps, &mut out);
        last = m.end();
    }
    out.push_str(&ed.content[last..]);
    Some(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ed_find(content: &str, find: &str, replace: &str, regex: bool) -> Editor {
        let mut ed = Editor::new("/tmp/a.txt".into(), content.into());
        super::super::v_recompute(&mut ed);
        ed.find = find.into();
        ed.replace = replace.into();
        ed.find_regex = regex;
        ed.find_case = true;
        ed
    }

    /// 混合行尾文件里，正则的 `.` 会吃进 CRLF 的 `\r`：匹配不能把 `\r\n` 拆开——选区终点
    /// 落进去，收起选区再打字就把字插到 `\r` 后面；替换则悄悄把这一行改成 LF。
    #[test]
    fn regex_matches_never_split_a_crlf_line_end() {
        let mut ed = ed_find("foo bar\r\nfoo\n", "foo.*", "X", true);
        rebuild_matches(&mut ed);
        assert_eq!(ed.find_matches, [(0, 7), (9, 12)]);
        let mut ed = ed_find("a\r\nb", "\\nb", "", true);
        rebuild_matches(&mut ed);
        assert_eq!(ed.find_matches, [(1, 4)], "起点要把 \\r 带上");
    }

    /// 「全部替换」只能动界面上数得出来的那些匹配。计数时空匹配被丢掉了（`a*` 在每个
    /// 位置都能空匹配），替换时却没丢——显示 1 项，实际把替换串塞满了全文。
    #[test]
    fn replace_all_only_touches_the_counted_matches() {
        let mut ed = ed_find("baab", "a*", "X", true);
        rebuild_matches(&mut ed);
        assert_eq!(ed.find_matches, vec![(1, 3)]);
        assert_eq!(replace_all_content(&ed).as_deref(), Some("bXb"));
        // 捕获组照常展开；字面模式里的 `$1` 就是字面
        let ed = ed_find("k=v; a=b", r"(\w)=(\w)", "$2=$1", true);
        assert_eq!(replace_all_content(&ed).as_deref(), Some("v=k; b=a"));
        let ed = ed_find("a.b", ".", "$1", false);
        assert_eq!(replace_all_content(&ed).as_deref(), Some("a$1b"));
    }

    /// 单个「替换」要在**原文的那个位置**上重新匹配，不能把命中的子串抠出来单独匹配：
    /// 依赖上下文的断言（`\B`、`^`、`\b`）在孤立子串上会不成立，于是点「替换」什么都没换。
    #[test]
    fn replacing_one_match_keeps_its_context() {
        let ed = ed_find("xfoo", r"\Bfoo", "bar", true);
        assert_eq!(replace_one_text(&ed, 1, 4), "bar");
        let ed = ed_find("ab12", r"(\d)(\d)", "$2$1", true);
        assert_eq!(replace_one_text(&ed, 2, 4), "21");
        let ed = ed_find("a.b", ".", "$1", false);
        assert_eq!(replace_one_text(&ed, 1, 2), "$1");
    }

    /// 替换后选中的是「替换文本之后」的第一处。替换发生在文首且替换成空串时，
    /// 新的第一处就在 0，不能被跳过。
    #[test]
    fn next_match_after_a_replacement_at_the_very_start() {
        let m = [(0, 2), (5, 7)];
        assert_eq!(match_after_replace(&m, 0), Some((0, 2)));
        assert_eq!(match_after_replace(&m, 3), Some((5, 7)));
        assert_eq!(match_after_replace(&m, 8), Some((0, 2))); // 回绕
        assert_eq!(match_after_replace(&[], 0), None);
    }

    /// 「上一个」必须跳过光标所在的匹配（跳转后光标停在匹配末尾）：
    /// 此前按起点 `a < caret` 判定会命中自己，表现为「上一个不管用」。
    #[test]
    fn nav_prev_skips_match_under_caret() {
        let m = [(10, 15), (20, 25), (30, 35)];
        // 光标在 [20,25] 末尾（选中该项后的落点）→ 上一个应到 [10,15]
        assert_eq!(nav_match(&m, 25, false), Some((10, 15)));
        // 光标在 [20,25] 起点 → 同样是上一处（当前项视为已选中）
        assert_eq!(nav_match(&m, 20, false), Some((10, 15)));
        // 光标在两匹配之间 → 最近的前一处
        assert_eq!(nav_match(&m, 28, false), Some((20, 25)));
        // 光标在匹配内部 → 上一处（与 VSCode 一致）
        assert_eq!(nav_match(&m, 22, false), Some((10, 15)));
        // 文首 → 回绕到最后一处
        assert_eq!(nav_match(&m, 0, false), Some((30, 35)));
        // 「下一个」行为不变
        assert_eq!(nav_match(&m, 25, true), Some((30, 35)));
        assert_eq!(nav_match(&m, 35, true), Some((10, 15))); // 回绕到首
        assert_eq!(nav_match(&[], 5, false), None);
    }
}
