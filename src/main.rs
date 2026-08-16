use eframe::egui;
use floki::app;

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
            .with_title("Floki"),
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "Floki",
        options,
        Box::new(|cc| {
            // This gives us image support:
            egui_extras::install_image_loaders(&cc.egui_ctx);

            let mut app = app::ExrApp::new(cc);
            // Open a file given on the command line. Until now argv was ignored
            // entirely, so `floki shot.0001.exr` (and the `run` / `soak` wrappers
            // in scripts/run-windows.ps1, whose help has always documented it)
            // silently opened an empty session. Routed through the normal
            // open/drop entry so a CLI launch exercises the real default path.
            if let Some(arg) = std::env::args().nth(1) {
                app.open_cli_path(std::path::PathBuf::from(arg));
            }
            Ok(Box::new(app))
        }),
    )
}
