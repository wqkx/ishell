<div align="center">

<img src="docs/logo.png" alt="iShell" width="300">

**A modern, cross-platform SSH client written in Rust**

System monitor · interactive terminal · SFTP file manager · port forwarding · jump hosts — all in one window

**⚡ High performance · low resource usage** — native Rust + GPU rendering: a single binary, fast startup, low memory/CPU footprint

**English** · [中文](README.zh-CN.md)

[![Release](https://img.shields.io/github/v/release/wqkx/ishell?display_name=tag)](https://github.com/wqkx/ishell/releases)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-blue)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

![iShell](docs/screenshots/hero.png)

</div>

## ✨ Overview

iShell puts everything you need for daily SSH work in a single window — live system info on the left,
an interactive terminal in the center, and an SFTP file manager at the bottom-right. It leans toward:

- **High performance, low resource usage (the core)** — pure Rust + a GPU immediate-mode UI (egui): a single binary, fast startup, low memory/CPU footprint, no runtime deps or background services.
- **A pure-Rust SSH stack (russh + ring)** — no dependency on the system OpenSSH / PuTTY; consistent behavior across platforms.
- **One codebase, three platforms** — Linux / macOS / Windows share the same code and UI.
- **Multilingual** — English / 中文, switchable from the right-click menu.
- **A clean, modern light UI** — warm theme, optional dark terminal, no toolbar clutter.

> A personal project, actively polished. Issues / PRs welcome.

## ⚙️ Footprint

| Metric | Value |
|---|---|
| Binary | single file, no runtime deps / daemon — **Linux ~12 MB · macOS ~8–9 MB · Windows ~10 MB** (size-optimized: opt-level `s` + fat LTO + strip) |
| Idle CPU | **≈ 0%** (one idle session; system info polled every 2 s) |
| Memory | **~80 MB** (idle, measured) — native app, **no Electron / JVM / Python** runtime; far below Electron-based clients that idle at hundreds of MB |

> Measured on Linux, release build, one idle session; varies slightly with GPU driver / resolution.

## 🚀 Features

**Connections & sessions**
- Multi-session tabs: status dots, **smooth drag-to-reorder animation**, overflow fade, close confirmation
- **Authentication**: password, key file, or **SSH agent** (`SSH_AUTH_SOCK` / Windows OpenSSH pipe)
- **Import `~/.ssh/config`** (pick which hosts; Host / HostName / User / Port / IdentityFile / ProxyJump)
- **Groups / tags / search** for saved connections
- Saved-password key stored in the **OS keychain** (Secret Service / Keychain / Credential Manager), with an encrypted-file fallback
- **Auto-reconnect** on drop (exponential backoff) + manual reconnect; **restores working dir** (OSC 7) on reconnect
- **Host-key verification** (known_hosts + trust-on-first-use, anti-MITM)

**Terminal**
- vt100 / 256-color, scrollback, Tab completion, focus locking
- **Selection copy / right-click copy & paste / Ctrl+Shift+C·V**, **Ctrl+scroll to resize font**
- **Clickable URLs**, **ERROR/WARN keyword highlighting**, **session logging** to file
- **Content search** (Ctrl+Shift+F, full scrollback, match highlighting)
- **Prefix + Up/Down** per-session history search
- Dark / light terminal toggle; full CJK / IME input

**Tunneling & batch**
- **Port forwarding**: local forward + dynamic SOCKS5 proxy
- **Jump host / ProxyJump**: reach internal targets through a bastion
- **Command broadcast**: send a command to every connected session at once

**Files & transfers**
- SFTP: tree + list, **name filter**, **click a header to sort by name / size / time** (size & time default to descending), drag-and-drop upload, **"open this dir in terminal"**, chmod / rename / delete / copy path, optional default download folder
- **Resumable transfers**: byte-level resume + auto-retry on transient errors; **auto-resume after reconnect** with a pause/resume/retry queue
- **Folder compress-download**: tar.gz on the server, single-file parallel download, pure-Rust unpack — fast for many small files
- **Concurrent transfers** (up to 6 per server; independent across servers), cancellable mid-transfer
- **Tabbed text editor** (its own OS window): **line numbers**, syntax highlighting, find & replace, large-file read-only virtualization (switchable to editable)
- **Lightweight image viewer** (its own OS window): double-click a `png / jpg / gif / bmp` — zoom / pan / fit / 1:1 / save-as

**Monitoring**
- Live monitor: CPU / memory / swap, **GPU (NVIDIA / AMD / Intel)**, network graph, disks, top processes (click for details + kill -9)

## 📸 Screenshots

| SFTP file manager + concurrent transfers |
|---|
| ![](docs/screenshots/files.png) |

| Quick Connect | Port Forwarding |
|---|---|
| ![](docs/screenshots/conn.png) | ![](docs/screenshots/fwd.png) |

| GPU details | Process details + kill -9 |
|---|---|
| ![](docs/screenshots/gpu.png) | ![](docs/screenshots/proc.png) |

| Tabbed editor — line numbers, large-file read-only (switchable), opens in its own window |
|---|
| ![](docs/screenshots/edit.png) |

## 📦 Install

Download the binary for your platform from [**Releases**](https://github.com/wqkx/ishell/releases):

| Platform | File |
|---|---|
| Linux x86_64 | `ishell-linux-x86_64` |
| macOS Apple Silicon | `ishell-macos-aarch64` |
| macOS Intel | `ishell-macos-x86_64` |
| Windows x86_64 | `ishell-windows-x86_64.exe` |

```bash
# Linux / macOS
chmod +x ishell-*            # make it executable
./ishell-linux-x86_64
```

- **macOS** (unsigned, first run): `xattr -dr com.apple.quarantine ./ishell-macos-aarch64`, or "System Settings → Privacy & Security → Open Anyway".
- **Windows** SmartScreen: click "More info → Run anyway".

## 🔧 Build from source

Requires [Rust](https://rustup.rs/) (stable). On the target platform:

```bash
cargo run --release
```

See [BUILD.md](BUILD.md) for per-platform details, dependencies, and cross builds.

## 🏗 Architecture

- The **frontend (egui, synchronous immediate mode)** and the **backend (tokio SSH worker, async)** are decoupled via channels.
- Each session = one independent worker task: an interactive shell channel, an SFTP channel, and a system-info probe every 2 s.
- The terminal keeps its screen model in `vt100`; egui renders it line-by-line with per-span color, and keyboard events are encoded back as ANSI sequences.

| Concern | Choice |
|---|---|
| GUI | `eframe` / `egui` 0.34 |
| SSH / SFTP | `russh` 0.61 (ring backend) / `russh-sftp` 2.3 |
| Terminal | `vt100` 0.16 |
| Async | `tokio` |
| Encrypted storage | `chacha20poly1305` |

## 🔒 Security

- **Host-key verification**: known_hosts is checked; an unknown host prompts you to confirm its SHA256 fingerprint (TOFU) before it is written; a changed key is rejected with a warning.
- **Saved-password encryption**: stored encrypted with ChaCha20-Poly1305; the key lives locally at `~/.config/ishell/key` (0600). This is at-rest encryption.

---

<div align="center">
Written in Rust · Linux / macOS / Windows
</div>
