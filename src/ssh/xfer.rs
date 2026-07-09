//! SSH 文件传输：从 ssh God Object 拆出，行为不变。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::Handle;
use russh::ChannelMsg;
use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{ConflictPolicy, WorkerEvent};

use super::auth::{exec_capture, exec_status, open_sftp, ClientHandler};
use super::sftp::{join_remote, remote_parent, sftp_overwrite};
use super::{sh_quote, UiSink};

/// 同一会话同时进行的最大传输数（不同会话各自独立）。
pub(super) const MAX_CONCURRENT_XFER: usize = 6;

/// 待执行/进行中的传输任务描述。
pub(super) enum PendingXfer {
    Download { id: u64, remote: String, local: String, policy: ConflictPolicy },
    Upload { id: u64, local: String, remote_dir: String, policy: ConflictPolicy },
    /// 跨主机直传：在本（源）主机上 rsync/scp 直推到目标主机
    Direct(Box<crate::proto::DirectSpec>),
}

impl PendingXfer {
    pub(super) fn id(&self) -> u64 {
        match self {
            PendingXfer::Download { id, .. } | PendingXfer::Upload { id, .. } => *id,
            PendingXfer::Direct(d) => d.id,
        }
    }
}

/// 单个传输的取消句柄：
/// - `flag` 供传输内部（含 download 已 detach 的分块子任务）协作式中止；
/// - `stop` 一次性信号，触发后直接 drop 整个传输 future——能立即中止卡在
///   SFTP `flush`/`shutdown`（pipelined 写的真正落地处）里的上传，标志位无法覆盖这里。
pub(super) struct XferCancel {
    pub(super) flag: Arc<AtomicBool>,
    pub(super) stop: Option<tokio::sync::oneshot::Sender<()>>,
}

/// 启动一个传输任务：登记取消句柄，spawn 后台任务，完成时通过 `done_tx` 通知主循环。
pub(super) fn start_xfer(
    handle: &Arc<Handle<ClientHandler>>,
    sink: &UiSink,
    done_tx: &UnboundedSender<u64>,
    cancels: &mut HashMap<u64, XferCancel>,
    p: PendingXfer,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let id = p.id();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    cancels.insert(id, XferCancel { flag: cancel.clone(), stop: Some(stop_tx) });
    let h = handle.clone();
    let s = sink.clone();
    let s_cancel = sink.clone();
    let cancel_work = cancel.clone();
    let dtx = done_tx.clone();
    tokio::spawn(async move {
        // 实际传输；被取消时整个 future 在 select 中被 drop，正在进行的 SFTP 写/flush 立即中止
        let work = async move {
            match open_sftp(&h).await {
                Ok(sftp) => {
                    let sftp = Arc::new(sftp);
                    match p {
                        PendingXfer::Download { id, remote, local, policy } => download(h.clone(), sftp, id, remote, local, policy, &s, cancel_work).await,
                        PendingXfer::Upload { id, local, remote_dir, policy } => upload(sftp.as_ref(), id, local, remote_dir, policy, &s, cancel_work).await,
                        PendingXfer::Direct(spec) => direct_transfer(h.clone(), sftp, *spec, &s, cancel_work).await,
                    }
                }
                Err(e) => s.send(WorkerEvent::TransferDone {
                    id, ok: false, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"), crate::i18n::Lang::En => format!("SFTP unavailable: {e}") }, refresh_dir: None,
                }),
            }
        };
        tokio::select! {
            biased; // 先看传输是否已完成，避免「刚好完成」时还报成取消
            _ = work => {}
            _ = &mut stop_rx => {
                // 通知可能已 detach 的子任务（download 分块）也尽快停下
                cancel.store(true, Ordering::Relaxed);
                s_cancel.send(WorkerEvent::TransferDone {
                    id, ok: false, message: crate::i18n::tr("已取消", "Canceled").into(), refresh_dir: None,
                });
            }
        }
        let _ = dtx.send(id);
    });
}
/// 生成 n 字节的随机十六进制串（用于临时文件名，避免可预测路径被 symlink 抢占）。
pub(super) fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if getrandom::getrandom(&mut b).is_err() {
        // getrandom 失败（极罕见）：用 pid + 单调计数器 + 栈地址(ASLR) 混出非常量回退，
        // 避免退化为固定全零名——临时文件名靠它防 /tmp 共享目录上的可预测 symlink 抢占。
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let mut x = (std::process::id() as u64)
            ^ CTR.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            ^ (&b as *const _ as u64);
        for byte in b.iter_mut() {
            // splitmix64 扩展
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *byte = ((z ^ (z >> 31)) & 0xff) as u8;
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 本地解压 tar.gz 到 dest 目录（纯 Rust，不依赖系统 tar）。
/// 逐条校验路径：拒绝绝对路径、`..` 组件与指向 dest 外的链接，防止路径穿越写任意本地文件。
fn extract_tar_gz(path: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    let f = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    std::fs::create_dir_all(dest)?;
    let dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    for entry in ar.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        if !tar_entry_path_safe(&entry_path) {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("拒绝不安全的归档路径：{}", entry_path.display()),
                    crate::i18n::Lang::En => format!("Refusing unsafe archive path: {}", entry_path.display()),
                }
            );
        }
        // unpack_in 将相对路径落在 dest 下；返回 false 表示被跳过（含 ..）
        if !entry.unpack_in(&dest)? {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("归档条目无法安全解压：{}", entry_path.display()),
                    crate::i18n::Lang::En => format!("Archive entry could not be unpacked safely: {}", entry_path.display()),
                }
            );
        }
    }
    Ok(())
}

/// 归档条目路径是否可安全解压到目标目录内（相对路径、无 `..`、非绝对）。
fn tar_entry_path_safe(p: &std::path::Path) -> bool {
    use std::path::Component;
    if p.as_os_str().is_empty() || p.is_absolute() {
        return false;
    }
    for c in p.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}
/// 下载（单文件或整个目录）并上报进度。大文件用多个并发分段读取流水线化，
/// 抵消 SFTP「单请求等一个往返」的吞吐瓶颈（高延迟链路上提速明显）。
/// 压缩下载一个目录：远端 tar.gz 打包到临时文件 → 单文件并发下载 → 本地解包。
/// 进度按压缩包字节上报。返回 Err 表示不支持/失败（上层回退到逐文件）。
async fn download_dir_compressed(
    handle: &Arc<Handle<ClientHandler>>,
    sftp: &Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote: &str,
    local: &str,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let name = basename(remote);
    let parent = remote_parent(remote);
    // 随机文件名：防止可预测路径在共享 /tmp 上被预置 symlink 抢占（竞态/越权写）
    let tmp_remote = format!("/tmp/.ishell_dl_{id}_{}.tar.gz", rand_hex(8));

    // 先登记传输行（total 未知），并把「打包中…」作为阶段提示上报——
    // 大目录 tar 打包可能耗时数十秒，此前 UI 一片空白，用户以为卡死。
    sink.send(WorkerEvent::TransferStart { id, name: name.clone(), total: 0, dir: crate::proto::TransferDir::Download, local: Some(local.to_string()) });
    sink.send(WorkerEvent::TransferNote { id, note: crate::i18n::tr("打包中…", "Packing…").into() });

    // 远端打包（czf：gzip 默认级别；-C 进入父目录，仅打包目标目录名）
    let cmd = format!("tar czf {} -C {} {}", sh_quote(&tmp_remote), sh_quote(&parent), sh_quote(&name));
    let (code, err) = exec_status(handle, &cmd).await?;
    if code != 0 {
        let _ = exec_status(handle, &format!("rm -f {}", sh_quote(&tmp_remote))).await;
        anyhow::bail!("{}", match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("tar 打包失败（{code}）：{err}"),
            crate::i18n::Lang::En => format!("tar pack failed ({code}): {err}"),
        });
    }
    let size = sftp.metadata(&tmp_remote).await.ok().and_then(|m| m.size).unwrap_or(0);
    // 打包完成：更新真实总量并清除阶段提示，进入正常字节进度
    sink.send(WorkerEvent::TransferStart { id, name: name.clone(), total: size, dir: crate::proto::TransferDir::Download, local: Some(local.to_string()) });
    sink.send(WorkerEvent::TransferNote { id, note: String::new() });

    // 下载压缩包到本地临时文件（并发分段 + 进度）
    let local_tgz = std::path::PathBuf::from(format!("{local}.ishelldl.{}.tgz", rand_hex(6)));
    let done = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let prog = {
        let (d, s, st) = (done.clone(), sink.clone(), stop.clone());
        tokio::spawn(async move {
            let mut last = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let v = d.load(Ordering::Relaxed);
                if v != last {
                    last = v;
                    s.send(WorkerEvent::TransferProgress { id, done: v });
                }
                if st.load(Ordering::Relaxed) {
                    break;
                }
            }
        })
    };
    let dl = download_file(sftp, &tmp_remote, &local_tgz, size, 0, cancel, &done).await; // 临时打包文件不跨次续传（mtime=0）
    stop.store(true, Ordering::Relaxed);
    let _ = prog.await;
    // 清理远端临时包（无论成败）
    let _ = exec_status(handle, &format!("rm -f {}", sh_quote(&tmp_remote))).await;
    dl?;
    sink.send(WorkerEvent::TransferProgress { id, done: size });

    // 本地解包到 local 的父目录（归档顶层即目录名，解包后落在 local）
    let dest = std::path::Path::new(local)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let tgz = local_tgz.clone();
    // 本地解包也可能耗时（大量小文件），提示「解包中…」避免进度条满了却迟迟不完成
    sink.send(WorkerEvent::TransferNote { id, note: crate::i18n::tr("解包中…", "Extracting…").into() });
    tokio::task::spawn_blocking(move || extract_tar_gz(&tgz, &dest)).await??;
    sink.send(WorkerEvent::TransferNote { id, note: String::new() });
    let _ = std::fs::remove_file(&local_tgz);
    Ok(())
}

/// 把一组路径用 POSIX 单引号转义后空格拼接（末尾带一个空格），用于安全嵌入 shell 命令。
fn join_quoted(items: &[String]) -> String {
    let mut s = String::new();
    for p in items {
        s.push_str(&sh_quote(p));
        s.push(' ');
    }
    s
}

/// 直传临时私钥目录的清理守卫：无论正常返回、`?` 早退，还是被取消（future 被 drop），
/// Drop 时都异步清除源主机上的临时私钥目录——避免目标主机私钥残留在源主机（凭据泄露）。
/// 取消路径下本函数栈已被展开，无法 `.await`，故 detach 一个清理任务到当前运行时。
struct TmpKeyGuard {
    handle: Arc<Handle<ClientHandler>>,
    dir: String,
}

impl Drop for TmpKeyGuard {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        let d = sh_quote(&self.dir);
        tokio::spawn(async move {
            let _ = exec_status(&handle, &format!("shred -uf {d}/key 2>/dev/null; rm -rf {d}")).await;
        });
    }
}

/// 跨主机「直传」：在源主机上用 rsync（无则 scp）把 srcs 直接推到目标主机，数据不经本地。
/// 目标认证仅支持「无口令密钥」：把 B 私钥临时投放到源主机 0700 私有目录，传完/取消即清（见 TmpKeyGuard）。
/// 任一步失败都回报失败，由上层弹「转中转」提醒。
async fn direct_transfer(
    handle: Arc<Handle<ClientHandler>>,
    sftp: Arc<russh_sftp::client::SftpSession>,
    spec: crate::proto::DirectSpec,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let id = spec.id;
    // 先用 du 估算源端总字节，让进度条与文件大小都按真实字节显示
    let total = direct_total_bytes(&handle, &spec.srcs).await;
    sink.send(WorkerEvent::TransferStart { id, name: spec.label.clone(), total, dir: crate::proto::TransferDir::Upload, local: None });
    let res = direct_transfer_inner(&handle, &sftp, &spec, sink, &cancel).await;
    match res {
        Ok(()) => {
            // 收尾把进度拉满（du 估算与实传可能有微小出入）
            if total > 0 {
                sink.send(WorkerEvent::TransferProgress { id, done: total });
            }
            sink.send(WorkerEvent::TransferDone {
                id, ok: true,
                message: crate::i18n::tr("直传完成", "Direct transfer done").into(),
                refresh_dir: None,
            });
        }
        Err(e) => sink.send(WorkerEvent::TransferDone {
            id, ok: false, message: format!("{e}"), refresh_dir: None,
        }),
    }
}

/// 用 du 估算一组源路径的总字节（grand total 行）；失败返回 0（进度条退化为不确定）。
async fn direct_total_bytes(handle: &Handle<ClientHandler>, srcs: &[String]) -> u64 {
    let q = join_quoted(srcs);
    // -s 汇总、-b 字节、-c 末尾输出 total 行；取 total 行的首列
    let out = exec_capture(handle, &format!("du -sbc -- {q}2>/dev/null | tail -1")).await.unwrap_or_default();
    out.split_whitespace().next().and_then(|t| t.parse::<u64>().ok()).unwrap_or(0)
}

async fn direct_transfer_inner(
    handle: &Arc<Handle<ClientHandler>>,
    sftp: &Arc<russh_sftp::client::SftpSession>,
    spec: &crate::proto::DirectSpec,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // 1) 读取本地 B 私钥（本进程可直接访问本地文件）
    let key_bytes = std::fs::read(&spec.key_path).map_err(|e| anyhow::anyhow!("{}", match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("读取目标私钥失败：{e}"),
        crate::i18n::Lang::En => format!("Read target key failed: {e}"),
    }))?;
    // 2) 先建 0700 私有目录（mkdir -m 700 在创建时即定权，杜绝「写入→改权」之间的可读窗口），
    //    密钥放其中。TmpKeyGuard 保证正常/失败/取消(future 被 drop) 各路径都清理该目录。
    let tmp_dir = format!("/tmp/.ishell_kd_{}_{}", spec.id, rand_hex(8));
    if !matches!(exec_status(handle, &format!("mkdir -m 700 {}", sh_quote(&tmp_dir))).await, Ok((0, _))) {
        anyhow::bail!("{}", match crate::i18n::current() {
            crate::i18n::Lang::Zh => "创建临时密钥目录失败".to_string(),
            crate::i18n::Lang::En => "Create temp key dir failed".to_string(),
        });
    }
    let _key_guard = TmpKeyGuard { handle: handle.clone(), dir: tmp_dir.clone() };
    let tmp_key = format!("{tmp_dir}/key");
    sftp_overwrite(sftp, &tmp_key, &key_bytes).await.map_err(|e| anyhow::anyhow!("{}", match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("投放临时密钥失败：{e}"),
        crate::i18n::Lang::En => format!("Place temp key failed: {e}"),
    }))?;
    let _ = exec_status(handle, &format!("chmod 600 {}", sh_quote(&tmp_key))).await; // 纵深防御（目录已 700）

    // 3) 拼接源路径与目标
    let srcs = join_quoted(&spec.srcs);
    // 目标强制以 "/" 结尾，令 rsync/scp 把源放「入目录」而非改名
    let dest = sh_quote(&format!("{}@{}:{}/", spec.dest_user, spec.dest_host, spec.dest_dir));
    // 主机密钥策略：本机已信任目标 → yes（拒绝未知/变更）；用户确认过的首次 → accept-new。
    // 绝不使用 StrictHostKeyChecking=no。
    let hk = if spec.dest_host_known { "yes" } else { "accept-new" };
    let ssh_opt = format!(
        "ssh -p {} -i {} -o StrictHostKeyChecking={} -o BatchMode=yes -o ConnectTimeout=15",
        spec.dest_port, sh_quote(&tmp_key), hk
    );

    // 4) 优先 rsync（可解析进度、可续传）；缺失则回退 scp（仅 spinner）
    let has_rsync = matches!(exec_status(handle, "command -v rsync >/dev/null 2>&1").await, Ok((0, _)));
    let cmd = if has_rsync {
        format!("rsync -a --info=progress2 -e {} -- {}{}", sh_quote(&ssh_opt), srcs, dest)
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
        anyhow::bail!("{}", match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("直传失败（码 {code}）：{}", if reason.is_empty() { "源主机无法连到目标主机" } else { reason }),
            crate::i18n::Lang::En => format!("Direct transfer failed (code {code}): {}", if reason.is_empty() { "source host cannot reach target" } else { reason }),
        });
    }
    Ok(())
}

/// 执行直传命令并解析 rsync `--info=progress2` 的「已传字节」（首列），按字节上报进度。
/// 返回 (退出码, stderr)。被取消时（cancel 置位）提前结束读取，外层 select 也会 drop 整个 future。
async fn exec_direct_progress(
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
fn last_rsync_bytes(s: &str) -> Option<u64> {
    let line = s.rsplit(['\r', '\n']).find(|seg| !seg.trim().is_empty())?;
    let tok = line.split_whitespace().next()?;
    let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// 给本地路径找一个不冲突的变体：`file.ext` → `file (1).ext`；目录 `dir` → `dir (1)`。
fn local_nonexistent(path: &str) -> String {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return path.to_string();
    }
    let is_dir = p.is_dir();
    let parent = p.parent();
    let fname = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = split_name(&fname, is_dir);
    for n in 1..10000u32 {
        let cand_name = match &ext { Some(e) => format!("{stem} ({n}).{e}"), None => format!("{stem} ({n})") };
        let cand = match parent { Some(d) => d.join(&cand_name), None => std::path::PathBuf::from(&cand_name) };
        if !cand.exists() {
            return cand.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// 给远端目录里的名字找一个不冲突的变体。
async fn remote_nonexistent(sftp: &russh_sftp::client::SftpSession, dir: &str, name: &str, is_dir: bool) -> String {
    let (stem, ext) = split_name(name, is_dir);
    for n in 1..10000u32 {
        let cand = match &ext { Some(e) => format!("{stem} ({n}).{e}"), None => format!("{stem} ({n})") };
        if sftp.metadata(&join_remote(dir, &cand)).await.is_err() {
            return cand;
        }
    }
    name.to_string()
}

/// 拆分文件名为 (主名, 扩展)；目录或无扩展时扩展为 None（首字符的点不算扩展）。
fn split_name(fname: &str, is_dir: bool) -> (String, Option<String>) {
    if is_dir {
        return (fname.to_string(), None);
    }
    match fname.rfind('.') {
        Some(d) if d > 0 => (fname[..d].to_string(), Some(fname[d + 1..].to_string())),
        _ => (fname.to_string(), None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn download(
    handle: Arc<Handle<ClientHandler>>,
    sftp: Arc<russh_sftp::client::SftpSession>,
    id: u64,
    remote: String,
    local: String,
    policy: ConflictPolicy,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = basename(&remote);
    let is_dir = sftp.metadata(&remote).await.map(|m| m.is_dir()).unwrap_or(false);

    // 冲突处理：本地目标已存在时，按策略 跳过 / 重命名 / 覆盖
    let local = if std::path::Path::new(&local).exists() {
        match policy {
            ConflictPolicy::Skip => {
                sink.send(WorkerEvent::TransferDone { id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已跳过（本地已存在）：{name}"), crate::i18n::Lang::En => format!("Skipped (exists): {name}") }, refresh_dir: None });
                return;
            }
            ConflictPolicy::Rename => local_nonexistent(&local),
            ConflictPolicy::Overwrite => local,
        }
    } else {
        local
    };

    // 目录优先走压缩下载（远端 tar.gz 打包 → 单文件并发下载 → 本地解包），
    // 大幅减少多小文件的逐个 SFTP 往返；任何失败则回退到逐文件下载。
    if is_dir {
        match download_dir_compressed(&handle, &sftp, id, &remote, &local, sink, &cancel).await {
            Ok(()) => {
                sink.send(WorkerEvent::TransferDone {
                    id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已下载 {name}"), crate::i18n::Lang::En => format!("Downloaded {name}") }, refresh_dir: None,
                });
                return;
            }
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    sink.send(WorkerEvent::TransferDone { id, ok: false, message: crate::i18n::tr("已取消", "Canceled").into(), refresh_dir: None });
                    return;
                }
                log::warn!("压缩下载失败，回退逐文件：{e}");
            }
        }
    }

    let res: anyhow::Result<()> = async {
        // 收集待下载文件：(远程绝对路径, 本地路径, 大小)
        let mut files: Vec<(String, std::path::PathBuf, u64)> = Vec::new();
        if is_dir {
            // 迭代遍历整棵目录树（避免 async 递归）
            let mut stack = vec![remote.clone()];
            while let Some(dir) = stack.pop() {
                let rd = sftp.read_dir(&dir).await?;
                for item in rd {
                    let n = item.file_name();
                    if n == "." || n == ".." {
                        continue;
                    }
                    let full = join_remote(&dir, &n);
                    let meta = item.metadata();
                    if meta.is_dir() {
                        stack.push(full);
                    } else {
                        let rel = full.strip_prefix(remote.as_str()).unwrap_or(&full).trim_start_matches('/');
                        files.push((full.clone(), std::path::Path::new(&local).join(rel), meta.size.unwrap_or(0)));
                    }
                }
            }
        } else {
            let sz = sftp.metadata(&remote).await.ok().and_then(|m| m.size).unwrap_or(0);
            files.push((remote.clone(), std::path::PathBuf::from(&local), sz));
        }

        let total: u64 = files.iter().map(|f| f.2).sum();
        sink.send(WorkerEvent::TransferStart {
            id, name: name.clone(), total, dir: crate::proto::TransferDir::Download, local: Some(local.clone()),
        });

        // 累计已下载字节（多任务共享）+ 周期性上报进度
        let done = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let prog = {
            let (d, s, st) = (done.clone(), sink.clone(), stop.clone());
            tokio::spawn(async move {
                let mut last = 0u64;
                loop {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let v = d.load(Ordering::Relaxed);
                    if v != last {
                        last = v;
                        s.send(WorkerEvent::TransferProgress { id, done: v });
                    }
                    if st.load(Ordering::Relaxed) {
                        break;
                    }
                }
            })
        };

        let result = async {
            for (rpath, lpath, size) in files {
                download_file(&sftp, &rpath, &lpath, size, sftp.metadata(&rpath).await.ok().and_then(|m| m.mtime).unwrap_or(0), &cancel, &done).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        stop.store(true, Ordering::Relaxed);
        let _ = prog.await;
        sink.send(WorkerEvent::TransferProgress { id, done: done.load(Ordering::Relaxed) });
        result
    }
    .await;

    match res {
        Ok(_) => sink.send(WorkerEvent::TransferDone {
            id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已下载 {name}"), crate::i18n::Lang::En => format!("Downloaded {name}") }, refresh_dir: None,
        }),
        Err(e) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() { crate::i18n::Lang::Zh => format!("下载失败：{e}"), crate::i18n::Lang::En => format!("Download failed: {e}") }
            };
            sink.send(WorkerEvent::TransferDone { id, ok: false, message, refresh_dir: None });
        }
    }
}

/// 同一文件内的并发分段数（流水线深度）。8 路足以在常见高延迟链路上跑满带宽。
const DL_PARALLEL: u64 = 8;
/// 每个分段一次抢占的字节数。
const DL_CHUNK: u64 = 1024 * 1024;
/// 单个文件传输遇到瞬时错误时的最大额外重试次数（配合断点续传）。
const XFER_RETRIES: u32 = 3;

/// 第 attempt 次重试前的退避时长（300ms·2^n，封顶约 4.8s）。
fn xfer_backoff(attempt: u32) -> Duration {
    Duration::from_millis(300u64 * (1u64 << attempt.min(4)))
}

/// 断点信息 sidecar 路径：`<local>.ishellpart`。
fn part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart");
    std::path::PathBuf::from(p)
}

/// 下载数据的临时文件路径：`<local>.ishellpart.data`。
/// 数据先写这里，全部完成后 rename 到目标——成功前绝不动目标文件；
/// 取消/失败只留 part 文件，目标（若原本存在）保持完好。
fn data_part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart.data");
    std::path::PathBuf::from(p)
}

/// 容纳 n 个分段标志位所需的字节数。
fn bitmap_len(n_chunks: u64) -> usize {
    n_chunks.div_ceil(8) as usize
}

/// 下载单个文件：大文件按偏移并发分段读取，定位写入本地，显著提升高延迟链路吞吐。
/// 数据全程写 `<local>.ishellpart.data`，完整后原子 rename 到目标——成功前不动目标文件。
/// `remote_mtime` 参与断点校验（0 = 不允许跨次续传，如临时打包文件）。
async fn download_file(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    rpath: &str,
    lpath: &std::path::Path,
    size: u64,
    remote_mtime: u32,
    cancel: &Arc<AtomicBool>,
    done: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    if let Some(parent) = lpath.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let data_part = data_part_path(lpath);

    // 小文件（或大小未知）：单流顺序读取；瞬时失败整体重试（重新建临时文件）。
    if size <= DL_CHUNK {
        let mut attempt = 0u32;
        loop {
            match download_small(sftp, rpath, &data_part, cancel, done).await {
                Ok(()) => {
                    finish_download(&data_part, lpath)?;
                    return Ok(());
                }
                Err(e) => {
                    if cancel.load(Ordering::Relaxed) || attempt >= XFER_RETRIES {
                        let _ = std::fs::remove_file(&data_part);
                        return Err(e);
                    }
                    attempt += 1;
                    tokio::time::sleep(xfer_backoff(attempt)).await;
                }
            }
        }
    }

    // 大文件：预分配，按偏移并发分段；用「分段完成位图」实现断点续传——
    // 位图持久化到 sidecar（<local>.ishellpart），重连/重发后只补未完成分段。
    let n_chunks = size.div_ceil(DL_CHUNK);
    let part = part_path(lpath);

    // 能否续传：sidecar 存在、记录的大小与远端 mtime 均一致、临时数据文件仍在。
    // 绑定 mtime：远端文件内容变化但大小不变时，旧分段不能复用（否则拼出混合损坏文件）。
    let resume_bm: Option<Vec<u8>> = if data_part.exists() && remote_mtime != 0 {
        std::fs::read(&part).ok().and_then(|d| {
            let ok = d.len() == 12 + bitmap_len(n_chunks)
                && u64::from_le_bytes(d[0..8].try_into().unwrap()) == size
                && u32::from_le_bytes(d[8..12].try_into().unwrap()) == remote_mtime;
            ok.then(|| d[12..].to_vec())
        })
    } else {
        None
    };

    let out = if resume_bm.is_some() {
        Arc::new(std::fs::OpenOptions::new().read(true).write(true).open(&data_part)?) // 续传：保留已写分段
    } else {
        let f = std::fs::File::create(&data_part)?;
        f.set_len(size)?;
        Arc::new(f)
    };
    let chunk_done: Arc<Vec<AtomicBool>> = Arc::new(
        (0..n_chunks)
            .map(|i| {
                let d = resume_bm.as_ref().is_some_and(|b| (b[(i / 8) as usize] >> (i % 8)) & 1 == 1);
                AtomicBool::new(d)
            })
            .collect(),
    );
    // 已完成分段计入进度（续传时进度条从断点开始）
    let pre: u64 = (0..n_chunks)
        .filter(|&i| chunk_done[i as usize].load(Ordering::Relaxed))
        .map(|i| std::cmp::min(DL_CHUNK, size - i * DL_CHUNK))
        .sum();
    if pre > 0 {
        done.fetch_add(pre, Ordering::Relaxed);
    }
    // sidecar 句柄（写头部 size+mtime + 预留位图区，保留续传位）
    let part_file = {
        let f = std::fs::File::create(&part)?;
        f.set_len(12 + bitmap_len(n_chunks) as u64)?;
        pwrite(&f, &size.to_le_bytes(), 0)?;
        pwrite(&f, &remote_mtime.to_le_bytes(), 8)?;
        if let Some(b) = &resume_bm {
            pwrite(&f, b, 12)?;
        }
        Arc::new(std::sync::Mutex::new(f))
    };

    let mut attempt = 0u32;
    loop {
        let cursor = Arc::new(AtomicU64::new(0)); // 本轮分段游标
        let workers = DL_PARALLEL.min(n_chunks.max(1));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..workers {
            let (sftp, out, cursor, done, cancel, chunk_done) =
                (sftp.clone(), out.clone(), cursor.clone(), done.clone(), cancel.clone(), chunk_done.clone());
            let part_file = part_file.clone();
            let rpath = rpath.to_string();
            set.spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut rf = sftp.open(&rpath).await?;
                let mut buf = vec![0u8; DL_CHUNK as usize];
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        anyhow::bail!("canceled");
                    }
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    if idx >= n_chunks {
                        break;
                    }
                    if chunk_done[idx as usize].load(Ordering::Relaxed) {
                        continue; // 上一轮已完成
                    }
                    let off = idx * DL_CHUNK;
                    let want = std::cmp::min(DL_CHUNK, size - off) as usize;
                    rf.seek(std::io::SeekFrom::Start(off)).await?;
                    let mut got = 0usize;
                    while got < want {
                        let n = rf.read(&mut buf[got..want]).await?;
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                    if got != want {
                        anyhow::bail!("short read");
                    }
                    pwrite(&out, &buf[..want], off)?;
                    chunk_done[idx as usize].store(true, Ordering::Relaxed);
                    done.fetch_add(want as u64, Ordering::Relaxed); // 每段只计一次
                    // 持久化该分段所在的位图字节（断点信息落盘）
                    let byte_i = (idx / 8) as usize;
                    let mut b = 0u8;
                    for bit in 0..8u64 {
                        let ci = byte_i as u64 * 8 + bit;
                        if ci < n_chunks && chunk_done[ci as usize].load(Ordering::Relaxed) {
                            b |= 1 << bit;
                        }
                    }
                    if let Ok(g) = part_file.lock() {
                        let _ = pwrite(&g, &[b], 12 + byte_i as u64);
                    }
                }
                Ok::<(), anyhow::Error>(())
            });
        }
        let mut first_err: Option<anyhow::Error> = None;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => {
                    first_err.get_or_insert(e.into());
                }
            }
        }
        if chunk_done.iter().all(|b| b.load(Ordering::Relaxed)) {
            let _ = std::fs::remove_file(&part); // 完成则清理断点文件
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&part); // 用户取消则不保留断点
            let _ = std::fs::remove_file(&data_part); // 数据临时文件一并清理，目标文件从未被动过
            anyhow::bail!("canceled");
        }
        if attempt >= XFER_RETRIES {
            return Err(first_err.unwrap_or_else(|| anyhow::anyhow!("incomplete transfer")));
        }
        attempt += 1;
        tokio::time::sleep(xfer_backoff(attempt)).await;
    }
    drop(out); // 关闭数据句柄后再 rename（Windows 需要）
    finish_download(&data_part, lpath)?;
    Ok(())
}

/// 下载完成收尾：临时数据文件原子替换到目标（先删已存在目标，Windows rename 不覆盖）。
fn finish_download(data_part: &std::path::Path, lpath: &std::path::Path) -> anyhow::Result<()> {
    if !lpath.exists() {
        // 目标不存在：直接换入
        std::fs::rename(data_part, lpath)?;
        return Ok(());
    }
    // 覆盖已有：备份 → 换入 → 删备份；换入失败则还原备份，原文件绝不丢失。
    let bak = lpath.with_extension(format!("ishell-bak-{}", rand_hex(6)));
    std::fs::rename(lpath, &bak)?; // 原文件安全存于 bak
    match std::fs::rename(data_part, lpath) {
        Ok(_) => {
            let _ = std::fs::remove_file(&bak);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&bak, lpath); // 换入失败：还原原文件
            let _ = std::fs::remove_file(data_part); // 清理未换入的临时数据文件，避免残留
            Err(e.into())
        }
    }
}

/// 小文件顺序下载；失败时回退本次已计入的进度字节，便于上层整体重试不重复计数。
async fn download_small(
    sftp: &Arc<russh_sftp::client::SftpSession>,
    rpath: &str,
    lpath: &std::path::Path,
    cancel: &Arc<AtomicBool>,
    done: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut added = 0u64;
    let res: anyhow::Result<()> = async {
        let mut rf = sftp.open(rpath).await?;
        let mut lf = tokio::fs::File::create(lpath).await?;
        let mut buf = vec![0u8; 128 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!("canceled");
            }
            let n = rf.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            lf.write_all(&buf[..n]).await?;
            done.fetch_add(n as u64, Ordering::Relaxed);
            added += n as u64;
        }
        lf.flush().await?;
        Ok(())
    }
    .await;
    if res.is_err() {
        done.fetch_sub(added, Ordering::Relaxed); // 回退，避免重试重复累加
    }
    res
}

/// 在指定偏移定位写入（跨平台）。
fn pwrite(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut off = offset;
        let mut b = buf;
        while !b.is_empty() {
            let n = file.seek_write(b, off)?;
            b = &b[n..];
            off += n as u64;
        }
        Ok(())
    }
}

/// 分块上传并上报进度。
async fn upload(
    sftp: &russh_sftp::client::SftpSession,
    id: u64,
    local: String,
    remote_dir: String,
    policy: ConflictPolicy,
    sink: &UiSink,
    cancel: Arc<AtomicBool>,
) {
    let name = local_basename(&local); // 本地路径用 Windows 兼容的取名（处理反斜杠/盘符）
    let is_dir = tokio::fs::metadata(&local).await.map(|m| m.is_dir()).unwrap_or(false);

    // 冲突处理：远端目标已存在时，按策略 跳过 / 重命名 / 覆盖
    let name = if sftp.metadata(&join_remote(&remote_dir, &name)).await.is_ok() {
        match policy {
            ConflictPolicy::Skip => {
                sink.send(WorkerEvent::TransferDone { id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已跳过（远端已存在）：{name}"), crate::i18n::Lang::En => format!("Skipped (exists): {name}") }, refresh_dir: None });
                return;
            }
            ConflictPolicy::Rename => remote_nonexistent(sftp, &remote_dir, &name, is_dir).await,
            ConflictPolicy::Overwrite => name,
        }
    } else {
        name
    };

    let res: anyhow::Result<()> = async {
        // 收集待上传文件：(本地路径, 远程路径, 大小)；目录则递归并记录要创建的远端目录
        let mut files: Vec<(std::path::PathBuf, String, u64)> = Vec::new();
        let mut mkdirs: Vec<String> = Vec::new();
        if is_dir {
            let local_root = std::path::PathBuf::from(&local);
            let root_remote = join_remote(&remote_dir, &name);
            mkdirs.push(root_remote.clone());
            let mut stack = vec![local_root.clone()];
            while let Some(dir) = stack.pop() {
                let mut rd = tokio::fs::read_dir(&dir).await?;
                while let Some(entry) = rd.next_entry().await? {
                    let p = entry.path();
                    let rel = p.strip_prefix(&local_root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                    let rpath = format!("{root_remote}/{rel}");
                    let ft = entry.file_type().await?;
                    if ft.is_dir() {
                        mkdirs.push(rpath);
                        stack.push(p);
                    } else if ft.is_file() {
                        let sz = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
                        files.push((p, rpath, sz));
                    }
                }
            }
        } else {
            let sz = tokio::fs::metadata(&local).await.map(|m| m.len()).unwrap_or(0);
            files.push((std::path::PathBuf::from(&local), join_remote(&remote_dir, &name), sz));
        }

        let total: u64 = files.iter().map(|f| f.2).sum();
        sink.send(WorkerEvent::TransferStart {
            id, name: name.clone(), total, dir: crate::proto::TransferDir::Upload, local: None,
        });

        // 先按深度建好远端目录（父先于子），已存在则忽略
        mkdirs.sort_by_key(|d| d.matches('/').count());
        for d in &mkdirs {
            let _ = sftp.create_dir(d.clone()).await;
        }

        // 逐文件上传：每个文件可断点续传 + 瞬时失败自动重试。
        let mut done_base = 0u64; // 已完成文件累计字节
        let last = AtomicU64::new(0); // 上次上报点（跨文件单调）
        for (lpath, rpath, sz) in files {
            let mut attempt = 0u32;
            loop {
                match upload_file_once(sftp, &lpath, &rpath, &cancel, done_base, id, sink, &last, attempt > 0).await {
                    Ok(()) => break,
                    Err(e) => {
                        if cancel.load(Ordering::Relaxed) || attempt >= XFER_RETRIES {
                            return Err(e);
                        }
                        attempt += 1;
                        tokio::time::sleep(xfer_backoff(attempt)).await;
                    }
                }
            }
            done_base += sz;
            sink.send(WorkerEvent::TransferProgress { id, done: done_base });
        }
        Ok(())
    }
    .await;
    match res {
        Ok(_) => sink.send(WorkerEvent::TransferDone {
            id, ok: true, message: match crate::i18n::current() { crate::i18n::Lang::Zh => format!("已上传 {name}"), crate::i18n::Lang::En => format!("Uploaded {name}") }, refresh_dir: Some(remote_dir),
        }),
        Err(e) => {
            let message = if cancel.load(Ordering::Relaxed) {
                crate::i18n::tr("已取消", "Canceled").to_string()
            } else {
                match crate::i18n::current() { crate::i18n::Lang::Zh => format!("上传失败：{e}"), crate::i18n::Lang::En => format!("Upload failed: {e}") }
            };
            sink.send(WorkerEvent::TransferDone { id, ok: false, message, refresh_dir: None });
        }
    }
}

/// 上传单个文件：以远端已有大小为起点续传；带进度节流上报。
async fn upload_file_once(
    sftp: &russh_sftp::client::SftpSession,
    lpath: &std::path::Path,
    rpath: &str,
    cancel: &Arc<AtomicBool>,
    done_base: u64,
    id: u64,
    sink: &UiSink,
    last: &AtomicU64,
    allow_resume: bool,
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    let local_size = tokio::fs::metadata(lpath).await.map(|m| m.len()).unwrap_or(0);
    // 续传只允许发生在**本次传输的失败重试**（allow_resume）：此时远端内容必然是
    // 本进程刚写入的本地前缀，按大小续写安全。首次尝试一律 TRUNCATE 从 0 全量写——
    // 盲按「远端大小 ≤ 本地大小」续传会把无关同名文件误判为已传前缀
    //（大小恰好相等时一个字节不写就报成功；远端较小时保留错误前缀再续尾部）。
    let start = if allow_resume {
        let remote_size = sftp.metadata(rpath).await.ok().and_then(|m| m.size).unwrap_or(0);
        if remote_size > 0 && remote_size <= local_size { remote_size } else { 0 }
    } else {
        0
    };

    // 续传(start>0)保留已传字节；从头(start==0)则 TRUNCATE 覆盖，避免残留旧尾部
    let flags = if start > 0 {
        OpenFlags::CREATE | OpenFlags::WRITE
    } else {
        OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE
    };
    let mut rf = sftp.open_with_flags(rpath, flags).await?;
    rf.seek(std::io::SeekFrom::Start(start)).await?;
    let mut lf = tokio::fs::File::open(lpath).await?;
    if start > 0 {
        lf.seek(std::io::SeekFrom::Start(start)).await?;
    }

    let mut buf = vec![0u8; 128 * 1024];
    let mut pos = start;
    loop {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!("canceled");
        }
        let n = lf.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        rf.write_all(&buf[..n]).await?;
        pos += n as u64;
        let done = done_base + pos;
        if done.saturating_sub(last.load(Ordering::Relaxed)) >= 256 * 1024 {
            last.store(done, Ordering::Relaxed);
            sink.send(WorkerEvent::TransferProgress { id, done });
        }
    }
    rf.flush().await?;
    rf.shutdown().await?;
    Ok(())
}

fn basename(path: &str) -> String {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_string()
}

/// 取「本地」路径的文件名：同时按 `/` 和 `\` 切分，正确处理 Windows 路径
/// （否则 `C:\Users\x\a.txt` 会被当成整体文件名上传，远端文件名也带上盘符路径）。
fn local_basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}
#[cfg(test)]
mod tests {
    use super::tar_entry_path_safe;
    use std::path::Path;

    #[test]
    fn tar_paths_reject_traversal() {
        assert!(tar_entry_path_safe(Path::new("ok/file.txt")));
        assert!(tar_entry_path_safe(Path::new("./nested/a")));
        assert!(!tar_entry_path_safe(Path::new("../escape")));
        assert!(!tar_entry_path_safe(Path::new("a/../../b")));
        assert!(!tar_entry_path_safe(Path::new("/abs/path")));
        assert!(!tar_entry_path_safe(Path::new("")));
    }
}
