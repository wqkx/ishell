use egui::{RichText, Sense};

use super::super::{normalize_path, FileAction, FilePanelState};
use super::helpers::{tool_btn, tool_btn_color, trailing_path};
use crate::theme::Palette;

/// 收藏夹弹窗最多同时显示几行（再多就在弹窗内滚动）。
/// 五行是「一眼扫得完」和「不糊住半个文件区」之间的折中。
const MAX_FAV_ROWS: usize = 5;

/// 收藏夹弹窗里那个可滚动的路径列表。
///
/// `min_scrolled_height` 不是可有可无的装饰，它是「收藏夹偶尔只剩两行高」的解药：
/// `ScrollArea` 的高度取的是 `min(可用高度, max_height)`（egui 0.34
/// `scroll_area.rs`：`available_rect_before_wrap().size().at_most(max_size)`），
/// 而弹窗所在 `Area` 给出的可用高度来自它**上一帧量到的尺寸**（`area.rs`:
/// `let max_rect = self.state.rect()`）。两者一咬合就是个**只能变小的棘轮**：
/// 头一回只有一两条收藏时弹窗被量成很矮，之后收藏变多，可用高度仍是那个矮值，
/// 列表再也长不回来，最终卡在 `min_scrolled_size` 的默认下限 64px——按真实行高 28px
/// 算正好「两行左右」，与用户描述的现象完全吻合。而 `Area` 状态是按 id 持久化的，
/// 所以一旦卡住就一直卡着，重开弹窗也不恢复。
///
/// 把下限顶到 `max_h`，高度恒等于 `max_h`，棘轮不成立；顺带也让 `Popup` 的自动翻转
/// 重新生效——此前列表会自己缩到「放得下」，弹窗因此从不觉得下方空间不够，
/// 于是永远不会翻到按钮上方去。
fn fav_scroll_area(max_h: f32) -> egui::ScrollArea {
    egui::ScrollArea::vertical()
        .max_height(max_h)
        .min_scrolled_height(max_h)
}

pub(super) fn toolbar(
    ui: &mut egui::Ui,
    state: &mut FilePanelState,
    actions: &mut Vec<FileAction>,
) {
    // 工具栏：扁平图标条（带浅色背景）
    use egui_phosphor::regular as icon;
    let mut bc_nav: Option<String> = None;
    egui::Frame::new()
        .fill(Palette::PANEL_2)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(6, 4))
        // 左侧外边距、不给右侧留边（右侧顶到边）
        .outer_margin(egui::Margin {
            left: 8,
            right: 0,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // 工具栏按钮：后退 / 刷新 / 复制路径 / 收藏夹（收藏放最后）。按钮之间不加
                // 分割线，保持一条扁平图标条。上传 / 删除 / 粘贴均不放工具栏：上传走拖拽或
                // 空白处右键菜单，删除走右键菜单或 Delete 键，粘贴走空白处右键菜单「粘贴到
                // 此目录」（保持工具栏精简）。上级目录改由面包屑点击。
                // 后退（浏览器式后退，可连续返回）
                let back_enabled = !state.nav_history.is_empty();
                let back_col = if back_enabled {
                    Palette::TEXT
                } else {
                    Palette::TEXT_DIM
                };
                if tool_btn_color(
                    ui,
                    icon::CARET_LEFT,
                    crate::i18n::tr("返回上一个目录", "Back"),
                    back_col,
                ) && back_enabled
                {
                    if let Some(prev) = state.nav_history.pop() {
                        state.nav_pending_back = true;
                        state.cwd = prev;
                        state.selected.clear();
                    }
                }

                // 刷新
                if tool_btn(
                    ui,
                    icon::ARROW_CLOCKWISE,
                    crate::i18n::tr("刷新", "Refresh"),
                ) && state.refresh_dir(&state.cwd.clone())
                {
                    actions.push(FileAction::List(state.cwd.clone()));
                }

                // 复制路径：点击后短暂显示绿色对勾，再恢复
                let now = ui.input(|i| i.time);
                let copied = state.copy_flash.is_some_and(|t| now - t < 1.1);
                let (ci, ctip, ccol) = if copied {
                    (
                        icon::CHECK,
                        crate::i18n::tr("已复制", "Copied"),
                        Palette::OK,
                    )
                } else {
                    (
                        icon::COPY,
                        crate::i18n::tr("复制当前路径", "Copy path"),
                        Palette::TEXT,
                    )
                };
                if tool_btn_color(ui, ci, ctip, ccol) && !state.cwd.is_empty() {
                    actions.push(FileAction::CopyPath(state.cwd.clone()));
                    state.copy_flash = Some(now);
                }
                if copied {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(150));
                }

                // 收藏夹（最后一个按钮）：弹出可滚动路径列表，点路径进入、右侧删除，点其它处关闭。
                // 用 phosphor STAR 图标（与刷新/复制等同一字体，笔画粗细一致）——已收藏用暖色
                // 实心感的强调色标识，未收藏用普通文字色；弹窗打开时按钮显示按下态（灰底）。
                let cwd_fav =
                    !state.cwd.is_empty() && state.favorites.iter().any(|f| f == &state.cwd);
                let pop_id = ui.make_persistent_id("fav_popup");
                let pop_open = egui::Popup::is_id_open(ui.ctx(), pop_id);
                let star_col = if cwd_fav {
                    Palette::ACCENT
                } else {
                    Palette::TEXT
                };
                let star = ui
                    .scope(|ui| {
                        let v = ui.visuals_mut();
                        v.widgets.inactive.weak_bg_fill = if pop_open {
                            Palette::TRACK
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
                        v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
                        v.widgets.active.bg_stroke = egui::Stroke::NONE;
                        ui.add(
                            egui::Button::new(RichText::new(icon::STAR).size(16.0).color(star_col))
                                .min_size(egui::vec2(30.0, 26.0))
                                .corner_radius(6.0),
                        )
                        .on_hover_text(crate::i18n::tr("收藏夹", "Favorites"))
                    })
                    .inner;
                // 点开收藏夹时从磁盘重读一次：别的标签页（同一台服务器可以开好几个）可能已经
                // 增删过收藏，而本标签手里还是它打开那一刻的快照。不重读的话，用户会看到
                // 「明明收藏了五六个，点开只剩两行」——那正是本次要修的现象。
                // 一次点击读一个小 JSON，开销可以忽略。
                if star.clicked() {
                    state.favorites = crate::store::load_favorites(&state.server_key);
                }
                let mut fav_nav: Option<String> = None;
                // 按**路径**删，不按下标：下标是相对本标签那份可能过时的快照算的。
                let mut fav_remove: Option<String> = None;
                // 收藏数量决定弹窗高度，最多五行；再多就滚动。
                let fav_rows = state.favorites.len().clamp(1, MAX_FAV_ROWS);
                let fav_row_h =
                    ui.spacing().interact_size.y.max(18.0) + ui.spacing().item_spacing.y;
                let fav_max_h = fav_row_h * fav_rows as f32;
                // 用 `Popup` 而不是 `popup_below_widget`：后者内部写死 `AboveOrBelow::Below`，
                // 不会翻转。文件区工具栏本就靠近窗口下沿，弹窗被屏幕底边裁掉后无论收藏多少
                // 都只剩两三行——这才是「收藏很多却只展开 2 行」的真正原因，跟高度设多少无关。
                // `Popup` 默认会在放不下时依次试 `RectAlign::symmetries` / `MENU_ALIGNS`，
                // 下方不够就自动翻到按钮上方。
                egui::Popup::from_toggle_button_response(&star)
                    .id(pop_id)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| {
                        ui.set_min_width(280.0);
                        if state.favorites.is_empty() {
                            crate::ui::empty_state(
                                ui,
                                egui_phosphor::regular::STAR,
                                crate::i18n::tr(
                                    "暂无收藏，右键文件夹/空白处可加入",
                                    "No favorites — right-click to add",
                                ),
                                false,
                            );
                        } else {
                            fav_scroll_area(fav_max_h).show(ui, |ui| {
                                for p in state.favorites.iter() {
                                    ui.horizontal(|ui| {
                                        ui.set_min_width(266.0);
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            RichText::new(icon::TRASH)
                                                                .size(12.0)
                                                                .color(Palette::TEXT_DIM),
                                                        )
                                                        .frame(false),
                                                    )
                                                    .on_hover_text(crate::i18n::tr(
                                                        "删除收藏",
                                                        "Remove",
                                                    ))
                                                    .clicked()
                                                {
                                                    fav_remove = Some(p.clone());
                                                }
                                                ui.with_layout(
                                                    egui::Layout::left_to_right(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        let disp = trailing_path(p, 40);
                                                        // `show_tooltip_when_elided(false)`：
                                                        // 否则 egui 自带的「省略即提示完整文字」
                                                        // 会和下面这条 tooltip 叠成两个（同
                                                        // table_rows.rs 里文件名那处的老毛病）。
                                                        if ui
                                                            .add(
                                                                egui::Label::new(
                                                                    RichText::new(format!(
                                                                        "{} {}",
                                                                        icon::FOLDER,
                                                                        disp
                                                                    ))
                                                                    .size(12.0)
                                                                    .color(Palette::TEXT),
                                                                )
                                                                .selectable(false)
                                                                .show_tooltip_when_elided(false)
                                                                .sense(Sense::click()),
                                                            )
                                                            .on_hover_text(p.as_str())
                                                            .clicked()
                                                        {
                                                            fav_nav = Some(p.clone());
                                                        }
                                                    },
                                                );
                                            },
                                        );
                                    });
                                }
                            });
                        }
                    });
                if let Some(p) = fav_remove {
                    state.favorites = crate::store::remove_favorite(&state.server_key, &p);
                }
                if let Some(p) = fav_nav {
                    state.cwd = p;
                    state.selected.clear();
                    egui::Popup::close_id(ui.ctx(), pop_id);
                }

                ui.add_space(4.0);
                ui.separator();
                // 路径栏最右侧：当前目录列举失败（无效/无权限路径）时显示「路径无效」标识
                let path_err =
                    state.nav_error.contains(&state.cwd) && !state.loading.contains(&state.cwd);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if path_err {
                        ui.add(
                            egui::Label::new(
                                RichText::new(icon::WARNING_CIRCLE)
                                    .color(Palette::DANGER)
                                    .size(15.0),
                            )
                            .sense(Sense::hover()),
                        )
                        .on_hover_text(crate::i18n::tr(
                            "路径无效或无法访问",
                            "Invalid or inaccessible path",
                        ));
                        ui.add_space(3.0);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if let Some(p) = super::super::pathbar::show(
                            ui,
                            &mut super::super::pathbar::Crumbs {
                                cwd: &state.cwd,
                                edit: &mut state.path_edit,
                                focus: &mut state.path_edit_focus,
                                rsel: &mut state.path_edit_rsel,
                                ime: &mut state.path_edit_ime,
                                pending: &mut state.pending_nav,
                                trail: &mut state.path_trail,
                                id_salt: "file_path_edit_box",
                            },
                        ) {
                            bc_nav = Some(p);
                        }
                    }); // 内层 left_to_right（面包屑/编辑框）
                }); // 外层 right_to_left（右侧无效标识）
            });
        });
    ui.add_space(2.0);
    if let Some(p) = bc_nav {
        // 展开 `~`（SFTP 不做 shell 的 tilde 展开）并规范化末尾多余的 "/"
        // （否则与 worker 返回的规范路径不匹配，无法进入）
        state.cwd = normalize_path(&state.expand_tilde(&p));
        state.selected.clear();
        // 「粘贴并转到」等跳转必须退出路径编辑态：否则若此前处于编辑态，路径栏会继续显示
        // 旧的编辑框内容，造成「列表变了、面包屑没变」的错觉。
        state.path_edit = None;
        state.path_edit_focus = false;
        state.pending_nav = None;
        // 显式「跳转到路径」是用户主动动作，可信度高于被动预取：若这个路径之前被判定过
        // 「无效」（比如粘贴的文件夹是刚在另一个终端里创建的，上次探测时还不存在），不能让
        // 那次陈旧判定继续拦着——清掉无效标记与占位空列表，强制重新查一次，不然只能靠用户
        // 先跳父级目录刷新（连带清子目录缓存）才能间接清掉这个陈旧状态。
        if state.nav_error.remove(&state.cwd) {
            state.listings.remove(&state.cwd);
            state.touch_listings();
        }
        // 跳到未缓存的路径（如「粘贴并转到」到一个此前没进过的目录）：本帧 sync_tree 已在改 cwd 前
        // 跑过、不会再列举它，若不在此显式发起 List，会卡在空白/不加载。命中缓存则无需重复请求。
        if !state.listings.contains_key(&state.cwd) && !state.loading.contains(&state.cwd) {
            state.loading.insert(state.cwd.clone());
            actions.push(FileAction::List(state.cwd.clone()));
        }
    }
}

#[cfg(test)]
mod fav_popup_tests {
    use super::*;
    use egui_phosphor::regular as icon;

    /// 收藏夹弹窗的高度是按「估算的每行高度 × 行数」给的（`fav_row_h`）。这个估算一旦小于
    /// **真实**行高，弹窗就会少显示几行——而且是无声的，看起来就像「收藏丢了」。
    ///
    /// 这里在无窗口的 egui 里按弹窗里逐字一致的标记摆一行出来，量它的真实高度，
    /// 确认估算没有偏小。（排查用户报的「只显示 2 行」时写的：结论是这里没问题，
    /// 真因是收藏列表本身被别的标签页的陈旧快照覆盖了，见 store::favorites 的测试。）
    /// 收藏夹弹窗「偶尔只剩两行左右的高度」的回归门禁。
    ///
    /// 复现的是 `fav_scroll_area` 注释里那个棘轮：`ScrollArea` 的高度是
    /// `min(可用高度, max_height)`，而 `Area` 给的可用高度来自它上一帧量到的尺寸。
    /// 先用「1 行」渲染一帧把 `Area` 尺寸压小，再用「5 行」渲染——没有
    /// `min_scrolled_height` 时第二帧长不回来，会停在 egui 的默认下限 64px
    /// （按 28px 行高算就是两行多一点，正是用户看到的样子）。
    #[test]
    fn favorites_popup_height_does_not_ratchet_down() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);

        let area_id = egui::Id::new("fav_scroll_ratchet_probe");
        let row_h = 28.0f32;
        let mut measured = 0.0f32;
        // 三帧：字体/样式生效 → 用 1 行压小 Area → 换成 5 行看能不能长回来
        for rows in [1usize, 1, 5] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::Area::new(area_id)
                    .fixed_pos(egui::pos2(10.0, 10.0))
                    .show(ctx, |ui| {
                        let max_h = row_h * rows as f32;
                        let out = fav_scroll_area(max_h).show(ui, |ui| {
                            for i in 0..rows {
                                ui.label(format!("/some/favorite/path/{i}"));
                            }
                        });
                        measured = out.inner_rect.height();
                    });
            });
        }
        let want = row_h * 5.0;
        assert!(
            measured >= want - 1.0,
            "收藏夹列表被上一帧的矮尺寸卡住了：只长到 {measured}px，要 {want}px。\
             去掉 min_scrolled_height 就会退回 egui 的 64px 下限（约两行），\
             用户会看到「收藏夹只展开两行左右」。"
        );
    }

    #[test]
    fn row_height_estimate_is_not_smaller_than_reality() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);

        let mut estimate = 0.0f32;
        let mut actual = 0.0f32;
        // 跑两帧：第一帧让 set_fonts / set_style 生效
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    estimate = ui.spacing().interact_size.y.max(18.0) + ui.spacing().item_spacing.y;
                    let r = ui
                        .horizontal(|ui| {
                            ui.set_min_width(266.0);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let _ = ui.add(
                                        egui::Button::new(
                                            RichText::new(icon::TRASH)
                                                .size(12.0)
                                                .color(Palette::TEXT_DIM),
                                        )
                                        .frame(false),
                                    );
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            let _ = ui.add(
                                                egui::Label::new(
                                                    RichText::new(format!(
                                                        "{} {}",
                                                        icon::FOLDER,
                                                        "/home/u/some/dir"
                                                    ))
                                                    .size(12.0)
                                                    .color(Palette::TEXT),
                                                )
                                                .selectable(false)
                                                .sense(Sense::click()),
                                            );
                                        },
                                    );
                                },
                            );
                        })
                        .response;
                    actual = r.rect.height();
                });
            });
        }
        assert!(
            estimate >= actual,
            "每行高度估算 {estimate} 小于真实行高 {actual}：弹窗会少显示几行收藏，\
             且看起来像是收藏丢了。改了行内控件/样式就要跟着调 fav_row_h。"
        );
        assert!(actual > 0.0, "没量到行高，测试本身失效了");
    }
}
