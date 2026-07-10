//! app 内的独立辅助函数（无 App 状态耦合）：路径/时长格式化、跨平台打开目录、内存归还等。

/// 本地端口预占用探测（添加端口转发前）：仅当明确 `AddrInUse` 才判为占用；
/// 其它错误（如 bind 地址非本机网卡）返回 false——不武断拦截，交给 worker 实际绑定决定。
pub(crate) fn local_port_in_use(host: &str, port: u16) -> bool {
    let h = if host.trim().is_empty() {
        "127.0.0.1"
    } else {
        host
    };
    match std::net::TcpListener::bind((h, port)) {
        Ok(_) => false, // 立即 drop 释放，worker 随后真正绑定
        Err(e) => e.kind() == std::io::ErrorKind::AddrInUse,
    }
}

/// 绑定地址是否为回环（仅本机可连）。空串按 127.0.0.1 处理。
pub(crate) fn is_loopback_bind(host: &str) -> bool {
    let h = host.trim();
    h.is_empty() || h == "127.0.0.1" || h == "localhost" || h == "::1"
}

/// 解毒 Mutex：持锁线程 panic 后仍可恢复状态，避免 UI 线程二次 unwrap 崩溃。
pub(crate) fn lock_mutex<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 本机 ~/.ssh/known_hosts 是否已记录该主机（用于直传时选择 StrictHostKeyChecking）。
pub(crate) fn host_in_known_hosts(host: &str, port: u16) -> bool {
    russh::keys::known_hosts::known_host_keys(host, port)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// 取远端路径的所在目录：去尾斜杠后截到最后一个 `/`；根下或无斜杠返回 `/`。
pub(crate) fn parent_dir(path: &str) -> String {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => t[..i].to_string(),
    }
}

/// 把秒数格式化为紧凑时长（用于传输 ETA）：`45s` / `3m20s` / `1h2m`。
pub(crate) fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// 监控栏（左侧菜单栏）统一右键菜单：语言 / 字体大小 / 折叠视图 / 强制 X11。
/// 背景层与各子控件、以及折叠后的细条都调用它，保证右键处处一致。
/// 终端边框色：偏向终端底色 b 的加权混合（1/3 窗口色 a + 2/3 终端色 b）。
/// 比 50/50 更贴近终端——深色主题更深、浅色主题更接近自身底色，过渡更自然。
pub(crate) fn blend_color(a: egui::Color32, b: egui::Color32) -> egui::Color32 {
    let mix = |x: u8, y: u8| ((x as u16 + 2 * y as u16) / 3) as u8;
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// 把已释放的堆内存归还给操作系统（glibc）。关闭大文件编辑器后调用。
pub(crate) fn trim_memory() {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// 用系统文件管理器打开文件所在目录。
/// 在标签条某一侧绘制渐隐遮罩，提示该方向还有被滚动隐藏的标签。
/// `left=true` 左侧（实色在左、向右透明）；否则右侧（向右渐变为实色）。
pub(crate) fn edge_fade(painter: &egui::Painter, rect: egui::Rect, left: bool, bg: egui::Color32) {
    let w = 18.0_f32.min(rect.width());
    let transp = egui::Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), 0);
    let (x0, x1, c0, c1) = if left {
        (rect.left(), rect.left() + w, bg, transp)
    } else {
        (rect.right() - w, rect.right(), transp, bg)
    };
    let (t, b) = (rect.top(), rect.bottom());
    let mut mesh = egui::Mesh::default();
    let uv = egui::epaint::WHITE_UV;
    mesh.vertices.push(egui::epaint::Vertex {
        pos: egui::pos2(x0, t),
        uv,
        color: c0,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: egui::pos2(x1, t),
        uv,
        color: c1,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: egui::pos2(x1, b),
        uv,
        color: c1,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: egui::pos2(x0, b),
        uv,
        color: c0,
    });
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// 在文件管理器中打开并**选中**该文件（而不仅是打开目录）。
pub(crate) fn open_containing_folder(file: &str) {
    #[cfg(target_os = "windows")]
    {
        // explorer /select, 选中文件
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{file}"))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        // Finder 中显示并选中
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(file)
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        // 优先 freedesktop D-Bus ShowItems（nautilus/dolphin/nemo 等都支持），可选中文件；
        // 不可用时退回 xdg-open 仅打开所在目录。
        let uri = file_uri(file);
        let dbus = std::process::Command::new("dbus-send")
            .args([
                "--type=method_call",
                "--dest=org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .spawn();
        if dbus.is_err() {
            let dir = std::path::Path::new(file)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}

/// 把本地绝对路径转成百分号编码的 file:// URI。
#[cfg(target_os = "linux")]
pub(crate) fn file_uri(p: &str) -> String {
    let mut s = String::from("file://");
    for b in p.bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

/// 本地下载目录：优先用户主目录下的 Downloads，否则当前目录。
pub(crate) fn downloads_dir() -> std::path::PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    if let Some(home) = home {
        let d = std::path::Path::new(&home).join("Downloads");
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    std::path::PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::{fmt_dur, is_loopback_bind, parent_dir};

    #[test]
    fn loopback_bind_detects_common_hosts() {
        assert!(is_loopback_bind(""));
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("::1"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.1"));
    }

    #[test]
    fn parent_dir_basics() {
        assert_eq!(parent_dir("/a/b/c"), "/a/b");
        assert_eq!(parent_dir("/a"), "/");
        assert_eq!(parent_dir("name"), "/");
        assert_eq!(parent_dir("/a/b/"), "/a");
    }

    #[test]
    fn fmt_dur_compact() {
        assert_eq!(fmt_dur(0), "0s");
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(80), "1m20s");
        assert_eq!(fmt_dur(3600), "1h0m");
        assert_eq!(fmt_dur(3723), "1h2m");
    }
}
