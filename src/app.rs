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

/// Result of an off-thread EXR decode, delivered back to the UI thread by
/// [`ExrApp::open_file`]'s worker and applied in [`ExrApp::apply_load_result`].
/// `path` identifies which request this is, so a stale result from a
/// superseded open can be discarded.
struct LoadResult {
    path: PathBuf,
    is_b: bool,
    result: Result<ExrData, String>,
}

/// Job sent to the dedicated EXR worker thread via `load_tx`.
struct LoadJob {
    path: PathBuf,
    is_b: bool,
}

/// Result of an off-thread `.cube` LUT parse. The GPU bind group is created
/// on the UI thread in [`ExrApp::apply_lut_load_result`] (wgpu device access);
/// only the file I/O + parsing runs off-thread.
struct LutLoadResult {
    path: String,
    result: Result<crate::color::cube::CubeLut, String>,
}

/// Top-level application state and the [`eframe::App`] implementation. Owns the
/// loaded A/B images, the `ExrViewer` canvas, OCIO/LUT colour state, and the
/// menu/tool UI. Fields marked `#[serde(skip)]` are runtime-only (images, GPU
/// handles); the rest persist across sessions.
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

    /// Persisted Diff/Matte visualization controls (issue #15). The live state
    /// lives on [`ExrViewer`] (where the UI mutates it); these mirror it for
    /// persistence and are round-tripped each frame around `viewer.ui`. Defaults
    /// reproduce the legacy behaviour (black-body / max-channel / no floor).
    #[serde(default)]
    diff_colormap: crate::gradient::Colormap,
    #[serde(default)]
    diff_metric: crate::gradient::DiffMetric,
    #[serde(default)]
    diff_floor: f32,
    /// User-saved named gradients (the preset library shared with the editor).
    #[serde(default)]
    custom_gradients: Vec<(String, crate::gradient::Gradient)>,

    /// Persisted viewport background (issue #18) + its named preset library.
    /// Live state lives on [`ExrViewer`]; round-tripped each frame like the diff
    /// controls. Default reproduces the legacy grey checker.
    #[serde(default)]
    background: crate::background::Background,
    #[serde(default)]
    background_presets: Vec<(String, crate::background::Background)>,

    /// Snapshot to clipboard (issue #19): when true, each snapshot also writes a
    /// timestamped PNG to `~/.floki/snapshots/`. The clipboard copy always happens.
    #[serde(default)]
    save_snapshots: bool,
    /// A framebuffer screenshot has been requested and we're awaiting its
    /// `Event::Screenshot` reply (transient).
    #[serde(skip)]
    snapshot_pending: bool,
    /// Last snapshot outcome, shown briefly in the status bar (transient).
    #[serde(skip)]
    snapshot_status: Option<String>,

    /// Throttled RAM/GPU-memory sampler for the bottom-bar readout (#51).
    #[serde(skip)]
    resource_monitor: crate::resource_monitor::ResourceMonitor,

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
    /// The LUT texture, kept alongside `lut_bg` so it can be `destroy()`ed
    /// explicitly when replaced (wgpu's lazy drop defers GPU memory release).
    #[serde(skip)]
    lut_texture: Option<eframe::egui_wgpu::wgpu::Texture>,
    pub lut_error: Option<String>,
    /// `.cube` LUT domain bounds (xyz + pad). Set in `reload_lut`, hydrated onto
    /// `ExrViewer` each frame so the GPU uniform remaps the lookup coordinate for
    /// non-unit-domain LUTs. `#[serde(skip)]` — re-derived from the LUT file.
    #[serde(skip)]
    lut_domain_min: [f32; 4],
    #[serde(skip)]
    lut_domain_max: [f32; 4],

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

    // Async image loading: a single dedicated worker thread processes load
    // requests one at a time (see `open_file`). Using one worker instead of
    // spawning a thread per file prevents multiple parallel EXR parses from
    // exhausting memory on large files — each parse can be GBs of working set.
    #[serde(skip)]
    loading_a: bool,
    #[serde(skip)]
    loading_b: bool,
    /// Job queue sender: send `LoadJob { path, is_b }` to the single worker thread.
    #[serde(skip)]
    load_tx: Option<std::sync::mpsc::Sender<LoadJob>>,
    /// Result receiver: the worker sends completed `LoadResult`s back here.
    #[serde(skip)]
    load_rx: Option<std::sync::mpsc::Receiver<LoadResult>>,

    // Async LUT loading: .cube parsing runs on a worker thread (see
    // `reload_lut`); the parsed CubeLut arrives over `lut_load_rx` and the
    // GPU bind group is created on the UI thread in `apply_lut_load_result`.
    #[serde(skip)]
    lut_loading: bool,
    /// Set by the Browse button so `apply_lut_load_result` knows to auto-enable
    /// the LUT on success. Startup reloads don't set this (the `enable_lut`
    /// field from persistence is respected, and cleared on failure).
    #[serde(skip)]
    lut_pending_auto_enable: bool,
    #[serde(skip)]
    lut_load_tx: Option<std::sync::mpsc::Sender<LutLoadResult>>,
    #[serde(skip)]
    lut_load_rx: Option<std::sync::mpsc::Receiver<LutLoadResult>>,
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
            diff_colormap: crate::gradient::Colormap::default(),
            diff_metric: crate::gradient::DiffMetric::default(),
            diff_floor: 0.0,
            custom_gradients: Vec::new(),
            background: crate::background::Background::default(),
            background_presets: Vec::new(),
            save_snapshots: false,
            snapshot_pending: false,
            snapshot_status: None,
            resource_monitor: crate::resource_monitor::ResourceMonitor::default(),
            show_help: false,
            show_settings: false,
            render_state: None,
            ocio_path: String::new(),
            lut_path: String::new(),
            enable_lut: false,
            lut_bg: None,
            lut_texture: None,
            lut_error: None,
            lut_domain_min: [0.0, 0.0, 0.0, 0.0],
            lut_domain_max: [1.0, 1.0, 1.0, 0.0],
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
            loading_a: false,
            loading_b: false,
            load_tx: None,
            load_rx: None,
            lut_loading: false,
            lut_pending_auto_enable: false,
            lut_load_tx: None,
            lut_load_rx: None,
        }
    }
}

impl ExrApp {
    /// Build the app: restore persisted state (or [`Default`]), then re-apply
    /// the saved theme and re-establish OCIO/LUT state for the loaded settings.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app: Self = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Self::default()
        };

        app.render_state = cc.wgpu_render_state.clone();

        if let Some(rs) = &app.render_state {
            let gpu_state = crate::gpu::GpuState::new(&rs.device, &rs.queue, rs.target_format);
            rs.renderer.write().callback_resources.insert(gpu_state);
        }

        // `lut_bg` is a GPU handle and can't persist, but `enable_lut`/`lut_path`
        // do. Without rebuilding the bind group here, a restart leaves the LUT
        // "enabled" in the UI but silently inert. Rebuild it, or clear the flag so
        // the persisted state matches reality.
        if app.enable_lut && !app.lut_path.is_empty() {
            app.reload_lut();
            // LUT loads asynchronously now; `apply_lut_load_result` will clear
            // `enable_lut` if the file was deleted since the last session.
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
                let mut renderer = rs.renderer.write();
                renderer.callback_resources.insert(pass);
                // Invalidate the cached OcioTargets so it is recreated on the
                // next frame against the new pipeline's bind group layout.
                // Without this, a stale scene_bind_group from the old layout
                // would be used with the new pipeline → wgpu validation error.
                renderer
                    .callback_resources
                    .remove::<crate::gpu::ocio_pass::OcioTargets>();
                drop(renderer);
                self.ocio_ready = true;
                self.ocio_error = None;
            }
            Err(e) => {
                self.ocio_error = Some(format!("Pipeline failed: {e}"));
                self.ocio_ready = false;
            }
        }
    }

    /// (Re)build the GPU LUT bind group from `self.lut_path`. The `.cube` file
    /// is parsed on a worker thread so the UI stays responsive on large LUTs
    /// (a 128³ LUT is ~2M rows); the GPU bind group is created on the UI thread
    /// in [`Self::apply_lut_load_result`] when the parse completes. A parse
    /// failure clears `lut_bg` and disables the LUT.
    fn reload_lut(&mut self) {
        if self.lut_path.is_empty() {
            return;
        }
        // Lazily create the LUT load channel.
        if self.lut_load_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.lut_load_tx = Some(tx);
            self.lut_load_rx = Some(rx);
        }
        let tx = self
            .lut_load_tx
            .clone()
            .expect("lut load channel initialized above");
        let path = self.lut_path.clone();
        self.lut_loading = true;
        self.lut_error = None;
        std::thread::spawn(move || {
            let result = crate::color::cube::CubeLut::load(&path)
                .map_err(|e| format!("Failed to load LUT: {}", e));
            let _ = tx.send(LutLoadResult { path, result });
        });
    }

    /// Apply a completed [`LutLoadResult`] from the worker thread: create the
    /// GPU bind group, capture the domain bounds, and update `lut_bg` /
    /// `lut_error` / `enable_lut`. Ignores stale results (a newer reload of a
    /// different path superseded this one).
    /// Snapshot to clipboard (#19): drive the hotkey trigger and consume the
    /// `Event::Screenshot` reply. Called once per frame from [`Self::ui`].
    fn process_snapshot(&mut self, ctx: &egui::Context) {
        // Cmd/Ctrl+Shift+S requests a snapshot (S avoids the viewer's plain R/G/B/A/C
        // channel keys). The menu button calls `request_snapshot` directly.
        let hotkey =
            ctx.input(|i| i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::S));
        if hotkey {
            self.request_snapshot(ctx);
        }

        // The screenshot is produced at the end of the requesting frame and the
        // reply lands as an event on a later frame; grab the most recent one.
        if !self.snapshot_pending {
            return;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().rev().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            self.snapshot_pending = false;
            self.finish_snapshot(&image, ctx.pixels_per_point());
        }
    }

    /// Ask egui to capture the next rendered frame. Idempotent while a capture is
    /// already in flight.
    fn request_snapshot(&mut self, ctx: &egui::Context) {
        if self.snapshot_pending {
            return;
        }
        if self.viewer.last_canvas_rect.is_none() {
            self.snapshot_status = Some("Snapshot: no image loaded".to_string());
            return;
        }
        self.snapshot_pending = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        ctx.request_repaint();
    }

    /// Crop the captured framebuffer to the image canvas, copy it to the clipboard,
    /// and (when enabled) save a timestamped PNG. Records a status string.
    fn finish_snapshot(&mut self, image: &egui::ColorImage, pixels_per_point: f32) {
        let Some(rect) = self.viewer.last_canvas_rect else {
            return;
        };
        let cropped = crate::snapshot::crop_to_rect(image, rect, pixels_per_point);

        let mut parts = Vec::new();
        match crate::snapshot::copy_to_clipboard(&cropped) {
            Ok(()) => parts.push("copied to clipboard".to_string()),
            Err(e) => parts.push(format!("clipboard failed: {e}")),
        }
        if self.save_snapshots {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            match crate::snapshot::save_png(&cropped, secs) {
                Ok(path) => parts.push(format!("saved {}", path.display())),
                Err(e) => parts.push(format!("save failed: {e}")),
            }
        }
        self.snapshot_status = Some(format!("Snapshot: {}", parts.join(", ")));
    }

    fn apply_lut_load_result(&mut self, res: LutLoadResult) {
        // Discard stale results from a superseded reload.
        if res.path != self.lut_path {
            // Clear transient flags so stale results don't bleed into the next reload.
            self.lut_loading = false;
            self.lut_pending_auto_enable = false;
            return;
        }
        self.lut_loading = false;
        let auto_enable = self.lut_pending_auto_enable;
        self.lut_pending_auto_enable = false;
        match res.result {
            Ok(lut) => {
                if let Some(rs) = &self.render_state {
                    let renderer = rs.renderer.read();
                    if let Some(gpu_state) =
                        renderer.callback_resources.get::<crate::gpu::GpuState>()
                    {
                        // Explicitly destroy the old LUT texture before
                        // replacing it, so GPU memory is released in this
                        // submission cycle rather than waiting for the next
                        // driver GC sweep.
                        if let Some(old_tex) = self.lut_texture.take() {
                            old_tex.destroy();
                        }
                        let (bg, tex) =
                            gpu_state.create_lut_bind_group(&rs.device, &rs.queue, &lut);
                        self.lut_bg = Some(bg);
                        self.lut_texture = Some(tex);
                        self.lut_error = None;
                        // Only update domain bounds once the bind group is live;
                        // moving them here keeps the shader state consistent.
                        self.lut_domain_min =
                            [lut.domain_min[0], lut.domain_min[1], lut.domain_min[2], 0.0];
                        self.lut_domain_max =
                            [lut.domain_max[0], lut.domain_max[1], lut.domain_max[2], 0.0];
                        if auto_enable {
                            self.enable_lut = true;
                        }
                    } else {
                        self.lut_error = Some("GPU state not found".to_string());
                        self.enable_lut = false;
                    }
                } else {
                    self.lut_error = Some("Render state not found".to_string());
                    self.enable_lut = false;
                }
            }
            Err(e) => {
                self.lut_error = Some(e);
                self.lut_bg = None;
                if let Some(old_tex) = self.lut_texture.take() {
                    old_tex.destroy();
                }
                self.enable_lut = false;
                self.lut_domain_min = [0.0, 0.0, 0.0, 0.0];
                self.lut_domain_max = [1.0, 1.0, 1.0, 0.0];
            }
        }
    }

    /// Begin loading an EXR into slot A or B. The decode runs on a worker thread
    /// so the UI stays responsive on large files; the result is delivered over
    /// `load_rx` and applied in [`Self::apply_load_result`]. Records the path
    /// up-front and raises the matching `loading_*` flag (which drives the
    /// spinner and keeps repaints flowing).
    fn open_file(&mut self, path: PathBuf, is_b: bool) {
        if !is_b {
            self.recent_files.retain(|p| p != &path);
            self.recent_files.insert(0, path.clone());
            self.recent_files.truncate(10);
            self.loaded_file = Some(path.clone());
            self.loading_a = true;
        } else {
            self.loaded_file_b = Some(path.clone());
            self.loading_b = true;
        }
        self.error_msg = None;

        // Lazily create the load channel + spawn a single dedicated worker
        // thread. The worker processes jobs one at a time, so rapidly opening
        // 5 files queues them instead of spawning 5 parallel GBs-of-RAM parses.
        // Stale jobs are discarded by `apply_load_result`'s path check.
        if self.load_rx.is_none() {
            let (job_tx, job_rx) = std::sync::mpsc::channel::<LoadJob>();
            let (result_tx, result_rx) = std::sync::mpsc::channel::<LoadResult>();
            std::thread::spawn(move || {
                for job in job_rx {
                    let result = ExrData::load(&job.path);
                    let _ = result_tx.send(LoadResult {
                        path: job.path,
                        is_b: job.is_b,
                        result,
                    });
                }
            });
            self.load_tx = Some(job_tx);
            self.load_rx = Some(result_rx);
        }
        let tx = self
            .load_tx
            .clone()
            .expect("load channel initialized above");
        let _ = tx.send(LoadJob { path, is_b });
    }

    /// Apply a completed [`LoadResult`] from the worker thread. Ignores stale
    /// results (a newer open of a different file superseded this one) by checking
    /// the result path against the currently-requested path for its slot.
    fn apply_load_result(&mut self, res: LoadResult) {
        let current = if res.is_b {
            self.loaded_file_b.as_ref()
        } else {
            self.loaded_file.as_ref()
        };
        if current != Some(&res.path) {
            // Superseded by a later open (or the slot was reset); drop it.
            return;
        }

        if res.is_b {
            self.loading_b = false;
        } else {
            self.loading_a = false;
        }

        match res.result {
            Ok(data) => {
                if res.is_b {
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
                    self.loading_b = false; // A discards any in-flight B load
                    self.viewer = ExrViewer::default(); // Reset viewer state
                }
                self.error_msg = None;
            }
            Err(e) => {
                if !res.is_b {
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

    /// Load EXR files dragged onto the window. While files are dragged over the
    /// window a left/right split overlay is drawn; on drop a single file routes
    /// by position (right half → reference Image B) and multiple files load
    /// first → A, second → B with the rest ignored. Non-EXR drops are ignored.
    fn handle_drag_and_drop(&mut self, ctx: &egui::Context) {
        // Hover preview while files are dragged in (before release). The cursor
        // position updates during the drag, so highlight the half it's currently
        // over — the half that will receive the drop — to make A vs B obvious.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let cx = screen.center().x;
            // The OS cursor moves during the drag even though winit delivers no
            // events, so query it directly (see `live_dropped_right`).
            let target_b = live_dropped_right(ctx).unwrap_or(false);

            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("dnd_overlay"),
            ));
            let left = egui::Rect::from_min_max(screen.min, egui::pos2(cx, screen.max.y));
            let right = egui::Rect::from_min_max(egui::pos2(cx, screen.min.y), screen.max);
            let active = if target_b { right } else { left };

            // Dim the whole window, then brighten the active half so it reads as
            // the live drop target.
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(150));
            painter.rect_filled(
                active,
                0.0,
                egui::Color32::from_rgba_unmultiplied(40, 90, 160, 70),
            );
            painter.rect_stroke(
                active,
                0.0,
                egui::Stroke::new(3.0, egui::Color32::from_rgb(90, 160, 240)),
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(cx, screen.top()),
                    egui::pos2(cx, screen.bottom()),
                ],
                (2.0, egui::Color32::from_white_alpha(180)),
            );

            let font = egui::FontId::proportional(28.0);
            let bright = egui::Color32::WHITE;
            let dim = egui::Color32::from_white_alpha(110);
            painter.text(
                egui::pos2(screen.left() + screen.width() * 0.25, screen.center().y),
                egui::Align2::CENTER_CENTER,
                "Drop for A",
                font.clone(),
                if target_b { dim } else { bright },
            );
            painter.text(
                egui::pos2(screen.left() + screen.width() * 0.75, screen.center().y),
                egui::Align2::CENTER_CENTER,
                "Drop for B (reference)",
                font,
                if target_b { bright } else { dim },
            );
            // Keep repainting so the highlight tracks the cursor smoothly.
            ctx.request_repaint();
        }

        // Handle files dropped this frame.
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let exr_paths: Vec<PathBuf> = dropped
            .into_iter()
            .filter_map(|f| f.path)
            .filter(|p| is_exr_path(p))
            .collect();
        let dropped_right = live_dropped_right(ctx).unwrap_or(false);
        for (path, is_b) in route_dropped_exrs(&exr_paths, dropped_right) {
            self.open_file(path, is_b);
        }
    }
}

/// Global OS cursor position in SCREEN-SPACE POINTS — the same space as
/// `ViewportInfo::inner_rect` — queried directly from the OS rather than via
/// winit events. During an external file drag winit delivers no cursor-move
/// events, so egui's pointer is stale, but the OS cursor itself keeps moving.
/// `None` on unsupported platforms.
///
/// Note the per-platform coordinate space: macOS `CGEvent` locations are already
/// in points (global display space), whereas Windows `GetCursorPos` returns
/// physical pixels, so only the Windows path divides by `pixels_per_point`.
fn global_cursor_pos_points(pixels_per_point: f32) -> Option<egui::Pos2> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT::default();
        // SAFETY: `GetCursorPos` writes a valid POINT; we pass a live pointer to it.
        unsafe { GetCursorPos(&mut p).ok()? };
        Some(egui::pos2(
            p.x as f32 / pixels_per_point,
            p.y as f32 / pixels_per_point,
        ))
    }
    #[cfg(target_os = "macos")]
    {
        use core_graphics::event::CGEvent;
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        // A null-ish event created from a session source carries the *current*
        // cursor location (the documented `CGEventCreate(NULL)` idiom). Already
        // in screen-space points, so `pixels_per_point` is not needed here.
        let _ = pixels_per_point;
        let src = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
        let loc = CGEvent::new(src).ok()?.location();
        Some(egui::pos2(loc.x as f32, loc.y as f32))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = pixels_per_point;
        None
    }
}

/// Whether `cursor_points` (screen-space points) is in the right half of
/// `window_rect` (also screen-space points) — i.e. the drop targets Image B.
/// Only X matters, so the cross-platform Y-origin difference is irrelevant.
/// Pure / testable.
fn cursor_targets_right(cursor_points: egui::Pos2, window_rect: egui::Rect) -> bool {
    cursor_points.x >= window_rect.center().x
}

/// Live drop-target side this frame from the OS cursor + window rect, or `None`
/// if either is unavailable (caller defaults to A / left).
fn live_dropped_right(ctx: &egui::Context) -> Option<bool> {
    let rect = ctx.input(|i| i.viewport().inner_rect)?;
    let cursor = global_cursor_pos_points(ctx.pixels_per_point())?;
    Some(cursor_targets_right(cursor, rect))
}

/// True if `path` has a (case-insensitive) `.exr` extension.
fn is_exr_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exr"))
}

/// Map dropped EXR paths to `(path, is_b)` load requests. A single file routes
/// by drop position (`dropped_right` → Image B); multiple files load first → A,
/// second → B, and any extras are ignored.
fn route_dropped_exrs(paths: &[PathBuf], dropped_right: bool) -> Vec<(PathBuf, bool)> {
    match paths {
        [] => Vec::new(),
        [single] => vec![(single.clone(), dropped_right)],
        [a, b, ..] => vec![(a.clone(), false), (b.clone(), true)],
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

        // Load EXR files dragged onto the window (and draw the drag-over overlay).
        self.handle_drag_and_drop(ui.ctx());

        // Drain completed async image loads (collect first so the `load_rx`
        // borrow ends before the `&mut self` apply call).
        let mut loaded = Vec::new();
        if let Some(rx) = &self.load_rx {
            while let Ok(res) = rx.try_recv() {
                loaded.push(res);
            }
        }
        for res in loaded {
            self.apply_load_result(res);
        }
        if self.loading_a || self.loading_b {
            // egui is reactive; keep polling the worker until the decode lands.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Drain completed async LUT loads.
        let mut lut_loaded = Vec::new();
        if let Some(rx) = &self.lut_load_rx {
            while let Ok(res) = rx.try_recv() {
                lut_loaded.push(res);
            }
        }
        for res in lut_loaded {
            self.apply_lut_load_result(res);
        }
        if self.lut_loading {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Snapshot to clipboard (#19): request a framebuffer screenshot on the
        // hotkey and consume the reply when it arrives.
        self.process_snapshot(ui.ctx());

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
                    ui.label(format!("Floki v{}", env!("CARGO_PKG_VERSION")));
                    ui.label("A professional tool for inspecting OpenEXR files.");
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
                self.lut_pending_auto_enable = true;
                self.reload_lut();
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
                        if ui.button("Viewport Background...").clicked() {
                            self.viewer.show_background_window = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .button("Snapshot to Clipboard")
                            .on_hover_text(
                                "Copy the current view to the clipboard (Cmd/Ctrl+Shift+S)",
                            )
                            .clicked()
                        {
                            self.request_snapshot(ui.ctx());
                            ui.close();
                        }
                        ui.checkbox(&mut self.save_snapshots, "Also save to ~/.floki/snapshots")
                            .on_hover_text("Write a timestamped PNG alongside the clipboard copy");
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
            if let Some(status) = &self.snapshot_status {
                ui.label(egui::RichText::new(status).weak());
            }

            // Discrete RAM/GPU readout, right-aligned (#51). `sample()` is throttled
            // internally, so this is cheap per frame; request a slow repaint so the
            // numbers keep ticking while the app is otherwise idle.
            if let Some(rs) = &self.render_state {
                let sample = self.resource_monitor.sample(&rs.device);
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
                use crate::resource_monitor::fmt_bytes;
                let mut text = format!(
                    "RAM {} · sys {}/{}",
                    fmt_bytes(sample.proc_bytes),
                    fmt_bytes(sample.sys_used),
                    fmt_bytes(sample.sys_total),
                );
                if let (Some(used), Some(budget)) = (sample.gpu_used, sample.gpu_budget) {
                    text.push_str(&format!(" · GPU {}/{}", fmt_bytes(used), fmt_bytes(budget)));
                }
                // Wrap the right-aligned label in a `horizontal` row first: a bare
                // right_to_left(Center) layout inside this auto-sized bottom panel would
                // grab the full available height to center within, feeding back and
                // growing the panel on every repaint. The horizontal row pins the band to
                // one line before we right-align inside it.
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(text).weak());
                    });
                });
            }

            ui.vertical(|ui| {
                let draw_nuke_status_line =
                    |ui: &mut egui::Ui,
                     prefix: &str,
                     data: Option<&ExrData>,
                     hover_pos: Option<(usize, usize)>,
                     val: Option<[f32; 4]>,
                     physical_index: usize,
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

                                        let channels_str = &d.channels_str;

                                        if let Some(layer) = d.image.layer_data.get(physical_index)
                                        {
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

                let ll_a = self
                    .exr_data
                    .as_ref()
                    .and_then(|d| d.logical_layers.get(self.viewer.active_layer));
                let phys_idx_a = ll_a.map(|l| l.physical_index).unwrap_or(0);
                let layer_name_a = ll_a.map(|l| l.name.as_str()).unwrap_or("");

                draw_nuke_status_line(
                    ui,
                    "A",
                    self.exr_data.as_ref(),
                    self.viewer.last_hover_pos_img,
                    self.viewer.last_sampled_val_a,
                    phys_idx_a,
                    layer_name_a,
                );

                if let Some(exr_b) = &self.exr_data_b {
                    let ll_b = exr_b.logical_layers.get(
                        self.viewer
                            .active_layer
                            .min(exr_b.logical_layers.len().saturating_sub(1)),
                    );
                    let phys_idx_b = ll_b.map(|l| l.physical_index).unwrap_or(0);
                    let layer_name_b = ll_b.map(|l| l.name.as_str()).unwrap_or("");

                    draw_nuke_status_line(
                        ui,
                        "B",
                        Some(exr_b),
                        self.viewer.last_hover_pos_img,
                        self.viewer.last_sampled_val_b,
                        phys_idx_b,
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
                                            let exp_mult =
                                                crate::render_math::exposure_to_multiplier(
                                                    self.viewer.exposure,
                                                );
                                            for (i, swatch) in
                                                self.viewer.swatches.iter().enumerate()
                                            {
                                                ui.horizontal(|ui| {
                                                    let [r, g, b, _a] = *swatch;

                                                    // Preview color patch using current sRGB mode and exposure/gamma
                                                    let mut disp_r = r * exp_mult;
                                                    let mut disp_g = g * exp_mult;
                                                    let mut disp_b = b * exp_mult;

                                                    if self.viewer.gamma != 1.0 {
                                                        disp_r = crate::render_math::apply_gamma(
                                                            disp_r,
                                                            self.viewer.gamma,
                                                        );
                                                        disp_g = crate::render_math::apply_gamma(
                                                            disp_g,
                                                            self.viewer.gamma,
                                                        );
                                                        disp_b = crate::render_math::apply_gamma(
                                                            disp_b,
                                                            self.viewer.gamma,
                                                        );
                                                    }

                                                    if self.viewer.srgb {
                                                        disp_r = crate::render_math::linear_to_srgb(
                                                            disp_r,
                                                        );
                                                        disp_g = crate::render_math::linear_to_srgb(
                                                            disp_g,
                                                        );
                                                        disp_b = crate::render_math::linear_to_srgb(
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

                                    // Up to 512 bars (256 bins × A/B); reserve to avoid reallocation.
                                    let mut shapes = Vec::with_capacity(512);
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
                    self.viewer.lut_domain_min = self.lut_domain_min;
                    self.viewer.lut_domain_max = self.lut_domain_max;
                    #[cfg(feature = "ocio")]
                    {
                        self.viewer.ocio_active = self.ocio_enabled && self.ocio_ready;
                        self.viewer.ocio_cpu = if self.viewer.ocio_active {
                            self.ocio_cpu.clone()
                        } else {
                            None
                        };
                    }
                    // Diff controls: push the persisted state into the viewer, let
                    // the mode-param UI mutate it during `ui`, then read it back so
                    // `save` persists the latest. Kept identical both ways, so no
                    // value is lost across frames.
                    self.viewer.diff_colormap = self.diff_colormap.clone();
                    self.viewer.diff_metric = self.diff_metric;
                    self.viewer.diff_floor = self.diff_floor;
                    self.viewer.custom_gradients = std::mem::take(&mut self.custom_gradients);
                    self.viewer.background = self.background.clone();
                    self.viewer.background_presets = std::mem::take(&mut self.background_presets);
                    self.viewer.ui(
                        ui,
                        data,
                        self.exr_data_b.as_ref(),
                        self.render_state.as_ref(),
                        self.lut_bg.clone(),
                    );
                    self.diff_colormap = self.viewer.diff_colormap.clone();
                    self.diff_metric = self.viewer.diff_metric;
                    self.diff_floor = self.viewer.diff_floor;
                    self.custom_gradients = std::mem::take(&mut self.viewer.custom_gradients);
                    self.background = self.viewer.background.clone();
                    self.background_presets = std::mem::take(&mut self.viewer.background_presets);
                } else if self.loading_a {
                    // A requested but its decode hasn't landed yet (no prior image
                    // to keep showing) — show progress instead of a blank canvas.
                    let name = self
                        .loaded_file
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.centered_and_justified(|ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Loading {name}…"));
                        });
                    });
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open an EXR file to begin.");
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exr::prelude::*;

    /// Tiny 2×2 RGBA EXR so the success path has a real `ExrData` to apply.
    fn write_rgba_exr(path: &std::path::Path) {
        const W: usize = 2;
        const H: usize = 2;
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F32(vec![0.5; W * H]),
            ));
        }
        let layer = Layer::new(
            (W, H),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer)
            .write()
            .to_file(path)
            .expect("write rgba exr fixture");
    }

    #[test]
    fn stale_load_result_is_ignored() {
        // A result for a path the user has since navigated away from must not
        // clobber state or clear the in-flight flag for the current request.
        let mut app = ExrApp {
            loaded_file: Some(PathBuf::from("current.exr")),
            loading_a: true,
            ..Default::default()
        };

        app.apply_load_result(LoadResult {
            path: PathBuf::from("superseded.exr"),
            is_b: false,
            result: Err("boom".to_string()),
        });

        assert!(
            app.error_msg.is_none(),
            "stale result must not surface its error"
        );
        assert!(
            app.loading_a,
            "stale result must leave the current load in flight"
        );
    }

    #[test]
    fn matching_error_result_surfaces_and_clears_loading() {
        let mut app = ExrApp {
            loaded_file: Some(PathBuf::from("current.exr")),
            loading_a: true,
            ..Default::default()
        };

        app.apply_load_result(LoadResult {
            path: PathBuf::from("current.exr"),
            is_b: false,
            result: Err("bad exr".to_string()),
        });

        assert_eq!(app.error_msg.as_deref(), Some("bad exr"));
        assert!(!app.loading_a, "matching result clears the loading flag");
        assert!(app.exr_data.is_none());
    }

    #[test]
    fn a_success_resets_b_and_clears_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.exr");
        write_rgba_exr(&path);
        let data_a = ExrData::load(&path).unwrap();
        let data_b = ExrData::load(&path).unwrap();

        let mut app = ExrApp {
            loaded_file: Some(path.clone()),
            loaded_file_b: Some(PathBuf::from("b.exr")),
            exr_data_b: Some(data_b),
            loading_a: true,
            loading_b: true,
            ..Default::default()
        };

        app.apply_load_result(LoadResult {
            path,
            is_b: false,
            result: Ok(data_a),
        });

        assert!(app.exr_data.is_some(), "A data applied");
        assert!(app.exr_data_b.is_none(), "B reset when A changes");
        assert!(app.loaded_file_b.is_none(), "B path cleared when A changes");
        assert!(
            !app.loading_a && !app.loading_b,
            "both loading flags cleared (A discards any in-flight B)"
        );
        assert!(app.error_msg.is_none());
    }

    #[test]
    fn is_exr_path_is_case_insensitive_and_extension_only() {
        assert!(is_exr_path(std::path::Path::new("/a/b/shot.exr")));
        assert!(is_exr_path(std::path::Path::new("SHOT.EXR")));
        assert!(is_exr_path(std::path::Path::new("render.Exr")));
        assert!(!is_exr_path(std::path::Path::new("note.txt")));
        assert!(!is_exr_path(std::path::Path::new("exr"))); // bare name, no extension
        assert!(!is_exr_path(std::path::Path::new("archive.exr.zip")));
    }

    #[test]
    fn route_single_drop_uses_position() {
        let p = vec![PathBuf::from("a.exr")];
        assert_eq!(
            route_dropped_exrs(&p, false),
            vec![(PathBuf::from("a.exr"), false)],
            "left half loads as A"
        );
        assert_eq!(
            route_dropped_exrs(&p, true),
            vec![(PathBuf::from("a.exr"), true)],
            "right half loads as B"
        );
    }

    #[test]
    fn route_multi_drop_is_a_then_b_rest_ignored() {
        let paths = vec![
            PathBuf::from("a.exr"),
            PathBuf::from("b.exr"),
            PathBuf::from("c.exr"),
        ];
        // Position is ignored once there are 2+ files: first → A, second → B.
        assert_eq!(
            route_dropped_exrs(&paths, true),
            vec![
                (PathBuf::from("a.exr"), false),
                (PathBuf::from("b.exr"), true),
            ],
        );
    }

    #[test]
    fn route_empty_drop_is_noop() {
        assert!(route_dropped_exrs(&[], false).is_empty());
    }

    #[test]
    fn cursor_targets_right_splits_on_window_center() {
        // Window spanning screen-points x: 0..1000 (center 500).
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1000.0, 800.0));
        assert!(!cursor_targets_right(egui::pos2(499.0, 10.0), rect));
        assert!(cursor_targets_right(egui::pos2(501.0, 10.0), rect));
        // Exactly at center counts as right (`>=`).
        assert!(cursor_targets_right(egui::pos2(500.0, 10.0), rect));
    }

    #[test]
    fn cursor_targets_right_uses_window_screen_center_not_origin() {
        // Off-origin window (e.g. dragged to the right of the primary monitor):
        // screen-points x 400..1400, center 900. Proves we compare against the
        // window's *screen-space* center, so multi-monitor / moved windows work.
        let rect = egui::Rect::from_min_max(egui::pos2(400.0, 0.0), egui::pos2(1400.0, 800.0));
        assert!(!cursor_targets_right(egui::pos2(850.0, 0.0), rect));
        assert!(cursor_targets_right(egui::pos2(950.0, 0.0), rect));
    }

    #[test]
    fn cursor_targets_right_handles_negative_origin_monitor() {
        // Secondary monitor to the left of primary: screen-points x -1920..-920,
        // center -1420. Cursor at -1500 is left of center -> A.
        let rect = egui::Rect::from_min_max(egui::pos2(-1920.0, 0.0), egui::pos2(-920.0, 800.0));
        assert!(!cursor_targets_right(egui::pos2(-1500.0, 0.0), rect));
        assert!(cursor_targets_right(egui::pos2(-1000.0, 0.0), rect));
    }
}
