#!/usr/bin/env bash
# 统一 bump 版本号：Cargo.toml 为唯一真源，同步 README / 桌面项 / CHANGELOG 头。
# 用法：./scripts/bump-version.sh 0.16.0
set -euo pipefail

NEW="${1:?用法: $0 X.Y.Z}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! [[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "错误：版本号须为 semver 三段式，例如 0.15.0" >&2
  exit 1
fi

OLD="$(grep -m1 '^version = ' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
if [[ "$OLD" == "$NEW" ]]; then
  echo "Cargo.toml 已是 $NEW，继续同步其它文件…"
fi

# 1) Cargo.toml + Cargo.lock
sed -i "s/^version = \".*\"/version = \"$NEW\"/" Cargo.toml
env -u CARGO_TARGET_DIR cargo check -q

# 2) README（英 / 中）
for f in README.md README.zh-CN.md; do
  if grep -q 'releases/tag/v' "$f"; then
    sed -i "s|releases/tag/v[0-9][0-9.]*|releases/tag/v$NEW|g" "$f"
    sed -i "s|\[v[0-9][0-9.]*\](https://github.com/wqkx/ishell/releases/tag/v$NEW)|[v$NEW](https://github.com/wqkx/ishell/releases/tag/v$NEW)|g" "$f"
  fi
done

# 3) Linux 桌面项：**不要**在这里写应用版本。
# .desktop 的 `Version=` 按规范指的是「桌面项规范的版本」（1.0/1.1/1.5），不是程序版本号。
# 早先这里把应用版本写了进去，`desktop-file-validate` 直接报 error：
#     value "0.16.13" for key "Version" ... is not a known version
# 非法的桌面项可能被桌面环境拒收或部分忽略（StartupWMClass 索引、菜单项等都靠它）。
# 应用版本已经在 Cargo.toml / README 里维护，桌面项不需要重复一份。

# 4) 拒绝遗留旧版本字面量（排除动画常量 0.14 秒、依赖版本、vendor）
if rg -n "0\\.14\\.[0-9]+" --glob '!vendor/**' --glob '!target/**' --glob '!Cargo.lock' . 2>/dev/null; then
  echo "警告：仍有 0.14.x 字面量，请手动检查上方匹配" >&2
fi

echo ""
echo "已 bump $OLD → $NEW"
echo "请编辑 CHANGELOG.md 补充 v$NEW 条目，然后："
echo "  git add -A && git commit -m \"chore(release): $NEW\""
echo "  git tag -a v$NEW -m \"Release v$NEW\" && git push origin main && git push origin v$NEW"
