//! SSH 文件传输：从 ssh God Object 拆出，行为不变。

mod direct;
mod download;
mod upload;
mod util;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use russh::client::Handle;
use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{ConflictPolicy, WorkerEvent};

use super::auth::{open_sftp, ClientHandler};
use super::UiSink;

use direct::direct_transfer;
use download::download;
use upload::{upload, upload_from_mcp};

/// 同一会话同时进行的最大传输数（不同会话各自独立）。
pub(super) const MAX_CONCURRENT_XFER: usize = 6;

/// 待执行/进行中的传输任务描述。
pub(super) enum PendingXfer {
    Download {
        id: u64,
        remote: String,
        local: String,
        policy: ConflictPolicy,
    },
    Upload {
        id: u64,
        local: String,
        remote_dir: String,
        remote_name: Option<String>,
        policy: ConflictPolicy,
    },
    UploadFromMcp {
        id: u64,
        source: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        size: u64,
        remote_path: String,
    },
    /// 跨主机直传：在本（源）主机上 rsync/scp 直推到目标主机
    Direct(Box<crate::proto::DirectSpec>),
}

impl PendingXfer {
    pub(super) fn id(&self) -> u64 {
        match self {
            PendingXfer::Download { id, .. }
            | PendingXfer::Upload { id, .. }
            | PendingXfer::UploadFromMcp { id, .. } => *id,
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
    cancels.insert(
        id,
        XferCancel {
            flag: cancel.clone(),
            stop: Some(stop_tx),
        },
    );
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
                        PendingXfer::Download {
                            id,
                            remote,
                            local,
                            policy,
                        } => {
                            download(h.clone(), sftp, id, remote, local, policy, &s, cancel_work)
                                .await
                        }
                        PendingXfer::Upload {
                            id,
                            local,
                            remote_dir,
                            remote_name,
                            policy,
                        } => {
                            upload(
                                sftp.as_ref(),
                                id,
                                local,
                                remote_dir,
                                remote_name,
                                policy,
                                &s,
                                cancel_work,
                            )
                            .await
                        }
                        PendingXfer::UploadFromMcp {
                            id,
                            source,
                            size,
                            remote_path,
                        } => {
                            upload_from_mcp(
                                sftp.as_ref(), id, source, size, remote_path, &s, cancel_work,
                            )
                            .await
                        }
                        PendingXfer::Direct(spec) => {
                            direct_transfer(h.clone(), sftp, *spec, &s, cancel_work).await
                        }
                    }
                }
                Err(e) => s.send(WorkerEvent::TransferDone {
                    id,
                    ok: false,
                    message: match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("SFTP 不可用：{e}"),
                        crate::i18n::Lang::En => format!("SFTP unavailable: {e}"),
                    },
                    refresh_dir: None,
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
