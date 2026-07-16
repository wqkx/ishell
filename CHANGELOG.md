# Changelog

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)。

## [0.16.8] - 2026-07-16

### Fixed
- 编辑器保存在弱网/连接静默失联（如 Windows 上笔记本休眠唤醒、Wi-Fi 切换、VPN 抖动导致
  TCP 连接死掉但未收到 RST）时可能永久卡在"保存中"：此前保存请求发出后只等
  `FileSaved`/`FileSaveFailed`/`FileSaveConflict` 三种结果之一，没有任何超时兜底，
  远端 worker 迟迟不返回时标签会一直显示保存中、"珊瑚→绿"动画也卡在中途不再推进。
  现在保存关联一个 30 秒截止时间，超时则解锁标签为可重试状态并弹"保存超时，请检查网络
  连接"提示；每次保存分配一个独立 id，超时后姗姗来迟的旧结果会被安全识别并丢弃，
  不会误更新一个已经关闭或已重新保存过的标签
- 粘贴文本（尤其是从 Windows 记事本/Office/浏览器复制的内容，几乎总是 CRLF 换行）未做
  换行符归一化就直接插入编辑器内容，与内部"统一 LF"的约定不符，导致保存后的文件混入
  裸 `\r`、且可能让"内容是否变化"的判断被这些杂散字符干扰
- `copy_from_remote` 与 `copy_to_remote` 对"本地"的解析点不一致：前者一直是在 iShell GUI
  所在机器上落盘，后者是在运行 `ishell-mcp` 的调用方机器上——两个工具的"本地"其实不是
  同一台机器，导致"先 `copy_from_remote` 下载、再 `copy_to_remote` 读同一路径"这类中转
  操作会莫名报"文件不存在"。现在 `copy_from_remote` 也改为在调用方机器本地流式落盘，
  与 `copy_to_remote` 对称；目前流式模式仅支持单个文件，目录请多次调用或改用
  `run_command` 执行 `tar`/`rsync`
- AI/MCP 反向转发会在每次连接/重连时于远端主机 `$HOME` 根目录下新建一个
  `.ishell-mcp-<nonce>.sock` 文件，且从未清理，长期使用会在主目录堆积大量残留文件；
  现在统一放进 `~/.ishell-mcp/` 子目录，连接正常断开时主动删除本次注册的文件，
  另外每次连接前顺手清理这个子目录里 mtime 超过 24 小时的旧文件兜底崩溃场景

### Added
- 新增 `copy_between_sessions` 工具：把一个已打开远端会话上的文件复制到另一个已打开远端
  会话，不需要先下载到本机再上传。优先尝试源主机直连目标主机（不经过运行 iShell 的机器
  中转，适合两台主机同一局域网/集群的场景）——会生成一个仅限本次使用的一次性密钥对，
  临时授权源主机免密连接目标主机，传输完成后立即撤销这个临时授权、删除临时密钥，不留
  长期可用的免密信任；如果直连不可行（网络不通、权限受限等），自动降级为经 iShell 进程
  内存中转（不落盘任何一方磁盘）。当前仅支持单个文件

## [0.16.7] - 2026-07-15

### Fixed
- AI/MCP `poll_run` 在对端（`ishell-mcp` 进程）因其自身调用超时提前断开连接后，主进程侧仍把
  那次等待计为"占用中"，导致后续 `poll_run` 一直被"已有一个 poll_run 在等待"拒绝，只能靠
  `interrupt`（会中断正在跑的命令）才能恢复；现在连接一断开就能识别出这是孤儿等待者并自动放行

### Changed
- `write_file` 工具说明补充"二进制文件请用 copy_to_remote"提示；服务器说明补充 `&` 优先级
  低于 `&&` 的提醒，避免 `md5sum f && nohup cmd &` 这类写法把整条 `&&` 链一起丢进后台

## [0.16.6] - 2026-07-14

### Fixed
- v0.16.5 的 Windows CI 失败：新增的路径校验单元测试用 POSIX 风格路径（如 `/tmp/x`）断言
  `Path::is_absolute()` 应为真，但该判定标准随平台而变——Windows 下没有盘符前缀的路径不算
  绝对路径，导致测试在 Windows 上失败；这几个用例现在只在 unix 上跑（本来这条 MCP 通道也
  只在 unix 上真正启用）

## [0.16.5] - 2026-07-14

### Added
- AI/MCP 新增 `copy_to_remote` / `copy_from_remote` 工具：本地与远端之间直接复制文件/目录
  （复用现有 SFTP 上传/下载通道），字节不经过 MCP 连接本身——大文件、整个目录应该用这两个，
  而不是要求把全部内容内联进请求/响应 JSON 的 `write_file`/`read_file`

### Fixed
- `copy_from_remote` 拉取目录且要求改名时，压缩下载分支落地路径固定沿用远端目录名，
  实际不会改到调用方要求的本地目标名（报告成功但目标路径其实不存在）
- 目录下载覆盖已存在目标时是合并语义（保留目标里远端已不存在的旧文件），与预期的
  「覆盖」不符；现在统一为镜像覆盖：目标下载完成后只包含这次拉取的内容
- `copy_to_remote` 的本地路径存在性检查此前是同步 `std::fs::metadata`，在 UI 线程逐帧执行，
  路径落在慢速/挂起的挂载点时会卡住整个界面；改为交给 worker 侧本就异步的检测
- `copy_to_remote`/`copy_from_remote` 的路径校验从只判断"是否绝对路径"加强为拒绝空文件名
  （如 `/`、`////`）和 `.`/`..` 路径段，避免早前相对路径被静默当成写到文件系统根目录

## [0.16.4] - 2026-07-14

### Changed
- 应用图标与 README 顶部 logo 换新（新的像素风 3D 立体字 wordmark），统一加上圆角，
  同步更新 `assets/icon.png` / `assets/icon.ico` / `assets/macos/icon.png` / `docs/logo.png`

## [0.16.3] - 2026-07-14

### Fixed
- htop 等启用 DECCKM（应用光标键模式）的全屏程序里，方向键被误判成改 NI 参数：终端此前
  无论远端是否启用该模式都只发普通形式的方向键/Home/End 转义序列；现按 DECCKM 状态切换
  SS3/普通两种编码，和真实终端行为一致

### Changed
- README 移除首屏大图，版本号更新

## [0.16.2] - 2026-07-14

### Changed
- README 将 AI/MCP 提升为核心功能介绍（此前只在文末一段容易被忽略）
- CI 发布流程一并打包 `ishell-mcp`（Linux/macOS 四个平台）

## [0.16.1] - 2026-07-14

### Fixed
- AI/MCP 本地 IPC 此前只在 Linux 上编译测试过，Windows CI 因 tokio 的 Unix domain socket
  不可用而编译失败；按平台 `#[cfg(unix)]` 分离，Windows 上该特性直接返回明确的
  "暂不支持 Windows" 提示，其余代码正常编译

## [0.16.0] - 2026-07-14

### Added
- **AI/MCP 终端控制通道**：让 AI 助手（如 Claude Code）复用已打开的终端会话执行命令，
  而不是每次另开一条丢失 cwd/环境/历史的 `ssh host cmd`；支持通过已有 SSH 连接自动反向
  转发到远端、AI 专属只读会话（`open_session`/`close_session`）、`read_history` /
  `list_saved_connections` / `send_input` / `write_file` / `read_file` 等工具，默认关闭

### Fixed
- 弱网下鼠标滚轮/本地回滚逐帧取整丢量，触控板小幅滚动被吞掉
- resize 与清屏（`ESC[2J`+`ESC[3J`）重建 vt100 parser 时静默丢失鼠标上报等私有模式，
  全屏 TUI 一次窗口缩放后滚动即失效
- SSH 连接握手（TCP + 密钥交换）无超时保护，弱网下可能无限期挂起，导致「第一次连不上、
  后面怎么点重连都连不上」
- 文件目录负缓存永不失效：目录被外部进程频繁改动时，一次「不存在」判定会一直挡住导航，
  只能手动刷新父目录才能恢复
- 文件传输窗口文件名过长时与右侧状态图标重叠
- macOS 上 `Apple Color Emoji.ttc` 体积可达 150~200+ MB，整份读入常驻内存是空载占用
  数百 MB 的直接原因，现按体积跳过超大 emoji 字体
- 全屏 TUI（htop/less 等）备用屏滚动兼容：无鼠标上报时把滚轮转发为方向键

## [0.15.1] - 2026-07-13

### Fixed
- 窗口长时间最小化后内存暴涨：系统信息快照原先挤在无界 mpsc 队列里，UI 不排空
  （最小化时 egui 基本停止调用 update）就一直堆积；改为 watch 通道只保留最新一份，
  内存占用与最小化时长无关，恢复窗口也不再有「积压爆发式重绘」
- 终端拖拽选区起点漏选按下字符；滚动条边缘拖入文本区时选区路由误判
- 标签栏拖拽重排时起始瞬间的位置跳动（抓取偏移改用真实按下位置）

## [0.15.0] - 2026-07-10

### Added
- 终端真彩色（24-bit RGB）与 Unicode 宽字符渲染
- 大文件打开阈值与只读提示；路径面包屑导航
- 发布流程：`scripts/bump-version.sh`、`RELEASE.md`、CI 版本一致性校验

### Fixed
- 编辑器关闭标签时清理 egui `TextEditState`，修复大文件关闭后内存滞留
- 关闭非当前标签时 `active` 索引错位（编辑器与看图工具）
- 保存并关闭、加载失败等路径统一走 `remove_tab_at`

### Changed
- 大规模模块拆分：所有 `src/**/*.rs` 控制在约 600 行以内
- CI clippy 策略收紧；`rustfmt` 全仓格式化

## [0.14.4] - 2026-07-05

数据安全与评审整改（保存校验、剪贴板、路径框等）。详见 GitHub Release。

[0.15.0]: https://github.com/wqkx/ishell/releases/tag/v0.15.0
[0.14.4]: https://github.com/wqkx/ishell/releases/tag/v0.14.4
