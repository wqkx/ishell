//! iShell —— 现代化 Rust SSH 客户端。
//!
//! 布局：顶部会话标签；左侧系统信息（CPU/内存/磁盘/网络/进程）；
//! 中间交互式终端；右下 SFTP 文件操作区。

// 发布构建下隐藏 Windows 控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod crash;
mod i18n;
mod limits;
mod local;
mod mcp_embed;
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

/// 窗口的应用标识。必须与 `assets/linux/ishell.desktop` 的 `StartupWMClass` 完全一致——
/// 桌面环境靠这对值把「正在运行的窗口」认成「那个 .desktop 描述的应用」，对不上就会出现
/// 「点系统通知又开一个新实例」这类现象。有测试守着两边不许走散。
const APP_ID: &str = "ishell";

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("ishell {}", version::VERSION);
        return Ok(());
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 必须在日志之后、eframe 之前：桌面项启动时 stderr 无人可见，出事得有一份可交的日志
    crash::install_panic_hook();

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

    // 关于 X11 下的 WM_CLASS：下面的 `with_app_id("ishell")` 在 X11 上**也是生效的**——
    // winit 在 Linux 把 `platform_specific.name` 做成 x11/wayland 共用字段，egui-winit 虽然
    // 只在 wayland 分支调 `with_name`，X11 后端读的是同一个值。实测 `xprop WM_CLASS` 得到
    // `"", "ishell"`：instance 段空、class 段是 ishell，而 GNOME 匹配 StartupWMClass 用的
    // 正是 class 段。曾经怀疑是"二进制文件名变成了 WM_CLASS"（winit 在 name 为 None 时确实
    // 会退回 argv[0]）并试图用 RESOURCE_NAME 兜底——那条路走不通，name 永远是 Some，
    // 环境变量根本不会被读。别再往这儿加。

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
            .with_app_id(APP_ID)
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

#[cfg(test)]
mod desktop_entry_tests {
    /// 直接编进测试里，改坏了当场挂——这个文件是被 `scripts/bump-version.sh` 自动改过的，
    /// 靠人眼复查守不住。
    const DESKTOP: &str = include_str!("../assets/linux/ishell.desktop");

    fn value_of(key: &str) -> Option<&'static str> {
        DESKTOP
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::trim))
    }

    /// `.desktop` 的 `Version=` 按规范是**桌面项规范的版本**，不是程序版本号。
    ///
    /// 早先 bump-version.sh 把应用版本写了进去（`Version=0.16.13`），
    /// `desktop-file-validate` 直接报 error。非法的桌面项可能被桌面环境拒收或部分忽略，
    /// 而 StartupWMClass 索引、菜单项都指着它。
    #[test]
    fn version_key_is_a_spec_version() {
        let v = value_of("Version").expect("桌面项应有 Version");
        assert!(
            ["1.0", "1.1", "1.4", "1.5"].contains(&v),
            "Version={v} 不是桌面项规范的版本号——是不是又把应用版本写进去了？"
        );
    }

    /// 窗口的 app_id 与桌面项的 StartupWMClass 必须一字不差，否则桌面环境认不出
    /// 「这个窗口就是那个应用」。
    #[test]
    fn startup_wm_class_matches_the_window_app_id() {
        assert_eq!(value_of("StartupWMClass"), Some(super::APP_ID));
    }

    /// 只能有一个主类目，否则应用会在菜单里出现两次（desktop-file-validate 的 hint）。
    #[test]
    fn categories_declare_a_single_main_category() {
        const MAIN: [&str; 12] = [
            "AudioVideo", "Audio", "Video", "Development", "Education", "Game", "Graphics",
            "Network", "Office", "Science", "Settings", "System",
        ];
        let cats = value_of("Categories").unwrap_or_default();
        let n = cats.split(';').filter(|c| MAIN.contains(c)).count();
        assert_eq!(n, 1, "Categories={cats} 里有 {n} 个主类目，应当只有一个");
    }
}
