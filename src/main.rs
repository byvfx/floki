mod app;
mod color;
mod exr_loader;
mod gpu;
mod tools;
mod viewer;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(mut setup) = wgpu_options.wgpu_setup {
        setup.device_descriptor =
            std::sync::Arc::new(|_| eframe::egui_wgpu::wgpu::DeviceDescriptor {
                label: Some("egui wgpu device"),
                required_features: eframe::egui_wgpu::wgpu::Features::default()
                    | eframe::egui_wgpu::wgpu::Features::FLOAT32_FILTERABLE,
                ..Default::default()
            });
        wgpu_options.wgpu_setup = eframe::egui_wgpu::WgpuSetup::CreateNew(setup);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("EXR Analyzer"),
        wgpu_options,
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
