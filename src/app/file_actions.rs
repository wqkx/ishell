//! 文件面板动作分发：从 App God Object 拆出，行为不变。
//! 均为 `impl App` 方法，签名与调用点不变。

use crate::proto::UiCommand;
use crate::ui::file_panel::FileAction;

use super::util::lock_mutex;
use super::{App, Transfer, XferSpec};

impl App {
    /// 翻译文件面板动作为 SFTP 指令或剪贴板操作。
    pub(super) fn handle_file_action(&mut self, idx: usize, action: FileAction) {
        let policy = self.conflict_policy;
        // 剪贴板 / 粘贴需同时访问 App 级剪贴板与会话信息，单独前置处理
        match action {
            FileAction::ClipCopy { items } => return self.set_clip(idx, items, false),
            FileAction::ClipCut { items } => return self.set_clip(idx, items, true),
            FileAction::Paste { dest_dir } => return self.start_paste(idx, dest_dir),
            _ => {}
        }
        // 大文件并发上限：须在借 session 之前检查（避免与 editor_state 借用冲突）
        if let FileAction::OpenFile { force: true, .. } = &action {
            let large_n = {
                let ed = lock_mutex(&self.editor_state);
                ed.tabs
                    .iter()
                    .filter(|t| t.editor.content.len() > crate::limits::LARGE_FILE_BYTES)
                    .count()
            };
            if large_n >= crate::limits::MAX_LARGE_TABS {
                let msg = match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!(
                        "已打开过多大文件（上限 {}），请先关闭后再打开",
                        crate::limits::MAX_LARGE_TABS
                    ),
                    crate::i18n::Lang::En => format!(
                        "Too many large files open (max {}); close one first",
                        crate::limits::MAX_LARGE_TABS
                    ),
                };
                if let Some(s) = self.sessions.get_mut(idx) {
                    s.status = msg.clone();
                }
                self.toast = Some((msg, self.ctx.input(|i| i.time)));
                return;
            }
        }
        let Some(s) = self.sessions.get_mut(idx) else {
            return;
        };
        match action {
            FileAction::List(path) => {
                let p = if path == "~" || path.is_empty() {
                    ".".into()
                } else {
                    path
                };
                let _ = s.cmd_tx.send(crate::proto::list_dir_cmd(p));
            }
            FileAction::Download(remote) => {
                let name = remote.rsplit('/').next().unwrap_or("download").to_string();
                let local = self.download_dir.join(&name).to_string_lossy().into_owned();
                // 去重：同一远端文件 → 同一本地路径 的任务已存在时复用它，避免重复任务，
                // 也顺带恢复此前失败/中断的同一传输（复用 id：worker 据本地已有分段续传，
                // 并通过覆盖取消句柄停掉可能仍在后台运行的旧任务）。
                let existing = s.transfers.iter().find(|t| {
                    t.ok != Some(true) // 已成功完成的不复用（本地可能已变，避免按旧偏移续传出错）
                        && matches!(&t.spec, Some(XferSpec::Download { remote: r, local: l }) if *r == remote && *l == local)
                }).map(|t| (t.id, t.ok, t.paused));
                match existing {
                    Some((_, None, false)) => {
                        s.status = match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{name} 正在下载中"),
                            crate::i18n::Lang::En => format!("{name} is already downloading"),
                        };
                    }
                    Some((id, _, _)) => {
                        let _ = s.cmd_tx.send(UiCommand::Download {
                            id,
                            remote,
                            local,
                            policy,
                        });
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None;
                            t.paused = false;
                            t.show_err = false;
                            t.speed = 0.0;
                            t.last_done = 0;
                            t.last_t = None;
                            t.message = crate::i18n::tr("重新下载 …", "Re-downloading …").into();
                        }
                    }
                    None => {
                        let id = s.next_xfer;
                        s.next_xfer += 1;
                        s.transfers.push(Transfer::new(
                            id,
                            name,
                            crate::proto::TransferDir::Download,
                            0,
                            Some(local.clone()),
                            Some(XferSpec::Download {
                                remote: remote.clone(),
                                local: local.clone(),
                            }),
                        ));
                        let _ = s.cmd_tx.send(UiCommand::Download {
                            id,
                            remote,
                            local,
                            policy,
                        });
                    }
                }
                self.show_transfers = true;
                self.xfer_just_opened = true;
            }
            FileAction::Upload { local, remote_dir } => {
                // 同时按 / 和 \ 取名，兼容 Windows 路径（否则显示带盘符的整段路径）
                let name = local
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or("upload")
                    .to_string();
                // 去重：同一本地文件 → 同一远端目录 的任务已存在时复用它（理由同 Download）。
                // 这是「上传中途失败/中断后再次上传出现两个任务、旧任务又续传」的根因修复。
                let existing = s.transfers.iter().find(|t| {
                    t.ok != Some(true) // 已成功完成的不复用（本地可能已变，避免按旧偏移续传出错）
                        && matches!(&t.spec, Some(XferSpec::Upload { local: l, remote_dir: r }) if *l == local && *r == remote_dir)
                }).map(|t| (t.id, t.ok, t.paused));
                match existing {
                    Some((_, None, false)) => {
                        // 已在进行中：忽略重复请求，仅提示并打开传输窗
                        s.status = match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("{name} 正在上传中"),
                            crate::i18n::Lang::En => format!("{name} is already uploading"),
                        };
                    }
                    Some((id, _, _)) => {
                        // 失败/中断/已完成：复用该任务重新上传（同 id，自动续传/覆盖）
                        let _ = s.cmd_tx.send(UiCommand::Upload {
                            id,
                            local,
                            remote_dir,
                            remote_name: None,
                            policy,
                        });
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None;
                            t.paused = false;
                            t.show_err = false;
                            t.speed = 0.0;
                            t.last_done = 0;
                            t.last_t = None;
                            t.message = crate::i18n::tr("重新上传 …", "Re-uploading …").into();
                        }
                    }
                    None => {
                        let id = s.next_xfer;
                        s.next_xfer += 1;
                        s.transfers.push(Transfer::new(
                            id,
                            name,
                            crate::proto::TransferDir::Upload,
                            0,
                            None,
                            Some(XferSpec::Upload {
                                local: local.clone(),
                                remote_dir: remote_dir.clone(),
                            }),
                        ));
                        let _ = s.cmd_tx.send(UiCommand::Upload {
                            id,
                            local,
                            remote_dir,
                            remote_name: None,
                            policy,
                        });
                    }
                }
                self.show_transfers = true;
                self.xfer_just_opened = true;
            }
            FileAction::Mkdir(path) => {
                let _ = s.cmd_tx.send(UiCommand::Mkdir(path));
            }
            FileAction::CreateFile(path) => {
                let _ = s.cmd_tx.send(UiCommand::CreateFile(path));
            }
            FileAction::Chmod { path, mode } => {
                let _ = s.cmd_tx.send(UiCommand::Chmod { path, mode });
            }
            FileAction::DeleteMany(paths) => {
                let _ = s.cmd_tx.send(UiCommand::DeleteMany { paths });
            }
            FileAction::Rename { from, to } => {
                let _ = s.cmd_tx.send(UiCommand::Rename { from, to });
            }
            FileAction::CopyPath(p) => {
                self.ctx.copy_text(p.clone());
                s.status = match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("已复制路径：{p}"),
                    crate::i18n::Lang::En => format!("Copied: {p}"),
                };
            }
            FileAction::OpenFile { path, force } => {
                s.status = match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("打开中：{path} …"),
                    crate::i18n::Lang::En => format!("Opening: {path} …"),
                };
                let id = s.next_xfer;
                s.next_xfer += 1;
                // 立即建占位标签（显示文件名 + 进度条），下载完成后由 FileOpened 填充内容
                s.pending.placeholder.push((id, path.clone()));
                let _ = s.cmd_tx.send(UiCommand::ReadFile { id, path, force });
            }
            FileAction::OpenImage { path } => {
                s.status = match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("打开中：{path} …"),
                    crate::i18n::Lang::En => format!("Opening: {path} …"),
                };
                let _ = s.cmd_tx.send(UiCommand::ReadImage { path });
            }
            FileAction::OpenPdf { path } => {
                // 与文本打开同构：先建占位标签（珊瑚线进度条），PdfInfo 就位后填充 PDF 视图
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.pending.placeholder.push((id, path.clone()));
                let _ = s.cmd_tx.send(UiCommand::PdfInfo { id, path });
            }
            FileAction::OpenDocx { path } => {
                let id = s.next_xfer;
                s.next_xfer += 1;
                s.pending.placeholder.push((id, path.clone()));
                let _ = s.cmd_tx.send(UiCommand::ReadDoc { id, path });
            }
            FileAction::Move { srcs, dest_dir } => {
                // 同会话内拖拽移动：直接走远端 mv（CopyMove 的 do_move 分支）
                let n = srcs.len();
                let _ = s.cmd_tx.send(UiCommand::CopyMove {
                    srcs,
                    dest_dir,
                    do_move: true,
                });
                s.status = match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("移动 {n} 项 …"),
                    crate::i18n::Lang::En => format!("Moving {n} item(s) …"),
                };
            }
            FileAction::Status(msg) => {
                // 状态栏留底 + 顶部醒目浮层（撤销等操作需要明确反馈，避免误操作）
                let now = self.ctx.input(|i| i.time);
                s.status = msg.clone();
                self.toast = Some((msg, now));
            }
            FileAction::CdTerminal(path) => {
                // 以 POSIX 单引号转义路径后在终端 cd，并聚焦终端
                // ai_owned 会话是只读的（AI 专用），这个入口不能往里面灌命令
                if !s.ai_owned {
                    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
                    let _ = s.cmd_tx.send(UiCommand::TerminalInput(
                        format!("cd {quoted}\r").into_bytes(),
                    ));
                    s.terminal.request_focus();
                }
            }
            // 已在函数开头前置处理并 return，此处仅为穷尽匹配
            FileAction::ClipCopy { .. } | FileAction::ClipCut { .. } | FileAction::Paste { .. } => {
            }
        }
    }
}
