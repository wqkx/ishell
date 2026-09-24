# iShell patches to eframe 0.34.3

Upstream crate from crates.io `eframe` 0.34.3, with a local fix so a minimized /
hidden window cannot freeze the event-loop thread (and with it MCP request handling):

1. **`src/native/run.rs`** — On Wayland, `Window::is_minimized` / `Occluded` are
   unsupported, and `request_redraw` waits for a compositor frame callback that is
   never delivered for a hidden surface (emilk/egui#5136).

   Strategy (not “always direct-paint”):
   - Prefer `request_redraw` so **visible** windows stay compositor-paced.
   - Arm a ~100 ms fallback; if `RedrawRequested` never arrives, paint directly
     (and throttle) so `App::logic` / MCP keep running.
   - Keep the **earliest** pending fallback deadline — rapid sub-interval repaints
     must not keep pushing it out, or direct paint never fires while hidden.
   - Clear the fallback when `RedrawRequested` is delivered.

Do **not** fold `is_invisible_or_minimized` into the paint-time `is_visible` flag:
that skipped `App::ui` whenever the OS reported the window invisible/minimized,
and on some desktops the first frames look “invisible”, so the UI never appeared.

iShell also disables vsync by default on Wayland (`DontWait`) so a fallback
direct paint cannot hang inside `swap_buffers` — see `src/main.rs`.
