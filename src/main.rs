//! iShell —— 现代化 Rust SSH 客户端。
//!
//! 布局：顶部会话标签；左侧系统信息（CPU/内存/磁盘/网络/进程）；
//! 中间交互式终端；右下 SFTP 文件操作区。

// 发布构建下隐藏 Windows 控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod proto;
mod ssh;
mod store;
mod terminal;
mod theme;
mod ui;

/// 应用图标（任务栏/窗口/Alt-Tab）。编译期内嵌 PNG，运行时解码为 RGBA。
fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/icon.png");
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let img = img.into_rgba8();
            let (width, height) = img.dimensions();
            egui::IconData { rgba: img.into_raw(), width, height }
        }
        Err(_) => egui::IconData::default(),
    }
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Logo / 图标生成模式：窄长（logo）或方形（icon）画布，用于截图生成素材
    let logo = std::env::var("ISHELL_LOGO").is_ok();
    let icon_gen = std::env::var("ISHELL_ICON").is_ok();
    let viewport = if icon_gen {
        egui::ViewportBuilder::default().with_inner_size([256.0, 256.0])
    } else if logo {
        egui::ViewportBuilder::default().with_inner_size([440.0, 156.0])
    } else {
        egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("iShell — Rust SSH 客户端")
            .with_icon(load_icon())
    };
    let native_options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "iShell",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
