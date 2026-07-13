# 发布 checklist

**版本号唯一真源：`Cargo.toml` 的 `version` 字段。** 代码通过 `crate::version::VERSION` 或 `env!("CARGO_PKG_VERSION")` 读取，不要在业务逻辑里硬编码版本字符串。

## 一键同步

```bash
./scripts/bump-version.sh 0.16.0   # 替换为目标版本
# 编辑 CHANGELOG.md
git add -A && git commit -m "chore(release): 0.16.0"
git tag -a v0.16.0 -m "Release v0.16.0"
git push origin main && git push origin v0.16.0
```

推送 `v*` 标签会触发 [`.github/workflows/release.yml`](.github/workflows/release.yml)（测试 + clippy + 全平台构建 + GitHub Release）。CI 会校验 **tag 与 Cargo.toml 版本一致**。

## 需保持一致的文件

| 位置 | 说明 |
|------|------|
| `Cargo.toml` | `version = "X.Y.Z"`（主版本） |
| `Cargo.lock` | `[[package]] name = "ishell"` 的 version（`cargo check` 自动更新） |
| `README.md` / `README.zh-CN.md` | 文首「最新版本」链接 `vX.Y.Z` |
| `CHANGELOG.md` | 新版本条目 |
| `assets/linux/ishell.desktop` | `Version=X.Y.Z` |
| `assets/macos/Info.plist` | 保持 `__VERSION__` 占位；CI 从 git tag 注入 |
| `src/version.rs` | `env!("CARGO_PKG_VERSION")`，无需手改 |
| `src/app/widgets.rs` | 关于对话框读编译期版本，无需手改 |
| `build.rs` | Windows exe 文件版本读 `CARGO_PKG_VERSION`，无需手改 |

## 本地安装

```bash
env -u CARGO_TARGET_DIR cargo build --release
cp target/release/ishell ~/.local/bin/ishell
ishell --version
```

> 注意：Cursor 沙箱可能设置 `CARGO_TARGET_DIR` 指向临时目录，本地安装前请 `env -u CARGO_TARGET_DIR` 或显式指定项目 `target/`。
