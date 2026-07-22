//! 本机系统监控：直接复用 SSH 侧的 `PROBE_CMD` 探针脚本 + `SysSampler` 解析器，只把「经 SSH
//! exec 采样」换成「本地 shell 采样」——于是本机会话的系统信息侧栏与远端 Linux 会话完全一致
//! （CPU/内存/磁盘/网络/进程/GPU）。仅 Linux（读 `/proc`）；进程详情/结束同样在本机执行。

use std::time::Duration;

use crate::proto::WorkerEvent;
use crate::ssh::sysinfo::{SysSampler, PROBE_CMD};
use crate::ssh::UiSink;

/// 本地跑一段 shell 取 stdout（失败/无 stdout 返回 None）。放到阻塞线程池执行——tokio 未启用
/// `process` 特性，且 `std::process` 是阻塞的，不能在异步线程上直接跑。
async fn run_shell(cmd: String) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    })
    .await
    .ok()
    .flatten()
}

/// 周期性采样本机系统信息（每 2 秒一份，与 SSH 会话同频），经 `send_sysinfo` 覆盖式上报。
/// 被 worker 在退出时 `abort()`。先发一次 `MonitorSupport(true)` 让侧栏亮起。
pub(super) async fn run_sampler(sink: UiSink) {
    sink.send(WorkerEvent::MonitorSupport(true));
    let mut sampler = SysSampler::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    loop {
        ticker.tick().await;
        if let Some(raw) = run_shell(PROBE_CMD.to_string()).await {
            let info = sampler.parse(&raw);
            sink.send_sysinfo(info);
        }
    }
}

/// 进程详情：cmdline（NUL→空格）/ cwd / exe，三行返回。与 SSH 侧同一条命令，本机执行。
pub(super) async fn proc_detail(pid: u32, sink: &UiSink) {
    let cmd = format!(
        "cat /proc/{pid}/cmdline 2>/dev/null | tr '\\0' ' '; echo; \
         readlink /proc/{pid}/cwd 2>/dev/null; readlink /proc/{pid}/exe 2>/dev/null"
    );
    if let Some(out) = run_shell(cmd).await {
        let mut it = out.split('\n');
        let cmdline = it.next().unwrap_or("").trim().to_string();
        let cwd = it.next().unwrap_or("").trim().to_string();
        let exe = it.next().unwrap_or("").trim().to_string();
        sink.send(WorkerEvent::ProcDetail {
            pid,
            cmd: cmdline,
            cwd,
            exe,
        });
    }
}

/// 结束进程：先 SIGTERM，短等后仍存活则 SIGKILL；据退出码如实反馈。与 SSH 侧同一序列，本机执行。
pub(super) async fn kill_proc(pid: u32, sink: &UiSink) {
    let cmd = format!(
        "kill -15 {pid} 2>/dev/null; sleep 0.3; \
         kill -0 {pid} 2>/dev/null && kill -9 {pid}; \
         kill -0 {pid} 2>/dev/null && exit 1 || exit 0"
    );
    let ok = tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    let msg = if ok {
        match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("已结束进程 {pid}"),
            crate::i18n::Lang::En => format!("Killed {pid}"),
        }
    } else {
        crate::i18n::tr(
            "结束进程失败：权限不足或进程不存在",
            "Kill failed: permission denied or no such process",
        )
        .into()
    };
    sink.send(WorkerEvent::Status(msg));
}
