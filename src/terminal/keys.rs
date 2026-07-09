//! 键盘与鼠标协议编码。

use egui::{Key, Modifiers};

pub(super) fn encode_key(key: Key, mods: Modifiers, out: &mut Vec<u8>) {
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


/// 编码一个鼠标事件为终端字节流。`cb` 为按钮码（含修饰位/移动位/滚轮位）。
/// `col`/`row` 为 0 基屏幕坐标，内部转 1 基。`press` 仅影响 SGR 的 M/m。
pub(super) fn encode_mouse(enc: vt100::MouseProtocolEncoding, cb: u8, col: u16, row: u16, press: bool, out: &mut Vec<u8>) {
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
