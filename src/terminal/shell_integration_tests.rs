//! 场景测试层：拿**真的 shell**（本机 PTY 起一个 bash）跑一遍 AI 命令的完整链路——
//! 注入集成片段 → 敲命令 → 等完成 → 读退出码与输出。
//!
//! 为什么要这一层：此前这个子系统的测试全是纯函数级的（闸门、转义、路径校验），而实际
//! 出问题的全是**调用方的编排**——哨兵被 `cat` 当输入吃掉、中断后残留污染下一条命令、
//! 首条命令退出码退化成 -1。这些缺陷没有一个能被纯函数测试碰到，全部由真人用出来。
//!
//! 这一层不需要网络、不需要 SSH、不需要 GUI：本机 PTY + [`Terminal`] 就够，跑完一轮 5 个
//! 场景通常在 2 秒内。`Terminal` 的输入输出正是 SSH worker 与本机 worker 共用的那条路径，
//! 所以这里验证的编排与远端会话上跑的是同一套。
//!
//! 不覆盖的部分（写明以防误以为已保障）：`App` 侧的超时/闸门策略需要 `eframe` 的
//! `CreationContext`，测试里造不出来，那部分的判据抽成了纯函数（见 `mcp_bridge` 的
//! `run_is_abandoned`）；SSH 通道本身的行为仍由 live 测试脚本覆盖。

use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize, SlavePty};

use super::Terminal;
use crate::app::view_state::AI_SESSION_SNIPPET;

/// 一个跑着真 bash 的终端：PTY 主端的读写 + 喂给它的 [`Terminal`]。
struct ShellHarness {
    term: Terminal,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl ShellHarness {
    /// 起一个交互式 bash（`--norc --noprofile`：不受本机 rc 影响，结果可复现）。
    fn start() -> Self {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("bash");
        cmd.args(["--norc", "--noprofile", "-i"]);
        cmd.env("TERM", "xterm-256color");
        cmd.env("PS1", "$ ");
        let child = SlavePty::spawn_command(&*pair.slave, cmd).expect("spawn bash");
        drop(pair.slave); // 不释放的话 shell 退出后读端等不到 EOF
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = pair.master.take_writer().expect("writer");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut h = Self {
            term: Terminal::new(),
            writer,
            rx,
            _child: child,
            _master: pair.master,
        };
        h.pump(Duration::from_millis(400)); // 等第一个提示符
        h
    }

    /// 把 PTY 上已到达的字节喂给 Terminal，最多等 `dur`。
    fn pump(&mut self, dur: Duration) {
        let until = Instant::now() + dur;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left.min(Duration::from_millis(50))) {
                Ok(bytes) => {
                    self.term.feed(&bytes);
                }
                Err(_) => continue,
            }
        }
    }

    /// 一直喂到捕获收束（或超时），返回 `(退出码, 输出)`。
    fn wait_done(&mut self, dur: Duration) -> Option<(i32, String)> {
        let until = Instant::now() + dur;
        while Instant::now() < until {
            self.pump(Duration::from_millis(50));
            if let Some(done) = self.term.take_ai_done() {
                return Some(done);
            }
        }
        None
    }

    fn type_line(&mut self, line: &str) {
        self.writer.write_all(format!("{line}\r").as_bytes()).expect("write");
        self.writer.flush().expect("flush");
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write");
        self.writer.flush().expect("flush");
    }

    /// 注入 AI 会话片段（与 `frame.rs` 的自动注入同一条路径：打字 + 吞回显）。
    fn inject(&mut self) {
        self.term.expect_auto_inject_echo(AI_SESSION_SNIPPET);
        self.type_line(AI_SESSION_SNIPPET);
        self.pump(Duration::from_millis(500));
    }

    /// 按集成模式跑一条命令（与 `mcp_bridge` 的 RunCommand 分支同构：只发命令，不打哨兵）。
    fn run(&mut self, command: &str) {
        self.term.arm_ai_capture_integration();
        self.type_line(command);
    }
}

/// 片段注入后，shell 必须开始发 OSC 133——这是「完成检测走集成而不是哨兵」的前提。
/// 反向对照：把 `AI_SESSION_SNIPPET` 换回只有 OSC 7 的 `OSC7_SNIPPET`，本条当场挂。
#[test]
fn injecting_the_snippet_turns_on_shell_integration() {
    let mut h = ShellHarness::start();
    assert!(!h.term.shell_integration_active(), "注入前不该有集成");
    h.inject();
    assert!(
        h.term.shell_integration_active(),
        "注入后 shell 应在每个提示符发 OSC 133"
    );
}

/// 最基本的一条：命令跑完拿到退出码与输出，且**屏幕上不该出现任何哨兵痕迹**
/// （集成模式压根不打哨兵行）。
#[test]
fn a_plain_command_reports_its_exit_code_and_output() {
    let mut h = ShellHarness::start();
    h.inject();
    h.run("echo hello-ishell");
    let (code, out) = h.wait_done(Duration::from_secs(5)).expect("命令该在 5 秒内完成");
    assert_eq!(code, 0);
    assert!(out.contains("hello-ishell"), "输出里应有命令结果，实际：{out:?}");
    assert!(
        !h.term.screen_text().contains("AI_DONE"),
        "集成模式不该往终端里打哨兵行"
    );
}

/// 非零退出码要如实带回（此前哨兵失配时会退化成 -1）。
#[test]
fn a_failing_command_reports_its_real_exit_code() {
    let mut h = ShellHarness::start();
    h.inject();
    h.run("(exit 42)");
    let (code, _) = h.wait_done(Duration::from_secs(5)).expect("命令该完成");
    assert_eq!(code, 42, "退出码必须来自 shell 的 $?");
}

/// **这条是整个架构改造的理由**：`cat` 会把紧跟其后的哨兵行当成自己的 stdin 吃掉，
/// 哨兵模式下这条运行永远 finished=false、把会话闸门占死。集成模式下标记由 shell 自己
/// 发出、不经过任何程序的 stdin，`cat` 收到 EOF 退出时照常收束。
/// 反向对照：把 `run` 换成哨兵模式（`arm_ai_capture` + 多打一行 printf），本条当场挂。
#[test]
fn an_interactive_command_still_completes() {
    let mut h = ShellHarness::start();
    h.inject();
    h.run("cat");
    h.pump(Duration::from_millis(300));
    h.send_raw(b"typed-into-cat\r");
    h.pump(Duration::from_millis(300));
    h.send_raw(&[0x04]); // Ctrl-D：cat 收到 EOF 退出
    let (code, out) = h.wait_done(Duration::from_secs(5)).expect("cat 退出后运行必须收束");
    assert_eq!(code, 0);
    assert!(out.contains("typed-into-cat"), "cat 的回显应在输出里，实际：{out:?}");
}

/// 中断也要有结论：Ctrl-C 之后 shell 打印新提示符时会发 `D;130`，运行就地收束，
/// 不再需要「interrupt 之后先 read_screen 确认队列干净」那套人工补救。
#[test]
fn interrupting_a_command_yields_a_result_instead_of_a_stuck_run() {
    let mut h = ShellHarness::start();
    h.inject();
    h.run("sleep 30");
    h.pump(Duration::from_millis(300));
    h.send_raw(&[0x03]); // Ctrl-C
    let (code, _) = h.wait_done(Duration::from_secs(5)).expect("中断后必须有结论");
    assert_eq!(code, 130, "SIGINT 结束的命令退出码是 128+2");
}

/// **杂散 D 不能把一条还没开始的运行报成完成。** 空行（裸回车）在 bash 里不展开 PS0
/// （没有 `C`）却照常跑 PROMPT_COMMAND（发 `D`），带的还是上一条命令的退出码。这条用真 bash
/// 把那个序列跑出来，确认捕获不认它、而随后真正的命令照常收束。
/// 反向对照：去掉 `capture_progress` 里「有 C 才认 D」的判据，第一条断言当场挂。
#[test]
fn a_stray_prompt_report_does_not_finish_the_run() {
    let mut h = ShellHarness::start();
    h.inject();
    h.run("(exit 9)"); // 先让「上一条命令的退出码」是个显眼的值
    let (c, _) = h.wait_done(Duration::from_secs(5)).expect("第一条该完成");
    assert_eq!(c, 9);

    h.term.arm_ai_capture_integration(); // 武装，但还没发命令
    h.send_raw(b"\r"); // 空行：只会产生一个没有 C 的 D
    assert!(
        h.wait_done(Duration::from_millis(800)).is_none(),
        "空行产生的 D 没有配对的 C，不该把这次运行报成完成（更不该报成 exit 9）"
    );
    h.type_line("echo after-stray");
    let (code, out) = h.wait_done(Duration::from_secs(5)).expect("真命令照常收束");
    assert_eq!(code, 0);
    assert!(out.contains("after-stray"), "实际：{out:?}");
}

/// 连发两条命令：第二条的输出不能混进第一条，退出码也不能串。
/// （哨兵模式下这里最容易出问题——上一条残留的标记行会被下一条捕获读到。）
#[test]
fn back_to_back_commands_do_not_bleed_into_each_other() {
    let mut h = ShellHarness::start();
    h.inject();
    h.run("echo first");
    let (c1, o1) = h.wait_done(Duration::from_secs(5)).expect("第一条该完成");
    h.run("echo second; (exit 7)");
    let (c2, o2) = h.wait_done(Duration::from_secs(5)).expect("第二条该完成");
    assert_eq!((c1, c2), (0, 7));
    assert!(o1.contains("first") && !o1.contains("second"), "第一条输出串了：{o1:?}");
    assert!(o2.contains("second") && !o2.contains("first"), "第二条输出串了：{o2:?}");
}
