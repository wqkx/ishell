//! Screenshot fixture driver.

use super::App;

impl App {
    /// 截图自检：到达指定帧请求截图，收到后写 PNG 并退出。
    pub(super) fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint(); // 保持持续渲染

        // 收到截图事件 -> 保存退出
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let [w, h] = [img.size[0] as u32, img.size[1] as u32];
            let mut buf = Vec::with_capacity((w * h * 4) as usize);
            for p in img.pixels.iter() {
                buf.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
            }
            if let Some(im) = image::RgbaImage::from_raw(w, h, buf) {
                let _ = im.save(&shot.path);
            }
            std::process::exit(0);
        }

        if std::time::Instant::now() >= shot.deadline && !shot.requested {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
    }
}
