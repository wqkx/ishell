//! 终端模拟：用 `vt100` 维护屏幕模型，并在 egui 中以等宽字体逐格渲染；
//! 同时把键盘事件编码为终端字节流。

use egui::{Color32, FontId, Key, Rect, Sense, Stroke, Vec2};

mod input;
mod keys;
mod osc;
mod paint;
mod search;
mod selection;
mod theme;
mod vt;

use input::HistState;
use keys::encode_mouse;
use osc::{open_url, parse_osc7};
use paint::{cell_format, find_row_urls, highlight_colors, paint_row_backgrounds};
use search::{Find, FindAction};
pub use theme::current_bg;
use theme::{term_theme, TermColors, TERM_THEMES};
use vt::{find_sub, incomplete_utf8_tail, serialize_row};

/// 默认字号（pt）。
const FONT_SIZE: f32 = 14.0;

/// 回看缓冲行数（固定默认，不做配置以免右键菜单过重；够覆盖常见回看需求）。
const DEFAULT_SCROLLBACK: usize = 5000;

pub struct Terminal {
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    scrollback: usize,
    /// 可调字号（Ctrl+滚轮）
    font_size: f32,
    /// 选区两端（屏幕字符坐标 row,col）；None 表示无选区
    sel_anchor: Option<(u16, u16)>,
    sel_cursor: Option<(u16, u16)>,
    /// 系统剪贴板（懒初始化，用于右键粘贴）
    clipboard: Option<arboard::Clipboard>,
    /// 终端配色索引（见 TERM_THEMES）；全局共享，切一个即全部同步
    theme: u8,
    /// 当前输入行的影子缓冲（用于前缀历史搜索）
    input_line: String,
    /// 本会话命令历史
    history: Vec<String>,
    /// 历史前缀搜索状态
    hist: Option<HistState>,
    /// 终端内容搜索
    find: Option<Find>,
    /// 当前命中所在的屏幕行（高亮用）
    search_hl: Option<u16>,
    /// 鼠标上报模式下当前按住的按钮基码（0=左 1=中 2=右），用于编码拖动事件
    held_btn: Option<u8>,
    /// 跨数据块暂存的不完整 UTF-8 尾字节（避免多字节中文被拆分后乱码）
    utf8_pending: Vec<u8>,
    /// 外部（如「在终端 cd 到此处」）请求下一帧聚焦终端
    focus_req: bool,
    /// 会话日志：开启后把远端原始字节追加写入该文件（typescript 式）
    log_file: Option<std::fs::File>,
    /// 关键字高亮（ERROR/WARN 等）开关
    highlight: bool,
    /// 由 OSC 7 解析到的当前工作目录（用于断线重连后恢复）
    osc7_cwd: Option<String>,
    /// 右键菜单「在文件列表中显示当前目录」请求：App 取走后导航文件区
    reveal_cwd: Option<String>,
    /// 无 cwd 时点该菜单 → 请求 App 弹确认框注入 OSC 7
    inject_request: bool,
    /// 待吞掉的「注入命令」回显（注入是我们替用户键入的，shell 必然回显，这里把它从输出里抹掉）
    echo_expect: Vec<u8>,
    echo_pos: usize,
    /// 回显匹配完成后，再吞掉紧随的回车换行（命令执行的 Enter 回显）
    echo_tail: bool,
    /// IME 预编辑串（拼音组字中的未提交文本），显示在光标处
    ime_preedit: String,
    /// 上一帧焦点状态（仅用于焦点变化时打印诊断日志）
    prev_focused: bool,
    /// 上次是否处于备用屏（vim/less 等）；用于离开备用屏时恢复光标可见，防「光标丢失」
    prev_alt: bool,
    /// 正在拖动右侧滚动条（区别于拖动选择文本）
    sb_dragging: bool,
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, DEFAULT_SCROLLBACK),
            cols: 80,
            rows: 24,
            scrollback: 0,
            font_size: FONT_SIZE,
            sel_anchor: None,
            sel_cursor: None,
            clipboard: None,
            theme: term_theme().load(std::sync::atomic::Ordering::Relaxed), // 全局配色，沿用上次选择

            input_line: String::new(),
            history: Vec::new(),
            hist: None,
            find: None,
            search_hl: None,
            held_btn: None,
            utf8_pending: Vec::new(),
            focus_req: false,
            log_file: None,
            highlight: true,
            osc7_cwd: None,
            reveal_cwd: None,
            inject_request: false,
            echo_expect: Vec::new(),
            echo_pos: 0,
            echo_tail: false,
            ime_preedit: String::new(),
            prev_focused: false,
            prev_alt: false,
            sb_dragging: false,
        }
    }

    /// 由 OSC 7 解析到的当前工作目录（若 shell 上报）。
    pub fn cwd(&self) -> Option<&str> {
        self.osc7_cwd.as_deref()
    }
    /// 取走「在文件列表中显示当前目录」请求（右键菜单触发）。
    pub fn take_reveal_cwd(&mut self) -> Option<String> {
        self.reveal_cwd.take()
    }
    /// 取走「无 cwd 时请求注入」标志。
    pub fn take_inject_request(&mut self) -> bool {
        std::mem::take(&mut self.inject_request)
    }
    /// 登记一段「我们替用户键入」的命令文本，其 shell 回显将从输出中被吞掉（不显示在终端）。
    /// 须在发送命令后、回显到达前调用（即点击注入的同一帧）。
    pub fn expect_echo(&mut self, s: &str) {
        self.echo_expect = s.as_bytes().to_vec();
        self.echo_pos = 0;
        self.echo_tail = false;
    }
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
    fn collect_lines(&mut self) -> Vec<String> {
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

    /// 请求下一帧让终端区域获得键盘焦点。
    pub fn request_focus(&mut self) {
        self.focus_req = true;
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
            use std::io::Write;
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

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// 渲染终端内容。返回本帧用户键盘输入产生的字节流（交给 worker 发送）。
    ///
    /// `focused` 表示终端区域是否持有焦点，决定是否采集键盘事件。
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<u8> {
        // 从全局配色同步：任一终端切换后，所有终端下一帧统一生效
        self.theme = term_theme().load(std::sync::atomic::Ordering::Relaxed);
        // Ctrl+Shift+F 切换终端内容搜索
        if ui.input(|i| {
            (i.modifiers.ctrl || i.modifiers.command) && i.modifiers.shift && i.key_pressed(Key::F)
        }) {
            if self.find.is_some() {
                self.find = None;
                self.search_hl = None;
            } else {
                self.find = Some(Find {
                    focus: true,
                    ..Default::default()
                });
            }
        }
        if self.find.is_some() {
            match self.draw_find_bar(ui) {
                FindAction::Search => self.run_search(),
                FindAction::Step(d) => self.search_step(d),
                FindAction::Close => {
                    self.find = None;
                    self.search_hl = None;
                    self.scrollback = 0;
                    self.parser.screen_mut().set_scrollback(0);
                }
                FindAction::None => {}
            }
        }

        let font = FontId::monospace(self.font_size);
        // 以字符 'M' 的宽高度量单元格尺寸
        let (char_w_raw, glyph_h) = ui.ctx().fonts_mut(|f| {
            let w = f.glyph_width(&font, 'M');
            let h = f.row_height(&font);
            (w, h)
        });
        // 把单元格宽高吸附到「整数物理像素」：否则逐格 col*char_w 累积非整数像素位置，
        // 等宽/中文字形采样落在像素缝里 → 发虚（mac Retina 尤其明显）。
        let ppp = ui.ctx().pixels_per_point();
        let snap = |v: f32| ((v * ppp).round().max(1.0)) / ppp;
        let char_w = snap(char_w_raw);
        // 行高 = 字形高度 × 1.2，避免上下两行过挤；字形在行内纵向居中
        let char_h = snap(glyph_h * 1.2);
        let cell = Vec2::new(char_w, char_h);

        let avail = ui.available_size();
        // 申请整块区域并捕获键盘/鼠标焦点
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        if resp.clicked() {
            resp.request_focus();
        }
        if self.focus_req {
            resp.request_focus();
            self.focus_req = false;
        }
        let focused = resp.has_focus();
        // 诊断：焦点变化时打印一次（IME 启用依赖终端持有焦点）
        if focused != self.prev_focused {
            log::debug!("terminal focus = {focused}");
            self.prev_focused = focused;
        }

        // 关键：终端聚焦时锁定 Tab / 方向键 / Esc，使其传给 shell（修复 Tab 补全），
        // 而不是被 egui 用于控件间焦点切换。
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }

        // 根据可用区域换算行列，必要时上报 resize（由调用方读 size 比较）
        let new_cols = (avail.x / char_w).floor().max(2.0) as u16;
        let new_rows = (avail.y / char_h).floor().max(1.0) as u16;
        self.resize(new_cols, new_rows);

        // 单元格定位（屏幕字符坐标）。捕获 cols/rows 副本以免与后续 &mut self 冲突。
        let (cols, rows) = (self.cols, self.rows);
        let cell_at = |pos: egui::Pos2| -> (u16, u16) {
            let c =
                (((pos.x - rect.left()) / char_w).floor() as i32).clamp(0, cols as i32 - 1) as u16;
            let r =
                (((pos.y - rect.top()) / char_h).floor() as i32).clamp(0, rows as i32 - 1) as u16;
            (r, c)
        };

        // 远端是否开启了鼠标上报（vim/htop/tmux 等）。按住 Shift 时临时强制本地选择（xterm 习惯）。
        let mmode = self.parser.screen().mouse_protocol_mode();
        let menc = self.parser.screen().mouse_protocol_encoding();
        let shift = ui.input(|i| i.modifiers.shift);
        let report_mouse = mmode != vt100::MouseProtocolMode::None && !shift;
        let mut mouse_out: Vec<u8> = Vec::new();

        // 滚轮：Ctrl 调字号；鼠标上报时发滚轮键（64/65）；否则本地回滚
        if resp.hovered() {
            let (scroll, ctrl) = ui.input(|i| {
                (
                    i.smooth_scroll_delta.y,
                    i.modifiers.ctrl || i.modifiers.command,
                )
            });
            if scroll != 0.0 {
                if ctrl {
                    self.font_size = (self.font_size + scroll.signum() * 1.0).clamp(8.0, 32.0);
                } else if report_mouse {
                    if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                        let (r, c) = cell_at(p);
                        let cb = if scroll > 0.0 { 64 } else { 65 };
                        let steps = ((scroll.abs() / char_h).round() as i32).clamp(1, 5);
                        for _ in 0..steps {
                            encode_mouse(menc, cb, c, r, true, &mut mouse_out);
                        }
                    }
                } else {
                    let lines = (scroll / char_h).round() as i64;
                    let nb = (self.scrollback as i64 + lines).clamp(0, DEFAULT_SCROLLBACK as i64)
                        as usize;
                    self.parser.screen_mut().set_scrollback(nb);
                    // 回读 vt100 按「实际历史行数」钳制后的真实值：否则 self.scrollback 可能远超真实历史，
                    // 之后要空滚很多步才重新移动视口（「死滚动」）。
                    self.scrollback = self.parser.screen().scrollback();
                    self.recompute_search_hl(); // 手动滚动：高亮跟随命中行（滚出视口才消失）
                }
            }
        }

        // 右侧滚动条几何：探测可回滚总行数（set MAX 读回再还原，仅改偏移不重排，开销很小）
        let max_sb = {
            let cur = self.scrollback;
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let m = self.parser.screen().scrollback();
            self.parser.screen_mut().set_scrollback(cur);
            m
        };
        let sb_w = 8.0;
        let sb_track = Rect::from_min_max(egui::pos2(rect.right() - sb_w, rect.top()), rect.max);

        if report_mouse {
            // 转发鼠标按键/移动给远端。注意：这些事件取自全局输入队列（未经 egui 分层命中），
            // 故须自行判定终端是否为该点最上层——否则弹窗（如「新建连接」）盖在终端上时，
            // 在弹窗内点击/双击会被透传到背后的鼠标上报程序（vim/tmux/htop）。
            let term_layer = resp.layer_id;
            let ctx = ui.ctx().clone();
            let on_top =
                |pos: egui::Pos2| rect.contains(pos) && ctx.layer_id_at(pos) == Some(term_layer);
            let events = ui.input(|i| i.events.clone());
            for ev in &events {
                match ev {
                    egui::Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    } if on_top(*pos) => {
                        let (r, c) = cell_at(*pos);
                        let base = match button {
                            egui::PointerButton::Primary => 0u8,
                            egui::PointerButton::Middle => 1,
                            egui::PointerButton::Secondary => 2,
                            _ => 0,
                        };
                        let mut cb = base;
                        if modifiers.alt {
                            cb += 8;
                        }
                        if modifiers.ctrl || modifiers.command {
                            cb += 16;
                        }
                        if *pressed {
                            self.held_btn = Some(base);
                            encode_mouse(menc, cb, c, r, true, &mut mouse_out);
                        } else {
                            self.held_btn = None;
                            // X10(Press) 模式不上报释放；SGR 用原按钮码，传统编码用 3
                            if mmode != vt100::MouseProtocolMode::Press {
                                let rel = if menc == vt100::MouseProtocolEncoding::Sgr {
                                    cb
                                } else {
                                    3
                                };
                                encode_mouse(menc, rel, c, r, false, &mut mouse_out);
                            }
                        }
                    }
                    egui::Event::PointerMoved(pos) if on_top(*pos) => {
                        let motion = mmode == vt100::MouseProtocolMode::AnyMotion
                            || (mmode == vt100::MouseProtocolMode::ButtonMotion
                                && self.held_btn.is_some());
                        if motion {
                            let (r, c) = cell_at(*pos);
                            let cb = 32 + self.held_btn.unwrap_or(3); // 32=移动标志位
                            encode_mouse(menc, cb, c, r, true, &mut mouse_out);
                        }
                    }
                    _ => {}
                }
            }
        } else {
            // 拖动起点落在右侧滚动条上 → 拖滚动条；否则本地拖拽选择文本
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    if max_sb > 0 && p.x >= sb_track.left() {
                        self.sb_dragging = true;
                    } else {
                        let c = cell_at(p);
                        self.sel_anchor = Some(c);
                        self.sel_cursor = Some(c);
                    }
                }
            } else if resp.dragged() && !self.sb_dragging {
                if let Some(p) = resp.interact_pointer_pos() {
                    self.sel_cursor = Some(cell_at(p));
                }
            }
            if self.sb_dragging {
                if let Some(p) = resp.interact_pointer_pos() {
                    let f = ((p.y - sb_track.top()) / sb_track.height().max(1.0)).clamp(0.0, 1.0);
                    let nb = (((1.0 - f) * max_sb as f32).round() as usize).min(max_sb);
                    self.scrollback = nb;
                    self.parser.screen_mut().set_scrollback(nb);
                    self.recompute_search_hl(); // 拖滚动条：高亮跟随命中行
                }
            }
            if resp.drag_stopped() {
                self.sb_dragging = false;
            }
            // 三击选整行 / 双击选词 / 单击清选区（本地选择模式）
            if resp.triple_clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (r, _) = cell_at(p);
                    self.sel_anchor = Some((r, 0));
                    self.sel_cursor = Some((r, self.cols.saturating_sub(1)));
                }
            } else if resp.double_clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (r, c) = cell_at(p);
                    if let Some((c0, c1)) = self.word_range_at(r, c) {
                        self.sel_anchor = Some((r, c0));
                        self.sel_cursor = Some((r, c1));
                    }
                }
            } else if resp.clicked() && !self.sb_dragging {
                self.clear_selection();
            }
        }

        let tc = TermColors::by_index(self.theme);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, tc.bg);

        let sel = self.ordered_selection();
        let screen = self.parser.screen();
        // origin 也吸附到物理像素网格，使每个单元格都落在整数像素上（配合上面 char_w/char_h 吸附）
        let origin = egui::pos2(
            (rect.min.x * ppp).round() / ppp,
            (rect.min.y * ppp).round() / ppp,
        );

        // 可见行中的链接：用于悬停下划线 + 点击打开（鼠标上报模式下让位给 TUI）
        let mut link_rects: Vec<(Rect, String)> = Vec::new();
        if !report_mouse {
            for row in 0..self.rows {
                for (sc, ec, url) in find_row_urls(screen, row, self.cols) {
                    let x0 = origin.x + sc as f32 * char_w;
                    let x1 = origin.x + (ec as f32 + 1.0) * char_w;
                    let y = origin.y + row as f32 * char_h;
                    link_rects.push((
                        Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + char_h)),
                        url,
                    ));
                }
            }
        }
        let hover_pos = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| rect.contains(*p));
        let hover_link = hover_pos.and_then(|p| {
            link_rects
                .iter()
                .find(|(r, _)| r.contains(p))
                .map(|(_, u)| u.clone())
        });

        for row in 0..self.rows {
            let y = origin.y + row as f32 * char_h;
            // 先绘制该行的背景块（处理非默认底色）
            paint_row_backgrounds(&painter, screen, row, self.cols, origin, cell, &tc);
            // 搜索命中行高亮（整行淡黄底，取主题 WARN 色保持一致）
            if self.search_hl == Some(row) {
                let w = crate::theme::Palette::WARN;
                painter.rect_filled(
                    Rect::from_min_max(
                        egui::pos2(origin.x, y),
                        egui::pos2(rect.right(), y + char_h),
                    ),
                    0.0,
                    Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), 90),
                );
            }
            // 选区高亮（半透明，文字仍可见）
            if let Some(((sr, sc), (er, ec))) = sel {
                if row >= sr && row <= er {
                    let c0 = if row == sr { sc } else { 0 };
                    let c1 = if row == er {
                        ec
                    } else {
                        self.cols.saturating_sub(1)
                    };
                    let x0 = origin.x + c0 as f32 * char_w;
                    let x1 = origin.x + (c1 as f32 + 1.0) * char_w;
                    let a = crate::theme::Palette::ACCENT;
                    painter.rect_filled(
                        Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + char_h)),
                        0.0,
                        Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 80),
                    );
                }
            }
            // 逐格绘制字形：固定网格定位，避免 CJK / 宽字符的字形步进破坏对齐。
            // 空内容（含宽字符的续格）跳过；宽字符自身在本格绘制，自然跨两格。
            // 关键字高亮：覆盖匹配单元格的文字颜色
            let hl = if self.highlight {
                highlight_colors(screen, row, self.cols)
            } else {
                Vec::new()
            };
            for col in 0..self.cols {
                let Some(c) = screen.cell(row, col) else {
                    continue;
                };
                let s = c.contents();
                if s.is_empty() {
                    continue;
                }
                let fmt = cell_format(c, &font, &tc);
                let color = hl.get(col as usize).copied().flatten().unwrap_or(fmt.color);
                let x = origin.x + col as f32 * char_w;
                if c.is_wide() {
                    // 全角字符（中文等）占 2 格：在两格内水平+纵向居中。
                    // 用反向放大的字号抵消 CJK 后备字体的全局缩小（CJK_SCALE），
                    // 让全角字以原始大小填满更多两格空间，减小字间距；行高 1.2× 留白足以容纳。
                    let wfont = FontId::monospace(self.font_size / crate::theme::CJK_SCALE);
                    painter.text(
                        egui::pos2(x + char_w, y + char_h / 2.0),
                        egui::Align2::CENTER_CENTER,
                        s,
                        wfont,
                        color,
                    );
                } else {
                    // 半角字符：纵向居中，使 1.2× 行高的额外留白上下均分
                    painter.text(
                        egui::pos2(x, y + char_h / 2.0),
                        egui::Align2::LEFT_CENTER,
                        s,
                        font.clone(),
                        color,
                    );
                }
                if fmt.underline.width > 0.0 {
                    // 下划线落在居中字形的底部附近
                    let uy = y + (char_h + glyph_h) / 2.0 - 1.0;
                    let w = if c.is_wide() { 2.0 * char_w } else { char_w };
                    painter.hline(x..=(x + w), uy, fmt.underline);
                }
            }
        }

        // 悬停的链接：手型光标 + 下划线；点击打开
        if let Some(p) = hover_pos {
            if let Some((r, _)) = link_rects.iter().find(|(r, _)| r.contains(p)) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                let uy = (origin.y + ((r.top() - origin.y) / char_h).round() * char_h)
                    + (char_h + glyph_h) / 2.0
                    - 1.0;
                painter.hline(
                    r.left()..=r.right(),
                    uy,
                    Stroke::new(1.0, crate::theme::Palette::ACCENT),
                );
            }
        }
        if resp.clicked() {
            if let Some(url) = &hover_link {
                open_url(url);
            }
        }

        // 光标
        if !screen.hide_cursor() && self.scrollback == 0 {
            let (cr, cc) = screen.cursor_position();
            let cpos = origin + Vec2::new(cc as f32 * char_w, cr as f32 * char_h);
            let crect = Rect::from_min_size(cpos, cell);
            // 失焦时仍用珊瑚色描边（而非低对比灰），避免点到文件栏/侧栏后光标看似「消失」
            if focused {
                painter.rect_filled(
                    crect,
                    1.0,
                    crate::theme::Palette::ACCENT.gamma_multiply(0.6),
                );
            } else {
                painter.rect_stroke(
                    crect,
                    1.0,
                    Stroke::new(1.2, crate::theme::Palette::ACCENT.gamma_multiply(0.8)),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // 启用 IME（中文 / fcitx 等输入法）：聚焦时上报输入区，并把候选框定位到光标处。
        // 否则平台不会在终端上激活输入法，导致无法输入中文。
        if focused {
            let (cr, cc) = screen.cursor_position();
            let ipos = origin + Vec2::new(cc as f32 * char_w, cr as f32 * char_h);
            let irect = Rect::from_min_size(ipos, cell);
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect: irect,
                    cursor_rect: irect,
                });
            });
            // 在光标处显示 IME 预编辑（组字中的拼音/候选），铺底 + 下划线以便辨识
            if !self.ime_preedit.is_empty() {
                let font = egui::FontId::monospace(self.font_size / crate::theme::CJK_SCALE);
                let galley = painter.layout_no_wrap(
                    self.ime_preedit.clone(),
                    font,
                    crate::theme::Palette::ACCENT,
                );
                let bg = Rect::from_min_size(ipos, galley.size());
                painter.rect_filled(bg, 0.0, crate::theme::Palette::PANEL);
                painter.galley(ipos, galley, crate::theme::Palette::ACCENT);
                painter.hline(
                    bg.x_range(),
                    bg.max.y - 1.0,
                    Stroke::new(1.0, crate::theme::Palette::ACCENT),
                );
            }
        }

        // 右侧滚动条（仅有可回滚历史时显示）：滑块高=视口/总量，位置由 scrollback 决定（0=底/最新）。
        if max_sb > 0 {
            let total = rows as f32 + max_sb as f32;
            let handle_h =
                (sb_track.height() * (rows as f32 / total)).clamp(24.0, sb_track.height());
            let pos_frac = 1.0 - (self.scrollback as f32 / max_sb as f32);
            let handle_top = sb_track.top() + (sb_track.height() - handle_h) * pos_frac;
            let handle = Rect::from_min_size(
                egui::pos2(sb_track.left() + 1.0, handle_top),
                Vec2::new(sb_w - 2.0, handle_h),
            );
            let hovered = hover_pos.is_some_and(|p| sb_track.contains(p));
            // 暖灰滑块，与全局暖色调一致
            let col = if self.sb_dragging {
                egui::Color32::from_rgb(114, 109, 97)
            } else if hovered {
                egui::Color32::from_rgb(144, 138, 124)
            } else {
                egui::Color32::from_rgb(179, 173, 159)
            };
            painter.rect_filled(handle, 3.0, col);
        }

        // 键盘输入
        let mut out = if focused {
            self.collect_input(ui)
        } else {
            Vec::new()
        };

        // 键盘复制/粘贴由 collect_input 内的 Copy/Cut/Paste 事件处理（egui 会把
        // Ctrl+C/X/V 转成这些事件而不再下发按键）。这里只处理右键菜单。
        let mut do_copy = false;
        let mut do_paste = false;
        let mut do_find = false;
        let mut start_log = false;
        resp.context_menu(|ui| {
            ui.set_min_width(170.0); // 菜单宽度足些，看着舒服
                                     // 菜单项不换行（否则英文较长的「Highlight ERROR/WARN」会折行，复选框被挤到两行正中）
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let sel = self.has_selection();
            if ui
                .add_enabled(sel, egui::Button::new(crate::i18n::tr("复制", "Copy")))
                .clicked()
            {
                do_copy = true;
                ui.close();
            }
            if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                do_paste = true;
                ui.close();
            }
            ui.separator();
            // 查找终端内容（等价快捷键 Ctrl+Shift+F；放菜单里更易发现、且不受桌面快捷键占用影响）
            if ui
                .button(format!(
                    "{}  {}",
                    egui_phosphor::regular::MAGNIFYING_GLASS,
                    crate::i18n::tr("查找…  (Ctrl+Shift+F)", "Find…  (Ctrl+Shift+F)")
                ))
                .clicked()
            {
                do_find = true;
                ui.close();
            }
            ui.separator();
            // 在文件列表中显示终端当前目录：已知 cwd 直接跳；未知则请求 App 弹确认框注入 OSC 7。
            if ui
                .button(crate::i18n::tr(
                    "在文件列表中显示当前目录",
                    "Show current dir in files",
                ))
                .clicked()
            {
                match self.osc7_cwd.clone() {
                    Some(c) => self.reveal_cwd = Some(c),
                    None => self.inject_request = true,
                }
                ui.close();
            }
            ui.separator();
            // 终端配色：多套主题（深/浅/近白/柔和深/经典浅），选中即全局同步并存盘
            ui.menu_button(crate::i18n::tr("终端配色", "Terminal theme"), |ui| {
                ui.set_min_width(120.0);
                for (i, (zh, en)) in TERM_THEMES.iter().enumerate() {
                    let i = i as u8;
                    if ui
                        .selectable_label(self.theme == i, crate::i18n::tr(zh, en))
                        .clicked()
                    {
                        term_theme().store(i, std::sync::atomic::Ordering::Relaxed);
                        crate::store::save_term_theme(i);
                        self.theme = i;
                        ui.close();
                    }
                }
            });
            // 高亮 ERROR/WARN：改成与「终端配色」一致的二级菜单（是 / 否）
            ui.menu_button(
                crate::i18n::tr("高亮 ERROR/WARN", "Highlight ERROR/WARN"),
                |ui| {
                    ui.set_min_width(90.0);
                    if ui
                        .selectable_label(self.highlight, crate::i18n::tr("是", "Yes"))
                        .clicked()
                    {
                        self.highlight = true;
                        ui.close();
                    }
                    if ui
                        .selectable_label(!self.highlight, crate::i18n::tr("否", "No"))
                        .clicked()
                    {
                        self.highlight = false;
                        ui.close();
                    }
                },
            );
            // 「强制 X11」已移至左侧监控栏的右键菜单，避免 shell 右键项过多
            ui.separator();
            // 会话日志录制
            if self.log_file.is_some() {
                if ui
                    .button(crate::i18n::tr("停止录制日志", "Stop recording"))
                    .clicked()
                {
                    self.log_file = None;
                    ui.close();
                }
            } else if ui
                .button(crate::i18n::tr("录制会话日志…", "Record session log…"))
                .clicked()
            {
                start_log = true;
                ui.close();
            }
        });
        if start_log {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("session.log")
                .save_file()
            {
                if let Ok(f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    self.log_file = Some(f);
                }
            }
        }
        if do_copy {
            if let Some(t) = self.selected_text() {
                ui.ctx().copy_text(t);
            }
        }
        if do_paste {
            if let Some(t) = self.read_clipboard() {
                out.extend_from_slice(t.as_bytes());
            }
        }
        // 右键菜单「查找」：无则打开并聚焦输入框，已开则把焦点定位到输入框
        if do_find {
            match &mut self.find {
                Some(f) => f.focus = true,
                None => {
                    self.find = Some(Find {
                        focus: true,
                        ..Default::default()
                    })
                }
            }
        }
        // 复制/粘贴（尤其右键菜单）后焦点会丢失，重新聚焦终端，免得还要再点一下
        if do_copy || do_paste {
            resp.request_focus();
        }
        // 有键盘输入/粘贴时回到底部：用户上滚看历史后，一旦打字就应跳回最新（与常见终端一致）。
        // 仅对键盘/粘贴产生的字节生效；鼠标上报(mouse_out)不触发。
        if !out.is_empty() && self.scrollback != 0 {
            self.scrollback = 0;
            self.parser.screen_mut().set_scrollback(0);
        }
        // 鼠标上报字节（若有）
        out.extend_from_slice(&mouse_out);
        out
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::paint::{brighten_rgb, vt_color, xterm256};
    use super::*;

    #[test]
    fn osc7_parsing() {
        let data = b"\x1b]7;file://host/home/user/%E4%B8%AD%E6%96%87\x07";
        assert_eq!(
            parse_osc7(data).as_deref(),
            Some("/home/user/\u{4e2d}\u{6587}")
        );
        assert_eq!(parse_osc7(b"no osc here"), None);
    }

    #[test]
    fn highlight_keywords() {
        let mut p = vt100::Parser::new(2, 80, 0);
        p.process(b"INFO ok then ERROR boom and WARN x");
        let hl = highlight_colors(p.screen(), 0, 80);
        let txt = "INFO ok then ERROR boom and WARN x";
        assert!(hl[txt.find("ERROR").unwrap()].is_some());
        assert!(hl[txt.find("WARN").unwrap()].is_some());
        assert!(hl[0].is_none()); // INFO 不在规则内
    }

    #[test]
    fn detect_urls_in_row() {
        let mut p = vt100::Parser::new(2, 80, 0);
        p.process(b"see https://example.com/a/b, or http://x.y/z! end");
        let got: Vec<String> = find_row_urls(p.screen(), 0, 80)
            .into_iter()
            .map(|(_, _, u)| u)
            .collect();
        assert_eq!(
            got,
            vec![
                "https://example.com/a/b".to_string(),
                "http://x.y/z".to_string()
            ]
        );
    }

    #[test]
    fn no_url_no_match() {
        let mut p = vt100::Parser::new(2, 80, 0);
        p.process(b"plain text httpsomething not a url");
        assert!(find_row_urls(p.screen(), 0, 80).is_empty());
    }

    #[test]
    fn detect_more_schemes() {
        let mut p = vt100::Parser::new(2, 120, 0);
        p.process(b"ftp://h/f sftp://h/x ssh://u@h file:///etc/hosts www.rust-lang.org");
        let got: Vec<String> = find_row_urls(p.screen(), 0, 120)
            .into_iter()
            .map(|(_, _, u)| u)
            .collect();
        // 安全收窄：仅 http/https/ftp/ftps 与裸 www.（ssh/sftp/file 会触发本地协议
        // 处理器，终端输出不可信，不再识别为可点击链接）
        assert_eq!(
            got,
            vec![
                "ftp://h/f".to_string(),
                "https://www.rust-lang.org".to_string(), // 裸 www. 自动补 https
            ]
        );
    }

    #[test]
    fn prefix_history_search() {
        let mut t = Terminal::new();
        for cmd in ["cd /tmp", "ls -la", "cd /var/log", "cat x"] {
            t.input_line = cmd.into();
            t.commit_line();
        }
        // 前缀 "cd " 上键 -> 最近的 "cd /var/log"，并带清行前缀 Ctrl+E/Ctrl+U
        t.input_line = "cd ".into();
        let b = t.history_nav(true);
        assert_eq!(&b[..2], &[0x05, 0x15]);
        assert_eq!(&b[2..], b"cd /var/log");
        assert_eq!(t.input_line, "cd /var/log");
        // 再上 -> "cd /tmp"
        assert_eq!(&t.history_nav(true)[2..], b"cd /tmp");
        // 下 -> 回到 "cd /var/log"
        assert_eq!(&t.history_nav(false)[2..], b"cd /var/log");
        // 下越过最新匹配 -> 恢复前缀
        assert_eq!(&t.history_nav(false)[2..], b"cd ");
        // 空行上键 -> 透传方向键
        t.input_line.clear();
        t.hist = None;
        assert_eq!(t.history_nav(true), b"\x1b[A");
    }

    #[test]
    fn terminal_search() {
        let mut t = Terminal::new();
        for i in 0..60 {
            t.feed(format!("line number {i}\r\n").as_bytes());
        }
        t.find = Some(Find {
            query: "number 5".into(),
            ..Default::default()
        });
        t.run_search();
        let f = t.find.as_ref().unwrap();
        // "number 5" 命中 5,50..59 等多行
        assert!(f.hits.len() >= 2, "应找到多处命中，实际 {}", f.hits.len());
        assert!(t.search_hl.is_some(), "应高亮命中行");
        // 不存在的查询无命中
        t.find = Some(Find {
            query: "zzzNOPE".into(),
            ..Default::default()
        });
        t.run_search();
        assert!(t.find.as_ref().unwrap().hits.is_empty());
    }

    #[test]
    fn truecolor_and_attrs_map() {
        let tc = TermColors::light();
        // 24 位真彩色直通
        assert_eq!(
            vt_color(vt100::Color::Rgb(0x12, 0x34, 0x56), tc.fg, &tc),
            Color32::from_rgb(0x12, 0x34, 0x56)
        );
        // 256 色板索引
        assert_eq!(
            vt_color(vt100::Color::Idx(196), tc.fg, &tc),
            xterm256(196, &tc)
        );
        // bold 提亮 / dim 变暗
        let base = Color32::from_rgb(100, 100, 100);
        assert!(brighten_rgb(base, 1.18).r() > base.r());
        assert!(brighten_rgb(base, 0.55).r() < base.r());
        // 解析端：喂入 SGR 38;2 后单元格应为 Rgb
        let mut t = Terminal::new();
        t.feed(b"\x1b[38;2;10;20;30mX\x1b[0m");
        let cell = t.parser.screen().cell(0, 0).expect("cell");
        assert_eq!(cell.contents(), "X");
        assert_eq!(cell.fgcolor(), vt100::Color::Rgb(10, 20, 30));
    }

    #[test]
    fn clear_wipes_scrollback() {
        let mut t = Terminal::new();
        for i in 0..50 {
            t.feed(format!("L{i}\r\n").as_bytes());
        }
        // clear：ESC[H ESC[2J ESC[3J
        t.feed(b"\x1b[H\x1b[2J\x1b[3J");
        t.feed(b"prompt$ ");
        // 即便上滚也看不到旧内容（scrollback 已清空）
        t.parser.screen_mut().set_scrollback(100);
        let s = t.parser.screen();
        let mut all = String::new();
        for r in 0..t.rows {
            for c in 0..t.cols {
                all.push_str(s.cell(r, c).map(|x| x.contents()).unwrap_or(""));
            }
        }
        assert!(!all.contains("L49"), "旧内容应已被清除");
        assert!(all.contains("prompt$"), "新提示符应保留");
    }
}
