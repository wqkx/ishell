//! 每帧事件处理：排空 worker 事件、填充编辑器标签、重连与内存整理。
//! 从 App::ui 拆出，行为不变。

use tokio::sync::mpsc::UnboundedSender;

use crate::proto::{Eol, UiCommand};
use crate::ui::file_panel;

use super::util::{lock_mutex, trim_memory};
use super::{App, ImageTab};

/// 本帧从各会话 drain 出的占位标签：(id, path, server title, uid, cmd_tx)
type FramePlaceholder = (u64, String, String, u64, UnboundedSender<UiCommand>);
/// 本帧待填入编辑器的文件内容：(uid, id, path, content, encoding, eol, mtime)。
/// **必须带 uid**：id 来自各会话**独立**的 `next_xfer` 计数器（见 file_actions.rs），
/// 跨会话必然重号，而编辑器标签是全局一张表——只按 id 找标签会填进别的会话的标签里。
type FrameFilled = (u64, u64, String, String, String, Eol, u32);
/// PDF 查找命中：(uid, path, hits, message)
type FramePdfSearch = (u64, String, Vec<(u32, String)>, Option<String>);

impl App {
    /// 处理本帧会话事件与编辑器/传输副作用（在布局绘制之前调用）。
    pub(super) fn process_frame_events(&mut self, ui: &mut egui::Ui) {
        // 0) AI/MCP 控制通道：排空本地 socket 收到的请求 + 检查各会话待完成的命令运行
        self.drain_mcp_calls();
        // 1) 排空所有会话的后台事件，并在连接成功后初始化文件树
        // 身份用会话 uid（稳定唯一），title 仅作显示——避免同名会话（默认 title=用户名）串台。
        let mut new_placeholders: Vec<FramePlaceholder> = Vec::new();
        let mut filled: Vec<FrameFilled> = Vec::new();
        let mut load_progress: Vec<(u64, u64, u64, u64)> = Vec::new(); // uid, id, done, total
        let mut load_fail: Vec<(u64, u64)> = Vec::new(); // uid, id
        let mut new_images: Vec<(String, Vec<u8>, String, u64)> = Vec::new(); // path, data, title, uid
        let mut saved: Vec<(u64, u64, String, u32)> = Vec::new(); // uid, id, path, mtime
        let mut save_progress: Vec<(u64, String, u64, u64)> = Vec::new(); // uid, path, done, total
        let mut conflicts: Vec<(u64, u64, String)> = Vec::new(); // uid, id, path
        let mut save_failed: Vec<(u64, u64, String, String)> = Vec::new(); // uid, id, path, message
        let mut warns: Vec<String> = Vec::new(); // 需弹 toast 的警告
        let mut too_large: Vec<(u64, u64, String, u64)> = Vec::new(); // uid, id, path, size
        let mut tails: Vec<(u64, String, Vec<u8>, u64, bool)> = Vec::new(); // uid, path, data, offset, truncated
        let mut pdf_infos: Vec<(u64, u64, u32)> = Vec::new(); // uid, 占位 id, 页数
        let mut pdf_pages: Vec<(u64, String, u32, Vec<u8>)> = Vec::new(); // uid, path, page, png
        let mut pdf_searches: Vec<FramePdfSearch> = Vec::new();
        let mut new_docs: Vec<(u64, u64, Vec<u8>)> = Vec::new(); // uid, 占位 id, docx 字节
        let mut relay_source: Vec<(u64, Result<u64, String>)> = Vec::new(); // op_id, Ok(size)/Err(msg)
        let mut copy_done: Vec<(u64, u64, bool, String)> = Vec::new(); // uid, op_id, ok, message
        let mut temp_key_trusted: Vec<(u64, bool, String)> = Vec::new();
        let mut temp_key_untrusted: Vec<u64> = Vec::new();
        let mut direct_relay_started: Vec<u64> = Vec::new();
        let mut direct_relay_done: Vec<(u64, bool, String)> = Vec::new();
        let mut evt_backlog = false;
        for s in &mut self.sessions {
            // 事件积压未排空（每帧预算保护渲染）时安排下一帧继续消化
            evt_backlog |= s.drain_events();
            if s.connected && !s.initialized {
                s.initialized = true;
                // Phase 1：本机会话的文件浏览尚未接本地 FS，先不初始化文件树（否则会发
                // ListDir，本机 worker 目前忽略，文件面板会一直转圈）。后续阶段接上后移除此 gate。
                if !s.cfg.is_local() {
                    s.init_files();
                }
            }
            for (id, path) in s.pending.placeholder.drain(..) {
                new_placeholders.push((id, path, s.title.clone(), s.uid, s.cmd_tx.clone()));
            }
            for (id, path, content, encoding, eol, mtime) in s.pending.open.drain(..) {
                filled.push((s.uid, id, path, content, encoding, eol, mtime));
            }
            for (id, path, mtime) in s.pending.saved.drain(..) {
                saved.push((s.uid, id, path, mtime));
            }
            for (path, done, total) in s.pending.save_progress.drain(..) {
                save_progress.push((s.uid, path, done, total));
            }
            for (path, data, offset, truncated) in s.pending.tail.drain(..) {
                tails.push((s.uid, path, data, offset, truncated));
            }
            for (id, path) in s.pending.conflict.drain(..) {
                conflicts.push((s.uid, id, path));
            }
            for w in s.pending.warn.drain(..) {
                warns.push(w);
            }
            for (id, path, msg) in s.pending.save_failed.drain(..) {
                save_failed.push((s.uid, id, path, msg));
            }
            for (id, path, size) in s.pending.too_large.drain(..) {
                too_large.push((s.uid, id, path, size));
            }
            for (id, done, total) in s.pending.load_progress.drain(..) {
                load_progress.push((s.uid, id, done, total));
            }
            for (id, msg) in s.pending.load_fail.drain(..) {
                load_fail.push((s.uid, id));
                // PDF 缺 poppler 等打开失败：保留文案弹 toast（原先丢弃 message，用户只见标签消失）
                if !msg.is_empty() {
                    warns.push(msg);
                }
            }
            for (path, data) in s.pending.image.drain(..) {
                new_images.push((path, data, s.title.clone(), s.uid));
            }
            for (id, pages) in s.pending.pdf_info.drain(..) {
                pdf_infos.push((s.uid, id, pages));
            }
            for (path, page, data) in s.pending.pdf_page.drain(..) {
                pdf_pages.push((s.uid, path, page, data));
            }
            for (path, hits, message) in s.pending.pdf_search.drain(..) {
                pdf_searches.push((s.uid, path, hits, message));
            }
            for (id, data) in s.pending.doc.drain(..) {
                new_docs.push((s.uid, id, data));
            }
            for x in s.pending.relay_source.drain(..) {
                relay_source.push(x);
            }
            for (id, ok, message) in s.pending.copy_done.drain(..) {
                copy_done.push((s.uid, id, ok, message));
            }
            for x in s.pending.temp_key_trusted.drain(..) {
                temp_key_trusted.push(x);
            }
            for x in s.pending.temp_key_untrusted.drain(..) {
                temp_key_untrusted.push(x);
            }
            for x in s.pending.direct_relay_started.drain(..) {
                direct_relay_started.push(x);
            }
            for x in s.pending.direct_relay_done.drain(..) {
                direct_relay_done.push(x);
            }
        }
        // 必须在上面这个 drain_events 循环之后调用：文件读写超时判定要晚于"本帧事件是否
        // 已经带来真正结果"的判断，否则会跟刚好本帧到达的完成事件打时序竞争（见该方法注释）。
        self.check_file_op_timeouts();
        self.advance_cross_copy_jobs(
            temp_key_trusted,
            temp_key_untrusted,
            direct_relay_started,
            direct_relay_done,
            relay_source,
            copy_done,
        );
        if evt_backlog {
            self.ctx.request_repaint();
        }
        // 必须在上面所有超时判定之后：那些判定全是每帧轮询的，而 egui 按需重绘——空闲窗口
        // 不转帧，它们就永远不被求值。这一下按最近的 deadline 排定时重绘，保证到点必有一帧。
        self.arm_timeout_repaint();
        // 设置持久化失败（磁盘满/只读/权限）也冒泡成顶部 toast，避免「以为已保存、其实没落盘」。
        warns.extend(crate::store::take_setting_write_errors());
        // 警告（如编码丢字）弹顶部 toast
        if let Some(w) = warns.into_iter().next_back() {
            self.toast = Some((w, self.ctx.input(|i| i.time)));
        }
        // 打开时发现文件实际超限：移除占位标签（复用 load_fail 移除逻辑），并在对应会话的文件面板
        // 弹「打开大文件」确认，确认后走 force=true 重新打开（列表里的旧大小已过时，双击前无法预判）。
        for (uid, id, path, size) in too_large {
            load_fail.push((uid, id));
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
        self.process_editor_load_events(
            ui,
            new_placeholders,
            filled,
            load_progress,
            load_fail,
            pdf_infos,
            pdf_pages,
            new_docs,
            pdf_searches,
        );
        self.process_editor_save_events(ui, saved, conflicts, save_progress, save_failed);
        // 必须在 process_editor_save_events 之后：保存超时判定要晚于「本帧是否已带来真正的
        // 保存结果」的处理，否则会与刚好本帧到达的 FileSaved/Failed/Conflict 打时序竞争
        //（明明已成功却先被判超时）。理由同 check_file_op_timeouts。
        self.check_editor_save_timeouts(ui);

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

        // AI/MCP 控制的响应性兜底节奏。
        //
        // 背景：MCP 请求（list_sessions/run_command/…）到达 socket 后由后台 tokio 线程
        // `ctx.request_repaint()` 唤醒 UI 线程来 `drain_mcp_calls()`。但实测发现——窗口彻底
        // 空闲、eframe 停在 `ControlFlow::Wait` 时，**跨线程的 `request_repaint()` 唤醒会丢**：
        // 请求只能干等到别的事件（SSH keepalive/定时器）偶然唤起一帧才被处理，实测单条
        // `list_sessions` 因此卡了 157 秒。而 MCP 代理是「每次工具调用新开一条连接」，所以
        // 这不是首次连接的一次性问题，而是每个调用都可能中招。
        //
        // 修法：不依赖那个会丢的跨线程唤醒，改用可靠的 `request_repaint_after`——它设的
        // `WaitUntil` 是 OS 层定时唤醒，连窗口被最小化/遮挡（compositor 停发重绘）时也照样
        // 触发。只要 AI 控制已启用且有已连接会话（此时反向转发 socket 正暴露、随时可能来
        // 请求），就每帧续一个短定时重绘，形成稳定的低频节奏，保证请求 ≤150ms 被排空。
        // 门控条件为假（普通用户没开 AI 控制，或没有活跃会话）时完全不介入，零额外开销。
        if crate::store::load_mcp_consent() && self.sessions.iter().any(|s| s.connected) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}
