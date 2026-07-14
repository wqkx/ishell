//! Editor-related frame event handling. Split from frame.rs; behavior unchanged.

use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{Eol, UiCommand};

use super::util::lock_mutex;
use super::{App, DocKind, EditorTab, SaveState};

type FramePlaceholder = (u64, String, String, u64, UnboundedSender<UiCommand>);
type FrameFilled = (u64, String, String, String, Eol, u32);
type FramePdfSearch = (u64, String, Vec<(u32, String)>, Option<String>);

impl App {
    pub(super) fn process_editor_load_events(
        &mut self,
        ui: &mut egui::Ui,
        new_placeholders: Vec<FramePlaceholder>,
        filled: Vec<FrameFilled>,
        load_progress: Vec<(u64, u64, u64)>,
        mut load_fail: Vec<u64>,
        pdf_infos: Vec<(u64, u32)>,
        pdf_pages: Vec<(u64, String, u32, Vec<u8>)>,
        new_docs: Vec<(u64, Vec<u8>)>,
        pdf_searches: Vec<FramePdfSearch>,
    ) {
        // 编辑器标签：立即建占位（loading）→ 进度更新 → 内容就位 → 失败移除。
        // PDF / Word 文档标签完整复用该框架（占位/进度/失败路径相同，就位时填充 doc 内容）。
        // docx 后台解析结果先收集（mpsc 无 peek；必须与其它事件一起纳入触发条件，
        // 否则「解析完成」那帧若无其它编辑器事件，下方块不执行 → 永远停在「渲染中」）
        let parsed: Vec<(
            u64,
            Result<
                (
                    crate::ui::docx::Doc,
                    std::collections::HashMap<String, egui::TextureHandle>,
                ),
                String,
            >,
        )> = self.doc_parse_rx.try_iter().collect();
        let opened_editor = !new_placeholders.is_empty()
            || !filled.is_empty()
            || !load_progress.is_empty()
            || !load_fail.is_empty()
            || !pdf_infos.is_empty()
            || !pdf_pages.is_empty()
            || !new_docs.is_empty()
            || !pdf_searches.is_empty()
            || !parsed.is_empty();
        if opened_editor {
            // 占位标签的 text_id 先在锁外分配（alloc_editor_id 借用 self）
            let mut ph_ids: Vec<egui::Id> = Vec::with_capacity(new_placeholders.len());
            for _ in &new_placeholders {
                ph_ids.push(self.alloc_editor_id());
            }
            let mut ed = lock_mutex(&self.editor_state);
            // 1) 新建占位标签（同服务器同文件已打开则切过去）
            for ((id, path, server, uid, tx), tid) in new_placeholders.into_iter().zip(ph_ids) {
                ed.focus = true;
                if let Some(i) = ed
                    .tabs
                    .iter()
                    .position(|t| t.uid == uid && t.editor.path == path)
                {
                    ed.active = i;
                } else {
                    let mut editor = crate::ui::editor::Editor::new(path, String::new());
                    editor.set_loading(true);
                    ed.tabs.push(EditorTab {
                        editor,
                        server,
                        uid,
                        cmd_tx: tx,
                        text_id: tid,
                        tid: id,
                        load_id: Some(id),
                        load_done: 0,
                        load_total: 0,
                        save: SaveState::Idle,
                        save_at: None,
                        save_done: 0,
                        save_total: 0,
                        save_done_at: None,
                        tail_offset: u64::MAX,
                        tail_pending: false,
                        tail_last: 0.0,
                        doc: None,
                        tail_carry: Vec::new(),
                    });
                    ed.active = ed.tabs.len() - 1;
                }
            }
            // 2) 下载进度 → 占位标签
            for (id, done, total) in load_progress {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    t.load_done = done;
                    t.load_total = total;
                }
            }
            // 3) 内容就位：占位标签变为可编辑、填入内容；恢复上次光标位置
            for (id, path, content, encoding, eol, mtime) in filled {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    let key = format!("{}|{}", t.server, path);
                    let large = content.len() > crate::limits::LARGE_FILE_BYTES;
                    let mut editor = crate::ui::editor::Editor::new(path, content);
                    editor.set_meta(encoding, eol, mtime);
                    // 大文件默认只读（整文件已在内存；状态栏可点「只读」解除）
                    if large {
                        editor.readonly = true;
                    }
                    if let Some(line) = crate::store::load_cursor_line(&key) {
                        editor.restore_line(line);
                    }
                    t.editor = editor;
                    t.load_id = None;
                }
            }
            // 3.5) 文档就位：占位标签变为 PDF / Word 查看器
            for (id, pages) in pdf_infos {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    t.doc = Some(DocKind::Pdf {
                        pages,
                        cur: 1,
                        zoom: 0.0,
                        cache: Vec::new(),
                        pending: std::collections::HashSet::new(),
                        flip_at: 0.0,
                        search: String::new(),
                        search_open: false,
                        hits: Vec::new(),
                        hit_sel: 0,
                        searching: false,
                        search_msg: None,
                    });
                    t.load_id = None;
                }
            }
            // docx 下载完成 → 后台线程解析 + 解码纹理（ctx.load_texture 线程安全），
            // UI 不冻结；占位文案切换为「渲染中 …」。结果经 doc_parse 通道回来装配。
            for (id, data) in new_docs {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                    t.editor.loading_note = Some(crate::i18n::tr("渲染中 …", "Rendering …").into());
                    // 进度条置满（下载已完成）
                    t.load_done = t.load_total.max(1);
                    t.load_total = t.load_total.max(1);
                    self.spawn_docx_parse(ui, id, data, t.uid, t.editor.path.clone());
                }
            }
            // 后台解析完成 → 装配文档标签
            for (id, res) in parsed {
                match res {
                    Ok((doc, images)) => {
                        if let Some(t) = ed.tabs.iter_mut().find(|t| t.load_id == Some(id)) {
                            let n = doc.blocks.len();
                            t.doc = Some(DocKind::Docx {
                                doc,
                                images,
                                heights: vec![0.0; n],
                                search: String::new(),
                                search_open: false,
                                hits: Vec::new(),
                                hit_sel: 0,
                                scroll_to: None,
                            });
                            t.load_id = None;
                            t.editor.loading_note = None;
                        }
                    }
                    Err(e) => {
                        self.toast = Some((
                            match crate::i18n::current() {
                                crate::i18n::Lang::Zh => format!("文档解析失败：{e}"),
                                crate::i18n::Lang::En => format!("Doc parse failed: {e}"),
                            },
                            ui.input(|i| i.time),
                        ));
                        load_fail.push(id);
                    }
                }
            }
            // PDF 查找结果 → 命中列表（跳到首个命中页）
            for (uid, path, hits_in, message) in pdf_searches {
                if let Some(t) = ed
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
                    if let Some(DocKind::Pdf {
                        hits,
                        hit_sel,
                        searching,
                        search_msg,
                        cur,
                        pages,
                        ..
                    }) = &mut t.doc
                    {
                        *searching = false;
                        *search_msg = message;
                        *hits = hits_in;
                        *hit_sel = 0;
                        if let Some((p, _)) = hits.first() {
                            *cur = (*p).clamp(1, *pages);
                        }
                    }
                }
            }
            // PDF 页渲染结果 → 页缓存
            for (uid, path, page, data) in pdf_pages {
                if let Some(t) = ed
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
                    if let Some(DocKind::Pdf { cache, pending, .. }) = &mut t.doc {
                        pending.remove(&page);
                        if data.is_empty() {
                            continue;
                        }
                        if let Ok(img) = image::load_from_memory(&data) {
                            let rgba = img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let color =
                                egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                            let tex = ui.ctx().load_texture(
                                format!("pdf:{uid}:{path}:{page}"),
                                color,
                                egui::TextureOptions::LINEAR,
                            );
                            cache.retain(|(p, _, _)| *p != page);
                            cache.push((page, tex, egui::vec2(size[0] as f32, size[1] as f32)));
                            // 小 LRU：只留最近 6 页（控制内存）
                            while cache.len() > 6 {
                                let _ = cache.remove(0);
                            }
                        }
                    }
                }
            }
            // 4) 失败：移除对应占位标签（含 TextEditState，避免加载失败仍占内存）
            for id in load_fail {
                if let Some(i) = ed.tabs.iter().position(|t| t.load_id == Some(id)) {
                    ed.remove_tab_at(ui.ctx(), i);
                }
            }
            // 编辑器是独立 deferred 子窗口：变化后必须显式唤醒它重绘（含进度条动画）。
            ui.ctx()
                .request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        }
    }

    pub(super) fn process_editor_save_events(
        &mut self,
        ui: &mut egui::Ui,
        saved: Vec<(u64, u64, String, u32)>,
        conflicts: Vec<(u64, u64, String)>,
        save_progress: Vec<(u64, String, u64, u64)>,
        save_failed: Vec<(u64, u64, String, String)>,
    ) {
        // 保存成功 → 更新对应标签 mtime（避免下次保存误判）；外部改动冲突 → 置标志，编辑器弹横幅。
        if !saved.is_empty()
            || !conflicts.is_empty()
            || !save_progress.is_empty()
            || !save_failed.is_empty()
        {
            let mut ed = lock_mutex(&self.editor_state);
            // 进度事件（FileSaveProgress）没有请求 id，仍按 path 匹配——只驱动动画，
            // 偶尔撞车顶多是动画进度短暂不准，不影响最终保存结果的正确性。
            for (uid, path, done, total) in save_progress {
                if let Some(t) = ed
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
                    t.save_done = done;
                    t.save_total = total;
                }
            }
            let mut close_after_save: Vec<(u64, u64)> = Vec::new(); // (uid, tid)
            // path 只是事件携带的上下文信息，实际匹配标签靠 (uid, id)——不再需要它，前缀下划线。
            for (uid, id, _path, mtime) in saved {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.tid == id) {
                    t.editor.set_mtime(mtime); // 回填服务器新 mtime，避免下次保存把「自己刚写入」误判为外部改动
                                               // 取出本次保存发出时的签名与关闭意图（Saving 状态里）；非 Saving 则忽略这条确认。
                    let (sent_rev, close_after) = match &t.save {
                        SaveState::Saving { rev, close_after } => (rev.clone(), *close_after),
                        _ => continue,
                    };
                    // 仅当修订签名（正文+编码+行尾）与发出保存时完全一致才算已保存
                    //（保存期间用户又编辑、或切了编码/行尾 → 远端并非该状态，不能清 dirty，也不能关闭）
                    if t.editor.save_rev() == sent_rev {
                        t.save = SaveState::Idle;
                        t.editor.mark_saved();
                        if close_after {
                            close_after_save.push((uid, t.tid));
                        }
                    } else if close_after {
                        // 保存期间内容又变了但用户要「保存并关闭」：用最新内容再存一次，
                        // 存完（届时签名一致）再关闭；否则「保存并关闭」会静默不生效。
                        let _ = t.cmd_tx.send(UiCommand::WriteFile {
                            id: t.tid,
                            path: t.editor.path.clone(),
                            content: t.editor.content.clone(),
                            encoding: t.editor.encoding().to_string(),
                            eol: t.editor.eol(),
                            expect_mtime: t.editor.mtime(),
                            force: false,
                        });
                        t.begin_save(true); // 重新进入保存中，保持关闭意图
                    } else {
                        // 保存成功但内容已变、无关闭意图：解锁，保留 dirty 交用户再存
                        t.save = SaveState::Idle;
                    }
                }
            }
            for (uid, id, _path) in conflicts {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.tid == id) {
                    // 冲突：进入 Conflict（未写入，保留 dirty）；「保存并关闭」意图自然丢弃，交用户处理
                    t.save = SaveState::Conflict;
                    t.save_at = None; // 冲突未写入：中止「已保存」动画
                    t.save_done_at = None;
                }
            }
            for (uid, id, _path, message) in save_failed {
                if let Some(t) = ed.tabs.iter_mut().find(|t| t.uid == uid && t.tid == id) {
                    t.save = SaveState::Idle; // 失败：解锁重试；dirty 未被清，标签仍显示未保存；关闭意图丢弃
                    t.save_at = None; // 中止保存动画
                    t.save_done_at = None;
                }
                self.toast = Some((
                    match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("保存失败：{message}"),
                        crate::i18n::Lang::En => format!("Save failed: {message}"),
                    },
                    ui.input(|i| i.time),
                ));
            }
            // 「保存并关闭」：确认成功后移除标签
            for (uid, tid) in close_after_save {
                if let Some(i) = ed.tabs.iter().position(|t| t.uid == uid && t.tid == tid) {
                    ed.remove_tab_at(ui.ctx(), i);
                }
            }
            ui.ctx()
                .request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        }    }
}
