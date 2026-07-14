//! 终端模拟：用 `vt100` 维护屏幕模型，并在 egui 中以等宽字体逐格渲染；
//! 同时把键盘事件编码为终端字节流。

use egui::{FontId, Key, Rect, Sense, Vec2};

mod feed;
mod input;
mod keys;
mod osc;
mod paint;
mod search;
mod selection;
mod theme;
mod ui_paint;
mod vt;
mod default;

use input::HistState;
use keys::encode_mouse;
use search::{Find, FindAction};
pub use theme::current_bg;
use theme::{term_theme, TERM_THEMES};
use ui_paint::PaintParams;

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
    /// 部分匹配中暂存的疑似回显字节：真实内容里偶然出现和 `echo_expect` 开头相同的字节时，
    /// 会先当作「可能是目标回显」而不立即输出；一旦后续字节证明只是巧合（失配），要把这些
    /// 暂存字节还给真实输出，不能凭空丢掉。
    echo_pending: Vec<u8>,
    /// IME 预编辑串（拼音组字中的未提交文本），显示在光标处
    ime_preedit: String,
    /// 上一帧焦点状态（仅用于焦点变化时打印诊断日志）
    prev_focused: bool,
    /// 上次是否处于备用屏（vim/less 等）；用于离开备用屏时恢复光标可见，防「光标丢失」
    prev_alt: bool,
    /// 正在拖动右侧滚动条（区别于拖动选择文本）
    sb_dragging: bool,
    /// AI/MCP 命令补全检测：等待中的哨兵前缀 + 滑动扫描缓冲（与 `strip_echo` 完全独立的
    /// 状态，避免耦合到已在生产使用的回显吞除逻辑）。见 `feed.rs` 里的扫描步骤。
    ai_capture: Option<AiCapture>,
    /// 哨兵命中后的 (退出码, 纯文本输出)，供 App 每帧取走（None=尚未完成/无正在等待的捕获）。
    ai_done: Option<(i32, String)>,
    /// 普通屏本地回滚的像素余量：smooth_scroll_delta 带惯性，连续多帧的小增量若逐帧
    /// round 会一直得 0（触控板/平滑滚轮尤其明显），导致"滚了但纹丝不动"。这里跨帧
    /// 累计，凑够一整行才真正推动 vt100 scrollback，不足一行的余量保留到下一帧。
    local_scroll_accum: f32,
    /// DECSET 1007（Alternate Scroll）：应用要求终端在未启用鼠标上报时，把备用滚轮
    /// 转成光标键。Codex 的 inline/main-screen TUI 也会使用该模式，不能只看
    /// `alternate_screen()` 判断滚轮是否应交给应用。
    alternate_scroll: bool,
    /// DEC 私有模式序列可能跨 SSH 数据块，保留足够短的尾部供下一次 feed 拼接扫描。
    mode_scan_tail: Vec<u8>,
}

struct AiCapture {
    /// 待匹配的前缀（含唯一 nonce），命中后紧跟的数字到下一个 `\x1e` 之间即退出码。
    prefix: Vec<u8>,
    /// 武装以来喂入的原始字节（命中前的这段即命令输出，剥 ANSI 后返回给 AI）。
    /// 超过上限会从前端裁掉最早的部分并标记 `truncated`（极端长输出兜底，不做无界增长）。
    buf: Vec<u8>,
    truncated: bool,
}

/// 逐行裁掉行尾空白，再裁掉结尾的连续空行——`screen_text`/`history_text` 共用的收尾步骤。
fn finalize_lines(lines: Vec<String>) -> Vec<String> {
    let mut lines: Vec<String> = lines.into_iter().map(|l| l.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
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
            echo_pending: Vec::new(),
            ime_preedit: String::new(),
            prev_focused: false,
            local_scroll_accum: 0.0,
            alternate_scroll: false,
            mode_scan_tail: Vec::new(),
            prev_alt: false,
            sb_dragging: false,
            ai_capture: None,
            ai_done: None,
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
        self.echo_pending.clear();
    }

    /// 武装一次「等待哨兵」捕获：`prefix` 是唯一前缀（含 nonce），命中后紧跟的退出码数字
    /// 到下一个 `\x1e` 之间会被解析出来，见 `take_ai_done`。
    pub fn arm_ai_capture(&mut self, prefix: Vec<u8>) {
        self.ai_capture = Some(AiCapture {
            prefix,
            buf: Vec::new(),
            truncated: false,
        });
        self.ai_done = None;
    }

    /// 是否已有一次 AI 捕获正在等待（同一时刻只允许一个，见调用方的 busy 判断）。
    pub fn ai_capture_pending(&self) -> bool {
        self.ai_capture.is_some()
    }

    /// 取走哨兵命中后的结果：`(退出码, 已剥离 ANSI 的纯文本输出)`（取走即清空；None=仍未完成）。
    pub fn take_ai_done(&mut self) -> Option<(i32, String)> {
        self.ai_done.take()
    }

    /// 放弃当前捕获（如 App 侧判定超时/请求被取消）。
    pub fn cancel_ai_capture(&mut self) {
        self.ai_capture = None;
        self.ai_done = None;
    }

    /// 捕获尚未完成时，看一眼「目前为止」已剥离 ANSI 的输出（不消费、不影响后续检测）。
    pub fn peek_ai_output(&self) -> Option<String> {
        self.ai_capture.as_ref().map(|c| vt::strip_ansi_to_text(&c.buf))
    }

    /// 当前可见屏幕的纯文本（tmux capture-pane 风格），自动裁掉尾部空行。
    pub fn screen_text(&mut self) -> String {
        let saved = self.scrollback;
        if saved != 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
        let cols = self.cols;
        let lines: Vec<String> = self.parser.screen().rows(0, cols).collect();
        if saved != 0 {
            self.parser.screen_mut().set_scrollback(saved);
        }
        finalize_lines(lines).join("\n")
    }

    /// 完整历史（回滚缓冲区 + 当前可见屏），从最早到最新排列；`max_lines == 0` 不限制，
    /// 否则只保留最后 `max_lines` 行（避免超长回滚一次性喂给 AI）。
    pub fn history_text(&mut self, max_lines: usize) -> String {
        let mut lines = finalize_lines(self.collect_lines());
        if max_lines > 0 && lines.len() > max_lines {
            let start = lines.len() - max_lines;
            lines.drain(..start);
        }
        lines.join("\n")
    }

    /// 请求下一帧让终端区域获得键盘焦点。
    pub fn request_focus(&mut self) {
        self.focus_req = true;
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

        // 滚轮：Ctrl 调字号；鼠标上报时发滚轮键（64/65）；全屏程序（htop/less/codex/
        // claude code 这类跑在备用屏幕、但没开鼠标上报的 TUI）转发方向键模拟滚动；
        // 否则（主屏幕、无鼠标上报）本地回滚。
        //
        // 鼠标滚轮键/方向键这两条路径都是「离散点击」语义（一次滚轮缺口 = 一次点击/
        // 一次按键），必须用本帧的原始滚轮事件，不能用 smooth_scroll_delta：后者带惯性
        // 平滑，一次真实滚动会在其后连续多帧里持续吐出衰减的非零值，若照旧按帧转换成
        // 点击/按键，会把一次滚动动作放大成几十次点击尾随发送给远端，表现为「点了但慢
        // 慢滚」的本地场景感觉正常，一到把滚轮语义变成离散点击/按键的远端 TUI 程序里就
        // 变成「越滚越快、停不下来」（这正是 claude code 里滚动特别快的根因）。本地像素级
        // 回滚（最后的 else 分支）仍用 smooth_scroll_delta，因为它要的就是这种连续平滑感。
        if resp.hovered() {
            let (smooth, raw, ctrl) = ui.input(|i| {
                // smooth_scroll_delta 带惯性平滑：一次真实滚动会在其后连续多帧持续吐出衰减的
                // 非零值。离散点击/按键语义的两条路径需要「这一帧真实发生了多少」，故从本帧
                // 原始事件里自己累加（单位统一换算成「行」，与 char_h 步长口径一致）。
                let mut raw_lines = 0.0f32;
                for ev in &i.events {
                    if let egui::Event::MouseWheel { unit, delta, .. } = ev {
                        raw_lines += match unit {
                            egui::MouseWheelUnit::Line => delta.y,
                            egui::MouseWheelUnit::Point => delta.y / char_h,
                            egui::MouseWheelUnit::Page => delta.y * rows as f32,
                        };
                    }
                }
                (
                    i.smooth_scroll_delta.y,
                    raw_lines,
                    i.modifiers.ctrl || i.modifiers.command,
                )
            });
            let alt = self.parser.screen().alternate_screen();
            if raw != 0.0 {
                // 诊断日志（RUST_LOG=debug 可见）：完整记录一次滚轮事件从「到达」到「落地」的
                // 全过程状态，而不是只挑几个字段——否则没法回答"事件到底有没有到、走了哪条路、
                // scrollback 是否真的变了"这几个关键问题（外部审查报告 5.3 的意见）。
                log::debug!(
                    "term scroll: alt={alt} alt_scroll={} report_mouse={report_mouse} mmode={mmode:?} \
                     menc={menc:?} app_cursor={} raw={raw:.3} smooth={smooth:.2} char_h={char_h:.2} \
                     sb_before={} accum_before={:.3}",
                    self.alternate_scroll,
                    self.parser.screen().application_cursor(),
                    self.scrollback,
                    self.local_scroll_accum,
                );
            }
            if ctrl && smooth != 0.0 {
                self.font_size = (self.font_size + smooth.signum() * 1.0).clamp(8.0, 32.0);
            } else if report_mouse {
                self.local_scroll_accum = 0.0; // 切换路径：旧余量不该带到鼠标上报语义里
                if raw != 0.0 {
                    if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                        let (r, c) = cell_at(p);
                        let cb = if raw > 0.0 { 64 } else { 65 };
                        let steps = (raw.abs().round() as i32).clamp(1, 3);
                        for _ in 0..steps {
                            encode_mouse(menc, cb, c, r, true, &mut mouse_out);
                        }
                    }
                }
            } else if alt || self.alternate_scroll {
                // 备用屏幕没有回滚历史（本地 set_scrollback 在这里是空操作），大多数不开
                // 鼠标上报的全屏程序（less/htop 等）靠自己拦截方向键实现翻页/滚动。DECSET
                // 1007 则是应用对这种转换的显式请求；Codex 即使运行在主屏也会使用它，因此
                // 这里不能只依赖 alternate_screen()。
                self.local_scroll_accum = 0.0;
                if raw != 0.0 {
                    let steps = (raw.abs().round() as i32).clamp(1, 3);
                    let key = if raw > 0.0 { b"\x1b[A".as_slice() } else { b"\x1b[B".as_slice() };
                    for _ in 0..steps {
                        mouse_out.extend_from_slice(key);
                    }
                }
            } else if smooth != 0.0 {
                // 本地像素级回滚要的是连续平滑感，用 smooth_scroll_delta；但触控板/平滑滚轮
                // 的单帧增量常常小于半个字符高度，逐帧 round 会让这些小增量永远舍入成 0、
                // 白白丢掉（"滚了但纹丝不动"）。这里跨帧累计余量，凑够整行才真正移动
                // scrollback，不足一行的部分留到下一帧继续累加，方向反转时自然抵消。
                self.local_scroll_accum += smooth;
                let whole_lines = (self.local_scroll_accum / char_h).trunc() as i64;
                if whole_lines != 0 {
                    self.local_scroll_accum -= whole_lines as f32 * char_h;
                    let nb = (self.scrollback as i64 + whole_lines)
                        .clamp(0, DEFAULT_SCROLLBACK as i64) as usize;
                    self.parser.screen_mut().set_scrollback(nb);
                    // 回读 vt100 按「实际历史行数」钳制后的真实值：否则 self.scrollback 可能远超真实历史，
                    // 之后要空滚很多步才重新移动视口（「死滚动」）。到达边界时顺带清空余量，避免
                    // 继续同向空滚积累巨量余数（外部审查报告 5.1 的边界注意事项）。
                    let clamped = self.parser.screen().scrollback();
                    if clamped != nb {
                        self.local_scroll_accum = 0.0;
                    }
                    self.scrollback = clamped;
                    self.recompute_search_hl(); // 手动滚动：高亮跟随命中行（滚出视口才消失）
                }
            }
            if raw != 0.0 {
                log::debug!(
                    "term scroll: -> sb_after={} accum_after={:.3} max_sb={}",
                    self.scrollback,
                    self.local_scroll_accum,
                    {
                        let cur = self.scrollback;
                        self.parser.screen_mut().set_scrollback(usize::MAX);
                        let m = self.parser.screen().scrollback();
                        self.parser.screen_mut().set_scrollback(cur);
                        m
                    },
                );
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
            // 拖动起点落在右侧滚动条上 → 拖滚动条；否则本地拖拽选择文本。
            // 拖拽需移动超过阈值才激活，此刻指针已离开真实按下点——路由判断（滚动条/文本）
            // 和选区锚点都必须用「按下位置」，否则起始处会被误判/漏选（与 editor.rs 的同类修法一致）。
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let press = crate::ui::drag_press_pos(ui, p);
                    if max_sb > 0 && press.x >= sb_track.left() {
                        self.sb_dragging = true;
                    } else {
                        let cur = cell_at(p);
                        self.sel_anchor = Some(cell_at(press));
                        self.sel_cursor = Some(cur);
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

        self.paint_terminal(
            ui,
            PaintParams {
                rect,
                resp: &resp,
                font: &font,
                glyph_h,
                char_w,
                char_h,
                cell,
                ppp,
                focused,
                report_mouse,
                max_sb,
                sb_w,
                sb_track,
            },
        );

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


#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
