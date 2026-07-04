//! App 的传输/中转/直传/粘贴相关方法（从 God Object 拆出，行为不变）。
//! 这些都是 `impl App` 的方法，签名与调用点不变；仅物理迁移以缩小 mod.rs。

use egui::RichText;

use crate::proto::{ConflictPolicy, UiCommand};
use crate::theme::Palette;

use super::util::parent_dir;
use super::widgets::{dialog_body, dialog_button};
use super::{App, DirectFallback, DirectJob, FileClip, PendingPaste, Relay, RelayPhase, Session, Transfer};

impl App {
    /// 复制 / 剪切选中项到 App 级剪贴板（跨 tab 共享）。
    pub(super) fn set_clip(&mut self, idx: usize, items: Vec<(String, bool)>, is_cut: bool) {
        let (uid, host, port, label) = match self.sessions.get(idx) {
            Some(s) => (s.uid, s.cfg.host.clone(), s.cfg.port, s.title.clone()),
            None => return,
        };
        let n = items.len();
        self.xfer.file_clip = Some(FileClip { items, is_cut, src_uid: uid, src_host: host, src_port: port, src_label: label });
        if let Some(s) = self.sessions.get_mut(idx) {
            s.status = match (is_cut, crate::i18n::current()) {
                (true, crate::i18n::Lang::Zh) => format!("已剪切 {n} 项（粘贴时移动）"),
                (false, crate::i18n::Lang::Zh) => format!("已复制 {n} 项到剪贴板"),
                (true, crate::i18n::Lang::En) => format!("Cut {n} item(s)"),
                (false, crate::i18n::Lang::En) => format!("Copied {n} item(s)"),
            };
        }
    }

    /// 粘贴到目标目录：同机直接 cp/mv；剪切或跨服务器需先二次确认。
    pub(super) fn start_paste(&mut self, idx: usize, dest_dir: String) {
        let Some(clip) = self.xfer.file_clip.as_ref() else { return };
        let Some(dest) = self.sessions.get(idx) else { return };
        let cross = clip.src_host != dest.cfg.host || clip.src_port != dest.cfg.port;
        let src_dir = clip.items.first().map(|(p, _)| parent_dir(p)).unwrap_or_default();
        let plan = PendingPaste {
            items: clip.items.clone(),
            is_cut: clip.is_cut,
            cross,
            src_uid: clip.src_uid,
            dest_uid: dest.uid,
            src_dir,
            dest_dir,
            src_label: clip.src_label.clone(),
            dest_label: dest.title.clone(),
            direct: false, // 传输方式由确认弹框里的互斥选择决定
        };
        // 仅「跨服务器」需执行前确认（重操作 + 选直传/中转）；同机无论复制还是移动都直接执行——
        // 同机移动是原子 mv，源在目标写成功前不会丢，无需二次确认。
        if plan.cross {
            self.xfer.confirm_direct = false; // 每次打开确认默认「中转」(更安全，直传会暴露私钥)
            self.xfer.pending_paste = Some(plan);
        } else {
            self.execute_paste(plan);
        }
    }

    /// 真正执行粘贴：同机服务器端 cp/mv；跨机建中转任务（下载→上传）。
    pub(super) fn execute_paste(&mut self, plan: PendingPaste) {
        let is_cut = plan.is_cut; // 提前取出：plan 在直传分支会被移动
        if !plan.cross {
            let srcs: Vec<String> = plan.items.iter().map(|(p, _)| p.clone()).collect();
            if let Some(di) = self.session_idx_by_uid(plan.dest_uid) {
                let n = srcs.len();
                let s = &mut self.sessions[di];
                let _ = s.cmd_tx.send(UiCommand::CopyMove { srcs, dest_dir: plan.dest_dir.clone(), do_move: plan.is_cut });
                s.status = match (plan.is_cut, crate::i18n::current()) {
                    (true, crate::i18n::Lang::Zh) => format!("移动 {n} 项 …"),
                    (false, crate::i18n::Lang::Zh) => format!("复制 {n} 项 …"),
                    (true, crate::i18n::Lang::En) => format!("Moving {n} …"),
                    (false, crate::i18n::Lang::En) => format!("Copying {n} …"),
                };
            }
        } else if plan.direct {
            self.execute_direct(plan);
        } else {
            // 跨服务器中转：源会话与目标会话都须在线
            let Some(di) = self.session_idx_by_uid(plan.dest_uid) else { return };
            let Some(si) = self.session_idx_by_uid(plan.src_uid) else {
                self.sessions[di].status = crate::i18n::tr("源会话已关闭，无法跨服务器粘贴", "Source session closed; cannot paste across servers").into();
                return;
            };
            for (src_path, is_dir) in &plan.items {
                let base = src_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("item").to_string();
                self.xfer.relay_seq += 1;
                // 中转临时目录：加入密码学随机段防 /tmp 可预测路径被 symlink 抢占；
                // Unix 下目录权限收紧为 0700，避免同机其他用户读取中转内容
                let mut rnd = [0u8; 8];
                let _ = getrandom::getrandom(&mut rnd);
                let rnd_hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
                let tmp = std::env::temp_dir()
                    .join("ishell-relay")
                    .join(format!("{}-{}-{}", std::process::id(), self.xfer.relay_seq, rnd_hex))
                    .join(&base);
                if let Some(parent) = tmp.parent() {
                    let _ = std::fs::create_dir_all(parent);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                        if let Some(root) = parent.parent() {
                            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
                        }
                    }
                }
                let dlid = {
                    let s = &mut self.sessions[si];
                    let id = s.next_xfer;
                    s.next_xfer += 1;
                    let _ = s.cmd_tx.send(UiCommand::Download { id, remote: src_path.clone(), local: tmp.to_string_lossy().into_owned(), policy: ConflictPolicy::Overwrite });
                    id
                };
                // 在目标会话预占一条「等待」上传占位行：源端下载期间 B 也有可见状态
                let up_id = {
                    let s = &mut self.sessions[di];
                    let id = s.next_xfer;
                    s.next_xfer += 1;
                    let mut t = Transfer::new(id, base.clone(), crate::proto::TransferDir::Upload, 0, None, None);
                    t.queued = true;
                    t.note = crate::i18n::tr("等待源端下载…", "Waiting for source download…").into();
                    s.transfers.push(t);
                    id
                };
                self.xfer.relays.push(Relay {
                    src_path: src_path.clone(),
                    is_dir: *is_dir,
                    src_uid: plan.src_uid,
                    dest_uid: plan.dest_uid,
                    dest_dir: plan.dest_dir.clone(),
                    is_cut: plan.is_cut,
                    tmp,
                    phase: RelayPhase::Down(dlid),
                    up_id,
                });
            }
            self.show_transfers = true;
            self.xfer_just_opened = true;
            let n = plan.items.len();
            self.sessions[di].status = match (plan.is_cut, crate::i18n::current()) {
                (true, crate::i18n::Lang::Zh) => format!("跨服务器移动 {n} 项（经本地中转）…"),
                (false, crate::i18n::Lang::Zh) => format!("跨服务器复制 {n} 项（经本地中转）…"),
                (true, crate::i18n::Lang::En) => format!("Cross-server move {n} (via local relay) …"),
                (false, crate::i18n::Lang::En) => format!("Cross-server copy {n} (via local relay) …"),
            };
        }
        // 剪切粘贴后清空剪贴板（复制保留，便于多次粘贴）
        if is_cut {
            self.xfer.file_clip = None;
        }
    }

    /// 删除中转临时文件/目录及其空的任务目录。
    pub(super) fn cleanup_relay_tmp(tmp: &std::path::Path, is_dir: bool) {
        if is_dir {
            let _ = std::fs::remove_dir_all(tmp);
        } else {
            let _ = std::fs::remove_file(tmp);
        }
        if let Some(parent) = tmp.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    /// 查询某会话某传输的完成状态（None=进行中/未知）。
    pub(super) fn transfer_ok(&self, uid: u64, id: u64) -> Option<bool> {
        let si = self.session_idx_by_uid(uid)?;
        self.sessions[si].transfers.iter().find(|t| t.id == id).and_then(|t| t.ok)
    }

    /// 取消目标解析：若 (uid,id) 命中某直传任务（源行或目标镜像行），标记该任务「已取消」
    /// 并返回源端真实传输 (src_uid, id)；否则原样返回。使镜像行的取消也能真正生效，
    /// 且用 cancelled 标记替代脆弱的取消文案比对（见 process_direct_jobs）。
    pub(super) fn cancel_target(&mut self, uid: u64, id: u64) -> (u64, u64) {
        if let Some(j) = self.xfer.direct_jobs
            .iter_mut()
            .find(|j| (j.src_uid == uid && j.id == id) || (j.dest_uid == uid && j.mir_id == id))
        {
            j.cancelled = true;
            (j.src_uid, j.id)
        } else {
            (uid, id)
        }
    }

    /// 查询某会话某传输的 (已传, 总量)（用于把源端下载进度反映到目标端「等待」行）。
    pub(super) fn transfer_done_total(&self, uid: u64, id: u64) -> Option<(u64, u64)> {
        let si = self.session_idx_by_uid(uid)?;
        self.sessions[si].transfers.iter().find(|t| t.id == id).map(|t| (t.done, t.total))
    }

    /// 把目标会话里预占的「等待」上传占位行标记为失败（源端下载失败/源会话关闭时）。
    pub(super) fn fail_placeholder(&mut self, dest_uid: u64, up_id: u64, msg: &str) {
        if let Some(di) = self.session_idx_by_uid(dest_uid) {
            if let Some(t) = self.sessions[di].transfers.iter_mut().find(|t| t.id == up_id) {
                t.ok = Some(false);
                t.queued = false;
                t.note = String::new();
                t.message = msg.to_string();
            }
        }
    }

    /// 推进跨服务器中转任务：下载完成→发起上传；上传完成→（剪切则删源）+ 清理临时。
    pub(super) fn process_relays(&mut self) {
        let mut i = 0;
        while i < self.xfer.relays.len() {
            enum Step { Wait, ToUpload, Done, Failed }
            let step = {
                let r = &self.xfer.relays[i];
                match r.phase {
                    RelayPhase::Down(id) => match self.transfer_ok(r.src_uid, id) {
                        Some(true) => Step::ToUpload,
                        Some(false) => Step::Failed,
                        None if self.session_idx_by_uid(r.src_uid).is_none() => Step::Failed,
                        None => Step::Wait,
                    },
                    RelayPhase::Up(id) => match self.transfer_ok(r.dest_uid, id) {
                        Some(true) => Step::Done,
                        Some(false) => Step::Failed,
                        None if self.session_idx_by_uid(r.dest_uid).is_none() => Step::Failed,
                        None => Step::Wait,
                    },
                }
            };
            match step {
                Step::Wait => {
                    // 仍在下载：把源端进度实时反映到目标端「等待」占位行的提示上
                    if let RelayPhase::Down(dlid) = self.xfer.relays[i].phase {
                        let (src_uid, dest_uid, up_id) = (self.xfer.relays[i].src_uid, self.xfer.relays[i].dest_uid, self.xfer.relays[i].up_id);
                        if let Some((done, total)) = self.transfer_done_total(src_uid, dlid) {
                            let note = if total > 0 {
                                let pct = (done as f64 / total as f64 * 100.0).round() as u32;
                                match crate::i18n::current() {
                                    crate::i18n::Lang::Zh => format!("等待源端下载 {pct}%…"),
                                    crate::i18n::Lang::En => format!("Waiting for source {pct}%…"),
                                }
                            } else {
                                crate::i18n::tr("等待源端下载…", "Waiting for source download…").into()
                            };
                            if let Some(di) = self.session_idx_by_uid(dest_uid) {
                                if let Some(t) = self.sessions[di].transfers.iter_mut().find(|t| t.id == up_id && t.queued) {
                                    t.note = note;
                                }
                            }
                        }
                    }
                    i += 1;
                }
                Step::ToUpload => {
                    let dest_uid = self.xfer.relays[i].dest_uid;
                    let tmp = self.xfer.relays[i].tmp.to_string_lossy().into_owned();
                    let dest_dir = self.xfer.relays[i].dest_dir.clone();
                    let up_id = self.xfer.relays[i].up_id;
                    if let Some(di) = self.session_idx_by_uid(dest_uid) {
                        // 复用粘贴时预占的 up_id：worker 的 TransferStart 会把占位行就地转为进行中
                        let _ = self.sessions[di].cmd_tx.send(UiCommand::Upload { id: up_id, local: tmp, remote_dir: dest_dir, policy: ConflictPolicy::Overwrite });
                        if let Some(t) = self.sessions[di].transfers.iter_mut().find(|t| t.id == up_id) {
                            t.queued = false;
                            t.note = String::new();
                        }
                        self.xfer.relays[i].phase = RelayPhase::Up(up_id);
                        i += 1;
                    } else {
                        let (t, d) = (self.xfer.relays[i].tmp.clone(), self.xfer.relays[i].is_dir);
                        Self::cleanup_relay_tmp(&t, d);
                        self.xfer.relays.remove(i);
                    }
                }
                Step::Done => {
                    let (t, d, is_cut, src_uid, src_path) = (
                        self.xfer.relays[i].tmp.clone(),
                        self.xfer.relays[i].is_dir,
                        self.xfer.relays[i].is_cut,
                        self.xfer.relays[i].src_uid,
                        self.xfer.relays[i].src_path.clone(),
                    );
                    // 剪切：上传成功后才删源（安全）
                    if is_cut {
                        if let Some(sidx) = self.session_idx_by_uid(src_uid) {
                            let _ = self.sessions[sidx].cmd_tx.send(UiCommand::DeleteMany { paths: vec![src_path] });
                        }
                    }
                    Self::cleanup_relay_tmp(&t, d);
                    self.xfer.relays.remove(i);
                }
                Step::Failed => {
                    // 若在下载阶段失败：目标端占位行还停在「等待」，标记其失败避免空挂
                    if let RelayPhase::Down(_) = self.xfer.relays[i].phase {
                        let (dest_uid, up_id) = (self.xfer.relays[i].dest_uid, self.xfer.relays[i].up_id);
                        self.fail_placeholder(dest_uid, up_id, crate::i18n::tr("源端下载失败", "Source download failed"));
                    }
                    let (t, d) = (self.xfer.relays[i].tmp.clone(), self.xfer.relays[i].is_dir);
                    Self::cleanup_relay_tmp(&t, d);
                    self.xfer.relays.remove(i);
                }
            }
        }
    }

    /// 执行跨服务器「直传」：据目标会话配置构造 DirectSpec，交源会话 worker 在源主机上
    /// 直接 rsync/scp 推到目标主机。仅目标为「无口令密钥」认证时可用；否则立即弹「转中转」。
    pub(super) fn execute_direct(&mut self, plan: PendingPaste) {
        let Some(si) = self.session_idx_by_uid(plan.src_uid) else {
            if let Some(di) = self.session_idx_by_uid(plan.dest_uid) {
                self.sessions[di].status = crate::i18n::tr("源会话已关闭，无法直传", "Source session closed; cannot direct-transfer").into();
            }
            return;
        };
        let Some(di) = self.session_idx_by_uid(plan.dest_uid) else { return };
        // 目标主机连接参数
        let dest_cfg = self.sessions[di].cfg.clone();
        // 仅支持「无口令密钥」认证的目标：取私钥本地路径
        let key_path = match &dest_cfg.auth {
            crate::proto::AuthMethod::KeyFile { path, passphrase } if passphrase.as_deref().map(|s| s.is_empty()).unwrap_or(true) => path.clone(),
            _ => {
                // 直传不可用（密码/agent/交互/口令密钥）：直接进入「转中转」提醒
                self.xfer.pending_direct_fallback.push(DirectFallback {
                    plan: PendingPaste { direct: false, ..plan.clone() },
                    reason: crate::i18n::tr(
                        "目标会话非「无口令密钥」认证，无法直传。",
                        "Target session is not passphrase-less key auth; direct transfer unavailable.",
                    ).into(),
                });
                return;
            }
        };
        let n = plan.items.len();
        let first = plan.items.first().map(|(p, _)| p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p).to_string()).unwrap_or_default();
        let label = if n > 1 { format!("{first} +{}", n - 1) } else { first };
        let tag = crate::i18n::tr("直传", "Direct").to_string();
        // 源会话（A）：真实数据通路所在行。total 由 worker 的 du 估算后回填（字节计）
        let id = {
            let s = &mut self.sessions[si];
            let id = s.next_xfer;
            s.next_xfer += 1;
            let mut t = Transfer::new(id, label.clone(), crate::proto::TransferDir::Upload, 0, None, None);
            t.tag = tag.clone();
            s.transfers.push(t);
            id
        };
        // 目标会话（B）：镜像进度行（直传不经 B，App 据源端进度同步显示），方向取「接收=下载」绿色
        let mir_id = {
            let s = &mut self.sessions[di];
            let mid = s.next_xfer;
            s.next_xfer += 1;
            let mut t = Transfer::new(mid, label.clone(), crate::proto::TransferDir::Download, 0, None, None);
            t.tag = tag.clone();
            s.transfers.push(t);
            mid
        };
        let spec = crate::proto::DirectSpec {
            id,
            srcs: plan.items.iter().map(|(p, _)| p.clone()).collect(),
            dest_user: dest_cfg.username.clone(),
            dest_host: dest_cfg.host.clone(),
            dest_port: dest_cfg.port,
            dest_dir: plan.dest_dir.clone(),
            key_path,
            label: label.clone(),
        };
        let _ = self.sessions[si].cmd_tx.send(UiCommand::DirectTransfer(Box::new(spec)));
        self.xfer.direct_jobs.push(DirectJob {
            id,
            mir_id,
            src_uid: plan.src_uid,
            dest_uid: plan.dest_uid,
            src_dir: plan.src_dir.clone(),
            dest_dir: plan.dest_dir.clone(),
            is_cut: plan.is_cut,
            items: plan.items.clone(),
            src_label: plan.src_label.clone(),
            dest_label: plan.dest_label.clone(),
            cancelled: false,
        });
        self.show_transfers = true;
        self.xfer_just_opened = true;
        self.sessions[si].status = match (plan.is_cut, crate::i18n::current()) {
            (true, crate::i18n::Lang::Zh) => format!("跨服务器移动 {n} 项（直传）…"),
            (false, crate::i18n::Lang::Zh) => format!("跨服务器复制 {n} 项（直传）…"),
            (true, crate::i18n::Lang::En) => format!("Cross-server move {n} (direct) …"),
            (false, crate::i18n::Lang::En) => format!("Cross-server copy {n} (direct) …"),
        };
        // 目标会话（B）也给一句状态（其传输浮窗里有镜像进度行）
        self.sessions[di].status = match (plan.is_cut, crate::i18n::current()) {
            (_, crate::i18n::Lang::Zh) => format!("正从源主机直传 {n} 项到此目录…"),
            (_, crate::i18n::Lang::En) => format!("Direct transfer of {n} item(s) into this folder…"),
        };
    }

    /// 收尾直传在目标端（B）的镜像进度行：标记完成/失败；完成时进度拉满。
    pub(super) fn finish_mirror(sessions: &mut [Session], job: &DirectJob, ok: bool, msg: &str) {
        if let Some(s) = sessions.iter_mut().find(|s| s.uid == job.dest_uid) {
            if let Some(t) = s.transfers.iter_mut().find(|t| t.id == job.mir_id) {
                t.ok = Some(ok);
                t.message = msg.to_string();
                if ok {
                    t.done = t.total;
                }
            }
        }
    }

    /// 推进直传任务：源会话上的直传传输完成 → 剪切则删源 + 刷新目标目录；失败 → 弹「转中转」提醒。
    pub(super) fn process_direct_jobs(&mut self) {
        let mut i = 0;
        while i < self.xfer.direct_jobs.len() {
            let (src_uid, sid, dest_uid, mir_id) = {
                let j = &self.xfer.direct_jobs[i];
                (j.src_uid, j.id, j.dest_uid, j.mir_id)
            };
            let status = match self.transfer_ok(src_uid, sid) {
                Some(ok) => Some(ok),
                None if self.session_idx_by_uid(src_uid).is_none() => Some(false),
                None => None,
            };
            match status {
                None => {
                    // 进行中：把源端（A）的真实进度同步到目标端（B）的镜像行
                    if let Some((done, total)) = self.transfer_done_total(src_uid, sid) {
                        if let Some(didx) = self.session_idx_by_uid(dest_uid) {
                            if let Some(t) = self.sessions[didx].transfers.iter_mut().find(|t| t.id == mir_id && t.ok.is_none()) {
                                t.total = total;
                                t.done = done;
                            }
                        }
                    }
                    i += 1;
                }
                Some(true) => {
                    let job = self.xfer.direct_jobs.remove(i);
                    // 目标端镜像行收尾为「完成」
                    Self::finish_mirror(&mut self.sessions, &job, true, crate::i18n::tr("直传完成", "Direct transfer done"));
                    // 剪切：直传成功后删源
                    if job.is_cut {
                        if let Some(sidx) = self.session_idx_by_uid(job.src_uid) {
                            let paths: Vec<String> = job.items.iter().map(|(p, _)| p.clone()).collect();
                            let _ = self.sessions[sidx].cmd_tx.send(UiCommand::DeleteMany { paths });
                        }
                    }
                    // 刷新目标目录（直传不经目标会话，需主动让其重列目录）
                    if let Some(didx) = self.session_idx_by_uid(job.dest_uid) {
                        let _ = self.sessions[didx].cmd_tx.send(UiCommand::ListDir(job.dest_dir.clone()));
                    }
                }
                Some(false) => {
                    let job = self.xfer.direct_jobs.remove(i);
                    // 用户主动取消（取消按钮已置 job.cancelled）不弹回退；据此与真失败区分，
                    // 不再靠比对本地化的「已取消」文案（脆弱：worker 可能回报「直传失败（码 -1）」）。
                    let cancelled = job.cancelled;
                    // 目标端镜像行收尾为「失败/取消」
                    Self::finish_mirror(&mut self.sessions, &job, false,
                        if cancelled { crate::i18n::tr("已取消", "Canceled") } else { crate::i18n::tr("直传失败", "Direct failed") });
                    // 真失败：入队「转中转」提醒，确认后走中转链路（队列避免多任务同帧互相覆盖）
                    if !cancelled && self.session_idx_by_uid(job.src_uid).is_some() && self.session_idx_by_uid(job.dest_uid).is_some() {
                        self.xfer.pending_direct_fallback.push(DirectFallback {
                            plan: PendingPaste {
                                items: job.items,
                                is_cut: job.is_cut,
                                cross: true,
                                src_uid: job.src_uid,
                                dest_uid: job.dest_uid,
                                src_dir: job.src_dir,
                                dest_dir: job.dest_dir,
                                src_label: job.src_label,
                                dest_label: job.dest_label,
                                direct: false,
                            },
                            reason: crate::i18n::tr("直传失败（源主机无法直推到目标主机）。", "Direct transfer failed (source cannot push to target).").into(),
                        });
                    }
                }
            }
        }
    }

    /// 直传失败后的「必须改用中转」提醒弹框。
    pub(super) fn direct_fallback_dialog(&mut self, ctx: &egui::Context) {
        let Some(fb) = self.xfer.pending_direct_fallback.first() else { return };
        let mut go = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("direct_fallback")).show(ctx, |ui| {
            dialog_body(ui, |ui| {
            ui.label(RichText::new(crate::i18n::tr("直传未成功，必须改用中转", "Direct transfer failed — relay is required")).size(16.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(&fb.reason).color(Palette::DANGER).size(11.0));
            ui.add_space(6.0);
            // 源主机 + 源目录 → 目标主机 + 目标目录
            ui.label(RichText::new(format!("{}  →  {}", fb.plan.src_label, fb.plan.dest_label)).color(Palette::TEXT).size(12.0).strong());
            ui.label(RichText::new(format!("{}  →  {}", fb.plan.src_dir, fb.plan.dest_dir)).monospace().size(11.0).color(Palette::TEXT_DIM));
            ui.add_space(6.0);
            ui.label(RichText::new(crate::i18n::tr("将改为经本地「下载→上传」中转，较慢但最通用。", "Will switch to local download→upload relay; slower but most compatible.")).color(Palette::TEXT_DIM).size(11.0));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let bw = 110.0;
                let total = bw * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                if dialog_button(ui, crate::i18n::tr("改用中转", "Use relay"), Some(Palette::OK), bw) {
                    go = true;
                }
                if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                    cancel = true;
                }
            });
            });
        });
        if go {
            if !self.xfer.pending_direct_fallback.is_empty() {
                let fb = self.xfer.pending_direct_fallback.remove(0);
                self.execute_paste(fb.plan);
            }
        } else if cancel {
            if !self.xfer.pending_direct_fallback.is_empty() {
                self.xfer.pending_direct_fallback.remove(0);
            }
        }
    }
}
