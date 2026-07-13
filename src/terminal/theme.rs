//! 终端配色主题。

use egui::Color32;

/// 全局终端配色开关（深=true）：所有终端共享同一值，切一个即全部同步。
/// 首次访问时从持久化设置初始化。
/// 全局终端配色索引：所有终端共享同一值，切一个即全部同步。首次访问从持久化设置初始化。
pub(super) fn term_theme() -> &'static std::sync::atomic::AtomicU8 {
    static F: std::sync::OnceLock<std::sync::atomic::AtomicU8> = std::sync::OnceLock::new();
    F.get_or_init(|| std::sync::atomic::AtomicU8::new(crate::store::load_term_theme()))
}

/// 终端配色（背景/默认前景/ANSI 16 色），可在深/浅之间切换。
pub(super) struct TermColors {
    pub(super) bg: Color32,
    pub(super) fg: Color32,
    pub(super) ansi: [(u8, u8, u8); 16],
}

impl TermColors {
    /// 深色（经典控制台，暖调近黑底 + 高对比 ANSI）。
    pub(super) fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(0x1e, 0x1c, 0x19),
            fg: Color32::from_rgb(0xe6, 0xe1, 0xd6),
            ansi: [
                (0x33, 0x30, 0x2b),
                (0xe0, 0x6c, 0x60),
                (0x8c, 0xb8, 0x5f),
                (0xe0, 0xb0, 0x55),
                (0x6f, 0xa8, 0xdc),
                (0xc2, 0x8c, 0xd8),
                (0x5f, 0xbf, 0xc4),
                (0xd8, 0xd2, 0xc4),
                (0x6f, 0x6b, 0x61),
                (0xee, 0x82, 0x76),
                (0xa6, 0xcf, 0x73),
                (0xf0, 0xc6, 0x6b),
                (0x86, 0xbd, 0xea),
                (0xd2, 0xa0, 0xe6),
                (0x76, 0xd2, 0xd6),
                (0xf2, 0xed, 0xe2),
            ],
        }
    }
    /// 浅色（暖米底，ANSI 已为浅底调校）。
    pub(super) fn light() -> Self {
        Self {
            bg: crate::theme::Palette::TERM_BG,
            fg: crate::theme::Palette::TERM_FG,
            ansi: [
                (0x3a, 0x38, 0x33),
                (0xc0, 0x4b, 0x3f),
                (0x4f, 0x86, 0x4a),
                (0xb5, 0x82, 0x2e),
                (0x2f, 0x6f, 0xb0),
                (0xa6, 0x55, 0x9d),
                (0x2b, 0x8a, 0x8f),
                (0xb8, 0xb2, 0xa3),
                (0x6f, 0x6b, 0x61),
                (0xc0, 0x56, 0x4b),
                (0x5b, 0x8a, 0x56),
                (0xc2, 0x8e, 0x3c),
                (0x35, 0x78, 0xbb),
                (0xb0, 0x60, 0xa6),
                (0x30, 0x95, 0x9a),
                (0x55, 0x52, 0x4a),
            ],
        }
    }
    /// 近白（干净的近白底 + 深前景，ANSI 偏饱和以在白底上清晰）。
    fn paper() -> Self {
        Self {
            bg: Color32::from_rgb(0xfc, 0xfc, 0xfa),
            fg: Color32::from_rgb(0x2a, 0x2a, 0x2a),
            ansi: [
                (0x3a, 0x3a, 0x3a),
                (0xcc, 0x33, 0x33),
                (0x2e, 0x8b, 0x2e),
                (0xb8, 0x86, 0x0b),
                (0x20, 0x60, 0xc0),
                (0x9c, 0x27, 0xb0),
                (0x00, 0x97, 0xa7),
                (0x5a, 0x5a, 0x5a),
                (0x7a, 0x7a, 0x7a),
                (0xe5, 0x39, 0x35),
                (0x38, 0x8e, 0x3c),
                (0xc9, 0xa2, 0x27),
                (0x19, 0x76, 0xd2),
                (0x8e, 0x24, 0xaa),
                (0x00, 0x83, 0x8f),
                (0x1a, 0x1a, 0x1a),
            ],
        }
    }
    /// 柔和深色（Catppuccin Mocha，与界面 Latte 同族，暗变体）。
    fn mocha() -> Self {
        Self {
            bg: Color32::from_rgb(0x1e, 0x1e, 0x2e),
            fg: Color32::from_rgb(0xcd, 0xd6, 0xf4),
            ansi: [
                (0x45, 0x47, 0x5a),
                (0xf3, 0x8b, 0xa8),
                (0xa6, 0xe3, 0xa1),
                (0xf9, 0xe2, 0xaf),
                (0x89, 0xb4, 0xfa),
                (0xf5, 0xc2, 0xe7),
                (0x94, 0xe2, 0xd5),
                (0xba, 0xc2, 0xde),
                (0x58, 0x5b, 0x70),
                (0xf3, 0x8b, 0xa8),
                (0xa6, 0xe3, 0xa1),
                (0xf9, 0xe2, 0xaf),
                (0x89, 0xb4, 0xfa),
                (0xf5, 0xc2, 0xe7),
                (0x94, 0xe2, 0xd5),
                (0xa6, 0xad, 0xc8),
            ],
        }
    }
    /// 经典浅色（Solarized Light）。
    fn solarized_light() -> Self {
        Self {
            bg: Color32::from_rgb(0xfd, 0xf6, 0xe3),
            fg: Color32::from_rgb(0x65, 0x7b, 0x83),
            ansi: [
                (0x07, 0x36, 0x42),
                (0xdc, 0x32, 0x2f),
                (0x85, 0x99, 0x00),
                (0xb5, 0x89, 0x00),
                (0x26, 0x8b, 0xd2),
                (0xd3, 0x36, 0x82),
                (0x2a, 0xa1, 0x98),
                (0xee, 0xe8, 0xd5),
                (0x00, 0x2b, 0x36),
                (0xcb, 0x4b, 0x16),
                (0x58, 0x6e, 0x75),
                (0x65, 0x7b, 0x83),
                (0x83, 0x94, 0x96),
                (0x6c, 0x71, 0xc4),
                (0x93, 0xa1, 0xa1),
                (0xfd, 0xf6, 0xe3),
            ],
        }
    }

    /// 按索引取配色（与 [`TERM_THEMES`] 顺序一致）。
    pub(super) fn by_index(i: u8) -> Self {
        match i {
            0 => Self::dark(),
            2 => Self::paper(),
            3 => Self::mocha(),
            4 => Self::solarized_light(),
            _ => Self::light(), // 1 及越界
        }
    }
}

/// 当前终端主题的背景色（供外层窗口把边框做成「暖米→终端底色」的渐变过渡）。
pub fn current_bg() -> Color32 {
    TermColors::by_index(term_theme().load(std::sync::atomic::Ordering::Relaxed)).bg
}

/// 终端配色清单（索引序）：(中文名, 英文名)。
pub(super) const TERM_THEMES: &[(&str, &str)] = &[
    ("暖黑", "Warm dark"),
    ("暖米", "Warm light"),
    ("近白", "Paper"),
    ("柔和深", "Mocha"),
    ("经典浅", "Solarized light"),
];
