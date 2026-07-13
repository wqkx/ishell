//! 构建脚本：仅在 Windows 上把应用图标（assets/icon.ico）嵌入 exe，
//! 使资源管理器/任务栏在程序文件上显示 logo。其它平台不做任何事。

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        let ver = env!("CARGO_PKG_VERSION");
        res.set("FileVersion", ver);
        res.set("ProductVersion", ver);
        if let Err(e) = res.compile() {
            eprintln!("嵌入 Windows 图标失败：{e}");
        }
    }
}
