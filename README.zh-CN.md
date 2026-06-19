<div align="center">

# iShell

**一个用 Rust 编写的现代化跨平台 SSH 客户端**

系统监控 · 交互式终端 · SFTP 文件管理 · 端口转发 · 跳板机 —— 一屏搞定

[English](README.md) · **中文**

[![Release](https://img.shields.io/github/v/release/wqkx/ishell?display_name=tag)](https://github.com/wqkx/ishell/releases)
[![Linux](https://github.com/wqkx/ishell/actions/workflows/linux.yml/badge.svg)](https://github.com/wqkx/ishell/actions/workflows/linux.yml)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-blue)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

![iShell](docs/screenshots/hero.png)

</div>

## ✨ 简介

iShell 的布局灵感来自 **FinalShell**——左侧实时系统信息、中间交互式终端、右下 SFTP 文件管理，
力求把日常 SSH 运维需要的东西放在同一个窗口里。和 FinalShell / Xshell / MobaXterm / Termius 等
工具相比，iShell 的取向是：

- **纯 Rust + GPU 即时模式 UI（egui）**——单可执行文件、启动快、占用低、无运行时依赖。
- **纯 Rust SSH 栈（russh + ring）**——不依赖系统 OpenSSH / PuTTY，跨平台行为一致。
- **三端一致**——Linux / macOS / Windows 同一套代码与界面。
- **多语言**——中文 / English，右键即可切换。
- **干净现代的浅色界面**——暖色主题、可切换深色终端，不堆砌工具栏。

> 个人项目，持续打磨中；欢迎 issue / PR。

## 🚀 功能

**连接与会话**
- 多会话标签：状态圆点、拖拽排序、溢出渐隐、关闭确认
- 保存连接（**密码本地加密** ChaCha20-Poly1305）、快速连接列表
- **断线自动重连**（指数退避）+ 手动重连
- **主机密钥校验**（known_hosts + 首次信任 TOFU，防中间人）

**终端**
- vt100 / 256 色、滚轮回滚、Tab 补全、焦点锁定
- **选中复制 / 右键复制粘贴 / Ctrl+Shift+C·V**、**Ctrl+滚轮调字号**
- **内容搜索**（Ctrl+Shift+F，全回滚缓冲，命中高亮）
- **输入前缀 + 上下键**的本会话历史检索
- 深 / 浅终端配色切换

**穿透与批量**
- **端口转发**：本地转发 + 动态 SOCKS5 代理
- **跳板机 / ProxyJump**：经堡垒机连接内网目标
- **命令广播**：向所有已连接会话同时发命令

**文件与监控**
- SFTP：树形目录 + 列表、**点击表头按名称/大小/时间排序**、拖拽上传、下载（含**文件夹递归**）、改权限 / 重命名 / 删除 / 复制路径、可选默认下载目录
- **多文件并发传输**（同一服务器最多 6 个，不同服务器互不影响）、**可中途取消**、失败可查看原因
- **多标签文本编辑器**：语法高亮、查找替换、大文件只读虚拟化（可切换为可编辑）
- 实时监控：CPU / 内存 / 交换、**GPU（NVIDIA / AMD / Intel）**、网络曲线、磁盘、进程 Top（点击查看详情 + 强制结束）

## 📸 截图

| 快速连接 | 端口转发 |
|---|---|
| ![](docs/screenshots/conn.png) | ![](docs/screenshots/fwd.png) |

| GPU 详情 | 进程详情 + 强制结束 |
|---|---|
| ![](docs/screenshots/gpu.png) | ![](docs/screenshots/proc.png) |

| 多标签编辑器（大文件只读 + 可切换） |
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
