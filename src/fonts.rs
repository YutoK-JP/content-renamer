use std::fs;
use std::path::Path;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

#[cfg(target_os = "macos")]
const JAPANESE_FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/Library/Fonts/Arial Unicode.ttf",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
];

pub fn configure_fonts(ctx: &egui::Context) {
    #[cfg(target_os = "macos")]
    if let Some(font_bytes) = load_first_available_font(JAPANESE_FONT_CANDIDATES) {
        let mut fonts = FontDefinitions::default();
        fonts
            .font_data
            .insert("jp_ui".to_string(), FontData::from_owned(font_bytes).into());

        if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
            family.insert(0, "jp_ui".to_string());
        }

        if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
            family.push("jp_ui".to_string());
        }

        ctx.set_fonts(fonts);
    }
}

#[cfg(target_os = "macos")]
fn load_first_available_font(paths: &[&str]) -> Option<Vec<u8>> {
    for path in paths {
        if let Ok(bytes) = fs::read(Path::new(path)) {
            return Some(bytes);
        }
    }

    None
}
