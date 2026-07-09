//! 单行输入 IME 绕过（fcitx/X11 Commit 门）。从 file_panel 拆出，行为不变。

pub(super) fn ime_apply_events(ui: &mut egui::Ui, id: egui::Id, buf: &mut String, preedit: &mut Option<(usize, usize)>) {
    let focused = ui.ctx().memory(|m| m.focused() == Some(id));
    if !focused {
        return;
    }
    let ime: Vec<egui::ImeEvent> = ui.input_mut(|i| {
        let evs: Vec<egui::ImeEvent> = i
            .events
            .iter()
            .filter_map(|e| if let egui::Event::Ime(ev) = e { Some(ev.clone()) } else { None })
            .collect();
        i.events.retain(|e| !matches!(e, egui::Event::Ime(_)));
        evs
    });
    if ime.is_empty() {
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
                let (s, e) = preedit.take().unwrap_or((caret, caret));
                let (s, e) = (s.min(buf.len()), e.min(buf.len()));
                buf.replace_range(s..e, &t);
                caret = s + t.len();
                *preedit = if t.is_empty() { None } else { Some((s, caret)) };
            }
            egui::ImeEvent::Commit(t) => {
                if t == "\n" || t == "\r" {
                    continue;
                }
                if let Some((s, e)) = preedit.take() {
                    let (s, e) = (s.min(buf.len()), e.min(buf.len()));
                    buf.replace_range(s..e, "");
                    caret = s;
                }
                let at = caret.min(buf.len());
                buf.insert_str(at, &t);
                caret = at + t.len();
            }
            egui::ImeEvent::Enabled => {}
            egui::ImeEvent::Disabled => {
                if let Some((s, e)) = preedit.take() {
                    let (s, e) = (s.min(buf.len()), e.min(buf.len()));
                    buf.replace_range(s..e, "");
                    caret = s;
                }
            }
        }
    }
    let cc = egui::text::CCursor::new(char_of_byte(buf, caret));
    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cc)));
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
    let out = egui::TextEdit::singleline(buf).id(id).desired_width(f32::INFINITY).show(ui);
    let resp = out.response.response; // TextEditOutput.response 是 AtomLayoutResponse，取其内层 Response
    // 回车提交：egui 单行不消费回车事件（`lost_focus()+key_pressed(Enter)` 官方惯用法），
    // 聚焦或本帧刚失焦时读到回车即视为提交，比单看 lost_focus 更可靠。
    let enter = (resp.has_focus() || resp.lost_focus())
        && ui.input(|i| i.key_pressed(egui::Key::Enter));
    (resp, enter)
}

/// 字符位 → 字节偏移（越界回退到串尾）。
pub(super) fn byte_of_char(s: &str, ch: usize) -> usize {
    s.char_indices().map(|(b, _)| b).chain(std::iter::once(s.len())).nth(ch).unwrap_or(s.len())
}

/// 字节偏移 → 字符位（非字符边界时向下取整，避免切片 panic）。
pub(super) fn char_of_byte(s: &str, b: usize) -> usize {
    let mut b = b.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    s[..b].chars().count()
}

