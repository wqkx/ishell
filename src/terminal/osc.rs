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
pub(super) fn parse_osc_notify(data: &[u8]) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for (seq_start, body_start, end) in osc_sequences(data) {
        let payload = &data[body_start..end];
        let _ = seq_start;
        if let Some(rest) = payload.strip_prefix(b"9;") {
            if let Ok(s) = std::str::from_utf8(rest) {
                let s = s.trim();
                if !s.is_empty() {
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
