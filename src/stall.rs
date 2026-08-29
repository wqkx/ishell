//! UI 线程「卡死」检测：抓到界面冻住，落一份日志，并自动关掉最可能的元凶。
//!
//! # 为什么需要它，以及为什么它不是一个普通的 watchdog
//!
//! X11 上把光标位置上报给输入法最终会走到 `XSetICValues`——**同步的 XIM 请求**，Xlib 发出去
//! 之后 `poll(timeout=-1)` 等回复，而 **Xlib 对 XIM 没有超时**。它跑在 winit 的事件循环线程
//! 上，也就是画界面的那个线程。fcitx 只要在某一次请求中途没了（崩溃、重启、远程桌面会话
//! 切换/重连），这个线程就永远停在那儿。用户实测抓到的栈：
//!
//! ```text
//! poll → _XReadEvents → XIfEvent → _XimRead → XSetICValues
//!      → winit::…::ImeContext::set_spot → eframe::run_native
//! ```
//!
//! 这不是 panic，[`crate::crash`] 的 `catch_unwind` 接不住；从别的线程也无法安全地把它拽
//! 回来。能做的只有两件事：**留下证据**（否则用户能提供的只有「它又卡死了」），以及
//! **让下一次启动不再踩同一个坑**——自动把「输入法候选框跟随光标」关掉，那条 `XSetICValues`
//! 就一次都不会再发出去（winit 只在坐标真的变了时才发，见 `store::load_ime_follow_caret`）。
//!
//! # 为什么不会误报
//!
//! egui 空闲时本来就几分钟不出一帧，「多久没出帧」单独看毫无意义。这里的探测**只在用户刚
//! 敲过键的窗口内进行**（[`ACTIVE_WINDOW`]）：那段时间里我们每 [`PROBE_EVERY`] 主动
//! `request_repaint()` 一次唤醒事件循环，活着的循环会在毫秒级出帧、把 `last_frame` 推新。
//! 用户停手 [`ACTIVE_WINDOW`] 之后探测自动停止，空闲期一次多余的唤醒都没有——而在探测
//! 期间 egui 本来就在为光标闪烁重绘，等于零额外成本。
//! 已知会把事件循环整个堵住、但**不是**卡死的调用（原生文件对话框）用 [`blocking`] 圈掉。
//!
//! # 残留的不确定性（所以这里只报告，不动任何设置）
//!
//! 探测依赖「后台线程 `request_repaint()` 能唤醒事件循环」。而 `frame.rs` 里记着一条实测：
//! eframe 停在 `ControlFlow::Wait` 时，**跨线程的唤醒会丢**（曾让一条 MCP 请求干等 157 秒）。
//! 连丢 [`STALL_AFTER`]/[`PROBE_EVERY`] 次才会误报，概率不高但不为零。
//! 顺带一提：`request_repaint()` 在 egui 里就是 `request_repaint_after(Duration::ZERO)`，
//! 换成后者没有任何区别，别在这上面白费力气。
//!
//! 正因为判定只是「一段时间没出帧」而非「证明线程卡住了」，本模块**只落一份诊断**，
//! 不替用户改任何设置——日志里给出该试什么，由用户定夺。

/// 窗口可见时卡死的那套说辞：用户实测抓到的栈就是这个形状。
const XIM_LIKELY: &str = "卡在输入法的同步 XIM 请求上（Xlib 对 XIM 没有超时）。典型栈：\n\
     \x20         poll → _XReadEvents → XIfEvent → _XimRead → XSetICValues\n\
     \x20         → winit::…::ImeContext::set_spot → eframe::run_native";

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 用户最后一次输入之后，继续探测多久。超过就认为人已经离开，停止探测。
const ACTIVE_WINDOW: Duration = Duration::from_secs(30);
/// 探测间隔：每隔这么久主动请求一帧。
const PROBE_EVERY: Duration = Duration::from_secs(2);
/// 探测窗口内多久没出过一帧就判定卡死。取 `PROBE_EVERY` 的数倍，留足调度抖动的余量。
const STALL_AFTER: Duration = Duration::from_secs(8);

/// 进程启动时刻，所有时间戳都以「距它多少毫秒」存进 `AtomicU64`（`Instant` 不能原子存）。
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
/// 只报一次：卡死之后每 2 秒往日志里灌一条毫无意义。
///
/// 按**原因**分开记，不是一个总闸：窗口最小化时的卡死不采取任何处置（见 [`on_stall`]），
/// 用一个总闸的话，它会把后面一次真正的、窗口可见的 XIM 卡死一并吞掉——那次既不写日志也
/// 不关跟随，正好回到「它又卡死了，一行记录都没有」这个本模块要根除的状态。
static REPORTED_HIDDEN: AtomicBool = AtomicBool::new(false);
static REPORTED_VISIBLE: AtomicBool = AtomicBool::new(false);
/// 最后一帧时窗口是不是最小化/被遮挡。卡死时用来判断该不该把账算到输入法头上——
/// 最小化状态下卡住更可能是窗口/绘制路径（eframe 仍会给最小化的窗口画帧并交换缓冲，
/// 而合成器可能已经不再给它调度垂直同步了），跟 XIM 无关。
static WAS_HIDDEN: AtomicBool = AtomicBool::new(false);
/// 当前有多少个「已知会把事件循环整个堵住」的调用在进行中（原生文件对话框）。
/// 大于 0 时探测一律不作数——见 [`blocking`]。
static BLOCKING: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 圈住一段**已知会阻塞事件循环**的调用，圈内不做卡死判定。
///
/// 典型就是原生文件选择框：`rfd` 在 Linux 上走 xdg-portal + pollster，是**同步**调用，
/// 而所有调用点都在绘制代码里（事件循环线程）。用户点开「上传文件」慢慢翻目录的那十几秒，
/// 界面确实一帧都不出——但那不是卡死。少了这个守卫，看门狗会当场写一条假的卡死记录；
/// 更糟的是它会把本次运行的「已报告」闩锁掉，之后一次**真的**卡死反而不再被记录，
/// 正好和这个模块的目的相反。
///
/// 用法：`let _guard = crate::stall::blocking();` 放在阻塞调用之前。析构时顺带把
/// 「上次出帧时刻」推到现在——否则对话框一关，看门狗看到的仍是十几秒前的旧时间戳，
/// 下一轮探测（还没来得及出帧）照样误报。
pub fn blocking() -> BlockingGuard {
    BLOCKING.fetch_add(1, Ordering::Relaxed);
    BlockingGuard
}

/// [`blocking`] 返回的守卫，析构即解除。
pub struct BlockingGuard;

impl Drop for BlockingGuard {
    fn drop(&mut self) {
        LAST_FRAME_MS.store(now_ms(), Ordering::Relaxed);
        BLOCKING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 每帧调用：记下「出帧了」，以及这一帧有没有用户输入。
///
/// `had_input` 决定探测窗口是否续期——只有真的在敲键盘/动鼠标才值得探，
/// 光标闪烁那种自发重绘不算。
pub fn note_frame(had_input: bool, hidden: bool) {
    let t = now_ms();
    LAST_FRAME_MS.store(t, Ordering::Relaxed);
    WAS_HIDDEN.store(hidden, Ordering::Relaxed);
    if had_input {
        LAST_INPUT_MS.store(t, Ordering::Relaxed);
    }
}

/// 判定是否该报告卡死。纯函数，便于测试——把条件写在这里而不是散在线程循环里，
/// 是因为「什么时候算卡死」正是这个模块唯一容易写错的地方。
///
/// - `now` / `last_frame` / `last_input` 均为距进程启动的毫秒数。
fn should_report(now: u64, last_frame: u64, last_input: u64, blocking: bool) -> bool {
    // 我们自己知道有东西在堵事件循环（原生文件对话框）：不出帧是预期内的，不报。
    if blocking {
        return false;
    }
    // 探测窗口外（用户早就停手了）：不出帧是正常的空闲，不报。
    if now.saturating_sub(last_input) > ACTIVE_WINDOW.as_millis() as u64 {
        return false;
    }
    // 窗口内，我们每 PROBE_EVERY 都在主动请求出帧；这么久还没出，就是卡住了。
    now.saturating_sub(last_frame) > STALL_AFTER.as_millis() as u64
}

/// 启动看门狗线程（只启动一次）。`ctx` 用于主动请求重绘来探测事件循环是否还活着。
pub fn spawn(ctx: egui::Context) {
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = START.get_or_init(Instant::now);
    let t = now_ms();
    LAST_FRAME_MS.store(t, Ordering::Relaxed);
    LAST_INPUT_MS.store(t, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("ishell-stall".into())
        .spawn(move || loop {
            std::thread::sleep(PROBE_EVERY);
            let now = now_ms();
            let last_input = LAST_INPUT_MS.load(Ordering::Relaxed);
            if now.saturating_sub(last_input) > ACTIVE_WINDOW.as_millis() as u64 {
                continue; // 用户停手了，不探——空闲期一次多余唤醒都不做
            }
            let blocking = BLOCKING.load(Ordering::Relaxed) > 0;
            if should_report(
                now,
                LAST_FRAME_MS.load(Ordering::Relaxed),
                last_input,
                blocking,
            ) {
                let hidden = WAS_HIDDEN.load(Ordering::Relaxed);
                let latch = if hidden {
                    &REPORTED_HIDDEN
                } else {
                    &REPORTED_VISIBLE
                };
                if !latch.swap(true, Ordering::Relaxed) {
                    on_stall(hidden);
                }
                continue; // 已经卡住了，别再请求重绘
            }
            // 唤醒事件循环：活着的话下一帧马上会把 last_frame 推新。
            ctx.request_repaint();
        })
        .ok();
}

/// 判定卡死之后做的事：**只落一份诊断**，不去动用户的任何设置。
///
/// 早先这里会自动把「输入法候选框跟随光标」关掉。撤掉了，两个理由：① 判定再怎么加护栏也
/// 只是「一段时间没出帧」，静默改用户设置的代价配不上这个确定性（真出过一次误报就知道了）；
/// ② 那条归因本身也没被证实——最小化时更像卡在绘制/缓冲交换上（见 `main.rs` 的
/// `ISHELL_NO_VSYNC`），关输入法跟随既没用又冤枉。日志里把两条候选都写清楚，由用户定夺。
fn on_stall(hidden: bool) {
    let follow = crate::store::load_ime_follow_caret();
    log::error!(
        "UI 线程已 {}s 没有出帧（用户刚刚还在输入）。窗口最小化/被遮挡={hidden}，\
         ime_follow_caret={follow}",
        STALL_AFTER.as_secs()
    );
    write_stall_log(hidden, follow);
}

/// 往 `crash.log` 里追加一条卡死记录——和 panic 走同一份文件，用户交一份就够。
///
/// 主线程此刻正卡在 Xlib 里，这里是**另一个线程**在写，只能碰文件系统，不能碰任何 UI 状态。
fn write_stall_log(hidden: bool, follow_was_on: bool) {
    use std::io::Write;
    let Some(path) = crate::store::crash_log_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let Ok(mut f) = opts.open(&path) else {
        return;
    };
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 「最可能」和「已处理」必须讲同一个故事：早先的版本无论如何都印那段 XIM 栈，而窗口
    // 最小化时紧接着又说「跟输入法无关」，同一份日志自相矛盾，反而误导排查。
    // 「最可能」与「可以试」必须讲同一个故事，而且都只是**建议**——本模块不替用户改任何设置。
    let (likely, action) = if hidden {
        (
            "窗口当时是最小化/被遮挡的，更可能卡在窗口与绘制路径上：合成器不再给一个图标化的\n\
             \x20         窗口调度垂直同步，而 eframe 仍会给它画帧并交换缓冲（原生平台上 is_visible\n\
             \x20         恒为真），缓冲交换就会一直等一个不来的帧回调。",
            "用 ISHELL_NO_VSYNC=1 启动再试一次（交换缓冲改成不等垂直同步）。与输入法无关。",
        )
    } else if follow_was_on {
        (
            XIM_LIKELY,
            "在设置里取消勾选「输入法候选框跟随光标」再试一次——关掉之后 iShell 恒定上报同一个\n\
             \x20         坐标，那条 XSetICValues 一次都不会发出去。",
        )
    } else {
        (
            XIM_LIKELY,
            "「输入法候选框跟随光标」本来就是关的——这次卡死另有原因，请抓一份栈。",
        )
    };
    let _ = writeln!(
        f,
        "\n=== ishell {} UI 线程卡死 ===\n\
         unix_time: {secs}\n\
         现象:     用户刚刚还在输入，但界面已经 {}s 没有出帧（看门狗每 2s 主动请求重绘，\
         活着的事件循环不可能这么久不响应）。\n\
         当时状态: 窗口最小化/被遮挡={hidden}，输入法候选框跟随光标={follow_was_on}\n\
         最可能:   {likely}\n\
         已处理:   {action}\n\
         想确认:   下次卡住时跑 gdb -p $(pidof ishell) -batch -ex \"thread apply all bt\"",
        crate::version::VERSION,
        STALL_AFTER.as_secs()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: u64 = ACTIVE_WINDOW.as_millis() as u64;
    #[allow(dead_code)]
    const STALL: u64 = STALL_AFTER.as_millis() as u64;

    /// 探测窗口内、久久不出帧 → 判定卡死。这是这个模块存在的理由。
    #[test]
    fn a_frozen_frame_loop_right_after_typing_is_reported() {
        // 刚敲过键（1s 前），但已经 STALL+1s 没出帧
        assert!(should_report(20_000, 20_000 - STALL - 1_000, 19_000, false));
    }

    /// **空闲不能误报**：用户早就停手了，不出帧是 egui 的正常行为（几分钟不出帧很常见）。
    /// 这条是 crash-diagnostics 那份笔记里「没做 watchdog 是刻意的」担心的那个假阳性，
    /// 探测窗口就是为它加的。
    #[test]
    fn a_long_idle_session_is_never_reported() {
        // 上次输入是 10 分钟前，之后一帧都没出过——完全正常
        assert!(!should_report(600_000, 100, 100, false));
        // 刚好落在窗口边界之外
        assert!(!should_report(WIN + 2, 0, 0, false));
    }

    /// 窗口内但出帧正常（看门狗每 2s 请求一次重绘，活着就会出帧）→ 不报。
    #[test]
    fn a_live_loop_inside_the_probe_window_is_not_reported() {
        assert!(!should_report(20_000, 19_000, 19_000, false));
        // 恰好等于阈值也不报（严格大于才算）
        assert!(!should_report(20_000, 20_000 - STALL, 19_500, false));
    }

    /// **原生文件对话框不是卡死。** `rfd` 在 Linux 上是同步调用，而所有调用点都在绘制代码里
    /// （事件循环线程）；用户点开「上传文件」慢慢翻目录的那十几秒，界面确实一帧都不出。
    /// 少了这条，看门狗会写一条假的卡死记录，还会把本次运行的「已报告」闩锁掉——之后一次
    /// **真的**卡死反而不再被记录，正好和这个模块的目的相反。
    #[test]
    fn a_blocking_native_dialog_is_never_reported_as_a_stall() {
        // 其余条件全部满足（刚敲过键、久久不出帧），只差 blocking 这一条
        assert!(should_report(20_000, 20_000 - STALL - 1_000, 19_000, false));
        assert!(
            !should_report(20_000, 20_000 - STALL - 1_000, 19_000, true),
            "文件对话框开着的时候不出帧是预期内的，不能报成卡死"
        );
    }

    /// 时间戳倒挂（时钟/调度抖动）不能算成卡死。
    #[test]
    fn out_of_order_timestamps_do_not_report() {
        assert!(!should_report(1_000, 5_000, 5_000, false));
    }
}
