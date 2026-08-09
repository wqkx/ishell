//! 崩溃诊断与「一帧 panic 不带走整个应用」的兜底。
//!
//! # 背景
//!
//! iShell 是 GUI 程序，绝大多数用户从桌面项启动，stderr 没人看得到。以前一旦 UI 线程
//! panic，用户能提供的全部信息就是「它崩了」「有时候卡死」——既没有栈、也没有位置，
//! 只能靠猜。这个模块做两件事：
//!
//! 1. [`install_panic_hook`]：把每次 panic 的位置、消息、线程名、版本号和栈追加写到
//!    `~/.config/ishell/crash.log`（同时照常走 log 和默认 hook，不改变原有行为）。
//! 2. [`on_frame_panic`] / [`on_frame_ok`]：配合 `App::ui` 里的 `catch_unwind`，让**一帧
//!    绘制中的 panic** 不再终止进程——对 SSH 客户端来说，崩一次意味着所有会话断开、
//!    编辑器里没保存的内容全丢，代价远大于「这一帧少画了点东西」。
//!
//! # 这个兜底不是万能的，别把它当成不用修 bug 的理由
//!
//! `catch_unwind` 只能接住**能展开**的 panic，接不住 abort（`panic=abort`、双重 panic、
//! OOM），也修不了「卡死」——挂起不是 panic。而且被接住的那一帧状态是半截的，界面可能
//! 短暂错乱。它的定位是「把一次崩溃降级成一次闪烁 + 一条日志」，真正的修法永远是让
//! 那段代码不 panic（例如 IME 偏移一律走 `crate::ui::ime_safe`）。
//!
//! 依赖 `Cargo.toml` 里的 `panic = "unwind"`（那里已有注释说明不要改成 abort）。

use std::sync::atomic::{AtomicU32, Ordering};

/// 连续多少帧都 panic 就不再兜底。
///
/// 一次性事故值得接住；连着几十帧都崩说明是稳定复现的坏状态，再接下去只是让一个
/// 画不出东西的窗口空转烧 CPU（用户看到的正是「卡死」），不如按原样崩掉，留下正常的
/// 退出路径和一份完整日志。
const GIVE_UP_AFTER: u32 = 30;

/// 累计多少次就不再兜底——**不管是否连续**。
///
/// 只看连续次数会漏掉最难受的一种：隔帧崩（panic → 恢复 → panic → …）。它每次都把
/// 连续计数清零，永远够不到 [`GIVE_UP_AFTER`]，于是应用带着一个半截界面无限空转烧 CPU，
/// 正是用户口中的「卡死」。累计上限把这条路一并封死。
const GIVE_UP_TOTAL: u32 = 100;

// 编译期守住两条上限的形状：兜底必须有限，且两条都得在——只留连续上限会被「隔帧崩」
// 绕过（每次恢复都把连续计数清零），应用就带着半截界面无限空转。
const _: () = assert!(GIVE_UP_AFTER >= 1 && GIVE_UP_AFTER <= 100);
const _: () = assert!(GIVE_UP_TOTAL >= GIVE_UP_AFTER && GIVE_UP_TOTAL <= 1000);

static CONSECUTIVE: AtomicU32 = AtomicU32::new(0);
static TOTAL: AtomicU32 = AtomicU32::new(0);
/// 用户点「知道了」时的累计次数。存计数而不是 bool：bool 是单向开关，点掉一次之后
/// 后续 99 次恢复都悄无声息，然后在累计上限那一刻毫无预兆地把整个进程带走。存计数就能在
/// 又崩了之后重新弹出来——每一次都是「状态可能已经不一致」的新警告，用户有机会先存盘。
static DISMISSED_AT: AtomicU32 = AtomicU32::new(0);

/// 安装 panic 钩子：日志 + 落盘，并保留默认行为（stderr 输出）。
///
/// 钩子自身绝不能 panic——里面所有 IO 错误一律忽略，不 unwrap。
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let msg = payload_str(info.payload());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        log::error!("panic at {loc} [thread {thread}]: {msg}");
        append_crash_log(&loc, &thread, &msg);
        default_hook(info);
    }));
}

/// 建配置目录，unix 上强制 0700——目录默认 0755，而里面装的是 crash.log 和已有的密钥/连接。
fn create_private_dir(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::create_dir_all(dir);
    }
}

/// 以 0600 打开（必要时创建）追加写文件。
///
/// 两步都要：`mode()` 只在**创建那一刻**生效，保证没有 0644 窗口；`set_permissions` 兜住
/// 「文件已经存在」的情况——比如旧版本或别的 umask 下留下的一份 0644 crash.log。
fn open_private_append(path: &std::path::Path) -> Option<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Some(f)
}

/// 取 panic 载荷里的可读消息（`panic!` 的两种常见载荷类型）。
fn payload_str(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// 追加一条崩溃记录。超过 [`MAX_CRASH_LOG`] 就先截断重来——这份文件是给人看的诊断，
/// 不是审计日志，不能因为反复崩溃把用户磁盘吃满。
///
/// **权限必须是 0600**：panic 消息里可能带用户数据。本模块要处理的正是切片越界那一类，
/// 而 Rust 的 `str::slice_error_fail` 会把出错的字符串片段原样嵌进消息里——那可能是正在
/// 编辑的远端文件内容，或远端路径/文件名。多用户机器上落一个 0644 的文件等于把它们
/// 送给同机所有人。同 `store::crypto` 的既有约定（见 commit d0b9e3b「权限位显式掩码」）。
fn append_crash_log(loc: &str, thread: &str, msg: &str) {
    const MAX_CRASH_LOG: u64 = 256 * 1024;
    use std::io::Write;

    let Some(path) = crate::store::crash_log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        create_private_dir(dir);
    }
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_CRASH_LOG {
        let _ = std::fs::remove_file(&path);
    }
    let bt = std::backtrace::Backtrace::force_capture();
    let Some(mut f) = open_private_append(&path) else {
        return;
    };
    let _ = writeln!(
        f,
        "\n=== ishell {} panic ===\nlocation: {loc}\nthread:   {thread}\nmessage:  {msg}\n{bt}",
        crate::version::VERSION
    );
}

/// 本帧绘制正常结束：清零连续计数。
pub fn on_frame_ok() {
    CONSECUTIVE.store(0, Ordering::Relaxed);
}

/// 本帧绘制 panic 且已被接住。连续崩太多次则不再兜底，让它照常崩。
pub fn on_frame_panic(ctx: &egui::Context) {
    let n = CONSECUTIVE.fetch_add(1, Ordering::Relaxed) + 1;
    let total = TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= GIVE_UP_AFTER {
        panic!("连续 {n} 帧绘制都 panic，停止兜底（详见 crash.log）");
    }
    if total >= GIVE_UP_TOTAL {
        panic!("累计 {total} 帧绘制 panic，停止兜底（详见 crash.log）");
    }
    // 这一帧的界面是半截的，必须马上重画一帧把它补完整
    ctx.request_repaint();
}

/// 恢复提示这一帧该不该显示：崩过、且崩的次数比用户点「知道了」那一刻更多。
///
/// 单独成函数是为了能被测到——写在 `recovery_notice` 里就只能靠测试重抄一遍判断条件，
/// 那种测试永远为真，把条件写反了也照样通过。
fn notice_visible(total: u32, dismissed_at: u32) -> bool {
    total > 0 && total > dismissed_at
}

/// 发生过被接住的 panic 时，在界面上说明情况——否则用户只会看到「偶尔闪一下」，
/// 既不知道该保存退出，也不知道有日志可交。
pub fn recovery_notice(ctx: &egui::Context) {
    let total = TOTAL.load(Ordering::Relaxed);
    if !notice_visible(total, DISMISSED_AT.load(Ordering::Relaxed)) {
        return;
    }
    let path = crate::store::crash_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/ishell/crash.log".into());
    egui::Window::new(crate::i18n::tr("内部错误已恢复", "Recovered from an internal error"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 16.0))
        .show(ctx, |ui| {
            // `i18n::tr` 只收 &'static str，这里要插值，故按语言分支各自 format
            ui.label(match crate::i18n::current() {
                crate::i18n::Lang::Zh => format!(
                    "界面绘制发生了 {total} 次内部错误并已自动恢复。当前状态可能不完全正确，建议保存重要内容后重启 iShell。"
                ),
                crate::i18n::Lang::En => format!(
                    "The UI hit {total} internal error(s) and recovered automatically. State may be inconsistent — save your work and restart iShell."
                ),
            });
            ui.label(
                egui::RichText::new(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("详细日志：{path}"),
                    crate::i18n::Lang::En => format!("Details: {path}"),
                })
                .weak(),
            );
            ui.add_space(6.0);
            if ui.button(crate::i18n::tr("知道了", "Dismiss")).clicked() {
                DISMISSED_AT.store(total, Ordering::Relaxed);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_reads_both_common_panic_shapes() {
        let a: Box<dyn std::any::Any + Send> = Box::new("boom");
        let b: Box<dyn std::any::Any + Send> = Box::new(String::from("boom2"));
        let c: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(payload_str(&*a), "boom");
        assert_eq!(payload_str(&*b), "boom2");
        assert_eq!(payload_str(&*c), "<non-string panic payload>");
    }

    /// crash.log 里可能有正在编辑的远端文件片段和路径（`str::slice_error_fail` 会把出错的
    /// 字符串嵌进 panic 消息），多用户机器上绝不能是 0644。已存在的旧文件也要被收紧。
    #[cfg(unix)]
    #[test]
    fn crash_log_is_created_and_repaired_as_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ishell-crashperm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_private_dir(&dir);
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "配置目录必须 0700"
        );

        let p = dir.join("crash.log");
        assert!(open_private_append(&p).is_some());
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600,
            "新建时必须 0600"
        );

        // 旧版本/别的 umask 留下的 0644 文件，再次打开时要被收紧
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_private_append(&p).is_some());
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600,
            "已存在的宽权限文件必须被收紧"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「知道了」记的是当时的累计次数，不是一个单向开关：再崩就得再弹。
    /// 存 bool 的话，点掉一次之后剩下 99 次恢复全程无声，然后在累计上限那一刻
    /// 毫无预兆地把进程带走。
    #[test]
    fn dismissal_is_rearmed_by_a_later_panic() {
        assert!(!notice_visible(0, 0), "没崩过就不该显示");
        assert!(notice_visible(1, 0), "崩过且没点掉，必须显示");
        assert!(!notice_visible(3, 3), "点掉那一刻不该再显示");
        assert!(notice_visible(4, 3), "之后又崩了必须重新弹出来");
    }
}
