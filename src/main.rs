mod app;
mod audio_file;
mod file_name;
#[cfg(target_os = "macos")]
mod finder_comment;
mod fonts;
mod lookup;
mod processor;
mod title_localizer;

use app::ContentRenamerApp;
use fonts::configure_fonts;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([920.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Theme Comment Writer",
        options,
        Box::new(|creation_context| {
            configure_fonts(&creation_context.egui_ctx);
            Ok(Box::new(ContentRenamerApp::default()))
        }),
    )
}
