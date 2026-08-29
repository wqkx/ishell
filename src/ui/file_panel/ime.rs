//! 单行输入 IME 绕过（fcitx/X11 Commit 门）。从 file_panel 拆出，行为不变。
//!
//! 组字区间的所有偏移计算一律走 `crate::ui::ime_safe`——`preedit` 是跨帧存在 state 里的，
//! 而 `buf` 可能被别处换掉（切目录重置路径框、对话框换类型、重命名行复用），陈旧偏移落在
//! 多字节字符中间会直接 panic。理由详见该模块文档。

use crate::ui::ime_safe::{byte_of_char, char_of_byte, insert_at, replace_preedit};

pub(super) fn ime_apply_events(
    ui: &mut egui::Ui,
    id: egui::Id,
    buf: &mut String,
    preedit: &mut Option<(usize, usize)>,
) {
    let focused = ui.ctx().memory(|m| m.focused() == Some(id));
    if !focused {
        // 组字状态自愈（失焦）：把没提交的组字撤掉。输入法半路没了（fcitx 崩溃/重启、
        // 远程桌面会话切换）时 `Disabled` 永远不会来，`preedit` 就一直挂着——框里留着一截
        // 没提交的拼音，而下一次组字还会拿这个陈旧区间去替换，替到别的地方去。
        // 用户「重启输入法再点回来」这条自救路径必须是干净的，否则重启了也还是乱的。
        if let Some(r) = preedit.take() {
            replace_preedit(buf, r, "");
        }
        return;
    }
    let ime: Vec<egui::ImeEvent> = ui.input_mut(|i| {
        let evs: Vec<egui::ImeEvent> = i
            .events
            .iter()
            .filter_map(|e| {
                if let egui::Event::Ime(ev) = e {
                    Some(ev.clone())
                } else {
                    None
                }
            })
            .collect();
        i.events.retain(|e| !matches!(e, egui::Event::Ime(_)));
        evs
    });
    if ime.is_empty() {
        // 组字状态自愈（收到裸文本却没有 Ime 事件）：XIM 组字期间按键会被输入法过滤，
        // 能收到裸 `Text` 就说明组字已经不在了——同上，撤掉没提交的那截。
        if preedit.is_some()
            && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(_))))
        {
            if let Some(r) = preedit.take() {
                replace_preedit(buf, r, "");
            }
        }
        return;
    }
    let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
    let caret_char = st
        .cursor
        .char_range()
        .map(|r| r.primary.index)
        .unwrap_or_else(|| buf.chars().count());
    let mut caret = byte_of_char(buf, caret_char);
    for ev in ime {
        match ev {
            egui::ImeEvent::Preedit(t) => {
                if t == "\n" || t == "\r" {
                    continue;
                }
                let r = preedit.take().unwrap_or((caret, caret));
                let (s, end) = replace_preedit(buf, r, &t);
                caret = end;
                *preedit = if t.is_empty() { None } else { Some((s, end)) };
            }
            egui::ImeEvent::Commit(t) => {
                if t == "\n" || t == "\r" {
                    continue;
                }
                if let Some(r) = preedit.take() {
                    caret = replace_preedit(buf, r, "").0;
                }
                caret = insert_at(buf, caret, &t);
            }
            egui::ImeEvent::Enabled => {}
            egui::ImeEvent::Disabled => {
                if let Some(r) = preedit.take() {
                    caret = replace_preedit(buf, r, "").0;
                }
            }
        }
    }
    let cc = egui::text::CCursor::new(char_of_byte(buf, caret));
    st.cursor
        .set_char_range(Some(egui::text::CCursorRange::one(cc)));
    st.store(ui.ctx(), id);
}

/// 单行输入框 + 自绘 IME：绕开 egui 0.34 `TextEdit` 的 Commit 门——fcitx(X11) 只发
/// `Ime(Commit)`、不发 `Enabled`/`Preedit`，egui 的 `ime_cursor_range` 门永假导致「中文只能
/// 输一次」（同 editor.rs 的修法，见 memory `ime-secondary-window-fix`）。本函数在 TextEdit
/// 渲染前抽走并自行落地 Ime 事件，绕开坏门；同时用键盘事件可靠检测回车提交。
/// 返回 (response, 本帧是否回车提交)。`preedit` 为跨帧维护的组字字节范围。
pub(super) fn ime_singleline(
    ui: &mut egui::Ui,
    id_src: &str,
    buf: &mut String,
    preedit: &mut Option<(usize, usize)>,
) -> (egui::Response, bool) {
    let id = egui::Id::new(id_src);
    ime_apply_events(ui, id, buf, preedit);
    let out = egui::TextEdit::singleline(buf)
        .id(id)
        .desired_width(f32::INFINITY)
        .show(ui);
    let resp = out.response.response; // TextEditOutput.response 是 AtomLayoutResponse，取其内层 Response
                                      // 回车提交：egui 单行不消费回车事件（`lost_focus()+key_pressed(Enter)` 官方惯用法），
                                      // 聚焦或本帧刚失焦时读到回车即视为提交，比单看 lost_focus 更可靠。
    let enter =
        (resp.has_focus() || resp.lost_focus()) && ui.input(|i| i.key_pressed(egui::Key::Enter));
    (resp, enter)
}
