# iShell patches to eframe 0.34.3

Upstream crate from crates.io `eframe` 0.34.3, with two local fixes so a minimized /
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

2. **`src/native/glow_integration.rs`** (and wgpu twin) — Treat
   `is_invisible_or_minimized` as not visible for paint/swap. On X11/macOS/Windows
   this skips `swap_buffers` while minimized so vsync cannot block forever on a
   surface the compositor is no longer presenting (glutin documents this for
   Wayland `SwapInterval::Wait`). `App::logic` still runs via `epi_integration`.

iShell also disables vsync by default on Wayland (`DontWait`) so a fallback
direct paint cannot hang inside `swap_buffers` — see `src/main.rs`.
