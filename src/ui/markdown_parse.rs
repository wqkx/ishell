//! Markdown → 扁平内容块（纯函数，不依赖 egui）。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::{Block, Kind, Marker, Span};

#[path = "markdown_html.rs"]
mod html;

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
    /// 正在收集的图片的 alt 文字（v1 只显示占位，不取图，地址不留）
    image: Option<String>,
    /// 正在收集的 HTML 块原文（按行到达，攒齐了再整块处理）
    html: Option<String>,
    /// 已打开、尚未闭合的 HTML 元素（跨事件保持，见 `markdown_html`）
    html_open: Vec<html::Open>,
    /// 处于 `<code>` / `<kbd>` 之类 HTML 行内代码元素内的层数
    html_code: u32,
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

impl Builder {
    fn push(&mut self, kind: Kind) {
        // 表格单元格里出现的块（HTML 表格的单元格里可以写 Markdown）：模型里单元格只装
        // 行内片段，所以代码块降级成行内代码，其余（分割线等）放弃——总比跑到表格外面强。
        if self.table.is_some() {
            if let Kind::Code { text, .. } = &kind {
                self.text(text, true);
            }
            return;
        }
        self.blocks.push(Block {
            kind,
            indent: self.lists.len().min(u8::MAX as usize) as u8,
            quote: self.quote,
        });
    }

    /// 结束当前段落：有文字才出块（顺带带走待用的列表标记）。
    fn flush_text(&mut self) {
        if self.in_cell() {
            return;
        }
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
        self.html_end_of_para();
    }

    /// 正在表格里：单元格内的分段不出块，只当作一次换行，文字继续攒在单元格里。
    /// （Markdown 表格的单元格只有行内内容，走不到这里；这是给 HTML 表格的单元格里
    /// 空行夹着写 Markdown 的情形准备的。）
    fn in_cell(&mut self) -> bool {
        if self.table.is_none() {
            return false;
        }
        self.marker = None;
        if self.spans.last().is_some_and(|s| !s.text.ends_with('\n')) {
            self.text("\n", false);
        }
        self.soft = false;
        true
    }

    /// 当前列表的下一个项标记。
    fn next_marker(&mut self) -> Marker {
        match self.lists.last_mut() {
            Some(Some(n)) => {
                let m = Marker::Number(*n);
                *n = n.saturating_add(1);
                m
            }
            _ => Marker::Bullet,
        }
    }

    /// 同 `flush_text`，但列表标记还没用掉时即使没有文字也出一个空段落——
    /// 空列表项、或列表项的首个子块是代码块/嵌套列表时，标记不能丢。
    fn flush_force(&mut self) {
        if self.table.is_none() && self.spans.is_empty() && self.marker.is_some() {
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
        if let Some(alt) = &mut self.image {
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
            Tag::HtmlBlock => {
                self.flush_force();
                self.html = Some(String::new());
            }
            Tag::List(first) => {
                // 紧凑列表项的文字没有 Paragraph 包着：嵌套列表开始前先收掉
                self.flush_text();
                self.lists.push(first);
            }
            Tag::Item => {
                // 父项的标记还没用掉（如 `- - a`）：先出掉，免得被子项覆盖
                self.flush_force();
                self.marker = Some(self.next_marker());
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
            Tag::Image { .. } => {
                // 嵌套图片（alt 里再放图片）极少见：只认最外层
                self.image.get_or_insert_with(String::new);
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
            TagEnd::HtmlBlock => {
                if let Some(raw) = self.html.take() {
                    self.html(&raw);
                }
                self.flush_text();
            }
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
                if let Some(alt) = self.image.take() {
                    // 带上外层链接：`[![徽章](b.svg)](url)` 的占位文字要能点开 url
                    self.spans.push(Span {
                        text: alt,
                        image: true,
                        link: self.links.last().cloned(),
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
                // 行内 HTML 的 <code>/<kbd> 之间的文字是作为普通 Text 事件送来的
                None => self.text(&t, self.html_code > 0),
            },
            Event::Code(t) => self.text(&t, true),
            Event::InlineHtml(t) => self.html(&t),
            // HTML 块按行到达：先攒着，块结束时整块处理（见 TagEnd::HtmlBlock）
            Event::Html(t) => match &mut self.html {
                Some(raw) => raw.push_str(&t),
                None => self.html(&t),
            },
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

/// 解析 Markdown 源码。对任意输入都不 panic、不失败：Markdown 没有「语法错误」（写错的
/// 标记按 CommonMark 规定退化成普通文字），内嵌 HTML 写错了也有兜底（见 `markdown_html`）。
pub fn parse(src: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut b = Builder::default();
    for ev in Parser::new_ext(src, opts) {
        b.event(ev);
    }
    // 文末统一收尾没闭合的 HTML 结构（列表、引用、表格……）
    b.html_unwind(0);
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
        assert_eq!(img.link, None, "不在链接里的图片不该变成可点的");
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
    fn inline_html_tags_become_styles() {
        let b = parse("按 <kbd>Ctrl</kbd> 键，<b>粗</b><i>斜</i><del>删</del> <a href=\"https://a.b\">链</a>\n");
        let Kind::Para { spans, .. } = &b[0].kind else {
            panic!("应为段落")
        };
        assert_eq!(plain(spans), "按 Ctrl 键，粗斜删 链");
        let find = |t: &str| spans.iter().find(|s| s.text == t).expect(t);
        assert!(find("Ctrl").code);
        assert!(find("粗").bold);
        assert!(find("斜").italic);
        assert!(find("删").strike);
        assert_eq!(find("链").link.as_deref(), Some("https://a.b"));
    }

    #[test]
    fn html_entities_are_decoded() {
        use super::html::decode_entities as d;
        assert_eq!(d("a &amp; b &lt;c&gt; &#20013;&#x6587; &quot;q&quot;"), "a & b <c> 中文 \"q\"");
        // 不认识的、没写完的原样保留
        assert_eq!(d("AT&T &bogus; &amp &#xZZ; &"), "AT&T &bogus; &amp &#xZZ; &");
        assert_eq!(para(&parse("<p>1 &lt; 2 &amp;&amp; 3 &gt; 2</p>\n"), 0).0, "1 < 2 && 3 > 2");
    }

    #[test]
    fn html_block_structures() {
        let b = parse("<h2 align=\"center\">标题</h2>\n<p>段一</p>\n<p>段二<br>第二行</p>\n<hr>\n<blockquote>引用</blockquote>\n");
        assert_eq!(para(&b, 0), ("标题".into(), 2, None));
        assert_eq!(para(&b, 1), ("段一".into(), 0, None));
        assert_eq!(para(&b, 2).0, "段二\n第二行");
        assert_eq!(b[3].kind, Kind::Rule);
        assert_eq!((para(&b, 4).0, b[4].quote), ("引用".into(), 1));
        assert_eq!(b.len(), 5);
    }

    #[test]
    fn html_lists() {
        let b = parse("<ul>\n<li>甲\n  <ol start=\"3\">\n  <li>乙</li>\n  <li>丙</li>\n  </ol>\n</li>\n<li>丁</li>\n</ul>\n");
        let got: Vec<_> = (0..b.len())
            .map(|i| (para(&b, i).0, b[i].indent, para(&b, i).2))
            .collect();
        assert_eq!(
            got,
            vec![
                ("甲".into(), 1, Some(Marker::Bullet)),
                ("乙".into(), 2, Some(Marker::Number(3))),
                ("丙".into(), 2, Some(Marker::Number(4))),
                ("丁".into(), 1, Some(Marker::Bullet)),
            ]
        );
    }

    #[test]
    fn html_pre_is_a_code_block() {
        let b = parse("<pre><code class=\"hljs language-Rust\">\nif a &lt; b {\n    <span class=\"k\">return</span>;\n}\n</code></pre>\n");
        assert_eq!(
            b[0].kind,
            Kind::Code {
                lang: "rust".into(),
                text: "if a < b {\n    return;\n}".into(),
            }
        );
    }

    #[test]
    fn html_table() {
        let b = parse("<table>\n<tr><th>名称</th><th>值</th></tr>\n<tr><td><b>a</b></td><td>1</td></tr>\n</table>\n\n后文\n");
        let Kind::Table { head, rows } = &b[0].kind else {
            panic!("应为表格：{:?}", b[0].kind)
        };
        assert_eq!(head.iter().map(|c| plain(c)).collect::<Vec<_>>(), ["名称", "值"]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0][0][0].bold);
        assert_eq!(plain(&rows[0][1]), "1");
        assert_eq!(para(&b, 1).0, "后文");
    }

    /// HTML 与 Markdown 交错：`<details>` 里空一行写 Markdown、表格单元格里空一行写
    /// Markdown。HTML 元素的状态必须跨事件保持，单元格里的段落也不能跑到表格外面去。
    #[test]
    fn markdown_inside_html_containers() {
        let b = parse("<details>\n<summary>展开</summary>\n\n- **项**\n\n</details>\n");
        assert_eq!(para(&b, 0).0, "展开");
        let Kind::Para { spans, .. } = &b[0].kind else { panic!() };
        assert!(spans[0].bold, "summary 应加粗");
        assert_eq!(para(&b, 1), ("项".into(), 0, Some(Marker::Bullet)));
        assert_eq!(b.len(), 2);

        let b = parse("<table>\n<tr>\n<td>\n\n**粗** 文字\n\n```sh\nls\n```\n\n</td>\n<td>二</td>\n</tr>\n</table>\n");
        assert_eq!(b.len(), 1, "单元格里的内容跑到表格外面了：{b:?}");
        let Kind::Table { head, .. } = &b[0].kind else { panic!() };
        assert_eq!(head.len(), 2);
        assert!(head[0][0].bold);
        assert!(plain(&head[0]).contains("文字"));
        assert!(head[0].iter().any(|s| s.code && s.text == "ls"));
        assert_eq!(plain(&head[1]), "二");
    }

    /// 写错的 HTML 逐类兜底（规则见 `markdown_html` 模块文档）。
    #[test]
    fn malformed_html_degrades_gracefully() {
        // 没闭合的行内样式只管到本段结束
        let b = parse("前 <b>粗\n\n下一段\n");
        let Kind::Para { spans, .. } = &b[1].kind else { panic!() };
        assert!(!spans[0].bold, "没闭合的 <b> 漏到了下一段");
        // 没闭合的标题同理
        let b = parse("<h1>标题\n\n正文\n");
        assert_eq!(para(&b, 0).1, 1);
        assert_eq!(para(&b, 1), ("正文".into(), 0, None));
        // 没闭合的链接同理
        let b = parse("<a href=\"https://a.b\">链\n\n正文\n");
        let Kind::Para { spans, .. } = &b[1].kind else { panic!() };
        assert_eq!(spans[0].link, None);

        // 多余的闭合标签忽略；嵌套错位自然收拢
        assert_eq!(para(&parse("甲</b></div></table>乙\n"), 0).0, "甲乙");
        let b = parse("<b>粗<i>粗斜</b>常</i>规\n");
        let Kind::Para { spans, .. } = &b[0].kind else { panic!() };
        assert_eq!(plain(spans), "粗粗斜常规");
        assert!(spans.iter().all(|s| s.text != "常规" || (!s.bold && !s.italic)));

        // 不构成标签的 `<` 原样显示
        assert_eq!(para(&parse("<p>1 < 2，a <- b，x <y</p>\n"), 0).0, "1 < 2，a <- b，x <y");
        // 引号没配对的属性不吞后面的内容
        assert_eq!(para(&parse("<p align=\"center>文字</p>\n\n后文\n"), 0).0, "文字");

        // 省略的闭合标签：<li> / <tr> / <td> / <p>
        let b = parse("<ul>\n<li>一\n<li>二\n</ul>\n");
        assert_eq!(b.len(), 2);
        assert_eq!((para(&b, 1).0, b[1].indent), ("二".into(), 1));
        let b = parse("<table><tr><td>a<td>b<tr><td>c<td>d</table>\n");
        let Kind::Table { head, rows } = &b[0].kind else { panic!("{b:?}") };
        assert_eq!((head.len(), rows.len(), rows[0].len()), (2, 1, 2));
        assert_eq!(parse("<p>一<p>二\n").len(), 2);

        // 缺父元素：<li> 外面没有列表、<td> 外面没有 <tr>
        let b = parse("<li>孤项</li>\n\n后文\n");
        assert_eq!((para(&b, 0).2, b[0].indent), (Some(Marker::Bullet), 1));
        let b = parse("<table><td>a</td><td>b</td></table>\n");
        assert!(matches!(&b[0].kind, Kind::Table { head, .. } if head.len() == 2));
        // 表格外的 <td> 不是表格：只留文字
        assert_eq!(para(&parse("<td>散</td>\n"), 0).0, "散");

        // 文末没闭合的块级结构：内容不能丢
        let b = parse("<table>\n<tr><td>a</td>\n");
        assert!(matches!(&b[0].kind, Kind::Table { head, .. } if plain(&head[0]) == "a"));
        let b = parse("<pre>\nlet x = 1;\n");
        assert!(matches!(&b[0].kind, Kind::Code { text, .. } if text == "let x = 1;"));
        let b = parse("<blockquote>\n引\n");
        assert_eq!((para(&b, 0).0, b[0].quote), ("引".into(), 1));
        // 没收尾的注释吞到块末，不显示
        assert!(parse("<!-- 半截注释\n还在注释里\n").is_empty());
    }

    /// HTML 块是**按行**送来的：逐行去标签会把跨行结构拆坏——多行注释整段漏成正文、
    /// 跨行标签的属性漏成正文、`<style>` 的内容被当成段落。必须整块处理。
    #[test]
    fn multi_line_html_blocks_do_not_leak_markup() {
        assert_eq!(parse("<!--\nTODO: 别显示\n-->\n\n正文\n").len(), 1);
        assert_eq!(para(&parse("<!--\nTODO: 别显示\n-->\n\n正文\n"), 0).0, "正文");

        let b = parse("<p align=\"center\">\n  <img src=x\n  width=200>\n  居中文字\n</p>\n");
        assert_eq!(b.len(), 1);
        assert_eq!(para(&b, 0).0, "x 居中文字"); // 图片占位（alt 缺省取文件名）+ 文字

        let b = parse("<style>\n.a { color: red }\n</style>\n\n<div>\n甲<br>乙\n</div>\n");
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!(para(&b, 0).0, "甲\n乙");
    }

    /// README 的标准徽章写法：链接里套图片。占位文字必须带着**外层链接**，否则点不开。
    #[test]
    fn image_inside_link_keeps_the_outer_link() {
        let b = parse("[![构建](badge.svg)](https://ci.example/run)\n");
        let Kind::Para { spans, .. } = &b[0].kind else {
            panic!("应为段落")
        };
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "构建");
        assert_eq!(spans[0].link.as_deref(), Some("https://ci.example/run"));
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
            <div align=\"center\">\n<img src=x>\n</div>\n\n***\n\n    缩进代码\n\n\
            <details><summary>折叠 &amp; 展开</summary>\n\n**内** <kbd>键</kbd>\n\n</details>\n\n\
            <table>\n<tr><th>头<td>格 <a href='u>链</a>\n\n- 项\n\n<tr><td><pre>码 &lt;</pre>\n</table>\n\n\
            <ul><li>一<ol start=\"x\"><li>二</ul></b></i> 1 < 2 &#x4e2d; &#99999999; <!-- 注 -->\n\n\
            <style>\n.a{}\n</style>\n\n<h3>题<b>粗\n\n```\n未闭合";
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
