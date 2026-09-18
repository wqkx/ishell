//! 全局界面状态（原子量支持的进程级开关）：缩放、侧栏/文件区折叠、关于窗、OSC7 同意。

/// 是否已同意「向 shell 注入 OSC 7 上报工作目录」。同意一次后持久化，后续静默注入。
/// 用全局原子（启动时从 store 载入），便于 Session 的连接回调直接读取。
pub(crate) static OSC7_CONSENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn osc7_consent() -> bool {
    OSC7_CONSENT.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_osc7_consent(v: bool) {
    OSC7_CONSENT.store(v, std::sync::atomic::Ordering::Relaxed);
    crate::store::save_osc7_consent(v);
}

/// 注入到交互式 shell 的 OSC 7 上报片段（bash 用 PROMPT_COMMAND，zsh 用 precmd）。
/// 仅作用于当前会话、不写 rc、不持久化；前导空格尽量不进 history。
pub(crate) const OSC7_SNIPPET: &str = r#" __ishell_cwd(){ printf '\033]7;file://localhost%s\007' "$PWD"; }; if [ -n "$ZSH_VERSION" ]; then autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __ishell_cwd 2>/dev/null; else case "$PROMPT_COMMAND" in *__ishell_cwd*) ;; *) PROMPT_COMMAND="__ishell_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}";; esac; fi; __ishell_cwd"#;

/// AI 专用会话注入的片段：OSC 7 工作目录上报 **+ shell 集成（OSC 133）**，一次打字装两件事。
///
/// 为什么 AI 会话要多装 OSC 133：AI 命令的「跑完没有、退出码多少」此前靠往 tty 里多打一行
/// `printf` 哨兵，而那一行和用户数据走同一条流——`cat`/REPL 会把它当 stdin 吃掉、中断会把它
/// 留在输入队列里、它自己的回显还要费劲吞掉。OSC 133 由 shell 在执行前/打印提示符时自己发，
/// 不经过任何程序的 stdin，退出码取自 shell 的 `$?`，上面那一族问题整体消失
/// （见 `terminal::CaptureMode`）。
///
/// 兼容性：zsh 走 preexec/precmd 钩子；bash 的「开始执行」用 PS0（bash ≥ 4.4），老 bash 只发
/// 得出 `D`——捕获端对「只有 D」也能收束，退化的代价只是输出里多带一段命令行回显。
/// 与 `OSC7_SNIPPET` 一样：仅作用于当前会话、不写 rc、前导空格尽量不进 history。
pub(crate) const AI_SESSION_SNIPPET: &str = r#" __ishell_cwd(){ printf '\033]7;file://localhost%s\007' "$PWD"; }; __ishell_end(){ __ishell_e=$?; printf '\033]133;D;%d\007' "$__ishell_e"; return $__ishell_e; }; if [ -n "$ZSH_VERSION" ]; then autoload -Uz add-zsh-hook 2>/dev/null; __ishell_pre(){ printf '\033]133;C\007'; }; __ishell_post(){ __ishell_end; __ishell_cwd; }; add-zsh-hook preexec __ishell_pre 2>/dev/null; add-zsh-hook precmd __ishell_post 2>/dev/null; else PS0=$'\033]133;C\007'; case "$PROMPT_COMMAND" in *__ishell_end*) ;; *) PROMPT_COMMAND="__ishell_end; __ishell_cwd${PROMPT_COMMAND:+;$PROMPT_COMMAND}";; esac; fi; __ishell_cwd"#;

// ===== 全局视图状态（折叠监控栏/文件栏、界面缩放）=====
// 设为进程级全局，便于侧栏背景层与各子控件（进程行/网卡/IP 等）共用同一右键菜单，
// 避免「右键到子控件上弹不出完整菜单」的不一致。
pub(crate) static SIDEBAR_COLLAPSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) static FILES_COLLAPSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) static ZOOM_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0); // 0 哨兵=未初始化(按 1.0)

/// 「关于」弹框是否显示（右键菜单触发；自由函数态，与折叠/缩放一致用全局）。
pub(crate) static ABOUT_OPEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn about_open() -> bool {
    ABOUT_OPEN.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_about_open(v: bool) {
    ABOUT_OPEN.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn sidebar_collapsed() -> bool {
    SIDEBAR_COLLAPSED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_sidebar_collapsed(v: bool) {
    SIDEBAR_COLLAPSED.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn files_collapsed() -> bool {
    FILES_COLLAPSED.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn set_files_collapsed(v: bool) {
    FILES_COLLAPSED.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn ui_zoom() -> f32 {
    let b = ZOOM_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if b == 0 {
        1.0
    } else {
        f32::from_bits(b)
    }
}

pub(crate) fn set_ui_zoom(z: f32) {
    // 量化到 5% 网格、夹在 70%–200%，变化才持久化
    let z = ((z * 20.0).round() / 20.0).clamp(0.7, 2.0);
    if (z - ui_zoom()).abs() > f32::EPSILON {
        ZOOM_BITS.store(z.to_bits(), std::sync::atomic::Ordering::Relaxed);
        crate::store::save_zoom(z);
    }
}

/// 启动时把已保存的缩放载入全局。
pub(crate) fn init_view_state() {
    ZOOM_BITS.store(
        crate::store::load_zoom().to_bits(),
        std::sync::atomic::Ordering::Relaxed,
    );
    OSC7_CONSENT.store(
        crate::store::load_osc7_consent(),
        std::sync::atomic::Ordering::Relaxed,
    );
}
