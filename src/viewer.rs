//! The image canvas: pan/zoom, channel/exposure/gamma controls, the six
//! [`CompareMode`]s, pixel sampling, histogram and contact sheet. State lives in
//! [`ExrViewer`]; [`ExrViewer::ui`] is the per-frame entry point.
//!
//! # Texture generation
//!
//! Two rendering paths produce what's drawn, and which `generate_*` function
//! runs depends on whether a GPU [`RenderState`](eframe::egui_wgpu::RenderState)
//! is available:
//!
//! - **GPU (default).** [`Self::build_layer_texture`](ExrViewer::build_layer_texture)
//!   uploads a layer's RGBA into a bind group; `gpu/shader.wgsl` then applies
//!   channel isolation, exposure, gamma, sRGB **and every compare mode**
//!   (wipe / diff / composite) in-shader. So in GPU mode a single generator
//!   serves all modes, cached per layer in `gpu_textures` / `gpu_textures_b`.
//!
//! - **CPU (no `render_state`, plus contact-sheet thumbnails).** The result is
//!   baked into an [`egui::TextureHandle`], so each compare mode needs its own
//!   generator. When an OCIO CPU processor is active each path dispatches to its
//!   `_ocio` sibling (display transform instead of the built-in sRGB tone math):
//!
//!   | Situation            | Function                       | OCIO-active variant               | Cache (key)                                 |
//!   |----------------------|--------------------------------|-----------------------------------|---------------------------------------------|
//!   | Single layer / thumb | `generate_texture`             | `generate_texture_ocio`           | `textures` / `textures_b` (per-layer slot)  |
//!   | [`CompareMode::DiffMatte`] | `generate_diff_texture`   | — (diff is tone-mode-agnostic)    | `diff_texture`, key `(active_layer, diff_multiplier)` |
//!   | [`CompareMode::Composite`] | `generate_composite_texture` | `generate_composite_texture_ocio` | `composite_texture`, key `(active_layer, blend_mode)` |
//!
//! All caches invalidate on a layer-count change; the per-layer slots also clear
//! on an OCIO-state change and via [`ExrViewer::invalidate_reference_textures`]
//! when B is replaced.

use crate::annotation::{Annotation, AnnotationKind, AnnotationTool};
use crate::exr_loader::ExrData;
use crate::gradient::{Colormap, DiffMetric, Gradient};
use eframe::egui;
use rayon::prelude::*;

/// Widen a linear RGB triple to the `vec4` the GPU uniforms expect (w unused).
fn rgb3_to_vec4(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

/// Sample a single channel at `(x, y)`. The `sample_data` match is invariant
/// for the whole channel — in hot pixel loops, prefer pre-extracting the F32
/// slice (the common case) via [`sample_channel_f32`] to avoid the per-pixel
/// enum dispatch. This function is the fallback for F16/U32 channels and the
/// single source of truth for the sampling logic (previously duplicated 8× as
/// inline `get_val` closures).
/// Read one float component from a channel at `(x, y)`, handling F32 (fast
/// path), F16, and U32 `FlatSamples`. Returns 0.0 for a missing channel.
/// `pub(crate)` so the proxy downsample path ([`crate::proxy`]) reuses the
/// single tested implementation instead of duplicating the enum match.
pub(crate) fn sample_channel(
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
    x: usize,
    y: usize,
    width: usize,
) -> f32 {
    if let Some(c) = chan {
        let index = y * width + x;
        match &c.sample_data {
            exr::image::FlatSamples::F16(s) => s[index].to_f32(),
            exr::image::FlatSamples::F32(s) => s[index],
            exr::image::FlatSamples::U32(s) => s[index] as f32 / u32::MAX as f32,
        }
    } else {
        0.0
    }
}

/// If the channel is F32 (the common EXR case), return its slice for direct
/// indexing — eliminates the per-pixel `FlatSamples` enum match in hot loops.
/// Non-F32 channels return `None`; fall back to [`sample_channel`] for those.
fn sample_channel_f32(
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
) -> Option<&[f32]> {
    chan.and_then(|c| match &c.sample_data {
        exr::image::FlatSamples::F32(s) => Some(s.as_slice()),
        _ => None,
    })
}

/// Read a pixel from a pre-extracted F32 slice, falling back to
/// [`sample_channel`] for non-F32 channels. Used in hot pixel loops to skip
/// the enum match on the F32 fast path.
#[inline]
fn pixel_val(
    f32_slice: Option<&[f32]>,
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
    x: usize,
    y: usize,
    width: usize,
) -> f32 {
    if let Some(s) = f32_slice {
        s[y * width + x]
    } else {
        sample_channel(chan, x, y, width)
    }
}

/// Like [`sample_channel`] but with a bounds check: returns `0.0` if `(x, y)`
/// is outside `[0, w) × [0, h)`. Needed in diff/composite generators where
/// images A and B may have different dimensions and the loop iterates over the
/// union. Previously duplicated 3× as inline 5-arg `get_val` closures.
fn sample_channel_bounded(
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> f32 {
    if x >= w || y >= h {
        return 0.0;
    }
    sample_channel(chan, x, y, w)
}

/// Contact-sheet thumbnail box, in pixels: both the on-screen cell size and the
/// resolution thumbnails are baked at (longest edge), so the two never drift.
const THUMB_BOX: usize = 256;

/// Output dimensions and source stride for a CPU texture bake. With `max_dim ==
/// None` (the full-res CPU-display fallback) this is the source size at stride 1.
/// With `Some(d)` (contact-sheet thumbnails) the source is point-decimated so the
/// longest edge is at most `d` — the per-pixel tone pipeline then runs over the
/// small output instead of the full frame, which is the difference between
/// processing ~34k pixels and ~8M for a 4K layer (re-baked on every frame swap
/// while the sheet is open). Aspect is preserved within rounding.
fn thumb_dims(width: usize, height: usize, max_dim: Option<usize>) -> (usize, usize, usize) {
    match max_dim {
        Some(d) if d > 0 && width.max(height) > d => {
            let stride = width.max(height).div_ceil(d);
            (width.div_ceil(stride), height.div_ceil(stride), stride)
        }
        _ => (width.max(1), height.max(1), 1),
    }
}

/// Which channel(s) the canvas isolates. `RGB` shows full colour; the rest show
/// a single channel as grayscale. Encoded for the shader via [`Self::as_u32`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
#[allow(clippy::upper_case_acronyms)] // RGB matches the documented channel_mode mapping
pub enum ChannelMode {
    #[default]
    RGB,
    R,
    G,
    B,
    A,
}

impl ChannelMode {
    /// Integer encoding shared with the GPU. This is the **single source of
    /// truth** for the `channel_mode` mapping; the `switch` in
    /// `gpu/shader.wgsl` must use these same values (RGB=0, R=1, G=2, B=3, A=4).
    /// Changing a value here requires the matching change in the shader.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::RGB => 0,
            Self::R => 1,
            Self::G => 2,
            Self::B => 3,
            Self::A => 4,
        }
    }
}

/// How A and B are shown together. `SingleA`/`SingleB` ignore the other image;
/// the rest require a loaded B. `Wipe`, `SideBySide` and `DiffMatte`/`Composite`
/// are all resolved in-shader on the GPU path (and have CPU-fallback generators
/// for `DiffMatte`/`Composite` — see the module-level docs).
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum CompareMode {
    SingleA,
    SingleB,
    Wipe,
    SideBySide,
    DiffMatte,
    Composite,
}

/// Compositing operator for [`CompareMode::Composite`] (premultiplied-alpha
/// aware). Encoded for the shader via [`Self::as_u32`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum BlendMode {
    #[default]
    Over,
    Under,
    Add,
    Multiply,
    Screen,
}

impl BlendMode {
    /// Integer encoding shared with the GPU. This is the **single source of
    /// truth** for the `blend_mode` mapping; the `switch` in `gpu/shader.wgsl`
    /// must use these same values (Over=0, Under=1, Add=2, Multiply=3, Screen=4).
    /// Changing a value here requires the matching change in the shader.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Over => 0,
            Self::Under => 1,
            Self::Add => 2,
            Self::Multiply => 3,
            Self::Screen => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Over => "Over",
            Self::Under => "Under",
            Self::Add => "Add",
            Self::Multiply => "Multiply",
            Self::Screen => "Screen",
        }
    }
}

/// Per-frame canvas geometry computed once in [`ExrViewer::ui`] and handed to the
/// GPU/CPU draw paths, so they take one value instead of a long parameter list
/// and don't recompute layout. All rects are in screen space.
struct CanvasLayout {
    /// Full canvas rect allocated for the image.
    rect: egui::Rect,
    /// Display-window rect (the EXR's framing), scaled + translated.
    disp_rect: egui::Rect,
    /// Data-window rect: where image A's pixels actually land.
    image_rect: egui::Rect,
    /// Size of image A in screen pixels (`tex_size * scale`).
    image_size: egui::Vec2,
    /// Pixel dimensions of image A's active layer.
    tex_size: egui::Vec2,
    /// Pixel dimensions of image B's active layer, if B is loaded.
    tex_size_b: Option<egui::Vec2>,
    /// Whether the active compare mode is side-by-side (skips overscan dimming
    /// and the display-window overlays).
    is_side_by_side: bool,
}

/// Cache key for the CPU `diff_texture`: `(layer, gain, colormap, metric, floor)`.
/// Compared by value (`Colormap` is `PartialEq`) to invalidate the cached diff
/// when any control that affects its pixels changes.
type DiffCacheKey = (usize, f32, Colormap, DiffMetric, f32);

/// Which feature the shared gradient editor is currently editing — the result of
/// "Apply" / "Save as preset" is routed accordingly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GradientTarget {
    DiffColormap,
    Background,
}

/// A pre-built T2 GPU texture (#56): the `BindGroup` to paint plus the owning
/// `Texture`. Eviction simply **drops** this handle; wgpu reclaims the VRAM once
/// no live reference remains (it refuses to free a texture whose view is still
/// bound, which is the safety we rely on). We deliberately do *not* call
/// `Texture::destroy()` — that forcibly frees regardless of references, and on
/// Vulkan a draw recorded this frame against a just-destroyed texture aborts the
/// process at submit (Metal tolerated it; Vulkan does not). The `BindGroup` is
/// shared (`Arc`) with the active-layer slot while displayed.
struct T2Texture {
    // Held to own the texture for the ring entry's lifetime: dropping this handle
    // (on eviction) releases our reference so wgpu can reclaim the VRAM once the
    // bind group is gone too. Not read directly — ownership/drop is the point.
    #[allow(dead_code)]
    texture: eframe::egui_wgpu::wgpu::Texture,
    bind_group: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
}

/// Pick the T2 frame to evict: the resident frame furthest from the on-screen
/// frame, which is itself never chosen (its texture is bound for paint). `None`
/// when nothing but the on-screen frame remains. Pure — the eviction policy is
/// unit-tested here; the surrounding handle drop is not.
fn t2_victim(frames: impl Iterator<Item = u32>, on_screen: Option<u32>) -> Option<u32> {
    let anchor = on_screen.unwrap_or(0);
    frames
        .filter(|&f| Some(f) != on_screen)
        .max_by_key(|&f| f.abs_diff(anchor))
}

/// All canvas state for one A/B pair: view transform, tone controls, the active
/// [`CompareMode`], the texture caches described in the module docs, plus
/// sampling/histogram/contact-sheet state. Driven each frame by [`Self::ui`].
pub struct ExrViewer {
    textures: Vec<Option<egui::TextureHandle>>,
    textures_b: Vec<Option<egui::TextureHandle>>,
    gpu_textures: Vec<Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>>,
    gpu_textures_b: Vec<Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>>,

    /// T2 GPU-texture ring (#56): pre-built active-layer textures keyed by frame
    /// number, so a sequence frame swap binds an already-uploaded texture instead
    /// of re-packing + re-uploading on the UI thread. Valid only for `t2_layer`;
    /// cleared on a layer switch. Empty / unused for a single image.
    t2_ring: std::collections::HashMap<u32, T2Texture>,
    /// The active layer `t2_ring` was built for; a change invalidates the ring.
    t2_layer: usize,
    /// Max frames the ring may hold (VRAM-budgeted by the app each frame). `0`
    /// disables T2 entirely → the lazy per-swap path (the safe fallback).
    t2_cap: usize,
    /// The sequence frame on screen, so `ui()` binds its T2 texture. `None` for a
    /// single image (lazy path).
    t2_frame: Option<u32>,
    diff_texture: Option<egui::TextureHandle>,
    /// Cache key for `diff_texture`: layer + every control that changes the diff
    /// pixels (gain, colormap identity, metric, noise floor).
    last_diff_params: DiffCacheKey,
    /// Diff visualization controls (see issue #15). The active colormap, the
    /// magnitude metric, and the noise floor. Hydrated from `ExrApp` each frame so
    /// they persist across sessions; mutated here by the mode-param UI.
    pub diff_colormap: Colormap,
    pub diff_metric: DiffMetric,
    pub diff_floor: f32,
    /// User-saved named gradients (the preset library shared with the gradient
    /// editor). Round-tripped through `ExrApp` for persistence.
    pub custom_gradients: Vec<(String, Gradient)>,
    /// Baked 256-entry colormap LUT bytes + the colormap they were baked from, so
    /// the GPU texture is re-uploaded only when the active gradient changes.
    /// Transient (rebuilt on demand).
    colormap_lut: Vec<u8>,
    colormap_sig: Option<Colormap>,
    /// Transient gradient-editor window state. Shared by the diff colormap editor
    /// and the background gradient editor; `gradient_editor_target` says which.
    gradient_editor_open: bool,
    editing_gradient: Gradient,
    new_preset_name: String,
    gradient_editor_target: GradientTarget,

    /// Customizable viewport background (issue #18). Hydrated from `ExrApp` each
    /// frame (persisted there); mutated by the background settings window.
    pub background: crate::background::Background,
    /// Named background presets (mode + colours + gradient). Round-tripped through
    /// `ExrApp` for persistence.
    pub background_presets: Vec<(String, crate::background::Background)>,
    /// Whether the background settings window is open, and the in-progress preset
    /// name. Transient.
    pub show_background_window: bool,
    new_bg_preset_name: String,
    /// Baked background-gradient LUT bytes + the ramp they were baked from, so the
    /// GPU texture is re-uploaded only when the gradient ramp changes.
    bg_gradient_lut: Vec<u8>,
    bg_gradient_sig: Option<Gradient>,
    composite_texture: Option<egui::TextureHandle>,
    last_composite_params: (usize, BlendMode), // (layer_index, blend_mode)
    pub blink_state: bool,
    pub blink_interval: f32,
    pub fullscreen: bool,
    // Add viewing options like exposure, gamma, srgb toggle
    pub exposure: f32,
    pub overscan_opacity: f32,
    pub gamma: f32,
    pub srgb: bool,
    pub enable_lut: bool,
    /// `.cube` LUT domain bounds (xyz + pad), hydrated from `ExrApp` each frame
    /// alongside `enable_lut`. Used to build the GPU uniform so non-unit-domain
    /// LUTs sample correctly. Defaults to the identity `[0,0,0,0]`/`[1,1,1,1]`.
    pub lut_domain_min: [f32; 4],
    pub lut_domain_max: [f32; 4],
    /// When true (OCIO config loaded + enabled), the single-image central path renders via the
    /// two-pass OCIO callback instead of the direct display chain. Set by the app.
    #[cfg(feature = "ocio")]
    pub ocio_active: bool,
    /// CPU display transform for thumbnails / CPU fallback (mirrors the GPU OCIO path). Set by
    /// the app; shared via `Rc` because `CpuProcessor` isn't `Clone`.
    #[cfg(feature = "ocio")]
    pub ocio_cpu: Option<std::rc::Rc<floki_ocio::CpuProcessor>>,
    /// Identity of the current OCIO CPU state; cached CPU textures are invalidated when it
    /// changes (toggle on/off or a new display/view).
    #[cfg(feature = "ocio")]
    ocio_sig: usize,
    pub show_tooltip: bool,
    pub channel_mode: ChannelMode,
    pub compare_mode: CompareMode,
    pub blend_mode: BlendMode,
    pub sample_aperture: usize,
    pub wipe_center: [f32; 2],
    pub wipe_angle: f32,
    pub wipe_line_opacity: f32,
    pub diff_multiplier: f32,
    pub active_layer: usize,
    pub show_contact_sheet: bool,
    pub normalize_side_by_side: bool,
    pub swatches: Vec<[f32; 4]>,
    pub histogram: Option<[u32; 256]>,
    pub histogram_b: Option<[u32; 256]>,
    /// Cache key for the computed bins: `(active_layer, log_histogram)`. The bins
    /// depend on both, so keying on the layer alone left stale bins when the log
    /// toggle flipped. Image-B load/unload is invalidated explicitly via
    /// [`ExrViewer::invalidate_histogram`] since B identity isn't in the key.
    histogram_key: Option<(usize, bool)>,
    pub log_histogram: bool,

    // View transform
    pub scale: f32,
    pub translation: egui::Vec2,
    pub first_frame: bool,
    pub last_hover_pos_img: Option<(usize, usize)>,
    pub last_sampled_val_a: Option<[f32; 4]>,
    pub last_sampled_val_b: Option<[f32; 4]>,

    /// Natural (unclipped) height of the contextual mode-param row, recorded each
    /// frame it renders so the slide-in animation knows how far to grow. Transient
    /// runtime state — not persisted.
    row2_full_height: f32,

    /// The image canvas rect (egui points) from the last frame, used by the
    /// snapshot feature (#19) to crop the framebuffer screenshot to the image
    /// area. Transient.
    pub last_canvas_rect: Option<egui::Rect>,

    /// The displayed image rect (egui points) from the last frame — the display
    /// window clamped to the canvas. The snapshot (#19, #52) crops to this so the
    /// saved frame is just the active image, not the surrounding background.
    /// `None` falls back to `last_canvas_rect`. Transient.
    pub last_image_rect: Option<egui::Rect>,

    /// Annotation overlay (#45) — all transient (per-session, never persisted).
    /// Shapes are stored in image space so they track pan/zoom.
    pub annotations: Vec<Annotation>,
    pub anno_tool: AnnotationTool,
    pub anno_color: egui::Color32,
    pub anno_width: f32,
    pub show_annotation_bar: bool,
    /// Shape being dragged out right now (committed on release).
    anno_in_progress: Option<Annotation>,
    /// Undo/redo stacks of whole-`annotations` snapshots.
    anno_undo: Vec<Vec<Annotation>>,
    anno_redo: Vec<Vec<Annotation>>,
    /// Active text placement: `(image-space anchor, buffer)` while typing.
    anno_text_edit: Option<([f32; 2], String)>,

    /// Low-res first-paint proxy for slot A (#58/#33): shown while the full
    /// `ExrData` decode is in flight. A tone-baked `egui::TextureHandle`
    /// (exposure/gamma/sRGB + background, mirroring the CPU `generate_texture`
    /// path) so `painter.image` renders a correctly tone-mapped preview. The
    /// full-res GPU path takes over when the decode lands and
    /// [`crate::app::ExrApp::swap_image_data`] clears this. Transient.
    proxy_texture: Option<egui::TextureHandle>,
    /// Full-resolution pixel dimensions of the proxy's source image, used for
    /// viewport layout so the proxy lands in the same rect the full-res render
    /// will occupy (the proxy texture itself is lower-res and upscaled).
    proxy_full_size: Option<egui::Vec2>,
}

impl Default for ExrViewer {
    fn default() -> Self {
        Self {
            textures: Vec::new(),
            textures_b: Vec::new(),
            gpu_textures: Vec::new(),
            gpu_textures_b: Vec::new(),
            t2_ring: std::collections::HashMap::new(),
            t2_layer: 0,
            t2_cap: 0,
            t2_frame: None,
            diff_texture: None,
            last_diff_params: (0, 0.0, Colormap::BlackBody, DiffMetric::MaxChannel, 0.0),
            diff_colormap: Colormap::BlackBody,
            diff_metric: DiffMetric::MaxChannel,
            diff_floor: 0.0,
            custom_gradients: Vec::new(),
            colormap_lut: Vec::new(),
            colormap_sig: None,
            gradient_editor_open: false,
            editing_gradient: Colormap::BlackBody.gradient(),
            new_preset_name: String::new(),
            gradient_editor_target: GradientTarget::DiffColormap,
            background: crate::background::Background::default(),
            background_presets: Vec::new(),
            show_background_window: false,
            new_bg_preset_name: String::new(),
            bg_gradient_lut: Vec::new(),
            bg_gradient_sig: None,
            composite_texture: None,
            last_composite_params: (0, BlendMode::Over),
            blink_state: false,
            blink_interval: 1.0,
            fullscreen: false,
            exposure: 0.0,
            overscan_opacity: 0.2,
            gamma: 1.0,
            srgb: true,
            enable_lut: false,
            lut_domain_min: [0.0, 0.0, 0.0, 0.0],
            lut_domain_max: [1.0, 1.0, 1.0, 0.0],
            #[cfg(feature = "ocio")]
            ocio_active: false,
            #[cfg(feature = "ocio")]
            ocio_cpu: None,
            #[cfg(feature = "ocio")]
            ocio_sig: 0,
            show_tooltip: true,
            channel_mode: ChannelMode::RGB,
            compare_mode: CompareMode::SingleA,
            blend_mode: BlendMode::Over,
            sample_aperture: 1,
            wipe_center: [0.5, 0.5],
            wipe_angle: 0.0,
            wipe_line_opacity: 1.0,
            diff_multiplier: 8.0,
            active_layer: 0,
            show_contact_sheet: false,
            normalize_side_by_side: true,
            swatches: Vec::new(),
            histogram: None,
            histogram_b: None,
            histogram_key: None,
            log_histogram: true,
            scale: 1.0,
            translation: egui::Vec2::ZERO,
            first_frame: true,
            last_hover_pos_img: None,
            last_sampled_val_a: None,
            last_sampled_val_b: None,
            row2_full_height: 0.0,
            last_canvas_rect: None,
            last_image_rect: None,
            annotations: Vec::new(),
            anno_tool: AnnotationTool::None,
            anno_color: egui::Color32::RED,
            anno_width: 3.0,
            show_annotation_bar: false,
            anno_in_progress: None,
            anno_undo: Vec::new(),
            anno_redo: Vec::new(),
            anno_text_edit: None,
            proxy_texture: None,
            proxy_full_size: None,
        }
    }
}

impl ExrViewer {
    /// Apply keyboard shortcuts that only mutate view state — compare-mode
    /// selection (1/2/Space) and channel isolation (R/G/B/A/C/F). Extracted
    /// from [`Self::ui`] as a rendering-free seam so the input handling can be
    /// driven headlessly in tests (no wgpu device required).
    ///
    /// `has_b` is whether a reference image (B) is loaded; the B-only shortcuts
    /// are inert without it. Channel shortcuts apply only in the single-view
    /// layout (not the contact sheet), matching the original inline behavior.
    pub fn handle_hotkeys(&mut self, ui: &egui::Ui, has_b: bool) {
        // Sending a viewport command requires `ui.ctx()`, which we cannot touch
        // while the input lock is held — defer it until after the closure.
        let mut fullscreen_changed = false;

        // When a text field wants keyboard input (e.g. the annotation text field or
        // a preset-name box), suppress the single-key viewer shortcuts so typing
        // "r" doesn't isolate the red channel. F11 / Esc stay live.
        let editing = ui.ctx().egui_wants_keyboard_input();

        ui.input(|i| {
            if !editing && i.key_pressed(egui::Key::Num1) {
                self.compare_mode = CompareMode::SingleA;
                self.blink_state = false;
            }
            if !editing && has_b && i.key_pressed(egui::Key::Num2) {
                self.compare_mode = CompareMode::SingleB;
                self.blink_state = false;
            }
            if !editing && has_b && i.key_pressed(egui::Key::Space) {
                self.blink_state = !self.blink_state;
            }

            // Full-screen toggle (F11) and ESC-to-exit work in any mode.
            if i.key_pressed(egui::Key::F11) {
                self.fullscreen = !self.fullscreen;
                fullscreen_changed = true;
            }
            // Esc first cancels any in-flight annotation tool/draw/text (#45),
            // then falls through to exiting fullscreen.
            if i.key_pressed(egui::Key::Escape) {
                if self.cancel_annotation() {
                    // consumed by annotation
                } else if self.fullscreen {
                    self.fullscreen = false;
                    fullscreen_changed = true;
                }
            }

            // Reset exposure (E) / gamma (Shift+G). Gamma deliberately uses
            // Shift+G because plain `G` isolates the green channel below.
            if !editing && i.key_pressed(egui::Key::E) {
                self.reset_exposure();
            }
            if !editing && i.modifiers.shift && i.key_pressed(egui::Key::G) {
                self.reset_gamma();
            }

            if !editing && !self.show_contact_sheet {
                if i.key_pressed(egui::Key::F) {
                    self.first_frame = true;
                }
                let prev_mode = self.channel_mode;
                if i.key_pressed(egui::Key::R) {
                    self.channel_mode = ChannelMode::R;
                }
                // Plain G only — Shift+G is the gamma reset handled above.
                if i.key_pressed(egui::Key::G) && !i.modifiers.shift {
                    self.channel_mode = ChannelMode::G;
                }
                if i.key_pressed(egui::Key::B) {
                    self.channel_mode = ChannelMode::B;
                }
                if i.key_pressed(egui::Key::A) {
                    self.channel_mode = ChannelMode::A;
                }
                if i.key_pressed(egui::Key::C) {
                    self.channel_mode = ChannelMode::RGB;
                }
                if self.channel_mode != prev_mode {
                    self.textures.fill(None);
                    self.textures_b.fill(None);
                    self.diff_texture = None;
                }
            }
        });

        if fullscreen_changed {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
    }

    /// Invalidate cached CPU display textures whose pixels depend on the
    /// exposure / gamma / sRGB tone pipeline, so they regenerate next frame.
    /// (The GPU path reads the live uniform each frame and needs no invalidation.)
    fn invalidate_tone(&mut self) {
        self.textures.fill(None);
        self.textures_b.fill(None);
        self.diff_texture = None;
        self.composite_texture = None;
    }

    fn reset_exposure(&mut self) {
        self.exposure = 0.0;
        self.invalidate_tone();
    }

    fn reset_gamma(&mut self) {
        self.gamma = 1.0;
        self.invalidate_tone();
    }

    /// Whether the active comparison mode has parameters that belong on the
    /// contextual second toolbar row. Drives the slide-in/out of that row.
    /// Pure and GPU-free so it can be unit-tested headlessly.
    ///
    /// Blink is checked first because, while blinking, [`Self::ui`] overwrites
    /// `compare_mode` with `SingleA`/`SingleB` each frame (which would otherwise
    /// report no params) — yet the blink-speed control still needs a home.
    fn has_mode_params(&self) -> bool {
        if self.blink_state {
            return true;
        }
        matches!(
            self.compare_mode,
            CompareMode::Wipe
                | CompareMode::DiffMatte
                | CompareMode::SideBySide
                | CompareMode::Composite
        )
    }

    /// Row 1 of the viewer controls — the always-present essentials: compare-mode
    /// selector, compact exposure/gamma drag-values, channel isolation, sample
    /// aperture, and a `Display ▾` menu for the rarely-touched options.
    fn primary_row(
        &mut self,
        ui: &mut egui::Ui,
        exr_data: &ExrData,
        has_b: bool,
        layer_count: usize,
    ) {
        ui.horizontal(|ui| {
            if has_b {
                ui.label("Compare:");
                ui.selectable_value(&mut self.compare_mode, CompareMode::SingleA, "A");
                ui.selectable_value(&mut self.compare_mode, CompareMode::SingleB, "B");

                ui.add_enabled_ui(!self.show_contact_sheet, |ui| {
                    ui.selectable_value(&mut self.compare_mode, CompareMode::Wipe, "Wipe");
                });
                ui.selectable_value(
                    &mut self.compare_mode,
                    CompareMode::SideBySide,
                    "Side-by-Side",
                );
                ui.add_enabled_ui(!self.show_contact_sheet, |ui| {
                    ui.selectable_value(&mut self.compare_mode, CompareMode::DiffMatte, "Diff");
                    ui.selectable_value(&mut self.compare_mode, CompareMode::Composite, "Comp");
                });
                if ui
                    .toggle_value(&mut self.blink_state, "Blink (Spc)")
                    .clicked()
                    && !self.blink_state
                {
                    self.compare_mode = CompareMode::SingleA;
                }
            }

            // Contact Sheet is a view-mode toggle that belongs with the compare
            // modes; it applies to any multi-layer image, with or without B.
            if layer_count > 1
                && ui
                    .toggle_value(&mut self.show_contact_sheet, "Contact Sheet")
                    .changed()
                && self.show_contact_sheet
                && (self.compare_mode == CompareMode::Wipe
                    || self.compare_mode == CompareMode::DiffMatte
                    || self.compare_mode == CompareMode::Composite)
            {
                self.compare_mode = CompareMode::SideBySide;
            }

            if has_b || layer_count > 1 {
                ui.separator();
            }

            // Compact exposure drag-value. Right-click resets to 0.0 (also key E).
            let exp = ui
                .add(
                    egui::DragValue::new(&mut self.exposure)
                        .speed(0.01)
                        .range(-5.0..=5.0)
                        .prefix("EV ")
                        .fixed_decimals(2),
                )
                .on_hover_text("Drag to adjust • right-click resets to 0.0 (key: E)");
            if exp.changed() {
                self.invalidate_tone();
            }
            if exp.secondary_clicked() {
                self.reset_exposure();
            }

            // Compact gamma drag-value. Right-click resets to 1.0 (also Shift+G).
            let gam = ui
                .add(
                    egui::DragValue::new(&mut self.gamma)
                        .speed(0.01)
                        .range(0.1..=5.0)
                        .prefix("γ ")
                        .fixed_decimals(2),
                )
                .on_hover_text("Drag to adjust • right-click resets to 1.0 (key: Shift+G)");
            if gam.changed() {
                self.invalidate_tone();
            }
            if gam.secondary_clicked() {
                self.reset_gamma();
            }

            if ui
                .button("⟲")
                .on_hover_text("Reset exposure (0.0) & gamma (1.0)")
                .clicked()
            {
                self.reset_exposure();
                self.reset_gamma();
            }
            if ui.checkbox(&mut self.srgb, "sRGB").changed() {
                self.invalidate_tone();
            }

            ui.separator();
            ui.label("Channel:");
            let prev_mode = self.channel_mode;
            ui.selectable_value(&mut self.channel_mode, ChannelMode::RGB, "RGB (C)");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::R, "R");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::G, "G");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::B, "B");
            ui.selectable_value(&mut self.channel_mode, ChannelMode::A, "A");
            if self.channel_mode != prev_mode {
                self.textures.fill(None);
            }

            ui.separator();
            // Sample aperture as a compact dropdown.
            let sample_label = match self.sample_aperture {
                1 => "1px",
                9 => "9×9",
                _ => "3×3",
            };
            ui.label("Sample:");
            egui::ComboBox::from_id_salt("sample_aperture")
                .selected_text(sample_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.sample_aperture, 1, "1px");
                    ui.selectable_value(&mut self.sample_aperture, 3, "3×3");
                    ui.selectable_value(&mut self.sample_aperture, 9, "9×9");
                });

            // Rarely-touched controls tucked behind a menu.
            ui.menu_button("Display ▾", |ui| {
                ui.label("Overscan Opacity:");
                ui.add(egui::Slider::new(&mut self.overscan_opacity, 0.0..=1.0));
                ui.checkbox(&mut self.show_tooltip, "Show Pixel Tooltip");
            });

            if !self.show_contact_sheet {
                ui.separator();
                if ui.button("Frame (F)").clicked() {
                    self.first_frame = true;
                }
                // Annotation overlay toggle (#45).
                if ui
                    .toggle_value(&mut self.show_annotation_bar, "✎ Annotate")
                    .on_hover_text("Mark up the view (arrows / box / pen / text) before a snapshot")
                    .clicked()
                    && !self.show_annotation_bar
                {
                    // Hiding the toolbar also drops the active tool so canvas drags
                    // pan again.
                    self.anno_tool = AnnotationTool::None;
                }

                // Layer (pass) selection
                if layer_count > 1 {
                    ui.label("Layer:");
                    let selected_name = exr_data
                        .logical_layers
                        .get(self.active_layer)
                        .map(|l| l.name.as_str())
                        .unwrap_or("Unnamed");
                    egui::ComboBox::from_id_salt("layer_select")
                        .selected_text(selected_name)
                        .show_ui(ui, |ui| {
                            for (i, ll) in exr_data.logical_layers.iter().enumerate() {
                                if ui
                                    .selectable_value(&mut self.active_layer, i, &ll.name)
                                    .clicked()
                                {
                                    self.first_frame = true;
                                }
                            }
                        });
                }
            }
        });
    }

    /// Row 2 content — only the active comparison mode's parameters. Rendered
    /// (clipped/animated) by [`Self::animated_mode_param_row`]. Kept in lockstep
    /// with [`Self::has_mode_params`]: every arm that draws here must report
    /// `true` there, and vice versa.
    fn mode_param_row(&mut self, ui: &mut egui::Ui) {
        // While blinking, `compare_mode` toggles A/B each frame, so key off
        // `blink_state` and expose the blink-speed control instead.
        if self.blink_state {
            ui.label("Blink speed:");
            ui.add(egui::Slider::new(&mut self.blink_interval, 0.05..=5.0).suffix("s"));
            return;
        }
        match self.compare_mode {
            CompareMode::Wipe => {
                // Each slider gets a left-side `ui.label(...)` (matching the
                // `Blend:` style below) for a consistent row; the two center
                // sliders are named so the wipe-center handle is self-describing.
                ui.label("Center X");
                ui.add(egui::Slider::new(&mut self.wipe_center[0], 0.0..=1.0));
                ui.label("Center Y");
                ui.add(egui::Slider::new(&mut self.wipe_center[1], 0.0..=1.0));
                ui.label("Angle °");
                ui.add(egui::Slider::new(&mut self.wipe_angle, -180.0..=180.0));
                ui.label("Line Opacity");
                ui.add(egui::Slider::new(&mut self.wipe_line_opacity, 0.0..=1.0));
            }
            CompareMode::DiffMatte => {
                ui.add(egui::Slider::new(&mut self.diff_multiplier, 0.0..=100.0).text("Diff Gain"));
                ui.separator();

                ui.label("Colormap");
                let mut pick: Option<Colormap> = None;
                egui::ComboBox::from_id_salt("diff_colormap_select")
                    .selected_text(self.diff_colormap.label())
                    .show_ui(ui, |ui| {
                        for cm in Colormap::PRESETS {
                            if ui
                                .selectable_label(self.diff_colormap == cm, cm.label())
                                .clicked()
                            {
                                pick = Some(cm);
                            }
                        }
                        if !self.custom_gradients.is_empty() {
                            ui.separator();
                            for (name, g) in &self.custom_gradients {
                                let selected = matches!(&self.diff_colormap, Colormap::Custom(cur) if cur == g);
                                if ui.selectable_label(selected, name).clicked() {
                                    pick = Some(Colormap::Custom(g.clone()));
                                }
                            }
                        }
                    });
                if let Some(cm) = pick {
                    self.diff_colormap = cm;
                }

                ui.label("Metric");
                egui::ComboBox::from_id_salt("diff_metric_select")
                    .selected_text(self.diff_metric.label())
                    .show_ui(ui, |ui| {
                        for m in DiffMetric::ALL {
                            ui.selectable_value(&mut self.diff_metric, m, m.label());
                        }
                    });

                ui.label("Floor");
                ui.add(egui::Slider::new(&mut self.diff_floor, 0.0..=0.25));

                // Legend / scale bar. Per-channel RGB has no colormap, so skip it.
                if self.diff_metric != DiffMetric::PerChannelRGB {
                    self.diff_legend(ui);
                }

                if ui.button("Edit gradient…").clicked() {
                    self.editing_gradient = self.diff_colormap.gradient();
                    self.gradient_editor_target = GradientTarget::DiffColormap;
                    self.gradient_editor_open = true;
                }
            }
            CompareMode::SideBySide => {
                ui.checkbox(&mut self.normalize_side_by_side, "Normalize Size");
            }
            CompareMode::Composite => {
                ui.label("Blend:");
                egui::ComboBox::from_id_salt("blend_mode_select")
                    .selected_text(self.blend_mode.label())
                    .show_ui(ui, |ui| {
                        for mode in [
                            BlendMode::Over,
                            BlendMode::Under,
                            BlendMode::Add,
                            BlendMode::Multiply,
                            BlendMode::Screen,
                        ] {
                            ui.selectable_value(&mut self.blend_mode, mode, mode.label());
                        }
                    });
            }
            CompareMode::SingleA | CompareMode::SingleB => {}
        }
    }

    /// Draw the diff colormap legend: a horizontal bar sampling the active
    /// gradient left→right, captioned with the diff magnitude `0 → 1/gain` that
    /// spans black→saturated. The legend is a visualization aid; it is not
    /// interactive.
    fn diff_legend(&self, ui: &mut egui::Ui) {
        let grad = self.diff_colormap.gradient();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 14.0), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            let n = rect.width().round().max(1.0) as usize;
            let denom = (n.saturating_sub(1)).max(1) as f32;
            for i in 0..n {
                let c = grad.sample(i as f32 / denom);
                let x = rect.left() + i as f32;
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x, rect.top()),
                        egui::pos2(x + 1.0, rect.bottom()),
                    ),
                    0.0,
                    egui::Color32::from_rgb(
                        (c[0] * 255.0 + 0.5) as u8,
                        (c[1] * 255.0 + 0.5) as u8,
                        (c[2] * 255.0 + 0.5) as u8,
                    ),
                );
            }
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                egui::StrokeKind::Inside,
            );
        }
        // `m` saturates at diff magnitude `1/gain` (the noise floor only shifts
        // where black ends, not where white begins).
        if self.diff_multiplier > 0.0 {
            ui.label(format!("0 – {:.3}", 1.0 / self.diff_multiplier))
                .on_hover_text("Diff magnitude spanned by the colormap (0 → saturated).");
        }
    }

    /// Modal-ish gradient editor (a floating [`egui::Window`]). Lets the user
    /// add/remove/move/recolor stops on a working copy and either apply it as the
    /// active diff colormap or save it as a named preset in `custom_gradients`.
    /// Rendered once per frame from [`Self::ui`] when `gradient_editor_open`.
    fn gradient_editor_window(&mut self, ctx: &egui::Context) {
        if !self.gradient_editor_open {
            return;
        }
        let mut open = self.gradient_editor_open;
        let mut apply = false;
        let mut save = false;
        egui::Window::new("Gradient editor")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                // Preview bar of the working gradient.
                let grad = Gradient::new(self.editing_gradient.stops.clone());
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(240.0, 18.0), egui::Sense::hover());
                if ui.is_rect_visible(rect) {
                    let painter = ui.painter_at(rect);
                    let n = rect.width().round().max(1.0) as usize;
                    let denom = (n.saturating_sub(1)).max(1) as f32;
                    for i in 0..n {
                        let c = grad.sample(i as f32 / denom);
                        let x = rect.left() + i as f32;
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(x, rect.top()),
                                egui::pos2(x + 1.0, rect.bottom()),
                            ),
                            0.0,
                            egui::Color32::from_rgb(
                                (c[0] * 255.0 + 0.5) as u8,
                                (c[1] * 255.0 + 0.5) as u8,
                                (c[2] * 255.0 + 0.5) as u8,
                            ),
                        );
                    }
                }
                ui.separator();

                // Per-stop rows: position slider, colour picker, delete.
                let mut remove: Option<usize> = None;
                let mut dirty = false;
                let len = self.editing_gradient.stops.len();
                for (i, stop) in self.editing_gradient.stops.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.add(egui::Slider::new(&mut stop.t, 0.0..=1.0)).changed() {
                            dirty = true;
                        }
                        if ui.color_edit_button_rgb(&mut stop.color).changed() {
                            dirty = true;
                        }
                        // Keep at least two stops so the gradient stays meaningful.
                        if len > 2 && ui.button("✕").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.editing_gradient.stops.remove(i);
                    dirty = true;
                }
                if ui.button("＋ Add stop").clicked() {
                    self.editing_gradient
                        .stops
                        .push(crate::gradient::GradientStop::new(0.5, [0.5, 0.5, 0.5]));
                    dirty = true;
                }
                // Re-sort by position if any stop moved (sampling assumes sorted).
                if dirty {
                    self.editing_gradient =
                        Gradient::new(std::mem::take(&mut self.editing_gradient.stops));
                }

                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Preset name");
                    ui.text_edit_singleline(&mut self.new_preset_name);
                });
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    let can_save = !self.new_preset_name.trim().is_empty();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save as preset"))
                        .clicked()
                    {
                        save = true;
                    }
                });
            });

        // Route "Apply" to whichever feature opened the editor.
        let apply_to_target = |s: &mut Self, grad: Gradient| match s.gradient_editor_target {
            GradientTarget::DiffColormap => s.diff_colormap = Colormap::Custom(grad),
            GradientTarget::Background => s.background.gradient = grad,
        };
        if apply {
            apply_to_target(self, self.editing_gradient.clone());
        }
        if save {
            let name = self.new_preset_name.trim().to_string();
            let grad = self.editing_gradient.clone();
            // The named-gradient library is shared by both editors.
            if let Some(slot) = self.custom_gradients.iter_mut().find(|(n, _)| n == &name) {
                slot.1 = grad.clone();
            } else {
                self.custom_gradients.push((name, grad.clone()));
            }
            apply_to_target(self, grad);
            self.new_preset_name.clear();
        }
        self.gradient_editor_open = open;
    }

    /// The viewport-background settings window (issue #18): mode selector, the
    /// per-mode colour/size/gradient controls, and a named-preset library. Mutates
    /// `self.background` live; rendered once per frame from [`Self::ui`] when
    /// `show_background_window`. Colours are linear (see `background` module docs).
    fn background_window(&mut self, ctx: &egui::Context) {
        if !self.show_background_window {
            return;
        }
        use crate::background::BackgroundMode;
        let mut open = self.show_background_window;
        egui::Window::new("Viewport background")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Mode");
                    egui::ComboBox::from_id_salt("bg_mode_select")
                        .selected_text(self.background.mode.label())
                        .show_ui(ui, |ui| {
                            for m in BackgroundMode::ALL {
                                ui.selectable_value(&mut self.background.mode, m, m.label());
                            }
                        });
                });
                ui.separator();

                match self.background.mode {
                    BackgroundMode::Checkerboard => {
                        ui.horizontal(|ui| {
                            ui.label("Dark");
                            ui.color_edit_button_rgb(&mut self.background.checker_dark);
                            ui.label("Light");
                            ui.color_edit_button_rgb(&mut self.background.checker_light);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Cell size");
                            ui.add(
                                egui::Slider::new(&mut self.background.checker_size, 2.0..=128.0)
                                    .suffix(" px"),
                            );
                        });
                    }
                    BackgroundMode::Solid => {
                        ui.horizontal(|ui| {
                            ui.label("Colour");
                            ui.color_edit_button_rgb(&mut self.background.solid);
                        });
                    }
                    BackgroundMode::Gradient => {
                        // Preview bar of the current gradient.
                        Self::gradient_preview_bar(ui, &self.background.gradient.clone());
                        ui.horizontal(|ui| {
                            ui.label("Angle");
                            ui.add(
                                egui::Slider::new(&mut self.background.gradient_angle, 0.0..=360.0)
                                    .suffix("°"),
                            );
                        });
                        if ui.button("Edit gradient…").clicked() {
                            self.editing_gradient = self.background.gradient.clone();
                            self.gradient_editor_target = GradientTarget::Background;
                            self.gradient_editor_open = true;
                        }
                    }
                }

                ui.separator();
                // Named background presets (mode + colours + gradient).
                ui.label("Presets");
                let mut load: Option<crate::background::Background> = None;
                let mut delete: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .max_height(110.0)
                    .show(ui, |ui| {
                        for (i, (name, preset)) in self.background_presets.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.button(name).clicked() {
                                    load = Some(preset.clone());
                                }
                                if ui.small_button("✕").clicked() {
                                    delete = Some(i);
                                }
                            });
                        }
                    });
                if let Some(bg) = load {
                    self.background = bg;
                }
                if let Some(i) = delete {
                    self.background_presets.remove(i);
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.new_bg_preset_name);
                    let can_save = !self.new_bg_preset_name.trim().is_empty();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save preset"))
                        .clicked()
                    {
                        let name = self.new_bg_preset_name.trim().to_string();
                        let bg = self.background.clone();
                        if let Some(slot) =
                            self.background_presets.iter_mut().find(|(n, _)| n == &name)
                        {
                            slot.1 = bg;
                        } else {
                            self.background_presets.push((name, bg));
                        }
                        self.new_bg_preset_name.clear();
                    }
                });
                if ui.button("Reset to default checker").clicked() {
                    self.background = crate::background::Background::default();
                }
            });
        self.show_background_window = open;
    }

    /// Paint a small horizontal bar previewing `grad` left→right. Shared by the
    /// gradient editor and the background window.
    fn gradient_preview_bar(ui: &mut egui::Ui, grad: &Gradient) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(240.0, 18.0), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            let n = rect.width().round().max(1.0) as usize;
            let denom = (n.saturating_sub(1)).max(1) as f32;
            for i in 0..n {
                let c = grad.sample(i as f32 / denom);
                let x = rect.left() + i as f32;
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x, rect.top()),
                        egui::pos2(x + 1.0, rect.bottom()),
                    ),
                    0.0,
                    egui::Color32::from_rgb(
                        (c[0] * 255.0 + 0.5) as u8,
                        (c[1] * 255.0 + 0.5) as u8,
                        (c[2] * 255.0 + 0.5) as u8,
                    ),
                );
            }
        }
    }

    // ----- Annotation overlay (#45) ------------------------------------------

    /// Push the current annotations onto the undo stack and clear redo. Call
    /// before any mutation (add / clear).
    fn push_anno_undo(&mut self) {
        self.anno_undo.push(self.annotations.clone());
        self.anno_redo.clear();
    }

    fn undo_annotation(&mut self) {
        if let Some(prev) = self.anno_undo.pop() {
            self.anno_redo
                .push(std::mem::replace(&mut self.annotations, prev));
        }
    }

    fn redo_annotation(&mut self) {
        if let Some(next) = self.anno_redo.pop() {
            self.anno_undo
                .push(std::mem::replace(&mut self.annotations, next));
        }
    }

    fn clear_annotations(&mut self) {
        if !self.annotations.is_empty() {
            self.push_anno_undo();
            self.annotations.clear();
        }
    }

    /// Cancel whatever annotation interaction is in flight (active tool, the
    /// in-progress drag, and any open text field). Bound to `Esc`.
    pub fn cancel_annotation(&mut self) -> bool {
        let was_active = self.anno_tool.is_active()
            || self.anno_in_progress.is_some()
            || self.anno_text_edit.is_some();
        self.anno_tool = AnnotationTool::None;
        self.anno_in_progress = None;
        self.anno_text_edit = None;
        was_active
    }

    /// Commit the in-progress text label (if non-empty) to the annotation list.
    fn commit_text_edit(&mut self) {
        if let Some((pos, text)) = self.anno_text_edit.take()
            && !text.trim().is_empty()
        {
            self.push_anno_undo();
            self.annotations.push(Annotation {
                kind: AnnotationKind::Text { pos, text },
                color: self.anno_color,
                width: self.anno_width,
            });
        }
    }

    /// Translate canvas drags/clicks into annotation shapes. Coordinates are
    /// converted to image space so shapes track pan/zoom.
    fn handle_annotation_input(
        &mut self,
        response: &egui::Response,
        image_rect: egui::Rect,
        scale: f32,
    ) {
        let scale = scale.max(1e-6);
        let to_img = |pos: egui::Pos2| {
            [
                (pos.x - image_rect.min.x) / scale,
                (pos.y - image_rect.min.y) / scale,
            ]
        };

        match self.anno_tool {
            AnnotationTool::Text => {
                if response.clicked()
                    && let Some(p) = response.interact_pointer_pos()
                {
                    // Commit any open field first, then start a new one.
                    self.commit_text_edit();
                    self.anno_text_edit = Some((to_img(p), String::new()));
                }
            }
            AnnotationTool::Arrow | AnnotationTool::Rect | AnnotationTool::Freehand => {
                if response.drag_started() {
                    if let Some(p) = response.interact_pointer_pos() {
                        let a = to_img(p);
                        let kind = match self.anno_tool {
                            AnnotationTool::Arrow => AnnotationKind::Arrow { a, b: a },
                            AnnotationTool::Rect => AnnotationKind::Rect { a, b: a },
                            _ => AnnotationKind::Freehand { points: vec![a] },
                        };
                        self.anno_in_progress = Some(Annotation {
                            kind,
                            color: self.anno_color,
                            width: self.anno_width,
                        });
                    }
                } else if response.dragged()
                    && let (Some(p), Some(ann)) = (
                        response.interact_pointer_pos(),
                        self.anno_in_progress.as_mut(),
                    )
                {
                    let cur = to_img(p);
                    match &mut ann.kind {
                        AnnotationKind::Arrow { b, .. } | AnnotationKind::Rect { b, .. } => {
                            *b = cur
                        }
                        AnnotationKind::Freehand { points } => points.push(cur),
                        AnnotationKind::Text { .. } => {}
                    }
                }
                if response.drag_stopped()
                    && let Some(ann) = self.anno_in_progress.take()
                {
                    self.push_anno_undo();
                    self.annotations.push(ann);
                }
            }
            AnnotationTool::None => {}
        }
    }

    /// Paint all committed annotations plus the in-progress shape. Text labels are
    /// drawn here too; the editable text field is a separate popup.
    fn draw_annotations(&self, painter: &egui::Painter, image_rect: egui::Rect, scale: f32) {
        for ann in &self.annotations {
            Self::draw_one_annotation(painter, ann, image_rect, scale);
        }
        if let Some(ann) = &self.anno_in_progress {
            Self::draw_one_annotation(painter, ann, image_rect, scale);
        }
    }

    fn draw_one_annotation(
        painter: &egui::Painter,
        ann: &Annotation,
        image_rect: egui::Rect,
        scale: f32,
    ) {
        let to_screen = |p: [f32; 2]| image_rect.min + egui::vec2(p[0] * scale, p[1] * scale);
        let stroke = egui::Stroke::new(ann.width, ann.color);
        match &ann.kind {
            AnnotationKind::Arrow { a, b } => {
                let (a, b) = (to_screen(*a), to_screen(*b));
                painter.line_segment([a, b], stroke);
                let dir = b - a;
                let len = dir.length();
                if len > 1.0 {
                    let n = dir / len;
                    let head = (len * 0.3).min(14.0);
                    let back = b - n * head;
                    let perp = egui::vec2(-n.y, n.x) * head * 0.5;
                    painter.line_segment([b, back + perp], stroke);
                    painter.line_segment([b, back - perp], stroke);
                }
            }
            AnnotationKind::Rect { a, b } => {
                let r = egui::Rect::from_two_pos(to_screen(*a), to_screen(*b));
                painter.rect_stroke(r, 0.0, stroke, egui::StrokeKind::Middle);
            }
            AnnotationKind::Freehand { points } => {
                if points.len() >= 2 {
                    let pts: Vec<egui::Pos2> = points.iter().map(|p| to_screen(*p)).collect();
                    painter.add(egui::Shape::line(pts, stroke));
                }
            }
            AnnotationKind::Text { pos, text } => {
                painter.text(
                    to_screen(*pos),
                    egui::Align2::LEFT_TOP,
                    text,
                    egui::FontId::proportional(16.0),
                    ann.color,
                );
            }
        }
    }

    /// The editable text field shown at the click point while placing a `Text`
    /// annotation. Enter commits, `Esc` cancels (handled in `handle_hotkeys`).
    fn annotation_text_popup(&mut self, ui: &mut egui::Ui, image_rect: egui::Rect, scale: f32) {
        let Some((pos, _)) = self.anno_text_edit.as_ref() else {
            return;
        };
        let screen = image_rect.min + egui::vec2(pos[0] * scale, pos[1] * scale);
        let mut commit = false;
        egui::Area::new(ui.id().with("anno_text_edit"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen)
            .show(ui.ctx(), |ui| {
                if let Some((_, buf)) = self.anno_text_edit.as_mut() {
                    let resp = ui.add(
                        egui::TextEdit::singleline(buf)
                            .hint_text("label…")
                            .desired_width(160.0),
                    );
                    // Auto-focus on open (buffer empty); keeps focus once typing.
                    if buf.is_empty() {
                        resp.request_focus();
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        commit = true;
                    }
                }
            });
        if commit {
            self.commit_text_edit();
        }
    }

    /// The annotation toolbar row: tool selection, colour, stroke width, undo/redo,
    /// clear. Shown under the mode-param row while `show_annotation_bar`.
    fn annotation_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Annotate:");
            for tool in AnnotationTool::DRAW_TOOLS {
                ui.selectable_value(&mut self.anno_tool, tool, tool.label());
            }
            ui.separator();
            ui.color_edit_button_srgba(&mut self.anno_color);
            ui.add(egui::Slider::new(&mut self.anno_width, 1.0..=12.0).text("Width"));
            ui.separator();
            if ui
                .add_enabled(!self.anno_undo.is_empty(), egui::Button::new("Undo"))
                .clicked()
            {
                self.undo_annotation();
            }
            if ui
                .add_enabled(!self.anno_redo.is_empty(), egui::Button::new("Redo"))
                .clicked()
            {
                self.redo_annotation();
            }
            if ui
                .add_enabled(!self.annotations.is_empty(), egui::Button::new("Clear all"))
                .clicked()
            {
                self.clear_annotations();
            }
        });
    }

    /// Render the contextual mode-param row with a vertical slide in/out. The
    /// row's natural height is captured each frame into `row2_full_height`; the
    /// visible slice is `full_height * t`, where `t` eases 0→1 as the row appears
    /// and 1→0 as it leaves. Contents are clipped to the revealed slice so they
    /// appear to slide out from under Row 1.
    fn animated_mode_param_row(&mut self, ui: &mut egui::Ui) {
        let id = ui.make_persistent_id("viewer_row2_anim");
        let t = ui
            .ctx()
            .animate_bool_with_time(id, self.has_mode_params(), 0.12);
        if t <= 0.0 {
            return;
        }
        ui.scope(|ui| {
            let full_h = self.row2_full_height.max(1.0);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), full_h * t),
                egui::Sense::hover(),
            );
            // Lay the full-height row out at the top of the allocated slice, then
            // clip to the slice so only `full_h * t` of it shows.
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(rect.width(), full_h),
                    ))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            child.set_clip_rect(rect);
            self.mode_param_row(&mut child);
            let measured = child.min_rect().height();
            if measured > 0.0 {
                self.row2_full_height = measured;
            }
        });
    }

    /// Drop cached CPU textures (thumbnails / fallback) when the OCIO CPU
    /// processor changes, so they regenerate with — or without — the display
    /// transform. A no-op while the processor identity is unchanged.
    #[cfg(feature = "ocio")]
    fn invalidate_ocio_cpu_textures(&mut self) {
        let sig = if self.ocio_active {
            self.ocio_cpu
                .as_ref()
                .map(|p| std::rc::Rc::as_ptr(p) as usize)
                .unwrap_or(1)
        } else {
            0
        };
        if sig != self.ocio_sig {
            self.ocio_sig = sig;
            self.textures.fill(None);
            self.textures_b.fill(None);
        }
    }

    /// While blink is active (and B is loaded), alternate the displayed image
    /// between A and B on `blink_interval`, requesting repaints to keep cycling.
    fn apply_blink_mode(&mut self, ui: &egui::Ui, has_b: bool) {
        if self.blink_state && has_b {
            ui.ctx().request_repaint();
            let time = ui.input(|i| i.time);
            if ((time / self.blink_interval as f64) as usize).is_multiple_of(2) {
                self.compare_mode = CompareMode::SingleA;
            } else {
                self.compare_mode = CompareMode::SingleB;
            }
        }
    }

    /// Resize the per-layer texture caches to the current A/B layer counts,
    /// clearing them (forcing regeneration) whenever a count changes.
    fn sync_texture_caches(&mut self, layer_count: usize, layer_count_b: usize) {
        if self.textures.len() != layer_count {
            self.textures.clear();
            self.textures.resize(layer_count, None);
            self.gpu_textures.clear();
            self.gpu_textures.resize(layer_count, None);
        }
        if self.textures_b.len() != layer_count_b {
            self.textures_b.clear();
            self.textures_b.resize(layer_count_b, None);
            self.gpu_textures_b.clear();
            self.gpu_textures_b.resize(layer_count_b, None);
        }
    }

    /// Render the contact sheet: a scrollable grid of per-layer thumbnails for A
    /// (and B alongside, in the side-by-side / wipe / diff modes). Clicking a
    /// thumbnail selects that layer and leaves the sheet.
    fn draw_contact_sheet(
        &mut self,
        ui: &mut egui::Ui,
        exr_data: &ExrData,
        exr_data_b: Option<&ExrData>,
    ) {
        let draw_sheet = |viewer: &mut Self,
                          ui: &mut egui::Ui,
                          data: &crate::exr_loader::ExrData,
                          is_a: bool| {
            let l_count = data.logical_layers.len();
            egui::ScrollArea::vertical()
                .id_salt(if is_a { "sheet_a" } else { "sheet_b" })
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(16.0, 16.0);
                        for i in 0..l_count {
                            // Contact-sheet cells are 256px; bake the thumbnail at
                            // that size rather than full-res (re-baked on every frame
                            // swap while the sheet is open over a sequence).
                            let tex_opt = if is_a {
                                if viewer.textures[i].is_none() {
                                    viewer.textures[i] =
                                        viewer.generate_texture(ui.ctx(), data, i, Some(THUMB_BOX));
                                }
                                viewer.textures[i].as_ref()
                            } else {
                                if viewer.textures_b[i].is_none() {
                                    viewer.textures_b[i] =
                                        viewer.generate_texture(ui.ctx(), data, i, Some(THUMB_BOX));
                                }
                                viewer.textures_b[i].as_ref()
                            };

                            if let Some(texture) = tex_opt {
                                // Reserve an EXACTLY uniform cell, then position the image and
                                // label by absolute geometry. Auto-layout (allocate_ui /
                                // vertical_centered) let cell heights vary by a few px (inherited
                                // item-spacing + variable label line count), and horizontal_wrapped
                                // then center-aligned those unequal cells by different amounts —
                                // producing the slight vertical "staircase". Fixed rects + paint_at
                                // remove that degree of freedom entirely.
                                let thumb_width = THUMB_BOX as f32;
                                let thumb_box = THUMB_BOX as f32;
                                let label_height = 30.0;
                                let tex_size = texture.size_vec2();
                                let aspect = if tex_size.y > 0.0 {
                                    tex_size.x / tex_size.y
                                } else {
                                    1.0
                                };
                                let (fit_w, fit_h) = if aspect >= 1.0 {
                                    (thumb_box, thumb_box / aspect)
                                } else {
                                    (thumb_box * aspect, thumb_box)
                                };
                                let name = data
                                    .logical_layers
                                    .get(i)
                                    .map(|l| l.name.as_str())
                                    .unwrap_or("Unnamed");

                                let (cell_rect, response) = ui.allocate_exact_size(
                                    egui::vec2(thumb_width, thumb_box + label_height),
                                    egui::Sense::click(),
                                );

                                // Image: centered horizontally, centered within the top square box.
                                let img_rect = egui::Rect::from_center_size(
                                    egui::pos2(
                                        cell_rect.center().x,
                                        cell_rect.top() + thumb_box * 0.5,
                                    ),
                                    egui::vec2(fit_w, fit_h),
                                );
                                egui::Image::new(texture).paint_at(ui, img_rect);

                                // Label: centered in the strip beneath the box.
                                ui.painter().text(
                                    egui::pos2(
                                        cell_rect.center().x,
                                        cell_rect.top() + thumb_box + label_height * 0.5,
                                    ),
                                    egui::Align2::CENTER_CENTER,
                                    format!("{i}: {name}"),
                                    egui::FontId::proportional(14.0),
                                    ui.visuals().strong_text_color(),
                                );

                                if response.clicked() {
                                    viewer.active_layer = i;
                                    viewer.show_contact_sheet = false;
                                    viewer.first_frame = true;
                                    if !is_a {
                                        viewer.compare_mode = CompareMode::SingleB;
                                    } else if viewer.compare_mode == CompareMode::SingleB {
                                        viewer.compare_mode = CompareMode::SingleA;
                                    }
                                }
                                if response.hovered() {
                                    response
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text("Click to view layer");
                                }
                            }
                        }
                    });
                });
        };

        if let CompareMode::SideBySide | CompareMode::Wipe | CompareMode::DiffMatte =
            self.compare_mode
        {
            if let Some(exr_b) = exr_data_b {
                ui.columns(2, |cols| {
                    cols[0].heading("Image A");
                    draw_sheet(self, &mut cols[0], exr_data, true);
                    cols[1].heading("Image B");
                    draw_sheet(self, &mut cols[1], exr_b, false);
                });
            } else {
                draw_sheet(self, ui, exr_data, true);
            }
        } else if self.compare_mode == CompareMode::SingleB {
            if let Some(exr_b) = exr_data_b {
                draw_sheet(self, ui, exr_b, false);
            } else {
                ui.label("Image B not loaded.");
            }
        } else {
            draw_sheet(self, ui, exr_data, true);
        }
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        exr_data: &ExrData,
        exr_data_b: Option<&ExrData>,
        gpu_resources: Option<&crate::gpu::GpuResources>,
        lut_bg_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        self.handle_hotkeys(ui, exr_data_b.is_some());

        #[cfg(feature = "ocio")]
        self.invalidate_ocio_cpu_textures();

        self.apply_blink_mode(ui, exr_data_b.is_some());

        let layer_count = exr_data.logical_layers.len();
        egui::Panel::top("viewer_controls").show_inside(ui, |ui| {
            self.primary_row(ui, exr_data, exr_data_b.is_some(), layer_count);
            self.animated_mode_param_row(ui);
            if self.show_annotation_bar {
                self.annotation_toolbar(ui);
            }
        });
        self.gradient_editor_window(ui.ctx());
        self.background_window(ui.ctx());

        let layer_count_b = exr_data_b.map(|d| d.logical_layers.len()).unwrap_or(0);
        self.sync_texture_caches(layer_count, layer_count_b);

        if self.show_contact_sheet {
            self.draw_contact_sheet(ui, exr_data, exr_data_b);
        } else {
            // Channel/frame hotkeys are handled up-front in `handle_hotkeys`.
            let (tw, th) = exr_data.logical_size(self.active_layer).unwrap_or((1, 1));
            let tex_size = egui::vec2(tw as f32, th as f32);
            let mut tex_size_b = None;
            if let Some(data_b) = exr_data_b {
                let layer_b = self
                    .active_layer
                    .min(data_b.logical_layers.len().saturating_sub(1));
                if let Some((bw, bh)) = data_b.logical_size(layer_b) {
                    tex_size_b = Some(egui::vec2(bw as f32, bh as f32));
                }
            }

            if let Some(gpu) = gpu_resources {
                // A layer switch invalidates the per-frame T2 ring (textures are
                // per-layer). Do this before binding/building below.
                self.ensure_t2_layer();
                // T2 (#56): bind the on-screen frame's pre-built texture if it is
                // resident, so the swap is an instant bind, not a re-upload.
                if self.t2_cap > 0
                    && let Some(frame) = self.t2_frame
                    && let Some(t2) = self.t2_ring.get(&frame)
                {
                    self.gpu_textures[self.active_layer] = Some(t2.bind_group.clone());
                }
                if self.gpu_textures[self.active_layer].is_none()
                    && let Some(t2) = Self::build_layer_texture(gpu, exr_data, self.active_layer)
                {
                    self.gpu_textures[self.active_layer] = Some(t2.bind_group.clone());
                    // Cache the freshly-built texture into the T2 ring for the
                    // on-screen frame (a lazy first paint feeds the ring).
                    if self.t2_cap > 0
                        && let Some(frame) = self.t2_frame
                    {
                        self.t2_ring.insert(frame, t2);
                        self.evict_t2();
                    }
                }
                if let Some(data_b) = exr_data_b {
                    let layer_b = self
                        .active_layer
                        .min(data_b.logical_layers.len().saturating_sub(1));
                    if self.gpu_textures_b[layer_b].is_none() {
                        self.gpu_textures_b[layer_b] =
                            Self::build_layer_texture(gpu, data_b, layer_b).map(|t2| t2.bind_group);
                    }
                }
            } else {
                if self.textures[self.active_layer].is_none() {
                    self.textures[self.active_layer] =
                        self.generate_texture(ui.ctx(), exr_data, self.active_layer, None);
                }
                if let Some(data_b) = exr_data_b {
                    let layer_b = self
                        .active_layer
                        .min(data_b.logical_layers.len().saturating_sub(1));
                    if self.textures_b[layer_b].is_none() {
                        self.textures_b[layer_b] =
                            self.generate_texture(ui.ctx(), data_b, layer_b, None);
                    }
                }
                let diff_key: DiffCacheKey = (
                    self.active_layer,
                    self.diff_multiplier,
                    self.diff_colormap.clone(),
                    self.diff_metric,
                    self.diff_floor,
                );
                if let Some(exr_b) = exr_data_b
                    && self.compare_mode == CompareMode::DiffMatte
                    && (self.diff_texture.is_none() || self.last_diff_params != diff_key)
                {
                    let layer_b = self
                        .active_layer
                        .min(exr_b.logical_layers.len().saturating_sub(1));
                    self.diff_texture = self.generate_diff_texture(
                        ui.ctx(),
                        exr_data,
                        exr_b,
                        self.active_layer,
                        layer_b,
                    );
                    self.last_diff_params = diff_key;
                }
                if let Some(exr_b) = exr_data_b
                    && self.compare_mode == CompareMode::Composite
                    && (self.composite_texture.is_none()
                        || self.last_composite_params != (self.active_layer, self.blend_mode))
                {
                    let layer_b = self
                        .active_layer
                        .min(exr_b.logical_layers.len().saturating_sub(1));
                    self.composite_texture = self.generate_composite_texture(
                        ui.ctx(),
                        exr_data,
                        exr_b,
                        self.active_layer,
                        layer_b,
                    );
                    self.last_composite_params = (self.active_layer, self.blend_mode);
                }
            }

            // Draw texture
            let has_texture = if gpu_resources.is_some() {
                self.gpu_textures[self.active_layer].is_some()
            } else {
                self.textures[self.active_layer].is_some()
            };
            if has_texture {
                let (rect, response) =
                    ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());

                // Record the canvas rect (egui points) so the snapshot (#19) can
                // crop the framebuffer screenshot to just the image area.
                self.last_canvas_rect = Some(rect);

                self.handle_canvas_interaction(ui, rect, &response, tex_size);
                // Render Image
                let image_size = tex_size * self.scale;

                // Side-by-Side draws each image at its own offset position, so the
                // single centered overscan geometry below does not apply: skip the
                // overscan dimming pass and its annotations in that mode.
                let is_side_by_side = matches!(self.compare_mode, CompareMode::SideBySide);

                let disp_window = exr_data.image.attributes.display_window;
                let phys_idx = exr_data.logical_layers[self.active_layer].physical_index;
                let data_window_min = exr_data.image.layer_data[phys_idx]
                    .attributes
                    .layer_position;

                let disp_size =
                    egui::vec2(disp_window.size.x() as f32, disp_window.size.y() as f32)
                        * self.scale;
                let disp_rect = egui::Rect::from_min_size(
                    rect.center() + self.translation - disp_size / 2.0,
                    disp_size,
                );

                // Record what the snapshot should crop to (#52): the active image area
                // rather than the whole canvas (which includes the background).
                self.last_image_rect = Some(crate::snapshot::active_area_rect(
                    rect,
                    disp_rect,
                    is_side_by_side,
                ));

                let data_offset = egui::vec2(
                    (data_window_min.0 - disp_window.position.x()) as f32,
                    (data_window_min.1 - disp_window.position.y()) as f32,
                ) * self.scale;

                let image_rect = egui::Rect::from_min_size(disp_rect.min + data_offset, image_size);

                // Annotation drawing (#45) consumes the canvas drag/click when a tool
                // is active (pan is suppressed above). Coordinates map through
                // `image_rect`/`scale` so shapes anchor to image pixels.
                if self.anno_tool.is_active() {
                    self.handle_annotation_input(&response, image_rect, self.scale);
                }

                // The display-window overlays below paint unclipped; the draw paths
                // recompute their own display-clipped painter from `layout`.
                let unclipped_painter = ui.painter().with_clip_rect(rect);

                // Draw display window bounding box
                if !is_side_by_side {
                    draw_dashed_rect(
                        &unclipped_painter,
                        disp_rect,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 100),
                        5.0,
                        5.0,
                    );
                }

                // Labels for display window
                let is_overscanned = image_rect.min.x < disp_rect.min.x
                    || image_rect.min.y < disp_rect.min.y
                    || image_rect.max.x > disp_rect.max.x
                    || image_rect.max.y > disp_rect.max.y;
                let is_cropped = image_rect.min.x > disp_rect.min.x
                    || image_rect.min.y > disp_rect.min.y
                    || image_rect.max.x < disp_rect.max.x
                    || image_rect.max.y < disp_rect.max.y;

                if (is_overscanned || is_cropped) && !is_side_by_side {
                    unclipped_painter.text(
                        disp_rect.left_bottom() + egui::vec2(0.0, 5.0),
                        egui::Align2::LEFT_TOP,
                        format!("{},{}", disp_window.position.x(), disp_window.position.y()),
                        egui::FontId::proportional(12.0),
                        egui::Color32::GRAY,
                    );
                    let top_right_x = disp_window.position.x() + disp_window.size.x() as i32;
                    let top_right_y = disp_window.position.y() + disp_window.size.y() as i32;
                    unclipped_painter.text(
                        disp_rect.right_top() - egui::vec2(0.0, 5.0),
                        egui::Align2::RIGHT_BOTTOM,
                        format!("{top_right_x},{top_right_y}"),
                        egui::FontId::proportional(12.0),
                        egui::Color32::GRAY,
                    );
                }

                let layout = CanvasLayout {
                    rect,
                    disp_rect,
                    image_rect,
                    image_size,
                    tex_size,
                    tex_size_b,
                    is_side_by_side,
                };

                if let Some(gpu) = gpu_resources {
                    self.draw_canvas_gpu(ui, &layout, exr_data_b, gpu, lut_bg_opt);
                } else {
                    self.draw_canvas_cpu(ui, &layout, exr_data_b);
                }

                // Annotation overlay on top of the image (and its in-progress shape).
                // Painted by egui, so it is included in the snapshot screenshot (#19).
                let anno_painter = ui.painter().with_clip_rect(rect);
                self.draw_annotations(&anno_painter, image_rect, self.scale);
                self.annotation_text_popup(ui, image_rect, self.scale);

                // Draw data window bounding box over the image
                if (is_overscanned || is_cropped) && !is_side_by_side {
                    draw_dashed_rect(
                        &unclipped_painter,
                        image_rect,
                        egui::Color32::from_rgba_unmultiplied(255, 200, 100, 180),
                        4.0,
                        4.0,
                    );

                    unclipped_painter.text(
                        image_rect.right_bottom() + egui::vec2(5.0, 5.0),
                        egui::Align2::LEFT_TOP,
                        format!(
                            "Overscan: {}x{} (pos: {}, {})",
                            tex_size.x, tex_size.y, data_window_min.0, data_window_min.1
                        ),
                        egui::FontId::proportional(12.0),
                        egui::Color32::from_rgb(255, 200, 100),
                    );
                }

                self.handle_pixel_sampling(
                    ui, &response, exr_data, exr_data_b, rect, image_rect, image_size, tex_size,
                    tex_size_b,
                );
            }
        }
    }

    /// Hover/sample readout for the canvas: map the cursor to image pixel
    /// coordinates (handling the side-by-side split), sample A (and B) at that
    /// pixel, cache the last sample, show the value tooltip, and add a swatch on
    /// Shift+Click. Geometry (`rect`/`image_rect`/sizes) comes from the caller's
    /// layout so this stays purely about sampling.
    #[allow(clippy::too_many_arguments)]
    fn handle_pixel_sampling(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        exr_data: &ExrData,
        exr_data_b: Option<&ExrData>,
        rect: egui::Rect,
        image_rect: egui::Rect,
        image_size: egui::Vec2,
        tex_size: egui::Vec2,
        tex_size_b: Option<egui::Vec2>,
    ) {
        let mut hovered_pixel = None;
        if let Some(pos) = response.hover_pos() {
            let mut hover_x = None;
            let mut hover_y = None;
            let mut hovered_b = false;

            if self.compare_mode == CompareMode::SideBySide && exr_data_b.is_some() {
                // Gate on `tex_size_b` (geometry), NOT on `self.textures_b`
                // (the CPU texture cache): the GPU path populates
                // `gpu_textures_b` and leaves `textures_b` empty, so the old
                // `textures_b[...].as_ref().is_some()` gate silently skipped
                // the entire B-side hover/sampling branch on the GPU path.
                // `tex_size_b` is the actual prerequisite for the geometry math
                // below (it's unwrapped multiple times here).
                if let Some(tex_size_b) = tex_size_b {
                    let mut image_size_b = tex_size_b * self.scale;
                    if self.normalize_side_by_side {
                        let scale_b = (tex_size.y * self.scale) / tex_size_b.y;
                        image_size_b = tex_size_b * scale_b;
                    }
                    let combined_width = image_size.x + image_size_b.x;
                    let combined_height = image_size.y.max(image_size_b.y);

                    let combined_rect = egui::Rect::from_center_size(
                        rect.center() + self.translation,
                        egui::vec2(combined_width, combined_height),
                    );

                    let mut image_rect_a = egui::Rect::from_min_size(combined_rect.min, image_size);
                    image_rect_a.set_center(egui::pos2(
                        image_rect_a.center().x,
                        combined_rect.center().y,
                    ));

                    let mut image_rect_b = egui::Rect::from_min_size(
                        egui::pos2(combined_rect.min.x + image_size.x, combined_rect.min.y),
                        image_size_b,
                    );
                    image_rect_b.set_center(egui::pos2(
                        image_rect_b.center().x,
                        combined_rect.center().y,
                    ));

                    if image_rect_a.contains(pos) {
                        let local = pos - image_rect_a.min;
                        hover_x = Some((local.x / self.scale) as usize);
                        hover_y = Some((local.y / self.scale) as usize);
                    } else if image_rect_b.contains(pos) {
                        let local = pos - image_rect_b.min;
                        let scale_b = if self.normalize_side_by_side {
                            (tex_size.y * self.scale) / tex_size_b.y
                        } else {
                            self.scale
                        };
                        hover_x = Some((local.x / scale_b) as usize);
                        hover_y = Some((local.y / scale_b) as usize);
                        hovered_b = true;
                    }
                }
            } else {
                let image_local_pos = pos - image_rect.min;
                if image_local_pos.x >= 0.0 && image_local_pos.y >= 0.0 {
                    hover_x = Some((image_local_pos.x / self.scale) as usize);
                    hover_y = Some((image_local_pos.y / self.scale) as usize);
                }
            }
            let mut val_a_opt = None;
            let mut val_b_opt = None;
            let mut x_final = None;
            let mut y_final = None;

            if let (Some(x), Some(y)) = (hover_x, hover_y) {
                // Check if within bounds of the hovered image
                let mut valid = false;
                if hovered_b {
                    if let Some(s) = tex_size_b
                        && x < s.x as usize
                        && y < s.y as usize
                    {
                        valid = true;
                    }
                } else if x < tex_size.x as usize && y < tex_size.y as usize {
                    valid = true;
                }

                if valid {
                    hovered_pixel = Some((x, y));
                    x_final = Some(x);
                    y_final = Some(y);
                    val_a_opt = self.sample_pixel(exr_data, self.active_layer, x, y);
                    val_b_opt = if let Some(exr_b) = exr_data_b {
                        let layer_b = self
                            .active_layer
                            .min(exr_b.logical_layers.len().saturating_sub(1));
                        self.sample_pixel(exr_b, layer_b, x, y)
                    } else {
                        None
                    };

                    self.last_hover_pos_img = Some((x, y));
                    self.last_sampled_val_a = val_a_opt;
                    self.last_sampled_val_b = val_b_opt;
                }
            }

            if self.show_tooltip
                && (val_a_opt.is_some() || val_b_opt.is_some())
                && let (Some(x), Some(y)) = (x_final, y_final)
            {
                egui::Window::new("Pixel Tooltip")
                    .fixed_pos(pos + egui::vec2(15.0, 15.0))
                    .title_bar(false)
                    .resizable(false)
                    .collapsible(false)
                    .show(ui.ctx(), |ui| {
                        ui.label(format!("x={x} y={y}"));

                        if let Some(val_a) = val_a_opt {
                            ui.horizontal(|ui| {
                                colored_rgba_label(
                                    ui,
                                    if val_b_opt.is_some() { "A:" } else { "" },
                                    val_a,
                                );
                                let (r, g, b) = (
                                    (val_a[0].clamp(0.0, 1.0) * 255.0) as u8,
                                    (val_a[1].clamp(0.0, 1.0) * 255.0) as u8,
                                    (val_a[2].clamp(0.0, 1.0) * 255.0) as u8,
                                );
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(16.0, 16.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    0.0,
                                    egui::Color32::from_rgb(r, g, b),
                                );
                            });
                            let (h, s, v, l) = rgb_to_hsvl(val_a[0], val_a[1], val_a[2]);
                            ui.label(
                                egui::RichText::new(format!("H:{h:.0} S:{s:.2} V:{v:.2} L:{l:.5}"))
                                    .color(egui::Color32::LIGHT_GRAY),
                            );
                        }

                        if let Some(val_b) = val_b_opt {
                            ui.horizontal(|ui| {
                                colored_rgba_label(ui, "B:", val_b);
                                let (r, g, b) = (
                                    (val_b[0].clamp(0.0, 1.0) * 255.0) as u8,
                                    (val_b[1].clamp(0.0, 1.0) * 255.0) as u8,
                                    (val_b[2].clamp(0.0, 1.0) * 255.0) as u8,
                                );
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(16.0, 16.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    0.0,
                                    egui::Color32::from_rgb(r, g, b),
                                );
                            });
                            let (h, s, v, l) = rgb_to_hsvl(val_b[0], val_b[1], val_b[2]);
                            ui.label(
                                egui::RichText::new(format!("H:{h:.0} S:{s:.2} V:{v:.2} L:{l:.5}"))
                                    .color(egui::Color32::LIGHT_GRAY),
                            );
                        }

                        if let (Some(val_a), Some(val_b)) = (val_a_opt, val_b_opt) {
                            let diff = [
                                (val_b[0] - val_a[0]).abs(),
                                (val_b[1] - val_a[1]).abs(),
                                (val_b[2] - val_a[2]).abs(),
                                (val_b[3] - val_a[3]).abs(),
                            ];
                            colored_rgba_label(ui, "Diff:", diff);
                        }
                    });

                // Shift+Click to add a persistent swatch
                if ui.input(|i| i.modifiers.shift)
                    && response.clicked()
                    && let Some(v) = val_a_opt.or(val_b_opt)
                {
                    self.swatches.push(v);
                }
            }
        }

        if hovered_pixel.is_none() {
            self.last_hover_pos_img = None;
            self.last_sampled_val_a = None;
            self.last_sampled_val_b = None;
        }
    }

    /// CPU fallback render path (no GPU `render_state`): paint the already-baked
    /// `egui::TextureHandle`s for the active compare mode. Mirrors the GPU path's
    /// layout, minus the in-shader effects (those are pre-applied when the CPU
    /// textures are generated).
    fn draw_canvas_cpu(&self, ui: &egui::Ui, layout: &CanvasLayout, exr_data_b: Option<&ExrData>) {
        let CanvasLayout {
            rect,
            disp_rect,
            image_rect,
            image_size,
            tex_size,
            tex_size_b,
            is_side_by_side,
        } = *layout;
        let unclipped_painter = ui.painter().with_clip_rect(rect);
        let painter = ui.painter().with_clip_rect(rect.intersect(disp_rect));

        // Defense-in-depth: the active-layer CPU texture is normally guaranteed
        // present by `has_texture` in `ExrViewer::ui()`, but that invariant crosses
        // a function-call boundary, so bail out cleanly rather than panicking here.
        let Some(texture) = self.textures[self.active_layer].as_ref() else {
            return;
        };
        let draw_image = |painter: &egui::Painter,
                          tex: &egui::TextureHandle,
                          clip_rect: egui::Rect,
                          target_rect: egui::Rect,
                          opacity: f32| {
            let alpha = opacity;
            let final_clip_rect = painter.clip_rect().intersect(clip_rect);
            painter.with_clip_rect(final_clip_rect).image(
                tex.id(),
                target_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::from_white_alpha((alpha * 255.0) as u8),
            );
        };

        let draw_all_cpu = |p: &egui::Painter, opac: f32| match self.compare_mode {
            CompareMode::SingleA => {
                draw_image(p, texture, rect, image_rect, opac);
            }
            CompareMode::SingleB => {
                if let Some(tex_b) = exr_data_b.and_then(|d| {
                    self.textures_b[self
                        .active_layer
                        .min(d.logical_layers.len().saturating_sub(1))]
                    .as_ref()
                }) {
                    draw_image(p, tex_b, rect, image_rect, opac);
                }
            }
            CompareMode::Wipe => {
                // CPU fallback: keep it vertical for simplicity, but use new center state
                let wipe_x = image_rect.min.x + image_rect.width() * self.wipe_center[0];
                let clamped_wipe_x = wipe_x.clamp(rect.min.x, rect.max.x);
                let mut rect_a = rect;
                rect_a.max.x = clamped_wipe_x;
                let mut rect_b = rect;
                rect_b.min.x = clamped_wipe_x;

                draw_image(p, texture, rect_a, image_rect, opac);
                if let Some(tex_b) = exr_data_b.and_then(|d| {
                    self.textures_b[self
                        .active_layer
                        .min(d.logical_layers.len().saturating_sub(1))]
                    .as_ref()
                }) {
                    draw_image(p, tex_b, rect_b, image_rect, opac);
                }

                let alpha = (self.wipe_line_opacity * 255.0) as u8;
                let color = egui::Color32::from_white_alpha(alpha);
                p.line_segment(
                    [
                        egui::pos2(wipe_x, rect.min.y),
                        egui::pos2(wipe_x, rect.max.y),
                    ],
                    (2.0, color),
                );
            }
            CompareMode::SideBySide => {
                let tex_b_opt = exr_data_b.and_then(|d| {
                    self.textures_b[self
                        .active_layer
                        .min(d.logical_layers.len().saturating_sub(1))]
                    .as_ref()
                });
                if let (Some(tex_b), Some(size_b)) = (tex_b_opt, tex_size_b) {
                    let mut image_size_b = size_b * self.scale;
                    if self.normalize_side_by_side {
                        let scale_b = (tex_size.y * self.scale) / size_b.y;
                        image_size_b = size_b * scale_b;
                    }
                    let combined_width = image_size.x + image_size_b.x;
                    let combined_height = image_size.y.max(image_size_b.y);

                    let combined_rect = egui::Rect::from_center_size(
                        rect.center() + self.translation,
                        egui::vec2(combined_width, combined_height),
                    );

                    let mut image_rect_a = egui::Rect::from_min_size(combined_rect.min, image_size);
                    image_rect_a.set_center(egui::pos2(
                        image_rect_a.center().x,
                        combined_rect.center().y,
                    ));

                    let mut image_rect_b = egui::Rect::from_min_size(
                        egui::pos2(combined_rect.min.x + image_size.x, combined_rect.min.y),
                        image_size_b,
                    );
                    image_rect_b.set_center(egui::pos2(
                        image_rect_b.center().x,
                        combined_rect.center().y,
                    ));

                    draw_image(p, texture, rect, image_rect_a, opac);
                    draw_image(p, tex_b, rect, image_rect_b, opac);

                    p.line_segment(
                        [
                            egui::pos2(image_rect_b.min.x, combined_rect.min.y),
                            egui::pos2(image_rect_b.min.x, combined_rect.max.y),
                        ],
                        (2.0, egui::Color32::GRAY),
                    );
                } else {
                    draw_image(p, texture, rect, image_rect, opac);
                }
            }
            CompareMode::DiffMatte => {
                if let Some(diff) = &self.diff_texture {
                    draw_image(p, diff, rect, image_rect, opac);
                }
            }
            CompareMode::Composite => {
                if let Some(comp) = &self.composite_texture {
                    draw_image(p, comp, rect, image_rect, opac);
                }
            }
        };

        if self.overscan_opacity > 0.0 && !is_side_by_side {
            draw_all_cpu(&unclipped_painter, self.overscan_opacity);
        }
        // Side-by-Side renders at full brightness with the full-canvas clip.
        draw_all_cpu(
            if is_side_by_side {
                &unclipped_painter
            } else {
                &painter
            },
            1.0,
        );
    }

    /// GPU render path (the default): build per-draw uniforms and emit wgpu
    /// paint callbacks for the active compare mode. Under OCIO the per-image
    /// pass-1 draws are accumulated into a single display-transform pass. Also
    /// handles the wipe-handle drag/scroll interaction (hence `&mut self`).
    fn draw_canvas_gpu(
        &mut self,
        ui: &egui::Ui,
        layout: &CanvasLayout,
        exr_data_b: Option<&ExrData>,
        gpu_resources: &crate::gpu::GpuResources,
        lut_bg_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        let render_state = gpu_resources.render_state();
        let CanvasLayout {
            rect,
            disp_rect,
            image_rect,
            image_size,
            tex_size,
            tex_size_b,
            ..
        } = *layout;
        let unclipped_painter = ui.painter().with_clip_rect(rect);
        let painter = ui.painter().with_clip_rect(rect.intersect(disp_rect));

        // Re-bake + upload the diff colormap and background gradient LUTs only when
        // their ramps change. The GPU textures are updated in place (stable
        // handles), so no bind-group rebuild is needed — see `GpuState`.
        let colormap_dirty = self.colormap_sig.as_ref() != Some(&self.diff_colormap);
        let bg_gradient_dirty = self.bg_gradient_sig.as_ref() != Some(&self.background.gradient);
        if colormap_dirty {
            self.colormap_lut = self
                .diff_colormap
                .gradient()
                .bake(crate::gradient::COLORMAP_LUT_SIZE);
            self.colormap_sig = Some(self.diff_colormap.clone());
        }
        if bg_gradient_dirty {
            self.bg_gradient_lut = self
                .background
                .gradient
                .bake(crate::gradient::COLORMAP_LUT_SIZE);
            self.bg_gradient_sig = Some(self.background.gradient.clone());
        }
        if colormap_dirty || bg_gradient_dirty {
            // GpuState is app-owned (#54) — read it directly off
            // `GpuResources` instead of the per-frame renderer typemap lookup.
            let gpu_state = gpu_resources.gpu_state.as_ref();
            if colormap_dirty {
                gpu_state.write_colormap(&render_state.queue, &self.colormap_lut);
            }
            if bg_gradient_dirty {
                gpu_state.write_bg_gradient(&render_state.queue, &self.bg_gradient_lut);
            }
        }

        // GPU RENDER PATH
        let uniform_data = crate::gpu::Uniforms {
            rect_min: [image_rect.min.x, image_rect.min.y],
            rect_max: [image_rect.max.x, image_rect.max.y],
            screen_size: [
                ui.ctx().content_rect().width(),
                ui.ctx().content_rect().height(),
            ],
            exposure: self.exposure,
            gamma: self.gamma,
            diff_multiplier: self.diff_multiplier,
            channel_mode: self.channel_mode.as_u32(),
            is_diff_mode: 0,
            srgb: if self.srgb { 1 } else { 0 },
            enable_lut: if self.enable_lut { 1 } else { 0 },
            opacity: 1.0,
            is_composite: 0,
            blend_mode: self.blend_mode.as_u32(),
            is_wipe_mode: if self.compare_mode == CompareMode::Wipe {
                1
            } else {
                0
            },
            wipe_center: self.wipe_center,
            wipe_angle: self.wipe_angle.to_radians(),
            skip_checker: 0,
            diff_metric: self.diff_metric.as_u32(),
            diff_floor: self.diff_floor,
            _pad2: 0,
            lut_domain_min: self.lut_domain_min,
            lut_domain_max: self.lut_domain_max,
            bg_checker_dark: rgb3_to_vec4(self.background.checker_dark),
            bg_checker_light: rgb3_to_vec4(self.background.checker_light),
            bg_solid: rgb3_to_vec4(self.background.solid),
            bg_mode: self.background.mode.as_u32(),
            bg_grad_angle: self.background.gradient_angle,
            bg_checker_size: self.background.checker_size,
            _pad3: 0,
        };

        // Acquire the persistent uniform ring buffer + the active LUT bind group
        // from the app-owned `GpuState` (#54). No per-frame renderer typemap
        // lookup: `GpuState` lives on `GpuResources`. `draw_gpu` writes per-draw
        // uniform data into the ring buffer via `queue.write_buffer` at a
        // dynamic offset — no per-frame `create_buffer_init` + `create_bind_group`
        // allocation. The bind group itself lives in `GpuState` and is fetched by
        // the paint callbacks via `callback_resources`.
        let gpu_state = gpu_resources.gpu_state.as_ref();
        let (uniform_buffer, uniform_stride, active_lut_bg, default_tex_bg) = {
            (
                gpu_state.uniform_buffer.clone(),
                gpu_state.uniform_stride,
                lut_bg_opt.unwrap_or_else(|| gpu_state.default_lut_bind_group.clone()),
                gpu_state.default_tex_bind_group.clone(),
            )
        };
        // Per-frame ring allocator: bumped by each `draw_gpu` call. Up to ~4
        // draws per frame fit well within the 16-slot ring (2 KB total).
        let uniform_offset = std::cell::Cell::new(0u32);
        #[cfg(feature = "ocio")]
        let ocio_active = self.ocio_active;
        // Under OCIO, draw_gpu accumulates pass-1 draws here instead of emitting a
        // callback per call; a single OcioCallback covering the whole frame (both
        // side-by-side images included) is emitted after draw_all.
        #[cfg(feature = "ocio")]
        let ocio_draws: std::cell::RefCell<Vec<crate::gpu::ocio_pass::OcioPass1Draw>> =
            std::cell::RefCell::new(Vec::new());
        // Running FNV-1a hash of everything that affects the OCIO render (uniforms +
        // texture identities) so the (expensive) display transform is skipped on
        // repaints that change nothing — hover, menus, animations.
        #[cfg(feature = "ocio")]
        let ocio_sig = std::cell::Cell::new(0xcbf29ce484222325u64);
        let draw_gpu = |painter: &egui::Painter,
                        bg_a: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
                        bg_b_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
                        clip_rect: egui::Rect,
                        target_rect: egui::Rect,
                        is_diff: bool,
                        is_composite: bool,
                        opacity: f32| {
            let mut u = uniform_data;
            u.rect_min = [target_rect.min.x, target_rect.min.y];
            u.rect_max = [target_rect.max.x, target_rect.max.y];
            u.is_diff_mode = if is_diff { 1 } else { 0 };
            u.is_composite = if is_composite { 1 } else { 0 };
            u.opacity = opacity;

            // OCIO path: pass 1 must emit scene-linear, so bypass the built-in
            // display chain (sRGB/gamma/.cube LUT). Exposure stays (linear).
            #[cfg(feature = "ocio")]
            if ocio_active {
                u.srgb = 0;
                u.gamma = 1.0;
                u.enable_lut = 0;
                // Don't bake the checker into scene-linear; it's composited
                // in display space (blit pass) after the OCIO transform.
                u.skip_checker = 1;
            }

            let queue = &render_state.queue;

            // Write this draw's uniform data into the persistent ring buffer
            // at the current offset, then bump the allocator. This replaces
            // the per-draw `create_buffer_init` + `create_bind_group` (two
            // wgpu object allocations + a staging copy per draw per frame).
            // `uniform_stride` is padded to the device's
            // `min_uniform_buffer_offset_alignment` (typically 256), so every
            // dynamic offset is valid — the raw Uniforms struct (128 bytes)
            // is written at the start of each padded slot.
            let offset = uniform_offset.get();
            uniform_offset.set(offset + uniform_stride);
            debug_assert!(
                offset + uniform_stride <= crate::gpu::UNIFORM_RING_SLOTS as u32 * uniform_stride,
                "uniform ring buffer overflow: too many draws this frame"
            );
            queue.write_buffer(&uniform_buffer, offset as u64, bytemuck::bytes_of(&u));

            let bg_b = bg_b_opt.unwrap_or_else(|| default_tex_bg.clone());
            let final_clip_rect = painter.clip_rect().intersect(clip_rect);

            // Diff is a false-color heat-map visualization (display-space,
            // not color-managed), so it always uses the normal pipeline —
            // even under OCIO it is NOT accumulated into the OCIO pass.
            #[cfg(feature = "ocio")]
            if ocio_active && !is_diff {
                // Fold this draw's inputs (uniform bytes + texture pointers) into
                // the per-frame render signature; OcioCallback re-renders only
                // when this changes.
                let mut h = ocio_sig.get();
                for chunk in bytemuck::bytes_of(&u).chunks(8) {
                    let mut b = [0u8; 8];
                    b[..chunk.len()].copy_from_slice(chunk);
                    h = (h ^ u64::from_le_bytes(b)).wrapping_mul(0x100000001b3);
                }
                for p in [
                    std::sync::Arc::as_ptr(&bg_a) as *const () as u64,
                    std::sync::Arc::as_ptr(&bg_b) as *const () as u64,
                    std::sync::Arc::as_ptr(&active_lut_bg) as *const () as u64,
                ] {
                    h = (h ^ p).wrapping_mul(0x100000001b3);
                }
                ocio_sig.set(h);

                // Accumulate; the single per-frame OcioCallback is emitted
                // after draw_all so one OCIO pass covers the whole frame.
                ocio_draws
                    .borrow_mut()
                    .push(crate::gpu::ocio_pass::OcioPass1Draw {
                        bg_a,
                        bg_b,
                        uniform_offset: offset,
                        lut_bg: active_lut_bg.clone(),
                    });
                return;
            }

            let callback = crate::gpu::ExrCallback {
                bg_a,
                bg_b,
                uniform_offset: offset,
                lut_bg: active_lut_bg.clone(),
            };
            painter.with_clip_rect(final_clip_rect).add(
                eframe::egui_wgpu::Callback::new_paint_callback(final_clip_rect, callback),
            );
        };

        let bg_a_opt = self.gpu_textures[self.active_layer].clone();
        if let Some(bg_a) = bg_a_opt {
            let comp_mode = if self.blink_state {
                if ((ui.input(|i| i.time) / self.blink_interval as f64) as usize).is_multiple_of(2)
                {
                    CompareMode::SingleA
                } else {
                    CompareMode::SingleB
                }
            } else {
                self.compare_mode
            };

            // Wipe interaction logic (drag to move, scroll to rotate)
            if self.compare_mode == CompareMode::Wipe {
                let center_screen = egui::pos2(
                    image_rect.min.x + image_rect.width() * self.wipe_center[0],
                    image_rect.min.y + image_rect.height() * self.wipe_center[1],
                );
                let handle_rect =
                    egui::Rect::from_center_size(center_screen, egui::vec2(24.0, 24.0));
                let handle_id = ui.id().with("wipe_handle");
                let response = ui.interact(handle_rect, handle_id, egui::Sense::drag());

                if response.dragged() {
                    let delta = response.drag_delta();
                    self.wipe_center[0] =
                        (self.wipe_center[0] + delta.x / image_rect.width()).clamp(0.0, 1.0);
                    self.wipe_center[1] =
                        (self.wipe_center[1] + delta.y / image_rect.height()).clamp(0.0, 1.0);
                }
                if response.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        self.wipe_angle = (self.wipe_angle + scroll * 2.0).clamp(-180.0, 180.0);
                    }
                }
            }

            let draw_all = |p: &egui::Painter, opac: f32| match comp_mode {
                CompareMode::SingleA => {
                    draw_gpu(p, bg_a.clone(), None, rect, image_rect, false, false, opac);
                }
                CompareMode::SingleB => {
                    if let Some(bg_b) = exr_data_b.and_then(|d| {
                        self.gpu_textures_b[self
                            .active_layer
                            .min(d.logical_layers.len().saturating_sub(1))]
                        .clone()
                    }) {
                        draw_gpu(p, bg_b, None, rect, image_rect, false, false, opac);
                    }
                }
                CompareMode::Wipe => {
                    let bg_b_opt = exr_data_b.and_then(|d| {
                        self.gpu_textures_b[self
                            .active_layer
                            .min(d.logical_layers.len().saturating_sub(1))]
                        .clone()
                    });
                    // Single draw call: the shader handles the wipe split.
                    // Bind the real B texture so the shader can sample it when
                    // is_wipe_mode is set; falls back to the default texture if
                    // no B image is loaded.
                    draw_gpu(
                        p,
                        bg_a.clone(),
                        bg_b_opt,
                        rect,
                        image_rect,
                        false,
                        false,
                        opac,
                    );

                    // Draw the rotated wipe line and handle
                    let center_screen = egui::pos2(
                        image_rect.min.x + image_rect.width() * self.wipe_center[0],
                        image_rect.min.y + image_rect.height() * self.wipe_center[1],
                    );
                    let angle_rad = self.wipe_angle.to_radians();
                    // Line direction is perpendicular to the normal (cos, sin)
                    let dir = egui::vec2(-angle_rad.sin(), angle_rad.cos());
                    let max_dist = image_rect.width().hypot(image_rect.height());
                    let p1 = center_screen + dir * max_dist;
                    let p2 = center_screen - dir * max_dist;

                    let alpha = (self.wipe_line_opacity * 255.0) as u8;
                    let color = egui::Color32::from_white_alpha(alpha);

                    p.line_segment([p1, p2], (2.0, color));
                    p.circle_filled(center_screen, 8.0, color);
                }
                CompareMode::SideBySide => {
                    let bg_b_opt = exr_data_b.and_then(|d| {
                        self.gpu_textures_b[self
                            .active_layer
                            .min(d.logical_layers.len().saturating_sub(1))]
                        .clone()
                    });
                    if let (Some(bg_b), Some(size_b)) = (bg_b_opt, tex_size_b) {
                        let mut image_size_b = size_b * self.scale;
                        if self.normalize_side_by_side {
                            let scale_b = (tex_size.y * self.scale) / size_b.y;
                            image_size_b = size_b * scale_b;
                        }
                        let combined_width = image_size.x + image_size_b.x;
                        let combined_height = image_size.y.max(image_size_b.y);
                        let combined_rect = egui::Rect::from_center_size(
                            rect.center() + self.translation,
                            egui::vec2(combined_width, combined_height),
                        );
                        let mut image_rect_a =
                            egui::Rect::from_min_size(combined_rect.min, image_size);
                        image_rect_a.set_center(egui::pos2(
                            image_rect_a.center().x,
                            combined_rect.center().y,
                        ));
                        let mut image_rect_b = egui::Rect::from_min_size(
                            egui::pos2(combined_rect.min.x + image_size.x, combined_rect.min.y),
                            image_size_b,
                        );
                        image_rect_b.set_center(egui::pos2(
                            image_rect_b.center().x,
                            combined_rect.center().y,
                        ));

                        draw_gpu(
                            p,
                            bg_a.clone(),
                            None,
                            rect,
                            image_rect_a,
                            false,
                            false,
                            opac,
                        );
                        draw_gpu(p, bg_b, None, rect, image_rect_b, false, false, opac);
                        p.line_segment(
                            [
                                egui::pos2(image_rect_b.min.x, combined_rect.min.y),
                                egui::pos2(image_rect_b.min.x, combined_rect.max.y),
                            ],
                            (2.0, egui::Color32::GRAY),
                        );
                    } else {
                        draw_gpu(p, bg_a.clone(), None, rect, image_rect, false, false, opac);
                    }
                }
                CompareMode::DiffMatte => {
                    let bg_b_opt = exr_data_b.and_then(|d| {
                        self.gpu_textures_b[self
                            .active_layer
                            .min(d.logical_layers.len().saturating_sub(1))]
                        .clone()
                    });
                    if let Some(bg_b) = bg_b_opt {
                        draw_gpu(
                            p,
                            bg_a.clone(),
                            Some(bg_b),
                            rect,
                            image_rect,
                            true,
                            false,
                            opac,
                        );
                    }
                }
                CompareMode::Composite => {
                    let bg_b_opt = exr_data_b.and_then(|d| {
                        self.gpu_textures_b[self
                            .active_layer
                            .min(d.logical_layers.len().saturating_sub(1))]
                        .clone()
                    });
                    if let Some(bg_b) = bg_b_opt {
                        draw_gpu(
                            p,
                            bg_a.clone(),
                            Some(bg_b),
                            rect,
                            image_rect,
                            false,
                            true,
                            opac,
                        );
                    }
                }
            };

            let is_sbs = matches!(comp_mode, CompareMode::SideBySide);

            // OCIO path: one pass over the whole frame. Accumulate the pass-1
            // draws (draw_gpu pushes into ocio_draws) and emit a single
            // OcioCallback. The checker + overscan dim are applied post-OCIO in
            // the blit, so there is no separate dim draw here. Diff opts out: it
            // renders a display-space heat map via the normal pipeline (see
            // draw_gpu), so OCIO never runs for it.
            #[cfg(feature = "ocio")]
            let ocio_handled = if self.ocio_active && !matches!(comp_mode, CompareMode::DiffMatte) {
                // Overscan is dimmed in the blit (when opacity > 0); when opacity
                // is 0 we hide it by clipping the callback to the display window.
                let overscan_dim = !is_sbs && self.overscan_opacity > 0.0;
                let slot_painter = if !is_sbs && self.overscan_opacity == 0.0 {
                    &painter
                } else {
                    &unclipped_painter
                };
                // Reserve the image slot BEFORE annotations so the image renders
                // beneath the wipe/SBS lines (same layer, insertion order).
                let slot = slot_painter.add(egui::Shape::Noop);
                let cb_clip = slot_painter.clip_rect();

                draw_all(&unclipped_painter, 1.0);

                let draws = std::mem::take(&mut *ocio_draws.borrow_mut());
                if !draws.is_empty() {
                    let display_format = render_state.target_format;
                    let content = ui.ctx().content_rect();
                    let blit_uniforms = crate::gpu::BlitUniforms {
                        display_min: [disp_rect.min.x, disp_rect.min.y],
                        display_max: [disp_rect.max.x, disp_rect.max.y],
                        screen_size: [content.width(), content.height()],
                        overscan_factor: if overscan_dim {
                            self.overscan_opacity
                        } else {
                            1.0
                        },
                        bg_mode: self.background.mode.as_u32() as f32,
                        bg_checker_size: self.background.checker_size,
                        bg_grad_angle: self.background.gradient_angle,
                        // Re-apply the user gamma in display space (#93): under
                        // OCIO the main shader runs with gamma=1 (OCIO owns the
                        // display chain), so the control would otherwise be inert.
                        gamma: self.gamma,
                        _pad_b: 0.0,
                        bg_checker_dark: rgb3_to_vec4(self.background.checker_dark),
                        bg_checker_light: rgb3_to_vec4(self.background.checker_light),
                        bg_solid: rgb3_to_vec4(self.background.solid),
                    };
                    // Finalize the render signature with the OCIO config/view
                    // identity (its CPU processor is rebuilt on any config change),
                    // so changing the display/view forces a re-render.
                    let mut render_sig = ocio_sig.get();
                    if let Some(p) = &self.ocio_cpu {
                        render_sig = (render_sig ^ (std::rc::Rc::as_ptr(p) as *const () as u64))
                            .wrapping_mul(0x100000001b3);
                    }
                    // Scissor the OCIO transform to the visible image so it skips
                    // the empty background. Side-by-side spans the canvas with two
                    // images, so it opts out (None = whole target).
                    let scissor_pts = if is_sbs {
                        None
                    } else {
                        Some([
                            image_rect.min.x,
                            image_rect.min.y,
                            image_rect.max.x,
                            image_rect.max.y,
                        ])
                    };
                    let callback = crate::gpu::ocio_pass::OcioCallback {
                        draws,
                        display_format,
                        blit_uniforms,
                        scissor_pts,
                        render_sig,
                    };
                    slot_painter.set(
                        slot,
                        eframe::egui_wgpu::Callback::new_paint_callback(cb_clip, callback),
                    );
                }
                true
            } else {
                false
            };
            #[cfg(not(feature = "ocio"))]
            let ocio_handled = false;

            if !ocio_handled {
                if self.overscan_opacity > 0.0 && !is_sbs {
                    draw_all(&unclipped_painter, self.overscan_opacity);
                }
                // Side-by-Side renders at full brightness with the full-canvas
                // clip (no display-window clip), so overscan dimming is skipped.
                draw_all(if is_sbs { &unclipped_painter } else { &painter }, 1.0);
            }
        }
    }

    /// The real render path: upload `layer_index`'s RGBA into a GPU bind group.
    /// The shader applies channel isolation, exposure, gamma, sRGB and every
    /// compare mode, so this one generator serves all modes; results are cached
    /// per layer in `gpu_textures` / `gpu_textures_b`. See the module-level docs.
    /// Build a GPU texture + bind group for one layer of an `ExrData`, returning
    /// the [`T2Texture`] (which keeps the `Texture` handle so it can be explicitly
    /// destroyed on eviction). UI-thread only (`queue.write_texture`).
    fn build_layer_texture(
        gpu_resources: &crate::gpu::GpuResources,
        exr_data: &ExrData,
        layer_index: usize,
    ) -> Option<T2Texture> {
        let render_state = gpu_resources.render_state();
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        // Pack into Rgba32Float
        let mut pixels = vec![0.0f32; width * height * 4];

        // Hoist the FlatSamples enum match out of the pixel loop: extract F32
        // slices (the common case) for direct indexing. Non-F32 channels
        // (rare: F16/U32) fall back to sample_channel per pixel.
        let r_s = sample_channel_f32(r_chan);
        let g_s = sample_channel_f32(g_chan);
        let b_s = sample_channel_f32(b_chan);
        let a_s = sample_channel_f32(a_chan);
        let has_alpha = a_chan.is_some();

        // Pack rows in parallel (mirrors the CPU fallback's par_chunks_mut
        // pattern). For a 4K layer this is ~8M iterations with 4 channel
        // reads each — single-threaded was a noticeable stall on layer switch.
        pixels
            .par_chunks_mut(width * 4)
            .enumerate()
            .for_each(|(y, row)| {
                for x in 0..width {
                    let i = x * 4;
                    row[i] = pixel_val(r_s, r_chan, x, y, width);
                    row[i + 1] = pixel_val(g_s, g_chan, x, y, width);
                    row[i + 2] = pixel_val(b_s, b_chan, x, y, width);
                    row[i + 3] = if has_alpha {
                        pixel_val(a_s, a_chan, x, y, width)
                    } else {
                        1.0
                    };
                }
            });

        let device = &render_state.device;
        let queue = &render_state.queue;

        let texture = device.create_texture(&eframe::egui_wgpu::wgpu::TextureDescriptor {
            label: Some("Exr GPU Texture"),
            size: eframe::egui_wgpu::wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: eframe::egui_wgpu::wgpu::TextureDimension::D2,
            format: eframe::egui_wgpu::wgpu::TextureFormat::Rgba32Float,
            usage: eframe::egui_wgpu::wgpu::TextureUsages::TEXTURE_BINDING
                | eframe::egui_wgpu::wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            eframe::egui_wgpu::wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: eframe::egui_wgpu::wgpu::Origin3d::ZERO,
                aspect: eframe::egui_wgpu::wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&pixels),
            eframe::egui_wgpu::wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((width * 4 * 4) as u32),
                rows_per_image: Some(height as u32),
            },
            eframe::egui_wgpu::wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&eframe::egui_wgpu::wgpu::TextureViewDescriptor::default());

        // GpuState is app-owned (#54) — read it directly off `GpuResources`
        // instead of the renderer typemap lookup.
        let gpu_state = gpu_resources.gpu_state.as_ref();

        let bind_group = device.create_bind_group(&eframe::egui_wgpu::wgpu::BindGroupDescriptor {
            label: Some("Exr Texture Bind Group"),
            layout: &gpu_state.bind_group_layout_tex,
            entries: &[
                eframe::egui_wgpu::wgpu::BindGroupEntry {
                    binding: 0,
                    resource: eframe::egui_wgpu::wgpu::BindingResource::TextureView(&view),
                },
                eframe::egui_wgpu::wgpu::BindGroupEntry {
                    binding: 1,
                    resource: eframe::egui_wgpu::wgpu::BindingResource::Sampler(&gpu_state.sampler),
                },
            ],
        });

        Some(T2Texture {
            texture,
            bind_group: std::sync::Arc::new(bind_group),
        })
    }

    // --- T2 GPU-texture ring (#56) -------------------------------------------

    /// Set the VRAM-budgeted T2 capacity (frames). `0` disables pre-upload and
    /// drops the ring → the lazy per-swap path. Shrinking evicts immediately.
    pub(crate) fn set_t2_cap(&mut self, cap: usize) {
        self.t2_cap = cap;
        if cap == 0 {
            self.clear_t2();
        } else {
            self.evict_t2();
        }
    }

    /// Tell the viewer which sequence frame is on screen, so `ui()` binds its T2
    /// texture. `None` for a single image (lazy path).
    pub(crate) fn set_t2_frame(&mut self, frame: Option<u32>) {
        self.t2_frame = frame;
    }

    /// Current T2 capacity in frames (`0` = disabled).
    pub(crate) fn t2_cap(&self) -> usize {
        self.t2_cap
    }

    /// Pre-build the T2 texture for `(frame, active layer)` and ring it, evicting
    /// to the cap. Returns `true` if it actually built (so the caller can amortize
    /// uploads across frames). No-op — returns `false` — when disabled, already
    /// resident, or the build fails. UI-thread only. Pass frames already resident
    /// in the T1 cache; T2 never triggers a decode.
    pub(crate) fn prebuild_t2(
        &mut self,
        gpu: &crate::gpu::GpuResources,
        exr_data: &ExrData,
        frame: u32,
    ) -> bool {
        if self.t2_cap == 0 {
            return false;
        }
        self.ensure_t2_layer();
        if self.t2_ring.contains_key(&frame) {
            return false;
        }
        let Some(t2) = Self::build_layer_texture(gpu, exr_data, self.active_layer) else {
            return false;
        };
        self.t2_ring.insert(frame, t2);
        self.evict_t2();
        true
    }

    /// Drop the whole ring when the active layer changes (textures are per-layer).
    fn ensure_t2_layer(&mut self) {
        if self.t2_layer != self.active_layer {
            self.clear_t2();
            self.t2_layer = self.active_layer;
        }
    }

    /// Evict T2 frames furthest from the on-screen frame until within the cap by
    /// **dropping** their handles; wgpu reclaims the VRAM once the texture has no
    /// live reference. The on-screen frame is never chosen (its bind group is
    /// bound for paint). We never call `Texture::destroy()` — see [`T2Texture`]
    /// for why a synchronous destroy aborts the process on Vulkan.
    fn evict_t2(&mut self) {
        let cap = self.t2_cap.max(1);
        while self.t2_ring.len() > cap {
            let Some(victim) = t2_victim(self.t2_ring.keys().copied(), self.t2_frame) else {
                break; // only the on-screen frame remains
            };
            // Drop the handle; wgpu frees the texture when no view is still bound.
            self.t2_ring.remove(&victim);
        }
    }

    /// Drop a single frame's T2 texture, if present. Used by the render-watch
    /// (#101) so a re-rendered frame's stale GPU texture is released and rebuilt
    /// from the fresh decode. Drop-only (no `destroy()`): if this frame is the one
    /// on screen, the bound bind group keeps the old texture alive until the next
    /// paint rebinds the fresh one — no in-flight draw is ever invalidated.
    pub(crate) fn evict_t2_frame(&mut self, frame: u32) {
        self.t2_ring.remove(&frame);
    }

    /// Drop every T2 texture (new sequence / disabled / layer switch). Drop-only:
    /// the on-screen frame's texture stays alive through its still-bound bind
    /// group (cloned into `gpu_textures[active_layer]`) and is freed by wgpu once
    /// that binding is replaced — critically, this clear can run *before* the
    /// central panel rebinds for the just-advanced frame, so the bound frame may
    /// differ from `t2_frame`; dropping is safe for either, a `destroy()` is not.
    pub(crate) fn clear_t2(&mut self) {
        self.t2_ring.clear();
    }

    /// CPU/thumbnail path: bake `layer_index` into an [`egui::TextureHandle`]
    /// with the full channel-select → exposure → gamma → sRGB tone pipeline.
    /// Used for contact-sheet thumbnails and as the fallback when no GPU
    /// `render_state` is available. Dispatches to `generate_texture_ocio`
    /// when an OCIO CPU processor is active.
    fn generate_texture(
        &self,
        ctx: &egui::Context,
        exr_data: &ExrData,
        layer_index: usize,
        max_dim: Option<usize>,
    ) -> Option<egui::TextureHandle> {
        #[cfg(feature = "ocio")]
        if self.ocio_active
            && let Some(proc) = &self.ocio_cpu
        {
            return self.generate_texture_ocio(ctx, exr_data, layer_index, proc, max_dim);
        }

        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;
        // Decimate to the thumbnail box when baking a contact-sheet cell; full-res
        // (stride 1) for the CPU-display fallback. See [`thumb_dims`].
        let (out_w, out_h, stride) = thumb_dims(width, height, max_dim);

        let mut pixels = vec![egui::Color32::BLACK; out_w * out_h];

        // Hoist all loop-invariant scalars out of the per-pixel work.
        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        // Viewport background (issue #18): one config, sampled per pixel below so
        // every CPU composite path agrees with the GPU `background_color`.
        let bg_cfg = &self.background;
        let gamma = self.gamma;
        let apply_gamma = self.gamma != 1.0;
        let apply_srgb = self.srgb;
        let channel_mode = self.channel_mode;

        // Process rows in parallel; each row is an independent, contiguous slice.
        // Output coordinates map back to source pixels at `stride` (point-sampled).
        pixels
            .par_chunks_mut(out_w)
            .enumerate()
            .for_each(|(oy, row)| {
                let y = (oy * stride).min(height - 1);
                for (ox, px) in row.iter_mut().enumerate() {
                    let x = (ox * stride).min(width - 1);
                    let mut r = sample_channel(r_chan, x, y, width);
                    let mut g = sample_channel(g_chan, x, y, width);
                    let mut b = sample_channel(b_chan, x, y, width);
                    let mut a = sample_channel(a_chan, x, y, width);

                    if a_chan.is_none() {
                        a = 1.0;
                    }

                    match channel_mode {
                        ChannelMode::R => {
                            g = r;
                            b = r;
                            a = 1.0;
                        }
                        ChannelMode::G => {
                            r = g;
                            b = g;
                            a = 1.0;
                        }
                        ChannelMode::B => {
                            r = b;
                            g = b;
                            a = 1.0;
                        }
                        ChannelMode::A => {
                            r = a;
                            g = a;
                            b = a;
                            a = 1.0;
                        }
                        ChannelMode::RGB => {}
                    }

                    let bg = bg_cfg.sample_linear(x as f32, y as f32, width as f32, height as f32);

                    // Apply exposure
                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    // Composite over checkerboard (assuming EXR is pre-multiplied)
                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg[0] * (1.0 - a_clamp);
                    g += bg[1] * (1.0 - a_clamp);
                    b += bg[2] * (1.0 - a_clamp);

                    if apply_gamma {
                        r = crate::render_math::apply_gamma(r, gamma);
                        g = crate::render_math::apply_gamma(g, gamma);
                        b = crate::render_math::apply_gamma(b, gamma);
                    }

                    if apply_srgb {
                        r = Self::linear_to_srgb(r);
                        g = Self::linear_to_srgb(g);
                        b = Self::linear_to_srgb(b);
                    }

                    let r_u8 = (r.clamp(0.0, 1.0) * 255.0) as u8;
                    let g_u8 = (g.clamp(0.0, 1.0) * 255.0) as u8;
                    let b_u8 = (b.clamp(0.0, 1.0) * 255.0) as u8;

                    *px = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                }
            });

        let color_image = egui::ColorImage {
            size: [out_w, out_h],
            source_size: egui::vec2(out_w as f32, out_h as f32),
            pixels,
        };

        Some(ctx.load_texture("exr_viewer", color_image, egui::TextureOptions::LINEAR))
    }

    /// CPU equivalent of the GPU OCIO path for thumbnails / CPU fallback: channel-select +
    /// exposure + checkerboard composite (scene-linear), then the OCIO display transform.
    #[cfg(feature = "ocio")]
    fn generate_texture_ocio(
        &self,
        ctx: &egui::Context,
        exr_data: &ExrData,
        layer_index: usize,
        proc: &std::rc::Rc<floki_ocio::CpuProcessor>,
        max_dim: Option<usize>,
    ) -> Option<egui::TextureHandle> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;
        // Decimate to the thumbnail box for contact-sheet cells (see [`thumb_dims`]);
        // full-res for the CPU-display fallback. OCIO then transforms the small buffer.
        let (out_w, out_h, stride) = thumb_dims(width, height, max_dim);

        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        // Viewport background (issue #18): one config, sampled per pixel below so
        // every CPU composite path agrees with the GPU `background_color`.
        let bg_cfg = &self.background;
        let channel_mode = self.channel_mode;

        // Build a scene-linear RGBA f32 buffer (exposure + checker composite), then let OCIO
        // transform it in one call (OCIO's CPU path is internally vectorized).
        let mut buf = vec![0.0_f32; out_w * out_h * 4];
        buf.par_chunks_mut(out_w * 4)
            .enumerate()
            .for_each(|(oy, row)| {
                let y = (oy * stride).min(height - 1);
                for ox in 0..out_w {
                    let x = (ox * stride).min(width - 1);
                    let mut r = sample_channel(r_chan, x, y, width);
                    let mut g = sample_channel(g_chan, x, y, width);
                    let mut b = sample_channel(b_chan, x, y, width);
                    let mut a = sample_channel(a_chan, x, y, width);
                    if a_chan.is_none() {
                        a = 1.0;
                    }
                    match channel_mode {
                        ChannelMode::R => {
                            g = r;
                            b = r;
                            a = 1.0;
                        }
                        ChannelMode::G => {
                            r = g;
                            b = g;
                            a = 1.0;
                        }
                        ChannelMode::B => {
                            r = b;
                            g = b;
                            a = 1.0;
                        }
                        ChannelMode::A => {
                            r = a;
                            g = a;
                            b = a;
                            a = 1.0;
                        }
                        ChannelMode::RGB => {}
                    }

                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    let bg = bg_cfg.sample_linear(x as f32, y as f32, width as f32, height as f32);
                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg[0] * (1.0 - a_clamp);
                    g += bg[1] * (1.0 - a_clamp);
                    b += bg[2] * (1.0 - a_clamp);

                    let o = ox * 4;
                    row[o] = r;
                    row[o + 1] = g;
                    row[o + 2] = b;
                    row[o + 3] = 1.0;
                }
            });

        if let Err(e) = proc.apply_rgba(&mut buf, out_w, out_h) {
            // Bail rather than display the untransformed buffer: clamping raw
            // scene-linear values to [0,1] would show wrong colors with no
            // indication the transform never ran. Returning None lets the
            // caller fall back / show nothing instead of silent garbage.
            log::error!("OCIO CPU transform failed: {e}");
            return None;
        }

        // User gamma in display space, after the OCIO transform (#93) — the OCIO
        // chain replaces the normal gamma/sRGB ops, so re-apply the control here
        // to match the GPU blit. `None` when gamma == 1.0 (no-op).
        let inv_gamma = (self.gamma != 1.0).then(|| 1.0 / self.gamma);
        let mut pixels = vec![egui::Color32::BLACK; out_w * out_h];
        pixels.par_iter_mut().enumerate().for_each(|(i, px)| {
            let o = i * 4;
            let mut c = [buf[o], buf[o + 1], buf[o + 2]];
            if let Some(ig) = inv_gamma {
                for v in &mut c {
                    *v = v.max(0.0).powf(ig);
                }
            }
            *px = egui::Color32::from_rgb(
                (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                (c[2].clamp(0.0, 1.0) * 255.0) as u8,
            );
        });

        let color_image = egui::ColorImage {
            size: [out_w, out_h],
            source_size: egui::vec2(out_w as f32, out_h as f32),
            pixels,
        };
        Some(ctx.load_texture("exr_viewer", color_image, egui::TextureOptions::LINEAR))
    }

    /// CPU-fallback parity for [`CompareMode::DiffMatte`]: `|A − B|` scaled by
    /// `diff_multiplier` and mapped through a heat ramp. Cached in `diff_texture`,
    /// keyed by `(active_layer, diff_multiplier)`. The GPU path (default) does
    /// this in-shader. Diff is tone-mode-agnostic, so there is no `_ocio` variant.
    fn generate_diff_texture(
        &self,
        ctx: &egui::Context,
        data_a: &ExrData,
        data_b: &ExrData,
        layer_a_idx: usize,
        layer_b_idx: usize,
    ) -> Option<egui::TextureHandle> {
        let (layer_a, r_chan_a, g_chan_a, b_chan_a, _) = data_a.logical_channels(layer_a_idx)?;
        let (layer_b, r_chan_b, g_chan_b, b_chan_b, _) = data_b.logical_channels(layer_b_idx)?;

        let width = layer_a.size.0.max(layer_b.size.0);
        let height = layer_a.size.1.max(layer_b.size.1);

        let mut pixels = vec![egui::Color32::BLACK; width * height];

        // VFX-style diff: per-pixel difference reduced per `diff_metric`, gained,
        // noise-floored, then mapped through the active colormap gradient. Display-
        // space false color — must stay in lockstep with the `is_diff_mode` branch
        // in gpu/shader.wgsl (the GPU path is what's normally on screen; this CPU
        // path serves thumbnails / GPU-less fallback).
        let gain = self.diff_multiplier;
        let nfloor = self.diff_floor;
        let denom = (1.0 - nfloor).max(1e-3);
        let metric = self.diff_metric;
        let grad = self.diff_colormap.gradient();
        let (aw, ah) = (layer_a.size.0, layer_a.size.1);
        let (bw, bh) = (layer_b.size.0, layer_b.size.1);

        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let sr = sample_channel_bounded(r_chan_a, x, y, aw, ah)
                        - sample_channel_bounded(r_chan_b, x, y, bw, bh);
                    let sg = sample_channel_bounded(g_chan_a, x, y, aw, ah)
                        - sample_channel_bounded(g_chan_b, x, y, bw, bh);
                    let sb = sample_channel_bounded(b_chan_a, x, y, aw, ah)
                        - sample_channel_bounded(b_chan_b, x, y, bw, bh);
                    let remap = |raw: f32| ((raw * gain - nfloor) / denom).clamp(0.0, 1.0);
                    let (cr, cg, cb) = match metric {
                        DiffMetric::PerChannelRGB => {
                            (remap(sr.abs()), remap(sg.abs()), remap(sb.abs()))
                        }
                        DiffMetric::Luminance => {
                            let m = remap((0.2126 * sr + 0.7152 * sg + 0.0722 * sb).abs());
                            let c = grad.sample(m);
                            (c[0], c[1], c[2])
                        }
                        DiffMetric::MaxChannel => {
                            let m = remap(sr.abs().max(sg.abs()).max(sb.abs()));
                            let c = grad.sample(m);
                            (c[0], c[1], c[2])
                        }
                    };
                    *px = egui::Color32::from_rgb(
                        (cr * 255.0 + 0.5) as u8,
                        (cg * 255.0 + 0.5) as u8,
                        (cb * 255.0 + 0.5) as u8,
                    );
                }
            });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture("exr_viewer_diff", color_image, egui::TextureOptions::LINEAR))
    }

    /// CPU-fallback parity for [`CompareMode::Composite`]. Blends A and B in
    /// linear space (premultiplied-alpha aware) per [`BlendMode`], then runs the
    /// same exposure → checkerboard → gamma → sRGB tone pipeline as
    /// [`Self::generate_texture`]. Like the CPU diff path it ignores per-channel
    /// isolation — the GPU path (the default) applies that after the blend.
    fn generate_composite_texture(
        &self,
        ctx: &egui::Context,
        data_a: &ExrData,
        data_b: &ExrData,
        layer_a_idx: usize,
        layer_b_idx: usize,
    ) -> Option<egui::TextureHandle> {
        #[cfg(feature = "ocio")]
        if self.ocio_active
            && let Some(proc) = &self.ocio_cpu
        {
            return self.generate_composite_texture_ocio(
                ctx,
                data_a,
                data_b,
                layer_a_idx,
                layer_b_idx,
                proc,
            );
        }

        let (layer_a, r_chan_a, g_chan_a, b_chan_a, a_chan_a) =
            data_a.logical_channels(layer_a_idx)?;
        let (layer_b, r_chan_b, g_chan_b, b_chan_b, a_chan_b) =
            data_b.logical_channels(layer_b_idx)?;

        let width = layer_a.size.0.max(layer_b.size.0);
        let height = layer_a.size.1.max(layer_b.size.1);

        let mut pixels = vec![egui::Color32::BLACK; width * height];

        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        // Viewport background (issue #18): one config, sampled per pixel below so
        // every CPU composite path agrees with the GPU `background_color`.
        let bg_cfg = &self.background;
        let gamma = self.gamma;
        let apply_gamma = self.gamma != 1.0;
        let apply_srgb = self.srgb;
        let blend_mode = self.blend_mode;
        let (aw, ah) = (layer_a.size.0, layer_a.size.1);
        let (bw, bh) = (layer_b.size.0, layer_b.size.1);

        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let ar = sample_channel_bounded(r_chan_a, x, y, aw, ah);
                    let ag = sample_channel_bounded(g_chan_a, x, y, aw, ah);
                    let ab = sample_channel_bounded(b_chan_a, x, y, aw, ah);
                    let aa = if a_chan_a.is_some() {
                        sample_channel_bounded(a_chan_a, x, y, aw, ah)
                    } else {
                        1.0
                    };

                    let br = sample_channel_bounded(r_chan_b, x, y, bw, bh);
                    let bg = sample_channel_bounded(g_chan_b, x, y, bw, bh);
                    let bb = sample_channel_bounded(b_chan_b, x, y, bw, bh);
                    let ba = if a_chan_b.is_some() {
                        sample_channel_bounded(a_chan_b, x, y, bw, bh)
                    } else {
                        1.0
                    };

                    // Premultiplied-alpha blends; keep in lockstep with the
                    // `blend_mode` switch in gpu/shader.wgsl.
                    let (mut r, mut g, mut b, a) = match blend_mode {
                        BlendMode::Over => (
                            ar + br * (1.0 - aa),
                            ag + bg * (1.0 - aa),
                            ab + bb * (1.0 - aa),
                            aa + ba * (1.0 - aa),
                        ),
                        BlendMode::Under => (
                            br + ar * (1.0 - ba),
                            bg + ag * (1.0 - ba),
                            bb + ab * (1.0 - ba),
                            ba + aa * (1.0 - ba),
                        ),
                        BlendMode::Add => (ar + br, ag + bg, ab + bb, (aa + ba).min(1.0)),
                        BlendMode::Multiply => (ar * br, ag * bg, ab * bb, aa),
                        BlendMode::Screen => (
                            ar + br - ar * br,
                            ag + bg - ag * bg,
                            ab + bb - ab * bb,
                            aa + ba - aa * ba,
                        ),
                    };

                    let bg = bg_cfg.sample_linear(x as f32, y as f32, width as f32, height as f32);

                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg[0] * (1.0 - a_clamp);
                    g += bg[1] * (1.0 - a_clamp);
                    b += bg[2] * (1.0 - a_clamp);

                    if apply_gamma {
                        r = crate::render_math::apply_gamma(r, gamma);
                        g = crate::render_math::apply_gamma(g, gamma);
                        b = crate::render_math::apply_gamma(b, gamma);
                    }

                    if apply_srgb {
                        r = Self::linear_to_srgb(r);
                        g = Self::linear_to_srgb(g);
                        b = Self::linear_to_srgb(b);
                    }

                    let r_u8 = (r.clamp(0.0, 1.0) * 255.0) as u8;
                    let g_u8 = (g.clamp(0.0, 1.0) * 255.0) as u8;
                    let b_u8 = (b.clamp(0.0, 1.0) * 255.0) as u8;

                    *px = egui::Color32::from_rgb(r_u8, g_u8, b_u8);
                }
            });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };

        Some(ctx.load_texture(
            "exr_viewer_composite",
            color_image,
            egui::TextureOptions::LINEAR,
        ))
    }

    /// CPU OCIO parity for [`CompareMode::Composite`]: blends A and B in linear space
    /// (exposure + checker composite, scene-linear) then runs the OCIO display transform —
    /// mirrors [`Self::generate_texture_ocio`]. As in that path the checker is composited
    /// pre-OCIO (an accepted parity nuance — this CPU path is fallback/thumbnails only).
    #[cfg(feature = "ocio")]
    fn generate_composite_texture_ocio(
        &self,
        ctx: &egui::Context,
        data_a: &ExrData,
        data_b: &ExrData,
        layer_a_idx: usize,
        layer_b_idx: usize,
        proc: &std::rc::Rc<floki_ocio::CpuProcessor>,
    ) -> Option<egui::TextureHandle> {
        let (layer_a, r_chan_a, g_chan_a, b_chan_a, a_chan_a) =
            data_a.logical_channels(layer_a_idx)?;
        let (layer_b, r_chan_b, g_chan_b, b_chan_b, a_chan_b) =
            data_b.logical_channels(layer_b_idx)?;

        let width = layer_a.size.0.max(layer_b.size.0);
        let height = layer_a.size.1.max(layer_b.size.1);

        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        // Viewport background (issue #18): one config, sampled per pixel below so
        // every CPU composite path agrees with the GPU `background_color`.
        let bg_cfg = &self.background;
        let blend_mode = self.blend_mode;
        let (aw, ah) = (layer_a.size.0, layer_a.size.1);
        let (bw, bh) = (layer_b.size.0, layer_b.size.1);

        let mut buf = vec![0.0_f32; width * height * 4];
        buf.par_chunks_mut(width * 4)
            .enumerate()
            .for_each(|(y, row)| {
                for x in 0..width {
                    let ar = sample_channel_bounded(r_chan_a, x, y, aw, ah);
                    let ag = sample_channel_bounded(g_chan_a, x, y, aw, ah);
                    let ab = sample_channel_bounded(b_chan_a, x, y, aw, ah);
                    let aa = if a_chan_a.is_some() {
                        sample_channel_bounded(a_chan_a, x, y, aw, ah)
                    } else {
                        1.0
                    };
                    let br = sample_channel_bounded(r_chan_b, x, y, bw, bh);
                    let bg = sample_channel_bounded(g_chan_b, x, y, bw, bh);
                    let bb = sample_channel_bounded(b_chan_b, x, y, bw, bh);
                    let ba = if a_chan_b.is_some() {
                        sample_channel_bounded(a_chan_b, x, y, bw, bh)
                    } else {
                        1.0
                    };

                    // Premultiplied-alpha blends; keep in lockstep with the `blend_mode`
                    // switch in gpu/shader.wgsl and `generate_composite_texture`.
                    let (mut r, mut g, mut b, a) = match blend_mode {
                        BlendMode::Over => (
                            ar + br * (1.0 - aa),
                            ag + bg * (1.0 - aa),
                            ab + bb * (1.0 - aa),
                            aa + ba * (1.0 - aa),
                        ),
                        BlendMode::Under => (
                            br + ar * (1.0 - ba),
                            bg + ag * (1.0 - ba),
                            bb + ab * (1.0 - ba),
                            ba + aa * (1.0 - ba),
                        ),
                        BlendMode::Add => (ar + br, ag + bg, ab + bb, (aa + ba).min(1.0)),
                        BlendMode::Multiply => (ar * br, ag * bg, ab * bb, aa),
                        BlendMode::Screen => (
                            ar + br - ar * br,
                            ag + bg - ag * bg,
                            ab + bb - ab * bb,
                            aa + ba - aa * ba,
                        ),
                    };

                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    let bg = bg_cfg.sample_linear(x as f32, y as f32, width as f32, height as f32);
                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg[0] * (1.0 - a_clamp);
                    g += bg[1] * (1.0 - a_clamp);
                    b += bg[2] * (1.0 - a_clamp);

                    let o = x * 4;
                    row[o] = r;
                    row[o + 1] = g;
                    row[o + 2] = b;
                    row[o + 3] = 1.0;
                }
            });

        if let Err(e) = proc.apply_rgba(&mut buf, width, height) {
            // Fail closed (see generate_texture_ocio): show nothing rather than
            // clamped, untransformed composite colors.
            log::error!("OCIO CPU composite transform failed: {e}");
            return None;
        }

        let mut pixels = vec![egui::Color32::BLACK; width * height];
        pixels.par_iter_mut().enumerate().for_each(|(i, px)| {
            let o = i * 4;
            *px = egui::Color32::from_rgb(
                (buf[o].clamp(0.0, 1.0) * 255.0) as u8,
                (buf[o + 1].clamp(0.0, 1.0) * 255.0) as u8,
                (buf[o + 2].clamp(0.0, 1.0) * 255.0) as u8,
            );
        });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };
        Some(ctx.load_texture(
            "exr_viewer_composite",
            color_image,
            egui::TextureOptions::LINEAR,
        ))
    }

    fn sample_pixel(
        &self,
        exr_data: &ExrData,
        layer_index: usize,
        x: usize,
        y: usize,
    ) -> Option<[f32; 4]> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        if x >= width || y >= height {
            return None;
        }

        // Aperture averaging: 1 (single pixel), 3 (3×3) or 9 (9×9). The window is
        // centered on (x, y) with edge-clamped coordinates so it stays valid at
        // the image border (replicate edge), and the average is over every sample
        // in the window.
        let radius = (self.sample_aperture / 2) as isize;
        let mut sum = [0.0f32; 4];
        let mut count = 0.0f32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let sx = (x as isize + dx).clamp(0, width as isize - 1) as usize;
                let sy = (y as isize + dy).clamp(0, height as isize - 1) as usize;
                sum[0] += sample_channel(r_chan, sx, sy, width);
                sum[1] += sample_channel(g_chan, sx, sy, width);
                sum[2] += sample_channel(b_chan, sx, sy, width);
                sum[3] += if a_chan.is_some() {
                    sample_channel(a_chan, sx, sy, width)
                } else {
                    1.0
                };
                count += 1.0;
            }
        }

        Some([
            sum[0] / count,
            sum[1] / count,
            sum[2] / count,
            sum[3] / count,
        ])
    }

    /// Thin re-export of [`crate::render_math::linear_to_srgb`] so existing
    /// `ExrViewer::linear_to_srgb(..)` call sites (here and in `app.rs`) keep
    /// working while the math lives in one tested place.
    pub fn linear_to_srgb(l: f32) -> f32 {
        crate::render_math::linear_to_srgb(l)
    }

    /// Invalidate the cached histogram so the next [`Self::calculate_histogram`] call
    /// recomputes. Call this when image B changes (load/unload) — B identity is
    /// not part of the cache key.
    pub fn invalidate_histogram(&mut self) {
        self.histogram_key = None;
    }

    /// Drop every cached reference-image (B) texture so the viewport rebuilds from the
    /// newly loaded data. The texture caches otherwise only refresh when the layer *count*
    /// changes, so re-loading a different B with the same layer count would keep showing the
    /// stale image. Clears the GPU bind groups, the CPU thumbnails, and the cached
    /// diff/composite textures (which both depend on B).
    pub fn invalidate_reference_textures(&mut self) {
        self.textures_b.fill(None);
        self.gpu_textures_b.fill(None);
        self.diff_texture = None;
        self.composite_texture = None;
    }

    /// Drop every cached image-A texture so the viewport rebuilds from the newly
    /// swapped data - the A-side counterpart of [`Self::invalidate_reference_textures`].
    /// The texture caches otherwise only refresh when the layer *count* changes, so
    /// swapping a different A with the same layer count (e.g. the next frame in an
    /// image sequence, #7) would keep showing the stale image. Clears the GPU bind
    /// groups, the CPU thumbnails, and the cached diff/composite textures (which both
    /// depend on A). Used by [`crate::app::ExrApp::swap_image_data`].
    pub fn invalidate_active_textures(&mut self) {
        self.textures.fill(None);
        self.gpu_textures.fill(None);
        self.diff_texture = None;
        self.composite_texture = None;
    }

    /// Clear the slot-A proxy (first-paint) texture. Called when the full-res
    /// `ExrData` lands ([`crate::app::ExrApp::swap_image_data`]) or the session
    /// resets — the proxy is no longer needed once full-res pixels are
    /// available.
    pub fn clear_proxy(&mut self) {
        self.proxy_texture = None;
        self.proxy_full_size = None;
    }

    /// Upload a low-res [`ProxyImage`] as the slot-A first-paint texture (#58).
    /// Bakes the exposure/gamma/sRGB + background tone pipeline into an
    /// `egui::TextureHandle` (mirroring the CPU `generate_texture` path) so the
    /// proxy renders correctly tone-mapped via `painter.image`. The full image
    /// dimensions are stored for layout. Idempotent: a repeat call replaces the
    /// previous upload.
    ///
    /// **OCIO note:** the proxy uses the non-OCIO tone pipeline even when OCIO
    /// is active. The proxy is a transient stand-in replaced near-instantly by
    /// the full OCIO render; an OCIO-accurate proxy is a follow-up refinement.
    ///
    /// `#[allow(dead_code)]`: wired to [`crate::app::ExrApp::set_proxy`], which
    /// #33's decode path calls from the worker thread once a low-res read lands.
    #[allow(dead_code)]
    pub fn set_proxy(&mut self, ctx: &egui::Context, proxy: crate::proxy::ProxyImage) {
        // Drop the previous upload first so its GPU memory is released before the
        // new texture is created, avoiding a transient double-allocation (egui's
        // lazy drop would otherwise defer it past the new upload).
        self.proxy_texture = None;
        self.proxy_full_size = Some(egui::vec2(
            proxy.full_width as f32,
            proxy.full_height as f32,
        ));
        self.proxy_texture = Self::generate_texture_proxy(self, ctx, &proxy);
    }

    /// Whether a slot-A proxy texture is currently uploaded.
    pub fn has_proxy(&self) -> bool {
        self.proxy_texture.is_some()
    }

    /// Apply the canvas zoom/pan interaction for one frame from `response`:
    /// first-frame fit-to-view, cursor-centered wheel/pinch zoom, and drag pan
    /// (suppressed while an annotation tool is active). Extracted from
    /// [`Self::ui`] so the proxy first-paint path ([`Self::draw_proxy`]) shares
    /// the exact same interaction model — the handoff from proxy to full-res is
    /// visually continuous because zoom/pan state is identical.
    fn handle_canvas_interaction(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        tex_size: egui::Vec2,
    ) {
        if self.first_frame {
            let scale_x = rect.width() / tex_size.x;
            let scale_y = rect.height() / tex_size.y;
            self.scale = scale_x.min(scale_y).min(1.0); // Fit but don't scale up past 1.0 initially
            self.translation = egui::Vec2::ZERO;
            self.first_frame = false;
        }

        // Handle Zoom: pinch / ctrl+scroll via zoom_delta(), plus the plain
        // mouse wheel via smooth_scroll_delta (which zoom_delta() does NOT report).
        if response.hovered() {
            let (zoom_delta, scroll_y) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.y));
            let wheel_zoom = (scroll_y * 0.004).exp();
            let total_zoom = zoom_delta * wheel_zoom;
            if total_zoom != 1.0
                && let Some(pos) = response.hover_pos()
            {
                // Zoom around the cursor
                let offset = pos - rect.center() - self.translation;
                self.translation -= offset * (total_zoom - 1.0);
                self.scale = (self.scale * total_zoom).clamp(0.01, 100.0);
            }
        }

        // Handle Panning — suppressed while an annotation tool is active so
        // its drag draws a shape instead of moving the image (#45).
        if response.dragged() && !self.anno_tool.is_active() {
            self.translation += response.drag_delta();
        }
    }

    /// First-paint render path: paint the slot-A proxy texture while the full
    /// `ExrData` decode is in flight (#58/#33). Lays out the image at the
    /// proxy's *full* dimensions (so the rect matches the upcoming full-res
    /// render) and applies the same zoom/pan interaction as [`Self::ui`], so
    /// the handoff to full-res is continuous. No panels / contact-sheet /
    /// compare modes — those need a full `ExrData`; the proxy is a stand-in
    /// until it arrives. Used by [`crate::app::ExrApp::draw_central_canvas`] in
    /// the loading branch when a proxy is available.
    pub fn draw_proxy(&mut self, ui: &mut egui::Ui) {
        let Some(tex_size) = self.proxy_full_size else {
            return;
        };
        if self.proxy_texture.is_none() {
            return;
        }

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        self.last_canvas_rect = Some(rect);
        self.handle_canvas_interaction(ui, rect, &response, tex_size);

        let image_size = tex_size * self.scale;
        let image_rect = egui::Rect::from_min_size(
            rect.center() + self.translation - image_size / 2.0,
            image_size,
        );
        self.last_image_rect = Some(image_rect);

        // Paint the tone-baked proxy texture, upscaled via linear filtering into
        // the full image rect. egui uploads the texture to the GPU itself, so this
        // works on both the CPU and GPU render paths (the full-res wgpu path takes
        // over once the decode lands).
        if let Some(tex) = self.proxy_texture.as_ref() {
            ui.painter().with_clip_rect(rect).image(
                tex.id(),
                image_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    }

    /// Build a tone-baked `egui::TextureHandle` from a low-res [`ProxyImage`],
    /// applying exposure/gamma/sRGB + background composite (mirroring the CPU
    /// [`Self::generate_texture`] path, minus channel-select — the proxy is
    /// already RGBA). Used by [`Self::set_proxy`]. The proxy's pixels are raw
    /// scene-linear RGBA32Float.
    #[allow(dead_code)]
    fn generate_texture_proxy(
        &self,
        ctx: &egui::Context,
        proxy: &crate::proxy::ProxyImage,
    ) -> Option<egui::TextureHandle> {
        let width = proxy.proxy_width;
        let height = proxy.proxy_height;
        if width == 0 || height == 0 {
            return None;
        }
        let mut pixels = vec![egui::Color32::BLACK; width * height];

        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        let bg_cfg = &self.background;
        let gamma = self.gamma;
        let apply_gamma = self.gamma != 1.0;
        let apply_srgb = self.srgb;
        let src = &proxy.pixels;

        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                for (x, px) in row.iter_mut().enumerate() {
                    let o = (y * width + x) * 4;
                    let mut r = src[o];
                    let mut g = src[o + 1];
                    let mut b = src[o + 2];
                    let a = src[o + 3];

                    // Apply exposure
                    r *= exp_mult;
                    g *= exp_mult;
                    b *= exp_mult;

                    // Composite over the viewport background (pre-multiplied).
                    let bg = bg_cfg.sample_linear(x as f32, y as f32, width as f32, height as f32);
                    let a_clamp = a.clamp(0.0, 1.0);
                    r += bg[0] * (1.0 - a_clamp);
                    g += bg[1] * (1.0 - a_clamp);
                    b += bg[2] * (1.0 - a_clamp);

                    if apply_gamma {
                        r = crate::render_math::apply_gamma(r, gamma);
                        g = crate::render_math::apply_gamma(g, gamma);
                        b = crate::render_math::apply_gamma(b, gamma);
                    }
                    if apply_srgb {
                        r = Self::linear_to_srgb(r);
                        g = Self::linear_to_srgb(g);
                        b = Self::linear_to_srgb(b);
                    }

                    *px = egui::Color32::from_rgb(
                        (r.clamp(0.0, 1.0) * 255.0) as u8,
                        (g.clamp(0.0, 1.0) * 255.0) as u8,
                        (b.clamp(0.0, 1.0) * 255.0) as u8,
                    );
                }
            });

        let color_image = egui::ColorImage {
            size: [width, height],
            source_size: egui::vec2(width as f32, height as f32),
            pixels,
        };
        Some(ctx.load_texture("exr_proxy", color_image, egui::TextureOptions::LINEAR))
    }

    pub fn calculate_histogram(&mut self, exr_data: &ExrData, exr_data_b: Option<&ExrData>) {
        let key = (self.active_layer, self.log_histogram);
        if self.histogram_key == Some(key) {
            return;
        }

        let log_histogram = self.log_histogram;
        let calc_bins = |data: &ExrData, layer_idx: usize| -> Option<[u32; 256]> {
            let (layer, r_chan, g_chan, b_chan, _) = data.logical_channels(layer_idx)?;
            let width = layer.size.0;
            let height = layer.size.1;

            // Hoist F32 slices (common case) for direct indexing.
            let r_s = sample_channel_f32(r_chan);
            let g_s = sample_channel_f32(g_chan);
            let b_s = sample_channel_f32(b_chan);

            // Parallelize per-row: each thread accumulates its own [u32; 256]
            // bins, then reduce by summing. For a 4K layer this is ~8M
            // iterations — single-threaded was a noticeable stall on every
            // layer/log-scale change.
            let bins = (0..height)
                .into_par_iter()
                .map(|y| {
                    let mut local = [0u32; 256];
                    for x in 0..width {
                        let r = pixel_val(r_s, r_chan, x, y, width);
                        let g = pixel_val(g_s, g_chan, x, y, width);
                        let b = pixel_val(b_s, b_chan, x, y, width);

                        let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;

                        let bin = if log_histogram {
                            let ev = if lum <= 0.0 {
                                -10.0
                            } else {
                                lum.log2().clamp(-10.0, 10.0)
                            };
                            ((ev + 10.0) / 20.0 * 255.0) as usize
                        } else {
                            (lum.clamp(0.0, 1.0) * 255.0) as usize
                        };

                        if bin < 256 {
                            local[bin] += 1;
                        }
                    }
                    local
                })
                .reduce(
                    || [0u32; 256],
                    |mut a, b| {
                        for i in 0..256 {
                            a[i] += b[i];
                        }
                        a
                    },
                );
            Some(bins)
        };

        self.histogram = calc_bins(exr_data, self.active_layer);
        self.histogram_b = exr_data_b.and_then(|d| {
            calc_bins(
                d,
                self.active_layer
                    .min(d.logical_layers.len().saturating_sub(1)),
            )
        });
        self.histogram_key = Some(key);
    }
}

fn rgb_to_hsvl(r: f32, g: f32, b: f32) -> (f32, f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let mut h = 0.0;
    if delta > 0.0 {
        if max == r {
            h = 60.0 * (((g - b) / delta) % 6.0);
        } else if max == g {
            h = 60.0 * (((b - r) / delta) + 2.0);
        } else if max == b {
            h = 60.0 * (((r - g) / delta) + 4.0);
        }
    }
    if h < 0.0 {
        h += 360.0;
    }

    let s = if max > 0.0 { delta / max } else { 0.0 };
    let v = max;
    let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;

    (h, s, v, l)
}

fn colored_rgba_label(ui: &mut egui::Ui, prefix: &str, val: [f32; 4]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if !prefix.is_empty() {
            ui.label(prefix);
        }
        ui.label(
            egui::RichText::new(format!("{:.5}", val[0]))
                .color(egui::Color32::from_rgb(255, 80, 80)),
        );
        ui.label(
            egui::RichText::new(format!("{:.5}", val[1]))
                .color(egui::Color32::from_rgb(80, 255, 80)),
        );
        ui.label(
            egui::RichText::new(format!("{:.5}", val[2]))
                .color(egui::Color32::from_rgb(100, 150, 255)),
        );
        ui.label(egui::RichText::new(format!("{:.5}", val[3])).color(egui::Color32::LIGHT_GRAY));
    });
}

fn draw_dashed_rect(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    dash_length: f32,
    gap_length: f32,
) {
    let draw_line = |start: egui::Pos2, end: egui::Pos2| {
        let dir = end - start;
        let len = dir.length();
        let stride = dash_length + gap_length;
        // Degenerate edge or non-advancing stride: nothing to draw (and the
        // latter would otherwise spin forever / divide by zero on `dir_norm`).
        if len <= f32::EPSILON || stride <= f32::EPSILON {
            return;
        }
        let dir_norm = dir / len;
        // Derive each dash offset from an integer index rather than accumulating
        // a float `t += stride`, so rounding error can't drift over a long edge
        // (and clippy's `while_float` is satisfied). Last index < len by ceil math.
        let steps = (len / stride).ceil() as usize;
        for i in 0..steps {
            let t = i as f32 * stride;
            let t_end = (t + dash_length).min(len);
            painter.line_segment(
                [start + dir_norm * t, start + dir_norm * t_end],
                (1.0, color),
            );
        }
    };

    draw_line(rect.left_top(), rect.right_top());
    draw_line(rect.right_top(), rect.right_bottom());
    draw_line(rect.right_bottom(), rect.left_bottom());
    draw_line(rect.left_bottom(), rect.left_top());
}

#[cfg(test)]
mod gui_tests {
    //! Headless GUI tests via `egui_kittest`, so they run anywhere — no wgpu
    //! device. Most drive the rendering-free [`ExrViewer::handle_hotkeys`] seam
    //! (events → `key_pressed` → state mutation); the smoke test additionally
    //! drives the full [`ExrViewer::ui`] CPU path (`render_state = None`) across
    //! every compare mode to guard the render/extraction seams.
    use super::{ChannelMode, CompareMode, ExrViewer};
    use crate::annotation::{Annotation, AnnotationKind, AnnotationTool};
    use crate::exr_loader::ExrData;
    use eframe::egui;
    use egui_kittest::Harness;
    use exr::prelude::*;

    #[test]
    fn thumb_dims_decimates_to_the_box_and_preserves_aspect() {
        use super::thumb_dims;
        // No cap (CPU-display fallback): full res, stride 1.
        assert_eq!(thumb_dims(4096, 2160, None), (4096, 2160, 1));
        // Image already within the box: untouched.
        assert_eq!(thumb_dims(200, 100, Some(256)), (200, 100, 1));
        // 4K landscape -> longest edge capped at the box, aspect preserved.
        let (w, h, stride) = thumb_dims(4096, 2160, Some(256));
        assert!(w <= 256 && h <= 256, "longest edge within the box: {w}x{h}");
        assert_eq!(stride, 16, "4096.div_ceil(256)");
        assert!(
            (w as f32 / h as f32 - 4096.0 / 2160.0).abs() < 0.05,
            "aspect kept"
        );
        // Portrait caps the height instead.
        let (w, h, _) = thumb_dims(1080, 1920, Some(256));
        assert!(
            w <= 256 && h <= 256 && h >= w,
            "portrait stays portrait: {w}x{h}"
        );
        // Degenerate: never produces a zero dimension.
        assert_eq!(thumb_dims(0, 0, Some(256)), (1, 1, 1));
    }

    #[test]
    fn t2_victim_evicts_furthest_and_protects_on_screen() {
        use super::t2_victim;
        // On-screen frame 5; the furthest resident frame is evicted, never 5.
        assert_eq!(t2_victim([3, 4, 5, 6, 9].into_iter(), Some(5)), Some(9));
        assert_eq!(t2_victim([1, 2, 5, 6].into_iter(), Some(5)), Some(1));
        // Only the on-screen frame left -> nothing to evict.
        assert_eq!(t2_victim([5].into_iter(), Some(5)), None);
        assert_eq!(t2_victim(std::iter::empty(), Some(5)), None);
    }

    /// Tiny 2×2 RGBA EXR fixture so the CPU render path has real data to draw.
    fn write_rgba_exr(path: &std::path::Path) {
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F32(vec![0.5; 4]),
            ));
        }
        Image::from_layer(Layer::new(
            (2, 2),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(path)
        .expect("write rgba exr fixture");
    }

    struct State {
        viewer: ExrViewer,
        has_b: bool,
    }

    fn harness(has_b: bool) -> Harness<'static, State> {
        Harness::new_ui_state(
            |ui, s: &mut State| {
                let has_b = s.has_b;
                s.viewer.handle_hotkeys(ui, has_b);
            },
            State {
                viewer: ExrViewer::default(),
                has_b,
            },
        )
    }

    #[test]
    fn channel_keys_isolate_and_reset() {
        let mut h = harness(false);

        for (key, expected) in [
            (egui::Key::R, ChannelMode::R),
            (egui::Key::G, ChannelMode::G),
            (egui::Key::B, ChannelMode::B),
            (egui::Key::A, ChannelMode::A),
            (egui::Key::C, ChannelMode::RGB), // C returns to full RGB
        ] {
            h.key_press(key);
            h.run();
            assert_eq!(h.state().viewer.channel_mode, expected, "key {key:?}");
        }
    }

    #[test]
    fn reset_keys_zero_exposure_and_gamma() {
        let mut h = harness(false);
        h.state_mut().viewer.exposure = 2.0;
        h.state_mut().viewer.gamma = 2.2;

        h.key_press(egui::Key::E);
        h.run();
        assert_eq!(h.state().viewer.exposure, 0.0, "E should reset exposure");
        // Gamma untouched by the exposure reset.
        assert_eq!(h.state().viewer.gamma, 2.2);

        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::G);
        h.run();
        assert_eq!(h.state().viewer.gamma, 1.0, "Shift+G should reset gamma");
    }

    #[test]
    fn plain_g_still_isolates_green_not_gamma_reset() {
        let mut h = harness(false);
        h.state_mut().viewer.gamma = 2.2;

        h.key_press(egui::Key::G);
        h.run();
        assert_eq!(
            h.state().viewer.channel_mode,
            ChannelMode::G,
            "plain G must isolate the green channel"
        );
        assert_eq!(
            h.state().viewer.gamma,
            2.2,
            "plain G must NOT reset gamma (that's Shift+G)"
        );
    }

    #[test]
    fn channel_keys_are_inert_in_contact_sheet() {
        let mut h = harness(false);
        h.state_mut().viewer.show_contact_sheet = true;
        let before = h.state().viewer.channel_mode;

        h.key_press(egui::Key::R);
        h.run();
        assert_eq!(
            h.state().viewer.channel_mode,
            before,
            "channel hotkeys must not fire in contact-sheet mode"
        );
    }

    #[test]
    fn compare_keys_switch_mode_when_reference_loaded() {
        let mut h = harness(true);

        h.key_press(egui::Key::Num2);
        h.run();
        assert_eq!(h.state().viewer.compare_mode, CompareMode::SingleB);

        h.key_press(egui::Key::Num1);
        h.run();
        assert_eq!(h.state().viewer.compare_mode, CompareMode::SingleA);
    }

    #[test]
    fn reference_only_shortcuts_are_inert_without_b() {
        let mut h = harness(false);
        let before = h.state().viewer.compare_mode;

        h.key_press(egui::Key::Num2);
        h.run();
        assert_eq!(
            h.state().viewer.compare_mode,
            before,
            "Num2 must do nothing without a reference image"
        );
    }

    #[test]
    fn space_toggles_blink_only_with_reference() {
        // With a reference image, Space toggles the blink (A/B flip) state.
        let mut h = harness(true);
        assert!(!h.state().viewer.blink_state);
        h.key_press(egui::Key::Space);
        h.run();
        assert!(
            h.state().viewer.blink_state,
            "Space should enable blink with B"
        );
        h.key_press(egui::Key::Space);
        h.run();
        assert!(
            !h.state().viewer.blink_state,
            "Space should toggle blink back off"
        );

        // Without a reference image, Space is inert.
        let mut h = harness(false);
        h.key_press(egui::Key::Space);
        h.run();
        assert!(
            !h.state().viewer.blink_state,
            "Space must be inert without B"
        );
    }

    #[test]
    fn test_blink_interval_math() {
        let blink_interval = 1.0;
        let is_even_phase = |time: f64| ((time / blink_interval) as usize).is_multiple_of(2);

        assert!(is_even_phase(0.0));
        assert!(is_even_phase(0.5));
        assert!(!is_even_phase(1.0));
        assert!(!is_even_phase(1.5));
        assert!(is_even_phase(2.0));

        let blink_interval = 0.5;
        let is_even_phase = |time: f64| ((time / blink_interval) as usize).is_multiple_of(2);
        assert!(is_even_phase(0.0));
        assert!(is_even_phase(0.25));
        assert!(!is_even_phase(0.5));
        assert!(!is_even_phase(0.75));
        assert!(is_even_phase(1.0));
    }

    #[test]
    fn has_mode_params_drives_contextual_row() {
        let mut v = ExrViewer::default();

        // Single-view modes carry no contextual params → no second row.
        // (Default `compare_mode` is `SingleA`, so check it before mutating.)
        assert_eq!(v.compare_mode, CompareMode::SingleA);
        assert!(!v.has_mode_params());
        v.compare_mode = CompareMode::SingleB;
        assert!(!v.has_mode_params());

        // Parameterized modes do.
        for mode in [
            CompareMode::Wipe,
            CompareMode::DiffMatte,
            CompareMode::SideBySide,
            CompareMode::Composite,
        ] {
            v.compare_mode = mode;
            assert!(v.has_mode_params(), "{mode:?} should show a contextual row");
        }

        // Blink wins even though it overwrites compare_mode to a single view.
        v.compare_mode = CompareMode::SingleB;
        v.blink_state = true;
        assert!(v.has_mode_params(), "blink exposes the speed control");
    }

    struct SmokeState {
        viewer: ExrViewer,
        a: ExrData,
        b: Option<ExrData>,
    }

    /// Drive the full `ExrViewer::ui` CPU path (no GPU `render_state`) with a
    /// loaded A and B across every compare mode plus the contact sheet, asserting
    /// it lays out without panicking. Exercises the seams extracted from `ui()`
    /// in #26 (contact sheet, pixel sampling, CPU draw paths).
    #[test]
    fn ui_renders_all_compare_modes_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.exr");
        let pb = dir.path().join("b.exr");
        write_rgba_exr(&pa);
        write_rgba_exr(&pb);
        let a = ExrData::load(&pa).unwrap();
        let b = ExrData::load(&pb).unwrap();

        let mut h = Harness::new_ui_state(
            |ui, s: &mut SmokeState| {
                // Disjoint field borrows: &mut viewer + &a/&b.
                let SmokeState { viewer, a, b } = s;
                viewer.ui(ui, a, b.as_ref(), None, None);
            },
            SmokeState {
                viewer: ExrViewer::default(),
                a,
                b: Some(b),
            },
        );

        for mode in [
            CompareMode::SingleA,
            CompareMode::SingleB,
            CompareMode::Wipe,
            CompareMode::SideBySide,
            CompareMode::DiffMatte,
            CompareMode::Composite,
        ] {
            h.state_mut().viewer.compare_mode = mode;
            h.run();
        }

        // Contact sheet (single + dual) must also lay out cleanly.
        h.state_mut().viewer.show_contact_sheet = true;
        h.run();
        h.state_mut().viewer.compare_mode = CompareMode::SideBySide;
        h.run();
    }

    #[test]
    fn draw_proxy_renders_without_panicking_and_records_rects() {
        // The first-paint path (#58): with no full ExrData, `draw_proxy` lays
        // out the canvas at the proxy's *full* dimensions and paints the
        // tone-baked proxy texture. Drives the CPU path headlessly (no wgpu).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.exr");
        write_rgba_exr(&path);
        let data = ExrData::load(&path).unwrap();
        // 4×4 full → 2×2 proxy.
        // (write_rgba_exr writes a 2×2; use a richer fixture for downsample.)
        let _ = data;

        // Build a synthetic proxy directly so the test doesn't depend on the
        // downsample seam (covered separately in `proxy::tests`).
        let proxy = crate::proxy::ProxyImage {
            full_width: 8,
            full_height: 4,
            proxy_width: 2,
            proxy_height: 1,
            pixels: vec![0.5, 0.5, 0.5, 1.0, 0.5, 0.5, 0.5, 1.0],
        };

        struct S {
            viewer: ExrViewer,
            proxy: Option<crate::proxy::ProxyImage>,
        }
        let mut h = Harness::new_ui_state(
            |ui, s: &mut S| {
                if let Some(p) = s.proxy.take() {
                    s.viewer.set_proxy(ui.ctx(), p);
                }
                s.viewer.draw_proxy(ui);
            },
            S {
                viewer: ExrViewer::default(),
                proxy: Some(proxy),
            },
        );
        h.run();
        assert!(h.state().viewer.has_proxy(), "proxy uploaded");
        assert_eq!(
            h.state().viewer.proxy_full_size,
            Some(egui::vec2(8.0, 4.0)),
            "full dims stored for layout"
        );
        assert!(
            h.state().viewer.last_canvas_rect.is_some(),
            "canvas rect recorded"
        );
        assert!(
            h.state().viewer.last_image_rect.is_some(),
            "image rect recorded"
        );

        // first_frame fit should have fired (scale set to fit the 8×4 image).
        assert!(!h.state().viewer.first_frame, "first_frame fit ran");
    }

    #[test]
    fn draw_proxy_noop_without_proxy_set() {
        // Calling draw_proxy before set_proxy must not panic / allocate.
        struct S {
            viewer: ExrViewer,
        }
        let mut h = Harness::new_ui_state(
            |ui, s: &mut S| {
                s.viewer.draw_proxy(ui);
            },
            S {
                viewer: ExrViewer::default(),
            },
        );
        h.run();
        assert!(!h.state().viewer.has_proxy());
        assert!(h.state().viewer.last_canvas_rect.is_none());
    }

    #[test]
    fn clear_proxy_drops_proxy_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.exr");
        write_rgba_exr(&path);
        let data = ExrData::load(&path).unwrap();
        let proxy = crate::proxy::ProxyImage::from_exr_data_downsampled(&data, 0, 1).unwrap();

        // Use a kittest ctx to load the texture (set_proxy needs an egui ctx).
        let mut h = Harness::new_ui_state(
            |ui, v: &mut ExrViewer| {
                v.set_proxy(
                    ui.ctx(),
                    crate::proxy::ProxyImage {
                        pixels: proxy.pixels.clone(),
                        ..proxy.clone()
                    },
                );
            },
            ExrViewer::default(),
        );
        h.run_steps(1);
        assert!(h.state().has_proxy());
        h.state_mut().clear_proxy();
        assert!(!h.state().has_proxy(), "proxy cleared");
        assert_eq!(h.state().proxy_full_size, None, "full-size hint cleared");
    }

    #[test]
    fn annotation_undo_redo_and_clear() {
        let mut v = ExrViewer::default();
        let mk = |x: f32| Annotation {
            kind: AnnotationKind::Rect {
                a: [0.0, 0.0],
                b: [x, x],
            },
            color: egui::Color32::RED,
            width: 3.0,
        };
        // Two committed shapes (mirrors handle_annotation_input's commit path).
        v.push_anno_undo();
        v.annotations.push(mk(1.0));
        v.push_anno_undo();
        v.annotations.push(mk(2.0));
        assert_eq!(v.annotations.len(), 2);

        v.undo_annotation();
        v.undo_annotation();
        assert_eq!(v.annotations.len(), 0);
        v.redo_annotation();
        v.redo_annotation();
        assert_eq!(v.annotations.len(), 2);

        // A fresh edit after undo clears the redo stack.
        v.undo_annotation();
        v.push_anno_undo();
        v.annotations.push(mk(9.0));
        assert!(v.anno_redo.is_empty());

        // Clear-all is itself undoable.
        v.clear_annotations();
        assert!(v.annotations.is_empty());
        v.undo_annotation();
        assert!(!v.annotations.is_empty());
    }

    #[test]
    fn cancel_annotation_resets_active_tool() {
        let mut v = ExrViewer::default();
        assert!(!v.cancel_annotation(), "nothing active → not consumed");
        v.anno_tool = AnnotationTool::Arrow;
        assert!(v.cancel_annotation(), "active tool → consumed");
        assert_eq!(v.anno_tool, AnnotationTool::None);
    }
}
