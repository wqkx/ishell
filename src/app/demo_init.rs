//! Startup-only demo and screenshot fixture setup.

use crate::proto::{AuthMethod, ConnectConfig, UiCommand};

use super::util::lock_mutex;
use super::{App, EditorTab, ForwardEntry, ImageTab, ProcPopup, SaveState, Transfer};

impl App {
    pub(super) fn apply_demo_flags(&mut self, cc: &eframe::CreationContext<'_>) {
        // 自检：自动连接（格式 host|port|user|keypath），免去手动登录
        if let Ok(spec) = std::env::var("ISHELL_AUTOCONNECT") {
            let parts: Vec<&str> = spec.split('|').collect();
            if parts.len() == 4 {
                if let Ok(port) = parts[1].parse() {
                    self.connect_form.open = false;
                    self.spawn_session(ConnectConfig {
                        host: parts[0].into(),
                        port,
                        username: parts[2].into(),
                        auth: if parts[3] == "agent" {
                            AuthMethod::Agent
                        } else {
                            AuthMethod::KeyFile {
                                path: parts[3].into(),
                                passphrase: None,
                            }
                        },
                        label: String::new(),
                        // 自检：ISHELL_JUMP="host|port|user|key" 时经跳板机连接
                        jump: std::env::var("ISHELL_JUMP").ok().and_then(|s| {
                            let p: Vec<&str> = s.split('|').collect();
                            (p.len() == 4).then(|| crate::proto::JumpHost {
                                host: p[0].into(),
                                port: p[1].parse().unwrap_or(22),
                                username: p[2].into(),
                                auth: AuthMethod::KeyFile {
                                    path: p[3].into(),
                                    passphrase: None,
                                },
                            })
                        }),
                        forward_agent: false,
                        transport: crate::proto::Transport::Ssh,
                    });
                }
            }
        }

        // 自检：ISHELL_LOCAL=1 时启动即开一个本机 PTY 终端（用于本机会话的截图自检）
        if std::env::var("ISHELL_LOCAL").is_ok() {
            self.connect_form.open = false;
            self.spawn_session(ConnectConfig::local());
        }

        // 自检：直接打开新建表单（截图核对输入框样式）
        if std::env::var("ISHELL_DEMO_FORM").is_ok() {
            self.connect_form.open_form_for_demo();
        }
        // 自检：打开快速连接列表（截图核对导入按钮）
        if std::env::var("ISHELL_DEMO_CONN").is_ok() {
            self.connect_form.open_dialog();
        }
        if std::env::var("ISHELL_DEMO_IMPORT").is_ok() {
            self.connect_form.open_import_demo();
        }
        if std::env::var("ISHELL_DEMO_DELETE").is_ok() {
            self.connect_form.open_delete_demo();
        }
        if std::env::var("ISHELL_DEMO_LIST").is_ok() {
            self.connect_form.open_list_demo();
        }
        // 自检：注入演示编辑器内容（截图核对代码高亮 + 多标签）
        if std::env::var("ISHELL_DEMO_EDIT").is_ok() {
            if let Some((server, uid, tx)) = self
                .sessions
                .first()
                .map(|s| (s.title.clone(), s.uid, s.cmd_tx.clone()))
            {
                let code = "use std::io;\n\n// 示例：读取并打印\nfn main() {\n    let mut s = String::new();\n    io::stdin().read_line(&mut s).unwrap();\n    let n: i32 = s.trim().parse().unwrap_or(0);\n    for i in 0..n {\n        println!(\"line {}\", i);\n    }\n}\n".to_string();
                let t1 = self.alloc_editor_id();
                // 大文件（>1MB）→ 只读模式，核对「改为可编辑」按钮
                let big: String = (0..40000)
                    .map(|i| format!("{i}: the quick brown fox jumps over the lazy dog\n"))
                    .collect();
                let t2 = self.alloc_editor_id();
                let t3 = self.alloc_editor_id();
                let mut ed = lock_mutex(&self.editor_state);
                ed.tabs.push(EditorTab {
                    editor: crate::ui::editor::Editor::new("/home/e5-1/demo.rs".into(), code),
                    server: server.clone(),
                    uid,
                    cmd_tx: tx.clone(),
                    text_id: t1,
                    tid: 1,
                    load_id: None,
                    load_done: 0,
                    load_total: 0,
                    save: SaveState::Idle,
                    save_at: None,
                    save_done: 0,
                    save_total: 0,
                    save_done_at: None,
                    save_op: 0,
                    save_deadline: None,
                    tail_offset: u64::MAX,
                    tail_pending: false,
                    tail_last: 0.0,
                    doc: None,
                    tail_carry: Vec::new(),
                });
                let mut big_ed = crate::ui::editor::Editor::new("/var/log/huge.log".into(), big);
                big_ed.readonly = true; // 演示大文件默认只读
                ed.tabs.push(EditorTab {
                    editor: big_ed,
                    server: server.clone(),
                    uid,
                    cmd_tx: tx.clone(),
                    text_id: t2,
                    tid: 2,
                    load_id: None,
                    load_done: 0,
                    load_total: 0,
                    save: SaveState::Idle,
                    save_at: None,
                    save_done: 0,
                    save_total: 0,
                    save_done_at: None,
                    save_op: 0,
                    save_deadline: None,
                    tail_offset: u64::MAX,
                    tail_pending: false,
                    tail_last: 0.0,
                    doc: None,
                    tail_carry: Vec::new(),
                });
                ed.tabs.push(EditorTab {
                    editor: crate::ui::editor::Editor::new(
                        "/etc/hosts".into(),
                        "127.0.0.1 localhost\n::1 localhost\n".into(),
                    ),
                    server,
                    uid,
                    cmd_tx: tx,
                    text_id: t3,
                    tid: 3,
                    load_id: None,
                    load_done: 0,
                    load_total: 0,
                    save: SaveState::Idle,
                    save_at: None,
                    save_done: 0,
                    save_total: 0,
                    save_done_at: None,
                    save_op: 0,
                    save_deadline: None,
                    tail_offset: u64::MAX,
                    tail_pending: false,
                    tail_last: 0.0,
                    doc: None,
                    tail_carry: Vec::new(),
                });
                ed.active = 1; // 默认显示大文件标签
            }
        }

        // 自检：看图工具——合成一张彩色渐变图打开
        if std::env::var("ISHELL_DEMO_IMG").is_ok() {
            if let Some((server, uid)) = self.sessions.first().map(|s| (s.title.clone(), s.uid)) {
                let (w, h) = (240usize, 160usize);
                let mut px = vec![0u8; w * h * 4];
                for y in 0..h {
                    for x in 0..w {
                        let i = (y * w + x) * 4;
                        px[i] = (x * 255 / w) as u8;
                        px[i + 1] = (y * 255 / h) as u8;
                        px[i + 2] = 128;
                        px[i + 3] = 255;
                    }
                }
                let color = egui::ColorImage::from_rgba_unmultiplied([w, h], &px);
                let tex = cc
                    .egui_ctx
                    .load_texture("demo_img", color, egui::TextureOptions::LINEAR);
                let mut data = Vec::new();
                if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, px) {
                    let _ = image::DynamicImage::ImageRgba8(buf).write_to(
                        &mut std::io::Cursor::new(&mut data),
                        image::ImageFormat::Png,
                    );
                }
                self.image.tabs.push(ImageTab {
                    server,
                    uid,
                    path: "/home/e5-1/pic/gradient.png".into(),
                    tex,
                    data,
                    size: egui::vec2(w as f32, h as f32),
                    zoom: 0.0,
                    offset: egui::Vec2::ZERO,
                });
            }
        }

        // 自检：自动建立一条本地转发（127.0.0.1:18022 → 127.0.0.1:22）
        if std::env::var("ISHELL_DEMO_FORWARD").is_ok() {
            use crate::proto::{ForwardKind, ForwardSpec};
            if let Some(s) = self.sessions.first_mut() {
                let id = s.next_forward;
                s.next_forward += 1;
                s.forwards.push(ForwardEntry {
                    id,
                    label: "127.0.0.1:18022 → 127.0.0.1:22".into(),
                    status: crate::i18n::tr("启动中 …", "Starting …").into(),
                    ok: true,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 18022,
                    kind: ForwardKind::Local {
                        remote_host: "127.0.0.1".into(),
                        remote_port: 22,
                    },
                });
                let _ = s.cmd_tx.send(UiCommand::AddForward(ForwardSpec {
                    id,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 18022,
                    kind: ForwardKind::Local {
                        remote_host: "127.0.0.1".into(),
                        remote_port: 22,
                    },
                }));
            }
            self.fwd.show = true;
        }

        // 自检：进程详情小窗
        if std::env::var("ISHELL_DEMO_PROC").is_ok() {
            self.popups.proc = Some(ProcPopup {
                pid: 1234,
                name: "gromacs_mpi".into(),
                cpu: 98.5,
                mem: 12.3,
                pos: egui::pos2(150.0, 300.0),
                cmd: "/opt/gromacs/bin/gmx mdrun -deffnm md -nb gpu".into(),
                cwd: "/home/e5-1/sim/run1".into(),
                exe: "/opt/gromacs/bin/gmx".into(),
                copied_t: None,
                confirm_kill: false,
                uid: self.sessions.first().map(|s| s.uid).unwrap_or(0),
            });
        }

        // 自检：生成多个标签，核对溢出渐隐 + 固定的「新建」按钮
        if std::env::var("ISHELL_DEMO_TABS").is_ok() {
            for n in 1..=12 {
                self.spawn_session(ConnectConfig {
                    host: "127.0.0.1".into(),
                    port: 9,
                    username: format!("srv-{n:02}"),
                    auth: AuthMethod::Password(String::new()),
                    label: String::new(),
                    jump: None,
                    forward_agent: false,
                    transport: crate::proto::Transport::Ssh,
                });
            }
        }

        // 自检：命令广播栏
        if std::env::var("ISHELL_DEMO_BCAST").is_ok() {
            self.show_broadcast = true;
            self.broadcast_input = "systemctl status nginx".into();
        }

        // 自检：显示退出确认框
        if std::env::var("ISHELL_DEMO_CLOSE").is_ok() {
            self.show_close_confirm = true;
        }

        // 自检：注入演示传输条目，便于截图核对传输浮窗
        if std::env::var("ISHELL_DEMO_XFER").is_ok() {
            if let Some(s) = self.sessions.first_mut() {
                use crate::proto::TransferDir::*;
                let mut demo = |id, name: &str, dir, done, total, ok, local: Option<String>| {
                    let mut t = Transfer::new(id, name.into(), dir, total, local, None);
                    t.done = done;
                    t.ok = ok;
                    s.transfers.push(t);
                };
                demo(
                    1,
                    "backup.tar.gz",
                    Download,
                    73_400_320,
                    104_857_600,
                    None,
                    None,
                );
                demo(2, "deploy.sh", Upload, 2048, 2048, Some(true), None);
                demo(
                    3,
                    "huge.bin",
                    Download,
                    1024,
                    2048,
                    Some(true),
                    Some("/root/Downloads/huge.bin".into()),
                );
                // 自检：再塞一批，验证滚动
                for i in 4..16u64 {
                    demo(
                        i,
                        &format!("file_{i}.dat"),
                        Download,
                        i * 1000,
                        20000,
                        if i % 3 == 0 { Some(true) } else { None },
                        None,
                    );
                }
            }
            self.show_transfers = true;
        }
    }
}
