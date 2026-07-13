//! App 的辅助窗口渲染方法（看图 / GPU 详情 / 进程详情 / 端口转发 / toast / 传输列表）。
//! 均为 `impl App` 方法，签名与调用点不变；从 God Object 物理拆出以缩小 mod.rs。

mod forward;
mod image;
mod popups;
mod toast;
mod transfer;
