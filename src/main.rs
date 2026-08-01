//! iShell —— 现代化 Rust SSH 客户端。
//!
//! 布局：顶部会话标签；左侧系统信息（CPU/内存/磁盘/网络/进程）；
//! 中间交互式终端；右下 SFTP 文件操作区。

// 发布构建下隐藏 Windows 控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod limits;
mod local;
mod mcp_protocol;
mod proto;
mod ssh;
mod store;
mod terminal;
mod textcodec;
mod theme;
mod ui;
mod version;

/// 应用图标（任务栏/窗口/Alt-Tab）。编译期内嵌 PNG，运行时解码为 RGBA。
fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/icon.png");
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let img = img.into_rgba8();
            let (width, height) = img.dimensions();
            egui::IconData {
                rgba: img.into_raw(),
                width,
                height,
            }
        }
        Err(_) => egui::IconData::default(),
    }
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("ishell {}", version::VERSION);
        return Ok(());
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 尽早加载界面语言：窗口标题等在 App::new 之前创建，需要语言已就位才能本地化（App::new 再设一次无妨）。
    if let Some(code) = store::load_lang() {
        i18n::set(i18n::Lang::from_code(&code));
    }

    // 强制 X11（XWayland）：Wayland 下 winit 类应用 fcitx/输入法常失效（与 Chrome/Electron 同病），
    // 清空 WAYLAND_DISPLAY 让 winit 退回 X11（其 XIM 输入法正常）。须在 eframe/winit 初始化前。
    // 由持久化设置或环境变量 ISHELL_X11 开启；仅 Linux 有意义。
    #[cfg(target_os = "linux")]
    if store::load_force_x11() || std::env::var_os("ISHELL_X11").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
        log::info!("已强制 X11 后端（清空 WAYLAND_DISPLAY）以修复输入法");
    }

    // X11 下把 WM_CLASS 的 instance 段钉成 "ishell"，与 ishell.desktop 的 StartupWMClass 对齐。
    //
    // 为什么需要这一手：下面的 `with_app_id("ishell")` **只在 Wayland 上生效**——egui-winit
    // 把它转成 winit 的 Wayland `with_name`，X11 那条分支根本没调用对应的 X11 `with_name`
    // （后者才写 WM_CLASS）。于是 X11 下 winit 退回默认值：**用 argv[0] 的文件名当 WM_CLASS**。
    //
    // 后果很具体：二进制若叫 `ishell-linux-x86_64`（发布产物就是这个名字），WM_CLASS 就成了
    // `ishell-linux-x86_64`，与 StartupWMClass=ishell 对不上；GNOME 因此无法把桌面通知/任务栏
    // 关联到**正在运行的那个窗口**，点通知只能按 .desktop 的 Exec= 再开一个新实例。
    //
    // winit 在取默认值时会先读 `RESOURCE_NAME` 作为 instance 段，而 GNOME 匹配
    // StartupWMClass 时 class 与 instance 两段都认——所以设这个环境变量即可，且不必要求
    // 用户把二进制改名。已显式设过的不覆盖（尊重用户/发行版的安排）。
    #[cfg(target_os = "linux")]
    if std::env::var_os("RESOURCE_NAME").is_none() {
        std::env::set_var("RESOURCE_NAME", "ishell");
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
            .with_title(i18n::tr(
                "iShell — Rust SSH 客户端",
                "iShell — Rust SSH Client",
            ))
            // app_id 必须与 Linux 桌面项 ishell.desktop 的基名/StartupWMClass 完全一致，
            // GNOME 等用它匹配 .desktop 取图标（不读窗口内嵌 _NET_WM_ICON）；统一小写避免大小写匹配失败
            .with_app_id("ishell")
            .with_icon(load_icon())
    };
    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "iShell",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
