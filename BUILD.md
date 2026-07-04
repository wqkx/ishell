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

## 首次运行的系统拦截

- **macOS**（未签名）：`xattr -dr com.apple.quarantine ./ishell`，或“系统设置→隐私与安全性→仍要打开”。
- **Windows**（SmartScreen）：点“更多信息→仍要运行”。正式分发需各平台的代码签名。
