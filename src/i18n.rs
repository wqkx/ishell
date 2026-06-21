//! 极简多语言：全局当前语言 + `tr(中文, English)` 取串。
//!
//! 用法：把界面文案写成 `tr("内存", "Mem")`，运行时按当前语言返回。
//! 英文尽量用缩写，保持与中文等宽，避免界面变形。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
    pub fn from_code(s: &str) -> Lang {
        if s.trim().eq_ignore_ascii_case("en") {
            Lang::En
        } else {
            Lang::Zh
        }
    }
    /// 菜单里显示的自称名（不随当前语言变化）
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }
}

static CUR: AtomicU8 = AtomicU8::new(0); // 0=Zh, 1=En

pub fn set(lang: Lang) {
    CUR.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    if CUR.load(Ordering::Relaxed) == 1 {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// 按当前语言返回文案。
#[inline]
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    match current() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 渲染语言选项（中文 / English），选中即切换并持久化。供右键菜单使用。
pub fn language_menu(ui: &mut egui::Ui) {
    for l in [Lang::Zh, Lang::En] {
        if ui.selectable_label(current() == l, l.label()).clicked() {
            set(l);
            crate::store::save_lang(l.code());
            ui.close();
        }
    }
}

/// 给任意 response 附加「右键 → 语言」菜单。
/// 用于操作栏各处（含会捕获次级点击的可点击行），避免出现右键死角。
pub fn lang_context_menu(resp: &egui::Response) {
    resp.context_menu(|ui| {
        ui.label(egui::RichText::new(tr("语言", "Language")).color(crate::theme::Palette::TEXT_DIM).size(11.0));
        language_menu(ui);
    });
}
