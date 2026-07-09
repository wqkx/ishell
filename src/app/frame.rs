//! 每帧事件处理：排空 worker 事件、填充编辑器标签、重连与内存整理。
//! 从 App::ui 拆出，行为不变。

use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{Eol, UiCommand};
use crate::ui::file_panel;

use super::util::{lock_mutex, trim_memory};
use super::{App, DocKind, EditorTab, ImageTab, SaveState};

/// 本帧从各会话 drain 出的占位标签：(id, path, server title, uid, cmd_tx)
type FramePlaceholder = (u64, String, String, u64, UnboundedSender<UiCommand>);
/// 本帧待填入编辑器的文件内容：(id, path, content, encoding, eol, mtime)
type FrameFilled = (u64, String, String, String, Eol, u32);
/// PDF 查找命中：(uid, path, hits, message)
type FramePdfSearch = (u64, String, Vec<(u32, String)>, Option<String>);

impl App {
    /// 处理本帧会话事件与编辑器/传输副作用（在布局绘制之前调用）。
    pub(super) fn process_frame_events(&mut self, ui: &mut egui::Ui) {
        // 1) 排空所有会话的后台事件，并在连接成功后初始化文件树
        // 身份用会话 uid（稳定唯一），title 仅作显示——避免同名会话（默认 title=用户名）串台。
        let mut new_placeholders: Vec<FramePlaceholder> = Vec::new();
        let mut filled: Vec<FrameFilled> = Vec::new();
        let mut load_progress: Vec<(u64, u64, u64)> = Vec::new();
        let mut load_fail: Vec<u64> = Vec::new();
        let mut new_images: Vec<(String, Vec<u8>, String, u64)> = Vec::new(); // path, data, title, uid
        let mut saved: Vec<(u64, String, u32)> = Vec::new(); // uid, path, mtime
        let mut save_progress: Vec<(u64, String, u64, u64)> = Vec::new(); // uid, path, done, total
        let mut conflicts: Vec<(u64, String)> = Vec::new(); // uid, path
        let mut save_failed: Vec<(u64, String, String)> = Vec::new(); // uid, path, message
        let mut warns: Vec<String> = Vec::new(); // 需弹 toast 的警告
        let mut too_large: Vec<(u64, u64, String, u64)> = Vec::new(); // uid, id, path, size
        let mut tails: Vec<(u64, String, Vec<u8>, u64, bool)> = Vec::new(); // uid, path, data, offset, truncated
        let mut pdf_infos: Vec<(u64, u32)> = Vec::new(); // 占位 id, 页数
        let mut pdf_pages: Vec<(u64, String, u32, Vec<u8>)> = Vec::new(); // uid, path, page, png
        let mut pdf_searches: Vec<FramePdfSearch> = Vec::new();
        let mut new_docs: Vec<(u64, Vec<u8>)> = Vec::new(); // 占位 id, docx 字节
        let mut evt_backlog = false;
        for s in &mut self.sessions {
            // 事件积压未排空（每帧预算保护渲染）时安排下一帧继续消化
            evt_backlog |= s.drain_events();
            if s.connected && !s.initialized {
                s.initialized = true;
                s.init_files();
            }
            for (id, path) in s.pending.placeholder.drain(..) {
                new_placeholders.push((id, path, s.title.clone(), s.uid, s.cmd_tx.clone()));
            }
            for (id, path, content, encoding, eol, mtime) in s.pending.open.drain(..) {
                filled.push((id, path, content, encoding, eol, mtime));
            }
            for (path, mtime) in s.pending.saved.drain(..) {
                saved.push((s.uid, path, mtime));
            }
            for (path, done, total) in s.pending.save_progress.drain(..) {
                save_progress.push((s.uid, path, done, total));
            }
            for (path, data, offset, truncated) in s.pending.tail.drain(..) {
                tails.push((s.uid, path, data, offset, truncated));
            }
            for path in s.pending.conflict.drain(..) {
                conflicts.push((s.uid, path));
            }
            for w in s.pending.warn.drain(..) {
                warns.push(w);
            }
            for (path, msg) in s.pending.save_failed.drain(..) {
                save_failed.push((s.uid, path, msg));
            }
            for (id, path, size) in s.pending.too_large.drain(..) {
                too_large.push((s.uid, id, path, size));
            }
            for p in s.pending.load_progress.drain(..) {
                load_progress.push(p);
            }
            for (id, msg) in s.pending.load_fail.drain(..) {
                load_fail.push(id);
                // PDF 缺 poppler 等打开失败：保留文案弹 toast（原先丢弃 message，用户只见标签消失）
                if !msg.is_empty() {
                    warns.push(msg);
                }
            }
            for (path, data) in s.pending.image.drain(..) {
                new_images.push((path, data, s.title.clone(), s.uid));
            }
            for x in s.pending.pdf_info.drain(..) {
                pdf_infos.push(x);
            }
            for (path, page, data) in s.pending.pdf_page.drain(..) {
                pdf_pages.push((s.uid, path, page, data));
            }
            for (path, hits, message) in s.pending.pdf_search.drain(..) {
                pdf_searches.push((s.uid, path, hits, message));
            }
            for x in s.pending.doc.drain(..) {
                new_docs.push(x);
            }
        }
        if evt_backlog {
            self.ctx.request_repaint();
        }
        // 警告（如编码丢字）弹顶部 toast
        if let Some(w) = warns.into_iter().next_back() {
            self.toast = Some((w, self.ctx.input(|i| i.time)));
        }
        // 打开时发现文件实际超限：移除占位标签（复用 load_fail 移除逻辑），并在对应会话的文件面板
        // 弹「打开大文件」确认，确认后走 force=true 重新打开（列表里的旧大小已过时，双击前无法预判）。
        for (uid, id, path, size) in too_large {
            load_fail.push(id);
            if let Some(s) = self.sessions.iter_mut().find(|s| s.uid == uid) {
                s.files.dialog = Some(file_panel::Dialog::ConfirmOpenLarge { path, size });
            }
        }
        // 跟随模式（tail -f）：应用增量 + 定时轮询下一次读取。
        // 注意：跟随期间不更新 tab 的 mtime——外部对文件「中间」的修改无法检测，
        // 保留旧 mtime 让保存必走冲突确认流程，避免静默覆盖他人修改。
        {
            let now = self.ctx.input(|i| i.time);
            let mut edst = lock_mutex(&self.editor_state);
            let mut any_follow = false;
            for (uid, path, data, offset, truncated) in tails {
                if let Some(t) = edst
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
                    t.tail_pending = false;
                    t.tail_offset = offset;
                    if !t.editor.follow {
                        continue; // 已关闭跟随：丢弃迟到的数据
                    }
                    if truncated {
                        t.editor.append_tail(crate::i18n::tr(
                            "\n--- 文件被截断/轮转，以下为新内容 ---\n",
                            "\n--- file truncated/rotated, new content follows ---\n",
                        ));
                    }
                    if !data.is_empty() {
                        // 跨块解码：与上一块留下的不完整尾字节拼接；UTF-8 时把本块末尾
                        // 不完整的多字节序列留到下一块（跨块字符不再变 �）
                        let mut bytes = std::mem::take(&mut t.tail_carry);
                        bytes.extend_from_slice(&data);
                        let enc = encoding_rs::Encoding::for_label(t.editor.encoding().as_bytes())
                            .unwrap_or(encoding_rs::UTF_8);
                        if enc == encoding_rs::UTF_8 {
                            let valid = match std::str::from_utf8(&bytes) {
                                Ok(_) => bytes.len(),
                                Err(e) => e.valid_up_to(),
                            };
                            // 仅当截断发生在末尾 ≤3 字节内才视为「不完整序列」暂存；
                            // 中间的真实坏字节照常替换输出，避免 carry 死循环
                            if bytes.len() - valid <= 3 && valid < bytes.len() {
                                t.tail_carry = bytes.split_off(valid);
                            }
                        }
                        if !bytes.is_empty() {
                            let (cow, _, _) = enc.decode(&bytes);
                            let txt = cow.replace("\r\n", "\n");
                            t.editor.append_tail(&txt);
                        }
                    }
                }
            }
            for t in edst.tabs.iter_mut() {
                if t.editor.follow {
                    any_follow = true;
                    if !t.tail_pending && t.tail_offset != u64::MAX && now - t.tail_last > 1.0 {
                        t.tail_pending = true;
                        t.tail_last = now;
                        let _ = t.cmd_tx.send(UiCommand::TailFile {
                            path: t.editor.path.clone(),
                            offset: t.tail_offset,
                        });
                    }
                }
            }
            if any_follow {
                // 维持轮询节奏 + 唤醒编辑器窗口显示新内容
                self.ctx
                    .request_repaint_after(std::time::Duration::from_millis(500));
                self.ctx
                    .request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
            }
        }
        // 跨服务器中转任务推进（下载完→上传，上传完→剪切则删源）
        self.process_relays();
        // 跨服务器直传任务推进（完成则删源/刷新；失败则弹「转中转」）
        self.process_direct_jobs();
        for (path, data, server, uid) in new_images {
            self.image.focus = true; // 打开/切换后聚焦看图窗口
                                     // 同一会话同一图片已打开则切到该标签（身份用 uid，不用可能重名的 title）
            if let Some(i) = self
                .image
                .tabs
                .iter()
                .position(|t| t.uid == uid && t.path == path)
            {
                self.image.active = i;
                continue;
            }
            match image::load_from_memory(&data) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let name = format!("img:{server}:{path}");
                    let tex = ui
                        .ctx()
                        .load_texture(name, color, egui::TextureOptions::LINEAR);
                    self.image.tabs.push(ImageTab {
                        server,
                        uid,
                        path,
                        tex,
                        data,
                        size: egui::vec2(size[0] as f32, size[1] as f32),
                        zoom: 0.0,
                        offset: egui::Vec2::ZERO,
                    });
                    self.image.active = self.image.tabs.len() - 1;
                }
                Err(e) => {
                    let msg = match crate::i18n::current() {
                        crate::i18n::Lang::Zh => format!("图片解码失败：{e}"),
                        crate::i18n::Lang::En => format!("Decode failed: {e}"),
                    };
                    if let Some(sess) = self.sessions.iter_mut().find(|s| s.uid == uid) {
                        sess.status = msg;
                    }
                }
            }
        }
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
            // 4) 失败：移除对应占位标签
            for id in load_fail {
                if let Some(i) = ed.tabs.iter().position(|t| t.load_id == Some(id)) {
                    ed.tabs.remove(i);
                    if ed.active >= ed.tabs.len() {
                        ed.active = ed.tabs.len().saturating_sub(1);
                    }
                }
            }
            // 编辑器是独立 deferred 子窗口：变化后必须显式唤醒它重绘（含进度条动画）。
            ui.ctx()
                .request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        }
        // 保存成功 → 更新对应标签 mtime（避免下次保存误判）；外部改动冲突 → 置标志，编辑器弹横幅。
        if !saved.is_empty()
            || !conflicts.is_empty()
            || !save_progress.is_empty()
            || !save_failed.is_empty()
        {
            let mut ed = lock_mutex(&self.editor_state);
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
            let mut close_after_save: Vec<(u64, String)> = Vec::new();
            for (uid, path, mtime) in saved {
                if let Some(t) = ed
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
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
                            close_after_save.push((uid, t.editor.path.clone()));
                        }
                    } else if close_after {
                        // 保存期间内容又变了但用户要「保存并关闭」：用最新内容再存一次，
                        // 存完（届时签名一致）再关闭；否则「保存并关闭」会静默不生效。
                        let _ = t.cmd_tx.send(UiCommand::WriteFile {
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
            for (uid, path) in conflicts {
                if let Some(t) = ed
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
                    // 冲突：进入 Conflict（未写入，保留 dirty）；「保存并关闭」意图自然丢弃，交用户处理
                    t.save = SaveState::Conflict;
                    t.save_at = None; // 冲突未写入：中止「已保存」动画
                    t.save_done_at = None;
                }
            }
            for (uid, path, message) in save_failed {
                if let Some(t) = ed
                    .tabs
                    .iter_mut()
                    .find(|t| t.uid == uid && t.editor.path == path)
                {
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
            for (uid, path) in close_after_save {
                if let Some(i) = ed
                    .tabs
                    .iter()
                    .position(|t| t.uid == uid && t.editor.path == path)
                {
                    let closed = ed.tabs.remove(i);
                    if closed.doc.is_none() {
                        crate::store::save_cursor_line(
                            &format!("{}|{}", closed.server, closed.editor.path),
                            closed.editor.caret_line(),
                        );
                    }
                    if ed.active >= ed.tabs.len() && !ed.tabs.is_empty() {
                        ed.active = ed.tabs.len() - 1;
                    }
                    ed.trim_request = true;
                }
            }
            ui.ctx()
                .request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        }

        // 断线自动重连：到点的执行重连，并安排下次唤醒（即使无交互也能触发）
        let now = std::time::Instant::now();
        let mut due: Vec<usize> = Vec::new();
        let mut next_wake: Option<std::time::Duration> = None;
        for (i, s) in self.sessions.iter().enumerate() {
            if let Some(at) = s.reconnect_at {
                if now >= at {
                    due.push(i);
                } else {
                    let d = at - now;
                    next_wake = Some(next_wake.map_or(d, |w: std::time::Duration| w.min(d)));
                }
            }
        }
        for i in due {
            if let Some(s) = self.sessions.get_mut(i) {
                s.reconnect_tries += 1;
            }
            self.reconnect_session(i);
        }
        if let Some(d) = next_wake {
            ui.ctx().request_repaint_after(d);
        }

        // 编辑器关闭标签后请求归还内存（deferred 回调里无法直接动 App，用共享标志传出）
        {
            let mut ed = lock_mutex(&self.editor_state);
            if ed.trim_request {
                ed.trim_request = false;
                self.trim_after = Some(4);
            }
        }
        // 关闭编辑器后延迟归还内存（等 galley 缓存淘汰）
        if let Some(n) = self.trim_after {
            if n == 0 {
                trim_memory();
                self.trim_after = None;
            } else {
                self.trim_after = Some(n - 1);
                ui.ctx().request_repaint();
            }
        }

        // 自检：注入网络曲线波形以核对点密度
        if self.demo_net {
            if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                s.net_hist.down.clear();
                s.net_hist.up.clear();
                // 仅 30 个点，便于核对「从右侧起、向左生长」与点密度
                for i in 0..30 {
                    let t = i as f64;
                    s.net_hist
                        .down
                        .push_back(((t * 0.4).sin() * 0.5 + 0.5) * 5.0e6);
                    s.net_hist
                        .up
                        .push_back(((t * 0.3).cos() * 0.5 + 0.5) * 2.0e6);
                }
            }
        }

        // 自检：注入假 GPU 数据并保持详情窗打开
        if self.demo_gpu {
            if let Some(s) = self.active.and_then(|i| self.sessions.get_mut(i)) {
                if let Some(si) = s.sysinfo.as_mut() {
                    si.gpus = vec![
                        crate::proto::GpuInfo {
                            index: 0,
                            name: "RTX 4090".into(),
                            util: 73.0,
                            mem_used_mb: 18000,
                            mem_total_mb: 24564,
                        },
                        crate::proto::GpuInfo {
                            index: 1,
                            name: "RTX 4090".into(),
                            util: 12.0,
                            mem_used_mb: 2000,
                            mem_total_mb: 24564,
                        },
                    ];
                }
            }
            self.popups.gpu = Some(egui::pos2(130.0, 130.0));
            self.popups.gpu_just_opened = true;
        }

        // 进程详情返回 -> 填充小窗
        if let Some(idx) = self.active {
            let detail = self
                .sessions
                .get_mut(idx)
                .and_then(|s| s.proc_detail.take());
            if let Some((pid, cmd, cwd, exe)) = detail {
                if let Some(pp) = &mut self.popups.proc {
                    if pp.pid == pid {
                        pp.cmd = cmd;
                        pp.cwd = cwd;
                        pp.exe = exe;
                    }
                }
            }
        }
    }
}
