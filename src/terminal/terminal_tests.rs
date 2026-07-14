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
