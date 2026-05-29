mod app;
mod exr_loader;
mod viewer;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("EXR Analyzer"),
        ..Default::default()
    };
    eframe::run_native(
        "EXR Analyzer",
        options,
        Box::new(|cc| {
            // This gives us image support:
            egui_extras::install_image_loaders(&cc.egui_ctx);

            Ok(Box::new(app::ExrApp::new(cc)))
        }),
    )
}
