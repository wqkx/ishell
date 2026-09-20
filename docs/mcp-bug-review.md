# iShell MCP 功能 bug 审查记录

> 状态：**已对照源码确认，并于 2026-09-20 修复可修项**。
> 范围：MCP 全部核心路径（agent、协议、bridge、xfer、SSH 反向转发、部署、store、事件路由）。
> 配套：`docs/mcp-multimachine-analysis.md`。

## A. 已确认并修复

### A1. `upload_from_mcp` 绕过 `resolve_upload_target`（权限丢失 / symlink 被顶 / 目录被换）

- 位置：`src/ssh/xfer/upload.rs`
- 受影响工具：`copy_to_remote`（流式）、`copy_between_sessions`（`RelayWriteFile` 复用同函数）
- **已修**：开头调用 `resolve_upload_target`，用解析后的 `target` 计算 tmp/bak，覆盖写时在写内容前对临时文件 `set_metadata` 继承 `orig_perm`。源读取另加 60s 空闲超时（见 B1）。

### A2. 部署后打进终端的注册命令未做 shell 引用

- 位置：`src/app/session_events.rs` → `session::mcp_register_cmd`
- **已修**：路径走 `quote_shell_arg`。家目录含空格 / `$` / 反引号时命令不再被截断或注入。

## B. 低危项（已缓解）

### B1. 卡住的上传可占满 MCP 连接信号量（~24h DoS 面）

- **已修（两处）**：
  1. 握手/Identify 与长操作分两档信号量（64 / 32）。上传占满工作名额时，新的 Identify 仍能进。
  2. `upload_from_mcp` 对调用方字节流单次 read 加 60s 空闲超时，FUSE/网络盘挂死不再拖满 24h。

### B2. 传输判定超时不对称（agent 5s vs GUI 30s）

- **已修**：agent 等判定改为 `VERDICT_READ_TIMEOUT`（45s），长于 GUI 的 30s。

### B3. `identify()` 在 ECONNREFUSED 时删除 socket 文件

- **已修**：ECONNREFUSED 先短延迟再连一次；仍拒绝才删，且**只删** `~/.ishell-mcp/mcp-*.sock`（反向转发孤儿），不动本机 `~/.config/ishell/` 下的存活 socket。Linux accept 队列满误删的窗口被收窄。

## C. 已排查、确认无问题的部分

| 区域 | 结论 |
| --- | --- |
| token / instance_id / socket 路径（`store/settings.rs`） | 稳定、0600、防 pid 回收、v2 文件名轮换逻辑正确 |
| `load_mcp_consent` 进程内缓存 | 显式 "0" 不会被默认值覆盖，正确 |
| 下载路径 `download_to_mcp` | size 承诺 + 尾字节检查 + verdict oneshot + `dl_rx` 回落 `resp_rx`，无悬挂 |
| agent 上传（`copy_to_remote_from_caller`） | biased select 防 EPIPE 盖掉真实错误、拒绝即时停写、尺寸承诺校验 |
| `drain_mcp_calls` / reclaim / tombstone / `arm_timeout_repaint` | deadline 与 `last_poll_at + RECLAIM_IDLE` 双排重绘，有测试覆盖 |
| SSH 反向转发生命周期 | nonce 路径防冲突、24h 清扫、6h touch 心跳、断开时 3s 注册窗口收尾 |
| `mcp_deploy` | 架构探测、事务写、chmod 755、`--version` 验证 |
| `advance_cross_copy_jobs` 状态机 | 迟到事件、临时公钥撤销、残留告警处理完整 |

## D. 多机场景

见 **`docs/mcp-multimachine-analysis.md`**。P1 改绑、P3 配置兜底告警、P4 主机名检测、I2/I5 诊断文案已落地。P2 显式 Rebind 工具、P6 密码提示符注入窗口仍是刻意保留的边界。
