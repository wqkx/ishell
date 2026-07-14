use tokio::sync::mpsc::UnboundedSender;

use super::{lock_mutex, pending, App, ForwardEntry, KbdPrompt, Transfer};
use crate::proto::{ConnectConfig, UiCommand, WorkerEvent};
use crate::ssh::{self, UiSink};
use crate::terminal::Terminal;
use crate::ui::file_panel::FilePanelState;
use crate::ui::sidebar::NetHistory;

/// 单个 SSH 会话的前台状态。
pub(super) struct Session {
    /// 稳定唯一 id（用于标签滑动动画在重排后仍追踪同一标签）
    pub(super) uid: u64,
    pub(super) title: String,
    /// 悬停提示（user@host，用于标签去掉 IP 后的消歧）
    pub(super) tip: String,
    pub(super) cmd_tx: UnboundedSender<UiCommand>,
    pub(super) evt_rx: std::sync::mpsc::Receiver<WorkerEvent>,
    /// 系统信息快照（独立 watch 通道，只保留最新一份，见 `UiSink::send_sysinfo`）
    pub(super) sysinfo_rx: tokio::sync::watch::Receiver<Option<crate::proto::SysInfo>>,
    pub(super) connected: bool,
    pub(super) status: String,
    pub(super) terminal: Terminal,
    pub(super) sysinfo: Option<crate::proto::SysInfo>,
    pub(super) net_hist: NetHistory,
    pub(super) files: FilePanelState,
    pub(super) last_size: (u16, u16),
    pub(super) initialized: bool,
    pub(super) transfers: Vec<Transfer>,
    pub(super) next_xfer: u64,
    /// 侧栏网络监控选中的网卡（空 = 全部）
    pub(super) selected_nic: String,
    /// 进程列表是否按内存排序（false = 按 CPU）
    pub(super) proc_sort_mem: bool,
    /// worker 事件缓冲（打开/保存/PDF/图片等），由 App 帧循环 drain
    pub(super) pending: pending::SessionPending,
    /// 向 worker 回复"是否信任未知主机"
    pub(super) hostkey_tx: UnboundedSender<bool>,
    /// 待确认的主机（host, 指纹, 是否为密钥变更）
    pub(super) pending_hostkey: Option<(String, String, bool)>,
    /// 待回答的键盘交互认证提示（None = 无）
    pub(super) kbd_prompt: Option<KbdPrompt>,
    /// 端口转发列表
    pub(super) forwards: Vec<ForwardEntry>,
    pub(super) next_forward: u64,
    /// 进程详情返回（pid, cmd, cwd, exe），由 App 取用后清空
    pub(super) proc_detail: Option<(u32, String, String, String)>,
    /// 连接配置（用于断线重连）
    pub(super) cfg: ConnectConfig,
    /// 是否曾成功连接（仅对掉线的会话自动重连，避免错误配置死循环）
    pub(super) was_connected: bool,
    /// 计划在此刻自动重连
    pub(super) reconnect_at: Option<std::time::Instant>,
    /// 已自动重连次数
    pub(super) reconnect_tries: u32,
    /// 由 OSC 7 记录的终端工作目录（断线重连后用于 cd 恢复）
    pub(super) last_cwd: String,
    /// 重连后待恢复 cwd
    pub(super) restore_cwd: bool,
    /// 待弹出「注入 OSC 7」确认框（右键功能在无 cwd 时触发）
    pub(super) osc7_confirm: bool,
    /// 已注入、等下个提示符上报 cwd 后把文件区跳过去
    pub(super) osc7_pending_reveal: bool,
    /// 远端是否支持 /proc 系统监控（None=尚未探测；false 时侧栏提示并跳过杀进程等）
    pub(super) monitor_ok: Option<bool>,
    /// AI/MCP 控制通道正在等待完成的一次命令运行（同一会话同一时刻只允许一条）
    pub(super) pending_ai_run: Option<super::PendingAiRun>,
    /// 是否由 AI 通过 `open_session` 新开（只读：用户键盘输入不会发给这个会话，只能看不能敲）
    pub(super) ai_owned: bool,
    /// AI/MCP 控制通道正在等待完成的一次文件读写（write_file/read_file，同一会话同一时刻
    /// 只允许一个）
    pub(super) pending_file_op: Option<super::PendingAiFileOp>,
    /// 最近因超时被清理的文件操作 id（"墓碑"）：worker 侧的 SFTP 操作本身没法取消，超时后
    /// 姗姗来迟的真实完成事件如果落到这里，必须直接丢弃——不能因为 pending_file_op 已经是
    /// None 就被误当成"普通编辑器操作"路由过去（可能凭空建一个用户没开过的编辑器标签，
    /// 或者把无关标签的保存状态搅乱）。有界环形缓冲，避免无限增长。
    pub(super) file_op_tombstones: std::collections::VecDeque<u64>,
    /// `copy_file`（本地→远端方向）为改名借用的临时符号链接目录：op_id -> 临时目录路径。
    /// 传输真正结束（`TransferDone`，见 session_events.rs）时删除；与 `pending_file_op`/
    /// 墓碑机制各自独立，保证哪怕响应已经因超时提前发出，临时目录也总会在传输实际完成后清理。
    pub(super) copy_tmp_dirs: std::collections::HashMap<u64, std::path::PathBuf>,
}

/// 传输的重发规格（断线重连/手动重试时据此重新发起，底层自动续传）。
#[derive(Clone)]
pub(super) enum XferSpec {
    Download { remote: String, local: String },
    Upload { local: String, remote_dir: String },
}

impl Session {
    pub(super) fn refresh_dir(&mut self, dir: Option<String>) {
        if let Some(dir) = dir {
            self.files.loading.insert(dir.clone());
            let _ = self.cmd_tx.send(UiCommand::ListDir(dir));
        }
    }

    /// 连接成功后初始化文件树：根 "/"，并定位到家目录。
    pub(super) fn init_files(&mut self) {
        self.files.root = "/".into();
        self.files.expanded.insert("/".into());
        // 只请求 "."（服务端解析为家目录）作为 cwd；树的其余层级由 sync_tree 自动补全。
        // 不预先请求 "/"，避免它先返回把 cwd 设成根目录。
        let _ = self.cmd_tx.send(UiCommand::ListDir(".".into()));
    }
}

impl App {
    /// 根据配置建立一个新会话（spawn worker）。
    /// 分配一个唯一的编辑器 TextEdit Id。
    pub(super) fn alloc_editor_id(&mut self) -> egui::Id {
        let id = egui::Id::new(("ed_txt", self.next_editor_id));
        self.next_editor_id += 1;
        id
    }

    /// 创建通道并在运行时启动一个 worker，返回 (cmd_tx, evt_rx, hostkey_tx)。
    fn spawn_worker(
        &self,
        cfg: ConnectConfig,
    ) -> (
        UnboundedSender<UiCommand>,
        std::sync::mpsc::Receiver<WorkerEvent>,
        tokio::sync::watch::Receiver<Option<crate::proto::SysInfo>>,
        UnboundedSender<bool>,
    ) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let (sysinfo_tx, sysinfo_rx) = tokio::sync::watch::channel(None);
        let (hostkey_tx, hostkey_rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = UiSink::new(evt_tx, self.ctx.clone(), std::sync::Arc::new(sysinfo_tx));
        self.runtime.spawn(ssh::run(cfg, cmd_rx, sink, hostkey_rx));
        (cmd_tx, evt_rx, sysinfo_rx, hostkey_tx)
    }

    pub(super) fn spawn_session(&mut self, cfg: ConnectConfig) {
        self.show_close_confirm = false; // 新建会话则取消退出提示
        let (cmd_tx, evt_rx, sysinfo_rx, hostkey_tx) = self.spawn_worker(cfg.clone());

        self.next_uid += 1;
        self.sessions.push(Session {
            uid: self.next_uid,
            title: if cfg.label.trim().is_empty() {
                cfg.username.clone()
            } else {
                cfg.label.trim().to_string()
            },
            tip: format!("{}@{}:{}", cfg.username, cfg.host, cfg.port),
            cmd_tx,
            evt_rx,
            sysinfo_rx,
            connected: false,
            status: crate::i18n::tr("连接中 …", "Connecting …").into(),
            terminal: Terminal::new(),
            sysinfo: None,
            net_hist: NetHistory::default(),
            files: {
                let key = format!("{}@{}:{}", cfg.username, cfg.host, cfg.port);
                FilePanelState {
                    favorites: crate::store::load_favorites(&key),
                    server_key: key,
                    ..Default::default()
                }
            },
            last_size: (0, 0),
            initialized: false,
            transfers: Vec::new(),
            next_xfer: 1,
            selected_nic: String::new(),
            proc_sort_mem: false,
            pending: pending::SessionPending::default(),
            hostkey_tx,
            pending_hostkey: None,
            kbd_prompt: None,
            forwards: Vec::new(),
            next_forward: 1,
            proc_detail: None,
            cfg,
            was_connected: false,
            reconnect_at: None,
            reconnect_tries: 0,
            last_cwd: String::new(),
            restore_cwd: false,
            osc7_confirm: false,
            osc7_pending_reveal: false,
            monitor_ok: None,
            pending_ai_run: None,
            ai_owned: false,
            pending_file_op: None,
            file_op_tombstones: std::collections::VecDeque::new(),
            copy_tmp_dirs: std::collections::HashMap::new(),
        });
        self.active = Some(self.sessions.len() - 1);
        self.tabbar.scroll_to_active = true; // 新建标签后滚动到可视区
    }

    /// 重连指定会话：用原配置重启 worker，重置连接相关状态，保留标签/目录等。
    pub(super) fn reconnect_session(&mut self, idx: usize) {
        let Some(s) = self.sessions.get(idx) else {
            return;
        };
        let cfg = s.cfg.clone();
        let (cmd_tx, evt_rx, sysinfo_rx, hostkey_tx) = self.spawn_worker(cfg);
        let Some(s) = self.sessions.get_mut(idx) else {
            return;
        };
        let uid = s.uid;
        s.cmd_tx = cmd_tx.clone();
        s.evt_rx = evt_rx;
        s.sysinfo_rx = sysinfo_rx;
        s.hostkey_tx = hostkey_tx;
        s.connected = false;
        s.initialized = false;
        s.terminal = Terminal::new();
        s.sysinfo = None;
        s.monitor_ok = None;
        s.pending_ai_run = None; // worker 已重启：旧的 AI 命令等待作废（对端 oneshot 断线会收到错误）
        s.pending_file_op = None; // 同上：旧的 write_file/read_file 等待也一并作废
        // worker 已重启，旧连接上那些还在跑的传输任务已经跟丢了，对应的临时改名目录不会再
        // 收到 TransferDone 来触发清理——这里直接清掉，避免残留在系统临时目录里。
        for dir in s.copy_tmp_dirs.drain().map(|(_, d)| d) {
            let _ = std::fs::remove_dir_all(&dir);
        }
        // M3：保留端口转发（不再 clear），标记「重连中」；Connected 事件里用新 worker 重建
        for f in &mut s.forwards {
            f.ok = true;
            f.status = crate::i18n::tr("重连中 …", "Reconnecting …").into();
        }
        s.pending_hostkey = None;
        s.kbd_prompt = None;
        s.reconnect_at = None;
        s.restore_cwd = true; // 重连成功后尝试 cd 回 last_cwd（保留不清空）
        s.status = crate::i18n::tr("重连中 …", "Reconnecting …").into();
        // M1：刷新该会话已打开编辑器标签的 cmd_tx——旧句柄随 worker 失效，否则重连后保存静默丢失。
        {
            let mut es = lock_mutex(&self.editor_state);
            for t in es.tabs.iter_mut().filter(|t| t.uid == uid) {
                t.cmd_tx = cmd_tx.clone();
            }
        }
    }

    /// 拖动排序：把会话从 `from` 移动到放置目标 `to` 处。
    pub(super) fn reorder_session(&mut self, from: usize, to: usize) {
        let len = self.sessions.len();
        if from >= len || to >= len || from == to {
            return;
        }
        let moved = self.sessions.remove(from);
        // 让被拖动标签落在放置目标的原始位置 `to`（双向一致，避免相邻正向拖动变成空操作）
        let dest = to.min(self.sessions.len());
        self.sessions.insert(dest, moved);
        // 重算当前激活索引
        self.active = self.active.map(|a| {
            if a == from {
                dest
            } else {
                let mut x = a;
                if a > from {
                    x -= 1;
                }
                if x >= dest {
                    x += 1;
                }
                x
            }
        });
    }

    pub(super) fn close_session(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        let s = self.sessions.remove(idx);
        for dir in s.copy_tmp_dirs.values() {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = s.cmd_tx.send(UiCommand::Disconnect);
        if self.sessions.is_empty() {
            self.active = None;
        } else {
            // 据「关闭项」与「当前 active」的相对位置正确调整，避免关闭非激活标签时误切会话：
            // 关在 active 左侧 → active 左移一位；关在右侧 → 不变；关的正是 active（或无 active）→ 落到邻近项。
            let new_len = self.sessions.len();
            self.active = Some(match self.active {
                Some(a) if a > idx => a - 1,
                Some(a) if a < idx => a,
                _ => idx.min(new_len - 1),
            });
        }
    }

    /// 切换会话标签（delta=+1 下一个 / -1 上一个，循环）。
    pub(super) fn switch_tab(&mut self, delta: i32) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let cur = self.active.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        self.active = Some(next);
        self.tabbar.scroll_to_active = true; // 切换后滚动到可视区
        if let Some(s) = self.sessions.get_mut(next) {
            s.terminal.request_focus();
        }
    }

    pub(super) fn session_idx_by_uid(&self, uid: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.uid == uid)
    }

    /// 与指定会话「同一台服务器」（host:port 相同）的所有会话下标，活动会话排在最前。
    /// 用于把多个标签页对同一服务器的传输任务汇总到同一个传输列表里。
    pub(super) fn same_server_idxs(&self, idx: usize) -> Vec<usize> {
        let Some(base) = self.sessions.get(idx) else {
            return Vec::new();
        };
        let (host, port) = (base.cfg.host.clone(), base.cfg.port);
        let mut out = vec![idx];
        for (i, s) in self.sessions.iter().enumerate() {
            if i != idx && s.cfg.host == host && s.cfg.port == port {
                out.push(i);
            }
        }
        out
    }
}
