//! Background DOCX parsing and texture preparation.

use super::App;

impl App {
    pub(super) fn spawn_docx_parse(
        &self,
        ui: &egui::Ui,
        id: u64,
        data: Vec<u8>,
        uid: u64,
        tpath: String,
    ) {
        let ctx2 = ui.ctx().clone();
        let tx = self.doc_parse_tx.clone();
        std::thread::spawn(move || {
            let res = match crate::ui::docx::parse(&data) {
                Ok(mut doc) => {
                    let mut images = std::collections::HashMap::new();
                    // 图片纹理上限 100 张：图海文档防内存/显存失控
                    for (name, bytes) in doc.media.iter().take(100) {
                        if let Ok(mut img) = image::load_from_memory(bytes) {
                            // 大图降采样到 ≤1600px：相机原图直接建纹理动辄几十 MB，
                            // 阅读视图用不到原始分辨率（这是 docx 内存高的大头）
                            if img.width() > 1600 || img.height() > 1600 {
                                img = img.thumbnail(1600, 1600);
                            }
                            let rgba = img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let color =
                                egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                            images.insert(
                                name.clone(),
                                ctx2.load_texture(
                                    format!("docx:{uid}:{tpath}:{name}"),
                                    color,
                                    egui::TextureOptions::LINEAR,
                                ),
                            );
                        }
                    }
                    doc.media = Vec::new(); // 原始字节释放（内存大头）
                    Ok((doc, images))
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send((uid, id, res));
            ctx2.request_repaint();
            ctx2.request_repaint_of(egui::ViewportId::from_hash_of("ishell_editor"));
        });
    }
}
