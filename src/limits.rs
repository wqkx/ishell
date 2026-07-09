//! 文件打开 / 编辑器内存相关上限（集中定义，避免魔法数字散落）。

/// 普通打开确认阈值：超过则弹确认并以只读打开；以下直接打开、可编辑。
pub const FILE_SOFT_LIMIT: u64 = 20 * 1024 * 1024;
/// 强制打开硬上限：超过则拒绝（即使确认打开）。
pub const FILE_HARD_LIMIT: u64 = 128 * 1024 * 1024;
/// 超过此大小的已打开标签视为「大文件」（默认只读 + 并发计数）。
pub const LARGE_FILE_BYTES: usize = FILE_SOFT_LIMIT as usize;
/// 同时打开的大文件标签上限（整文件驻留内存，防止多标签拖垮本机）。
pub const MAX_LARGE_TABS: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    // 编译期锁定软/硬上限关系，避免运行时对常量 assert 触发 clippy。
    const _: () = assert!(FILE_SOFT_LIMIT < FILE_HARD_LIMIT);
    const _: () = assert!(LARGE_FILE_BYTES as u64 == FILE_SOFT_LIMIT);

    #[test]
    fn file_limits_match_documented_defaults() {
        assert_eq!(FILE_SOFT_LIMIT, 20 * 1024 * 1024);
        assert_eq!(FILE_HARD_LIMIT, 128 * 1024 * 1024);
        assert_eq!(MAX_LARGE_TABS, 2);
    }
}
