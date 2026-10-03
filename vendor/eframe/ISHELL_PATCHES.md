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

2. **`src/native/glow_integration.rs`** — While the window is minimized (and after
   the first frame), skip only `swap_buffers`; `App::ui` and painting still run so
   texture deltas are not lost. Decision in `winit_integration::skip_swap_while_minimized`;
   `EpiIntegration::is_first_frame` added for it. The wgpu twin is **not** patched:
   iShell builds only the `glow` backend, and wgpu presents inside painting.

Do **not** fold `is_invisible_or_minimized` into the paint-time `is_visible` flag
(0.24.2 did, reverted in 0.24.4): eframe creates windows hidden and shows them from
the first painted frame (`post_rendering`). X11, Windows and macOS report that hidden
window as invisible, so the first frame never happened and the UI never appeared.
Only Wayland (which reports `None`) escaped.

iShell also disables vsync by default in Wayland sessions (`DontWait`) so a fallback
direct paint cannot hang inside `swap_buffers` — including when iShell is forced onto
X11 (XWayland) for input methods; see `src/main.rs`.
