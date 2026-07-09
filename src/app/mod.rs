//! 应用主体：会话管理 + 顶部标签 + 三区布局（系统信息 / 终端 / 文件）。

mod types;
mod util;
mod view_state;
mod widgets;
pub(in crate::app) use types::{
    DirectFallback, DirectJob, DocKind, EditorState, EditorTab, FileClip, ForwardEntry, ForwardUi,
    ImageTab, ImageView, KbdPrompt, PendingPaste, Popups, ProcPopup, Relay, RelayPhase, SaveState,
    Shot, Snippets, TabBar, Transfer, Transfers, XferFilter,
};
#[allow(unused_imports)]
use util::*;
#[allow(unused_imports)]
use view_state::*;
#[allow(unused_imports)]
use widgets::*;
mod demo_init;
mod dialogs;
mod doc_parse;
mod doc_view;
mod editor_win;
mod editor_close;
mod editor_window_view;
mod file_actions;
mod frame;
mod frame_editor;
mod layout;
mod layout_tabs;
mod layout_body;
mod pending;
mod screenshot;
mod session;
mod session_events;
mod transfers;
mod windows;
pub(in crate::app) use session::{Session, XferSpec};
pub use widgets::view_context_menu;

use std::sync::{Arc, Mutex};

use egui::RichText;

use crate::proto::{ConflictPolicy, UiCommand};
use crate::theme::Palette;
use crate::ui::connect::ConnectForm;
use crate::ui::sidebar;

pub struct App {
    runtime: Arc<tokio::runtime::Runtime>,
    ctx: egui::Context,
    sessions: Vec<Session>,
    active: Option<usize>,
    /// 主窗口会话标签条的拖拽重排 + 滚动状态
    tabbar: TabBar,
    /// 会话唯一 id 计数器（标签滑动动画用）
    next_uid: u64,
    connect_form: ConnectForm,
    /// 默认下载目录（可在传输窗中修改，持久化）
    download_dir: std::path::PathBuf,
    /// 传输列表的状态筛选（全部 / 进行中 / 已完成 / 失败）
    xfer_filter: XferFilter,
    /// 传输进度浮窗是否显示
    show_transfers: bool,
    /// 传输浮窗刚打开（本帧跳过"点击外部关闭"判定）
    xfer_just_opened: bool,
    /// 顶部浮层提示 (文案, 起始时刻)：用于撤销等需要醒目反馈的操作，数秒后自动淡出
    toast: Option<(String, f64)>,
    /// 显示"确认退出"对话框
    show_close_confirm: bool,
    /// 待确认关闭的标签（仅当该会话仍连接中时弹确认）
    pending_close_tab: Option<usize>,
    /// 已确认可以关闭
    allow_close: bool,
    /// 编辑器状态：放在 Arc<Mutex> 里，供 deferred viewport 回调（'static + Send + Sync，
    /// 无法借用 &mut self）与主 update() 共享。改 deferred 是为根治 macOS 多窗口闪烁
    /// （immediate viewport 与主窗口同帧渲染、强耦合焦点，触发 Stage Manager 不停重拍）。
    editor_state: Arc<Mutex<EditorState>>,
    /// 看图工具状态（标签、激活项、聚焦请求、拖动重排）
    image: ImageView,
    /// docx 后台解析结果通道：(占位标签 id, 解析结果)
    doc_parse_tx: std::sync::mpsc::Sender<(
        u64,
        Result<
            (
                crate::ui::docx::Doc,
                std::collections::HashMap<String, egui::TextureHandle>,
            ),
            String,
        >,
    )>,
    doc_parse_rx: std::sync::mpsc::Receiver<(
        u64,
        Result<
            (
                crate::ui::docx::Doc,
                std::collections::HashMap<String, egui::TextureHandle>,
            ),
            String,
        >,
    )>,
    /// 下一个编辑器 TextEdit Id 序号
    next_editor_id: u64,
    /// 关闭大文件编辑器后延迟若干帧再 malloc_trim（等 galley 缓存被淘汰）
    trim_after: Option<u32>,
    /// 端口转发管理窗口 UI 状态（开关 / 表单 / 编辑 / 删除确认 / 校验错误）
    fwd: ForwardUi,
    /// 上一帧活动会话的 uid——切换会话时复位「跨会话易串台」的临时 UI 态（转发确认/编辑、进程弹窗）
    active_uid_prev: Option<u64>,
    /// 命令广播栏是否显示 + 输入内容
    show_broadcast: bool,
    broadcast_input: String,
    // 折叠监控栏/文件栏与界面缩放改为进程级全局状态（见本文件底部 view 状态），
    // 以便侧栏背景层与各子控件共用同一右键菜单。
    /// 传输冲突策略（目标已存在时；默认覆盖）
    conflict_policy: ConflictPolicy,
    /// 文件传输/复制粘贴/跨服务器中转与直传的聚合状态（从 App 抽出的内聚字段组）
    xfer: Transfers,
    /// 命令片段库（窗口开关 + 数据 + 编辑表单缓冲）
    snip: Snippets,
    /// 进程/GPU 详情小窗状态
    popups: Popups,
    /// 自检：每帧注入假 GPU 数据并保持详情窗打开（仅截图核对用）
    demo_gpu: bool,
    /// 自检：注入网络曲线波形（仅截图核对密度用）
    demo_net: bool,
    /// 自检截图模式（由环境变量触发，正常使用时为 None）
    shot: Option<Shot>,
    /// Logo 生成模式（ISHELL_LOGO）：只画 logo 圆角矩形
    logo: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply(&cc.egui_ctx);
        // docx 后台解析结果通道（解析/纹理解码在工作线程，UI 不冻结）
        let (doc_parse_tx, doc_parse_rx) = std::sync::mpsc::channel();
        // 载入已保存的界面缩放到全局视图状态
        init_view_state();
        // 载入已保存语言（默认中文）
        if let Some(code) = crate::store::load_lang() {
            crate::i18n::set(crate::i18n::Lang::from_code(&code));
        }
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("无法创建 tokio 运行时"),
        );
        let mut form = ConnectForm::default();
        form.open = true; // 启动即弹出连接框

        let shot = std::env::var("ISHELL_SHOT").ok().map(|path| {
            let secs: u64 = std::env::var("ISHELL_SHOT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            Shot {
                path,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(secs),
                requested: false,
            }
        });

        let mut app = Self {
            runtime,
            ctx: cc.egui_ctx.clone(),
            sessions: Vec::new(),
            active: None,
            tabbar: TabBar {
                total_w: 1.0,
                ..Default::default()
            },
            next_uid: 0,
            connect_form: form,
            download_dir: crate::store::load_download_dir()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(downloads_dir),
            xfer_filter: XferFilter::All,
            show_transfers: false,
            xfer_just_opened: false,
            toast: None,
            show_close_confirm: false,
            pending_close_tab: None,
            allow_close: false,
            editor_state: Arc::new(Mutex::new(EditorState::default())),
            image: ImageView::default(),
            doc_parse_tx,
            doc_parse_rx,
            next_editor_id: 0,
            trim_after: None,
            fwd: ForwardUi::default(),
            active_uid_prev: None,
            show_broadcast: false,
            broadcast_input: String::new(),
            conflict_policy: crate::store::load_conflict_policy()
                .map(|s| ConflictPolicy::from_str(&s))
                .unwrap_or(ConflictPolicy::Overwrite),
            xfer: Transfers::default(),
            snip: Snippets {
                list: crate::store::load_snippets(),
                run: true,
                ..Default::default()
            },
            popups: Popups::default(),
            demo_gpu: std::env::var("ISHELL_DEMO_GPU").is_ok(),
            demo_net: std::env::var("ISHELL_DEMO_NET").is_ok(),
            shot,
            logo: std::env::var("ISHELL_LOGO").is_ok() || std::env::var("ISHELL_ICON").is_ok(),
        };

        app.apply_demo_flags(cc);
        app
    }
}

impl eframe::App for App {
    // 窗口清屏色用主题背景，避免各区域间隙露出黑色
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.logo {
            return [1.0, 1.0, 1.0, 1.0]; // logo 模式白底，圆角矩形（米色）落在白底上便于裁切/置于浅色页
        }
        Palette::BG.to_normalized_gamma_f32()
    }

    // eframe 0.34 的现代入口：所有面板通过 `show_inside` 嵌入根 `ui`。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Logo 生成模式：透明画布上画一个圆角矩形（初始界面背景色）+ iShell（accent 色、同字体）
        if self.logo {
            {
                let square = std::env::var("ISHELL_ICON").is_ok();
                if square {
                    // 应用图标：填满方形画布
                    let painter = ui.painter();
                    let rect = ui.max_rect().shrink(8.0);
                    painter.rect_filled(rect, 30.0, Palette::BG);
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "iShell",
                        egui::FontId::proportional(76.0),
                        Palette::ACCENT,
                    );
                } else {
                    // logo：圆角矩形贴合文字，四周边距大致相等（避免左右过宽）
                    let galley = ui.ctx().fonts_mut(|f| {
                        f.layout_no_wrap(
                            "iShell".to_owned(),
                            egui::FontId::proportional(76.0),
                            Palette::ACCENT,
                        )
                    });
                    let sz = galley.size();
                    // 上下内边距小一些（galley 自带行间距），让视觉四边接近
                    let rect = egui::Rect::from_center_size(
                        ui.max_rect().center(),
                        sz + egui::vec2(64.0, 40.0),
                    );
                    let painter = ui.painter();
                    painter.rect_filled(rect, 26.0, Palette::BG);
                    painter.galley(rect.center() - sz / 2.0, galley, Palette::ACCENT);
                }
            }
            let ctx = ui.ctx().clone();
            self.drive_screenshot(&ctx);
            return;
        }

        // 注：曾在此「聚焦文本框时每帧持续重绘」以试图修复 X11/fcitx 的输入法提交延迟，
        // 但该延迟实为 winit X11/XIM 的事件投递限制（提交事件晚到一拍），重绘并不能解决；
        // 反而导致开了编辑器/多窗口后两窗口 60fps 永动重绘 → macOS Stage Manager 缩略图不停闪。
        // 故移除：egui 本就在收到按键/IME 事件时反应式重绘，正常输入不受影响。

        // 全局界面缩放（左侧栏可调）：仅在变化时设置，避免每帧触发重排
        if (ui.ctx().zoom_factor() - ui_zoom()).abs() > f32::EPSILON {
            ui.ctx().set_zoom_factor(ui_zoom());
        }

        // 活动会话切换时，复位「跨会话易串台」的临时 UI 态：转发的删除确认/编辑、进程详情弹窗
        // （否则 Ctrl+Tab 切走后，转发窗按 id 的确认/编辑可能命中新会话同 id 的另一条；进程弹窗显示陈旧）。
        let cur_active_uid = self
            .active
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.uid);
        if cur_active_uid != self.active_uid_prev {
            self.active_uid_prev = cur_active_uid;
            self.fwd.confirm_del = None;
            self.fwd.editing = None;
            self.fwd.error = None;
            self.popups.proc = None;
        }

        self.process_frame_events(ui);

        // 2) 连接对话框（浮动窗口）
        let ctx = ui.ctx().clone();
        if let Some(cfg) = self.connect_form.show(&ctx) {
            self.spawn_session(cfg);
        }

        // 3) 左侧操作栏：独立全高区域
        let mut proc_click: Option<(u32, egui::Pos2)> = None;
        let mut gpu_click: Option<egui::Pos2> = None;
        if !sidebar_collapsed() {
            egui::Panel::left("sidebar")
                .resizable(true)
                .default_size(300.0)
                .size_range(220.0..=460.0)
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL)
                        .inner_margin(egui::Margin {
                            left: 10,
                            right: 10,
                            top: 8,
                            bottom: 8,
                        }),
                )
                .show_inside(ui, |ui| {
                    // 背景层右键弹语言菜单：在子控件之前注册，置于最底层 z 序，
                    // 这样不会抢走进程行/网卡/IP 等子控件的左键；空白处右键仍可触发。
                    let bg = ui.interact(
                        ui.max_rect(),
                        ui.id().with("sidebar_bg"),
                        egui::Sense::click(),
                    );
                    // 监控栏右键：语言 / 字体大小 / 折叠开关 / 强制 X11 的统一入口
                    view_context_menu(&bg);
                    match self.active {
                        Some(idx) if idx < self.sessions.len() => {
                            let s = &mut self.sessions[idx];
                            let mon = s.monitor_ok;
                            sidebar::show(
                                ui,
                                s.sysinfo.as_ref(),
                                &s.net_hist,
                                &mut s.selected_nic,
                                &mut s.proc_sort_mem,
                                &mut proc_click,
                                &mut gpu_click,
                                mon,
                            );
                        }
                        _ => {
                            ui.add_space(16.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(egui_phosphor::regular::PLUGS)
                                        .size(28.0)
                                        .color(Palette::TEXT_DIM),
                                );
                                ui.label(
                                    RichText::new(crate::i18n::tr("未连接", "Not connected"))
                                        .color(Palette::TEXT_DIM),
                                );
                            });
                        }
                    }
                });
        } else {
            // 折叠态：保留一条细边，提供展开按钮 + 同样的右键菜单（否则收起后无处可点回来）
            egui::Panel::left("sidebar_strip")
                .resizable(false)
                .default_size(20.0)
                .size_range(20.0..=20.0)
                .frame(
                    egui::Frame::new()
                        .fill(Palette::PANEL_2)
                        .inner_margin(egui::Margin::same(2)),
                )
                .show_inside(ui, |ui| {
                    let bg = ui.interact(
                        ui.max_rect(),
                        ui.id().with("sidebar_strip_bg"),
                        egui::Sense::click(),
                    );
                    view_context_menu(&bg);
                    ui.add_space(4.0);
                    ui.vertical_centered(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(egui_phosphor::regular::CARET_RIGHT)
                                        .size(14.0)
                                        .color(Palette::TEXT_DIM),
                                )
                                .frame(false),
                            )
                            .on_hover_text(crate::i18n::tr(
                                "展开系统监控栏",
                                "Expand monitor sidebar",
                            ))
                            .clicked()
                        {
                            set_sidebar_collapsed(false);
                        }
                    });
                });
        }
        // 进程行被点击：打开详情小窗并请求详情
        if let Some((pid, pos)) = proc_click {
            let mut popup = None;
            if let Some(s) = self.active.and_then(|i| self.sessions.get(i)) {
                if let Some(p) = s
                    .sysinfo
                    .as_ref()
                    .and_then(|si| si.procs.iter().find(|p| p.pid == pid))
                {
                    popup = Some(ProcPopup {
                        pid,
                        name: p.name.clone(),
                        cpu: p.cpu,
                        mem: p.mem,
                        pos,
                        cmd: String::new(),
                        cwd: String::new(),
                        exe: String::new(),
                        copied_t: None,
                        confirm_kill: false,
                        uid: s.uid,
                    });
                }
                let _ = s.cmd_tx.send(UiCommand::ProcDetail(pid));
            }
            if let Some(pp) = popup {
                self.popups.proc = Some(pp);
                self.popups.proc_just_opened = true;
            }
        }
        if let Some(pos) = gpu_click {
            self.popups.gpu = Some(pos);
            self.popups.gpu_just_opened = true;
        }

        // Ctrl+Tab / Ctrl+Shift+Tab 切换会话标签（consume 以免终端把 Tab 发往远端）
        if !self.sessions.is_empty() {
            let ctx = ui.ctx();
            if ctx.input_mut(|i| {
                i.consume_key(
                    egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                    egui::Key::Tab,
                )
            }) {
                self.switch_tab(-1);
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
                self.switch_tab(1);
            }
        }

        // 4) 顶部选项卡（仅位于右侧区域之上）
        self.top_tabs(ui);

        // 4.5) 命令广播栏
        self.broadcast_bar(ui);

        // 5) 右侧主体
        match self.active {
            Some(idx) if idx < self.sessions.len() => self.right_body(ui, idx),
            _ => self.welcome(ui),
        }

        // 顶部浮层提示（撤销结果等醒目反馈）
        self.toast_overlay(&ctx);

        // 传输进度浮窗
        self.transfer_window(&ctx);

        // 端口转发管理浮窗
        self.forward_window(&ctx);

        // 进程详情小窗
        self.proc_popup_window(&ctx);

        // 「关于」弹框（右键菜单触发）
        about_window(&ctx);

        // GPU 详情小窗
        self.gpu_popup_window(&ctx);

        // 文本编辑器浮窗
        self.editor_window(&ctx);

        // 看图工具浮窗
        self.image_window(&ctx);

        // 未知主机指纹确认（TOFU）
        self.host_key_dialog(&ctx);

        // 键盘交互认证（OTP / 2FA）
        self.kbd_prompt_dialog(&ctx);

        // 关闭活动标签二次确认
        self.close_tab_dialog(&ctx);

        // 粘贴确认（跨服务器：含「直传/中转」互斥选择）
        self.paste_confirm_dialog(&ctx);
        // 直传目标主机密钥未记录时的 TOFU 确认
        self.direct_hostkey_dialog(&ctx);
        // 直传失败后的「必须改用中转」提醒
        self.direct_fallback_dialog(&ctx);

        // 命令片段库
        self.snippets_window(&ctx);

        // 关闭确认：仍有会话连接时，先弹确认
        self.handle_close(&ctx);

        // 自检截图驱动
        self.drive_screenshot(&ctx);
    }
}
