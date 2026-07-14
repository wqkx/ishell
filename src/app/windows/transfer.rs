//! 传输进度浮窗。

use egui::RichText;

use crate::proto::{ConflictPolicy, UiCommand};
use crate::theme::Palette;

use super::super::util::*;
use super::super::{App, Transfer, XferFilter, XferSpec};

impl App {
    pub(in crate::app) fn transfer_window(&mut self, ctx: &egui::Context) {
        use egui_phosphor::regular as icon;
        if !self.show_transfers {
            return;
        }
        let Some(idx) = self.active else { return };
        // 同一服务器（host:port）的所有会话：把它们的传输任务汇总到同一个列表里展示
        let server_idxs = self.same_server_idxs(idx);
        let mut close_win = false;
        let mut clear = false;
        let mut cancel_all = false;
        let mut retry_all = false;
        let mut pick_dir = false;
        // 动作均以 (会话 uid, 传输 id) 标识，确保多标签同服务器时路由到正确的会话 worker
        let mut cancel_id: Option<(u64, u64)> = None;
        let mut toggle_err: Option<(u64, u64)> = None;
        let mut remove_id: Option<(u64, u64)> = None;
        let mut delete_id: Option<(u64, u64, String)> = None;
        let mut resume_id: Option<(u64, u64)> = None;
        let mut cycle_policy = false;
        let dl_dir = self.download_dir.to_string_lossy().into_owned();
        // 冲突策略短标签（中/英）+ 按策略区分的图标，用于标题栏按钮显示
        let policy_label = match (self.conflict_policy, crate::i18n::current()) {
            (ConflictPolicy::Overwrite, crate::i18n::Lang::Zh) => "覆盖",
            (ConflictPolicy::Skip, crate::i18n::Lang::Zh) => "跳过",
            (ConflictPolicy::Rename, crate::i18n::Lang::Zh) => "重命名",
            (ConflictPolicy::Overwrite, crate::i18n::Lang::En) => "Overwrite",
            (ConflictPolicy::Skip, crate::i18n::Lang::En) => "Skip",
            (ConflictPolicy::Rename, crate::i18n::Lang::En) => "Rename",
        };
        let policy_icon = match self.conflict_policy {
            ConflictPolicy::Overwrite => icon::SWAP,       // 覆盖=替换
            ConflictPolicy::Skip => icon::SKIP_FORWARD,    // 跳过
            ConflictPolicy::Rename => icon::PENCIL_SIMPLE, // 重命名
        };
        let win = egui::Window::new("transfer_win")
            .title_bar(false) // 隐藏过大的默认标题，使用自定义紧凑标题
            .anchor(egui::Align2::RIGHT_TOP, [-10.0, 44.0])
            .default_width(330.0)
            .resizable(false)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(Palette::PANEL)
                    .inner_margin(10),
            )
            .show(ctx, |ui| {
                // 自定义紧凑标题栏
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}  {}", icon::ARROWS_DOWN_UP, crate::i18n::tr("文件传输", "Transfers"))).strong().size(13.0).color(Palette::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(egui::Button::new(RichText::new(icon::X).size(12.0).color(Palette::TEXT_DIM)).frame(false)).clicked() {
                            close_win = true;
                        }
                        // 冲突策略：目标已存在时的默认处理；点击循环切换（覆盖→跳过→重命名），持久化
                        if ui
                            .add(egui::Button::new(RichText::new(format!("{} {}", policy_icon, policy_label)).size(11.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(crate::i18n::tr("目标已存在时的默认处理（点击切换：覆盖 / 跳过 / 重命名）", "Default when target exists (click to cycle: Overwrite / Skip / Rename)"))
                            .clicked()
                        {
                            cycle_policy = true;
                        }
                        if ui
                            .add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(13.0).color(Palette::TEXT_DIM)).frame(false))
                            .on_hover_text(match crate::i18n::current() { crate::i18n::Lang::Zh => format!("选择默认下载文件夹\n当前：{}", dl_dir), crate::i18n::Lang::En => format!("Set default download folder\nCurrent: {}", dl_dir) })
                            .clicked()
                        {
                            pick_dir = true;
                        }
                    });
                });
                ui.separator();

                // 状态筛选：紧凑的 frameless 文字 chips（带计数），仅在有任务时显示——
                // 避免空列表时占位、也避免大按钮 + 多余分隔线让顶部拥挤。
                // 先在借用 s 之前算各状态计数（借用随即结束），再据此渲染并允许改 self.xfer_filter。
                let counts = server_idxs.iter().filter_map(|&i| self.sessions.get(i)).fold(
                    (0usize, 0usize, 0usize, 0usize),
                    |(tot, act, dn, fl), s| {
                        (
                            tot + s.transfers.len(),
                            act + s.transfers.iter().filter(|t| t.ok.is_none()).count(),
                            dn + s.transfers.iter().filter(|t| t.ok == Some(true)).count(),
                            fl + s.transfers.iter().filter(|t| t.ok == Some(false)).count(),
                        )
                    },
                );
                if counts.0 > 0 {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 9.0;
                        for (f, zh, en, n) in [
                            (XferFilter::All, "全部", "All", counts.0),
                            (XferFilter::Active, "进行中", "Active", counts.1),
                            (XferFilter::Done, "已完成", "Done", counts.2),
                            (XferFilter::Failed, "失败", "Failed", counts.3),
                        ] {
                            let on = self.xfer_filter == f;
                            // 激活=强调色加粗；有失败时「失败」用危险色；其余弱色
                            let col = if on {
                                Palette::ACCENT
                            } else if matches!(f, XferFilter::Failed) && n > 0 {
                                Palette::DANGER
                            } else {
                                Palette::TEXT_DIM
                            };
                            let mut rt = RichText::new(format!("{} {}", crate::i18n::tr(zh, en), n)).size(11.0).color(col);
                            if on {
                                rt = rt.strong();
                            }
                            if ui.add(egui::Button::new(rt).frame(false).small()).clicked() {
                                self.xfer_filter = f;
                            }
                        }
                    });
                    ui.add_space(2.0);
                }
                let filter = self.xfer_filter;

                // 汇总同服务器所有会话的传输（活动会话在前，各自新→旧），元素为 (会话 uid, &Transfer)
                let items: Vec<(u64, &Transfer)> = server_idxs
                    .iter()
                    .filter_map(|&i| self.sessions.get(i))
                    .flat_map(|s| s.transfers.iter().rev().map(move |t| (s.uid, t)))
                    .collect();
                let total_len = items.len();
                if total_len == 0 {
                    ui.add_space(6.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::DOWNLOAD_SIMPLE, crate::i18n::tr("暂无传输任务", "No transfers"), false);
                }
                let mut open_dir: Option<String> = None;
                let mut shown = 0usize; // 当前筛选下实际展示的条数（用于「无匹配」提示）
                // 列表过长时滚动：约 8 条高度封顶，其余可滚动查看
                egui::ScrollArea::vertical().max_height(400.0).auto_shrink([false, true]).show(ui, |ui| {
                for (uid, t) in items.iter().copied().filter(|(_, t)| match filter {
                    XferFilter::All => true,
                    XferFilter::Active => t.ok.is_none(),
                    XferFilter::Done => t.ok == Some(true),
                    XferFilter::Failed => t.ok == Some(false),
                }).take(50) {
                    shown += 1;
                    // 下载=绿色，上传=珊瑚橙，颜色区分方向
                    let (dir_icon, dir_col) = match t.dir {
                        crate::proto::TransferDir::Download => (icon::DOWNLOAD_SIMPLE, Palette::OK),
                        crate::proto::TransferDir::Upload => (icon::UPLOAD_SIMPLE, Palette::ACCENT),
                    };
                    // 整个传输项包进一个感知点击的 scope，便于整体右键（否则右键会穿透到下方终端）
                    let item = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                        // 文件名过长时需要跟右侧状态图标留出空间、省略号截断，而不是原样撑开
                        // 和图标区域重叠——先出右侧（图标/按钮，天然宽度小），再把剩余宽度
                        // 让给左侧名字标签做截断（Sides::shrink_left 正是为这个场景设计的）。
                        egui::Sides::new().shrink_left().truncate().show(
                            ui,
                            |ui| {
                                ui.label(RichText::new(dir_icon).color(dir_col).size(13.0));
                                ui.label(RichText::new(&t.name).size(12.0).color(Palette::TEXT));
                            },
                            |ui| {
                                match t.ok {
                                    Some(true) => {
                                        ui.label(RichText::new(icon::CHECK_CIRCLE).color(Palette::OK).size(13.0));
                                        // 下载完成：保留「打开所在文件夹」按钮
                                        if let Some(local) = &t.local {
                                            if ui.add(egui::Button::new(RichText::new(icon::FOLDER_OPEN).size(12.0).color(Palette::TEXT_DIM)).frame(false))
                                                .on_hover_text(crate::i18n::tr("在文件管理器中显示", "Show in file manager"))
                                                .clicked()
                                            {
                                                open_dir = Some(local.clone());
                                            }
                                        }
                                    }
                                    Some(false) => {
                                        // 失败：可重试（有重发规格时）+ 状态按钮展开原因
                                        if ui.add(egui::Button::new(RichText::new(icon::WARNING_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("点击查看失败原因", "Click for reason"))
                                            .clicked()
                                        {
                                            toggle_err = Some((uid, t.id));
                                        }
                                        if t.spec.is_some()
                                            && ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                                .on_hover_text(crate::i18n::tr("重试", "Retry"))
                                                .clicked()
                                        {
                                            resume_id = Some((uid, t.id));
                                        }
                                    }
                                    None if t.paused => {
                                        // 已中断/暂停：续传按钮
                                        if ui.add(egui::Button::new(RichText::new(icon::ARROW_CLOCKWISE).color(Palette::ACCENT).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("续传", "Resume"))
                                            .clicked()
                                        {
                                            resume_id = Some((uid, t.id));
                                        }
                                    }
                                    None if t.queued => {
                                        // 等待态（中转目标端，正等源端下载）：时钟图标 + 转圈，不提供取消（受中转任务管控）
                                        ui.label(RichText::new(icon::CLOCK).color(Palette::TEXT_DIM).size(13.0))
                                            .on_hover_text(crate::i18n::tr("等待中", "Waiting"));
                                        ui.spinner();
                                    }
                                    None => {
                                        // 进行中：取消按钮 + 转圈
                                        if ui.add(egui::Button::new(RichText::new(icon::X_CIRCLE).color(Palette::DANGER).size(13.0)).frame(false))
                                            .on_hover_text(crate::i18n::tr("取消", "Cancel"))
                                            .clicked()
                                        {
                                            cancel_id = Some((uid, t.id));
                                        }
                                        ui.spinner();
                                    }
                                }
                            },
                        );
                        let done = t.ok == Some(true);
                        let frac = if done { 1.0 } else if t.total > 0 { t.done as f32 / t.total as f32 } else { 0.0 };
                        let pct = (frac.clamp(0.0, 1.0) * 100.0).round() as i32;
                        // 进行中/失败：条上居中显示百分比；完成：条上分两端（大小靠左、100% 靠右）
                        let mut bar = egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                            .fill(dir_col)
                            .desired_height(10.0)
                            .corner_radius(2.0);
                        if !done {
                            bar = bar.text(RichText::new(format!("{pct}%")).size(10.0));
                        }
                        let bar_resp = ui.add(bar);
                        if done {
                            let rect = bar_resp.rect;
                            let p = ui.painter_at(rect);
                            let font = egui::FontId::proportional(10.0);
                            // 大小靠左；有模式徽标（如「直传」）时显示在文件大小之后
                            let left_label = if t.tag.is_empty() {
                                crate::ui::fmt_bytes(t.total as f64)
                            } else {
                                format!("{} · {}", crate::ui::fmt_bytes(t.total as f64), t.tag)
                            };
                            p.text(
                                egui::pos2(rect.left() + 6.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                left_label,
                                font.clone(),
                                egui::Color32::WHITE,
                            );
                            // 100% 靠右
                            p.text(
                                egui::pos2(rect.right() - 6.0, rect.center().y),
                                egui::Align2::RIGHT_CENTER,
                                "100%",
                                font,
                                egui::Color32::WHITE,
                            );
                        }
                        // 进行中才显示详情行（已传/总量 + 实时速度）；完成后不再单独一行
                        if t.ok.is_none() {
                            // 有阶段提示（打包/解包/等待/直传）时优先显示提示，替代字节读数——
                            // 这些阶段没有逐字节进度，显示「0 B / 0 B」会误导。
                            if !t.note.is_empty() {
                                ui.label(RichText::new(&t.note).size(10.0).color(Palette::TEXT_DIM));
                            } else {
                                let mut detail = format!("{} / {}", crate::ui::fmt_bytes(t.done as f64), crate::ui::fmt_bytes(t.total as f64));
                                // 模式徽标（如「直传」）紧跟在文件大小之后
                                if !t.tag.is_empty() {
                                    detail.push_str(&format!("  ·  {}", t.tag));
                                }
                                if t.speed > 0.0 {
                                    detail.push_str(&format!("  ·  {}", crate::ui::fmt_rate(t.speed)));
                                    // ETA：剩余字节 / 当前速度（仅未暂停、有剩余、速度有效时）
                                    if !t.paused && t.total > t.done {
                                        let eta = ((t.total - t.done) as f64 / t.speed).round() as u64;
                                        detail.push_str(&format!("  ·  {} {}", crate::i18n::tr("剩余", "ETA"), fmt_dur(eta)));
                                    }
                                }
                                ui.label(RichText::new(detail).size(10.0).color(Palette::TEXT_DIM));
                            }
                        }
                        // 失败且已展开：显示失败原因
                        if t.ok == Some(false) && t.show_err && !t.message.is_empty() {
                            ui.label(RichText::new(&t.message).color(Palette::DANGER).size(11.0));
                        }
                    });
                    // 右键菜单：打开所在文件 / 删除记录 / 删文件并删记录
                    item.response.context_menu(|ui| {
                        if let Some(local) = &t.local {
                            if ui.button(crate::i18n::tr("打开所在文件", "Reveal file")).clicked() {
                                open_dir = Some(local.clone());
                                ui.close();
                            }
                        }
                        // 仅「已完成/失败」的行可移除；进行中/等待中的行移除会让其追踪任务
                        // （直传 DirectJob / 中转 Relay）永久卡住 poll 不存在的 id，故不提供
                        if t.ok.is_some() {
                            if ui.button(crate::i18n::tr("删除记录", "Remove from list")).clicked() {
                                remove_id = Some((uid, t.id));
                                ui.close();
                            }
                            if let Some(local) = &t.local {
                                if ui.button(RichText::new(crate::i18n::tr("删除文件并移除记录", "Delete file & remove")).color(Palette::DANGER)).clicked() {
                                    delete_id = Some((uid, t.id, local.clone()));
                                    ui.close();
                                }
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                // 有任务但当前筛选下一条都没有：给出「无匹配」提示，避免看着像空列表
                if shown == 0 && total_len > 0 {
                    ui.add_space(6.0);
                    crate::ui::empty_state(ui, egui_phosphor::regular::MAGNIFYING_GLASS, crate::i18n::tr("该筛选下暂无任务", "No transfers match this filter"), false);
                }
                });
                if let Some(p) = open_dir {
                    open_containing_folder(&p);
                }
                if total_len > 0 {
                    ui.separator();
                    // 批量操作：仅在对应状态存在时显示，避免无意义按钮
                    let any_active = items.iter().any(|(_, t)| t.ok.is_none());
                    let any_failed = items.iter().any(|(_, t)| t.ok == Some(false) && t.spec.is_some());
                    let any_done = items.iter().any(|(_, t)| t.ok.is_some());
                    ui.horizontal(|ui| {
                        if any_active && ui.button(crate::i18n::tr("全部取消", "Cancel all")).clicked() {
                            cancel_all = true;
                        }
                        if any_failed && ui.button(crate::i18n::tr("重试失败", "Retry failed")).clicked() {
                            retry_all = true;
                        }
                        if any_done && ui.button(crate::i18n::tr("清除已完成", "Clear done")).clicked() {
                            clear = true;
                        }
                    });
                }
            });
        if clear {
            for &i in &server_idxs {
                if let Some(s) = self.sessions.get_mut(i) {
                    s.transfers.retain(|t| t.ok.is_none());
                }
            }
        }
        // 全部取消：对同服务器所有会话里进行中的任务下发取消
        if cancel_all {
            // 跳过 queued 占位行（worker 未登记）；镜像行经 cancel_target 转到源端真实传输
            let raw: Vec<(u64, u64)> = server_idxs
                .iter()
                .filter_map(|&i| self.sessions.get(i))
                .flat_map(|s| {
                    s.transfers
                        .iter()
                        .filter(|t| t.ok.is_none() && !t.queued)
                        .map(move |t| (s.uid, t.id))
                })
                .collect();
            for (uid, id) in raw {
                let (tu, ti) = self.cancel_target(uid, id);
                if let Some(s) = self
                    .session_idx_by_uid(tu)
                    .and_then(|i| self.sessions.get(i))
                {
                    let _ = s.cmd_tx.send(UiCommand::CancelTransfer(ti));
                }
            }
            self.xfer_just_opened = true;
        }
        // 重试全部失败：对同服务器各会话每个有重发规格的失败任务重新发起（续传语义，覆盖）
        if retry_all {
            for &i in &server_idxs {
                if let Some(s) = self.sessions.get_mut(i) {
                    let targets: Vec<(u64, XferSpec)> = s
                        .transfers
                        .iter()
                        .filter(|t| t.ok == Some(false))
                        .filter_map(|t| t.spec.clone().map(|sp| (t.id, sp)))
                        .collect();
                    for (id, spec) in targets {
                        match spec {
                            XferSpec::Download { remote, local } => {
                                let _ = s.cmd_tx.send(UiCommand::Download {
                                    id,
                                    remote,
                                    local,
                                    policy: ConflictPolicy::Overwrite,
                                });
                            }
                            XferSpec::Upload { local, remote_dir } => {
                                let _ = s.cmd_tx.send(UiCommand::Upload {
                                    id,
                                    local,
                                    remote_dir,
                                    policy: ConflictPolicy::Overwrite,
                                });
                            }
                        }
                        if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                            t.ok = None;
                            t.paused = false;
                            t.show_err = false;
                            t.message = crate::i18n::tr("重试中 …", "Retrying …").into();
                        }
                    }
                }
            }
            self.xfer_just_opened = true;
        }
        // 取消传输：镜像行/源行都经 cancel_target 路由到源端真实传输并标记 cancelled
        if let Some((uid, id)) = cancel_id {
            let (tu, ti) = self.cancel_target(uid, id);
            if let Some(s) = self
                .session_idx_by_uid(tu)
                .and_then(|i| self.sessions.get(i))
            {
                let _ = s.cmd_tx.send(UiCommand::CancelTransfer(ti));
            }
            self.xfer_just_opened = true; // 避免点击被当作窗外点击而关窗
        }
        // 续传/重试：按重发规格重新发起，底层据已有字节自动续传
        if let Some((uid, id)) = resume_id {
            if let Some(i) = self.session_idx_by_uid(uid) {
                let s = &mut self.sessions[i];
                if let Some(spec) = s
                    .transfers
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.spec.clone())
                {
                    match spec {
                        XferSpec::Download { remote, local } => {
                            let _ = s.cmd_tx.send(UiCommand::Download {
                                id,
                                remote,
                                local,
                                policy: ConflictPolicy::Overwrite,
                            });
                        }
                        XferSpec::Upload { local, remote_dir } => {
                            let _ = s.cmd_tx.send(UiCommand::Upload {
                                id,
                                local,
                                remote_dir,
                                policy: ConflictPolicy::Overwrite,
                            });
                        }
                    }
                    if let Some(t) = s.transfers.iter_mut().find(|t| t.id == id) {
                        t.ok = None;
                        t.paused = false;
                        t.show_err = false;
                        t.message = crate::i18n::tr("续传中 …", "Resuming …").into();
                    }
                }
            }
            self.xfer_just_opened = true;
        }
        // 切换失败原因展开
        if let Some((uid, id)) = toggle_err {
            if let Some(i) = self.session_idx_by_uid(uid) {
                if let Some(t) = self.sessions[i].transfers.iter_mut().find(|t| t.id == id) {
                    t.show_err = !t.show_err;
                }
            }
            self.xfer_just_opened = true;
        }
        // 删除记录（仅移除列表项）
        if let Some((uid, id)) = remove_id {
            if let Some(i) = self.session_idx_by_uid(uid) {
                self.sessions[i].transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 删除文件并移除记录
        if let Some((uid, id, path)) = delete_id {
            let _ = std::fs::remove_file(&path);
            if let Some(i) = self.session_idx_by_uid(uid) {
                self.sessions[i].transfers.retain(|t| t.id != id);
            }
            self.xfer_just_opened = true;
        }
        // 选择默认下载目录（原生文件夹选择器）
        if pick_dir {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title(crate::i18n::tr(
                    "选择默认下载文件夹",
                    "Select default download folder",
                ))
                .pick_folder()
            {
                self.download_dir = dir.clone();
                crate::store::save_download_dir(&dir.to_string_lossy());
            }
            self.xfer_just_opened = true; // 选择期间点击不算"外部点击"，避免关窗
        }
        // 冲突策略循环切换：覆盖 → 跳过 → 重命名 → 覆盖，并持久化
        if cycle_policy {
            self.conflict_policy = match self.conflict_policy {
                ConflictPolicy::Overwrite => ConflictPolicy::Skip,
                ConflictPolicy::Skip => ConflictPolicy::Rename,
                ConflictPolicy::Rename => ConflictPolicy::Overwrite,
            };
            crate::store::save_conflict_policy(self.conflict_policy.as_str());
            self.xfer_just_opened = true; // 切换点击不算窗外点击
        }
        // 点击窗口外部任意位置自动隐藏（打开当帧除外，避免被开启动作立即关闭）
        let clicked_outside = win
            .as_ref()
            .map(|r| r.response.clicked_elsewhere())
            .unwrap_or(false);
        if close_win || (clicked_outside && !self.xfer_just_opened) {
            self.show_transfers = false;
        }
        self.xfer_just_opened = false;
    }
}
