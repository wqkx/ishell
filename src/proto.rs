//! 前台（egui，同步）与后台（tokio SSH worker，异步）之间的通信消息。
//!
//! - UI -> Worker：使用 `tokio::sync::mpsc`（`send` 为非阻塞同步调用，UI 线程可直接用）
//! - Worker -> UI：使用 `std::sync::mpsc`（UI 每帧 `try_recv` 排空）

/// 一次 SSH 连接的配置。
#[derive(Clone, Debug)]
pub struct ConnectConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

#[derive(Clone, Debug)]
pub enum AuthMethod {
    Password(String),
    /// 私钥文件路径（可选 passphrase）
    KeyFile { path: String, passphrase: Option<String> },
}

/// UI -> Worker 指令。
#[derive(Debug)]
pub enum UiCommand {
    /// 终端键盘输入（已编码为字节流）
    TerminalInput(Vec<u8>),
    /// 终端尺寸变化（字符列/行）
    Resize { cols: u16, rows: u16 },
    /// 请求列出远程目录（SFTP）
    ListDir(String),
    /// 下载远程文件到本地路径
    Download { id: u64, remote: String, local: String },
    /// 上传本地文件到远程目录
    Upload { id: u64, local: String, remote_dir: String },
    /// 新建目录
    Mkdir(String),
    /// 新建空文件
    CreateFile(String),
    /// 修改权限（八进制低 9 位）
    Chmod { path: String, mode: u32 },
    /// 删除文件或目录
    Delete { path: String, is_dir: bool },
    /// 重命名 / 移动
    Rename { from: String, to: String },
    /// 读取文本文件内容（用于编辑器打开）；force=true 时放宽大小限制
    ReadFile { path: String, force: bool },
    /// 写回文本文件内容（保存）
    WriteFile { path: String, content: String },
    /// 新增端口转发
    AddForward(ForwardSpec),
    /// 移除端口转发
    RemoveForward(u64),
    /// 主动断开
    Disconnect,
}

/// 端口转发类型。
#[derive(Clone, Debug)]
pub enum ForwardKind {
    /// 本地转发：本地端口 -> 远端 host:port
    Local { remote_host: String, remote_port: u16 },
    /// 动态转发：本地 SOCKS5 代理
    Dynamic,
}

/// 一条端口转发配置。
#[derive(Clone, Debug)]
pub struct ForwardSpec {
    pub id: u64,
    /// 本地监听地址（默认 127.0.0.1）
    pub bind_host: String,
    pub bind_port: u16,
    pub kind: ForwardKind,
}

/// Worker -> UI 事件。
#[derive(Debug)]
pub enum WorkerEvent {
    /// 状态文本（连接中、认证中等）
    Status(String),
    /// 连接并打开 shell 成功
    Connected,
    /// 连接断开（携带原因）
    Disconnected(String),
    /// 来自远程 shell 的原始字节，喂给 vt100 解析器
    TerminalData(Vec<u8>),
    /// 周期性系统信息快照
    SysInfo(Box<SysInfo>),
    /// 目录列表结果
    DirListing { path: String, entries: Vec<FileEntry> },
    /// 首次连接的未知主机，请 UI 确认是否信任其指纹（TOFU）
    HostKeyPrompt { host: String, fingerprint: String },
    /// 文本文件已读取，打开编辑器
    FileOpened { path: String, content: String },
    /// 一次文件操作成功完成（携带提示文本与需要刷新的目录）
    OpDone { message: String, refresh_dir: Option<String> },
    /// 传输开始（携带总字节数与方向）
    TransferStart { id: u64, name: String, total: u64, dir: TransferDir },
    /// 传输进度（已完成字节）
    TransferProgress { id: u64, done: u64 },
    /// 传输结束
    TransferDone { id: u64, ok: bool, message: String, refresh_dir: Option<String> },
    /// 端口转发状态更新（监听中 / 失败原因）
    ForwardStatus { id: u64, ok: bool, message: String },
    /// 错误提示
    Error(String),
}

/// 传输方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDir {
    Upload,
    Download,
}

/// 远程系统信息快照（由 worker 解析远程命令输出得到）。
#[derive(Clone, Debug, Default)]
pub struct SysInfo {
    pub hostname: String,
    pub ip: String,
    pub os: String,
    pub uptime: String,
    pub load: [f32; 3],
    /// 总体 CPU 使用率 0..100
    pub cpu_percent: f32,
    /// 每核心使用率 0..100
    pub cpu_cores: Vec<f32>,
    /// 内存（KB）
    pub mem_total_kb: u64,
    pub mem_used_kb: u64,
    pub swap_total_kb: u64,
    pub swap_used_kb: u64,
    /// 网络瞬时速率（字节/秒）—— 所有网卡之和（默认「全部」）
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    /// 各网卡的瞬时速率
    pub nets: Vec<NetIface>,
    pub disks: Vec<DiskInfo>,
    pub procs: Vec<ProcInfo>,
}

#[derive(Clone, Debug)]
pub struct NetIface {
    pub name: String,
    pub rx_bps: f64,
    pub tx_bps: f64,
}

#[derive(Clone, Debug)]
pub struct DiskInfo {
    pub mount: String,
    pub total_kb: u64,
    pub used_kb: u64,
    pub percent: f32,
}

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: f32,
}

/// SFTP 目录条目。
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_link: bool,
    pub size: u64,
    /// 修改时间（unix 秒），0 表示未知
    pub mtime: u64,
    /// 权限位（八进制低位），用于展示 rwx 字符串
    pub perm: u32,
    pub owner: String,
}
