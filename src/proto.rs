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
    /// 标签显示名（来自连接名称，空则回退为用户名）；不含 IP
    pub label: String,
    /// 可选跳板机：先连它，再经 direct-tcpip 连到目标主机
    pub jump: Option<JumpHost>,
    /// 转发本机 ssh-agent（OpenSSH 的 `-A`）：远端进程可复用本机 agent 私钥
    pub forward_agent: bool,
}

/// 跳板机（堡垒机）连接信息。
#[derive(Clone, Debug)]
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

/// 传输目标已存在时的冲突处理策略（全局设置，默认覆盖）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// 覆盖（默认）：继续传输，替换/续写目标
    Overwrite,
    /// 跳过：目标已存在则不传输
    Skip,
    /// 重命名：自动取不冲突的新名（如 `name (1)`）
    Rename,
}

impl ConflictPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictPolicy::Overwrite => "overwrite",
            ConflictPolicy::Skip => "skip",
            ConflictPolicy::Rename => "rename",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "skip" => ConflictPolicy::Skip,
            "rename" => ConflictPolicy::Rename,
            _ => ConflictPolicy::Overwrite,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AuthMethod {
    Password(String),
    /// 私钥文件路径（可选 passphrase）
    KeyFile { path: String, passphrase: Option<String> },
    /// 使用本机 ssh-agent 中的私钥（SSH_AUTH_SOCK / Windows OpenSSH 命名管道）
    Agent,
    /// 键盘交互（keyboard-interactive）：按服务器提示逐项输入，支持 OTP / 二次验证
    Interactive,
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
    Download { id: u64, remote: String, local: String, policy: ConflictPolicy },
    /// 上传本地文件到远程目录
    Upload { id: u64, local: String, remote_dir: String, policy: ConflictPolicy },
    /// 取消某个传输任务（进行中或排队中）
    CancelTransfer(u64),
    /// 新建目录
    Mkdir(String),
    /// 新建空文件
    CreateFile(String),
    /// 修改权限（八进制低 9 位）
    Chmod { path: String, mode: u32 },
    /// 批量删除文件/目录：一条 rm 处理所有路径（单通道），避免多文件并发开过多 SSH 通道被拒
    DeleteMany { paths: Vec<String> },
    /// 重命名 / 移动
    Rename { from: String, to: String },
    /// 远端批量复制 / 移动到目标目录（经 shell 执行 cp -a / mv）
    CopyMove { srcs: Vec<String>, dest_dir: String, do_move: bool },
    /// 跨主机「直传」：在源主机上直接用 rsync/scp 把 srcs 推到目标主机 dest（数据不经本地）。
    /// 仅目标为「无口令密钥」认证时可用；key_path 为本地（本进程可直接读）的 B 私钥路径，
    /// worker 会临时上传到源主机 /tmp（0600）供 ssh 使用、用完即删。失败由上层提示转中转。
    DirectTransfer(Box<DirectSpec>),
    /// 键盘交互认证：用户对服务器提示的逐项回答（顺序与 prompts 一致）
    KbdResponse(Vec<String>),
    /// 读取文本文件内容（用于编辑器打开）；force=true 时放宽大小限制。id 关联占位标签的下载进度
    ReadFile { id: u64, path: String, force: bool },
    /// 跟随读取（tail -f）：从 offset 读到文件末尾（单次上限 512KB）。
    /// offset = u64::MAX 表示初始化——只返回当前文件大小、不读数据。
    TailFile { path: String, offset: u64 },
    /// 读取图片文件原始字节（用于看图工具打开）
    ReadImage { path: String },
    /// 查询 PDF 页数（远端 pdfinfo）。id 关联编辑器窗口的占位标签；
    /// 未装 poppler 等失败走 FileLoadFailed（移除占位 + 提示）。
    PdfInfo { id: u64, path: String },
    /// 渲染 PDF 单页为 PNG（远端 pdftoppm 输出到 stdout）
    PdfPage { path: String, page: u32, dpi: u32 },
    /// PDF 全文查找（远端 pdftotext，按换页符定位页码；不分大小写）
    PdfSearch { path: String, query: String },
    /// 读取文档原始字节（docx 查看器）。id 关联占位标签，进度复用 FileLoadProgress。
    ReadDoc { id: u64, path: String },
    /// 写回文本文件内容（保存）。content 为内部 LF 文本，worker 按 eol 还原行尾、按 encoding 编码后写入。
    /// expect_mtime≠0 且与远端当前 mtime 不一致且 !force 时，判定外部已改动、拒绝写入并回报冲突。
    WriteFile { path: String, content: String, encoding: String, eol: Eol, expect_mtime: u32, force: bool },
    /// 查询进程详情（cmdline / cwd / exe）
    ProcDetail(u32),
    /// 强制结束进程（kill -9）
    KillProc(u32),
    /// 新增端口转发
    AddForward(ForwardSpec),
    /// 移除端口转发
    RemoveForward(u64),
    /// 主动断开
    Disconnect,
}

/// 跨主机直传的目标主机参数（由 UI 据目标会话配置构造，交源会话 worker 执行）。
#[derive(Clone, Debug)]
pub struct DirectSpec {
    /// 源会话里登记的传输 id（进度/完成事件回到源会话）
    pub id: u64,
    /// 源主机上的待传绝对路径
    pub srcs: Vec<String>,
    pub dest_user: String,
    pub dest_host: String,
    pub dest_port: u16,
    /// 目标主机上的目标目录
    pub dest_dir: String,
    /// 本地 B 私钥文件路径（worker 同进程可直接读，再临时投放到源主机）
    pub key_path: String,
    /// 展示名（首个条目名 + 数量），用于传输行标题
    pub label: String,
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
    /// 目录列举失败。`retryable`=true 表示会话级错误（弱网/SFTP 通道重连中），UI 应保留
    /// loading 并稍后自动重试；=false 表示路径级错误（不存在/无权限），UI 标记该路径无效。
    DirListFailed { path: String, message: String, retryable: bool },
    /// 未知主机请 UI 确认指纹（TOFU）；changed=true 表示主机密钥**已变更**（更危险）
    HostKeyPrompt { host: String, fingerprint: String, changed: bool },
    /// 键盘交互认证：服务器下发一组提示，请 UI 收集回答后回 `KbdResponse`
    /// prompts 每项为 (提示文本, 是否回显)；echo=false 的项应做密码遮蔽
    KbdPrompt { name: String, instructions: String, prompts: Vec<(String, bool)> },
    /// 文本文件已读取，填充对应占位编辑器标签（id 与 ReadFile 一致）。
    /// content 已按探测到的编码解码、行尾统一为 LF；encoding/eol 用于保存时还原；mtime 用于外部改动检测。
    FileOpened { id: u64, path: String, content: String, encoding: String, eol: Eol, mtime: u32 },
    /// 保存成功（携带新的 mtime，编辑器据此更新，避免下次保存误判为外部改动）
    FileSaved { path: String, mtime: u32 },
    /// 保存写入进度（驱动编辑器标签的「珊瑚→绿」保存动画，跟随实际上传速度）
    FileSaveProgress { path: String, done: u64, total: u64 },
    /// 保存时检测到文件已被外部修改（未写入）；UI 提示用户是否覆盖
    FileSaveConflict { path: String },
    /// 保存失败（网络/权限/磁盘等）：标签保持未保存状态并提示
    FileSaveFailed { path: String, message: String },
    /// 打开时发现文件实际大小超限（列表里的旧大小已过时）：请 UI 弹「打开大文件」确认，可强制打开
    FileTooLarge { id: u64, path: String, size: u64 },
    /// 跟随读取返回：data 为新增原始字节（可能为空）；offset 为下次读取起点；
    /// truncated = 文件被截断/轮转（此时 offset 已重置为新大小）
    FileTail { path: String, data: Vec<u8>, offset: u64, truncated: bool },
    /// PDF 页数查询成功（失败走 FileLoadFailed）。id 与占位标签对应。
    PdfInfo { id: u64, path: String, pages: u32 },
    /// PDF 单页渲染结果（PNG 字节；空表示该页渲染失败）
    PdfPage { path: String, page: u32, data: Vec<u8> },
    /// PDF 查找结果：命中 (页码, 该页首个命中行片段)；message=失败原因（如缺 pdftotext）
    PdfSearch { path: String, query: String, hits: Vec<(u32, String)>, message: Option<String> },
    /// 文档原始字节已读取（docx 查看器）。id 与占位标签对应。
    DocOpened { id: u64, path: String, data: Vec<u8> },
    /// 文本文件下载进度（驱动占位标签上的珊瑚色进度条）
    FileLoadProgress { id: u64, done: u64, total: u64 },
    /// 文本文件打开失败（移除占位标签 + 提示）
    FileLoadFailed { id: u64, message: String },
    /// 图片文件已读取（原始字节），打开看图工具
    ImageOpened { path: String, data: Vec<u8> },
    /// 一次文件操作成功完成（携带提示文本与需要刷新的目录）
    OpDone { message: String, refresh_dir: Option<String> },
    /// 传输开始（携带总字节数与方向）
    TransferStart { id: u64, name: String, total: u64, dir: TransferDir, local: Option<String> },
    /// 传输进度（已完成字节）
    TransferProgress { id: u64, done: u64 },
    /// 传输阶段提示（如「打包中…」「解包中…」「直传中…」）；空串表示清除提示。
    /// 在传输进行中（ok=None）于详情行替代字节读数显示，让长耗时的非传输阶段对用户可见。
    TransferNote { id: u64, note: String },
    /// 传输结束
    TransferDone { id: u64, ok: bool, message: String, refresh_dir: Option<String> },
    /// 进程详情返回
    ProcDetail { pid: u32, cmd: String, cwd: String, exe: String },
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

/// 行尾风格（编辑器内部统一用 LF，保存时按原文件还原）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
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
    /// GPU 列表（无 NVIDIA GPU 或无 nvidia-smi 时为空）
    pub gpus: Vec<GpuInfo>,
}

/// 单块 GPU 信息（来自 nvidia-smi）。
#[derive(Clone, Debug)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    /// 使用率 0..100
    pub util: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
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
    /// df 报告的「真实可用」（已扣除 root 保留块），并非 total-used
    pub avail_kb: u64,
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
    /// 符号链接跟随解析出的「规范目标绝对路径」。仅 `is_link` 时尝试填充：
    /// 解析成功（目标存在）为 `Some`，断链 / 未解析为 `None`。
    pub link_target: Option<String>,
    /// 该符号链接最终指向一个目录（用于「跟随进入」判定与图标着色）。
    pub link_dir: bool,
}
