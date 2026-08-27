use eframe::egui;
use floki::app;
use floki::gpu::{
    REQUIRED_DEVICE_FEATURES, missing_float32_filterable_message, probe_float32_filterable_adapters,
};

fn main() -> eframe::Result<()> {
    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

    // #247: probe before eframe opens a window. Without this, an adapter that
    // exists but lacks FLOAT32_FILTERABLE dies inside request_device with nothing
    // the user can report. We still require the feature below — this only fails
    // legibly (and logs every adapter for bug reports either way).
    let probe = probe_float32_filterable_adapters();
    for adapter in &probe.adapters {
        log::info!(
            "GPU adapter: {} ({}){}",
            adapter.name,
            adapter.backend,
            if probe.capable.as_ref().is_some_and(|c| c == adapter) {
                " [FLOAT32_FILTERABLE]"
            } else {
                ""
            }
        );
    }
    let Some(chosen) = probe.capable else {
        let msg = missing_float32_filterable_message(&probe.adapters);
        eprintln!("{msg}");
        log::error!("{msg}");
        // Native dialog so "it doesn't open" still leaves something on screen.
        // Headless / no-display: show() is best-effort; stderr already has the text.
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Floki cannot start")
            .set_description(&msg)
            .show();
        return Ok(());
    };
    log::info!(
        "Selected GPU adapter {} ({}) — FLOAT32_FILTERABLE present",
        chosen.name,
        chosen.backend
    );

    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(mut setup) = wgpu_options.wgpu_setup {
        setup.device_descriptor =
            std::sync::Arc::new(|_| eframe::egui_wgpu::wgpu::DeviceDescriptor {
                label: Some("egui wgpu device"),
                required_features: eframe::egui_wgpu::wgpu::Features::default()
                    | REQUIRED_DEVICE_FEATURES,
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
