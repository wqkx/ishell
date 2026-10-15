//! 本机终端会话的 MCP 登记：给自己拉起的那个 shell 写一个会话文件。
//!
//! 与远端会话（`ssh/mcp_session.rs`）是同一份文件格式、同一套「代理沿进程祖先找主人」的规则；
//! 区别只在于：这里 shell 是 iShell 自己 `spawn` 出来的，pid 本来就知道，不用去远端猜。
//! 端点是本进程自己监听的 socket（`store::mcp_socket_path`），不经任何反向转发。

use std::path::PathBuf;

/// 会话文件的持有者：drop 时删文件（shell 退出 / 会话被关）。
pub(super) struct SessionFile(PathBuf);

impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 为本机会话的 shell 写会话文件。`Ok(None)` = 本次运行没有 MCP 可登记（开关关着 / 没在监听），
/// 不是错误；`Err` = 该登记却失败了，调用方要让用户看见。
pub(super) fn register(shell_pid: u32) -> Result<Option<SessionFile>, String> {
    #[cfg(not(unix))]
    {
        let _ = shell_pid;
        Ok(None)
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if !crate::store::load_mcp_consent() || !crate::store::mcp_listening() {
            return Ok(None);
        }
        let key = crate::store::mcp_run_key().ok_or("本机熵源不可用")?;
        let endpoint = crate::store::mcp_socket_path()
            .and_then(|p| p.to_str().map(str::to_string))
            .ok_or("无法确定 MCP socket 路径")?;
        let home = std::env::var_os("HOME").ok_or("没有 HOME")?;
        let dir = crate::mcp_protocol::session_dir(std::path::Path::new(&home));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("无法创建 {}：{e}", dir.display()))?;
        // 清掉 shell 已经不在的旧文件（崩溃残留）
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                // 写到一半崩溃留下的 `<pid>.tmp.<n>`（含密钥）：超过一分钟就是残留，删。
                let is_stale_tmp = e.file_name().to_str().is_some_and(|n| n.contains(".tmp."))
                    && e.metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(60));
                if is_stale_tmp {
                    let _ = std::fs::remove_file(e.path());
                    continue;
                }
                let alive = e
                    .file_name()
                    .to_str()
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_none_or(|p| crate::mcp_protocol::proc_info(p).is_some());
                if !alive {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let start = crate::mcp_protocol::proc_info(shell_pid)
            .ok_or("读不到本机 shell 的进程信息")?
            .start;
        let rec = crate::mcp_protocol::SessionRecord {
            endpoint,
            key: key.to_string(),
            shell_pid,
            shell_start: start,
        };
        let path = dir.join(shell_pid.to_string());
        let tmp = dir.join(format!("{shell_pid}.tmp.{}", std::process::id()));
        let write = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(rec.render().as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("无法写会话文件 {}：{e}", path.display()));
        }
        Ok(Some(SessionFile(path)))
    }
}
