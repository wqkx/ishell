//! 内嵌 HTML → 与 Markdown 同一套内容块（渲染层无需区分来源）。
//!
//! README 里常见的 HTML 写法都翻译成对应结构：`<b>/<i>/<s>/<code>/<kbd>`、`<a href>`、
//! `<br>/<hr>/<img>`、`<h1>`–`<h6>`、`<p>/<div>/<details>/<summary>`、`<ul>/<ol>/<li>`、
//! `<blockquote>`、`<pre>`、`<table>/<tr>/<th>/<td>`，实体（`&amp;` `&#39;` …）解码，
//! 注释与 `<style>/<script>` 的内容丢弃。不认识的标签只丢标签、保留其中文字。
//!
//! # 写错了怎么办
//!
//! 预览的对象是用户**正在写**的文档，HTML 随时可能是半截的，所以这里没有「解析失败」：
//!
//! - 已打开的元素记在一个栈里。闭合标签找栈里最近的同名元素，把它和它上面的一起收掉
//!   （嵌套错位 `<b><i></b></i>` 因此自然收拢）；找不到同名的就是多余的闭合标签，忽略。
//! - 没闭合的**行内**样式（`<b>`、`<a>` …）和标题只管到本段结束，不会把后面整篇文档
//!   都变粗；没闭合的**块级**结构（列表、引用、表格）延续到文末再统一收尾，和浏览器一致。
//! - 该有的父元素缺了就补：`<li>` 外面没有列表、`<td>` 外面没有 `<tr>`。
//! - 该有的闭合标签省了也认：新的 `<li>` / `<tr>` / `<td>` / `<p>` 会收掉上一个。
//! - 不构成标签的 `<`（`1 < 2`、`a <- b`、缺 `>` 的半截标签）按普通文字原样显示。
//! - 引号没配对的属性不会吞掉后面的内容：退回到第一个 `>` 作为标签结尾。
//!
//! Markdown 与 HTML 可以交错（`<details>` 里空一行写 Markdown、表格单元格里写 Markdown），
//! 所以元素栈挂在 `Builder` 上、跨事件保持，而不是每段 HTML 各自为政。

use super::super::{Kind, Span};
use super::{Builder, TableBuild};

/// 元素在栈里登记的「收尾时要撤销什么」。
#[derive(Clone, Copy, PartialEq)]
pub(super) enum El {
    Bold,
    Italic,
    Strike,
    Code,
    /// 带 href 的 `<a>`（往链接栈里压了一项）
    Link,
    /// 不带 href 的 `<a>`（锚点）：只为配对闭合标签
    Anchor,
    Heading,
    Summary,
    Quote,
    List,
    Item,
    /// `<p>` / `<div>` 等：只起分段作用
    Block,
    Table,
    Row,
    Cell,
}

pub(super) struct Open {
    name: String,
    el: El,
}

enum Markup<'a> {
    /// 注释 / DOCTYPE / 处理指令：整体丢弃
    Skip,
    Open(String, &'a str),
    Close(String),
}

/// 标签内找收尾的 `>`（引号里的不算）。引号没配对时退回第一个 `>`。
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.bytes().enumerate() {
        match (quote, c) {
            (None, b'>') => return Some(i),
            (None, b'"' | b'\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
    }
    s.find('>')
}

/// 找下一处真正的标记，返回 (标记前的文本长度, 标记, 标记之后的剩余)。
/// 不构成标记的 `<` 被跳过（留在文本里）。所有切分点都落在 ASCII 字节上，不会切坏多字节字符。
fn next_markup(s: &str) -> Option<(usize, Markup<'_>, &str)> {
    let mut from = 0;
    while let Some(off) = s[from..].find('<') {
        let lt = from + off;
        let tail = &s[lt..];
        if let Some(body) = tail.strip_prefix("<!--") {
            // 没收尾的注释一直延续到末尾
            let after = body.find("-->").map_or("", |e| &body[e + 3..]);
            return Some((lt, Markup::Skip, after));
        }
        let b = tail.as_bytes();
        if matches!(b.get(1), Some(b'!' | b'?')) {
            if let Some(e) = tail.find('>') {
                return Some((lt, Markup::Skip, &tail[e + 1..]));
            }
        } else {
            let closing = b.get(1) == Some(&b'/');
            let ns = if closing { 2 } else { 1 };
            if b.get(ns).is_some_and(|c| c.is_ascii_alphabetic()) {
                let nl = b[ns..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == b'-')
                    .count();
                let after_name = &tail[ns + nl..];
                // 标签名后面只能是空白 / `/` / `>`，否则不是标签（如 `<a@b.c>`）
                let boundary = after_name
                    .bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_whitespace() || c == b'/' || c == b'>');
                if let Some(e) = tag_end(after_name).filter(|_| boundary) {
                    let name = tail[ns..ns + nl].to_ascii_lowercase();
                    let markup = if closing {
                        Markup::Close(name)
                    } else {
                        Markup::Open(name, &after_name[..e])
                    };
                    return Some((lt, markup, &after_name[e + 1..]));
                }
            }
        }
        from = lt + 1;
    }
    None
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "times" => '×',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        _ => return None,
    })
}

/// 解码字符实体；不认识的、没写完的（缺 `;`）原样保留。
pub(super) fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let body = &rest[amp + 1..];
        let hit = body
            .find(';')
            .filter(|e| *e <= 10)
            .and_then(|e| entity(&body[..e]).map(|c| (c, e)));
        match hit {
            Some((c, e)) => {
                out.push(c);
                rest = &body[e + 1..];
            }
            None => {
                out.push('&');
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 取属性值（名字不分大小写；值可带单/双引号或不带；引号没收尾就取到末尾）。
fn attr(attrs: &str, want: &str) -> Option<String> {
    let mut s = attrs;
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if s.is_empty() {
            return None;
        }
        let nl = s
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(s.len());
        if nl == 0 {
            s = &s[1..]; // 孤立的 `=`
            continue;
        }
        let name = &s[..nl];
        s = s[nl..].trim_start();
        let mut val = "";
        if let Some(v) = s.strip_prefix('=') {
            let v = v.trim_start();
            match v.chars().next().filter(|c| matches!(c, '"' | '\'')) {
                Some(q) => {
                    let body = &v[1..];
                    let e = body.find(q).unwrap_or(body.len());
                    val = &body[..e];
                    s = body.get(e + 1..).unwrap_or("");
                }
                None => {
                    let e = v.find(char::is_whitespace).unwrap_or(v.len());
                    val = &v[..e];
                    s = &v[e..];
                }
            }
        }
        if name.eq_ignore_ascii_case(want) {
            return Some(decode_entities(val));
        }
    }
}

/// 把 `rest` 在 `</name` 处切开：返回 (元素内容, 闭合标签之后的剩余)。没有闭合标签就全算内容。
fn split_at_close<'a>(rest: &'a str, name: &str) -> (&'a str, &'a str) {
    // ASCII 小写化不改变字节偏移，可以直接拿来切原串
    let lower = rest.to_ascii_lowercase();
    match lower.find(&format!("</{name}")) {
        Some(c) => {
            let after = rest[c..].find('>').map_or("", |e| &rest[c + e + 1..]);
            (&rest[..c], after)
        }
        None => (rest, ""),
    }
}

impl Builder {
    fn html_push(&mut self, name: &str, el: El) {
        self.html_open.push(Open {
            name: name.to_string(),
            el,
        });
    }

    fn html_has(&self, el: El) -> bool {
        self.html_open.iter().any(|o| o.el == el)
    }

    /// 一段 HTML（一个行内标签，或一整个 HTML 块）。
    pub(super) fn html(&mut self, raw: &str) {
        let mut rest = raw;
        while let Some((lt, markup, after)) = next_markup(rest) {
            self.html_text(&rest[..lt]);
            rest = after;
            match markup {
                Markup::Skip => {}
                Markup::Close(name) => self.html_close(&name),
                Markup::Open(name, attrs) => rest = self.html_start(&name, attrs, rest),
            }
        }
        self.html_text(rest);
    }

    /// HTML 里的文字：解码实体，按 HTML 规则折叠空白（含换行的空白按软换行处理，
    /// 于是中文之间不补空格）。
    fn html_text(&mut self, raw: &str) {
        let text = decode_entities(raw);
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let ws = rest.len() - rest.trim_start().len();
            if ws > 0 {
                let ends_blank = self
                    .spans
                    .last()
                    .is_none_or(|s| s.text.ends_with(char::is_whitespace));
                // 段首、或紧跟在换行/空格之后的空白不产生任何东西
                if ends_blank {
                } else if rest[..ws].contains('\n') {
                    self.soft = true;
                } else if !self.soft {
                    self.push_span(" ", false);
                }
                rest = &rest[ws..];
                continue;
            }
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            self.text(&rest[..end], self.html_code > 0);
            rest = &rest[end..];
        }
    }

    /// `<pre>`：内容原样成代码块（里面的标签丢弃，`<code class="language-x">` 给出语言）。
    fn html_pre(&mut self, body: &str) {
        let mut lang = String::new();
        let mut text = String::new();
        let mut rest = body;
        while let Some((lt, markup, after)) = next_markup(rest) {
            text.push_str(&decode_entities(&rest[..lt]));
            rest = after;
            if let Markup::Open(name, attrs) = markup {
                if name == "br" {
                    text.push('\n');
                } else if name == "code" && lang.is_empty() {
                    let class = attr(attrs, "class").unwrap_or_default();
                    lang = class
                        .split_whitespace()
                        .find_map(|c| c.strip_prefix("language-").or(c.strip_prefix("lang-")))
                        .unwrap_or("")
                        .to_lowercase();
                }
            }
        }
        text.push_str(&decode_entities(rest));
        let text = text.strip_prefix('\n').unwrap_or(&text);
        let text = text.strip_suffix('\n').unwrap_or(text);
        self.flush_force();
        self.push(Kind::Code {
            lang,
            text: text.to_string(),
        });
    }

    /// 开始标签。返回处理后的剩余输入（`<pre>` / `<style>` 这类会自己吃掉内容）。
    fn html_start<'a>(&mut self, name: &str, attrs: &str, rest: &'a str) -> &'a str {
        match name {
            // 内容不是给人读的：连同闭合标签一起跳过
            "style" | "script" | "head" | "title" | "template" => {
                return split_at_close(rest, name).1;
            }
            "pre" => {
                let (body, after) = split_at_close(rest, "pre");
                self.html_pre(body);
                return after;
            }
            "br" => self.text("\n", false),
            "hr" => {
                self.flush_force();
                self.push(Kind::Rule);
            }
            "img" => {
                // alt 缺省时用文件名，好歹知道是哪张图
                let alt = attr(attrs, "alt").filter(|a| !a.is_empty()).or_else(|| {
                    let src = attr(attrs, "src")?;
                    Some(src.rsplit('/').next().unwrap_or(&src).to_string())
                });
                self.soft_gap();
                self.spans.push(Span {
                    text: alt.unwrap_or_default(),
                    image: true,
                    link: self.links.last().cloned(),
                    ..Span::default()
                });
            }
            "b" | "strong" => {
                self.bold += 1;
                self.html_push(name, El::Bold);
            }
            "i" | "em" | "cite" | "var" | "dfn" => {
                self.italic += 1;
                self.html_push(name, El::Italic);
            }
            "s" | "del" | "strike" => {
                self.strike += 1;
                self.html_push(name, El::Strike);
            }
            "code" | "kbd" | "samp" | "tt" => {
                self.html_code += 1;
                self.html_push(name, El::Code);
            }
            "a" => match attr(attrs, "href").filter(|h| !h.is_empty()) {
                Some(href) => {
                    self.links.push(href);
                    self.html_push(name, El::Link);
                }
                None => self.html_push(name, El::Anchor),
            },
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.html_close_implied(&["p"], &[]);
                self.flush_text();
                self.heading = name.as_bytes()[1] - b'0';
                self.html_push(name, El::Heading);
            }
            "p" => {
                self.html_close_implied(&["p"], &["li", "td", "th", "blockquote", "div"]);
                self.flush_text();
                self.html_push(name, El::Block);
            }
            "div" | "center" | "section" | "article" | "header" | "footer" | "main" | "nav"
            | "aside" | "figure" | "figcaption" | "details" | "dl" | "dt" | "dd" | "address"
            | "caption" => {
                self.flush_text();
                self.html_push(name, El::Block);
            }
            "summary" => {
                self.flush_text();
                self.bold += 1;
                self.html_push(name, El::Summary);
            }
            "blockquote" => {
                self.flush_force();
                self.quote = self.quote.saturating_add(1);
                self.html_push(name, El::Quote);
            }
            "ul" | "ol" | "menu" => {
                self.flush_text();
                let first = (name == "ol").then(|| {
                    attr(attrs, "start")
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(1)
                });
                self.lists.push(first);
                self.html_push(name, El::List);
            }
            "li" => {
                // 省略了 </li>：新的一项收掉同一列表里的上一项
                self.html_close_implied(&["li"], &["ul", "ol", "menu"]);
                // 外面没有列表：补一个无序列表，标记才有缩进列可落
                if !self.html_has(El::List) {
                    self.flush_text();
                    self.lists.push(None);
                    self.html_push("ul", El::List);
                }
                self.flush_force();
                self.marker = Some(self.next_marker());
                self.html_push(name, El::Item);
            }
            "table" => {
                // 表格里再套表格：模型是扁平的，放不下——内层的单元格并进外层
                if self.table.is_none() {
                    self.flush_force();
                    self.table = Some(TableBuild::default());
                    self.html_push(name, El::Table);
                }
            }
            "tr" if self.html_has(El::Table) => {
                self.html_close_implied(&["tr"], &["table"]);
                self.html_push(name, El::Row);
            }
            "td" | "th" if self.html_has(El::Table) => {
                self.html_close_implied(&["td", "th"], &["tr", "table"]);
                if !self.html_has(El::Row) {
                    self.html_push("tr", El::Row);
                }
                // 单元格之间的零散文字（缩进空白、<caption> 之外的杂项）不属于任何单元格
                self.spans.clear();
                self.soft = false;
                self.html_push(name, El::Cell);
            }
            // 不认识的标签：丢标签、留文字
            _ => {}
        }
        rest
    }

    /// 图片等「原子片段」直接进 spans、不经 `text()`，待处理的软换行要先落成空格。
    fn soft_gap(&mut self) {
        if std::mem::take(&mut self.soft) && !self.spans.is_empty() {
            self.push_span(" ", false);
        }
    }

    /// 闭合标签：收掉栈里最近的同名元素及其上面的一切；没有同名的就是多余的，忽略。
    fn html_close(&mut self, name: &str) {
        match self.html_open.iter().rposition(|o| o.name == name) {
            Some(i) => self.html_unwind(i),
            // 孤立的 </p> 常被当分段符用
            None if name == "p" => self.flush_text(),
            None => {}
        }
    }

    /// 省略闭合标签的情形：从栈顶往下找 `names` 之一并收掉；先碰到 `barriers` 就停
    /// （那说明要找的元素不在当前容器里）。
    fn html_close_implied(&mut self, names: &[&str], barriers: &[&str]) {
        for i in (0..self.html_open.len()).rev() {
            let n = self.html_open[i].name.as_str();
            if names.contains(&n) {
                self.html_unwind(i);
                return;
            }
            if barriers.contains(&n) {
                return;
            }
        }
    }

    /// 把栈收到只剩 `to` 个元素（文末以 0 调用，统一收尾所有没闭合的结构）。
    pub(super) fn html_unwind(&mut self, to: usize) {
        while self.html_open.len() > to {
            if let Some(open) = self.html_open.pop() {
                self.html_undo(open.el);
            }
        }
    }

    fn html_undo(&mut self, el: El) {
        match el {
            El::Bold => self.bold = self.bold.saturating_sub(1),
            El::Italic => self.italic = self.italic.saturating_sub(1),
            El::Strike => self.strike = self.strike.saturating_sub(1),
            El::Code => self.html_code = self.html_code.saturating_sub(1),
            El::Link => {
                self.links.pop();
            }
            El::Anchor => {}
            El::Heading => {
                self.flush_text();
                self.heading = 0;
            }
            El::Summary => {
                self.flush_text();
                self.bold = self.bold.saturating_sub(1);
            }
            El::Quote => {
                self.flush_text();
                self.quote = self.quote.saturating_sub(1);
            }
            El::List => {
                self.flush_text();
                self.lists.pop();
            }
            El::Item => self.flush_force(),
            El::Block => self.flush_text(),
            El::Cell => {
                self.soft = false;
                let cell = std::mem::take(&mut self.spans);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            El::Row => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    if row.is_empty() {
                        // 空行（<tr></tr>）不占位
                    } else if t.head.is_empty() && t.rows.is_empty() {
                        t.head = row; // 首行当表头（HTML 表格不一定写 <th>）
                    } else {
                        t.rows.push(row);
                    }
                }
            }
            El::Table => {
                if let Some(t) = self.table.take() {
                    self.spans.clear(); // 最后一个单元格之后的零散文字
                    if !t.head.is_empty() {
                        self.push(Kind::Table {
                            head: t.head,
                            rows: t.rows,
                        });
                    }
                }
            }
        }
    }

    /// 一段文字出块之后调用：没闭合的行内样式和标题到此为止，不带进下一段。
    pub(super) fn html_end_of_para(&mut self) {
        let mut i = 0;
        while i < self.html_open.len() {
            match self.html_open[i].el {
                El::Bold => self.bold = self.bold.saturating_sub(1),
                El::Italic => self.italic = self.italic.saturating_sub(1),
                El::Strike => self.strike = self.strike.saturating_sub(1),
                El::Code => self.html_code = self.html_code.saturating_sub(1),
                El::Link => {
                    self.links.pop();
                }
                El::Anchor => {}
                El::Heading => self.heading = 0,
                El::Summary => self.bold = self.bold.saturating_sub(1),
                _ => {
                    i += 1;
                    continue;
                }
            }
            self.html_open.remove(i);
        }
    }
}
