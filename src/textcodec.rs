//! 文本文件编码探测/解码。SSH（SFTP 读取）与本机（本地 FS 读取）两条路径共用同一套逻辑，
//! 避免各写一遍导致「远端能正确识别 GBK、本机不能」这类不一致。

/// 探测字节的字符编码并解码为 String，返回 (文本, 编码名)。
/// UTF-8(含 BOM) 优先；非 UTF-8 用 chardetng 猜测（中文环境多为 GBK/GB18030）。
pub(crate) fn decode_text(data: &[u8]) -> (String, String) {
    // UTF-8 BOM
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (
            String::from_utf8_lossy(&data[3..]).into_owned(),
            "UTF-8".into(),
        );
    }
    // 无损 UTF-8 直接用
    if let Ok(s) = std::str::from_utf8(data) {
        return (s.to_string(), "UTF-8".into());
    }
    // 非 UTF-8：探测后解码
    let mut det = chardetng::EncodingDetector::new();
    det.feed(data, true);
    let enc = det.guess(None, true);
    let (cow, actual, _) = enc.decode(data);
    (cow.into_owned(), actual.name().to_string())
}
