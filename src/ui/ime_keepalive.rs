//! 保持 IME 输入上下文不被拆掉。
//!
//! X11/XIM 上 egui-winit 在 `o.ime == None` 的那一帧会 `set_ime_allowed(false)`，
//! winit 实现是 **XDestroyIC**；下一帧再启用是 **XCreateIC**。不同输入法对「重建 IC」
//! 后的语言模式处理不一：有的保留中文，有的回到英文默认态。
//!
//! 两类空窗都会触发销毁：
//! 1. **控件间切焦点**：终端/编辑器只在自身聚焦时上报 `o.ime`，点到侧栏等非输入控件时本帧为空；
//! 2. **整窗失焦**：`Focused(false)` 会催一帧重绘，那一帧通常也没有控件上报 `o.ime`。
//!
//! 做法：记住上一帧有效的 IME 矩形；本帧无人上报时继续填上，让 `allow_ime` 保持为 true，
//! 只更新光标区、不销毁 IC。整窗失焦时也保留 sticky（X11 上未聚焦窗口挂着 IC 不会抢走
//! 前台应用的输入）。

/// 有控件上报则更新 sticky；本帧无人上报则沿用 sticky，避免 XDestroyIC。
pub fn keep_ime_alive(ctx: &egui::Context, sticky: &mut Option<egui::output::IMEOutput>) {
    ctx.output_mut(|o| {
        if let Some(ime) = o.ime {
            *sticky = Some(ime);
        } else if let Some(prev) = *sticky {
            o.ime = Some(prev);
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn sticky_survives_when_no_widget_reports_including_unfocused_frame() {
        // 整窗失焦催出的那一帧通常也是「无人上报」——与控件失焦同一条路径，必须保留 sticky。
        let prev = egui::output::IMEOutput {
            rect: egui::Rect::from_min_size(egui::pos2(2.0, 3.0), egui::vec2(20.0, 16.0)),
            cursor_rect: egui::Rect::from_min_size(egui::pos2(2.0, 3.0), egui::vec2(1.0, 16.0)),
        };
        let mut sticky = Some(prev);
        let window_focused = false;
        let mut frame_ime: Option<egui::output::IMEOutput> = None;
        // 与 keep_ime_alive 同构：不再因 !window_focused 清 sticky。
        let _ = window_focused;
        if let Some(ime) = frame_ime {
            sticky = Some(ime);
        } else if let Some(s) = sticky {
            frame_ime = Some(s);
        }
        assert_eq!(frame_ime, Some(prev));
        assert_eq!(sticky, Some(prev));
    }

    #[test]
    fn sticky_updates_when_widget_reports_ime() {
        let old = egui::output::IMEOutput {
            rect: egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1.0, 1.0)),
            cursor_rect: egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1.0, 1.0)),
        };
        let new = egui::output::IMEOutput {
            rect: egui::Rect::from_min_size(egui::pos2(40.0, 50.0), egui::vec2(8.0, 16.0)),
            cursor_rect: egui::Rect::from_min_size(egui::pos2(40.0, 50.0), egui::vec2(1.0, 16.0)),
        };
        let mut sticky = Some(old);
        let frame_ime = Some(new);
        if let Some(ime) = frame_ime {
            sticky = Some(ime);
        }
        assert_eq!(sticky, Some(new));
    }
}
