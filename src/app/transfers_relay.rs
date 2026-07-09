//! Relay transfer lifecycle helpers.

use crate::proto::{ConflictPolicy, UiCommand};

use super::super::{App, RelayPhase};

impl App {
    /// 删除中转临时文件/目录及其空的任务目录。
    pub(in crate::app) fn cleanup_relay_tmp(tmp: &std::path::Path, is_dir: bool) {
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
    pub(in crate::app) fn transfer_ok(&self, uid: u64, id: u64) -> Option<bool> {
        let si = self.session_idx_by_uid(uid)?;
        self.sessions[si]
            .transfers
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.ok)
    }

    /// 取消目标解析：若 (uid,id) 命中某直传任务（源行或目标镜像行），标记该任务「已取消」
    /// 并返回源端真实传输 (src_uid, id)；否则原样返回。使镜像行的取消也能真正生效，
    /// 且用 cancelled 标记替代脆弱的取消文案比对（见 process_direct_jobs）。
    pub(in crate::app) fn cancel_target(&mut self, uid: u64, id: u64) -> (u64, u64) {
        if let Some(j) = self
            .xfer
            .direct_jobs
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
    pub(in crate::app) fn transfer_done_total(&self, uid: u64, id: u64) -> Option<(u64, u64)> {
        let si = self.session_idx_by_uid(uid)?;
        self.sessions[si]
            .transfers
            .iter()
            .find(|t| t.id == id)
            .map(|t| (t.done, t.total))
    }

    /// 把目标会话里预占的「等待」上传占位行标记为失败（源端下载失败/源会话关闭时）。
    pub(in crate::app) fn fail_placeholder(&mut self, dest_uid: u64, up_id: u64, msg: &str) {
        if let Some(di) = self.session_idx_by_uid(dest_uid) {
            if let Some(t) = self.sessions[di]
                .transfers
                .iter_mut()
                .find(|t| t.id == up_id)
            {
                t.ok = Some(false);
                t.queued = false;
                t.note = String::new();
                t.message = msg.to_string();
            }
        }
    }

    /// 推进跨服务器中转任务：下载完成→发起上传；上传完成→（剪切则删源）+ 清理临时。
    pub(in crate::app) fn process_relays(&mut self) {
        let mut i = 0;
        while i < self.xfer.relays.len() {
            enum Step {
                Wait,
                ToUpload,
                Done,
                Failed,
            }
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
                        let (src_uid, dest_uid, up_id) = (
                            self.xfer.relays[i].src_uid,
                            self.xfer.relays[i].dest_uid,
                            self.xfer.relays[i].up_id,
                        );
                        if let Some((done, total)) = self.transfer_done_total(src_uid, dlid) {
                            let note = if total > 0 {
                                let pct = (done as f64 / total as f64 * 100.0).round() as u32;
                                match crate::i18n::current() {
                                    crate::i18n::Lang::Zh => format!("等待源端下载 {pct}%…"),
                                    crate::i18n::Lang::En => format!("Waiting for source {pct}%…"),
                                }
                            } else {
                                crate::i18n::tr("等待源端下载…", "Waiting for source download…")
                                    .into()
                            };
                            if let Some(di) = self.session_idx_by_uid(dest_uid) {
                                if let Some(t) = self.sessions[di]
                                    .transfers
                                    .iter_mut()
                                    .find(|t| t.id == up_id && t.queued)
                                {
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
                        let _ = self.sessions[di].cmd_tx.send(UiCommand::Upload {
                            id: up_id,
                            local: tmp,
                            remote_dir: dest_dir,
                            policy: ConflictPolicy::Overwrite,
                        });
                        if let Some(t) = self.sessions[di]
                            .transfers
                            .iter_mut()
                            .find(|t| t.id == up_id)
                        {
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
                            let _ = self.sessions[sidx].cmd_tx.send(UiCommand::DeleteMany {
                                paths: vec![src_path],
                            });
                        }
                    }
                    Self::cleanup_relay_tmp(&t, d);
                    self.xfer.relays.remove(i);
                }
                Step::Failed => {
                    // 若在下载阶段失败：目标端占位行还停在「等待」，标记其失败避免空挂
                    if let RelayPhase::Down(_) = self.xfer.relays[i].phase {
                        let (dest_uid, up_id) =
                            (self.xfer.relays[i].dest_uid, self.xfer.relays[i].up_id);
                        self.fail_placeholder(
                            dest_uid,
                            up_id,
                            crate::i18n::tr("源端下载失败", "Source download failed"),
                        );
                    }
                    let (t, d) = (self.xfer.relays[i].tmp.clone(), self.xfer.relays[i].is_dir);
                    Self::cleanup_relay_tmp(&t, d);
                    self.xfer.relays.remove(i);
                }
            }
        }
    }
}
