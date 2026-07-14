//! Session 事件排空：从 App God Object 拆出，行为不变。
//! `Session::drain_events` 每帧从 worker 通道取事件并更新会话状态。

use crate::proto::{ConflictPolicy, UiCommand, WorkerEvent};

use super::{KbdPrompt, Session, Transfer, XferSpec};

impl Session {
    /// 排空后台事件，更新本地状态。
    /// 排空 worker 事件，带每帧预算：终端数据 ≤2MB、事件 ≤512 条/帧。
    /// 超出预算的事件留在队列、下一帧继续（返回 true 表示还有积压需要重绘）——
    /// 远端持续大量输出时 UI 仍按帧渲染，不会被「全量排空循环」饿死。
    pub(crate) fn drain_events(&mut self) -> bool {
        // 系统信息走独立 watch 通道（只保留最新一份，不随 UI 排空节奏堆积），每帧单独取一次。
        if self.sysinfo_rx.has_changed().unwrap_or(false) {
            if let Some(info) = self.sysinfo_rx.borrow_and_update().clone() {
                // 历史曲线记录当前选中网卡（空=全部）的速率
                let (rx, tx) = if self.selected_nic.is_empty() {
                    (info.net_rx_bps, info.net_tx_bps)
                } else {
                    info.nets
                        .iter()
                        .find(|n| n.name == self.selected_nic)
                        .map(|n| (n.rx_bps, n.tx_bps))
                        .unwrap_or((info.net_rx_bps, info.net_tx_bps))
                };
                self.net_hist.push(rx, tx);
                self.sysinfo = Some(info);
            }
        }
        let mut term_budget: usize = 2 * 1024 * 1024;
        let mut evt_budget: usize = 512;
        loop {
            if evt_budget == 0 || term_budget == 0 {
                return true; // 预算耗尽且可能仍有积压
            }
            let Ok(ev) = self.evt_rx.try_recv() else {
                return false;
            };
            evt_budget -= 1;
            match ev {
                WorkerEvent::Status(s) => {
                    // ⚠ 前缀的警告（如编码丢字）转交 App 层弹顶部 toast，避免只写状态栏被后续消息滚走
                    if s.starts_with('⚠') {
                        self.pending.warn.push(s.clone());
                    }
                    self.status = s;
                }
                WorkerEvent::Connected => {
                    self.connected = true;
                    self.was_connected = true;
                    self.reconnect_tries = 0;
                    self.reconnect_at = None;
                    self.status = crate::i18n::tr("已连接", "Connected").into();
                    // 重连后恢复工作目录（若断线前由 OSC 7 记录过）；ai_owned 会话从不做
                    // OSC 7 注入（见 layout_body.rs），restore_cwd 理论上不会为 true，这里
                    // 仍显式排除一下，避免以后有别的路径意外置位时悄悄破了只读约定。
                    if self.restore_cwd && !self.last_cwd.is_empty() && !self.ai_owned {
                        let quoted = format!("'{}'", self.last_cwd.replace('\'', "'\\''"));
                        let _ = self.cmd_tx.send(UiCommand::TerminalInput(
                            format!("cd {quoted}\r").into_bytes(),
                        ));
                    }
                    self.restore_cwd = false;
                    // OSC 7 注入改为「点菜单时按需注入」（那时 shell 闲置在提示符，回显可被可靠吞掉），
                    // 不在连接时自动注入，避免与 MOTD/首个提示符的输出竞争、以及每次连接都注入。
                    // 断线前被中断的传输：重连后用新通道重发，底层据本地/远端已有字节自动续传
                    for t in &mut self.transfers {
                        if !t.paused {
                            continue;
                        }
                        match &t.spec {
                            Some(XferSpec::Download { remote, local }) => {
                                let _ = self.cmd_tx.send(UiCommand::Download {
                                    id: t.id,
                                    remote: remote.clone(),
                                    local: local.clone(),
                                    policy: ConflictPolicy::Overwrite,
                                });
                            }
                            Some(XferSpec::Upload { local, remote_dir }) => {
                                let _ = self.cmd_tx.send(UiCommand::Upload {
                                    id: t.id,
                                    local: local.clone(),
                                    remote_dir: remote_dir.clone(),
                                    policy: ConflictPolicy::Overwrite,
                                });
                            }
                            None => continue,
                        }
                        t.paused = false;
                        t.message = crate::i18n::tr("续传中 …", "Resuming …").into();
                    }
                    // 断线前建立的端口转发：用新通道重建（沿用原 id/配置）。首次连接时 forwards 为空，无操作。
                    let readd: Vec<crate::proto::ForwardSpec> = self
                        .forwards
                        .iter()
                        .map(|f| crate::proto::ForwardSpec {
                            id: f.id,
                            bind_host: f.bind_host.clone(),
                            bind_port: f.bind_port,
                            kind: f.kind.clone(),
                        })
                        .collect();
                    for spec in readd {
                        let _ = self.cmd_tx.send(UiCommand::AddForward(spec));
                    }
                }
                WorkerEvent::Disconnected(reason) => {
                    self.connected = false;
                    self.status = reason;
                    // 断线意味着这个会话上任何挂起的 AI 命令都注定等不到哨兵了（worker 重启
                    // 后旧连接的输出流已经没了）——给还在等的 poll_run 一个明确的"未完成"
                    // 响应，而不是让它一直空等到自己的超时，也避免这个会话被"忙碌"卡住。
                    self.cancel_pending_ai_run("会话已断线");
                    self.cancel_pending_file_op();
                    // 进行中的传输标记为暂停，等重连后续传（不计为失败）
                    for t in &mut self.transfers {
                        if t.spec.is_some() && t.ok != Some(true) {
                            t.ok = None;
                            t.paused = true;
                            t.speed = 0.0;
                            t.message =
                                crate::i18n::tr("已中断，重连后续传", "Interrupted; will resume")
                                    .into();
                        }
                    }
                    // 仅对"曾连上又掉线"的会话自动重连，最多 5 次，指数退避
                    const MAX_TRIES: u32 = 5;
                    if self.was_connected && self.reconnect_tries < MAX_TRIES {
                        let secs = (2u64 << self.reconnect_tries.min(4)).min(30); // 2,4,8,16,30
                        self.reconnect_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(secs));
                        let tail = match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{secs}s 后重连"),
                            crate::i18n::Lang::En => format!("reconnect in {secs}s"),
                        };
                        self.status = format!("{} · {}", self.status, tail);
                    } else if self.was_connected && self.reconnect_tries >= MAX_TRIES {
                        let msg = crate::i18n::tr(
                            "自动重连已停止（已达 5 次），请手动重新连接",
                            "Auto-reconnect stopped after 5 tries; reconnect manually",
                        );
                        self.status = format!("{} · {}", self.status, msg);
                        self.pending.warn.push(msg.into());
                    }
                }
                WorkerEvent::MonitorSupport(ok) => {
                    self.monitor_ok = Some(ok);
                    if !ok {
                        self.pending.warn.push(
                            crate::i18n::tr(
                                "远端非 Linux 或缺少 /proc，系统监控已禁用",
                                "Remote is not Linux or lacks /proc; system monitor disabled",
                            )
                            .into(),
                        );
                    }
                }
                WorkerEvent::TerminalData(bytes) => {
                    term_budget = term_budget.saturating_sub(bytes.len());
                    self.terminal.feed(&bytes);
                    if let Some(c) = self.terminal.cwd() {
                        self.last_cwd = c.to_string();
                    }
                }
                WorkerEvent::DirListing { path, entries } => {
                    self.files.on_listing(path, entries);
                }
                WorkerEvent::DirListFailed {
                    path,
                    message,
                    retryable,
                } => {
                    self.status = message;
                    self.files.on_list_failed(path, retryable);
                }
                WorkerEvent::ProcDetail { pid, cmd, cwd, exe } => {
                    self.proc_detail = Some((pid, cmd, cwd, exe));
                }
                WorkerEvent::ForwardStatus { id, ok, message } => {
                    if let Some(f) = self.forwards.iter_mut().find(|f| f.id == id) {
                        f.ok = ok;
                        f.status = message;
                    }
                }
                WorkerEvent::KbdPrompt {
                    name,
                    instructions,
                    prompts,
                } => {
                    let answers = vec![String::new(); prompts.len()];
                    self.kbd_prompt = Some(KbdPrompt {
                        name,
                        instructions,
                        prompts,
                        answers,
                    });
                }
                WorkerEvent::HostKeyPrompt {
                    host,
                    fingerprint,
                    changed,
                } => {
                    self.pending_hostkey = Some((host, fingerprint, changed));
                    self.status =
                        crate::i18n::tr("等待确认主机指纹 …", "Awaiting host key …").into();
                }
                WorkerEvent::FileOpened {
                    id,
                    path,
                    content,
                    encoding,
                    eol,
                    mtime,
                } => {
                    // 先看是不是 MCP read_file 在等这次读取（或者是个该丢弃的迟到墓碑）——
                    // 是的话直接把内容 move 给它，不转发给编辑器 UI（这种读取没有对应的
                    // 编辑器标签页），也不需要为了先探测匹配而白白 clone 一份大内容。
                    if self.file_read_op_would_resolve(id) {
                        self.try_resolve_file_read(id, Ok(content));
                    } else {
                        self.pending
                            .open
                            .push((id, path, content, encoding, eol, mtime));
                        self.status = crate::i18n::tr("已打开文件", "File opened").into();
                    }
                }
                WorkerEvent::FileSaved { id, path, mtime } => {
                    if !self.file_write_op_would_resolve(id) {
                        self.pending.saved.push((id, path, mtime));
                    } else {
                        self.try_resolve_file_write(id, Ok(mtime));
                    }
                }
                WorkerEvent::FileSaveProgress { path, done, total } => {
                    self.pending.save_progress.push((path, done, total));
                }
                WorkerEvent::FileTail {
                    path,
                    data,
                    offset,
                    truncated,
                } => {
                    self.pending.tail.push((path, data, offset, truncated));
                }
                WorkerEvent::PdfInfo { id, path: _, pages } => {
                    self.pending.pdf_info.push((id, pages));
                }
                WorkerEvent::PdfPage { path, page, data } => {
                    self.pending.pdf_page.push((path, page, data));
                }
                WorkerEvent::PdfSearch {
                    path,
                    query: _,
                    hits,
                    message,
                } => {
                    self.pending.pdf_search.push((path, hits, message));
                }
                WorkerEvent::DocOpened { id, path: _, data } => {
                    self.pending.doc.push((id, data));
                }
                WorkerEvent::FileTooLarge { id, path, size } => {
                    // 绝大多数情况下没有 MCP 文件操作在等：先廉价判断一下，避免每次普通的
                    // "打开大文件"事件都白白 format! 一次。
                    if self.file_read_op_would_resolve(id) {
                        self.try_resolve_file_read(id, Err(format!("文件过大（{size} 字节）")));
                    } else {
                        self.pending.too_large.push((id, path, size));
                    }
                }
                WorkerEvent::FileSaveFailed { id, path, message } => {
                    if self.file_write_op_would_resolve(id) {
                        self.try_resolve_file_write(id, Err(message));
                    } else {
                        self.pending.save_failed.push((id, path, message));
                    }
                }
                WorkerEvent::FileSaveConflict { id, path } => {
                    if self.file_write_op_would_resolve(id) {
                        self.try_resolve_file_write(id, Err("文件已被外部修改，写入被拒绝".into()));
                    } else {
                        self.pending.conflict.push((id, path));
                    }
                }
                WorkerEvent::FileLoadProgress { id, done, total } => {
                    self.pending.load_progress.push((id, done, total));
                }
                WorkerEvent::FileLoadFailed { id, message } => {
                    if self.file_read_op_would_resolve(id) {
                        self.try_resolve_file_read(id, Err(message));
                    } else {
                        self.pending.load_fail.push((id, message.clone()));
                        self.status = match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("打开失败：{message}"),
                            crate::i18n::Lang::En => format!("Open failed: {message}"),
                        };
                    }
                }
                WorkerEvent::ImageOpened { path, data } => {
                    self.pending.image.push((path, data));
                    self.status = crate::i18n::tr("已打开图片", "Image opened").into();
                }
                WorkerEvent::OpDone {
                    message,
                    refresh_dir,
                } => {
                    self.status = message;
                    // 刷新操作目标目录。拖拽移动到「非当前目录」的文件夹时，源目录(cwd)
                    // 已在前端乐观移除被移动项、不在此刷新，避免整目录重载导致的跳动。
                    self.refresh_dir(refresh_dir);
                }
                WorkerEvent::TransferStart {
                    id,
                    name,
                    total,
                    dir,
                    local,
                } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.name = name;
                        t.total = total;
                        t.dir = dir;
                        // worker 已真正开传：清除「等待」占位态
                        t.queued = false;
                        // 冲突重命名后，worker 上报的是最终本地路径；更新它，使「打开所在文件夹」定位到重命名后的文件
                        if local.is_some() {
                            t.local = local;
                        }
                    } else {
                        self.transfers
                            .push(Transfer::new(id, name, dir, total, local, None));
                    }
                }
                WorkerEvent::TransferProgress { id, done } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        let now = std::time::Instant::now();
                        match t.last_t {
                            Some(prev) => {
                                let dt = now.duration_since(prev).as_secs_f64();
                                if dt >= 0.25 {
                                    let inst = done.saturating_sub(t.last_done) as f64 / dt;
                                    // 指数平滑，读数更稳
                                    t.speed = if t.speed <= 0.0 {
                                        inst
                                    } else {
                                        t.speed * 0.6 + inst * 0.4
                                    };
                                    t.last_done = done;
                                    t.last_t = Some(now);
                                }
                            }
                            None => {
                                t.last_done = done;
                                t.last_t = Some(now);
                            }
                        }
                        t.done = done;
                    }
                }
                WorkerEvent::TransferNote { id, note } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.note = note;
                    }
                }
                WorkerEvent::TransferDone {
                    id,
                    ok,
                    message,
                    refresh_dir,
                } => {
                    let connected = self.connected;
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.note = String::new();
                        t.queued = false;
                        if !ok && !connected && t.spec.is_some() {
                            // 断线引起的失败：转为暂停，等重连续传
                            t.paused = true;
                            t.speed = 0.0;
                            t.message =
                                crate::i18n::tr("已中断，重连后续传", "Interrupted; will resume")
                                    .into();
                        } else {
                            t.ok = Some(ok);
                            if ok && t.total == 0 {
                                t.total = t.done;
                            }
                            t.message = message.clone();
                            t.speed = 0.0;
                        }
                    }
                    self.status = message;
                    // 上传成功：记下「待选中」的文件名，列表刷新后在该目录选中它（拖动上传后高亮所传文件）
                    if ok {
                        if let Some((dir, name)) = self
                            .transfers
                            .iter()
                            .find(|t| t.id == id)
                            .and_then(|t| match &t.spec {
                                Some(XferSpec::Upload { remote_dir, .. }) => {
                                    Some((remote_dir.clone(), t.name.clone()))
                                }
                                _ => None,
                            })
                        {
                            match &mut self.files.pending_select {
                                Some((d, names)) if *d == dir => {
                                    names.insert(name);
                                }
                                _ => {
                                    self.files.pending_select =
                                        Some((dir, std::iter::once(name).collect()))
                                }
                            }
                        }
                    }
                    self.refresh_dir(refresh_dir);
                }
                WorkerEvent::Error(e) => self.status = e,
            }
        }
    }
}
