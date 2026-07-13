//! Remote byte feed, scrollback collection, and resize reflow helpers.

use std::io::Write;

use super::{
    osc::parse_osc7,
    vt::{find_sub, incomplete_utf8_tail, serialize_row},
    Terminal, DEFAULT_SCROLLBACK,
};

impl Terminal {
    /// 从输入字节里剥掉待吞的注入命令回显；遇到非预期可见字节即放弃（保证不误吞真实输出）。
    fn strip_echo(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut aborted = false;
        for &b in input {
            if aborted {
                out.push(b);
                continue;
            }
            if self.echo_pos < self.echo_expect.len() {
                if b == self.echo_expect[self.echo_pos] {
                    self.echo_pos += 1;
                    if self.echo_pos >= self.echo_expect.len() {
                        self.echo_tail = true; // 命令体已吞完，接着吞回车换行
                    }
                    continue;
                }
                if b == b'\r' || b == b'\n' {
                    continue; // 终端自动换行/回显格式，忽略
                }
                // 出现非预期可见字节：放弃吞回显，原样输出剩余（避免误吞真实内容）
                self.echo_expect.clear();
                self.echo_pos = 0;
                self.echo_tail = false;
                aborted = true;
                out.push(b);
            } else if self.echo_tail {
                if b == b'\r' || b == b'\n' {
                    continue;
                }
                self.echo_tail = false;
                aborted = true;
                out.push(b);
            } else {
                aborted = true;
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
                self.parser = vt100::Parser::new(self.rows, self.cols, DEFAULT_SCROLLBACK);
                self.scrollback = 0;
                self.parser.process(after);
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
        if self.parser.screen().alternate_screen() {
            self.cols = cols;
            self.rows = rows;
            self.parser.screen_mut().set_size(rows, cols);
        } else {
            let prev_sb = self.scrollback;
            let data = self.serialize_buffer();
            let mut np = vt100::Parser::new(rows, cols, DEFAULT_SCROLLBACK);
            np.process(&data);
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
