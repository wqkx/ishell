//! 连接对话框：左侧已保存连接列表，右侧录入主机/端口/用户名 + 密码或私钥。

use egui::RichText;

use crate::proto::{AuthMethod, ConnectConfig, JumpHost};
use crate::store::{self, SavedConnection};
use crate::theme::Palette;

#[derive(Clone, PartialEq)]
enum AuthKind {
    Password,
    Key,
    /// 本机 ssh-agent
    Agent,
    /// 键盘交互（支持 OTP / 二次验证）
    Interactive,
}

#[derive(PartialEq)]
enum Mode {
    /// 快速连接列表（默认）
    List,
    /// 新建 / 编辑表单
    Form,
}

/// 对话框表单状态（在 App 中长期持有）。
pub struct ConnectForm {
    pub open: bool,
    mode: Mode,
    name: String,
    host: String,
    port: String,
    username: String,
    password: String,
    key_path: String,
    passphrase: String,
    auth: AuthKind,
    /// 转发本机 ssh-agent（-A）
    forward_agent: bool,
    /// 分组与标签（组织用）
    group: String,
    tags: String,
    /// 快速连接列表的搜索过滤词
    search: String,
    // —— 跳板机 ——
    use_jump: bool,
    j_host: String,
    j_port: String,
    j_username: String,
    j_password: String,
    j_key_path: String,
    j_passphrase: String,
    j_auth: AuthKind,
    error: Option<String>,
    saved: Vec<SavedConnection>,
    /// 列表中选中的连接（高亮）
    sel: Option<usize>,
    /// 待确认删除的连接索引
    confirm_delete: Option<usize>,
    /// 导入等操作的提示信息（中性绿色）
    notice: Option<String>,
    /// 导入 ssh/config 的候选列表（含勾选状态）；Some 时显示选择对话框
    import_candidates: Option<Vec<(SavedConnection, bool)>>,
    /// 正在编辑的原始连接标识 (name, host)；None=新建。
    /// 用它定位原记录，这样即使改了 host/名称也能更新原条目而非新增。
    editing: Option<(String, String)>,
    /// 进入表单后下一帧自动聚焦「主机」输入框（一次性）
    focus_host: bool,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::List,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            username: "root".into(),
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            auth: AuthKind::Password,
            forward_agent: false,
            group: String::new(),
            tags: String::new(),
            search: String::new(),
            use_jump: false,
            j_host: String::new(),
            j_port: "22".into(),
            j_username: "root".into(),
            j_password: String::new(),
            j_key_path: String::new(),
            j_passphrase: String::new(),
            j_auth: AuthKind::Password,
            error: None,
            saved: store::load(),
            sel: None,
            confirm_delete: None,
            notice: None,
            import_candidates: None,
            editing: None,
            focus_host: false,
        }
    }
}

impl ConnectForm {
    /// 打开对话框：始终回到「快速连接」列表并从磁盘重新加载，确保已保存的连接可见。
    pub fn open_dialog(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        self.saved = store::load();
        self.error = None;
    }

    /// 自检：直接打开到新建表单（仅供截图）。
    pub fn open_form_for_demo(&mut self) {
        self.open = true;
        self.mode = Mode::Form;
        self.reset_form();
        self.focus_host = true;
    }

    /// 自检：打开导入选择对话框（仅供截图）。
    pub fn open_import_demo(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        let mk = |n: &str, h: &str, u: &str, a: &str| {
            (SavedConnection { name: n.into(), host: h.into(), port: 22, username: u.into(), auth_kind: a.into(), ..Default::default() }, true)
        };
        self.import_candidates = Some(vec![
            mk("web", "10.0.0.5", "deploy", "key"),
            mk("db", "db.internal", "root", "agent"),
            mk("gw", "gw.example.com", "admin", "password"),
        ]);
    }

    /// 自检：打开分组列表（仅供截图）。
    pub fn open_list_demo(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        let mk = |n: &str, h: &str, u: &str, g: &str, t: &str| SavedConnection {
            name: n.into(), host: h.into(), port: 22, username: u.into(), group: g.into(), tags: t.into(), ..Default::default()
        };
        self.saved = vec![
            mk("生产 Web", "10.0.0.5", "deploy", "生产环境", "web,nginx"),
            mk("生产 DB", "10.0.0.9", "root", "生产环境", "db,mysql"),
            mk("测试机", "192.168.1.20", "test", "测试环境", "qa"),
            mk("跳板机", "gw.example.com", "admin", "测试环境", "bastion"),
            mk("家里 NAS", "192.168.50.2", "nas", "", "home"),
        ];
    }

    /// 自检：打开删除确认对话框（仅供截图）。
    pub fn open_delete_demo(&mut self) {
        self.open = true;
        self.mode = Mode::List;
        self.saved = vec![SavedConnection { name: "生产数据库".into(), host: "10.0.0.9".into(), port: 22, username: "root".into(), ..Default::default() }];
        self.confirm_delete = Some(0);
    }

    /// 渲染对话框。返回 `Some(config)` 表示用户点击了「连接」且校验通过。
    pub fn show(&mut self, ctx: &egui::Context) -> Option<ConnectConfig> {
        if !self.open {
            return None;
        }
        let mut result = None;
        let mut open = self.open;

        let title = if self.mode == Mode::List { crate::i18n::tr("快速连接", "Quick Connect") } else { crate::i18n::tr("新建连接", "New Connection") };
        // 表单较窄（贴合输入列宽，按钮右对齐不至于飘太远）；列表略宽以容纳连接条目
        let win_width = if self.mode == Mode::List { 520.0 } else { 380.0 };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(win_width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match self.mode {
                Mode::List => self.list_view(ui, &mut result),
                Mode::Form => self.form_view(ui, &mut result),
            });

        // 删除确认弹窗
        if let Some(i) = self.confirm_delete {
            let name = self.saved.get(i).map(|c| c.name.clone()).unwrap_or_default();
            let mut close = false;
            egui::Window::new(crate::i18n::tr("确认删除", "Confirm delete"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(300.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(crate::i18n::tr("确认删除", "Confirm delete")).size(15.0).strong());
                        ui.add_space(6.0);
                        ui.label(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("确定删除连接「{name}」吗？"), crate::i18n::Lang::En => format!("Delete \"{name}\"?") });
                    });
                    ui.add_space(12.0);
                    let bw = 80.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        if ui
                            .add(egui::Button::new(RichText::new(crate::i18n::tr("删除", "Delete")).color(egui::Color32::WHITE)).fill(Palette::DANGER).min_size(egui::vec2(bw, 0.0)))
                            .clicked()
                        {
                            if i < self.saved.len() {
                                self.saved.remove(i);
                                if let Err(e) = store::save(&self.saved) {
                                    self.error = Some(e);
                                }
                            }
                            self.sel = None;
                            close = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            close = true;
                        }
                    });
                });
            if close {
                self.confirm_delete = None;
            }
        }

        // 导入 ssh/config 的选择对话框
        if self.import_candidates.is_some() {
            let mut do_import = false;
            let mut cancel = false;
            egui::Window::new(crate::i18n::tr("导入 ssh/config", "Import ssh/config"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_width(400.0);
                    let cands = self.import_candidates.as_mut().unwrap();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(match crate::i18n::current() {
                            crate::i18n::Lang::Zh => format!("发现 {} 台主机，选择要导入的：", cands.len()),
                            crate::i18n::Lang::En => format!("{} hosts found — choose to import:", cands.len()),
                        }).color(Palette::TEXT_DIM).size(12.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let all = cands.iter().all(|(_, s)| *s);
                            if ui.button(if all { crate::i18n::tr("全不选", "None") } else { crate::i18n::tr("全选", "All") }).clicked() {
                                let v = !all;
                                for c in cands.iter_mut() {
                                    c.1 = v;
                                }
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, false]).show(ui, |ui| {
                        for (c, sel) in cands.iter_mut() {
                            let auth = match c.auth_kind.as_str() { "key" => "key", "agent" => "agent", "interactive" => "2fa", _ => "pwd" };
                            ui.checkbox(sel, format!("{}   {}@{}:{}  · {auth}", c.name, c.username, c.host, c.port));
                        }
                    });
                    ui.separator();
                    ui.add_space(8.0);
                    let bw = 90.0;
                    let total = bw * 2.0 + ui.spacing().item_spacing.x;
                    ui.horizontal(|ui| {
                        ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
                        let n = cands.iter().filter(|(_, s)| *s).count();
                        let enabled = n > 0;
                        if ui.add_enabled(enabled, egui::Button::new(RichText::new(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("导入选中（{n}）"), crate::i18n::Lang::En => format!("Import ({n})") }).color(egui::Color32::WHITE)).fill(Palette::ACCENT).min_size(egui::vec2(bw, 0.0))).clicked() {
                            do_import = true;
                        }
                        if ui.add(egui::Button::new(crate::i18n::tr("取消", "Cancel")).min_size(egui::vec2(bw, 0.0))).clicked() {
                            cancel = true;
                        }
                    });
                });
            if do_import {
                self.apply_import();
            } else if cancel {
                self.import_candidates = None;
            }
        }

        if !open {
            self.open = false;
        }
        if result.is_some() {
            self.open = false;
        }
        result
    }

    /// 快速连接列表视图。
    fn list_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        use egui_phosphor::regular as icon;
        ui.horizontal(|ui| {
            ui.heading(RichText::new(crate::i18n::tr("快速连接", "Quick Connect")).size(18.0).color(Palette::TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new(format!("{}  {}", icon::PLUS, crate::i18n::tr("新建", "New"))).color(egui::Color32::WHITE))
                            .fill(Palette::ACCENT)
                            .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .clicked()
                {
                    self.reset_form();
                    self.mode = Mode::Form;
                    self.focus_host = true;
                }
                // 从 ~/.ssh/config 导入
                if ui
                    .add(egui::Button::new(RichText::new(format!("{}  {}", icon::DOWNLOAD_SIMPLE, crate::i18n::tr("导入 ssh/config", "Import ssh/config"))).color(Palette::TEXT)).wrap_mode(egui::TextWrapMode::Extend))
                    .on_hover_text(crate::i18n::tr("从 ~/.ssh/config 导入主机（无密钥默认用 agent）", "Import hosts from ~/.ssh/config"))
                    .clicked()
                {
                    self.open_import_dialog();
                }
            });
        });
        ui.label(RichText::new(crate::i18n::tr("单击选择，双击连接", "Click to select, double-click to connect")).color(Palette::TEXT_DIM).size(11.0));
        if let Some(msg) = &self.notice {
            ui.label(RichText::new(msg).color(Palette::OK).size(11.0));
        }
        ui.separator();
        ui.set_min_width(500.0);

        // 搜索框（名称/主机/用户/分组/标签）
        if !self.saved.is_empty() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(Palette::TEXT_DIM));
                let clear_w = if self.search.is_empty() { 0.0 } else { 22.0 };
                ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(ui.available_width() - clear_w - 4.0).hint_text(crate::i18n::tr("搜索名称/主机/用户/分组/标签", "Search name/host/user/group/tags")));
                if !self.search.is_empty()
                    && ui.add(egui::Button::new(RichText::new(icon::X).size(11.0).color(Palette::TEXT_DIM)).frame(false)).clicked()
                {
                    self.search.clear();
                }
            });
            ui.add_space(2.0);
        }

        egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
            if self.saved.is_empty() {
                crate::ui::empty_state(
                    ui,
                    egui_phosphor::regular::PLUGS,
                    crate::i18n::tr("还没有保存的连接，点击右上角「新建」", "No saved connections. Click \"New\" top-right."),
                    true,
                );
                return;
            }
            // 过滤
            let q = self.search.trim().to_lowercase();
            let mut filtered: Vec<usize> = self
                .saved
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    q.is_empty()
                        || c.name.to_lowercase().contains(&q)
                        || c.host.to_lowercase().contains(&q)
                        || c.username.to_lowercase().contains(&q)
                        || c.group.to_lowercase().contains(&q)
                        || c.tags.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();
            if filtered.is_empty() {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(crate::i18n::tr("无匹配", "No match")).color(Palette::TEXT_DIM));
                });
                return;
            }
            // 按（是否未分组, 分组名, 名称）排序：命名分组在前、未分组在后
            filtered.sort_by_key(|&i| {
                let c = &self.saved[i];
                (c.group.is_empty(), c.group.to_lowercase(), c.name.to_lowercase())
            });

            let mut connect_idx = None;
            let mut edit_idx = None;
            let mut del_idx = None;
            let mut sel_idx = None;
            let mut cur_group: Option<String> = None;
            for &i in &filtered {
                let c = &self.saved[i];
                // 分组标题（分组变化时插入一行）
                if cur_group.as_deref() != Some(c.group.as_str()) {
                    cur_group = Some(c.group.clone());
                    let label = if c.group.is_empty() { crate::i18n::tr("未分组", "Ungrouped").to_string() } else { c.group.clone() };
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("{}  {}", icon::FOLDER, label)).strong().color(Palette::TEXT_DIM).size(12.0));
                }
                let selected = self.sel == Some(i);
                // 预分配固定行高，整行可点击；高亮仅覆盖本行
                let row_h = 30.0;
                let full_w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::click());
                if selected {
                    ui.painter().rect_filled(rect, 6.0, Palette::ACCENT_SOFT);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 6.0, Palette::PANEL_2);
                }
                // 在该行 rect 内绘制内容（编辑/删除按钮在上层，单独响应点击）
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.label(RichText::new(icon::DESKTOP_TOWER).color(Palette::ACCENT));
                        ui.add_space(2.0);
                        ui.label(RichText::new(&c.name).color(Palette::TEXT));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(egui::Button::new(RichText::new(icon::TRASH).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("删除", "Delete")).clicked() {
                                del_idx = Some(i);
                            }
                            if ui.add(egui::Button::new(RichText::new(icon::PENCIL_SIMPLE).color(Palette::TEXT_DIM)).frame(false)).on_hover_text(crate::i18n::tr("编辑", "Edit")).clicked() {
                                edit_idx = Some(i);
                            }
                            ui.add_space(10.0);
                            // 固定宽列：用户名 + IP，跨行对齐
                            ui.allocate_ui_with_layout(egui::vec2(80.0, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.label(RichText::new(&c.username).color(Palette::TEXT_DIM).size(12.0));
                            });
                            ui.allocate_ui_with_layout(egui::vec2(150.0, row_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.label(RichText::new(format!("{}:{}", c.host, c.port)).color(Palette::TEXT_DIM).size(12.0));
                            });
                        });
                    },
                );
                if resp.clicked() {
                    sel_idx = Some(i);
                }
                if resp.double_clicked() {
                    connect_idx = Some(i);
                }
            }
            if let Some(i) = sel_idx {
                self.sel = Some(i);
            }
            if let Some(i) = connect_idx {
                self.load_saved(i);
                if let Ok(cfg) = self.build() {
                    *result = Some(cfg);
                }
            }
            if let Some(i) = edit_idx {
                self.load_saved(i);
                self.mode = Mode::Form;
                self.focus_host = true;
            }
            if let Some(i) = del_idx {
                self.confirm_delete = Some(i);
            }
        });
    }

    /// 解析 ~/.ssh/config 并打开选择对话框（默认全选）。
    fn open_import_dialog(&mut self) {
        let imported = store::import_ssh_config();
        if imported.is_empty() {
            self.notice = Some(crate::i18n::tr("未找到 ~/.ssh/config 或无可导入主机", "No ~/.ssh/config hosts found").into());
        } else {
            self.import_candidates = Some(imported.into_iter().map(|c| (c, true)).collect());
        }
    }

    /// 把选中的候选项按 名称+host 去重合并后保存。
    fn apply_import(&mut self) {
        let Some(cands) = self.import_candidates.take() else { return };
        let mut added = 0;
        let mut updated = 0;
        for (c, sel) in cands {
            if !sel {
                continue;
            }
            if let Some(slot) = self.saved.iter_mut().find(|s| s.name == c.name && s.host == c.host) {
                *slot = c;
                updated += 1;
            } else {
                self.saved.push(c);
                added += 1;
            }
        }
        match store::save(&self.saved) {
            Ok(()) => {
                self.notice = Some(match crate::i18n::current() {
                    crate::i18n::Lang::Zh => format!("已导入：新增 {added}，更新 {updated}"),
                    crate::i18n::Lang::En => format!("Imported: {added} new, {updated} updated"),
                });
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn reset_form(&mut self) {
        self.name.clear();
        self.host.clear();
        self.port = "22".into();
        self.username = "root".into();
        self.password.clear();
        self.key_path.clear();
        self.passphrase.clear();
        self.auth = AuthKind::Password;
        self.forward_agent = false;
        self.group.clear();
        self.tags.clear();
        self.use_jump = false;
        self.j_host.clear();
        self.j_port = "22".into();
        self.j_username = "root".into();
        self.j_password.clear();
        self.j_key_path.clear();
        self.j_passphrase.clear();
        self.j_auth = AuthKind::Password;
        self.error = None;
        self.editing = None; // 新建：无原始记录
    }

    /// 新建 / 编辑表单视图。
    fn form_view(&mut self, ui: &mut egui::Ui, result: &mut Option<ConnectConfig>) {
        ui.set_min_width(340.0);
        let w = 250.0; // 私钥路径输入框宽度（其余输入框填满）
        egui::Grid::new("conn_form")
            .num_columns(2)
            .spacing([12.0, 12.0])
            .min_col_width(64.0)
            .show(ui, |ui| {
                ui.label(crate::i18n::tr("名称", "Name"));
                ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(f32::INFINITY).hint_text(crate::i18n::tr("便于识别，可留空", "For display, optional")));
                ui.end_row();

                ui.label(crate::i18n::tr("主机", "Host"));
                let host_resp = ui.add(egui::TextEdit::singleline(&mut self.host).desired_width(f32::INFINITY));
                if self.focus_host {
                    host_resp.request_focus(); // 进入表单后自动聚焦主机框
                    self.focus_host = false;
                }
                ui.end_row();

                ui.label(crate::i18n::tr("端口", "Port"));
                ui.add(egui::TextEdit::singleline(&mut self.port).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label(crate::i18n::tr("用户名", "User"));
                ui.add(egui::TextEdit::singleline(&mut self.username).desired_width(f32::INFINITY));
                ui.end_row();

                ui.label(crate::i18n::tr("分组", "Group"));
                ui.add(egui::TextEdit::singleline(&mut self.group).desired_width(f32::INFINITY).hint_text(crate::i18n::tr("可留空，用于归类", "Optional folder")));
                ui.end_row();

                ui.label(crate::i18n::tr("标签", "Tags"));
                ui.add(egui::TextEdit::singleline(&mut self.tags).desired_width(f32::INFINITY).hint_text(crate::i18n::tr("逗号分隔，参与搜索", "Comma-separated, searchable")));
                ui.end_row();

                ui.label(crate::i18n::tr("认证方式", "Auth"));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.auth, AuthKind::Password, crate::i18n::tr("密码", "Password"));
                    ui.selectable_value(&mut self.auth, AuthKind::Key, crate::i18n::tr("私钥", "Key"));
                    ui.selectable_value(&mut self.auth, AuthKind::Agent, crate::i18n::tr("Agent", "Agent"));
                    ui.selectable_value(&mut self.auth, AuthKind::Interactive, crate::i18n::tr("交互/2FA", "2FA"));
                });
                ui.end_row();

                match self.auth {
                    AuthKind::Agent => {
                        ui.label("");
                        ui.label(RichText::new(crate::i18n::tr("使用本机 ssh-agent 中的私钥（先 ssh-add）", "Use keys from local ssh-agent (ssh-add first)")).color(Palette::TEXT_DIM).size(11.0));
                        ui.end_row();
                    }
                    AuthKind::Interactive => {
                        ui.label("");
                        ui.label(RichText::new(crate::i18n::tr("登录时按服务器提示逐项输入（支持验证码 / 二次验证）", "Answer server prompts at login (OTP / 2FA)")).color(Palette::TEXT_DIM).size(11.0));
                        ui.end_row();
                    }
                    AuthKind::Password => {
                        ui.label(crate::i18n::tr("密码", "Password"));
                        ui.add(egui::TextEdit::singleline(&mut self.password).desired_width(f32::INFINITY).password(true));
                        ui.end_row();
                    }
                    AuthKind::Key => {
                        ui.label(crate::i18n::tr("私钥路径", "Key file"));
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.key_path).desired_width(w - 36.0));
                            if ui
                                .button(egui_phosphor::regular::FOLDER_OPEN)
                                .on_hover_text(crate::i18n::tr("浏览选择私钥文件", "Browse for key file"))
                                .clicked()
                            {
                                if let Some(path) = rfd::FileDialog::new().set_title(crate::i18n::tr("选择私钥文件", "Select key file")).pick_file() {
                                    self.key_path = path.to_string_lossy().into_owned();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label(crate::i18n::tr("私钥口令", "Passphrase"));
                        ui.add(egui::TextEdit::singleline(&mut self.passphrase).desired_width(f32::INFINITY).password(true));
                        ui.end_row();
                    }
                }
            });

        ui.add_space(6.0);
        ui.checkbox(&mut self.forward_agent, crate::i18n::tr("转发本机 ssh-agent（-A）", "Forward ssh-agent (-A)"))
            .on_hover_text(crate::i18n::tr(
                "让远端任意进程复用本机 agent 的全部私钥，无密钥级限制；仅在完全信任的主机上开启",
                "Lets any remote process use all keys in your local agent with no per-key restriction; enable only on fully trusted hosts",
            ));
        if self.forward_agent {
            ui.label(RichText::new(crate::i18n::tr(
                "⚠ 风险：远端任意进程可使用本机 agent 中的全部私钥，等同于把本地身份交给该主机。",
                "⚠ Risk: any remote process can use every key in your local agent — equivalent to handing your local identity to that host.",
            )).color(Palette::DANGER).size(11.0));
        }

        ui.add_space(8.0);
        ui.checkbox(&mut self.use_jump, crate::i18n::tr("通过跳板机连接（ProxyJump）", "Via jump host (ProxyJump)"));
        if self.use_jump {
            egui::Grid::new("jump_form")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .min_col_width(64.0)
                .show(ui, |ui| {
                    ui.label(crate::i18n::tr("跳板主机", "Jump host"));
                    ui.add(egui::TextEdit::singleline(&mut self.j_host).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label(crate::i18n::tr("跳板端口", "Jump port"));
                    ui.add(egui::TextEdit::singleline(&mut self.j_port).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label(crate::i18n::tr("跳板用户", "Jump user"));
                    ui.add(egui::TextEdit::singleline(&mut self.j_username).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label(crate::i18n::tr("跳板认证", "Jump auth"));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.j_auth, AuthKind::Password, crate::i18n::tr("密码", "Password"));
                        ui.selectable_value(&mut self.j_auth, AuthKind::Key, crate::i18n::tr("私钥", "Key"));
                        ui.selectable_value(&mut self.j_auth, AuthKind::Agent, crate::i18n::tr("Agent", "Agent"));
                    });
                    ui.end_row();
                    match self.j_auth {
                        // 跳板机不提供交互/2FA 选项；此分支仅为枚举穷尽
                        AuthKind::Interactive => {}
                        AuthKind::Agent => {
                            ui.label("");
                            ui.label(RichText::new(crate::i18n::tr("使用本机 ssh-agent", "Use local ssh-agent")).color(Palette::TEXT_DIM).size(11.0));
                            ui.end_row();
                        }
                        AuthKind::Password => {
                            ui.label(crate::i18n::tr("跳板密码", "Jump pwd"));
                            ui.add(egui::TextEdit::singleline(&mut self.j_password).desired_width(f32::INFINITY).password(true));
                            ui.end_row();
                        }
                        AuthKind::Key => {
                            ui.label(crate::i18n::tr("跳板私钥", "Jump key"));
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.j_key_path).desired_width(w - 36.0));
                                if ui.button(egui_phosphor::regular::FOLDER_OPEN).on_hover_text(crate::i18n::tr("浏览选择私钥文件", "Browse for key file")).clicked() {
                                    if let Some(path) = rfd::FileDialog::new().set_title(crate::i18n::tr("选择跳板机私钥", "Select jump key file")).pick_file() {
                                        self.j_key_path = path.to_string_lossy().into_owned();
                                    }
                                }
                            });
                            ui.end_row();
                            ui.label(crate::i18n::tr("私钥口令", "Passphrase"));
                            ui.add(egui::TextEdit::singleline(&mut self.j_passphrase).desired_width(f32::INFINITY).password(true));
                            ui.end_row();
                        }
                    }
                });
        }

        if let Some(err) = &self.error {
            ui.add_space(4.0);
            ui.label(RichText::new(err).color(Palette::DANGER));
        }

        // 凭据安全透明度：密码/口令始终加密存储；如实展示「主密钥存哪」，钥匙串不可用时给出警示。
        {
            use egui_phosphor::regular as eicon;
            let (ic, txt, col) = if store::key_perms_were_loose() {
                (
                    eicon::WARNING_CIRCLE,
                    crate::i18n::tr("本地 key 文件权限曾过宽，已自动收紧为 0600", "Local key file perms were too open; tightened to 0600"),
                    Palette::DANGER,
                )
            } else {
                match store::key_storage() {
                    store::KeyStorage::Keychain => (
                        eicon::LOCK_KEY,
                        crate::i18n::tr("密码已加密，主密钥存于系统钥匙串", "Passwords encrypted; master key in system keychain"),
                        Palette::TEXT_DIM,
                    ),
                    store::KeyStorage::LocalFile => (
                        eicon::LOCK_KEY,
                        crate::i18n::tr("密码已加密；系统钥匙串不可用，主密钥存于本地文件 (0600)", "Encrypted; keychain unavailable — master key in local file (0600)"),
                        Palette::WARN,
                    ),
                    store::KeyStorage::None => (
                        eicon::WARNING_CIRCLE,
                        crate::i18n::tr("加密不可用，密码将以明文保存", "Encryption unavailable — passwords stored in plaintext"),
                        Palette::DANGER,
                    ),
                }
            };
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{ic}  {txt}")).color(col).size(11.0));
        }

        ui.add_space(10.0);
        // 右对齐、主操作（连接）置最右（macOS 习惯）；保存/返回为次级幽灵按钮
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(
                egui::Button::new(RichText::new(format!("{}  {}", egui_phosphor::regular::PLUGS_CONNECTED, crate::i18n::tr("连接", "Connect"))).color(egui::Color32::WHITE))
                    .fill(Palette::ACCENT)
                    .wrap_mode(egui::TextWrapMode::Extend),
            ).clicked() {
                match self.build() {
                    Ok(cfg) => {
                        // 直接点「连接」也默认保存到快速连接（新建即落库，下次免重填；编辑则更新原条目）
                        self.save_current();
                        *result = Some(cfg);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button(crate::i18n::tr("保存", "Save")).on_hover_text(crate::i18n::tr("保存到快速连接", "Save to quick connect")).clicked() {
                match self.build() {
                    Ok(_) => {
                        self.save_current();
                        self.mode = Mode::List;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button(crate::i18n::tr("返回", "Back")).clicked() {
                self.error = None;
                self.mode = Mode::List;
            }
        });
    }

    fn load_saved(&mut self, i: usize) {
        let c = self.saved[i].clone();
        // 记住原始标识，保存时据此更新原条目（即使改了 host/名称）
        self.editing = Some((c.name.clone(), c.host.clone()));
        self.name = c.name;
        self.host = c.host;
        self.port = c.port.to_string();
        self.username = c.username;
        self.password = c.password;
        self.key_path = c.key_path;
        self.passphrase = c.passphrase;
        self.auth = match c.auth_kind.as_str() { "key" => AuthKind::Key, "agent" => AuthKind::Agent, "interactive" => AuthKind::Interactive, _ => AuthKind::Password };
        self.forward_agent = c.forward_agent;
        self.group = c.group;
        self.tags = c.tags;
        self.use_jump = c.use_jump;
        self.j_host = c.jump_host;
        self.j_port = c.jump_port.to_string();
        self.j_username = c.jump_username;
        self.j_password = c.jump_password;
        self.j_key_path = c.jump_key_path;
        self.j_passphrase = c.jump_passphrase;
        self.j_auth = match c.jump_auth_kind.as_str() { "key" => AuthKind::Key, "agent" => AuthKind::Agent, _ => AuthKind::Password };
        self.error = None;
    }

    /// 把当前表单保存/更新到列表（按 名称+host 去重）。
    fn save_current(&mut self) {
        let name = if self.name.trim().is_empty() {
            format!("{}@{}", self.username.trim(), self.host.trim())
        } else {
            self.name.trim().to_string()
        };
        let entry = SavedConnection {
            name: name.clone(),
            host: self.host.trim().to_string(),
            port: self.port.trim().parse().unwrap_or(22),
            username: self.username.trim().to_string(),
            auth_kind: match self.auth { AuthKind::Key => "key".into(), AuthKind::Agent => "agent".into(), AuthKind::Interactive => "interactive".into(), AuthKind::Password => "password".into() },
            forward_agent: self.forward_agent,
            password: self.password.clone(),
            key_path: self.key_path.trim().to_string(),
            passphrase: self.passphrase.clone(),
            use_jump: self.use_jump,
            jump_host: self.j_host.trim().to_string(),
            jump_port: self.j_port.trim().parse().unwrap_or(22),
            jump_username: self.j_username.trim().to_string(),
            jump_auth_kind: match self.j_auth { AuthKind::Key => "key".into(), AuthKind::Agent => "agent".into(), AuthKind::Interactive => "interactive".into(), AuthKind::Password => "password".into() },
            jump_password: self.j_password.clone(),
            jump_key_path: self.j_key_path.trim().to_string(),
            jump_passphrase: self.j_passphrase.clone(),
            group: self.group.trim().to_string(),
            tags: self.tags.trim().to_string(),
        };
        // 定位原记录：编辑态按「进入编辑时的原始 (name, host)」匹配，这样改了 host/名称仍更新原条目；
        // 新建态则按新的 (name, host) 去重，避免重复新增。
        let slot = match &self.editing {
            Some((on, oh)) => self.saved.iter_mut().find(|c| &c.name == on && &c.host == oh),
            None => self.saved.iter_mut().find(|c| c.name == name && c.host == entry.host),
        };
        if let Some(slot) = slot {
            *slot = entry.clone();
        } else {
            self.saved.push(entry.clone());
        }
        // 更新编辑标识为新值，便于在同一编辑会话内二次保存仍命中同一条目
        self.editing = Some((entry.name, entry.host));
        match store::save(&self.saved) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(e),
        }
    }

    fn build(&self) -> Result<ConnectConfig, String> {
        if self.host.trim().is_empty() {
            return Err(crate::i18n::tr("请填写主机地址", "Enter host").into());
        }
        let port: u16 = self.port.trim().parse().map_err(|_| crate::i18n::tr("端口非法", "Invalid port").to_string())?;
        if self.username.trim().is_empty() {
            return Err(crate::i18n::tr("请填写用户名", "Enter user").into());
        }
        let auth = match self.auth {
            AuthKind::Password => AuthMethod::Password(self.password.clone()),
            AuthKind::Agent => AuthMethod::Agent,
            AuthKind::Interactive => AuthMethod::Interactive,
            AuthKind::Key => {
                if self.key_path.trim().is_empty() {
                    return Err(crate::i18n::tr("请填写私钥路径", "Enter key file").into());
                }
                AuthMethod::KeyFile {
                    path: self.key_path.trim().to_string(),
                    passphrase: if self.passphrase.is_empty() {
                        None
                    } else {
                        Some(self.passphrase.clone())
                    },
                }
            }
        };
        let jump = if self.use_jump {
            if self.j_host.trim().is_empty() {
                return Err(crate::i18n::tr("请填写跳板主机地址", "Enter jump host").into());
            }
            let jport: u16 = self.j_port.trim().parse().map_err(|_| crate::i18n::tr("跳板端口非法", "Invalid jump port").to_string())?;
            if self.j_username.trim().is_empty() {
                return Err(crate::i18n::tr("请填写跳板用户名", "Enter jump user").into());
            }
            let jauth = match self.j_auth {
                AuthKind::Password => AuthMethod::Password(self.j_password.clone()),
                AuthKind::Agent => AuthMethod::Agent,
                AuthKind::Interactive => AuthMethod::Interactive,
                AuthKind::Key => {
                    if self.j_key_path.trim().is_empty() {
                        return Err(crate::i18n::tr("请填写跳板私钥路径", "Enter jump key file").into());
                    }
                    AuthMethod::KeyFile {
                        path: self.j_key_path.trim().to_string(),
                        passphrase: if self.j_passphrase.is_empty() { None } else { Some(self.j_passphrase.clone()) },
                    }
                }
            };
            Some(JumpHost {
                host: self.j_host.trim().to_string(),
                port: jport,
                username: self.j_username.trim().to_string(),
                auth: jauth,
            })
        } else {
            None
        };
        Ok(ConnectConfig {
            host: self.host.trim().to_string(),
            port,
            username: self.username.trim().to_string(),
            auth,
            label: self.name.trim().to_string(),
            jump,
            forward_agent: self.forward_agent,
        })
    }
}
