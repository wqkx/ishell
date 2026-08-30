use super::paint::{brighten_rgb, find_row_urls, highlight_colors, vt_color, xterm256};
use super::theme::TermColors;
use super::*;
use egui::Color32;

#[test]
fn osc7_parsing() {
    let data = b"\x1b]7;file://host/home/user/%E4%B8%AD%E6%96%87\x07";
    assert_eq!(
        osc::parse_osc7(data).as_deref(),
        Some("/home/user/\u{4e2d}\u{6587}")
    );
    assert_eq!(osc::parse_osc7(b"no osc here"), None);
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
fn ai_capture_detects_sentinel_and_exit_code() {
    let mut t = Terminal::new();
    t.feed(b"prompt$ echo hi\r\nhi\r\n");
    // 武装捕获，喂入一批混合了「正常输出 + 哨兵」的字节（模拟一次 feed 里全齐）
    t.arm_ai_capture(b"\x1eAI_DONE_42:".to_vec());
    assert!(t.ai_capture_pending());
    t.feed(b"more output\r\n\x1eAI_DONE_42:7\x1e");
    let (code, out) = t.take_ai_done().expect("应已命中哨兵");
    assert_eq!(code, 7);
    assert!(!t.ai_capture_pending()); // 命中后自动清空
    assert!(t.take_ai_done().is_none()); // 取走即清空，第二次为 None
    // 武装之后（"prompt$ echo hi\r\nhi\r\n" 之前的内容不算）才开始记录输出
    assert!(!out.contains("prompt$"));
    assert!(out.contains("more output"));
}

#[test]
fn ai_capture_sentinel_split_across_feed_calls() {
    let mut t = Terminal::new();
    t.arm_ai_capture(b"\x1eAI_DONE_99:".to_vec());
    // 哨兵前缀被拆到两次 feed() 里，退出码和结束标记又是第三次
    t.feed(b"output\r\n\x1eAI_DO");
    assert!(t.take_ai_done().is_none());
    // 命中前可以看到「目前为止」的部分输出（此时哨兵前缀还没凑完，残留片段属预期）
    assert_eq!(t.peek_ai_output().as_deref(), Some("output\nAI_DO"));
    t.feed(b"NE_99:");
    assert!(t.take_ai_done().is_none());
    t.feed(b"0\x1e");
    let (code, out) = t.take_ai_done().expect("应已命中哨兵");
    assert_eq!(code, 0);
    assert_eq!(out, "output\n");
}

#[test]
fn ai_capture_ignores_unmatched_prefix() {
    let mut t = Terminal::new();
    t.arm_ai_capture(b"\x1eAI_DONE_1:".to_vec());
    // 不同 nonce 的哨兵不应触发命中
    t.feed(b"\x1eAI_DONE_2:0\x1e");
    assert!(t.take_ai_done().is_none());
    assert!(t.ai_capture_pending());
    t.cancel_ai_capture();
    assert!(!t.ai_capture_pending());
}

#[test]
fn expect_echo_survives_unrelated_bytes_arriving_first() {
    // 复现场景：AI 命令先发真实命令（其自身回显不该被吞），紧接着发标记行（回显要被吞掉）。
    // 两条命令的回显可能在同一批/相邻几批字节里先后到达，标记行回显不一定是 armed 之后
    // 第一批收到的字节。之前的实现一旦第一个字节对不上就永久放弃吞回显，导致标记行原样漏出。
    let mut t = Terminal::new();
    let marker = "printf '\x1eAI_DONE_1:%d\x1e' $?; printf '\\r\\x1b[K'";
    t.expect_echo(marker);
    // 先到达的是真实命令自己的回显+输出：不该被吞，也不该打断后续对标记行的匹配。
    t.feed(b"echo hi\r\nhi\r\n");
    // 标记行的回显紧随其后到达：应被完整吞掉，不出现在可见输出里。
    t.feed(marker.as_bytes());
    t.feed(b"\r\n");
    let visible = t.screen_text();
    assert!(visible.contains("hi"), "真实命令输出不应被误吞：{visible:?}");
    assert!(
        !visible.contains("printf"),
        "标记行回显应被吞掉，不应出现在可见终端里：{visible:?}"
    );
}

#[test]
fn expect_echo_coincidental_first_char_in_real_content_not_lost() {
    // 复现场景（真实环境里跑 `hostname && whoami && pwd` 触发过）：真实命令的回显里偶然
    // 出现和标记行开头相同的字符（这里是 "pwd" 里的 'p'，标记行以 "printf" 开头），
    // 旧实现会把这个 'p' 当成「可能是目标回显」的开头暂存起来，紧接着 'w' 对不上就整体
    // 放弃匹配——不仅把这个 'p' 弄丢了（"pwd" 变成 "wd"），还因为放弃时把 echo_expect
    // 清空，导致后面真正的标记行回显再也不会被吞、原样漏了出来。
    let mut t = Terminal::new();
    let marker = "printf '\x1eAI_DONE_2:%d\x1e' $?; printf '\\r\\x1b[K'";
    t.expect_echo(marker);
    t.feed(b"hostname && whoami && pwd\r\n");
    t.feed(b"host\nuser\n/home/user\r\n");
    t.feed(marker.as_bytes());
    t.feed(b"\r\n");
    let visible = t.screen_text();
    assert!(
        visible.contains("pwd"),
        "巧合命中标记行首字符的真实字节不应丢失：{visible:?}"
    );
    assert!(
        !visible.contains("printf"),
        "标记行回显不应因为前面一次巧合失配就漏出来：{visible:?}"
    );
}

#[test]
fn ai_capture_end_to_end_matches_real_mcp_bridge_wire_format() {
    // 之前几条 ai_capture 测试用的前缀都带一个原始 \x1e 字节开头（`b"\x1eAI_DONE_42:"`），
    // 但 mcp_bridge.rs 里 RunCommand 实际发的前缀早就改成纯文本 "AI_DONE_{nonce}:"
    // （不带原始控制字节——见该文件里关于 ECHOCTL 的注释），真正的 \x1e 只由 printf
    // 在执行后的输出里产生。这里用跟生产完全一致的格式走一遍完整流程，确保两边没有
    // 悄悄分叉、单测测的不是实际线上跑的东西。
    let mut t = Terminal::new();
    let prefix = "AI_DONE_123456789:";
    let marker = format!("printf '{prefix}%d\\x1e' $?; printf '\\r\\x1b[K'");
    t.expect_echo(&marker);
    t.arm_ai_capture(prefix.as_bytes().to_vec());
    // 真实命令自己的回显 + 输出（不该被吞，也不该打断后面对标记行的匹配）
    t.feed(b"echo hi\r\nhi\r\n");
    // 标记行的回显（应被完整吞掉）
    t.feed(marker.as_bytes());
    t.feed(b"\r\n");
    // printf 真正执行后的输出：前缀 + 退出码 + 真实 \x1e 字节（不是转义文本）
    t.feed(format!("{prefix}0\x1e").as_bytes());
    let (code, out) = t.take_ai_done().expect("应已命中哨兵");
    assert_eq!(code, 0);
    // Terminal::take_ai_done 只负责剥 ANSI，不负责裁掉命令自身的回显——那是
    // mcp_bridge.rs::trim_command_echo_and_prompt 的职责（见该文件里的单测），这里
    // 保留原始回显是符合预期的。
    assert_eq!(out, "echo hi\nhi\n");
    let visible = t.screen_text();
    assert!(!visible.contains("printf"), "标记行不应出现在可见终端里：{visible:?}");
}

#[test]
fn strip_ansi_removes_escapes_and_normalizes_newlines() {
    use super::vt::strip_ansi_to_text;
    let raw = b"\x1b[32mgreen\x1b[0m text\r\nline2\x1b]0;title\x07end";
    assert_eq!(strip_ansi_to_text(raw), "green text\nline2end");
}

#[test]
fn screen_text_matches_visible_rows() {
    let mut t = Terminal::new();
    t.feed(b"line one\r\nline two\r\n");
    let s = t.screen_text();
    assert!(s.contains("line one"));
    assert!(s.contains("line two"));
    // 尾部空行应被裁掉，不残留大片空白
    assert!(!s.ends_with('\n'));
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

#[test]
fn growing_main_screen_keeps_existing_scrollback() {
    let mut t = Terminal::new();
    assert!(t.resize(80, 10));
    for i in 0..40 {
        t.feed(format!("L{i}\r\n").as_bytes());
    }
    t.parser.screen_mut().set_scrollback(usize::MAX);
    let before = t.parser.screen().scrollback();
    assert!(before > 0);
    t.parser.screen_mut().set_scrollback(0);
    t.scrollback = 0;

    // 回归：旧实现会把全部缓冲按更高视口重放，导致历史被吸回可见区；应用紧接着
    // 清屏重绘后，max scrollback 就从非零变成 0。
    assert!(t.resize(100, 30));
    t.parser.screen_mut().set_scrollback(usize::MAX);
    assert_eq!(t.parser.screen().scrollback(), before);
}

#[test]
fn replies_to_cursor_position_query_in_output_order() {
    let mut t = Terminal::new();
    let reply = t.feed(b"\x1b[2;3H\x1b[6n\x1b[5;6H");

    // 查询发生在第 2 行第 3 列；后续光标移动不能污染已经生成的 CPR。
    assert_eq!(reply, b"\x1b[2;3R");
    assert_eq!(t.parser.screen().cursor_position(), (4, 5));
}

#[test]
fn replies_to_cursor_position_query_split_across_feeds() {
    let mut t = Terminal::new();
    assert!(t.feed(b"abc\x1b[").is_empty());
    assert_eq!(t.feed(b"6n"), b"\x1b[1;4R");
}

#[test]
fn top_anchored_scroll_region_writes_to_scrollback() {
    let mut t = Terminal::new();
    assert!(t.resize(20, 5));
    t.feed(b"history-row\r\nlive-one\r\nlive-two");

    // Codex/ratatui 的 inline history insertion：限制顶部区域后用 CSI S 将首行
    // 推出屏幕。真实终端会把该行放入 scrollback；原 vt100 0.16.2 会直接丢弃。
    t.feed(b"\x1b[1;3r\x1b[S");
    t.parser.screen_mut().set_scrollback(usize::MAX);
    assert_eq!(t.parser.screen().scrollback(), 1);
    assert_eq!(t.parser.screen().cell(0, 0).unwrap().contents(), "h");
}

// ── 按键编码（keys.rs::encode_key）─────────────────────────────────────────
// 这些组合此前要么丢修饰键、要么完全不发，导致在 iShell 里跑 Claude Code 等 TUI 时
// 大量快捷键失灵（Shift+Tab 切模式、Shift+Enter 换行、Ctrl+方向键按词跳转等）。

/// 便捷：按 key+修饰编码一次，返回字节。
fn enc(key: egui::Key, mods: egui::Modifiers, app_cursor: bool) -> Vec<u8> {
    let mut v = Vec::new();
    keys::encode_key(key, mods, app_cursor, &mut v);
    v
}

const NONE: egui::Modifiers = egui::Modifiers::NONE;
const SHIFT: egui::Modifiers = egui::Modifiers::SHIFT;
const ALT: egui::Modifiers = egui::Modifiers::ALT;
const CTRL: egui::Modifiers = egui::Modifiers::CTRL;

#[test]
fn shift_tab_encodes_back_tab() {
    // Claude Code 用 Shift+Tab 切换权限模式；此前发的是普通 \t（=Tab 补全）。
    assert_eq!(enc(egui::Key::Tab, SHIFT, false), b"\x1b[Z");
    assert_eq!(enc(egui::Key::Tab, NONE, false), b"\t");
}

#[test]
fn shift_or_alt_enter_encodes_esc_cr_for_newline_without_submit() {
    // 裸回车=提交；Shift/Alt+Enter=换行不提交（等价 /terminal-setup 给别的终端配的映射）。
    assert_eq!(enc(egui::Key::Enter, NONE, false), b"\r");
    assert_eq!(enc(egui::Key::Enter, SHIFT, false), b"\x1b\r");
    assert_eq!(enc(egui::Key::Enter, ALT, false), b"\x1b\r");
}

#[test]
fn modified_arrows_carry_modifier_param() {
    // 无修饰：普通/SS3 短形式（受 DECCKM 影响）
    assert_eq!(enc(egui::Key::ArrowLeft, NONE, false), b"\x1b[D");
    assert_eq!(enc(egui::Key::ArrowLeft, NONE, true), b"\x1bOD");
    // 带修饰：一律 CSI 1;<m> X 长形式，即便在应用光标模式下
    assert_eq!(enc(egui::Key::ArrowRight, CTRL, false), b"\x1b[1;5C"); // 按词右移
    assert_eq!(enc(egui::Key::ArrowRight, CTRL, true), b"\x1b[1;5C");
    assert_eq!(enc(egui::Key::ArrowLeft, SHIFT, false), b"\x1b[1;2D"); // 选择
    assert_eq!(enc(egui::Key::Home, CTRL, false), b"\x1b[1;5H");
}

#[test]
fn plain_up_down_use_ss3_in_app_cursor_mode() {
    // ipython/prompt_toolkit、vim、fzf 等在应用光标模式(DECCKM)下靠 SS3 形式(`ESC O A/B`)的
    // 方向键导航（补全菜单/光标）；普通模式(bash 提示符)是 CSI(`ESC [ A/B`)。collect_input 在
    // 应用光标模式下不再把裸上下键拦成本地历史，透传到这里编码——这两组序列必须对得上，
    // 否则 ipython 补全菜单按上下键无效（只能 Tab），正是本次修复的现象。
    assert_eq!(enc(egui::Key::ArrowUp, NONE, false), b"\x1b[A");
    assert_eq!(enc(egui::Key::ArrowUp, NONE, true), b"\x1bOA");
    assert_eq!(enc(egui::Key::ArrowDown, NONE, false), b"\x1b[B");
    assert_eq!(enc(egui::Key::ArrowDown, NONE, true), b"\x1bOB");
}

#[test]
fn tilde_keys_carry_modifier_param() {
    assert_eq!(enc(egui::Key::Delete, NONE, false), b"\x1b[3~");
    assert_eq!(enc(egui::Key::Delete, CTRL, false), b"\x1b[3;5~");
    assert_eq!(enc(egui::Key::PageUp, SHIFT, false), b"\x1b[5;2~");
}

#[test]
fn function_keys_encode() {
    // 此前 F1..F12 落到 `_ => {}`，按下什么都不发。
    assert_eq!(enc(egui::Key::F1, NONE, false), b"\x1bOP");
    assert_eq!(enc(egui::Key::F1, CTRL, false), b"\x1b[1;5P");
    assert_eq!(enc(egui::Key::F5, NONE, false), b"\x1b[15~");
    assert_eq!(enc(egui::Key::F12, NONE, false), b"\x1b[24~");
    assert_eq!(enc(egui::Key::F12, SHIFT, false), b"\x1b[24;2~");
}

#[test]
fn alt_letter_encodes_meta_prefix() {
    // readline 的 Meta 惯例：Alt+B/F 按词移动、Alt+D 删词。
    assert_eq!(enc(egui::Key::B, ALT, false), b"\x1bb");
    assert_eq!(enc(egui::Key::F, ALT, false), b"\x1bf");
    // Alt+Shift+B -> 大写
    let alt_shift = egui::Modifiers { alt: true, shift: true, ..Default::default() };
    assert_eq!(enc(egui::Key::B, alt_shift, false), b"\x1bB");
    // Alt+Backspace = 删除前一个词
    assert_eq!(enc(egui::Key::Backspace, ALT, false), b"\x1b\x7f");
}

#[test]
fn ctrl_letter_and_symbols_encode_control_chars() {
    assert_eq!(enc(egui::Key::C, CTRL, false), &[0x03]); // 中断
    assert_eq!(enc(egui::Key::Space, CTRL, false), &[0x00]); // set-mark
    assert_eq!(enc(egui::Key::Slash, CTRL, false), &[0x1f]); // 撤销
    assert_eq!(enc(egui::Key::Backslash, CTRL, false), &[0x1c]); // SIGQUIT
    // Ctrl+_ (=Ctrl+Shift+-) 发 US；而裸 Ctrl+- 必须不发——它被 egui 内建的
    // zoom_with_keyboard 绑成界面缩小（COMMAND 在 Linux/Windows 上就是 Ctrl），
    // 若这里也发 0x1f 就会「既缩放又发撤销」。
    let ctrl_shift = egui::Modifiers { ctrl: true, shift: true, ..Default::default() };
    assert_eq!(enc(egui::Key::Minus, ctrl_shift, false), &[0x1f]);
    assert!(enc(egui::Key::Minus, CTRL, false).is_empty());
    // Alt+Ctrl+B -> ESC 前缀 + 控制字符
    let alt_ctrl = egui::Modifiers { alt: true, ctrl: true, ..Default::default() };
    assert_eq!(enc(egui::Key::B, alt_ctrl, false), b"\x1b\x02");
}

#[test]
fn copy_paste_shortcuts_are_not_sent_to_terminal() {
    // Ctrl+Shift+C/V/F 保留给复制/粘贴/查找，不能当终端输入发下去。
    let cs = egui::Modifiers { ctrl: true, shift: true, ..Default::default() };
    assert!(enc(egui::Key::C, cs, false).is_empty());
    assert!(enc(egui::Key::V, cs, false).is_empty());
}

/// 回归：从终端复制被「软换行」折断的长行时，不能凭空插入换行符。
/// 现象：一条没有换行的长命令/URL 被终端折到多屏幕行，复制粘贴出来却带了 \n，
/// 把命令拆断。根因是 selected_text 无条件在行间补 \n，没区分软换行与真实换行。
#[test]
fn selection_does_not_insert_newline_across_soft_wrap() {
    let mut t = Terminal::new();
    assert!(t.resize(10, 4)); // 10 列，方便构造折行
    // 24 个字符、中间没有任何 \n：终端会把它折成 3 个屏幕行，并给前两行置 wrapped
    t.feed(b"abcdefghijklmnopqrstuvwx");
    assert!(t.parser.screen().row_wrapped(0), "前提：第 0 行应是软换行");
    assert!(t.parser.screen().row_wrapped(1), "前提：第 1 行应是软换行");

    t.sel_anchor = Some((0, 0));
    t.sel_cursor = Some((2, 3)); // 选到第三行的 'x'
    let s = t.selected_text().unwrap();
    assert_eq!(s, "abcdefghijklmnopqrstuvwx", "软换行不该变成 \\n");
    assert!(!s.contains('\n'));
}

/// 对照：真实换行（收到 \n）仍要保留换行符，别把两条命令粘成一条。
#[test]
fn selection_keeps_newline_for_real_line_break() {
    let mut t = Terminal::new();
    assert!(t.resize(20, 4));
    t.feed(b"line-one\r\nline-two");
    assert!(!t.parser.screen().row_wrapped(0), "前提：第 0 行是真实换行、非软换行");

    t.sel_anchor = Some((0, 0));
    t.sel_cursor = Some((1, 7));
    assert_eq!(t.selected_text().unwrap(), "line-one\nline-two");
}

/// 回归：选区锚定**内容**（绝对历史行）——本地滚动与远端新输出后，
/// 复制到的仍是当初选中的那几行。此前选区存视图坐标：滚动后高亮停在
/// 屏幕原位、复制到的是位移后的其他行（需重新选择）。
#[test]
fn selection_follows_content_across_scroll_and_new_output() {
    let mut t = Terminal::new();
    assert!(t.resize(10, 4));
    // 10 条真实行：前几行被推入历史
    t.feed(b"L0\r\nL1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9");
    assert!(t.parser.screen().scrollback_total() > 0, "前提：已有历史行");

    // L3 已推入历史：先滚到最旧历史处，在视图中找到它并换算成绝对行
    t.parser.screen_mut().set_scrollback(usize::MAX);
    t.scrollback = t.parser.screen().scrollback();
    let mut row_l3 = None;
    for r in 0..4 {
        let s = t.parser.screen();
        if s.cell(r, 0).map(|c| c.contents()) == Some("L")
            && s.cell(r, 1).map(|c| c.contents()) == Some("3")
        {
            row_l3 = Some(r);
            break;
        }
    }
    let abs = t.abs_of_view(row_l3.expect("L3 应在历史视图中"));
    // 回到底部（模拟用户选完后的常态）
    t.parser.screen_mut().set_scrollback(0);
    t.scrollback = 0;
    t.sel_anchor = Some((abs, 0));
    t.sel_cursor = Some((abs, 9));

    // 1) 用户上滚查看历史（scrollback 偏移变化）→ 复制内容不变
    t.parser.screen_mut().set_scrollback(2);
    t.scrollback = 2;
    assert_eq!(t.selected_text().unwrap(), "L3");

    // 2) 远端有新输出（历史行数增长、内容上滚）→ 复制内容仍不变
    t.feed(b"\r\nNEW1\r\nNEW2");
    assert_eq!(t.selected_text().unwrap(), "L3");

    // 3) 回到底部后依然不变
    t.parser.screen_mut().set_scrollback(0);
    t.scrollback = 0;
    assert_eq!(t.selected_text().unwrap(), "L3");
}

/// Shift+Home/↑ 键盘选区：首次以终端光标为锚，之后垂直扩展，复制内容随之增长。
#[test]
fn shift_select_via_keyboard() {
    let mut t = Terminal::new();
    assert!(t.resize(20, 4));
    t.feed(b"cmd\r\nout1\r\nout2\r\nout3$ ");
    assert_eq!(t.parser.screen().cursor_position().0, 3, "前提：光标在末行");

    // Shift+Home：以光标（末行）为锚、游标移到行首 → 选中整行
    t.shift_select(egui::Key::Home);
    assert!(t.has_selection());
    assert_eq!(t.selected_text().unwrap(), "out3$");

    // Shift+↑：锚不动、游标逐行上移，选区向上一行行扩展
    t.shift_select(egui::Key::ArrowUp);
    assert_eq!(t.selected_text().unwrap(), "out2\nout3$");
    t.shift_select(egui::Key::ArrowUp);
    assert_eq!(t.selected_text().unwrap(), "out1\nout2\nout3$");
}

/// OSC 9 / OSC 777 通知序列 → 产出待上报通知（AI CLI 通知 hook 的载体）。
#[test]
fn osc_notify_produces_notices() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]9;build finished\x07"); // BEL 终止
    t.feed(b"\x1b]777;notify;Claude Code;Task complete\x1b\\"); // ST 终止
    t.feed(b"\x1b]9;\x07"); // 空内容：忽略
    let ns = t.take_notices();
    assert_eq!(ns.len(), 2);
    assert_eq!(ns[0].title, None);
    assert_eq!(ns[0].body, "build finished");
    assert_eq!(ns[1].title.as_deref(), Some("Claude Code"));
    assert_eq!(ns[1].body, "Task complete");
    assert!(t.take_notices().is_empty(), "取走后应清空");
}

/// 在本标签里"跑过 AI CLI"：走真实路径（敲命令 + 回车），不直接改字段。
fn run_ai_cli(t: &mut Terminal, cmd: &str) {
    t.input_line = cmd.to_string();
    t.commit_line();
}

/// BEL 响铃 → 生成通知，预览取光标所在行的提示文本（确认菜单常见于此）。
#[test]
fn bell_notice_previews_cursor_line() {
    let mut t = Terminal::new();
    run_ai_cli(&mut t, "claude");
    t.feed(b"1. Yes  2. No\x07");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0].title, None);
    assert_eq!(ns[0].body, "1. Yes  2. No");
}

/// 普通 shell 标签里的 BEL **不该**产生通知。
///
/// 裸 BEL 和补全失败、readline 报错发的是同一个字节——不加这道门，每个标签都会不停弹提醒，
/// 这正是这个功能此前难用的根因。Claude Code 在 Linux 上只发裸 BEL（不发 OSC 9），所以门
/// 不能简单地"只认 OSC"，只能靠"本标签跑过 AI CLI"来分辨。
#[test]
fn bell_in_a_plain_shell_tab_is_not_a_notice() {
    let mut t = Terminal::new();
    run_ai_cli(&mut t, "ls -la"); // 普通命令，不该开门
    t.feed(b"bash: no match for glob\x07"); // 字节串字面量只能是 ASCII
    assert!(
        t.take_notices().is_empty(),
        "普通 shell 标签的响铃被当成了通知"
    );
}

/// OSC 9/777 **不受**上面那道门限制：发这个序列本身就是程序在明确要求提醒用户。
/// 且它会顺带把本标签标记为 AI 标签，此后该程序的裸 BEL 也算数。
#[test]
fn osc_notify_needs_no_ai_gate_and_opens_it() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]9;task done\x07");
    assert_eq!(t.take_notices().len(), 1, "OSC 通知不该被 AI 门拦下");
    t.feed(b"continue? [y/N]\x07");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1, "发过 OSC 通知的程序，其后续 BEL 也该算数");
    assert_eq!(ns[0].body, "continue? [y/N]");
}

/// `OSC 9;4;<state>;<pct>` 是 ConEmu / Windows Terminal 的**进度条**协议（cargo、ripgrep、
/// winget 都在发，结束时还补一条 `9;4;0;0` 清零），不是通知。
///
/// 放行它有两层后果，第二层才是真正难受的：弹一条正文是 `4;1;50` 的怪通知只是烦；更要命
/// 的是它会顺带把本标签的 `ai_cli_seen` 打开，此后这个标签的**每一次裸 BEL**（shell 补全
/// 失败、readline 报错发的是同一个字节）都会弹提醒——在一个普通 shell 里跑一次
/// `cargo build` 就踩到了。这正是那道 AI 门本来要防的事。
#[test]
fn conemu_progress_is_not_a_notification_and_does_not_arm_bell_alerts() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]9;4;1;50\x07");
    t.feed(b"\x1b]9;4;0;0\x07"); // 结束时的清零上报
    assert!(t.take_notices().is_empty(), "进度上报不该产生任何通知");
    assert!(!t.ai_cli_seen, "进度上报不该把本标签标记成「跑过 AI CLI」");
    // 门必须还关着：没跑过 AI CLI 的标签，裸 BEL 不提醒
    t.feed(b"1. Yes  2. No\x07");
    assert!(
        t.take_notices().is_empty(),
        "进度上报之后，裸 BEL 仍应被 AI 门拦住"
    );
}

/// 但真正的 OSC 9 通知一条都不能被误伤——包括正文恰好以数字开头的。判据只认 `4;`
/// 这一个子命令，不是「首段是数字就跳过」。
#[test]
fn real_osc9_notifications_are_not_mistaken_for_progress() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]9;build finished\x07");
    t.feed(b"\x1b]9;404 not found\x07"); // 数字开头，但不是 `4;` 子命令
    t.feed(b"\x1b]9;42\x07"); // 纯数字、没有分号
    let ns = t.take_notices();
    assert_eq!(ns.len(), 3, "普通 OSC 9 通知不得被进度判据误伤");
    assert_eq!(ns[0].body, "build finished");
    assert_eq!(ns[1].body, "404 not found");
    assert_eq!(ns[2].body, "42");
}

/// codex 在 tmux 下发的是 DCS 透传变体：`ESC P tmux ; ESC ESC ] 9 ; <msg> BEL ESC \`。
///
/// 两件事都要对：内容要解析出来（双写的 ESC 让 OSC 扫描器在第二个 ESC 上对上 `ESC ]`），
/// 而且**只能弹一条**——里面那个 BEL 是 OSC 的终止符，不是响铃。`count_bel` 起初不认得
/// DCS，把它算成真响铃，同一条通知弹了两遍。
#[test]
fn codex_tmux_passthrough_notification_is_parsed_once() {
    let mut t = Terminal::new();
    t.feed(b"\x1bPtmux;\x1b\x1b]9;codex done\x07\x1b\\");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1, "tmux 透传的 OSC 9 应当且只当一条通知");
    assert_eq!(ns[0].body, "codex done");
}

/// DCS 内部的 BEL 不是响铃：哪怕本标签已是 AI 标签（BEL 门是开的），也不能因为一段
/// DCS 里夹了 0x07 就弹提醒。
#[test]
fn bel_inside_a_dcs_string_is_not_a_bell() {
    let mut t = Terminal::new();
    run_ai_cli(&mut t, "claude");
    t.feed(b"\x1bPsome\x07payload\x1b\\");
    assert!(
        t.take_notices().is_empty(),
        "DCS 内部的 0x07 被当成了响铃"
    );
}

/// 空闲的 zsh 提示符**不是**「忙」。
///
/// zsh 的 zle 每进一次行编辑就发 `smkx`（含 DECCKM `\e[?1h`）、接受命令行时才发 `rmkx`
/// 复位——实测序列 `?1h ?2004h` → `?1l ?2004l`。DECCKM 曾被当作 `appears_busy` 的"忙"信号，
/// 于是对 zsh 用户语义完全反了：空闲判忙、真跑命令判闲，MCP 自动配对因此永久静默失效。
/// 这条测试把语义钉住，防止 DECCKM 被当成全屏程序信号加回来。
#[test]
fn idle_zsh_prompt_is_not_busy() {
    let mut t = Terminal::new();
    // zsh 进入 zle：应用光标 + 括号粘贴，正是空闲提示符的状态
    t.feed(b"\x1b[?1h\x1b=\x1b[?2004h user@host:~$ ");
    // feed 会置 last_output_at，而「1s 内有输出」本身就算忙——清掉它，才测得到屏幕模式那条
    // 规则（否则这条测试无论 DECCKM 算不算忙都会挂，验不出任何东西）。
    t.last_output_at = None;
    assert!(
        !t.appears_busy(),
        "空闲 zsh 提示符被判成忙 —— DECCKM 又被当成忙信号了"
    );
}

/// 反向确认没留检测缺口：全屏程序（备用屏）和鼠标上报仍然算忙。
/// 同样清掉 `last_output_at`，确保判定来自屏幕模式而不是「刚有输出」。
#[test]
fn fullscreen_and_mouse_reporting_still_count_as_busy() {
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1049h"); // 备用屏：vim/htop/tmux
    t.last_output_at = None;
    assert!(t.appears_busy(), "备用屏必须算忙");

    let mut t2 = Terminal::new();
    t2.feed(b"\x1b[?1000h"); // 鼠标上报
    t2.last_output_at = None;
    assert!(t2.appears_busy(), "鼠标上报必须算忙");
}

/// iShell 自己装的 hook 用 OSC 777 的标题位带类别标记：`ishell:done` = 任务完成，
/// 其余一律按「需要人干涉」。这条分类是设置里「仅需要我处理时」那一档的唯一依据——
/// 判错就等于要么漏掉等你确认的提示，要么每轮都被完成提醒打断。
#[test]
fn ishell_tagged_notices_are_classified_and_tag_is_hidden() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]777;notify;ishell:done;task finished\x07");
    t.feed(b"\x1b]777;notify;ishell:need;needs your confirmation\x07");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 2);
    assert_eq!(ns[0].kind, NoticeKind::Done, "ishell:done 应判为「任务完成」");
    assert_eq!(ns[1].kind, NoticeKind::Need, "ishell:need 应判为「需要人干涉」");
    // 标记是内部用的，不能漏进界面文字里
    for n in &ns {
        assert_eq!(n.title, None, "类别标记应被剥掉，不该当成标题显示");
        assert!(!n.body.contains("ishell:"));
    }
}

/// 无标记的来源要和「有标记」区分开：裸响铃是 Bell（App 层永不弹），第三方主动发的
/// OSC 通知是 Untagged（照弹，只是分不出档）。两者都不能被误判成 Done 而被分档过滤掉。
#[test]
fn unclassified_sources_keep_their_own_kind() {
    let mut t = Terminal::new();
    run_ai_cli(&mut t, "claude");
    t.feed(b"continue? [y/N]\x07");           // 裸 BEL
    t.feed(b"\x1b]9;codex done\x07");         // 第三方 OSC 9,无标记
    t.feed(b"\x1b]777;notify;MyTool;hi\x07"); // 别人的 OSC 777,标题不是 iShell 标记
    let ns = t.take_notices();
    assert_eq!(ns.len(), 3);
    assert_eq!(ns[0].kind, NoticeKind::Bell, "裸响铃必须能被单独认出来并滤掉");
    assert_eq!(ns[1].kind, NoticeKind::Untagged);
    assert_eq!(ns[2].kind, NoticeKind::Untagged);
    // 别人的标题要原样保留（只有 iShell 自己的标记才剥）
    assert_eq!(ns[2].title.as_deref(), Some("MyTool"));
}

/// SSH 是按包喂进来的，一条转义序列完全可能横跨两次 `feed`。
///
/// 切断时若不做跨块拼接会**同时**错两件事：通知本身丢掉（找不到终止符就跳过），而下一块
/// 开头那半截被当成普通文本扫描，给 OSC 收尾的 BEL 就成了"真响铃"——通知没弹，反倒多出
/// 一条内容不对的响铃提醒。和 tmux 那个 DCS bug 是同一类。
#[test]
fn osc_notification_split_across_feeds_is_recovered() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]9;split noti");
    assert!(t.take_notices().is_empty(), "半条序列不该产出任何东西");
    t.feed(b"fication\x07");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1, "拼回来后应当且只当一条通知");
    assert_eq!(ns[0].body, "split notification");
}

/// 切在 `ESC` 和 `]` 之间（最刁钻的位置）也要能拼回来。
#[test]
fn osc_split_between_esc_and_bracket_is_recovered() {
    let mut t = Terminal::new();
    t.feed(b"hello\x1b");
    t.feed(b"]9;after esc\x07");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0].body, "after esc");
}

/// tmux 的 DCS 透传被切断时同样要拼回来，且仍然只弹一条。
#[test]
fn dcs_passthrough_split_across_feeds_is_parsed_once() {
    let mut t = Terminal::new();
    t.feed(b"\x1bPtmux;\x1b\x1b]9;codex");
    t.feed(b" done\x07\x1b\\");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1, "被切断的 DCS 透传应当且只当一条通知");
    assert_eq!(ns[0].body, "codex done");
}

/// 拼接不能把已经数过的 BEL 再数一遍：完整的一块之后紧跟一个真响铃，只能算一条。
#[test]
fn carryover_does_not_double_count_bells() {
    let mut t = Terminal::new();
    run_ai_cli(&mut t, "claude");
    t.feed(b"\x1b]9;first\x07");
    assert_eq!(t.take_notices().len(), 1);
    t.feed(b"plain \x07");
    assert_eq!(t.take_notices().len(), 1, "第二块里只有一个真响铃");
}

/// 未终止的序列不能让暂存无限增长：超过上限就整段丢弃，从干净状态重扫。
#[test]
fn unterminated_sequence_tail_is_capped() {
    let mut t = Terminal::new();
    run_ai_cli(&mut t, "claude");
    t.feed(b"\x1b]9;");
    // 远超 8KB 的垃圾内容且始终不终止
    for _ in 0..12 {
        t.feed(&vec![b'x'; 1024]);
    }
    let _ = t.take_notices();
    // 暂存已被丢弃 → 这一块是干净的一条完整通知，不会被前面那堆垃圾污染
    t.feed(b"\x1b]9;fresh\x07");
    let ns = t.take_notices();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0].body, "fresh");
}

// ─────────────────────────── 分包鲁棒性（属性式，无新依赖） ───────────────────────────
//
// `feed()` 是按 SSH 通道包调用的，一条转义序列完全可能被切在任意两个字节之间。它内部为此
// 维护了**四套**跨调用状态：`utf8_pending`（半个多字节字符）、`query_tail`（半个 DSR 查询）、
// `notice_tail`（未终止的 OSC/DCS）、`echo_*`（注入命令的回显吞除）。这类 bug 历史上出过好几次
// （通知丢失、BEL 误计、tmux DCS 透传弹两遍），而且都不是崩溃，是**静默算错**。
//
// 这里用「同一段字节，整块喂 vs 在每一个可能的位置切两半喂」做对拍。不引 proptest/fuzz：
// 手写的确定性遍历在 CI 里稳定可复现，也和这个文件既有的写法一致。

/// 一段刻意混装的语料：CJK（多字节）、CSI、SGR、OSC 7、OSC 9 通知、DCS(tmux 透传)、
/// 裸 BEL、DSR 查询、以及被截断的尾巴。
const SPLIT_CORPUS: &[&[u8]] = &[
    b"hello \xe4\xb8\xad\xe6\x96\x87 world\r\n",
    b"\x1b[31mred\x1b[0m normal\r\n",
    b"\x1b]7;file://h/tmp/\xe4\xb8\xad\x07",
    b"\x1b]9;task done\x07",
    b"\x1b]777;notify;Title;Body\x07",
    b"\x1bPtmux;\x1b\x1b]9;codex done\x07\x1b\\",
    b"prompt$ \x07",
    b"\x1b[6n",
    b"\x1b[2J\x1b[3Jcleared\r\n",
    b"\x1b]9;4;1;50\x07progress\r\n",
];

/// 只取**能真正弹给用户**的通知。
///
/// 刻意滤掉 `NoticeKind::Bell`：它在 App 层被 `notice_should_alert` 的 `(K::Bell, _) => false`
/// 无条件挡掉，从来不会呈现给用户；而且它的正文是「`process` 完整包之后的光标行预览」——
/// 按设计就依赖包的形状（`feed` 里那句注释「预览在 process 之后取」说的就是这个），
/// 同一段字节切在不同位置，光标行本来就可能落在不同内容上。拿它去要求分包不变性，
/// 是在给一份既不可见、又按定义与分包有关的数据立规矩。
///
/// 剩下的三类（Need / Done / Untagged）才是会变成桌面通知的，它们必须与分包无关。
fn visible_notices(t: &mut Terminal) -> Vec<(Option<String>, String)> {
    t.take_notices()
        .into_iter()
        .filter(|n| n.kind != NoticeKind::Bell)
        .map(|n| (n.title, n.body))
        .collect()
}

/// 一次 feed 的可观测结果：(屏幕文本, 会弹给用户的通知, 要回给远端的应答字节)
type FeedOutcome = (String, Vec<(Option<String>, String)>, Vec<u8>);

fn feed_whole(data: &[u8]) -> FeedOutcome {
    let mut t = Terminal::new();
    let replies = t.feed(data);
    let notices = visible_notices(&mut t);
    (t.screen_text(), notices, replies)
}

fn feed_split_at(data: &[u8], at: usize) -> FeedOutcome {
    let mut t = Terminal::new();
    let mut replies = t.feed(&data[..at]);
    replies.extend(t.feed(&data[at..]));
    let notices = visible_notices(&mut t);
    (t.screen_text(), notices, replies)
}

/// **核心不变量**：在任意一个字节位置把输入切成两包，屏幕内容、通知、以及要回给远端的
/// 应答字节，都必须和整块喂进去完全一致。
///
/// 切在哪里是网络决定的，用户不该因为「这一包恰好断在 `ESC ]` 中间」就少收到一条通知、
/// 或者多响一次铃。
#[test]
fn feeding_the_same_bytes_split_anywhere_gives_the_same_result() {
    for (ci, chunk) in SPLIT_CORPUS.iter().enumerate() {
        let want = feed_whole(chunk);
        for at in 1..chunk.len() {
            let got = feed_split_at(chunk, at);
            assert_eq!(
                got.0, want.0,
                "语料 #{ci} 在第 {at} 字节切开后，屏幕内容不一致"
            );
            assert_eq!(
                got.1, want.1,
                "语料 #{ci} 在第 {at} 字节切开后，通知不一致"
            );
            assert_eq!(
                got.2, want.2,
                "语料 #{ci} 在第 {at} 字节切开后，回给远端的应答不一致"
            );
        }
    }
}

/// 整段语料串起来再做同样的对拍——跨语料的边界（一条序列结束、下一条开始）也要覆盖到。
#[test]
fn split_invariance_holds_across_the_whole_corpus() {
    let all: Vec<u8> = SPLIT_CORPUS.concat();
    let want = feed_whole(&all);
    // 全长逐字节切太慢，按步长扫（步长与语料长度互质，保证扫过各种相对位置）
    let step = 7;
    let mut at = 1;
    while at < all.len() {
        let got = feed_split_at(&all, at);
        assert_eq!(got.0, want.0, "整段语料在第 {at} 字节切开后屏幕不一致");
        assert_eq!(got.1, want.1, "整段语料在第 {at} 字节切开后通知不一致");
        at += step;
    }
}

/// **绝不 panic**：任意字节序列（含非法 UTF-8、孤立 ESC、超长参数、嵌套转义）喂进去，
/// 无论怎么切包都不能把应用带崩。终端喂的是远端来的不可信字节，崩了就是被一段输出打死。
#[test]
fn feed_never_panics_on_arbitrary_bytes() {
    let nasty: &[&[u8]] = &[
        b"\xff\xfe\xfd",                       // 非法 UTF-8
        b"\xe4\xb8",                           // 半个 CJK 字符
        b"\x1b",                               // 孤立 ESC
        b"\x1b[",                              // 半截 CSI
        b"\x1b]",                              // 半截 OSC
        b"\x1bP",                              // 半截 DCS
        b"\x1b[999999999999999999999m",        // 超大参数
        b"\x1b]9;",                            // OSC 9 无正文无终止
        b"\x1b]\x1b]\x1b]\x07",                // 嵌套/重复 OSC 引导
        b"\x00\x01\x02\x07\x08\x0b\x0c\x0e\x0f", // 控制字符大杂烩
        b"\x1b]7;file://\x07",                 // OSC 7 空路径
        b"\x1b]7;file://h\x07",                // OSC 7 无 '/' 路径
        b"\x1b]777;notify;\x07",               // 空标题空正文
    ];
    for (i, data) in nasty.iter().enumerate() {
        for at in 0..=data.len() {
            let mut t = Terminal::new();
            let _ = t.feed(&data[..at]);
            let _ = t.feed(&data[at..]);
            let _ = t.take_notices();
            let _ = t.screen_text();
            let _ = t.history_text(50);
            // 再补一刀：把同样的字节倒着喂一遍，制造更奇怪的状态机路径
            let mut t2 = Terminal::new();
            for b in data.iter().rev() {
                let _ = t2.feed(&[*b]);
            }
            let _ = t2.screen_text();
            assert!(i < nasty.len()); // 走到这里就算过（没 panic）
        }
    }
}

/// 逐字节喂（最极端的分包）也不能崩，且屏幕上的可见文本要和整块喂一致。
/// 这里只比屏幕：逐字节下每个字节都是一次「未终止序列」，`notice_tail` 会反复搬运整段，
/// 通知的时机与整块喂天然不同，不是这条测试要管的事。
#[test]
fn byte_by_byte_feeding_still_renders_the_same_text() {
    for (ci, chunk) in SPLIT_CORPUS.iter().enumerate() {
        let want = feed_whole(chunk).0;
        let mut t = Terminal::new();
        for b in chunk.iter() {
            let _ = t.feed(&[*b]);
        }
        assert_eq!(t.screen_text(), want, "语料 #{ci} 逐字节喂出来的屏幕不一致");
    }
}

/// 粘贴必须按远端的 bracketed paste 状态套括号。
///
/// 不套的后果是**静默且严重**的：多行内容会被 shell / TUI 当成一行行**敲进去**——每个换行
/// 都是一次回车。粘一段脚本进去等于逐行执行了它；粘一段多行提示词给 Claude Code 之类的
/// TUI，会被拆成好几次提交。开了 `CSI ?2004h` 的程序（bash 5、zsh、ipython、几乎所有
/// Ink/TUI 应用）正是靠这对括号把「粘贴」和「键入」区分开的。
#[test]
fn paste_is_bracketed_only_when_the_far_side_asked_for_it() {
    let mut t = Terminal::new();
    // 远端没开：原样发出，不加任何东西
    assert_eq!(t.wrap_paste(b"a\nb"), b"a\nb".to_vec());

    // 远端开启 bracketed paste
    let _ = t.feed(b"\x1b[?2004h");
    assert_eq!(
        t.wrap_paste(b"a\nb"),
        b"\x1b[200~a\nb\x1b[201~".to_vec(),
        "开了 bracketed paste 却没套括号：多行粘贴会被逐行当成回车敲进去"
    );

    // 关掉之后又回到原样
    let _ = t.feed(b"\x1b[?2004l");
    assert_eq!(t.wrap_paste(b"a\nb"), b"a\nb".to_vec());
}

/// 粘贴内容里自带的结束标记必须剔除。
///
/// 留着的话，被粘贴的文本可以**自己把括号关掉**，让它后半段重新变成「键入」——这是
/// bracketed paste 众所周知的注入面：一段看起来无害的文本里藏一个 `ESC[201~`，
/// 后面跟的命令就会被当成用户亲手敲的。
#[test]
fn a_paste_cannot_close_its_own_bracket() {
    let mut t = Terminal::new();
    let _ = t.feed(b"\x1b[?2004h");
    let out = t.wrap_paste(b"safe\x1b[201~rm -rf /\n");
    assert_eq!(
        out,
        b"\x1b[200~saferm -rf /\n\x1b[201~".to_vec(),
        "粘贴内容里的结束标记没有被剔除，它能自己关掉括号"
    );
    // 整个输出里结束标记只能出现一次，且必须在最末尾
    let end = b"\x1b[201~";
    let hits = out.windows(end.len()).filter(|w| *w == end).count();
    assert_eq!(hits, 1, "结束标记出现了 {hits} 次");
    assert!(out.ends_with(end));
}

/// 在无窗口 egui 里把一批输入事件喂给终端，返回它决定发往远端的字节。
fn feed_events(t: &mut Terminal, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<u8> {
    let mut out = Vec::new();
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        events,
        ..Default::default()
    };
    let _ = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            out = t.collect_input(ui);
        });
    });
    out
}

fn key_v(pressed: bool) -> egui::Event {
    egui::Event::Key {
        key: egui::Key::V,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::default(),
    }
}

/// 「Ctrl+V 在 Claude Code 里贴图毫无反应」的回归门禁。
///
/// 机制见 `input.rs`：egui-winit 在**按下**时就把 Ctrl+V 认成粘贴命令、只读剪贴板里的
/// **文本**，读不到就 `return`——连 `Event::Key` 都不再往下发，终端彻底收不到这一下按键。
/// 我们靠「有 V 的松开、却没有 V 的按下」把它认出来。这里锁住三条：
///
/// 1. 正常的文本粘贴**不能**再补一个 0x16（否则每次粘贴都会多出一个控制字符）；
/// 2. 被吞掉的那一下**必须**补上（剪贴板里没有图时就是 0x16 本身）；
/// 3. 裸 V（没按 Ctrl）**绝不能**被误判成粘贴。
///
/// 特别注意第 3 条和判据的选择：**不能**改用松开时的 `modifiers.ctrl` 来判——先松 Ctrl 还是
/// 先松 V 取决于用户手指，先松 Ctrl 时那个判据直接失效，表现就是「时灵时不灵」。
#[test]
fn a_ctrl_v_swallowed_by_egui_is_recovered_on_release() {
    let ctx = egui::Context::default();

    // 1) 剪贴板有文本：egui 在按下时给出 Paste（没有 Key 按下），随后一个 V 松开。
    let mut t = Terminal::new();
    let out = feed_events(
        &mut t,
        &ctx,
        vec![egui::Event::Paste("hello".into()), key_v(false)],
    );
    assert_eq!(out, b"hello".to_vec(), "文本粘贴之后不该再补任何字节");

    // 2) 剪贴板里没有文本（图片 / 空）：egui 什么都不发，只剩一个 V 松开。
    //    无头环境里拿不到剪贴板图片，因此走「空剪贴板」那条：补发 0x16。
    let mut t = Terminal::new();
    let out = feed_events(&mut t, &ctx, vec![key_v(false)]);
    assert_eq!(
        out,
        vec![0x16],
        "被 egui 吞掉的 Ctrl+V 没有补回来——远端程序（Claude Code 等）收不到这一下按键"
    );

    // 3) 裸 V：按下和松开都到得了，是普通输入，绝不能补 0x16。
    let mut t = Terminal::new();
    let out = feed_events(
        &mut t,
        &ctx,
        vec![key_v(true), egui::Event::Text("v".into()), key_v(false)],
    );
    assert_eq!(out, b"v".to_vec(), "普通的 V 被误判成了被吞掉的 Ctrl+V");
}

/// 松开顺序不能影响判定：先松 Ctrl 再松 V 时，松开事件里的 `modifiers.ctrl` 已经是 false，
/// 按修饰键判的实现会在这里漏掉。
#[test]
fn recovery_does_not_depend_on_which_key_is_released_first() {
    let ctx = egui::Context::default();
    let mut t = Terminal::new();
    // 用户先松开 Ctrl（修饰键归零），再松开 V
    let out = feed_events(
        &mut t,
        &ctx,
        vec![
            egui::Event::Key {
                key: egui::Key::V,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::default(), // ctrl 已经松掉了
            },
        ],
    );
    assert_eq!(out, vec![0x16], "先松 Ctrl 再松 V 时漏掉了这一下按键");
}

/// 输入法在组字**半路没了**（fcitx 崩溃/重启、远程桌面会话切换）时，`Ime(Disabled)` 永远
/// 不会来，`ime_preedit` 就会一直挂在光标处——屏幕上留着一截没提交的拼音，用户重启输入法
/// 也擦不掉，看起来像终端花了。
///
/// 自愈判据：本帧收到了普通文本输入，却一条 `Ime` 事件都没有。XIM 组字期间按键会被输入法
/// 过滤掉，能收到裸 `Text` 就说明组字已经不在了。
#[test]
fn a_dead_ime_does_not_leave_a_ghost_preedit() {
    let ctx = egui::Context::default();
    let mut t = Terminal::new();

    // 组字开始：屏幕上出现拼音
    let _ = feed_events(
        &mut t,
        &ctx,
        vec![egui::Event::Ime(egui::ImeEvent::Preedit("zhong".into()))],
    );
    assert_eq!(t.ime_preedit, "zhong", "预编辑没有被记下来，测试前提不成立");

    // 输入法这时候没了：没有 Disabled，用户接着敲了个普通字母
    let out = feed_events(&mut t, &ctx, vec![egui::Event::Text("a".into())]);
    assert_eq!(out, b"a".to_vec(), "普通输入本身必须照常发出去");
    assert!(
        t.ime_preedit.is_empty(),
        "输入法没了之后那截没提交的拼音还留在屏幕上，用户重启输入法也擦不掉"
    );
}

/// 自愈不能误伤正常组字：Preedit 之后紧跟 Commit 是最常见的路径，中间那一帧有 Ime 事件，
/// 判据（「有裸 Text 且完全没有 Ime 事件」）不成立，组字必须原样走完。
#[test]
fn self_healing_does_not_cancel_a_normal_composition() {
    let ctx = egui::Context::default();
    let mut t = Terminal::new();
    let _ = feed_events(
        &mut t,
        &ctx,
        vec![egui::Event::Ime(egui::ImeEvent::Preedit("zh".into()))],
    );
    // 同一帧里既有 Preedit 又有 Text（某些输入法会这样）——有 Ime 事件就不许清
    let _ = feed_events(
        &mut t,
        &ctx,
        vec![
            egui::Event::Ime(egui::ImeEvent::Preedit("zhong".into())),
            egui::Event::Text("x".into()),
        ],
    );
    assert_eq!(t.ime_preedit, "zhong", "有 Ime 事件的那一帧不该触发自愈");

    // 正常提交
    let out = feed_events(
        &mut t,
        &ctx,
        vec![egui::Event::Ime(egui::ImeEvent::Commit("中".into()))],
    );
    assert_eq!(out, "中".as_bytes().to_vec());
    assert!(t.ime_preedit.is_empty(), "提交之后预编辑应当清空");
}

/// 终端侧同一条不变量：关掉「候选框跟随光标」之后，上报给输入法的坐标必须与光标位置
/// **完全无关**。winit 只在坐标真的变了时才发 `XSetICValues`（同步 XIM 请求，Xlib 无超时地
/// 等输入法回复），恒定上报 = 一条都不发 = 画界面的线程不可能卡在 `_XimRead` 里。
/// 用户抓到的栈正是停在 `set_spot → XSetICValues → _XimRead → poll(timeout=-1)`。
#[test]
fn terminal_ime_spot_is_constant_when_following_is_off() {
    use super::ui_paint::ime_rect;
    let area = egui::Rect::from_min_size(egui::pos2(4.0, 8.0), egui::vec2(600.0, 400.0));
    let cell = egui::vec2(8.0, 16.0);
    let a = ime_rect(false, egui::pos2(100.0, 100.0), area, cell);
    let b = ime_rect(false, egui::pos2(500.0, 300.0), area, cell);
    assert_eq!(a, b, "关掉跟随后坐标仍随光标变——那条会冻住界面的 XSetICValues 还是会发");
    assert!(area.contains_rect(a));

    // 开着的时候必须真的跟随
    let c = ime_rect(true, egui::pos2(100.0, 100.0), area, cell);
    let d = ime_rect(true, egui::pos2(500.0, 300.0), area, cell);
    assert_ne!(c, d, "开着跟随却不动，候选框永远停在一个地方");
}

/// **本次连接以来一个字节都没收到 ≠ shell 闲在提示符上。**
///
/// 这两条判据是「程序替用户敲键盘」的安全前提。`do_reconnect` 会 `Terminal::new()`，
/// 而 `WorkerEvent::Connected`（「SSH 连上了」，不是「shell 在提示符上等着」）在同一帧
/// 被排空——那一刻 MOTD 还差一个网络往返。若把「还没收到过输出」读成「已经静止」，
/// 注入就必然落在最不该落的那一帧。
///
/// 反向对照：把 `output_idle_for` 改回 `is_none_or`，第一条断言当场挂。
#[test]
fn a_connection_with_no_output_yet_is_not_idle() {
    let mut t = Terminal::new();
    let d = std::time::Duration::from_millis(1);
    assert!(!t.output_idle_for(d), "还没见过这个 shell，不能算静止");
    assert!(!t.appears_busy(), "也不该算成忙——它只是还没说话");
    t.feed(b"user@host:~$ ");
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(t.output_idle_for(d), "收到输出并静止之后才算闲");
}

/// **刚替用户敲过一行，就不能马上再敲第二行。** `expect_echo` 是整体覆写：第二次武装会把
/// 第一条命令没吞完的回显状态冲掉，那条命令就原样留在屏幕上。
///
/// 只挡「同一帧」不够——回显要走一个远端往返才回来，而注入走 `cmd_tx`，既不更新
/// `last_output_at` 也不更新 `last_input_at`，下一帧（重绘心跳 150/200ms）那几道「静止」
/// 判据仍然全部放行。所以这里用的是时间窗，不是帧。
///
/// 反向对照：把 `injection_idle_for` 改成恒真，第二条断言当场挂。
#[test]
fn a_fresh_injection_blocks_the_next_one() {
    let mut t = Terminal::new();
    let d = std::time::Duration::from_millis(50);
    assert!(t.injection_idle_for(d), "从没注入过：不挡");
    let armed = std::time::Instant::now();
    t.expect_echo("cd '/tmp'");
    // 只在「确实是刚武装完」时断言：并行跑测试时本线程可能在这两句之间被挤掉超过 d，
    // 那是调度噪声不是回归。门禁宁可少断言一次，也不能偶发挂。
    if armed.elapsed() < d / 4 {
        assert!(!t.injection_idle_for(d), "刚注入完：挡住下一条");
    }
    std::thread::sleep(std::time::Duration::from_millis(60));
    assert!(t.injection_idle_for(d), "过了时间窗才放行");
}
