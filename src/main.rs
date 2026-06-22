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

    // 强制 X11（XWayland）：Wayland 下 winit 类应用 fcitx/输入法常失效（与 Chrome/Electron 同病），
    // 清空 WAYLAND_DISPLAY 让 winit 退回 X11（其 XIM 输入法正常）。须在 eframe/winit 初始化前。
    // 由持久化设置或环境变量 ISHELL_X11 开启；仅 Linux 有意义。
    #[cfg(target_os = "linux")]
    if store::load_force_x11() || std::env::var_os("ISHELL_X11").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
        log::info!("已强制 X11 后端（清空 WAYLAND_DISPLAY）以修复输入法");
    }

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
            // app_id 必须与 Linux 桌面项 ishell.desktop 的基名/StartupWMClass 完全一致，
            // GNOME 等用它匹配 .desktop 取图标（不读窗口内嵌 _NET_WM_ICON）；统一小写避免大小写匹配失败
            .with_app_id("ishell")
            .with_icon(load_icon())
    };
    let native_options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu, // 跨平台色彩一致，修复 Windows 颜色洗白
        ..Default::default()
    };

    eframe::run_native(
        "iShell",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
