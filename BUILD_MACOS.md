# 构建 macOS 版本

> 说明：iShell 是 GUI 程序（eframe/winit/wgpu），macOS 版必须链接 Apple 的
> Metal/AppKit 等系统框架，**无法在 Linux 上可靠交叉编译**。请用下面任一方式在
> macOS 环境（本机或 CI）上编译。代码已做平台适配（文件选择器按平台选后端）。

## 方式一：在 Mac 上本地编译（最简单）

```bash
# 1. 安装 Rust（如未装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 2. 进入项目目录后编译
#    Apple Silicon（M1/M2/M3…）默认就是 aarch64，直接：
cargo build --release
#    产物：target/release/ishell

#    Intel Mac：
#    rustup target add x86_64-apple-darwin
#    cargo build --release --target x86_64-apple-darwin

# 3. 运行
./target/release/ishell
```

macOS 上不需要额外系统库（egui 用系统框架）。**无需 GTK**。

> 注：项目根目录的 `.cargo/config.toml` 指向国内 rsproxy 镜像（为在中国的服务器加速）。
> 在 Mac 上若该镜像慢或不可用，删除它即用官方源：`rm .cargo/config.toml`。

## 方式二：用 GitHub Actions 自动产出（没有 Mac 也能拿到二进制）

仓库已带 `.github/workflows/macos.yml`：

1. 把项目推到 GitHub 仓库。
2. 打开仓库 **Actions** 页 → 选 “macOS Build” → **Run workflow**
   （或推一个 `v*` 标签，如 `git tag v0.1.0 && git push --tags`）。
3. 运行结束后在该次运行的 **Artifacts** 下载：
   - `ishell-aarch64-apple-darwin`（Apple Silicon）
   - `ishell-x86_64-apple-darwin`（Intel）

CI 使用 GitHub 免费的 macOS runner，自动安装 Rust 并编译两种架构。

## 打包成 .app / 通用二进制（可选）

```bash
# 通用二进制（同时支持 Intel + Apple Silicon）
rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
lipo -create -output ishell-universal \
  target/x86_64-apple-darwin/release/ishell \
  target/aarch64-apple-darwin/release/ishell

# 生成 .app 包（可用 cargo-bundle）
cargo install cargo-bundle
cargo bundle --release
```

## 首次运行被 Gatekeeper 拦截

未签名的二进制首次运行会被拦。临时放行：

```bash
xattr -dr com.apple.quarantine ./ishell      # 或对 .app
```

或在“系统设置 → 隐私与安全性”里点“仍要打开”。正式分发需 Apple 开发者证书签名 + 公证。
