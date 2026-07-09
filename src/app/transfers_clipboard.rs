//! Clipboard paste and same-host/cross-host paste dispatch.

use crate::proto::{ConflictPolicy, UiCommand};

use super::super::util::parent_dir;
use super::super::{App, FileClip, PendingPaste, Relay, RelayPhase, Transfer};

impl App {
    /// 复制 / 剪切选中项到 App 级剪贴板（跨 tab 共享）。
    pub(in crate::app) fn set_clip(
        &mut self,
        idx: usize,
        items: Vec<(String, bool)>,
        is_cut: bool,
    ) {
        let (uid, host, port, label) = match self.sessions.get(idx) {
            Some(s) => (s.uid, s.cfg.host.clone(), s.cfg.port, s.title.clone()),
            None => return,
        };
        let n = items.len();
        self.xfer.file_clip = Some(FileClip {
            items,
            is_cut,
            src_uid: uid,
            src_host: host,
            src_port: port,
            src_label: label,
        });
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
    pub(in crate::app) fn start_paste(&mut self, idx: usize, dest_dir: String) {
        let Some(clip) = self.xfer.file_clip.as_ref() else {
            return;
        };
        let Some(dest) = self.sessions.get(idx) else {
            return;
        };
        let cross = clip.src_host != dest.cfg.host || clip.src_port != dest.cfg.port;
        let src_dir = clip
            .items
            .first()
            .map(|(p, _)| parent_dir(p))
            .unwrap_or_default();
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
    pub(in crate::app) fn execute_paste(&mut self, plan: PendingPaste) {
        let is_cut = plan.is_cut; // 提前取出：plan 在直传分支会被移动
        if !plan.cross {
            let srcs: Vec<String> = plan.items.iter().map(|(p, _)| p.clone()).collect();
            if let Some(di) = self.session_idx_by_uid(plan.dest_uid) {
                let n = srcs.len();
                let s = &mut self.sessions[di];
                let _ = s.cmd_tx.send(UiCommand::CopyMove {
                    srcs,
                    dest_dir: plan.dest_dir.clone(),
                    do_move: plan.is_cut,
                });
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
            let Some(di) = self.session_idx_by_uid(plan.dest_uid) else {
                return;
            };
            let Some(si) = self.session_idx_by_uid(plan.src_uid) else {
                self.sessions[di].status = crate::i18n::tr(
                    "源会话已关闭，无法跨服务器粘贴",
                    "Source session closed; cannot paste across servers",
                )
                .into();
                return;
            };
            for (src_path, is_dir) in &plan.items {
                let base = src_path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or("item")
                    .to_string();
                self.xfer.relay_seq += 1;
                // 中转临时目录：加入密码学随机段防 /tmp 可预测路径被 symlink 抢占；
                // Unix 下目录权限收紧为 0700，避免同机其他用户读取中转内容
                let mut rnd = [0u8; 8];
                let _ = getrandom::getrandom(&mut rnd);
                let rnd_hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
                let tmp = std::env::temp_dir()
                    .join("ishell-relay")
                    .join(format!(
                        "{}-{}-{}",
                        std::process::id(),
                        self.xfer.relay_seq,
                        rnd_hex
                    ))
                    .join(&base);
                if let Some(parent) = tmp.parent() {
                    let _ = std::fs::create_dir_all(parent);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            parent,
                            std::fs::Permissions::from_mode(0o700),
                        );
                        if let Some(root) = parent.parent() {
                            let _ = std::fs::set_permissions(
                                root,
                                std::fs::Permissions::from_mode(0o700),
                            );
                        }
                    }
                }
                let dlid = {
                    let s = &mut self.sessions[si];
                    let id = s.next_xfer;
                    s.next_xfer += 1;
                    let _ = s.cmd_tx.send(UiCommand::Download {
                        id,
                        remote: src_path.clone(),
                        local: tmp.to_string_lossy().into_owned(),
                        policy: ConflictPolicy::Overwrite,
                    });
                    id
                };
                // 在目标会话预占一条「等待」上传占位行：源端下载期间 B 也有可见状态
                let up_id = {
                    let s = &mut self.sessions[di];
                    let id = s.next_xfer;
                    s.next_xfer += 1;
                    let mut t = Transfer::new(
                        id,
                        base.clone(),
                        crate::proto::TransferDir::Upload,
                        0,
                        None,
                        None,
                    );
                    t.queued = true;
                    t.note =
                        crate::i18n::tr("等待源端下载…", "Waiting for source download…").into();
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
                (true, crate::i18n::Lang::En) => {
                    format!("Cross-server move {n} (via local relay) …")
                }
                (false, crate::i18n::Lang::En) => {
                    format!("Cross-server copy {n} (via local relay) …")
                }
            };
        }
        // 剪切粘贴后清空剪贴板（复制保留，便于多次粘贴）
        if is_cut {
            self.xfer.file_clip = None;
        }
    }
}
