# 构建 iShell（Linux / macOS / Windows）

iShell 是纯 Rust + eframe/egui 的 GUI 程序，三大平台均可构建。GUI 程序无法在 Linux
上可靠交叉编译到 macOS/Windows（需各自的系统 SDK/框架），请在对应平台或用 CI 构建。

代码已做平台适配：
- 文件选择器：Linux 用 xdg-portal（纯 Rust，免 GTK），macOS/Windows 用原生后端。
- 配置目录：Linux/macOS `~/.config/ishell`，Windows `%APPDATA%\ishell`。
- 加密后端用 ring（无 aws-lc 的 C/cmake 依赖），跨平台更省事。

## 各平台本地编译

```bash
# 通用（在目标平台上执行）
cargo build --release
#   产物：target/release/ishell（Windows 为 ishell.exe）
```

- **Linux**：可能需要 `libxkbcommon`、`libgl1`（多数桌面已自带）。
- **macOS**：无需额外系统库（用系统框架）。Apple Silicon 默认 aarch64；Intel 加
  `--target x86_64-apple-darwin`。
- **Windows**：用默认 MSVC 工具链（安装 “Visual Studio Build Tools” 的 C++ 生成工具）。

> 项目根 `.cargo/config.toml` 指向国内 rsproxy 镜像（为中国网络加速）。在镜像慢/不可用
> 的环境（如海外 CI）删除它即用官方源：`rm .cargo/config.toml`。

## 用 GitHub Actions 自动产出（无需本机有对应系统）

仓库自带工作流 `.github/workflows/release.yml`，推 `v*` 标签或在 Actions 页手动 Run，
一次产出全部平台：
- Linux：`ishell-linux-x86_64`（裸二进制 + 带图标/desktop 的 tar.gz）
- macOS：`ishell-macos-aarch64` / `ishell-macos-x86_64`（另附 .app 包）
- Windows：`ishell-windows-x86_64.exe`

产物在该次运行的 **Artifacts** 下载。

## 测试

```bash
cargo test              # 单元 + 回归测试（CI 每次 push/PR 都跑这条）
cargo clippy --all-targets
```

`.github/workflows/ci.yml` 在每次 push/PR 上跑 `cargo test`，并对
aarch64-linux / aarch64-darwin / x86_64-windows 各做一次 `cargo check --all-targets`。
**交叉那几个 job 很重要**：`release.yml` 只在推 `v*` 标签时才碰这些平台，于是
`#[cfg(windows)]` / `#[cfg(not(unix))]` 分支平时根本不会被编译，改坏了要等到发版当天才炸。

### live SFTP 集成测试（需要一台真 sshd）

有一组测试连**真实的 sshd**，验证那些只有在真服务器上才成立的事：SFTP 的 rename 语义、
SETSTAT 会不会截断文件、事务写换入前后的权限位、断点续传拼出来的字节、撤销临时信任之后
`authorized_keys` 的权限。它们默认带 `#[ignore]`，`cargo test` 与 CI 都不会跑到。

```bash
scripts/test-live-sftp.sh <host> <user> <无口令私钥路径> [端口]
# 例：在测试服务器上连它自己（公钥本来就在 authorized_keys 里，最省事）
scripts/test-live-sftp.sh 127.0.0.1 "$USER" ~/.ssh/id_ed25519
```

测试只在服务器的 `/tmp/ishell-upload-it-<随机>/` 里折腾，不碰你真实的 `~/.ssh`。
失败时工作目录会留着好排查，清理：`rm -rf /tmp/ishell-upload-it-*`。

**改动传输/SFTP 相关代码后请手动跑一次这组**——CI 覆盖不到它们，而它们守的恰恰是
「失败了会丢用户文件」的那几条路径。

## 交叉编译 ishell-mcp 代理

MCP 代理 `ishell-mcp`（见 README 的「AI / MCP integration」）要部署到**运行 AI 客户端的
那台机器**，它未必和你的构建机同平台。和上面的 GUI 不同，代理是无窗口、无系统框架依赖的
纯 Rust 程序，交叉编译要省事得多：

- **交叉到其它 Linux 架构**（如 x86_64 → arm64）：用 [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
  借 [Zig](https://ziglang.org/) 自带的多平台 libc 当链接器——**免 root、免装系统交叉工具链**。
  这正是没走传统 `gcc-aarch64-linux-gnu`（要 apt/root）的原因：
  ```bash
  # 一次性准备：装 zig（解压到用户目录即可）+ cargo-zigbuild + 目标 std
  cargo install cargo-zigbuild
  rustup target add aarch64-unknown-linux-gnu
  # 交叉编译（产物为合法 aarch64 ELF）
  cargo zigbuild --release --bin ishell-mcp --target aarch64-unknown-linux-gnu
  ```
  注意：这里的 **Zig 只是被当成「自带多平台 libc 的 C 交叉编译器/链接器」（`zig cc`）来用，
  代码仍是 100% Rust，不涉及 Zig 语言**。
- **交叉到 macOS**：Rust 部分能编过，但**链接会失败**——`ring`/std 的 macOS 部分需要
  `-framework CoreFoundation`，而系统框架来自 **Apple SDK**（zig 不含、授权受限）。所以
  macOS 代理请**在 Mac 上构建，或用 CI**（同上一节 GUI 的做法），而不是硬交叉。

> 代理与 GUI 的线协议是配套编译的（`MCP_PROTOCOL_VERSION`），必须同版本。版本不符时代理会
> 在连接时明确报错、提示重新部署，不会静默出错——升级 iShell 时记得一并重新部署代理。

## 首次运行的系统拦截

- **macOS**（未签名）：`xattr -dr com.apple.quarantine ./ishell`，或“系统设置→隐私与安全性→仍要打开”。
- **Windows**（SmartScreen）：点“更多信息→仍要运行”。正式分发需各平台的代码签名。
