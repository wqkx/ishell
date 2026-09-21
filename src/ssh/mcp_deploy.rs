//! 把内嵌的 `ishell-mcp` 代理二进制部署到当前这台服务器。
//!
//! 用途见 [`crate::mcp_embed`]：AI 跑在服务器上时，代理也必须在那台服务器上，而它和 GUI
//! 必须**同版本**。手工装（下载正确架构 → scp → install-mcp.sh）既繁琐又容易忘记升级；
//! iShell 手上本来就有这条连接的 SFTP 通道，一次点击就能做完。
//!
//! 流程：`uname -m` 定架构 → 建 `~/.ishell-mcp/bin` → 事务写（临时文件 + rename 原子换入，
//! 这样覆盖一个**正在运行**的代理也不会撞 "Text file busy"）→ `chmod 755` → 跑一次
//! `--version` 确认它在那台机器上真能执行（架构挑错时这一步会当场暴露，而不是等用户去用
//! 的时候收到一句莫名其妙的 "cannot execute binary file"）。

use russh::client::Handle;

use super::auth::{exec_capture, exec_capture_bytes, exec_status, open_sftp, ClientHandler};
use super::sftp::sftp_write_atomic;
use super::UiSink;

/// 探测远端架构。不要加 `2>/dev/null`：csh/tcsh 会把它当成语法错误，uname 根本不跑。
fn arch_probe_command() -> &'static str {
    "uname -m"
}

/// 两级都 `mkdir -p -m 700`，再 `chmod 700`。`-m` 只作用于新建的最后一级，已有的
/// 755 目录不会被 mkdir 改掉，所以 chmod 必须点名家目录下的 `.ishell-mcp` 和 bin 目录。
fn ensure_private_mcp_dirs_command(home: &str, dir: &str) -> String {
    format!(
        "mkdir -p -m 700 '{home}/.ishell-mcp' && mkdir -p -m 700 '{dir}' && chmod 700 '{home}/.ishell-mcp' '{dir}'"
    )
}

/// 部署结果：`(是否成功, 给用户看的话)`。第二个返回值成功时是远端可执行文件的绝对路径。
pub(super) async fn deploy_mcp_agent(
    handle: &Handle<ClientHandler>,
    sink: &UiSink,
) -> (bool, String) {
    let zh = matches!(crate::i18n::current(), crate::i18n::Lang::Zh);
    macro_rules! fail {
        ($zh:expr, $en:expr) => {
            return (false, if zh { $zh } else { $en })
        };
    }

    // 1) 架构。不用 `2>/dev/null`：csh/tcsh 把这写成语法错误，uname 根本没跑，
    //    stdout 是空的，后面就报「这台服务器是 ，…」。stderr 本来也不进 exec_capture。
    let uname = match exec_capture(handle, arch_probe_command()).await {
        Ok(u) => u,
        Err(e) => fail!(
            format!("探测服务器架构失败：{e}"),
            format!("Could not detect the server architecture: {e}")
        ),
    };
    let arch = uname
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
        .unwrap_or("")
        .to_string();
    let Some(blob) = crate::mcp_embed::agent_for_uname(&arch) else {
        let have = crate::mcp_embed::embedded_arches().join(" / ");
        let have = if have.is_empty() {
            if zh {
                "（本次构建没有内嵌任何代理）".to_string()
            } else {
                "(this build embeds none)".to_string()
            }
        } else {
            have
        };
        fail!(
            format!("这台服务器是 {arch}，本版 iShell 只内嵌了 {have} 的代理。请按 README 手工安装。"),
            format!("This server is {arch}; this iShell build only embeds {have}. Install manually — see the README.")
        );
    };

    // 2) 目标路径。用 SFTP 的 canonicalize(".") 拿家目录，不去猜 /home/<user>。
    let sftp = match open_sftp(handle).await {
        Ok(s) => s,
        Err(e) => fail!(
            format!("打开 SFTP 通道失败：{e}"),
            format!("Could not open an SFTP channel: {e}")
        ),
    };
    let home = match sftp.canonicalize(".").await {
        Ok(h) => h.trim_end_matches('/').to_string(),
        Err(e) => fail!(
            format!("无法确定服务器上的家目录：{e}"),
            format!("Could not resolve the home directory on the server: {e}")
        ),
    };
    // 家目录里带单引号的情况极其罕见，但下面 chmod 要经 shell，宁可明说也不要拼出一条
    // 会被引号截断的命令。
    if home.contains('\'') {
        fail!(
            format!("家目录路径里含单引号（{home}），无法安全地执行安装命令，请按 README 手工安装。"),
            format!("The home directory path contains a single quote ({home}); refusing to build a shell command. Install manually — see the README.")
        );
    }
    let dir = format!("{home}/{}", crate::mcp_embed::REMOTE_DIR);
    let path = format!("{dir}/{}", crate::mcp_embed::REMOTE_NAME);

    // 3) 建目录 + 事务写
    //
    // 用 `mkdir -p -m 700` 而不是 SFTP 的逐级 create_dir：`~/.ishell-mcp` 同时是反向转发
    // 落 socket 的目录，那边就是按 700 建的（见 ssh/mod.rs）。若这里先用默认 umask 把它
    // 建成 755，socket 目录就悄悄变松了。`-m` 只作用于**最后一级**，所以两级各建一次。
    // `-m` 只作用于新建目录，已存在的 755 `~/.ishell-mcp` 不会被改。socket 落在这里，
    // 补一次 chmod，把以前用默认 umask 建出来的目录收紧。
    match exec_status(handle, &ensure_private_mcp_dirs_command(&home, &dir))
    .await
    {
        Ok((0, _)) => {}
        Ok((_, err)) => fail!(
            format!("在服务器上创建 {dir} 失败：{}", err.trim()),
            format!("Could not create {dir} on the server: {}", err.trim())
        ),
        Err(e) => fail!(
            format!("在服务器上创建 {dir} 失败：{e}"),
            format!("Could not create {dir} on the server: {e}")
        ),
    }
    if let Err(e) = sftp_write_atomic(&sftp, &path, blob, sink).await {
        fail!(
            format!("写入 {path} 失败：{e}"),
            format!("Failed to write {path}: {e}")
        );
    }

    // 4) 置可执行位。新建文件继承不到权限（事务写只复制**原文件**的权限位，首次安装没有原文件），
    //    不 chmod 的话传上去的是一个不能执行的文件。
    match exec_status(handle, &format!("chmod 755 '{path}'")).await {
        Ok((0, _)) => {}
        Ok((_, err)) => fail!(
            format!("chmod 755 {path} 失败：{}", err.trim()),
            format!("chmod 755 {path} failed: {}", err.trim())
        ),
        Err(e) => fail!(
            format!("chmod 755 {path} 失败：{e}"),
            format!("chmod 755 {path} failed: {e}")
        ),
    }

    // 5) 真跑一次，确认架构没挑错、动态库也齐。
    //
    // 不能只在输出里找子串 "ishell-mcp"：执行失败的 shell 报错（cannot execute /
    // Permission denied / not found）自己就带这条路径，路径里必然有这个名字，检查恒过。
    // 要退出码 0，并且 stdout 是 `ishell-mcp <版本> proto <数字>` 这一行。
    match exec_capture_bytes(handle, &format!("'{path}' --version")).await {
        Ok((0, out, _)) if mcp_version_line_ok(&String::from_utf8_lossy(&out)) => (true, path),
        Ok((code, out, err)) => {
            let detail = format!("{}{}", String::from_utf8_lossy(&out), err);
            let detail = detail.trim();
            let detail = if detail.is_empty() {
                format!("exit {code}")
            } else {
                format!("exit {code}: {detail}")
            };
            (
                false,
                if zh {
                    format!("已传到 {path}，但在服务器上执行 --version 失败：{detail}")
                } else {
                    format!("Uploaded to {path}, but --version failed on the server: {detail}")
                },
            )
        }
        Err(e) => (
            false,
            if zh {
                format!("已传到 {path}，但在服务器上执行不起来：{e}")
            } else {
                format!("Uploaded to {path}, but it does not run on the server: {e}")
            },
        ),
    }
}

/// `--version` 的 stdout。大小写敏感：报错文本里的路径不该混进来。
fn mcp_version_line_ok(stdout: &str) -> bool {
    let mut parts = stdout.trim().split_whitespace();
    let name = parts.next();
    let ver = parts.next();
    let proto_word = parts.next();
    let proto = parts.next();
    name == Some("ishell-mcp")
        && ver.is_some_and(|v| {
            !v.is_empty()
                && v.chars().all(|c| c.is_ascii_digit() || c == '.')
                && v.chars().any(|c| c.is_ascii_digit())
        })
        && proto_word == Some("proto")
        && proto.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        && parts.next().is_none()
}

#[cfg(test)]
mod version_line_tests {
    use super::mcp_version_line_ok;

    #[test]
    fn accepts_real_version_line_only() {
        assert!(mcp_version_line_ok("ishell-mcp 0.23.0 proto 7\n"));
        assert!(!mcp_version_line_ok(
            "bash: /home/u/.ishell-mcp/bin/ishell-mcp: cannot execute binary file"
        ));
        assert!(!mcp_version_line_ok(
            "sh: /home/u/.ishell-mcp/bin/ishell-mcp: not found"
        ));
        assert!(!mcp_version_line_ok("ISHELL-MCP 1.0 proto 1"));
        assert!(!mcp_version_line_ok("ishell-mcp 0.23.0 proto 7 extra"));
        assert!(!mcp_version_line_ok("ishell-mcp .. proto 7"));
        assert!(!mcp_version_line_ok("ishell-mcp 0.23.0 protocol 7"));
        assert!(!mcp_version_line_ok(""));
        assert!(mcp_version_line_ok("  ishell-mcp 1 proto 12  "));
    }

    #[test]
    fn arch_probe_is_plain_uname_and_dirs_are_chmodded() {
        assert_eq!(super::arch_probe_command(), "uname -m");
        assert!(!super::arch_probe_command().contains('>'));
        let cmd = super::ensure_private_mcp_dirs_command("/home/u", "/home/u/.ishell-mcp/bin");
        assert!(cmd.contains("mkdir -p -m 700 '/home/u/.ishell-mcp'"));
        assert!(cmd.contains("mkdir -p -m 700 '/home/u/.ishell-mcp/bin'"));
        assert!(cmd.contains("chmod 700 '/home/u/.ishell-mcp' '/home/u/.ishell-mcp/bin'"));
    }
}

/// 部署路径的**真·服务器**集成测试。
///
/// 这条路径的价值全在「装上去真的能跑」，而这句话只有在一台真机上才验得了：架构挑得对不对、
/// 目录权限、可执行位、以及那句 `--version` 到底打不打得出来。纯单元测试一条都碰不到。
///
/// 与 `live_sftp_*` 那组共用同一批环境变量，跑法：
///
/// ```text
/// scripts/test-live-sftp.sh 127.0.0.1 "$USER" ~/.ssh/id_ed25519   # 那组
/// ISHELL_TEST_SSH_HOST=… ISHELL_TEST_SSH_USER=… ISHELL_TEST_SSH_KEY=… \
///   cargo test -- --ignored live_deploy                            # 这条
/// ```
///
/// 注意：它走的是**产品自己的** `connect()`，因此会按 TOFU 把目标主机写进 `~/.ssh/known_hosts`
/// （测试里对主机密钥确认一律答"信任"）；且会真的往服务器的 `~/.ishell-mcp/bin/` 装一份代理。
/// 两者都是这个功能本来就要做的事，测试不额外造假。
#[cfg(test)]
mod live_deploy_tests {
    use super::*;
    use crate::proto::{AuthMethod, ConnectConfig, Transport};

    /// 环境变量没配齐、或本次构建压根没内嵌代理时，直接跳过（返回 None）。
    fn cfg_or_skip() -> Option<ConnectConfig> {
        if !crate::mcp_embed::has_embedded() {
            eprintln!("跳过：本次构建没有内嵌代理（设 ISHELL_EMBED_MCP_* 后重编，见 BUILD.md）");
            return None;
        }
        let host = std::env::var("ISHELL_TEST_SSH_HOST").ok()?;
        let user = std::env::var("ISHELL_TEST_SSH_USER").ok()?;
        let key = std::env::var("ISHELL_TEST_SSH_KEY").ok()?;
        let port = std::env::var("ISHELL_TEST_SSH_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(22);
        Some(ConnectConfig {
            host,
            port,
            username: user,
            auth: AuthMethod::KeyFile {
                path: key,
                passphrase: None,
            },
            label: "live-deploy-test".into(),
            jump: None,
            forward_agent: false,
            forward_x11: false,
            transport: Transport::default(),
        })
    }

    #[tokio::test]
    #[ignore = "需要真实 sshd，见模块文档"]
    async fn live_deploy_installs_a_runnable_agent() {
        let Some(cfg) = cfg_or_skip() else { return };

        // 产品用的那套 sink：事件收到一个普通 channel 里，测试不关心内容。
        let (evt_tx, _evt_rx) = std::sync::mpsc::sync_channel(64);
        let (sysinfo_tx, _sysinfo_rx) = tokio::sync::watch::channel(None);
        let sink = UiSink::new(
            evt_tx,
            egui::Context::default(),
            std::sync::Arc::new(sysinfo_tx),
        );
        // 主机密钥确认：一律答"信任"（TOFU 首次连接会问一次）。
        let (hostkey_tx, hostkey_rx) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..4 {
            let _ = hostkey_tx.send(true);
        }
        let (_cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let (handle, _jump, _rf, _x11, _prelude) =
            super::super::auth::connect(&cfg, &sink, hostkey_rx, &mut cmd_rx)
                .await
                .expect("连上测试服务器");

        let (ok, msg) = deploy_mcp_agent(&handle, &sink).await;
        assert!(ok, "部署失败：{msg}");
        assert!(
            msg.ends_with(&format!("/{}", crate::mcp_embed::REMOTE_NAME)),
            "成功时应当返回远端可执行文件的绝对路径，实际是：{msg}"
        );

        // 再独立确认一遍：那个路径上的东西确实可执行、且报的是**本次构建**的版本。
        // 版本对不上意味着嵌进来的是别的构建的产物——那正是这个功能要根除的问题。
        let out = exec_capture(&handle, &format!("'{msg}' --version 2>&1"))
            .await
            .expect("在服务器上执行装好的代理");
        assert!(
            out.contains(env!("CARGO_PKG_VERSION")),
            "装上去的代理版本不是本次构建的（{}）：{out}",
            env!("CARGO_PKG_VERSION")
        );

        // 可执行位必须真的置上了（chmod 那步是新写的，最容易漏）。
        let (code, _) = exec_status(&handle, &format!("test -x '{msg}'"))
            .await
            .expect("检查可执行位");
        assert_eq!(code, 0, "{msg} 没有可执行位");
    }
}
