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

/// OSC 52 剪贴板：TUI 程序（opencode/nvim/tmux）写系统剪贴板的标准通道，远端程序唯一
/// 通道。选择器只认 `c`；`?` 查询不回读（防剪贴板内容被远端一问一答骗走）；空负载 =
/// 清剪贴板；base64 剥空白容错（tmux 透传常折行）。反向对照：把 `?` 的 continue 去掉、
/// 把空白过滤去掉，中间两条断言当场挂。
#[test]
fn osc52_parsing() {
    // aGVsbG8= = "hello"
    assert_eq!(
        osc::parse_osc52(b"\x1b]52;c;aGVsbG8=\x07", 0),
        vec!["hello".to_string()]
    );
    // ST 终止符同样认
    assert_eq!(
        osc::parse_osc52(b"\x1b]52;c;aGVsbG8=\x1b\\", 0),
        vec!["hello".to_string()]
    );
    // 查询不回读；非 c 选择器不认；空负载 = 清剪贴板
    assert!(osc::parse_osc52(b"\x1b]52;c;?\x07", 0).is_empty());
    assert!(osc::parse_osc52(b"\x1b]52;s;aGVsbG8=\x07", 0).is_empty());
    assert_eq!(osc::parse_osc52(b"\x1b]52;c;\x07", 0), vec![String::new()]);
    // 负载空白剥掉再解；解不开的跳过不波及其它
    assert_eq!(
        osc::parse_osc52(b"\x1b]52;c;aGVs\r\n bG8=\x07", 0),
        vec!["hello".to_string()]
    );
    assert!(osc::parse_osc52(b"\x1b]52;c;!!!not-base64!!!\x07", 0).is_empty());
    // 终止符落在已扫前缀里（carried）= 上一轮已处理过，跳过
    assert!(osc::parse_osc52(b"\x1b]52;c;aGVsbG8=\x07", 20).is_empty());
}

/// OSC 133（shell 集成）：只认 `C`（开始执行）与 `D;<code>`（结束+退出码），`A`/`B` 忽略。
/// 这是 AI 命令完成检测的**首选**判据——由 shell 自己发，不经过任何程序的 stdin。
#[test]
fn osc133_parsing() {
    use osc::Osc133::*;
    assert_eq!(
        osc::parse_osc133(b"\x1b]133;C;aid=T\x07", 0, Some("T")),
        vec![(14, CommandStart)] // C 记的是序列**末尾**：它之前（含标记自己）都不算输出
    );
    assert_eq!(
        osc::parse_osc133(b"\x1b]133;D;42;aid=T\x07", 0, Some("T")),
        vec![(0, CommandEnd(Some(42)))]
    );
    // ST 终止符、以及不带退出码的 D
    assert_eq!(
        osc::parse_osc133(b"\x1b]133;D;aid=T\x1b\\", 0, Some("T")),
        vec![(0, CommandEnd(None))]
    );
    // 提示符标记与其它 OSC 一概忽略
    assert!(osc::parse_osc133(b"\x1b]133;A;aid=T\x07\x1b]7;file://h/tmp\x07", 0, Some("T")).is_empty());
    // carried：终止符落在已扫前缀里的，上一轮已处理过
    assert!(osc::parse_osc133(b"\x1b]133;C;aid=T\x07", 20, Some("T")).is_empty());
}

/// 集成模式的捕获：`C` 之前的字节（命令行回显/上一个提示符）不算输出，`D` 到达即收束。
/// 反向对照：把 `C` 的 drain 去掉，第一条断言里会混进提示符与命令行回显。
#[test]
fn integration_capture_takes_output_between_c_and_d() {
    let mut t = Terminal::new();
    t.set_integration_token("T".into());
    t.arm_ai_capture_integration();
    t.feed(b"$ echo hi\r\n\x1b]133;C;aid=T\x07hi\r\n\x1b]133;D;0;aid=T\x07$ ");
    let (code, out) = t.take_ai_done().expect("D 到达即收束");
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "hi", "只取 C 与 D 之间的输出，实际：{out:?}");
}

/// 跨块到达同样要收束：SSH 一条命令的输出天然会被拆成多个包。
#[test]
fn integration_capture_survives_chunk_splits() {
    let mut t = Terminal::new();
    t.set_integration_token("T".into());
    t.arm_ai_capture_integration();
    t.feed(b"\x1b]133;C;aid=T\x07par");
    assert!(t.take_ai_done().is_none(), "还没收到 D");
    t.feed(b"tial\r\n\x1b]13");
    t.feed(b"3;D;7;aid=T\x07");
    let (code, out) = t.take_ai_done().expect("拆包后仍要收束");
    assert_eq!(code, 7);
    assert_eq!(out.trim(), "partial");
}

/// **杂散 D 必须被忽略。** bash 对一个空行（裸回车）不展开 PS0（没有 `C`）却照常跑
/// PROMPT_COMMAND（发 `D`），而且带的是**上一条**命令的 `$?`。实测（bash 5.1，PTY）：
/// 输入 `b"\r"` → `\r\n ESC]133;D;5 BEL prompt`。认下这种 D 就是「命令还没跑就报完成、
/// 退出码还是别人的」。
/// 反向对照：去掉 `capture_progress` 里 `c_supported && !*started` 那道判据，第一条断言当场挂。
#[test]
fn integration_capture_ignores_a_d_without_its_c() {
    let mut t = Terminal::new();
    t.set_integration_token("T".into());
    t.feed(b"\x1b]133;C;aid=T\x07"); // 这个 shell 发得出 C（此后没有 C 的 D 只能是杂散的）
    t.feed(b"\x1b]133;D;0;aid=T\x07$ ");
    t.arm_ai_capture_integration();
    t.feed(b"\r\n\x1b]133;D;5;aid=T\x07$ "); // 空行产生的杂散 D
    assert!(
        t.take_ai_done().is_none(),
        "没有配对 C 的 D 不是本次运行的结束，更不该把上一条命令的退出码当成结果"
    );
    t.feed(b"real\r\n\x1b]133;C;aid=T\x07out\r\n\x1b]133;D;3;aid=T\x07$ ");
    let (code, out) = t.take_ai_done().expect("真正配对的 C/D 照常收束");
    assert_eq!(code, 3);
    assert_eq!(out.trim(), "out");
}

/// 反过来：shell **从来发不出** `C`（bash < 4.4 无 PS0）时，`D` 必须照认——否则那种远端上
/// 每条命令都会等到自动回收。代价是输出多带一段命令行回显，由调用方 trim。
#[test]
fn integration_capture_still_finishes_on_shells_that_never_send_c() {
    let mut t = Terminal::new();
    t.set_integration_token("T".into());
    t.arm_ai_capture_integration();
    t.feed(b"echo hi\r\nhi\r\n\x1b]133;D;0;aid=T\x07$ ");
    let (code, out) = t.take_ai_done().expect("发不出 C 的 shell 也要能收束");
    assert_eq!(code, 0);
    assert!(out.contains("hi"), "实际：{out:?}");
}

/// 队首等不到回显时必须被丢弃，否则它把后面排队的全堵死（见 `ECHO_ARM_TTL`）。
/// 反向对照：去掉 `strip_echo` 开头那段 TTL 清扫，断言里的 "hello" 会漏到输出上。
#[test]
fn a_head_arm_that_never_echoes_is_dropped_instead_of_blocking_the_queue() {
    let mut t = Terminal::new();
    t.expect_echo("never-echoed"); // 例：命令让 vim 进了 raw 模式，这行根本不会被回显
    t.expect_echo("hello");
    t.backdate_echo_head(std::time::Duration::from_secs(10));
    t.feed(b"hello\r\nworld\r\n");
    let screen = t.screen_text();
    assert!(
        !screen.contains("hello"),
        "队首过期后第二条武装该接上并吞掉它的回显，实际屏幕：{screen:?}"
    );
    assert!(screen.contains("world"), "真实输出不能被误吞：{screen:?}");
}

/// 部分匹配（pos>0）期间无限吞换行会把「巧合前缀 + 用户回车」黏成一行，且队列卡死。
/// 反向对照：去掉 `ECHO_PARTIAL_NEWLINE_CAP` 分支，"exp" 与 "bash" 黏在一起、换行消失。
#[test]
fn partial_echo_match_stops_swallowing_newlines_after_cap() {
    let mut t = Terminal::new();
    // 注入命令以 "export" 开头；用户恰好敲了 "exp" + 回车
    t.expect_echo("export ISHELL_PAIR_TOKEN=secret");
    t.feed(b"exp\r\n");
    t.feed(b"bash: exp: command not found\r\n");
    let screen = t.screen_text();
    assert!(screen.contains("exp"), "巧合前缀不应永久吞掉：{screen:?}");
    assert!(
        screen.contains("bash: exp: command not found") || screen.contains("command not found"),
        "真实换行后的输出行必须独立上屏：{screen:?}"
    );
    // 队列应已自愈：后续真回显仍可被吞（或至少不再卡死导致整段漏出——这里验证不 panic / 可继续喂）
    t.feed(b"more\r\n");
    assert!(t.screen_text().contains("more"));
}

/// 没有集成的会话（fish/csh、片段没注入）必须仍走哨兵回退，标志别乱置真。
#[test]
fn shell_integration_flag_only_turns_on_with_osc133() {
    let mut t = Terminal::new();
    t.set_integration_token("T".into());
    t.feed(b"$ ls\r\nfoo bar\r\n");
    assert!(!t.shell_integration_active());
    t.feed(b"\x1b]133;D;0;aid=T\x07");
    assert!(t.shell_integration_active());
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
        // shell 把敲的字回显出来了——历史只收回显过的输入（见
        // `unechoed_input_never_enters_local_history`）
        t.feed(format!("\r\n$ {cmd}").as_bytes());
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

/// 普通提示符下（有输入但还没进历史导航）按 Down：必须发 CSI B，不能吞成「按了没反应」。
/// 反向对照：`hist.is_none()` 时 `return Vec::new()`，这条挂。
#[test]
fn down_arrow_without_hist_nav_is_forwarded() {
    let mut t = Terminal::new();
    t.input_line = "partial".into();
    t.hist = None;
    assert_eq!(t.history_nav(false), b"\x1b[B");
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
    assert!(
        visible.contains("hi"),
        "真实命令输出不应被误吞：{visible:?}"
    );
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
    assert!(
        !visible.contains("printf"),
        "标记行不应出现在可见终端里：{visible:?}"
    );
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

/// DSR 5n（状态查询）与 DA（CSI c / CSI 0 c）现代 TUI 常用来探测终端能力；此前一律沉默。
#[test]
fn replies_to_dsr_status_and_device_attributes() {
    let mut t = Terminal::new();
    assert_eq!(t.feed(b"\x1b[5n"), b"\x1b[0n");
    assert_eq!(t.feed(b"\x1b[c"), b"\x1b[?1;2c");
    assert_eq!(t.feed(b"\x1b[0c"), b"\x1b[?1;2c");
    // 分包：半截 DA
    assert!(t.feed(b"\x1b[").is_empty());
    assert_eq!(t.feed(b"c"), b"\x1b[?1;2c");
}

/// DCS/OSC 负载里碰巧出现的清屏/查询字节绝不能触发重建解析器或假应答。
#[test]
fn clear_and_cpr_inside_dcs_are_ignored() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 5));
    t.feed(b"keep-me\r\n");
    // 撑满可见屏，把 keep-me 推进 scrollback——否则裸 2J（vt100 自清屏）也会抹掉它，
    // 分不清是「误触发了我们的 [3J 重建」还是「vt100 清了当前屏」。
    for _ in 0..8 {
        t.feed(b"pad\r\n");
    }
    assert!(
        t.history_text(50).contains("keep-me"),
        "前置条件：keep-me 应已在历史里"
    );
    let replies = t.feed(b"\x1bPtmux;\x1b[2J\x1b[3J\x1b[6n\x1b\\");
    assert!(replies.is_empty(), "DCS 内查询不应应答：{replies:?}");
    let hist = t.history_text(50);
    assert!(
        hist.contains("keep-me"),
        "DCS 内假 clear 清掉了历史：{hist:?}"
    );
    // 真 clear 仍生效
    t.feed(b"\x1b[2J\x1b[3Jfresh\r\n");
    let hist = t.history_text(50);
    assert!(hist.contains("fresh"), "{hist:?}");
    assert!(
        !hist.contains("keep-me"),
        "真 clear 后旧历史应消失：{hist:?}"
    );
}

/// OSC 10/11 颜色查询按当前主题回 rgb:RRRR/GGGG/BBBB。
#[test]
fn replies_to_osc_color_queries() {
    let mut t = Terminal::new();
    let r = t.feed(b"\x1b]10;?\x07\x1b]11;?\x07");
    let s = String::from_utf8_lossy(&r);
    assert!(s.contains("]10;rgb:"), "缺前景应答：{s}");
    assert!(s.contains("]11;rgb:"), "缺背景应答：{s}");
}

/// OSC 0/2 设置窗口标题；空串清除。
#[test]
fn osc_sets_and_clears_window_title() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]0;vim: main.rs\x07");
    assert_eq!(t.window_title(), Some("vim: main.rs"));
    t.feed(b"\x1b]2;\x07");
    assert_eq!(t.window_title(), None);
}

/// 搜索命中用绝对历史行锚定：scrollback 满后修剪不改 scrollback_total，命中仍指向同一内容。
#[test]
fn search_hits_survive_scrollback_trim() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 5));
    // 灌满超过默认 scrollback 上限，迫使修剪 retained 行。
    for i in 0..5200 {
        t.feed(format!("line-{i:04}\r\n").as_bytes());
    }
    t.feed(b"UNIQUE-NEEDLE\r\n");
    t.find = Some(search::Find {
        query: "UNIQUE-NEEDLE".into(),
        ..Default::default()
    });
    t.run_search();
    let f = t.find.as_ref().expect("find state");
    assert!(!f.hits.is_empty(), "应命中 UNIQUE-NEEDLE");
    let hit = f.hits[0];
    // 再推几行触发更多修剪
    for i in 0..100 {
        t.feed(format!("pad-{i}\r\n").as_bytes());
    }
    t.jump_to_current();
    let total = t.parser.screen().scrollback_total();
    let kept = t.parser.screen().scrollback_rows();
    assert!(
        hit + kept >= total,
        "绝对命中 {hit} 已漂出 retained 窗（total={total} kept={kept}）"
    );
    assert!(t.search_hl.is_some(), "跳转到命中后应有高亮行");
}

/// 进出备用屏须清本地选区，否则在 less 上拖选、退出后复制会拿到主屏陈旧内容。
#[test]
fn entering_or_leaving_alt_screen_clears_selection() {
    let mut t = Terminal::new();
    t.feed(b"main history\r\n");
    t.sel_anchor = Some((0, 0));
    t.sel_cursor = Some((0, 4));
    t.feed(b"\x1b[?1049h"); // 进备用屏
    assert!(t.sel_anchor.is_none() && t.sel_cursor.is_none());
    t.sel_anchor = Some((0, 0));
    t.sel_cursor = Some((0, 1));
    t.feed(b"\x1b[?1049l"); // 出备用屏
    assert!(t.sel_anchor.is_none() && t.sel_cursor.is_none());
}

/// DEC 1047/1048：老程序仍发这两条；此前 vendor 走 unhandled 空操作。
#[test]
fn dec_1047_and_1048_toggle_alt_and_cursor() {
    let mut t = Terminal::new();
    t.feed(b"\x1b[10;20H"); // 主屏光标
    let (r0, c0) = t.parser.screen().cursor_position();
    t.feed(b"\x1b[?1048h\x1b[?1047h"); // 存光标 + 进备用屏
    assert!(t.parser.screen().alternate_screen());
    t.feed(b"\x1b[1;1H");
    t.feed(b"\x1b[?1047l\x1b[?1048l"); // 出备用屏 + 恢复光标
    assert!(!t.parser.screen().alternate_screen());
    let (r1, c1) = t.parser.screen().cursor_position();
    assert_eq!((r1, c1), (r0, c0), "1048 应恢复进备用屏前的光标");
}

/// DECAWM（`CSI ? 7`）：关闭后行末覆盖末列，不再折行。
#[test]
fn decawm_off_does_not_wrap_at_eol() {
    let mut t = Terminal::new();
    assert!(t.resize(10, 3));
    t.feed(b"\x1b[?7l"); // 关自动折行
    t.feed(b"\x1b[1;1H");
    t.feed(b"ABCDEFGHIJKLMNOP"); // 超过 10 列
    let (row, col) = t.parser.screen().cursor_position();
    assert_eq!(row, 0, "不应折到下一行：row={row} col={col}");
    // 末列应是最后写入的字符之一
    let last = t
        .parser
        .screen()
        .cell(0, 9)
        .map(|c| c.contents())
        .unwrap_or_default();
    assert!(!last.is_empty(), "末列应有覆盖写入的字符");
}

/// 跨行搜索：软折行无分隔拼接能命中；硬换行不能把行尾和行首粘成一个词。
#[test]
fn search_matches_across_soft_wrap_only() {
    let mut t = Terminal::new();
    assert!(t.resize(8, 6));
    // 8 列软折：aaaHELLO | WORLD
    t.feed(b"aaaHELLOWORLD");
    assert!(t.parser.screen().row_wrapped(0), "前提：第 0 行应是软折行");
    t.find = Some(search::Find {
        query: "HELLOWORLD".into(),
        ..Default::default()
    });
    t.run_search();
    assert!(
        !t.find.as_ref().unwrap().hits.is_empty(),
        "软折行应命中 HELLOWORLD"
    );

    let mut hard = Terminal::new();
    assert!(hard.resize(40, 6));
    hard.feed(b"aaaHELLO\r\nWORLDbbb\r\n");
    assert!(!hard.parser.screen().row_wrapped(0));
    hard.find = Some(search::Find {
        query: "HELLOWORLD".into(),
        ..Default::default()
    });
    hard.run_search();
    assert!(
        hard.find.as_ref().unwrap().hits.is_empty(),
        "硬换行不应把两行粘成 HELLOWORLD"
    );

    let mut sh = Terminal::new();
    assert!(sh.resize(40, 6));
    sh.feed(b"ends\r\nhere\r\n");
    sh.find = Some(search::Find {
        query: "sh".into(),
        ..Default::default()
    });
    sh.run_search();
    assert!(
        sh.find.as_ref().unwrap().hits.is_empty(),
        "硬换行两侧的 s+h 不应命中 sh"
    );
}

#[test]
fn search_soft_wrap_spans_more_than_two_rows() {
    let mut t = Terminal::new();
    assert!(t.resize(4, 8));
    // 4 列：HELL / OWOR / LDXY / Z —— HELLOWORLD 跨三行
    t.feed(b"HELLOWORLDXYZ");
    assert!(t.parser.screen().row_wrapped(0));
    assert!(t.parser.screen().row_wrapped(1));
    t.find = Some(search::Find {
        query: "HELLOWORLD".into(),
        ..Default::default()
    });
    t.run_search();
    assert!(
        !t.find.as_ref().unwrap().hits.is_empty(),
        "三行软折应命中 HELLOWORLD"
    );
}

#[test]
fn search_soft_wrap_hit_can_start_on_a_later_row() {
    let mut t = Terminal::new();
    assert!(t.resize(4, 8));
    // xxxx | HELL | OWOR | LD
    t.feed(b"xxxxHELLOWORLD");
    assert!(t.parser.screen().row_wrapped(0));
    assert!(t.parser.screen().row_wrapped(1));
    t.find = Some(search::Find {
        query: "HELLOWORLD".into(),
        ..Default::default()
    });
    t.run_search();
    let hits = &t.find.as_ref().unwrap().hits;
    assert!(
        hits.iter().any(|&abs| abs >= 1),
        "匹配从第二行软折开始，命中应落在那一行：{hits:?}"
    );
}

#[test]
fn search_hard_newline_regex_can_end_on_the_break() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 6));
    t.feed(b"foo\r\nbar\r\n");
    t.find = Some(search::Find {
        query: "foo\n".into(),
        regex: true,
        ..Default::default()
    });
    t.run_search();
    assert!(
        !t.find.as_ref().unwrap().hits.is_empty(),
        "正则 foo\\n 应命中硬换行"
    );
}

#[test]
fn search_hard_newline_regex_can_continue_into_the_next_line() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 6));
    t.feed(b"foo\r\nbar\r\n");
    t.find = Some(search::Find {
        query: "foo\nbar".into(),
        regex: true,
        ..Default::default()
    });
    t.run_search();
    assert!(
        !t.find.as_ref().unwrap().hits.is_empty(),
        "正则 foo\\nbar 应跨硬换行命中"
    );
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
    let alt_shift = egui::Modifiers {
        alt: true,
        shift: true,
        ..Default::default()
    };
    assert_eq!(enc(egui::Key::B, alt_shift, false), b"\x1bB");
    // Alt+Backspace = 删除前一个词
    assert_eq!(enc(egui::Key::Backspace, ALT, false), b"\x1b\x7f");
}

/// Alt+标点此前落到空分支什么都不发；readline Meta（Alt+. 插上参等）依赖它。
#[test]
fn alt_punctuation_encodes_meta_prefix() {
    assert_eq!(enc(egui::Key::Period, ALT, false), b"\x1b.");
    assert_eq!(enc(egui::Key::Comma, ALT, false), b"\x1b,");
    assert_eq!(enc(egui::Key::Slash, ALT, false), b"\x1b/");
    assert_eq!(enc(egui::Key::Minus, ALT, false), b"\x1b-");
    let alt_shift = egui::Modifiers {
        alt: true,
        shift: true,
        ..Default::default()
    };
    assert_eq!(enc(egui::Key::Period, alt_shift, false), b"\x1b>");
    assert_eq!(enc(egui::Key::Equals, alt_shift, false), b"\x1b+");
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
    let ctrl_shift = egui::Modifiers {
        ctrl: true,
        shift: true,
        ..Default::default()
    };
    assert_eq!(enc(egui::Key::Minus, ctrl_shift, false), &[0x1f]);
    assert!(enc(egui::Key::Minus, CTRL, false).is_empty());
    // Alt+Ctrl+B -> ESC 前缀 + 控制字符
    let alt_ctrl = egui::Modifiers {
        alt: true,
        ctrl: true,
        ..Default::default()
    };
    assert_eq!(enc(egui::Key::B, alt_ctrl, false), b"\x1b\x02");
}

#[test]
fn copy_paste_shortcuts_are_not_sent_to_terminal() {
    // Ctrl+Shift+C/V/F 保留给复制/粘贴/查找，不能当终端输入发下去。
    let cs = egui::Modifiers {
        ctrl: true,
        shift: true,
        ..Default::default()
    };
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
    assert!(
        !t.parser.screen().row_wrapped(0),
        "前提：第 0 行是真实换行、非软换行"
    );

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
    assert!(t.take_notices().is_empty(), "DCS 内部的 0x07 被当成了响铃");
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
    assert_eq!(
        ns[0].kind,
        NoticeKind::Done,
        "ishell:done 应判为「任务完成」"
    );
    assert_eq!(
        ns[1].kind,
        NoticeKind::Need,
        "ishell:need 应判为「需要人干涉」"
    );
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
    t.feed(b"continue? [y/N]\x07"); // 裸 BEL
    t.feed(b"\x1b]9;codex done\x07"); // 第三方 OSC 9,无标记
    t.feed(b"\x1b]777;notify;MyTool;hi\x07"); // 别人的 OSC 777,标题不是 iShell 标记
    let ns = t.take_notices();
    assert_eq!(ns.len(), 3);
    assert_eq!(
        ns[0].kind,
        NoticeKind::Bell,
        "裸响铃必须能被单独认出来并滤掉"
    );
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
    // 远超上限（1MiB）的垃圾内容且始终不终止
    for _ in 0..40 {
        t.feed(&vec![b'x'; 32 * 1024]);
    }
    assert!(t.notice_tail.len() <= 1024 * 1024, "暂存超过了上限");
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
    // ST（`ESC \`）终止的 OSC：切在 ESC 与 `\` 之间时曾整条丢失
    b"\x1b]9;st terminated\x1b\\after\r\n",
    b"\x1b]777;notify;T;st body\x1b\\",
    // 同步输出帧（DEC 私有模式 2026）：整帧攒齐才上屏，任意分包结果须一致
    b"\x1b[?2026h\x1b[1;1H\x1b[J\x1b[1;1Hframe one\r\n\x1b[?2026l",
    // 帧内又来个 2026h（畸形但容错）：一个 2026l 即结束，多余的 h 喂给 vt100 忽略
    b"\x1b[?2026h\x1b[2;1Hframe two\r\n\x1b[?2026h\x1b[3;1Hstill frame two\r\n\x1b[?2026l",
    // 落单的 2026l：不在帧内，随普通段喂给 vt100 忽略即可
    b"\x1b[?2026lstray sync-off\r\n",
    // 真实 codex 同步帧（PTY 抓包，空闲转圈时的一整帧重绘）：整屏擦除 + 逐行清空 + 逐行重建
    b"\x1b[?2026h\x1b[1;1H\x1b[J\x1b[1;42H\x1b[0m\x1b[m\x1b[K\x1b[2;42H\x1b[0m\x1b[m\x1b[K\x1b[3;42H\x1b[0m\x1b[m\x1b[K\x1b[4;42H\x1b[0m\x1b[m\x1b[K\x1b[5;42H\x1b[0m\x1b[m\x1b[K\x1b[6;42H\x1b[0m\x1b[m\x1b[K\x1b[7;2H\x1b[0m\x1b[m\x1b[K\x1b[8;2H\x1b[0m\x1b[m\x1b[K\x1b[9;27H\x1b[0m\x1b[m\x1b[K\x1b[10;2H\x1b[0m\x1b[m\x1b[K\x1b[11;18H\x1b[0m\x1b[m\x1b[K\x1b[1;1H\x1b[2m\xe2\x95\xad\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x95\xae\x1b[2;1H\xe2\x94\x82 >_ \x1b[22m\x1b[1mOpenAI Codex\x1b[22m\x1b[2m\x1b[2m (v0.154.0)            \xe2\x94\x82\x1b[3;1H\xe2\x94\x82                                       \xe2\x94\x82\x1b[4;1H\xe2\x94\x82 model:     \x1b[3mloading\x1b[23m   \x1b[22m\x1b[;m/model\x1b[2m\x1b[;m to change \xe2\x94\x82\x1b[5;1H\xe2\x94\x82 directory: \x1b[22mloading\x1b[2m                    \xe2\x94\x82\x1b[6;1H\xe2\x95\xb0\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x94\x80\xe2\x95\xaf\x1b[7;1H\x1b[22m \x1b[8;1H \x1b[9;1H\x1b[1m\xe2\x80\xba\x1b[22m \x1b[2mAsk Codex to do anything\x1b[10;1H\x1b[22m \x1b[11;1H  \x1b[2m? for shortcuts\x1b[m\x1b[m\x1b[0m\x1b[1;1H\x1b[0 q\x1b[1;1H\x1b[2m\xe2\x95\xad\x1b[m\x1b[m\x1b[0m\x1b[9;3H\x1b[?25h\x1b[?2026l",
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
            assert_eq!(got.1, want.1, "语料 #{ci} 在第 {at} 字节切开后，通知不一致");
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
        b"\xff\xfe\xfd",                         // 非法 UTF-8
        b"\xe4\xb8",                             // 半个 CJK 字符
        b"\x1b",                                 // 孤立 ESC
        b"\x1b[",                                // 半截 CSI
        b"\x1b]",                                // 半截 OSC
        b"\x1bP",                                // 半截 DCS
        b"\x1b[999999999999999999999m",          // 超大参数
        b"\x1b]9;",                              // OSC 9 无正文无终止
        b"\x1b]\x1b]\x1b]\x07",                  // 嵌套/重复 OSC 引导
        b"\x00\x01\x02\x07\x08\x0b\x0c\x0e\x0f", // 控制字符大杂烩
        b"\x1b]7;file://\x07",                   // OSC 7 空路径
        b"\x1b]7;file://h\x07",                  // OSC 7 无 '/' 路径
        b"\x1b]777;notify;\x07",                 // 空标题空正文
        b"\x1b[?2026h",                          // 未终止的同步帧（看门狗兜底）
        b"\x1b[?2026l",                          // 落单的同步结束标记
        b"\x1b[?2026h\x1b[?2026h\x1b[?2026l",    // 连续嵌套的同步标记
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

/// 1 行高终端：写满一行再多写 1 字符会触发 col_wrap → row_inc_scroll。
/// 上游 vt100 在 `prev_pos.row -= scrolled` 处对 u16 下溢 panic（debug）；面板可拖到
/// 1 行，远端任意超长输出就能把整个 UI 崩掉。反向对照：把 saturating_sub 改回 `-=` 即挂。
#[test]
fn one_row_terminal_wrap_never_panics() {
    let mut t = Terminal::new();
    assert!(t.resize(80, 1));
    // 写满 80 列再多一个字符：必定走 col_wrap
    let line = vec![b'x'; 81];
    let _ = t.feed(&line);
    let _ = t.screen_text();
    // 再灌一长串，覆盖连续折行 / scrollback 路径
    let _ = t.feed(&vec![b'y'; 200]);
    let _ = t.history_text(50);
}

/// 同上场景也必须被「任意字节绝不 panic」覆盖（1 行尺寸 + nasty 字节）。
#[test]
fn feed_never_panics_on_one_row_terminal() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 1));
    let nasty: &[&[u8]] = &[
        b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxY", // 刚好折行
        b"\xff\xfe\x1b[999m",
        b"\x1b[?2026hpartial\x1b[?2026l",
    ];
    for data in nasty {
        let _ = t.feed(data);
        let _ = t.screen_text();
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
    // 远端没开：不加括号（换行按回车键发，见
    // `unbracketed_paste_sends_newlines_as_carriage_returns`）
    assert_eq!(t.wrap_paste(b"a\nb"), b"a\rb".to_vec());

    // 远端开启 bracketed paste
    let _ = t.feed(b"\x1b[?2004h");
    assert_eq!(
        t.wrap_paste(b"a\nb"),
        b"\x1b[200~a\nb\x1b[201~".to_vec(),
        "开了 bracketed paste 却没套括号：多行粘贴会被逐行当成回车敲进去"
    );

    // 关掉之后又回到不加括号
    let _ = t.feed(b"\x1b[?2004l");
    assert_eq!(t.wrap_paste(b"a\nb"), b"a\rb".to_vec());
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
        vec![egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::default(), // ctrl 已经松掉了
        }],
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
    assert_eq!(
        a, b,
        "关掉跟随后坐标仍随光标变——那条会冻住界面的 XSetICValues 还是会发"
    );
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

/// **刚替用户敲过一行，就不能马上再敲第二行。** 两次武装挨太近，提示符上一片注入痕迹
///（吞除队列按打字顺序各吞各的回显，覆写不再发生——这道时间窗隔开的是观感与注入节奏）。
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

/// 回显吞除的排队语义：先武装的注入与后武装的哨兵**同时**挂着，两个回显按打字先后
/// 各吞各的——后一次武装不再覆盖前一次（旧行为：哨兵 expect_echo 会冲掉注入的吞除，
/// 450 字符的 OSC 7 片段回显漏进屏幕与捕获输出）。
/// 反向对照：把 `expect_echo` 改回整体覆写，第一条断言当场挂（片段回显漏上屏幕）。
#[test]
fn echo_swallow_queues_in_typing_order() {
    let mut t = Terminal::new();
    let osc7 = " __ishell_cwd(){ :; }";
    let marker = "printf '\\x1eAI_DONE_1:%d\\x1e' $?; printf '\\r\\x1b[K'";
    t.expect_auto_inject_echo(osc7);
    t.expect_echo(marker);
    // 远端回显按打字顺序回来：先 OSC 7 片段行，后标记行
    let echoed = format!("{osc7}\r\n{marker}\r\n");
    t.feed(echoed.as_bytes());
    let screen = t.parser.screen().contents();
    assert!(!screen.contains(osc7.trim()), "OSC 7 片段回显应被吞掉");
    assert!(!screen.contains("AI_DONE_1"), "哨兵回显应被吞掉");
    t.feed(b"real output\r\n");
    assert!(t.parser.screen().contents().contains("real output"));
}

/// 哨兵捕获对「未被吞掉的标记回显」免疫：捕获前缀以真实 0x1E 字节开头，而打字回显里
/// 只有字面 `\x1e` 四个字符、没有控制字节——首条命令与 MOTD 交织导致吞除失配、标记
/// 回显残留在流里时，捕获不会把回显误当哨兵（旧行为：退出码区域解析出 `%d` 垃圾，
/// 实测首条命令 exit_code=-1 而命令实际成功）。回显文本仍会作为噪声留在输出里
/// （那是吞除失配本身的代价，本测试管的是退出码不再退化）。
#[test]
fn ai_capture_ignores_literal_escape_text_in_unswallowed_echo() {
    let mut t = Terminal::new();
    // 吞除**没有**武装（模拟首条命令时吞除失配的场景）：标记行的打字回显原样留在流里
    let typed_marker = "printf '\\x1eAI_DONE_7:%d\\x1e' $?; printf '\\r\\x1b[K'";
    t.arm_ai_capture(b"\x1eAI_DONE_7:".to_vec());
    t.feed(typed_marker.as_bytes());
    t.feed(b"\r\n");
    assert!(t.take_ai_done().is_none(), "字面转义文本不构成哨兵");
    // 真实的 printf 输出：真实 0x1E 开头 + 退出码 + 真实 0x1E
    t.feed(b"\x1eAI_DONE_7:0\x1e");
    let (code, _out) = t.take_ai_done().expect("应命中真实哨兵");
    assert_eq!(code, 0, "退出码来自真实输出，不能是 -1");
}

/// 同步输出（DEC 私有模式 2026）专项测试。
///
/// 背景：kimi/codex 这类「内联视口」TUI 每帧都是「整屏擦除 + 逐行重建」，帧体包在
/// `ESC[?2026h`…`ESC[?2026l` 里、依赖终端在帧结束前不上屏。帧体本身又是几十个小
/// write，一帧几乎必然跨多个数据包。`feed()` 在 2026h 起把字节攒进 `sync_buf`、到
/// 2026l 才一次性喂给 vt100，中间态因此对任何 paint 都不可见。

/// 原子性：帧未结束（没有 `2026l`）时，帧内的清屏/绘制一律不上屏；结束后整帧一次生效。
#[test]
fn sync_frame_is_buffered_until_sync_off() {
    let mut t = Terminal::new();
    t.feed(b"old content\r\n");
    let before = t.screen_text();
    // 帧的前半：清屏 + 写了一行——但没有 2026l，屏幕上必须还是旧内容
    let replies = t.feed(b"\x1b[?2026h\x1b[1;1H\x1b[J\x1b[1;1Hnew frame");
    assert!(replies.is_empty());
    assert_eq!(t.screen_text(), before, "帧未结束时中间态上了屏");
    assert!(t.sync_active, "应处于帧内");
    // 帧的后半 + 结束标记：此刻整帧一次性上屏
    t.feed(b" continues\r\n\x1b[?2026l");
    let after = t.screen_text();
    assert!(
        after.contains("new frame continues"),
        "结束后整帧应已上屏：{after:?}"
    );
    assert!(!after.contains("old content"), "帧内清屏应随整帧生效");
    assert!(!t.sync_active, "结束后应退出帧内状态");
}

/// 对拍：codex 形态的帧（整屏擦除 + 逐行清空重建）在**任意**字节位置切成两包，
/// 第一包喂完后屏幕上都不许出现帧内容（中间态两边都不可见），两包喂完后与整块一致。
#[test]
fn codex_style_frame_is_atomic_under_any_split() {
    let frame: &[u8] = b"\x1b[?2026h\x1b[1;1H\x1b[J\x1b[1;1H\x1b[K\x1b[2;1H\x1b[K\x1b[1;1HBOX\r\n\x1b[2;1Hcontent\x1b[?2026l";
    let mut base = Terminal::new();
    base.feed(b"prompt$ ");
    base.feed(frame);
    let want = base.screen_text();
    for at in 1..frame.len() {
        let mut t = Terminal::new();
        t.feed(b"prompt$ ");
        t.feed(&frame[..at]);
        assert!(
            !t.screen_text().contains("BOX"),
            "切点 {at}：帧未完整送达，中间态却上了屏"
        );
        t.feed(&frame[at..]);
        assert_eq!(t.screen_text(), want, "切点 {at}：最终屏幕与整块不一致");
    }
}

/// 帧内的光标查询（`ESC[6n`）在整帧完成时才产生应答，且光标位置与整块喂入一致；
/// 任意切包下应答字节都必须相同（查询可能被切在 `ESC[6` / `n` 之间，靠暂存拼回）。
#[test]
fn sync_frame_answers_cursor_query_at_frame_end() {
    let frame: &[u8] = b"\x1b[?2026h\x1b[5;10H\x1b[6n\x1b[?2026l";
    let mut whole = Terminal::new();
    let want_replies = whole.feed(frame);
    assert_eq!(
        want_replies,
        b"\x1b[5;10R".to_vec(),
        "应答应是查询点的光标位置"
    );
    for at in 1..frame.len() {
        let mut t = Terminal::new();
        let mut r = t.feed(&frame[..at]);
        r.extend(t.feed(&frame[at..]));
        assert_eq!(r, want_replies, "切点 {at}：回给远端的应答不一致");
    }
}

/// 帧内嵌 `clear`（`ESC[2J ESC[3J`）照常走「重建解析器、真正清空回滚缓冲」的特例，
/// 只是时机推迟到帧结束——清的是旧历史，帧自己写的内容要留下。
#[test]
fn sync_frame_containing_clear_still_clears_scrollback() {
    let mut t = Terminal::new();
    for i in 0..30 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    assert!(
        t.history_text(100).contains("line 0"),
        "清屏前应能回看到最早的历史行"
    );
    t.feed(b"\x1b[?2026h\x1b[2J\x1b[3J\x1b[1;1Hcleared in frame\r\n\x1b[?2026l");
    let history = t.history_text(100);
    assert!(
        !history.contains("line 0"),
        "帧内 clear 后旧历史应被清空：{history:?}"
    );
    assert!(history.contains("cleared in frame"), "帧自己写的内容应留下");
}

/// 看门狗：程序崩溃在帧中间（`2026l` 永远不来）时，下一包到达会把超时半帧强刷上去，
/// 画面不能永久冻结。这里直接把帧开始时刻拨回过去来确定性触发。
#[test]
fn unterminated_sync_frame_flushes_on_watchdog() {
    let mut t = Terminal::new();
    t.feed(b"before\r\n");
    t.feed(b"\x1b[?2026h\x1b[1;1Hhalf frame"); // 无 2026l
    assert!(t.screen_text().contains("before"), "未结束帧不上屏");
    assert!(t.sync_active);
    // 模拟看门狗超时
    t.sync_since = std::time::Instant::now() - std::time::Duration::from_millis(500);
    t.feed(b"x");
    let s = t.screen_text();
    assert!(s.contains("half frame"), "看门狗应把半帧刷上去：{s:?}");
    assert!(s.contains('x'), "看门狗刷完后本次字节照常处理");
    assert!(!t.sync_active, "看门狗刷完后应复位帧内状态");
}

/// 定时路径：不必等下一包——`tick_sync_watchdog` 到期就刷。反向对照：只留 feed 入口
/// 看门狗时，远端帧中静默挂起画面永久冻结。
#[test]
fn sync_watchdog_ticks_without_next_packet() {
    let mut t = Terminal::new();
    t.feed(b"\x1b[?2026hstuck half");
    assert!(t.sync_active);
    assert!(!t.screen_text().contains("stuck"));
    t.sync_since = std::time::Instant::now() - std::time::Duration::from_millis(500);
    let _ = t.tick_sync_watchdog();
    assert!(t.screen_text().contains("stuck"));
    assert!(!t.sync_active);
    assert!(t.sync_watchdog_remaining().is_none());
}

/// 帧缓冲上限：超过 `SYNC_BUF_CAP` 强制刷掉，内存有界；刷掉后同步状态复位、
/// 仍能正常进下一帧。走「多包累积、始终没有 2026l」的 None 分支——那才是真正的
/// 上限检查路径（Some 分支是随 2026l 正常 flush，不查上限）。
#[test]
fn oversized_sync_frame_flushes_and_recovers() {
    let mut t = Terminal::new();
    let mut data = b"\x1b[?2026h".to_vec();
    data.extend(std::iter::repeat_n(b'a', super::feed::SYNC_BUF_CAP + 64));
    let replies = t.feed(&data); // 无 2026l：None 分支内命中上限
    assert!(replies.is_empty());
    assert!(t.screen_text().contains('a'));
    assert!(!t.sync_active, "超限 flush 后同步状态应复位");
    // 之后还能正常进帧
    t.feed(b"\x1b[?2026h\x1b[1;1Htail\r\n\x1b[?2026l");
    assert!(t.screen_text().contains("tail"));
    assert!(!t.sync_active);
}

/// resize 落在帧中间：攒着的半帧不清掉，之后照常灌进**新**解析器，顺序不变、不崩。
/// 两个分支都要覆盖：直接 set_size 的扩容分支（rows 变大），以及序列化重建解析器
/// 的缩窄分支（cols 变小）——后者才是「半帧灌进新解析器」的真正场景。
#[test]
fn resize_mid_sync_frame_does_not_panic() {
    // 扩容分支：rows 24 -> 40 直接 set_size
    let mut t = Terminal::new();
    t.feed(b"\x1b[?2026h\x1b[1;1Hpartial");
    assert!(t.resize(100, 40));
    assert!(t.sync_active, "resize 不应打断帧内状态");
    t.feed(b" rest\r\n\x1b[?2026l");
    assert!(t.screen_text().contains("partial rest"));
    assert!(!t.sync_active);

    // 缩窄重建分支：cols 80 -> 60 走 serialize_buffer + 重建解析器
    let mut t = Terminal::new();
    t.feed(b"before\r\n\x1b[?2026h\x1b[1;1Hpartial");
    assert!(t.resize(60, 20));
    assert!(t.sync_active, "重建解析器也不应打断帧内状态");
    t.feed(b" rest\r\n\x1b[?2026l");
    assert!(t.screen_text().contains("partial rest"));
    assert!(!t.sync_active);
}

/// 断线丢弃未完成的同步帧：半帧属于已死的会话，重连后绝不允许被看门狗刷上新屏幕。
#[test]
fn disconnect_drops_unfinished_sync_frame() {
    let mut t = Terminal::new();
    t.feed(b"alive\r\n");
    t.feed(b"\x1b[?2026h\x1b[1;1Hstale dead frame"); // 无 2026l
    assert!(t.sync_active);
    t.reset_sync(); // 断线重连时 App 调用
    assert!(!t.sync_active);
    // 重连后的新输出：旧半帧一个字都不许出现
    t.feed(b"new session\r\n");
    let s = t.screen_text();
    assert!(s.contains("new session"), "新会话输出应在屏上：{s:?}");
    assert!(!s.contains("stale dead frame"), "旧会话半帧上了屏：{s:?}");
    // 同步机制照常工作
    t.feed(b"\x1b[?2026h\x1b[1;1Hfresh frame\r\n\x1b[?2026l");
    assert!(t.screen_text().contains("fresh frame"));
}

/// 落单的 `2026l`（不在帧内）：原样喂给 vt100 忽略，后续输出不受影响。
#[test]
fn stray_sync_off_outside_frame_is_inert() {
    let mut t = Terminal::new();
    t.feed(b"\x1b[?2026lplain\r\n");
    assert!(t.screen_text().contains("plain"));
    assert!(!t.sync_active);
}

/// 连续交错：普通段 → 帧A → 普通段 → 帧B，全部一次喂入，各段效果按序生效。
#[test]
fn interleaved_plain_segments_and_frames_apply_in_order() {
    let mut t = Terminal::new();
    t.feed(
        b"one\r\n\x1b[?2026htwo-frame\r\n\x1b[?2026lthree\r\n\x1b[?2026hfour-frame\r\n\x1b[?2026l",
    );
    let history = t.history_text(50);
    let pos_one = history.find("one").unwrap();
    let pos_two = history.find("two-frame").unwrap();
    let pos_three = history.find("three").unwrap();
    let pos_four = history.find("four-frame").unwrap();
    assert!(
        pos_one < pos_two && pos_two < pos_three && pos_three < pos_four,
        "交错段落的生效顺序乱了：{history:?}"
    );
}

#[test]
fn sgr_blink_strikethrough_double_underline() {
    let mut p = vt100::Parser::new(5, 40, 0);
    p.process(b"\x1b[5mB\x1b[0m\x1b[9mS\x1b[0m\x1b[21mD");
    let s = p.screen();
    assert!(s.cell(0, 0).unwrap().blink());
    assert!(s.cell(0, 1).unwrap().strikethrough());
    assert!(s.cell(0, 2).unwrap().double_underline());
    assert!(s.cell(0, 2).unwrap().underline());
}

#[test]
fn resize_reflow_keeps_blink_strike_double_underline() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 10));
    t.feed(b"\x1b[5mB\x1b[9mS\x1b[21mD\r\n");
    assert!(t.resize(20, 6), "缩行才走序列化重排");
    let screen = t.parser.screen();
    let mut saw_b = false;
    let mut saw_s = false;
    let mut saw_d = false;
    for row in 0..t.rows {
        for col in 0..t.cols {
            let Some(c) = screen.cell(row, col) else {
                continue;
            };
            match c.contents() {
                "B" => {
                    assert!(c.blink(), "重排后闪烁丢了");
                    saw_b = true;
                }
                "S" => {
                    assert!(c.strikethrough(), "重排后删除线丢了");
                    saw_s = true;
                }
                "D" => {
                    assert!(c.double_underline(), "重排后双下划线丢了");
                    saw_d = true;
                }
                _ => {}
            }
        }
    }
    assert!(saw_b && saw_s && saw_d, "重排后三个字形都应该还在");
}

#[test]
fn resize_reflow_keeps_single_underline_distinct_from_double() {
    let mut t = Terminal::new();
    assert!(t.resize(40, 10));
    t.feed(b"\x1b[4mU\x1b[0m\r\n");
    assert!(t.resize(20, 6));
    let screen = t.parser.screen();
    let mut saw = false;
    for row in 0..t.rows {
        for col in 0..t.cols {
            let Some(c) = screen.cell(row, col) else {
                continue;
            };
            if c.contents() == "U" {
                assert!(c.underline(), "单下划线重排后丢了");
                assert!(!c.double_underline(), "单下划线不应变成双下划线");
                saw = true;
            }
        }
    }
    assert!(saw, "重排后应还能找到 U");
}

#[test]
fn osc8_hyperlink_span_and_split_across_feeds() {
    let mut t = Terminal::new();
    // 标准 OSC 8：开始 → 文本 → 结束
    t.feed(b"\x1b]8;;https://example.com\x07click\x1b]8;;\x07");
    assert_eq!(t.osc8_spans.len(), 1);
    let (abs, sc, ec, url) = &t.osc8_spans[0];
    assert_eq!(url, "https://example.com");
    assert_eq!(*abs, 0);
    assert_eq!(*sc, 0);
    assert_eq!(*ec, 4); // "click" 五列 0..=4
    assert!(t.screen_text().contains("click"));
    assert!(!t.screen_text().contains("example.com"));

    // 跨包：半截 OSC 8 开头不应丢，拼完后生效
    let mut t2 = Terminal::new();
    t2.feed(b"\x1b]8;;https://a.co");
    assert!(t2.osc8_spans.is_empty());
    t2.feed(b"\x07hi\x1b]8;;\x07");
    assert_eq!(t2.osc8_spans.len(), 1);
    assert_eq!(t2.osc8_spans[0].3, "https://a.co");
    assert_eq!(t2.osc8_spans[0].1, 0);
    assert_eq!(t2.osc8_spans[0].2, 1); // "hi"
}

#[test]
fn osc8_not_clickable_on_alternate_screen() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]8;;https://example.com\x07click\x1b]8;;\x07");
    assert!(!t.osc8_links_for_row(0).is_empty());
    t.feed(b"\x1b[?1049h");
    assert!(
        t.osc8_links_for_row(0).is_empty(),
        "备用屏不应点到主屏 OSC 8"
    );
    t.feed(b"\x1b]8;;https://alt.example\x07vim\x1b]8;;\x07");
    assert!(t.osc8_spans.iter().all(|s| s.3 != "https://alt.example"));
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.osc8_links_for_row(0)[0].2, "https://example.com");
}

#[test]
fn osc8_hard_newline_does_not_include_blanks_or_next_col0() {
    let mut t = Terminal::new();
    assert!(t.resize(20, 5));
    t.feed(b"\x1b]8;;https://e.co\x07hi\r\n\x1b]8;;\x07");
    assert_eq!(t.osc8_spans.len(), 1, "{:?}", t.osc8_spans);
    let (abs, sc, ec, url) = &t.osc8_spans[0];
    assert_eq!(url, "https://e.co");
    assert_eq!(*abs, 0);
    assert_eq!((*sc, *ec), (0, 1));
}

#[test]
fn osc8_st_terminator_and_params_are_not_part_of_the_url() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]8;id=1;https://st.example\x1b\\click\x1b]8;;\x1b\\");
    assert_eq!(t.osc8_spans.len(), 1, "{:?}", t.osc8_spans);
    assert_eq!(t.osc8_spans[0].3, "https://st.example");
    assert_eq!((t.osc8_spans[0].1, t.osc8_spans[0].2), (0, 4));
    assert!(t.screen_text().contains("click"));
    assert!(!t.screen_text().contains("st.example"));
}

#[test]
fn malformed_osc8_does_not_hide_a_later_real_link() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]8;http://bad\x07\x1b]8;;https://ok.example\x07go\x1b]8;;\x07");
    let text = t.screen_text();
    assert!(text.contains("go"), "{text:?}");
    assert_eq!(t.osc8_spans.len(), 1, "{:?}", t.osc8_spans);
    assert_eq!(t.osc8_spans[0].3, "https://ok.example");
}

#[test]
fn osc8_prefix_split_outside_a_string_is_kept() {
    let mut t = Terminal::new();
    t.feed(b"pre\x1b]8");
    assert!(t.osc8_spans.is_empty());
    assert!(t.screen_text().contains("pre"));
    t.feed(b";;https://p.co\x07ab\x1b]8;;\x07");
    assert_eq!(t.osc8_spans.len(), 1, "{:?}", t.osc8_spans);
    assert_eq!(t.osc8_spans[0].3, "https://p.co");
    assert!(t.screen_text().contains("ab"));
}

#[test]
fn osc8_trim_follows_the_live_row_while_scrolled_back() {
    let mut t = Terminal::new();
    assert!(t.resize(20, 5));
    for _ in 0..10 {
        t.feed(b"history line\r\n");
    }
    t.parser.screen_mut().set_scrollback(3);
    t.feed(b"\x1b]8;;https://e.co\x07hi\r\n\x1b]8;;\x07");
    assert_eq!(t.osc8_spans.len(), 1, "{:?}", t.osc8_spans);
    let (_, sc, ec, url) = &t.osc8_spans[0];
    assert_eq!(url, "https://e.co");
    assert_eq!(
        (*sc, *ec),
        (0, 1),
        "回看历史时行尾空格仍应裁掉：{:?}",
        t.osc8_spans
    );
    // 同一条换行在回看时会让 scrollback 跟着加一（视口不跳）。裁剪中途会把
    // scrollback 拨到 0 去读活屏，放不回去的话这里会停在 0，而不是和普通换行一样。
    let mut plain = Terminal::new();
    assert!(plain.resize(20, 5));
    for _ in 0..10 {
        plain.feed(b"history line\r\n");
    }
    plain.parser.screen_mut().set_scrollback(3);
    plain.feed(b"hi\r\n");
    assert_eq!(
        t.parser.screen().scrollback(),
        plain.parser.screen().scrollback(),
        "裁剪不该改掉用户的回看位置"
    );
}

#[test]
fn query_prefix_inside_dcs_is_not_a_cpr() {
    let mut t = Terminal::new();
    let r1 = t.feed(b"\x1bPpayload\x1b[6");
    assert!(!is_cpr(&r1), "DCS 负载里的 ESC[6 不能进 query_tail：{r1:?}");
    let r2 = t.feed(b"n\x1b\\OK\r\n");
    assert!(!is_cpr(&r2), "拼上下一块的 n 也不能变成 CPR：{r2:?}");
    assert!(t.screen_text().contains("OK"), "{}", t.screen_text());
}

fn is_cpr(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            if let Some(rel) = bytes[i + 2..].iter().position(|&b| b == b'R') {
                let body = &bytes[i + 2..i + 2 + rel];
                if !body.is_empty()
                    && body.iter().all(|b| b.is_ascii_digit() || *b == b';')
                    && body.contains(&b';')
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

#[test]
fn unterminated_osc8_does_not_swallow_following_output() {
    let mut t = Terminal::new();
    let mut buf = b"\x1b]8;".to_vec();
    buf.extend(std::iter::repeat(b'A').take(2000));
    buf.extend_from_slice(b"VISIBLE\r\n");
    t.feed(&buf);
    assert!(
        t.screen_text().contains("VISIBLE"),
        "缺分号的 OSC 8 不应扣住后续输出：{}",
        t.screen_text()
    );
    assert!(
        t.query_tail.len() <= 512,
        "query_tail 不应无界增长：{}",
        t.query_tail.len()
    );
}

#[test]
fn malformed_osc8_does_not_swallow_following_output() {
    let mut t = Terminal::new();
    // 少一个分号的 OSC 8（BEL 收尾）+ 带分号的颜色序列 + 另一条用 BEL 收尾的标题。
    let bytes = b"\x1b]8;http://x\x07\x1b[1;32m$\x1b[0m \x1b]0;title\x07ok";
    t.feed(bytes);
    let text = t.screen_text();
    assert!(text.contains('$'), "颜色提示符被吞了：{text:?}");
    assert!(text.contains("ok"), "标题后面的文本被吞了：{text:?}");
    assert!(
        t.osc8_spans.iter().all(|s| !s.3.contains("32m")),
        "畸形 OSC 8 不应借后面的分号造出链接：{:?}",
        t.osc8_spans
    );
    assert_eq!(t.window_title.as_deref(), Some("title"));
}

#[test]
fn abandoned_osc8_keeps_trailing_query_prefix() {
    let mut t = Terminal::new();
    let mut buf = b"\x1b]8;".to_vec();
    buf.extend(std::iter::repeat(b'A').take(2000));
    buf.extend_from_slice(b"\x1b[6");
    let r1 = t.feed(&buf);
    assert!(!is_cpr(&r1), "半截查询不该在这一包就应答：{r1:?}");
    let r2 = t.feed(b"nVISIBLE");
    assert!(is_cpr(&r2), "超限 OSC 8 不应把尾部的 CPR 前缀清掉：{r2:?}");
    assert!(t.screen_text().contains("VISIBLE"), "{}", t.screen_text());
}

#[test]
fn osc8_prefix_inside_dcs_is_not_held() {
    let mut t = Terminal::new();
    // 包边界切在未终止 DCS 负载里的 ESC 之后。下一块以 [6n 开头时不能拼出 CPR。
    let r1 = t.feed(b"\x1bPpayload\x1b");
    assert!(!is_cpr(&r1));
    let r2 = t.feed(b"[6n\x1b\\OK");
    assert!(
        !is_cpr(&r2),
        "DCS 里的 ESC 不能和下一块的 [6n 拼成 CPR：{r2:?}"
    );
    assert!(t.screen_text().contains("OK"), "{}", t.screen_text());
}

#[test]
fn osc8_introducer_inside_dcs_is_not_a_link() {
    let mut t = Terminal::new();
    // 包切在未终止 DCS 的 ESC 之后。下一块以 ]8; 开头时不能拼出超链接。
    t.feed(b"\x1bPpayload\x1b");
    t.feed(b"]8;;https://phantom.example\x07nope\x1b\\AFTER");
    assert!(
        t.osc8_spans.is_empty(),
        "DCS 里的 ESC]8; 不能变成链接：{:?}",
        t.osc8_spans
    );
    let text = t.screen_text();
    assert!(text.contains("AFTER"), "{text:?}");
    assert!(!text.contains("phantom"), "{text:?}");
}

#[test]
fn malformed_osc8_closed_in_one_packet_does_not_hold_the_next() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]8;http://x\x07");
    t.feed(b"OK");
    let text = t.screen_text();
    assert!(text.contains("OK"), "{text:?}");
    assert!(t.osc8_spans.is_empty(), "{:?}", t.osc8_spans);
}

/// OSC 7 上报的目录来自**远端输出**（`cat` 一个恶意文件、嵌套 ssh 到别的主机都能发），而它
/// 会在断线重连时被拼进 `cd '…'` 自动敲回 shell。目录里带控制字符（`%15`=Ctrl+U 清行、
/// `%0D`=回车）就等于让远端输出替用户敲命令。带控制字符的目录一律不认。
#[test]
fn osc7_cwd_with_control_characters_is_rejected() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]7;file://h/home/u\x07");
    assert_eq!(t.cwd(), Some("/home/u"));
    for evil in [
        &b"\x1b]7;file://h/tmp%15touch%20/tmp/pwned%0D\x07"[..],
        b"\x1b]7;file://h/tmp%0Aid\x07",
        b"\x1b]7;file://h/tmp%03\x07",
        b"\x1b]7;file://h/tmp%7F\x07",
        b"\x1b]7;file://h/tmp%1b[2J\x07",
    ] {
        t.feed(evil);
        assert_eq!(t.cwd(), Some("/home/u"), "接受了带控制字符的目录：{evil:?}");
    }
    // 正常的中文 / 空格路径照常接受
    t.feed("\x1b]7;file://h/tmp/%E4%B8%AD%20文\x07".as_bytes());
    assert_eq!(t.cwd(), Some("/tmp/中 文"));
}

/// 本地前缀历史只该收「在提示符上敲、且回显出来了」的命令。密码提示符不回显——
/// 原先照样把输入收进历史，之后敲同一个首字母再按 ↑，密码就被明文打回命令行。
#[test]
fn unechoed_input_never_enters_local_history() {
    let mut t = Terminal::new();
    // 正常命令：敲了、也回显了
    t.feed(b"user@h:~$ ");
    t.push_input_line("sudo ls");
    t.feed(b"sudo ls");
    t.commit_line();
    assert_eq!(t.history, vec!["sudo ls".to_string()]);
    // 密码：敲了，屏幕上没有回显
    t.feed(b"\r\n[sudo] password for user: ");
    t.push_input_line("Passw0rd");
    t.commit_line();
    assert_eq!(t.history, vec!["sudo ls".to_string()], "无回显的输入进了历史");
    // 回显还没到齐（网络慢、手快）：到了一半以上就认，不然正常命令也记不住
    t.feed(b"\r\nuser@h:~$ ");
    t.push_input_line("make test");
    t.feed(b"make t");
    t.commit_line();
    assert_eq!(t.history.len(), 2);
    // 带换行/控制字符的输入（未开 bracketed paste 的多行粘贴）不进历史：
    // 之后 ↑ 会把它原样重发，其中的换行等于替用户按了回车
    t.feed(b"\r\nuser@h:~$ ");
    t.push_input_line("echo a\necho b");
    t.feed(b"echo a\r\necho b");
    t.commit_line();
    assert_eq!(t.history.len(), 2);
}

/// 没闭合的 OSC 8 链接（`ls --hyperlink` 被 Ctrl+C 打断就会留下）跨过海量输出后收束：
/// 只有最后 256 行有用，不能为中间每一行都克隆一份 URL。
///
/// 这是**钉子不是门禁**：修复前结果同样 ≤256（循环结束后才裁），差别只在中途的瞬时
/// 内存与耗时，断言测不出来。留着是保证这条路径至少被执行到、不 panic。
#[test]
fn unclosed_osc8_across_many_lines_stays_bounded() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]8;;http://example.com/x\x07link");
    t.feed(&b"\n".repeat(200_000));
    t.feed(b"\x1b]8;;\x07");
    assert!(t.osc8_spans.len() <= 256);
}

/// 滚动条滑块最小 24pt；但轨道本身比 24pt 还矮时 `f32::clamp(24, h)` 会因 min > max
/// 直接 panic——终端区被压得很矮（窗口最小 + 文件面板拉到最高）且有回滚历史时就是这样。
#[test]
fn scroll_handle_height_never_panics_on_a_tiny_track() {
    for track in [0.0_f32, 1.0, 10.0, 23.9, 24.0, 400.0] {
        let h = super::ui_paint::scroll_handle_h(track, 24, 5000);
        assert!(h <= track.max(0.0) + f32::EPSILON, "滑块比轨道还高：{h} > {track}");
    }
    assert_eq!(super::ui_paint::scroll_handle_h(400.0, 24, 5000), 24.0);
}

/// 跑一帧完整的终端 `ui()`（含鼠标处理、右键菜单），返回发往远端的字节。
fn ui_frame(t: &mut Terminal, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<u8> {
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
            out = t.ui(ui);
        });
    });
    out
}

/// `never_typed` 是自动注入（配对 token、重连 cd）唯一的安全边界：用户只要亲手往远端送过
/// 东西，就不许再替他敲键盘。原先只有键盘事件算数——鼠标上报、右键粘贴、被吞掉按下事件
/// 的 Ctrl+V 都是用户送出的字节，却不翻这个闸门。
///
/// 这条测的是**调用方**：真跑一帧 `ui()`，鼠标在终端里点一下。右键菜单粘贴与它共用同一行
/// 置位代码，但无头环境读不到剪贴板、合成不出那条路径，没有被直接测到。
#[test]
fn mouse_reports_and_swallowed_paste_count_as_user_input() {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1000h"); // 远端程序开了鼠标上报
    ui_frame(&mut t, &ctx, vec![]); // 首帧：布局
    assert!(t.never_typed());
    let pos = egui::pos2(200.0, 200.0);
    ui_frame(&mut t, &ctx, vec![egui::Event::PointerMoved(pos)]);
    let out = ui_frame(
        &mut t,
        &ctx,
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }],
    );
    assert!(!out.is_empty(), "前提：这次点击确实上报给了远端");
    assert!(!t.never_typed(), "鼠标上报了字节，闸门却仍认为用户没动过手");

    // 被 egui-winit 吞掉按下事件的 Ctrl+V：只有松开事件到达，补发 0x16
    let mut t = Terminal::new();
    let out = feed_events(&mut t, &ctx, vec![key_v(false)]);
    assert_eq!(out, vec![0x16], "前提：补发了 0x16");
    assert!(!t.never_typed());
}

fn key_ev(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// 本地前缀历史靠一份「输入行影子」猜远端命令行上现在是什么。只要按过它跟踪不了的键
/// （Ctrl+W 删词、←/→ 移动光标、Tab 补全……），影子就不再可信：此后按 ↑ 必须原样交给
/// 远端 shell，而不是拿过期的前缀去搜历史、再用 `^E^U` 把用户当前的命令行覆盖掉。
#[test]
fn history_search_stands_down_once_the_line_can_no_longer_be_tracked() {
    let ctx = egui::Context::default();
    let ctrl = egui::Modifiers {
        ctrl: true,
        command: true,
        ..Default::default()
    };
    for untrackable in [
        key_ev(egui::Key::W, ctrl),
        key_ev(egui::Key::ArrowLeft, Default::default()),
        key_ev(egui::Key::Tab, Default::default()),
        key_ev(egui::Key::Delete, Default::default()),
        key_ev(egui::Key::Home, Default::default()),
    ] {
        let mut t = Terminal::new();
        t.history = vec!["ls foo bar".into()];
        feed_events(&mut t, &ctx, vec![egui::Event::Text("ls foo".into())]);
        feed_events(&mut t, &ctx, vec![untrackable.clone()]);
        let out = feed_events(&mut t, &ctx, vec![key_ev(egui::Key::ArrowUp, Default::default())]);
        assert_eq!(out, b"\x1b[A", "按过 {untrackable:?} 之后 ↑ 仍被本地历史拦下并改写了命令行");
    }
    // 对照：一直可跟踪时照常做前缀搜索
    let mut t = Terminal::new();
    t.history = vec!["ls foo bar".into()];
    feed_events(&mut t, &ctx, vec![egui::Event::Text("ls foo".into())]);
    let out = feed_events(&mut t, &ctx, vec![key_ev(egui::Key::ArrowUp, Default::default())]);
    assert_eq!(out, b"\x05\x15ls foo bar");
    // 回车提交后重新开始跟踪
    let mut t = Terminal::new();
    t.history = vec!["ls foo bar".into()];
    feed_events(&mut t, &ctx, vec![egui::Event::Text("x".into())]);
    feed_events(&mut t, &ctx, vec![key_ev(egui::Key::ArrowLeft, Default::default())]);
    feed_events(&mut t, &ctx, vec![key_ev(egui::Key::Enter, Default::default())]);
    feed_events(&mut t, &ctx, vec![egui::Event::Text("ls foo".into())]);
    let out = feed_events(&mut t, &ctx, vec![key_ev(egui::Key::ArrowUp, Default::default())]);
    assert_eq!(out, b"\x05\x15ls foo bar");
}

fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

/// 在终端里按下、拖到终端外面松开：远端必须收到这次释放。收不到的话 vim/tmux 里的拖选
/// 就卡住了，而且我们这边也一直以为键还按着——之后光标只要划过终端，就持续上报
/// 「按住左键拖动」。
#[test]
fn a_button_released_outside_the_terminal_is_still_reported() {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1002h\x1b[?1006h"); // 按键+拖动上报，SGR 编码
    ui_frame(&mut t, &ctx, vec![]);
    let inside = egui::pos2(200.0, 200.0);
    ui_frame(&mut t, &ctx, vec![egui::Event::PointerMoved(inside)]);
    let out = ui_frame(&mut t, &ctx, vec![button(inside, true)]);
    assert!(out.ends_with(b"M"), "前提：按下已上报 {out:?}");
    let outside = egui::pos2(5000.0, 5000.0);
    let out = ui_frame(
        &mut t,
        &ctx,
        vec![egui::Event::PointerMoved(outside), button(outside, false)],
    );
    assert!(out.ends_with(b"m"), "在终端外松开，远端没收到释放：{out:?}");
    // 之后在终端里移动：没有键按着，ButtonMotion 模式下不该再上报拖动
    let out = ui_frame(&mut t, &ctx, vec![egui::Event::PointerMoved(egui::pos2(220.0, 220.0))]);
    assert!(out.is_empty(), "键早松开了，却还在上报拖动：{out:?}");
}

/// 触控板 / 高分辨率滚轮每个事件只有一两个像素。离散路径（鼠标上报、备用屏转方向键）
/// 必须攒够一行才发一步；原先 `clamp(1, 3)` 让每个非零事件都至少发一步，轻扫一行的
/// 距离变成十几行。
#[test]
fn tiny_wheel_deltas_accumulate_instead_of_each_becoming_a_step() {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1000h\x1b[?1006h");
    ui_frame(&mut t, &ctx, vec![]);
    let pos = egui::pos2(200.0, 200.0);
    ui_frame(&mut t, &ctx, vec![egui::Event::PointerMoved(pos)]);
    let mut steps = 0;
    for _ in 0..12 {
        let out = ui_frame(
            &mut t,
            &ctx,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 2.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::default(),
            }],
        );
        steps += out.iter().filter(|b| **b == b'M').count();
    }
    // 12 个 2px 的事件共 24px：一两行的距离
    assert!((1..=3).contains(&steps), "24px 的滚动发出了 {steps} 步滚轮");
    // 普通滚轮一格（Line 单位）照旧一步
    let out = ui_frame(
        &mut t,
        &ctx,
        vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        }],
    );
    assert_eq!(out.iter().filter(|b| **b == b'M').count(), 1);
}

/// 鼠标上报的移动事件按**单元格**去重：光标在同一格里挪几十个像素，不该给远端发几十条
/// 一模一样的坐标（xterm 只在跨格时上报）。
#[test]
fn pointer_motion_inside_one_cell_is_reported_once() {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1003h\x1b[?1006h"); // 任意移动都上报
    ui_frame(&mut t, &ctx, vec![]);
    let mut reports = 0;
    for dx in 0..6 {
        let out = ui_frame(
            &mut t,
            &ctx,
            vec![egui::Event::PointerMoved(egui::pos2(200.0 + dx as f32 * 0.5, 200.0))],
        );
        reports += out.iter().filter(|b| **b == b'M').count();
    }
    assert_eq!(reports, 1, "同一格内的移动被重复上报");
}

/// `clear` 发的是 `ESC[H ESC[2J ESC[3J`。网络把 `[3J` 切到下一包时，回滚缓冲同样要清——
/// 原先要求两段落在同一次处理里，切开后 `clear` 完还能上滚看到旧内容。
#[test]
fn clear_split_between_2j_and_3j_still_clears_scrollback() {
    let mut t = Terminal::new();
    for i in 0..30 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    assert!(t.history_text(100).contains("line 0"));
    t.feed(b"\x1b[H\x1b[2J");
    t.feed(b"\x1b[3Jprompt$ ");
    let history = t.history_text(100);
    assert!(!history.contains("line 0"), "分包后的 clear 没清掉回滚：{history:?}");
    assert!(history.contains("prompt$"));
    // 单独一个 [3J（前面没有 [2J）不触发重建
    let mut t = Terminal::new();
    for i in 0..30 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    t.feed(b"text");
    t.feed(b"\x1b[3J");
    assert!(t.history_text(100).contains("line 0"));
}

/// `clear` 重建解析器时要把旧模式带过去（鼠标上报、光标隐藏……），但那是「重建之前」的
/// 状态：`[3J` 之后程序自己发的模式切换必须在它之后生效，不能被回放盖掉。
#[test]
fn modes_set_after_a_clear_are_not_overridden_by_the_restored_ones() {
    let mut t = Terminal::new();
    t.feed(b"\x1b[?25l\x1b[?1000h");
    t.feed(b"\x1b[2J\x1b[3J\x1b[?25h\x1b[?1000l");
    assert!(!t.parser.screen().hide_cursor(), "程序刚恢复的光标又被藏回去了");
    assert_eq!(
        t.parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None,
        "程序刚关掉的鼠标上报又被打开了"
    );
    // 没被改动的模式照旧保留
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1000h");
    t.feed(b"\x1b[2J\x1b[3J");
    assert_ne!(t.parser.screen().mouse_protocol_mode(), vt100::MouseProtocolMode::None);
}

/// 远端 nvim / tmux 的「复制」走 OSC 52，几 KB 的选区就超过一个旧的 8KB 上限：序列跨包时
/// 前半截被丢掉，后半截找不到开头，这次复制就静默失败了。
#[test]
fn a_long_osc_split_across_packets_is_kept_until_it_terminates() {
    let mut t = Terminal::new();
    let mut first = b"\x1b]52;c;".to_vec();
    first.extend(std::iter::repeat_n(b'A', 40_000));
    t.feed(&first);
    assert!(t.notice_tail.len() >= 40_000, "未终止的长 OSC 被提前丢弃");
    // 真正没完没了的序列仍然有界
    let mut t = Terminal::new();
    t.feed(b"\x1b]52;c;");
    for _ in 0..80 {
        t.feed(&vec![b'A'; 32 * 1024]);
    }
    assert!(t.notice_tail.len() <= 1024 * 1024);
}

/// 通知序列来自远端输出，不可信：一段刷屏的 `OSC 9` 不能变成成千上万条桌面通知
///（macOS 上每条都要起一个 osascript 进程）。
#[test]
fn a_flood_of_notifications_is_rate_limited() {
    let mut t = Terminal::new();
    t.feed(&b"\x1b]9;spam\x07".repeat(5000));
    assert!(t.notices.len() <= 10, "一次输出产生了 {} 条通知", t.notices.len());
    assert!(!t.notices.is_empty(), "限速不是全丢");
    // 分多包喂也一样
    for _ in 0..200 {
        t.feed(b"\x1b]9;spam\x07");
    }
    assert!(t.notices.len() <= 10);
}

/// 给 AI 的命令输出要是干净文本。`ESC ( B`（选字符集，`tput sgr0` 每次复位都发）是三字节
/// 序列，只跳两字节的话每次复位都在输出里留下一个 `B`；DCS/APC 的负载也不该漏成正文。
#[test]
fn captured_output_has_no_escape_residue() {
    use super::vt::strip_ansi_to_text as strip;
    assert_eq!(strip(b"a\x1b(B\x1b[mb"), "ab");
    assert_eq!(strip(b"a\x1b)0b\x1b*Bc"), "abc");
    assert_eq!(strip(b"a\x1bPtmux;payload\x1b\\b"), "ab");
    assert_eq!(strip(b"a\x1b_apc\x1b\\b\x1b^pm\x1b\\c"), "abc");
    assert_eq!(strip(b"a\x1b=\x1b>\x1b7\x1b8b"), "ab"); // 双字节序列照旧
    assert_eq!(strip(b"a\x1b"), "a"); // 孤立 ESC
}

/// `133;C` 这条序列自己被包边界切开时，它后半截的字节不能混进捕获到的输出里。
#[test]
fn a_command_start_marker_split_across_packets_leaves_no_residue() {
    let seq = b"$ echo hi\r\n\x1b]133;C;aid=T\x07hi\r\n\x1b]133;D;0;aid=T\x07$ ";
    let c_start = seq.windows(2).position(|w| w == b"\x1b]").unwrap();
    for cut in c_start + 1..c_start + 8 {
        let mut t = Terminal::new();
    t.set_integration_token("T".into());
        t.arm_ai_capture_integration();
        t.feed(&seq[..cut]);
        t.feed(&seq[cut..]);
        let (code, out) = t.take_ai_done().expect("应收束");
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "hi", "在第 {cut} 字节切开后输出里混进了序列残余：{out:?}");
    }
}

fn url_list(text: &str) -> Vec<String> {
    super::paint::urls_in_text(text).into_iter().map(|u| u.2).collect()
}

/// 链接识别要经得起中文环境：紧贴汉字的链接要认得出来，后面的全角标点和汉字不能被吞进
/// 链接里；成对的括号属于链接（维基百科），落单的右括号不属于。
#[test]
fn urls_next_to_chinese_text_and_brackets() {
    assert_eq!(url_list("见 https://a.com，然后重试"), ["https://a.com"]);
    assert_eq!(url_list("详见https://a.com/x 结束"), ["https://a.com/x"]);
    assert_eq!(url_list("（https://a.com/x）。"), ["https://a.com/x"]);
    assert_eq!(url_list("打开「https://a.com」即可"), ["https://a.com"]);
    assert_eq!(
        url_list("https://en.wikipedia.org/wiki/Rust_(programming_language)"),
        ["https://en.wikipedia.org/wiki/Rust_(programming_language)"]
    );
    assert_eq!(url_list("(see https://a.com/x)"), ["https://a.com/x"]);
    assert_eq!(url_list("[https://a.com/x]."), ["https://a.com/x"]);
    // 路径里的中文属于链接；sftp:// 里的 ftp:// 不算
    assert_eq!(url_list("https://zh.wikipedia.org/wiki/中文 "), ["https://zh.wikipedia.org/wiki/中文"]);
    assert!(url_list("sftp://host/path").is_empty());
    assert_eq!(url_list("www.Example.com."), ["https://www.Example.com"]);
}

/// 长链接被终端折到下一行：两行上的任何一格都要指向**完整**的链接，
/// 而不是第一行点开半截、第二行点不了。
#[test]
fn a_url_wrapped_across_rows_is_whole_on_every_row() {
    let mut t = Terminal::new();
    t.resize(20, 6);
    let url = "https://example.com/aaaaaaaaaaaaaaaa/bbbbbbbbbbbb/end";
    t.feed(format!("go {url} ok\r\n").as_bytes());
    let screen = t.parser.screen();
    for row in 0..3u16 {
        let found = super::paint::find_row_urls(screen, row, 20);
        assert_eq!(
            found.iter().map(|u| u.2.as_str()).collect::<Vec<_>>(),
            [url],
            "第 {row} 行"
        );
    }
    // 各行覆盖的列：首行从 URL 开始处到行尾，中间行整行，末行到 URL 结束处
    assert_eq!(super::paint::find_row_urls(screen, 0, 20)[0].0, 3);
    assert_eq!(super::paint::find_row_urls(screen, 1, 20)[0].0, 0);
    assert_eq!(super::paint::find_row_urls(screen, 1, 20)[0].1, 19);
}

/// 行尾只剩一格、下一个是宽字符（汉字）时，它被折到下一行——这也是软换行。不标记的话，
/// 复制中文长行会在这里多出一个换行，查找和缩放重排也会把它当成两行。
#[test]
fn a_wide_char_pushed_to_the_next_row_marks_a_soft_wrap() {
    let mut t = Terminal::new();
    t.resize(10, 5);
    t.feed("abcdefghi中文\r\nnext".as_bytes());
    assert!(t.parser.screen().row_wrapped(0), "宽字符折行没有被记成软换行");
    assert!(!t.parser.screen().row_wrapped(1), "真正的换行不该被记成软换行");
    t.sel_anchor = Some((0, 0));
    t.sel_cursor = Some((1, 3));
    assert_eq!(t.selected_text().as_deref(), Some("abcdefghi中文"));
}

/// 从汉字的右半格开始选：高亮画出了这个字的一半，复制结果里就得有这个字。
#[test]
fn selecting_from_the_right_half_of_a_wide_char_includes_it() {
    let mut t = Terminal::new();
    t.feed("中文abc".as_bytes());
    t.sel_anchor = Some((0, 1)); // 「中」的右半格
    t.sel_cursor = Some((0, 4));
    assert_eq!(t.selected_text().as_deref(), Some("中文a"));
}

/// 上滚回看历史时远端继续输出：视口要钉在正在看的内容上，不能一行行被推走。
#[test]
fn viewport_stays_anchored_while_new_output_arrives() {
    let mut t = Terminal::new();
    for i in 0..200 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    t.scrollback = 50;
    t.parser.screen_mut().set_scrollback(50);
    let before = t.parser.screen().contents();
    assert!(before.contains("line 140"), "前提：视口确实在历史里");
    for i in 0..7 {
        t.feed(format!("new {i}\r\n").as_bytes());
    }
    // 绘制前会「探测最大回滚、再按我们记的偏移还原」——这里照做一遍
    let ours = t.scrollback;
    t.parser.screen_mut().set_scrollback(usize::MAX);
    t.parser.screen_mut().set_scrollback(ours);
    assert_eq!(t.parser.screen().contents(), before, "回看的内容被新输出推走了");
    assert_eq!(t.scrollback, 57);
}

fn find_in(t: &mut Terminal, query: &str) {
    t.find = Some(super::search::Find {
        query: query.into(),
        ..Default::default()
    });
    t.run_search();
}

/// `clear`、缩小窗口都会重建解析器，行坐标体系随之归零。查找命中记的是旧坐标，
/// 必须跟着重算，否则查找栏还显示旧的命中数，「下一个」跳到不相干的行。
#[test]
fn find_hits_are_recomputed_when_the_buffer_is_rebuilt() {
    let mut t = Terminal::new();
    for i in 0..60 {
        t.feed(format!("needle {i}\r\n").as_bytes());
    }
    find_in(&mut t, "needle");
    assert!(t.find.as_ref().unwrap().hits.len() >= 60);
    t.feed(b"\x1b[H\x1b[2J\x1b[3Jone needle here\r\n");
    assert_eq!(t.find.as_ref().unwrap().hits.len(), 1, "clear 之后还留着旧命中");
    // 缩小重排同理
    let mut t = Terminal::new();
    for i in 0..60 {
        t.feed(format!("needle {i}\r\n").as_bytes());
    }
    find_in(&mut t, "needle");
    let (cols, rows) = (t.cols, t.rows);
    t.resize(cols - 10, rows - 2);
    let hits = t.find.as_ref().unwrap().hits.clone();
    let total = t.parser.screen().scrollback_total() + t.rows as usize;
    assert!(hits.iter().all(|&h| h < total), "重排后命中行号越界：{hits:?} / {total}");
    assert_eq!(hits.len(), 60);
}

/// 查找高亮标的是「当前命中在屏幕上的第几行」。停在底部时远端继续输出，内容上移，
/// 高亮必须跟着那一行走，而不是留在原来的屏幕行上盖住别的内容。
#[test]
fn the_find_highlight_follows_its_line_as_output_scrolls() {
    let mut t = Terminal::new();
    for i in 0..10 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    t.feed(b"the needle\r\n");
    find_in(&mut t, "needle");
    t.jump_to_current();
    t.scrollback = 0;
    t.parser.screen_mut().set_scrollback(0);
    t.recompute_search_hl();
    let row = t.search_hl.expect("命中在屏幕上");
    assert!(t.screen_text().lines().nth(row as usize).unwrap().contains("needle"));
    for i in 0..(t.rows as usize) {
        t.feed(format!("more {i}\r\n").as_bytes());
        if let Some(r) = t.search_hl {
            let line = t.screen_text().lines().nth(r as usize).unwrap_or("").to_string();
            assert!(line.contains("needle"), "高亮留在了第 {r} 行，那里是 {line:?}");
        }
    }
}

/// 宽字符折行留下的那格空白，不能在别的消费方那里变成一个真空格：缩放重排后文字要连着，
/// 跨折行处的查找也要搜得到。
#[test]
fn the_gap_left_by_a_pushed_wide_char_never_becomes_a_space() {
    let mut t = Terminal::new();
    t.resize(10, 5);
    t.feed("abcdefghi中文\r\n".as_bytes());
    find_in(&mut t, "i中");
    assert_eq!(t.find.as_ref().unwrap().hits.len(), 1, "跨折行处搜不到");
    // 先缩小（走重排路径）再看：一行放得下了，文字必须连着
    t.resize(30, 4);
    assert!(
        t.screen_text().contains("abcdefghi中文"),
        "重排后内容变了：{:?}",
        t.screen_text()
    );
}

/// 拖窗口的角：同时变高又变窄。宽度变了就得回流——原先只要变高就走「直接改尺寸」，
/// 每一行超出新宽度的部分被直接截掉，既不显示也不进历史。同时回滚缓冲不能因此被吸回
/// 可见区（那是变高时不回流的本意）。
#[test]
fn growing_taller_while_changing_width_still_reflows() {
    let mut t = Terminal::new();
    t.resize(20, 6);
    for i in 0..30 {
        t.feed(format!("row{i:02} 0123456789abcd\r\n").as_bytes());
    }
    let sb_before = {
        t.parser.screen_mut().set_scrollback(usize::MAX);
        let m = t.parser.screen().scrollback();
        t.parser.screen_mut().set_scrollback(0);
        m
    };
    t.resize(12, 9); // 更高、更窄
    let all = t.collect_lines().join("");
    for i in 0..30 {
        assert!(
            all.contains(&format!("row{i:02} 0123456789abcd")),
            "第 {i} 行被截断了：变窄后超出的部分丢了"
        );
    }
    t.parser.screen_mut().set_scrollback(usize::MAX);
    let sb_after = t.parser.screen().scrollback();
    t.parser.screen_mut().set_scrollback(0);
    assert!(sb_after >= sb_before, "回滚缓冲被吸回可见区：{sb_before} → {sb_after}");
}

fn char_under_cursor(t: &Terminal) -> String {
    let sc = t.parser.screen();
    let (r, c) = sc.cursor_position();
    sc.cell(r, c).map(|x| x.contents().to_string()).unwrap_or_default()
}

/// 缩放窗口触发回流之后，光标必须还在原来那个字符上。原先回流完光标一律落到内容末尾：
/// 正在命令行中间编辑时拖一下窗口，光标就跑到行尾去了（屏幕上的位置和 shell 以为的位置
/// 从此对不上，直到 shell 重画提示符）。
#[test]
fn the_cursor_stays_on_its_character_across_a_reflow() {
    // 光标在一行中间
    let mut t = Terminal::new();
    t.resize(40, 10);
    t.feed(b"first line\r\n$ hello world\x1b[5D");
    assert_eq!(char_under_cursor(&t), "w");
    t.resize(8, 9); // 变窄：这一行折成好几段
    assert_eq!(char_under_cursor(&t), "w", "变窄后");
    t.resize(60, 8); // 再变宽：接回一行
    assert_eq!(char_under_cursor(&t), "w", "变宽后");

    // 光标在汉字上
    let mut t = Terminal::new();
    t.resize(40, 10);
    t.feed("$ 文件名 中文\x1b[4D".as_bytes());
    assert_eq!(char_under_cursor(&t), "中");
    t.resize(7, 9);
    assert_eq!(char_under_cursor(&t), "中");
    t.resize(30, 8);
    assert_eq!(char_under_cursor(&t), "中");

    // 光标在行尾（最常见的提示符状态）：仍在行尾，后面没有字符
    let mut t = Terminal::new();
    t.resize(40, 10);
    t.feed(b"output\r\n$ ls -la");
    t.resize(30, 9);
    let (r, c) = t.parser.screen().cursor_position();
    assert_eq!(t.screen_text().lines().nth(r as usize), Some("$ ls -la"));
    assert_eq!(c, 8);

    // 光标后面还有内容（上移过光标的全屏式输出）：光标不动，后面的内容也还在
    let mut t = Terminal::new();
    t.resize(40, 10);
    t.feed(b"aaa\r\nbbb\r\nccc\x1b[2A\x1b[1G");
    assert_eq!(char_under_cursor(&t), "a");
    t.resize(30, 9);
    assert_eq!(char_under_cursor(&t), "a");
    assert!(t.screen_text().contains("ccc"));
}

/// OSC 133 是输出里的字节，谁都能发。只有带**本次注入的 token** 的标记才算数：
/// 命令输出里伪造的 `133;D;0` 不能让 AI 的捕获提前收束、拿到一个假的「退出码 0」；
/// 没注入过我们片段的终端（比如用户自己的 shell 配了别家的集成）一条都不认。
#[test]
fn forged_osc133_markers_are_ignored() {
    // 没登记 token：不认，也不打开集成模式
    let mut t = Terminal::new();
    t.feed(b"\x1b]133;C\x07out\x1b]133;D;0\x07");
    assert!(!t.shell_integration_active(), "不带 token 的 133 把集成模式打开了");

    // 登记了 token：伪造的（不带 / 带错 token）不算，真的才算
    let mut t = Terminal::new();
    t.set_integration_token("secret".into());
    t.arm_ai_capture_integration();
    t.feed(b"$ cat evil.txt\r\n\x1b]133;C;aid=secret\x07");
    t.feed(b"line one\r\n\x1b]133;D;0\x07\x1b]133;D;0;aid=guess\x07line two\r\n");
    assert!(t.take_ai_done().is_none(), "输出里伪造的 D 让捕获提前结束了");
    t.feed(b"\x1b]133;D;7;aid=secret\x07$ ");
    let (code, out) = t.take_ai_done().expect("真正的 D 才收束");
    assert_eq!(code, 7);
    assert!(out.contains("line one") && out.contains("line two"), "{out:?}");
}

/// 字段顺序、多余字段都不影响解析；退出码缺省仍是 None。
#[test]
fn osc133_fields_are_parsed_by_position_and_key() {
    use osc::Osc133::*;
    let p = |s: &[u8]| osc::parse_osc133(s, 0, Some("T")).into_iter().map(|x| x.1).collect::<Vec<_>>();
    assert_eq!(p(b"\x1b]133;D;3;aid=T\x07"), [CommandEnd(Some(3))]);
    assert_eq!(p(b"\x1b]133;D;aid=T\x07"), [CommandEnd(None)]);
    assert_eq!(p(b"\x1b]133;D;3;err=x;aid=T;cl=m\x07"), [CommandEnd(Some(3))]);
    assert_eq!(p(b"\x1b]133;C;aid=T\x07"), [CommandStart]);
    assert!(p(b"\x1b]133;D;3\x07").is_empty());
    assert!(p(b"\x1b]133;D;3;aid=TT\x07").is_empty());
    assert!(p(b"\x1b]133;D;3;aid=\x07").is_empty());
}

/// 注入的片段里每条 133 标记都带 token，而且 token 只含字母数字（它被原样拼进引号里）。
#[test]
fn the_injected_snippet_tags_every_marker_with_the_token() {
    let token = crate::app::view_state::new_integration_token();
    assert!(token.len() >= 16 && token.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(token, crate::app::view_state::new_integration_token(), "token 应每次不同");
    let snippet = crate::app::view_state::ai_session_snippet(&token);
    let tagged = snippet.matches(&format!(";aid={token}")).count();
    assert_eq!(tagged, snippet.matches("]133;").count(), "有 133 标记没带 token");
    assert!(tagged >= 3);
    assert!(!snippet.contains("@T@"));
}

/// OSC 52 写剪贴板的放行条件：开关开着，且不超过大小上限。
#[test]
fn osc52_writes_respect_the_switch_and_the_size_cap() {
    use super::selection::{osc52_accept, OSC52_MAX};
    assert!(osc52_accept(true, 0));
    assert!(osc52_accept(true, OSC52_MAX));
    assert!(!osc52_accept(true, OSC52_MAX + 1));
    assert!(!osc52_accept(false, 10));
}

/// 全屏程序（备用屏）里发 `ESC[2J ESC[3J` 只是它在清自己的画面。原先照样重建解析器：
/// 新解析器在主屏，程序被踢出备用屏，主屏原来的内容也一并没了。
#[test]
fn clearing_inside_the_alternate_screen_stays_on_it() {
    let mut t = Terminal::new();
    t.feed(b"main content\r\n\x1b[?1049h");
    t.feed(b"tui\x1b[2J\x1b[3Jredrawn");
    assert!(t.parser.screen().alternate_screen(), "清屏把程序踢回了主屏");
    assert!(t.screen_text().contains("redrawn"));
    t.feed(b"\x1b[?1049l");
    assert!(t.screen_text().contains("main content"), "退出全屏程序后主屏内容没了");
}

/// 我们自己扫转义序列的那套规则要和解析器一致，否则同一段字节两边理解不同：
/// - `ESC ESC [6n`：前一个 ESC 被后一个打断，后面是一条正常的查询，要应答；
/// - OSC 被 CAN / SUB 打断：到此为止，后面的是普通输出，不是这条 OSC 的内容。
#[test]
fn escape_scanning_agrees_with_the_parser() {
    assert_eq!(Terminal::new().feed(b"\x1b\x1b[6n"), Terminal::new().feed(b"\x1b[6n"));
    for abort in [0x18u8, 0x1a] {
        let mut t = Terminal::new();
        let mut bytes = b"\x1b]0;title".to_vec();
        bytes.push(abort);
        bytes.extend_from_slice(b"visible\x07");
        t.feed(&bytes);
        assert_eq!(t.window_title, None, "被打断的 OSC 仍被当成了标题");
        assert!(t.screen_text().contains("visible"));
    }
}

/// 光标写满一行后「悬」在行尾，解析器报的列等于列数。画光标、报输入法位置都要钳回
/// 最后一列，不然光标画到了终端区域外面。
#[test]
fn a_cursor_hanging_at_the_right_margin_is_reported_inside_the_grid() {
    let mut t = Terminal::new();
    t.resize(10, 3);
    t.feed(b"0123456789");
    assert_eq!(t.cursor_cell(), (0, 9));
}

/// 未开 bracketed paste 时，粘贴内容里的换行要按「回车键」发（`\r`），与 xterm 一致。
/// 发 `\n` 的话，raw 模式的程序收到的是 Ctrl+J 而不是回车（nano 里那是「对齐段落」）。
#[test]
fn unbracketed_paste_sends_newlines_as_carriage_returns() {
    let t = Terminal::new();
    assert_eq!(t.wrap_paste(b"a\nb\r\nc\rd"), b"a\rb\rc\rd");
    let mut t = Terminal::new();
    t.feed(b"\x1b[?2004h");
    assert_eq!(t.wrap_paste(b"a\nb"), b"\x1b[200~a\nb\x1b[201~", "括号粘贴原样发");
}

/// 缩放回流会换一个新的解析器。当前的文字属性（颜色、粗体……）也是状态的一部分：
/// 程序设了红色之后窗口缩了一下，接下来的输出仍应是红色。
#[test]
fn the_current_text_attributes_survive_a_reflow() {
    let mut t = Terminal::new();
    t.resize(40, 10);
    t.feed(b"\x1b[31;1mred ");
    t.resize(30, 9);
    t.feed(b"X");
    let sc = t.parser.screen();
    let (r, c) = sc.cursor_position();
    let cell = sc.cell(r, c - 1).unwrap();
    assert_eq!(cell.contents(), "X");
    assert_eq!(cell.fgcolor(), vt100::Color::Idx(1), "回流后颜色丢了");
    assert!(cell.bold(), "回流后粗体丢了");
}

/// 一条没完没了、始终不终止的 OSC：解析器会把它整条攒在内存里（没有上限）。超过我们
/// 自己的上限后要让解析器放弃这条序列——之后的输出才会重新显示，内存也不再涨。
#[test]
fn an_endless_osc_is_abandoned_instead_of_swallowing_everything() {
    let mut t = Terminal::new();
    t.feed(b"\x1b]0;");
    for _ in 0..40 {
        t.feed(&vec![b'A'; 32 * 1024]);
    }
    // 后面是**纯文本**（没有 ESC / BEL 来顺带终止那条 OSC）：只有解析器真的放弃了它，
    // 这些字才会显示出来
    t.feed(b"\r\nvisible again\r\n");
    assert!(
        t.screen_text().contains("visible again"),
        "解析器还陷在那条 OSC 里，后面的输出全被吞了"
    );
}

/// DCS / OSC 的负载被分成两包时，第二包开头那截仍是负载：里面碰巧出现的清屏、查询序列
/// 不能当真（单包内早就不当真了，见 `clear_and_cpr_inside_dcs_are_ignored`）。
#[test]
fn a_string_payload_continued_in_the_next_packet_is_still_payload() {
    let mut t = Terminal::new();
    for i in 0..30 {
        t.feed(format!("line {i}\r\n").as_bytes());
    }
    t.feed(b"\x1bPq#0;2;0;0;0");
    let replies = t.feed(b"payload\x1b[2J\x1b[3J\x1b[6nmore\x1b\\after\r\n");
    assert!(replies.is_empty(), "负载里的查询被应答了：{replies:?}");
    let history = t.history_text(100);
    assert!(history.contains("line 0"), "负载里的清屏序列真的清了回滚");
    assert!(history.contains("after"));
}

/// 右键属于本地菜单（复制 / 粘贴是这个应用最常用的入口）：不再同时转发给远端——
/// 否则 tmux 弹它自己的菜单、vim 扩展选区，和本地菜单叠在一起。
#[test]
fn right_click_is_not_forwarded_to_the_remote() {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1000h\x1b[?1006h");
    ui_frame(&mut t, &ctx, vec![]);
    let pos = egui::pos2(200.0, 200.0);
    ui_frame(&mut t, &ctx, vec![egui::Event::PointerMoved(pos)]);
    let mut out = Vec::new();
    for pressed in [true, false] {
        out.extend(ui_frame(
            &mut t,
            &ctx,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }],
        ));
    }
    assert!(out.is_empty(), "右键被上报给了远端：{out:?}");
}

/// 传统（非 SGR）鼠标编码下，释放事件的按钮码是 3，但修饰键位要照带（xterm 如此）。
#[test]
fn legacy_mouse_release_keeps_modifier_bits() {
    let ctx = egui::Context::default();
    crate::theme::apply(&ctx);
    let mut t = Terminal::new();
    t.feed(b"\x1b[?1000h");
    ui_frame(&mut t, &ctx, vec![]);
    let pos = egui::pos2(200.0, 200.0);
    ui_frame(&mut t, &ctx, vec![egui::Event::PointerMoved(pos)]);
    let alt = egui::Modifiers {
        alt: true,
        ..Default::default()
    };
    let ev = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: alt,
    };
    let press = ui_frame(&mut t, &ctx, vec![ev(true)]);
    let release = ui_frame(&mut t, &ctx, vec![ev(false)]);
    assert_eq!(press[3], 32 + 8, "按下：左键 + Alt");
    assert_eq!(release[3], 32 + 3 + 8, "释放：3 + Alt");
}
