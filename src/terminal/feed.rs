//! Remote byte feed, scrollback collection, and resize reflow helpers.

use std::io::Write;

use super::{
    osc::parse_osc7,
    vt::{find_sub, incomplete_utf8_tail, serialize_row, strip_ansi_to_text},
    Terminal, DEFAULT_SCROLLBACK,
};

/// AI 捕获缓冲上限：超过后从前端裁掉最早的部分（兜底极端长输出，不做无界增长）。
const AI_CAPTURE_CAP: usize = 4 * 1024 * 1024;

impl Terminal {
    /// 从输入字节里剥掉待吞的注入命令回显。武装后目标回显之前可能先到达其它真实内容（如
    /// AI 命令场景里先发的真实命令自己的回显）——这些字节原样透传、不影响继续等待目标出现。
    /// 真实内容里偶然出现和目标回显开头相同的字节时，会先暂存（`echo_pending`）当作「可能是
    /// 目标回显」而不立即输出；一旦后续字节证明只是巧合（失配），把暂存字节原样还给真实输出，
    /// 并从当前字节重新判断是否是新一轮匹配的开头——不清空 `echo_expect`（否则真正的目标回显
    /// 到达时也不会被吞了），只重置匹配进度，继续等目标出现。
    fn strip_echo(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.echo_pos < self.echo_expect.len() {
                if b == self.echo_expect[self.echo_pos] {
                    self.echo_pos += 1;
                    self.echo_pending.push(b);
                    if self.echo_pos >= self.echo_expect.len() {
                        self.echo_tail = true; // 命令体已吞完，接着吞回车换行
                        self.echo_pending.clear(); // 确认命中，暂存字节真正吞掉
                    }
                    continue;
                }
                if (b == b'\r' || b == b'\n') && self.echo_pos > 0 {
                    continue; // 部分匹配中，终端自动换行/回显格式，忽略
                }
                // 失配：先把暂存的疑似字节还给真实输出（之前只是巧合的部分匹配），
                // 再看这个字节本身是不是新一轮匹配的开头。
                out.append(&mut self.echo_pending);
                self.echo_pos = 0;
                if b == self.echo_expect[0] {
                    self.echo_pos = 1;
                    self.echo_pending.push(b);
                    if self.echo_pos >= self.echo_expect.len() {
                        // echo_expect 只有一个字节：这一个字节本身就已经是完整匹配，
                        // 需要立即确认命中，否则这个字节会卡在「已匹配完但没置 echo_tail」
                        // 的状态里，既不会被吞掉标记为完成，也不会被当成正常字节输出。
                        self.echo_tail = true;
                        self.echo_pending.clear();
                    }
                } else {
                    out.push(b);
                }
            } else if self.echo_tail {
                if b == b'\r' || b == b'\n' {
                    continue;
                }
                self.echo_tail = false;
                out.push(b);
            } else {
                out.push(b);
            }
        }
        out
    }

    /// 收集终端全部行文本（含回滚缓冲）。会临时改动 scrollback 并复原。
    pub(super) fn collect_lines(&mut self) -> Vec<String> {
        let saved = self.parser.screen().scrollback();
        // 设到最大以探测回滚总长度（内部会 clamp 到实际长度）
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let sb = self.parser.screen().scrollback();
        let rows = self.rows as usize;
        let cols = self.cols;
        let mut lines: Vec<String> = Vec::new();
        let mut off = sb;
        loop {
            self.parser.screen_mut().set_scrollback(off);
            let start_idx = sb - off;
            for (k, line) in self.parser.screen().rows(0, cols).enumerate() {
                let idx = start_idx + k;
                if idx >= lines.len() {
                    lines.push(line);
                }
            }
            if off == 0 {
                break;
            }
            off = off.saturating_sub(rows);
        }
        self.parser.screen_mut().set_scrollback(saved);
        lines
    }

    /// 把整个缓冲（回滚 + 可见屏）连同颜色/属性序列化为带 SGR 的字节流（行间 `\r\n`）。
    /// 供 resize 重排使用：vt100 的 `set_size` 不回流（缩小截断底部、放大底部补空白），
    /// 重建解析器并重放这段字节即可让内容按新宽度回流并贴底（颜色/粗体等属性保留）。
    fn serialize_buffer(&mut self) -> Vec<u8> {
        let saved = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let sb = self.parser.screen().scrollback();
        let rows = self.rows as usize;
        let cols = self.cols;
        // 按「全局行索引」去重收集每行的序列化字节（与 collect_lines 同样的遍历方式）
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut off = sb;
        loop {
            self.parser.screen_mut().set_scrollback(off);
            let start_idx = sb - off;
            {
                let screen = self.parser.screen();
                for r in 0..rows {
                    let idx = start_idx + r;
                    if idx < lines.len() {
                        continue;
                    }
                    lines.push(serialize_row(screen, r as u16, cols));
                }
            }
            if off == 0 {
                break;
            }
            off = off.saturating_sub(rows);
        }
        self.parser.screen_mut().set_scrollback(saved);
        // 去掉末尾空行（多为放大补出的空白/未用行），重放后内容自然贴底
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(line);
        }
        out
    }

    /// 喂入来自远程的原始字节。
    pub fn feed(&mut self, bytes: &[u8]) {
        // 注入命令的回显吞除（仅当有待吞内容时）
        let stripped;
        let bytes: &[u8] = if self.echo_pos < self.echo_expect.len() || self.echo_tail {
            stripped = self.strip_echo(bytes);
            &stripped
        } else {
            bytes
        };
        // AI/MCP 命令补全检测：扫描哨兵前缀，命中后解析退出码。纯只读扫描，不影响
        // 后续渲染/日志——与 strip_echo 状态完全独立，不耦合已在生产使用的回显吞除逻辑。
        // 哨兵行自身的可见性由注入方（见 mcp_bridge）用 `\r\x1b[K` 自擦除处理，这里无需
        // 剔除字节、也就不影响 parser.process() 的正常喂入顺序。
        if let Some(cap) = &mut self.ai_capture {
            cap.buf.extend_from_slice(bytes);
            if let Some(p0) = find_sub(&cap.buf, &cap.prefix) {
                let after = p0 + cap.prefix.len();
                if let Some(p1) = find_sub(&cap.buf[after..], b"\x1e") {
                    let code = std::str::from_utf8(&cap.buf[after..after + p1])
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(-1);
                    let mut text = strip_ansi_to_text(&cap.buf[..p0]);
                    if cap.truncated {
                        text = format!("[输出过长，已截断保留末尾部分]\n{text}");
                    }
                    self.ai_done = Some((code, text));
                    self.ai_capture = None;
                }
            } else if cap.buf.len() > AI_CAPTURE_CAP {
                // 兜底：命令迟迟不结束、输出持续增长时，只保留最近一段，避免无界内存增长
                let keep = AI_CAPTURE_CAP / 2;
                let drop = cap.buf.len() - keep;
                cap.buf.drain(..drop);
                cap.truncated = true;
            }
        }
        // 会话日志：原样落盘（可用 cat 回放）
        if let Some(f) = &mut self.log_file {
            let _ = f.write_all(bytes);
        }
        // OSC 7 上报的工作目录（shell 配置后会发；用于断线重连恢复 cwd）
        if let Some(p) = parse_osc7(bytes) {
            self.osc7_cwd = Some(p);
        }
        // 合并上次暂存的不完整 UTF-8 前缀，并把本次结尾不完整的多字节序列暂存到下次，
        // 避免一个中文字符被拆在两个数据块里导致乱码。
        let mut data = std::mem::take(&mut self.utf8_pending);
        data.extend_from_slice(bytes);
        let hold = incomplete_utf8_tail(&data);
        let split = data.len() - hold;
        self.utf8_pending = data[split..].to_vec();
        data.truncate(split);
        let bytes = &data[..];
        if bytes.is_empty() {
            return;
        }

        // `clear` 会发 ESC[2J（清屏）+ ESC[3J（清回滚缓冲）。vt100 不处理 [3J，
        // 导致旧内容仍留在 scrollback（可上滚看到）。这里在 [3J 处重建解析器，
        // 真正清空回滚缓冲；[3J 之后的字节（新提示符等）喂入全新解析器。
        if find_sub(bytes, b"\x1b[2J").is_some() {
            if let Some(pos) = find_sub(bytes, b"\x1b[3J") {
                let (before, after) = bytes.split_at(pos + 4);
                self.parser.process(before);
                // 与 resize 的普通屏重建同理：清屏该清的是内容和回滚缓冲，不该连带清掉鼠标
                // 上报等私有模式——否则 `clear` 这个日常动作就会悄悄把已开启的鼠标上报关掉。
                let restore = Self::mode_restore_bytes(self.parser.screen());
                self.parser = vt100::Parser::new(self.rows, self.cols, DEFAULT_SCROLLBACK);
                self.scrollback = 0;
                self.parser.process(after);
                self.parser.process(&restore);
                self.ensure_cursor_after_alt();
                return;
            }
        }
        self.parser.process(bytes);
        self.ensure_cursor_after_alt();
    }

    /// 离开备用屏（如退出 vim/less/htop）时，确保光标恢复可见——有些程序异常退出（被 Ctrl+C/
    /// 断线打断）会漏发「显示光标」(`ESC[?25h`)，导致回到 shell 后光标一直不显示（「光标丢失」）。
    fn ensure_cursor_after_alt(&mut self) {
        let alt = self.parser.screen().alternate_screen();
        if self.prev_alt && !alt && self.parser.screen().hide_cursor() {
            self.parser.process(b"\x1b[?25h");
        }
        self.prev_alt = alt;
    }

    /// 普通屏重建解析器（见 `resize`）时，鼠标上报/应用键盘/应用光标键/括号粘贴等私有模式
    /// 不属于「屏幕内容」，`serialize_buffer` 不会带上它们；远端程序（尤其是不切备用屏、
    /// 靠自己重绘实现内部滚动的 TUI，如 codex/claude code 这类 chat CLI）并不知道 iShell
    /// 内部悄悄重建了一次解析器，也就不会重新发送这些模式的开启序列——不主动补回的话，一次
    /// 窗口缩放就会把已经开启的鼠标上报静默清空，此后滚轮/按键行为全都对不上远端预期（曾出现
    /// 「codex 里滚动一次、缩放窗口后再也滚不动，且看到的是进入 codex 之前的历史」）。
    /// 这里把旧解析器当前生效的模式换算成等价的开启转义序列，喂给新解析器，效果等同于远端
    /// 重新发了一遍——不改变新解析器"从头历史重放"这个既有设计，只是把旁路状态一并接上。
    fn mode_restore_bytes(old: &vt100::Screen) -> Vec<u8> {
        let mut out = Vec::new();
        if old.application_keypad() {
            out.extend_from_slice(b"\x1b=");
        }
        if old.application_cursor() {
            out.extend_from_slice(b"\x1b[?1h");
        }
        if old.bracketed_paste() {
            out.extend_from_slice(b"\x1b[?2004h");
        }
        if old.hide_cursor() {
            out.extend_from_slice(b"\x1b[?25l");
        }
        match old.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => {}
            vt100::MouseProtocolMode::Press => out.extend_from_slice(b"\x1b[?9h"),
            vt100::MouseProtocolMode::PressRelease => out.extend_from_slice(b"\x1b[?1000h"),
            vt100::MouseProtocolMode::ButtonMotion => out.extend_from_slice(b"\x1b[?1002h"),
            vt100::MouseProtocolMode::AnyMotion => out.extend_from_slice(b"\x1b[?1003h"),
        }
        match old.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => {}
            vt100::MouseProtocolEncoding::Utf8 => out.extend_from_slice(b"\x1b[?1005h"),
            vt100::MouseProtocolEncoding::Sgr => out.extend_from_slice(b"\x1b[?1006h"),
        }
        out
    }

    /// 调整逻辑尺寸（字符行列）。返回是否真的变化。
    ///
    /// 普通屏用「重排」：vt100 的 `set_size` 不回流——缩小直接截断底部行（最新内容/提示符丢失，
    /// 且不进回滚），放大则在底部补空白（内容贴顶、底部留白），导致「缩小再放大底部不恢复」。
    /// 这里序列化整缓冲 → 按新尺寸重建解析器 → 重放，使内容按新宽度回流并贴底（保留颜色/属性）。
    /// 全屏程序（alt-screen）无回滚、会被远端 SIGWINCH 全量重绘，直接 `set_size` 即可。
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return false;
        }
        if self.parser.screen().alternate_screen() || rows > self.rows {
            // 备用屏由应用收到 SIGWINCH 后自行重绘。普通屏放高时也必须直接扩容：如果把
            // scrollback 序列化后按更高的视口重放，历史行会被吸回可见区；Codex 等 TUI
            // 随后的清屏重绘会覆盖这些行，表现为 max scrollback 在缩小再放大后归零。
            // 直接 set_size 会保留既有 scrollback，只在底部增加空白，远端重绘后即可填满。
            self.cols = cols;
            self.rows = rows;
            self.parser.screen_mut().set_size(rows, cols);
        } else {
            let prev_sb = self.scrollback;
            let data = self.serialize_buffer();
            let restore = Self::mode_restore_bytes(self.parser.screen());
            let mut np = vt100::Parser::new(rows, cols, DEFAULT_SCROLLBACK);
            np.process(&data);
            np.process(&restore);
            self.parser = np;
            self.cols = cols;
            self.rows = rows;
            // 保留回看位置（按新缓冲的最大可回看行数钳制），避免 resize 一律跳回底部
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let max_sb = self.parser.screen().scrollback();
            let nb = prev_sb.min(max_sb);
            self.parser.screen_mut().set_scrollback(nb);
            self.scrollback = nb;
            // 选区/搜索高亮跨回流坐标会错位 → 清掉（回看位置仍保留）
            self.sel_anchor = None;
            self.sel_cursor = None;
            self.search_hl = None;
        }
        true
    }
}
