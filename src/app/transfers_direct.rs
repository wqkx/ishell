//! Direct cross-server transfer lifecycle and dialogs.

use egui::RichText;

use crate::proto::UiCommand;
use crate::theme::Palette;

use super::super::util::host_in_known_hosts;
use super::super::widgets::{dialog_body, dialog_button};
use super::super::{App, DirectFallback, DirectJob, PendingPaste, Session, Transfer};

impl App {
    /// 执行跨服务器「直传」：据目标会话配置构造 DirectSpec，交源会话 worker 在源主机上
    /// 直接 rsync/scp 推到目标主机。仅目标为「无口令密钥」认证时可用；否则立即弹「转中转」。
    /// `hostkey_confirmed`：目标不在本机 known_hosts 时须先经 UI 确认 TOFU。
    pub(in crate::app) fn execute_direct(&mut self, plan: PendingPaste) {
        self.execute_direct_inner(plan, false);
    }

    fn execute_direct_inner(&mut self, plan: PendingPaste, hostkey_confirmed: bool) {
        let Some(si) = self.session_idx_by_uid(plan.src_uid) else {
            if let Some(di) = self.session_idx_by_uid(plan.dest_uid) {
                self.sessions[di].status = crate::i18n::tr(
                    "源会话已关闭，无法直传",
                    "Source session closed; cannot direct-transfer",
                )
                .into();
            }
            return;
        };
        let Some(di) = self.session_idx_by_uid(plan.dest_uid) else {
            return;
        };
        // 目标主机连接参数
        let dest_cfg = self.sessions[di].cfg.clone();
        // 仅支持「无口令密钥」认证的目标：取私钥本地路径
        let key_path = match &dest_cfg.auth {
            crate::proto::AuthMethod::KeyFile { path, passphrase }
                if passphrase.as_deref().map(|s| s.is_empty()).unwrap_or(true) =>
            {
                path.clone()
            }
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
        let dest_host_known = host_in_known_hosts(&dest_cfg.host, dest_cfg.port);
        if !dest_host_known && !hostkey_confirmed {
            self.xfer.pending_direct_hostkey = Some(plan);
            return;
        }
        let n = plan.items.len();
        let first = plan
            .items
            .first()
            .map(|(p, _)| {
                p.rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(p)
                    .to_string()
            })
            .unwrap_or_default();
        let label = if n > 1 {
            format!("{first} +{}", n - 1)
        } else {
            first
        };
        let tag = crate::i18n::tr("直传", "Direct").to_string();
        // 源会话（A）：真实数据通路所在行。total 由 worker 的 du 估算后回填（字节计）
        let id = {
            let s = &mut self.sessions[si];
            let id = s.next_xfer;
            s.next_xfer += 1;
            let mut t = Transfer::new(
                id,
                label.clone(),
                crate::proto::TransferDir::Upload,
                0,
                None,
                None,
            );
            t.tag = tag.clone();
            s.transfers.push(t);
            id
        };
        // 目标会话（B）：镜像进度行（直传不经 B，App 据源端进度同步显示），方向取「接收=下载」绿色
        let mir_id = {
            let s = &mut self.sessions[di];
            let mid = s.next_xfer;
            s.next_xfer += 1;
            let mut t = Transfer::new(
                mid,
                label.clone(),
                crate::proto::TransferDir::Download,
                0,
                None,
                None,
            );
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
            dest_host_known,
        };
        let _ = self.sessions[si]
            .cmd_tx
            .send(UiCommand::DirectTransfer(Box::new(spec)));
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
            (_, crate::i18n::Lang::En) => {
                format!("Direct transfer of {n} item(s) into this folder…")
            }
        };
    }

    /// 收尾直传在目标端（B）的镜像进度行：标记完成/失败；完成时进度拉满。
    pub(in crate::app) fn finish_mirror(
        sessions: &mut [Session],
        job: &DirectJob,
        ok: bool,
        msg: &str,
    ) {
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
    pub(in crate::app) fn process_direct_jobs(&mut self) {
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
                            if let Some(t) = self.sessions[didx]
                                .transfers
                                .iter_mut()
                                .find(|t| t.id == mir_id && t.ok.is_none())
                            {
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
                    Self::finish_mirror(
                        &mut self.sessions,
                        &job,
                        true,
                        crate::i18n::tr("直传完成", "Direct transfer done"),
                    );
                    // 剪切：直传成功后删源
                    if job.is_cut {
                        if let Some(sidx) = self.session_idx_by_uid(job.src_uid) {
                            let paths: Vec<String> =
                                job.items.iter().map(|(p, _)| p.clone()).collect();
                            let _ = self.sessions[sidx]
                                .cmd_tx
                                .send(UiCommand::DeleteMany { paths });
                        }
                    }
                    // 刷新目标目录（直传不经目标会话，需主动让其重列目录）
                    if let Some(didx) = self.session_idx_by_uid(job.dest_uid) {
                        let _ = self.sessions[didx]
                            .cmd_tx
                            .send(crate::proto::list_dir_cmd(job.dest_dir.clone()));
                    }
                }
                Some(false) => {
                    let job = self.xfer.direct_jobs.remove(i);
                    // 用户主动取消（取消按钮已置 job.cancelled）不弹回退；据此与真失败区分，
                    // 不再靠比对本地化的「已取消」文案（脆弱：worker 可能回报「直传失败（码 -1）」）。
                    let cancelled = job.cancelled;
                    // 目标端镜像行收尾为「失败/取消」
                    Self::finish_mirror(
                        &mut self.sessions,
                        &job,
                        false,
                        if cancelled {
                            crate::i18n::tr("已取消", "Canceled")
                        } else {
                            crate::i18n::tr("直传失败", "Direct failed")
                        },
                    );
                    // 真失败：入队「转中转」提醒，确认后走中转链路（队列避免多任务同帧互相覆盖）
                    if !cancelled
                        && self.session_idx_by_uid(job.src_uid).is_some()
                        && self.session_idx_by_uid(job.dest_uid).is_some()
                    {
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
                            reason: crate::i18n::tr(
                                "直传失败（源主机无法直推到目标主机）。",
                                "Direct transfer failed (source cannot push to target).",
                            )
                            .into(),
                        });
                    }
                }
            }
        }
    }

    /// 直传目标不在本机 known_hosts：确认后才允许源机 accept-new（首次 TOFU）。
    pub(in crate::app) fn direct_hostkey_dialog(&mut self, ctx: &egui::Context) {
        let Some(plan) = self.xfer.pending_direct_hostkey.as_ref() else {
            return;
        };
        let dest = plan.dest_label.clone();
        let mut go = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("direct_hostkey")).show(ctx, |ui| {
            dialog_body(ui, |ui| {
                ui.label(RichText::new(crate::i18n::tr(
                    "直传目标主机密钥未在本机记录",
                    "Direct-transfer target host key not in local known_hosts",
                )).size(16.0).strong());
                ui.add_space(8.0);
                ui.label(RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("目标：{dest}"),
                    crate::i18n::Lang::En => format!("Target: {dest}"),
                }).size(12.0));
                ui.add_space(6.0);
                ui.label(RichText::new(crate::i18n::tr(
                    "继续将使源主机以 accept-new 首次信任该目标（指纹确认发生在源机，不经本机 iShell 弹窗）。仅在确认目标无误时继续；否则请改用中转。",
                    "Continuing lets the source host trust the target via accept-new on first connect (TOFU on the source, not in iShell). Continue only if the target is correct; otherwise use relay.",
                )).color(Palette::DANGER).size(11.0));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 110.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    if dialog_button(ui, crate::i18n::tr("仍直传", "Direct anyway"), Some(Palette::DANGER), bw) {
                        go = true;
                    }
                    if dialog_button(ui, crate::i18n::tr("取消", "Cancel"), None, bw) {
                        cancel = true;
                    }
                });
            });
        });
        if go {
            if let Some(plan) = self.xfer.pending_direct_hostkey.take() {
                self.execute_direct_inner(plan, true);
            }
        } else if cancel {
            self.xfer.pending_direct_hostkey = None;
        }
    }

    /// 直传失败后的「必须改用中转」提醒弹框。
    pub(in crate::app) fn direct_fallback_dialog(&mut self, ctx: &egui::Context) {
        let Some(fb) = self.xfer.pending_direct_fallback.first() else {
            return;
        };
        let mut go = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("direct_fallback")).show(ctx, |ui| {
            dialog_body(ui, |ui| {
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "直传未成功，必须改用中转",
                        "Direct transfer failed — relay is required",
                    ))
                    .size(16.0)
                    .strong(),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(&fb.reason).color(Palette::DANGER).size(11.0));
                ui.add_space(6.0);
                // 源主机 + 源目录 → 目标主机 + 目标目录
                ui.label(
                    RichText::new(format!("{}  →  {}", fb.plan.src_label, fb.plan.dest_label))
                        .color(Palette::TEXT)
                        .size(12.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(format!("{}  →  {}", fb.plan.src_dir, fb.plan.dest_dir))
                        .monospace()
                        .size(11.0)
                        .color(Palette::TEXT_DIM),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(crate::i18n::tr(
                        "将改为经本地「下载→上传」中转，较慢但最通用。",
                        "Will switch to local download→upload relay; slower but most compatible.",
                    ))
                    .color(Palette::TEXT_DIM)
                    .size(11.0),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let bw = 110.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                    if dialog_button(
                        ui,
                        crate::i18n::tr("改用中转", "Use relay"),
                        Some(Palette::OK),
                        bw,
                    ) {
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
        } else if cancel && !self.xfer.pending_direct_fallback.is_empty() {
            self.xfer.pending_direct_fallback.remove(0);
        }
    }
}
