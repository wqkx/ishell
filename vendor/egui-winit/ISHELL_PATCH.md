This directory vendors `egui-winit` 0.34.3 from crates.io.

iShell carries one behavioral patch in `src/lib.rs` (`Ime::Preedit(text, None)`
in `fn on_ime`): a preedit event that carries text but no cursor position is
forwarded as `ImeEvent::Preedit(text)` instead of having its text discarded and
being reported as `ImeEvent::Disabled`. Only an *empty* preedit still ends the
composition.

Upstream binds that text to `_` and unconditionally calls `ime_event_disable()`,
on the assumption that "no cursor" means "composition finished". That does not
hold on X11/fcitx: pressing a modifier (notably Shift) mid-composition emits
`Preedit("<pinyin so far>", None)` — the composition is still live, the IME just
does not report a caret for that tick. Applications then see `Disabled`, drop the
pending preedit from the text box, and the user watches the pinyin they already
typed vanish; the next keystroke produces a cursor-carrying `Preedit` with the
full string, so the "lost" characters suddenly reappear. Every text input in
iShell (terminal, editor, path bar, dialogs) was affected, since they all treat
`Disabled` as "composition aborted, remove the preedit".

The macOS-specific branch below the patch (upstream PR emilk/egui#7973, which
synthesises `Preedit("")` so backspace can delete the last composed character) is
untouched — it only concerns the empty-text case, which still falls through to
the original code path.
