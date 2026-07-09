use egui::RichText;

use super::super::find::{build_find_regex, find_widget, goto_widget, FindOut};
use super::super::Editor;
use super::edit::{v_apply, v_insert};
use super::geom::{v_line_of, v_line_range};
use crate::theme::Palette;
use crate::ui::highlight::Indent;

#[derive(Default)]
pub(super) struct ChromeActions {
    pub(super) do_copy: bool,
    pub(super) do_cut: bool,
    pub(super) do_paste: bool,
    pub(super) do_selall: bool,
}

pub(super) fn show_status_and_find(ui: &mut egui::Ui, ed: &mut Editor, text_id: egui::Id) {
    // 底部状态栏（仿小文件编辑器）：缩进可切换（矩形按钮、贴左）+ 语言贴右。
    egui::Panel::bottom("editor_status_v")
        .frame(egui::Frame::new().fill(Palette::PANEL_2).inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.scope(|ui| {
                    let v = ui.visuals_mut();
                    v.widgets.inactive.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.hovered.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.active.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.open.corner_radius = egui::CornerRadius::ZERO;
                    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                    v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0);
                    // 字号与状态栏其它项一致（11），否则默认按钮字号显得突兀地大
                    ui.menu_button(RichText::new(format!("{} {}", crate::i18n::tr("缩进", "Indent"), ed.indent.label())).size(11.0).color(Palette::TEXT_DIM), |ui| {
                        ui.set_min_width(120.0);
                        for ind in [Indent::Spaces(2), Indent::Spaces(4), Indent::Tab] {
                            if ui.selectable_label(ed.indent == ind, RichText::new(ind.label()).size(12.0)).clicked() {
                                ed.indent = ind;
                                ui.close();
                            }
                        }
                    });
                    // 自动换行开关：开启时长行折行、无横向滚动
                    ui.add_space(6.0);
                    let wrap_col = if ed.wrap { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(crate::i18n::tr("换行", "Wrap")).color(wrap_col).size(11.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr("点击切换自动换行", "Toggle word wrap"))
                        .clicked()
                    {
                        ed.wrap = !ed.wrap;
                        ed.vgoal_col = None; // 列语义改变，重置目标列
                    }
                });
                if !ed.status.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&ed.status).color(Palette::TEXT_DIM).size(11.0));
                }
                if ed.msel.len() > 1 {
                    ui.add_space(8.0);
                    let n = ed.msel.len();
                    let label = match crate::i18n::current() {
                        crate::i18n::Lang::En => format!("{n} cursors"),
                        _ => format!("{n} 光标"),
                    };
                    ui.label(RichText::new(label).color(Palette::ACCENT).size(11.0));
                }
                // 括号 lint 概述（不匹配时红字）
                if let Some(msg) = &ed.lint_msg {
                    ui.add_space(8.0);
                    ui.label(RichText::new(msg).color(Palette::DANGER).size(11.0));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(ed.language.as_str()).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 大文件只读徽标：点击可解除（整文件已在内存，编辑仍可能占较多 RAM）
                    if ed.readonly && !ed.follow {
                        if ui
                            .add(egui::Label::new(RichText::new(crate::i18n::tr("只读", "Read-only")).color(Palette::WARN).size(11.0)).sense(egui::Sense::click()))
                            .on_hover_text(crate::i18n::tr(
                                "大文件默认只读（整文件已载入内存）。点击改为可编辑。",
                                "Large files open read-only (fully loaded). Click to enable editing.",
                            ))
                            .clicked()
                        {
                            ed.unlock_req = true;
                        }
                        ui.add_space(10.0);
                    }
                    // 跟随（tail -f）：↧ 图标，开启时珊瑚色；点击由 app 层切换（需发起 SFTP 命令）
                    let f_col = if ed.follow { Palette::ACCENT } else { Palette::TEXT_DIM };
                    if ui
                        .add(egui::Label::new(RichText::new(format!("{} {}", egui_phosphor::regular::ARROW_LINE_DOWN, crate::i18n::tr("跟随", "Follow"))).color(f_col).size(11.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr(
                            "跟随文件末尾（tail -f）：自动追加新内容并滚到底，开启期间只读。\n拖选/查看历史时暂停滚动，Ctrl+End 回到底部恢复跟随。",
                            "Follow file tail (tail -f): auto-append & scroll, read-only while on.\nScrolling pauses while selecting/browsing; Ctrl+End resumes.",
                        ))
                        .clicked()
                    {
                        ed.follow_req = true;
                    }
                    ui.add_space(10.0);
                    // 光标位置 Ln:Col（主光标，1 基；列按字符计）
                    let cl = v_line_of(ed, ed.vcaret);
                    let (lsx, _) = v_line_range(ed, cl);
                    let col = ed.content[lsx..ed.vcaret.min(ed.content.len())].chars().count() + 1;
                    ui.label(RichText::new(format!("Ln {}, Col {}", cl + 1, col)).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 行尾：点击切换 LF/CRLF
                    let eol_txt = match ed.eol() { crate::proto::Eol::Crlf => "CRLF", crate::proto::Eol::Lf => "LF" };
                    if ui.add(egui::Label::new(RichText::new(eol_txt).color(Palette::TEXT_DIM).size(11.0)).sense(egui::Sense::click())).on_hover_text(crate::i18n::tr("点击切换行尾 LF/CRLF", "Click to toggle LF/CRLF")).clicked() {
                        let n = match ed.eol() { crate::proto::Eol::Crlf => crate::proto::Eol::Lf, crate::proto::Eol::Lf => crate::proto::Eol::Crlf };
                        ed.set_eol(n);
                    }
                    ui.add_space(10.0);
                    // 编码：点击从菜单选择（保存时按所选编码写回）
                    ui.menu_button(RichText::new(ed.encoding()).color(Palette::TEXT_DIM).size(11.0), |ui| {
                        ui.set_min_width(120.0);
                        for enc in ["UTF-8", "GBK", "GB18030", "Big5", "Shift_JIS", "EUC-KR", "windows-1252", "ISO-8859-1"] {
                            if ui.selectable_label(ed.encoding() == enc, enc).clicked() {
                                ed.set_encoding(enc.to_string());
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text(crate::i18n::tr("点击选择保存编码", "Click to choose save encoding"));
                });
            });
        });

    // 查找/替换：VSCode 风格浮层（共用 find_widget），按字节定位/替换、可撤销。
    if ed.show_find {
        match find_widget(ui, ed, text_id, ed.vcaret) {
            FindOut::Goto(a, b) => {
                ed.vsel = Some(a);
                ed.vcaret = b;
                ed.pending_scroll = Some(v_line_of(ed, b));
            }
            FindOut::ReplaceOne(a, b) => {
                // 与「全部替换」保持一致：正则模式下展开捕获组（$1 等），字面模式直接用替换串。
                let rep: String = if ed.find_regex {
                    match build_find_regex(&ed.find, ed.find_case, ed.find_word, ed.find_regex) {
                        Some(re) => re
                            .replace(&ed.content[a..b], ed.replace.as_str())
                            .into_owned(),
                        None => ed.replace.clone(),
                    }
                } else {
                    ed.replace.clone()
                };
                v_apply(ed, a, b - a, &rep);
                ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
            }
            FindOut::ReplaceAll(newc) => {
                let old = ed.content.len();
                v_apply(ed, 0, old, &newc);
                ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
            }
            FindOut::None => {}
        }
    }
    // 跳转到行
    if ed.goto_open {
        if let Some(n) = goto_widget(ui, ed, text_id) {
            let line = (n - 1).min(ed.vlines.len().saturating_sub(1));
            ed.vcaret = v_line_range(ed, line).0;
            ed.vsel = None;
            ed.vgoal_col = None;
            ed.pending_scroll = Some(line);
        }
    }
}

pub(super) fn apply_context_menu_actions(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    actions: ChromeActions,
) {
    let ChromeActions {
        do_copy,
        do_cut,
        do_paste,
        do_selall,
    } = actions;
    // 右键菜单动作（闭包外应用）
    if do_selall {
        ed.vsel = Some(0);
        ed.vcaret = ed.content.len();
    }
    // 复制/剪切用「冻结的右键选区」(menu_sel)，避免右键折叠选区后复制不到
    if do_copy || do_cut {
        if let Some((a, b)) = ed.menu_sel {
            let (a, b) = (a.min(ed.content.len()), b.min(ed.content.len()));
            if b > a {
                ui.ctx().copy_text(ed.content[a..b].to_string());
                if do_cut {
                    v_apply(ed, a, b - a, "");
                    ed.vgoal_col = None;
                }
            }
        }
    }
    if do_paste {
        if let Some(t) = arboard::Clipboard::new()
            .ok()
            .and_then(|mut c| c.get_text().ok())
        {
            if !t.is_empty() {
                // 有冻结选区则替换它，否则插入到光标
                if let Some((a, b)) = ed.menu_sel.filter(|&(a, b)| b > a) {
                    let (a, b) = (a.min(ed.content.len()), b.min(ed.content.len()));
                    v_apply(ed, a, b - a, &t);
                } else {
                    v_insert(ed, &t);
                }
                ed.vgoal_col = None;
            }
        }
    }
    if do_copy || do_cut || do_paste {
        ed.menu_sel = None;
    }
}
