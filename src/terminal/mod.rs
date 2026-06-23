//! 终端模拟：用 `vt100` 维护屏幕模型，并在 egui 中以等宽字体逐格渲染；
//! 同时把键盘事件编码为终端字节流。

use egui::{Color32, FontId, Key, Modifiers, Rect, Sense, Stroke, TextFormat, Vec2};

/// 默认字号（pt）。
const FONT_SIZE: f32 = 14.0;

/// 全局终端配色开关（深=true）：所有终端共享同一值，切一个即全部同步。
/// 首次访问时从持久化设置初始化。
fn term_dark_flag() -> &'static std::sync::atomic::AtomicBool {
    static F: std::sync::OnceLock<std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();
    F.get_or_init(|| std::sync::atomic::AtomicBool::new(crate::store::load_term_dark()))
}

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
    /// 终端配色：true=深色（默认），false=浅色（随主题暖色）
    dark: bool,
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
    /// IME 预编辑串（拼音组字中的未提交文本），显示在光标处
    ime_preedit: String,
    /// 上一帧焦点状态（仅用于焦点变化时打印诊断日志）
    prev_focused: bool,
}

/// 终端搜索状态。
#[derive(Default)]
struct Find {
    query: String,
    hits: Vec<usize>, // 命中行的绝对行号（顶部为 0）
    cur: usize,
    focus: bool,
}

enum FindAction {
    None,
    Search,
    Step(i32),
    Close,
}

/// 前缀历史搜索状态：记住起始前缀与当前命中位置。
struct HistState {
    prefix: String,
    idx: usize,
}

/// 终端配色（背景/默认前景/ANSI 16 色），可在深/浅之间切换。
struct TermColors {
    bg: Color32,
    fg: Color32,
    ansi: [(u8, u8, u8); 16],
}

impl TermColors {
    /// 深色（经典控制台，暖调近黑底 + 高对比 ANSI）。
    fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(0x1e, 0x1c, 0x19),
            fg: Color32::from_rgb(0xe6, 0xe1, 0xd6),
            ansi: [
                (0x33, 0x30, 0x2b), (0xe0, 0x6c, 0x60), (0x8c, 0xb8, 0x5f), (0xe0, 0xb0, 0x55),
                (0x6f, 0xa8, 0xdc), (0xc2, 0x8c, 0xd8), (0x5f, 0xbf, 0xc4), (0xd8, 0xd2, 0xc4),
                (0x6f, 0x6b, 0x61), (0xee, 0x82, 0x76), (0xa6, 0xcf, 0x73), (0xf0, 0xc6, 0x6b),
                (0x86, 0xbd, 0xea), (0xd2, 0xa0, 0xe6), (0x76, 0xd2, 0xd6), (0xf2, 0xed, 0xe2),
            ],
        }
    }
    /// 浅色（暖米底，ANSI 已为浅底调校）。
    fn light() -> Self {
        Self {
            bg: crate::theme::Palette::TERM_BG,
            fg: crate::theme::Palette::TERM_FG,
            ansi: [
                (0x3a, 0x38, 0x33), (0xc0, 0x4b, 0x3f), (0x4f, 0x86, 0x4a), (0xb5, 0x82, 0x2e),
                (0x2f, 0x6f, 0xb0), (0xa6, 0x55, 0x9d), (0x2b, 0x8a, 0x8f), (0xb8, 0xb2, 0xa3),
                (0x6f, 0x6b, 0x61), (0xc0, 0x56, 0x4b), (0x5b, 0x8a, 0x56), (0xc2, 0x8e, 0x3c),
                (0x35, 0x78, 0xbb), (0xb0, 0x60, 0xa6), (0x30, 0x95, 0x9a), (0x55, 0x52, 0x4a),
            ],
        }
    }
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 5000),
            cols: 80,
            rows: 24,
            scrollback: 0,
            font_size: FONT_SIZE,
            sel_anchor: None,
            sel_cursor: None,
            clipboard: None,
            dark: term_dark_flag().load(std::sync::atomic::Ordering::Relaxed), // 全局配色，沿用上次选择

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
            ime_preedit: String::new(),
            prev_focused: false,
        }
    }

    /// 由 OSC 7 解析到的当前工作目录（若 shell 上报）。
    pub fn cwd(&self) -> Option<&str> {
        self.osc7_cwd.as_deref()
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

    /// 重新执行搜索（查询变化时）。
    fn run_search(&mut self) {
        let q = match &self.find {
            Some(f) if !f.query.is_empty() => f.query.clone(),
            _ => {
                if let Some(f) = &mut self.find {
                    f.hits.clear();
                }
                self.search_hl = None;
                return;
            }
        };
        let lines = self.collect_lines();
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(&q))
            .map(|(i, _)| i)
            .collect();
        if let Some(f) = &mut self.find {
            f.hits = hits;
            f.cur = 0;
        }
        self.jump_to_current();
    }

    /// 切换到上/下一个命中。
    fn search_step(&mut self, dir: i32) {
        if let Some(f) = &mut self.find {
            let n = f.hits.len();
            if n == 0 {
                return;
            }
            f.cur = ((f.cur as i32 + dir).rem_euclid(n as i32)) as usize;
        }
        self.jump_to_current();
    }

    /// 滚动到当前命中行并记录高亮行。
    fn jump_to_current(&mut self) {
        let line_idx = match &self.find {
            Some(f) if !f.hits.is_empty() => f.hits[f.cur.min(f.hits.len() - 1)],
            _ => {
                self.search_hl = None;
                return;
            }
        };
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let sb = self.parser.screen().scrollback();
        let rows = self.rows as usize;
        let r = rows / 3; // 命中尽量落在上 1/3
        let start_idx = line_idx.saturating_sub(r);
        let off = sb.saturating_sub(start_idx);
        self.parser.screen_mut().set_scrollback(off);
        self.scrollback = off.min(sb);
        // 屏幕行 = 绝对行号 - 窗口起始行号
        let win_start = sb.saturating_sub(self.scrollback);
        self.search_hl = line_idx.checked_sub(win_start).map(|r| r as u16);
    }

    /// 选区按阅读顺序的 (起, 止)（含两端）。
    fn ordered_selection(&self) -> Option<((u16, u16), (u16, u16))> {
        let (a, b) = (self.sel_anchor?, self.sel_cursor?);
        if (a.0, a.1) <= (b.0, b.1) {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    /// 提取选中文本（按行拼接，行尾去除多余空格）。
    fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.ordered_selection()?;
        let screen = self.parser.screen();
        let mut out = String::new();
        for row in sr..=er {
            let c0 = if row == sr { sc } else { 0 };
            let c1 = if row == er { ec } else { self.cols.saturating_sub(1) };
            let mut line = String::new();
            for col in c0..=c1 {
                let Some(cell) = screen.cell(row, col) else {
                    line.push(' ');
                    continue;
                };
                // 宽字符（中文等）的续格不输出，避免复制出来每个汉字后多一个空格
                if cell.is_wide_continuation() {
                    continue;
                }
                let ch = cell.contents();
                line.push_str(if ch.is_empty() { " " } else { &ch });
            }
            out.push_str(line.trim_end());
            if row != er {
                out.push('\n');
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn has_selection(&self) -> bool {
        matches!((self.sel_anchor, self.sel_cursor), (Some(a), Some(b)) if a != b)
    }

    fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_cursor = None;
    }

    /// 读系统剪贴板（懒初始化）。
    fn read_clipboard(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut()?.get_text().ok()
    }

    /// 请求下一帧让终端区域获得键盘焦点。
    pub fn request_focus(&mut self) {
        self.focus_req = true;
    }

    /// 喂入来自远程的原始字节。
    pub fn feed(&mut self, bytes: &[u8]) {
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
                self.parser = vt100::Parser::new(self.rows, self.cols, 5000);
                self.scrollback = 0;
                self.parser.process(after);
                return;
            }
        }
        self.parser.process(bytes);
    }

    /// 调整逻辑尺寸（字符行列）。返回是否真的变化。
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
        true
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// 渲染终端内容。返回本帧用户键盘输入产生的字节流（交给 worker 发送）。
    ///
    /// `focused` 表示终端区域是否持有焦点，决定是否采集键盘事件。
    /// 渲染搜索栏，返回用户动作。
    fn draw_find_bar(&mut self, ui: &mut egui::Ui) -> FindAction {
        use egui_phosphor::regular as icon;
        let mut action = FindAction::None;
        egui::Frame::new()
            .fill(crate::theme::Palette::PANEL_2)
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let f = self.find.as_mut().unwrap();
                    ui.label(egui::RichText::new(icon::MAGNIFYING_GLASS).color(crate::theme::Palette::TEXT_DIM));
                    let resp = ui.add(egui::TextEdit::singleline(&mut f.query).desired_width(180.0).hint_text(crate::i18n::tr("查找终端内容", "Find in terminal")));
                    if f.focus {
                        resp.request_focus();
                        f.focus = false;
                    }
                    if resp.changed() {
                        action = FindAction::Search;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        action = FindAction::Step(1);
                        resp.request_focus();
                    }
                    let cnt = if f.hits.is_empty() {
                        "0/0".to_string()
                    } else {
                        format!("{}/{}", f.cur + 1, f.hits.len())
                    };
                    ui.label(egui::RichText::new(cnt).color(crate::theme::Palette::TEXT_DIM).size(11.0));
                    if ui.button(icon::CARET_UP).on_hover_text(crate::i18n::tr("上一个", "Prev")).clicked() {
                        action = FindAction::Step(-1);
                    }
                    if ui.button(icon::CARET_DOWN).on_hover_text(crate::i18n::tr("下一个", "Next")).clicked() {
                        action = FindAction::Step(1);
                    }
                    if ui.button(icon::X).clicked() {
                        action = FindAction::Close;
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    action = FindAction::Close;
                }
            });
        action
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<u8> {
        // 从全局配色同步：任一终端切换后，所有终端下一帧统一生效
        self.dark = term_dark_flag().load(std::sync::atomic::Ordering::Relaxed);
        // Ctrl+Shift+F 切换终端内容搜索
        if ui.input(|i| (i.modifiers.ctrl || i.modifiers.command) && i.modifiers.shift && i.key_pressed(Key::F)) {
            if self.find.is_some() {
                self.find = None;
                self.search_hl = None;
            } else {
                self.find = Some(Find { focus: true, ..Default::default() });
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
        let (char_w, glyph_h) = ui.ctx().fonts_mut(|f| {
            let w = f.glyph_width(&font, 'M');
            let h = f.row_height(&font);
            (w, h)
        });
        // 行高 = 字形高度 × 1.2，避免上下两行过挤；字形在行内纵向居中
        let char_h = glyph_h * 1.2;
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
            let c = (((pos.x - rect.left()) / char_w).floor() as i32).clamp(0, cols as i32 - 1) as u16;
            let r = (((pos.y - rect.top()) / char_h).floor() as i32).clamp(0, rows as i32 - 1) as u16;
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
            let (scroll, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl || i.modifiers.command));
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
                    let nb = (self.scrollback as i64 + lines).clamp(0, 5000) as usize;
                    self.scrollback = nb;
                    self.parser.screen_mut().set_scrollback(nb);
                    self.search_hl = None; // 手动滚动后高亮失效
                }
            }
        }

        if report_mouse {
            // 转发鼠标按键/移动给远端
            let events = ui.input(|i| i.events.clone());
            for ev in &events {
                match ev {
                    egui::Event::PointerButton { pos, button, pressed, modifiers } if rect.contains(*pos) => {
                        let (r, c) = cell_at(*pos);
                        let base = match button {
                            egui::PointerButton::Primary => 0u8,
                            egui::PointerButton::Middle => 1,
                            egui::PointerButton::Secondary => 2,
                            _ => 0,
                        };
                        let mut cb = base;
                        if modifiers.alt { cb += 8; }
                        if modifiers.ctrl || modifiers.command { cb += 16; }
                        if *pressed {
                            self.held_btn = Some(base);
                            encode_mouse(menc, cb, c, r, true, &mut mouse_out);
                        } else {
                            self.held_btn = None;
                            // X10(Press) 模式不上报释放；SGR 用原按钮码，传统编码用 3
                            if mmode != vt100::MouseProtocolMode::Press {
                                let rel = if menc == vt100::MouseProtocolEncoding::Sgr { cb } else { 3 };
                                encode_mouse(menc, rel, c, r, false, &mut mouse_out);
                            }
                        }
                    }
                    egui::Event::PointerMoved(pos) if rect.contains(*pos) => {
                        let motion = mmode == vt100::MouseProtocolMode::AnyMotion
                            || (mmode == vt100::MouseProtocolMode::ButtonMotion && self.held_btn.is_some());
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
            // 本地拖拽选择文本
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let c = cell_at(p);
                    self.sel_anchor = Some(c);
                    self.sel_cursor = Some(c);
                }
            } else if resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    self.sel_cursor = Some(cell_at(p));
                }
            }
            if resp.clicked() {
                self.clear_selection();
            }
        }

        let tc = if self.dark { TermColors::dark() } else { TermColors::light() };
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, tc.bg);

        let sel = self.ordered_selection();
        let screen = self.parser.screen();
        let origin = rect.min;

        // 可见行中的链接：用于悬停下划线 + 点击打开（鼠标上报模式下让位给 TUI）
        let mut link_rects: Vec<(Rect, String)> = Vec::new();
        if !report_mouse {
            for row in 0..self.rows {
                for (sc, ec, url) in find_row_urls(screen, row, self.cols) {
                    let x0 = origin.x + sc as f32 * char_w;
                    let x1 = origin.x + (ec as f32 + 1.0) * char_w;
                    let y = origin.y + row as f32 * char_h;
                    link_rects.push((Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, y + char_h)), url));
                }
            }
        }
        let hover_pos = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
        let hover_link = hover_pos.and_then(|p| link_rects.iter().find(|(r, _)| r.contains(p)).map(|(_, u)| u.clone()));

        for row in 0..self.rows {
            let y = origin.y + row as f32 * char_h;
            // 先绘制该行的背景块（处理非默认底色）
            paint_row_backgrounds(&painter, screen, row, self.cols, origin, cell, &tc);
            // 搜索命中行高亮（整行淡黄底）
            if self.search_hl == Some(row) {
                painter.rect_filled(
                    Rect::from_min_max(egui::pos2(origin.x, y), egui::pos2(rect.right(), y + char_h)),
                    0.0,
                    Color32::from_rgba_unmultiplied(0xc2, 0x8e, 0x3c, 90),
                );
            }
            // 选区高亮（半透明，文字仍可见）
            if let Some(((sr, sc), (er, ec))) = sel {
                if row >= sr && row <= er {
                    let c0 = if row == sr { sc } else { 0 };
                    let c1 = if row == er { ec } else { self.cols.saturating_sub(1) };
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
            let hl = if self.highlight { highlight_colors(screen, row, self.cols) } else { Vec::new() };
            for col in 0..self.cols {
                let Some(c) = screen.cell(row, col) else { continue };
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
                    painter.text(egui::pos2(x, y + char_h / 2.0), egui::Align2::LEFT_CENTER, s, font.clone(), color);
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
                let uy = (origin.y + ((r.top() - origin.y) / char_h).round() * char_h) + (char_h + glyph_h) / 2.0 - 1.0;
                painter.hline(r.left()..=r.right(), uy, Stroke::new(1.0, crate::theme::Palette::ACCENT));
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
                painter.rect_filled(crect, 1.0, crate::theme::Palette::ACCENT.gamma_multiply(0.6));
            } else {
                painter.rect_stroke(crect, 1.0, Stroke::new(1.2, crate::theme::Palette::ACCENT.gamma_multiply(0.8)), egui::StrokeKind::Inside);
            }
        }

        // 启用 IME（中文 / fcitx 等输入法）：聚焦时上报输入区，并把候选框定位到光标处。
        // 否则平台不会在终端上激活输入法，导致无法输入中文。
        if focused {
            let (cr, cc) = screen.cursor_position();
            let ipos = origin + Vec2::new(cc as f32 * char_w, cr as f32 * char_h);
            let irect = Rect::from_min_size(ipos, cell);
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput { rect: irect, cursor_rect: irect });
            });
            // 在光标处显示 IME 预编辑（组字中的拼音/候选），铺底 + 下划线以便辨识
            if !self.ime_preedit.is_empty() {
                let font = egui::FontId::monospace(self.font_size / crate::theme::CJK_SCALE);
                let galley = painter.layout_no_wrap(self.ime_preedit.clone(), font, crate::theme::Palette::ACCENT);
                let bg = Rect::from_min_size(ipos, galley.size());
                painter.rect_filled(bg, 0.0, crate::theme::Palette::PANEL);
                painter.galley(ipos, galley, crate::theme::Palette::ACCENT);
                painter.hline(bg.x_range(), bg.max.y - 1.0, Stroke::new(1.0, crate::theme::Palette::ACCENT));
            }
        }

        // 键盘输入
        let mut out = if focused { self.collect_input(ui) } else { Vec::new() };

        // 键盘复制/粘贴由 collect_input 内的 Copy/Cut/Paste 事件处理（egui 会把
        // Ctrl+C/X/V 转成这些事件而不再下发按键）。这里只处理右键菜单。
        let mut do_copy = false;
        let mut do_paste = false;
        let mut start_log = false;
        resp.context_menu(|ui| {
            // 菜单项不换行（否则英文较长的「Highlight ERROR/WARN」会折行，复选框被挤到两行正中）
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let sel = self.has_selection();
            if ui.add_enabled(sel, egui::Button::new(crate::i18n::tr("复制", "Copy"))).clicked() {
                do_copy = true;
                ui.close();
            }
            if ui.button(crate::i18n::tr("粘贴", "Paste")).clicked() {
                do_paste = true;
                ui.close();
            }
            ui.separator();
            let theme_label = if self.dark {
                crate::i18n::tr("切换为浅色终端", "Light terminal")
            } else {
                crate::i18n::tr("切换为深色终端", "Dark terminal")
            };
            if ui.button(theme_label).clicked() {
                let nd = !self.dark;
                // 写全局开关：所有终端同步切换；并存盘记住选择
                term_dark_flag().store(nd, std::sync::atomic::Ordering::Relaxed);
                crate::store::save_term_dark(nd);
                self.dark = nd;
                ui.close();
            }
            if ui.checkbox(&mut self.highlight, crate::i18n::tr("高亮 ERROR/WARN", "Highlight ERROR/WARN")).clicked() {
                ui.close();
            }
            // 「强制 X11」已移至左侧监控栏的右键菜单，避免 shell 右键项过多
            ui.separator();
            // 会话日志录制
            if self.log_file.is_some() {
                if ui.button(crate::i18n::tr("停止录制日志", "Stop recording")).clicked() {
                    self.log_file = None;
                    ui.close();
                }
            } else if ui.button(crate::i18n::tr("录制会话日志…", "Record session log…")).clicked() {
                start_log = true;
                ui.close();
            }
            ui.separator();
            ui.menu_button(crate::i18n::tr("语言", "Language"), crate::i18n::language_menu);
        });
        if start_log {
            if let Some(path) = rfd::FileDialog::new().set_file_name("session.log").save_file() {
                if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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
        // 复制/粘贴（尤其右键菜单）后焦点会丢失，重新聚焦终端，免得还要再点一下
        if do_copy || do_paste {
            resp.request_focus();
        }
        // 鼠标上报字节（若有）
        out.extend_from_slice(&mouse_out);
        out
    }

    /// 把本帧键盘事件编码为终端字节，并维护输入行影子缓冲 / 前缀历史搜索。
    fn collect_input(&mut self, ui: &egui::Ui) -> Vec<u8> {
        let mut out = Vec::new();
        let events: Vec<egui::Event> = ui.input(|i| i.events.clone());
        let shift = ui.input(|i| i.modifiers.shift);
        // 全屏程序（vim/less/htop 等用备用屏幕）下不拦截方向键，避免破坏其交互
        let alt = self.parser.screen().alternate_screen();
        if alt {
            self.input_line.clear();
            self.hist = None;
        }
        for ev in events {
            match ev {
                egui::Event::Text(t) => {
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    out.extend_from_slice(t.as_bytes());
                }
                // 输入法预编辑（组字中）：暂存以在光标处显示，不发往远端
                egui::Event::Ime(egui::ImeEvent::Preedit(s)) => {
                    log::debug!("IME Preedit: {s:?}");
                    self.ime_preedit = s;
                }
                // 输入法提交（中文等）：清空预编辑，提交串以 UTF-8 发往远端
                egui::Event::Ime(egui::ImeEvent::Commit(t)) => {
                    log::debug!("IME Commit: {t:?}");
                    self.ime_preedit.clear();
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    out.extend_from_slice(t.as_bytes());
                }
                // 输入法启用/禁用：清掉残留预编辑
                egui::Event::Ime(egui::ImeEvent::Enabled) | egui::Event::Ime(egui::ImeEvent::Disabled) => {
                    log::debug!("IME enabled/disabled event");
                    self.ime_preedit.clear();
                }
                egui::Event::Paste(t) => {
                    if !alt {
                        self.input_line.push_str(&t);
                        self.hist = None;
                    }
                    out.extend_from_slice(t.as_bytes());
                }
                // egui 把 Ctrl+C / Ctrl+X 转成 Copy/Cut 事件且不再下发按键，需在此处理：
                // 终端里 Ctrl+C 应发 SIGINT(0x03)，而不是“复制”。
                egui::Event::Copy => {
                    // macOS：Cmd+C 复制（Ctrl+C 仍以按键事件到达 -> 走 encode_key 发 0x03）。
                    // 其它平台：command 即 Ctrl —— 无 Shift 发 SIGINT，按住 Shift 才是复制。
                    let copy_selection = cfg!(target_os = "macos") || shift;
                    if copy_selection {
                        if let Some(t) = self.selected_text() {
                            ui.ctx().copy_text(t);
                        }
                    } else {
                        out.push(0x03);
                        if !alt {
                            self.input_line.clear();
                            self.hist = None;
                        }
                    }
                }
                egui::Event::Cut => {
                    // 终端无“剪切”语义：非 macOS 下 Ctrl+X 发 0x18
                    #[cfg(not(target_os = "macos"))]
                    if !shift {
                        out.push(0x18);
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    // 有前缀时，上下键做「本会话历史前缀搜索」（仅普通修饰、非全屏）
                    let plain = !modifiers.ctrl && !modifiers.alt && !modifiers.command && !modifiers.shift;
                    if !alt && plain && matches!(key, Key::ArrowUp | Key::ArrowDown) {
                        out.extend_from_slice(&self.history_nav(key == Key::ArrowUp));
                        continue;
                    }
                    if !alt {
                        match key {
                            Key::Enter => self.commit_line(),
                            Key::Backspace => {
                                self.input_line.pop();
                                self.hist = None;
                            }
                            Key::C | Key::U if modifiers.ctrl => {
                                self.input_line.clear();
                                self.hist = None;
                            }
                            _ => {}
                        }
                    }
                    encode_key(key, modifiers, &mut out);
                }
                _ => {}
            }
        }
        out
    }

    /// 上/下键的历史前缀搜索；返回应发送给远端的字节。
    fn history_nav(&mut self, up: bool) -> Vec<u8> {
        // 空行：交给远端 shell 自身的历史
        if self.input_line.is_empty() {
            self.hist = None;
            return if up { b"\x1b[A".to_vec() } else { b"\x1b[B".to_vec() };
        }
        let prefix = match &self.hist {
            Some(h) => h.prefix.clone(),
            None => self.input_line.clone(),
        };
        let start = self.hist.as_ref().map(|h| h.idx as isize).unwrap_or(self.history.len() as isize);
        if up {
            let mut i = start - 1;
            while i >= 0 {
                let cand = &self.history[i as usize];
                if cand.starts_with(&prefix) && cand != &self.input_line {
                    let m = cand.clone();
                    self.hist = Some(HistState { prefix, idx: i as usize });
                    return self.rewrite_line(&m);
                }
                i -= 1;
            }
            Vec::new() // 没有更早的匹配，保持不变
        } else {
            if self.hist.is_none() {
                return Vec::new(); // 不在搜索中，下键无意义
            }
            let mut i = start + 1;
            while (i as usize) < self.history.len() {
                let cand = &self.history[i as usize];
                if cand.starts_with(&prefix) {
                    let m = cand.clone();
                    self.hist = Some(HistState { prefix, idx: i as usize });
                    return self.rewrite_line(&m);
                }
                i += 1;
            }
            // 越过最新匹配：恢复到最初输入的前缀
            self.hist = None;
            self.rewrite_line(&prefix.clone())
        }
    }

    /// 清空远端当前行并键入 `text`（Ctrl+E 到行尾 + Ctrl+U 清行 + 文本）。
    fn rewrite_line(&mut self, text: &str) -> Vec<u8> {
        let mut out = vec![0x05, 0x15]; // Ctrl+E, Ctrl+U
        out.extend_from_slice(text.as_bytes());
        self.input_line = text.to_string();
        out
    }

    /// 回车提交：把当前行加入历史（去重相邻、去空）。
    fn commit_line(&mut self) {
        if !self.input_line.trim().is_empty()
            && self.history.last().map(|s| s != &self.input_line).unwrap_or(true)
        {
            self.history.push(self.input_line.clone());
            if self.history.len() > 500 {
                self.history.remove(0);
            }
        }
        self.input_line.clear();
        self.hist = None;
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

/// 在字节流中查找子序列，返回起始下标。
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 若字节流结尾是一个**不完整**的多字节 UTF-8 序列，返回需要暂存的尾字节数；否则 0。
/// 用于避免一个 UTF-8 字符被拆分在两次 `feed` 之间导致乱码。
fn incomplete_utf8_tail(b: &[u8]) -> usize {
    let mut cont = 0usize; // 已统计的连续字节（0b10xxxxxx）数量
    let mut i = b.len();
    while i > 0 && cont < 3 {
        i -= 1;
        let byte = b[i];
        if byte & 0b1100_0000 == 0b1000_0000 {
            cont += 1; // 连续字节，继续往前找首字节
            continue;
        }
        // 找到序列首字节（或单字节）：按首字节判断该序列需要的总长度
        let need = if byte & 0b1000_0000 == 0 {
            0 // ASCII，单字节，完整
        } else if byte & 0b1110_0000 == 0b1100_0000 {
            1 // 2 字节
        } else if byte & 0b1111_0000 == 0b1110_0000 {
            2 // 3 字节（绝大多数中文）
        } else if byte & 0b1111_1000 == 0b1111_0000 {
            3 // 4 字节
        } else {
            0 // 非法首字节，按完整处理，交给解析器
        };
        // 还差连续字节 -> 把「首字节 + 已有连续字节」整体暂存
        return if need > cont { cont + 1 } else { 0 };
    }
    0
}

#[cfg(test)]
mod utf8_tail_tests {
    use super::incomplete_utf8_tail;
    #[test]
    fn detects_split_multibyte() {
        // "你"=E4 BD A0：完整应为 0；缺尾则需暂存
        assert_eq!(incomplete_utf8_tail(&[0xE4, 0xBD, 0xA0]), 0);
        assert_eq!(incomplete_utf8_tail(&[0xE4, 0xBD]), 2); // 缺 1 个连续字节
        assert_eq!(incomplete_utf8_tail(&[0xE4]), 1); // 只有首字节
        assert_eq!(incomplete_utf8_tail(b"abc"), 0); // 纯 ASCII
        assert_eq!(incomplete_utf8_tail(&[0x41, 0xE4, 0xBD, 0xA0]), 0); // A + 完整"你"
    }
}

/// 把特殊按键编码为 ANSI 转义序列；Ctrl 组合键编码为控制字符。
fn encode_key(key: Key, mods: Modifiers, out: &mut Vec<u8>) {
    // Ctrl+Shift+C/V 保留给复制/粘贴，不作为终端输入
    if (mods.ctrl || mods.command) && mods.shift && matches!(key, Key::C | Key::V | Key::F) {
        return;
    }
    // Ctrl + 字母 -> 0x01..0x1a
    if mods.ctrl {
        if let Some(c) = key_to_ascii_letter(key) {
            out.push((c as u8 - b'a') + 1);
            return;
        }
    }
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => out.push(0x7f),
        Key::Tab => out.push(b'\t'),
        Key::Escape => out.push(0x1b),
        Key::ArrowUp => out.extend_from_slice(b"\x1b[A"),
        Key::ArrowDown => out.extend_from_slice(b"\x1b[B"),
        Key::ArrowRight => out.extend_from_slice(b"\x1b[C"),
        Key::ArrowLeft => out.extend_from_slice(b"\x1b[D"),
        Key::Home => out.extend_from_slice(b"\x1b[H"),
        Key::End => out.extend_from_slice(b"\x1b[F"),
        Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
        Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
        Key::Insert => out.extend_from_slice(b"\x1b[2~"),
        Key::Delete => out.extend_from_slice(b"\x1b[3~"),
        _ => {}
    }
}

fn key_to_ascii_letter(key: Key) -> Option<char> {
    use Key::*;
    let c = match key {
        A => 'a', B => 'b', C => 'c', D => 'd', E => 'e', F => 'f', G => 'g',
        H => 'h', I => 'i', J => 'j', K => 'k', L => 'l', M => 'm', N => 'n',
        O => 'o', P => 'p', Q => 'q', R => 'r', S => 's', T => 't', U => 'u',
        V => 'v', W => 'w', X => 'x', Y => 'y', Z => 'z',
        _ => return None,
    };
    Some(c)
}

/// 把 vt100 颜色映射到 egui 颜色（含 256 色板）。
fn vt_color(c: vt100::Color, default: Color32, tc: &TermColors) -> Color32 {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => xterm256(i, tc),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

/// 编码一个鼠标事件为终端字节流。`cb` 为按钮码（含修饰位/移动位/滚轮位）。
/// `col`/`row` 为 0 基屏幕坐标，内部转 1 基。`press` 仅影响 SGR 的 M/m。
fn encode_mouse(enc: vt100::MouseProtocolEncoding, cb: u8, col: u16, row: u16, press: bool, out: &mut Vec<u8>) {
    let cx = col as u32 + 1;
    let cy = row as u32 + 1;
    match enc {
        vt100::MouseProtocolEncoding::Sgr => {
            let m = if press { 'M' } else { 'm' };
            out.extend_from_slice(format!("\x1b[<{cb};{cx};{cy}{m}").as_bytes());
        }
        // 传统 X10/normal 编码：ESC [ M (cb+32) (x+32) (y+32)，坐标上限 223
        _ => {
            let b = 32u32.saturating_add(cb as u32);
            let x = 32 + cx.min(223);
            let y = 32 + cy.min(223);
            out.extend_from_slice(&[0x1b, b'[', b'M', b as u8, x as u8, y as u8]);
        }
    }
}

/// 用系统默认浏览器打开 URL。
fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

/// 解析 OSC 7（`ESC ] 7 ; file://host/path BEL|ST`），返回最后一个上报的本地路径。
fn parse_osc7(data: &[u8]) -> Option<String> {
    let pat = b"\x1b]7;";
    let mut result = None;
    let mut i = 0;
    while i + pat.len() <= data.len() {
        let Some(rel) = data[i..].windows(pat.len()).position(|w| w == pat) else { break };
        let start = i + rel + pat.len();
        let mut end = start;
        while end < data.len() {
            if data[end] == 0x07 || (data[end] == 0x1b && data.get(end + 1) == Some(&b'\\')) {
                break;
            }
            end += 1;
        }
        if end >= data.len() {
            break; // 序列不完整
        }
        if let Ok(s) = std::str::from_utf8(&data[start..end]) {
            if let Some(rest) = s.strip_prefix("file://") {
                // 去掉 host 段，取从第一个 '/' 起的路径并做 percent 解码
                if let Some(slash) = rest.find('/') {
                    result = Some(percent_decode(&rest[slash..]));
                }
            }
        }
        i = end + 1;
    }
    result
}

/// 简单 percent 解码（%XX -> 字节）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 关键字高亮规则（小写匹配子串 -> 颜色）。red=错误，orange=警告。
const HL_RULES: &[(&str, Color32)] = &[
    ("error", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("fatal", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("panic", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("fail", Color32::from_rgb(0xd0, 0x40, 0x40)),
    ("warn", Color32::from_rgb(0xc8, 0x8a, 0x20)),
];

/// 计算一行各单元格的高亮覆盖色（None=不覆盖）。关键字为 ASCII，按 1 列/字符。
fn highlight_colors(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<Option<Color32>> {
    let mut chars: Vec<(u16, char)> = Vec::new();
    let mut col = 0u16;
    while col < cols {
        let wide = screen.cell(row, col).is_some_and(|c| c.is_wide());
        match screen.cell(row, col) {
            Some(c) if c.is_wide_continuation() => {}
            Some(c) => chars.push((col, c.contents().chars().next().unwrap_or(' '))),
            None => chars.push((col, ' ')),
        }
        col += if wide { 2 } else { 1 };
    }
    let text: String = chars.iter().map(|(_, c)| *c).collect();
    let lower = text.to_lowercase();
    let mut out = vec![None; cols as usize];
    for (kw, color) in HL_RULES {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(kw) {
            let start = from + rel;
            let start_char = text[..start].chars().count();
            for k in 0..kw.chars().count() {
                if let Some(&(c, _)) = chars.get(start_char + k) {
                    out[c as usize] = Some(*color);
                }
            }
            from = start + kw.len();
        }
    }
    out
}

/// 在一行里查找 http(s) 链接，返回 (起列, 止列(含), url)。列号按屏幕单元格计。
fn find_row_urls(screen: &vt100::Screen, row: u16, cols: u16) -> Vec<(u16, u16, String)> {
    // 逐字符记录 (起始列, 字符)；宽字符续格跳过
    let mut chars: Vec<(u16, char)> = Vec::new();
    let mut col = 0u16;
    while col < cols {
        let wide = screen.cell(row, col).is_some_and(|c| c.is_wide());
        match screen.cell(row, col) {
            Some(c) if c.is_wide_continuation() => {}
            Some(c) => chars.push((col, c.contents().chars().next().unwrap_or(' '))),
            None => chars.push((col, ' ')),
        }
        col += if wide { 2 } else { 1 };
    }
    let text: String = chars.iter().map(|(_, c)| *c).collect();
    let mut urls = Vec::new();
    let mut byte = 0usize;
    while let Some(rel) = text[byte..].find("http") {
        let start = byte + rel;
        let rest = &text[start..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            // 扩展到空白或明显的分隔符为止
            let mut end = start;
            for (k, ch) in rest.char_indices() {
                if ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | '`' | '|') {
                    break;
                }
                end = start + k + ch.len_utf8();
            }
            let url = text[start..end]
                .trim_end_matches(|c| matches!(c, '.' | ',' | ';' | ':' | ')' | ']' | '}' | '!' | '?'))
                .to_string();
            let ulen = url.chars().count();
            if ulen > 0 {
                let start_char = text[..start].chars().count();
                let sc = chars[start_char.min(chars.len() - 1)].0;
                let ec = chars[(start_char + ulen - 1).min(chars.len() - 1)].0;
                urls.push((sc, ec, url));
            }
            byte = end;
        } else {
            byte = start + 4;
        }
    }
    urls
}

fn cell_format(c: &vt100::Cell, font: &FontId, tc: &TermColors) -> TextFormat {
    let mut fg = vt_color(c.fgcolor(), tc.fg, tc);
    // 反显：文字改用背景色（实际背景块在 paint_row_backgrounds 中绘制）
    if c.inverse() {
        fg = vt_color(c.bgcolor(), tc.bg, tc);
    }
    let mut f = TextFormat {
        font_id: font.clone(),
        color: fg,
        ..Default::default()
    };
    if c.underline() {
        f.underline = Stroke::new(1.0, fg);
    }
    f
}

/// 逐格绘制非默认背景色（egui 文本布局不便携带逐段背景，单独画矩形）。
fn paint_row_backgrounds(
    painter: &egui::Painter,
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
    origin: egui::Pos2,
    cell: Vec2,
    tc: &TermColors,
) {
    for col in 0..cols {
        if let Some(c) = screen.cell(row, col) {
            // 宽字符（中文等）的续格由其首格统一铺底，避免只盖住半个字
            if c.is_wide_continuation() {
                continue;
            }
            let mut bg = vt_color(c.bgcolor(), Color32::TRANSPARENT, tc);
            if c.inverse() {
                bg = vt_color(c.fgcolor(), tc.fg, tc);
            }
            if bg != Color32::TRANSPARENT {
                let w = if c.is_wide() { cell.x * 2.0 } else { cell.x };
                let pos = origin + Vec2::new(col as f32 * cell.x, row as f32 * cell.y);
                painter.rect_filled(Rect::from_min_size(pos, Vec2::new(w, cell.y)), 0.0, bg);
            }
        }
    }
}

/// xterm 256 色板（0..15 取当前终端配色的 ANSI 表）。
fn xterm256(i: u8, tc: &TermColors) -> Color32 {
    match i {
        0..=15 => {
            let (r, g, b) = tc.ansi[i as usize];
            Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(conv(r), conv(g), conv(b))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc7_parsing() {
        let data = b"\x1b]7;file://host/home/user/%E4%B8%AD%E6%96%87\x07";
        assert_eq!(parse_osc7(data).as_deref(), Some("/home/user/\u{4e2d}\u{6587}"));
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
        let got: Vec<String> = find_row_urls(p.screen(), 0, 80).into_iter().map(|(_, _, u)| u).collect();
        assert_eq!(got, vec!["https://example.com/a/b".to_string(), "http://x.y/z".to_string()]);
    }

    #[test]
    fn no_url_no_match() {
        let mut p = vt100::Parser::new(2, 80, 0);
        p.process(b"plain text httpsomething not a url");
        assert!(find_row_urls(p.screen(), 0, 80).is_empty());
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
        t.find = Some(Find { query: "number 5".into(), ..Default::default() });
        t.run_search();
        let f = t.find.as_ref().unwrap();
        // "number 5" 命中 5,50..59 等多行
        assert!(f.hits.len() >= 2, "应找到多处命中，实际 {}", f.hits.len());
        assert!(t.search_hl.is_some(), "应高亮命中行");
        // 不存在的查询无命中
        t.find = Some(Find { query: "zzzNOPE".into(), ..Default::default() });
        t.run_search();
        assert!(t.find.as_ref().unwrap().hits.is_empty());
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
