//! 构建脚本：
//! 1. Windows 上把应用图标（assets/icon.ico）嵌入 exe，使资源管理器/任务栏显示 logo；
//! 2. 把预先编好的 `ishell-mcp` 代理二进制嵌进 ishell，供「一键部署到服务器」使用。
//!
//! ## 为什么代理二进制要从环境变量喂进来，而不是这里现编
//!
//! `ishell-mcp` 和 `ishell` 是**同一个 crate 的两个 `[[bin]]`**。构建脚本跑在编译这个 crate
//! 的过程**之中**，那时 `ishell-mcp` 还没有产物，`include_bytes!` 无从谈起；在 build.rs 里递归
//! 调 cargo 去编自己更是死路。所以顺序必须由外部保证：先 `cargo build --bin ishell-mcp`，
//! 再把产物路径经 `ISHELL_EMBED_MCP_<ARCH>` 交给这次 `cargo build --bin ishell`。
//!
//! 一个都不设时**不报错**，只是不嵌（`ishell::mcp_embed::AGENTS` 为空、UI 里那个部署入口
//! 不出现）。日常 `cargo check`/`cargo test` 因此照常能跑，不必先去编代理。

use std::io::Write;

/// 支持嵌入的目标：(环境变量名, 该二进制适用的 `uname -m` 值)。
///
/// 只做 Linux：这个功能的用途是「把代理推到我 SSH 上去的那台服务器上跑」，那头基本都是
/// Linux。macOS 服务器不存在，Windows 服务器也不走这条路。
const TARGETS: &[(&str, &str)] = &[
    ("ISHELL_EMBED_MCP_X86_64", "x86_64"),
    ("ISHELL_EMBED_MCP_AARCH64", "aarch64"),
];

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

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo 必然提供 OUT_DIR"));
    let mut src = String::from(
        "// 由 build.rs 生成，勿手改。\n\
         pub static AGENTS: &[(&str, &[u8])] = &[\n",
    );
    for (var, arch) in TARGETS {
        println!("cargo:rerun-if-env-changed={var}");
        let Ok(path) = std::env::var(var) else { continue };
        if path.trim().is_empty() {
            continue;
        }
        // 路径写死进生成的源码里，故也要跟着它重跑。
        println!("cargo:rerun-if-changed={path}");
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => {
                src.push_str(&format!(
                    "    ({arch:?}, include_bytes!({path:?})),\n"
                ));
            }
            _ => panic!(
                "{var}={path} 指向的文件不存在或不是普通文件。\
                 要么把它指到 `cargo build --release --bin ishell-mcp` 的产物上，要么别设这个变量。"
            ),
        }
    }
    src.push_str("];\n");
    let dest = out.join("embedded_mcp.rs");
    let mut f = std::fs::File::create(&dest).expect("写生成文件失败");
    f.write_all(src.as_bytes()).expect("写生成文件失败");
}
