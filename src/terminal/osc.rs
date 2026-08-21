//! OSC 7 解析与 URL 打开。

pub(super) fn open_url(url: &str) {
    // 终端输出内容不可信：先做 scheme 白名单（避免 file:// 打开本地任意文件、
    // 或恶意注册协议触发任意处理器）；裸 www. 补 https。
    let normalized = if url.to_ascii_lowercase().starts_with("www.") {
        format!("https://{url}")
    } else {
        url.to_string()
    };
    let lower = normalized.to_ascii_lowercase();
    const ALLOWED: [&str; 4] = ["http://", "https://", "ftp://", "ftps://"];
    if !ALLOWED.iter().any(|p| lower.starts_with(p)) {
        log::warn!("拒绝打开非白名单 scheme 的 URL：{url}");
        return;
    }
    let url = normalized.as_str();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    // Windows：不经 cmd（`cmd /C start` 会解释 URL 中的 & ^ 等元字符，恶意 URL 可
    // 触发本地命令）；rundll32 的 FileProtocolHandler 以单参数接收 URL，无 shell 解释。
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
}

/// 解析 OSC 7（`ESC ] 7 ; file://host/path BEL|ST`），返回最后一个上报的本地路径。
pub(super) fn parse_osc7(data: &[u8]) -> Option<String> {
    let pat = b"\x1b]7;";
    let mut result = None;
    let mut i = 0;
    while i + pat.len() <= data.len() {
        let Some(rel) = data[i..].windows(pat.len()).position(|w| w == pat) else {
            break;
        };
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

/// 解析 OSC 通知序列，返回 (标题, 正文) 列表：
/// - OSC 9  ：`ESC ] 9 ; <message> BEL|ST`（iTerm2/ConEmu 式，仅正文）
/// - OSC 777：`ESC ] 777 ; notify ; <title> ; <body> BEL|ST`（urxvt 式，标题+正文）
/// Claude Code 等 AI CLI 的通知 hook、以及 `printf '\e]9;done\a'` 类脚本都走这两类。
/// `carried`：`data` 开头有多少字节是**上一次调用已经扫过**的（`feed()` 为了拼接被截断的
/// 序列而留下的尾巴，见 `Terminal::notice_tail`）。终止符落在这段前缀里的 OSC 序列，上一轮
/// 就已经完整、已经报过一次通知了，这一轮必须跳过。
///
/// 不跳会重复弹通知，而且是**只在特定切包位置**才出现的那种偶发：典型是 codex/Claude Code
/// 在 tmux 下发的 DCS 透传 `ESC P tmux ; ESC ESC ] 9 ; <msg> BEL ESC \`——SSH 若恰好把包切在
/// BEL 之后、结尾的 `ESC \` 之前（只差两个字节，很容易发生），那条内层 OSC 在第一包里就已经
/// 终止并报过通知；而外层 DCS 还没终止，于是整段被留作 `notice_tail` 原样带到第二包重扫一遍，
/// 同一条通知就弹了两次。`count_bel` 没这个问题（它对未终止的 OSC/DCS 一路吃到末尾且不计数），
/// 所以 `feed()` 里那段「不会重复计数」的说明只对响铃成立，对通知不成立。
pub(super) fn parse_osc_notify(data: &[u8], carried: usize) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for (seq_start, body_start, end) in osc_sequences(data) {
        // 终止符在已扫过的前缀里 = 上一轮已经报过这一条
        if end < carried {
            continue;
        }
        let payload = &data[body_start..end];
        let _ = seq_start;
        if let Some(rest) = payload.strip_prefix(b"9;") {
            if let Ok(s) = std::str::from_utf8(rest) {
                let s = s.trim();
                if !s.is_empty() && !is_conemu_progress(s) {
                    out.push((None, s.to_string()));
                }
            }
        } else if let Some(rest) = payload.strip_prefix(b"777;notify;") {
            if let Ok(s) = std::str::from_utf8(rest) {
                let (title, body) = match s.split_once(';') {
                    Some((t, b)) => (t.trim(), b.trim()),
                    None => (s.trim(), ""),
                };
                if !title.is_empty() || !body.is_empty() {
                    out.push((
                        if title.is_empty() {
                            None
                        } else {
                            Some(title.to_string())
                        },
                        body.to_string(),
                    ));
                }
            }
        }
    }
    out
}

/// `OSC 9` 的这段负载（已去掉 `9;` 前缀）是不是 ConEmu 的**进度条**上报，而不是通知。
///
/// `OSC 9;4;<state>;<percent>` 是 ConEmu / Windows Terminal 的进度协议，如今 cargo、
/// ripgrep、winget 等一大批工具都在发（结束时还会补一条 `9;4;0;0` 清零）。把它当通知放行
/// 有两层后果，第二层才是真正难受的：
///   1. 弹出一条正文是 `4;1;50` 的莫名其妙的桌面通知；
///   2. `feed()` 紧接着会把 `ai_cli_seen` 置真——**此后这个标签的所有裸 BEL 都开始提醒**，
///      而裸 BEL 和 shell 补全失败、readline 报错发的是同一个字节。也就是说在一个普通
///      shell 标签里跑一次 `cargo build`，就把这个标签变成了「每次补全失败都弹提醒」，
///      正是 `push_bel_notice` 注释里说「这个功能此前难用的根因」要防的那件事。
///
/// 判据刻意只认 `4`（进度），不泛化成「首段是数字就跳过」：iTerm2 式的 `OSC 9;<正文>`
/// 通知正文是自由文本，真有人发一条以数字加分号开头的通知（`9;404;not found`）也不该被
/// 吞掉。将来若发现 `9;9;<cwd>`（ConEmu 设置工作目录，Windows 上 clink 会发）之类也误报，
/// 再按同样方式逐个加，不要改成通配。
fn is_conemu_progress(payload: &str) -> bool {
    matches!(payload.split(';').next(), Some("4")) && payload.contains(';')
}

/// 枚举数据块里的完整 OSC 序列：(序列起始, 负载起始(ESC]x; 的 `x` 处), 负载结束(BEL/ST 前))。
/// 不完整序列（无终止符）跳过——与 parse_osc7 的既有行为一致。
fn osc_sequences(data: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= data.len() {
        let Some(rel) = data[i..].windows(2).position(|w| w == b"\x1b]") else {
            break;
        };
        let start = i + rel;
        let body = start + 2;
        let mut end = body;
        while end < data.len() {
            if data[end] == 0x07 || (data[end] == 0x1b && data.get(end + 1) == Some(&b'\\')) {
                break;
            }
            end += 1;
        }
        if end >= data.len() {
            break; // 不完整：等下一块（与 osc7 处理一致，直接放弃本次）
        }
        out.push((start, body, end));
        i = end + 1;
    }
    out
}

/// 统计**转义序列之外**的 BEL（0x07）数量。OSC 序列本身以 BEL 终止（`ESC]9;...\x07`），
/// 直接 `contains(0x07)` 会把通知序列的终止符误当成响铃。
pub(super) fn count_bel(data: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            0x1b => {
                // 跳过整段转义序列：OSC（到 BEL/ST）、CSI（到终结字节）、ESC+单字节
                match data.get(i + 1) {
                    Some(b']') => {
                        i += 2;
                        while i < data.len() {
                            if data[i] == 0x07 {
                                i += 1;
                                break;
                            }
                            if data[i] == 0x1b && data.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    Some(b'[') => {
                        i += 2;
                        while i < data.len() {
                            let done = (0x40..=0x7e).contains(&data[i]);
                            i += 1;
                            if done {
                                break;
                            }
                        }
                    }
                    // DCS / SOS / PM / APC：同属「字符串类」转义序列，一律以 ST(`ESC \`) 收尾，
                    // 内容可以是任意字节——**包括 BEL**。少了这一支，tmux 的透传形式
                    // （`ESC P tmux ; ESC ESC ] 9 ; <msg> BEL ESC \`，codex 在 tmux 里就这么发
                    // 通知）会被当成「ESC+1 字节」跳过，里面那个 OSC 终止用的 BEL 就被算成真
                    // 响铃，于是同一条通知既走 OSC 又走 BEL，弹两遍。
                    Some(b'P') | Some(b'X') | Some(b'^') | Some(b'_') => {
                        i += 2;
                        while i < data.len() {
                            if data[i] == 0x1b && data.get(i + 1) == Some(&b'\\') {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                    }
                    _ => i += 2,
                }
            }
            0x07 => {
                n += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    n
}

/// 若缓冲区结尾停在一条**未终止的字符串类转义序列**里，返回它的起始下标。
///
/// 「字符串类」指 OSC(`ESC ]`) 与 DCS/SOS/PM/APC(`ESC P/X/^/_`)：它们的内容可以是任意
/// 字节（**包括 BEL**），必须靠终止符划界。调用方据此把这一截留到下一个数据块前面再扫，
/// 避免一条被 SSH 分包切断的通知既丢了通知、又把它的 BEL 终止符误记成响铃。
///
/// 结尾只有一个孤零零的 `ESC` 也算——它后面是什么类型的序列还不知道，同样得留着。
///
/// CSI(`ESC [`) 不在此列：它的参数/中间字节都落在 0x20–0x3f，终结字节落在 0x40–0x7e，
/// 无论怎么切断都不可能有 0x07 混在里面，留不留都不影响响铃计数。
pub(super) fn unterminated_string_tail(data: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            i += 1;
            continue;
        }
        let start = i;
        match data.get(i + 1) {
            None => return Some(start), // 结尾的裸 ESC
            Some(&kind @ (b']' | b'P' | b'X' | b'^' | b'_')) => {
                let bel_terminates = kind == b']'; // 只有 OSC 认 BEL 作终止符
                i += 2;
                let mut terminated = false;
                while i < data.len() {
                    if bel_terminates && data[i] == 0x07 {
                        i += 1;
                        terminated = true;
                        break;
                    }
                    if data[i] == 0x1b {
                        match data.get(i + 1) {
                            Some(b'\\') => {
                                i += 2;
                                terminated = true;
                            }
                            // ST 的后半个字节还没到：整段留到下一块再判
                            None => {}
                            Some(_) => {
                                i += 1;
                                continue;
                            }
                        }
                        break;
                    }
                    i += 1;
                }
                if !terminated {
                    return Some(start);
                }
            }
            // CSI 与 ESC+单字节：不可能藏 BEL，跳过即可（见上文）
            Some(b'[') => {
                i += 2;
                while i < data.len() {
                    let fin = (0x40..=0x7e).contains(&data[i]);
                    i += 1;
                    if fin {
                        break;
                    }
                }
            }
            _ => i += 2,
        }
    }
    None
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
