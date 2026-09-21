//! 本机终端 worker：在运行 iShell 的这台机器上直接起一个 PTY + 交互式 shell，**不走 SSH**。
//!
//! 与 SSH worker（`src/ssh/mod.rs`）共用同一套 `UiCommand`/`WorkerEvent` 协议——终端只需要
//! `TerminalInput`/`Resize`（进）与 `TerminalData`（出）这几条纯字节消息，于是 `src/terminal/`、
//! `Session` 结构、字节级协议、重连逻辑全部原样复用，SSH 专有代码一行不碰。
//!
//! portable-pty 的读/写是**阻塞** I/O，所以：一条 std 线程阻塞读 PTY，把字节经 tokio 通道喂给
//! 下面的异步 select 循环；另一条 std 线程阻塞写 PTY，从 std 通道收键盘输入。master 句柄留在
//! 异步循环里做 resize，child 留着退出时 kill。
//!
//! 终端见本模块；本地文件浏览/读写/增删改（Phase 2）在 `files` 子模块，产出与 SFTP 侧相同的
//! `WorkerEvent`。传输（Phase 3）尚未支持，相关命令暂被忽略。

mod files;
mod sys;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize, SlavePty};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::proto::{ConnectConfig, UiCommand, WorkerEvent};
use crate::pty_size::{DEFAULT_PTY_COLS, DEFAULT_PTY_ROWS};
use crate::ssh::UiSink;

/// worker 入口：起本地 PTY shell，桥接 UI 通道，直到 shell 退出或用户断开。
pub async fn run(_cfg: ConnectConfig, mut cmd_rx: UnboundedReceiver<UiCommand>, sink: UiSink) {
    sink.send(WorkerEvent::Status(
        crate::i18n::tr("正在启动本机终端 …", "Starting local terminal …").into(),
    ));

    let (pty_cols, pty_rows, mut cmd_prelude) =
        crate::pty_size::resolve_initial_pty_size(&mut cmd_rx, std::collections::VecDeque::new())
            .await;

    let (master, mut child, out_rx, in_tx) = match spawn_pty(pty_cols, pty_rows) {
        Ok(v) => v,
        Err(e) => {
            sink.send(WorkerEvent::Disconnected(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("本机终端启动失败：{e}"),
                crate::i18n::Lang::En => format!("Local terminal failed: {e}"),
            }));
            return;
        }
    };
    let mut out_rx = out_rx;

    // 本机系统监控：Linux 直接采本地 /proc（与 SSH 会话同一套 PROBE_CMD + 解析器，侧栏一致）；
    // 非 Linux 无 /proc → 关闭监控（session_events 对本机会话不会弹「远端非 Linux」的误导提示）。
    let sysinfo_task: Option<tokio::task::JoinHandle<()>> = {
        #[cfg(target_os = "linux")]
        {
            Some(tokio::spawn(sys::run_sampler(sink.clone())))
        }
        #[cfg(not(target_os = "linux"))]
        {
            sink.send(WorkerEvent::MonitorSupport(false));
            None
        }
    };
    sink.send(WorkerEvent::Connected);

    // 后台作业继承 slave fd 时 master 读端永远等不到 EOF——不能只靠 out_rx 关闭判定退出。
    // 轮询 child.try_wait：shell 已死就收尾（先排空残留输出，再发 ShellExited）。
    let mut child_poll = tokio::time::interval(std::time::Duration::from_millis(200));
    child_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            // PTY 输出 → 终端。通道关闭（None）= 读线程收到 EOF = shell 已退出。
            chunk = out_rx.recv() => match chunk {
                Some(bytes) => sink.send(WorkerEvent::TerminalData(bytes)),
                None => {
                    // 读线程 EOF = shell 已退出。退出码带给 UI：挂起的 AI 运行能拿到
                    // finished=true + 真实退出码（exit 42 不再丢码），而不是笼统的断线。
                    // portable_pty 的 ExitStatus 是自有结构体（不是 std 的枚举）：
                    // exit_code() 直接给出码；被信号杀死时它记为 code=1。
                    let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
                    sink.send(WorkerEvent::ShellExited(code));
                    sink.send(WorkerEvent::Disconnected(
                        crate::i18n::tr("本机终端已退出", "Local terminal exited").into(),
                    ));
                    break;
                }
            },
            _ = child_poll.tick() => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // 排空读线程已送出、尚未被 select 取走的残留输出
                        while let Ok(bytes) = out_rx.try_recv() {
                            sink.send(WorkerEvent::TerminalData(bytes));
                        }
                        let code = status.exit_code() as i32;
                        sink.send(WorkerEvent::ShellExited(code));
                        sink.send(WorkerEvent::Disconnected(
                            crate::i18n::tr("本机终端已退出", "Local terminal exited").into(),
                        ));
                        break;
                    }
                    Ok(None) | Err(_) => {}
                }
            },
            cmd = async {
                if let Some(c) = cmd_prelude.pop_front() {
                    Some(c)
                } else {
                    cmd_rx.recv().await
                }
            } => match cmd {
                Some(UiCommand::TerminalInput(bytes)) => {
                    // 写线程退出（PTY 关闭）时通道发送会失败——忽略即可，随后的 EOF 会收尾。
                    let _ = in_tx.send(bytes);
                }
                Some(UiCommand::Resize { cols, rows }) => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Some(UiCommand::Disconnect) | None => {
                    // 主动断开 / UI 侧通道关闭：杀掉 shell 子进程并收尾。
                    // `kill` 之后必须 `wait` 回收，否则每关一个本机标签就留一个僵尸进程
                    // （上面的 EOF 分支本来就是这么做的，这里漏了）。
                    let _ = child.kill();
                    let _ = child.wait();
                    sink.send(WorkerEvent::Disconnected(
                        crate::i18n::tr("已断开", "Disconnected").into(),
                    ));
                    break;
                }
                // 文件浏览/读写/增删改（Phase 2）：交给本地文件处理器，独立任务执行不阻塞终端 I/O。
                // 尚不支持的命令（传输/转发/进程/PDF 等）由 files::handle 内部忽略。
                Some(other) => {
                    let s = sink.clone();
                    tokio::spawn(async move {
                        files::handle(other, &s).await;
                    });
                }
            },
        }
    }
    // 循环退出：停掉系统信息采样任务；master/in_tx 在此 drop → PTY 关闭、写线程结束；
    // out_rx drop → 读线程下次发送失败退出。
    if let Some(t) = sysinfo_task {
        t.abort();
    }
}

/// 起一个本地 PTY + shell 子进程，返回 (master 句柄, child 句柄, PTY 输出接收端, PTY 输入发送端)。
///
/// 读/写各用一条 std 阻塞线程桥接到异步侧：
/// - 读线程：阻塞 `read` PTY，字节经 `out_tx` 送出；EOF/错误即退出（`out_tx` 随之 drop，
///   下游 `recv()` 收到 `None`，据此判定 shell 已退出）。
/// - 写线程：从 `in_rx` 收键盘字节，阻塞 `write_all`+`flush` 到 PTY；PTY 关闭即退出。
#[allow(clippy::type_complexity)]
fn spawn_pty(
    cols: u16,
    rows: u16,
) -> anyhow::Result<(
    Box<dyn MasterPty + Send>,
    Box<dyn Child + Send + Sync>,
    UnboundedReceiver<Vec<u8>>,
    std::sync::mpsc::Sender<Vec<u8>>,
)> {
    use std::io::{Read, Write};

    let cols = if cols == 0 { DEFAULT_PTY_COLS } else { cols };
    let rows = if rows == 0 { DEFAULT_PTY_ROWS } else { rows };
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // 组装 shell 命令：继承本进程环境（保证 PATH/HOME 等齐全），再显式声明终端能力。
    let mut cmd = CommandBuilder::new(default_shell());
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Some(home) = home_dir() {
        cmd.cwd(home);
    }

    let child = SlavePty::spawn_command(&*pair.slave, cmd)?;
    // slave 端 fd 留着的话，shell 退出后 master 的读端永远等不到 EOF——spawn 完立即释放。
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = pair.master;

    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF 或读错误：shell 已退出
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).is_err() {
                        break; // 下游已关闭
                    }
                }
            }
        }
        // out_tx 在此 drop → 下游 recv() 收到 None
    });

    let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        while let Ok(bytes) = in_rx.recv() {
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break; // PTY 已关闭
            }
        }
    });

    Ok((master, child, out_rx, in_tx))
}

/// 本机默认交互式 shell：unix 取 `$SHELL`（须是可执行文件），否则依次试
/// bash → zsh → sh；Windows 取 `%COMSPEC%`（回退 `powershell.exe`）。
fn default_shell() -> String {
    #[cfg(unix)]
    {
        resolve_unix_shell()
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
    }
}

/// `$SHELL` 若指向存在的可执行文件就用它，否则沿 bash/zsh/sh 回退。
#[cfg(unix)]
fn resolve_unix_shell() -> String {
    let candidates: Vec<String> = {
        let mut v = Vec::new();
        if let Ok(s) = std::env::var("SHELL") {
            if !s.is_empty() {
                v.push(s);
            }
        }
        for p in [
            "/bin/bash",
            "/bin/zsh",
            "/bin/sh",
            "/usr/bin/bash",
            "/usr/bin/zsh",
            "/usr/bin/sh",
        ] {
            v.push(p.into());
        }
        v
    };
    for c in &candidates {
        if path_is_executable(c) {
            return c.clone();
        }
    }
    // 最后兜底：即使路径当前不可见（chroot/奇怪环境）也交给 spawn 报错。
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| "/bin/sh".into())
}

#[cfg(unix)]
fn path_is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let p = std::path::Path::new(path);
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
#[cfg(unix)]
mod shell_fallback_tests {
    use super::resolve_unix_shell;

    #[test]
    fn resolves_to_an_existing_executable() {
        let sh = resolve_unix_shell();
        assert!(super::path_is_executable(&sh), "应落到可执行的 shell：{sh}");
    }

    #[test]
    fn non_executable_file_is_not_a_shell() {
        let dir = std::env::temp_dir().join(format!("ishell-shell-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-shell");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o644);
        std::fs::set_permissions(&path, perm).unwrap();
        assert!(
            !super::path_is_executable(path.to_str().unwrap()),
            "没有可执行位的普通文件不能当 shell"
        );
        assert!(!super::path_is_executable(
            dir.join("missing").to_str().unwrap()
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 本机家目录：unix `$HOME`，Windows `%USERPROFILE%`。取不到则 None（PTY 用继承的 cwd）。
pub(super) fn home_dir() -> Option<String> {
    #[cfg(unix)]
    {
        std::env::var("HOME").ok()
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok()
    }
}
