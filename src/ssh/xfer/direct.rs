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
    let (code, err) = exec_direct_progress(handle, &cmd, spec.id, sink, cancel, None).await?;
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
/// `on_exec_started`：`channel_open_session`/`exec` 都成功之后（意味着已经真正连上目标
/// 主机并提交了命令）才调用一次——`copy_between_sessions` 用它来在这个时刻才发
/// `DirectRelayStarted`，解除 App 层"20s 内未建立连接就转中转"的短超时；`direct_transfer`
/// 这条既有调用路径不需要这个信号，传 `None`。
pub(super) async fn exec_direct_progress(
    handle: &Handle<ClientHandler>,
    cmd: &str,
    id: u64,
    sink: &UiSink,
    cancel: &Arc<AtomicBool>,
    on_exec_started: Option<fn(&UiSink, u64)>,
) -> anyhow::Result<(i32, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd).await?;
    if let Some(cb) = on_exec_started {
        cb(sink, id);
    }
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

/// 跨会话拷贝-直连优先模式（目标会话侧）：临时把一次性公钥追加进这台主机的
/// authorized_keys，建立"源主机可以免密连过来"的单向信任。带 `restrict`（OpenSSH 7.2+
/// 组合开关：禁端口/agent/X11 转发 + 禁 PTY/login shell）收紧这把临时密钥的能力——
/// 即便撤销失败遗留，能做的事也严格限定在文件传输，不会变成一把可以任意登录的钥匙。
pub(crate) async fn trust_temp_key(handle: &Handle<ClientHandler>, pub_key_line: &str) -> anyhow::Result<()> {
    let home = exec_capture(handle, "echo -n $HOME").await?;
    let home = home.trim();
    if home.is_empty() {
        anyhow::bail!("探测不到远端 HOME");
    }
    let ssh_dir = format!("{home}/.ssh");
    let auth_keys = format!("{ssh_dir}/authorized_keys");
    let lock = format!("{ssh_dir}/.ishell-relay.lock");
    // 先建 ~/.ssh（flock 的锁文件要落在这里），再在文件锁保护下追加公钥——与并发的
    // untrust_temp_key 互斥。多个直连拷贝并发写同一台目标主机时，untrust 的 grep→mv
    // 读改写若和另一次 trust 的追加/另一次 untrust 交错，会丢标记（残留）或互相覆盖；
    // 用同一把 flock 串行化两者即可根治。flock 缺失（极少见）时 `|| true` 退化为无锁执行
    //——单次操作本身仍正确（追加是原子的），只是失去跨并发操作的串行保证。
    let cmd = format!(
        "mkdir -p -m 700 {ssh_dir} && ( flock 9 2>/dev/null || true; \
         touch {ak} && chmod 600 {ak} && printf '%s\\n' {line} >> {ak} ) 9>{lock}",
        ssh_dir = sh_quote(&ssh_dir),
        ak = sh_quote(&auth_keys),
        line = sh_quote(pub_key_line),
        lock = sh_quote(&lock),
    );
    let (code, err) = exec_status(handle, &cmd).await?;
    if code != 0 {
        anyhow::bail!("写入 authorized_keys 失败（码 {code}）：{}", err.trim());
    }
    Ok(())
}

/// 撤销 `trust_temp_key` 追加的那一行：按 marker 注释精确匹配删除（不按整行文本匹配，
/// 避免公钥内容里出现 shell 特殊字符时 `grep -v` 出问题）。尽力而为——失败只在调用方
/// 记日志，不向上抛，因为清理失败不应该掩盖"传输本身成功/失败"这个对用户更重要的结果。
pub(crate) async fn untrust_temp_key(handle: &Handle<ClientHandler>, marker: &str) -> anyhow::Result<()> {
    let home = exec_capture(handle, "echo -n $HOME").await?;
    let home = home.trim();
    if home.is_empty() {
        anyhow::bail!("探测不到远端 HOME");
    }
    let ssh_dir = format!("{home}/.ssh");
    let auth_keys = format!("{ssh_dir}/authorized_keys");
    let lock = format!("{ssh_dir}/.ishell-relay.lock");
    // 临时文件名带随机后缀：即使 flock 缺失退化为无锁，并发 untrust 也不会互相踩同一个
    // 临时文件（叠加下面的 flock 是双保险）。
    let tmp_path = format!("{auth_keys}.ishell_tmp.{}", super::rand_hex(6));
    // 与 trust_temp_key 用同一把 flock 串行化，避免并发直连拷贝对同一台目标主机的
    // authorized_keys 做读改写时互相覆盖/丢标记（见 trust_temp_key 注释）。
    // 注意：不能写成 `grep ... && mv ... || true`——`grep -v` 在“每一行都匹配 marker”
    // （比如这个临时公钥恰好是 authorized_keys 里唯一一行，常见于纯密码认证账户第一次
    // 建立信任的场景）时不会有任何输出，退出码是 1（非 0），`&&` 直接短路、`mv` 被跳过，
    // 公钥这一行就永远删不掉——即便走的是正常撤销路径也会遗留。grep 退出码 0/1 都表示
    // 命令本身成功执行（1 只是“没有匹配到应保留的行”，输出为空文件也是合法结果），
    // 只有 2+ 才是真正的执行错误（比如文件不可读），此时不应该用这个空/错误的临时文件
    // 覆盖原文件。
    let cmd = format!(
        "( flock 9 2>/dev/null || true; \
         grep -vF {marker} {ak} > {tmp} 2>/dev/null; code=$?; \
         if [ \"$code\" -le 1 ]; then mv {tmp} {ak}; else rm -f {tmp}; fi ) 9>{lock}",
        marker = sh_quote(marker),
        ak = sh_quote(&auth_keys),
        tmp = sh_quote(&tmp_path),
        lock = sh_quote(&lock),
    );
    let _ = exec_status(handle, &cmd).await;
    Ok(())
}

/// 跨会话拷贝-直连优先模式（源会话侧）：把一次性私钥投放到本主机、直接 scp/rsync 到目标
/// 主机，字节完全不经过运行 iShell 的机器。主机密钥策略固定 `accept-new`——这是程序化的
/// 一次性操作，没有人工确认目标指纹的界面，工具描述里会明确告知调用方这一点。
pub(crate) async fn direct_relay_copy(
    handle: Arc<Handle<ClientHandler>>,
    sftp: Arc<russh_sftp::client::SftpSession>,
    op_id: u64,
    src_path: String,
    dest_user: String,
    dest_host: String,
    dest_port: u16,
    dest_path: String,
    priv_key_pem: Vec<u8>,
    cancel: Arc<AtomicBool>,
    sink: &UiSink,
) {
    let tmp_dir = format!("/tmp/.ishell_relay_{op_id}_{}", rand_hex(8));
    if !matches!(
        exec_status(&handle, &format!("mkdir -m 700 {}", sh_quote(&tmp_dir))).await,
        Ok((0, _))
    ) {
        sink.send(WorkerEvent::DirectRelayDone {
            op_id,
            ok: false,
            message: "创建临时密钥目录失败".into(),
        });
        return;
    }
    let _key_guard = TmpKeyGuard { handle: handle.clone(), dir: tmp_dir.clone() };
    let tmp_key = format!("{tmp_dir}/key");
    if let Err(e) = sftp_overwrite(&sftp, &tmp_key, &priv_key_pem).await {
        sink.send(WorkerEvent::DirectRelayDone {
            op_id,
            ok: false,
            message: format!("投放临时密钥失败：{e}"),
        });
        return;
    }
    let _ = exec_status(&handle, &format!("chmod 600 {}", sh_quote(&tmp_key))).await; // 纵深防御（目录已 700）

    // 直连**绝不**直接写 dest_path：先传到目标主机上的一个临时名，传完才 mv 就位。
    //
    // 因为 cancel 拦不住已经发出去的远端命令：App 层的 20s 短超时只是置位 cancel 然后转中转，
    // 而 exec_direct_progress 是先 channel_open_session + exec（命令已经在目标主机上跑起来了）
    // 才在收数据的循环里看 cancel。最坏的排序是——中转已经把完整文件原子换入 dest_path
    // （upload_from_mcp 是事务写），姗姗来迟的 scp 这时才以 O_TRUNC 打开 dest_path、写了半截、
    // 随通道关闭而死：留下一个半截文件，而调用方收到的是「中转成功」。
    //
    // 传临时名 + 成功才 mv 之后，这个排序最坏只留下一个孤儿临时文件，dest_path 分毫未动。
    // 这是靠**构造**消除竞争，而不是靠证明「远端进程已经死了」——后者根本证不出来：跳出
    // exec_direct_progress 的收数据循环只是本地不再读，远端的 scp 进程照样在跑。
    let dest_tmp = format!("{dest_path}.ishell-direct-tmp-{}", rand_hex(6));
    let dest_spec = sh_quote(&format!("{dest_user}@{dest_host}:{dest_tmp}"));
    let host_spec = sh_quote(&format!("{dest_user}@{dest_host}"));
    let src = sh_quote(&src_path);
    let ssh_opt = format!(
        "ssh -p {dest_port} -i {} -o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=15",
        sh_quote(&tmp_key),
    );
    let has_rsync = matches!(
        exec_status(&handle, "command -v rsync >/dev/null 2>&1").await,
        Ok((0, _))
    );
    let xfer = if has_rsync {
        format!("rsync -a --info=progress2 -e {} -- {src} {dest_spec}", sh_quote(&ssh_opt))
    } else {
        format!(
            "scp -P {dest_port} -i {} -o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=15 -- {src} {dest_spec}",
            sh_quote(&tmp_key),
        )
    };
    // 就位与清理都在目标主机上执行，所以远端命令要再包一层引号（sh_quote 可安全嵌套：多一跳
    // shell 就多包一层）。`mv -f` 在同目录内是原子的——临时名就是在 dest_path 后面缀出来的。
    // 任一步失败都顺手清掉临时文件，再以非 0 退出交给 App 层转中转；清理是尽力而为（此刻临时
    // 公钥可能已经被撤销，这条 ssh 会连不上），失败不改变判定结果，孤儿文件的名字也带着
    // `.ishell-direct-tmp-` 前缀，便于人工识别。
    let commit = format!(
        "{ssh_opt} -- {host_spec} {}",
        sh_quote(&format!("mv -f {} {}", sh_quote(&dest_tmp), sh_quote(&dest_path))),
    );
    let discard = format!(
        "{ssh_opt} -- {host_spec} {}",
        sh_quote(&format!("rm -f {}", sh_quote(&dest_tmp))),
    );
    let cmd = format!("{{ {xfer} && {commit}; }} || {{ {discard} >/dev/null 2>&1; exit 1; }}");

    // 临时私钥的清理交由 _key_guard 在作用域结束（含超时取消时的 future drop）异步完成。
    // `DirectRelayStarted` 必须在 exec_direct_progress 内部真正 `channel_open_session`+
    // `exec` 成功之后才发（见该函数的 `on_exec_started` 回调）——如果像这里曾经写的那样
    // 在调用前就无条件发送，会让 App 层"20s 内未建连就转中转"的短超时形同虚设（`started`
    // 瞬间变 true，短超时永远不会命中，网络不通时只能死等到总超时才降级）。
    fn announce_started(sink: &UiSink, op_id: u64) {
        sink.send(WorkerEvent::DirectRelayStarted { op_id });
    }
    // 已经被放弃了就别再把命令发出去：上面几步（mkdir/投私钥/探测 rsync）都是 await，20s 的
    // 短超时完全可能在这期间到点。这一下**不是**正确性所依赖的——cancel 和 exec 之间永远存在
    // 窗口，正确性靠的是上面「传临时名、成功才 mv」的构造——它只是避免平白无故在目标主机上
    // 起一条注定要被丢弃的传输、少留一个孤儿临时文件。
    if cancel.load(Ordering::Relaxed) {
        sink.send(WorkerEvent::DirectRelayDone {
            op_id,
            ok: false,
            message: "直连尝试在发起前已被放弃（短超时到点，转中转）".into(),
        });
        return;
    }
    match exec_direct_progress(&handle, &cmd, op_id, sink, &cancel, Some(announce_started)).await {
        Ok((0, _)) => sink.send(WorkerEvent::DirectRelayDone {
            op_id,
            ok: true,
            message: "直连完成".into(),
        }),
        Ok((code, err)) => {
            let reason = err.trim();
            sink.send(WorkerEvent::DirectRelayDone {
                op_id,
                ok: false,
                message: format!(
                    "直连失败（码 {code}）：{}",
                    if reason.is_empty() { "源主机无法连到目标主机" } else { reason }
                ),
            });
        }
        Err(e) => sink.send(WorkerEvent::DirectRelayDone {
            op_id,
            ok: false,
            message: format!("直连执行失败：{e}"),
        }),
    }
}
