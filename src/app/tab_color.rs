//! 会话标签的颜色标记。一组压过暖米白底、彼此分得开的颜色，用法接近 macOS 的文件夹颜色标签。

use egui::Color32;

/// 内置标签色。没有「灰」：底色本身就是暖灰，灰标签看不出来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabColor {
    Coral,
    Rose,
    Amber,
    Sage,
    Teal,
    Blue,
    Plum,
}

impl TabColor {
    pub(super) const ALL: [TabColor; 7] = [
        TabColor::Coral,
        TabColor::Rose,
        TabColor::Amber,
        TabColor::Sage,
        TabColor::Teal,
        TabColor::Blue,
        TabColor::Plum,
    ];

    /// 色点，以及选中标签底下那条线用的实色。明度靠近，避免某一颗特别扎眼。
    pub(super) fn rgb(self) -> Color32 {
        match self {
            TabColor::Coral => Color32::from_rgb(0xd1, 0x6a, 0x48),
            TabColor::Rose => Color32::from_rgb(0xc2, 0x5d, 0x6c),
            TabColor::Amber => Color32::from_rgb(0xc8, 0x96, 0x3e),
            TabColor::Sage => Color32::from_rgb(0x6d, 0x94, 0x60),
            TabColor::Teal => Color32::from_rgb(0x3f, 0x8f, 0x86),
            TabColor::Blue => Color32::from_rgb(0x5d, 0x73, 0xa6),
            TabColor::Plum => Color32::from_rgb(0x8a, 0x64, 0x8f),
        }
    }

    /// 菜单里的名字（中文，英文）。
    pub(super) fn label(self) -> (&'static str, &'static str) {
        match self {
            TabColor::Coral => ("珊瑚", "Coral"),
            TabColor::Rose => ("玫红", "Rose"),
            TabColor::Amber => ("琥珀", "Amber"),
            TabColor::Sage => ("苔绿", "Sage"),
            TabColor::Teal => ("青瓷", "Teal"),
            TabColor::Blue => ("靛蓝", "Indigo"),
            TabColor::Plum => ("梅子", "Plum"),
        }
    }

    /// 把这颗颜色以 `amount`（0..1）混进底色，给标签铺一层能看出来、但不盖住字的浅色。
    pub(super) fn tint(self, base: Color32, amount: f32) -> Color32 {
        let dye = self.rgb();
        let t = amount.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Color32::from_rgb(
            mix(base.r(), dye.r()),
            mix(base.g(), dye.g()),
            mix(base.b(), dye.b()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::TabColor;

    #[test]
    fn palette_colors_differ() {
        let mut seen = std::collections::HashSet::new();
        for c in TabColor::ALL {
            let col = c.rgb();
            assert!(
                seen.insert((col.r(), col.g(), col.b())),
                "duplicate tab color {c:?}"
            );
        }
        assert_eq!(seen.len(), TabColor::ALL.len());
    }
}
