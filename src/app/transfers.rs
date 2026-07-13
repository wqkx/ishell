//! App 的传输/中转/直传/粘贴相关方法（从 God Object 拆出，行为不变）。
//! 这些都是 `impl App` 的方法，签名与调用点不变；仅物理迁移以缩小 mod.rs。

#[path = "transfers_clipboard.rs"]
mod clipboard;
#[path = "transfers_direct.rs"]
mod direct;
#[path = "transfers_relay.rs"]
mod relay;
