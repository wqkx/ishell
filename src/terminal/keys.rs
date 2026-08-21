//! 键盘与鼠标协议编码。

use egui::{Key, Modifiers};

/// `app_cursor`：远端是否已启用「应用光标键模式」（DECCKM，`CSI ?1h`）。ncurses 程序
/// （htop/vim/less 等）初始化时靠 terminfo 的 `smkx` 能力打开这个模式，之后期望方向键/
/// Home/End 编码为 SS3 形式（`ESC O A` 等），不再是普通形式（`ESC [ A`）。此前无论模式如何
/// 都只发普通形式——htop 开启 DECCKM 后收到的 `ESC [ A` 对不上它的解析器，会把转义序列
/// 拆开当成散落的普通字符处理，其中 `[`/`]` 恰好是 htop 的「降/升 nice」快捷键，表现为
/// 「按上下键变成改了 NI」。同理受影响的不止 htop，任何按标准 DECCKM 协议走的全屏程序都会。
pub(super) fn encode_key(key: Key, mods: Modifiers, app_cursor: bool, out: &mut Vec<u8>) {
    // Ctrl+Shift+C/V 保留给复制/粘贴，不作为终端输入
    if (mods.ctrl || mods.command) && mods.shift && matches!(key, Key::C | Key::V | Key::F) {
        return;
    }

    // 光标/编辑键（方向键、Home/End）：无修饰时按 DECCKM 发 SS3 或 CSI 短形式；带修饰时统一
    // 发 `CSI 1;<m> X` 长形式——xterm 惯例，即便处于应用光标模式，带修饰的键也走 CSI 而非 SS3
    // （SS3 形式没有携带参数的位置）。缺了这个，Ctrl+←/→（按词跳转）、Shift+←/→（选择）
    // 会被降级成无修饰的普通方向键。
    if let Some(f) = cursor_final(key) {
        match mod_param(&mods) {
            None if app_cursor => {
                out.extend_from_slice(b"\x1bO");
                out.push(f);
            }
            None => {
                out.extend_from_slice(b"\x1b[");
                out.push(f);
            }
            Some(m) => {
                out.extend_from_slice(format!("\x1b[1;{m}").as_bytes());
                out.push(f);
            }
        }
        return;
    }

    // tilde 系（Insert/Delete/PageUp/PageDown）：`CSI <n> ~`，带修饰为 `CSI <n>;<m> ~`
    if let Some(n) = tilde_num(key) {
        match mod_param(&mods) {
            None => out.extend_from_slice(format!("\x1b[{n}~").as_bytes()),
            Some(m) => out.extend_from_slice(format!("\x1b[{n};{m}~").as_bytes()),
        }
        return;
    }

    // 功能键 F1..F12（此前完全没编码，按下什么都不发）
    if let Some(seq) = func_seq(key, mod_param(&mods)) {
        out.extend_from_slice(seq.as_bytes());
        return;
    }

    // Ctrl + 字母 -> 0x01..0x1a；Ctrl + 少数符号 -> 对应控制字符。
    // 同时按住 Alt 时再加 ESC 前缀（Meta 惯例，如 Alt+Ctrl+B）。
    if mods.ctrl {
        if let Some(c) = key_to_ascii_letter(key) {
            if mods.alt {
                out.push(0x1b);
            }
            out.push((c as u8 - b'a') + 1);
            return;
        }
        if let Some(c) = ctrl_symbol(key, &mods) {
            if mods.alt {
                out.push(0x1b);
            }
            out.push(c);
            return;
        }
    }

    match key {
        // Shift+Enter / Alt+Enter -> `ESC CR`：让 TUI 能把它和普通回车区分开，用作"换行但
        // 不提交"。裸 xterm 编码里 Shift+Enter 和 Enter 都是 CR、无法区分，所以 Claude Code
        // 的 `/terminal-setup` 就是去给 iTerm2/VSCode 写这条 `ESC CR` 映射——iShell 自己就是
        // 终端，直接内建，用户不用再配。
        Key::Enter => {
            if mods.shift || mods.alt {
                out.extend_from_slice(b"\x1b\r");
            } else {
                out.push(b'\r');
            }
        }
        // Shift+Tab -> `CSI Z`（back-tab）。此前发的是普通 `\t`，于是 Claude Code 里用来
        // 切换权限模式（plan / auto-accept）的 Shift+Tab 变成了普通 Tab 补全。
        Key::Tab => {
            if mods.shift {
                out.extend_from_slice(b"\x1b[Z");
            } else if mods.alt {
                out.extend_from_slice(b"\x1b\t");
            } else {
                out.push(b'\t');
            }
        }
        // Alt+Backspace = 删除前一个词（readline 惯例）
        Key::Backspace => {
            if mods.alt {
                out.extend_from_slice(&[0x1b, 0x7f]);
            } else {
                out.push(0x7f);
            }
        }
        Key::Escape => out.push(0x1b),
        _ => {
            // Alt + 可打印键 -> `ESC <char>`（Meta 惯例：Alt+B/F 按词移动、Alt+D 删词等）。
            // 无修饰的可打印字符不在这里处理——它们走 egui 的 Text 事件。
            if mods.alt && !mods.ctrl {
                if let Some(c) = key_to_ascii_letter(key) {
                    let ch = if mods.shift {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    };
                    out.push(0x1b);
                    out.push(ch as u8);
                } else if let Some(d) = key_to_ascii_digit(key) {
                    out.push(0x1b);
                    out.push(d);
                }
            }
        }
    }
}

/// xterm 修饰键参数：1 + shift(1) + alt(2) + ctrl(4)。无修饰返回 None（发不带参数的短形式）。
/// 只看 ctrl 不看 command：macOS 上 Cmd 是应用级快捷键（复制/粘贴由 egui 转成 Copy/Paste
/// 事件），不参与终端按键编码。
fn mod_param(mods: &Modifiers) -> Option<u8> {
    let mut m = 0u8;
    if mods.shift {
        m |= 1;
    }
    if mods.alt {
        m |= 2;
    }
    if mods.ctrl {
        m |= 4;
    }
    (m != 0).then_some(m + 1)
}

/// 方向键/Home/End 的 CSI 终结字符。
fn cursor_final(key: Key) -> Option<u8> {
    Some(match key {
        Key::ArrowUp => b'A',
        Key::ArrowDown => b'B',
        Key::ArrowRight => b'C',
        Key::ArrowLeft => b'D',
        Key::Home => b'H',
        Key::End => b'F',
        _ => return None,
    })
}

/// tilde 系按键的数字参数（`CSI <n> ~`）。
fn tilde_num(key: Key) -> Option<u8> {
    Some(match key {
        Key::Insert => 2,
        Key::Delete => 3,
        Key::PageUp => 5,
        Key::PageDown => 6,
        _ => return None,
    })
}

/// F1..F12 的转义序列。F1-F4 无修饰用 SS3（`ESC O P..S`），带修饰用 `CSI 1;<m> P..S`；
/// F5-F12 一律 `CSI <n> ~` / `CSI <n>;<m> ~`。
fn func_seq(key: Key, m: Option<u8>) -> Option<String> {
    let ss3 = |c: char| -> String {
        match m {
            None => format!("\x1bO{c}"),
            Some(m) => format!("\x1b[1;{m}{c}"),
        }
    };
    let tilde = |n: u8| -> String {
        match m {
            None => format!("\x1b[{n}~"),
            Some(m) => format!("\x1b[{n};{m}~"),
        }
    };
    Some(match key {
        Key::F1 => ss3('P'),
        Key::F2 => ss3('Q'),
        Key::F3 => ss3('R'),
        Key::F4 => ss3('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        _ => return None,
    })
}

/// Ctrl + 符号 -> 控制字符。
fn ctrl_symbol(key: Key, mods: &Modifiers) -> Option<u8> {
    Some(match key {
        Key::Space => 0x00,        // Ctrl+Space = NUL（set-mark）
        Key::OpenBracket => 0x1b,  // Ctrl+[ = ESC
        Key::Backslash => 0x1c,    // Ctrl+\ = FS（SIGQUIT）
        Key::CloseBracket => 0x1d, // Ctrl+] = GS
        Key::Slash => 0x1f,        // Ctrl+/ = US（readline/编辑器的撤销）
        // Ctrl+_ 实际是 Ctrl+Shift+-，同样是 US。这里**必须**要求 shift：egui 内建的
        // `zoom_with_keyboard`（默认开启）把 `COMMAND + Minus` 绑成界面缩小，而 COMMAND 在
        // Linux/Windows 上就是 Ctrl——若把裸 Ctrl+- 也映射成 0x1f，按一下会同时缩小界面
        // 和往终端发撤销。带 shift 的组合不匹配那条缩放快捷键，不会冲突。
        Key::Minus if mods.shift => 0x1f,
        _ => return None,
    })
}

fn key_to_ascii_digit(key: Key) -> Option<u8> {
    Some(match key {
        Key::Num0 => b'0',
        Key::Num1 => b'1',
        Key::Num2 => b'2',
        Key::Num3 => b'3',
        Key::Num4 => b'4',
        Key::Num5 => b'5',
        Key::Num6 => b'6',
        Key::Num7 => b'7',
        Key::Num8 => b'8',
        Key::Num9 => b'9',
        _ => return None,
    })
}

fn key_to_ascii_letter(key: Key) -> Option<char> {
    use Key::*;
    let c = match key {
        A => 'a',
        B => 'b',
        C => 'c',
        D => 'd',
        E => 'e',
        F => 'f',
        G => 'g',
        H => 'h',
        I => 'i',
        J => 'j',
        K => 'k',
        L => 'l',
        M => 'm',
        N => 'n',
        O => 'o',
        P => 'p',
        Q => 'q',
        R => 'r',
        S => 's',
        T => 't',
        U => 'u',
        V => 'v',
        W => 'w',
        X => 'x',
        Y => 'y',
        Z => 'z',
        _ => return None,
    };
    Some(c)
}

/// 编码一个鼠标事件为终端字节流。`cb` 为按钮码（含修饰位/移动位/滚轮位）。
/// `col`/`row` 为 0 基屏幕坐标，内部转 1 基。`press` 仅影响 SGR 的 M/m。
pub(super) fn encode_mouse(
    enc: vt100::MouseProtocolEncoding,
    cb: u8,
    col: u16,
    row: u16,
    press: bool,
    out: &mut Vec<u8>,
) {
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

#[cfg(test)]
mod encode_tests {
    use super::encode_key;
    use egui::{Key, Modifiers};

    fn enc(key: Key, mods: Modifiers, app_cursor: bool) -> Vec<u8> {
        let mut out = Vec::new();
        encode_key(key, mods, app_cursor, &mut out);
        out
    }
    fn s(key: Key, mods: Modifiers, app_cursor: bool) -> String {
        String::from_utf8_lossy(&enc(key, mods, app_cursor)).into_owned()
    }
    fn ctrl() -> Modifiers {
        Modifiers { ctrl: true, ..Default::default() }
    }
    fn shift() -> Modifiers {
        Modifiers { shift: true, ..Default::default() }
    }
    fn alt() -> Modifiers {
        Modifiers { alt: true, ..Default::default() }
    }

    /// DECCKM（应用光标键模式）：htop/vim/less 这类 ncurses 程序初始化时会开它，之后期望
    /// 方向键是 SS3（`ESC O A`）而不是 `ESC [ A`。发错形式的后果不是"没反应"，而是对端把
    /// 转义序列拆成散字符——`[`/`]` 恰好是 htop 的降/升 nice，表现成「按上下键改了 NI」。
    /// 这条注释在源码里，但没有任何测试守着它。
    #[test]
    fn arrows_follow_the_application_cursor_mode() {
        let none = Modifiers::default();
        assert_eq!(s(Key::ArrowUp, none, false), "\x1b[A");
        assert_eq!(s(Key::ArrowUp, none, true), "\x1bOA", "DECCKM 下必须发 SS3");
        assert_eq!(s(Key::ArrowDown, none, true), "\x1bOB");
        assert_eq!(s(Key::ArrowRight, none, true), "\x1bOC");
        assert_eq!(s(Key::ArrowLeft, none, true), "\x1bOD");
        assert_eq!(s(Key::Home, none, true), "\x1bOH");
        assert_eq!(s(Key::End, none, true), "\x1bOF");
    }

    /// 带修饰的方向键**即使在 DECCKM 下也走 CSI 长形式**——SS3 没有携带参数的位置。
    /// 少了这条，Ctrl+←/→（按词跳转）、Shift+←/→（选择）会被降级成普通方向键。
    #[test]
    fn modified_arrows_always_use_the_csi_long_form() {
        for app_cursor in [false, true] {
            assert_eq!(s(Key::ArrowLeft, ctrl(), app_cursor), "\x1b[1;5D");
            assert_eq!(s(Key::ArrowRight, shift(), app_cursor), "\x1b[1;2C");
            assert_eq!(s(Key::ArrowUp, alt(), app_cursor), "\x1b[1;3A");
        }
    }

    /// xterm 的修饰位编码：shift=1 alt=2 ctrl=4，参数是位和 +1。组合键（Ctrl+Shift 等）
    /// 一旦算错，对端收到的是另一个键，且不会报错、只会"行为不对"。
    #[test]
    fn modifier_parameter_follows_the_xterm_bitmask() {
        let cs = Modifiers { ctrl: true, shift: true, ..Default::default() };
        let ca = Modifiers { ctrl: true, alt: true, ..Default::default() };
        let all = Modifiers { ctrl: true, alt: true, shift: true, ..Default::default() };
        assert_eq!(s(Key::ArrowUp, cs, false), "\x1b[1;6A"); // 1|4 +1 = 6
        assert_eq!(s(Key::ArrowUp, ca, false), "\x1b[1;7A"); // 2|4 +1 = 7
        assert_eq!(s(Key::ArrowUp, all, false), "\x1b[1;8A"); // 1|2|4 +1 = 8
    }

    /// tilde 系（Insert/Delete/PageUp/PageDown）：`CSI n ~`，带修饰 `CSI n;m ~`。
    #[test]
    fn tilde_keys_encode_their_number() {
        let none = Modifiers::default();
        assert_eq!(s(Key::Insert, none, false), "\x1b[2~");
        assert_eq!(s(Key::Delete, none, false), "\x1b[3~");
        assert_eq!(s(Key::PageUp, none, false), "\x1b[5~");
        assert_eq!(s(Key::PageDown, none, false), "\x1b[6~");
        assert_eq!(s(Key::Delete, ctrl(), false), "\x1b[3;5~");
    }

    /// Ctrl+字母 → 0x01..0x1a；同时按 Alt 再加 ESC 前缀（Meta 惯例）。
    /// Ctrl+C 是 0x03——这条要是坏了，用户就中断不了任何东西。
    #[test]
    fn ctrl_letters_map_to_control_characters() {
        assert_eq!(enc(Key::C, ctrl(), false), vec![0x03]);
        assert_eq!(enc(Key::A, ctrl(), false), vec![0x01]);
        assert_eq!(enc(Key::Z, ctrl(), false), vec![0x1a]);
        assert_eq!(enc(Key::D, ctrl(), false), vec![0x04]);
        // Alt+Ctrl+B：ESC 前缀 + 0x02
        let ca = Modifiers { ctrl: true, alt: true, ..Default::default() };
        assert_eq!(enc(Key::B, ca, false), vec![0x1b, 0x02]);
    }

    /// Ctrl+Shift+C/V/F 留给复制/粘贴/查找，**不能**当终端输入发出去。
    /// 发出去的后果是：想复制却给远端发了 Ctrl+C，把正在跑的命令打断。
    #[test]
    fn ctrl_shift_cvf_are_reserved_for_the_ui() {
        let cs = Modifiers { ctrl: true, shift: true, ..Default::default() };
        for k in [Key::C, Key::V, Key::F] {
            assert!(
                enc(k, cs, false).is_empty(),
                "Ctrl+Shift+{k:?} 不该发到终端——它是复制/粘贴/查找的快捷键"
            );
        }
        // 但普通 Ctrl+C 必须照发
        assert_eq!(enc(Key::C, ctrl(), false), vec![0x03]);
    }

    /// Shift+Enter / Alt+Enter → `ESC CR`：让 TUI 能把它和普通回车区分开（"换行但不提交"）。
    /// 裸 xterm 编码里两者都是 CR、无法区分，Claude Code 的 /terminal-setup 就是去给别的
    /// 终端写这条映射——iShell 自己内建，用户不用配。
    #[test]
    fn shift_enter_is_distinguishable_from_plain_enter() {
        let none = Modifiers::default();
        assert_eq!(enc(Key::Enter, none, false), b"\r".to_vec());
        assert_eq!(enc(Key::Enter, shift(), false), b"\x1b\r".to_vec());
        assert_eq!(enc(Key::Enter, alt(), false), b"\x1b\r".to_vec());
    }

    /// 功能键曾经完全没编码（按下什么都不发）。F1-F4 无修饰走 SS3，F5+ 走 `CSI n ~`。
    #[test]
    fn function_keys_are_encoded() {
        let none = Modifiers::default();
        assert_eq!(s(Key::F1, none, false), "\x1bOP");
        assert_eq!(s(Key::F4, none, false), "\x1bOS");
        assert_eq!(s(Key::F5, none, false), "\x1b[15~");
        assert_eq!(s(Key::F12, none, false), "\x1b[24~");
        for k in [Key::F1, Key::F5, Key::F12] {
            assert!(!enc(k, none, false).is_empty(), "{k:?} 不该什么都不发");
        }
    }

    /// **不变量**：编码结果要么为空（被 UI 保留的快捷键），要么是一个完整的转义序列或
    /// 控制字符——绝不能发出**半截**序列（只有 `ESC` 或以 `ESC [` 结尾）。半截序列会让
    /// 对端的状态机一直等下去，把随后的正常输入一起吃掉。
    #[test]
    fn no_key_ever_emits_a_truncated_escape_sequence() {
        let all_keys = [
            Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight,
            Key::Home, Key::End, Key::Insert, Key::Delete, Key::PageUp, Key::PageDown,
            Key::Enter, Key::Tab, Key::Escape, Key::Backspace, Key::Space,
            Key::A, Key::C, Key::Z, Key::Num0, Key::Num9,
            Key::F1, Key::F4, Key::F5, Key::F12,
        ];
        let mod_sets = [
            Modifiers::default(),
            ctrl(),
            shift(),
            alt(),
            Modifiers { ctrl: true, shift: true, ..Default::default() },
            Modifiers { ctrl: true, alt: true, ..Default::default() },
            Modifiers { ctrl: true, alt: true, shift: true, ..Default::default() },
        ];
        for k in all_keys {
            for m in mod_sets {
                for app_cursor in [false, true] {
                    let bytes = enc(k, m, app_cursor);
                    if bytes.is_empty() {
                        continue; // 被 UI 保留，合法
                    }
                    // Esc 键本身就该发一个裸 ESC，这是它的正确编码，不是"半截序列"。
                    // （这条豁免是写完这个测试后被它自己逼出来的：第一版没有它，Esc 被误报。）
                    if !matches!(k, Key::Escape) {
                        assert_ne!(bytes, vec![0x1b_u8], "{k:?}+{m:?} 只发了一个裸 ESC");
                    }
                    assert!(
                        !bytes.ends_with(b"\x1b["),
                        "{k:?}+{m:?} 发出了半截 CSI：{bytes:?}"
                    );
                    assert!(
                        !bytes.ends_with(b"\x1bO"),
                        "{k:?}+{m:?} 发出了半截 SS3：{bytes:?}"
                    );
                }
            }
        }
    }
}
