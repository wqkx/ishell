//! Screenshot fixture driver.

use super::App;

impl App {
    /// 截图自检：到达指定帧请求截图，收到后写 PNG 并退出。
    pub(super) fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint(); // 保持持续渲染

        // 收到截图事件 -> 保存退出
        save_captured(ctx, &shot.path);

        // ISHELL_SHOT_EDITOR=1：截编辑器那个独立 OS 窗口而不是主窗口。截图在被截 viewport
        // 自己绘制时才会真正执行，事件也送到它自己的输入里（由编辑器窗口回调里的
        // `save_editor_shot` 接）——所以要一直催它重绘，否则空闲的编辑器窗口永远不出图。
        let target = if std::env::var_os("ISHELL_SHOT_EDITOR").is_some() {
            let vid = egui::ViewportId::from_hash_of("ishell_editor");
            ctx.request_repaint_of(vid);
            vid
        } else {
            egui::ViewportId::ROOT
        };
        if std::time::Instant::now() >= shot.deadline && !shot.requested {
            shot.requested = true;
            ctx.send_viewport_cmd_to(
                target,
                egui::ViewportCommand::Screenshot(egui::UserData::default()),
            );
        }
    }
}

/// 本帧输入里有截图事件就写成 PNG 并退出进程；没有则无操作。
fn save_captured(ctx: &egui::Context, path: &str) {
    let image = ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        })
    });
    let Some(img) = image else { return };
    let [w, h] = [img.size[0] as u32, img.size[1] as u32];
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    for p in img.pixels.iter() {
        buf.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
    }
    if let Some(im) = image::RgbaImage::from_raw(w, h, buf) {
        let _ = im.save(path);
    }
    std::process::exit(0);
}

/// 编辑器窗口侧的截图自检出口（见 `drive_screenshot` 的 ISHELL_SHOT_EDITOR）。
/// 正常使用时没人请求截图，这里只是扫一遍本帧事件。
pub(super) fn save_editor_shot(vctx: &egui::Context) {
    let has_shot = vctx.input(|i| {
        i.events
            .iter()
            .any(|e| matches!(e, egui::Event::Screenshot { .. }))
    });
    if has_shot {
        if let Ok(path) = std::env::var("ISHELL_SHOT") {
            save_captured(vctx, &path);
        }
    }
}
