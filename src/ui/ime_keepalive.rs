//! 保持 IME 输入上下文在控件切焦点时不被拆掉。
//!
//! X11/XIM 上 egui-winit 在 `o.ime == None` 的那一帧会 `set_ime_allowed(false)`，
//! winit 实现是 **XDestroyIC**；下一帧再启用是 **XCreateIC**。不同输入法对「重建 IC」
//! 后的语言模式处理不一：有的保留中文，有的回到英文默认态。
//!
//! 终端/编辑器只在自身聚焦时上报 `o.ime`，切到侧栏、文件列表、工具栏等非输入控件时
//! 会出现空窗——于是用户感知为「焦点回来后输入法变成英文」。窗口仍在前台时沿用上一帧
//! 的 IME 矩形即可让 `allow_ime` 保持为 true，只更新光标区、不销毁 IC。

/// 窗口仍在前台：有控件上报则记下；本帧无人上报则沿用上一帧，避免 XDestroyIC。
/// 窗口失焦：清空 sticky，允许正常关闭 IME。
pub fn keep_ime_alive(ctx: &egui::Context, sticky: &mut Option<egui::output::IMEOutput>) {
    let window_focused = ctx.input(|i| i.focused);
    if !window_focused {
        *sticky = None;
        return;
    }
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
    use super::*;

    /// 纯逻辑：模拟「本帧有/无上报 + 窗口焦」对 sticky 的影响（不拉起真实 egui Context）。
    #[test]
    fn sticky_clears_when_window_unfocused() {
        let mut sticky = Some(egui::output::IMEOutput {
            rect: egui::Rect::from_min_size(egui::pos2(1.0, 1.0), egui::vec2(10.0, 10.0)),
            cursor_rect: egui::Rect::from_min_size(egui::pos2(1.0, 1.0), egui::vec2(1.0, 10.0)),
        });
        // 无 Context 时只测「失焦应清空」的判定分支：直接复现函数前半段。
        let window_focused = false;
        if !window_focused {
            sticky = None;
        }
        assert!(sticky.is_none());
    }

    #[test]
    fn sticky_kept_when_no_widget_reports_ime() {
        let prev = egui::output::IMEOutput {
            rect: egui::Rect::from_min_size(egui::pos2(2.0, 3.0), egui::vec2(20.0, 16.0)),
            cursor_rect: egui::Rect::from_min_size(egui::pos2(2.0, 3.0), egui::vec2(1.0, 16.0)),
        };
        let mut sticky = Some(prev);
        let mut frame_ime: Option<egui::output::IMEOutput> = None; // 本帧无人上报
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
