//! 轻量 Word(docx) 阅读视图：zip 容器 + `word/document.xml` 拉流解析。
//!
//! 定位是「重排阅读」而非版式保真：段落/标题/加粗斜体下划线/列表/表格（含单元格内
//! 图片）/内嵌图片（按文档指定的显示尺寸），页眉页脚、分栏、浮动定位、公式等不支持。
//! 标题识别读取 `word/styles.xml` 的 outlineLvl（样式 id 本地化也能正确映射）。
//! `.doc` 老二进制格式不在此处理（上层提示另存为 .docx）。

/// 文本片段（一段内的一个格式区间）。
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// 表格单元格内容片段：文本或内嵌图片。
pub enum CellPiece {
    Text(String),
    Image(String),
}

/// 段落水平对齐。
#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// 阅读视图内容块。
pub enum Block {
    /// 段落：heading 0=正文、1..=6 标题层级；bullet=列表项
    Para {
        spans: Vec<Span>,
        heading: u8,
        bullet: bool,
        align: Align,
    },
    /// 表格：行 × 列，单元格为富内容片段
    Table(Vec<Vec<Vec<CellPiece>>>),
    /// 内嵌图片：media 名 + 文档指定的显示尺寸（像素，0=未指定按原图）
    Image { name: String, w: f32, h: f32 },
}

/// 解析结果：内容块 + 用到的媒体文件字节（上层解码为纹理）。
pub struct Doc {
    pub blocks: Vec<Block>,
    pub media: Vec<(String, Vec<u8>)>,
}

/// EMU（English Metric Unit）→ 屏幕像素：914400 EMU/inch ÷ 96 dpi = 9525 EMU/px。
const EMU_PER_PX: f64 = 9525.0;

#[path = "docx_parse.rs"]
mod docx_parse;
#[path = "docx_render.rs"]
mod docx_render;

pub use docx_parse::parse;
pub use docx_render::{block_text, render_virtual};
