//! 开 PTY 前解析初始行列，避免先以 80×24 起 shell 再 resize 造成的闪屏。

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::proto::UiCommand;

/// 默认尺寸：UI 尚未上报、或超时未等到 Resize 时的兜底。
pub const DEFAULT_PTY_COLS: u16 = 80;
pub const DEFAULT_PTY_ROWS: u16 = 24;

/// 等待首帧真实窗口尺寸的上限。鉴权期间 UI 通常已把 Resize 打进通道；
/// 若窗口尚未 layout，最多等这么久再退回 80×24。
const WAIT_BUDGET: Duration = Duration::from_millis(1500);

/// 从命令通道取出初始 PTY 尺寸，并把等待期间其它命令原样放进 `prelude`，
/// 供 worker 主循环在开 shell 之后优先消化（避免丢 AddForward / TerminalInput）。
pub async fn resolve_initial_pty_size(
    cmd_rx: &mut UnboundedReceiver<UiCommand>,
    seeded: VecDeque<UiCommand>,
) -> (u16, u16, VecDeque<UiCommand>) {
    let mut prelude = VecDeque::new();
    let mut size = (DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS);
    let mut got = false;

    // 键盘交互认证等待期间缓存下来的命令（首帧 Resize 常在这里），再排干通道里
    // 认证完成后新到的。任一处见过合法 Resize 就不必再空等 1.5s。
    for cmd in seeded
        .into_iter()
        .chain(std::iter::from_fn(|| cmd_rx.try_recv().ok()))
    {
        match cmd {
            UiCommand::Resize { cols, rows } if cols > 0 && rows > 0 => {
                size = (cols, rows);
                got = true;
            }
            UiCommand::Disconnect => {
                prelude.push_front(UiCommand::Disconnect);
                return (size.0, size.1, prelude);
            }
            other => prelude.push_back(other),
        }
    }
    if got {
        return (size.0, size.1, prelude);
    }

    let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
    while tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, cmd_rx.recv()).await {
            Ok(Some(UiCommand::Resize { cols, rows })) if cols > 0 && rows > 0 => {
                return (cols, rows, prelude);
            }
            Ok(Some(UiCommand::Disconnect)) => {
                prelude.push_front(UiCommand::Disconnect);
                break;
            }
            Ok(Some(other)) => prelude.push_back(other),
            Ok(None) => break, // 通道关闭
            Err(_) => break,   // 超时
        }
    }
    (size.0, size.1, prelude)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test]
    async fn uses_queued_resize_without_waiting() {
        let (tx, mut rx) = unbounded_channel();
        tx.send(UiCommand::Resize {
            cols: 120,
            rows: 40,
        })
        .unwrap();
        let (c, r, prelude) = resolve_initial_pty_size(&mut rx, VecDeque::new()).await;
        assert_eq!((c, r), (120, 40));
        assert!(prelude.is_empty());
    }

    #[tokio::test]
    async fn buffers_non_resize_commands_in_prelude() {
        let (tx, mut rx) = unbounded_channel();
        tx.send(UiCommand::TerminalInput(b"echo hi".to_vec()))
            .unwrap();
        tx.send(UiCommand::Resize {
            cols: 100,
            rows: 30,
        })
        .unwrap();
        let (c, r, mut prelude) = resolve_initial_pty_size(&mut rx, VecDeque::new()).await;
        assert_eq!((c, r), (100, 30));
        assert!(matches!(
            prelude.pop_front(),
            Some(UiCommand::TerminalInput(_))
        ));
    }

    #[tokio::test]
    async fn falls_back_to_80x24_when_channel_empty() {
        let (_tx, mut rx) = unbounded_channel::<UiCommand>();
        // 用不阻塞的 try 路径：空通道 + 极短超时。把 WAIT 测到会慢，这里只验证 try_recv 空时
        // 仍返回默认——通过塞一个立即超时场景：先 drop sender 让 recv 立刻 None。
        drop(_tx);
        let (c, r, _) = resolve_initial_pty_size(&mut rx, VecDeque::new()).await;
        assert_eq!((c, r), (DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS));
    }

    #[tokio::test]
    async fn seeded_resize_from_kbd_wait_skips_timeout() {
        let (_tx, mut rx) = unbounded_channel::<UiCommand>();
        drop(_tx);
        let mut seed = VecDeque::new();
        seed.push_back(UiCommand::TerminalInput(b"x".to_vec()));
        seed.push_back(UiCommand::Resize {
            cols: 110,
            rows: 33,
        });
        let (c, r, mut prelude) = resolve_initial_pty_size(&mut rx, seed).await;
        assert_eq!((c, r), (110, 33));
        assert!(matches!(
            prelude.pop_front(),
            Some(UiCommand::TerminalInput(_))
        ));
    }
}
