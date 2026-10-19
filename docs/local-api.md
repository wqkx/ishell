# iShell 本机 API

让桌面上的**本地程序**（尤其是本地 AI agent）通过 HTTP+JSON 控制 iShell，能力与 [MCP](../README.zh-CN.md#-ai--mcp-集成) 一一对应，业务规则完全相同——**只能读写调用方自己用 `open_session` 开的会话**，你自己打开的窗口它看不到、碰不了。

它和 MCP 是两扇独立的门：MCP 给服务器上的 agent（经 SSH 反向转发，靠进程祖先认主人），本机 API 给本机进程（没有祖先可认，门只有一把令牌）。开关也各自独立。

## 开启

左侧面板底部的 **「本机 API」** 按钮：点一下开、再点关，状态一眼可见（绿点 + `127.0.0.1:端口`；红点 = 启动失败，悬停看原因）。**默认关闭**；开关状态会记住。右键可复制地址。

开启后 iShell 写一个发现文件 `~/.config/ishell/api.json`（Windows：`%APPDATA%\ishell\api.json`；Unix 上权限 0600）：

```json
{"url": "http://127.0.0.1:47621", "token": "…64 位十六进制…", "pid": 12345, "version": "0.28.0"}
```

- **令牌每次开启随机生成**，关掉再开就换新；只在这个文件里，不进日志。
- **端口固定 47621**，被占用就在面板上明说「启动失败」，**不会自动换端口**——“谁拥有 API”只有一个答案（多开 iShell 时只有先开的那个占得到）。要改端口显式设环境变量 `ISHELL_API_PORT`。
- 调用方读这个文件取地址和令牌，**不要扫描端口**。

## 每个请求必须带

| 请求头 | 说明 |
|---|---|
| `Authorization: Bearer <token>` | 令牌。缺失/错误 → 401 |
| `X-Ishell-Actor: <名字>` | 你这个调用方的名字，1–64 个字符 `[A-Za-z0-9._-]`。它决定哪些会话算“你开的”：同名调用方共享会话，不同名互相不可见。缺失/非法 → 400 |
| `Content-Type: application/json` | 有请求体时 |
| `Host` | 必须是 `127.0.0.1:端口` / `localhost:端口` / `[::1]:端口`（防 DNS 重绑定） |

**不得带 `Origin` 头**（浏览器发起的跨站请求必带它，一律 403）；不发 CORS 头，不应答 `OPTIONS`。一条连接一个请求（`Connection: close`），不支持分块传输（`Transfer-Encoding`）；`Expect: 100-continue`（curl 发较大请求体时会带）认证通过后会应答 `100 Continue`。请求头上限 16 KiB，请求体上限 32 MiB（大文件用 `copy_to_remote`）。

## 操作

`POST /v1/<操作>`，请求体是 JSON 对象（参数同名同义于对应的 MCP 工具）；无参数的 `list_sessions` / `list_saved_connections` 也可 `GET`。`GET /v1/list_sessions?filter=子串` 与 MCP 的 `filter` 同义。

| 操作 | 参数 | 说明 |
|---|---|---|
| `GET /v1/info` | — | 版本 / pid / 主机名 |
| `list_sessions` | `filter?`（POST 体或 GET `?filter=`） | 只列**你自己开的**会话；`filter` 按标题/主机名子串过滤 |
| `list_saved_connections` | — | 已保存连接（名称/主机/用户/端口，不含凭据） |
| `open_session` | `name` | 用已保存连接开一个新会话（归你所有） |
| `close_session` | `session_uid` | 只能关你自己开的 |
| `run_command` | `session_uid, command, timeout_ms?`（默认 15000） | 单行命令；超时返回 `finished:false`，用 `poll_run` 续等 |
| `start_command` | `session_uid, command` | `run_command` 的最小等待窗口版（100ms），之后 `poll_run` |
| `poll_run` | `session_uid, run_id?, timeout_ms?` | 续等一条未完成的运行，不重发命令 |
| `interrupt` | `session_uid` | 发 Ctrl-C |
| `send_input` | `session_uid, text, escapes?` | 原样发送按键；`escapes:true` 时解析 `\r \n \t \\ \xHH` |
| `read_screen` | `session_uid` | 当前一屏 |
| `read_history` | `session_uid, max_lines?`（默认 200，0=不限） | 回滚 + 当前屏 |
| `write_file` | `session_uid, path, content, timeout_ms?` | 经 SFTP 写远端文本文件（覆盖） |
| `read_file` | `session_uid, path, force?, timeout_ms?` | 读远端文本文件 |
| `copy_to_remote` | `session_uid, local_path, remote_path, timeout_ms?` | **本机**文件/目录 → 远端（`local_path` 是 iShell 所在这台电脑的绝对路径） |
| `copy_from_remote` | `session_uid, remote_path, local_path, timeout_ms?` | 远端单文件 → **本机** `local_path`（绝对路径，不含 `.`/`..`；先写临时文件、确认传输成功才原子换入，失败不破坏已有同名文件） |
| `copy_between_sessions` | `src_session_uid, src_remote_path, dest_session_uid, dest_remote_path, timeout_ms?` | 两个你自己的会话之间复制单文件 |

### 响应

成功：HTTP 200，`{"ok": true, "type": "<结果类型>", "data": <内容>}`。例如 `run_command` → `type:"Run"`，`data:{"run_id":1,"finished":true,"output":"…","exit_code":0}`。

失败：`{"ok": false, "error": "<人话原因>"}`，HTTP 状态码——400 请求不合法（缺字段、类型不对）、401 令牌、403 Host/Origin、404 未知操作、422 业务错误（会话不存在或不是你的、已有命令在跑、连接失败等）、503 并发已满。`error` 文案与 MCP 的报错同源，直接给 agent 看即可。

## 例子

```bash
J=~/.config/ishell/api.json
URL=$(jq -r .url $J); TOKEN=$(jq -r .token $J)
H=(-H "Authorization: Bearer $TOKEN" -H "X-Ishell-Actor: my-agent" -H "Content-Type: application/json")

curl -s "${H[@]}" $URL/v1/list_saved_connections
curl -s "${H[@]}" -d '{"name":"e5"}' $URL/v1/open_session                  # → data.uid
curl -s "${H[@]}" -d '{"session_uid":1,"command":"nproc"}' $URL/v1/run_command
curl -s "${H[@]}" -d '{"session_uid":1}' $URL/v1/close_session
```

```python
import json, os, urllib.request
info = json.load(open(os.path.expanduser("~/.config/ishell/api.json")))
def call(op, **args):
    req = urllib.request.Request(f'{info["url"]}/v1/{op}', json.dumps(args).encode(),
        {"Authorization": "Bearer " + info["token"], "X-Ishell-Actor": "my-agent",
         "Content-Type": "application/json"})
    try:
        return json.load(urllib.request.urlopen(req, timeout=600))
    except urllib.error.HTTPError as e:      # 业务错误也是 JSON：{"ok": false, "error": ...}
        return json.load(e)
```

## 安全模型（与边界）

- 只绑 `127.0.0.1`；认证前只缓冲请求头（≤16 KiB、5 秒、并发 64），**令牌通过之前不读请求体**；令牌常量时间比较。
- 回环 HTTP 浏览器里的网页也打得到：`Host` 校验挡 DNS 重绑定，`Origin` 一律拒、无 CORS 头挡跨站请求。
- 门只有一把令牌：**本机上任何能读到 `api.json` 的进程（同一用户）都能用它**——所以默认关闭、由你在面板上显式开启，只在你信任本机其它程序时开。多用户机器上文件权限 0600 把其它用户挡在外面（Windows 靠 `%APPDATA%` 的默认 ACL）。
- iShell 正常退出时删发现文件；被杀/崩溃留下的旧文件，下次启动时（写它的进程已不在则）清掉。
- `X-Ishell-Actor` 是**自报**的：持令牌的进程可以冒用另一个 API 调用方的名字拿走它的会话。iShell 内部把它记成 `api:<名字>`，与 MCP 代理的身份永不相等，所以 API 调用方碰不到 MCP agent 的会话（反之亦然：MCP 握手会拒绝以 `api:` 开头的 actor）。
- AI 命令实时出现在对应标签里——它做了什么你始终看得见。
- 关闭开关 = 立刻删发现文件、断开所有在途连接、拒绝新请求。

## 已知边界 / 未验证

- 只有 Linux 上做过端到端验证（真实 GUI + 真 sshd + 真 HTTP 客户端，含用 XTest 点面板按钮开关）。macOS / Windows 与 Linux 共用同一份代码（回环 TCP、纯 Rust），但**没有实机运行过**；Windows 上令牌文件的权限只依赖 `%APPDATA%` 的默认 ACL，没有显式收紧。
- 不提供事件推送（SSE/WebSocket）：看输出用 `read_screen` / `poll_run`。
- 调用方在请求处理中途把连接**半关闭**（发完请求就 `shutdown(SHUT_WR)`）会被当成断开、请求被取消；普通 HTTP 客户端（curl、requests、urllib）不会这样。
