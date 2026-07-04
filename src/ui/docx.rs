//! 轻量 Word(docx) 阅读视图：zip 容器 + `word/document.xml` 拉流解析。
//!
//! 定位是「重排阅读」而非版式保真：段落/标题/加粗斜体下划线/列表/表格（含单元格内
//! 图片）/内嵌图片（按文档指定的显示尺寸），页眉页脚、分栏、浮动定位、公式等不支持。
//! 标题识别读取 `word/styles.xml` 的 outlineLvl（样式 id 本地化也能正确映射）。
//! `.doc` 老二进制格式不在此处理（上层提示另存为 .docx）。

use std::collections::HashMap;
use std::io::Read;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText};

use crate::theme::Palette;

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
    Para { spans: Vec<Span>, heading: u8, bullet: bool, align: Align },
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

/// 解析 docx 字节。
pub fn parse(data: &[u8]) -> anyhow::Result<Doc> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))?;
    let read_entry = |zip: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str| -> Option<Vec<u8>> {
        let mut f = zip.by_name(name).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        Some(buf)
    };
    let doc_xml = read_entry(&mut zip, "word/document.xml")
        .ok_or_else(|| anyhow::anyhow!(crate::i18n::tr("不是有效的 docx（缺 word/document.xml）", "Not a valid docx (missing word/document.xml)")))?;

    // 关系表：rId → media 目标路径（图片引用解析用）
    let mut rels: HashMap<String, String> = HashMap::new();
    if let Some(rx) = read_entry(&mut zip, "word/_rels/document.xml.rels") {
        let mut reader = quick_xml::Reader::from_reader(rx.as_slice());
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Empty(e)) | Ok(quick_xml::events::Event::Start(e)) if e.local_name().as_ref() == b"Relationship" => {
                    let (mut id, mut target) = (None, None);
                    for a in e.attributes().flatten() {
                        match a.key.as_ref() {
                            b"Id" => id = String::from_utf8(a.value.to_vec()).ok(),
                            b"Target" => target = String::from_utf8(a.value.to_vec()).ok(),
                            _ => {}
                        }
                    }
                    if let (Some(i), Some(t)) = (id, target) {
                        rels.insert(i, t);
                    }
                }
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
    }

    // 样式表：styleId → 标题层级（outlineLvl+1）。样式 id 本地化（如 "1"=标题1）也能映射。
    let mut style_heading: HashMap<String, u8> = HashMap::new();
    if let Some(sx) = read_entry(&mut zip, "word/styles.xml") {
        let mut reader = quick_xml::Reader::from_reader(sx.as_slice());
        let mut buf = Vec::new();
        let mut cur_style: Option<String> = None;
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => match e.local_name().as_ref() {
                    b"style" => cur_style = attr_val(&e, b"styleId"),
                    b"outlineLvl" => {
                        if let (Some(sid), Some(v)) = (&cur_style, attr_val(&e, b"val")) {
                            if let Ok(lvl) = v.parse::<u8>() {
                                style_heading.insert(sid.clone(), (lvl + 1).min(6));
                            }
                        }
                    }
                    _ => {}
                },
                Ok(quick_xml::events::Event::End(e)) if e.local_name().as_ref() == b"style" => cur_style = None,
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
    }

    // 主文档拉流解析
    let mut blocks: Vec<Block> = Vec::new();
    let mut used_media: Vec<String> = Vec::new();
    {
        let mut reader = quick_xml::Reader::from_reader(doc_xml.as_slice());
        let mut buf = Vec::new();
        // 段状态
        let mut spans: Vec<Span> = Vec::new();
        let mut heading = 0u8;
        let mut bullet = false;
        let mut align = Align::Left;
        // run 格式状态
        let (mut b, mut i, mut u) = (false, false, false);
        let mut in_rpr = false;
        let mut in_text = false;
        // 图片显示尺寸（wp:extent 在 blip 之前出现）
        let mut extent: (f32, f32) = (0.0, 0.0);
        // 表格状态（只处理最外层表格；嵌套内容并入单元格）
        let mut tbl_depth = 0usize;
        let mut table: Vec<Vec<Vec<CellPiece>>> = Vec::new();
        let mut row: Vec<Vec<CellPiece>> = Vec::new();
        let mut cell: Vec<CellPiece> = Vec::new();

        // 单元格里追加文本（与上一片段合并）
        fn cell_text(cell: &mut Vec<CellPiece>, s: &str) {
            if let Some(CellPiece::Text(t)) = cell.last_mut() {
                t.push_str(s);
            } else {
                cell.push(CellPiece::Text(s.to_string()));
            }
        }
        let mut flush_para = |spans: &mut Vec<Span>, heading: &mut u8, bullet: &mut bool, align: &mut Align, blocks: &mut Vec<Block>| {
            if !spans.is_empty() {
                blocks.push(Block::Para { spans: std::mem::take(spans), heading: *heading, bullet: *bullet, align: *align });
            }
            *heading = 0;
            *bullet = false;
            *align = Align::Left;
        };

        loop {
            let ev = match reader.read_event_into(&mut buf) {
                Ok(e) => e,
                Err(_) => break,
            };
            use quick_xml::events::Event;
            match &ev {
                Event::Start(e) | Event::Empty(e) => {
                    let empty = matches!(&ev, Event::Empty(_));
                    match e.local_name().as_ref() {
                        b"tbl" => {
                            if tbl_depth == 0 {
                                flush_para(&mut spans, &mut heading, &mut bullet, &mut align, &mut blocks);
                                table.clear();
                            }
                            tbl_depth += 1;
                        }
                        b"tr" if tbl_depth == 1 => row.clear(),
                        b"tc" if tbl_depth == 1 => cell.clear(),
                        b"rPr" if !empty => in_rpr = true,
                        b"b" if in_rpr => b = !attr_off(e),
                        b"i" if in_rpr => i = !attr_off(e),
                        b"u" if in_rpr => u = !attr_val(e, b"val").is_some_and(|v| v == "none"),
                        b"pStyle" => {
                            if let Some(v) = attr_val(e, b"val") {
                                // 首选 styles.xml 的 outlineLvl 映射；退路：样式名含 Heading/标题+数字
                                if let Some(lvl) = style_heading.get(&v) {
                                    heading = *lvl;
                                } else if v.to_ascii_lowercase().contains("heading") || v.contains("标题") {
                                    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
                                    heading = digits.parse::<u8>().unwrap_or(1).clamp(1, 6);
                                }
                            }
                        }
                        b"outlineLvl" => {
                            // 段落直接指定大纲级别
                            if let Some(v) = attr_val(e, b"val") {
                                if let Ok(lvl) = v.parse::<u8>() {
                                    heading = (lvl + 1).min(6);
                                }
                            }
                        }
                        b"numPr" => bullet = true,
                        b"jc" => {
                            // 段落对齐：center/right/end → 居中/居右（both/distribute 按左对齐处理）
                            align = match attr_val(e, b"val").as_deref() {
                                Some("center") => Align::Center,
                                Some("right") | Some("end") => Align::Right,
                                _ => Align::Left,
                            };
                        }
                        b"t" => in_text = true,
                        b"br" | b"cr" => {
                            if tbl_depth > 0 {
                                cell_text(&mut cell, "\n");
                            } else {
                                push_span(&mut spans, "\n", b, i, u);
                            }
                        }
                        b"tab" if !in_text => {
                            if tbl_depth > 0 {
                                cell_text(&mut cell, "\t");
                            } else {
                                push_span(&mut spans, "    ", b, i, u);
                            }
                        }
                        b"extent" => {
                            // wp:extent cx/cy（EMU）→ 显示像素
                            let cx = attr_val(e, b"cx").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                            let cy = attr_val(e, b"cy").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                            extent = ((cx / EMU_PER_PX) as f32, (cy / EMU_PER_PX) as f32);
                        }
                        b"blip" => {
                            // 图片引用：r:embed=rId → rels → word/ 下的目标路径
                            if let Some(rid) = attr_val(e, b"embed") {
                                if let Some(target) = rels.get(&rid) {
                                    let name = target.trim_start_matches("/word/").trim_start_matches("./").to_string();
                                    if tbl_depth > 0 {
                                        cell.push(CellPiece::Image(name.clone()));
                                    } else {
                                        flush_para(&mut spans, &mut heading, &mut bullet, &mut align, &mut blocks);
                                        blocks.push(Block::Image { name: name.clone(), w: extent.0, h: extent.1 });
                                    }
                                    if !used_media.contains(&name) {
                                        used_media.push(name);
                                    }
                                    extent = (0.0, 0.0);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"p" => {
                        if tbl_depth == 0 {
                            flush_para(&mut spans, &mut heading, &mut bullet, &mut align, &mut blocks);
                        } else {
                            // 表格内段落：文本换行；样式状态必须复位，否则污染表格后的正文段
                            cell_text(&mut cell, "\n");
                            heading = 0;
                            bullet = false;
                            align = Align::Left;
                        }
                    }
                    b"rPr" => in_rpr = false,
                    b"r" => {
                        b = false;
                        i = false;
                        u = false;
                    }
                    b"t" => in_text = false,
                    b"tc" if tbl_depth == 1 => {
                        // 收尾：去掉末尾多余换行；末段落若成了空文本则丢弃
                        let mut drop_last = false;
                        if let Some(CellPiece::Text(t)) = cell.last_mut() {
                            let trimmed = t.trim_end().to_string();
                            drop_last = trimmed.is_empty();
                            *t = trimmed;
                        }
                        if drop_last && cell.len() > 1 {
                            cell.pop();
                        }
                        row.push(std::mem::take(&mut cell));
                    }
                    b"tr" if tbl_depth == 1 => {
                        if !row.is_empty() {
                            table.push(std::mem::take(&mut row));
                        }
                    }
                    b"tbl" => {
                        tbl_depth = tbl_depth.saturating_sub(1);
                        if tbl_depth == 0 && !table.is_empty() {
                            blocks.push(Block::Table(std::mem::take(&mut table)));
                        }
                    }
                    _ => {}
                },
                Event::Text(t) => {
                    if in_text {
                        if let Ok(txt) = t.unescape() {
                            if tbl_depth > 0 {
                                cell_text(&mut cell, &txt);
                            } else {
                                push_span(&mut spans, &txt, b, i, u);
                            }
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        flush_para(&mut spans, &mut heading, &mut bullet, &mut align, &mut blocks);
    }

    // 读取用到的媒体文件
    let mut media = Vec::new();
    for name in used_media {
        if let Some(bytes) = read_entry(&mut zip, &format!("word/{name}")) {
            media.push((name, bytes));
        }
    }
    Ok(Doc { blocks, media })
}

/// 相同格式并入上一个 span，减少碎片。
fn push_span(spans: &mut Vec<Span>, text: &str, b: bool, i: bool, u: bool) {
    if let Some(last) = spans.last_mut() {
        if last.bold == b && last.italic == i && last.underline == u {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(Span { text: text.to_string(), bold: b, italic: i, underline: u });
}

/// 读取属性值（按 local name 匹配，忽略命名空间前缀）。
fn attr_val(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let k = a.key.as_ref();
        let local = k.rsplit(|c| *c == b':').next().unwrap_or(k);
        (local == key).then(|| String::from_utf8_lossy(&a.value).into_owned())
    })
}

/// `<w:b w:val="0"/>` 之类的显式关闭。
fn attr_off(e: &quick_xml::events::BytesStart) -> bool {
    attr_val(e, b"val").is_some_and(|v| v == "0" || v == "false" || v == "none")
}

/// 渲染单个内容块。
fn render_block(ui: &mut egui::Ui, block: &Block, images: &HashMap<String, egui::TextureHandle>) {
    match block {
        Block::Para { spans, heading, bullet, align } => {
            let size = match heading {
                1 => 21.0,
                2 => 18.0,
                3 => 16.0,
                4..=6 => 15.0,
                _ => 14.0,
            };
            let mut job = LayoutJob::default();
            if *bullet {
                job.append("•  ", 0.0, TextFormat::simple(FontId::proportional(size), Palette::TEXT_DIM));
            }
            for s in spans {
                let mut fmt = TextFormat::simple(FontId::proportional(size), if *heading > 0 || s.bold { Color32::from_rgb(0x17, 0x15, 0x12) } else { Palette::TEXT });
                fmt.italics = s.italic;
                if s.underline {
                    fmt.underline = egui::Stroke::new(1.0, Palette::TEXT_DIM);
                }
                job.append(&s.text, 0.0, fmt);
            }
            // 宽度取整：稳定 LayoutJob 哈希，命中 egui 的 galley 缓存（否则每帧微小的
            // 浮点宽度变化会让全部段落每帧重排——大文档卡顿与内存暴涨的主因）
            job.wrap.max_width = ui.available_width().floor().max(40.0);
            // 段落对齐（w:jc）：直接以 Label 为放置单元（不能再包 horizontal——
            // 行容器会占满整行宽，把 top_down 的水平对齐吞掉，全部变成居左）
            let layout = match align {
                Align::Center => egui::Layout::top_down(egui::Align::Center),
                Align::Right => egui::Layout::top_down(egui::Align::Max),
                Align::Left => egui::Layout::top_down(egui::Align::Min),
            };
            ui.with_layout(layout, |ui| {
                ui.label(job);
            });
            if *heading > 0 {
                ui.add_space(3.0);
            }
        }
        Block::Table(rows) => {
            let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            if cols == 0 {
                return;
            }
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, Palette::BORDER))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::same(6))
                .show(ui, |ui| {
                    egui::Grid::new(rows.as_ptr()).striped(true).min_col_width(40.0).spacing([16.0, 5.0]).show(ui, |ui| {
                        for (ri, r) in rows.iter().enumerate() {
                            for c in 0..cols {
                                ui.vertical(|ui| {
                                    let Some(cell) = r.get(c) else { return };
                                    for piece in cell {
                                        match piece {
                                            CellPiece::Text(t) => {
                                                if !t.is_empty() {
                                                    // 折行限宽：长文本不再把表格撑出显示边界
                                                    let color = if ri == 0 { Color32::from_rgb(0x17, 0x15, 0x12) } else { Palette::TEXT };
                                                    let job = LayoutJob::simple(t.clone(), FontId::proportional(12.5), color, 260.0);
                                                    ui.label(job);
                                                }
                                            }
                                            CellPiece::Image(name) => {
                                                if let Some(tex) = images.get(name) {
                                                    let size = tex.size_vec2();
                                                    let w = size.x.min(220.0).max(1.0);
                                                    ui.add(egui::Image::new((tex.id(), egui::vec2(w, w / size.x * size.y))).corner_radius(2.0));
                                                }
                                            }
                                        }
                                    }
                                });
                            }
                            ui.end_row();
                        }
                    });
                });
        }
        Block::Image { name, w, h } => {
            if let Some(tex) = images.get(name) {
                let orig = tex.size_vec2();
                // 文档指定的显示尺寸优先（HiDPI 文档常缩小放置大图）；未指定按原图
                let (mut dw, mut dh) = if *w > 1.0 && *h > 1.0 { (*w, *h) } else { (orig.x, orig.y) };
                let maxw = (ui.available_width() - 8.0).max(1.0);
                if dw > maxw {
                    dh *= maxw / dw;
                    dw = maxw;
                }
                ui.add(egui::Image::new((tex.id(), egui::vec2(dw, dh))).corner_radius(2.0));
            } else {
                ui.label(RichText::new(crate::i18n::tr("[图片]", "[image]")).color(Palette::TEXT_DIM).size(12.0));
            }
        }
    }
}

/// 提取某内容块的纯文本（查找用；图片块为空串）。
pub fn block_text(block: &Block) -> String {
    match block {
        Block::Para { spans, .. } => spans.iter().map(|s| s.text.as_str()).collect(),
        Block::Table(rows) => {
            let mut out = String::new();
            for r in rows {
                for cell in r {
                    for p in cell {
                        if let CellPiece::Text(t) = p {
                            out.push_str(t);
                            out.push(' ');
                        }
                    }
                }
            }
            out
        }
        Block::Image { .. } => String::new(),
    }
}

/// 视口裁剪渲染（长文档性能核心）：屏幕外且已知高度的块直接占位跳过；
/// 视口内正常渲染并更新高度缓存。首帧全量布局，其后滚动只付可见部分成本。
/// `hilite` = 当前查找命中块（淡染标识）；`scroll_to` = 待滚动目标块（消费后置 None）。
pub fn render_virtual(
    ui: &mut egui::Ui,
    doc: &Doc,
    images: &HashMap<String, egui::TextureHandle>,
    heights: &mut Vec<f32>,
    vp: egui::Rect,
    hilite: Option<usize>,
    scroll_to: &mut Option<usize>,
) {
    if heights.len() != doc.blocks.len() {
        heights.resize(doc.blocks.len(), 0.0);
    }
    ui.spacing_mut().item_spacing.y = 7.0;
    let origin = ui.min_rect().top();
    let full_w = ui.available_width().floor();
    for (bi, block) in doc.blocks.iter().enumerate() {
        let h = heights[bi];
        let top = ui.cursor().top() - origin; // 相对内容起点（与 vp 同坐标系）
        let skip = h > 0.0 && (top + h < vp.min.y - 300.0 || top > vp.max.y + 300.0) && *scroll_to != Some(bi);
        let before = ui.cursor().top();
        if skip {
            // 屏幕外：按缓存高度占位（±300px 余量防边缘跳动）
            ui.allocate_space(egui::vec2(full_w, h));
        } else {
            render_block(ui, block, images);
            heights[bi] = (ui.cursor().top() - before - ui.spacing().item_spacing.y).max(1.0);
        }
        let rect = egui::Rect::from_min_max(egui::pos2(ui.min_rect().left(), before), egui::pos2(ui.min_rect().right(), ui.cursor().top()));
        if *scroll_to == Some(bi) {
            ui.scroll_to_rect(rect, Some(egui::Align::Center));
            *scroll_to = None;
        }
        if hilite == Some(bi) {
            // 当前命中块：淡黄染 + 细边框（α 低不遮字）
            let w = Palette::WARN;
            ui.painter().rect_filled(rect.expand2(egui::vec2(4.0, 2.0)), 4.0, egui::Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), 22));
            ui.painter().rect_stroke(rect.expand2(egui::vec2(4.0, 2.0)), 4.0, egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), 120)), egui::StrokeKind::Outside);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_docx(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opt = zip::write::SimpleFileOptions::default();
            for (name, content) in entries {
                w.start_file(*name, opt).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn parse_paragraphs_and_styles() {
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p>
            <w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t> normal</w:t></w:r></w:p>
            <w:p><w:pPr><w:numPr/></w:pPr><w:r><w:t>item</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
        </w:body></w:document>"#;
        let doc = parse(&make_docx(&[("word/document.xml", xml)])).unwrap();
        assert_eq!(doc.blocks.len(), 4);
        assert!(matches!(&doc.blocks[0], Block::Para { heading: 1, .. }));
        match &doc.blocks[1] {
            Block::Para { spans, .. } => {
                assert_eq!(spans.len(), 2);
                assert!(spans[0].bold && spans[0].text == "bold");
                assert!(!spans[1].bold);
            }
            _ => panic!("expect para"),
        }
        assert!(matches!(&doc.blocks[2], Block::Para { bullet: true, .. }));
        match &doc.blocks[3] {
            Block::Table(rows) => {
                assert_eq!(rows[0].len(), 2);
                assert!(matches!(&rows[0][0][0], CellPiece::Text(t) if t == "A"));
                assert!(matches!(&rows[0][1][0], CellPiece::Text(t) if t == "B"));
            }
            _ => panic!("expect table"),
        }
    }

    #[test]
    fn localized_style_via_styles_xml_and_no_table_pollution() {
        // 样式 id 为本地化 "1"（中文 Word 的标题1），靠 styles.xml 的 outlineLvl 映射识别
        let styles = r#"<?xml version="1.0"?><w:styles xmlns:w="x">
            <w:style w:styleId="1"><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style>
        </w:styles>"#;
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
            <w:p><w:pPr><w:pStyle w:val="1"/></w:pPr><w:r><w:t>中文标题</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:pPr><w:pStyle w:val="1"/></w:pPr><w:r><w:t>C</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            <w:p><w:r><w:t>after table</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let doc = parse(&make_docx(&[("word/document.xml", xml), ("word/styles.xml", styles)])).unwrap();
        assert!(matches!(&doc.blocks[0], Block::Para { heading: 1, .. }));
        // 表格内的 pStyle 不得污染表格后的正文段
        assert!(matches!(&doc.blocks[2], Block::Para { heading: 0, .. }), "表格后正文被误判为标题");
    }

    #[test]
    fn image_extent_size() {
        // extent 952500 EMU = 100px
        let rels = r#"<?xml version="1.0"?><Relationships xmlns="x"><Relationship Id="rId1" Target="media/img1.png"/></Relationships>"#;
        let xml = r#"<?xml version="1.0"?><w:document xmlns:w="x" xmlns:wp="y" xmlns:a="z" xmlns:r="r"><w:body>
            <w:p><w:r><w:drawing><wp:inline><wp:extent cx="952500" cy="476250"/>
            <a:blip r:embed="rId1"/></wp:inline></w:drawing></w:r></w:p>
        </w:body></w:document>"#;
        let doc = parse(&make_docx(&[
            ("word/document.xml", xml),
            ("word/_rels/document.xml.rels", rels),
            ("word/media/img1.png", "fakepng"),
        ]))
        .unwrap();
        match &doc.blocks[0] {
            Block::Image { name, w, h } => {
                assert_eq!(name, "media/img1.png");
                assert!((*w - 100.0).abs() < 0.6 && (*h - 50.0).abs() < 0.6, "w={w} h={h}");
            }
            _ => panic!("expect image"),
        }
        assert_eq!(doc.media.len(), 1);
    }
}
