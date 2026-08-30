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
    /// 「等 shell 闲下来再 cd 回去」的截止时刻。到点还没等到闲置窗口就放弃——不能让这个
    /// 意图无限期挂着，几分钟后突然往用户的 shell 里敲一行 `cd`。见 `cwd_restore_decision`。
    pub(super) restore_cwd_until: Option<std::time::Instant>,
    /// 待弹出「注入 OSC 7」确认框（右键功能在无 cwd 时触发）
    pub(super) osc7_confirm: bool,
    /// 已注入、等下个提示符上报 cwd 后把文件区跳过去
    pub(super) osc7_pending_reveal: bool,
    /// 「在终端打开当前目录」忙碌拦截的强制窗口：首次点击若终端疑似在跑任务则 toast 拦截，
    /// 在此刻之前再次点击视为用户确认、强制注入 cd（应对任务已停但判定信号未消失的误拦）
    pub(super) cd_force_until: Option<std::time::Instant>,
    /// 本会话是否已注入过 MCP 配对 token（export ISHELL_MCP_TOKEN）。断线重连后远端是
    /// 新 shell（env 已丢），Connected 时复位以便重新注入。
    pub(super) mcp_token_injected: bool,
    /// 远端是否支持 /proc 系统监控（None=尚未探测；false 时侧栏提示并跳过杀进程等）
    pub(super) monitor_ok: Option<bool>,
    /// AI/MCP 控制通道正在等待完成的一次命令运行（同一会话同一时刻只允许一条）
    pub(super) pending_ai_run: Option<super::PendingAiRun>,
    /// 「粘贴图片」正在上传的传输 id → 传完要打进终端的**远端路径**。
    /// 见 [`Session::paste_image`]。
    pub(super) pending_paste_image: std::collections::HashMap<u64, String>,
    /// 是否由 AI 通过 `open_session` 新开（只读：用户键盘输入不会发给这个会话，只能看不能敲）
    pub(super) ai_owned: bool,
    /// AI/MCP 控制通道正在等待完成的文件读写（write_file/read_file/copy_to_remote/
    /// copy_from_remote/copy_between_sessions）。允许同一会话同时挂多个——SFTP 天然支持
    /// 并发，worker 侧也已有 `MAX_CONCURRENT_XFER` 并发+排队，所以这里放成一个列表让 AI
    /// 能一次并行发起多文件传输；用 `op_id` 区分各条，上限见 `MAX_CONCURRENT_FILE_OPS`。
    pub(super) pending_file_ops: Vec<super::PendingAiFileOp>,
    /// 最近因超时被清理的文件操作 id（"墓碑"）：worker 侧的 SFTP 操作本身没法取消，超时后
    /// 姗姗来迟的真实完成事件如果落到这里，必须直接丢弃——不能因为 pending_file_op 已经是
    /// None 就被误当成"普通编辑器操作"路由过去（可能凭空建一个用户没开过的编辑器标签，
    /// 或者把无关标签的保存状态搅乱）。有界环形缓冲，避免无限增长。
    pub(super) file_op_tombstones: std::collections::VecDeque<u64>,
}

/// 传输的重发规格（断线重连/手动重试时据此重新发起，底层自动续传）。
#[derive(Clone)]
pub(super) enum XferSpec {
    Download { remote: String, local: String },
    Upload { local: String, remote_dir: String },
}

/// 把一个路径变成可以直接打进 shell 的一个词。**不需要引号时原样返回**——绝大多数远端路径
/// 都是干净的，无谓加引号只会让用户看着别扭。
///
/// `windows_style`：目标 shell 是 cmd/PowerShell（只有「本机」会话且 iShell 跑在 Windows 上
/// 才成立）。那两个 shell 不认 POSIX 的单引号转义，用双引号；而 Windows 路径里不可能出现
/// `"`（文件名非法字符），所以双引号里不需要再转义什么。
fn quote_shell_arg(path: &str, windows_style: bool) -> String {
    let needs = path.is_empty()
        || path
            .chars()
            .any(|c| !(c.is_alphanumeric() || "/._-:\\~+=,@".contains(c)));
    if !needs {
        return path.to_string();
    }
    if windows_style {
        format!("\"{}\"", path.replace('"', ""))
    } else {
        // POSIX：单引号里除了单引号本身什么都不用转义
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// 恢复工作目录这一步该怎么走。
///
/// 抽成纯函数是因为这里唯一容易写错的是**放弃的条件**：意图挂着不放，就会在几分钟后突然
/// 往用户的 shell 里敲一行 `cd`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CwdRestore {
    /// 还没等到安全窗口，下一帧再看。
    Wait,
    /// 现在可以注入。
    Inject,
    /// 放弃：用户已经自己动手了，或者等太久了。
    GiveUp,
}

/// - `never_typed`：本次连接以来用户一个键都没敲过。敲过就说明他已经在用这个 shell 了，
///   此时替他 `cd` 只会打断他——直接放弃，别再等。
/// - `idle`：终端不忙、远端输出与用户输入都静止了一段时间（与 MCP 配对标识注入同一套判据）。
/// - `expired`：等过了截止时刻。
pub(super) fn cwd_restore_decision(never_typed: bool, idle: bool, expired: bool) -> CwdRestore {
    if !never_typed {
        return CwdRestore::GiveUp;
    }
    if idle {
        return CwdRestore::Inject;
    }
    if expired {
        return CwdRestore::GiveUp;
    }
    CwdRestore::Wait
}

impl Session {
    /// 现在往这个 shell 里替用户敲一行东西安全吗——「提示符几乎确定闲着」的那套判据。
    ///
    /// 与 MCP 配对标识的自动注入共用同一个函数**是有意的**：两处干的是同一件事（程序替用户
    /// 敲键盘），判据分成两份迟早会漂移，而漂移的后果是往某个正在等输入的程序（sudo/ssh 的
    /// 密码提示符）里打字。
    pub(super) fn shell_idle_for_injection(&self) -> bool {
        const QUIET: std::time::Duration = std::time::Duration::from_secs(2);
        self.connected
            && !self.ai_owned
            && self.pending_ai_run.is_none()
            && !self.terminal.ai_capture_pending()
            && !self.terminal.appears_busy()
            && self.terminal.output_idle_for(QUIET)
            && self.terminal.input_idle_for(QUIET)
    }

    /// 用户往终端里粘了一张图（Ctrl+V / 右键粘贴，剪贴板里是图片而不是文本）。
    ///
    /// 终端协议里没有「贴一张图」这回事，字节流塞不进去。能落地的做法只有一条：把图片
    /// **变成一个文件**，再把**路径**打进终端——Claude Code / Codex 这类 AI CLI 认路径，
    /// 会自己去读那个文件。
    ///
    /// 为什么必须走「上传」而不能只给本机路径：用户的 AI CLI 常常跑在**远端**服务器上
    /// （经 iShell 的 SSH 会话），远端进程既读不到本机的 `/tmp`，也读不到本机剪贴板——
    /// 这正是「Ctrl+V 贴图在 iShell 里没反应」的深层原因，光把按键透传过去也解决不了。
    /// 落到远端 `/tmp` 而不是家目录或当前目录：`/tmp` 一定存在、一定可写，不用先 mkdir，
    /// 也不会往用户正在干活的目录里丢文件。
    pub(super) fn paste_image(&mut self, png: Vec<u8>) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let name = format!("ishell-paste-{stamp}.png");
        let local = std::env::temp_dir().join(&name);
        // 截图很可能带敏感内容，别让同机的其他账号顺手看到（默认 umask 通常是 644）。
        if let Err(e) = std::fs::write(&local, &png).and_then(|()| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }) {
            self.status = match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!("粘贴的图片写入本地临时文件失败：{e}"),
                crate::i18n::Lang::En => format!("Could not save the pasted image locally: {e}"),
            };
            return;
        }
        let local = local.to_string_lossy().into_owned();
        // 本机会话：进程就在这台机器上，直接给本地路径，不用上传。
        if self.cfg.is_local() {
            self.type_pasted_path(&local);
            return;
        }
        let remote = format!("/tmp/{name}");
        let id = self.next_xfer;
        self.next_xfer += 1;
        self.transfers.push(Transfer::new(
            id,
            name.clone(),
            crate::proto::TransferDir::Upload,
            png.len() as u64,
            Some(local.clone()),
            // 不给 spec：断线重连时不该自动重传一张早已过期的剪贴板临时图。
            None,
        ));
        let _ = self.cmd_tx.send(crate::proto::UiCommand::Upload {
            id,
            local,
            remote_dir: "/tmp".into(),
            remote_name: Some(name),
            policy: crate::proto::ConflictPolicy::Overwrite,
        });
        self.pending_paste_image.insert(id, remote);
        self.status = crate::i18n::tr("正在上传粘贴的图片 …", "Uploading pasted image …").into();
    }

    /// 把图片路径打进终端（末尾补一个空格，方便接着往下写提示词）。不回车。
    pub(super) fn type_pasted_path(&mut self, path: &str) {
        // 需要时加引号。远端那条路径是我们自己拼的 `/tmp/ishell-paste-<毫秒>.png`，不含特殊
        // 字符；**本机**那条来自 `std::env::temp_dir()`，Windows 上常见
        // `C:\Users\Some Name\AppData\Local\Temp\…`——带空格，原样打进 shell 会被拆成两个词，
        // AI 拿到的是半截路径。Linux 上 `$TMPDIR` 同样可以是任何东西。
        //
        // 引号风格按**会话类型**而不是本机平台选：远端会话一律是 POSIX shell（哪怕 iShell
        // 跑在 Windows 上），只有「本机」会话在 Windows 上才是 cmd/PowerShell。
        let quoted = quote_shell_arg(path, self.cfg.is_local() && cfg!(windows));
        let _ = self
            .cmd_tx
            .send(crate::proto::UiCommand::TerminalInput(
                format!("{quoted} ").into_bytes(),
            ));
        self.terminal.push_input_line(&quoted);
        self.status = match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("已粘贴图片：{path}"),
            crate::i18n::Lang::En => format!("Pasted image: {path}"),
        };
    }

    pub(super) fn refresh_dir(&mut self, dir: Option<String>) {
        if let Some(dir) = dir {
            self.files.loading.insert(dir.clone());
            let _ = self.cmd_tx.send(crate::proto::list_dir_cmd(dir));
        }
    }

    /// 连接成功后初始化文件树：根 "/"，并定位到家目录。
    pub(super) fn init_files(&mut self) {
        self.files.root = "/".into();
        self.files.expanded.insert("/".into());
        // 只请求 "."（服务端解析为家目录）作为 cwd；树的其余层级由 sync_tree 自动补全。
        // 不预先请求 "/"，避免它先返回把 cwd 设成根目录。
        let _ = self.cmd_tx.send(crate::proto::list_dir_cmd(".".into()));
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
        // 按传输类型分叉：本机会话跑本地 PTY worker（不连任何主机），其余走 SSH worker。
        // 本机 worker 不需要 hostkey_rx（无 TOFU 主机密钥确认），忽略即可。
        if cfg.is_local() {
            self.runtime.spawn(crate::local::run(cfg, cmd_rx, sink));
        } else {
            self.runtime.spawn(ssh::run(cfg, cmd_rx, sink, hostkey_rx));
        }
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
            // 本机会话的 host/port 只是占位（见 ConnectConfig::local），不拿去拼 user@host:port
            // 这种毫无意义的悬停提示；给一个明确的「本机」标识。
            tip: if cfg.is_local() {
                format!("{} · {}", crate::i18n::tr("本机", "Local machine"), cfg.username)
            } else {
                format!("{}@{}:{}", cfg.username, cfg.host, cfg.port)
            },
            cmd_tx,
            evt_rx,
            sysinfo_rx,
            connected: false,
            status: crate::i18n::tr("连接中 …", "Connecting …").into(),
            terminal: Terminal::new(),
            sysinfo: None,
            net_hist: NetHistory::default(),
            files: {
                // 收藏夹/文件面板的持久化键：本机会话用一个稳定的合成键（host/port 无意义），
                // 避免和某台真实主机的 "user@host:port" 撞键。
                let key = if cfg.is_local() {
                    format!("local:{}", cfg.username)
                } else {
                    format!("{}@{}:{}", cfg.username, cfg.host, cfg.port)
                };
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
            restore_cwd_until: None,
            osc7_confirm: false,
            osc7_pending_reveal: false,
            cd_force_until: None,
            mcp_token_injected: false,
            monitor_ok: None,
            pending_ai_run: None,
            pending_paste_image: std::collections::HashMap::new(),
            ai_owned: false,
            pending_file_ops: Vec::new(),
            file_op_tombstones: std::collections::VecDeque::new(),
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
        s.pending_file_ops.clear(); // 同上：旧的 write_file/read_file/copy 等待也一并作废
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

#[cfg(test)]
mod paste_path_tests {
    use super::quote_shell_arg;

    /// 干净路径不加引号——远端那条 `/tmp/ishell-paste-<毫秒>.png` 是最常见的形状。
    #[test]
    fn a_clean_path_is_left_alone() {
        assert_eq!(
            quote_shell_arg("/tmp/ishell-paste-1788000000000.png", false),
            "/tmp/ishell-paste-1788000000000.png"
        );
        assert_eq!(quote_shell_arg("C:\\Temp\\a.png", true), "C:\\Temp\\a.png");
    }

    /// **带空格必须引起来。** Windows 的临时目录几乎一定含空格
    /// （`C:\Users\Some Name\AppData\Local\Temp`），不引的话打进 shell 就被拆成两个词，
    /// AI 拿到的是半截路径、读不到那张图。
    #[test]
    fn a_path_with_spaces_is_quoted() {
        assert_eq!(
            quote_shell_arg("/tmp/my pics/a.png", false),
            "'/tmp/my pics/a.png'"
        );
        assert_eq!(
            quote_shell_arg("C:\\Users\\Some Name\\AppData\\Local\\Temp\\a.png", true),
            "\"C:\\Users\\Some Name\\AppData\\Local\\Temp\\a.png\""
        );
    }

    /// 单引号、`$`、反引号这些在 POSIX shell 里有含义的字符也必须被关住——路径里出现它们
    /// 时不引就等于让文件名参与命令解析。
    #[test]
    fn shell_metacharacters_cannot_escape_the_quotes() {
        assert_eq!(quote_shell_arg("/tmp/a'b.png", false), "'/tmp/a'\\''b.png'");
        assert_eq!(quote_shell_arg("/tmp/$(id).png", false), "'/tmp/$(id).png'");
        assert_eq!(quote_shell_arg("/tmp/a`id`.png", false), "'/tmp/a`id`.png'");
        assert_eq!(quote_shell_arg("", false), "''");
    }
}

#[cfg(test)]
mod cwd_restore_tests {
    use super::{cwd_restore_decision, CwdRestore};

    /// shell 闲下来了才注入。`Connected` 那一刻远端多半正在吐 MOTD/banner，甚至可能停在
    /// 某个登录脚本起的程序上——此前是在那一刻直接打一行 `cd …\r` 过去。
    #[test]
    fn injects_only_once_the_shell_is_actually_idle() {
        assert_eq!(cwd_restore_decision(true, false, false), CwdRestore::Wait);
        assert_eq!(cwd_restore_decision(true, true, false), CwdRestore::Inject);
    }

    /// **用户一动手就放弃。** 他已经在用这个 shell 了（可能正对着某个提示符输密码），
    /// 此时替他 `cd` 只会打断他，而且那一行可能被别的程序吃掉。
    #[test]
    fn gives_up_as_soon_as_the_user_starts_typing() {
        assert_eq!(cwd_restore_decision(false, true, false), CwdRestore::GiveUp);
        assert_eq!(cwd_restore_decision(false, false, false), CwdRestore::GiveUp);
    }

    /// 等太久也放弃：意图不能无限期挂着，否则几分钟后突然往用户的 shell 里敲一行 `cd`。
    #[test]
    fn gives_up_after_the_deadline() {
        assert_eq!(cwd_restore_decision(true, false, true), CwdRestore::GiveUp);
        // 但截止时刻到了、同时又恰好闲下来的那一帧，注入优先——意图本来就该在这时兑现
        assert_eq!(cwd_restore_decision(true, true, true), CwdRestore::Inject);
    }
}
