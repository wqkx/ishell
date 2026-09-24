# iShell patches to eframe 0.34.3

Upstream crate from crates.io `eframe` 0.34.3, with two local fixes so a minimized /
hidden window cannot freeze the event-loop thread (and with it MCP request handling):

1. **`src/native/run.rs`** — On Wayland, `Window::is_minimized` / `Occluded` are
   unsupported, and `request_redraw` waits for a compositor frame callback that is
   never delivered for a hidden surface. When a repaint timer fires, paint
   directly instead of calling `request_redraw` (same path already used for
   invisible windows on Windows). See emilk/egui#5136.

2. **`src/native/glow_integration.rs`** (and wgpu twin) — Treat
   `is_invisible_or_minimized` as not visible for paint/swap. On X11/macOS/Windows
   this skips `swap_buffers` while minimized so vsync cannot block forever on a
   surface the compositor is no longer presenting (glutin documents this for
   Wayland `SwapInterval::Wait`).
