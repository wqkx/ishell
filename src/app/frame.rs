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
    /// 每帧的**非绘制**工作，由 [`eframe::App::logic`] 调用——**窗口最小化/不可见时也照常
    /// 调用**，这正是它存在的理由。
    ///
    /// eframe 只在窗口可见时调用 `App::ui`（`epi_integration.rs`：
    /// `if is_visible { app.update(); app.ui(); }`）。iShell 原本把每帧工作全放在 `ui` 里，
    /// 于是最小化窗口之后 MCP 请求就再也没人排空——现象是「最小化 iShell 之后 AI 完全操作
    /// 不了它」。这里放的三件事都不需要 `Ui`，且都不能因为窗口看不见就停摆：
    ///
    /// 1. 排空 MCP 请求 + 检查各会话待完成的 AI 命令运行；
    /// 2. 排空各会话的后台事件——`run_command` 靠终端输出里的哨兵判断命令结束，不喂数据
    ///    就永远等不到（事件只是搬进 `s.pending.*`，等窗口回来由 `process_frame_events`
    ///    消费，不会丢）；
    /// 3. 续下一帧——最小化时 eframe 会把重绘节流到 ≥100ms，但**前提是应用还在请求重绘**，
    ///    不续就直接睡死了。
    pub(super) fn pump_background(&mut self, ctx: &egui::Context) {
        self.drain_mcp_calls();
        let mut backlog = false;
        // 跨会话拷贝状态机要吃的那几条 pending 也在这里收走：它同样是纯后台推进，不碰 Ui，
        // 而且一旦停摆，`copy_between_sessions` 会卡在某个中间阶段既不前进也不超时。
        let mut relay_source: Vec<(u64, Result<u64, String>)> = Vec::new();
        let mut copy_done: Vec<(u64, u64, bool, String)> = Vec::new();
        let mut temp_key_trusted: Vec<(u64, bool, String)> = Vec::new();
        let mut temp_key_untrusted: Vec<(u64, bool, String)> = Vec::new();
        let mut direct_relay_started: Vec<u64> = Vec::new();
        let mut direct_relay_done: Vec<(u64, bool, String)> = Vec::new();
        for s in &mut self.sessions {
            backlog |= s.drain_events();
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
        // 必须在上面那轮 drain_events **之后**：文件读写的超时判定要晚于「本帧事件是否已经
        // 带来真正结果」，否则会跟刚好本帧到达的完成事件打时序竞争（见该方法注释）。
        self.check_file_op_timeouts();
        self.advance_cross_copy_jobs(
            temp_key_trusted,
            temp_key_untrusted,
            direct_relay_started,
            direct_relay_done,
            relay_source,
            copy_done,
        );
        // 事件没排空（每帧有预算，防止一次性把渲染拖垮）：马上再来一帧接着消化。
        // 这是**同线程**的 request_repaint，可靠（不可靠的是跨线程那条，见下面的说明）。
        if backlog {
            ctx.request_repaint();
        }
        // 必须在上面所有超时判定之后：那些判定全是每帧轮询的，而 egui 按需重绘——空闲窗口
        // 不转帧，它们就永远不被求值。这一下按最近的 deadline 排定时重绘，保证到点必有一帧。
        self.arm_timeout_repaint();

        // AI/MCP 控制的响应性兜底节奏。
        //
        // 背景：MCP 请求到达 socket 后由后台 tokio 线程 `ctx.request_repaint()` 唤醒 UI
        // 线程来排空。但实测发现——窗口彻底空闲、eframe 停在 `ControlFlow::Wait` 时，
        // **跨线程的 `request_repaint()` 唤醒会丢**：请求只能干等到别的事件偶然唤起一帧，
        // 实测单条 `list_sessions` 卡了 157 秒。而 MCP 代理是「每次工具调用新开一条连接」，
        // 所以这不是首次连接的一次性问题，每个调用都可能中招。
        //
        // 修法：改用可靠的 `request_repaint_after`——它设的 `WaitUntil` 是 OS 层定时唤醒。
        // 只要 AI 控制已启用且有已连接会话（此时反向转发 socket 正暴露、随时可能来请求），
        // 就每帧续一个短定时重绘。门控为假时完全不介入，零额外开销。
        if crate::store::load_mcp_consent() && self.sessions.iter().any(|s| s.connected) {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }

    pub(super) fn process_frame_events(&mut self, ui: &mut egui::Ui) {
        // AI/MCP 请求、后台事件、以及所有纯后台轮询（文件操作超时、跨会话拷贝、按 deadline
        // 排重绘）都已由 `pump_background` 做过——它挂在 `App::logic` 上，eframe 保证在
        // `App::ui` 之前调用，且窗口不可见时也调用。这里只负责把 `s.pending.*` 里攒下的
        // 东西落到 UI 状态上（开标签、贴图、弹 toast…），那些是真的需要 `Ui` 的部分。
        // 连接成功后初始化文件树
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
        // 终端通知的判据：都要在借用 sessions 之前算好。
        let active_uid = self.active.and_then(|i| self.sessions.get(i).map(|s| s.uid));
        let window_focused = self.ctx.input(|i| i.focused);
        let notify_mode = crate::store::load_ai_notify_mode();
        for s in &mut self.sessions {
            // 这里**不再** drain_events：`pump_background` 已经排空过（它挂在 `App::logic`
            // 上，eframe 保证先于 `App::ui` 调用）。再排一次会把 `Session::drain_events`
            // 里那份「每帧最多 2MB / 512 条」的预算翻倍——那份预算正是用来防止一个话痨
            // 远端把渲染饿死的，不能白白放大到两倍。
            if s.connected && !s.initialized {
                s.initialized = true;
                // 本机会话也初始化文件树：init_files 发 ListDir("."), 本机 worker 解析到家目录
                // （见 local::files）。SSH 会话则由 SFTP 解析到远端家目录。
                s.init_files();
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
            // 终端通知（BEL 响铃 / OSC 9/777）：AI CLI 等待确认或完成时提醒用户。
            //
            // 关于「不弹」的两条规则，写清楚免得日后又被当成 bug 来回改：
            //  1. **裸 BEL 一律不弹。** 它和 shell 补全失败、readline 报错是同一个字节，
            //     Claude Code 也会在既非「等你确认」也非「已完成」时响它——拿它当判据必然
            //     误报，这就是「通知不准确」的根因。要分得准只能靠发送方标注，也就是
            //     右键菜单「配置 AI 完成通知」装的那个 hook（见 NoticeKind）。
            //  2. 按 `AiNotifyMode` 分档：默认只提醒「需要人干涉」的，滤掉每轮都来的
            //     「任务完成」。第三方 OSC 9/777 没有标记但确实是在主动要求提醒，照弹。
            //  3. **窗口在前台、且通知来自你正看着的那个标签**才不弹——人就在那儿盯着。
            //     条件必须是这两个的与：只看「窗口在前台」会把后台标签的通知也吞掉，
            //     那才是真的丢消息。（曾经因为这条规则，用户在当前标签里 `printf '\a'`
            //     测不出任何反应，误以为功能坏了——所以判据必须是"标签级"而非"窗口级"。）
            for n in s.terminal.take_notices() {
                if !notice_should_alert(n.kind, notify_mode) {
                    continue;
                }
                if window_focused && Some(s.uid) == active_uid {
                    continue;
                }
                // 两行文本：OSC 带标题就 `标题：正文`，否则取有内容的那个；BEL 的正文是
                // 光标行预览，可能整行是空的（比如响铃时屏幕刚好被清），给一句兜底。
                let text = match (n.title, n.body.trim()) {
                    (Some(t), b) if !b.is_empty() => format!("{}：{b}", t.trim()),
                    (Some(t), _) => t.trim().to_string(),
                    (None, b) if !b.is_empty() => b.to_string(),
                    (None, _) => crate::i18n::tr("需要你处理", "Needs your attention").to_string(),
                };
                // 同一会话只保留最新一条：上一条说的事已经被这一条取代了，留着只是让浮层
                // 越堆越乱。不再按「3 秒内才合并」——间隔多久都一样是旧消息。
                self.ai_notices.retain(|x| x.session_uid != s.uid);
                // 焦点不在 iShell 上时顺带发一条系统通知（Ubuntu 的通知气泡 / macOS 通知
                // 中心）：这时候用户多半在别的窗口里，浮层卡片他根本看不见。
                // 传 uid 是为了让同一会话的后一条**替换**前一条，而不是在通知中心里越堆越多。
                if !window_focused {
                    os_notify(s.uid, &s.title, &text);
                }
                self.ai_notices.push(super::AiNotice {
                    session_uid: s.uid,
                    session_title: s.title.clone(),
                    text,
                });
            }
        }
        // 用户自己切回（并且正看着）某个标签，那条通知的使命就完成了——不该还要手动点掉它。
        // 条件与上面「不弹」的规则**严格对称**（窗口在前台 且 是活动标签）：少了 window_focused
        // 这一半，窗口在后台时刚给活动标签生成的通知会在同一帧被抹掉，那就是丢消息。
        // 系统气泡也一起关掉，否则通知中心里会留下一条早已看过的。
        //
        // 边沿触发（只在「正看着的标签」变化时做一次）：见 App::last_focused_tab。
        let focused_tab = if window_focused { active_uid } else { None };
        if focused_tab != self.last_focused_tab {
            self.last_focused_tab = focused_tab;
            if let Some(uid) = focused_tab {
                self.ai_notices.retain(|x| x.session_uid != uid);
                os_notify_close(uid);
            }
        }
        // 超时判定、跨会话拷贝推进、按 deadline 排重绘都在 `pump_background` 里——它们是
        // 纯后台轮询，放在只有窗口可见时才跑的这里，窗口一藏起来就全停摆了。
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
        // 本机↔远端传输善后推进（成功后剪切删源 / 刷新本机落地目录）
        self.process_local_xfers();
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

        // 续帧节奏见 `pump_background`——那边挂在 `App::logic` 上，最小化时也照常跑；
        // 放在这里的话窗口一最小化就停了（`App::ui` 不再被调用），MCP 会直接失联。
    }
}

/// 发一条**操作系统级**通知（Ubuntu 的通知气泡 / macOS 通知中心）。
///
/// 只在 iShell 窗口不在前台时调用——那时浮层卡片用户根本看不见，而"AI 在等你"这件事恰恰
/// 是他离开窗口期间最需要知道的。
///
/// 不引第三方通知库：各平台自带的命令行入口就够，且省掉一整条 D-Bus/AppKit 依赖。
/// 一律 `spawn` 不 `wait`——通知进程慢或卡住都不该拖住 UI 帧循环；失败（比如没装
/// `notify-send`）静默忽略，浮层卡片仍在，不会因此丢消息。
/// `notify-send` 的完整参数表（抽出来是为了能单测——通知本身在测试里没法观察）。
///
/// 末尾的 `--` 不可省：摘要/正文是位置参数，而正文来自终端输出，AI CLI 常输出以 `-`
/// 开头的 markdown 列表项——没有 `--` 时 `notify-send` 会把它当成选项解析，整条系统
/// 通知就被静默丢掉了。
///
/// 参数直接传给进程，不经 shell，正文没有被解释成命令的机会。`desktop-entry` 提示告诉
/// GNOME 这条通知属于哪个 `.desktop`，点击时才谈得上「激活已有窗口」而不是另开一个；
/// 它需要和窗口的 WM_CLASS/app_id 对上（见 `main.rs` 的 `APP_ID`）。
#[cfg(any(target_os = "linux", test))]
fn notify_send_args<'a>(summary: &'a str, body: &'a str) -> [&'a str; 9] {
    [
        "-a",
        "iShell",
        "-u",
        "normal",
        "-h",
        "string:desktop-entry:ishell",
        "--",
        summary,
        body,
    ]
}

/// 把字符串包成 GVariant 文本格式的字面量（`gdbus call` 的每个参数都按 GVariant 解析，
/// 裸字符串是非法的，必须带引号）。
///
/// 正文来自终端输出，什么都可能有：反斜杠、双引号、换行。前两者必须转义，换行直接换成
/// 空格——GVariant 字面量里的裸换行会让整条命令解析失败，而通知本来也是单行展示。
#[cfg(any(target_os = "linux", test))]
fn gvariant_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `org.freedesktop.Notifications.Notify` 的 gdbus 调用参数。
///
/// 为什么绕开 `notify-send` 走 D-Bus：替换（`replaces_id`）和拿回通知 id 是 Notify 方法
/// **自带**的能力，而 `notify-send` 要到 libnotify 0.8.0 才有对应的 `-r` / `-p`。
/// Ubuntu 22.04 上还是 0.7.9，给它传 `-p` 会「Unknown option」直接失败、一条通知都发不出去。
/// 走 D-Bus 就没有这个版本矩阵，所有发行版一个行为。
///
/// `replaces_id` 传 0 表示新建；传上一条的 id 就是原地替换它。
#[cfg(any(target_os = "linux", test))]
fn gdbus_notify_args(summary: &str, body: &str, replaces_id: u32) -> Vec<String> {
    [
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        // 没有通知服务时 gdbus 会一直等到它自己 25s 的默认超时，那条后台线程就挂在那儿。
        // 真实的 Notify 调用是毫秒级返回的，5s 足够宽松。放在 --method 之前，好让位置参数
        // 紧跟在方法名后面（测试就按这个位置关系断言）。
        "--timeout",
        "5",
        "--method",
        "org.freedesktop.Notifications.Notify",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .chain([
        gvariant_str("iShell"),          // app_name
        replaces_id.to_string(),         // replaces_id：0=新建，否则替换那一条
        gvariant_str(""),                // app_icon：留空，图标由 desktop-entry 决定
        gvariant_str(summary),
        gvariant_str(body),
        "[]".to_string(),                // actions：无按钮
        // desktop-entry 告诉 GNOME 这条通知属于哪个 .desktop，点击才谈得上「激活已有窗口」
        // 而不是另开一个（要和窗口 app_id 对上，见 main.rs 的 APP_ID）。
        "{'desktop-entry': <'ishell'>}".to_string(),
        // expire_timeout = -1（由通知服务决定）。**必须写成 `int32 -1` 这种带类型前缀的
        // 形式**：裸 `-1` 会被 gdbus 的选项解析当成一个未知短选项，直接打印 Usage 退出，
        // 一条通知都发不出去（实测过）。带前缀后首字符不是 `-`，就不会被误认。
        "int32 -1".to_string(),
    ])
    .collect()
}

/// 从 `gdbus call` 的输出里取通知 id。输出形如 `(uint32 12,)`。
#[cfg(any(target_os = "linux", test))]
fn parse_gdbus_uint32(out: &str) -> Option<u32> {
    let rest = out.split_once("uint32")?.1;
    let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 这条终端通知该不该提醒用户。
///
/// 分成独立函数是为了能直接测——策略藏在几百行的 `frame()` 里，改坏了没有任何东西拦得住，
/// 而这几条规则已经被来回改过三次了。
fn notice_should_alert(
    kind: crate::terminal::NoticeKind,
    mode: crate::store::AiNotifyMode,
) -> bool {
    use crate::store::AiNotifyMode as M;
    use crate::terminal::NoticeKind as K;
    match (kind, mode) {
        // 总开关关闭
        (_, M::Off) => false,
        // 裸响铃永不提醒：分不出类别，用它当判据必然误报（详见 NoticeKind::Bell）
        (K::Bell, _) => false,
        // 「任务完成」在「只提醒需要人干涉」这档下滤掉
        (K::Done, M::NeedsInput) => false,
        // 剩下的都提醒：明确标了 Need 的、以及第三方主动发的无标记 OSC 通知
        _ => true,
    }
}

/// 系统通知的操作，串行交给唯一一条通知线程处理。
#[cfg(target_os = "linux")]
enum NotifyOp {
    Show { uid: u64, summary: String, body: String },
    Close { uid: u64 },
}

/// 通往通知线程的队列。
///
/// 为什么必须串行、而不是每条通知起一个线程各自读写一张共享表：
/// 「本次要替换哪一条」这个决定依赖上一次调用**返回**的通知 id，而 id 要等 gdbus 跑完才
/// 拿得到。AI CLI 是成串发通知的——两条抢在第一次 gdbus 返回前进来，就都会读到「没有上
/// 一条」，于是各建一个气泡（本该只有一个），而且先建的那个 id 被后者覆盖、再也关不掉。
/// 反过来，用户在 id 写回之前切回标签，`Close` 也会扑空、气泡一直挂着。
///
/// 排成一条队列后，读 id、发通知、写回 id 天然是原子的，那张表也就不用再共享了
/// （直接是通知线程的局部变量，连锁都不需要）。
#[cfg(target_os = "linux")]
static NOTIFY_TX: std::sync::LazyLock<std::sync::mpsc::Sender<NotifyOp>> =
    std::sync::LazyLock::new(|| {
        let (tx, rx) = std::sync::mpsc::channel::<NotifyOp>();
        std::thread::spawn(move || {
            // uid → 该会话当前挂在通知服务里的那条通知 id
            let mut ids: std::collections::HashMap<u64, u32> = Default::default();
            while let Ok(op) = rx.recv() {
                match op {
                    NotifyOp::Show { uid, summary, body } => {
                        let replaces = ids.get(&uid).copied().unwrap_or(0);
                        match gdbus_notify(&summary, &body, replaces) {
                            Some(id) => {
                                ids.insert(uid, id);
                            }
                            // 没有 gdbus / 调用失败：退回 notify-send。发得出去，但没法替换
                            // 和关闭——宁可少两个新特性，也不能让通知整个消失。
                            None => {
                                ids.remove(&uid);
                                let _ = std::process::Command::new("notify-send")
                                    .args(notify_send_args(&summary, &body))
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn();
                            }
                        }
                    }
                    NotifyOp::Close { uid } => {
                        if let Some(id) = ids.remove(&uid) {
                            gdbus_close(id);
                        }
                    }
                }
            }
        });
        tx
    });

/// 发一条通知，返回通知服务分配的 id（失败返回 None）。在通知线程上同步执行。
#[cfg(target_os = "linux")]
fn gdbus_notify(summary: &str, body: &str, replaces: u32) -> Option<u32> {
    let out = std::process::Command::new("gdbus")
        .args(gdbus_notify_args(summary, body, replaces))
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_gdbus_uint32(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
fn gdbus_close(id: u32) {
    let _ = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--timeout",
            "5",
            "--method",
            "org.freedesktop.Notifications.CloseNotification",
            &id.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// 既非 linux 也非 macos 的目标上，下面的参数确实都用不到——别把这个 allow 挪到别的函数
// 头上（曾经在中间插入新函数时被带走过一次）。
#[allow(unused_variables)]
fn os_notify(uid: u64, session_title: &str, body: &str) {
    let summary = match crate::i18n::current() {
        crate::i18n::Lang::Zh => format!("iShell · {session_title}"),
        crate::i18n::Lang::En => format!("iShell · {session_title}"),
    };
    // 只是入队；发送在通知线程上做，UI 线程一步都不等（gdbus 最坏要 5s 才超时）。
    #[cfg(target_os = "linux")]
    let _ = NOTIFY_TX.send(NotifyOp::Show {
        uid,
        summary: summary.clone(),
        body: body.to_string(),
    });
    #[cfg(not(target_os = "linux"))]
    let _ = uid; // macOS 的 osascript 没有替换/关闭通知的等价能力，uid 用不上
    #[cfg(target_os = "macos")]
    {
        // osascript 只能收一段脚本文本，所以必须自己转义：反斜杠在前、双引号在后，
        // 顺序反了会把已转义的反斜杠再转一次。换行也要去掉（AppleScript 字符串不能跨行）。
        fn esc(s: &str) -> String {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace(['\n', '\r'], " ")
        }
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(body),
            esc(&summary)
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// 关掉某会话还挂在通知中心里的那条系统气泡（用户已经切回该标签、看过了）。
///
/// 走 `gdbus` 直接调 `org.freedesktop.Notifications.CloseNotification`——`notify-send`
/// 只能发不能关。缺 gdbus（非 GNOME/无 glib 工具）时静默失败：气泡留着而已，不影响功能。
fn os_notify_close(uid: u64) {
    #[cfg(target_os = "linux")]
    let _ = NOTIFY_TX.send(NotifyOp::Close { uid });
    #[cfg(not(target_os = "linux"))]
    let _ = uid;
}

#[cfg(test)]
mod os_notify_tests {
    /// 正文以 `-` 开头（AI CLI 输出 markdown 列表项时很常见）必须被 `--` 挡在选项解析之外，
    /// 否则 `notify-send` 会把它当选项、整条系统通知静默消失。
    #[test]
    fn body_starting_with_dash_is_after_double_dash() {
        let args = super::notify_send_args("iShell · web", "- 任务已完成");
        let sep = args
            .iter()
            .position(|a| *a == "--")
            .expect("必须有 -- 分隔符");
        assert_eq!(args[sep + 1], "iShell · web");
        assert_eq!(args[sep + 2], "- 任务已完成");
        assert_eq!(sep + 3, args.len(), "-- 之后只能有摘要和正文两个位置参数");
    }

    /// `-h desktop-entry` 是「点通知激活已有窗口」的关键，别在重构里掉了。
    #[test]
    fn carries_desktop_entry_hint() {
        let args = super::notify_send_args("s", "b");
        assert!(args.contains(&"string:desktop-entry:ishell"));
    }

    /// **别再给 notify-send 加选项。** 系统上装的很可能是 libnotify 0.7.9（Ubuntu 22.04），
    /// 它不认识 0.8.0 才加的 `-p`/`-r`/`-e`，而 notify-send 遇到未知选项是直接报错退出、
    /// 一条通知都不发。替换/关闭那套能力走 gdbus，不要往这里塞。
    #[test]
    fn notify_send_sticks_to_options_that_exist_in_0_7() {
        const SINCE_0_8: [&str; 6] = ["-p", "--print-id", "-r", "--replace-id", "-e", "--transient"];
        let args = super::notify_send_args("s", "b");
        for opt in SINCE_0_8 {
            assert!(!args.contains(&opt), "{opt} 在 libnotify 0.7.x 上会让整条通知发不出去");
        }
    }

    /// gdbus 的每个参数都按 GVariant 解析，裸字符串非法；正文来自终端输出，
    /// 引号/反斜杠/换行都必须处理掉，否则整条命令解析失败 = 通知静默消失。
    #[test]
    fn gvariant_escapes_quotes_backslashes_and_control_chars() {
        assert_eq!(super::gvariant_str(r#"a"b"#), r#""a\"b""#);
        assert_eq!(super::gvariant_str(r"a\b"), r#""a\\b""#);
        assert_eq!(super::gvariant_str("a\nb\tc"), r#""a b c""#);
        assert_eq!(super::gvariant_str(""), r#""""#);
    }

    /// replaces_id：0 = 新建，非 0 = 原地替换同一会话的上一条。位置固定在第 2 个参数，
    /// 传错位就会把它当成别的字段。
    #[test]
    fn notify_call_carries_replaces_id_in_the_right_slot() {
        let a = super::gdbus_notify_args("sum", "body", 0);
        let m = a.iter().position(|x| x == "--method").unwrap();
        // --method <名字> 之后依次是 app_name, replaces_id, app_icon, summary, body, ...
        assert_eq!(a[m + 2], r#""iShell""#);
        assert_eq!(a[m + 3], "0");
        assert_eq!(a[m + 5], r#""sum""#);
        assert_eq!(a[m + 6], r#""body""#);
        assert_eq!(super::gdbus_notify_args("s", "b", 77)[m + 3], "77");
        assert!(a.iter().any(|x| x.contains("desktop-entry")));
    }

    /// 位置参数一个都不能以 `-` 开头，否则 gdbus 的选项解析会把它当短选项，直接打印
    /// Usage 退出——通知一条都发不出去。踩过一次：expire_timeout 写成裸 `-1` 就是这样。
    /// 正文来自终端输出，同样可能以 `-` 开头（markdown 列表项），所以它必须被引号包住。
    #[test]
    fn no_positional_argument_can_look_like_an_option() {
        let a = super::gdbus_notify_args("- 摘要", "- 正文", 0);
        let m = a.iter().position(|x| x == "--method").unwrap();
        for (i, arg) in a.iter().enumerate().skip(m + 2) {
            assert!(!arg.starts_with('-'), "第 {i} 个位置参数 {arg:?} 会被当成选项");
        }
    }

    /// 拿不到 id 就既不能替换也不能关闭；输出格式是 `(uint32 12,)`。
    #[test]
    fn parses_the_notification_id_out_of_gdbus_output() {
        assert_eq!(super::parse_gdbus_uint32("(uint32 12,)\n"), Some(12));
        assert_eq!(super::parse_gdbus_uint32("(uint32 0,)"), Some(0));
        assert_eq!(super::parse_gdbus_uint32("Error: whatever"), None);
        assert_eq!(super::parse_gdbus_uint32(""), None);
    }
}

#[cfg(test)]
mod notice_policy_tests {
    use super::notice_should_alert as alert;
    use crate::store::AiNotifyMode as M;
    use crate::terminal::NoticeKind as K;

    /// 「通知不准确」的根因：裸 BEL 和 shell 补全失败、readline 报错是同一个字节，
    /// Claude Code 也会在既非等待确认、也非任务完成时响它。任何档位下都不该弹。
    #[test]
    fn a_bare_bell_never_alerts_in_any_mode() {
        for m in [M::Off, M::NeedsInput, M::All] {
            assert!(!alert(K::Bell, m), "裸响铃在 {m:?} 档下仍然弹了");
        }
    }

    /// 分档过滤只针对「任务完成」——它每轮都来。等你确认的那条任何档位都不能滤。
    #[test]
    fn done_is_filtered_only_in_needs_input_mode() {
        assert!(!alert(K::Done, M::NeedsInput));
        assert!(alert(K::Done, M::All));
        assert!(alert(K::Need, M::NeedsInput));
        assert!(alert(K::Need, M::All));
    }

    /// 第三方程序主动发 OSC 9/777 本身就是在明确要求提醒用户，不能因为没有 iShell 的
    /// 标记就被当成「任务完成」滤掉——那会漏掉 codex 之类工具真正等你处理的提示。
    #[test]
    fn untagged_osc_notifications_still_alert() {
        assert!(alert(K::Untagged, M::NeedsInput));
        assert!(alert(K::Untagged, M::All));
    }

    /// 总开关关掉就是全关，不留例外。
    #[test]
    fn off_silences_everything() {
        for k in [K::Need, K::Done, K::Untagged, K::Bell] {
            assert!(!alert(k, M::Off), "{k:?} 在 Off 档下仍然弹了");
        }
    }
}
