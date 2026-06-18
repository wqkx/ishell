//! iShell —— 现代化 Rust SSH 客户端（仿 FinalShell 布局）。
//!
//! 布局：顶部会话标签；左侧系统信息（CPU/内存/磁盘/网络/进程）；
//! 中间交互式终端；右下 SFTP 文件操作区。

// 发布构建下隐藏 Windows 控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod proto;
mod ssh;
mod store;
mod terminal;
mod theme;
mod ui;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("iShell — Rust SSH 客户端"),
        ..Default::default()
    };

    eframe::run_native(
        "iShell",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
