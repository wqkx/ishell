use egui::RichText;

use super::super::find::{
    find_widget, goto_widget, match_after_replace, rebuild_matches, replace_one_text, FindOut,
};
use super::super::Editor;
use super::edit::{normalize_paste, v_apply, v_insert, v_mixed_eol, v_unify_eol};
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
                    // —— 字号缩放：A- / 当前值 / A+（点击数字恢复默认）。仅影响编辑器，持久化。——
                    let base = egui::TextStyle::Monospace.resolve(ui.style()).size; // 未设置时的默认字号
                    let cur = ed.font_pt.unwrap_or(base);
                    let set_font = |ed: &mut Editor, pt: f32| {
                        let n = pt.clamp(8.0, 40.0);
                        ed.font_pt = Some(n);
                        crate::store::save_editor_font(n);
                    };
                    ui.add_space(10.0);
                    if ui
                        .add(egui::Label::new(RichText::new("A-").color(Palette::TEXT_DIM).size(12.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr("缩小字号", "Decrease font size"))
                        .clicked()
                    {
                        set_font(ed, cur - 1.0);
                    }
                    ui.add_space(5.0);
                    if ui
                        .add(egui::Label::new(RichText::new(format!("{}", cur.round() as i32)).color(Palette::TEXT_DIM).size(11.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr("点击恢复默认字号", "Click to reset font size"))
                        .clicked()
                    {
                        set_font(ed, base);
                    }
                    ui.add_space(5.0);
                    if ui
                        .add(egui::Label::new(RichText::new("A+").color(Palette::TEXT_DIM).size(13.0)).sense(egui::Sense::click()))
                        .on_hover_text(crate::i18n::tr("放大字号", "Increase font size"))
                        .clicked()
                    {
                        set_font(ed, cur + 1.0);
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
                    let caret = crate::ui::ime_safe::floor_boundary(&ed.content, ed.vcaret).max(lsx);
                    let col = ed.content[lsx..caret].chars().count() + 1;
                    ui.label(RichText::new(format!("Ln {}, Col {}", cl + 1, col)).color(Palette::TEXT_DIM).size(11.0));
                    ui.add_space(10.0);
                    // 行尾：点击切换 LF/CRLF。混合行尾（部分行 CRLF，行末标着 CR）时显示「混合」，
                    // 点开可统一成一种——否则选 LF 什么都不会发生（文件本来就按 LF 记），没有办法
                    // 把它变成纯 LF。
                    let eol_txt = match ed.eol() { crate::proto::Eol::Crlf => "CRLF", crate::proto::Eol::Lf => "LF" };
                    if v_mixed_eol(ed) {
                        ui.menu_button(RichText::new(crate::i18n::tr("混合", "Mixed")).color(Palette::WARN).size(11.0), |ui| {
                            ui.set_min_width(150.0);
                            let editable = !ed.is_readonly();
                            for (label, eol) in [
                                (crate::i18n::tr("统一为 LF", "Convert all to LF"), crate::proto::Eol::Lf),
                                (crate::i18n::tr("统一为 CRLF", "Convert all to CRLF"), crate::proto::Eol::Crlf),
                            ] {
                                if ui.add_enabled(editable, egui::Button::new(label)).clicked() {
                                    v_unify_eol(ed, eol);
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text(crate::i18n::tr(
                            "行尾不统一：标着 CR 的行是 CRLF，其余是 LF。保存时各行保持原样；点击可统一成一种。",
                            "Mixed line endings: lines marked CR end in CRLF, the rest in LF. Saving keeps each line as is; click to convert.",
                        ));
                    } else if ui.add(egui::Label::new(RichText::new(eol_txt).color(Palette::TEXT_DIM).size(11.0)).sense(egui::Sense::click())).on_hover_text(crate::i18n::tr("点击切换行尾 LF/CRLF", "Click to toggle LF/CRLF")).clicked() {
                        let n = match ed.eol() { crate::proto::Eol::Crlf => crate::proto::Eol::Lf, crate::proto::Eol::Lf => crate::proto::Eol::Crlf };
                        ed.set_eol(n);
                    }
                    ui.add_space(10.0);
                    // 编码菜单：两个不同的动作，必须分开——
                    //   · 按编码重新打开：文件字节不变，换一种编码去**读**（自动识别错了、显示乱码时用）；
                    //   · 保存为编码：内容不变，下次保存时换一种编码去**写**（转换文件编码）。
                    // 原先只有后者：文件被认错编码时，用户选了正确的编码，显示纹丝不动，
                    // 一保存反而把乱码按新编码写了回去。
                    const ENCODINGS: [&str; 9] = ["UTF-8", crate::textcodec::UTF8_BOM, "GBK", "GB18030", "Big5", "Shift_JIS", "EUC-KR", "windows-1252", "ISO-8859-1"];
                    ui.menu_button(RichText::new(ed.encoding()).color(Palette::TEXT_DIM).size(11.0), |ui| {
                        ui.set_min_width(170.0);
                        ui.menu_button(crate::i18n::tr("按编码重新打开", "Reopen with encoding"), |ui| {
                            ui.set_min_width(120.0);
                            for enc in ENCODINGS {
                                if ui.button(enc).clicked() {
                                    ed.reopen_req = Some(enc.to_string());
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text(crate::i18n::tr(
                            "显示乱码时用：按所选编码重新读取文件（文件本身不变）。\n需要先保存或撤销未保存的修改。",
                            "For garbled text: re-read the file using this encoding (the file is not changed).\nRequires no unsaved changes.",
                        ));
                        ui.menu_button(crate::i18n::tr("保存为编码", "Save with encoding"), |ui| {
                            ui.set_min_width(120.0);
                            for enc in ENCODINGS {
                                if ui.selectable_label(ed.encoding() == enc, enc).clicked() {
                                    ed.set_encoding(enc.to_string());
                                    ui.close();
                                }
                            }
                        })
                        .response
                        .on_hover_text(crate::i18n::tr(
                            "转换文件编码：内容不变，下次保存时按所选编码写入。",
                            "Convert the file: content is kept and written in this encoding on next save.",
                        ));
                    })
                    .response
                    .on_hover_text(crate::i18n::tr("编码：重新打开 / 保存为", "Encoding: reopen / save as"));
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
                let rep = replace_one_text(ed, a, b);
                let rep_end = a + rep.len();
                v_apply(ed, a, b - a, &rep);
                // VSCode 行为：替换后立即选中**下一处**匹配——光标落回匹配范围内，
                // 计数即时刷新为「第 X 项」；此前光标停在被替换文本末尾（不在任何匹配内），
                // 计数退化为总数，用户得再点一次「下一个」才恢复。
                ed.find_sig = 0; // 强制 rebuild_matches 重算（签名里含内容版本）
                rebuild_matches(ed);
                if let Some((na, nb)) = match_after_replace(&ed.find_matches, rep_end) {
                    ed.vsel = Some(na);
                    ed.vcaret = nb;
                    ed.pending_scroll = Some(v_line_of(ed, nb));
                } else {
                    ed.pending_scroll = Some(v_line_of(ed, ed.vcaret));
                }
            }
            FindOut::ReplaceAll(newc) => {
                let old = ed.content.len();
                // 整篇重写后 v_apply 会把光标放到文末；留在原来那一行，视图才不会跳走
                let line = v_line_of(ed, ed.vcaret);
                v_apply(ed, 0, old, &newc);
                let line = line.min(ed.vlines.len().saturating_sub(1));
                ed.vcaret = v_line_range(ed, line).0;
                ed.pending_scroll = Some(line);
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
            // 冻结选区是菜单打开那一刻的偏移，菜单开着时内容可能已变：收敛到合法边界再切
            let (a, b) = crate::ui::ime_safe::clamp_range(&ed.content, (a, b));
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
                let t = normalize_paste(&t); // 与 Ctrl+V 一致：CRLF 归一成 LF
                // 有冻结选区则替换它，否则插入到光标
                if let Some((a, b)) = ed.menu_sel.filter(|&(a, b)| b > a) {
                    let (a, b) = crate::ui::ime_safe::clamp_range(&ed.content, (a, b));
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
