//! 文本文件编码探测/解码。SSH（SFTP 读取）与本机（本地 FS 读取）两条路径共用同一套逻辑，
//! 避免各写一遍导致「远端能正确识别 GBK、本机不能」这类不一致。

/// 带 BOM 的 UTF-8 的编码名（encoding_rs 没有这个标签，是我们自己的约定）。
pub(crate) const UTF8_BOM: &str = "UTF-8 BOM";

/// 按编码名取 encoding_rs 编码；认不出的（含 [`UTF8_BOM`]）按 UTF-8。
pub(crate) fn encoding_for(name: &str) -> &'static encoding_rs::Encoding {
    encoding_rs::Encoding::for_label(name.as_bytes()).unwrap_or(encoding_rs::UTF_8)
}

/// 解码后的文本 → (内部用的 LF 文本, 保存时要还原的行尾)。
///
/// 只有**每一个**换行都是 CRLF 才算 CRLF 文件、统一成 LF。混合行尾的文件按 LF 处理并
/// **原样保留**其中的 `\r`：内部只有一个行尾标志，把它当成 CRLF 的话，保存时原本是 LF
/// 的行会全部被改成 CRLF——用户只改了一个字，diff 却是整个文件。
pub(crate) fn split_eol(decoded: String) -> (String, crate::proto::Eol) {
    let lf = decoded.bytes().filter(|b| *b == b'\n').count();
    let crlf = decoded.matches("\r\n").count();
    if crlf > 0 && crlf == lf {
        (decoded.replace("\r\n", "\n"), crate::proto::Eol::Crlf)
    } else {
        (decoded, crate::proto::Eol::Lf)
    }
}

/// 把编辑器内容编码成要写回文件的字节：还原行尾、按原编码编码（含 BOM）。
///
/// 目标编码表示不了的字符是**错误**而不是警告：encoding_rs 会把它们写成 `&#NNNN;`，
/// 文件里的字就此变成一串数字引用，而编辑器里显示的仍是原字、标签还被标成已保存——
/// 等用户发现时已经关掉标签、找不回来了。宁可不存，让用户换个编码。
pub(crate) fn encode_for_save(
    content: String,
    eol: crate::proto::Eol,
    encoding: &str,
) -> Result<Vec<u8>, String> {
    let text = match eol {
        // 先归一再展开：混合行尾的文件是按 LF 打开的、内容里还留着 `\r`，直接把 `\n` 换成
        // `\r\n` 会把原本就是 CRLF 的行写成 `\r\r\n`。
        crate::proto::Eol::Crlf => content.replace("\r\n", "\n").replace('\n', "\r\n"),
        crate::proto::Eol::Lf => content,
    };
    if encoding == UTF8_BOM {
        let mut out = vec![0xEF, 0xBB, 0xBF];
        out.extend_from_slice(text.as_bytes());
        return Ok(out);
    }
    let enc = encoding_for(encoding);
    // encoding_rs 不能**编码**成 UTF-16（`encode` 会悄悄输出 UTF-8）：自己来，连 BOM 一起
    //（UTF-16 只会经由 BOM 被识别出来，解码时 BOM 已剥掉）。
    if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
        let le = enc == encoding_rs::UTF_16LE;
        let mut out = Vec::with_capacity(text.len() * 2 + 2);
        for unit in std::iter::once(0xFEFFu16).chain(text.encode_utf16()) {
            out.extend_from_slice(&if le { unit.to_le_bytes() } else { unit.to_be_bytes() });
        }
        return Ok(out);
    }
    let (bytes, _, had_unmappable) = enc.encode(&text);
    if had_unmappable {
        return Err(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!(
                "有字符无法用 {encoding} 编码表示，未保存。请在状态栏把编码改为 UTF-8 后再保存"
            ),
            crate::i18n::Lang::En => format!(
                "Some characters cannot be represented in {encoding}; not saved. \
                 Switch the encoding to UTF-8 in the status bar and save again"
            ),
        });
    }
    Ok(bytes.into_owned())
}

/// 跟随（tail -f）按块读文件：这一块末尾有多少字节属于**还没读完**的内容，要留到下一块
/// 一起解码。两种情况：
/// - 一个多字节字符被块边界切开（UTF-8 最多差 3 字节；GBK / Big5 / Shift_JIS / EUC-KR
///   差 1 字节，GB18030 最多 3）；
/// - 块以 `\r` 结尾——它可能是 `\r\n` 的前一半，单独解码就成了孤立的 `\r`。
///
/// 只看「末尾」：块中间真正的坏字节照常按替换字符输出，不然会永远留着不放。
pub(crate) fn tail_carry_len(enc: &'static encoding_rs::Encoding, bytes: &[u8]) -> usize {
    if bytes.last() == Some(&b'\r') {
        return 1;
    }
    let ends_broken = |b: &[u8]| {
        let (text, had_errors) = enc.decode_without_bom_handling(b);
        had_errors && text.ends_with('\u{FFFD}')
    };
    if !ends_broken(bytes) {
        return 0;
    }
    (1..=3.min(bytes.len()))
        .find(|k| !ends_broken(&bytes[..bytes.len() - k]))
        .unwrap_or(0)
}

/// 探测字节的字符编码并解码，返回 (文本, 编码名, 是否有损)。
/// UTF-8(含 BOM) 优先；非 UTF-8 用 chardetng 猜测（中文环境多为 GBK/GB18030）。
/// 「有损」= 有字节在该编码下不合法、已被替换成 U+FFFD：内存里的文本编不回原字节。
pub(crate) fn decode_text(data: &[u8]) -> (String, String, bool) {
    // UTF-8 BOM：记成单独的编码名，保存时才知道要把 BOM 写回去
    if let Some(body) = data.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let text = String::from_utf8_lossy(body);
        let lossy = matches!(text, std::borrow::Cow::Owned(_));
        return (text.into_owned(), UTF8_BOM.into(), lossy);
    }
    // 无损 UTF-8 直接用
    if let Ok(s) = std::str::from_utf8(data) {
        return (s.to_string(), "UTF-8".into(), false);
    }
    // 非 UTF-8：探测后解码
    let mut det = chardetng::EncodingDetector::new();
    det.feed(data, true);
    let enc = det.guess(None, true);
    let (cow, actual, lossy) = enc.decode(data);
    (cow.into_owned(), actual.name().to_string(), lossy)
}

/// 不探测、按指定编码解码（「按编码重新打开」：自动探测猜错时由用户指定）。
/// 返回 (文本, 编码名, 是否有损)；编码名原样返回，保存时沿用。
pub(crate) fn decode_as(data: &[u8], encoding: &str) -> (String, String, bool) {
    if encoding == UTF8_BOM {
        let body = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
        let text = String::from_utf8_lossy(body);
        let lossy = matches!(text, std::borrow::Cow::Owned(_));
        return (text.into_owned(), UTF8_BOM.into(), lossy);
    }
    // 去掉与该编码对应的 BOM（UTF-16 靠它，UTF-8 文件带了也不该显示成 U+FEFF）
    let (cow, lossy) = encoding_for(encoding).decode_with_bom_removal(data);
    (cow.into_owned(), encoding.to_string(), lossy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Eol;

    /// 打开再保存、一个字没改：写回去的必须和原文件逐字节相同。
    fn roundtrip(data: &[u8]) -> Vec<u8> {
        let (decoded, encoding, lossy) = decode_text(data);
        assert!(!lossy);
        let (content, eol) = split_eol(decoded);
        encode_for_save(content, eol, &encoding).expect("应可无损写回")
    }

    #[test]
    fn untouched_files_round_trip_byte_for_byte() {
        let gbk = encoding_rs::GBK.encode("中文内容，第二行\r\n结束\r\n").0.into_owned();
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("hi 中\n".encode_utf16().flat_map(|u| u.to_le_bytes()));
        for (name, data) in [
            ("UTF-8 LF", "a\nb\n".as_bytes().to_vec()),
            ("UTF-8 CRLF", "a\r\nb\r\n".as_bytes().to_vec()),
            // 带 BOM：BOM 不能丢（依赖它的 Windows 工具会认不出编码）
            ("UTF-8 BOM", b"\xEF\xBB\xBFa\r\nb".to_vec()),
            // 混合行尾：原本是 LF 的行不能被改成 CRLF
            ("mixed EOL", "a\r\nb\nc\r\n".as_bytes().to_vec()),
            ("lone CR", "a\rb\n".as_bytes().to_vec()),
            ("GBK", gbk),
            ("UTF-16LE", utf16),
        ] {
            assert_eq!(roundtrip(&data), data, "{name} 没有原样写回");
        }
    }

    #[test]
    fn only_uniformly_crlf_text_is_normalised() {
        assert_eq!(split_eol("a\r\nb\r\n".into()), ("a\nb\n".into(), Eol::Crlf));
        assert_eq!(split_eol("a\nb\n".into()), ("a\nb\n".into(), Eol::Lf));
        assert_eq!(split_eol("a\r\nb\n".into()), ("a\r\nb\n".into(), Eol::Lf));
        assert_eq!(split_eol("no newline".into()), ("no newline".into(), Eol::Lf));
    }

    /// 混合行尾的文件按 LF 打开、内容里留着 `\r`。用户在状态栏切到 CRLF 再保存，是要把
    /// 整个文件统一成 CRLF——原本就是 CRLF 的行不能变成 `\r\r\n`。
    #[test]
    fn switching_a_mixed_file_to_crlf_does_not_double_the_cr() {
        let (content, eol) = split_eol("a\r\nb\nc\r\n".into());
        assert_eq!(eol, Eol::Lf);
        let out = encode_for_save(content, Eol::Crlf, "UTF-8").unwrap();
        assert_eq!(out, b"a\r\nb\r\nc\r\n");
    }

    /// 有损解码要如实报告：这些文件一保存就会把替换字符写回去。
    #[test]
    fn lossy_decoding_is_reported() {
        assert!(!decode_text("纯 UTF-8".as_bytes()).2);
        assert!(decode_text(b"\xEF\xBB\xBFok \xFF bad").2, "BOM 之后的非法字节");
        let gbk = encoding_rs::GBK.encode("中文").0.into_owned();
        assert!(!decode_text(&gbk).2);
        // 文件里本来就有合法的 U+FFFD：不算有损
        assert!(!decode_text("a\u{FFFD}b".as_bytes()).2);
    }

    /// 探测猜错时由用户指定编码重新解码：结果按指定的来，编码名沿用到保存。
    #[test]
    fn decoding_with_an_explicit_encoding() {
        let gbk = encoding_rs::GBK.encode("中文内容").0.into_owned();
        assert_eq!(decode_as(&gbk, "GBK"), ("中文内容".into(), "GBK".into(), false));
        // 同样的字节按 UTF-8 读是有损的
        assert!(decode_as(&gbk, "UTF-8").2);
        // UTF-8 BOM / UTF-16：BOM 不进正文
        assert_eq!(decode_as(b"\xEF\xBB\xBFhi", UTF8_BOM).0, "hi");
        assert_eq!(decode_as(b"\xEF\xBB\xBFhi", "UTF-8").0, "hi");
        let mut u16le = vec![0xFF, 0xFE];
        u16le.extend("hi".encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode_as(&u16le, "UTF-16LE").0, "hi");
        // 指定编码重新打开、原样保存：字节不变
        let (text, enc, _) = decode_as(&gbk, "GBK");
        let (content, eol) = split_eol(text);
        assert_eq!(encode_for_save(content, eol, &enc).unwrap(), gbk);
    }

    /// 目标编码表示不了的字符：拒绝保存，而不是写成 `&#20013;` 再报「已保存」。
    #[test]
    fn unrepresentable_characters_refuse_to_save() {
        assert!(encode_for_save("中文".into(), Eol::Lf, "ISO-8859-1").is_err());
        assert!(encode_for_save("emoji 😀".into(), Eol::Lf, "GBK").is_err());
        assert_eq!(
            encode_for_save("中文\n".into(), Eol::Crlf, "GBK").unwrap(),
            encoding_rs::GBK.encode("中文\r\n").0.into_owned()
        );
        assert_eq!(encode_for_save("é".into(), Eol::Lf, "ISO-8859-1").unwrap(), vec![0xE9]);
        // 认不出的编码名按 UTF-8，不丢字
        assert_eq!(encode_for_save("中".into(), Eol::Lf, "nonsense").unwrap(), "中".as_bytes());
    }

    /// 跟随模式的分块：不管块边界切在哪，拼回去的文本都必须和整段解码一致。
    #[test]
    fn tail_chunks_decode_the_same_wherever_the_boundary_falls() {
        for (enc, text) in [
            (encoding_rs::UTF_8, "日志 line\r\n第二行 😀 end\r\n"),
            (encoding_rs::GBK, "日志 line\r\n第二行 完 end\r\n"),
            (encoding_rs::GB18030, "日志 𠀀 line\r\n第二行\r\n"),
            (encoding_rs::BIG5, "日誌 line\r\n第二行\r\n"),
            (encoding_rs::SHIFT_JIS, "ログ line\r\n二行目\r\n"),
        ] {
            let data = enc.encode(text).0.into_owned();
            let want = text.replace("\r\n", "\n");
            for cut in 0..=data.len() {
                let mut got = String::new();
                let mut carry: Vec<u8> = Vec::new();
                for chunk in [&data[..cut], &data[cut..]] {
                    let mut bytes = std::mem::take(&mut carry);
                    bytes.extend_from_slice(chunk);
                    let keep = tail_carry_len(enc, &bytes);
                    carry = bytes.split_off(bytes.len() - keep);
                    got.push_str(&enc.decode_without_bom_handling(&bytes).0.replace("\r\n", "\n"));
                }
                // 文件真的以半个字符 / 孤立 \r 结尾时，残留留在 carry 里等下一块
                got.push_str(&enc.decode_without_bom_handling(&carry).0.replace("\r\n", "\n"));
                assert_eq!(got, want, "{} 在第 {cut} 字节切开后内容变了", enc.name());
            }
        }
        // 块中间的坏字节不算「没读完」：不能一直留着
        assert_eq!(tail_carry_len(encoding_rs::UTF_8, b"ab\xFFcd"), 0);
    }
}
