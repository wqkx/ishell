//! 内嵌的 `ishell-mcp` 代理二进制，用于「一键把 AI 控制代理部署到当前服务器」。
//!
//! ## 这是干什么用的
//!
//! AI（Claude Code / Codex）通过 `ishell-mcp` 这个代理进程连回 iShell。代理必须跑在
//! **AI 所在的那台机器**上——而很多人的 AI 就跑在自己 SSH 上去的服务器里。此前那台服务器上
//! 的代理得手工装：从 release 里下对版本、scp 上去、跑 `scripts/install-mcp.sh`。版本还必须
//! 和 GUI 严格一致，否则连上就报「版本不一致，请重新部署」。
//!
//! 把代理二进制直接嵌进 ishell 之后，这一串变成一次点击：iShell 已经有到那台服务器的 SFTP
//! 通道，`uname -m` 挑架构、传上去、置可执行位、原子换入即可。版本一致由构造保证——嵌进来的
//! 那份就是和本 GUI 一起编出来的。
//!
//! ## 没嵌的时候
//!
//! `AGENTS` 为空（见 build.rs：不设 `ISHELL_EMBED_MCP_*` 就不嵌），部署入口不显示，
//! 手工安装那条路原样保留。日常开发 `cargo check` 因此不需要先编代理。

include!(concat!(env!("OUT_DIR"), "/embedded_mcp.rs"));

/// 部署到远端的标准位置（与 `scripts/install-mcp.sh` 一致，MCP 客户端配置里填的就是它）。
pub const REMOTE_DIR: &str = ".ishell-mcp/bin";
/// 部署后的文件名。
pub const REMOTE_NAME: &str = "ishell-mcp";

/// 按远端 `uname -m` 的输出挑一份代理二进制。
///
/// `uname -m` 在 64 位 ARM 上有 `aarch64` / `arm64` 两种写法（Linux 报前者，某些系统报后者），
/// x86_64 上也有 `amd64` 的别名，这里都认。
pub fn agent_for_uname(uname_m: &str) -> Option<&'static [u8]> {
    let arch = match uname_m.trim().to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return None,
    };
    AGENTS.iter().find(|(a, _)| *a == arch).map(|(_, b)| *b)
}

/// 本次构建到底嵌了哪些架构（UI 里用于决定要不要显示部署入口、以及提示支持范围）。
pub fn embedded_arches() -> Vec<&'static str> {
    AGENTS.iter().map(|(a, _)| *a).collect()
}

/// 是否嵌了任何代理二进制。
pub fn has_embedded() -> bool {
    !AGENTS.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 架构别名必须都能认出来：认错了就会把 x86 的二进制推到 ARM 机器上，
    /// 用户拿到的是一句莫名其妙的 "cannot execute binary file"。
    #[test]
    fn uname_aliases_map_to_the_same_arch() {
        // 没嵌任何东西时（日常 cargo test 就是这种情况）只能验「都返回 None」，
        // 嵌了的话则要求别名之间结果一致。
        assert_eq!(
            agent_for_uname("amd64").is_some(),
            agent_for_uname("x86_64").is_some()
        );
        assert_eq!(
            agent_for_uname("arm64").is_some(),
            agent_for_uname("aarch64").is_some()
        );
        assert!(agent_for_uname("  X86_64\n").is_some() == agent_for_uname("x86_64").is_some());
    }

    /// 嵌进来的必须真是一个 Linux 可执行文件。构建时把 `ISHELL_EMBED_MCP_*` 指错
    /// （比如指到了 `.d` 依赖文件或某个脚本）编译一样过，但推到服务器上才发现跑不起来。
    #[test]
    fn every_embedded_blob_is_an_elf() {
        for (arch, blob) in AGENTS {
            assert!(
                blob.starts_with(b"\x7fELF"),
                "{arch} 那份嵌入的二进制不是 ELF（前 4 字节 {:?}）——ISHELL_EMBED_MCP_* 多半指错了文件",
                &blob[..blob.len().min(4)]
            );
            // ELF header 第 5 字节：1=32 位 2=64 位；第 19-20 字节是机器类型
            // （0x3e=x86-64，0xb7=AArch64）。只校验位宽与架构对得上，够挡住「拿 x86 的
            // 二进制去填 aarch64 那一格」这种最容易犯的错。
            assert_eq!(blob[4], 2, "{arch}：不是 64 位 ELF");
            let machine = u16::from_le_bytes([blob[18], blob[19]]);
            let want = match *arch {
                "x86_64" => 0x3e,
                "aarch64" => 0xb7,
                other => panic!("未知架构标签 {other}"),
            };
            assert_eq!(machine, want, "{arch}：ELF 机器类型是 {machine:#x}，架构填错了");
        }
    }

    #[test]
    fn unknown_arch_is_none() {
        assert!(agent_for_uname("riscv64").is_none());
        assert!(agent_for_uname("").is_none());
    }
}
