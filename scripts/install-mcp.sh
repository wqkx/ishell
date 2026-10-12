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

# 3) 打印注册命令与多机配对提示（只需一次；已注册过则跳过）
cat <<EOF

在这台机器上执行以下命令，把 iShell 注册给 Claude Code（只需一次）：

  claude mcp add ishell -s user -- "$dest"

其它客户端（Codex 等）把 command 指到同一路径即可。

之后升级 iShell 时，重跑本脚本覆盖同一位置即可；若忘记升级代理，代理会在连接时
以「版本不一致，请重新部署」明确报错，不会静默出错。

连接哪台 iShell 完全由「启动 AI 的那个 iShell 终端」决定：开启 AI 控制后，iShell 会在连上后的空闲
提示符处往该终端注入 ISHELL_MCP_ENDPOINT 和 ISHELL_MCP_KEY，在那个 shell 里启动的 AI 只会连到
那一台 iShell，不会串到别人的电脑。不是从 iShell 终端启动的 AI 一律连不上（没有手动配置的口子）。
注入要求「连上后还没动过键盘」（避免把内容打进某个正在等输入的密码提示符）——连上就立刻打字的
话本次不注入，可在终端右键「立即注入 MCP 连接环境」补上。Codex 需要在 config.toml 里加：
  env_vars = ["ISHELL_MCP_ENDPOINT", "ISHELL_MCP_KEY"]
详见 README「AI / MCP 集成」。
EOF
