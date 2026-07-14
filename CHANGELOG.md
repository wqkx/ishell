# Changelog

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)。

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
