//! 跨主机直传（源机 rsync/scp）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use russh::client::Handle;
use russh::ChannelMsg;

use crate::proto::WorkerEvent;

use super::super::auth::{exec_capture, exec_status, ClientHandler};
use super::super::sftp::sftp_overwrite;
use super::super::{sh_quote, UiSink};
use super::rand_hex;
use super::util::join_quoted;

struct TmpKeyGuard {
    handle: Arc<Handle<ClientHandler>>,
    dir: String,
}

impl Drop for TmpKeyGuard {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        let d = sh_quote(&self.dir);
        tokio::spawn(async move {
            let _ = exec_status(
                &handle,
                &format!("shred -uf {d}/key 2>/dev/null; rm -rf {d}"),
            )
            .await;
        });
    }
}

/// 跨主机「直传」：在源主机上用 rsync（无则 scp）把 srcs 直接推到目标主机，数据不经本地。
/// 目标认证仅支持「无口令密钥」：把 B 私钥临时投放到源主机 0700 私有目录，传完/取消即清（见 TmpKeyGuard）。
/// 任一步失败都回报失败，由上层弹「转中转」提醒。
pub(super) async fn direct_transfer(
    handle: Arc<Handle<ClientHandler>>,
    sftp: Arc<russh_sftp::client::SftpSession>,
    spec: crate::proto::DirectSpec,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let id = spec.id;
    // 先用 du 估算源端总字节，让进度条与文件大小都按真实字节显示
    let total = direct_total_bytes(&handle, &spec.srcs).await;
    sink.send(WorkerEvent::TransferStart {
        id,
        name: spec.label.clone(),
        total,
        dir: crate::proto::TransferDir::Upload,
        local: None,
    });
    let res = direct_transfer_inner(&handle, &sftp, &spec, sink, &cancel).await;
    match res {
        Ok(()) => {
            // 收尾把进度拉满（du 估算与实传可能有微小出入）
            if total > 0 {
                sink.send(WorkerEvent::TransferProgress { id, done: total });
            }
            sink.send(WorkerEvent::TransferDone {
                id,
                ok: true,
                message: crate::i18n::tr("直传完成", "Direct transfer done").into(),
                refresh_dir: None,
            });
        }
        Err(e) => sink.send(WorkerEvent::TransferDone {
            id,
            ok: false,
            message: format!("{e}"),
            refresh_dir: None,
        }),
    }
}

/// 用 du 估算一组源路径的总字节（grand total 行）；失败返回 0（进度条退化为不确定）。
pub(super) async fn direct_total_bytes(handle: &Handle<ClientHandler>, srcs: &[String]) -> u64 {
    let q = join_quoted(srcs);
    // -s 汇总、-b 字节、-c 末尾输出 total 行；取 total 行的首列
    let out = exec_capture(handle, &format!("du -sbc -- {q}2>/dev/null | tail -1"))
        .await
        .unwrap_or_default();
    out.split_whitespace()
        .next()
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0)
}

pub(super) async fn direct_transfer_inner(
    handle: &Arc<Handle<ClientHandler>>,
    sftp: &Arc<russh_sftp::client::SftpSession>,
    spec: &crate::proto::DirectSpec,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // 1) 读取本地 B 私钥（本进程可直接访问本地文件）
    let key_bytes = std::fs::read(&spec.key_path).map_err(|e| {
        anyhow::anyhow!(
            "{}",
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("读取目标私钥失败：{e}"),
                crate::i18n::Lang::En => format!("Read target key failed: {e}"),
            }
        )
    })?;
    // 2) 先建 0700 私有目录（mkdir -m 700 在创建时即定权，杜绝「写入→改权」之间的可读窗口），
    //    密钥放其中。TmpKeyGuard 保证正常/失败/取消(future 被 drop) 各路径都清理该目录。
    let tmp_dir = format!("/tmp/.ishell_kd_{}_{}", spec.id, rand_hex(8));
    if !matches!(
        exec_status(handle, &format!("mkdir -m 700 {}", sh_quote(&tmp_dir))).await,
        Ok((0, _))
    ) {
        anyhow::bail!(
            "{}",
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => "创建临时密钥目录失败".to_string(),
                crate::i18n::Lang::En => "Create temp key dir failed".to_string(),
            }
        );
    }
    let _key_guard = TmpKeyGuard {
        handle: handle.clone(),
        dir: tmp_dir.clone(),
    };
    let tmp_key = format!("{tmp_dir}/key");
    sftp_overwrite(sftp, &tmp_key, &key_bytes)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("投放临时密钥失败：{e}"),
                    crate::i18n::Lang::En => format!("Place temp key failed: {e}"),
                }
            )
        })?;
    let _ = exec_status(handle, &format!("chmod 600 {}", sh_quote(&tmp_key))).await; // 纵深防御（目录已 700）

    // 3) 拼接源路径与目标
    let srcs = join_quoted(&spec.srcs);
    // 目标强制以 "/" 结尾，令 rsync/scp 把源放「入目录」而非改名
    let dest = sh_quote(&format!(
        "{}@{}:{}/",
        spec.dest_user, spec.dest_host, spec.dest_dir
    ));
    // 主机密钥策略：本机已信任目标 → yes（拒绝未知/变更）；用户确认过的首次 → accept-new。
    // 绝不使用 StrictHostKeyChecking=no。
    let hk = if spec.dest_host_known {
        "yes"
    } else {
        "accept-new"
    };
    let ssh_opt = format!(
        "ssh -p {} -i {} -o StrictHostKeyChecking={} -o BatchMode=yes -o ConnectTimeout=15",
        spec.dest_port,
        sh_quote(&tmp_key),
        hk
    );

    // 4) 优先 rsync（可解析进度、可续传）；缺失则回退 scp（仅 spinner）
    let has_rsync = matches!(
        exec_status(handle, "command -v rsync >/dev/null 2>&1").await,
        Ok((0, _))
    );
    let cmd = if has_rsync {
        format!(
            "rsync -a --info=progress2 -e {} -- {}{}",
            sh_quote(&ssh_opt),
            srcs,
            dest
        )
    } else {
        // scp 用大写 -P 指定端口；其余 ssh 选项同样适用
        format!(
            "scp -P {} -i {} -o StrictHostKeyChecking={} -o BatchMode=yes -o ConnectTimeout=15 -r -- {}{}",
            spec.dest_port, sh_quote(&tmp_key), hk, srcs, dest
        )
    };

    // 临时私钥的清理交由 _key_guard 在作用域结束（含取消时的 future drop）异步完成
    let (code, err) = exec_direct_progress(handle, &cmd, spec.id, sink, cancel).await?;
    if code != 0 {
        let reason = err.trim();
        anyhow::bail!(
            "{}",
            match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!(
                    "直传失败（码 {code}）：{}",
                    if reason.is_empty() {
                        "源主机无法连到目标主机"
                    } else {
                        reason
                    }
                ),
                crate::i18n::Lang::En => format!(
                    "Direct transfer failed (code {code}): {}",
                    if reason.is_empty() {
                        "source host cannot reach target"
                    } else {
                        reason
                    }
                ),
            }
        );
    }
    Ok(())
}

/// 执行直传命令并解析 rsync `--info=progress2` 的「已传字节」（首列），按字节上报进度。
/// 返回 (退出码, stderr)。被取消时（cancel 置位）提前结束读取，外层 select 也会 drop 整个 future。
pub(super) async fn exec_direct_progress(
    handle: &Handle<ClientHandler>,
    cmd: &str,
    id: u64,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<(i32, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    let mut code = -1i32;
    let mut err = Vec::new();
    let mut tail = String::new();
    while let Some(msg) = channel.wait().await {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match msg {
            ChannelMsg::Data { data } => {
                // rsync 进度写在 stdout，用 \r 原地刷新；累积当前行并提取首列「已传字节」
                tail.push_str(&String::from_utf8_lossy(&data));
                if let Some(done) = last_rsync_bytes(&tail) {
                    sink.send(WorkerEvent::TransferProgress { id, done });
                }
                // 仅保留最后一段（CR/LF 之后），防止缓冲无限增长；CR/LF 为 ASCII，切分点是字符边界
                if let Some(p) = tail.rfind(['\r', '\n']) {
                    tail = tail[p + 1..].to_string();
                }
            }
            ChannelMsg::ExtendedData { data, ext: 1 } => err.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,
            _ => {}
        }
    }
    Ok((code, String::from_utf8_lossy(&err).into_owned()))
}

/// 从 rsync `--info=progress2` 输出里提取「最新一行」的首列已传字节数。
/// 进度行形如 `  1,234,567  45%  10.50MB/s  0:00:05`，用 \r 原地刷新；
/// 取 CR/LF 后最后一段非空行的首 token，去掉千分位逗号后解析为字节。
pub(super) fn last_rsync_bytes(s: &str) -> Option<u64> {
    let line = s.rsplit(['\r', '\n']).find(|seg| !seg.trim().is_empty())?;
    let tok = line.split_whitespace().next()?;
    let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}
