#!/usr/bin/env bash
# 发版前的完整自检——**在你自己的机器/构建服务器上跑，不花 CI 额度**。
#
# 为什么需要它：CI（.github/workflows/release.yml）只在推 `v*` 标签时触发，而且为了省额度
# 只在 linux-x64 那条腿上跑测试与 clippy。也就是说：平时 push 到 main 没有任何自动检查，
# 发版当天才第一次跑——那时通常正赶时间，挂了很难受。这个脚本就是把那份检查提前挪到本地。
#
# 用法：
#   scripts/precheck.sh            # 测试 + clippy + 交叉编译检查
#   scripts/precheck.sh --quick    # 只跑测试 + clippy（跳过交叉，快很多）
#
# 跑完全绿再打标签推 CI，基本不会在 CI 上翻车。
#
# 不包含的两项，各有原因：
# - `cargo fmt --check`：本仓库从未跑过 rustfmt（实测 200+ 处差异），为加检查重排全仓库会把
#   真实改动淹没在噪声里。要做该是单独一次「全仓 fmt」提交。
# - live SFTP 集成测试：需要一台真 sshd，见 scripts/test-live-sftp.sh。**改动传输/SFTP
#   相关代码后请另外手动跑它**——它守的是「失败了会丢用户文件」的那几条路径。

set -uo pipefail

QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

cd "$(dirname "$0")/.."

FAILED=()
step() {
  local name="$1"; shift
  printf '\n\033[1m==> %s\033[0m\n' "$name"
  if "$@"; then
    printf '\033[32m    OK\033[0m\n'
  else
    printf '\033[31m    FAILED\033[0m\n'
    FAILED+=("$name")
  fi
}

step "cargo test" cargo test

# 与 release.yml 的门禁保持**同一套** flag，免得本地绿、CI 红
step "cargo clippy" cargo clippy --all-targets -- \
  -W clippy::all \
  -A clippy::too_many_arguments \
  -A clippy::type_complexity \
  -A deprecated

if [ "$QUICK" = "0" ]; then
  # 交叉编译检查：release.yml 会为这些平台出包，而 `#[cfg(windows)]` /
  # `#[cfg(not(unix))]` 分支平时根本不会被编译——改坏了要等到发版当天才炸。
  # 这里只 check（不 build、不链接），几十秒就能回答"这些 cfg 分支还能编译吗"。
  #
  # Windows 走 cargo-zigbuild：本机通常没有 mingw，普通 cargo 会在 ring 编 C 时失败。
  # 缺少工具链的目标直接跳过并说明，不算失败——不是每台开发机都装齐了。
  for t in x86_64-pc-windows-gnu aarch64-unknown-linux-gnu; do
    if ! rustup target list --installed 2>/dev/null | grep -qx "$t"; then
      printf '\n\033[33m==> 跳过交叉检查 %s（target 未安装：rustup target add %s）\033[0m\n' "$t" "$t"
      continue
    fi
    if command -v cargo-zigbuild >/dev/null 2>&1; then
      step "cross-check $t (zigbuild)" cargo zigbuild --target "$t" --bins
    else
      printf '\n\033[33m==> 跳过交叉检查 %s（没装 cargo-zigbuild）\033[0m\n' "$t"
    fi
  done
fi

printf '\n────────────────────────────\n'
if [ ${#FAILED[@]} -eq 0 ]; then
  printf '\033[32m全部通过\033[0m\n'
  exit 0
fi
printf '\033[31m失败：%s\033[0m\n' "${FAILED[*]}"
exit 1
