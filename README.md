<div align="center">

<img src="docs/logo.png" alt="iShell" width="300">

**A modern, AI-native SSH terminal written in Rust**

Let Claude Code, Codex CLI, or any MCP-compatible agent drive a real, persistent terminal session — plus system monitor · SFTP file manager · port forwarding · jump hosts, all in one window

**English** · [中文](README.zh-CN.md)

[![Release](https://img.shields.io/github/v/release/wqkx/ishell?display_name=tag)](https://github.com/wqkx/ishell/releases)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-blue)
![Rust](https://img.shields.io/badge/built%20with-Rust-orange?logo=rust)

> **Latest release:** [v0.16.11](https://github.com/wqkx/ishell/releases/tag/v0.16.11)

</div>

## Why iShell

Everything you need for daily SSH work in **one window** — and it stays out of your way.

- 🤖 **Let AI drive the terminal (MCP)** — Claude Code, Codex CLI, and any other MCP-compatible agent can operate a real, persistent terminal session (cwd/env/history intact) instead of spawning a throwaway `ssh host cmd` that loses all context every time; commands and output show up in real time in the tab you're looking at. Off by default, opt-in when you want it. See "AI / MCP integration" below.
- ⚡ **Fast & lightweight** — pure Rust + GPU immediate-mode UI. A single binary (~8–12 MB), instant startup, **~0% idle CPU**, **~80 MB RAM**. No Electron / JVM / Python, no daemon, no runtime deps.
- 🎯 **Refined user experience** — a clean, warm light theme; smooth drag-to-reorder tabs; no toolbar clutter; English / 中文 switchable on the fly; sensible defaults so it just works.
- 📁 **Effortless file operations** — multi-select rubber-band, batch delete/download, server-side copy/move, **resumable** transfers that **auto-resume after reconnect**, and folder **compress-download** (tar.gz) for thousands of small files.
- 🔗 **Terminal ↔ files, linked** — "open this dir in terminal" from the file list, "reveal the terminal's current dir in the file list" the other way, and the working directory is **restored on reconnect** (OSC 7).
- 🧰 **Complete feature set** — agent auth & forwarding, jump hosts, port forwarding + SOCKS5, command broadcast & snippets, live CPU/GPU/net/disk/process monitoring with `kill -9`.
- ✍️ **A genuinely powerful editor** — a virtualized code editor that opens in its own window: **multi-cursor (Ctrl+D)**, syntax highlighting, find & replace, encoding/EOL auto-detect, Chinese IME, and it stays fast on huge files.

## ⚙️ Footprint

| Metric | Value |
|---|---|
| Binary | single file, no runtime deps / daemon — **Linux ~12 MB · macOS ~8–9 MB · Windows ~10 MB** (size-optimized: opt-level `s` + fat LTO + strip) |
| Idle CPU | **≈ 0%** (one idle session; system info polled every 2 s) |
| Memory | **~80 MB** (idle, measured) — native app, **no Electron / JVM / Python** runtime; far below Electron-based clients that idle at hundreds of MB |

> Measured on Linux, release build, one idle session; varies slightly with GPU driver / resolution.

## 🚀 Features

**AI / MCP integration** (off by default — see "AI / MCP integration" below)
- Let an AI assistant drive a real terminal session directly — shared, visible tab, commands and output in real time, instead of another SSH connection that loses all context
- Works with **Claude Code, Codex CLI, and any other MCP-compatible client** — one binary, standard MCP stdio transport
- Full tool set: run a command and wait for completion, keep waiting on long tasks, read screen/history, send raw keystrokes (for interactive prompts), interrupt, open/close sessions, read/write remote files
- **Automatic reverse-forward** over your existing SSH connection so the AI can reach back and control this iShell from the remote server too, no extra setup needed

**Connections & sessions**
- Multi-session tabs: status dots, **smooth drag-to-reorder animation**, overflow fade, close confirmation
- **Authentication**: password, key file, **SSH agent** (`SSH_AUTH_SOCK` / Windows OpenSSH pipe), or **keyboard-interactive (OTP / 2FA)**
- **Agent forwarding (`-A`)**: let remote processes reuse your local ssh-agent keys (no re-auth across hops)
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

**Terminal ↔ files integration**
- **File list → terminal**: right-click a folder → "Open in terminal" (or "Open current dir in terminal") `cd`s that session there
- **Terminal → file list**: right-click the terminal → "Show current dir in the file list" jumps the SFTP panel to the shell's current directory (via OSC 7, with a one-time consent prompt if the shell doesn't emit it)
- **Working dir restored on reconnect**, so a dropped session comes back where you left it

**Tunneling & batch**
- **Port forwarding**: local forward + dynamic SOCKS5 proxy
- **Jump host / ProxyJump**: reach internal targets through a bastion
- **Command broadcast**: send a command to every connected session at once
- **Command snippets**: save frequent commands, send to the current session terminal in one click (optional auto-Enter), persisted

**Files & transfers**
- SFTP: tree + list, **name filter**, **click a header to sort by name / size / time** (size & time default to descending), drag-and-drop upload, chmod / rename / copy path, optional default download folder
- **Multi-select batch ops**: Ctrl/Shift + rubber-band select; **batch delete** (Delete key / toolbar, recursive for folders), **batch download**
- **Remote copy / move**: right-click "Copy / Cut" + "Paste here", done entirely on the server (multi-select, recursive)
- **Resumable downloads**: chunk-bitmap resume bound to remote size+mtime, auto-retry on transient errors and **auto-resume after reconnect**; transfers can be cancelled or retried (no manual pause)
- **Folder compress-download**: tar.gz on the server, single-file parallel download, pure-Rust unpack — fast for many small files
- **Concurrent transfers** (up to 6 per server; independent across servers), cancellable mid-transfer
- **Lightweight image viewer** (its own OS window): double-click a `png / jpg / gif / bmp` — zoom / pan / fit / 1:1 / save-as

**Built-in code editor** (its own OS window, tabbed)
- **Unified virtualized editor** — renders only the visible lines, so even huge files open instantly and stay smooth at low memory; every file uses the same full-featured editor
- **Multi-cursor (Ctrl+D)** — accumulate selections of the same word, then **type / delete / move them all at once** (VS Code-style)
- **Syntax highlighting**, current-line highlight, **bracket matching**, indent guides, auto-close brackets
- **Find & replace** (regex / case / whole-word, match highlighting), **Go to line** (Ctrl+G), word & document navigation
- **Comment toggle**, duplicate / move / delete line, undo / redo
- **Encoding auto-detect** (UTF-8 / GBK / … via chardetng) and **EOL (LF / CRLF)** detection — both **clickable in the status bar to switch**, with safe re-encoding on save
- **External-change detection** — guards against overwriting a file edited on the server since you opened it
- Full **Chinese IME** input, double-click word select, fixed line-number gutter, download-progress tabs

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

| Code editor — multi-cursor, syntax highlighting, find & replace, opens in its own window |
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

## ❓ Troubleshooting

**Chinese/IME (fcitx/ibus) won't type on Linux Wayland?**
Some Wayland desktops (KDE Plasma / GNOME) have flaky `text-input-v3` support for winit-based apps, so fcitx-style IMEs never activate or compose (same issue as Chrome/Electron). Fix: **switch to X11 (XWayland)**, where XIM input works. Two ways (either one):

- **In-app**: right-click the terminal → check "**Force X11 (fix IME · restart)**" → **restart iShell**. The setting is persisted — set it once.
- **Env var**: `ISHELL_X11=1 ./ishell-linux-x86_64` (or temporarily `WAYLAND_DISPLAY= ./ishell-linux-x86_64`).

> Trade-off: forcing X11 loses some native-Wayland niceties (e.g. smoother fractional scaling) in exchange for a working IME — the same trade-off as Chrome's `--ozone-platform=x11`. The default is still Wayland; it only switches when you enable this.

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
- The code editor renders only visible lines over a virtualized scroll area, so file size barely affects memory or latency.

| Concern | Choice |
|---|---|
| GUI | `eframe` / `egui` 0.34 |
| SSH / SFTP | `russh` 0.61 (ring backend) / `russh-sftp` 2.3 |
| Terminal | `vt100` 0.16 |
| Async | `tokio` |
| Encrypted storage | `chacha20poly1305` |

## 🤖 AI / MCP integration

Let an AI assistant (Claude Code, Codex CLI, or any other MCP-compatible agent) drive a real terminal session — instead of spawning a
throwaway `ssh host cmd` that loses your shell's cwd, env, and history every time. It can either
take over a tab you already have open, or open a brand-new one itself from a saved connection
(read-only, for the AI's own use — a human can't type into it). Either way, the tab gets a clear
🤖 indicator in the tab bar.

- **Off by default.** Enable it via the right-click settings menu → "Allow AI to control terminal
  via MCP" (takes effect after restart). It only listens on a local Unix domain socket — one per
  iShell process (`~/.config/ishell/mcp-<pid>.sock`, mode `0600`) — no network port is opened.
- **Shared, visible terminal.** Commands the AI runs — and their output — appear in the
  corresponding terminal tab in real time, exactly as if you'd typed them yourself.
- **Setup**: build the companion binary once (`cargo build --release --bin ishell-mcp`), then
  install it to the standard location. On the machine that runs your AI client, run:
  ```bash
  scripts/install-mcp.sh target/release/ishell-mcp
  ```
  This copies it to **`~/.ishell-mcp/bin/ishell-mcp`** (a stable, self-namespaced path that
  survives repo moves/rebuilds) and prints the register command. It speaks the standard MCP stdio
  transport, so it isn't tied to any one client — point any of these at that path:
  - **Claude Code** — register it globally (not scoped to one project) in one line:
    ```bash
    claude mcp add ishell -s user -- ~/.ishell-mcp/bin/ishell-mcp
    ```
  - **Codex CLI** — same idea:
    ```bash
    codex mcp add ishell -- ~/.ishell-mcp/bin/ishell-mcp
    ```
    (or add it by hand to `~/.codex/config.toml`: `[mcp_servers.ishell]` / `command = "~/.ishell-mcp/bin/ishell-mcp"` — check `codex mcp --help` if the CLI/config format has changed since this was written)
  - **Any other MCP-compatible client** (Cursor, Windsurf, Cline, …) — most accept the generic form:
    ```json
    { "mcpServers": { "ishell": { "command": "~/.ishell-mcp/bin/ishell-mcp" } } }
    ```
  - **Keep the two in sync**: the GUI and `ishell-mcp` share a wire protocol compiled into both,
    so they must be the same build. When you upgrade iShell, re-run `install-mcp.sh` to refresh the
    proxy. If you forget, the proxy detects the version mismatch on connect and fails with a clear
    "redeploy ishell-mcp" message instead of misbehaving silently. (`ishell-mcp --version` prints
    its crate + protocol version.)
- **Tools exposed**:
  - `list_sessions` / `list_saved_connections`: list currently open sessions / all saved
    connection configs;
  - `open_session`: open a new read-only session from a saved connection for the AI's own use;
    first use of a given connection pops a confirmation dialog for you to approve, no repeat
    prompt for the rest of that run;
  - `close_session`: close a session the AI itself opened (it can't close yours);
  - `run_command`: run a command and wait for completion or a timeout, returning output + exit
    code; `poll_run` keeps waiting on a timed-out command without resending it; for long tasks,
    just pass a large `timeout_ms` directly (up to 24h) instead of polling with `sleep`;
  - `send_input`: send raw keystrokes straight to an interactive prompt (a `sudo` password,
    continuing input inside `vim`/a REPL), bypassing completion detection;
  - `read_screen`: a tmux `capture-pane`-style dump of the visible screen, for interactive
    programs like `vim`/`top`; `read_history` reads that session's full scrollback from the start,
    not just one screen;
  - `interrupt`: send Ctrl+C — also the escape hatch for the concurrency guard: a session only
    ever allows one pending AI command at a time, and calling `interrupt` immediately frees it up
    if stuck (at the cost of losing that command's result);
  - `write_file` / `read_file`: read/write a remote text file **inlined in the request/response**,
    over the existing SFTP connection — convenient for small files, but the whole content goes
    through the JSON-RPC payload;
  - `copy_to_remote` / `copy_from_remote`: copy a local file/directory to/from the remote side over
    the same SFTP connection **without inlining bytes into the MCP request** — use these instead of
    `write_file`/`read_file` for large files or whole directories.
- **Remote access, automatic.** Whenever iShell opens an SSH session to a server (with this
  setting on), it also reverse-forwards its local MCP socket to `~/.ishell-mcp-<nonce>.sock`
  **on that remote server** (a random suffix per connection, so a reconnect never collides with
  a not-yet-expired previous forward), over the very same authenticated/encrypted SSH connection
  (no new listening port anywhere, no extra credentials to manage). Anyone who can already SSH
  into that server can reach iShell through the forwarded socket — so only enable this for
  servers you'd trust with that level of access. `ishell-mcp` auto-discovers the current forwarded
  socket on its own (it re-probes on every call, picking the newest connectable `mcp-*.sock` /
  `~/.ishell-mcp-*.sock`) — just run it on (or with SSH access to) that server, no path to
  configure and no need to reconnect the MCP client after iShell reconnects:
  ```bash
  /path/to/ishell-mcp
  ```
- **Manual alternative**: forward the socket yourself with plain SSH instead of relying on the
  automatic reverse-forward above (substitute the actual pid in the socket name):
  ```bash
  ssh -N -L /tmp/ishell-mcp.sock:$HOME/.config/ishell/mcp-<pid>.sock user@ishell-host &
  ISHELL_MCP_SOCKET=/tmp/ishell-mcp.sock /path/to/ishell-mcp
  ```

## 🔒 Security

- **Host-key verification**: known_hosts is checked; an unknown host prompts you to confirm its SHA256 fingerprint (TOFU) before it is written; a changed key is rejected with a warning.
- **Saved-password encryption**: stored encrypted with ChaCha20-Poly1305; the key lives locally at `~/.config/ishell/key` (0600). This is at-rest encryption.

## 📄 License

[MIT](LICENSE) — a permissive license. Do whatever you want; just keep the copyright notice.

---

<div align="center">
Written in Rust · Linux / macOS / Windows
</div>
