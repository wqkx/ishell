#!/usr/bin/env bash
# 跑「live SFTP」那组集成测试：它们连一台**真实的 sshd**，验证那些只有在真服务器上才成立
# 的事情——rename 语义、SETSTAT 会不会截断文件、换入前后的权限位、断点续传拼出来的字节。
#
# 这些测试默认带 #[ignore]，`cargo test` 不会跑到（CI 里也没有 sshd）。没有这个脚本的话，
# 它们就是「只有当初写的人知道怎么跑」的测试，几个月后必然烂掉——所以有了这个文件。
#
# 用法：
#   scripts/test-live-sftp.sh <host> <user> <私钥路径> [端口]
#
# 例（在测试服务器上连它自己，公钥本来就在 authorized_keys 里，最省事）：
#   scripts/test-live-sftp.sh 127.0.0.1 "$USER" ~/.ssh/id_ed25519
#
# 注意：
# - 测试会在服务器的 /tmp 下建 `ishell-upload-it-<随机>/` 工作目录并在里面折腾文件，
#   **不碰**你真实的 ~/.ssh（authorized_keys 那条测试用的是工作目录里自己造的同名文件）。
# - 私钥必须是**无口令**的：测试代码用 `load_secret_key(path, None)` 读它。
# - 跑完的工作目录不会自动删（失败时留着好排查）；清理：`rm -rf /tmp/ishell-upload-it-*`。

set -euo pipefail

if [ $# -lt 3 ]; then
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

HOST="$1"
USER_NAME="$2"
KEY="$3"
PORT="${4:-22}"

if [ ! -r "$KEY" ]; then
  echo "读不到私钥：$KEY" >&2
  exit 1
fi

# 私钥权限过宽时 ssh 生态普遍会拒用；这里也提前拦一下，免得报出来的错误指向别处
PERM="$(stat -c %a "$KEY" 2>/dev/null || stat -f %Lp "$KEY" 2>/dev/null || echo '')"
case "$PERM" in
  600|400) ;;
  '') echo "警告：拿不到 $KEY 的权限位，跳过检查" >&2 ;;
  *) echo "私钥 $KEY 权限是 $PERM，请先 chmod 600" >&2; exit 1 ;;
esac

echo "==> live SFTP 测试：${USER_NAME}@${HOST}:${PORT}"

export ISHELL_TEST_SSH_HOST="$HOST"
export ISHELL_TEST_SSH_PORT="$PORT"
export ISHELL_TEST_SSH_USER="$USER_NAME"
export ISHELL_TEST_SSH_KEY="$KEY"

# --test-threads=1：每个测试各开一条 SSH 连接，并行跑容易撞上 sshd 的 MaxStartups
# （夹具里已经有退避重试，串行再稳一层，失败信息也好读）。
cargo test -- --ignored --test-threads=1 live_sftp "${@:5}"
