//! 现代浅色主题 + 中文字体加载。

use egui::{Color32, CornerRadius, Stroke};

/// 中文后备字体的全局缩放（UI 上让中英文同观感、按钮内垂直居中）。
/// 终端按此值反向放大以抵消缩小，从而减小全角字之间的间距。
pub const CJK_SCALE: f32 = 0.92;

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

    // 统一小圆角
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

    // egui 0.34 按明/暗主题分别存 Style；`set_global_style` 只改「当前激活主题」那一份。
    // Windows 浅色模式下激活的是浅色槽 → 会落到 egui 默认浅色样式（灰底 #f8f8f8 + 默认间距），
    // 我们的自定义样式被忽略。故：①固定浅色主题（不跟随系统深色）；
    // ②把自定义样式写入明/暗两个槽，确保任何系统主题下都生效。
    ctx.set_theme(egui::ThemePreference::Light);
    ctx.all_styles_mut(|s| *s = style.clone());
}

/// 尝试加载系统中常见的中文字体，让远程中文输出/文件名正常显示。
fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        // Linux：Noto CJK / 文泉驿
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        // macOS：苹方为主，附带多种系统中文字体作后备（不同 macOS 版本路径/字体不尽相同）
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/Library/Fonts/Arial Unicode.ttf",
        // Windows：微软雅黑 / 黑体 / 宋体
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];

    let mut fonts = egui::FontDefinitions::default();

    // Phosphor 图标字体（按钮/文件类型图标）
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

    // 系统中文字体（作为各字体族的后备）。
    // 逐个候选读取，并**校验该字面确实能渲染汉字「中」**——避免选到一个不含 CJK 字形的
    // 字面（如 macOS 上字体集合的家族名被本地化为「苹方-简」时旧逻辑按 "SC" 匹配会落空），
    // 那种情况下中文会全部显示为「口」字方块。
    if let Some((path, data, idx)) = candidates.iter().find_map(|p| {
        let data = std::fs::read(p).ok()?;
        let idx = cjk_face_index(&data)?; // None = 该文件无可渲染汉字的字面，跳过
        Some((*p, data, idx))
    }) {
        log::info!("加载中文字体：{path}（face #{idx}）");
        // 选含汉字的字面作为各字体族的「后备」：英文/ASCII 仍走系统默认等宽字体
        // （终端英文更好看），仅缺字（中文）时回退到该 CJK 字体。
        let mut fd = egui::FontData::from_owned(data);
        fd.index = idx;
        // 中文（Noto CJK 等）的垂直度量比 Latin 字体更高、字面更满，按钮上会显得偏大且靠下。
        // 略缩小并上移，使中英文在同一行/按钮内大小观感一致、垂直居中。
        fd.tweak = egui::FontTweak {
            scale: CJK_SCALE,
            y_offset_factor: -0.06,
            ..Default::default()
        };
        fonts.font_data.insert("cjk".to_owned(), std::sync::Arc::new(fd));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".to_owned());
        }
    } else {
        log::warn!("未找到系统中文字体，中文可能显示为方块");
    }

    ctx.set_fonts(fonts);
}

/// 取某个字面的 Family 名（name_id=1）。
fn face_family(face: &ttf_parser::Face) -> Option<String> {
    face.names()
        .into_iter()
        .filter(|n| n.name_id == ttf_parser::name_id::FAMILY)
        .find_map(|n| n.to_string())
}

/// 在字体集合中挑出**确实能渲染汉字**的字面索引。
/// 必要条件：该字面含汉字「中」的字形（直接按字形表校验，不靠家族名——因为
/// macOS 苹方等字体家族名可能被本地化为「苹方-简」，按 "SC" 匹配会落空）。
/// 偏好顺序：简体且非 Mono > 非 Mono > 任意含汉字的字面。返回 None 表示该文件不含 CJK。
fn cjk_face_index(data: &[u8]) -> Option<u32> {
    let n = ttf_parser::fonts_in_collection(data).unwrap_or(1);
    let mut any_cjk = None; // 任意能渲染「中」的字面
    let mut non_mono = None; // 能渲染「中」且非 Mono
    for i in 0..n {
        let Ok(face) = ttf_parser::Face::parse(data, i) else { continue };
        // 关键校验：该字面必须能渲染汉字，否则中文会是「口」字方块
        if face.glyph_index('中').is_none() {
            continue;
        }
        any_cjk.get_or_insert(i);
        let fam = face_family(&face).unwrap_or_default();
        if fam.contains("Mono") {
            continue; // 等宽 CJK 面字距偏大，UI 上不优先
        }
        if fam.contains("SC") || fam.contains('简') {
            return Some(i); // 优先简体
        }
        non_mono.get_or_insert(i);
    }
    non_mono.or(any_cjk)
}
