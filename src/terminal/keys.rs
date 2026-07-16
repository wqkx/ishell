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
