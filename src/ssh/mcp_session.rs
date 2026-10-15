//! 往远端登记「这个登录 shell 属于哪台 iShell」：写 `~/.ishell-mcp/sess/<主机名>/<shell pid>`（按主机分目录：`$HOME` 可能被多台机器共享，
//! 而清扫是按「pid 在本机是否存在」判断的，混放会互相误删）。
//!
//! 为什么是文件、不是往终端里敲 `export`：见 `mcp_protocol` 的「认证层」一节。要点——身份由进程
//! 祖先决定（内核给的事实），不需要用户「连上后别动键盘」，agent 过滤环境也无所谓，shell 历史里
//! 也不会留下密钥。
//!
//! 怎么找到「这条连接的登录 shell」：本连接的 sshd 会话进程是所有通道（登录 shell、各个 exec 命令、
//! sftp）共同的父进程；登录 shell 是它那些子进程里**带 tty** 的那个。只用 POSIX 的 `ps`/`awk`，
//! Linux 与 macOS 通用，不依赖 `/proc`。

use russh::client::Handle;

use super::auth::{exec_status, exec_with_stdin, ClientHandler};
use super::sh_quote;

/// 主机名规整（与 `mcp_protocol::host_tag` 同一规则；登记与清理两段脚本共用）。
const HOST_TAG_SH: &str = r#"h=$(printf '%s' "$(uname -n 2>/dev/null)" | tr -c 'A-Za-z0-9._-' '_')
case $h in ''|.|..) h=unknown;; esac
"#;

/// 远端执行的 POSIX `sh` 脚本。stdin 两行：端点路径、密钥（**不进命令行**）。
/// 成功时 stdout 打出登录 shell 的 pid；退出码 3 = 还没找到登录 shell（调用方重试）。
const REGISTER_SCRIPT: &str = r#"
umask 077
IFS= read -r ep || exit 2
IFS= read -r key || exit 2
@HOST_TAG@
d="$HOME/.ishell-mcp/sess/$h"
mkdir -p "$d" || exit 4
chmod 700 "$HOME/.ishell-mcp" "$HOME/.ishell-mcp/sess" "$d" 2>/dev/null
c=$$
while :; do
  c=$(ps -o ppid= -p "$c" 2>/dev/null | tr -d ' ')
  [ -n "$c" ] && [ "$c" -gt 1 ] 2>/dev/null || exit 3
  case $(ps -o comm= -p "$c" 2>/dev/null) in *sshd*) break;; esac
done
pid=$(ps -A -o pid= -o ppid= -o tty= 2>/dev/null | awk -v c="$c" '$2==c && $3 ~ /(pts|tty|pty|console)/ {print $1; exit}')
[ -n "$pid" ] || exit 3
et=$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ')
[ -n "$et" ] || exit 3
secs=$(printf '%s' "$et" | awk -F'[-:]' '{n=NF; if(n==2)s=$1*60+$2; else if(n==3)s=$1*3600+$2*60+$3; else s=$1*86400+$2*3600+$3*60+$4; print s}')
start=$(( $(date +%s) - secs ))
find "$d" -type f -name '*.tmp.*' -mmin +1 -exec rm -f {} + 2>/dev/null
for f in "$d"/*; do
  [ -e "$f" ] || continue
  p=${f##*/}
  case $p in ''|*[!0-9]*) continue;; esac
  kill -0 "$p" 2>/dev/null || rm -f "$f"
done
t="$d/$pid.tmp.$$"
printf 'ishell-mcp-session/1\n%s\n%s\n%s\n%s\n' "$ep" "$key" "$pid" "$start" > "$t" && mv -f "$t" "$d/$pid" || { rm -f "$t"; exit 5; }
printf '%s\n' "$pid"
"#;

/// 登记这条连接。登录 shell 在连接刚建立的头一瞬可能还没起来，所以找不到（退出码 3）时重试几次。
///
/// 成功返回登录 shell 的 pid（断开时据此删文件）；失败返回给人看的原因——**调用方必须把它显示
/// 出来**，不能静默：登记失败意味着在这个终端里启动的 AI 连不上 iShell。
pub(super) async fn register(
    handle: &Handle<ClientHandler>,
    endpoint: &str,
    key: &str,
) -> Result<u32, String> {
    let cmd = format!("sh -c {}", sh_quote(&REGISTER_SCRIPT.replace("@HOST_TAG@\n", HOST_TAG_SH)));
    let input = format!("{endpoint}\n{key}\n");
    let mut last = String::from("未知原因");
    for attempt in 0..12 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        // 单次执行设上限：远端 ps/sh 卡住时不能让整个登记（连同收尾）无限等。
        let run = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            exec_with_stdin(handle, &cmd, input.as_bytes()),
        )
        .await;
        match run {
            // 开通道失败（服务端 MaxSessions 撞满等）是瞬时的：与「还没找到登录 shell」一样重试。
            Err(_) => last = "远端执行登记脚本超时（15 秒无响应）".into(),
            Ok(Err(e)) => last = format!("无法在远端执行登记脚本：{e}"),
            Ok(Ok((0, out, _))) => {
                return out
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| format!("远端返回了无法理解的结果：{out:?}"));
            }
            Ok(Ok((3, _, _))) => last = "找不到这条连接的登录 shell（远端的 ps 输出里没有它的 tty 子进程）".into(),
            Ok(Ok((4, _, e))) => return Err(format!("无法创建 ~/.ishell-mcp/sess：{}", e.trim())),
            Ok(Ok((5, _, e))) => return Err(format!("无法写会话文件：{}", e.trim())),
            Ok(Ok((code, _, e))) => return Err(format!("登记脚本退出码 {code}：{}", e.trim())),
        }
    }
    Err(last)
}

/// 断开时清理：删掉本次登记的会话文件（按**端点路径**认——不依赖 shell pid，所以登记做到一半被收尾
/// 打断、没来得及记下 pid 的情形也清得掉）和反向转发的 socket 文件。尽力而为；残留的会话文件靠
/// 「pid 已不存在就清」与启动时间校验兜底。一条命令、一个通道。
pub(super) async fn unregister(handle: &Handle<ClientHandler>, endpoint: &str) {
    let script = format!(
        r#"ep={ep}
{snippet}for f in "$HOME/.ishell-mcp/sess/$h"/*; do
  [ -f "$f" ] && [ "$(sed -n 2p "$f" 2>/dev/null)" = "$ep" ] && rm -f "$f"
done
rm -f "$ep""#,
        ep = sh_quote(endpoint),
        snippet = HOST_TAG_SH,
    );
    let _ = exec_status(handle, &format!("sh -c {}", sh_quote(&script))).await;
}
