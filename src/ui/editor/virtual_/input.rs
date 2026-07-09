use super::super::Editor;
use super::commands::{
    auto_close_for, line_comment, skip_closing_pair, v_delete_line, v_duplicate_line, v_move_line,
    v_toggle_comment,
};
use super::edit::{
    v_apply, v_backspace, v_block_indent, v_complete_accept, v_complete_refresh, v_ctrl_d,
    v_delete_fwd, v_delete_selection, v_delete_word, v_insert, v_move_doc, v_move_edge, v_move_h,
    v_move_v, v_move_word, v_multi_backspace, v_multi_copy, v_multi_delete, v_multi_move,
    v_multi_replace, v_newline_indent, v_redo, v_undo,
};
use super::geom::{next_char_boundary, v_sel_range};
use super::wrap::v_recompute;

pub(super) fn handle_input(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    focused: bool,
    page: isize,
) -> (bool, bool, bool) {
    let mut save = false;
    // ——— 输入（聚焦时）———
    // 视角跟随只看「光标是否真的移动」：按下任意键/输入法每帧都发事件会让旧的 moved 一直为真、
    // 导致不停把视角拉回光标、无法自由滚动。这里记下输入前的光标，处理完输入后用差异判断。
    let prev_caret = ed.vcaret;
    // 自绘 IME（同 egui 路径，绕开 egui Commit 门）：处理组字/提交，并在下方上报 o.ime 激活+定位候选框。
    if focused {
        let ime_events: Vec<egui::ImeEvent> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| {
                    if let egui::Event::Ime(ev) = e {
                        Some(ev.clone())
                    } else {
                        None
                    }
                })
                .collect()
        });
        for ev in ime_events {
            match ev {
                egui::ImeEvent::Enabled => {}
                egui::ImeEvent::Preedit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    // 组字是临时的：直接改 content、不入撤销栈
                    let (s, e) = ed
                        .vime_preedit
                        .take()
                        .or_else(|| v_sel_range(ed))
                        .unwrap_or((ed.vcaret, ed.vcaret));
                    let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                    ed.content.replace_range(s..e, &t);
                    let end = s + t.len();
                    ed.vcaret = end;
                    ed.vsel = None;
                    ed.msel.clear();
                    ed.vime_preedit = if t.is_empty() { None } else { Some((s, end)) };
                    v_recompute(ed);
                }
                egui::ImeEvent::Commit(t) => {
                    if t == "\n" || t == "\r" {
                        continue;
                    }
                    if let Some((s, e)) = ed.vime_preedit.take() {
                        let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                        ed.content.replace_range(s..e, "");
                        ed.vcaret = s;
                        ed.vsel = None;
                        v_recompute(ed);
                    }
                    // 多光标模式：英文/输入法提交也要作用到全部光标（系统输入法激活后字母走 Commit 而非 Text）
                    if ed.msel.is_empty() {
                        v_insert(ed, &t);
                    } else {
                        v_multi_replace(ed, &t);
                    }
                }
                egui::ImeEvent::Disabled => {
                    if let Some((s, e)) = ed.vime_preedit.take() {
                        let (s, e) = (s.min(ed.content.len()), e.min(ed.content.len()));
                        ed.content.replace_range(s..e, "");
                        ed.vcaret = s;
                        ed.vsel = None;
                        v_recompute(ed);
                    }
                }
            }
        }
        // 已自绘处理，移除 Ime 事件，避免主循环重复处理
        ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Ime(_))));
    }
    if !focused {
        ed.complete = None; // 失焦关闭补全弹窗
    }
    let mut typed = false; // 本帧是否有字符输入（补全触发 + 光标闪烁重置）
    if focused {
        let vver0 = ed.vver;
        let caret0 = ed.vcaret;
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            // 补全弹窗打开时优先消费导航键：↑↓ 选择、Enter/Tab 接受、Esc 关闭
            if ed.complete.is_some() {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = &ev
                {
                    let n = ed
                        .complete
                        .as_ref()
                        .map(|(v, _, _)| v.len())
                        .unwrap_or(0)
                        .max(1);
                    match key {
                        egui::Key::ArrowDown if !modifiers.any() => {
                            if let Some((_, sel, _)) = &mut ed.complete {
                                *sel = (*sel + 1) % n;
                            }
                            continue;
                        }
                        egui::Key::ArrowUp if !modifiers.any() => {
                            if let Some((_, sel, _)) = &mut ed.complete {
                                *sel = (*sel + n - 1) % n;
                            }
                            continue;
                        }
                        egui::Key::Enter | egui::Key::Tab if !modifiers.any() => {
                            let sel = ed.complete.as_ref().map(|(_, s, _)| *s).unwrap_or(0);
                            v_complete_accept(ed, sel);
                            continue;
                        }
                        egui::Key::Escape => {
                            ed.complete = None;
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            // 跟随 / 大文件只读：吞掉常规修改输入（导航/复制/查找仍可用）
            if ed.is_readonly() {
                match &ev {
                    egui::Event::Text(_)
                    | egui::Event::Paste(_)
                    | egui::Event::Ime(_)
                    | egui::Event::Cut => continue,
                    egui::Event::Key {
                        key:
                            egui::Key::Backspace | egui::Key::Delete | egui::Key::Enter | egui::Key::Tab,
                        pressed: true,
                        ..
                    } => continue,
                    _ => {}
                }
            }
            if matches!(&ev, egui::Event::Text(t) if !t.is_empty()) {
                typed = true;
            }
            // 多光标模式（msel 非空）：编辑/复制作用于全部选区；移动等其它键退出多选、走常规
            if !ed.msel.is_empty() {
                let mut handled = true;
                match &ev {
                    egui::Event::Text(t) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Paste(t) if !t.is_empty() => v_multi_replace(ed, t),
                    egui::Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => {
                        v_multi_replace(ed, t)
                    }
                    egui::Event::Copy => {
                        let s = v_multi_copy(ed);
                        if !s.is_empty() {
                            ui.ctx().copy_text(s);
                        }
                    }
                    egui::Event::Cut => {
                        let s = v_multi_copy(ed);
                        if !s.is_empty() {
                            ui.ctx().copy_text(s);
                            v_multi_replace(ed, "");
                        }
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let cmd = modifiers.command || modifiers.ctrl;
                        match key {
                            egui::Key::Escape => ed.msel.clear(),
                            egui::Key::Backspace => v_multi_backspace(ed),
                            egui::Key::Delete => v_multi_delete(ed),
                            egui::Key::Enter => v_multi_replace(ed, "\n"),
                            egui::Key::Tab => {
                                let u = ed.indent.unit();
                                v_multi_replace(ed, &u);
                            }
                            egui::Key::D if cmd => v_ctrl_d(ed),
                            egui::Key::ArrowLeft if !cmd => v_multi_move(ed, false),
                            egui::Key::ArrowRight if !cmd => v_multi_move(ed, true),
                            // 纵向导航 / Ctrl+组合键 → 退出多选、走常规处理
                            egui::Key::ArrowUp
                            | egui::Key::ArrowDown
                            | egui::Key::Home
                            | egui::Key::End
                            | egui::Key::PageUp
                            | egui::Key::PageDown
                                if !cmd =>
                            {
                                ed.msel.clear();
                                handled = false;
                            }
                            _ if cmd => {
                                ed.msel.clear();
                                handled = false;
                            }
                            // 普通字母/符号键：会另发 Text 事件做多光标插入，这里不清 msel
                            _ => handled = false,
                        }
                    }
                    _ => handled = false,
                }
                if handled {
                    continue; // 视角跟随由「光标是否移动」统一判断
                }
            }
            match ev {
                egui::Event::Text(t) if !t.is_empty() => {
                    // 已在闭合符前再敲同一闭合符 → 跳过（如 "" 中间再敲 " 移到右侧）
                    if v_sel_range(ed).is_none() && skip_closing_pair(ed, &t) {
                        ed.vcaret = next_char_boundary(&ed.content, ed.vcaret);
                        ed.vgoal_col = None;
                    // 自动补全括号/引号：无选区→插入成对并把光标放中间；有选区→用括号包裹并保留选中
                    } else if let Some(close) = auto_close_for(&t) {
                        if let Some((a, b)) = v_sel_range(ed) {
                            let inner = ed.content[a..b].to_string();
                            v_apply(ed, a, b - a, &format!("{t}{inner}{close}"));
                            ed.vsel = Some(a + t.len());
                            ed.vcaret = a + t.len() + inner.len();
                            ed.vgoal_col = None;
                        } else {
                            v_insert(ed, &format!("{t}{close}"));
                            ed.vcaret -= close.len();
                        }
                    } else {
                        v_insert(ed, &t);
                    }
                }
                egui::Event::Paste(t) if !t.is_empty() => v_insert(ed, &t),
                egui::Event::Ime(egui::ImeEvent::Commit(t)) if !t.is_empty() => v_insert(ed, &t),
                egui::Event::Copy => {
                    if let Some(s) = v_sel_range(ed).map(|(a, b)| ed.content[a..b].to_string()) {
                        ui.ctx().copy_text(s);
                    }
                }
                egui::Event::Cut => {
                    if let Some(s) = v_sel_range(ed).map(|(a, b)| ed.content[a..b].to_string()) {
                        ui.ctx().copy_text(s);
                        v_delete_selection(ed);
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let cmd = modifiers.command || modifiers.ctrl;
                    match key {
                        egui::Key::S if cmd => save = true,
                        egui::Key::F if cmd => ed.open_find(),
                        egui::Key::G if cmd => {
                            ed.goto_open = !ed.goto_open;
                            if ed.goto_open {
                                ed.goto_focus = true;
                            }
                        }
                        egui::Key::A if cmd => {
                            ed.vsel = Some(0);
                            ed.vcaret = ed.content.len();
                        }
                        egui::Key::D if cmd => v_ctrl_d(ed),
                        egui::Key::Z if cmd && modifiers.shift => v_redo(ed),
                        egui::Key::Z if cmd => v_undo(ed),
                        egui::Key::Y if cmd => v_redo(ed),
                        egui::Key::Slash if cmd => {
                            if let Some(p) = line_comment(&ed.language) {
                                v_toggle_comment(ed, p);
                            }
                        }
                        egui::Key::K if cmd && modifiers.shift => v_delete_line(ed),
                        egui::Key::Backspace if cmd => v_delete_word(ed, false),
                        egui::Key::Delete if cmd => v_delete_word(ed, true),
                        egui::Key::Backspace => v_backspace(ed),
                        egui::Key::Delete => v_delete_fwd(ed),
                        egui::Key::Enter => v_newline_indent(ed),
                        egui::Key::Tab if modifiers.shift => v_block_indent(ed, false),
                        egui::Key::Tab => {
                            // 选区跨行 → 块缩进；否则插入一个缩进单位
                            if v_sel_range(ed).is_some_and(|(a, b)| ed.content[a..b].contains('\n'))
                            {
                                v_block_indent(ed, true);
                            } else {
                                let u = ed.indent.unit();
                                v_insert(ed, &u);
                            }
                        }
                        egui::Key::ArrowUp if modifiers.alt && modifiers.shift => {
                            v_duplicate_line(ed, false)
                        }
                        egui::Key::ArrowDown if modifiers.alt && modifiers.shift => {
                            v_duplicate_line(ed, true)
                        }
                        egui::Key::ArrowUp if modifiers.alt => v_move_line(ed, true),
                        egui::Key::ArrowDown if modifiers.alt => v_move_line(ed, false),
                        egui::Key::ArrowLeft if cmd => v_move_word(ed, false, modifiers.shift),
                        egui::Key::ArrowRight if cmd => v_move_word(ed, true, modifiers.shift),
                        egui::Key::ArrowLeft => v_move_h(ed, false, modifiers.shift),
                        egui::Key::ArrowRight => v_move_h(ed, true, modifiers.shift),
                        egui::Key::ArrowUp => v_move_v(ed, -1, modifiers.shift),
                        egui::Key::ArrowDown => v_move_v(ed, 1, modifiers.shift),
                        egui::Key::Home if cmd => v_move_doc(ed, false, modifiers.shift),
                        egui::Key::End if cmd => v_move_doc(ed, true, modifiers.shift),
                        egui::Key::Home => v_move_edge(ed, false, modifiers.shift),
                        egui::Key::End => v_move_edge(ed, true, modifiers.shift),
                        egui::Key::PageUp => v_move_v(ed, -page, modifiers.shift),
                        egui::Key::PageDown => v_move_v(ed, page, modifiers.shift),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        ed.vcaret = ed.vcaret.min(ed.content.len());
        // 补全触发/维护：有字符输入 →（重新）打开；其它编辑（如退格）→ 按新前缀刷新；
        // 纯光标移动 → 关闭（避免弹窗脱离输入上下文）
        if typed || (ed.complete.is_some() && ed.vver != vver0) {
            v_complete_refresh(ed);
        } else if ed.complete.is_some() && ed.vcaret != caret0 {
            ed.complete = None;
        }
    }
    // 真正判断光标是否移动（仅此情况才让视角跟随；无移动时自由滚动、绝不拉回）
    let moved = ed.vcaret != prev_caret;
    // 光标移动或输入时重置闪烁相位，使光标立即显示
    if focused && (moved || typed) {
        ed.caret_blink_at = ui.input(|i| i.time);
    }
    (save, moved, typed)
}
