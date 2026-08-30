<div align="center">

<img src="docs/logo.png" alt="iShell" width="300">

**一个面向 AI 工作流的现代化终端，用 Rust 编写**

让 Claude Code、Codex CLI 等任意 MCP 兼容的 AI 助手直接驱动一个真实、持久的终端会话——外加系统监控 · SFTP 文件管理 · 端口转发 · 跳板机，一屏搞定

[English](README.md) · **中文**

[![Release](https://img.shields.io/github/v/release/wqkx/ishell?display_name=tag)](https://github.com/wqkx/ishell/releases)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-blue)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

> **最新版本：** [v0.19.0](https://github.com/wqkx/ishell/releases/tag/v0.19.0)

</div>

## 为什么是 iShell

日常 SSH 运维需要的一切都在**同一个窗口**里——而且不打扰你。

- 🤖 **让 AI 直接驱动终端（MCP）** —— Claude Code / Codex 等可操作真实持久会话（cwd/环境/历史保留），命令实时出现在标签里；默认开启（设置里可关），配套代理二进制内置在 iShell 里、一次点击即可装到服务器上。多机共用一台 AI 服务器时请启用配对。详见「AI / MCP 集成」。
- ⚡ **快、占用低** —— 纯 Rust + GPU 即时模式 UI。单文件（约 8–12 MB）、秒开、**空闲 CPU ≈ 0%**、**内存约 80 MB**。无 Electron / JVM / Python，无守护进程，无运行时依赖。
- 🎯 **用心打磨的体验** —— 干净的暖色浅色主题、标签平滑拖拽排序、不堆砌工具栏、中文 / English 随时切换、默认值合理，开箱即用。
- 📁 **便捷的文件操作** —— 框选多选、批量删除/下载、远端服务器侧复制/移动、**下载断点续传**且**断线后自动续传**、文件夹 **压缩下载**（tar.gz）应对成千上万小文件。
- 🔗 **终端 ↔ 文件 联动** —— 文件列表「在终端打开此目录」，反向「在文件列表中显示终端当前目录」，断线重连还会**恢复工作目录**（OSC 7）。
- 🧰 **功能完善** —— Agent 认证与转发、跳板机、端口转发 + SOCKS5、命令广播与片段库、CPU/GPU/网络/磁盘/进程实时监控并可 `kill -9`。
- ✍️ **真正强大的编辑器** —— 独立窗口的虚拟化代码编辑器：**多光标（Ctrl+D）**、语法高亮、查找替换、编码/换行自动识别、中文输入法，超大文件依旧流畅。

## ⚙️ 资源占用

| 指标 | 数值 |
|---|---|
| 二进制 | 单文件，无运行时依赖/守护进程 —— **Linux ~12 MB · macOS ~8–9 MB · Windows ~10 MB**（体积优化：opt-level `s` + fat LTO + strip） |
| 空闲 CPU | **≈ 0%**（单会话空闲，系统信息 2s 采集一次） |
| 内存 | **约 80 MB**（空闲，实测）——原生程序，**无 Electron / JVM / Python** 运行时，远低于 Electron 类客户端动辄数百 MB 的常驻占用 |

> 实测环境：Linux，release 构建，单会话空闲；具体随 GPU 驱动/分辨率略有差异。

## 🚀 功能

**AI / MCP 集成**（默认开启，详见下文「AI / MCP 集成」一节）
- 让 Claude Code / Codex CLI 等 AI **驱动真实终端会话**（保留 cwd/环境/历史），命令与输出实时可见
- 完整工具集：跑命令、读屏幕/历史、交互输入、中断、开关会话、读写/传输远端文件
- SSH 反向转发到远端后，AI 在服务器上也能回控本机 iShell；**多机共用一台 AI 服务器时务必启用配对**（见下文）

**连接与会话**
- 多会话标签：状态圆点、**平滑拖拽排序动画**、溢出渐隐、关闭确认
- **认证**：密码、私钥文件、**SSH Agent**（`SSH_AUTH_SOCK` / Windows OpenSSH 命名管道），或 **键盘交互（OTP / 2FA 二次验证）**
- **Agent 转发（`-A`）**：远端进程复用本机 ssh-agent 的私钥（多跳免再次输密）
- **导入 `~/.ssh/config`**（勾选要导入的主机；Host / HostName / User / Port / IdentityFile / ProxyJump）
- 保存连接的**分组 / 标签 / 搜索**
- 保存密码的主密钥存入**系统钥匙串**（Secret Service / Keychain / 凭据管理器），不可用时回退到加密文件
- **断线自动重连**（指数退避）+ 手动重连；重连后**恢复工作目录**（OSC 7）
- **主机密钥校验**（known_hosts + 首次信任 TOFU，防中间人）

**终端**
- vt100 / 256 色、滚轮回滚、Tab 补全、焦点锁定
- **选中复制 / 右键复制粘贴 / Ctrl+Shift+C·V**、**Ctrl+滚轮调字号**
- **URL 可点击**、**ERROR/WARN 关键字高亮**、**会话日志录制**
- **内容搜索**（Ctrl+Shift+F，全回滚缓冲，命中高亮）
- **输入前缀 + 上下键**的本会话历史检索
- 深 / 浅终端配色切换；完整中文 / 输入法支持

**终端 ↔ 文件 联动**
- **文件列表 → 终端**：右键文件夹 →「在终端打开此目录」（或「在终端打开当前目录」），让该会话 `cd` 过去
- **终端 → 文件列表**：终端区右键 →「在文件列表中显示当前目录」，把 SFTP 面板跳转到 shell 的当前目录（基于 OSC 7；若 shell 未上报则一次性确认后注入）
- **断线重连恢复工作目录**，掉线回来还在原处

**穿透与批量**
- **端口转发**：本地转发 + 动态 SOCKS5 代理
- **跳板机 / ProxyJump**：经堡垒机连接内网目标
- **命令广播**：向所有已连接会话同时发命令
- **命令片段库**：保存常用命令，一键发送到当前会话终端（可选自动回车），持久化保存

**文件与传输**
- SFTP：树形目录 + 列表、**名称过滤**、**点击表头按名称/大小/时间排序**（大小、时间首次点击为降序）、拖拽上传、改权限 / 重命名 / 复制路径、可选默认下载目录
- **多选批量操作**：Ctrl/Shift 多选 + 框选；**批量删除**（Delete 键 / 工具栏，含文件夹递归）、**批量下载**
- **远端复制 / 移动**：右键「复制 / 剪切」+「粘贴到此目录」，在远端直接完成（含多选、目录递归）
- **下载断点续传**：分块位图续传（绑定远端大小 + mtime）+ 瞬时失败自动重试、**断线重连后自动续传**；传输可取消/重试（无手动暂停）
- **文件夹压缩下载**：远端 tar.gz 打包、单文件并发下载、纯 Rust 解包——多小文件更快
- **多文件并发传输**（同一服务器最多 6 个，不同服务器互不影响）、可中途取消
- **超轻量看图工具**（独立 OS 窗口）：双击 `png / jpg / gif / bmp`——缩放/平移/适应/1:1/另存为

**内置代码编辑器**（独立 OS 窗口、多标签）
- **统一虚拟化编辑器**——只渲染可见行，超大文件秒开、低内存、滚动流畅；所有文件共用同一套全功能编辑器
- **多光标（Ctrl+D）**——累加选中相同的词，然后**同时输入 / 删除 / 移动**（VS Code 风格）
- **语法高亮**、当前行高亮、**括号匹配**、缩进参考线、自动补全括号
- **查找替换**（正则 / 大小写 / 全词，命中高亮）、**跳转到行**（Ctrl+G）、按词 / 整文跳转
- **注释切换**、复制 / 移动 / 删除整行、撤销 / 重做
- **编码自动识别**（UTF-8 / GBK / … 经 chardetng）与**换行符（LF / CRLF）**识别——二者均可**在状态栏点击切换**，保存时安全重编码
- **外部改动检测**——保存前防止覆盖「打开后又被服务器端改过」的文件
- 完整**中文输入法**、双击选词、固定行号列、下载进度标签

**监控**
- 实时监控：CPU / 内存 / 交换、**GPU（NVIDIA / AMD / Intel）**、网络曲线、磁盘、进程 Top（点击查看详情 + 强制结束）

## 📸 截图

| SFTP 文件管理 + 并发传输 |
|---|
| ![](docs/screenshots/files.png) |

| 快速连接 | 端口转发 |
|---|---|
| ![](docs/screenshots/conn.png) | ![](docs/screenshots/fwd.png) |

| GPU 详情 | 进程详情 + 强制结束 |
|---|---|
| ![](docs/screenshots/gpu.png) | ![](docs/screenshots/proc.png) |

| 代码编辑器 —— 多光标、语法高亮、查找替换、独立窗口打开 |
|---|
| ![](docs/screenshots/edit.png) |

## 📦 安装

从 [**Releases**](https://github.com/wqkx/ishell/releases) 下载对应平台的可执行文件：

| 平台 | 文件 |
|---|---|
| Linux x86_64 | `ishell-linux-x86_64` |
| macOS Apple Silicon | `ishell-macos-aarch64` |
| macOS Intel | `ishell-macos-x86_64` |
| Windows x86_64 | `ishell-windows-x86_64.exe` |

```bash
# Linux / macOS
chmod +x ishell-*            # 赋可执行权限
./ishell-linux-x86_64
```

- **macOS** 未签名首次运行：`xattr -dr com.apple.quarantine ./ishell-macos-aarch64`，或“系统设置 → 隐私与安全性 → 仍要打开”。
- **Windows** SmartScreen：点“更多信息 → 仍要运行”。

## ❓ 常见问题

**Linux Wayland 下输入法（fcitx/ibus）打不了中文？**
部分 Wayland 桌面（如 KDE Plasma / GNOME）对 winit 类应用的 `text-input-v3` 协议支持有坑，导致 fcitx 等输入法无法激活、组字（和 Chrome/Electron 同病）。解决：**改走 X11（XWayland）**，其 XIM 输入法正常。两种开启方式（任选其一）：

- **应用内**：终端区右键 → 勾选「**强制 X11（修复输入法·重启生效）**」→ **重启 iShell**。该设置持久化，设一次即可。
- **环境变量**：`ISHELL_X11=1 ./ishell-linux-x86_64`（或临时 `WAYLAND_DISPLAY= ./ishell-linux-x86_64`）。

> 权衡：强制 X11 会损失部分原生 Wayland 体验（如分数缩放更顺滑），换来输入法可用——与 Chrome 的 `--ozone-platform=x11` 同理。默认仍走 Wayland，仅在你开启后切换。

**用 fcitx 打字/删除时整个界面冻住（远程桌面下多见）？**
把**设置 → 启动选项 →「输入法候选框跟随光标」**取消勾选。即时生效，不用重启。

原因：把光标位置告诉输入法，在 X11 上是一次**同步的 XIM 请求**（`XSetICValues`），Xlib 发出去之后**无限期等回复、没有超时**——而这个调用就发生在画界面的那个线程上。输入法只要在这中间没了（崩溃、重启、远程桌面会话切换/重连），这个线程就永远停在那儿。实测抓到的栈：

```text
poll(timeout=-1) → _XReadEvents → XIfEvent → _XimRead → XSetICValues
                 → winit::…::ImeContext::set_spot → eframe::run_native
```

winit 只在坐标**真的变了**时才发这条请求，所以恒定上报同一个坐标 = 一条都不会发出去。中文照常能打，只是候选框停在输入区左上角。iShell 也会自己发现界面卡住：往 `~/.config/ishell/crash.log` 追加一条说明——只报告，不会背着你改设置。


**最小化窗口之后整个界面像被挂起、AI 也操作不了？**
先用 `ISHELL_NO_VSYNC=1 ./ishell-linux-x86_64` 启动试一次。如果最小化之后一切正常，那就是下面这条。

读框架源码得到的线索：eframe 会给**最小化的窗口照样画帧并交换缓冲**——它内部判断可见性用的 `viewport.info.visible()` 在原生平台上**没有任何一处赋值**（`Occluded` 事件写的是 `info.occluded`），恒为真，于是绘制和 `gl_surface.swap_buffers()` 都不会被跳过。而默认 `vsync: true` 让 glutin 用 `SwapInterval::Wait(1)`，那次交换要等一个垂直同步；一个已经被图标化、合成器不再呈现的窗口很可能永远等不到，画界面的线程就停在那儿。这条链路里只有这一个会阻塞的调用。

零成本的判别方法（不用装 gdb）：**打几个字 → 最小化 → 等 15 秒 → 还原 → 看 `~/.config/ishell/crash.log`**。
- 有记录（写着「窗口最小化/被遮挡=true」）→ 只是 UI 线程被阻塞，进程本身活着，就是上面这条。
- 没有记录、而 AI 确实操作不了 → 连看门狗线程都被冻住了，那是 iShell 之外的东西在挂起整个进程。

关掉垂直同步的代价是连续滚动时可能撕裂，所以不设为默认。


## 🔧 从源码构建

需要 [Rust](https://rustup.rs/)（stable）。在目标平台上：

```bash
cargo run --release
```

各平台细节、依赖与交叉构建见 [BUILD.md](BUILD.md)。

## 🏗 架构

- **前台（egui，同步即时模式）** 与 **后台（tokio SSH worker，异步）** 通过 channel 解耦。
- 每个会话 = 一个独立 worker 任务：交互式 shell 通道、SFTP 通道、每 2s 一次的系统信息探测。
- 终端用 `vt100` 维护屏幕模型，egui 逐行分段着色渲染，键盘事件编码为 ANSI 序列回写。
- 代码编辑器在虚拟化滚动区上只渲染可见行，大文件滚动流畅、延迟低；内存随文件大小线性增长（正文常驻内存）。

| 关注点 | 选型 |
|---|---|
| GUI | `eframe` / `egui` 0.34 |
| SSH / SFTP | `russh` 0.61（ring 后端） / `russh-sftp` 2.3 |
| 终端 | `vt100` 0.16 |
| 异步 | `tokio` |
| 加密存储 | `chacha20poly1305` |

## 🤖 AI / MCP 集成

让 AI（Claude Code、Codex CLI 等）驱动**真实、持久**的终端会话——保留 cwd / 环境 / 历史，而不是每次另开一条丢光上下文的 `ssh host cmd`。可接管你已打开的标签，也可按已保存连接新开只读 AI 会话（人不能往里打字）；标签栏有 🤖 标识。

### 开启与接入

1. **0.19 起默认开启**（此前默认关闭）。设置菜单里的「允许 AI 通过 MCP 控制终端」用来关掉它。仅本机 Unix socket（`~/.config/ishell/mcp-<pid>.sock`，`0600`），不监听网络端口；改这个开关需重启。
2. 在**跑 AI 的那台机器**上安装代理：
   - **AI 跑在你 SSH 上去的服务器上** —— 在那台服务器的终端里右键 →「**安装 AI 控制代理到这台服务器**」。iShell 自带配套的 `ishell-mcp`，经现有 SFTP 通道推到 `~/.ishell-mcp/bin/ishell-mcp` 并置可执行位，再把注册命令打进终端。版本一致由构造保证，不会再出现「版本不一致，请重新部署」。*发版包只有 Linux 版内嵌代理，且只嵌自己这条腿的架构*（Linux x86_64 的 iShell 可部署到 x86_64 Linux 服务器）；本次构建没嵌任何架构时这个菜单项不显示。想要一份多架构都嵌全的包见 BUILD.md。
   - **AI 跑在本机，或服务器是别的架构** —— 手工装：
     ```bash
     scripts/install-mcp.sh target/release/ishell-mcp   # → ~/.ishell-mcp/bin/ishell-mcp
     claude mcp add ishell -s user -- ~/.ishell-mcp/bin/ishell-mcp   # Claude Code
     # codex mcp add ishell -- ~/.ishell-mcp/bin/ishell-mcp          # Codex
     ```
     其它客户端把 `command` 指到同一路径即可。GUI 与 `ishell-mcp` **必须同版本**；升级后重跑安装脚本。

### 工具一览

| 工具 | 作用 |
|------|------|
| `list_sessions` / `list_saved_connections` | 列出打开中的会话 / 已保存连接 |
| `open_session` / `close_session` | 按已保存连接开/关 AI 专用只读会话（首次连接需当面确认） |
| `run_command` / `poll_run` / `start_command` | 执行并等待 / 继续等待 / 立即启动长任务（最长 24h） |
| `send_input` / `interrupt` | 交互输入；Ctrl+C（同时释放卡住的挂起命令） |
| `read_screen` / `read_history` | 可见屏 / 完整回滚历史 |
| `write_file` / `read_file` | 小文本经 JSON 内联读写（大文件勿用） |
| `copy_to_remote` / `copy_from_remote` | 流式单文件传输（字节不进 MCP JSON）；`local_path` 在 **跑 ishell-mcp 的机器** |
| `copy_between_sessions` | 两个已打开远端会话之间复制单文件 |

### 远端回控（反向转发）

开关打开后，连上 SSH 时会把本机 MCP socket 反向转发到远端 `~/.ishell-mcp/mcp-<随机>.sock`（走现有加密通道，无额外端口）。远端的 `ishell-mcp` 会自动探测，一般无需配路径。

**谁能 SSH 到那台服务器（同账号），谁就能经转发 socket 触达这边的 iShell——只对你信任的服务器开这个开关。** 能碰到 socket 就能开它自己的会话、能只读；而写入**你自己打开的**会话，每次都仍要你当面授权。

### ⚠️ 多机共用一台 AI 服务器（必读）

典型场景：AI 跑在共享服务器上，多台电脑各自用 iShell 连过去。转发 socket 会堆在同一远端目录，未配对时代理只能弹窗让人点选——**打扰别人，点错还会改到别人的环境**。

正确做法（任选其一，推荐 1）：

1. **设置 →「自动注入配对标识（多机共用 AI 服务器）」**（**默认关**）。开启后每个新会话空闲时会被打进 ` export ISHELL_MCP_TOKEN=…`；此后在该终端启动的 AI / `ishell-mcp` 只绑定你这台电脑。默认关是因为它毕竟是程序替你在自己的 shell 里执行命令——只在这种共用服务器的拓扑下才打开。
2. AI 不在 iShell 终端里跑时：设置里「复制配对配置」，把 `ISHELL_MCP_TOKEN=…` 写进该 AI 的 MCP server 环境变量。
3. 未配对仍可点确认窗选机器，但**共享账号下务必配对**。

配对 token 会进入远端 shell 环境（及同 UID 可见的 `/proc/*/environ`）——这是共享账号上启用配对的代价；不要把 token 发到不可信主机，也不要贴进聊天。

手动隧道（不用自动转发时）：

```bash
ssh -N -L /tmp/ishell-mcp.sock:$HOME/.config/ishell/mcp-<pid>.sock user@ishell-host &
ISHELL_MCP_SOCKET=/tmp/ishell-mcp.sock /path/to/ishell-mcp
```

### 其它注意

- **AI 只能随便动自己开的会话。** 写你自己打开的那些标签，永远要当面授权，没有任何开关能绕过。**设置 →「AI 新开会话无需逐次确认」**（默认开）只管另一档：AI 用某条已保存连接新开一个自己的会话。
- AI 命令实时出现在目标标签——它做了什么你始终看得见。
- GUI 与 `ishell-mcp` 必须同版本；升级后重跑 `install-mcp.sh`。

## 🔒 安全

- **主机密钥校验**：known_hosts 校验，未知主机首次连接弹窗确认 SHA256 指纹（TOFU）并写入；密钥改变则拒绝告警。
- **保存密码加密**：以 ChaCha20-Poly1305 加密落盘，密钥优先系统钥匙串，不可用时回退本地 `~/.config/ishell/key`（0600）。
- **MCP**：0.19 起默认开启（设置里可关）；仅本机 socket（`0600`），不监听网络端口；**AI 只能随便动自己开的会话**，写入你自己打开的会话一律当面授权、没有开关能绕过；多机共享 AI 服务器时用配对 token 避免串台。配对 token 以 `0600` 落盘，且**本身从不出现在线上**——配对走双向挑战-应答（两侧互相出示 `HMAC(token, 随机数)`），代理验过对端的证明才出示自己的，因此既不会被冒充的 socket 骗走密钥，也不会把密钥送给任何连上来的人。已知残留风险：同账号的攻击者若在你的代理与你的 iShell 之间**实时中继**双方的挑战与证明，仍可冒充——这类 socket 上没有信道绑定可用（大家共用同一个 UID，`SO_PEERCRED` 也无从区分），无法根除。

## 📄 许可证

[MIT](LICENSE) —— 宽松许可证。随意使用/修改/分发/商用，保留版权声明即可。

---

<div align="center">
用 Rust 编写 · Linux / macOS / Windows
</div>
