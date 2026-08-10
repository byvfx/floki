//! The image canvas: pan/zoom, channel/exposure/gamma controls, the six
//! [`CompareMode`]s, pixel sampling, histogram and contact sheet. State lives in
//! [`ExrViewer`]; [`ExrViewer::ui`] is the per-frame entry point.
//!
//! # Texture generation
//!
//! Rendering is **GPU-only** (#59 removed the CPU viewport render path; the app
//! requires a GPU and aborts without one):
//! [`Self::build_layer_texture`](ExrViewer::build_layer_texture) uploads a
//! layer's RGBA into a bind group; `gpu/shader.wgsl` then applies channel
//! isolation, exposure, gamma, sRGB **and every compare mode** (wipe / diff /
//! composite) in-shader, so a single generator serves all modes, cached per layer
//! in `gpu_textures` / `gpu_textures_b`. Under OCIO the display chain is the
//! two-pass OCIO callback ([`crate::gpu::ocio_pass`]) instead.
//!
//! The only CPU bake that remains is [`Self::generate_texture`] for
//! contact-sheet thumbnails — the **headless / no-GPU fallback** (used by tests
//! and when no GPU is present). With a GPU, thumbnails render through
//! [`crate::gpu::thumbnail`] (OCIO included). Thumbnail caches invalidate on a
//! layer-count change, an OCIO-state change
//! ([`ExrViewer::invalidate_thumbnails_on_ocio_change`]), and via
//! [`ExrViewer::invalidate_reference_textures`] when B is replaced.

use crate::annotation::{Annotation, AnnotationKind, AnnotationTool};
use crate::exr_loader::ExrData;
use crate::gradient::{Colormap, DiffMetric, Gradient};
use crate::render_program;
use eframe::egui;
use exr::prelude::f16;
use rayon::prelude::*;

/// Widen a linear RGB triple to the `vec4` the GPU uniforms expect (w unused).
fn rgb3_to_vec4(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

/// The unscaled canvas bounds that "Frame" (F) fits into the viewport. Every mode
/// except Side-by-Side draws B over A in the same rect, so the extent is just the
/// A image; Side-by-Side lays A and B out horizontally (B height-normalized to A
/// when enabled), so framing must fit their *combined* width or the second image
/// spills off-screen. Mirrors the SBS layout in [`ExrViewer::emit_mode_draws`],
/// measured in unscaled space (the `scale` cancels out of the fit ratio).
fn framing_bounds(
    mode: CompareMode,
    normalize_sbs: bool,
    tex_size: egui::Vec2,
    tex_size_b: Option<egui::Vec2>,
) -> egui::Vec2 {
    let Some(size_b) = tex_size_b.filter(|_| mode == CompareMode::SideBySide) else {
        return tex_size; // single image, or B not loaded
    };
    // Normalized B is scaled so its height matches A's; otherwise B keeps its own
    // size. Guard a degenerate zero-height B (never produced by a real texture).
    let b = if normalize_sbs && size_b.y > 0.0 {
        egui::vec2(size_b.x * (tex_size.y / size_b.y), tex_size.y)
    } else {
        size_b
    };
    egui::vec2(tex_size.x + b.x, tex_size.y.max(b.y))
}

// Pure pixel access + thumbnail decimation live in `crate::pixels` (#153), so
// `gpu/` and `proxy` never import from this UI module.
use crate::pixels::{pixel_val, sample_channel, sample_channel_f32, thumb_dims};

/// Contact-sheet thumbnail box, in pixels: both the on-screen cell size and the
/// resolution thumbnails are baked at (longest edge), so the two never drift.
const THUMB_BOX: usize = 256;

/// Max contact-sheet thumbnails baked per layout pass (#144). Playback no longer
/// re-bakes the sheet every frame swap (the app freezes it while the transport is
/// busy and refreshes on settle), so the remaining bursts are one-off: the settle
/// refresh, a tone/LUT/OCIO/background wipe, and the first open. Amortizing those
/// over a few frames — visible cells first — keeps a 40-AOV settle from stalling
/// the UI thread in a single frame. Off-screen cells wait until scrolled in.
const THUMB_BAKES_PER_FRAME: usize = 4;

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
/// aware). Encoded for the shader via [`Self::as_u32`]. Serde-serializable so the
/// Layers panel can persist each layer's blend (#99 PR-B; safe fieldless enum).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, serde::Serialize, serde::Deserialize)]
pub enum BlendMode {
    #[default]
    Over,
    Under,
    Add,
    Multiply,
    Screen,
}

impl BlendMode {
    /// Every variant, in menu order — the single list the blend pickers iterate
    /// (the A/B Composite combo and the Layers-panel per-row combo, #99 PR-B.4).
    pub const ALL: [Self; 5] = [
        Self::Over,
        Self::Under,
        Self::Add,
        Self::Multiply,
        Self::Screen,
    ];

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
    /// Horizontal anamorphic unsqueeze factor for B (#179). A's factor is already
    /// baked into `image_size`/`image_rect`, so single-image/composite/wipe/diff
    /// draws need nothing further; only the Side-by-Side path lays B out separately
    /// and must apply B's own factor here.
    par_b: f32,
}

/// One resolved Layers-panel composite layer for [`ExrViewer::draw_comp_composite`]
/// (#99 PR-B.3), in bottom→top order. The app builds these from
/// `comp_stack.composite_at` + `comp_sources` (looking up each `Draw`'s source
/// texture), so the viewer stays unaware of decode / source storage — it only
/// folds the given bind groups through the accumulate ping-pong.
pub struct CompDraw {
    /// The layer's GPU texture bind group (its `CompSource`'s), bound as `tex_a`.
    pub bind_group: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
    /// How this layer combines with the accumulation below it (ignored for the
    /// bottom layer, which is a plain copy).
    pub blend: BlendMode,
    /// Layer opacity in `[0, 1]`.
    pub opacity: f32,
}

/// Per-position flags for a comp layer at stack index `i` of `n` (#99 PR-B.3),
/// returned as `(is_composite, is_top)`. The bottom layer (`i == 0`) is a plain
/// copy into the cleared accumulation (`is_composite = false`, its blend unused);
/// every layer above blends over the accumulation (`is_composite = true`). Only the
/// top layer (`i == n-1`) applies the global view ops (exposure / channel
/// isolation), so they hit the finished composite exactly once — the lower layers
/// neutralize them (the exposure-once invariant from PR-A.4). Pure, so the ordering
/// contract is unit-testable without a GPU.
fn comp_layer_flags(i: usize, n: usize) -> (bool, bool) {
    (i != 0, i + 1 == n)
}

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

/// Frame-keyed GPU-texture ring with a pure map policy (#153): cap-shrink
/// eviction, layer-switch invalidation, and on-screen protection, factored out
/// of [`ExrViewer`] so the risky part is unit-testable. Generic over the payload
/// `T` — production stores [`T2Texture`] (which needs a GPU device), but the
/// policy has no GPU dependency, so the tests use a trivial payload.
///
/// Eviction is **drop-only**: removing an entry drops its `T`, which for
/// `T2Texture` releases the VRAM reference (wgpu reclaims once no view is bound).
/// We never `Texture::destroy()` — see [`T2Texture`] for why a synchronous
/// destroy aborts the process on Vulkan.
struct T2Ring<T> {
    /// Pre-built payloads keyed by sequence frame number.
    map: std::collections::HashMap<u32, T>,
    /// The active layer the ring was built for; a change invalidates it.
    layer: usize,
    /// Max frames the ring may hold (VRAM-budgeted by the app). `0` disables it.
    cap: usize,
    /// The on-screen frame — never evicted (its texture is bound for paint).
    frame: Option<u32>,
}

impl<T> T2Ring<T> {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            layer: 0,
            cap: 0,
            frame: None,
        }
    }

    fn cap(&self) -> usize {
        self.cap
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn contains(&self, frame: u32) -> bool {
        self.map.contains_key(&frame)
    }

    fn get(&self, frame: u32) -> Option<&T> {
        self.map.get(&frame)
    }

    /// Set the on-screen frame (bound for paint, never evicted).
    fn set_frame(&mut self, frame: Option<u32>) {
        self.frame = frame;
    }

    /// Ring a payload for `frame`. Does not evict — the caller pairs this with
    /// [`Self::evict_to_cap`] so a freshly-built and a pre-built insert share one
    /// eviction pass.
    fn insert(&mut self, frame: u32, value: T) {
        self.map.insert(frame, value);
    }

    /// Drop a single frame's payload, if present — a re-rendered frame (#101).
    fn evict_frame(&mut self, frame: u32) {
        self.map.remove(&frame);
    }

    /// Drop the whole ring (new sequence / disabled / layer switch).
    fn clear(&mut self) {
        self.map.clear();
    }

    /// Invalidate the ring when the active layer changed (textures are
    /// per-layer). Returns whether it cleared.
    fn ensure_layer(&mut self, active: usize) -> bool {
        if self.layer != active {
            self.map.clear();
            self.layer = active;
            true
        } else {
            false
        }
    }

    /// Set the capacity; an unchanged cap is a no-op. Shrinking to `0` clears the
    /// ring (→ the lazy per-swap path); otherwise it evicts to the new cap.
    fn set_cap(&mut self, cap: usize) {
        if cap == self.cap {
            return;
        }
        self.cap = cap;
        if cap == 0 {
            self.map.clear();
        } else {
            self.evict_to_cap();
        }
    }

    /// Evict frames furthest from the on-screen frame until within the cap
    /// (floored at 1, so the on-screen frame always survives). Drop-only. Returns
    /// how many were evicted.
    fn evict_to_cap(&mut self) -> usize {
        let cap = self.cap.max(1);
        let mut evicted = 0;
        while self.map.len() > cap {
            let Some(victim) = t2_victim(self.map.keys().copied(), self.frame) else {
                break; // only the on-screen frame remains
            };
            self.map.remove(&victim);
            evicted += 1;
        }
        evicted
    }
}

/// Per-frame GPU draw context for the canvas (#152): the base uniforms plus the
/// persistent ring buffer, LUT/default bind groups, and the interior-mutable
/// per-frame accumulators (`uniform_offset` ring allocator, `overscan_factor`,
/// `ocio_sig`, `ocio_draws`). It replaces the old `draw_gpu` closure + its loose
/// `Cell`/`RefCell` captures, so [`ExrViewer::emit_mode_draws`] can dispatch the
/// compare modes as a plain method call and the OCIO tail can read the
/// accumulators back after the draws land.
struct DrawCtx<'a> {
    render_state: &'a eframe::egui_wgpu::RenderState,
    /// The per-frame base uniforms; [`Self::draw`] copies and overrides per draw.
    uniform_data: crate::gpu::Uniforms,
    /// Persistent uniform ring buffer (app-owned `GpuState`, #54).
    uniform_buffer: eframe::egui_wgpu::wgpu::Buffer,
    /// Padded slot stride (device `min_uniform_buffer_offset_alignment`).
    uniform_stride: u32,
    /// Active `.cube` LUT bind group (or `GpuState`'s default when none).
    active_lut_bg: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
    /// Fallback B-texture bind group when a draw has no B image.
    default_tex_bg: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
    /// Whether OCIO is active — draws accumulate into `ocio_draws` instead of
    /// emitting a per-call callback.
    ocio_active: bool,
    /// Force the accumulate path even when OCIO is off (#99 render-unify R2). The
    /// layer-stack composite always folds through the scene ping-pong (all blend
    /// modes need it); with OCIO off the callback's display stage is the sRGB
    /// display-encode pass instead of the OCIO transform. `false` for the A/B path,
    /// which uses the direct non-OCIO draw when OCIO is off.
    force_accumulate: bool,
    /// Ring allocator, bumped by each draw (below the reserved offscreen slot).
    uniform_offset: std::cell::Cell<u32>,
    /// Overscan dim factor for the next draw (`1.0` = none); set per branch.
    overscan_factor: std::cell::Cell<f32>,
    /// When set, the next draw runs with the global view ops (exposure, channel
    /// isolation) neutralized. Used for the lower layers of the layer-stack
    /// accumulate ping-pong: those ops are applied once, on the TOP layer, so the
    /// finished composite is exposed / channel-isolated exactly once rather than
    /// compounding per layer (`fs_main` applies both after the blend). `false` for
    /// every independent draw (single / wipe / side-by-side / the top composite draw).
    neutral_view_ops: std::cell::Cell<bool>,
    /// Per-draw blend override for the N-layer Layers-panel composite (#99 PR-B.3):
    /// each comp layer carries its own [`BlendMode`], unlike the single
    /// `self.blend_mode` the A/B composite bakes into the base uniform. `Some(b)`
    /// overrides `u.blend_mode` for the next draw; `None` (every A/B path) keeps the
    /// base uniform's blend.
    blend_override: std::cell::Cell<Option<BlendMode>>,
    /// Running FNV-1a hash of everything affecting the OCIO render, so the
    /// display transform is skipped on repaints that change nothing.
    ocio_sig: std::cell::Cell<u64>,
    /// Accumulated OCIO pass-1 draws; drained into one `OcioCallback` per frame.
    ocio_draws: std::cell::RefCell<Vec<crate::gpu::ocio_pass::OcioPass1Draw>>,
}

impl DrawCtx<'_> {
    /// Emit one image draw at `target_rect`, clipped to `clip_rect`. Under OCIO
    /// (and not diff) the draw is accumulated into `ocio_draws` for the single
    /// per-frame pass; otherwise an `ExrCallback` paints it immediately.
    #[allow(clippy::too_many_arguments)] // intrinsic to one placed GPU draw
    fn draw(
        &self,
        painter: &egui::Painter,
        bg_a: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
        bg_b_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
        clip_rect: egui::Rect,
        target_rect: egui::Rect,
        is_diff: bool,
        is_composite: bool,
        opacity: f32,
    ) {
        let mut u = self.uniform_data;
        u.rect_min = [target_rect.min.x, target_rect.min.y];
        u.rect_max = [target_rect.max.x, target_rect.max.y];
        u.is_diff_mode = if is_diff { 1 } else { 0 };
        u.is_composite = if is_composite { 1 } else { 0 };
        u.opacity = opacity;
        u.overscan_factor = self.overscan_factor.get();
        // Under OCIO the only `is_composite` draw is the layer-stack accumulate top
        // layer (composite folds through the scene ping-pong there — see `emit_mode_draws`
        // + `OcioCallback::accumulate`), whose `tex_b` is the screen-sized scene
        // accumulation. Flag it so the shader samples `tex_b` at screen coords, not the
        // image-local uv. The non-OCIO single-pass composite keeps `tex_b` as an image (0).
        // The accumulate path (OCIO on, or forced for the OCIO-off composite, R2).
        let accumulate = self.ocio_active || self.force_accumulate;
        u.composite_accum = if is_composite && accumulate { 1 } else { 0 };

        // Accumulate pass 1 must emit scene-linear, so bypass the built-in display
        // chain (sRGB/gamma/.cube LUT) — the display stage (OCIO transform, or the
        // sRGB display-encode when OCIO is off) applies it after. Exposure stays.
        if accumulate {
            u.srgb = 0;
            u.gamma = 1.0;
            u.enable_lut = 0;
            // Don't bake the checker into scene-linear; it's composited in display
            // space (blit pass) after the display stage.
            u.skip_checker = 1;
        }

        // Layer-stack accumulate: neutralize the global view ops on the lower
        // layers so exposure / channel isolation apply once (on the top layer) to
        // the finished composite instead of compounding per layer (see the field
        // doc). `fs_main` applies both after the blend, so the topmost draw over an
        // un-exposed accumulation yields exactly `composite × 2^EV`.
        if self.neutral_view_ops.get() {
            u.exposure = 0.0;
            u.channel_mode = 0; // ChannelMode::RGB
        }

        // Per-layer blend for the Layers-panel composite (#99 PR-B.3): the A/B
        // composite bakes a single `self.blend_mode` into the base uniform, but each
        // comp layer blends differently, so the emitter sets this per draw.
        if let Some(blend) = self.blend_override.get() {
            u.blend_mode = blend.as_u32();
        }

        let queue = &self.render_state.queue;

        // Write this draw's uniform data into the persistent ring buffer
        // at the current offset, then bump the allocator. This replaces
        // the per-draw `create_buffer_init` + `create_bind_group` (two
        // wgpu object allocations + a staging copy per draw per frame).
        // `uniform_stride` is padded to the device's
        // `min_uniform_buffer_offset_alignment` (typically 256), so every
        // dynamic offset is valid — the raw Uniforms struct (128 bytes)
        // is written at the start of each padded slot.
        // The viewport allocates slots below the reserved offscreen slot
        // (#148). On overflow, saturate to the last viewport slot instead
        // of writing past the ring: the overflowing draws share uniforms
        // (wrong image placement for one frame) but there's no wgpu
        // validation error mid-frame. Debug builds still assert so an
        // overflow is caught in development (relevant once #104's N-way
        // compare raises the per-frame draw count).
        let ring_cap = crate::gpu::UNIFORM_RING_OFFSCREEN_SLOT as u32 * self.uniform_stride;
        let mut offset = self.uniform_offset.get();
        if offset + self.uniform_stride > ring_cap {
            debug_assert!(
                false,
                "uniform ring buffer overflow: too many draws this frame"
            );
            log::error!(
                target: "floki::gpu",
                "uniform ring overflow: too many draws this frame; reusing the last slot"
            );
            offset = ring_cap - self.uniform_stride;
        }
        self.uniform_offset.set(offset + self.uniform_stride);
        queue.write_buffer(&self.uniform_buffer, offset as u64, bytemuck::bytes_of(&u));

        let bg_b = bg_b_opt.unwrap_or_else(|| self.default_tex_bg.clone());
        let final_clip_rect = painter.clip_rect().intersect(clip_rect);

        // Diff is a false-color heat-map visualization (display-space,
        // not color-managed), so it always uses the normal pipeline —
        // even under OCIO it is NOT accumulated into the OCIO pass.
        if accumulate && !is_diff {
            // Fold this draw's inputs (uniform bytes + texture pointers) into
            // the per-frame render signature; OcioCallback re-renders only
            // when this changes.
            let mut h = self.ocio_sig.get();
            for chunk in bytemuck::bytes_of(&u).chunks(8) {
                let mut b = [0u8; 8];
                b[..chunk.len()].copy_from_slice(chunk);
                h = (h ^ u64::from_le_bytes(b)).wrapping_mul(0x100000001b3);
            }
            for p in [
                std::sync::Arc::as_ptr(&bg_a) as *const () as u64,
                std::sync::Arc::as_ptr(&bg_b) as *const () as u64,
                std::sync::Arc::as_ptr(&self.active_lut_bg) as *const () as u64,
            ] {
                h = (h ^ p).wrapping_mul(0x100000001b3);
            }
            self.ocio_sig.set(h);

            // Accumulate; the single per-frame OcioCallback is emitted
            // after the draws so one OCIO pass covers the whole frame.
            self.ocio_draws
                .borrow_mut()
                .push(crate::gpu::ocio_pass::OcioPass1Draw {
                    bg_a,
                    bg_b,
                    uniform_offset: offset,
                    lut_bg: self.active_lut_bg.clone(),
                });
            return;
        }

        let callback = crate::gpu::ExrCallback {
            bg_a,
            bg_b,
            uniform_offset: offset,
            lut_bg: self.active_lut_bg.clone(),
        };
        painter.with_clip_rect(final_clip_rect).add(
            eframe::egui_wgpu::Callback::new_paint_callback(final_clip_rect, callback),
        );
    }
}

/// Persisted viewer display preferences, single-owned by [`ExrViewer`] (#151).
///
/// These used to be mirrored on `ExrApp` and cloned into the viewer before
/// `viewer.ui()` and read back after, every frame. Now the viewer owns them
/// outright; `ExrApp` keeps a nested `persisted_prefs` bridge that its eframe
/// `save()`/load path syncs to/from this struct at persist time only. Every
/// member type is already serde-ready.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ViewerPrefs {
    /// Diff visualization controls (#15): active colormap, magnitude metric,
    /// noise floor. Mutated by the mode-param UI.
    pub diff_colormap: Colormap,
    pub diff_metric: DiffMetric,
    pub diff_floor: f32,
    /// User-saved named gradients — the preset library shared with the gradient
    /// editor.
    pub custom_gradients: Vec<(String, Gradient)>,
    /// Customizable viewport background (#18); mutated by the background window.
    pub background: crate::background::Background,
    /// Named background presets (mode + colours + gradient).
    pub background_presets: Vec<(String, crate::background::Background)>,
    /// Anamorphic display (#179): when true, a non-1.0 EXR `pixelAspectRatio` is
    /// applied as a horizontal display stretch (unsqueeze) so anamorphic footage
    /// shows at its intended wide aspect. A no-op when PAR == 1.0. On by default,
    /// matching RV / Nuke / Baselight; toggling it off returns to raw square-pixel
    /// display.
    pub anamorphic_unsqueeze: bool,
}

impl Default for ViewerPrefs {
    fn default() -> Self {
        Self {
            diff_colormap: Colormap::BlackBody,
            diff_metric: DiffMetric::MaxChannel,
            diff_floor: 0.0,
            custom_gradients: Vec::new(),
            background: crate::background::Background::default(),
            background_presets: Vec::new(),
            anamorphic_unsqueeze: true,
        }
    }
}

/// All canvas state for one A/B pair: view transform, tone controls, the active
/// [`CompareMode`], the texture caches described in the module docs, plus
/// sampling/histogram/contact-sheet state. Driven each frame by [`Self::ui`].
pub struct ExrViewer {
    /// Per-layer **decimated** contact-sheet thumbnails (A/B). The CPU thumbnail
    /// bake ([`Self::generate_texture`]) is the headless / no-GPU fallback; when a
    /// GPU is present, `gpu_thumbnails` is used instead (#67). Cleared on a layer
    /// *count* change or any tone/OCIO change.
    thumbnails: Vec<Option<egui::TextureHandle>>,
    thumbnails_b: Vec<Option<egui::TextureHandle>>,
    /// GPU contact-sheet thumbnails (#67): per-layer `(egui TextureId, owned
    /// Rgba8Unorm target, full-res size)`. Used instead of `thumbnails` when a GPU
    /// is present and OCIO is off; the CPU `thumbnails` path is the headless / OCIO
    /// fallback. The owned `Texture` keeps the registered view alive; the
    /// `TextureId` must be `free_texture`d on eviction — deferred via
    /// `pending_thumb_frees` since the `egui_wgpu::Renderer` is only reachable from
    /// `draw_contact_sheet`. `_b` is the side-by-side B cache (symmetric).
    gpu_thumbnails: Vec<
        Option<(
            egui::TextureId,
            eframe::egui_wgpu::wgpu::Texture,
            egui::Vec2,
        )>,
    >,
    gpu_thumbnails_b: Vec<
        Option<(
            egui::TextureId,
            eframe::egui_wgpu::wgpu::Texture,
            egui::Vec2,
        )>,
    >,
    /// `TextureId`s awaiting `free_texture`, drained at the top of
    /// `draw_contact_sheet` (the only site with the renderer handle). Invalidation
    /// sites can't free directly (no `gpu_resources`), so they push here instead.
    pending_thumb_frees: Vec<egui::TextureId>,
    /// Cumulative contact-sheet thumbnail bakes, for the playback debug overlay
    /// (#144). Flat while the sheet is frozen during playback; steps up once per
    /// settle. A per-frame proxy for "the sheet isn't re-baking every frame".
    pub(crate) dbg_thumb_bakes: u64,
    /// The background the GPU thumbnails were baked with (#67). The backdrop is
    /// composited into the cached texture, but background edits (settings window /
    /// gradient editor / preset load) don't go through `invalidate_tone`; a
    /// signature compare in `draw_contact_sheet` re-renders the sheet when it
    /// changes, catching every mutation path.
    gpu_thumb_bg: Option<crate::background::Background>,
    gpu_textures: Vec<Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>>,
    gpu_textures_b: Vec<Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>>,

    /// T2 GPU-texture ring (#56): pre-built active-layer textures keyed by frame
    /// number, so a sequence frame swap binds an already-uploaded texture instead
    /// of re-packing + re-uploading on the UI thread. Valid only for its built
    /// layer; cleared on a layer switch. Empty / unused for a single image. The
    /// map policy (cap-shrink eviction, layer invalidation, on-screen protection)
    /// lives in the unit-tested [`T2Ring`]; the app drives it every frame via
    /// `set_t2_cap`/`set_t2_frame`/`prebuild_t2` (#153).
    ///
    /// **Per-`SourceId` (#99):** one ring per source, created lazily. Today the
    /// only keys are the A/B compare slots ([`Self::T2_SOURCE_A`] /
    /// [`Self::T2_SOURCE_B`]) — B mirrors A but keyed on B's frame number and built
    /// for B's layer (`active_layer` clamped to B's layer count), pre-uploading the
    /// compared sequence ahead of the playhead. An absent ring reads as disabled
    /// (cap/len 0). Phase 2 rings the comp stack's N sources under their own ids.
    t2_rings: std::collections::BTreeMap<crate::layer::SourceId, T2Ring<T2Texture>>,
    /// Reused staging buffer for the Rgba16Float pack (#142 U3): holds one
    /// layer's interleaved half bit-patterns, so `build_layer_texture` doesn't
    /// page-fault a fresh ~66 MB allocation every build during playback.
    t2_staging: Vec<u16>,
    /// Persisted display preferences, single-owned here (#151): diff controls,
    /// custom gradients, background + presets. `ExrApp` mirrors these to disk only
    /// at eframe `save()`/load time; the UI mutates them in place.
    pub prefs: ViewerPrefs,
    /// Baked 256-entry colormap LUT (f32 texels) + the colormap they were baked
    /// from, so the GPU texture is re-uploaded only when the active gradient
    /// changes. Transient (rebuilt on demand).
    colormap_lut: Vec<f32>,
    colormap_sig: Option<Colormap>,
    /// Transient gradient-editor window state. Shared by the diff colormap editor
    /// and the background gradient editor; `gradient_editor_target` says which.
    gradient_editor_open: bool,
    editing_gradient: Gradient,
    new_preset_name: String,
    gradient_editor_target: GradientTarget,

    /// Whether the background settings window is open, and the in-progress preset
    /// name. Transient.
    pub show_background_window: bool,
    new_bg_preset_name: String,
    /// Baked background-gradient LUT (f32 texels) + the ramp they were baked from,
    /// so the GPU texture is re-uploaded only when the gradient ramp changes.
    bg_gradient_lut: Vec<f32>,
    bg_gradient_sig: Option<Gradient>,
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
    pub ocio_active: bool,
    /// Monotonic generation of the OCIO display transform, set by the app each
    /// frame and bumped whenever the OCIO pass is rebuilt (config / display /
    /// view change). Together with `ocio_active` it forms the signature that
    /// invalidates contact-sheet thumbnails when the managed look changes — the
    /// replacement for the old `ocio_cpu` Rc-pointer identity (#59 removed the CPU
    /// OCIO processor).
    pub ocio_render_gen: u64,
    /// Last-applied `(ocio_active, ocio_render_gen)` signature; thumbnails are
    /// re-rendered when it changes. See [`Self::invalidate_thumbnails_on_ocio_change`].
    ocio_sig: u64,
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
    /// Manual anamorphic squeeze override (#179): when `Some(f)`, the display
    /// stretch uses `f` instead of the EXR header `pixelAspectRatio` (for footage
    /// with a missing/wrong PAR). Session-only — not persisted, since it is
    /// image-specific. Gated by [`ViewerPrefs::anamorphic_unsqueeze`].
    pub pixel_aspect_override: Option<f32>,
    pub last_hover_pos_img: Option<(usize, usize)>,
    pub last_sampled_val_a: Option<[f32; 4]>,
    pub last_sampled_val_b: Option<[f32; 4]>,
    /// When set (by the app from `Playback::sampling_suppressed`), the canvas
    /// pixel readout is suppressed: no sampling, the cached values are cleared so
    /// the status bar shows nothing stale, and a hover hint explains why
    /// (INV-SAMPLE, #7). Always false outside sequence playback.
    pub suppress_sampling: bool,

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

    /// The two-layer A/B comp stack (#114): slot A at the bottom, slot B on top.
    /// The existing `compare_mode` / `blend_mode` / `active_layer` fields remain
    /// the source of truth; each frame [`crate::render_program::resolve`]
    /// reconfigures this stack from them and reads its `composite_at`, so the draw
    /// paths dispatch off the layer model instead of branching on `CompareMode`
    /// inline. `layer_ids` are stable for the session — the cache-key seam toward
    /// `(LayerId, source_frame)` (`src/cache.rs`).
    layer_stack: crate::layer::LayerStack,
    layer_ids: (crate::layer::LayerId, crate::layer::LayerId),
}

impl Default for ExrViewer {
    fn default() -> Self {
        let (layer_stack, layer_ids) = crate::render_program::new_ab_stack();
        Self {
            thumbnails: Vec::new(),
            thumbnails_b: Vec::new(),
            gpu_thumbnails: Vec::new(),
            gpu_thumbnails_b: Vec::new(),
            pending_thumb_frees: Vec::new(),
            dbg_thumb_bakes: 0,
            gpu_thumb_bg: None,
            gpu_textures: Vec::new(),
            gpu_textures_b: Vec::new(),
            t2_rings: std::collections::BTreeMap::new(),
            t2_staging: Vec::new(),
            prefs: ViewerPrefs::default(),
            colormap_lut: Vec::new(),
            colormap_sig: None,
            gradient_editor_open: false,
            editing_gradient: Colormap::BlackBody.gradient(),
            new_preset_name: String::new(),
            gradient_editor_target: GradientTarget::DiffColormap,
            show_background_window: false,
            new_bg_preset_name: String::new(),
            bg_gradient_lut: Vec::new(),
            bg_gradient_sig: None,
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
            ocio_active: false,
            ocio_render_gen: 0,
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
            pixel_aspect_override: None,
            last_hover_pos_img: None,
            last_sampled_val_a: None,
            last_sampled_val_b: None,
            suppress_sampling: false,
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
            layer_stack,
            layer_ids,
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
                    self.thumbnails.fill(None);
                    self.thumbnails_b.fill(None);
                    self.invalidate_gpu_thumbnails(true, true);
                }
            }
        });

        if fullscreen_changed {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
    }

    /// Drain the GPU contact-sheet thumbnail caches (#67), queuing every
    /// registered `TextureId` for deferred `free_texture` (drained in
    /// `draw_contact_sheet`, which holds the renderer). Mirrors the A/B scoping of
    /// the CPU `thumbnails.fill(None)` sites: `a`/`b` select which side(s) to clear.
    fn invalidate_gpu_thumbnails(&mut self, a: bool, b: bool) {
        if a {
            for slot in self.gpu_thumbnails.iter_mut() {
                if let Some((id, _, _)) = slot.take() {
                    self.pending_thumb_frees.push(id);
                }
            }
        }
        if b {
            for slot in self.gpu_thumbnails_b.iter_mut() {
                if let Some((id, _, _)) = slot.take() {
                    self.pending_thumb_frees.push(id);
                }
            }
        }
    }

    /// Free queued GPU thumbnail texture ids (#67). Invalidation sites have no
    /// renderer handle, so they defer into `pending_thumb_frees`; this is the
    /// one place that holds the renderer. Runs every frame from [`Self::ui`]
    /// (and again inside the contact sheet after its own invalidations). With
    /// no GPU nothing was ever registered — just clear the queue.
    fn drain_thumb_frees(&mut self, gpu_resources: Option<&crate::gpu::GpuResources>) {
        if self.pending_thumb_frees.is_empty() {
            return;
        }
        if let Some(gpu) = gpu_resources {
            let mut renderer = gpu.render_state().renderer.write();
            for id in self.pending_thumb_frees.drain(..) {
                renderer.free_texture(&id);
            }
        } else {
            self.pending_thumb_frees.clear();
        }
    }

    /// Invalidate cached contact-sheet thumbnails whose pixels depend on the
    /// exposure / gamma / sRGB tone pipeline, so they regenerate next frame.
    /// (The central viewport is GPU-only and reads the live uniform each frame, so
    /// it needs no invalidation; only the baked thumbnails do.)
    /// `pub(crate)`: the app calls this when LUT state changes (toggle/reload,
    /// #147) — the LUT is baked into thumbnails exactly like exposure/gamma.
    pub(crate) fn invalidate_tone(&mut self) {
        self.thumbnails.fill(None);
        self.thumbnails_b.fill(None);
        // GPU thumbnails bake the tone into a cached texture (unlike the live
        // viewport uniform), so an exposure/gamma change must re-render them.
        self.invalidate_gpu_thumbnails(true, true);
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
                self.thumbnails.fill(None);
                self.thumbnails_b.fill(None);
                self.invalidate_gpu_thumbnails(true, true);
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

                ui.separator();
                // Anamorphic unsqueeze (#179): apply the EXR `pixelAspectRatio` as a
                // horizontal display stretch. The master toggle persists via
                // `ViewerPrefs`; the optional custom factor overrides the header PAR.
                ui.checkbox(&mut self.prefs.anamorphic_unsqueeze, "Unsqueeze anamorphic")
                    .on_hover_text(
                        "Stretch non-square-pixel (anamorphic) footage to its display aspect",
                    );
                ui.add_enabled_ui(self.prefs.anamorphic_unsqueeze, |ui| {
                    let header_par = exr_data.image.attributes.pixel_aspect;
                    let mut custom = self.pixel_aspect_override.is_some();
                    if ui
                        .checkbox(&mut custom, "Custom factor")
                        .on_hover_text("Override the header pixel aspect ratio")
                        .changed()
                    {
                        // Seed the override from the header PAR, or a common 2× squeeze
                        // when the header is square/absent.
                        let seed = if header_par > 0.0 && header_par != 1.0 {
                            header_par
                        } else {
                            2.0
                        };
                        self.pixel_aspect_override = custom.then_some(seed);
                    }
                    if let Some(factor) = self.pixel_aspect_override.as_mut() {
                        ui.add(egui::DragValue::new(factor).speed(0.01).range(0.1..=4.0));
                    } else {
                        ui.label(format!("Header PAR: {header_par}"));
                    }
                });
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
                    .selected_text(self.prefs.diff_colormap.label())
                    .show_ui(ui, |ui| {
                        for cm in Colormap::PRESETS {
                            if ui
                                .selectable_label(self.prefs.diff_colormap == cm, cm.label())
                                .clicked()
                            {
                                pick = Some(cm);
                            }
                        }
                        if !self.prefs.custom_gradients.is_empty() {
                            ui.separator();
                            for (name, g) in &self.prefs.custom_gradients {
                                let selected = matches!(&self.prefs.diff_colormap, Colormap::Custom(cur) if cur == g);
                                if ui.selectable_label(selected, name).clicked() {
                                    pick = Some(Colormap::Custom(g.clone()));
                                }
                            }
                        }
                    });
                if let Some(cm) = pick {
                    self.prefs.diff_colormap = cm;
                }

                ui.label("Metric");
                egui::ComboBox::from_id_salt("diff_metric_select")
                    .selected_text(self.prefs.diff_metric.label())
                    .show_ui(ui, |ui| {
                        for m in DiffMetric::ALL {
                            ui.selectable_value(&mut self.prefs.diff_metric, m, m.label());
                        }
                    });

                ui.label("Floor");
                ui.add(egui::Slider::new(&mut self.prefs.diff_floor, 0.0..=0.25));

                // Legend / scale bar. Per-channel RGB has no colormap, so skip it.
                if self.prefs.diff_metric != DiffMetric::PerChannelRGB {
                    self.diff_legend(ui);
                }

                if ui.button("Edit gradient…").clicked() {
                    self.editing_gradient = self.prefs.diff_colormap.gradient();
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
                        for mode in BlendMode::ALL {
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
        let grad = self.prefs.diff_colormap.gradient();
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
                egui::Stroke::new(1.0_f32, egui::Color32::from_gray(90)),
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
            GradientTarget::DiffColormap => s.prefs.diff_colormap = Colormap::Custom(grad),
            GradientTarget::Background => s.prefs.background.gradient = grad,
        };
        if apply {
            apply_to_target(self, self.editing_gradient.clone());
        }
        if save {
            let name = self.new_preset_name.trim().to_string();
            let grad = self.editing_gradient.clone();
            // The named-gradient library is shared by both editors.
            if let Some(slot) = self
                .prefs
                .custom_gradients
                .iter_mut()
                .find(|(n, _)| n == &name)
            {
                slot.1 = grad.clone();
            } else {
                self.prefs.custom_gradients.push((name, grad.clone()));
            }
            apply_to_target(self, grad);
            self.new_preset_name.clear();
        }
        self.gradient_editor_open = open;
    }

    /// The viewport-background settings window (issue #18): mode selector, the
    /// per-mode colour/size/gradient controls, and a named-preset library. Mutates
    /// `self.prefs.background` live; rendered once per frame from [`Self::ui`] when
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
                        .selected_text(self.prefs.background.mode.label())
                        .show_ui(ui, |ui| {
                            for m in BackgroundMode::ALL {
                                ui.selectable_value(&mut self.prefs.background.mode, m, m.label());
                            }
                        });
                });
                ui.separator();

                match self.prefs.background.mode {
                    BackgroundMode::Checkerboard => {
                        ui.horizontal(|ui| {
                            ui.label("Dark");
                            ui.color_edit_button_rgb(&mut self.prefs.background.checker_dark);
                            ui.label("Light");
                            ui.color_edit_button_rgb(&mut self.prefs.background.checker_light);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Cell size");
                            ui.add(
                                egui::Slider::new(
                                    &mut self.prefs.background.checker_size,
                                    2.0..=128.0,
                                )
                                .suffix(" px"),
                            );
                        });
                    }
                    BackgroundMode::Solid => {
                        ui.horizontal(|ui| {
                            ui.label("Colour");
                            ui.color_edit_button_rgb(&mut self.prefs.background.solid);
                        });
                    }
                    BackgroundMode::Gradient => {
                        // Preview bar of the current gradient.
                        Self::gradient_preview_bar(ui, &self.prefs.background.gradient);
                        ui.horizontal(|ui| {
                            ui.label("Angle");
                            ui.add(
                                egui::Slider::new(
                                    &mut self.prefs.background.gradient_angle,
                                    0.0..=360.0,
                                )
                                .suffix("°"),
                            );
                        });
                        if ui.button("Edit gradient…").clicked() {
                            self.editing_gradient = self.prefs.background.gradient.clone();
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
                        for (i, (name, preset)) in self.prefs.background_presets.iter().enumerate()
                        {
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
                    self.prefs.background = bg;
                }
                if let Some(i) = delete {
                    self.prefs.background_presets.remove(i);
                }
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.new_bg_preset_name);
                    let can_save = !self.new_bg_preset_name.trim().is_empty();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Save preset"))
                        .clicked()
                    {
                        let name = self.new_bg_preset_name.trim().to_string();
                        let bg = self.prefs.background.clone();
                        if let Some(slot) = self
                            .prefs
                            .background_presets
                            .iter_mut()
                            .find(|(n, _)| n == &name)
                        {
                            slot.1 = bg;
                        } else {
                            self.prefs.background_presets.push((name, bg));
                        }
                        self.new_bg_preset_name.clear();
                    }
                });
                if ui.button("Reset to default checker").clicked() {
                    self.prefs.background = crate::background::Background::default();
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
        scale: egui::Vec2,
    ) {
        // Per-axis scale `(scale * par, scale)`: dividing by it maps a screen
        // point back to *native* image pixels, so annotations are stored in
        // native space and stay anchored across an anamorphic squeeze/unsqueeze
        // toggle (#179).
        let scale = egui::vec2(scale.x.max(1e-6), scale.y.max(1e-6));
        let to_img = |pos: egui::Pos2| {
            [
                (pos.x - image_rect.min.x) / scale.x,
                (pos.y - image_rect.min.y) / scale.y,
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

    /// Sanitize a raw unsqueeze factor (#179): return `1.0` (no stretch) when the
    /// anamorphic toggle is off, or when the factor is non-finite or ≤ 0 — a
    /// malformed/absent header PAR must not collapse the image to a near-zero
    /// width, and the reciprocal used in screen↔image coordinate mapping must stay
    /// finite.
    fn sanitize_unsqueeze(&self, factor: f32) -> f32 {
        if !self.prefs.anamorphic_unsqueeze {
            return 1.0;
        }
        if factor.is_finite() && factor > 0.0 {
            factor
        } else {
            1.0
        }
    }

    /// Effective horizontal unsqueeze factor for the **primary (A)** image, whose
    /// header pixel aspect ratio is `header_par` (#179). The manual override wins
    /// over the header PAR when set. The reference (B) image is unsqueezed from its
    /// own header only (`sanitize_unsqueeze` on B's PAR in [`Self::ui`]), so a
    /// custom factor set to fix A does not distort a differently-squeezed B.
    fn unsqueeze_factor(&self, header_par: f32) -> f32 {
        self.sanitize_unsqueeze(self.pixel_aspect_override.unwrap_or(header_par))
    }

    /// Paint all committed annotations plus the in-progress shape. Text labels are
    /// drawn here too; the editable text field is a separate popup. `scale` is the
    /// per-axis screen scale `(scale * par, scale)`; the x component carries the
    /// anamorphic unsqueeze so shapes track the stretched image (#179).
    fn draw_annotations(&self, painter: &egui::Painter, image_rect: egui::Rect, scale: egui::Vec2) {
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
        scale: egui::Vec2,
    ) {
        let to_screen = |p: [f32; 2]| image_rect.min + egui::vec2(p[0] * scale.x, p[1] * scale.y);
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
    fn annotation_text_popup(
        &mut self,
        ui: &mut egui::Ui,
        image_rect: egui::Rect,
        scale: egui::Vec2,
    ) {
        let Some((pos, _)) = self.anno_text_edit.as_ref() else {
            return;
        };
        let screen = image_rect.min + egui::vec2(pos[0] * scale.x, pos[1] * scale.y);
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

    /// Re-render contact-sheet thumbnails when the OCIO managed look changes —
    /// toggled on/off (`ocio_active`) or a new config / display / view
    /// (`ocio_render_gen`, bumped by the app's `rebuild_ocio_pass`). A no-op while
    /// the signature is unchanged. Replaces the old `ocio_cpu` Rc-pointer identity
    /// trick now that the CPU OCIO processor is gone (#59).
    fn invalidate_thumbnails_on_ocio_change(&mut self) {
        // 0 is the OCIO-off sentinel; when active, mix the generation through an
        // odd multiplier (a bijection on u64, so distinct generations stay
        // distinct). The generation is >= 1 whenever OCIO is active (set by
        // `rebuild_ocio_pass` before `ocio_ready`), so the product is non-zero and
        // never collides with the off sentinel.
        let sig = if self.ocio_active {
            self.ocio_render_gen.wrapping_mul(0x100000001b3)
        } else {
            0
        };
        if sig != self.ocio_sig {
            self.ocio_sig = sig;
            self.thumbnails.fill(None);
            self.thumbnails_b.fill(None);
            // Toggling OCIO flips the GPU thumbnail backend (display shader vs the
            // OCIO two-pass); clear the GPU cache so stale thumbnails don't linger.
            self.invalidate_gpu_thumbnails(true, true);
        }
    }

    /// While blink is active (and B is loaded), alternate the displayed image
    /// between A and B on `blink_interval`, requesting repaints to keep cycling.
    fn apply_blink_mode(&mut self, ui: &egui::Ui, has_b: bool) {
        if self.blink_state && has_b {
            let time = ui.input(|i| i.time);
            let interval = f64::from(self.blink_interval);
            let phase = (time / interval) as usize;
            // Wake exactly at the next A/B flip instead of repainting at the
            // full refresh rate between flips (#146): the image only changes
            // every `blink_interval`, but a bare request_repaint re-ran the
            // whole frame (including the OCIO passes) continuously.
            let next_flip = (phase + 1) as f64 * interval;
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(
                    (next_flip - time).max(0.0),
                ));
            if phase.is_multiple_of(2) {
                self.compare_mode = CompareMode::SingleA;
            } else {
                self.compare_mode = CompareMode::SingleB;
            }
        }
    }

    /// Resize the per-layer thumbnail / GPU-texture caches to the current A/B
    /// layer counts, clearing them (forcing regeneration) whenever a count
    /// changes. `thumbnails` length is the sentinel (it tracks the layer count for
    /// both the CPU and GPU thumbnail caches).
    fn sync_texture_caches(&mut self, layer_count: usize, layer_count_b: usize) {
        if self.thumbnails.len() != layer_count {
            self.thumbnails.clear();
            self.thumbnails.resize(layer_count, None);
            // Queue any registered GPU thumbnail ids for deferred free before the
            // slots are dropped (the renderer is only reachable in the sheet draw).
            for (id, _, _) in self.gpu_thumbnails.drain(..).flatten() {
                self.pending_thumb_frees.push(id);
            }
            self.gpu_thumbnails.resize_with(layer_count, || None);
            self.gpu_textures.clear();
            self.gpu_textures.resize(layer_count, None);
        }
        if self.thumbnails_b.len() != layer_count_b {
            self.thumbnails_b.clear();
            self.thumbnails_b.resize(layer_count_b, None);
            for (id, _, _) in self.gpu_thumbnails_b.drain(..).flatten() {
                self.pending_thumb_frees.push(id);
            }
            self.gpu_thumbnails_b.resize_with(layer_count_b, || None);
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
        gpu_resources: Option<&crate::gpu::GpuResources>,
        lut_bg_opt: Option<&std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        // The GPU thumbnail path applies whenever a GPU is present — non-OCIO via
        // the display shader ([`generate`], Phase 1), OCIO via the two-pass
        // [`generate_ocio`] (Phase 2). Under OCIO it requires the `Rgba8Unorm`
        // thumbnail pass to be published (config loaded); until then the CPU
        // `thumbnails` cache is the fallback.
        let ocio_ready_for_gpu = !self.ocio_active
            || gpu_resources.is_some_and(crate::gpu::GpuResources::has_ocio_thumbnail_pass);
        let use_gpu = gpu_resources.is_some() && ocio_ready_for_gpu;

        // The checker backdrop is baked into each *non-OCIO* GPU thumbnail, but
        // background edits don't run through `invalidate_tone`. Re-render the sheet
        // when it changes (#122 review) — a signature compare catches the settings
        // window / gradient editor / preset-load paths alike. Queue happens before
        // the drain below so the stale ids are freed this frame. (OCIO thumbnails
        // skip the background composite, so they don't depend on it.)
        if use_gpu
            && !self.ocio_active
            && self.gpu_thumb_bg.as_ref() != Some(&self.prefs.background)
        {
            self.invalidate_gpu_thumbnails(true, true);
            self.gpu_thumb_bg = Some(self.prefs.background.clone());
        }

        // Drain any ids queued by the invalidations just above so they're freed
        // this frame (the steady-state drain runs in `ui()`, #148).
        self.drain_thumb_frees(gpu_resources);

        // Tone snapshot for the GPU thumbnail render. `enable_lut` now honours the
        // user `.cube` LUT: Phase 2 threads the real LUT bind group through to the
        // draw (non-OCIO display shader) / OCIO pass 1, so the thumbnail matches the
        // viewport. `lut_bg_opt` may be `Some` even while the LUT is disabled (a
        // file is loaded but the toggle is off); the generators bind it only when
        // `tone.enable_lut` is set, so passing it unconditionally is safe.
        let lut_ref = lut_bg_opt;
        let tone = crate::gpu::thumbnail::ThumbnailTone {
            exposure: self.exposure,
            gamma: self.gamma,
            srgb: self.srgb,
            enable_lut: self.enable_lut,
            channel_mode: self.channel_mode.as_u32(),
            lut_domain_min: self.lut_domain_min,
            lut_domain_max: self.lut_domain_max,
            background: self.prefs.background.clone(),
        };

        let draw_sheet = |viewer: &mut Self,
                          ui: &mut egui::Ui,
                          data: &crate::exr_loader::ExrData,
                          is_a: bool| {
            let l_count = data.logical_layers.len();
            egui::ScrollArea::vertical()
                .id_salt(if is_a { "sheet_a" } else { "sheet_b" })
                .show(ui, |ui| {
                    // Cap the thumbnails baked per side per frame so a burst
                    // (settle refresh, tone/OCIO/background wipe, first open)
                    // spreads over a few frames instead of stalling one (#144).
                    // Fresh per `ScrollArea::show`: an A/B compare sheet runs this
                    // closure once per side, so each side gets its own budget.
                    // Visible cells are served first; off-screen cells never
                    // consume it.
                    let mut bake_budget = THUMB_BAKES_PER_FRAME;
                    let mut needs_more = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(16.0, 16.0);
                        for i in 0..l_count {
                            // Reserve an EXACTLY uniform 256px cell (square thumb box
                            // + label strip) BEFORE deciding to bake — the size is
                            // independent of the thumbnail, so the scroll extent and
                            // layout stay stable while cells fill in over frames, and
                            // the rect drives the visibility test. Absolute geometry
                            // (fixed rects + paint_at) avoids the vertical "staircase"
                            // auto-layout produced from variable cell heights.
                            let thumb_box = THUMB_BOX as f32;
                            let label_height = 30.0;
                            let (cell_rect, response) = ui.allocate_exact_size(
                                egui::vec2(thumb_box, thumb_box + label_height),
                                egui::Sense::click(),
                            );

                            // Aspect from the layer's FULL-RES size, not the
                            // decimated thumbnail dims — `thumb_dims` can collapse a
                            // thin axis to 1px, distorting aspect and diverging from
                            // the GPU path (which reports full-res). Known before any
                            // bake so the placeholder is framed correctly too. (#122)
                            let aspect = data.logical_size(i).map_or(1.0, |(w, h)| {
                                if h > 0 { w as f32 / h as f32 } else { 1.0 }
                            });
                            let (fit_w, fit_h) = if aspect >= 1.0 {
                                (thumb_box, thumb_box / aspect)
                            } else {
                                (thumb_box * aspect, thumb_box)
                            };
                            let img_rect = egui::Rect::from_center_size(
                                egui::pos2(cell_rect.center().x, cell_rect.top() + thumb_box * 0.5),
                                egui::vec2(fit_w, fit_h),
                            );

                            // Bake lazily into the dedicated thumbnail cache (NOT the
                            // full-res `textures` slots — a closed sheet must not
                            // leave a low-res thumbnail in the CPU-fallback view), but
                            // only for visible cells and only up to the per-frame
                            // budget. A visible cell left un-baked requests another
                            // frame; off-screen cells wait until scrolled in. The GPU
                            // path (#67) renders through the display shader into a
                            // cached Rgba8Unorm texture; the CPU path is the headless
                            // / OCIO-not-ready fallback.
                            let visible = ui.is_rect_visible(cell_rect);
                            let image: Option<egui::Image<'_>> = if use_gpu {
                                let gpu = gpu_resources.expect("use_gpu implies gpu_resources");
                                let ocio_active = viewer.ocio_active;
                                let missing = if is_a {
                                    viewer.gpu_thumbnails[i].is_none()
                                } else {
                                    viewer.gpu_thumbnails_b[i].is_none()
                                };
                                if missing && visible {
                                    if bake_budget > 0 {
                                        let baked = if ocio_active {
                                            crate::gpu::thumbnail::generate_ocio(
                                                gpu, data, i, THUMB_BOX, &tone, lut_ref,
                                            )
                                        } else {
                                            crate::gpu::thumbnail::generate(
                                                gpu, data, i, THUMB_BOX, &tone, lut_ref,
                                            )
                                        };
                                        let ok = baked.is_some();
                                        if is_a {
                                            viewer.gpu_thumbnails[i] = baked;
                                        } else {
                                            viewer.gpu_thumbnails_b[i] = baked;
                                        }
                                        if ok {
                                            bake_budget -= 1;
                                            viewer.dbg_thumb_bakes += 1;
                                        }
                                    } else {
                                        needs_more = true;
                                    }
                                }
                                let slot = if is_a {
                                    viewer.gpu_thumbnails[i].as_ref()
                                } else {
                                    viewer.gpu_thumbnails_b[i].as_ref()
                                };
                                slot.map(|(id, _, size)| {
                                    egui::Image::new(egui::load::SizedTexture::new(*id, *size))
                                })
                            } else {
                                let missing = if is_a {
                                    viewer.thumbnails[i].is_none()
                                } else {
                                    viewer.thumbnails_b[i].is_none()
                                };
                                if missing && visible {
                                    if bake_budget > 0 {
                                        let baked = viewer.generate_texture(
                                            ui.ctx(),
                                            data,
                                            i,
                                            Some(THUMB_BOX),
                                        );
                                        let ok = baked.is_some();
                                        if is_a {
                                            viewer.thumbnails[i] = baked;
                                        } else {
                                            viewer.thumbnails_b[i] = baked;
                                        }
                                        if ok {
                                            bake_budget -= 1;
                                            viewer.dbg_thumb_bakes += 1;
                                        }
                                    } else {
                                        needs_more = true;
                                    }
                                }
                                let tex = if is_a {
                                    viewer.thumbnails[i].as_ref()
                                } else {
                                    viewer.thumbnails_b[i].as_ref()
                                };
                                tex.map(egui::Image::new)
                            };

                            let name = data
                                .logical_layers
                                .get(i)
                                .map(|l| l.name.as_str())
                                .unwrap_or("Unnamed");

                            // Image centered in the top square box; a neutral
                            // placeholder holds the cell (and reads as "loading")
                            // until it bakes.
                            match image {
                                Some(image) => image.paint_at(ui, img_rect),
                                None => {
                                    ui.painter().rect_filled(
                                        img_rect,
                                        4.0,
                                        ui.visuals().extreme_bg_color,
                                    );
                                }
                            }

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
                    });
                    // Visible cells still waiting on the budget: come back next frame
                    // to finish the burst (#144).
                    if needs_more {
                        ui.ctx().request_repaint();
                    }
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

        // Free queued GPU thumbnail ids every frame (#148): invalidations fire
        // with the contact sheet closed too (tone changes, frame swaps), and
        // draining only in the sheet draw kept one stale generation of
        // thumbnail textures in VRAM until the sheet was next opened.
        self.drain_thumb_frees(gpu_resources);

        self.invalidate_thumbnails_on_ocio_change();

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
            self.draw_contact_sheet(ui, exr_data, exr_data_b, gpu_resources, lut_bg_opt.as_ref());
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

            // Anamorphic unsqueeze factors (#179): A from its header
            // `pixelAspectRatio` plus the manual override; B from B's header only
            // (the override is A-specific, so it must not distort a differently-
            // squeezed reference). Both are `1.0` (a no-op) when the toggle is off
            // or the PAR is 1.0, so square-pixel footage is untouched.
            let par = self.unsqueeze_factor(exr_data.image.attributes.pixel_aspect);
            let par_b = exr_data_b
                .map(|d| self.sanitize_unsqueeze(d.image.attributes.pixel_aspect))
                .unwrap_or(1.0);

            if let Some(gpu) = gpu_resources {
                // A layer switch invalidates the per-frame T2 ring (textures are
                // per-layer). Do this before binding/building below. The ring is
                // per-`SourceId` (#99); the primary (A) is `T2_SOURCE_A`.
                let ring_a = self
                    .t2_rings
                    .entry(Self::T2_SOURCE_A)
                    .or_insert_with(T2Ring::new);
                ring_a.ensure_layer(self.active_layer);
                // T2 (#56): bind the on-screen frame's pre-built texture if it is
                // resident, so the swap is an instant bind, not a re-upload.
                if ring_a.cap() > 0
                    && let Some(frame) = ring_a.frame
                    && let Some(t2) = ring_a.get(frame)
                {
                    self.gpu_textures[self.active_layer] = Some(t2.bind_group.clone());
                }
                if self.gpu_textures[self.active_layer].is_none()
                    && let Some(t2) = Self::build_layer_texture(
                        gpu,
                        exr_data,
                        self.active_layer,
                        &mut self.t2_staging,
                    )
                {
                    self.gpu_textures[self.active_layer] = Some(t2.bind_group.clone());
                    // Cache the freshly-built texture into the T2 ring for the
                    // on-screen frame (a lazy first paint feeds the ring).
                    if ring_a.cap() > 0
                        && let Some(frame) = ring_a.frame
                    {
                        ring_a.insert(frame, t2);
                        ring_a.evict_to_cap();
                    }
                }
                if let Some(data_b) = exr_data_b {
                    let layer_b = self.t2_layer_for(data_b);
                    // The compared source (B) rings under `T2_SOURCE_B`. A layer
                    // switch invalidates its per-frame ring (per-layer).
                    let ring_b = self
                        .t2_rings
                        .entry(Self::T2_SOURCE_B)
                        .or_insert_with(T2Ring::new);
                    ring_b.ensure_layer(layer_b);
                    // B T2 (#166): bind the on-screen B frame's pre-built texture
                    // if resident, so a locked-step B swap is an instant bind, not
                    // a re-upload (mirrors A above).
                    if ring_b.cap() > 0
                        && let Some(frame) = ring_b.frame
                        && let Some(t2) = ring_b.get(frame)
                    {
                        self.gpu_textures_b[layer_b] = Some(t2.bind_group.clone());
                    }
                    if self.gpu_textures_b[layer_b].is_none()
                        && let Some(t2) =
                            Self::build_layer_texture(gpu, data_b, layer_b, &mut self.t2_staging)
                    {
                        self.gpu_textures_b[layer_b] = Some(t2.bind_group.clone());
                        // Lazy first paint feeds the B ring for the on-screen frame.
                        if ring_b.cap() > 0
                            && let Some(frame) = ring_b.frame
                        {
                            ring_b.insert(frame, t2);
                            ring_b.evict_to_cap();
                        }
                    }
                }
            }

            // Draw texture. GPU is the only render path (#59); without a GPU
            // (headless tests) nothing is uploaded, so the canvas draw is skipped.
            let has_texture = self.gpu_textures[self.active_layer].is_some();
            if has_texture {
                let (rect, response) =
                    ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());

                // Record the canvas rect (egui points) so the snapshot (#19) can
                // crop the framebuffer screenshot to just the image area.
                self.last_canvas_rect = Some(rect);

                // Fit-to-view (#179): frame the *unsqueezed* extents so "Frame (F)"
                // and the first-paint fit account for the wider anamorphic image.
                let fit_size = egui::vec2(tex_size.x * par, tex_size.y);
                let fit_size_b = tex_size_b.map(|b| egui::vec2(b.x * par_b, b.y));
                self.handle_canvas_interaction(ui, rect, &response, fit_size, fit_size_b);
                // Render Image — the x extent carries the anamorphic unsqueeze (#179).
                let image_size = egui::vec2(tex_size.x * self.scale * par, tex_size.y * self.scale);
                // Per-axis screen scale for all screen↔image coordinate mapping
                // (annotations, pixel readout): x includes the unsqueeze, y does not.
                let view_scale = egui::vec2(self.scale * par, self.scale);

                // Side-by-Side draws each image at its own offset position, so the
                // single centered overscan geometry below does not apply: skip the
                // overscan dimming pass and its annotations in that mode.
                let is_side_by_side = matches!(self.compare_mode, CompareMode::SideBySide);

                let disp_window = exr_data.image.attributes.display_window;
                let phys_idx = exr_data.logical_layers[self.active_layer].physical_index;
                let data_window_min = exr_data.image.layer_data[phys_idx]
                    .attributes
                    .layer_position;

                let disp_size = egui::vec2(
                    disp_window.size.x() as f32 * self.scale * par,
                    disp_window.size.y() as f32 * self.scale,
                );
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
                    (data_window_min.0 - disp_window.position.x()) as f32 * self.scale * par,
                    (data_window_min.1 - disp_window.position.y()) as f32 * self.scale,
                );

                let image_rect = egui::Rect::from_min_size(disp_rect.min + data_offset, image_size);

                // Annotation drawing (#45) consumes the canvas drag/click when a tool
                // is active (pan is suppressed above). Coordinates map through
                // `image_rect`/`scale` so shapes anchor to image pixels.
                if self.anno_tool.is_active() {
                    self.handle_annotation_input(&response, image_rect, view_scale);
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

                // Always-on display-window resolution ("format") readout — shown
                // for every image, unlike the overscan/bbox annotations below which
                // stay gated to data≠display. `disp_window` is full-res even for a
                // downsampled proxy (`downsampled()` preserves display_window), so
                // this never flickers on scrub.
                if !is_side_by_side {
                    unclipped_painter.text(
                        disp_rect.right_bottom() + egui::vec2(0.0, 5.0),
                        egui::Align2::RIGHT_TOP,
                        format!("{}x{}", disp_window.size.x(), disp_window.size.y()),
                        egui::FontId::proportional(12.0),
                        egui::Color32::from_gray(200),
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
                    par_b,
                };

                if let Some(gpu) = gpu_resources {
                    self.draw_canvas_gpu(ui, &layout, exr_data_b, gpu, lut_bg_opt);
                }

                // Annotation overlay on top of the image (and its in-progress shape).
                // Painted by egui, so it is included in the snapshot screenshot (#19).
                let anno_painter = ui.painter().with_clip_rect(rect);
                self.draw_annotations(&anno_painter, image_rect, view_scale);
                self.annotation_text_popup(ui, image_rect, view_scale);

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
                    tex_size_b, par, par_b,
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
        par: f32,
        par_b: f32,
    ) {
        // INV-SAMPLE (#7): while a sequence is advancing (or a seek's frame is
        // still decoding), suppress the readout — the displayed frame can lag the
        // playhead and sampling a full ~600 MB `ExrData` on every hover is
        // wasteful. Clear the cached sample so the status bar shows nothing stale,
        // and hint why. Re-enables automatically once paused and the frame lands.
        if self.suppress_sampling {
            self.last_hover_pos_img = None;
            self.last_sampled_val_a = None;
            self.last_sampled_val_b = None;
            if self.show_tooltip {
                response
                    .clone()
                    .on_hover_text_at_pointer("readout paused during playback / seek");
            }
            return;
        }

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
                    // B's x extent carries B's own unsqueeze (`par_b`); A's
                    // `image_size` already includes `par` from the caller (#179).
                    let mut image_size_b =
                        egui::vec2(tex_size_b.x * self.scale * par_b, tex_size_b.y * self.scale);
                    if self.normalize_side_by_side {
                        let scale_b = (tex_size.y * self.scale) / tex_size_b.y;
                        image_size_b =
                            egui::vec2(tex_size_b.x * scale_b * par_b, tex_size_b.y * scale_b);
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
                        // Map back to native pixels: x undoes the unsqueeze (#179).
                        hover_x = Some((local.x / (self.scale * par)) as usize);
                        hover_y = Some((local.y / self.scale) as usize);
                    } else if image_rect_b.contains(pos) {
                        let local = pos - image_rect_b.min;
                        let scale_b = if self.normalize_side_by_side {
                            (tex_size.y * self.scale) / tex_size_b.y
                        } else {
                            self.scale
                        };
                        hover_x = Some((local.x / (scale_b * par_b)) as usize);
                        hover_y = Some((local.y / scale_b) as usize);
                        hovered_b = true;
                    }
                }
            } else {
                let image_local_pos = pos - image_rect.min;
                if image_local_pos.x >= 0.0 && image_local_pos.y >= 0.0 {
                    // Map back to native pixels: x undoes the unsqueeze (#179).
                    hover_x = Some((image_local_pos.x / (self.scale * par)) as usize);
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

    /// Reconfigure the held A/B [`crate::layer::LayerStack`] from the current
    /// viewer state and resolve it into the [`crate::render_program::RenderProgram`]
    /// the draw paths dispatch on (#114). `effective_mode` is the compare mode
    /// *after* any blink override, so blink alternates A/B through the model like a
    /// normal `Single{A,B}` switch. This is the single seam where the draw paths
    /// read the layer model instead of branching on `CompareMode` directly.
    fn render_program(
        &mut self,
        effective_mode: CompareMode,
    ) -> crate::render_program::RenderProgram {
        let input = crate::render_program::ResolveInput {
            compare_mode: effective_mode,
            blend_mode: self.blend_mode,
            active_layer: self.active_layer,
            wipe_position: self.wipe_center[0],
        };
        crate::render_program::resolve(&mut self.layer_stack, self.layer_ids, &input)
    }

    /// GPU render path (the default): build per-draw uniforms and emit wgpu
    /// paint callbacks for the active compare mode. Under OCIO the per-image
    /// pass-1 draws are accumulated into a single display-transform pass. Also
    /// handles the wipe-handle drag/scroll interaction (hence `&mut self`).
    /// Re-bake and upload the diff-colormap and background-gradient LUTs when
    /// their ramps change. The GPU textures are updated in place (stable handles
    /// off the app-owned `GpuState`, #54), so no bind-group rebuild is needed.
    /// Split out of `draw_canvas_gpu` (#152) to keep it focused on draw emission.
    fn sync_gradient_luts(&mut self, gpu_resources: &crate::gpu::GpuResources) {
        let render_state = gpu_resources.render_state();
        let colormap_dirty = self.colormap_sig.as_ref() != Some(&self.prefs.diff_colormap);
        let bg_gradient_dirty =
            self.bg_gradient_sig.as_ref() != Some(&self.prefs.background.gradient);
        if colormap_dirty {
            self.colormap_lut = self
                .prefs
                .diff_colormap
                .gradient()
                .bake(crate::gradient::COLORMAP_LUT_SIZE);
            self.colormap_sig = Some(self.prefs.diff_colormap.clone());
        }
        if bg_gradient_dirty {
            self.bg_gradient_lut = self
                .prefs
                .background
                .gradient
                .bake(crate::gradient::COLORMAP_LUT_SIZE);
            self.bg_gradient_sig = Some(self.prefs.background.gradient.clone());
        }
        if colormap_dirty || bg_gradient_dirty {
            let gpu_state = gpu_resources.gpu_state.as_ref();
            if colormap_dirty {
                gpu_state.write_colormap(&render_state.queue, &self.colormap_lut);
            }
            if bg_gradient_dirty {
                gpu_state.write_bg_gradient(&render_state.queue, &self.bg_gradient_lut);
            }
        }
    }

    /// Assemble the per-frame base [`crate::gpu::Uniforms`] shared by every draw
    /// this frame; `draw_gpu` copies it and overrides the per-draw fields (rect,
    /// diff/composite flags, opacity, overscan). Pure — reads the viewer's tone,
    /// compare, LUT-domain and background state plus the frame geometry. Split out
    /// of `draw_canvas_gpu` (#152).
    fn build_frame_uniforms(
        &self,
        image_rect: egui::Rect,
        disp_rect: egui::Rect,
        screen_size: [f32; 2],
    ) -> crate::gpu::Uniforms {
        crate::gpu::Uniforms {
            rect_min: [image_rect.min.x, image_rect.min.y],
            rect_max: [image_rect.max.x, image_rect.max.y],
            screen_size,
            display_min: [disp_rect.min.x, disp_rect.min.y],
            display_max: [disp_rect.max.x, disp_rect.max.y],
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
            diff_metric: self.prefs.diff_metric.as_u32(),
            diff_floor: self.prefs.diff_floor,
            // Per-draw value comes from the `overscan_factor` cell in `draw_gpu`.
            overscan_factor: 1.0,
            lut_domain_min: self.lut_domain_min,
            lut_domain_max: self.lut_domain_max,
            bg_checker_dark: rgb3_to_vec4(self.prefs.background.checker_dark),
            bg_checker_light: rgb3_to_vec4(self.prefs.background.checker_light),
            bg_solid: rgb3_to_vec4(self.prefs.background.solid),
            bg_mode: self.prefs.background.mode.as_u32(),
            bg_grad_angle: self.prefs.background.gradient_angle,
            bg_checker_size: self.prefs.background.checker_size,
            // Set per-draw in `DrawCtx::draw` for the layer-stack accumulate top layer.
            composite_accum: 0,
        }
    }

    /// Wipe-mode handle interaction: drag the center handle to move the split,
    /// scroll while hovering it to rotate. Mutates `wipe_center`/`wipe_angle`.
    /// Split out of `draw_canvas_gpu` (#152).
    fn handle_wipe_interaction(&mut self, ui: &egui::Ui, image_rect: egui::Rect) {
        let center_screen = egui::pos2(
            image_rect.min.x + image_rect.width() * self.wipe_center[0],
            image_rect.min.y + image_rect.height() * self.wipe_center[1],
        );
        let handle_rect = egui::Rect::from_center_size(center_screen, egui::vec2(24.0, 24.0));
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

    /// Dispatch the active compare mode's draws through `ctx` (#152), one per
    /// `Arrangement` arm: Stacked (single / composite), Wipe (one shader draw plus
    /// the handle overlay), SideBySide (two placed draws plus the divider), and
    /// Diff. Extracted from the old `draw_all` closure. `p`/`opac` are the target
    /// painter and layer opacity; `bg_a` is the active-layer A bind group.
    #[allow(clippy::too_many_arguments)] // frame ctx + program + geometry + painter
    fn emit_mode_draws(
        &self,
        ctx: &DrawCtx,
        program: &crate::render_program::RenderProgram,
        layout: &CanvasLayout,
        exr_data_b: Option<&ExrData>,
        bg_a: &std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
        p: &egui::Painter,
        opac: f32,
    ) {
        let CanvasLayout {
            rect,
            image_rect,
            image_size,
            tex_size,
            tex_size_b,
            par_b,
            ..
        } = *layout;
        // Slot-B GPU bind group for the active layer, clamped to B's count.
        let pick_b = || {
            exr_data_b.and_then(|d| {
                self.gpu_textures_b[self
                    .active_layer
                    .min(d.logical_layers.len().saturating_sub(1))]
                .clone()
            })
        };
        match program.arrangement {
            // Single (one draw) and Composite (A over B — see the stacking-order note
            // below).
            render_program::Arrangement::Stacked => {
                if program.is_composite {
                    if let Some(bg_b) = pick_b() {
                        if ctx.ocio_active {
                            // OCIO path: fold the composite through the scene ping-pong
                            // (OcioCallback::accumulate). Order preserves today's look —
                            // A renders over B (#99 decision): bottom = B (a plain copy,
                            // is_composite=0), top = A over the accumulation
                            // (is_composite=1, the blend rides A). `tex_b` on the top draw
                            // is ignored by the ping-pong (it binds the prior accumulation),
                            // so pass B to keep the render signature reacting to B changing.
                            //
                            // Exposure / channel isolation are global view ops: neutralize
                            // them on the bottom (B) layer so they apply once, on the top
                            // (A) layer, to the finished composite instead of compounding.
                            ctx.neutral_view_ops.set(true);
                            ctx.draw(p, bg_b.clone(), None, rect, image_rect, false, false, opac);
                            ctx.neutral_view_ops.set(false);
                            ctx.draw(
                                p,
                                bg_a.clone(),
                                Some(bg_b),
                                rect,
                                image_rect,
                                false,
                                true,
                                opac,
                            );
                        } else {
                            // Non-OCIO: a single 2-input pass (no offscreen to ping-pong
                            // through) — A over B in the blend shader, unchanged.
                            ctx.draw(
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
                } else if let Some(draw) = program.draws.first() {
                    match draw.input {
                        render_program::ProgramInput::A => {
                            ctx.draw(p, bg_a.clone(), None, rect, image_rect, false, false, opac);
                        }
                        render_program::ProgramInput::B => {
                            if let Some(bg_b) = pick_b() {
                                ctx.draw(p, bg_b, None, rect, image_rect, false, false, opac);
                            }
                        }
                    }
                }
            }
            render_program::Arrangement::Wipe { .. } => {
                let bg_b_opt = pick_b();
                // Single draw call: the shader handles the wipe split. Bind the
                // real B texture so the shader can sample it when is_wipe_mode is
                // set; falls back to the default texture if no B image is loaded.
                ctx.draw(
                    p,
                    bg_a.clone(),
                    bg_b_opt,
                    rect,
                    image_rect,
                    false,
                    false,
                    opac,
                );

                // Draw the rotated wipe line and handle.
                let center_screen = egui::pos2(
                    image_rect.min.x + image_rect.width() * self.wipe_center[0],
                    image_rect.min.y + image_rect.height() * self.wipe_center[1],
                );
                let angle_rad = self.wipe_angle.to_radians();
                // Line direction is perpendicular to the normal (cos, sin).
                let dir = egui::vec2(-angle_rad.sin(), angle_rad.cos());
                let max_dist = image_rect.width().hypot(image_rect.height());
                let p1 = center_screen + dir * max_dist;
                let p2 = center_screen - dir * max_dist;

                let alpha = (self.wipe_line_opacity * 255.0) as u8;
                let color = egui::Color32::from_white_alpha(alpha);

                p.line_segment([p1, p2], (2.0, color));
                p.circle_filled(center_screen, 8.0, color);
            }
            render_program::Arrangement::SideBySide => {
                let bg_b_opt = pick_b();
                if let (Some(bg_b), Some(size_b)) = (bg_b_opt, tex_size_b) {
                    // B's x extent carries B's own unsqueeze (`par_b`); A's
                    // `image_size` already includes `par` from the caller (#179).
                    let mut image_size_b =
                        egui::vec2(size_b.x * self.scale * par_b, size_b.y * self.scale);
                    if self.normalize_side_by_side {
                        let scale_b = (tex_size.y * self.scale) / size_b.y;
                        image_size_b = egui::vec2(size_b.x * scale_b * par_b, size_b.y * scale_b);
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

                    ctx.draw(
                        p,
                        bg_a.clone(),
                        None,
                        rect,
                        image_rect_a,
                        false,
                        false,
                        opac,
                    );
                    ctx.draw(p, bg_b, None, rect, image_rect_b, false, false, opac);
                    p.line_segment(
                        [
                            egui::pos2(image_rect_b.min.x, combined_rect.min.y),
                            egui::pos2(image_rect_b.min.x, combined_rect.max.y),
                        ],
                        (2.0, egui::Color32::GRAY),
                    );
                } else {
                    ctx.draw(p, bg_a.clone(), None, rect, image_rect, false, false, opac);
                }
            }
            render_program::Arrangement::Diff => {
                if let Some(bg_b) = pick_b() {
                    ctx.draw(
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
        }
    }

    /// Render the Layers-panel composite (#99 PR-B.3): fold `draws` bottom→top
    /// through the OCIO scene ping-pong (the PR-A accumulate path), reusing
    /// [`DrawCtx`] + [`crate::gpu::ocio_pass::OcioCallback`] verbatim — only the
    /// draw *source* differs (the panel's N sources instead of A/B). `base_size` is
    /// the bottom layer's pixel size; every layer currently draws at that one canvas
    /// rect (per-layer placement is the follow-up, #102/#104), so a differently-
    /// sized layer is stretched to fit for now.
    ///
    /// Requires OCIO active (the ping-pong lives on the OCIO path) + a GPU; the
    /// caller ([`crate::app::ExrApp::draw_comp_central`]) gates on both and shows a
    /// fallback message otherwise. A no-op when `draws` is empty. Global tone
    /// (exposure / gamma / channel isolation / background) comes from the shared
    /// viewer fields, so it applies to the composite exactly as to a single image;
    /// per-composite tone controls land with the row controls (PR-B.4).
    pub(crate) fn draw_comp_composite(
        &mut self,
        ui: &mut egui::Ui,
        base_size: (usize, usize),
        draws: &[CompDraw],
        gpu_resources: &crate::gpu::GpuResources,
        lut_bg_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        if draws.is_empty() {
            return;
        }
        let render_state = gpu_resources.render_state();
        let (bw, bh) = base_size;
        let tex_size = egui::vec2(bw.max(1) as f32, bh.max(1) as f32);

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        self.last_canvas_rect = Some(rect);
        // Shared pan/zoom with the A/B view (the scale/translation fields are the
        // same); framing on first paint fits the base layer (no B extent).
        self.handle_canvas_interaction(ui, rect, &response, tex_size, None);

        let image_size = egui::vec2(tex_size.x * self.scale, tex_size.y * self.scale);
        let image_rect = egui::Rect::from_center_size(rect.center() + self.translation, image_size);
        // The comp stack has no separate display window yet: display == image.
        let disp_rect = image_rect;
        self.last_image_rect = Some(image_rect);

        // Re-bake + upload the gradient LUTs on ramp change (stable handles otherwise).
        self.sync_gradient_luts(gpu_resources);

        let content = ui.ctx().content_rect();
        let mut uniform_data =
            self.build_frame_uniforms(image_rect, disp_rect, [content.width(), content.height()]);
        // The composite is a stack, never a wipe — regardless of the A/B compare_mode
        // the shared `build_frame_uniforms` reads.
        uniform_data.is_wipe_mode = 0;

        let gpu_state = gpu_resources.gpu_state.as_ref();
        let ctx = DrawCtx {
            render_state,
            uniform_data,
            uniform_buffer: gpu_state.uniform_buffer.clone(),
            uniform_stride: gpu_state.uniform_stride,
            active_lut_bg: lut_bg_opt.unwrap_or_else(|| gpu_state.default_lut_bind_group.clone()),
            default_tex_bg: gpu_state.default_tex_bind_group.clone(),
            ocio_active: self.ocio_active,
            force_accumulate: true,
            uniform_offset: std::cell::Cell::new(0u32),
            overscan_factor: std::cell::Cell::new(1.0f32),
            neutral_view_ops: std::cell::Cell::new(false),
            blend_override: std::cell::Cell::new(None),
            ocio_sig: std::cell::Cell::new(0xcbf29ce484222325u64),
            ocio_draws: std::cell::RefCell::new(Vec::new()),
        };

        let painter = ui.painter().with_clip_rect(rect);
        let n = draws.len();
        for (i, d) in draws.iter().enumerate() {
            let (is_composite, is_top) = comp_layer_flags(i, n);
            // Neutralize the global view ops on every layer but the top, so exposure
            // / channel isolation apply once to the finished composite (PR-A.4).
            ctx.neutral_view_ops.set(!is_top);
            // The bottom layer is a plain copy (blend unused); layers above carry
            // their own blend.
            ctx.blend_override
                .set(if is_composite { Some(d.blend) } else { None });
            // `tex_b` on an accumulate draw is the prior accumulation (bound by the
            // ping-pong itself), so pass None here — the shader samples the scene
            // target in screen space via `composite_accum`.
            ctx.draw(
                &painter,
                d.bind_group.clone(),
                None,
                rect,
                image_rect,
                false, // is_diff
                is_composite,
                d.opacity,
            );
        }
        ctx.neutral_view_ops.set(false);
        ctx.blend_override.set(None);

        let ocio_draws = std::mem::take(&mut *ctx.ocio_draws.borrow_mut());
        if ocio_draws.is_empty() {
            // `force_accumulate` means every drawable layer accumulated; empty here
            // only if `draws` had no bindable layer — nothing to composite.
            return;
        }
        let blit_uniforms = crate::gpu::BlitUniforms {
            display_min: [disp_rect.min.x, disp_rect.min.y],
            display_max: [disp_rect.max.x, disp_rect.max.y],
            screen_size: [content.width(), content.height()],
            overscan_factor: 1.0,
            bg_mode: self.prefs.background.mode.as_u32() as f32,
            bg_checker_size: self.prefs.background.checker_size,
            bg_grad_angle: self.prefs.background.gradient_angle,
            gamma: self.gamma,
            _pad_b: 0.0,
            bg_checker_dark: rgb3_to_vec4(self.prefs.background.checker_dark),
            bg_checker_light: rgb3_to_vec4(self.prefs.background.checker_light),
            bg_solid: rgb3_to_vec4(self.prefs.background.solid),
        };
        // Fold the display stage into the signature (comp path): toggling OCIO leaves
        // pass-1's scene-linear uniforms — hence `ocio_sig` — unchanged, and the
        // Enable-OCIO checkbox doesn't bump `ocio_render_gen`, so without this a toggle
        // would keep the stale cached `display_view` from the prior mode (the OCIO
        // transform vs the OCIO-off sRGB display-encode).
        let display_stage_salt = if self.ocio_active {
            0
        } else {
            0x9E37_79B9_7F4A_7C15
        };
        let render_sig = (ctx.ocio_sig.get() ^ self.ocio_render_gen ^ display_stage_salt)
            .wrapping_mul(0x100000001b3);
        let scissor_pts = Some([
            image_rect.min.x,
            image_rect.min.y,
            image_rect.max.x,
            image_rect.max.y,
        ]);
        let callback = crate::gpu::ocio_pass::OcioCallback {
            draws: ocio_draws,
            accumulate: true,
            // OCIO off → the display stage is the sRGB display-encode pass (R2).
            use_display_encode: !self.ocio_active,
            display_format: render_state.target_format,
            blit_uniforms,
            scissor_pts,
            render_sig,
        };
        painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
            painter.clip_rect(),
            callback,
        ));
    }

    fn draw_canvas_gpu(
        &mut self,
        ui: &egui::Ui,
        layout: &CanvasLayout,
        exr_data_b: Option<&ExrData>,
        gpu_resources: &crate::gpu::GpuResources,
        lut_bg_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        let render_state = gpu_resources.render_state();
        // `image_size`/`tex_size`/`tex_size_b` are pulled from `layout` inside
        // `emit_mode_draws`; here we only need the frame rects.
        let CanvasLayout {
            rect,
            disp_rect,
            image_rect,
            ..
        } = *layout;
        let unclipped_painter = ui.painter().with_clip_rect(rect);
        let painter = ui.painter().with_clip_rect(rect.intersect(disp_rect));

        // Re-bake + upload the diff colormap and background gradient LUTs only
        // when their ramps change (stable GPU handles, no bind-group rebuild).
        self.sync_gradient_luts(gpu_resources);

        // GPU RENDER PATH: the per-frame base uniforms shared by every draw.
        let content = ui.ctx().content_rect();
        let uniform_data =
            self.build_frame_uniforms(image_rect, disp_rect, [content.width(), content.height()]);

        // Build the per-frame draw context: the persistent uniform ring buffer
        // + LUT/default bind groups from the app-owned `GpuState` (#54, no
        // per-frame renderer typemap lookup), plus the interior-mutable per-frame
        // accumulators. `DrawCtx::draw` writes each draw into the ring buffer at a
        // dynamic offset — no per-draw `create_buffer_init` + `create_bind_group`.
        let gpu_state = gpu_resources.gpu_state.as_ref();
        let ctx = DrawCtx {
            render_state,
            uniform_data,
            uniform_buffer: gpu_state.uniform_buffer.clone(),
            uniform_stride: gpu_state.uniform_stride,
            active_lut_bg: lut_bg_opt.unwrap_or_else(|| gpu_state.default_lut_bind_group.clone()),
            default_tex_bg: gpu_state.default_tex_bind_group.clone(),
            ocio_active: self.ocio_active,
            force_accumulate: false,
            uniform_offset: std::cell::Cell::new(0u32),
            overscan_factor: std::cell::Cell::new(1.0f32),
            neutral_view_ops: std::cell::Cell::new(false),
            blend_override: std::cell::Cell::new(None),
            ocio_sig: std::cell::Cell::new(0xcbf29ce484222325u64),
            ocio_draws: std::cell::RefCell::new(Vec::new()),
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

            // Wipe interaction (drag the handle to move, scroll to rotate).
            if self.compare_mode == CompareMode::Wipe {
                self.handle_wipe_interaction(ui, image_rect);
            }

            // Resolve the layer model into the render program the draw paths
            // dispatch on. `comp_mode` already folds in the blink override, so
            // blink alternates A/B through the model.
            let program = self.render_program(comp_mode);

            let is_sbs = matches!(program.arrangement, render_program::Arrangement::SideBySide);

            // OCIO path: one pass over the whole frame. Accumulate the pass-1
            // draws (draw_gpu pushes into ocio_draws) and emit a single
            // OcioCallback. The checker + overscan dim are applied post-OCIO in
            // the blit, so there is no separate dim draw here. Diff opts out: it
            // renders a display-space heat map via the normal pipeline (see
            // draw_gpu), so OCIO never runs for it.
            let ocio_handled = if self.ocio_active
                && !matches!(program.arrangement, render_program::Arrangement::Diff)
            {
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

                self.emit_mode_draws(
                    &ctx,
                    &program,
                    layout,
                    exr_data_b,
                    &bg_a,
                    &unclipped_painter,
                    1.0,
                );

                let draws = std::mem::take(&mut *ctx.ocio_draws.borrow_mut());
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
                        bg_mode: self.prefs.background.mode.as_u32() as f32,
                        bg_checker_size: self.prefs.background.checker_size,
                        bg_grad_angle: self.prefs.background.gradient_angle,
                        // Re-apply the user gamma in display space (#93): under
                        // OCIO the main shader runs with gamma=1 (OCIO owns the
                        // display chain), so the control would otherwise be inert.
                        gamma: self.gamma,
                        _pad_b: 0.0,
                        bg_checker_dark: rgb3_to_vec4(self.prefs.background.checker_dark),
                        bg_checker_light: rgb3_to_vec4(self.prefs.background.checker_light),
                        bg_solid: rgb3_to_vec4(self.prefs.background.solid),
                    };
                    // Finalize the render signature with the OCIO generation
                    // (bumped by the app on any config / display / view change), so
                    // switching the managed look forces a re-render.
                    let render_sig =
                        (ctx.ocio_sig.get() ^ self.ocio_render_gen).wrapping_mul(0x100000001b3);
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
                        // Composite folds its draws through the scene ping-pong; every
                        // other arrangement renders its draws in the single pass-1 loop.
                        accumulate: program.is_composite,
                        // The A/B path emits this callback only under OCIO, so it always
                        // takes the OCIO display transform, never the display-encode pass.
                        use_display_encode: false,
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

            if !ocio_handled {
                // One draw for everything (#146): the shader blends fragments
                // outside the display window at `overscan_factor`, so the old
                // dim pre-pass (which re-ran the full fragment shader over the
                // whole display window every repaint) is gone. Side-by-Side
                // renders at full brightness with the full-canvas clip; with
                // opacity 0 the overscan is hidden, so keep the display-window
                // scissor and skip the dim entirely.
                if is_sbs {
                    self.emit_mode_draws(
                        &ctx,
                        &program,
                        layout,
                        exr_data_b,
                        &bg_a,
                        &unclipped_painter,
                        1.0,
                    );
                } else if self.overscan_opacity > 0.0 {
                    ctx.overscan_factor.set(self.overscan_opacity);
                    self.emit_mode_draws(
                        &ctx,
                        &program,
                        layout,
                        exr_data_b,
                        &bg_a,
                        &unclipped_painter,
                        1.0,
                    );
                } else {
                    self.emit_mode_draws(&ctx, &program, layout, exr_data_b, &bg_a, &painter, 1.0);
                }
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
        staging: &mut Vec<u16>,
    ) -> Option<T2Texture> {
        let render_state = gpu_resources.render_state();
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        use eframe::egui_wgpu::wgpu;
        let device = &render_state.device;
        let queue = &render_state.queue;

        // Choose the upload format from the source channel types (#142). EXR
        // beauty data is overwhelmingly F16: packing + uploading it as
        // Rgba16Float is lossless and halves the bandwidth and VRAM vs
        // Rgba32Float. A present F32/U32 channel keeps 32F to preserve precision.
        // Both bind as `texture_2d<f32>` under the same filterable:false layout,
        // so nothing downstream (shader, sampler, bind group) changes.
        let use_f16 = crate::pixels::all_channels_f16([r_chan, g_chan, b_chan, a_chan]);
        let has_alpha = a_chan.is_some();
        let format = if use_f16 {
            wgpu::TextureFormat::Rgba16Float
        } else {
            wgpu::TextureFormat::Rgba32Float
        };

        let extent = wgpu::Extent3d {
            width: width as u32,
            height: height as u32,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Exr GPU Texture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let dst = wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        };
        // Row stride: 4 channels × the format's bytes-per-channel (2 for 16F, 4 for 32F).
        let buf_layout = |bytes_per_channel: usize| wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some((width * 4 * bytes_per_channel) as u32),
            rows_per_image: Some(height as u32),
        };

        // Pack rows in parallel (a 4K layer is ~8M pixels — single-threaded was a
        // noticeable stall on layer switch).
        if use_f16 {
            // Fast path (#142 U2): copy half bit-patterns straight from the source
            // slices — `to_bits` is a reinterpret, so this skips the per-pixel
            // `to_f32` widening the F32 path pays. Absent channels default: rgb → 0
            // (half `0x0000`), alpha → 1.0.
            let r_s = crate::pixels::f16_slice(r_chan);
            let g_s = crate::pixels::f16_slice(g_chan);
            let b_s = crate::pixels::f16_slice(b_chan);
            let a_s = crate::pixels::f16_slice(a_chan);
            let one = f16::from_f32(1.0).to_bits();
            // Reuse the per-viewer staging buffer (#142 U3): every element is
            // overwritten below, so the `resize` fill is inert and — during
            // playback of a fixed-size sequence — a no-op, avoiding a fresh
            // ~66 MB page-faulted allocation per build.
            staging.resize(width * height * 4, 0);
            let pixels = staging.as_mut_slice();
            pixels
                .par_chunks_mut(width * 4)
                .enumerate()
                .for_each(|(y, row)| {
                    for x in 0..width {
                        let idx = y * width + x;
                        let i = x * 4;
                        row[i] = r_s.map_or(0, |s| s[idx].to_bits());
                        row[i + 1] = g_s.map_or(0, |s| s[idx].to_bits());
                        row[i + 2] = b_s.map_or(0, |s| s[idx].to_bits());
                        row[i + 3] = if has_alpha {
                            a_s.map_or(one, |s| s[idx].to_bits())
                        } else {
                            one
                        };
                    }
                });
            queue.write_texture(dst, bytemuck::cast_slice(&*pixels), buf_layout(2), extent);
        } else {
            // F32/U32 sources: hoist the F32 slices (direct index) and widen the
            // rest per pixel via `sample_channel`.
            let r_s = sample_channel_f32(r_chan);
            let g_s = sample_channel_f32(g_chan);
            let b_s = sample_channel_f32(b_chan);
            let a_s = sample_channel_f32(a_chan);
            let mut pixels = vec![0.0f32; width * height * 4];
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
            queue.write_texture(dst, bytemuck::cast_slice(&pixels), buf_layout(4), extent);
        }

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

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

    /// Build a standalone GPU texture + bind group for one AOV of an `ExrData`,
    /// for the Layers-panel composite sources (#99 PR-B.2). Wraps
    /// [`Self::build_layer_texture`] (all the F16/F32 packing logic) with a fresh
    /// staging buffer and hands back just the two GPU handles the caller owns — the
    /// `Texture` (kept to own the VRAM) and the `Arc<BindGroup>` (bound as a
    /// composite layer). `None` if the AOV is out of range or the upload fails.
    /// UI-thread only (`queue.write_texture`). `add` is rare, so the throwaway
    /// staging `Vec` here — unlike the per-frame T2 path — is not worth pooling.
    pub(crate) fn build_source_texture(
        gpu_resources: &crate::gpu::GpuResources,
        exr_data: &ExrData,
        aov: usize,
    ) -> Option<(
        eframe::egui_wgpu::wgpu::Texture,
        std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
    )> {
        let mut staging = Vec::new();
        let t = Self::build_layer_texture(gpu_resources, exr_data, aov, &mut staging)?;
        Some((t.texture, t.bind_group))
    }

    // --- T2 GPU-texture ring (#56) -------------------------------------------

    /// T2-ring source ids while the app is A/B-pinned (#99): the primary (A) and
    /// the compared follower (B), mirroring `ExrApp::{A,B}_SOURCE`. Phase 2 rings
    /// the comp stack's N sources under their own ids.
    const T2_SOURCE_A: crate::layer::SourceId = crate::layer::SourceId(0);
    const T2_SOURCE_B: crate::layer::SourceId = crate::layer::SourceId(1);

    /// `source`'s T2 ring, created empty on first use. Mutating entry points
    /// (`set_t2_cap`, `set_t2_frame`) go through here; reads (`t2_cap`, `t2_len`)
    /// tolerate an absent ring as "disabled".
    fn t2_ring_mut(&mut self, source: crate::layer::SourceId) -> &mut T2Ring<T2Texture> {
        self.t2_rings.entry(source).or_insert_with(T2Ring::new)
    }

    /// The layer a source's T2 ring builds for: the active layer clamped to the
    /// source's own layer count. A no-op for the primary (its active layer is
    /// always in range); the clamp only bites a differently-shaped compared source
    /// (#98 Phase 1 / #99). Kept as one helper so the ring's `ensure_layer` key,
    /// the GPU build, and the bind all agree.
    fn t2_layer_for(&self, exr_data: &ExrData) -> usize {
        self.active_layer
            .min(exr_data.logical_layers.len().saturating_sub(1))
    }

    /// Set the VRAM-budgeted T2 capacity (frames) for `source`. `0` disables
    /// pre-upload and drops that ring → the lazy per-swap path. Shrinking evicts
    /// immediately. Called every frame from `tick_budgets` — an unchanged cap is a
    /// no-op. The app splits the VRAM budget across active sources (#166/#99), so
    /// each source derives its own count from its own resolution.
    pub(crate) fn set_t2_cap(&mut self, source: crate::layer::SourceId, cap: usize) {
        self.t2_ring_mut(source).set_cap(cap);
    }

    /// Tell the viewer which sequence frame of `source` is on screen, so `ui()`
    /// binds its T2 texture. `None` for a single image / non-sequence (lazy path).
    pub(crate) fn set_t2_frame(&mut self, source: crate::layer::SourceId, frame: Option<u32>) {
        self.t2_ring_mut(source).set_frame(frame);
    }

    /// `source`'s current T2 capacity in frames (`0` = disabled / no ring).
    pub(crate) fn t2_cap(&self, source: crate::layer::SourceId) -> usize {
        self.t2_rings.get(&source).map_or(0, |r| r.cap())
    }

    /// Number of GPU textures currently resident in `source`'s T2 ring
    /// (instrumentation).
    pub(crate) fn t2_len(&self, source: crate::layer::SourceId) -> usize {
        self.t2_rings.get(&source).map_or(0, |r| r.len())
    }

    /// Pre-build the T2 texture for `(frame, source's layer)` and ring it under
    /// `source`, evicting to the cap. Returns `true` if it actually built (so the
    /// caller can amortize uploads across frames). No-op — returns `false` — when
    /// disabled, already resident, or the build fails. UI-thread only. Pass frames
    /// already resident in that source's T1 cache; T2 never triggers a decode. The
    /// ring bookkeeping is [`T2Ring`]'s; only the GPU build stays here.
    pub(crate) fn prebuild_t2(
        &mut self,
        source: crate::layer::SourceId,
        gpu: &crate::gpu::GpuResources,
        exr_data: &ExrData,
        frame: u32,
    ) -> bool {
        let layer = self.t2_layer_for(exr_data);
        let ring = self.t2_rings.entry(source).or_insert_with(T2Ring::new);
        if ring.cap() == 0 {
            return false;
        }
        ring.ensure_layer(layer);
        if ring.contains(frame) {
            return false;
        }
        let Some(t2) = Self::build_layer_texture(gpu, exr_data, layer, &mut self.t2_staging) else {
            return false;
        };
        ring.insert(frame, t2);
        ring.evict_to_cap();
        true
    }

    /// Drop a single frame's T2 texture, if present. Used by the render-watch
    /// (#101) so a re-rendered frame's stale GPU texture is released and rebuilt
    /// from the fresh decode. Drop-only (no `destroy()`): if this frame is the one
    /// on screen, the bound bind group keeps the old texture alive until the next
    /// paint rebinds the fresh one — no in-flight draw is ever invalidated.
    pub(crate) fn evict_t2_frame(&mut self, source: crate::layer::SourceId, frame: u32) {
        if let Some(ring) = self.t2_rings.get_mut(&source) {
            ring.evict_frame(frame);
        }
    }

    /// Drop every T2 texture in `source`'s ring (new sequence / disabled / layer
    /// switch / source dropped). Drop-only: the on-screen frame's texture stays
    /// alive through its still-bound bind group (cloned into `gpu_textures*`) and is
    /// freed by wgpu once that binding is replaced — critically, this clear can run
    /// *before* the central panel rebinds for the just-advanced frame, so the bound
    /// frame may differ from the ring's on-screen frame; dropping is safe for
    /// either, a `destroy()` is not.
    pub(crate) fn clear_t2(&mut self, source: crate::layer::SourceId) {
        if let Some(ring) = self.t2_rings.get_mut(&source) {
            ring.clear();
        }
    }

    /// CPU contact-sheet thumbnail bake: decimate `layer_index` to the thumbnail
    /// box and bake it into an [`egui::TextureHandle`] with the channel-select →
    /// exposure → gamma → sRGB tone pipeline. This is the **headless / no-GPU
    /// fallback** for the contact sheet (used by tests and when no GPU is
    /// present); with a GPU, thumbnails render through [`crate::gpu::thumbnail`]
    /// (OCIO included). #59 removed the CPU viewport render and the CPU OCIO
    /// processor, so this is non-OCIO only and `max_dim` is always the thumb box.
    fn generate_texture(
        &self,
        ctx: &egui::Context,
        exr_data: &ExrData,
        layer_index: usize,
        max_dim: Option<usize>,
    ) -> Option<egui::TextureHandle> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;
        // A zero-sized layer (malformed EXR) has no pixels to bake and would
        // underflow the `width - 1` / `height - 1` source clamps below.
        if width == 0 || height == 0 {
            return None;
        }
        // Decimate to the thumbnail box (`max_dim`). See [`thumb_dims`].
        let (out_w, out_h, stride) = thumb_dims(width, height, max_dim);

        let mut pixels = vec![egui::Color32::BLACK; out_w * out_h];

        // Hoist all loop-invariant scalars out of the per-pixel work.
        let exp_mult = crate::render_math::exposure_to_multiplier(self.exposure);
        // Viewport background (issue #18): one config, sampled per pixel below so
        // every CPU composite path agrees with the GPU `background_color`.
        let bg_cfg = &self.prefs.background;
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

    pub(crate) fn sample_pixel(
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

    /// Drop the cached image-B **viewport** bind groups so the compare draws
    /// rebuild from the newly swapped B data. In locked-step A/B playback (#98)
    /// this runs on every B frame swap (how the next B frame paints); split from
    /// the thumbnail clear so B playback doesn't re-bake the contact sheet every
    /// frame (mirror the A #144 split).
    pub fn invalidate_reference_viewport(&mut self) {
        self.gpu_textures_b.fill(None);
    }

    /// Drop the cached image-B contact-sheet **thumbnails** (CPU + GPU). Skipped
    /// while the transport is busy (`ExrApp::thumbs_suppressed`) during B playback,
    /// refreshed on settle (#98/#144).
    pub fn invalidate_reference_thumbnails(&mut self) {
        self.thumbnails_b.fill(None);
        self.invalidate_gpu_thumbnails(false, true);
    }

    /// Drop every cached reference-image (B) texture so the viewport rebuilds from the
    /// newly loaded data. The caches otherwise only refresh when the layer *count*
    /// changes, so re-loading a different B with the same layer count would keep showing the
    /// stale image. Clears the GPU bind groups and the contact-sheet thumbnails (CPU + GPU).
    pub fn invalidate_reference_textures(&mut self) {
        self.invalidate_reference_thumbnails();
        self.invalidate_reference_viewport();
    }

    /// Drop the cached image-A **viewport** bind groups so the central canvas
    /// rebuilds from the newly swapped data. This is the half of the A swap that
    /// must run on *every* frame — it's how the next sequence frame actually
    /// paints. Split from the thumbnail clear so playback can rebuild the viewport
    /// per frame without re-baking the contact sheet every swap (#144).
    pub fn invalidate_active_viewport(&mut self) {
        self.gpu_textures.fill(None);
    }

    /// Drop the cached image-A contact-sheet **thumbnails** (CPU + GPU) so the
    /// sheet re-bakes from the newly swapped data. Skipped while the transport is
    /// busy (`ExrApp::thumbs_suppressed`) and run once on settle (#144), so the
    /// sheet freezes during playback instead of re-baking every layer per frame.
    pub fn invalidate_active_thumbnails(&mut self) {
        self.thumbnails.fill(None);
        self.invalidate_gpu_thumbnails(true, false);
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
        tex_size_b: Option<egui::Vec2>,
    ) {
        if self.first_frame {
            // Fit the whole visible layout, not just A: Side-by-Side is wider than
            // the A image (A + B), so framing on `tex_size` alone clips B.
            let fit = framing_bounds(
                self.compare_mode,
                self.normalize_side_by_side,
                tex_size,
                tex_size_b,
            );
            let scale_x = rect.width() / fit.x;
            let scale_y = rect.height() / fit.y;
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
        // Proxy first-paint is always the single slot-A image (no B), so framing
        // has no combined layout to fit.
        self.handle_canvas_interaction(ui, rect, &response, tex_size, None);

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
        let bg_cfg = &self.prefs.background;
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
    use super::{BlendMode, ChannelMode, CompareMode, ExrViewer};
    use crate::annotation::{Annotation, AnnotationKind, AnnotationTool};
    use crate::exr_loader::ExrData;
    use crate::render_program;
    use eframe::egui;
    use egui_kittest::Harness;
    use exr::prelude::*;

    #[test]
    fn comp_layer_flags_bottom_copies_top_applies_view_ops() {
        use super::comp_layer_flags;
        // Single layer: it is both the bottom (plain copy, is_composite=false) and the
        // top (applies view ops once).
        assert_eq!(comp_layer_flags(0, 1), (false, true));
        // Four-layer stack bottom→top: only i==0 is a copy; only i==3 is the top.
        assert_eq!(
            comp_layer_flags(0, 4),
            (false, false),
            "bottom: copy, not top"
        );
        assert_eq!(
            comp_layer_flags(1, 4),
            (true, false),
            "middle blends, not top"
        );
        assert_eq!(
            comp_layer_flags(2, 4),
            (true, false),
            "middle blends, not top"
        );
        assert_eq!(
            comp_layer_flags(3, 4),
            (true, true),
            "top blends + view ops"
        );
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

    // The T2 ring policy (#153), tested with a trivial `()` payload — the map
    // policy has no GPU dependency, so these run headless like every other test.
    // Sorted resident frames, to assert which survived eviction.
    fn resident(ring: &super::T2Ring<()>) -> Vec<u32> {
        let mut keys: Vec<u32> = ring.map.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn t2ring_evicts_furthest_and_protects_on_screen() {
        let mut ring: super::T2Ring<()> = super::T2Ring::new();
        ring.set_cap(3);
        ring.set_frame(Some(5));
        for f in [3, 4, 5, 6, 9] {
            ring.insert(f, ());
        }
        assert_eq!(ring.evict_to_cap(), 2, "over cap by 2");
        // Furthest from 5 (9, then 3) go first; the on-screen frame is kept.
        assert_eq!(resident(&ring), vec![4, 5, 6]);
        assert!(ring.contains(5), "on-screen frame is never evicted");
    }

    #[test]
    fn t2ring_shrinking_cap_evicts_down_immediately() {
        let mut ring: super::T2Ring<()> = super::T2Ring::new();
        ring.set_cap(6);
        ring.set_frame(Some(10));
        for f in [10, 11, 12, 13, 20, 21] {
            ring.insert(f, ());
        }
        ring.set_cap(2); // external memory pressure lowers the cap
        assert_eq!(ring.len(), 2, "shrink evicts on the cap change, not later");
        assert!(ring.contains(10), "on-screen frame survives the shrink");
    }

    #[test]
    fn t2ring_cap_zero_clears_and_disables() {
        let mut ring: super::T2Ring<()> = super::T2Ring::new();
        ring.set_cap(4);
        ring.set_frame(Some(1));
        for f in 0..4 {
            ring.insert(f, ());
        }
        ring.set_cap(0);
        assert_eq!(ring.len(), 0, "cap 0 drops the whole ring");
        assert_eq!(ring.cap(), 0);
    }

    #[test]
    fn t2ring_evict_to_cap_floors_at_one() {
        // evict_to_cap is only reached with cap >= 1 in production, but the floor
        // is the safety belt: even at cap 0 the on-screen texture (bound for
        // paint) must survive rather than be freed mid-frame.
        let mut ring: super::T2Ring<()> = super::T2Ring::new();
        ring.cap = 0; // force the degenerate path directly
        ring.set_frame(Some(2));
        for f in [1, 2, 3] {
            ring.insert(f, ());
        }
        ring.evict_to_cap();
        assert_eq!(resident(&ring), vec![2], "floors at the on-screen frame");
    }

    #[test]
    fn t2ring_layer_switch_clears_else_noops() {
        let mut ring: super::T2Ring<()> = super::T2Ring::new();
        ring.set_cap(4);
        for f in 0..3 {
            ring.insert(f, ());
        }
        assert!(!ring.ensure_layer(0), "same layer: no clear");
        assert_eq!(ring.len(), 3);
        assert!(ring.ensure_layer(1), "layer change invalidates the ring");
        assert_eq!(ring.len(), 0);
        assert!(!ring.ensure_layer(1), "stays put on the new layer");
    }

    #[test]
    fn framing_bounds_fits_the_combined_layout_only_in_side_by_side() {
        use super::framing_bounds;
        let a = egui::vec2(1920.0, 1080.0);
        let b = egui::vec2(1000.0, 2000.0);
        // Every non-SBS mode frames the A image, regardless of B or normalize.
        assert_eq!(framing_bounds(CompareMode::SingleA, true, a, Some(b)), a);
        assert_eq!(framing_bounds(CompareMode::Composite, false, a, Some(b)), a);
        assert_eq!(framing_bounds(CompareMode::DiffMatte, true, a, Some(b)), a);
        // SBS with no B loaded falls back to the single image.
        assert_eq!(framing_bounds(CompareMode::SideBySide, true, a, None), a);
        // SBS unnormalized: combined width, tallest height.
        assert_eq!(
            framing_bounds(CompareMode::SideBySide, false, a, Some(b)),
            egui::vec2(2920.0, 2000.0)
        );
        // SBS normalized: B scaled to A's height (1080) → width 1000*1080/2000 = 540.
        let f = framing_bounds(CompareMode::SideBySide, true, a, Some(b));
        assert!((f.x - 2460.0).abs() < 0.01, "combined width {f:?}");
        assert!(
            (f.y - 1080.0).abs() < 0.01,
            "equal heights when normalized {f:?}"
        );
    }

    #[test]
    fn unsqueeze_factor_gates_on_toggle_and_override() {
        let mut v = ExrViewer::default();

        // Toggle on (the default), no override: the header PAR is used verbatim,
        // and PAR 1.0 is a no-op for square-pixel footage.
        assert!(v.prefs.anamorphic_unsqueeze);
        assert_eq!(v.unsqueeze_factor(2.0), 2.0);
        assert_eq!(v.unsqueeze_factor(1.0), 1.0);

        // Manual override wins over the header PAR while the toggle is on — but
        // only for the primary (A) image. The reference (B) image is unsqueezed
        // from its own header via `sanitize_unsqueeze`, which ignores the override,
        // so a custom factor set to fix A does not distort a differently-squeezed B.
        v.pixel_aspect_override = Some(1.33);
        assert_eq!(v.unsqueeze_factor(2.0), 1.33);
        assert_eq!(v.sanitize_unsqueeze(2.0), 2.0);

        // Toggle off returns raw (1.0) regardless of header PAR or override.
        v.prefs.anamorphic_unsqueeze = false;
        assert_eq!(v.unsqueeze_factor(2.0), 1.0);
        assert_eq!(v.unsqueeze_factor(1.0), 1.0);
        assert_eq!(v.sanitize_unsqueeze(2.0), 1.0);

        // A degenerate factor (0, negative, or NaN — e.g. a malformed/absent
        // header PAR) falls back to 1.0 (no stretch) instead of collapsing the
        // image to a near-zero width.
        v.prefs.anamorphic_unsqueeze = true;
        v.pixel_aspect_override = Some(0.0);
        assert_eq!(v.unsqueeze_factor(2.0), 1.0);
        v.pixel_aspect_override = None;
        assert_eq!(v.unsqueeze_factor(0.0), 1.0);
        assert_eq!(v.unsqueeze_factor(-2.0), 1.0);
        assert_eq!(v.unsqueeze_factor(f32::NAN), 1.0);
    }

    #[test]
    fn t2ring_evict_frame_drops_one_and_ignores_absent() {
        let mut ring: super::T2Ring<()> = super::T2Ring::new();
        ring.set_cap(4);
        for f in [7, 8, 9] {
            ring.insert(f, ());
        }
        ring.evict_frame(8); // a re-rendered frame
        assert_eq!(resident(&ring), vec![7, 9]);
        ring.evict_frame(100); // absent: no-op, no panic
        assert_eq!(ring.len(), 2);
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

    /// The #114 seam: `render_program` must plumb the live viewer fields
    /// (compare_mode, blend_mode, active_layer, wipe_center) into the layer model
    /// and resolve them through `LayerStack::composite_at`. The pure mapping is
    /// covered in `render_program`'s own tests; this guards the field wiring.
    #[test]
    fn render_program_plumbs_viewer_state_into_the_layer_model() {
        let mut v = ExrViewer::default();

        // SingleA (the default) solos slot A: one stacked draw of A.
        let p = v.render_program(CompareMode::SingleA);
        assert_eq!(p.arrangement, render_program::Arrangement::Stacked);
        assert_eq!(p.draws.len(), 1);
        assert_eq!(p.draws[0].input, render_program::ProgramInput::A);
        assert!(!p.is_composite);

        // Composite plumbs blend_mode onto the top (B) draw + flags is_composite,
        // and active_layer threads into every draw's AOV.
        v.blend_mode = BlendMode::Screen;
        v.active_layer = 2;
        let p = v.render_program(CompareMode::Composite);
        assert!(p.is_composite);
        assert_eq!(p.draws.len(), 2);
        assert_eq!(
            (p.draws[1].input, p.draws[1].blend),
            (render_program::ProgramInput::B, BlendMode::Screen)
        );
        assert!(p.draws.iter().all(|d| d.aov == 2), "active_layer → aov");

        // Wipe carries wipe_center[0] as the split position.
        v.wipe_center = [0.3, 0.5];
        let p = v.render_program(CompareMode::Wipe);
        assert_eq!(
            p.arrangement,
            render_program::Arrangement::Wipe { position: 0.3 }
        );

        // DiffMatte is the diff inspection — two inputs, never a composite.
        let p = v.render_program(CompareMode::DiffMatte);
        assert_eq!(p.arrangement, render_program::Arrangement::Diff);
        assert!(!p.is_composite);
        assert_eq!(p.draws.len(), 2);
    }

    struct SmokeState {
        viewer: ExrViewer,
        a: ExrData,
        b: Option<ExrData>,
    }

    /// Drive `ExrViewer::ui` headless (no GPU `render_state`) with a loaded A and
    /// B across every compare mode plus the contact sheet, asserting it lays out
    /// without panicking. Without a GPU the central canvas is not drawn (#59 made
    /// rendering GPU-only), so this exercises the non-render seams: state
    /// transitions, contact sheet, pixel sampling, overlays.
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

    /// The headless contact sheet (no GPU) bakes per-layer thumbnails into the
    /// CPU `thumbnails` cache. This is the no-GPU fallback that survived the #59
    /// CPU-viewport-render deletion; with a real GPU the sheet uses
    /// `gpu_thumbnails` instead (`crate::gpu::thumbnail`).
    #[test]
    fn headless_contact_sheet_bakes_cpu_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.exr");
        write_rgba_exr(&pa);
        let a = ExrData::load(&pa).unwrap();

        let mut h = Harness::new_ui_state(
            |ui, s: &mut SmokeState| {
                let SmokeState { viewer, a, b } = s;
                viewer.ui(ui, a, b.as_ref(), None, None);
            },
            SmokeState {
                viewer: ExrViewer::default(),
                a,
                b: None,
            },
        );

        // Open the sheet and lay it out. (egui_kittest's first frame is a sizing
        // pass that doesn't run the ScrollArea content; a second frame bakes it.)
        h.state_mut().viewer.show_contact_sheet = true;
        h.run();
        h.run();
        assert!(
            h.state().viewer.thumbnails[0].is_some(),
            "headless sheet baked into the CPU thumbnail cache"
        );

        // Re-baking is idempotent: a cleared slot refills next layout pass.
        h.state_mut().viewer.thumbnails.fill(None);
        h.run();
        h.run();
        assert!(
            h.state().viewer.thumbnails[0].is_some(),
            "thumbnail cache refills after invalidation"
        );
    }

    /// #144: the mechanism that lets playback freeze the sheet — a populated
    /// cache is NOT re-baked on redraw, and only an explicit invalidation (what
    /// the app does once on settle) triggers a fresh bake. The `dbg_thumb_bakes`
    /// counter is the same signal the debug overlay shows.
    #[test]
    fn contact_sheet_does_not_rebake_until_invalidated() {
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.exr");
        write_rgba_exr(&pa);
        let a = ExrData::load(&pa).unwrap();

        let mut h = Harness::new_ui_state(
            |ui, s: &mut SmokeState| {
                let SmokeState { viewer, a, b } = s;
                viewer.ui(ui, a, b.as_ref(), None, None);
            },
            SmokeState {
                viewer: ExrViewer::default(),
                a,
                b: None,
            },
        );

        // Open + lay out (first frame sizes, second bakes the ScrollArea content).
        h.state_mut().viewer.show_contact_sheet = true;
        h.run();
        h.run();
        let baked = h.state().viewer.dbg_thumb_bakes;
        assert!(baked > 0, "sheet baked at least one thumbnail");

        // Freeze: with the cache populated and no invalidation, further redraws
        // must NOT re-bake — this is exactly what lets the app skip invalidation
        // every playback frame swap.
        h.run();
        h.run();
        assert_eq!(
            h.state().viewer.dbg_thumb_bakes,
            baked,
            "a populated sheet does not re-bake on redraw (the #144 freeze)"
        );

        // Settle refresh: the one invalidation the app issues on settle re-bakes.
        h.state_mut().viewer.invalidate_active_thumbnails();
        h.run();
        h.run();
        assert!(
            h.state().viewer.dbg_thumb_bakes > baked,
            "an invalidation re-bakes on the next layout pass"
        );
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
