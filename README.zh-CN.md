<div align="center">

<img src="docs/logo.png" alt="iShell" width="300">

**一个用 Rust 编写的现代化跨平台 SSH 客户端**

系统监控 · 交互式终端 · SFTP 文件管理 · 端口转发 · 跳板机 —— 一屏搞定

**⚡ 高性能 · 低资源占用** —— 原生 Rust + GPU 渲染，单文件、启动快、内存/CPU 占用低，常驻无负担

[English](README.md) · **中文**

[![Release](https://img.shields.io/github/v/release/wqkx/ishell?display_name=tag)](https://github.com/wqkx/ishell/releases)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-blue)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

![iShell](docs/screenshots/hero.png)

</div>

## ✨ 简介

iShell 把日常 SSH 运维需要的东西放在同一个窗口里——左侧实时系统信息、中间交互式终端、右下 SFTP 文件管理。
其取向是：

- **高性能、低资源占用（核心）**——纯 Rust + GPU 即时模式 UI（egui）：单可执行文件、启动快、内存/CPU 占用低、无运行时依赖与后台常驻服务。
- **纯 Rust SSH 栈（russh + ring）**——不依赖系统 OpenSSH / PuTTY，跨平台行为一致。
- **三端一致**——Linux / macOS / Windows 同一套代码与界面。
- **多语言**——中文 / English，右键即可切换。
- **干净现代的浅色界面**——暖色主题、可切换深色终端，不堆砌工具栏。

> 个人项目，持续打磨中；欢迎 issue / PR。

## ⚙️ 资源占用

| 指标 | 数值 |
|---|---|
| 二进制 | 单文件，无运行时依赖/守护进程 —— **Linux ~12 MB · macOS ~8–9 MB · Windows ~10 MB**（体积优化：opt-level `s` + fat LTO + strip） |
| 空闲 CPU | **≈ 0%**（单会话空闲，系统信息 2s 采集一次） |
| 内存 | **约 80 MB**（空闲，实测）——原生程序，**无 Electron / JVM / Python** 运行时，远低于 Electron 类客户端动辄数百 MB 的常驻占用 |

> 实测环境：Linux，release 构建，单会话空闲；具体随 GPU 驱动/分辨率略有差异。

## 🚀 功能

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

**穿透与批量**
- **端口转发**：本地转发 + 动态 SOCKS5 代理
- **跳板机 / ProxyJump**：经堡垒机连接内网目标
- **命令广播**：向所有已连接会话同时发命令
- **命令片段库**：保存常用命令，一键发送到当前会话终端（可选自动回车），持久化保存

**文件与传输**
- SFTP：树形目录 + 列表、**名称过滤**、**点击表头按名称/大小/时间排序**（大小、时间首次点击为降序）、拖拽上传、**「在终端打开此目录」**、改权限 / 重命名 / 复制路径、可选默认下载目录
- **多选批量操作**：Ctrl/Shift 多选 + 框选；**批量删除**（Delete 键 / 工具栏，含文件夹递归）、**批量下载**
- **远端复制 / 移动**：右键「复制 / 剪切」+「粘贴到此目录」，在远端直接完成（含多选、目录递归）
- **断点续传**：按字节续传 + 瞬时失败自动重试；**断线重连后自动续传**，含暂停/继续/重试队列
- **文件夹压缩下载**：远端 tar.gz 打包、单文件并发下载、纯 Rust 解包——多小文件更快
- **多文件并发传输**（同一服务器最多 6 个，不同服务器互不影响）、可中途取消
- **多标签文本编辑器**（独立 OS 窗口）：**行号**、语法高亮、查找替换、大文件只读虚拟化（可切换为可编辑）
- **超轻量看图工具**（独立 OS 窗口）：双击 `png / jpg / gif / bmp`——缩放/平移/适应/1:1/另存为

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

| 多标签编辑器 —— 行号、大文件只读（可切换）、独立窗口打开 |
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

| 关注点 | 选型 |
|---|---|
| GUI | `eframe` / `egui` 0.34 |
| SSH / SFTP | `russh` 0.61（ring 后端） / `russh-sftp` 2.3 |
| 终端 | `vt100` 0.16 |
| 异步 | `tokio` |
| 加密存储 | `chacha20poly1305` |

## 🔒 安全

- **主机密钥校验**：known_hosts 校验，未知主机首次连接弹窗确认 SHA256 指纹（TOFU）并写入；密钥改变则拒绝告警。
- **保存密码加密**：以 ChaCha20-Poly1305 加密落盘，密钥存于本地 `~/.config/ishell/key`（0600）；属 at-rest 加密。

---

<div align="center">
用 Rust 编写 · Linux / macOS / Windows
</div>
