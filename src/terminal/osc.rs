//! OSC 7 解析与 URL 打开。

pub(super) fn open_url(url: &str) {
    // 终端输出内容不可信：先做 scheme 白名单（避免 file:// 打开本地任意文件、
    // 或恶意注册协议触发任意处理器）；裸 www. 补 https。
    let normalized = if url.to_ascii_lowercase().starts_with("www.") { format!("https://{url}") } else { url.to_string() };
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
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

/// 解析 OSC 7（`ESC ] 7 ; file://host/path BEL|ST`），返回最后一个上报的本地路径。
pub(super) fn parse_osc7(data: &[u8]) -> Option<String> {
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
