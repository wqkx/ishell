//! Spawn helpers for SSH worker UI commands. Split from mod.rs; behavior unchanged.

use std::sync::Arc;

use russh::client::Handle;

use super::auth::{exec_capture_bytes, ClientHandler};
use super::{sh_quote, UiSink};
use crate::proto::WorkerEvent;

pub(super) fn spawn_pdf_info(
    handle: Arc<Handle<ClientHandler>>,
    sink: UiSink,
    id: u64,
    path: String,
) {
    tokio::spawn(async move {
        let cmd = format!("pdfinfo {}", sh_quote(&path));
        let hint = crate::i18n::tr(
            "无法读取 PDF：远端需要 poppler-utils（Debian/Ubuntu: apt install poppler-utils）",
            "Cannot read PDF: remote needs poppler-utils (Debian/Ubuntu: apt install poppler-utils)",
        );
        // 失败统一走 FileLoadFailed：复用编辑器占位标签的「移除 + 提示」路径
        match exec_capture_bytes(&handle, &cmd).await {
            Ok((0, out, _)) => {
                let text = String::from_utf8_lossy(&out);
                let pages = text
                    .lines()
                    .find_map(|l| l.strip_prefix("Pages:"))
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                if pages > 0 {
                    sink.send(WorkerEvent::PdfInfo { id, path, pages });
                } else {
                    sink.send(WorkerEvent::FileLoadFailed {
                        id,
                        message: crate::i18n::tr("无法解析 PDF 页数", "Cannot parse PDF page count").into(),
                    });
                }
            }
            Ok((127, _, _)) => sink.send(WorkerEvent::FileLoadFailed { id, message: hint.into() }),
            Ok((code, _, err)) => {
                let e = err.trim().to_string();
                let msg = if e.is_empty() { format!("pdfinfo exit {code}") } else { e };
                sink.send(WorkerEvent::FileLoadFailed { id, message: msg });
            }
            Err(e) => sink.send(WorkerEvent::FileLoadFailed { id, message: e.to_string() }),
        }
    });
}

pub(super) fn spawn_pdf_page(
    handle: Arc<Handle<ClientHandler>>,
    sink: UiSink,
    path: String,
    page: u32,
    dpi: u32,
) {
    tokio::spawn(async move {
        // pdftoppm 不指定输出名时把 PNG 写到 stdout（已在目标环境实测）
        let cmd = format!(
            "pdftoppm -png -r {} -f {} -l {} {}",
            dpi.clamp(36, 300),
            page,
            page,
            sh_quote(&path)
        );
        let data = match exec_capture_bytes(&handle, &cmd).await {
            Ok((0, out, _)) if out.starts_with(b"\x89PNG") => out,
            _ => Vec::new(), // 失败：空数据，UI 显示该页加载失败
        };
        sink.send(WorkerEvent::PdfPage { path, page, data });
    });
}

pub(super) fn spawn_pdf_search(
    handle: Arc<Handle<ClientHandler>>,
    sink: UiSink,
    path: String,
    query: String,
) {
    tokio::spawn(async move {
        // pdftotext 输出以 \f（换页符）分页 → 逐页找命中（不分大小写）
        let cmd = format!("pdftotext {} -", sh_quote(&path));
        match exec_capture_bytes(&handle, &cmd).await {
            Ok((0, out, _)) => {
                let text = String::from_utf8_lossy(&out);
                // 扫描件/无文本层：提取结果只剩换页符与空白，明确告知（而非「无结果」误导）
                if text.chars().all(|c| c.is_whitespace() || c == '\u{c}') {
                    sink.send(WorkerEvent::PdfSearch {
                        path,
                        query,
                        hits: Vec::new(),
                        message: Some(crate::i18n::tr(
                            "该 PDF 无文本层（可能是扫描件），无法搜索",
                            "PDF has no text layer (scanned?), cannot search",
                        ).into()),
                    });
                    return;
                }
                let needle = query.to_lowercase();
                // 跨行回退匹配：pdftotext 会把版面断行输出，中文词组常被
                // 换行截断（无空格分词）——去掉全部空白后再比一次
                let needle_ns: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
                let mut hits: Vec<(u32, String)> = Vec::new();
                for (pi, page) in text.split('\u{c}').enumerate() {
                    if hits.len() >= 200 {
                        break;
                    }
                    let lower = page.to_lowercase();
                    let hit = lower.contains(&needle)
                        || (!needle_ns.is_empty()
                            && lower
                                .chars()
                                .filter(|c| !c.is_whitespace())
                                .collect::<String>()
                                .contains(&needle_ns));
                    if hit {
                        // 取首个命中行作为片段（截 ~80 字符）
                        let snippet = page
                            .lines()
                            .find(|l| l.to_lowercase().contains(&needle))
                            .map(|l| l.trim().chars().take(80).collect::<String>())
                            .unwrap_or_default();
                        hits.push((pi as u32 + 1, snippet));
                    }
                }
                sink.send(WorkerEvent::PdfSearch { path, query, hits, message: None });
            }
            Ok((127, _, _)) => sink.send(WorkerEvent::PdfSearch {
                path,
                query,
                hits: Vec::new(),
                message: Some(crate::i18n::tr(
                    "远端缺少 pdftotext（poppler-utils）",
                    "Remote missing pdftotext (poppler-utils)",
                ).into()),
            }),
            Ok((code, _, err)) => {
                let e = err.trim().to_string();
                sink.send(WorkerEvent::PdfSearch {
                    path,
                    query,
                    hits: Vec::new(),
                    message: Some(if e.is_empty() { format!("pdftotext exit {code}") } else { e }),
                });
            }
            Err(e) => sink.send(WorkerEvent::PdfSearch {
                path,
                query,
                hits: Vec::new(),
                message: Some(e.to_string()),
            }),
        }
    });
}
