//! 连接诊断日志：`~/.config/ishell/connect.log`（Windows 为 `%APPDATA%\ishell\connect.log`）。
//!
//! GUI 从桌面启动时 stderr 没人看得到，「某台服务器时好时坏、换个启动方式又好了」这类问题
//! 事后什么都查不到。这里把每次连接的起点（进程上下文）、结果与耗时落盘；**失败时**再补一次
//! 独立的网络探测——把「域名解析」「TCP 能否建立」和「SSH 握手/认证」三层分开，操作系统
//! 的原始错误码（macOS 的 `No route to host`、Windows 的 10060/10065 等）直接记下来。
//!
//! 只记诊断信息：主机、端口、用户名、认证**种类**，绝不记口令/私钥/口令短语。
//! 探测只在失败后跑，成功的连接路径与以前完全一样。

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::proto::{AuthMethod, ConnectConfig};

/// 超过就把旧文件换成 `.old` 重新开始：这是给人看的诊断，不是审计日志。
const MAX_LOG: u64 = 256 * 1024;
/// 探测里解析和每个 TCP 连接各自的等待上限。
const PROBE_STEP: Duration = Duration::from_secs(5);
/// 最多探测几个解析出来的地址。
const PROBE_MAX_ADDRS: usize = 4;

fn log_path() -> Option<PathBuf> {
    Some(crate::store::crash_log_path()?.with_file_name("connect.log"))
}

/// `YYYY-MM-DD HH:MM:SS`（UTC）。不引入时间库：日期换算用标准的 days→civil 算法。
fn timestamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// 追加一行。任何 IO 错误一律忽略——诊断日志不能反过来影响连接。
pub fn note(line: &str) {
    let Some(path) = log_path() else { return };
    if let Some(dir) = path.parent() {
        crate::crash::create_private_dir(dir);
    }
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_LOG {
        let old = path.with_extension("log.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&path, &old);
    }
    let Some(mut f) = crate::crash::open_private_append(&path) else {
        return;
    };
    let _ = writeln!(f, "[{} UTC] {line}", timestamp(SystemTime::now()));
}

fn auth_kind(a: &AuthMethod) -> &'static str {
    match a {
        AuthMethod::Password(_) => "password",
        AuthMethod::KeyFile { .. } => "key-file",
        AuthMethod::Agent => "ssh-agent",
        AuthMethod::Interactive => "keyboard-interactive",
    }
}

/// 一次连接开始：目标与进程上下文。上下文里放的都是「终端启动 vs 双击启动」会不同的东西。
pub fn begin(cfg: &ConnectConfig) {
    let jump = cfg.jump.as_ref().map_or(String::new(), |j| {
        format!(
            " via {}@{}:{} ({})",
            j.username,
            j.host,
            j.port,
            auth_kind(&j.auth)
        )
    });
    let cwd = std::env::current_dir().map_or_else(|e| format!("<{e}>"), |p| p.display().to_string());
    note(&format!(
        "connect {}@{}:{} auth={}{jump} | ishell {} {}/{} pid={} cwd={cwd} SSH_AUTH_SOCK={} HOME={}",
        cfg.username,
        cfg.host,
        cfg.port,
        auth_kind(&cfg.auth),
        crate::version::VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        if std::env::var_os("SSH_AUTH_SOCK").is_some() { "set" } else { "unset" },
        if std::env::var_os("HOME").is_some() { "set" } else { "unset" },
    ));
}

/// 失败后的网络探测：解析主机名，再对每个地址单独试 TCP。与 SSH 库的连接互相独立，
/// 只回答「这台机器此刻能不能和这个地址建立 TCP」。
pub async fn probe(host: String, port: u16) {
    for line in probe_lines(&host, port).await {
        note(&line);
    }
}

async fn probe_lines(host: &str, port: u16) -> Vec<String> {
    let mut out = Vec::new();
    let t = std::time::Instant::now();
    let addrs = match tokio::time::timeout(PROBE_STEP, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(it)) => it.take(PROBE_MAX_ADDRS).collect::<Vec<_>>(),
        Ok(Err(e)) => {
            out.push(format!("probe {host}:{port} resolve FAILED in {:?}: {e}", t.elapsed()));
            return out;
        }
        Err(_) => {
            out.push(format!("probe {host}:{port} resolve TIMED OUT after {PROBE_STEP:?}"));
            return out;
        }
    };
    out.push(format!("probe {host}:{port} resolved in {:?}: {addrs:?}", t.elapsed()));
    for addr in addrs {
        let t = std::time::Instant::now();
        let verdict = match tokio::time::timeout(PROBE_STEP, tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(_)) => "TCP ok".to_string(),
            Ok(Err(e)) => format!("TCP FAILED: {e} (kind={:?}, os_error={:?})", e.kind(), e.raw_os_error()),
            Err(_) => format!("TCP TIMED OUT after {PROBE_STEP:?}"),
        };
        out.push(format!("probe {addr}: {verdict} in {:?}", t.elapsed()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_matches_known_instants() {
        let at = |s: u64| timestamp(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01 00:00:00");
        // 闰日与年末边界
        assert_eq!(at(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(at(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(at(1_793_000_000 + 86_399), "2026-10-27 07:33:19");
    }

    #[test]
    fn probe_reports_a_refused_port_with_the_os_error() {
        // 绑定后立刻释放，得到一个当前没人监听的回环端口
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let lines = rt.block_on(probe_lines("127.0.0.1", port));
        assert!(lines[0].contains("resolved"), "{lines:?}");
        assert!(lines[1].contains("TCP FAILED") && lines[1].contains("ConnectionRefused"), "{lines:?}");
    }

    #[test]
    fn probe_reports_an_open_port_as_ok() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let lines = rt.block_on(probe_lines("127.0.0.1", port));
        assert!(lines[1].contains("TCP ok"), "{lines:?}");
    }
}
