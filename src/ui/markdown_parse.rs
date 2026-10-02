//! Markdown → 扁平内容块（纯函数，不依赖 egui）。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::{Block, Kind, Marker, Span};

#[derive(Default)]
struct TableBuild {
    head: Vec<Vec<Span>>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    /// 当前段落已累积的行内片段
    spans: Vec<Span>,
    bold: u32,
    italic: u32,
    strike: u32,
    links: Vec<String>,
    /// 正在收集的图片：(alt, 地址)
    image: Option<(String, String)>,
    heading: u8,
    /// 列表栈：Some(n)=有序列表的下一个编号，None=无序
    lists: Vec<Option<u64>>,
    /// 当前列表项尚未用掉的标记（由该项的首个块带走）
    marker: Option<Marker>,
    quote: u8,
    /// 正在收集的代码块：(语言, 正文)
    code: Option<(String, String)>,
    table: Option<TableBuild>,
    /// 上一个事件是软换行：下一段文字到来时再决定要不要补空格
    soft: bool,
}

/// 中日文字符及全角标点：软换行两侧都是这类字符时不补空格
/// （源码里为了行宽折行的中文段落，渲染出来不该多出空格）。
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x2E80..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F
    )
}

/// 去掉 HTML 标签，只留标签之间的文字（`<br>` 记为换行；注释整体丢弃）。
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let tail = &rest[lt..];
        let end = if tail.starts_with("<!--") {
            tail.find("-->").map(|e| e + 3)
        } else {
            tail.find('>').map(|e| e + 1)
        };
        let Some(end) = end else {
            // 没有收尾：不是标签，原样保留
            out.push_str(tail);
            return out;
        };
        let tag = tail[..end].to_ascii_lowercase();
        if tag.starts_with("<br") {
            out.push('\n');
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

impl Builder {
    fn push(&mut self, kind: Kind) {
        self.blocks.push(Block {
            kind,
            indent: self.lists.len().min(u8::MAX as usize) as u8,
            quote: self.quote,
        });
    }

    /// 结束当前段落：有文字才出块（顺带带走待用的列表标记）。
    fn flush_text(&mut self) {
        self.soft = false;
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let marker = self.marker.take();
        let heading = self.heading;
        self.push(Kind::Para {
            spans,
            heading,
            marker,
        });
    }

    /// 同 `flush_text`，但列表标记还没用掉时即使没有文字也出一个空段落——
    /// 空列表项、或列表项的首个子块是代码块/嵌套列表时，标记不能丢。
    fn flush_force(&mut self) {
        if self.spans.is_empty() && self.marker.is_some() {
            let marker = self.marker.take();
            self.soft = false;
            self.push(Kind::Para {
                spans: Vec::new(),
                heading: 0,
                marker,
            });
        } else {
            self.flush_text();
        }
    }

    fn push_span(&mut self, text: &str, code: bool) {
        if text.is_empty() {
            return;
        }
        let span = Span {
            text: String::new(),
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code,
            image: false,
            link: self.links.last().cloned(),
        };
        // 与上一个片段格式相同就并进去，少出 LayoutJob 分段
        if let Some(last) = self.spans.last_mut() {
            if !last.image
                && last.bold == span.bold
                && last.italic == span.italic
                && last.strike == span.strike
                && last.code == span.code
                && last.link == span.link
            {
                last.text.push_str(text);
                return;
            }
        }
        self.spans.push(Span {
            text: text.to_string(),
            ..span
        });
    }

    fn text(&mut self, t: &str, code: bool) {
        if let Some((alt, _)) = &mut self.image {
            alt.push_str(t);
            return;
        }
        if self.soft {
            self.soft = false;
            let prev = self
                .spans
                .last()
                .and_then(|s| s.text.chars().next_back());
            let next = t.chars().next();
            let glue = matches!((prev, next), (Some(a), Some(b)) if is_cjk(a) && is_cjk(b));
            if !glue && prev.is_some() {
                self.push_span(" ", false);
            }
        }
        self.push_span(t, code);
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.flush_text(),
            Tag::Heading { level, .. } => {
                self.flush_text();
                self.heading = level as u8;
            }
            Tag::BlockQuote(..) => {
                self.flush_force();
                self.quote = self.quote.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush_force();
                let lang = match kind {
                    // info string 形如 `rust,ignore` / `python {.numberLines}`：只取首个词
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                        .next()
                        .unwrap_or("")
                        .to_lowercase(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            // 文首的 YAML front matter：当作 yaml 代码块显示
            Tag::MetadataBlock(..) => {
                self.flush_force();
                self.code = Some(("yaml".into(), String::new()));
            }
            Tag::HtmlBlock => self.flush_force(),
            Tag::List(first) => {
                // 紧凑列表项的文字没有 Paragraph 包着：嵌套列表开始前先收掉
                self.flush_text();
                self.lists.push(first);
            }
            Tag::Item => {
                // 父项的标记还没用掉（如 `- - a`）：先出掉，免得被子项覆盖
                self.flush_force();
                self.marker = Some(match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = Marker::Number(*n);
                        *n = n.saturating_add(1);
                        m
                    }
                    _ => Marker::Bullet,
                });
            }
            Tag::Table(..) => {
                self.flush_force();
                self.table = Some(TableBuild::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.row.clear();
                }
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
            Tag::Image { dest_url, .. } => {
                // 嵌套图片（alt 里再放图片）极少见：只认最外层
                self.image
                    .get_or_insert_with(|| (String::new(), dest_url.to_string()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_text(),
            TagEnd::Heading(..) => {
                self.flush_text();
                self.heading = 0;
            }
            TagEnd::BlockQuote(..) => {
                self.flush_text();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock | TagEnd::MetadataBlock(..) => {
                if let Some((lang, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    self.push(Kind::Code { lang, text });
                }
            }
            TagEnd::HtmlBlock => self.flush_text(),
            TagEnd::List(..) => {
                self.flush_text();
                self.lists.pop();
            }
            TagEnd::Item => self.flush_force(),
            TagEnd::TableCell => {
                self.soft = false;
                let cell = std::mem::take(&mut self.spans);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push(Kind::Table {
                        head: t.head,
                        rows: t.rows,
                    });
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => {
                if let Some((alt, url)) = self.image.take() {
                    self.spans.push(Span {
                        text: alt,
                        image: true,
                        link: Some(url),
                        ..Span::default()
                    });
                }
            }
            _ => {}
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => match &mut self.code {
                Some((_, body)) => body.push_str(&t),
                None => self.text(&t, false),
            },
            Event::Code(t) => self.text(&t, true),
            Event::InlineHtml(t) => {
                let plain = strip_tags(&t);
                if !plain.is_empty() {
                    self.text(&plain, false);
                }
            }
            // HTML 块按行到达：每行去标签后当作同一段里的一行（行间按软换行衔接）
            Event::Html(t) => {
                let plain = strip_tags(&t);
                let plain = plain.trim();
                if !plain.is_empty() {
                    self.text(plain, false);
                    self.soft = true;
                }
            }
            Event::SoftBreak => self.soft = true,
            Event::HardBreak => self.text("\n", false),
            Event::Rule => {
                self.flush_force();
                self.push(Kind::Rule);
            }
            Event::TaskListMarker(done) => self.marker = Some(Marker::Task(done)),
            _ => {}
        }
    }
}

/// 解析 Markdown 源码。对任意输入都不 panic、不失败（Markdown 没有「语法错误」）。
pub fn parse(src: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut b = Builder::default();
    for ev in Parser::new_ext(src, opts) {
        b.event(ev);
    }
    b.flush_force();
    b.blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// 取第 i 块的段落内容：(纯文本, 标题层级, 标记)
    fn para(blocks: &[Block], i: usize) -> (String, u8, Option<Marker>) {
        match &blocks[i].kind {
            Kind::Para {
                spans,
                heading,
                marker,
            } => (plain(spans), *heading, *marker),
            other => panic!("第 {i} 块不是段落：{other:?}"),
        }
    }

    #[test]
    fn headings_and_chinese_paragraphs() {
        let b = parse("# 标题一\n\n正文第一段。\n\n### 三级 *斜体*\n");
        assert_eq!(b.len(), 3);
        assert_eq!(para(&b, 0), ("标题一".into(), 1, None));
        assert_eq!(para(&b, 1), ("正文第一段。".into(), 0, None));
        assert_eq!(para(&b, 2), ("三级 斜体".into(), 3, None));
    }

    #[test]
    fn inline_styles_split_into_spans() {
        let b = parse("普通 **粗** *斜* ~~删~~ `码` [链](https://a.b/c)\n");
        let Kind::Para { spans, .. } = &b[0].kind else {
            panic!("应为段落")
        };
        let find = |t: &str| spans.iter().find(|s| s.text == t).expect(t);
        assert!(find("粗").bold);
        assert!(find("斜").italic);
        assert!(find("删").strike);
        assert!(find("码").code);
        assert_eq!(find("链").link.as_deref(), Some("https://a.b/c"));
        assert!(!find("普通 ").bold);
    }

    /// 源码里按行宽折行的中文段落，渲染后不能在折行处多出空格；英文则要补。
    #[test]
    fn soft_break_joins_cjk_but_spaces_latin() {
        assert_eq!(para(&parse("第一行\n第二行\n"), 0).0, "第一行第二行");
        assert_eq!(para(&parse("first\nsecond\n"), 0).0, "first second");
        assert_eq!(para(&parse("中文\nEnglish\n"), 0).0, "中文 English");
    }

    #[test]
    fn nested_and_ordered_lists_keep_depth_and_numbers() {
        let b = parse("- 甲\n  - 乙\n  - 丙\n- 丁\n\n3. 三\n4. 四\n");
        let got: Vec<_> = (0..b.len())
            .map(|i| (para(&b, i).0, b[i].indent, para(&b, i).2))
            .collect();
        assert_eq!(
            got,
            vec![
                ("甲".into(), 1, Some(Marker::Bullet)),
                ("乙".into(), 2, Some(Marker::Bullet)),
                ("丙".into(), 2, Some(Marker::Bullet)),
                ("丁".into(), 1, Some(Marker::Bullet)),
                ("三".into(), 1, Some(Marker::Number(3))),
                ("四".into(), 1, Some(Marker::Number(4))),
            ]
        );
    }

    /// 松散列表项（项内有空行分段）：只有首段带标记，后续段落同缩进、无标记。
    #[test]
    fn loose_item_only_first_paragraph_has_marker() {
        let b = parse("- 首段\n\n  续段\n\n- 第二项\n");
        assert_eq!(para(&b, 0), ("首段".into(), 0, Some(Marker::Bullet)));
        assert_eq!(para(&b, 1), ("续段".into(), 0, None));
        assert_eq!(b[1].indent, 1);
        assert_eq!(para(&b, 2), ("第二项".into(), 0, Some(Marker::Bullet)));
    }

    #[test]
    fn task_list_markers() {
        let b = parse("- [x] 完成\n- [ ] 待办\n");
        assert_eq!(para(&b, 0).2, Some(Marker::Task(true)));
        assert_eq!(para(&b, 1).2, Some(Marker::Task(false)));
        assert_eq!(para(&b, 0).0, "完成");
    }

    /// 空列表项、以及首个子块就是代码块的列表项：标记不能丢。
    #[test]
    fn marker_survives_items_without_leading_text() {
        let b = parse("1.\n2. 有字\n");
        assert_eq!(para(&b, 0), (String::new(), 0, Some(Marker::Number(1))));
        assert_eq!(para(&b, 1).2, Some(Marker::Number(2)));

        let b = parse("- ```sh\n  ls\n  ```\n");
        assert_eq!(para(&b, 0).2, Some(Marker::Bullet));
        assert!(matches!(&b[1].kind, Kind::Code { lang, text } if lang == "sh" && text == "ls"));
        assert_eq!(b[1].indent, 1);
    }

    #[test]
    fn fenced_code_keeps_lang_and_body_verbatim() {
        let b = parse("```Rust,ignore\nfn main() {\n    // **不是粗体**\n}\n```\n");
        assert_eq!(
            b[0].kind,
            Kind::Code {
                lang: "rust".into(),
                text: "fn main() {\n    // **不是粗体**\n}".into(),
            }
        );
    }

    /// 未闭合的围栏：其后全部内容都算代码（CommonMark 规定），不能丢字。
    #[test]
    fn unclosed_fence_swallows_rest() {
        let b = parse("前文\n\n```py\nprint(1)\n# 标题？\n");
        assert_eq!(b.len(), 2);
        assert!(
            matches!(&b[1].kind, Kind::Code { lang, text } if lang == "py" && text == "print(1)\n# 标题？")
        );
    }

    #[test]
    fn gfm_table() {
        let b = parse("| 名称 | 值 |\n|---|---:|\n| **a** | 1 |\n| b |\n");
        let Kind::Table { head, rows } = &b[0].kind else {
            panic!("应为表格：{:?}", b[0].kind)
        };
        assert_eq!(head.iter().map(|c| plain(c)).collect::<Vec<_>>(), ["名称", "值"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(plain(&rows[0][0]), "a");
        assert!(rows[0][0][0].bold);
        assert_eq!(plain(&rows[0][1]), "1");
    }

    #[test]
    fn blockquote_depth_and_rule() {
        let b = parse("> 外\n>\n> > 内\n\n---\n\n尾\n");
        assert_eq!((para(&b, 0).0, b[0].quote), ("外".into(), 1));
        assert_eq!((para(&b, 1).0, b[1].quote), ("内".into(), 2));
        assert_eq!(b[2].kind, Kind::Rule);
        assert_eq!((para(&b, 3).0, b[3].quote), ("尾".into(), 0));
    }

    #[test]
    fn image_becomes_placeholder_span() {
        let b = parse("见 ![架构图](img/a.png) 所示\n");
        let Kind::Para { spans, .. } = &b[0].kind else {
            panic!("应为段落")
        };
        let img = spans.iter().find(|s| s.image).expect("应有图片片段");
        assert_eq!(img.text, "架构图");
        assert_eq!(img.link.as_deref(), Some("img/a.png"));
    }

    #[test]
    fn front_matter_is_a_yaml_code_block() {
        let b = parse("---\ntitle: 文档\n---\n\n# 正文\n");
        assert_eq!(
            b[0].kind,
            Kind::Code {
                lang: "yaml".into(),
                text: "title: 文档".into(),
            }
        );
        assert_eq!(para(&b, 1).1, 1);
    }

    #[test]
    fn html_keeps_text_drops_tags() {
        assert_eq!(strip_tags("<b>粗</b> 与 <!-- 注释 > --> 尾"), "粗 与  尾");
        assert_eq!(strip_tags("a<br/>b"), "a\nb");
        assert_eq!(strip_tags("1 < 2"), "1 < 2");
        assert_eq!(para(&parse("按 <kbd>Ctrl</kbd> 键\n"), 0).0, "按 Ctrl 键");
    }

    /// 预览跑在 UI 线程：解析 panic = 整个应用闪退、连带丢掉未保存的改动。
    /// 用户正在编辑的 Markdown 随时可能处于「写了一半」的状态，所以把一份覆盖各种构造的
    /// 语料在**每个字符边界**处截断后都解析一遍——任何前缀都不能 panic。
    #[test]
    fn never_panics_on_any_prefix() {
        const CORPUS: &str = "---\nk: v\n---\n# 标题 `码`\n\n段落 **粗 *嵌套* 体** ~~删~~ \
            [链接 ![图](a.png)](http://x) <kbd>键</kbd>\\\n硬换行\n\n\
            > 引用\n> - 列表\n>   1. 有序\n>      ```rs\n>      let s = \"😀\";\n>      ```\n\n\
            - [ ] 任务\n- [x] 完成\n\t- 制表符缩进\n\n\
            | a | b |\n|:--|--:|\n| 1 | `2` |\n| 仅一列\n\n\
            <div align=\"center\">\n<img src=x>\n</div>\n\n***\n\n    缩进代码\n\n```\n未闭合";
        let mut cuts: Vec<usize> = CORPUS.char_indices().map(|(i, _)| i).collect();
        cuts.push(CORPUS.len());
        for cut in cuts {
            let _ = parse(&CORPUS[..cut]);
        }
        for nasty in [
            "",
            "\u{0}\u{1}\u{7f}",
            "\r\n\r\n\t\t   ",
            "[[[[[[[[[[[[[[[[[[[[",
            "****************",
            "> > > > > > > > > > > >",
            "- - - - - - - - - - - -",
            "|||||\n|-|-|\n|",
            "<<<<<<<<!--",
            "![](",
            "```\n```\n```",
        ] {
            let _ = parse(nasty);
        }
    }
}
