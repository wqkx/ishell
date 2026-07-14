//! 现代浅色主题 + 中文字体加载。

use egui::{Color32, CornerRadius, Stroke};

/// 中文后备字体的全局缩放（UI 上让中英文同观感、按钮内垂直居中）。
/// 终端按此值反向放大以抵消缩小，从而减小全角字之间的间距。
pub const CJK_SCALE: f32 = 0.92;

// ——— 设计令牌（统一圆角 / 字阶，全局复用，避免散落的魔法数字）———
/// 圆角刻度：控件 / 浮层·菜单 / 窗口
pub const R_SM: u8 = 6;
pub const R_MD: u8 = 8;
pub const R_LG: u8 = 12;
/// 字阶（pt）：注释 / 正文 / 强调 / 标题（供各 UI 复用，将逐步替换散落的硬编码字号）。
/// 暂允许未使用：本轮先建立令牌，字号统一替换作为后续单独一轮（跨多文件）进行。
#[allow(dead_code)]
pub const FS_NOTE: f32 = 11.0;
#[allow(dead_code)]
pub const FS_BODY: f32 = 12.0;
#[allow(dead_code)]
pub const FS_STRONG: f32 = 13.0;
#[allow(dead_code)]
pub const FS_TITLE: f32 = 16.0;

/// 主题色板：参考 **Claude / Anthropic** 官网风格——暖米白底 + Claude 珊瑚橙强调色。
pub struct Palette;
impl Palette {
    /// 窗口背景（暖米白，最底层画布）。注意：底色保持低饱和的克制灰调，
    /// 太黄/太花会淹没前景信息——暖色感交给强调色与小元素，不靠大面积底色。
    pub const BG: Color32 = Color32::from_rgb(0xec, 0xe9, 0xe1);
    /// 面板背景（象牙白卡片，较画布更亮以拉开层级）
    pub const PANEL: Color32 = Color32::from_rgb(0xfd, 0xfc, 0xf9);
    /// 次级面板 / 选项卡条（bone，提亮一档避免与画布糊在一起）
    pub const PANEL_2: Color32 = Color32::from_rgb(0xf4, 0xf1, 0xea);
    /// 进度条轨道 / 悬停底（暖灰）
    pub const TRACK: Color32 = Color32::from_rgb(0xe7, 0xe2, 0xd6);
    /// 信息卡片底（暖奶油：介于 PANEL_2 与 TRACK 之间，用于折线图/速率卡/磁盘卡等）
    pub const CARD: Color32 = Color32::from_rgb(0xef, 0xea, 0xdc);
    /// 分隔线 / 边框（调浅一档，少用硬边框、多靠柔影分区，更显轻盈）
    pub const BORDER: Color32 = Color32::from_rgb(0xdc, 0xd7, 0xc9);
    /// 主强调色（Claude 珊瑚橙，略加饱和让主操作更活泼）
    pub const ACCENT: Color32 = Color32::from_rgb(0xd9, 0x70, 0x49);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xf7, 0xe7, 0xdc);
    /// 文本（暖近黑 / 暖灰，非纯黑）
    pub const TEXT: Color32 = Color32::from_rgb(0x2a, 0x28, 0x24);
    /// 次要文字（加深以满足 WCAG AA 小字对比）
    pub const TEXT_DIM: Color32 = Color32::from_rgb(0x54, 0x51, 0x4a);
    /// 语义色（柔和暖调，较前略提明度/饱和更清亮）
    pub const OK: Color32 = Color32::from_rgb(0x5e, 0x94, 0x57);
    /// 网络曲线专用色（较语义色更柔和低饱和，浅底上不刺眼）：下行鼠尾草绿 / 上行柔珊瑚
    pub const NET_DOWN: Color32 = Color32::from_rgb(0x76, 0xa2, 0x67);
    pub const NET_UP: Color32 = Color32::from_rgb(0xdf, 0x8a, 0x66);
    pub const WARN: Color32 = Color32::from_rgb(0xcc, 0x94, 0x38);
    pub const DANGER: Color32 = Color32::from_rgb(0xcb, 0x5a, 0x4d);
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
    // 文本渲染沿用浅色默认 Linear coverage→alpha。「增实」曲线（TwoCoverageMinusCoverageSq）
    // 实测浅底上颜色过深且无助于清晰度，已回退。
    v.override_text_color = Some(Palette::TEXT);
    v.panel_fill = Palette::BG;
    v.window_fill = Palette::PANEL;
    v.window_stroke = Stroke::new(1.0, Palette::BORDER);
    v.extreme_bg_color = Palette::PANEL_2;
    v.faint_bg_color = Palette::PANEL_2;
    v.hyperlink_color = Palette::ACCENT;
    v.selection.bg_fill = Palette::ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0, Palette::ACCENT);
    // 柔影：更大更淡的投影（Apple 质感核心），少用硬边框、靠阴影表达层次
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(28),
    };
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 3],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(26),
    };
    v.window_corner_radius = CornerRadius::same(R_LG);
    v.menu_corner_radius = CornerRadius::same(R_MD);

    // 统一控件圆角（令牌）
    let r = CornerRadius::same(R_SM);

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

    // 间距：8pt 网格，更舒展的留白（Apple 风格）
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(14);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size.y = 28.0;
    // 滚动条：macOS 式悬浮细滚动条（覆盖内容、不挤占布局，悬停加粗），更轻盈
    style.spacing.scroll = egui::style::ScrollStyle::floating();
    // 复选框 / 单选框略放大，更清爽易点
    style.spacing.icon_width = 16.0;
    style.spacing.icon_width_inner = 9.0;
    // 单向滚动区（如横向标签条）允许用竖直滚轮滚动（默认 false 导致标签条滚不动）
    style.always_scroll_the_only_direction = true;

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

    // 等宽主字体：内嵌 JetBrains Mono（OFL 许可）。egui 内置的 Hack 笔画偏细、
    // 灰度抗锯齿下发虚；JetBrains Mono 笔画厚实、hinting 佳，全平台观感一致。
    fonts.font_data.insert(
        "jbmono".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/JetBrainsMono-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "jbmono".to_owned());

    // UI 比例字体：egui 默认是 Ubuntu-Light（细体，浅底上发灰）。
    // Linux/Windows 换成系统常规字重 sans；中文仍回退到下方 cjk 字体。
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        let sans: &[&str] = &[
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/opentype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ];
        #[cfg(windows)]
        let sans: &[&str] = &["C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/arial.ttf"];
        for p in sans {
            if let Ok(data) = std::fs::read(p) {
                fonts.font_data.insert(
                    "ui_sans".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(data)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "ui_sans".to_owned());
                log::info!("UI 字体：{p}");
                break;
            }
        }
    }

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
        fonts
            .font_data
            .insert("cjk".to_owned(), std::sync::Arc::new(fd));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
    } else {
        log::warn!("未找到系统中文字体，中文可能显示为方块");
    }

    // Unicode 符号 / Powerline / 杂项符号：补 JetBrains Mono 与 CJK 未覆盖的字形
    // （框线、箭头、几何图形、Nerd Font 常用码位等）。放在 emoji 之前，优先用单色矢量字。
    // 可加载多个符号字体（按顺序追加到回退链）：Symbols → DejaVu → 可选 Nerd Font
    let symbol_candidates = [
        // Linux 通用符号
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansSymbols2-Regular.otf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/ancient-scripts/Symbola_hint.ttf",
        "/usr/share/fonts/truetype/symbola/Symbola.ttf",
        // Nerd Font / Powerline 私用区图标（若用户已安装）
        "/usr/share/fonts/truetype/nerd-fonts/SymbolsNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf",
        "/usr/share/fonts/opentype/nerd-fonts/SymbolsNerdFontMono-Regular.otf",
        // macOS
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/Library/Fonts/Apple Symbols.ttf",
        // Windows
        "C:/Windows/Fonts/seguisym.ttf",
        "C:/Windows/Fonts/symbol.ttf",
    ];
    let mut sym_i = 0usize;
    for path in symbol_candidates {
        if !std::path::Path::new(path).is_file() {
            continue;
        }
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let key = format!("symbols{sym_i}");
        sym_i += 1;
        log::info!("加载符号字体：{path}");
        fonts.font_data.insert(
            key.clone(),
            std::sync::Arc::new(egui::FontData::from_owned(data)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(key.clone());
        }
        // 最多挂 3 个，避免字体链过长拖慢布局
        if sym_i >= 3 {
            break;
        }
    }

    // Emoji：终端/UI 缺字形时回退（彩色 emoji 在部分后端可能降级为单色，仍优于豆腐块）
    let emoji_candidates = [
        // Linux
        "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/truetype/noto/NotoEmoji-Regular.ttf",
        "/usr/share/fonts/truetype/joypixels/JoyPixels.ttf",
        // macOS
        "/System/Library/Fonts/Apple Color Emoji.ttc",
        // Windows
        "C:/Windows/Fonts/seguiemj.ttf",
    ];
    // 体积上限：macOS 的 Apple Color Emoji.ttc 为兼容多档 Retina 位图字形，体积可达
    // 150~200+ MB，整份读入并作为 Arc<FontData> 常驻，是"iShell 刚打开什么都没做就占
    // 几百 MB"的直接原因——emoji 图标本就是锦上添花，不值得为它长期占用这么大内存。
    // Linux/Windows 的彩色 emoji 字体通常远小于此（Noto/Segoe UI Emoji 一般 <30 MB），
    // 用体积而非按平台硬编码判断，同样能兜住"某天系统 emoji 字体也变巨大"的情况。
    const EMOJI_FONT_SIZE_CAP: u64 = 32 * 1024 * 1024;
    if let Some(path) = emoji_candidates.iter().find(|p| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.len() <= EMOJI_FONT_SIZE_CAP)
            .unwrap_or(false)
    }) {
        if let Ok(data) = std::fs::read(path) {
            log::info!("加载 Emoji 字体：{path}");
            let mut fd = egui::FontData::from_owned(data);
            // Apple Color Emoji 等 TTC：优先 face 0
            fd.index = 0;
            fonts
                .font_data
                .insert("emoji".to_owned(), std::sync::Arc::new(fd));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("emoji".to_owned());
            }
        }
    } else if emoji_candidates
        .iter()
        .any(|p| std::path::Path::new(p).is_file())
    {
        log::warn!("系统 Emoji 字体体积过大（>{}MB），为控制内存占用已跳过加载，部分 Unicode 符号可能显示为方块", EMOJI_FONT_SIZE_CAP / 1024 / 1024);
    } else {
        log::warn!("未找到系统 Emoji 字体，部分 Unicode 符号可能显示为方块");
    }

    // 仅 macOS：UI 比例字体用系统 SF Pro（原生且常规字重）；等宽已统一为内嵌 JetBrains Mono。
    // 拉丁字形走系统字体，中文仍回退到上面的 cjk 字体。
    #[cfg(target_os = "macos")]
    {
        let paths = [
            "/System/Library/Fonts/SFNS.ttf",
            "/System/Library/Fonts/SFNSText.ttf",
            "/System/Library/Fonts/SFNSDisplay.ttf",
            "/System/Library/Fonts/Supplemental/Helvetica.ttc",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        ];
        for p in paths {
            if let Ok(data) = std::fs::read(p) {
                fonts.font_data.insert(
                    "mac_ui".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(data)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "mac_ui".to_owned());
                log::info!("mac 字体：Proportional ← {p}");
                break;
            }
        }
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
        let Ok(face) = ttf_parser::Face::parse(data, i) else {
            continue;
        };
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
