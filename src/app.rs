use crate::exr_loader::ExrData;
use crate::viewer::ExrViewer;
use eframe::egui;
use rfd::FileDialog;
use std::path::{Path, PathBuf};

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
            ThemeChoice::Dark => Self::Dark,
            ThemeChoice::Light => Self::Light,
            ThemeChoice::System => Self::System,
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
    /// True for an image-sequence frame (#7): apply via `swap_image_data` to
    /// preserve the viewer session, rather than starting a fresh session.
    seq_frame: bool,
    /// Playback frame number (meaningful only when `seq_frame`); the cache key.
    frame: u32,
    /// Supersession epoch at issue time (#57).
    epoch: u64,
    result: Result<ExrData, String>,
}

/// serde default for `t2_enabled` (bool's own default is `false`).
fn ret_true() -> bool {
    true
}

/// Job sent to the dedicated EXR worker thread via `load_tx`.
struct LoadJob {
    path: PathBuf,
    is_b: bool,
    /// True when this is a playback frame: skip the first-paint proxy and apply
    /// as a session-preserving swap on arrival (#7).
    seq_frame: bool,
    /// Playback frame number (meaningful only when `seq_frame`); the cache key.
    frame: u32,
    /// Supersession epoch at issue time (#57); the result is dropped if it no
    /// longer matches `Playback::epoch` on arrival.
    epoch: u64,
}

/// Message from the decode worker to the UI thread, delivered over `load_rx`. A
/// slot-A load first sends a `Proxy` (a fast low-res first paint, #33) when one
/// is available, then always sends `Loaded` with the full decode.
enum LoadMsg {
    Proxy {
        path: PathBuf,
        proxy: crate::proxy::ProxyImage,
    },
    // Boxed: `LoadResult` holds a full `ExrData` inline, dwarfing the `Proxy`
    // variant (`large_enum_variant`).
    Loaded(Box<LoadResult>),
}

/// Result of an off-thread `.cube` LUT parse. The GPU bind group is created
/// on the UI thread in [`ExrApp::apply_lut_load_result`] (wgpu device access);
/// only the file I/O + parsing runs off-thread.
struct LutLoadResult {
    path: String,
    result: Result<crate::color::cube::CubeLut, String>,
}

/// Edge length of the baked OCIO 3D LUT (#24). 65³ keeps saturated-highlight error well under
/// 0.02 vs the analytic ACES transform (33³ measured ~0.04 there); ~4.4 MB as RGBA f32, a
/// trivial VRAM cost for a viewer.
#[cfg(feature = "ocio")]
const OCIO_BAKE_LUT_SIZE: u32 = 65;

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
    // `Arc` so a decoded frame can be the active image (tier T3) and stay
    // resident in the playback ring cache (tier T1) at once, without cloning the
    // (often 600 MB+) pixel buffers. See docs/playback/memory-contract.md.
    #[serde(skip)]
    exr_data: Option<std::sync::Arc<ExrData>>,
    #[serde(skip)]
    exr_data_b: Option<std::sync::Arc<ExrData>>,
    #[serde(skip)]
    error_msg: Option<String>,
    #[serde(skip)]
    viewer: ExrViewer,

    /// Image-sequence playback state (#7). Persists prefs (fps / loop / direction
    /// / pacing); the loaded sequence, playhead, and clock reset on each open.
    #[serde(default)]
    playback: crate::playback::Playback,

    /// T1 ring cache of decoded frames (#56): a scrub-back or loop replay is an
    /// instant cache hit. Cleared on each new sequence.
    #[serde(skip)]
    frame_cache: crate::cache::FrameCache,
    /// Resident-frame budget for `frame_cache`, recomputed from the RAM budget
    /// (`budget::max_t1`) each status tick once a frame's size is measured.
    #[serde(skip)]
    frame_cache_cap: usize,
    /// One frame's measured `approx_bytes()`, captured on the first decode (a
    /// sequence is homogeneous). Sizes the cache budget.
    #[serde(skip)]
    frame_bytes: Option<usize>,
    /// Sequence frame numbers submitted to the worker but not yet returned (#57).
    /// Bounds decode-ahead concurrency and prevents re-requesting an in-flight
    /// frame; cleared on every seek so superseded decodes can't be miscounted.
    #[serde(skip)]
    inflight: std::collections::HashSet<u32>,

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

    /// Pre-upload sequence frames to GPU textures ahead of the playhead (#56, the
    /// T2 ring) for smoother playback. On by default; a kill-switch back to the
    /// lazy per-swap path if it misbehaves on a given GPU. Persisted.
    #[serde(default = "ret_true")]
    t2_enabled: bool,
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

    /// App-owned GPU core (#54): the single home for the persistent `GpuState`
    /// and the OCIO pass publisher. `None` in the CPU-only path (no wgpu
    /// device) and during `Default::default()` / before `new(cc)` wires it up.
    /// Replaces the former direct `callback_resources` ownership; the app is
    /// now the source of truth, with egui holding `Arc` clones for its paint
    /// callbacks.
    #[serde(skip)]
    pub gpu_resources: Option<crate::gpu::GpuResources>,

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
    /// Bake the OCIO display transform to a 3D LUT (#24): trades a tiny amount of accuracy for
    /// a cheap per-pixel texture lookup instead of the analytic ACES ALU — smoother pan/zoom
    /// on weak GPUs. Off by default (analytic is the reference).
    #[cfg(feature = "ocio")]
    #[serde(default)]
    pub ocio_bake_lut: bool,
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
    load_rx: Option<std::sync::mpsc::Receiver<LoadMsg>>,

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
            playback: crate::playback::Playback::default(),
            frame_cache: crate::cache::FrameCache::new(),
            // Conservative starting budget until the first frame is measured and
            // `budget::max_t1` recomputes it from real RAM headroom.
            frame_cache_cap: 8,
            frame_bytes: None,
            inflight: std::collections::HashSet::new(),
            recent_files: Vec::new(),
            theme: ThemeChoice::default(),
            diff_colormap: crate::gradient::Colormap::default(),
            diff_metric: crate::gradient::DiffMetric::default(),
            diff_floor: 0.0,
            custom_gradients: Vec::new(),
            background: crate::background::Background::default(),
            background_presets: Vec::new(),
            save_snapshots: false,
            t2_enabled: true,
            snapshot_pending: false,
            snapshot_status: None,
            resource_monitor: crate::resource_monitor::ResourceMonitor::default(),
            show_help: false,
            show_settings: false,
            gpu_resources: None,
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
            ocio_bake_lut: false,
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
    #[must_use]
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app: Self = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Self::default()
        };

        app.gpu_resources = cc
            .wgpu_render_state
            .clone()
            .map(crate::gpu::GpuResources::new);

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
            bake_lut_size: if self.ocio_bake_lut {
                OCIO_BAKE_LUT_SIZE
            } else {
                0
            },
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
        let Some(gpu) = &self.gpu_resources else {
            self.ocio_ready = false;
            return;
        };
        let rs = gpu.render_state();
        match crate::gpu::ocio_pass::OcioGpuPass::from_bundle(
            &rs.device,
            &rs.queue,
            &bundle,
            rs.target_format,
        ) {
            Ok(pass) => {
                // Publish the new pass + invalidate the cached OcioTargets in
                // one named call (the old inline `insert` + `remove::<OcioTargets>()`
                // pair lived here with a scary comment about stale layouts — #54).
                gpu.publish_ocio_pass(pass);
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
                .map_err(|e| format!("Failed to load LUT: {e}"));
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
        // Crop to the active image area (#52), falling back to the full canvas.
        let Some(rect) = self.viewer.last_image_rect.or(self.viewer.last_canvas_rect) else {
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
                if let Some(gpu) = &self.gpu_resources {
                    let gpu_state = gpu.gpu_state.clone();
                    let rs = gpu.render_state();
                    // Explicitly destroy the old LUT texture before
                    // replacing it, so GPU memory is released in this
                    // submission cycle rather than waiting for the next
                    // driver GC sweep.
                    if let Some(old_tex) = self.lut_texture.take() {
                        old_tex.destroy();
                    }
                    let (bg, tex) = gpu_state.create_lut_bind_group(&rs.device, &rs.queue, &lut);
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
            // An explicit slot-A open (re)evaluates sequence mode: opening one
            // frame of a numbered sequence enables playback over its siblings; a
            // lone image leaves single-image behavior unchanged (#7).
            self.detect_sequence(&path);
        } else {
            self.loaded_file_b = Some(path.clone());
            self.loading_b = true;
        }
        self.error_msg = None;

        let tx = self.ensure_worker();
        let _ = tx.send(LoadJob {
            path,
            is_b,
            seq_frame: false,
            frame: 0,
            epoch: self.playback.epoch,
        });
    }

    /// Lazily create the load channel + spawn the single dedicated worker thread,
    /// returning a sender for [`LoadJob`]s. The worker processes jobs one at a
    /// time, so rapidly queued requests serialize instead of spawning many
    /// parallel GBs-of-RAM parses. Stale results are discarded by
    /// `apply_load_result`'s path check.
    fn ensure_worker(&mut self) -> std::sync::mpsc::Sender<LoadJob> {
        if self.load_rx.is_none() {
            let (job_tx, job_rx) = std::sync::mpsc::channel::<LoadJob>();
            let (result_tx, result_rx) = std::sync::mpsc::channel::<LoadMsg>();
            std::thread::spawn(move || {
                for job in job_rx {
                    // Slot-A first-paint proxy (#33): a fast low-res read so the
                    // image appears before the full decode lands. Skipped for
                    // slot B (a reference) and for playback frames (#7), which
                    // swap straight to full-res. `from_exr_fast_read` returns None
                    // for small / tiled / deep files anyway.
                    if !job.is_b
                        && !job.seq_frame
                        && let Some(proxy) = crate::proxy::ProxyImage::from_exr_fast_read(&job.path)
                    {
                        let _ = result_tx.send(LoadMsg::Proxy {
                            path: job.path.clone(),
                            proxy,
                        });
                    }
                    let result = ExrData::load(&job.path);
                    let _ = result_tx.send(LoadMsg::Loaded(Box::new(LoadResult {
                        path: job.path,
                        is_b: job.is_b,
                        seq_frame: job.seq_frame,
                        frame: job.frame,
                        epoch: job.epoch,
                        result,
                    })));
                }
            });
            self.load_tx = Some(job_tx);
            self.load_rx = Some(result_rx);
        }
        self.load_tx
            .clone()
            .expect("load channel initialized above")
    }

    // --- Image-sequence playback (#7) ----------------------------------------

    /// Evaluate sequence mode for a freshly opened slot-A `path`: enter playback
    /// over the detected siblings (placing the playhead on the opened frame), or
    /// clear playback for a lone image. Either way the frame cache is dropped —
    /// it is keyed by frame number, which a different sequence reuses.
    fn detect_sequence(&mut self, path: &std::path::Path) {
        self.frame_cache.clear();
        self.frame_bytes = None;
        // A different sequence reuses frame numbers, so drop the T2 GPU ring too
        // (and reset the on-screen frame; the first show re-sets it).
        self.viewer.clear_t2();
        self.viewer.set_t2_frame(None);
        // Drop any prior sequence's in-flight frames (a different sequence reuses
        // frame numbers); `enter`/`clear` bump the epoch so their results are
        // dropped. `loading_a` is left to `open_file`, which owns this open.
        self.inflight.clear();
        match crate::sequence::detect_from_file(path) {
            Some(seq) => {
                let start = seq.number_of(path).unwrap_or(seq.range.0);
                self.playback.enter(seq, start);
            }
            None => self.playback.clear(),
        }
    }

    /// Move the playhead to `frame` and display it. A resident frame (#56) shows
    /// instantly from the T1 ring; a miss is marked pending and decoded by
    /// [`Self::pump_decode`]. A hole (no file) holds the previous frame — nothing
    /// is requested, so playback never stalls.
    fn request_sequence_frame(&mut self, frame: u32) {
        self.error_msg = None;
        let Some(path) = self.playback.frame_path(frame).map(Path::to_path_buf) else {
            // Hole: keep showing the last real frame; prefetch may still run.
            self.playback.pending = None;
            self.pump_decode();
            return;
        };
        self.loaded_file = Some(path);

        if let Some(data) = self.frame_cache.get(crate::cache::Slot::A, frame) {
            // Cache hit: show immediately, no decode round-trip.
            self.loading_a = false;
            self.playback.pending = None;
            self.viewer.set_t2_frame(Some(frame)); // bind this frame's T2 texture
            self.swap_image_arc(data, false);
            self.playback.note_shown(std::time::Instant::now());
        } else {
            // Miss: mark the playhead as awaited; `pump_decode` submits it (the
            // want-list puts the playhead first, so it beats any prefetch).
            self.playback.pending = Some(frame);
        }
        self.pump_decode();
    }

    /// How many frames to decode ahead of the playhead — bounded by the T1 ring
    /// (`decode_ahead = min(configured, max_t1 − 1)`, tying #57 back-pressure to
    /// the #56 budget) and a hard cap so a huge sequence can't queue the world.
    fn prefetch_depth(&self) -> usize {
        const MAX_PREFETCH: usize = 16;
        self.frame_cache_cap.saturating_sub(1).min(MAX_PREFETCH)
    }

    /// Decode-ahead pump (#57): with at most one sequence decode outstanding,
    /// submit the highest-priority frame the scheduler wants — the awaited
    /// playhead first, then prefetch ahead in the play direction. Called after
    /// the playhead moves, after each result lands (the worker just freed up), and
    /// each playing tick. A no-op while a decode is in flight or a non-sequence
    /// load is busy, which is what keeps it to one outstanding job.
    fn pump_decode(&mut self) {
        if !self.playback.is_active() || !self.inflight.is_empty() || self.loading_a {
            return;
        }
        // Prefetch only while playing; paused/scrubbing decodes just the playhead.
        let depth = if self.playback.is_playing() {
            self.prefetch_depth()
        } else {
            0
        };
        let resident = self.frame_cache.resident(crate::cache::Slot::A);
        let wants = crate::scheduler::want_list(
            self.playback.current_frame,
            self.playback.in_point,
            self.playback.out_point,
            self.playback.direction,
            self.playback.loop_mode,
            &resident,
            depth,
        );
        // Submit the first want that has a real file (skip holes).
        for w in wants {
            let Some(path) = self.playback.frame_path(w).map(Path::to_path_buf) else {
                continue; // a hole — nothing to decode there.
            };
            self.inflight.insert(w);
            // The awaited playhead drives the "loading" state; prefetch is silent.
            if Some(w) == self.playback.pending {
                self.loading_a = true;
            }
            let tx = self.ensure_worker();
            let _ = tx.send(LoadJob {
                path,
                is_b: false,
                seq_frame: true,
                frame: w,
                epoch: self.playback.epoch,
            });
            break;
        }
    }

    /// Pre-upload T2 GPU textures (#56) for the on-screen frame and the next few
    /// T1-cached frames ahead of the playhead, within the VRAM budget. Builds at
    /// most a couple per call to amortize the upload across UI frames; only
    /// touches frames already resident in T1 (never decodes). UI-thread only.
    fn pump_t2(&mut self) {
        if !self.playback.is_active() || self.viewer.t2_cap() == 0 {
            return;
        }
        let Some(gpu) = self.gpu_resources.as_ref() else {
            return;
        };
        let depth = self.viewer.t2_cap().saturating_sub(1);
        // Empty resident set -> want_list returns the playhead + the window ahead;
        // we then keep only frames actually cached in T1.
        let wants = crate::scheduler::want_list(
            self.playback.current_frame,
            self.playback.in_point,
            self.playback.out_point,
            self.playback.direction,
            self.playback.loop_mode,
            &std::collections::HashSet::new(),
            depth,
        );
        let mut built = 0;
        for w in std::iter::once(self.playback.current_frame).chain(wants) {
            if built >= 2 {
                break; // amortize: at most two uploads per frame
            }
            if let Some(arc) = self.frame_cache.peek(crate::cache::Slot::A, w)
                && self.viewer.prebuild_t2(gpu, &arc, w)
            {
                built += 1;
            }
        }
    }

    /// Supersede every in-flight sequence decode: bump the epoch (so late results
    /// are dropped on arrival) and forget the in-flight set / awaited playhead.
    /// Called on every seek / scrub / direction change (#57).
    fn invalidate_inflight(&mut self) {
        self.playback.bump_epoch();
        self.inflight.clear();
        self.loading_a = false;
        self.playback.pending = None;
    }

    /// Step the playhead by `delta` frames, clamped to the in/out range (no
    /// wrap), and pause. Drives the back/forward transport buttons and arrow keys.
    fn playback_step(&mut self, delta: i32) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.state = crate::playback::PlayState::Paused;
        let (lo, hi) = (self.playback.in_point, self.playback.out_point);
        let next = (i64::from(self.playback.current_frame) + i64::from(delta))
            .clamp(i64::from(lo), i64::from(hi)) as u32;
        self.playback.current_frame = next;
        self.invalidate_inflight(); // a seek supersedes any in-flight decode
        self.request_sequence_frame(next);
    }

    /// Jump the playhead to an absolute frame number (clamped) and pause. Drives
    /// the scrubber and jump-to-in/out buttons (a P0 seek).
    fn playback_scrub_to(&mut self, frame: u32) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.state = crate::playback::PlayState::Paused;
        let next = frame.clamp(self.playback.in_point, self.playback.out_point);
        self.playback.current_frame = next;
        self.invalidate_inflight(); // a scrub supersedes any in-flight decode
        self.request_sequence_frame(next);
    }

    /// Toggle play/pause. Starting playback anchors the frame clock to now.
    fn playback_toggle(&mut self) {
        use crate::playback::PlayState;
        if !self.playback.is_active() {
            return;
        }
        if self.playback.state == PlayState::Playing {
            self.playback.state = PlayState::Paused;
        } else {
            self.playback.start_playing(std::time::Instant::now());
        }
    }

    /// Stop and rewind to the in point.
    fn playback_stop(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.state = crate::playback::PlayState::Stopped;
        let in_point = self.playback.in_point;
        self.playback.current_frame = in_point;
        self.invalidate_inflight();
        self.request_sequence_frame(in_point);
    }

    /// Set the in point to the playhead (the `I` key / Set In button). Prefetch
    /// may have run past the new boundary, so supersede in-flight decodes.
    fn playback_set_in(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.set_in();
        self.invalidate_inflight();
    }

    /// Set the out point to the playhead (the `O` key / Set Out button).
    fn playback_set_out(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.set_out();
        self.invalidate_inflight();
    }

    /// Reset the in/out trim to the full sequence range (the Reset button).
    fn playback_reset_trim(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.reset_trim();
        self.invalidate_inflight();
    }

    /// Move the playhead one frame in the play direction **without** issuing a
    /// decode. Returns `false` when `Once` has reached the boundary (the caller
    /// pauses). Drop-frames pacing steps through several of these per tick and
    /// requests only the frame it lands on, so skipped frames are never decoded.
    fn step_playhead(&mut self) -> bool {
        match crate::playback::advance(
            self.playback.current_frame,
            self.playback.in_point,
            self.playback.out_point,
            self.playback.direction,
            self.playback.loop_mode,
        ) {
            Some((next, dir)) => {
                self.playback.direction = dir;
                self.playback.current_frame = next;
                self.playback.frames_since_anchor += 1;
                true
            }
            None => false,
        }
    }

    /// Advance one frame in the play direction and request it. Returns `false`
    /// when `Once` has reached the boundary (the caller pauses). Pure of wall-time,
    /// so tests can drive playback frame-by-frame.
    fn advance_playhead(&mut self) -> bool {
        if self.step_playhead() {
            self.request_sequence_frame(self.playback.current_frame);
            true
        } else {
            false
        }
    }

    /// Per-frame playback clock. While playing and no decode is in flight, advance
    /// to the next frame once its absolute deadline (`anchor + n·period`) passes —
    /// drift-free pacing. Decode-bound playback (stutter) naturally drops the
    /// effective fps: the next request waits for the previous frame to land.
    fn tick_playback(&mut self, ctx: &egui::Context) {
        use crate::playback::{Pacing, PlayState};
        if !self.playback.is_active() || self.playback.state != PlayState::Playing {
            return;
        }
        let period = self.playback.period();
        match self.playback.pacing {
            Pacing::Stutter => self.tick_stutter(period),
            Pacing::DropFrames => self.tick_drop_frames(period),
        }
        // Keep the decode-ahead ring filling even between advances.
        self.pump_decode();
        // Keep the clock ticking even while idle between frames.
        ctx.request_repaint_after(period);
    }

    /// Stutter pacing: advance only when the playhead frame is ready (not awaiting
    /// a decode). With decode-ahead the next frame is usually already resident, so
    /// this advances smoothly; when decode falls behind it holds, dropping the
    /// effective fps without skipping frames. A review tool's default.
    fn tick_stutter(&mut self, period: std::time::Duration) {
        use crate::playback::PlayState;
        if self.playback.pending.is_some() || self.loading_a {
            return; // still waiting on the current frame — hold.
        }
        let now = std::time::Instant::now();
        let anchor = *self.playback.anchor.get_or_insert(now);
        let due = anchor + period * self.playback.frames_since_anchor;
        if now < due {
            return;
        }
        if self.advance_playhead() {
            // If decode fell behind by more than a frame, drop the accumulated lag
            // (anchor to now) so we don't burst-catch-up — stutter holds
            // wall-time-independent, never skipping.
            if now > due + period {
                self.playback.anchor = Some(now);
                self.playback.frames_since_anchor = 0;
            }
        } else {
            self.playback.state = PlayState::Paused;
        }
    }

    /// Drop-frames pacing: the clock advances on wall-time regardless of decode
    /// readiness, skipping straight to the latest due frame. Intermediate frames
    /// are stepped over with [`Self::step_playhead`] (no decode) and only the
    /// landing frame is requested — so when decode can't keep up you see skipped
    /// frames at a steady wall-clock rate instead of a slowing stutter. A hard
    /// per-tick cap re-anchors after a long stall so the catch-up can't spiral.
    fn tick_drop_frames(&mut self, period: std::time::Duration) {
        use crate::playback::PlayState;
        // Cap the catch-up burst (e.g. after the window was backgrounded): beyond
        // this many frames in one tick, re-anchor to now and move on.
        const MAX_SKIP: u32 = 240;
        let now = std::time::Instant::now();
        let anchor = *self.playback.anchor.get_or_insert(now);
        let mut steps = 0u32;
        let mut hit_boundary = false;
        while now >= anchor + period * self.playback.frames_since_anchor {
            if !self.step_playhead() {
                hit_boundary = true; // Once reached the boundary.
                break;
            }
            steps += 1;
            if steps >= MAX_SKIP {
                self.playback.anchor = Some(now);
                self.playback.frames_since_anchor = 0;
                break;
            }
        }
        if steps > 0 {
            // Show whatever the playhead landed on; if it isn't resident yet the
            // display holds until it decodes while the clock keeps moving.
            self.request_sequence_frame(self.playback.current_frame);
        }
        if hit_boundary {
            self.playback.state = PlayState::Paused;
        }
    }

    /// Context-gated playback keys: with a sequence loaded, Space is play/pause
    /// (consumed so the viewer's blink toggle doesn't also fire) and Left/Right
    /// step. Without a sequence, nothing is consumed and Space stays blink-compare.
    fn handle_playback_keys(&mut self, ctx: &egui::Context) {
        if !self.playback.is_active() {
            return;
        }
        // Don't steal keys from a focused text field (e.g. the fps `DragValue`):
        // Space/←/→ must reach the widget. Mirrors the viewer's hotkey gating.
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (space, left, right, set_in, set_out) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::Space),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                i.consume_key(egui::Modifiers::NONE, egui::Key::I),
                i.consume_key(egui::Modifiers::NONE, egui::Key::O),
            )
        });
        if space {
            self.playback_toggle();
        }
        if left {
            self.playback_step(-1);
        }
        if right {
            self.playback_step(1);
        }
        if set_in {
            self.playback_set_in();
        }
        if set_out {
            self.playback_set_out();
        }
    }

    /// Apply a completed [`LoadResult`] from the worker thread. Ignores stale
    /// results (a newer open of a different file superseded this one) by checking
    /// the result path against the currently-requested path for its slot.
    fn apply_load_result(&mut self, res: LoadResult) {
        // Playback frame (#7): supersession is by **epoch** (#57), not path —
        // sequences recur the same paths under loop/ping-pong/scrub-back, so a
        // stale frame could otherwise be mistaken for the current one. Apply as a
        // session-preserving swap (zoom/pan/exposure/channel/compare/annotations
        // carry across frames; reference B is untouched) and cache it (T1, #56).
        if res.seq_frame {
            if res.epoch != self.playback.epoch {
                return; // a seek/scrub/direction change superseded this decode.
            }
            self.inflight.remove(&res.frame);
            match res.result {
                Ok(data) => {
                    let arc = std::sync::Arc::new(data);
                    // Measure one frame to size the cache budget (homogeneous seq).
                    self.frame_bytes.get_or_insert_with(|| arc.approx_bytes());
                    self.frame_cache
                        .insert(crate::cache::Slot::A, res.frame, arc.clone());
                    self.frame_cache.evict_to(
                        self.frame_cache_cap,
                        self.playback.current_frame,
                        self.playback.direction,
                        self.playback.is_playing(),
                    );
                    // Show it only if it's the frame the playhead is waiting on;
                    // a prefetched frame ahead of the playhead is just cached.
                    if res.frame == self.playback.current_frame {
                        self.loading_a = false;
                        self.playback.pending = None;
                        self.viewer.set_t2_frame(Some(res.frame));
                        self.swap_image_arc(arc, false);
                        self.playback.note_shown(std::time::Instant::now());
                    }
                    self.error_msg = None;
                }
                Err(e) => {
                    if res.frame == self.playback.current_frame {
                        self.loading_a = false;
                        self.playback.pending = None;
                        self.error_msg = Some(e);
                    }
                }
            }
            // The worker just freed up — submit the next wanted frame.
            self.pump_decode();
            return;
        }

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
                    // B is a reference slot, not a new session: swap the pixel
                    // source while preserving the viewer's session state.
                    self.swap_image_data(data, true);
                } else {
                    // An explicit open of a new A starts a fresh session: drop the
                    // reference (meaningless on its own) in both paths below.
                    self.exr_data_b = None; // Reset B when A changes
                    self.loaded_file_b = None;
                    self.loading_b = false; // A discards any in-flight B load
                    if self.viewer.has_proxy() {
                        // A proxy painted first and already established the fresh
                        // view (and the user may have panned/zoomed it); swap to
                        // full-res preserving that view so the handoff is
                        // continuous. swap_image_data clears the proxy.
                        self.swap_image_data(data, false);
                    } else {
                        // No proxy: the full decode is this image's first paint —
                        // reset the viewer so it fits the new image.
                        self.exr_data = Some(std::sync::Arc::new(data));
                        self.reset_viewer_session();
                    }
                    // If this open started a sequence, seed the T1 ring with the
                    // opened frame so a scrub-back to it is an instant hit (#56).
                    if self.playback.is_active()
                        && let Some(arc) = &self.exr_data
                    {
                        self.frame_bytes.get_or_insert_with(|| arc.approx_bytes());
                        self.frame_cache.insert(
                            crate::cache::Slot::A,
                            self.playback.current_frame,
                            arc.clone(),
                        );
                    }
                }
                self.error_msg = None;
            }
            Err(e) => {
                if !res.is_b {
                    self.exr_data = None;
                }
                self.error_msg = Some(e);
            }
        }
    }

    /// Replace the pixel source for slot A or B **without** resetting viewer
    /// session state (zoom, pan, compare mode, channel mode, annotations,
    /// swatches, tone/OCIO/LUT controls). This is the per-frame path for
    /// image-sequence playback (#7): a new frame lands but the user's view is
    /// preserved.
    ///
    /// Invalidates only image-derived caches (textures, histogram, sampled
    /// values) and clamps `active_layer` to the new image's layer count. The
    /// *other* slot (e.g. a fixed reference B while A plays a sequence) is left
    /// untouched - swapping A does not drop B, unlike an explicit open.
    ///
    /// Contrast [`Self::reset_viewer_session`], used for an explicit open / new
    /// session, which drops B and resets the entire viewer.
    fn swap_image_data(&mut self, data: ExrData, is_b: bool) {
        self.swap_image_arc(std::sync::Arc::new(data), is_b);
    }

    /// As [`Self::swap_image_data`], but takes an already-`Arc`'d image so a
    /// playback cache hit (#56) can show a resident frame without cloning its
    /// pixel buffers — the same `Arc` is held by the T1 ring and the active slot.
    fn swap_image_arc(&mut self, data: std::sync::Arc<ExrData>, is_b: bool) {
        if is_b {
            self.exr_data_b = Some(data);
            // The texture caches only rebuild on a layer-count change, so a new B
            // with the same layer count would keep showing the previous image.
            // Force the reference textures (and the B-dependent diff/composite)
            // to regenerate from the new data.
            self.viewer.invalidate_reference_textures();
            // B isn't part of the histogram cache key - refresh it so the
            // B histogram appears without waiting for a layer change.
            self.viewer.invalidate_histogram();
            self.viewer.last_sampled_val_b = None;
        } else {
            let layer_count = data.logical_layers.len();
            self.exr_data = Some(data);
            // The full-res A decode has landed: drop the slot-A first-paint proxy
            // (#58). The viewer's zoom/pan session state is preserved so the
            // handoff from proxy to full-res is visually continuous.
            self.viewer.clear_proxy();
            // Clamp the active layer to the new image's last valid index. A
            // sequence normally has identical structure frame-to-frame, but guard
            // against a frame with fewer layers so the per-layer texture index
            // stays valid (sync_texture_caches resizes the cache but does not
            // clamp). A true clamp (not reset-to-0) keeps the user's selection
            // when the new image still has that index in range.
            self.viewer.active_layer = self.viewer.active_layer.min(layer_count.saturating_sub(1));
            self.viewer.invalidate_active_textures();
            self.viewer.invalidate_histogram();
            self.viewer.last_sampled_val_a = None;
            self.viewer.last_hover_pos_img = None;
        }
        self.error_msg = None;
    }

    /// Reset the entire viewer to defaults - the "new session" path for an
    /// explicit open / new sequence. Drops zoom, pan, compare mode, channel
    /// mode, annotations, swatches, and tone/OCIO/LUT view state. The caller is
    /// responsible for clearing the image slots (e.g. dropping B when A
    /// changes). Contrast [`Self::swap_image_data`], which replaces pixels while
    /// preserving session state for per-frame playback (#7).
    fn reset_viewer_session(&mut self) {
        self.viewer = ExrViewer::default();
    }

    /// Apply a worker-produced first-paint proxy (#33) to slot A. Dropped if the
    /// open was superseded (a newer open of a different file) or the full-res
    /// decode already landed — in both cases a late proxy would be a regression.
    fn apply_proxy(&mut self, ctx: &egui::Context, path: &Path, proxy: crate::proxy::ProxyImage) {
        if self.loaded_file.as_deref() != Some(path) || self.exr_data.is_some() {
            return;
        }
        self.set_proxy(ctx, proxy);
    }

    /// Set the slot-A first-paint proxy (#58/#33): upload a low-res
    /// [`ProxyImage`] so the viewport shows the image immediately while the
    /// full-res decode is still in flight. Called from [`Self::apply_proxy`] when
    /// the worker's fast low-res read (#33) arrives; the full decode later calls
    /// [`Self::swap_image_data`], which clears the proxy. No-op if the slot-A
    /// full image is already loaded.
    fn set_proxy(&mut self, ctx: &egui::Context, proxy: crate::proxy::ProxyImage) {
        if self.exr_data.is_some() {
            // Full-res already landed; a late proxy would be a step backwards.
            return;
        }
        if !self.viewer.has_proxy() {
            // First proxy for this open: establish the fresh-session view so the
            // proxy fits-to-view and doesn't inherit the previous image's
            // zoom/pan. Gated on has_proxy so a progressive proxy update (#33)
            // doesn't wipe the user's interaction. The full-res handoff
            // (apply_load_result) then preserves whatever view the user adjusts.
            self.reset_viewer_session();
        }
        // Bake the persisted background into the proxy texture (the viewer was
        // just reset above, so its background is the default until synced) to
        // avoid a background jump when the full-res render takes over.
        self.viewer.background = self.background.clone();
        self.viewer.set_proxy(ctx, proxy);
        ctx.request_repaint();
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
            self.reset_viewer_session();
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

        self.poll_async_loads(ui.ctx());

        // Sequence playback (#7): consume transport keys (Space/←/→) before the
        // viewer sees them, then run the frame clock. Both are no-ops unless a
        // sequence is loaded, so single-image behavior is unchanged.
        self.handle_playback_keys(ui.ctx());
        self.tick_playback(ui.ctx());

        // Snapshot to clipboard (#19): request a framebuffer screenshot on the
        // hotkey and consume the reply when it arrives.
        self.process_snapshot(ui.ctx());

        self.draw_help_window(ui.ctx());
        self.draw_tools_window(ui.ctx());
        self.draw_color_management_window(ui.ctx());
        self.draw_menu_bar(ui);
        self.draw_status_bar(ui);
        // Transport bar sits just above the status bar (added after it, so it
        // stacks above); a no-op panel unless a sequence is loaded.
        self.draw_transport_bar(ui);
        self.draw_side_panel(ui);
        self.draw_central_canvas(ui);
        // Pre-upload T2 GPU textures ahead of the playhead (#56). After the canvas
        // so the on-screen frame's texture exists; self-gates when T2 is off.
        self.pump_t2();
    }
}

impl ExrApp {
    fn poll_async_loads(&mut self, ctx: &egui::Context) {
        // Drain async image messages (collect first so the `load_rx` borrow ends
        // before the `&mut self` apply calls). A slot-A load delivers a `Proxy`
        // first (when available), then `Loaded` with the full decode.
        let mut msgs = Vec::new();
        if let Some(rx) = &self.load_rx {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            match msg {
                LoadMsg::Proxy { path, proxy } => self.apply_proxy(ctx, &path, proxy),
                LoadMsg::Loaded(res) => self.apply_load_result(*res),
            }
        }
        if self.loading_a || self.loading_b {
            // egui is reactive; keep polling the worker until the decode lands.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
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
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn draw_help_window(&mut self, ctx: &egui::Context) {
        if self.show_help {
            egui::Window::new("Help & Shortcuts")
                .open(&mut self.show_help)
                .show(ctx, |ui| {
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
    }

    fn draw_tools_window(&mut self, ctx: &egui::Context) {
        if self.show_tools_window {
            egui::Window::new("EXR Header Converter").open(&mut self.show_tools_window).show(ctx, |ui| {
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
                                    .text(format!("{done}/{total}")),
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
                        ui.add(egui::ProgressBar::new(frac).text(format!("{done}/{total}")));
                        ui.label(&self.conversion_status);
                    }
            });
        }
    }

    fn draw_color_management_window(&mut self, ctx: &egui::Context) {
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
                .show(ctx, |ui| {
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
                            if ui
                                .checkbox(
                                    &mut self.ocio_bake_lut,
                                    "Bake to 3D LUT (faster, slight accuracy trade-off)",
                                )
                                .on_hover_text(
                                    "Replace the per-pixel ACES math with a baked 3D-LUT \
                                     lookup. Much cheaper on weak GPUs; visually \
                                     indistinguishable for SDR. Off uses the exact analytic \
                                     transform.",
                                )
                                .changed()
                            {
                                ocio_rebuild_requested = true;
                            }
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
    }

    fn draw_menu_bar(&mut self, ui: &mut egui::Ui) {
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
    }

    fn draw_status_bar(&mut self, ui: &mut egui::Ui) {
        // Status bar must be added BEFORE the side panel. egui allocates panel space
        // in call order; if the side panel (whose content can grow taller than the
        // window when Image B is loaded) is added first, it expands the parent UI's
        // bottom edge past the window and the bottom panel anchors off-screen.
        egui::Panel::bottom("status_bar").show_inside(ui, |ui| {
            if let Some(status) = &self.snapshot_status {
                ui.label(egui::RichText::new(status).weak());
            }

            // Discrete RAM/VRAM readout, right-aligned (#51). `sample()` is throttled
            // internally, so this is cheap per frame; request a slow repaint so the
            // numbers keep ticking while the app is otherwise idle.
            if let Some(gpu) = &self.gpu_resources {
                let sample = self.resource_monitor.sample(&gpu.render_state().device);
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));

                // Resize the T1 ring to the live RAM budget (#56). Recomputed
                // each status tick; shrinks under other memory pressure.
                if let Some(bytes) = self.frame_bytes {
                    let cache_bytes = self.frame_cache.len() as u64 * bytes as u64;
                    self.frame_cache_cap =
                        crate::budget::t1_capacity(&sample, bytes, cache_bytes).max(2);
                }

                // Resize the T2 GPU-texture ring to the live VRAM budget (#56).
                // Conservative: capped low, and disabled (→ lazy path) unless at
                // least a couple of frames comfortably fit, since a wgpu OOM
                // aborts the process. Off entirely when the user disables it or
                // no sequence is loaded.
                const T2_HARD_CAP: usize = 8;
                let t2_cap = if self.t2_enabled && self.playback.is_active() {
                    self.exr_data
                        .as_ref()
                        .and_then(|d| d.logical_size(self.viewer.active_layer))
                        .map_or(0, |(w, h)| {
                            let fits = crate::budget::max_t2(&sample, w, h);
                            if fits < 2 { 0 } else { fits.min(T2_HARD_CAP) }
                        })
                } else {
                    0
                };
                self.viewer.set_t2_cap(t2_cap);
                use crate::resource_monitor::fmt_bytes;
                let mut text = format!(
                    "RAM {} · sys {}/{}",
                    fmt_bytes(sample.proc_bytes),
                    fmt_bytes(sample.sys_used),
                    fmt_bytes(sample.sys_total),
                );
                if let (Some(used), Some(budget)) = (sample.gpu_used, sample.gpu_budget) {
                    use std::fmt::Write as _;
                    let _ = write!(text, " · VRAM {}/{}", fmt_bytes(used), fmt_bytes(budget));
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
                                                    "x={x} y={y} {layer_name}"
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
                                                    "H:{h:.0} S:{s:.2} V:{val_v:.2} L:{l:.5}"
                                                ))
                                                .color(egui::Color32::LIGHT_GRAY),
                                            );
                                        } else {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "x=-- y=-- {layer_name}"
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
                    self.exr_data.as_deref(),
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
    }

    /// Transport controls for image-sequence playback (#7). A no-op unless a
    /// sequence is loaded. Scrubber + play/pause/stop/step/jump + reverse +
    /// loop-mode + editable target fps and measured fps.
    fn draw_transport_bar(&mut self, ui: &mut egui::Ui) {
        use crate::playback::{Direction, LoopMode, Pacing};
        if !self.playback.is_active() {
            return;
        }
        egui::Panel::bottom("transport_bar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let (lo, hi) = (self.playback.in_point, self.playback.out_point);
                let playing = self.playback.is_playing();

                if ui.button("|<").on_hover_text("Jump to in").clicked() {
                    self.playback_scrub_to(lo);
                }
                if ui.button("<").on_hover_text("Step back (←)").clicked() {
                    self.playback_step(-1);
                }
                if ui
                    .button(if playing { "Pause" } else { "Play" })
                    .on_hover_text("Play/Pause (Space)")
                    .clicked()
                {
                    self.playback_toggle();
                }
                if ui.button("Stop").on_hover_text("Stop and rewind").clicked() {
                    self.playback_stop();
                }
                if ui.button(">").on_hover_text("Step forward (→)").clicked() {
                    self.playback_step(1);
                }
                if ui.button(">|").on_hover_text("Jump to out").clicked() {
                    self.playback_scrub_to(hi);
                }

                ui.separator();

                let mut reverse = self.playback.direction == Direction::Reverse;
                if ui
                    .toggle_value(&mut reverse, "Rev")
                    .on_hover_text("Reverse play direction")
                    .changed()
                {
                    self.playback.direction = if reverse {
                        Direction::Reverse
                    } else {
                        Direction::Forward
                    };
                    // Direction change invalidates prefetch (it ran the other way).
                    self.invalidate_inflight();
                }

                let loop_label = match self.playback.loop_mode {
                    LoopMode::Once => "Once",
                    LoopMode::Loop => "Loop",
                    LoopMode::PingPong => "Ping-Pong",
                };
                if ui
                    .button(loop_label)
                    .on_hover_text("Cycle loop mode")
                    .clicked()
                {
                    self.playback.loop_mode = match self.playback.loop_mode {
                        LoopMode::Once => LoopMode::Loop,
                        LoopMode::Loop => LoopMode::PingPong,
                        LoopMode::PingPong => LoopMode::Once,
                    };
                }

                // Pacing toggle (#7): stutter plays every frame; drop-frames holds
                // wall-clock rate and skips. Wired through to `tick_playback`.
                let drop = self.playback.pacing == Pacing::DropFrames;
                let pacing_label = if drop { "Drop" } else { "Stutter" };
                if ui
                    .button(pacing_label)
                    .on_hover_text(
                        "Pacing when decode can't keep up. Stutter: play every \
                         frame, fps drops. Drop: hold wall-clock rate, skip frames.",
                    )
                    .clicked()
                {
                    self.playback.pacing = if drop {
                        Pacing::Stutter
                    } else {
                        Pacing::DropFrames
                    };
                }

                ui.separator();

                // In/out trim (#7). Set to the playhead; Reset restores the full
                // sequence span.
                if ui
                    .button("Set In")
                    .on_hover_text("Trim in point to the playhead (I)")
                    .clicked()
                {
                    self.playback_set_in();
                }
                if ui
                    .button("Set Out")
                    .on_hover_text("Trim out point to the playhead (O)")
                    .clicked()
                {
                    self.playback_set_out();
                }
                if ui
                    .button("Reset")
                    .on_hover_text("Reset trim to the full range")
                    .clicked()
                {
                    self.playback_reset_trim();
                }

                ui.separator();

                ui.add(
                    egui::DragValue::new(&mut self.playback.fps_target)
                        .range(1.0..=120.0)
                        .speed(0.25)
                        .suffix(" fps"),
                )
                .on_hover_text("Target fps");
                ui.label(
                    egui::RichText::new(format!("{:.1} actual", self.playback.measured_fps)).weak(),
                );

                ui.separator();

                // T2 GPU pre-upload kill-switch (#56). Off → the lazy per-swap
                // path (decode-ahead still smooths via the T1 ring).
                if ui
                    .checkbox(&mut self.t2_enabled, "GPU cache")
                    .on_hover_text(
                        "Pre-upload upcoming frames to GPU textures for smoother \
                         playback. Turn off if you see VRAM pressure.",
                    )
                    .changed()
                    && !self.t2_enabled
                {
                    self.viewer.clear_t2();
                }
            });

            // Timeline row: full-width span with the trimmed region + holes drawn
            // distinctly, plus the frame readout.
            ui.horizontal(|ui| {
                let cur = self.playback.current_frame;
                let (in_pt, out_pt) = (self.playback.in_point, self.playback.out_point);
                ui.label(format!("{cur}  [{in_pt}–{out_pt}]"));
                // A hole holds the previous frame; flag it so the readout isn't
                // mistaken for a decoded frame.
                if self.playback.frame_path(cur).is_none() {
                    ui.label(egui::RichText::new("(hole)").weak());
                }
                self.draw_timeline(ui);
            });
        });
    }

    /// Draw the playback timeline over the full sequence span: the trimmed
    /// `[in, out]` region is highlighted, holes are marked distinctly, the in/out
    /// edges and playhead are drawn as vertical ticks. Click or drag scrubs to the
    /// frame under the cursor (a P0 seek, clamped to the trim).
    fn draw_timeline(&mut self, ui: &mut egui::Ui) {
        let Some((lo, hi)) = self.playback.full_range() else {
            return;
        };
        let (in_pt, out_pt) = (self.playback.in_point, self.playback.out_point);
        let cur = self.playback.current_frame;
        let span = hi.saturating_sub(lo); // 0 for a single-frame sequence

        let width = ui.available_width().max(64.0);
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::click_and_drag());
        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            let visuals = ui.visuals();
            // Map a frame number to an x inside `rect` (single-frame → center).
            let x_of = |f: u32| {
                let t = if span == 0 {
                    0.5
                } else {
                    f32::from(u16::try_from(f.saturating_sub(lo)).unwrap_or(u16::MAX))
                        / span.max(1) as f32
                };
                rect.left() + t.clamp(0.0, 1.0) * rect.width()
            };

            // Track background.
            painter.rect_filled(rect, 3.0, visuals.extreme_bg_color);
            // Trimmed [in, out] region.
            let trim = egui::Rect::from_min_max(
                egui::pos2(x_of(in_pt), rect.top()),
                egui::pos2(x_of(out_pt), rect.bottom()),
            );
            painter.rect_filled(trim, 3.0, visuals.selection.bg_fill.gamma_multiply(0.4));
            // Holes: distinct vertical marks across the full span.
            if let Some(seq) = self.playback.sequence.as_ref() {
                let hole_color = egui::Color32::from_rgb(206, 92, 60);
                for &h in &seq.holes {
                    let x = x_of(h);
                    painter.line_segment(
                        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                        egui::Stroke::new(1.5, hole_color),
                    );
                }
            }
            // In/out edges.
            for f in [in_pt, out_pt] {
                let x = x_of(f);
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(2.0, visuals.widgets.active.fg_stroke.color),
                );
            }
            // Playhead (drawn last, on top).
            let px = x_of(cur);
            painter.line_segment(
                [
                    egui::pos2(px, rect.top() - 2.0),
                    egui::pos2(px, rect.bottom() + 2.0),
                ],
                egui::Stroke::new(2.0, visuals.strong_text_color()),
            );
            painter.rect_stroke(
                rect,
                3.0,
                egui::Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Inside,
            );
        }

        // Scrub on click or drag, clamped to the trim by `playback_scrub_to`.
        if (resp.clicked() || resp.dragged())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let t = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            let frame = lo + (t * span as f32).round() as u32;
            self.playback_scrub_to(frame);
        }
    }

    fn draw_side_panel(&mut self, ui: &mut egui::Ui) {
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
                            ui.colored_label(egui::Color32::RED, format!("Error: {err}"));
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
                                        .id_salt(format!("image_metadata_header_{idx}"))
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
                                                        "image_custom_attrs_header_{idx}"
                                                    ))
                                                    .default_open(false)
                                                    .show(ui, |ui| {
                                                        for (name, val) in attrs.other.iter() {
                                                            ui.horizontal_wrapped(|ui| {
                                                                ui.strong(format!("{name}: "));
                                                                ui.label(format!("{val:?}"));
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
                                                            "layer_attrs_header_{idx}_{i}"
                                                        ))
                                                        .default_open(false)
                                                        .show(ui, |ui| {
                                                            for (name, val) in
                                                                layer.attributes.other.iter()
                                                            {
                                                                ui.horizontal_wrapped(|ui| {
                                                                    ui.strong(format!("{name}: "));
                                                                    ui.label(format!("{val:?}"));
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
                                                            "Float: {r:.4}, {g:.4}, {b:.4}"
                                                        ));
                                                        ui.label(format!(
                                                            "8-bit: {r_u8}, {g_u8}, {b_u8}"
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
                                                            "HSV: {h:.1}°, {s:.2}, {v:.2}"
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
                                    .calculate_histogram(exr_data, self.exr_data_b.as_deref());

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
    }

    fn draw_central_canvas(&mut self, ui: &mut egui::Ui) {
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
                        self.exr_data_b.as_deref(),
                        self.gpu_resources.as_ref(),
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
                    // to keep showing). If a low-res first-paint proxy (#58) is
                    // available, render it; otherwise show a spinner.
                    if self.viewer.has_proxy() {
                        // Hydrate the same per-frame viewer state the full `ui`
                        // path uses, so the proxy renders with the user's tone /
                        // LUT / OCIO-toggled settings (OCIO itself isn't applied
                        // to the proxy — see `set_proxy`'s OCIO note).
                        self.viewer.enable_lut = false; // proxy is a pre-baked CPU texture
                        self.viewer.draw_proxy(ui);
                    } else {
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

    /// A multi-pass EXR with `n_passes` logical layers (`pass0`, `pass1`, ...),
    /// each RGBA, in a single physical layer. Exercises logical-layer grouping
    /// (Blender-style prefixed channels) so `active_layer` clamping is testable.
    fn write_multi_pass_exr(path: &std::path::Path, n_passes: usize) {
        const W: usize = 2;
        const H: usize = 2;
        let mut list = smallvec::SmallVec::new();
        for p in 0..n_passes {
            for name in ["R", "G", "B", "A"] {
                list.push(AnyChannel::new(
                    Text::from(format!("pass{p}.{name}").as_str()),
                    FlatSamples::F32(vec![0.5; W * H]),
                ));
            }
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
            .expect("write multi-pass exr fixture");
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
            seq_frame: false,
            frame: 0,
            epoch: 0,
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
            seq_frame: false,
            frame: 0,
            epoch: 0,
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
            exr_data_b: Some(std::sync::Arc::new(data_b)),
            loading_a: true,
            loading_b: true,
            ..Default::default()
        };

        app.apply_load_result(LoadResult {
            path,
            is_b: false,
            seq_frame: false,
            frame: 0,
            epoch: 0,
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
    fn swap_image_data_a_preserves_viewer_state_and_b() {
        // The per-frame playback path (#7): a new A frame lands but the user's
        // view (zoom, pan, exposure, channel mode, swatches, annotations) and
        // the reference B must be preserved. Contrast the open path above,
        // which resets the viewer and drops B.
        let dir = tempfile::tempdir().unwrap();
        let path_a0 = dir.path().join("a0.exr");
        let path_a1 = dir.path().join("a1.exr");
        let path_b = dir.path().join("b.exr");
        write_rgba_exr(&path_a0);
        write_rgba_exr(&path_a1);
        write_rgba_exr(&path_b);
        let a1 = ExrData::load(&path_a1).unwrap();
        let b = ExrData::load(&path_b).unwrap();

        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&path_a0).unwrap())),
            exr_data_b: Some(std::sync::Arc::new(b)),
            ..Default::default()
        };
        // Simulate a user mid-session: non-default view + annotation + swatch.
        app.viewer.scale = 3.5;
        app.viewer.translation = egui::Vec2::new(12.0, -7.0);
        app.viewer.exposure = 1.25;
        app.viewer.channel_mode = crate::viewer::ChannelMode::R;
        app.viewer.swatches.push([0.1, 0.2, 0.3, 1.0]);
        app.viewer.annotations.push(crate::annotation::Annotation {
            kind: crate::annotation::AnnotationKind::Rect {
                a: [1.0, 1.0],
                b: [5.0, 5.0],
            },
            color: egui::Color32::RED,
            width: 2.0,
        });

        app.swap_image_data(a1, false);

        assert!(app.exr_data.is_some(), "new A applied");
        assert!(
            app.exr_data_b.is_some(),
            "B preserved across A frame swap (unlike the open path)"
        );
        assert_eq!(app.viewer.scale, 3.5, "zoom preserved");
        assert_eq!(
            app.viewer.translation,
            egui::Vec2::new(12.0, -7.0),
            "pan preserved"
        );
        assert_eq!(app.viewer.exposure, 1.25, "exposure preserved");
        assert_eq!(
            app.viewer.channel_mode,
            crate::viewer::ChannelMode::R,
            "channel mode preserved"
        );
        assert_eq!(app.viewer.swatches.len(), 1, "swatches preserved");
        assert_eq!(app.viewer.annotations.len(), 1, "annotations preserved");
        assert!(app.error_msg.is_none());
    }

    #[test]
    fn swap_image_data_b_preserves_a_and_viewer_state() {
        // Swapping B is a reference refresh: A and the user's view are untouched.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.exr");
        let path_b0 = dir.path().join("b0.exr");
        let path_b1 = dir.path().join("b1.exr");
        write_rgba_exr(&path_a);
        write_rgba_exr(&path_b0);
        write_rgba_exr(&path_b1);
        let b1 = ExrData::load(&path_b1).unwrap();

        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&path_a).unwrap())),
            exr_data_b: Some(std::sync::Arc::new(ExrData::load(&path_b0).unwrap())),
            ..Default::default()
        };
        app.viewer.scale = 2.0;
        app.viewer.exposure = -0.5;

        app.swap_image_data(b1, true);

        assert!(app.exr_data.is_some(), "A untouched");
        assert!(app.exr_data_b.is_some(), "new B applied");
        assert_eq!(app.viewer.scale, 2.0, "zoom preserved");
        assert_eq!(app.viewer.exposure, -0.5, "exposure preserved");
    }

    #[test]
    fn swap_image_data_clamps_active_layer_to_new_layer_count() {
        // A sequence normally has identical layer structure frame-to-frame, but
        // guard against a frame with fewer passes so `active_layer` stays a valid
        // index into the per-layer texture cache (which would otherwise panic).
        let dir = tempfile::tempdir().unwrap();
        let path_3pass = dir.path().join("three.exr");
        let path_1pass = dir.path().join("one.exr");
        write_multi_pass_exr(&path_3pass, 3);
        write_multi_pass_exr(&path_1pass, 1);
        let one = ExrData::load(&path_1pass).unwrap();

        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&path_3pass).unwrap())),
            ..Default::default()
        };
        assert_eq!(app.exr_data.as_ref().unwrap().logical_layers.len(), 3);
        app.viewer.active_layer = 2; // valid for 3 passes, invalid for 1

        app.swap_image_data(one, false);

        assert_eq!(
            app.exr_data.as_ref().unwrap().logical_layers.len(),
            1,
            "new (smaller) A applied"
        );
        assert_eq!(
            app.viewer.active_layer, 0,
            "active_layer clamped to a valid index for the new layer count"
        );
    }

    #[test]
    fn reset_viewer_session_clears_view_state() {
        // The open/new-session path: the viewer is fully reset. The caller is
        // responsible for the image slots (here we only exercise the viewer reset).
        let mut app = ExrApp::default();
        app.viewer.scale = 4.0;
        app.viewer.translation = egui::Vec2::new(99.0, 99.0);
        app.viewer.exposure = 2.0;
        app.viewer.swatches.push([0.0; 4]);
        app.viewer.annotations.push(crate::annotation::Annotation {
            kind: crate::annotation::AnnotationKind::Rect {
                a: [0.0, 0.0],
                b: [1.0, 1.0],
            },
            color: egui::Color32::RED,
            width: 1.0,
        });

        app.reset_viewer_session();

        assert_eq!(app.viewer.scale, 1.0, "zoom reset");
        assert_eq!(app.viewer.translation, egui::Vec2::ZERO, "pan reset");
        assert_eq!(app.viewer.exposure, 0.0, "exposure reset");
        assert!(app.viewer.swatches.is_empty(), "swatches cleared");
        assert!(app.viewer.annotations.is_empty(), "annotations cleared");
    }

    #[test]
    fn gpu_resources_is_none_in_default_and_cpu_path() {
        // #54: the GPU core is app-owned on `ExrApp::gpu_resources`. Without a
        // wgpu render surface (Default / headless tests / CPU-only builds),
        // it stays `None` and the viewer takes the CPU path — the contract the
        // headless test suite relies on. (A device-backed `GpuResources` can't
        // be constructed without a wgpu device, so we assert the None branch.)
        let app = ExrApp::default();
        assert!(
            app.gpu_resources.is_none(),
            "gpu_resources is None without a render surface"
        );
    }

    #[test]
    fn swap_image_data_clears_proxy_when_full_decode_lands() {
        // The #58↔#55 contract: a proxy is shown during the async decode, then
        // `swap_image_data` (the full-res landing) clears it. The viewer's
        // zoom/pan session state is preserved across the handoff.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.exr");
        write_rgba_exr(&path);
        let data = ExrData::load(&path).unwrap();
        let proxy = crate::proxy::ProxyImage::from_exr_data_downsampled(&data, 0, 1).unwrap();

        let mut app = ExrApp {
            loaded_file: Some(path),
            loading_a: true, // full decode in flight
            ..Default::default()
        };
        // Simulate the decode worker delivering a proxy first. `set_proxy` needs
        // an egui ctx to load the texture; borrow one from a throwaway harness
        // (state callback gives `&egui::Context` directly). Recover `app` via
        // `std::mem::take` (ExrApp: Default).
        {
            use egui_kittest::Harness;
            let mut h = Harness::new_ui_state(
                |ui, app: &mut ExrApp| {
                    app.set_proxy(
                        ui.ctx(),
                        crate::proxy::ProxyImage {
                            pixels: proxy.pixels.clone(),
                            ..proxy.clone()
                        },
                    );
                },
                app,
            );
            // `set_proxy` calls `ctx.request_repaint`, so use `run_steps(1)`
            // instead of `run()` (which would loop on the repaint request).
            h.run_steps(1);
            app = std::mem::take(h.state_mut());
        }
        assert!(app.viewer.has_proxy(), "proxy set during load");
        app.viewer.scale = 2.5; // user panned/zoomed while the proxy showed

        // Full decode lands → swap clears the proxy, preserves view state.
        app.swap_image_data(data, false);

        assert!(
            !app.viewer.has_proxy(),
            "proxy cleared once full data lands"
        );
        assert_eq!(app.viewer.scale, 2.5, "zoom preserved across handoff");
        assert!(app.exr_data.is_some(), "full data applied");
    }

    #[test]
    fn a_success_with_proxy_preserves_view_and_clears_proxy() {
        // End-to-end seam (#58/#55): the real load-completion path
        // (`apply_load_result`) must take the swap branch when a proxy is showing
        // so the proxy→full-res handoff preserves the user's view, while still
        // dropping the now-meaningless reference B (an explicit new-A open).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.exr");
        write_rgba_exr(&path);
        let data = ExrData::load(&path).unwrap();
        let data_b = ExrData::load(&path).unwrap();
        let proxy = crate::proxy::ProxyImage::from_exr_data_downsampled(&data, 0, 1).unwrap();

        let mut app = ExrApp {
            loaded_file: Some(path.clone()),
            loaded_file_b: Some(PathBuf::from("b.exr")),
            exr_data_b: Some(std::sync::Arc::new(data_b)),
            loading_a: true, // full decode in flight
            loading_b: true,
            ..Default::default()
        };
        // Decode worker delivers a proxy first (needs an egui ctx to upload).
        {
            use egui_kittest::Harness;
            let mut h = Harness::new_ui_state(
                |ui, app: &mut ExrApp| {
                    app.set_proxy(
                        ui.ctx(),
                        crate::proxy::ProxyImage {
                            pixels: proxy.pixels.clone(),
                            ..proxy.clone()
                        },
                    );
                },
                app,
            );
            h.run_steps(1);
            app = std::mem::take(h.state_mut());
        }
        assert!(app.viewer.has_proxy(), "proxy set during load");
        app.viewer.scale = 2.5; // user panned/zoomed on the proxy

        // Full decode lands through the real completion path.
        app.apply_load_result(LoadResult {
            path,
            is_b: false,
            seq_frame: false,
            frame: 0,
            epoch: 0,
            result: Ok(data),
        });

        assert!(app.exr_data.is_some(), "A data applied");
        assert!(!app.viewer.has_proxy(), "proxy cleared on handoff");
        assert_eq!(app.viewer.scale, 2.5, "view preserved across handoff");
        assert!(app.exr_data_b.is_none(), "B dropped on explicit new-A open");
        assert!(app.loaded_file_b.is_none(), "B path cleared");
        assert!(!app.loading_a && !app.loading_b, "loading flags cleared");
    }

    #[test]
    fn set_proxy_is_noop_when_full_data_already_loaded() {
        // A late proxy arriving after the full decode must not clobber full-res.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.exr");
        write_rgba_exr(&path);
        let data = ExrData::load(&path).unwrap();
        let proxy = crate::proxy::ProxyImage::from_exr_data_downsampled(&data, 0, 1).unwrap();

        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(data)), // already loaded
            ..Default::default()
        };
        use egui_kittest::Harness;
        let mut h = Harness::new_ui_state(
            |ui, app: &mut ExrApp| {
                app.set_proxy(
                    ui.ctx(),
                    crate::proxy::ProxyImage {
                        pixels: proxy.pixels.clone(),
                        ..proxy.clone()
                    },
                );
            },
            app,
        );
        h.run_steps(1);
        app = std::mem::take(h.state_mut());
        assert!(
            !app.viewer.has_proxy(),
            "late proxy ignored when full data present"
        );
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

    // --- Sequence playback (#7, Phase 2) -------------------------------------

    use crate::playback::{Direction, LoopMode, PlayState};

    /// Create `count` empty frame files `s.0001.exr..` in `dir`. Empty is enough
    /// for detection + transport-state tests (no decode); use real EXRs only when
    /// a frame must actually load.
    fn touch_sequence(dir: &std::path::Path, count: u32) {
        for n in 1..=count {
            std::fs::write(dir.join(format!("s.{n:04}.exr")), b"").unwrap();
        }
    }

    #[test]
    fn detect_sequence_enters_playback_on_a_numbered_frame() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();

        app.detect_sequence(&dir.path().join("s.0002.exr"));
        assert!(app.playback.is_active(), "siblings -> sequence mode");
        assert_eq!(
            app.playback.current_frame, 2,
            "playhead on the opened frame"
        );
        assert_eq!((app.playback.in_point, app.playback.out_point), (1, 3));

        // A lone image leaves sequence mode (single-image behavior unchanged).
        let solo = tempfile::tempdir().unwrap();
        std::fs::write(solo.path().join("only.0001.exr"), b"").unwrap();
        app.detect_sequence(&solo.path().join("only.0001.exr"));
        assert!(!app.playback.is_active());
    }

    #[test]
    fn step_and_scrub_move_playhead_request_frame_and_pause() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));

        app.playback_step(1);
        assert_eq!(app.playback.current_frame, 2);
        assert_eq!(
            app.loaded_file.as_deref(),
            Some(dir.path().join("s.0002.exr").as_path()),
            "the stepped-to frame is the requested load"
        );
        assert!(app.loading_a, "a decode is in flight");
        assert_eq!(app.playback.pending, Some(2));
        assert_eq!(app.playback.state, PlayState::Paused, "stepping pauses");

        // Scrub past the end clamps to the out point.
        app.playback_scrub_to(99);
        assert_eq!(app.playback.current_frame, 5);

        // Step back clamps to the in point (no wrap).
        app.playback_scrub_to(1);
        app.playback_step(-1);
        assert_eq!(app.playback.current_frame, 1);
    }

    #[test]
    fn sequence_frame_arrival_swaps_and_preserves_the_view() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("f.0001.exr");
        let f2 = dir.path().join("f.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2);
        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&f1).unwrap())),
            ..Default::default()
        };
        app.detect_sequence(&f1);
        // User mid-session: non-default view.
        app.viewer.scale = 3.0;
        app.viewer.exposure = 1.5;

        // Step to frame 2 (sets loaded_file + pending), then deliver it. The
        // result must carry the live epoch — the step bumped it.
        app.playback_step(1);
        let data2 = ExrData::load(&f2).unwrap();
        app.apply_load_result(LoadResult {
            path: f2,
            is_b: false,
            seq_frame: true,
            frame: 2,
            epoch: app.playback.epoch,
            result: Ok(data2),
        });

        assert!(app.exr_data.is_some(), "frame 2 applied");
        assert_eq!(app.viewer.scale, 3.0, "zoom preserved across the frame");
        assert_eq!(app.viewer.exposure, 1.5, "exposure preserved");
        assert!(!app.loading_a, "decode flag cleared");
        assert_eq!(app.playback.pending, None, "pending cleared on arrival");
    }

    #[test]
    fn playing_advances_through_frames_and_loops() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.playback.loop_mode = LoopMode::Loop;

        app.playback_toggle();
        assert_eq!(app.playback.state, PlayState::Playing);

        // advance_playhead is wall-time-independent, so we can drive frames directly.
        assert!(app.advance_playhead());
        assert_eq!(app.playback.current_frame, 2);
        assert!(app.advance_playhead());
        assert_eq!(app.playback.current_frame, 3);
        assert!(app.advance_playhead());
        assert_eq!(app.playback.current_frame, 1, "looped back to the in point");
    }

    #[test]
    fn once_mode_advance_signals_stop_at_the_boundary() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 2);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0002.exr")); // start at the out point
        app.playback.loop_mode = LoopMode::Once;
        app.playback.direction = Direction::Forward;

        // At the out point, Once has nowhere to go: the clock would pause.
        assert!(!app.advance_playhead(), "Once at boundary -> stop");
    }

    #[test]
    fn space_is_play_pause_with_a_sequence_and_consumed_from_the_viewer() {
        use egui_kittest::Harness;
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        assert_eq!(app.playback.state, PlayState::Stopped);

        let mut h = Harness::new_ui_state(
            |ui, app: &mut ExrApp| app.handle_playback_keys(ui.ctx()),
            app,
        );
        h.key_press(egui::Key::Space);
        h.run();
        app = std::mem::take(h.state_mut());
        assert_eq!(
            app.playback.state,
            PlayState::Playing,
            "Space starts playback when a sequence is loaded"
        );
        assert!(
            !app.viewer.blink_state,
            "Space was consumed by playback, not the blink toggle"
        );
    }

    // --- T1 ring cache + epoch (#56/#57, Phase 3) ----------------------------

    /// Deliver a sequence frame to the app as the worker would, at the live epoch.
    fn deliver_frame(app: &mut ExrApp, path: &std::path::Path, frame: u32) {
        let data = ExrData::load(path).unwrap();
        app.apply_load_result(LoadResult {
            path: path.to_path_buf(),
            is_b: false,
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            result: Ok(data),
        });
    }

    #[test]
    fn scrub_back_hits_the_cache_without_a_decode() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("c.0001.exr");
        let f2 = dir.path().join("c.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2);
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);

        // Step to 2 and deliver it -> frame 2 is now resident.
        app.playback_step(1);
        deliver_frame(&mut app, &f2, 2);
        assert!(app.frame_cache.contains(crate::cache::Slot::A, 2));

        // Scrub to 1 (not cached) -> a real decode is in flight.
        app.playback_scrub_to(1);
        assert!(
            app.loading_a && app.playback.pending == Some(1),
            "miss decodes"
        );

        // Scrub back to 2 (cached) -> shown instantly, no decode issued.
        app.playback_scrub_to(2);
        assert!(!app.loading_a, "cache hit issues no decode");
        assert_eq!(app.playback.pending, None);
    }

    #[test]
    fn stale_epoch_sequence_result_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("c.0001.exr");
        let f2 = dir.path().join("c.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2);
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);

        // Request frame 2; capture the epoch its decode was issued under.
        app.playback_step(1);
        let stale_epoch = app.playback.epoch;

        // The user scrubs away before frame 2 lands — this bumps the epoch.
        app.playback_scrub_to(1);
        assert_ne!(app.playback.epoch, stale_epoch);

        // The late frame-2 result arrives carrying the old epoch: it must be
        // dropped (recurring paths break the path check; the epoch saves us).
        let data2 = ExrData::load(&f2).unwrap();
        app.apply_load_result(LoadResult {
            path: f2,
            is_b: false,
            seq_frame: true,
            frame: 2,
            epoch: stale_epoch,
            result: Ok(data2),
        });
        assert!(
            !app.frame_cache.contains(crate::cache::Slot::A, 2),
            "stale-epoch frame is not cached"
        );
        assert_eq!(
            app.playback.current_frame, 1,
            "playhead stays where the user left it"
        );
    }

    // --- Decode-ahead prefetch (#57, Phase 4) --------------------------------

    /// Write `count` real RGBA EXR frames `c.0001.exr..` and return the dir.
    fn write_sequence(count: u32) -> (tempfile::TempDir, Vec<std::path::PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = (1..=count)
            .map(|n| {
                let p = dir.path().join(format!("c.{n:04}.exr"));
                write_rgba_exr(&p);
                p
            })
            .collect();
        (dir, paths)
    }

    #[test]
    fn playing_prefetches_upcoming_frames_into_the_ring() {
        let (dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        app.frame_cache_cap = 4; // prefetch depth = 3
        app.playback_toggle(); // Playing, playhead on frame 1
        app.pump_decode(); // submits the playhead (frame 1, not yet cached)
        assert!(app.inflight.contains(&1) && app.inflight.len() == 1);

        // Frame 1 lands: shown + cached, and the worker is immediately re-tasked
        // with the next upcoming frame.
        deliver_frame(&mut app, &paths[0], 1);
        assert!(app.frame_cache.contains(crate::cache::Slot::A, 1));
        assert!(app.inflight.contains(&2), "prefetching frame 2 ahead");

        // Frame 2 is ahead of the playhead: cached but NOT shown; prefetch rolls on.
        deliver_frame(&mut app, &paths[1], 2);
        assert!(app.frame_cache.contains(crate::cache::Slot::A, 2));
        assert!(app.inflight.contains(&3));
        let _ = dir;
        assert_eq!(
            app.playback.current_frame, 1,
            "playhead unmoved — only the clock advances it, not prefetch"
        );
    }

    #[test]
    fn prefetch_is_bounded_by_the_ring_and_never_overfetches() {
        // A capacity-2 ring (depth 1) must not request a frame it would have to
        // immediately evict — otherwise it would re-decode it forever.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        app.frame_cache_cap = 2; // prefetch depth = 1
        app.playback_toggle();
        app.pump_decode();
        deliver_frame(&mut app, &paths[0], 1); // caches 1, prefetches 2
        assert!(app.inflight.contains(&2));
        deliver_frame(&mut app, &paths[1], 2); // caches 2; window (just frame 2) is full
        assert!(
            app.inflight.is_empty(),
            "ring full within the window -> nothing more requested"
        );
        assert!(app.frame_cache.contains(crate::cache::Slot::A, 1));
        assert!(app.frame_cache.contains(crate::cache::Slot::A, 2));
    }

    #[test]
    fn scrub_invalidates_in_flight_prefetch() {
        let (_dir, paths) = write_sequence(8);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        app.frame_cache_cap = 4;
        app.playback_toggle();
        app.pump_decode();
        deliver_frame(&mut app, &paths[0], 1); // now prefetching ahead (frame 2)
        assert!(app.inflight.contains(&2));

        // User scrubs to frame 6: the in-flight prefetch is forgotten and the new
        // playhead is requested instead.
        app.playback_scrub_to(6);
        assert!(!app.inflight.contains(&2), "old prefetch dropped on seek");
        assert!(app.inflight.contains(&6), "new playhead requested");
        assert_eq!(app.playback.current_frame, 6);
    }

    // --- Transport polish (#7, Phase 5) --------------------------------------

    #[test]
    fn set_in_out_trims_and_clamps_the_scrub_range() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 10);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0005.exr")); // playhead 5, range 1..=10

        app.playback_set_in(); // in -> playhead (5)
        assert_eq!((app.playback.in_point, app.playback.out_point), (5, 10));
        app.playback_scrub_to(8);
        app.playback_set_out(); // out -> playhead (8)
        assert_eq!((app.playback.in_point, app.playback.out_point), (5, 8));

        // Scrubbing now clamps to the trimmed range, not the full span.
        app.playback_scrub_to(1);
        assert_eq!(app.playback.current_frame, 5, "clamped to the in point");
        app.playback_scrub_to(99);
        assert_eq!(app.playback.current_frame, 8, "clamped to the out point");

        // Reset restores the full sequence span.
        app.playback_reset_trim();
        assert_eq!((app.playback.in_point, app.playback.out_point), (1, 10));
    }

    #[test]
    fn drop_frames_skips_to_the_due_frame_without_decoding_intermediates() {
        use crate::playback::Pacing;
        let (_dir, paths) = write_sequence(10);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]); // playhead 1, range 1..=10
        app.playback.loop_mode = LoopMode::Once;
        app.playback.pacing = Pacing::DropFrames;
        app.playback.state = PlayState::Playing;
        app.playback.fps_target = 24.0;
        let period = app.playback.period();
        // Backdate the anchor so ~4 frame deadlines are already due this tick.
        app.playback.anchor = Some(std::time::Instant::now() - period.mul_f32(3.5));
        app.playback.frames_since_anchor = 0;
        // Drop-frames ignores the readiness gate that would hold stutter.
        app.loading_a = true;
        app.playback.pending = Some(99);

        app.tick_drop_frames(period);

        assert_eq!(
            app.playback.current_frame, 5,
            "skipped straight to the wall-clock-due frame"
        );
        assert_eq!(
            app.loaded_file.as_deref(),
            Some(paths[4].as_path()),
            "only the landing frame is requested — skipped frames are never decoded"
        );
    }

    #[test]
    fn sequence_advance_holds_the_b_reference() {
        let (_dir, paths) = write_sequence(3);
        let b_dir = tempfile::tempdir().unwrap();
        let bpath = b_dir.path().join("ref.exr");
        write_rgba_exr(&bpath);

        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        // Load a fixed B reference (A-plays / B-holds).
        let bref = std::sync::Arc::new(ExrData::load(&bpath).unwrap());
        app.exr_data_b = Some(bref.clone());
        app.loaded_file_b = Some(bpath.clone());

        // Play A across frames; B must never be touched by the slot-A swaps.
        app.playback_toggle();
        app.advance_playhead();
        deliver_frame(&mut app, &paths[1], 2);
        app.advance_playhead();
        deliver_frame(&mut app, &paths[2], 3);

        assert_eq!(
            app.playback.current_frame, 3,
            "A advanced through the sequence"
        );
        assert!(
            app.exr_data_b
                .as_ref()
                .is_some_and(|b| std::sync::Arc::ptr_eq(b, &bref)),
            "B is held as the same Arc — playing A never swaps or clears it"
        );
        assert_eq!(
            app.loaded_file_b.as_deref(),
            Some(bpath.as_path()),
            "B's loaded path is unchanged"
        );
    }
}
