//! 现代浅色主题 + 中文字体加载。

use egui::{Color32, CornerRadius, Stroke};

/// 主题色板：参考 **Claude / Anthropic** 官网风格——暖米白底 + Claude 珊瑚橙强调色。
pub struct Palette;
impl Palette {
    /// 窗口背景（暖米白，略深）
    pub const BG: Color32 = Color32::from_rgb(0xeb, 0xe8, 0xe0);
    /// 面板背景（象牙白卡片）
    pub const PANEL: Color32 = Color32::from_rgb(0xfa, 0xf9, 0xf5);
    /// 次级面板 / 选项卡条（bone）
    pub const PANEL_2: Color32 = Color32::from_rgb(0xf0, 0xee, 0xe6);
    /// 进度条轨道（暖灰）
    pub const TRACK: Color32 = Color32::from_rgb(0xe3, 0xdf, 0xd3);
    /// 分隔线 / 边框（加深一档，让面板/分区边界更清晰）
    pub const BORDER: Color32 = Color32::from_rgb(0xcd, 0xc7, 0xb5);
    /// 主强调色（Claude 珊瑚橙）
    pub const ACCENT: Color32 = Color32::from_rgb(0xd9, 0x77, 0x57);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xf5, 0xe4, 0xda);
    /// 文本（暖近黑 / 暖灰）
    pub const TEXT: Color32 = Color32::from_rgb(0x2a, 0x28, 0x24);
    /// 次要文字（加深以满足 WCAG AA 小字对比）
    pub const TEXT_DIM: Color32 = Color32::from_rgb(0x54, 0x51, 0x4a);
    /// 语义色（柔和暖调）
    pub const OK: Color32 = Color32::from_rgb(0x5b, 0x8a, 0x56);
    pub const WARN: Color32 = Color32::from_rgb(0xc2, 0x8e, 0x3c);
    pub const DANGER: Color32 = Color32::from_rgb(0xc0, 0x56, 0x4b);
    /// 终端背景（比面板更深一些的暖灰）与默认前景
    pub const TERM_BG: Color32 = Color32::from_rgb(0xd7, 0xd2, 0xc4);
    pub const TERM_FG: Color32 = Color32::from_rgb(0x2a, 0x28, 0x24);
}

/// 应用全局视觉风格。
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut style = (*ctx.global_style()).clone();
    // 从浅色预设出发，避免残留的深色字段（如窗口标题栏发黑）
    style.visuals = egui::Visuals::light();
    let v = &mut style.visuals;
    v.dark_mode = false;
    v.override_text_color = Some(Palette::TEXT);
    v.panel_fill = Palette::BG;
    v.window_fill = Palette::PANEL;
    v.window_stroke = Stroke::new(1.0, Palette::BORDER);
    v.extreme_bg_color = Palette::PANEL_2;
    v.faint_bg_color = Palette::PANEL_2;
    v.hyperlink_color = Palette::ACCENT;
    v.selection.bg_fill = Palette::ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0, Palette::ACCENT);
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: Color32::from_black_alpha(50),
    };
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 3],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(40),
    };
    v.window_corner_radius = CornerRadius::same(10);

    // 统一小圆角（FinalShell 风格的小圆角矩形）
    let r = CornerRadius::same(4);

    // 非交互（标签、分隔线）
    v.widgets.noninteractive.bg_fill = Palette::PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Palette::BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Palette::TEXT);
    v.widgets.noninteractive.corner_radius = r;

    // 普通控件（按钮静止）：简洁，浅底无边框
    v.widgets.inactive.bg_fill = Palette::PANEL_2;
    v.widgets.inactive.weak_bg_fill = Palette::PANEL_2;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, Palette::TEXT);
    v.widgets.inactive.corner_radius = r;
    v.widgets.inactive.expansion = 0.0;

    // 悬停：轻微变深，无边框
    v.widgets.hovered.bg_fill = Palette::TRACK;
    v.widgets.hovered.weak_bg_fill = Palette::TRACK;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, Palette::TEXT);
    v.widgets.hovered.corner_radius = r;
    v.widgets.hovered.expansion = 0.0;

    // 按下 / 激活
    v.widgets.active.bg_fill = Palette::ACCENT;
    v.widgets.active.weak_bg_fill = Palette::ACCENT;
    v.widgets.active.bg_stroke = Stroke::NONE;
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.active.corner_radius = r;
    v.widgets.active.expansion = 0.0;

    // 选中态（open/selected）
    v.widgets.open.bg_fill = Palette::PANEL_2;
    v.widgets.open.bg_stroke = Stroke::new(1.0, Palette::BORDER);
    v.widgets.open.fg_stroke = Stroke::new(1.0, Palette::TEXT);
    v.widgets.open.corner_radius = r;

    // 关闭标签文本选中：悬停文件/文件夹显示普通指针而非文本 I 形光标
    style.interaction.selectable_labels = false;
    style.interaction.multi_widget_text_select = false;

    // 间距：行高更舒展
    style.spacing.item_spacing = egui::vec2(6.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(12);
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size.y = 26.0;

    ctx.set_global_style(style);
}

/// 尝试加载系统中常见的中文字体，让远程中文输出/文件名正常显示。
fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
    ];

    let mut fonts = egui::FontDefinitions::default();

    // Phosphor 图标字体（按钮/文件类型图标）
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    // 系统中文字体（作为各字体族的后备）
    if let Some((path, data)) = candidates
        .iter()
        .find_map(|p| std::fs::read(p).ok().map(|d| (*p, d)))
    {
        log::info!("加载中文字体：{path}");
        // 比例族：用普通（Sans）CJK 面作为后备
        let prop_idx = cjk_face_index(&data, false);
        let mut prop_fd = egui::FontData::from_owned(data.clone());
        prop_fd.index = prop_idx;
        fonts.font_data.insert("cjk".to_owned(), std::sync::Arc::new(prop_fd));
        fonts.families.entry(egui::FontFamily::Proportional).or_default().push("cjk".to_owned());

        // 等宽族：若字体集合内含等宽（Mono）CJK 面，则**前置**为主字体——
        // 其 Latin 为半角、CJK 为全角，恰好 1:2，终端按 2 格绘制 CJK 不再有多余间距。
        // 找不到等宽面时退回用普通 CJK 作后备（仅消除方块，间距略宽，可接受）。
        match cjk_mono_face_index(&data) {
            Some(mono_idx) => {
                log::info!("终端使用等宽 CJK 字面 index={mono_idx}");
                let mut mono_fd = egui::FontData::from_owned(data);
                mono_fd.index = mono_idx;
                fonts.font_data.insert("cjk-mono".to_owned(), std::sync::Arc::new(mono_fd));
                fonts.families.entry(egui::FontFamily::Monospace).or_default().insert(0, "cjk-mono".to_owned());
            }
            None => {
                fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".to_owned());
            }
        }
    } else {
        log::warn!("未找到系统中文字体，中文可能显示为方块");
    }

    ctx.set_fonts(fonts);
}

/// 取字体集合中某个字面的 Family 名（name_id=1）。
fn face_family(data: &[u8], index: u32) -> Option<String> {
    let face = ttf_parser::Face::parse(data, index).ok()?;
    face.names()
        .into_iter()
        .filter(|n| n.name_id == ttf_parser::name_id::FAMILY)
        .find_map(|n| n.to_string())
}

/// 选择普通（非 Mono）CJK 字面索引；优先简体（SC），否则第一个非 Mono 面，再不行用 0。
fn cjk_face_index(data: &[u8], _prop: bool) -> u32 {
    let n = ttf_parser::fonts_in_collection(data).unwrap_or(1);
    let mut first_non_mono = None;
    for i in 0..n {
        let Some(fam) = face_family(data, i) else { continue };
        if fam.contains("Mono") {
            continue;
        }
        if fam.contains("SC") {
            return i;
        }
        first_non_mono.get_or_insert(i);
    }
    first_non_mono.unwrap_or(0)
}

/// 选择等宽（Mono）CJK 字面索引；优先简体（SC）。无 Mono 面则返回 None。
fn cjk_mono_face_index(data: &[u8]) -> Option<u32> {
    let n = ttf_parser::fonts_in_collection(data).unwrap_or(1);
    let mut first_mono = None;
    for i in 0..n {
        let Some(fam) = face_family(data, i) else { continue };
        if !fam.contains("Mono") {
            continue;
        }
        if fam.contains("SC") {
            return Some(i);
        }
        first_mono.get_or_insert(i);
    }
    first_mono
}
