//! Markdown 渲染预览：`pulldown-cmark` 解析为扁平内容块，再用 egui 重排绘制。
//!
//! 定位与 `docx` 阅读视图一致——「看得出效果」而非浏览器级保真：标题 / 加粗斜体删除线 /
//! 行内代码 / 链接 / 有序·无序·任务列表（含嵌套）/ 引用 / 围栏代码块（复用编辑器的轻量
//! 高亮）/ GFM 表格 / 分割线。图片只显示占位文字（文件在远端，v1 不取）；内嵌 HTML 只保留
//! 标签之间的文字。
//!
//! 解析（`markdown_parse`）是纯函数、可单测；渲染（`markdown_render`）走视口裁剪 + 高度缓存，
//! 做法照搬 `docx_render::render_virtual`。

use egui::text::LayoutJob;

/// 行内片段（一段内的一个格式区间）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    /// 行内代码
    pub code: bool,
    /// 图片占位：`text` 为 alt 文字（图片地址不保留；`link` 是外层链接，如徽章）
    pub image: bool,
    /// 链接目标
    pub link: Option<String>,
}

/// 列表项标记。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Marker {
    Bullet,
    Number(u64),
    Task(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// 段落：heading 0=正文、1..=6 标题层级；marker=列表项的首段才有
    Para {
        spans: Vec<Span>,
        heading: u8,
        marker: Option<Marker>,
    },
    /// 代码块：lang 为围栏标注的语言（小写，可能为空）
    Code { lang: String, text: String },
    /// 表格：表头行 + 数据行，单元格为行内片段
    Table {
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    /// 分割线
    Rule,
}

/// 内容块。嵌套结构（列表 / 引用）被压平成「块 + 缩进层级」，渲染只需一个线性列表，
/// 才能做按块的视口裁剪。
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub kind: Kind,
    /// 所在列表的嵌套深度（0 = 不在列表内）
    pub indent: u8,
    /// 所在引用的嵌套深度
    pub quote: u8,
}

/// 超过该大小不提供预览：解析在 UI 线程同步进行，首帧还要全量布局一遍。
pub const PREVIEW_LIMIT: usize = 1024 * 1024;

/// 某个编辑器的预览状态：解析结果 + 渲染缓存，按内容版本失效。
#[derive(Default)]
pub struct Preview {
    blocks: Vec<Block>,
    /// 各块上次量到的高度（0 = 未知），供视口裁剪占位
    heights: Vec<f32>,
    /// 代码块的高亮布局缓存（与 blocks 等长；非代码块恒为 None）
    code_jobs: Vec<Option<LayoutJob>>,
    /// 解析时的内容版本；None = 尚未解析
    ver: Option<u64>,
}

impl Preview {
    /// 内容版本变了才重新解析（不能每帧解析）。
    pub fn sync(&mut self, src: &str, ver: u64) {
        if self.ver == Some(ver) {
            return;
        }
        self.ver = Some(ver);
        self.blocks = parse(src);
        // 高度缓存故意不清：旧值当作估值继续用于屏幕外占位（进入视口时自会重量）。
        // 清掉的话下一帧所有块都得重新布局——跟随模式每秒追加一次，就是每秒全量重排一次。
        self.code_jobs.clear();
    }
}

#[path = "markdown_parse.rs"]
mod markdown_parse;
#[path = "markdown_render.rs"]
mod markdown_render;

pub use markdown_parse::parse;
pub use markdown_render::show;
