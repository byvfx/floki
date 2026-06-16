use crate::exr_loader::ExrData;
use crate::viewer::ExrViewer;
use eframe::egui;
use rfd::FileDialog;
use std::path::PathBuf;

/// User theme preference, persisted across sessions. Maps to egui's
/// [`egui::ThemePreference`]; `System` follows the OS light/dark setting.
#[derive(serde::Deserialize, serde::Serialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

impl From<ThemeChoice> for egui::ThemePreference {
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::System => egui::ThemePreference::System,
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ExrApp {
    #[serde(skip)]
    loaded_file: Option<PathBuf>,
    #[serde(skip)]
    loaded_file_b: Option<PathBuf>,
    #[serde(skip)]
    exr_data: Option<ExrData>,
    #[serde(skip)]
    exr_data_b: Option<ExrData>,
    #[serde(skip)]
    error_msg: Option<String>,
    #[serde(skip)]
    viewer: ExrViewer,

    recent_files: Vec<PathBuf>,
    theme: ThemeChoice,

    #[serde(skip)]
    show_help: bool,
    #[serde(skip)]
    show_settings: bool,

    #[serde(skip)]
    pub render_state: Option<eframe::egui_wgpu::RenderState>,

    ocio_path: String,
    lut_path: String,
    pub enable_lut: bool,
    #[serde(skip)]
    pub lut_bg: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    pub lut_error: Option<String>,

    #[cfg(feature = "ocio")]
    #[serde(default)]
    ocio_display: String,
    #[cfg(feature = "ocio")]
    #[serde(default)]
    ocio_view: String,
    #[cfg(feature = "ocio")]
    #[serde(default)]
    ocio_input_cs: String,
    #[cfg(feature = "ocio")]
    #[serde(default)]
    pub ocio_enabled: bool,
    #[cfg(feature = "ocio")]
    #[serde(skip)]
    ocio_config: Option<floki_ocio::OcioConfig>,
    #[cfg(feature = "ocio")]
    #[serde(skip)]
    ocio_displays: Vec<floki_ocio::Display>,
    #[cfg(feature = "ocio")]
    #[serde(skip)]
    ocio_colorspaces: Vec<String>,
    #[cfg(feature = "ocio")]
    #[serde(skip)]
    ocio_error: Option<String>,
    #[cfg(feature = "ocio")]
    #[serde(skip)]
    ocio_ready: bool,
    #[cfg(feature = "ocio")]
    #[serde(skip)]
    ocio_cpu: Option<std::rc::Rc<floki_ocio::CpuProcessor>>,

    #[serde(skip)]
    show_tools_window: bool,
    #[serde(skip)]
    tools_input_dir: String,
    #[serde(skip)]
    tools_output_dir: String,
    #[serde(skip)]
    conversion_progress: Option<(usize, usize)>,
    #[serde(skip)]
    conversion_status: String,
    #[serde(skip)]
    conversion_receiver: Option<std::sync::mpsc::Receiver<(usize, usize, String)>>,
    #[serde(skip)]
    conversion_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for ExrApp {
    fn default() -> Self {
        Self {
            loaded_file: None,
            loaded_file_b: None,
            exr_data: None,
            exr_data_b: None,
            error_msg: None,
            viewer: ExrViewer::default(),
            recent_files: Vec::new(),
            theme: ThemeChoice::default(),
            show_help: false,
            show_settings: false,
            render_state: None,
            ocio_path: String::new(),
            lut_path: String::new(),
            enable_lut: false,
            lut_bg: None,
            lut_error: None,
            #[cfg(feature = "ocio")]
            ocio_display: String::new(),
            #[cfg(feature = "ocio")]
            ocio_view: String::new(),
            #[cfg(feature = "ocio")]
            ocio_input_cs: String::new(),
            #[cfg(feature = "ocio")]
            ocio_enabled: false,
            #[cfg(feature = "ocio")]
            ocio_config: None,
            #[cfg(feature = "ocio")]
            ocio_displays: Vec::new(),
            #[cfg(feature = "ocio")]
            ocio_colorspaces: Vec::new(),
            #[cfg(feature = "ocio")]
            ocio_error: None,
            #[cfg(feature = "ocio")]
            ocio_ready: false,
            #[cfg(feature = "ocio")]
            ocio_cpu: None,
            show_tools_window: false,
            tools_input_dir: String::new(),
            tools_output_dir: String::new(),
            conversion_progress: None,
            conversion_status: String::new(),
            conversion_receiver: None,
            conversion_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl ExrApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app: Self = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Self::default()
        };

        app.render_state = cc.wgpu_render_state.clone();

        if let Some(rs) = &app.render_state {
            let gpu_state = crate::gpu::GpuState::new(&rs.device, rs.target_format);
            rs.renderer.write().callback_resources.insert(gpu_state);
        }

        // `lut_bg` is a GPU handle and can't persist, but `enable_lut`/`lut_path`
        // do. Without rebuilding the bind group here, a restart leaves the LUT
        // "enabled" in the UI but silently inert. Rebuild it, or clear the flag so
        // the persisted state matches reality.
        if app.enable_lut && !app.lut_path.is_empty() {
            app.reload_lut();
            if app.lut_bg.is_none() {
                app.enable_lut = false;
            }
        }

        // OCIO state (config handle + GPU pass) can't persist either; rebuild from the
        // persisted path/display/view if OCIO was enabled.
        #[cfg(feature = "ocio")]
        if app.ocio_enabled {
            app.reload_ocio();
            if !app.ocio_ready {
                app.ocio_enabled = false;
            }
        }

        app
    }

    /// Load the OCIO config (from `ocio_path`, or built-in `ocio://default` if empty),
    /// enumerate its color spaces/displays/views, pick sensible defaults, and build the GPU
    /// pass. Errors land in `ocio_error` and clear `ocio_ready`.
    #[cfg(feature = "ocio")]
    fn reload_ocio(&mut self) {
        use floki_ocio::{ConfigSource, OcioConfig};

        self.ocio_error = None;
        // Precedence: explicit path > $OCIO env > built-in ACES.
        let env_ocio = std::env::var("OCIO").ok().filter(|v| !v.trim().is_empty());
        let src = if !self.ocio_path.trim().is_empty() {
            ConfigSource::File(std::path::Path::new(&self.ocio_path))
        } else if env_ocio.is_some() {
            ConfigSource::Env
        } else {
            ConfigSource::BuiltIn("ocio://default")
        };
        let cfg = match OcioConfig::load(src) {
            Ok(c) => c,
            Err(e) => {
                self.ocio_error = Some(format!("Load failed: {e}"));
                self.ocio_ready = false;
                self.ocio_config = None;
                return;
            }
        };

        self.ocio_colorspaces = cfg.color_spaces().into_iter().map(|c| c.name).collect();
        self.ocio_displays = cfg.displays();

        // Default any unset / now-invalid selections from the config.
        if !self
            .ocio_displays
            .iter()
            .any(|d| d.name == self.ocio_display)
        {
            self.ocio_display = cfg.default_display();
        }
        let views = self
            .ocio_displays
            .iter()
            .find(|d| d.name == self.ocio_display)
            .cloned();
        if let Some(d) = &views
            && !d.views.contains(&self.ocio_view)
        {
            self.ocio_view = d.default_view.clone();
        }
        if self.ocio_input_cs.is_empty() || !self.ocio_colorspaces.contains(&self.ocio_input_cs) {
            self.ocio_input_cs = cfg
                .scene_linear_colorspace()
                .filter(|s| self.ocio_colorspaces.contains(s))
                .or_else(|| self.ocio_colorspaces.first().cloned())
                .unwrap_or_default();
        }

        self.ocio_config = Some(cfg);
        self.rebuild_ocio_pass();
    }

    /// Rebuild just the GPU pass from the current config + input/display/view selection
    /// (cheaper than reloading the config when the user changes a dropdown).
    #[cfg(feature = "ocio")]
    fn rebuild_ocio_pass(&mut self) {
        use floki_ocio::DisplayTransformRequest;

        self.ocio_cpu = None;
        let Some(cfg) = &self.ocio_config else {
            self.ocio_ready = false;
            return;
        };
        if self.ocio_input_cs.is_empty()
            || self.ocio_display.is_empty()
            || self.ocio_view.is_empty()
        {
            self.ocio_ready = false;
            return;
        }
        let req = DisplayTransformRequest {
            input_colorspace: self.ocio_input_cs.clone(),
            display: self.ocio_display.clone(),
            view: self.ocio_view.clone(),
        };
        let bundle = match cfg.build_gpu_shader(&req) {
            Ok(b) => b,
            Err(e) => {
                self.ocio_error = Some(format!("Shader build failed: {e}"));
                self.ocio_ready = false;
                return;
            }
        };
        // CPU processor for thumbnails / fallback (best-effort; GPU path is primary).
        self.ocio_cpu = cfg.build_cpu_processor(&req).ok().map(std::rc::Rc::new);
        let Some(rs) = &self.render_state else {
            self.ocio_ready = false;
            return;
        };
        match crate::gpu::ocio_pass::OcioGpuPass::from_bundle(
            &rs.device,
            &rs.queue,
            &bundle,
            rs.target_format,
        ) {
            Ok(pass) => {
                rs.renderer.write().callback_resources.insert(pass);
                self.ocio_ready = true;
                self.ocio_error = None;
            }
            Err(e) => {
                self.ocio_error = Some(format!("Pipeline failed: {e}"));
                self.ocio_ready = false;
            }
        }
    }

    /// (Re)build the GPU LUT bind group from `self.lut_path`, updating `lut_bg`
    /// and `lut_error` to reflect the outcome. A parse failure clears `lut_bg`
    /// and disables the LUT. Does not force `enable_lut` on success — callers
    /// decide whether loading should auto-enable (the Browse button does).
    fn reload_lut(&mut self) {
        if self.lut_path.is_empty() {
            return;
        }
        let path = self.lut_path.clone();
        match crate::color::cube::CubeLut::load(&path) {
            Ok(lut) => {
                if let Some(rs) = &self.render_state {
                    let renderer = rs.renderer.read();
                    if let Some(gpu_state) =
                        renderer.callback_resources.get::<crate::gpu::GpuState>()
                    {
                        self.lut_bg =
                            Some(gpu_state.create_lut_bind_group(&rs.device, &rs.queue, &lut));
                        self.lut_error = None;
                    } else {
                        self.lut_error = Some("GPU state not found".to_string());
                    }
                } else {
                    self.lut_error = Some("Render state not found".to_string());
                }
            }
            Err(e) => {
                self.lut_error = Some(format!("Failed to load LUT: {}", e));
                self.lut_bg = None;
                self.enable_lut = false;
            }
        }
    }

    fn open_file(&mut self, path: PathBuf, is_b: bool) {
        if !is_b {
            self.recent_files.retain(|p| p != &path);
            self.recent_files.insert(0, path.clone());
            self.recent_files.truncate(10);
            self.loaded_file = Some(path.clone());
        } else {
            self.loaded_file_b = Some(path.clone());
        }

        match ExrData::load(&path) {
            Ok(data) => {
                if is_b {
                    self.exr_data_b = Some(data);
                    // The texture caches only rebuild on a layer-count change, so a new B
                    // with the same layer count would keep showing the previous image.
                    // Force the reference textures (and the B-dependent diff/composite) to
                    // regenerate from the new data.
                    self.viewer.invalidate_reference_textures();
                    // B isn't part of the histogram cache key — refresh it so the
                    // B histogram appears without waiting for a layer change.
                    self.viewer.invalidate_histogram();
                } else {
                    self.exr_data = Some(data);
                    self.exr_data_b = None; // Reset B when A changes
                    self.loaded_file_b = None;
                    self.viewer = ExrViewer::default(); // Reset viewer state
                }
                self.error_msg = None;
            }
            Err(e) => {
                if !is_b {
                    self.exr_data = None;
                }
                self.error_msg = Some(e.to_string());
            }
        }
    }

    /// Explicitly release a loaded image and its resources without restarting.
    /// Unloading A also drops B (a reference is meaningless on its own) and
    /// resets the viewer, dropping every `Arc<BindGroup>` GPU handle. Unloading
    /// B only clears B; the viewer's `textures_b`/`gpu_textures_b` are freed on
    /// the next `viewer.ui` pass when its layer count falls to zero.
    fn unload(&mut self, is_b: bool) {
        if is_b {
            self.exr_data_b = None;
            self.loaded_file_b = None;
            // B-only compare modes are meaningless without B.
            self.viewer.compare_mode = crate::viewer::CompareMode::SingleA;
            self.viewer.blink_state = false;
            // Drop B's histogram (not part of the cache key).
            self.viewer.invalidate_histogram();
        } else {
            self.exr_data = None;
            self.loaded_file = None;
            self.exr_data_b = None;
            self.loaded_file_b = None;
            self.viewer = ExrViewer::default();
        }
        self.error_msg = None;
    }
}

impl eframe::App for ExrApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply the persisted theme preference. Idempotent per frame; `System`
        // tracks the OS light/dark setting via egui's input each frame.
        ui.ctx().set_theme(self.theme);

        if self.show_help {
            egui::Window::new("Help & Shortcuts")
                .open(&mut self.show_help)
                .show(ui.ctx(), |ui| {
                    ui.heading("Keyboard Shortcuts");
                    ui.label("1 - View Image A");
                    ui.label("2 - View Image B (when reference loaded)");
                    ui.label("Space - Toggle Blink comparison (when reference loaded)");
                    ui.label("R / G / B / A - Isolate specific channel");
                    ui.label("C - Return to full color composite");
                    ui.label("F - Frame image to fit the window");
                    ui.label("F11 - Toggle full-screen (ESC or F11 to exit)");
                    ui.label("E - Reset exposure to 0.0");
                    ui.label("Shift+G - Reset gamma to 1.0");
                    ui.label("(or right-click the Exposure / Gamma labels to reset)");

                    ui.add_space(5.0);
                    ui.heading("Mouse Controls");
                    ui.label("Left Click + Drag - Pan image");
                    ui.label("Scroll Wheel - Zoom in and out");
                    ui.label("Shift + Left Click - Sample pixel color and save to swatches");

                    ui.add_space(10.0);
                    ui.heading("Features");
                    ui.label("• Dual Contact Sheets: Enable 'Contact Sheet' and use Compare Modes (A, B, A|B) to view side-by-side contact sheets.");
                    ui.label("• Metadata Explorer: When two images are loaded, EXR Info automatically displays metadata and layers for both Image A and Image B.");
                    ui.label("• Variable Sampling: Pick 1px / 3×3 / 9×9 to average the pixel readout over an aperture.");
                    ui.label("• Compositing: Load Image B, choose 'Comp', and pick a blend mode (Over / Under / Add / Multiply / Screen).");

                    ui.add_space(10.0);
                    ui.heading("About");
                    ui.label("Floki - A professional tool for inspecting OpenEXR files.");
                    ui.add_space(5.0);
                    ui.hyperlink("https://github.com/byvfx/floki");
                });
        }

        if self.show_tools_window {
            egui::Window::new("EXR Header Converter").open(&mut self.show_tools_window).show(ui.ctx(), |ui| {
                ui.heading("Batch Convert EXR Headers");
                ui.label("This tool processes all EXR files in a directory and renames their channels to standard RGBA format.");
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.label("Input Directory:");
                    if ui.button("Browse...").clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.tools_input_dir = path.to_string_lossy().to_string();
                            self.tools_output_dir = path.join("converted").to_string_lossy().to_string();
                        }
                });
                ui.add(egui::TextEdit::singleline(&mut self.tools_input_dir).desired_width(f32::INFINITY));

                ui.add_space(5.0);

                ui.horizontal(|ui| {
                    ui.label("Output Directory:");
                    if ui.button("Browse...").clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.tools_output_dir = path.to_string_lossy().to_string();
                        }
                });
                ui.add(egui::TextEdit::singleline(&mut self.tools_output_dir).desired_width(f32::INFINITY));

                ui.add_space(10.0);

                if self.conversion_receiver.is_none() {
                    if ui.button("Start Conversion").clicked() && !self.tools_input_dir.is_empty() && !self.tools_output_dir.is_empty() {
                        let (sender, receiver) = std::sync::mpsc::channel();
                        self.conversion_receiver = Some(receiver);
                        self.conversion_status = "Starting...".to_string();
                        self.conversion_progress = Some((0, 0));

                        self.conversion_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
                        let cancel_flag = self.conversion_cancel.clone();

                        let in_dir = std::path::PathBuf::from(self.tools_input_dir.trim().trim_matches(|c| c == '"' || c == '\''));
                        let out_dir = std::path::PathBuf::from(self.tools_output_dir.trim().trim_matches(|c| c == '"' || c == '\''));

                        std::thread::spawn(move || {
                            crate::tools::run_conversion_task(in_dir, out_dir, sender, cancel_flag);
                        });
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(false, |ui| {
                            let _ = ui.button("Start Conversion");
                        });
                        if ui.button("Cancel").clicked() {
                            self.conversion_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                            self.conversion_status = "Cancelling...".to_string();
                        }
                    });
                }

                if let Some(rx) = &self.conversion_receiver {
                    let mut finished = false;
                    loop {
                        match rx.try_recv() {
                            Ok((done, total, msg)) => {
                                self.conversion_status = msg;
                                self.conversion_progress = Some((done, total));
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                // Worker thread exited (completed or cancelled).
                                finished = true;
                                break;
                            }
                        }
                    }

                    if let Some((done, total)) = self.conversion_progress
                        && total > 0 {
                            let frac = (done as f32 / total as f32).clamp(0.0, 1.0);
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .text(format!("{}/{}", done, total)),
                            );
                        }
                    ui.label(&self.conversion_status);

                    if finished {
                        self.conversion_receiver = None;
                    } else {
                        // egui is reactive: without this the progress bar would
                        // freeze until the next input event. Poll ~20x/sec.
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(50));
                    }
                } else if let Some((done, total)) = self.conversion_progress
                    && total > 0 {
                        let frac = (done as f32 / total as f32).clamp(0.0, 1.0);
                        ui.add(egui::ProgressBar::new(frac).text(format!("{}/{}", done, total)));
                        ui.label(&self.conversion_status);
                    }
            });
        }

        if self.show_settings {
            // `.open(&mut self.show_settings)` holds a field borrow for the whole
            // closure, so we can't call the whole-`self` `reload_lut` inside it.
            // Record the request and act on it after the window block closes.
            let mut lut_reload_requested = false;
            #[cfg(feature = "ocio")]
            let mut ocio_load_requested = false;
            #[cfg(feature = "ocio")]
            let mut ocio_rebuild_requested = false;
            // Snapshot the enumerations so the combos can read them while the closure holds
            // a mutable borrow of `self` for the selections.
            #[cfg(feature = "ocio")]
            let ocio_displays = self.ocio_displays.clone();
            #[cfg(feature = "ocio")]
            let ocio_colorspaces = self.ocio_colorspaces.clone();
            egui::Window::new("Color Management")
                .open(&mut self.show_settings)
                .show(ui.ctx(), |ui| {
                    ui.heading("Settings");
                    ui.add_space(5.0);

                    #[cfg(not(feature = "ocio"))]
                    {
                        ui.label("OCIO Environment / Config Path:");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.ocio_path);
                            ui.add_enabled(false, egui::Button::new("Browse"))
                                .on_disabled_hover_text("Build with --features ocio");
                        });
                    }

                    #[cfg(feature = "ocio")]
                    {
                        ui.label(
                            "OCIO Config (.ocio) — empty uses built-in ACES (ocio://default):",
                        );
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut self.ocio_path);
                            if ui.button("Browse").clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("OCIO", &["ocio"])
                                    .pick_file()
                            {
                                self.ocio_path = path.to_string_lossy().to_string();
                                ocio_load_requested = true;
                            }
                            if ui.button("Load").clicked() {
                                ocio_load_requested = true;
                            }
                        });

                        // Clarify what an empty path resolves to.
                        if self.ocio_path.trim().is_empty() {
                            let hint =
                                match std::env::var("OCIO").ok().filter(|v| !v.trim().is_empty()) {
                                    Some(v) => format!("Empty path → using $OCIO: {v}"),
                                    None => "Empty path → using built-in ACES (ocio://default)"
                                        .to_string(),
                                };
                            ui.label(egui::RichText::new(hint).weak());
                        }

                        if !ocio_displays.is_empty() {
                            egui::ComboBox::from_label("Input color space")
                                .selected_text(self.ocio_input_cs.clone())
                                .show_ui(ui, |ui| {
                                    for cs in &ocio_colorspaces {
                                        if ui
                                            .selectable_value(
                                                &mut self.ocio_input_cs,
                                                cs.clone(),
                                                cs,
                                            )
                                            .clicked()
                                        {
                                            ocio_rebuild_requested = true;
                                        }
                                    }
                                });
                            egui::ComboBox::from_label("Display")
                                .selected_text(self.ocio_display.clone())
                                .show_ui(ui, |ui| {
                                    for d in &ocio_displays {
                                        if ui
                                            .selectable_value(
                                                &mut self.ocio_display,
                                                d.name.clone(),
                                                &d.name,
                                            )
                                            .clicked()
                                        {
                                            // Reset the view if it isn't valid for the new display.
                                            if let Some(nd) = ocio_displays
                                                .iter()
                                                .find(|x| x.name == self.ocio_display)
                                                && !nd.views.contains(&self.ocio_view)
                                            {
                                                self.ocio_view = nd.default_view.clone();
                                            }
                                            ocio_rebuild_requested = true;
                                        }
                                    }
                                });
                            let cur_views = ocio_displays
                                .iter()
                                .find(|d| d.name == self.ocio_display)
                                .map(|d| d.views.clone())
                                .unwrap_or_default();
                            egui::ComboBox::from_label("View")
                                .selected_text(self.ocio_view.clone())
                                .show_ui(ui, |ui| {
                                    for v in &cur_views {
                                        if ui
                                            .selectable_value(&mut self.ocio_view, v.clone(), v)
                                            .clicked()
                                        {
                                            ocio_rebuild_requested = true;
                                        }
                                    }
                                });
                            ui.checkbox(&mut self.ocio_enabled, "Enable OCIO");
                        }
                        if let Some(err) = &self.ocio_error {
                            ui.label(egui::RichText::new(err).color(egui::Color32::RED));
                        } else if self.ocio_ready {
                            ui.label(
                                egui::RichText::new("OCIO active").color(egui::Color32::GREEN),
                            );
                        }
                    }

                    ui.add_space(10.0);

                    ui.label("Custom LUT Path (.cube, .3dl):");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.lut_path);
                        if ui.button("Browse").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("LUT", &["cube"])
                                .pick_file()
                        {
                            self.lut_path = path.to_string_lossy().to_string();
                            lut_reload_requested = true;
                        }
                    });
                    ui.checkbox(&mut self.enable_lut, "Enable Custom LUT");
                    if let Some(err) = &self.lut_error {
                        ui.label(egui::RichText::new(err).color(egui::Color32::RED));
                    }
                    if self.lut_bg.is_some() {
                        ui.label(
                            egui::RichText::new("LUT loaded and active!")
                                .color(egui::Color32::GREEN),
                        );
                    }
                });

            if lut_reload_requested {
                self.reload_lut();
                if self.lut_bg.is_some() {
                    self.enable_lut = true; // Auto-enable on successful load
                }
            }

            #[cfg(feature = "ocio")]
            if ocio_load_requested {
                self.reload_ocio();
                if self.ocio_ready {
                    self.ocio_enabled = true; // Auto-enable on successful load
                }
            } else if ocio_rebuild_requested {
                self.rebuild_ocio_pass();
            }
        }

        // Full-screen mode (#2) hides the menu bar and side panel for a clean,
        // distraction-free viewport. ESC / F11 (handled in the viewer) restores.
        if !self.viewer.fullscreen {
            egui::Panel::top("top_panel").show_inside(ui, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui.button("Open EXR...").clicked() {
                            if let Some(path) = FileDialog::new()
                                .add_filter("EXR Image", &["exr"])
                                .pick_file()
                            {
                                self.open_file(path, false);
                            }
                            ui.close();
                        }
                        if ui.button("Open Reference (Image B)...").clicked() {
                            if let Some(path) = FileDialog::new()
                                .add_filter("EXR Image", &["exr"])
                                .pick_file()
                            {
                                self.open_file(path, true);
                            }
                            ui.close();
                        }
                        ui.menu_button("Open Recent A", |ui| {
                            if self.recent_files.is_empty() {
                                ui.label("No recent files");
                            } else {
                                let mut clicked_path = None;
                                for path in &self.recent_files {
                                    if ui
                                        .button(
                                            path.file_name().unwrap_or_default().to_string_lossy(),
                                        )
                                        .clicked()
                                    {
                                        clicked_path = Some(path.clone());
                                    }
                                }
                                if let Some(path) = clicked_path {
                                    self.open_file(path, false);
                                    ui.close();
                                }
                            }
                        });
                        ui.menu_button("Open Recent B", |ui| {
                            if self.recent_files.is_empty() {
                                ui.label("No recent files");
                            } else {
                                let mut clicked_path = None;
                                for path in &self.recent_files {
                                    if ui
                                        .button(
                                            path.file_name().unwrap_or_default().to_string_lossy(),
                                        )
                                        .clicked()
                                    {
                                        clicked_path = Some(path.clone());
                                    }
                                }
                                if let Some(path) = clicked_path {
                                    self.open_file(path, true);
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        ui.add_enabled_ui(self.exr_data.is_some(), |ui| {
                            if ui.button("Close Image A").clicked() {
                                self.unload(false);
                                ui.close();
                            }
                        });
                        ui.add_enabled_ui(self.exr_data_b.is_some(), |ui| {
                            if ui.button("Close Image B").clicked() {
                                self.unload(true);
                                ui.close();
                            }
                        });
                        ui.separator();
                        if ui.button("Quit").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });

                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut self.viewer.show_contact_sheet, "Contact Sheet");
                    });

                    ui.menu_button("Settings", |ui| {
                        if ui.button("Color Management...").clicked() {
                            self.show_settings = true;
                            ui.close();
                        }
                    });

                    ui.menu_button("Theme", |ui| {
                        ui.selectable_value(&mut self.theme, ThemeChoice::Dark, "Dark");
                        ui.selectable_value(&mut self.theme, ThemeChoice::Light, "Light");
                        ui.selectable_value(&mut self.theme, ThemeChoice::System, "System");
                    });

                    ui.menu_button("Tools", |ui| {
                        if ui.button("EXR Header Converter").clicked() {
                            self.show_tools_window = true;
                            ui.close();
                        }
                    });

                    ui.menu_button("Help", |ui| {
                        if ui.button("Keyboard Shortcuts").clicked() {
                            self.show_help = true;
                            ui.close();
                        }
                    });
                });
            });
        }

        // Status bar must be added BEFORE the side panel. egui allocates panel space
        // in call order; if the side panel (whose content can grow taller than the
        // window when Image B is loaded) is added first, it expands the parent UI's
        // bottom edge past the window and the bottom panel anchors off-screen.
        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            ui.vertical(|ui| {
                let draw_nuke_status_line =
                    |ui: &mut egui::Ui,
                     prefix: &str,
                     data: Option<&ExrData>,
                     hover_pos: Option<(usize, usize)>,
                     val: Option<[f32; 4]>,
                     layer_name: &str| {
                        if let Some(d) = data {
                            // Scroll each row horizontally on its own. Wrapping the whole
                            // vertical stack in one ScrollArea hides the stacked row
                            // height from the auto-sizing bottom panel, collapsing it.
                            egui::ScrollArea::horizontal()
                                .id_salt(prefix)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let disp_w = d.image.attributes.display_window.size.x();
                                        let disp_h = d.image.attributes.display_window.size.y();

                                        // Resolve the *physical* layer backing this logical
                                        // layer. `physical_index` maps into `image.layer_data`;
                                        // never index it with a logical-layer position.
                                        let phys_idx = d
                                            .logical_layers
                                            .iter()
                                            .find(|l| l.name == layer_name)
                                            .map(|l| l.physical_index)
                                            .unwrap_or(0);

                                        // Built fresh each frame (immediate mode),
                                        // but stop once we pass the 50-char display
                                        // cap so we don't allocate a Vec + join every
                                        // layer name (Blender EXRs have 100+) only to
                                        // truncate it away.
                                        let mut channels_str = String::new();
                                        for name in d.logical_layers.iter().map(|l| l.name.as_str())
                                        {
                                            if !channels_str.is_empty() {
                                                channels_str.push(',');
                                            }
                                            channels_str.push_str(name);
                                            if channels_str.len() > 50 {
                                                channels_str.truncate(50);
                                                channels_str.push_str("...");
                                                break;
                                            }
                                        }

                                        if let Some(layer) = d.image.layer_data.get(phys_idx) {
                                            let data_window_min = layer.attributes.layer_position;
                                            let data_w = layer.size.0;
                                            let data_h = layer.size.1;

                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{}: {}x{} bbox: {} {} {} {} channels: {}",
                                                    prefix,
                                                    disp_w,
                                                    disp_h,
                                                    data_window_min.x(),
                                                    data_window_min.y(),
                                                    data_w,
                                                    data_h,
                                                    channels_str
                                                ))
                                                .color(egui::Color32::DARK_GRAY),
                                            );
                                        }

                                        ui.add_space(10.0);

                                        if let (Some((x, y)), Some(v)) = (hover_pos, val) {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "x={} y={} {}",
                                                    x, y, layer_name
                                                ))
                                                .strong()
                                                .color(egui::Color32::WHITE),
                                            );
                                            ui.spacing_mut().item_spacing.x = 4.0;
                                            ui.label(
                                                egui::RichText::new(format!("{:.5}", v[0]))
                                                    .color(egui::Color32::from_rgb(255, 80, 80)),
                                            );
                                            ui.label(
                                                egui::RichText::new(format!("{:.5}", v[1]))
                                                    .color(egui::Color32::from_rgb(80, 255, 80)),
                                            );
                                            ui.label(
                                                egui::RichText::new(format!("{:.5}", v[2]))
                                                    .color(egui::Color32::from_rgb(100, 150, 255)),
                                            );
                                            ui.label(
                                                egui::RichText::new(format!("{:.5}", v[3]))
                                                    .color(egui::Color32::LIGHT_GRAY),
                                            );

                                            // Swatch
                                            let (r, g, b) = (
                                                (v[0].clamp(0.0, 1.0) * 255.0) as u8,
                                                (v[1].clamp(0.0, 1.0) * 255.0) as u8,
                                                (v[2].clamp(0.0, 1.0) * 255.0) as u8,
                                            );
                                            let (rect, _response) = ui.allocate_exact_size(
                                                egui::vec2(20.0, 14.0),
                                                egui::Sense::hover(),
                                            );
                                            ui.painter().rect_filled(
                                                rect,
                                                0.0,
                                                egui::Color32::from_rgb(r, g, b),
                                            );

                                            // HSVL
                                            ui.add_space(10.0);
                                            let max = v[0].max(v[1]).max(v[2]);
                                            let min = v[0].min(v[1]).min(v[2]);
                                            let delta = max - min;
                                            let mut h = 0.0;
                                            if delta > 0.0 {
                                                if max == v[0] {
                                                    h = 60.0 * (((v[1] - v[2]) / delta) % 6.0);
                                                } else if max == v[1] {
                                                    h = 60.0 * (((v[2] - v[0]) / delta) + 2.0);
                                                } else if max == v[2] {
                                                    h = 60.0 * (((v[0] - v[1]) / delta) + 4.0);
                                                }
                                            }
                                            if h < 0.0 {
                                                h += 360.0;
                                            }
                                            let s = if max > 0.0 { delta / max } else { 0.0 };
                                            let val_v = max;
                                            let l = 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];

                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "H:{:.0} S:{:.2} V:{:.2} L:{:.5}",
                                                    h, s, val_v, l
                                                ))
                                                .color(egui::Color32::LIGHT_GRAY),
                                            );
                                        } else {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "x=-- y=-- {}",
                                                    layer_name
                                                ))
                                                .color(egui::Color32::DARK_GRAY),
                                            );
                                        }
                                    });
                                });
                        }
                    };

                let layer_name_a = self
                    .exr_data
                    .as_ref()
                    .and_then(|d| d.logical_layers.get(self.viewer.active_layer))
                    .map(|l| l.name.as_str())
                    .unwrap_or("");

                draw_nuke_status_line(
                    ui,
                    "A",
                    self.exr_data.as_ref(),
                    self.viewer.last_hover_pos_img,
                    self.viewer.last_sampled_val_a,
                    layer_name_a,
                );

                if let Some(exr_b) = &self.exr_data_b {
                    let layer_name_b = exr_b
                        .logical_layers
                        .get(
                            self.viewer
                                .active_layer
                                .min(exr_b.logical_layers.len().saturating_sub(1)),
                        )
                        .map(|l| l.name.as_str())
                        .unwrap_or("");

                    draw_nuke_status_line(
                        ui,
                        "B",
                        Some(exr_b),
                        self.viewer.last_hover_pos_img,
                        self.viewer.last_sampled_val_b,
                        layer_name_b,
                    );
                }
            });
        });

        if !self.viewer.fullscreen {
            egui::Panel::left("side_panel")
                .resizable(true)
                .min_size(200.0)
                .show_inside(ui, |ui| {
                    // Whole sidebar scrolls as one column so Color Sampler / Histogram
                    // are never pushed below the window when Image B doubles the content.
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.heading("EXR Info");
                        ui.separator();
                        if let Some(err) = &self.error_msg {
                            ui.colored_label(egui::Color32::RED, format!("Error: {}", err));
                            ui.separator();
                        }

                        let mut files_to_show = vec![];
                        if let (Some(path), Some(data)) = (&self.loaded_file, &self.exr_data) {
                            files_to_show.push(("Image A", path, data));
                        }
                        if let (Some(path), Some(data)) = (&self.loaded_file_b, &self.exr_data_b) {
                            files_to_show.push(("Image B", path, data));
                        }

                        if !files_to_show.is_empty() {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for (idx, (label, path, exr_data)) in
                                    files_to_show.iter().enumerate()
                                {
                                    if idx > 0 {
                                        ui.separator();
                                        ui.add_space(10.0);
                                    }
                                    ui.heading(format!(
                                        "{}: {}",
                                        label,
                                        path.file_name().unwrap_or_default().to_string_lossy()
                                    ));
                                    ui.add_space(5.0);

                                    egui::CollapsingHeader::new("Image Metadata")
                                        .id_salt(format!("image_metadata_header_{}", idx))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            let attrs = &exr_data.image.attributes;
                                            ui.label(format!(
                                                "Display Window: {}x{} at {},{}",
                                                attrs.display_window.size.x(),
                                                attrs.display_window.size.y(),
                                                attrs.display_window.position.x(),
                                                attrs.display_window.position.y()
                                            ));
                                            ui.label(format!(
                                                "Pixel Aspect: {}",
                                                attrs.pixel_aspect
                                            ));

                                            if !attrs.other.is_empty() {
                                                ui.add_space(5.0);
                                                egui::CollapsingHeader::new("Custom Attributes")
                                                    .id_salt(format!(
                                                        "image_custom_attrs_header_{}",
                                                        idx
                                                    ))
                                                    .default_open(false)
                                                    .show(ui, |ui| {
                                                        for (name, val) in attrs.other.iter() {
                                                            ui.horizontal_wrapped(|ui| {
                                                                ui.strong(format!("{}: ", name));
                                                                ui.label(format!("{:?}", val));
                                                            });
                                                        }
                                                    });
                                            }
                                        });

                                    ui.separator();
                                    ui.heading("Layers");

                                    for (i, ll) in exr_data.logical_layers.iter().enumerate() {
                                        let is_selected = self.viewer.active_layer == i;

                                        if ui.selectable_label(is_selected, &ll.name).clicked() {
                                            self.viewer.active_layer = i;
                                        }

                                        if is_selected
                                            && let Some(layer) =
                                                exr_data.image.layer_data.get(ll.physical_index)
                                        {
                                            ui.indent("layer_details", |ui| {
                                                ui.label(format!(
                                                    "Resolution: {}x{}",
                                                    layer.size.0, layer.size.1
                                                ));
                                                let chan_name = |idx: Option<usize>| {
                                                    idx.and_then(|j| layer.channel_data.list.get(j))
                                                        .map(|c| c.name.to_string())
                                                        .unwrap_or_else(|| "-".to_string())
                                                };
                                                ui.label(format!(
                                                    "Channels: R={} G={} B={} A={}",
                                                    chan_name(ll.r),
                                                    chan_name(ll.g),
                                                    chan_name(ll.b),
                                                    chan_name(ll.a),
                                                ));

                                                if !layer.attributes.other.is_empty() {
                                                    ui.add_space(5.0);
                                                    egui::CollapsingHeader::new("Layer Attributes")
                                                        .id_salt(format!(
                                                            "layer_attrs_header_{}_{}",
                                                            idx, i
                                                        ))
                                                        .default_open(false)
                                                        .show(ui, |ui| {
                                                            for (name, val) in
                                                                layer.attributes.other.iter()
                                                            {
                                                                ui.horizontal_wrapped(|ui| {
                                                                    ui.strong(format!(
                                                                        "{}: ",
                                                                        name
                                                                    ));
                                                                    ui.label(format!("{:?}", val));
                                                                });
                                                            }
                                                        });
                                                }
                                            });
                                        }
                                    }
                                }
                            });
                        }

                        if let Some(_path) = &self.loaded_file {
                            if let Some(exr_data) = &self.exr_data {
                                ui.separator();
                                ui.heading("Color Sampler");

                                if !self.viewer.swatches.is_empty() {
                                    ui.horizontal(|ui| {
                                        ui.label(format!("{} saved", self.viewer.swatches.len()));
                                        if ui.button("Clear All").clicked() {
                                            self.viewer.swatches.clear();
                                        }
                                    });
                                    ui.add_space(5.0);

                                    egui::ScrollArea::vertical()
                                        .id_salt("swatches_scroll")
                                        .show(ui, |ui| {
                                            let mut to_remove = None;
                                            for (i, swatch) in
                                                self.viewer.swatches.iter().enumerate()
                                            {
                                                ui.horizontal(|ui| {
                                                    let [r, g, b, _a] = *swatch;

                                                    // Preview color patch using current sRGB mode and exposure/gamma
                                                    let mut disp_r =
                                                        r * self.viewer.exposure.exp2();
                                                    let mut disp_g =
                                                        g * self.viewer.exposure.exp2();
                                                    let mut disp_b =
                                                        b * self.viewer.exposure.exp2();

                                                    if self.viewer.gamma != 1.0 {
                                                        let inv_gamma = 1.0 / self.viewer.gamma;
                                                        disp_r = if disp_r > 0.0 {
                                                            disp_r.powf(inv_gamma)
                                                        } else {
                                                            0.0
                                                        };
                                                        disp_g = if disp_g > 0.0 {
                                                            disp_g.powf(inv_gamma)
                                                        } else {
                                                            0.0
                                                        };
                                                        disp_b = if disp_b > 0.0 {
                                                            disp_b.powf(inv_gamma)
                                                        } else {
                                                            0.0
                                                        };
                                                    }

                                                    if self.viewer.srgb {
                                                        disp_r =
                                                        crate::viewer::ExrViewer::linear_to_srgb(
                                                            disp_r,
                                                        );
                                                        disp_g =
                                                        crate::viewer::ExrViewer::linear_to_srgb(
                                                            disp_g,
                                                        );
                                                        disp_b =
                                                        crate::viewer::ExrViewer::linear_to_srgb(
                                                            disp_b,
                                                        );
                                                    }

                                                    let r_u8 =
                                                        (disp_r.clamp(0.0, 1.0) * 255.0) as u8;
                                                    let g_u8 =
                                                        (disp_g.clamp(0.0, 1.0) * 255.0) as u8;
                                                    let b_u8 =
                                                        (disp_b.clamp(0.0, 1.0) * 255.0) as u8;

                                                    let color =
                                                        egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                                                    let (rect, _resp) = ui.allocate_exact_size(
                                                        egui::vec2(20.0, 20.0),
                                                        egui::Sense::hover(),
                                                    );
                                                    ui.painter().rect_filled(rect, 2.0, color);

                                                    // Display values
                                                    ui.vertical(|ui| {
                                                        ui.label(format!(
                                                            "Float: {:.4}, {:.4}, {:.4}",
                                                            r, g, b
                                                        ));
                                                        ui.label(format!(
                                                            "8-bit: {}, {}, {}",
                                                            r_u8, g_u8, b_u8
                                                        ));
                                                        // HSV mapping
                                                        let max = r.max(g).max(b);
                                                        let min = r.min(g).min(b);
                                                        let c = max - min;
                                                        let h = if c == 0.0 {
                                                            0.0
                                                        } else if max == r {
                                                            60.0 * (((g - b) / c) % 6.0)
                                                        } else if max == g {
                                                            60.0 * (((b - r) / c) + 2.0)
                                                        } else {
                                                            60.0 * (((r - g) / c) + 4.0)
                                                        };
                                                        let h = if h < 0.0 { h + 360.0 } else { h };
                                                        let s =
                                                            if max == 0.0 { 0.0 } else { c / max };
                                                        let v = max;
                                                        ui.label(format!(
                                                            "HSV: {:.1}°, {:.2}, {:.2}",
                                                            h, s, v
                                                        ));
                                                    });

                                                    if ui.button("X").clicked() {
                                                        to_remove = Some(i);
                                                    }
                                                });
                                                ui.separator();
                                            }
                                            if let Some(i) = to_remove {
                                                self.viewer.swatches.remove(i);
                                            }
                                        });
                                } else {
                                    ui.label("Shift+Click on the image to save a swatch.");
                                }

                                ui.separator();
                                ui.heading("Histogram");
                                ui.horizontal(|ui| {
                                    // The histogram cache key includes log_histogram,
                                    // so flipping this auto-invalidates — no manual reset.
                                    ui.checkbox(
                                        &mut self.viewer.log_histogram,
                                        "Log Scale (-10 to +10 EV)",
                                    );
                                });

                                self.viewer
                                    .calculate_histogram(exr_data, self.exr_data_b.as_ref());

                                if let Some(bins) = &self.viewer.histogram {
                                    let (rect, _resp) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), 80.0),
                                        egui::Sense::hover(),
                                    );
                                    let mut max_val = *bins.iter().max().unwrap_or(&1) as f32;
                                    if let Some(bins_b) = &self.viewer.histogram_b {
                                        max_val =
                                            max_val.max(*bins_b.iter().max().unwrap_or(&1) as f32);
                                    }
                                    let max_val = max_val.max(1.0);

                                    let mut shapes = vec![];
                                    let bar_width = rect.width() / 256.0;

                                    for (i, &count) in bins.iter().enumerate() {
                                        let h = (count as f32 / max_val).powf(0.5) * rect.height();
                                        let x = rect.min.x + i as f32 * bar_width;
                                        let y = rect.max.y - h;

                                        shapes.push(egui::Shape::rect_filled(
                                            egui::Rect::from_min_max(
                                                egui::pos2(x, y),
                                                egui::pos2(x + bar_width.max(1.0), rect.max.y),
                                            ),
                                            0.0,
                                            egui::Color32::from_white_alpha(150), // White for A
                                        ));
                                    }

                                    if let Some(bins_b) = &self.viewer.histogram_b {
                                        for (i, &count) in bins_b.iter().enumerate() {
                                            let h =
                                                (count as f32 / max_val).powf(0.5) * rect.height();
                                            let x = rect.min.x + i as f32 * bar_width;
                                            let y = rect.max.y - h;

                                            shapes.push(egui::Shape::rect_filled(
                                                egui::Rect::from_min_max(
                                                    egui::pos2(x, y),
                                                    egui::pos2(x + bar_width.max(1.0), rect.max.y),
                                                ),
                                                0.0,
                                                egui::Color32::from_rgba_unmultiplied(
                                                    255, 50, 50, 150,
                                                ), // Red for B
                                            ));
                                        }
                                    }
                                    ui.painter().extend(shapes);
                                }
                            }
                        } else {
                            ui.label("No file loaded.");
                        }
                    });
                });
        }

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.loaded_file.is_some() {
                if let Some(data) = &self.exr_data {
                    self.viewer.enable_lut = self.enable_lut && self.lut_bg.is_some();
                    #[cfg(feature = "ocio")]
                    {
                        self.viewer.ocio_active = self.ocio_enabled && self.ocio_ready;
                        self.viewer.ocio_cpu = if self.viewer.ocio_active {
                            self.ocio_cpu.clone()
                        } else {
                            None
                        };
                    }
                    self.viewer.ui(
                        ui,
                        data,
                        self.exr_data_b.as_ref(),
                        self.render_state.as_ref(),
                        self.lut_bg.clone(),
                    );
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open an EXR file to begin.");
                });
            }
        });
    }
}
