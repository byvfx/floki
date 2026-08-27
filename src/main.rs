use eframe::egui;
use floki::app;

/// Enumerate the GPU adapters and summarize them for the preflight (#247).
///
/// Uses the same env-derived instance descriptor as the device eframe will create,
/// so `WGPU_BACKEND` selects the same set here as there — a probe that looked at a
/// different list than the app ends up using would be worse than none.
fn survey_adapters() -> Vec<floki::AdapterSummary> {
    use eframe::egui_wgpu::wgpu;
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
        .iter()
        .map(|a| {
            let info = a.get_info();
            floki::AdapterSummary {
                name: info.name.clone(),
                backend: format!("{:?}", info.backend),
                device_type: format!("{:?}", info.device_type),
                float32_filterable: a.features().contains(wgpu::Features::FLOAT32_FILTERABLE),
            }
        })
        .collect()
}

fn main() -> eframe::Result<()> {
    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

    // GPU preflight (#247). `FLOAT32_FILTERABLE` is required at device creation, so
    // an adapter without it made `request_device` fail and the app never opened a
    // window — no message, nothing for a tester to report but "it doesn't open".
    // Probe first and say something a person can act on.
    let adapters = survey_adapters();
    for a in &adapters {
        log::info!(
            target: "floki",
            "adapter: {} ({}, {}) float32_filterable={}",
            a.name, a.backend, a.device_type, a.float32_filterable
        );
    }
    if let Some(msg) = floki::gpu_preflight_error(&adapters) {
        // Both, deliberately: the dialog is for the person who double-clicked the
        // exe and has no console, the stderr copy is what ends up pasted into a bug
        // report.
        eprintln!("{msg}");
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Floki cannot start")
            .set_description(&msg)
            .show();
        std::process::exit(1);
    }

    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(mut setup) = wgpu_options.wgpu_setup {
        setup.device_descriptor =
            std::sync::Arc::new(|adapter: &eframe::egui_wgpu::wgpu::Adapter| {
                // Which adapter eframe actually picked, which the survey above cannot
                // know — it lists what exists, this is what got chosen. Worth a line in
                // every run, not just a failing one: "which GPU was it on" is the first
                // question of most bug reports here.
                let info = adapter.get_info();
                let ok = adapter
                    .features()
                    .contains(eframe::egui_wgpu::wgpu::Features::FLOAT32_FILTERABLE);
                log::info!(
                    target: "floki",
                    "using adapter: {} ({:?}, {:?}) driver={:?} float32_filterable={ok}",
                    info.name, info.backend, info.device_type, info.driver_info
                );
                if !ok {
                    // The preflight found *some* qualifying adapter or we would have
                    // exited; eframe picking a different one is the case that survey
                    // can't catch, and device creation is about to fail. Say why here,
                    // since this is the last point that knows.
                    log::error!(
                        target: "floki",
                        "the chosen adapter lacks FLOAT32_FILTERABLE — device creation \
                         will fail. Another adapter on this machine does support it; \
                         set WGPU_BACKEND, or select the discrete GPU for Floki."
                    );
                }
                eframe::egui_wgpu::wgpu::DeviceDescriptor {
                    label: Some("egui wgpu device"),
                    required_features: eframe::egui_wgpu::wgpu::Features::default()
                        | eframe::egui_wgpu::wgpu::Features::FLOAT32_FILTERABLE,
                    ..Default::default()
                }
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
