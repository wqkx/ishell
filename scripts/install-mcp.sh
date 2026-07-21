#!/usr/bin/env bash
# 把 ishell-mcp 代理二进制安装到标准位置 ~/.ishell-mcp/bin/ishell-mcp，并打印 Claude Code
# 的注册命令。在**运行 Claude Code 的那台机器**上执行（代理与 Claude Code 同机）。
#
# 用法：
#   ./install-mcp.sh [path-to-ishell-mcp-binary]
# 不传参时，脚本会在自身目录及相邻的 dist/ 里找 ishell-mcp / ishell-mcp-<平台> 文件。
set -euo pipefail

dest_dir="${HOME}/.ishell-mcp/bin"
dest="${dest_dir}/ishell-mcp"

# 1) 定位源二进制
src="${1:-}"
if [ -z "$src" ]; then
  here="$(cd "$(dirname "$0")" && pwd)"
  for cand in "$here"/ishell-mcp "$here"/ishell-mcp-* "$here"/../dist/ishell-mcp "$here"/../dist/ishell-mcp-*; do
    if [ -f "$cand" ]; then src="$cand"; break; fi
  done
fi
if [ -z "$src" ] || [ ! -f "$src" ]; then
  echo "找不到 ishell-mcp 二进制。用法：$0 <path-to-ishell-mcp>" >&2
  exit 1
fi

# 2) 安装：先写临时名再 rename 原子替换——避免覆盖正在运行的二进制时报 "Text file busy"
mkdir -p "$dest_dir"
tmp="${dest}.new.$$"
cp "$src" "$tmp"
chmod +x "$tmp"
mv -f "$tmp" "$dest"

echo "已安装：$dest"
"$dest" --version 2>/dev/null || true

# 3) 打印注册命令（只需一次；已注册过则跳过）
cat <<EOF

在这台机器上执行以下命令，把 iShell 注册给 Claude Code（只需一次）：

  claude mcp add ishell -- "$dest"

之后升级 iShell 时，重跑本脚本覆盖同一位置即可；若忘记升级代理，代理会在连接时
以「版本不一致，请重新部署」明确报错，不会静默出错。
EOF
