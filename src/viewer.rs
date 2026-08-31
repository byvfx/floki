//! The image canvas: pan/zoom, channel/exposure/gamma controls, the six
//! `Arrangement`s, pixel sampling, histogram and contact sheet. State lives in
//! [`ExrViewer`]; `draw_comp_composite` is the per-frame entry point.
//!
//! # Texture generation
//!
//! Rendering is **GPU-only** (#59 removed the CPU viewport render path; the app
//! requires a GPU and aborts without one):
//! [`Self::build_layer_texture`](ExrViewer::build_layer_texture) uploads a
//! layer's RGBA into a bind group; `gpu/shader.wgsl` then applies channel
//! isolation, exposure, gamma, sRGB **and every arrangement** (wipe / diff /
//! composite) in-shader, so a single generator serves them all. Under OCIO the
//! display chain is the two-pass OCIO callback ([`crate::gpu::ocio_pass`])
//! instead.
//!
//! **Where a built texture lives.** Not in the viewer. [`LayerTexture`] is this
//! module's own return type and does not cross the boundary:
//! `build_source_texture` destructures it into a
//! `(wgpu::Texture, Arc<wgpu::BindGroup>)` pair, and `ExrApp::CompSource` stores
//! those as its own `texture` / `bind_group` fields — one pair per source.
//! Followers build on the upload worker pool ([`crate::tex_upload`], #202) and are
//! bound by `ExrApp::collect_comp_textures` when the workers finish; a still
//! tracks AOV switches through `ExrApp::ensure_comp_aov`. There is no per-layer
//! texture cache in `ExrViewer` any more: the `gpu_textures` vector it used to
//! keep was written and never read, and went with the A/B viewport in #302.
//!
//! The only CPU bake that remains is [`Self::generate_texture`] for
//! contact-sheet thumbnails — the **headless / no-GPU fallback** (used by tests
//! and when no GPU is present). With a GPU, thumbnails render through
//! [`crate::gpu::thumbnail`] (OCIO included). Thumbnail caches invalidate on a
//! layer-count change and on an OCIO-state change
//! ([`ExrViewer::invalidate_thumbnails_on_ocio_change`]).

use crate::annotation::{Annotation, AnnotationKind, AnnotationTool};
use crate::exr_loader::ExrData;
use crate::gradient::{Colormap, DiffMetric, Gradient};
use eframe::egui;
use exr::prelude::f16;
use rayon::prelude::*;

/// Widen a linear RGB triple to the `vec4` the GPU uniforms expect (w unused).
fn rgb3_to_vec4(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

thread_local! {
    /// Per-thread interleave scratch for [`ExrViewer::build_layer_texture`] (#202).
    ///
    /// Thread-local rather than caller-owned because the builders run on both the
    /// UI thread and every [`crate::tex_upload`] worker, and a shared buffer would
    /// need a lock in the hottest path in the app. Grows to the largest layer built
    /// on that thread and stays — during playback of a fixed-size sequence the
    /// `resize` is a no-op, which is the entire point: a fresh allocation here
    /// zero-fills ~111 MB on 4.6K footage immediately before every byte is
    /// overwritten.
    /// Held as `f32`, not `u16`, purely for **alignment**. Both paths cast this
    /// buffer to the type they pack, and a cast is only sound in the
    /// wider-to-narrower direction: `Vec<f32>` guarantees 4-byte alignment, so
    /// reinterpreting it as `[u16]` always works, while a `Vec<u16>` is only
    /// guaranteed 2-byte alignment and reinterpreting *that* as `[f32]` can panic
    /// inside `bytemuck` on an allocator that doesn't happen to over-align.
    static STAGING: std::cell::RefCell<Vec<f32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Cost of [`ExrViewer::build_layer_texture`], split by phase (#202).
///
/// Nothing timed the paint path before this, so "the synchronous texture build is
/// the bottleneck" was an assumption. It mattered *which* phase was expensive,
/// because they imply different fixes, and the split was not what it looked like:
/// once the redundant per-build staging allocation was removed, the remaining cost
/// was ~80% `queue.write_texture` (a full-frame `memcpy` into wgpu's staging belt)
/// and only ~20% the `rayon`-parallel channel interleave. So the fix could not be
/// "move the interleave to a worker" — it had to move the whole build.
///
/// **Read the totals as worker time, not UI-thread time.** Before #202 these
/// counters were accumulated entirely on the paint thread, and `texb_tot` against
/// 1000 ms read directly as "fraction of the UI thread spent uploading" (it peaked
/// at ~94%). The builds now run on [`crate::tex_upload`]'s pool, so the sum is
/// across N workers and legitimately exceeds one second per second of wall clock.
/// The UI-thread question is now answered by `texq` and by frame time, not here.
///
/// Process-global and lock-free because the builders are associated functions
/// reached from several call sites (`ensure_comp_frame` via the pool,
/// `ensure_base_frame`, the Add flow) on several threads; threading
/// a collector through all of them would touch far more code than the measurement
/// is worth. The 1 Hz trace drains the counters, so each line reports the cost
/// incurred during that second.
pub(crate) mod tex_build_stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::time::Duration;

    static N: AtomicU64 = AtomicU64::new(0);
    static ALLOC_NS: AtomicU64 = AtomicU64::new(0);
    static PACK_NS: AtomicU64 = AtomicU64::new(0);
    static WRITE_NS: AtomicU64 = AtomicU64::new(0);
    static BIND_NS: AtomicU64 = AtomicU64::new(0);
    static MAX_NS: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);

    /// One completed build. `alloc` is `create_texture`, `pack` the channel
    /// interleave into the staging buffer, `write` the `queue.write_texture` call,
    /// `bind` the view + bind-group creation.
    pub fn record(alloc: Duration, pack: Duration, write: Duration, bind: Duration, bytes: u64) {
        let ns = |d: Duration| d.as_nanos() as u64;
        let total = ns(alloc) + ns(pack) + ns(write) + ns(bind);
        N.fetch_add(1, Relaxed);
        ALLOC_NS.fetch_add(ns(alloc), Relaxed);
        PACK_NS.fetch_add(ns(pack), Relaxed);
        WRITE_NS.fetch_add(ns(write), Relaxed);
        BIND_NS.fetch_add(ns(bind), Relaxed);
        BYTES.fetch_add(bytes, Relaxed);
        MAX_NS.fetch_max(total, Relaxed);
    }

    /// Totals accumulated since the last drain, and reset: `(builds, alloc_ms,
    /// pack_ms, write_ms, bind_ms, slowest_build_ms, mb_uploaded)`.
    ///
    /// The four phase figures are **sums over the window**, not per-build averages.
    /// They are summed across the [`crate::tex_upload`] pool, so the total is
    /// throughput and can legitimately exceed one second per second of wall clock —
    /// it is *not* paint-thread occupancy. It was exactly that before the builds
    /// moved off the UI thread, and reading it that way now would overstate the
    /// cost by the worker count. `texq` is the UI-side number.
    pub fn drain() -> (u64, f32, f32, f32, f32, f32, f32) {
        let ms = |a: &AtomicU64| a.swap(0, Relaxed) as f64 / 1e6;
        (
            N.swap(0, Relaxed),
            ms(&ALLOC_NS) as f32,
            ms(&PACK_NS) as f32,
            ms(&WRITE_NS) as f32,
            ms(&BIND_NS) as f32,
            ms(&MAX_NS) as f32,
            BYTES.swap(0, Relaxed) as f64 as f32 / (1024.0 * 1024.0),
        )
    }
}

/// The unscaled canvas bounds that "Frame" (F) fits into the viewport. Every mode
/// except Side-by-Side draws B over A in the same rect, so the extent is just the
/// A image; Side-by-Side lays A and B out horizontally (B height-normalized to A
/// when enabled), so framing must fit their *combined* width or the second image
/// spills off-screen. Mirrors the SBS layout in [`ExrViewer::draw_comp_composite`],
/// measured in unscaled space (the `scale` cancels out of the fit ratio).
///
/// Takes `side_by_side` as a plain bool rather than a compare enum so the comp path
/// can drive it from its own `Arrangement` (#99 Slice 2a) — the A/B compare enum is
/// slated for deletion with the render retire.
fn framing_bounds(
    side_by_side: bool,
    normalize_sbs: bool,
    tex_size: egui::Vec2,
    tex_size_b: Option<egui::Vec2>,
) -> egui::Vec2 {
    let Some(size_b) = tex_size_b.filter(|_| side_by_side) else {
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

/// Compositing operator for a layer in the comp stack (premultiplied-alpha
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
    /// This layer's own **effective** pixel aspect — its manual override if it has
    /// one, else its source's header `pixelAspectRatio` — used for its own unsqueeze
    /// (#254 / #263). Resolved by the app via `Draw::effective_par`, where the layer
    /// and its source are both in hand; the viewer only gates it through the global
    /// unsqueeze toggle (`sanitize_unsqueeze`).
    ///
    /// Named `eff_par` rather than `par` deliberately: it was the raw header value
    /// until #263, and every reader needs to know the override is already folded in.
    pub eff_par: f32,
    /// This layer's own full-res pixel dimensions, so it can be placed at its own
    /// display size rather than stretched to the bottom layer's rect (#254). Same
    /// role `CompSideB::tex_size` has played for the Side-by-Side pane since #99 —
    /// that pane was the only place in the composite where a layer kept its own
    /// geometry, and it is the model this generalizes to N layers.
    pub tex_size: egui::Vec2,
    /// EXR origin of this layer's data window (`layer_position`), so the bbox overlay
    /// can report where the pixels sit in the file's coordinate space rather than on
    /// screen (#251). `tex_size` is the matching size.
    pub data_pos: (i32, i32),
    /// Whether this is the **current** layer — the `Layer:` selection the AOV
    /// controls, EXR Info and the pixel readout already act on.
    ///
    /// The overscan overlay draws this layer's bounding box, while the white format
    /// box comes from the canvas layer. That split is Nuke's: the dashed box is the
    /// viewer's format, the bbox is the viewed node's output, and the two are allowed
    /// to disagree — a layer whose bbox does not match the shot format is exactly what
    /// the overlay is for.
    pub is_current: bool,
}

/// The right-hand pane of a comp Side-by-Side (#99 Slice 2a). A compare shows the two
/// *layers themselves* — pane A the current layer, pane B this one — so the caller
/// reduces `draws` to pane A's single layer and hands pane B here. Because it is one
/// layer it needs no accumulate ping-pong: it is laid into the scene beside pane A as a
/// single placed overlay draw.
pub(crate) struct CompSideB {
    /// The layer's texture + blend/opacity, resolved exactly like a `CompDraw`.
    ///
    /// Its `tex_size` / `par` are the pane's own geometry. They used to be duplicated
    /// onto this struct because `CompDraw` had no geometry of its own; #254 gave every
    /// layer the same two fields, so the copies would only be a way for the pane and
    /// the layer to disagree.
    pub draw: CompDraw,
}

/// Where each compare pane lands on screen, per arrangement (#99 Slice 2a/2b).
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CompPanes {
    /// Pane A's rect — the whole image rect in every arrangement but Side-by-Side.
    pub image_rect: egui::Rect,
    /// Pane B's own rect. `Some` **only** for Side-by-Side: Wipe and Diff combine both
    /// layers inside `image_rect` in a single shader draw, so they have no second rect.
    pub rect_b: Option<egui::Rect>,
    /// The region the display stage + background cover — both panes in Side-by-Side.
    pub disp_rect: egui::Rect,
    /// Screen-x of the Side-by-Side divider, `None` otherwise.
    pub divider_x: Option<f32>,
}

/// Resolve the on-screen pane geometry for a comp arrangement. Side-by-Side splits the
/// canvas into two abutting panes via [`side_by_side_layout`]; **every other**
/// arrangement — Stacked, and the two single-rect 2-input modes Wipe and Diff — gives
/// pane A the whole rect. `side_b` is `Some((tex_size, par))` whenever a compare is
/// live, which is *not* the same as "the panes are split": keying the split on side B's
/// presence squeezes Wipe/Diff into the left half. Pure, so that distinction is
/// unit-testable without a GPU.
pub(crate) fn comp_pane_layout(
    arrangement: crate::layer::Arrangement,
    canvas_center: egui::Pos2,
    translation: egui::Vec2,
    scale: f32,
    image_size_a: egui::Vec2,
    side_b: Option<(egui::Vec2, f32)>,
    normalize: bool,
) -> CompPanes {
    let split = matches!(arrangement, crate::layer::Arrangement::SideBySide)
        .then_some(side_b)
        .flatten();
    match split {
        Some((tex_size_b, par_b)) => {
            let l = side_by_side_layout(
                canvas_center,
                translation,
                scale,
                image_size_a,
                tex_size_b,
                par_b,
                normalize,
            );
            CompPanes {
                image_rect: l.rect_a,
                rect_b: Some(l.rect_b),
                disp_rect: l.rect_a.union(l.rect_b),
                divider_x: Some(l.divider_x),
            }
        }
        None => {
            let image_rect =
                egui::Rect::from_center_size(canvas_center + translation, image_size_a);
            CompPanes {
                image_rect,
                rect_b: None,
                // The comp stack has no separate display window yet: display == image.
                disp_rect: image_rect,
                divider_x: None,
            }
        }
    }
}

/// Where one composite layer lands on screen (#254): its own pixel dimensions at its
/// own effective pixel aspect, centred on the canvas.
///
/// The composite used to resolve a single PAR and stretch every layer to the bottom
/// layer's rect, so an anamorphic plate over a square previs was squeezed back to 1:1.
/// This is the same rule [`side_by_side_layout`] has always applied to its second pane
/// — `tex_size * scale * its own par` — generalized from two panes to N layers.
///
/// `eff_par` is the layer's effective pixel aspect (see `ExrViewer::unsqueeze_factor`:
/// the manual override if set, else the header PAR, gated on the unsqueeze toggle).
/// Centred rather than offset because `Layer::transform`'s canvas-space offset is still
/// ignored by the renderer; honouring it is a separate change. Pure.
pub(crate) fn comp_layer_rect(
    canvas_center: egui::Pos2,
    scale: f32,
    tex_size: egui::Vec2,
    eff_par: f32,
) -> egui::Rect {
    egui::Rect::from_center_size(
        canvas_center,
        egui::vec2(tex_size.x * scale * eff_par, tex_size.y * scale),
    )
}

/// The canvas layer's EXR window geometry, for the overscan overlay (#251): the
/// display window ("format") and the data window the pixels actually cover, both in
/// native image pixels with their EXR origins.
///
/// Carried rather than recomputed because the viewer has no `ExrData` — the comp path
/// hands it only bind groups and sizes, which is what keeps decode and storage out of
/// the renderer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct CompFormat {
    pub disp_pos: (i32, i32),
    pub disp_size: (usize, usize),
    pub data_pos: (i32, i32),
    pub data_size: (usize, usize),
}

/// Where the display window lands on screen, given where its **data** window landed.
///
/// The old A/B path ran this the other way — it centred the display window and offset
/// the image from it. The comp path centres each layer's *data* window (`comp_layer_rect`,
/// #254), so the display window is derived from that instead. Same offset, opposite
/// sign; deriving it this way adds the overlay without moving a single layer.
///
/// `view_scale` is the per-axis screen scale `(scale * par, scale)`, so the x term
/// carries the anamorphic unsqueeze and the box tracks a squeezed image. Pure.
pub(crate) fn comp_display_rect(
    layer_rect: egui::Rect,
    view_scale: egui::Vec2,
    f: CompFormat,
) -> egui::Rect {
    let offset = egui::vec2(
        (f.disp_pos.0 - f.data_pos.0) as f32 * view_scale.x,
        (f.disp_pos.1 - f.data_pos.1) as f32 * view_scale.y,
    );
    egui::Rect::from_min_size(
        layer_rect.min + offset,
        egui::vec2(
            f.disp_size.0 as f32 * view_scale.x,
            f.disp_size.1 as f32 * view_scale.y,
        ),
    )
}

/// How far the data window runs past the display window, as pixels per side and as a
/// percentage of the format (#251).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub(crate) struct Overscan {
    /// The data window extends past the display window on at least one side.
    pub overscanned: bool,
    /// The data window falls *short* of the display window on at least one side —
    /// a crop rather than an overscan. Both can be true at once on a data window
    /// that is offset as well as differently sized.
    pub cropped: bool,
    /// Extra width / height as a percentage of the display window, e.g. a
    /// 2200-wide data window in a 1920 format is `+14.6`. Negative when cropped.
    pub extra_w_pct: f32,
    pub extra_h_pct: f32,
    /// Margin in pixels on each side: how far the data window extends past the
    /// display window (negative where it falls short).
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

impl Overscan {
    /// What the caption calls this window relationship.
    ///
    /// Three cases, not two: a data window that is *offset* as well as differently
    /// sized runs past the format on one side and falls short on another, so both
    /// flags are set. Labelling that "Overscan" describes half of it and hides the
    /// half that costs you — a side with no pixels where the format expects them.
    /// Pure.
    #[must_use]
    pub fn label(self) -> &'static str {
        match (self.overscanned, self.cropped) {
            (true, true) => "Overscan + crop",
            (false, true) => "Crop",
            // Neither is unreachable from the overlay, which only draws when one of
            // them holds; name it honestly rather than asserting.
            _ => "Overscan",
        }
    }
}

/// Measure a layer's overscan (#251).
///
/// The percentage is expressed against the **display window**, per axis, as the total
/// extra across both sides — so a symmetric 140px margin on a 1920 format reads
/// `+14.6%`, not `+7.3%`. Total rather than per-side because the margins need not be
/// symmetric (a data window can be offset), and one number per axis that always means
/// the same thing beats a number that silently assumes symmetry.
///
/// A zero-sized display window yields zero percentages rather than a division by zero;
/// the pixel margins are still reported, since those are well defined regardless. Pure.
pub(crate) fn overscan_of(f: CompFormat) -> Overscan {
    let (dw, dh) = (f.disp_size.0 as i64, f.disp_size.1 as i64);
    let (tw, th) = (f.data_size.0 as i64, f.data_size.1 as i64);
    let left = i64::from(f.disp_pos.0) - i64::from(f.data_pos.0);
    let top = i64::from(f.disp_pos.1) - i64::from(f.data_pos.1);
    let right = (i64::from(f.data_pos.0) + tw) - (i64::from(f.disp_pos.0) + dw);
    let bottom = (i64::from(f.data_pos.1) + th) - (i64::from(f.disp_pos.1) + dh);
    let pct = |extra: i64, whole: i64| {
        if whole > 0 {
            extra as f32 / whole as f32 * 100.0
        } else {
            0.0
        }
    };
    Overscan {
        overscanned: left > 0 || right > 0 || top > 0 || bottom > 0,
        cropped: left < 0 || right < 0 || top < 0 || bottom < 0,
        extra_w_pct: pct(tw - dw, dw),
        extra_h_pct: pct(th - dh, dh),
        left: left as i32,
        right: right as i32,
        top: top as i32,
        bottom: bottom as i32,
    }
}

/// Which pane a blink compare shows at `time` (seconds), and when it next flips
/// (#99 Slice 3g). Flips every `interval` seconds; returns `(shows_b, next_flip_at)`.
/// The caller schedules a repaint for the flip rather than repainting continuously —
/// the image only changes at the boundary, and a bare per-frame repaint re-runs the
/// whole render including the OCIO passes (#146). A non-positive or non-finite
/// interval degenerates to "always pane A", never a divide-by-zero. Pure.
pub(crate) fn blink_phase(time: f64, interval: f32) -> (bool, f64) {
    let interval = f64::from(interval);
    if !(interval.is_finite() && interval > 0.0) || !time.is_finite() {
        return (false, time);
    }
    let phase = (time / interval).floor();
    let shows_b = (phase as i64).rem_euclid(2) == 1;
    (shows_b, (phase + 1.0) * interval)
}

/// The drawn wipe line: its handle centre plus the two endpoints spanning the image.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct WipeLine {
    pub center: egui::Pos2,
    pub p1: egui::Pos2,
    pub p2: egui::Pos2,
}

/// Screen geometry of the wipe divider over `image_rect`: `wipe_center` is normalized
/// (0..1 across the rect) and `angle_deg` rotates the split. The line runs
/// perpendicular to the split normal `(cos θ, sin θ)` and is extended past the rect's
/// diagonal so it always spans the image. Pure, and shared by the A/B and comp paths so
/// the two can't drift.
pub(crate) fn wipe_line_endpoints(
    image_rect: egui::Rect,
    wipe_center: [f32; 2],
    angle_deg: f32,
) -> WipeLine {
    let center = egui::pos2(
        image_rect.min.x + image_rect.width() * wipe_center[0],
        image_rect.min.y + image_rect.height() * wipe_center[1],
    );
    let a = angle_deg.to_radians();
    let dir = egui::vec2(-a.sin(), a.cos());
    let max_dist = image_rect.width().hypot(image_rect.height());
    WipeLine {
        center,
        p1: center + dir * max_dist,
        p2: center - dir * max_dist,
    }
}

/// Which side of the wipe a screen position falls on, matching `fs_main`'s split
/// exactly: the offset from the wipe centre is measured **in rect-relative pixels**
/// (uv scaled by the rect size, so a non-square image doesn't skew the angle) and
/// projected onto the normal `(cos θ, sin θ)`; `dist >= 0` is the `tex_b` side. Pure,
/// so the wipe pixel readout can name the layer actually under the cursor.
pub(crate) fn wipe_side_at(
    pos: egui::Pos2,
    image_rect: egui::Rect,
    wipe_center: [f32; 2],
    angle_deg: f32,
) -> CompSide {
    let size = image_rect.size();
    let uv = egui::vec2(
        (pos.x - image_rect.min.x) / size.x.max(f32::EPSILON),
        (pos.y - image_rect.min.y) / size.y.max(f32::EPSILON),
    );
    let to_pixel = egui::vec2(
        (uv.x - wipe_center[0]) * size.x,
        (uv.y - wipe_center[1]) * size.y,
    );
    let a = angle_deg.to_radians();
    if to_pixel.x * a.cos() + to_pixel.y * a.sin() >= 0.0 {
        CompSide::B
    } else {
        CompSide::A
    }
}

/// Which Side-by-Side pane a cursor is over. Pure so the hover→pane mapping is
/// unit-testable without a GPU.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum CompSide {
    /// The composite pane (the whole canvas when not side-by-side).
    A,
    /// The current-layer pane.
    B,
}

/// Pick the Side-by-Side pane under `pos` (#99 Slice 2a). `rect_b` is `None` outside
/// Side-by-Side, where the whole image rect is the composite. B is tested first: the
/// two rects abut exactly, so a cursor on the shared edge belongs to B — matching the
/// render, where side B's overlay draw is laid down last and wins that pixel column.
/// Returns `None` when the cursor is over neither pane. Pure.
pub(crate) fn pick_comp_side(
    pos: egui::Pos2,
    rect_a: egui::Rect,
    rect_b: Option<egui::Rect>,
) -> Option<CompSide> {
    if rect_b.is_some_and(|r| r.contains(pos)) {
        return Some(CompSide::B);
    }
    rect_a.contains(pos).then_some(CompSide::A)
}

/// Which compare pane the cursor is over, across every arrangement (#99 Slices 2b/3g).
/// Side-by-Side splits on two rects; a Wipe (`wipe` = `Some((centre, angle))`) overlays
/// both layers in `rect_a` and splits on the wipe line; a Blink (`blink_b` = `Some`)
/// fills `rect_a` with one pane at a time, so the *phase* decides, not the cursor.
/// `None` when the cursor is over no pane at all, so the readout blanks rather than
/// reporting the wrong layer. Pure.
pub(crate) fn comp_hover_side(
    pos: egui::Pos2,
    rect_a: egui::Rect,
    rect_b: Option<egui::Rect>,
    wipe: Option<([f32; 2], f32)>,
    blink_b: Option<bool>,
) -> Option<CompSide> {
    if let Some(showing_b) = blink_b {
        let side = if showing_b { CompSide::B } else { CompSide::A };
        return rect_a.contains(pos).then_some(side);
    }
    match wipe {
        Some((center, angle)) => rect_a
            .contains(pos)
            .then(|| wipe_side_at(pos, rect_a, center, angle)),
        None => pick_comp_side(pos, rect_a, rect_b),
    }
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

/// The two placed image rects + divider x for a Side-by-Side arrangement. Pure
/// geometry so it's unit-testable without a GPU and shared by the render path and the
/// hover→pixel mapping.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct SxsLayout {
    pub rect_a: egui::Rect,
    pub rect_b: egui::Rect,
    /// Screen-x of the divider between the two images (== `rect_b.min.x`).
    pub divider_x: f32,
}

/// Lay out two images side by side (#179 / #99 render-retire): A on the left at
/// `image_size_a` (already carrying A's unsqueeze), B on the right sized from its
/// native `tex_size_b` × `scale` × `par_b`, or — when `normalize` — rescaled so B's
/// height matches A's. Both are vertically centered in a combined rect anchored at
/// `canvas_center + translation`. Pure.
pub(crate) fn side_by_side_layout(
    canvas_center: egui::Pos2,
    translation: egui::Vec2,
    scale: f32,
    image_size_a: egui::Vec2,
    tex_size_b: egui::Vec2,
    par_b: f32,
    normalize: bool,
) -> SxsLayout {
    let image_size_b = if normalize && tex_size_b.y != 0.0 {
        // Match B's height to A's on-screen height (`image_size_a.y == tex.y*scale`).
        let scale_b = image_size_a.y / tex_size_b.y;
        egui::vec2(tex_size_b.x * scale_b * par_b, tex_size_b.y * scale_b)
    } else {
        egui::vec2(tex_size_b.x * scale * par_b, tex_size_b.y * scale)
    };
    let combined = egui::vec2(
        image_size_a.x + image_size_b.x,
        image_size_a.y.max(image_size_b.y),
    );
    let combined_rect = egui::Rect::from_center_size(canvas_center + translation, combined);
    let cy = combined_rect.center().y;

    let mut rect_a = egui::Rect::from_min_size(combined_rect.min, image_size_a);
    rect_a.set_center(egui::pos2(rect_a.center().x, cy));
    let mut rect_b = egui::Rect::from_min_size(
        egui::pos2(combined_rect.min.x + image_size_a.x, combined_rect.min.y),
        image_size_b,
    );
    rect_b.set_center(egui::pos2(rect_b.center().x, cy));

    SxsLayout {
        rect_a,
        rect_b,
        divider_x: rect_b.min.x,
    }
}

/// Which feature the shared gradient editor is currently editing — the result of
/// "Apply" / "Save as preset" is routed accordingly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GradientTarget {
    DiffColormap,
    Background,
}

/// A built GPU texture for one layer: the `BindGroup` to paint plus the owning
/// `Texture`. Dropping this handle releases our reference; wgpu reclaims the VRAM
/// once no live reference remains (it refuses to free a texture whose view is
/// still bound, which is the safety we rely on). We deliberately do *not* call
/// `Texture::destroy()` — that forcibly frees regardless of references, and on
/// Vulkan a draw recorded this frame against a just-destroyed texture aborts the
/// process at submit (Metal tolerated it; Vulkan does not). The `BindGroup` is
/// shared (`Arc`) with the active-layer slot while displayed.
///
/// Was `T2Texture`, named for the pre-upload ring that produced it; the ring was
/// retired in #299 and this is now simply what [`ExrViewer::build_layer_texture`]
/// hands back, on the upload worker or the UI thread alike.
struct LayerTexture {
    // Held to own the texture for the entry's lifetime: dropping this handle
    // releases our reference so wgpu can reclaim the VRAM once the bind group is
    // gone too. Not read directly — ownership/drop is the point.
    #[allow(dead_code)]
    texture: eframe::egui_wgpu::wgpu::Texture,
    bind_group: std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
}

/// Per-frame GPU draw context for the canvas (#152): the base uniforms plus the
/// persistent ring buffer, LUT/default bind groups, and the interior-mutable
/// per-frame accumulators (`uniform_offset` ring allocator, `overscan_factor`,
/// `ocio_sig`, `ocio_draws`). It replaces the old `draw_gpu` closure + its loose
/// `Cell`/`RefCell` captures, so [`ExrViewer::draw_comp_composite`] can dispatch the
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
    /// The screen-space quad the next **accumulate fold** rasterizes (#257) — the
    /// running union of the layer rects. A fold clears the whole target, so it must
    /// cover everything the accumulation below it occupies, but no more. `None`
    /// (every path but the layer-stack loop) falls back to the draw's own rect,
    /// which is the pre-#257 geometry.
    fold_rect: std::cell::Cell<Option<egui::Rect>>,
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
        // Only an accumulate fold reads this (see the `Uniforms::fold_*` docs); every
        // other draw gets its own rect, making the vertex math the plain placed quad.
        let fold = self.fold_rect.get().unwrap_or(target_rect);
        u.fold_min = [fold.min.x, fold.min.y];
        u.fold_max = [fold.max.x, fold.max.y];
        u.is_diff_mode = if is_diff { 1 } else { 0 };
        u.is_composite = if is_composite { 1 } else { 0 };
        u.opacity = opacity;
        u.overscan_factor = self.overscan_factor.get();
        // Under OCIO the only `is_composite` draw is the layer-stack accumulate top
        // layer (composite folds through the scene ping-pong there — see `draw_comp_composite`
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
/// the arrangement, the texture caches described in the module docs, plus
/// sampling/histogram/contact-sheet state. Driven each frame by the comp path.
pub struct ExrViewer {
    /// Per-layer **decimated** contact-sheet thumbnails (A/B). The CPU thumbnail
    /// bake ([`Self::generate_texture`]) is the headless / no-GPU fallback; when a
    /// GPU is present, `gpu_thumbnails` is used instead (#67). Cleared on a layer
    /// *count* change or any tone/OCIO change.
    thumbnails: Vec<Option<egui::TextureHandle>>,
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
    pub blink_interval: f32,
    pub fullscreen: bool,
    // Add viewing options like exposure, gamma, srgb toggle
    pub exposure: f32,
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
    /// Cache key for the computed bins: `(disc, layer_idx, log_histogram)`. The
    /// bins depend on all three, so keying on the layer alone left stale bins when
    /// the log toggle flipped. `disc` is a source discriminator so switching which
    /// image the histogram reflects (classic A vs a comp layer's source, #99 R4)
    /// invalidates it — classic uses [`Self::HIST_DISC_CLASSIC`], comp passes its
    /// `SourceId`. Image-B load/unload is invalidated explicitly via
    /// [`ExrViewer::invalidate_histogram`] since B identity isn't in the key.
    histogram_key: Option<(u64, usize, bool)>,
    pub log_histogram: bool,

    // View transform
    pub scale: f32,
    pub translation: egui::Vec2,
    pub first_frame: bool,
    /// Display-window ("format") size of whatever the canvas is currently showing,
    /// for the always-on `WxH` readout at the image's bottom-right (#251). Set by
    /// the app each frame before `draw_comp_composite`, alongside the other mirrored
    /// per-frame viewer state; `None` draws nothing.
    ///
    /// This is the **display** window, not the data window: the format is the
    /// question a comp TD is actually asking ("is this the right res?"), and a render
    /// with overscan has a data window larger than it. It is also full-res even while
    /// a proxy is on screen — `ExrData::downsampled` preserves `display_window` — so
    /// the label does not flicker to the proxy size on scrub. That detail was in the
    /// original implementation and is the reason this reads the display window rather
    /// than the texture size.
    pub(crate) comp_format: Option<CompFormat>,
    /// Opacity applied to the data-window pixels *outside* the display window — the
    /// overscan dim (#251). `0.0` hides the overscan entirely, `1.0` shows it at full
    /// strength (no dim). The GPU side of this never went away: `Uniforms::overscan_factor`
    /// and its `shader.wgsl` counterpart survived the A/B deletion still implementing
    /// the dim, pinned to `1.0` at every construction site, so restoring it was
    /// re-plumbing a value rather than new rendering.
    ///
    /// Session-only, matching the original — it is a per-look adjustment rather than a
    /// standing preference.
    pub overscan_opacity: f32,
    pub last_hover_pos_img: Option<(usize, usize)>,
    pub last_sampled_val_a: Option<[f32; 4]>,
    /// Natural (unclipped) height of the contextual mode-param row, recorded each
    /// frame it renders so the slide-in animation knows how far to grow. Transient
    /// runtime state — not persisted.

    /// The image canvas rect (egui points) from the last frame, used by the
    /// snapshot feature (#19) to crop the framebuffer screenshot to the image
    /// area. Transient.
    pub last_canvas_rect: Option<egui::Rect>,

    /// The displayed image rect (egui points) from the last frame — the display
    /// window clamped to the canvas. The snapshot (#19, #52) crops to this so the
    /// saved frame is just the active image, not the surrounding background.
    /// `None` falls back to `last_canvas_rect`. Transient.
    pub last_image_rect: Option<egui::Rect>,

    /// The current-layer pane's rect in a comp Side-by-Side (#99 Slice 2a); `None` in
    /// every other arrangement, where `last_image_rect` covers the whole image. Paired
    /// with [`pick_comp_side`] so the pixel readout samples the pane under the cursor
    /// rather than always reporting the composite. Transient.
    pub last_image_rect_b: Option<egui::Rect>,
    /// Last `evt=layer_geom` line emitted, so the per-layer geometry trace fires on
    /// change instead of every frame (#254). Runtime-only.
    dbg_layer_geom: Option<String>,

    /// `(wipe_center, wipe_angle)` when the last comp frame drew a Wipe (#99 Slice 2b);
    /// `None` otherwise. Wipe overlays both layers in one rect, so the pixel readout
    /// splits on the wipe line ([`comp_hover_side`]) rather than on two rects.
    /// Transient.
    pub last_wipe: Option<([f32; 2], f32)>,

    /// Which pane the last comp frame's Blink arrangement showed (#99 Slice 3g);
    /// `None` outside Blink. Blink fills the rect with one pane at a time, so the
    /// pixel readout follows the phase rather than the cursor. Transient.
    pub last_blink_b: Option<bool>,

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
}

impl Default for ExrViewer {
    fn default() -> Self {
        Self {
            thumbnails: Vec::new(),
            gpu_thumbnails: Vec::new(),
            pending_thumb_frees: Vec::new(),
            dbg_thumb_bakes: 0,
            gpu_thumb_bg: None,
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
            blink_interval: 1.0,
            fullscreen: false,
            exposure: 0.0,
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
            histogram_key: None,
            log_histogram: true,
            scale: 1.0,
            translation: egui::Vec2::ZERO,
            first_frame: true,
            comp_format: None,
            overscan_opacity: 0.2,
            last_hover_pos_img: None,
            last_sampled_val_a: None,
            last_canvas_rect: None,
            last_image_rect: None,
            last_image_rect_b: None,
            dbg_layer_geom: None,
            last_wipe: None,
            last_blink_b: None,
            annotations: Vec::new(),
            anno_tool: AnnotationTool::None,
            anno_color: egui::Color32::RED,
            anno_width: 3.0,
            show_annotation_bar: false,
            anno_in_progress: None,
            anno_undo: Vec::new(),
            anno_redo: Vec::new(),
            anno_text_edit: None,
        }
    }
}

impl ExrViewer {
    /// Every viewer keyboard shortcut: channel isolation (R/G/B/A/C), frame-fit
    /// (F), the tone resets (E / Shift+G), and fullscreen (F11 / Esc). Called once
    /// per frame from the comp central path, which is the only viewport (#99).
    /// Rendering-free, so the input handling is driven headlessly in tests (no wgpu
    /// device required).
    ///
    /// The single-key shortcuts are suppressed while a text field wants keyboard
    /// input or the contact sheet is open; F11 / Esc deliberately are not — see the
    /// ordering note in the body.
    pub fn handle_channel_hotkeys(&mut self, ui: &egui::Ui) {
        // Fullscreen (F11) and Esc (#99 Slice 3f) run **before** the suppression gate
        // below, because they must stay live exactly when the single-key shortcuts
        // must not: Esc's first job is cancelling an in-flight annotation, and the
        // annotation *text* field holds keyboard focus, so gating Esc on
        // `egui_wants_keyboard_input` would make it impossible to cancel one.
        //
        // `viewer.fullscreen` still gates the menu bar, the timeline panel, and the
        // side panel, but nothing could set it once `handle_hotkeys` went unreachable.
        // Esc only falls through to leaving fullscreen when there was no annotation to
        // cancel. Sending a viewport command needs `ui.ctx()`, which can't be touched
        // while the input lock is held, so the reads come first.
        // `T` toggles the contact sheet. Like F11/Esc it sits above the gate — the
        // sheet sets `show_contact_sheet`, which the gate keys off, so handling it
        // below would let `T` open the sheet but never close it.
        // Read this BEFORE the input closure: `egui_wants_keyboard_input` locks the
        // same context, and touching `ui.ctx()` while the input lock is held
        // deadlocks (the same constraint the viewport command below is subject to).
        let typing = ui.ctx().egui_wants_keyboard_input();
        let (f11, esc, sheet) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::F11),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::T) && !typing,
            )
        });
        if sheet {
            self.show_contact_sheet = !self.show_contact_sheet;
        }
        let mut fullscreen_changed = false;
        if f11 {
            self.fullscreen = !self.fullscreen;
            fullscreen_changed = true;
        }
        if esc && !self.cancel_annotation() && self.fullscreen {
            self.fullscreen = false;
            fullscreen_changed = true;
        }
        if fullscreen_changed {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }

        if ui.ctx().egui_wants_keyboard_input() || self.show_contact_sheet {
            return;
        }
        let (reset_exp, reset_gam) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::E),
                i.key_pressed(egui::Key::G) && i.modifiers.shift,
            )
        });
        if reset_exp {
            self.reset_exposure();
        }
        if reset_gam {
            self.reset_gamma();
        }
        let next = ui.input(|i| {
            if i.key_pressed(egui::Key::F) {
                self.first_frame = true;
            }
            let mut next = self.channel_mode;
            if i.key_pressed(egui::Key::R) {
                next = ChannelMode::R;
            }
            // Plain G only — Shift+G is the gamma reset handled by `handle_hotkeys`.
            if i.key_pressed(egui::Key::G) && !i.modifiers.shift {
                next = ChannelMode::G;
            }
            if i.key_pressed(egui::Key::B) {
                next = ChannelMode::B;
            }
            if i.key_pressed(egui::Key::A) {
                next = ChannelMode::A;
            }
            if i.key_pressed(egui::Key::C) {
                next = ChannelMode::RGB;
            }
            next
        });
        self.set_channel_mode(next);
    }

    /// Set the channel-isolation mode, invalidating the cached CPU + GPU
    /// thumbnails on an actual change. The single owner of that invalidation so
    /// it can't drift between the C/R/G/B/A hotkeys, the top control row, and the
    /// status-bar quick toggle (#192) — the composite honors `channel_mode` on its
    /// top layer, so this drives isolation in comp mode too.
    pub fn set_channel_mode(&mut self, mode: ChannelMode) {
        if self.channel_mode == mode {
            return;
        }
        self.channel_mode = mode;
        self.thumbnails.fill(None);
        self.invalidate_gpu_thumbnails();
    }

    /// Drain the GPU contact-sheet thumbnail caches (#67), queuing every
    /// registered `TextureId` for deferred `free_texture` (drained in
    /// `draw_contact_sheet`, which holds the renderer). Mirrors the A/B scoping of
    /// the CPU `thumbnails.fill(None)` sites.
    fn invalidate_gpu_thumbnails(&mut self) {
        for slot in self.gpu_thumbnails.iter_mut() {
            if let Some((id, _, _)) = slot.take() {
                self.pending_thumb_frees.push(id);
            }
        }
    }

    /// Free queued GPU thumbnail texture ids (#67). Invalidation sites have no
    /// renderer handle, so they defer into `pending_thumb_frees`; this is the
    /// one place that holds the renderer. Runs every frame from the comp path
    /// (and again inside the contact sheet after its own invalidations). With
    /// no GPU nothing was ever registered — just clear the queue.
    pub(crate) fn drain_thumb_frees(&mut self, gpu_resources: Option<&crate::gpu::GpuResources>) {
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
        // GPU thumbnails bake the tone into a cached texture (unlike the live
        // viewport uniform), so an exposure/gamma change must re-render them.
        self.invalidate_gpu_thumbnails();
    }

    /// `pub(crate)`: the comp viewport bar hosts the tone controls now (#99 Slice 3a).
    pub(crate) fn reset_exposure(&mut self) {
        self.exposure = 0.0;
        self.invalidate_tone();
    }

    /// `pub(crate)` — see [`Self::reset_exposure`].
    pub(crate) fn reset_gamma(&mut self) {
        self.gamma = 1.0;
        self.invalidate_tone();
    }

    /// The Wipe parameters: split centre, angle, and the divider-line opacity.
    /// `pub(crate)` and shared with the comp viewport bar (#99 Slice 3c) so the two
    /// entry points can't drift — the comp path is the only *reachable* one now.
    /// `wipe_line_opacity` in particular has no other control, so a persisted `0.0`
    /// would otherwise leave the comp wipe line invisible with no way back.
    pub(crate) fn wipe_params_ui(&mut self, ui: &mut egui::Ui) {
        // Each slider gets a left-side `ui.label(...)` for a consistent row; the two
        // centre sliders are named so the wipe-centre handle is self-describing.
        ui.label("Center X");
        ui.add(egui::Slider::new(&mut self.wipe_center[0], 0.0..=1.0));
        ui.label("Center Y");
        ui.add(egui::Slider::new(&mut self.wipe_center[1], 0.0..=1.0));
        ui.label("Angle °");
        ui.add(egui::Slider::new(&mut self.wipe_angle, -180.0..=180.0));
        ui.label("Line Opacity");
        ui.add(egui::Slider::new(&mut self.wipe_line_opacity, 0.0..=1.0));
    }

    /// The Diff parameters: gain, colormap, metric, noise floor, the legend, and the
    /// gradient-editor hook. `pub(crate)` and shared with the comp viewport bar
    /// (#99 Slice 3c); comp Diff consumes all of these and had no UI for any of them.
    pub(crate) fn diff_params_ui(&mut self, ui: &mut egui::Ui) {
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
                        let selected =
                            matches!(&self.prefs.diff_colormap, Colormap::Custom(cur) if cur == g);
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
    /// Rendered once per frame by [`crate::app::ExrApp`] when `gradient_editor_open`
    /// (#99 Slice 3b — it used to hang off the now-deleted `ExrViewer::ui`).
    pub(crate) fn gradient_editor_window(&mut self, ctx: &egui::Context) {
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
    /// `self.prefs.background` live; rendered once per frame by
    /// [`crate::app::ExrApp`] when `show_background_window` (#99 Slice 3b — it used to
    /// hang off the now-deleted `ExrViewer::ui`, leaving the View-menu item a dead click once the R4
    /// collapse made `ui` unreachable). Colours are linear (see `background` docs).
    pub(crate) fn background_window(&mut self, ctx: &egui::Context) {
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
    /// clear. `pub(crate)` so the comp viewport bar can host it (#99 Slice 3d).
    pub(crate) fn annotation_toolbar(&mut self, ui: &mut egui::Ui) {
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
    pub(crate) fn invalidate_thumbnails_on_ocio_change(&mut self) {
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
            // Toggling OCIO flips the GPU thumbnail backend (display shader vs the
            // OCIO two-pass); clear the GPU cache so stale thumbnails don't linger.
            self.invalidate_gpu_thumbnails();
        }
    }

    /// While blink is active (and B is loaded), alternate the displayed image
    /// between A and B on `blink_interval`, requesting repaints to keep cycling.
    pub(crate) fn sync_texture_caches(&mut self, layer_count: usize) {
        if self.thumbnails.len() != layer_count {
            self.thumbnails.clear();
            self.thumbnails.resize(layer_count, None);
            // Queue any registered GPU thumbnail ids for deferred free before the
            // slots are dropped (the renderer is only reachable in the sheet draw).
            for (id, _, _) in self.gpu_thumbnails.drain(..).flatten() {
                self.pending_thumb_frees.push(id);
            }
            self.gpu_thumbnails.resize_with(layer_count, || None);
        }
    }

    /// Render the contact sheet: a scrollable grid of per-layer (AOV) thumbnails for
    /// one source. Returns the clicked layer index, and closes the sheet — the caller
    /// decides what "select this layer" means (the comp path points the current comp
    /// layer's `aov` at it).
    ///
    /// One source, not two (#99 Slice 3e): the sheet used to fork on the A/B compare mode to
    /// draw an A/B two-column variant, but that was purely presentational and the A/B
    /// path is gone. It needs **no viewport** — just the decoded image, a tone
    /// snapshot, and the thumbnail caches — so it works the same from either caller.
    ///
    /// The caller **must** have sized the caches via [`Self::sync_texture_caches`]:
    /// the per-cell indexing below is unguarded.
    pub(crate) fn draw_contact_sheet(
        &mut self,
        ui: &mut egui::Ui,
        exr_data: &ExrData,
        gpu_resources: Option<&crate::gpu::GpuResources>,
        lut_bg_opt: Option<&std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) -> Option<usize> {
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
            self.invalidate_gpu_thumbnails();
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

        let mut clicked: Option<usize> = None;
        let draw_sheet = |viewer: &mut Self,
                          ui: &mut egui::Ui,
                          data: &crate::exr_loader::ExrData,
                          clicked: &mut Option<usize>| {
            let l_count = data.logical_layers.len();
            egui::ScrollArea::vertical()
                .id_salt("sheet_a")
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
                                if viewer.gpu_thumbnails[i].is_none() && visible {
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
                                        viewer.gpu_thumbnails[i] = baked;
                                        if ok {
                                            bake_budget -= 1;
                                            viewer.dbg_thumb_bakes += 1;
                                        }
                                    } else {
                                        needs_more = true;
                                    }
                                }
                                viewer.gpu_thumbnails[i].as_ref().map(|(id, _, size)| {
                                    egui::Image::new(egui::load::SizedTexture::new(*id, *size))
                                })
                            } else {
                                if viewer.thumbnails[i].is_none() && visible {
                                    if bake_budget > 0 {
                                        let baked = viewer.generate_texture(
                                            ui.ctx(),
                                            data,
                                            i,
                                            Some(THUMB_BOX),
                                        );
                                        let ok = baked.is_some();
                                        viewer.thumbnails[i] = baked;
                                        if ok {
                                            bake_budget -= 1;
                                            viewer.dbg_thumb_bakes += 1;
                                        }
                                    } else {
                                        needs_more = true;
                                    }
                                }
                                viewer.thumbnails[i].as_ref().map(egui::Image::new)
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
                                // Report the pick and close; the caller decides what
                                // selecting a layer means for its model.
                                *clicked = Some(i);
                                viewer.active_layer = i;
                                viewer.show_contact_sheet = false;
                                viewer.first_frame = true;
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

        draw_sheet(self, ui, exr_data, &mut clicked);
        clicked
    }
    /// Hover/sample readout for the canvas: map the cursor to image pixel
    /// coordinates (handling the side-by-side split), sample A (and B) at that
    /// pixel, cache the last sample, show the value tooltip, and add a swatch on
    /// Shift+Click. Geometry (`rect`/`image_rect`/sizes) comes from the caller's
    /// layout so this stays purely about sampling.
    #[allow(clippy::too_many_arguments)]
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
    /// of `draw_comp_composite` (#152).
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
            // Overridden per draw by `DrawCtx::draw`; equal to the image rect here so
            // any draw that doesn't set a fold quad rasterizes exactly its own rect.
            fold_min: [image_rect.min.x, image_rect.min.y],
            fold_max: [image_rect.max.x, image_rect.max.y],
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
            // The only caller is the comp path, which sets this per-arrangement right
            // after (`draw_comp_composite`). It used to be derived from the A/B
            // `compare_mode`, which the comp path overrode anyway (#99 Slice 3h).
            is_wipe_mode: 0,
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
    /// Split out of `draw_comp_composite` (#152).
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

    /// Render the Layers-panel composite (#99 PR-B.3): fold `draws` bottom→top
    /// through the OCIO scene ping-pong (the PR-A accumulate path), reusing
    /// [`DrawCtx`] + [`crate::gpu::ocio_pass::OcioCallback`] verbatim — only the
    /// draw *source* differs (the panel's N sources instead of A/B). `base_size` is
    /// the bottom layer's pixel size; every layer currently draws at that one canvas
    /// rect (per-layer placement is the follow-up, #102/#104), so a differently-
    /// sized layer is stretched to fit for now.
    ///
    /// Renders in **any** colour mode (#99 R2): OCIO on → the OCIO transform; OCIO
    /// off → the sRGB display-encode pass (`use_display_encode`), so it no longer
    /// requires OCIO active. Requires a GPU; the caller
    /// ([`crate::app::ExrApp::draw_comp_central`]) gates on that (and a non-empty
    /// stack) and shows a fallback message otherwise. A no-op when `draws` is empty.
    /// Global tone (exposure / gamma / channel isolation / background) comes from the
    /// shared viewer fields, so it applies to the composite exactly as to a single
    /// image; per-composite tone controls land with the row controls (PR-B.4).
    #[allow(clippy::too_many_arguments)] // frame geometry + draws + arrangement + gpu handles
    pub(crate) fn draw_comp_composite(
        &mut self,
        ui: &mut egui::Ui,
        base_size: (usize, usize),
        base_par: f32,
        draws: &[CompDraw],
        arrangement: crate::layer::Arrangement,
        side_b: Option<CompSideB>,
        gpu_resources: &crate::gpu::GpuResources,
        lut_bg_opt: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    ) {
        if draws.is_empty() {
            return;
        }
        // Side-by-Side needs a resolved current layer; without one (hidden, soloed out,
        // trimmed blank) the caller already falls back, but guard here too so the
        // geometry below can rely on the pair.
        use crate::layer::Arrangement;
        let side_b = side_b.filter(|_| arrangement != Arrangement::Stacked);
        let is_sbs = side_b.is_some() && arrangement == Arrangement::SideBySide;
        // Wipe and Diff are single **2-input** draws (`tex_a` + `tex_b`, split or
        // differenced in the shader) over one shared rect, not two placed draws. Both
        // panes are single layers, so each binds directly as an image texture — no
        // offscreen round-trip, and `composite_accum` stays 0 so the shader samples
        // `tex_b` at image-local uv.
        let is_wipe = side_b.is_some() && matches!(arrangement, Arrangement::Wipe { .. });
        let is_diff = side_b.is_some() && arrangement == Arrangement::Diff;
        // Blink alternates the two panes in place: one draw at the full rect, binding
        // whichever pane the phase selects. Schedule the repaint for the flip itself.
        let blink_b = (side_b.is_some() && arrangement == Arrangement::Blink).then(|| {
            let (shows_b, next_flip) = blink_phase(ui.input(|i| i.time), self.blink_interval);
            let wait = (next_flip - ui.input(|i| i.time)).max(0.0);
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(wait));
            shows_b
        });
        let render_state = gpu_resources.render_state();
        let (bw, bh) = base_size;
        let tex_size = egui::vec2(bw.max(1) as f32, bh.max(1) as f32);
        // Anamorphic unsqueeze (#194 / #179): stretch the *canvas* horizontally by the
        // base layer's effective pixel aspect, the same CPU-side geometry stretch the
        // classic A/B path applies. The stretch is uniform across the image rect, so
        // the cursor→pixel readout (`comp_hover_pixel` on `last_image_rect`) stays
        // correct with no extra term.
        //
        // `base_par` arrives already resolved (override-or-header, per #263), so this
        // only applies the global `anamorphic_unsqueeze` toggle and clamps a bad
        // factor. It must NOT go back through `unsqueeze_factor`: that would re-apply
        // the viewer-wide override on top of the per-layer one and collapse the very
        // distinction this change draws.
        let par = self.sanitize_unsqueeze(base_par);

        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        self.last_canvas_rect = Some(rect);
        // Shared pan/zoom with the A/B view (the scale/translation fields are the
        // same); framing on first paint fits the *unsqueezed* base layer extents.
        // Side-by-Side lays the composite and the current layer out horizontally, so the
        // first-paint fit must span both or the second pane spills off-screen.
        let par_b = side_b
            .as_ref()
            .map(|b| self.sanitize_unsqueeze(b.draw.eff_par));
        let fit_b = side_b
            .as_ref()
            .zip(par_b)
            .map(|(b, pb)| egui::vec2(b.draw.tex_size.x * pb, b.draw.tex_size.y));
        self.handle_canvas_interaction(
            ui,
            rect,
            &response,
            egui::vec2(tex_size.x * par, tex_size.y),
            fit_b,
            is_sbs,
        );

        let image_size = egui::vec2(tex_size.x * self.scale * par, tex_size.y * self.scale);
        let panes = comp_pane_layout(
            arrangement,
            rect.center(),
            self.translation,
            self.scale,
            image_size,
            side_b
                .as_ref()
                .zip(par_b)
                .map(|(b, pb)| (b.draw.tex_size, pb)),
            self.normalize_side_by_side,
        );
        let image_rect = panes.image_rect;
        // The canvas layer's display window, if it has one distinct from its pixels
        // (#251). Only in Stacked and only for the canvas layer: the overlay describes
        // one image's format, and a compare shows two, which is the same reason the
        // format label is suppressed in Side-by-Side.
        //
        // `image_rect` *is* the bottom layer's rect — `comp_layer_rect` centres it on
        // the canvas at the canvas's own size and PAR, which is how `image_rect` is
        // built — so the display window is derived from it directly.
        let canvas_format = self.comp_format.filter(|_| !is_sbs);
        let disp_rect = match canvas_format {
            // The dim, the background gradient's normalization and the white format
            // box all key off the real display window. Where the canvas layer has no
            // overscan this returns `image_rect` exactly, so nothing changes for the
            // common case and the shader's dim gate is always inside.
            Some(f) => comp_display_rect(image_rect, egui::vec2(self.scale * par, self.scale), f),
            // A compare: two panes, two possible formats. Suppressed for the same
            // reason the format label is.
            None => panes.disp_rect,
        };
        // Whether the canvas layer has pixels outside its own format — the condition
        // for the dim to have anything to dim.
        let canvas_overscanned = canvas_format.is_some_and(|f| overscan_of(f).overscanned);
        self.last_image_rect = Some(image_rect);
        // Per-axis screen scale `(scale * par, scale)`: annotations are stored in
        // *native* image pixels, so dividing by this maps a screen point back and they
        // stay anchored across an anamorphic squeeze/unsqueeze toggle (#179).
        let view_scale = egui::vec2(self.scale * par, self.scale);
        // Annotation drawing (#45), restored into the comp path (#99 Slice 3d) — it
        // lived only in the legacy branch, so the whole feature went dark with the R4
        // collapse while `handle_canvas_interaction` still checked `anno_tool` and
        // suppressed pan for a tool that could no longer be selected.
        self.handle_annotation_input(&response, image_rect, view_scale);
        // The pane-B rect, for the per-pane pixel readout (`pick_comp_side`). `None`
        // outside Side-by-Side: Wipe overlays both layers in the *same* rect, so its
        // readout splits via `wipe_side_at` (see `comp_hover_side`), and Diff is a
        // false-colour blend of both.
        self.last_image_rect_b = panes.rect_b;
        // Drag the wipe handle / scroll to rotate, exactly as the A/B path does. Runs
        // before the uniforms + the line are built, so a drag lands this same frame.
        if is_wipe {
            self.handle_wipe_interaction(ui, image_rect);
        }
        // Recorded *after* the drag, so the readout splits on the line as drawn.
        self.last_wipe = is_wipe.then_some((self.wipe_center, self.wipe_angle));
        // Which pane blink is showing, so the readout reports the visible one.
        self.last_blink_b = blink_b;

        // Re-bake + upload the gradient LUTs on ramp change (stable handles otherwise).
        self.sync_gradient_luts(gpu_resources);

        let content = ui.ctx().content_rect();
        let mut uniform_data =
            self.build_frame_uniforms(image_rect, disp_rect, [content.width(), content.height()]);
        // A stack is never a wipe, whatever the A/B `compare_mode` the shared
        // `build_frame_uniforms` reads says; the comp Wipe arrangement drives it here.
        uniform_data.is_wipe_mode = u32::from(is_wipe);

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
            // Left inert. The overscan dim (#251) is applied once, in the display
            // blit — `fs_main`'s `eff_opacity` is discarded for every draw with
            // `skip_checker == 1`, which is all of the comp layers, and under OCIO the
            // dim has to happen post-transform anyway. Setting it here as well would
            // dim twice on any path that honours it *and* reaches the blit.
            overscan_factor: std::cell::Cell::new(1.0f32),
            neutral_view_ops: std::cell::Cell::new(false),
            blend_override: std::cell::Cell::new(None),
            fold_rect: std::cell::Cell::new(None),
            ocio_sig: std::cell::Cell::new(0xcbf29ce484222325u64),
            ocio_draws: std::cell::RefCell::new(Vec::new()),
        };

        let painter = ui.painter().with_clip_rect(rect);
        // Reserve the image slot BEFORE the divider so the GPU quad renders *beneath*
        // it (same layer, insertion order) — appending the callback last would paint
        // the composite straight over the line. Mirrors `draw_comp_composite`'s slot.
        let slot = painter.add(egui::Shape::Noop);

        // The union of every rect drawn into the scene, which is what the display stage
        // must cover (#257) — the scissor below, and the blit's display window. Declared
        // here rather than at the layer loop because Wipe / Diff / Blink draw *before*
        // it and are not part of it: with per-layer sizing (#254) a Blink B pane can be
        // wider than `image_rect`, and a union that only saw the stack would scissor the
        // display stage back to the canvas and crop it.
        let mut union_rect: Option<egui::Rect> = None;
        // A free fn rather than a closure: a closure capturing `union_rect` mutably
        // would keep the borrow alive past the last `cover` call and block reading it
        // for the scissor below.
        fn cover(union: &mut Option<egui::Rect>, r: egui::Rect) {
            *union = Some(union.map_or(r, |u| u.union(r)));
        }

        // Wipe / Diff: one draw binding pane A as `tex_a` and pane B as `tex_b`, both at
        // the same rect; the shader splits on the wipe line or emits the difference heat
        // map. Diff is a display-space false colour, so `DrawCtx::draw` routes it past
        // the accumulate path to an immediate callback (`ocio_draws` stays empty and the
        // early return below is the exit) — the same opt-out the A/B path uses.
        if is_wipe || is_diff {
            let b = side_b.as_ref().expect("wipe/diff imply a resolved pane B");
            ctx.draw(
                &painter,
                draws[0].bind_group.clone(),
                Some(b.draw.bind_group.clone()),
                rect,
                image_rect,
                is_diff,
                false, // not an accumulate fold: tex_b is an image, not the scene
                1.0,
            );
            cover(&mut union_rect, image_rect);
            if is_wipe {
                // The draggable wipe line + handle, over the image (the reserved slot
                // keeps the GPU quad underneath). Clipped to the image like the A/B
                // path: the endpoints are deliberately pushed past the rect's diagonal
                // so the line spans the image at any angle, and without the clip that
                // overshoot runs across the whole viewport.
                let wp = painter.with_clip_rect(image_rect);
                let w = wipe_line_endpoints(image_rect, self.wipe_center, self.wipe_angle);
                let color = egui::Color32::from_white_alpha((self.wipe_line_opacity * 255.0) as u8);
                wp.line_segment([w.p1, w.p2], (2.0, color));
                wp.circle_filled(w.center, 8.0, color);
            }
        }

        // Blink: one draw at the full rect, binding the pane the phase selects. Pane A
        // reuses the normal accumulate fold below (it is a one-layer "composite"), so
        // only the B phase needs its own draw.
        if blink_b == Some(true)
            && let Some(b) = side_b.as_ref()
        {
            // Pane B at its own display size (#254), like every other layer. Pane A is
            // the accumulate fold below, which already places itself, so both phases of
            // the blink now hold their own aspect instead of the B phase snapping to
            // the canvas the A phase happened to set.
            let rect_b = comp_layer_rect(
                image_rect.center(),
                self.scale,
                b.draw.tex_size,
                self.sanitize_unsqueeze(b.draw.eff_par),
            );
            ctx.draw(
                &painter,
                b.draw.bind_group.clone(),
                None,
                rect,
                rect_b,
                false, // is_diff
                false, // is_composite
                b.draw.opacity,
            );
            // Without this the display stage would be scissored back to `image_rect`
            // (pane A's canvas) and crop the B pane wherever its own PAR made it wider.
            cover(&mut union_rect, rect_b);
        }

        // Wipe/Diff emitted their single 2-input draw, and Blink's B phase its own; the
        // accumulate fold below is for Stacked (the whole composite), Side-by-Side pane
        // A, and Blink's A phase.
        let stack_draws = if is_wipe || is_diff || blink_b == Some(true) {
            &[][..]
        } else {
            draws
        };
        let n = stack_draws.len();
        // Per-layer resolved geometry, logged on change. The playback trace describes
        // decode and residency but has never said a word about *where* each layer
        // landed, so a placement bug could only be investigated by measuring the
        // picture in a screenshot — which is how #254 was found, and how long it took
        // to characterize.
        //
        // Signature-gated rather than throttled: geometry is static while nothing
        // moves, so "log when it differs" is silent in the steady state and immediate
        // on the frame that changes. Formatting is skipped entirely unless the target
        // is enabled, so this costs a bool check in a normal run.
        let debug_geom = log::log_enabled!(target: "floki::playback", log::Level::Debug);
        let mut geom: Vec<String> = Vec::new();
        // Rect + data-window geometry of the current layer, and of the canvas layer as
        // a fallback, both filled in by the loop.
        let mut current_bbox: Option<(egui::Rect, (i32, i32), egui::Vec2)> = None;
        let mut canvas_bbox: Option<(egui::Rect, (i32, i32), egui::Vec2)> = None;
        for (i, d) in stack_draws.iter().enumerate() {
            // Each layer at its own display size (#254), centred on the canvas — the
            // composite no longer resolves one pixel aspect and stretches everything
            // to the bottom layer's rect. This generalizes what `side_by_side_layout`
            // has always done for its second pane (`tex_size * scale * its own par`)
            // from two panes to N layers.
            //
            // Centring, not offsetting: `Layer::transform` carries a canvas-space
            // offset and is still ignored by the renderer, so honouring it is a
            // separate change. Every layer shares a centre until then.
            let layer_par = self.sanitize_unsqueeze(d.eff_par);
            let layer_rect =
                comp_layer_rect(image_rect.center(), self.scale, d.tex_size, layer_par);
            cover(&mut union_rect, layer_rect);
            // The current layer's placed rect, for the bbox half of the overscan
            // overlay (#251). Taken here rather than recomputed after the loop so it
            // is the exact rect this layer was drawn at, not a second derivation of it
            // that could drift.
            if d.is_current {
                current_bbox = Some((layer_rect, d.data_pos, d.tex_size));
            }
            // The canvas layer's own rect, as the fallback below.
            if i == 0 {
                canvas_bbox = Some((layer_rect, d.data_pos, d.tex_size));
            }
            if debug_geom {
                // Printing each layer's own rect beside its own aspect is the point:
                // a `par=2.000` layer handed the same rect as a `par=1.000` one is
                // #254, stated in one line.
                //
                // `par` is the layer's *effective* aspect (#263) — a manual override
                // if it has one, else its header PAR — and `eff` is that after the
                // global unsqueeze toggle. Whether the value came from the header or
                // an override is deliberately not carried on `CompDraw`: one aspect
                // field that is always the one to draw with beats two that a caller
                // can pick wrongly, and the Layers panel shows both figures anyway.
                geom.push(format!(
                    "l{i}:par={:.3},eff={:.3},x=[{:.0},{:.0}],y=[{:.0},{:.0}]",
                    d.eff_par,
                    layer_par,
                    layer_rect.min.x,
                    layer_rect.max.x,
                    layer_rect.min.y,
                    layer_rect.max.y,
                ));
            }
            // The fold must cover this layer *and* everything already accumulated
            // beneath it — its pass clears the whole target, so a pixel it skips is
            // lost. `union_rect` is exactly that, and it already includes
            // `layer_rect`. While the layers share a rect this equals the draw's own
            // rect, so the rasterized area is unchanged from before #257.
            ctx.fold_rect.set(union_rect);
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
                layer_rect,
                false, // is_diff
                is_composite,
                d.opacity,
            );
        }
        ctx.neutral_view_ops.set(false);
        ctx.blend_override.set(None);
        // The Side-by-Side overlay below is a placed copy, not a fold — clear the hint
        // so it rasterizes its own pane rect rather than the composite's union.
        ctx.fold_rect.set(None);
        if debug_geom {
            // `canvas` is the rect every layer shares; `disp` is the display window the
            // overscan gate and the background gradient key off. Both are here because
            // a layer drawn outside either one is exactly the class of bug this line
            // exists to catch — and on #254 they turned out to be different bugs.
            // `union` is what the display stage is scissored to (#257) — the region
            // the composite can actually reach. A layer rect outside it is the
            // black-outside-disp bug, now visible in the log rather than only in a
            // screenshot.
            let u = union_rect.unwrap_or(image_rect);
            let line = format!(
                "evt=layer_geom n={} base_par={:.3} par={:.3} canvas=[{:.0},{:.0}]x[{:.0},{:.0}] \
                 disp=[{:.0},{:.0}]x[{:.0},{:.0}] union=[{:.0},{:.0}]x[{:.0},{:.0}] {}",
                n,
                base_par,
                par,
                image_rect.min.x,
                image_rect.max.x,
                image_rect.min.y,
                image_rect.max.y,
                disp_rect.min.x,
                disp_rect.max.x,
                disp_rect.min.y,
                disp_rect.max.y,
                u.min.x,
                u.max.x,
                u.min.y,
                u.max.y,
                geom.join(" "),
            );
            if self.dbg_layer_geom.as_deref() != Some(line.as_str()) {
                log::debug!(target: "floki::playback", "{line}");
                self.dbg_layer_geom = Some(line);
            }
        }

        // Always-on display-window resolution ("format") readout at the image's
        // bottom-right (#251). Present on every image before the #99 A/B collapse
        // deleted it with the old render path, and never restored on the comp path —
        // this is that label, in its original position, styling and alignment.
        //
        // It reports the format of whatever **pane A** is showing: the composite's
        // canvas layer when Stacked, and the current layer in a compare (the app
        // hands `base_size` / `comp_format` over to that layer there). That is
        // "the viewer's format" in the Nuke sense, rather than each input's.
        //
        // Suppressed in Side-by-Side, exactly as the original was: the two panes are
        // different layers at possibly different formats, so a single label under
        // both of them would be naming one pane while appearing to describe the pair.
        // Per-layer formats are in the Layers panel, which is where a mismatch is
        // meant to be compared.
        //
        // Anchored to `disp_rect`, which is now the real display window on an
        // overscanned render — so the label sits on the box it names.
        //
        // Drawn with the same unclipped painter as the annotations below, and before
        // the early return, so the Diff path gets it too.
        if let Some(f) = self.comp_format
            && !is_sbs
        {
            let (fw, fh) = f.disp_size;
            painter.text(
                disp_rect.right_bottom() + egui::vec2(0.0, 5.0),
                egui::Align2::RIGHT_TOP,
                format!("{fw}x{fh}"),
                egui::FontId::proportional(12.0),
                egui::Color32::from_gray(200),
            );
        }

        // The overscan / crop overlay (#251), the other half of the readout above: the
        // white display-window box with its corner coordinates, and the orange
        // data-window box with how far past the format the pixels actually run.
        //
        // Only drawn when the two windows differ — on a render where they agree the
        // boxes would be one rectangle drawn twice, which is noise. The data window
        // exceeding the display window is a normal and important property of a render,
        // and until this there was nothing in the UI that said so: not that a frame has
        // overscan, nor how much, nor where the format boundary sits.
        // The two boxes describe different things on purpose (Nuke's split): the white
        // one is the **canvas layer's** display window — the shot format — and the
        // orange one is the **current layer's** bounding box. A layer whose bbox does
        // not match the format is precisely what this is for, so forcing both onto one
        // layer would hide the case worth seeing.
        //
        // The current layer need not be *drawn* — it can be hidden, soloed out, or
        // trimmed away at this frame — and when it isn't, the overlay falls back to the
        // canvas layer. Without that fallback the whole overlay vanished whenever the
        // `Layer:` selection pointed at an invisible layer, taking the format box with
        // it: a 40%-overscan render on screen with nothing marking its boundary, because
        // of a selection that wasn't contributing a pixel. The layer you are actually
        // looking at is the one to describe.
        let bbox = current_bbox
            .or(canvas_bbox)
            .zip(canvas_format)
            .map(|((r, pos, size), f)| {
                let combined = CompFormat {
                    data_pos: pos,
                    data_size: (size.x as usize, size.y as usize),
                    ..f
                };
                (r, combined, overscan_of(combined))
            });
        if let Some((bbox_rect, f, o)) = bbox.filter(|(_, _, o)| o.overscanned || o.cropped) {
            // Display window: dashed white, the format boundary.
            draw_dashed_rect(
                &painter,
                disp_rect,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 100),
                5.0,
                5.0,
            );
            // Its corners in EXR coordinates, so the numbers match what the file says
            // rather than where it happens to sit on screen.
            painter.text(
                disp_rect.left_bottom() + egui::vec2(0.0, 5.0),
                egui::Align2::LEFT_TOP,
                format!("{},{}", f.disp_pos.0, f.disp_pos.1),
                egui::FontId::proportional(12.0),
                egui::Color32::GRAY,
            );
            painter.text(
                disp_rect.right_top() - egui::vec2(0.0, 5.0),
                egui::Align2::RIGHT_BOTTOM,
                format!(
                    "{},{}",
                    f.disp_pos.0 + f.disp_size.0 as i32,
                    f.disp_pos.1 + f.disp_size.1 as i32
                ),
                egui::FontId::proportional(12.0),
                egui::Color32::GRAY,
            );
            // Data window: dashed orange, the current layer's actual pixel coverage.
            let orange = egui::Color32::from_rgb(255, 200, 100);
            draw_dashed_rect(
                &painter,
                bbox_rect,
                egui::Color32::from_rgba_unmultiplied(255, 200, 100, 180),
                4.0,
                4.0,
            );
            // Size, origin, and how much of it is overscan. The percentage is the part
            // that was never there before: pixel dimensions answer "how big", but
            // "+14.6% × +20.4%" is the figure you actually compare between a plate and
            // a render, and it is the one a delivery spec is written in.
            painter.text(
                bbox_rect.right_bottom() + egui::vec2(5.0, 5.0),
                egui::Align2::LEFT_TOP,
                format!(
                    "{}: {}x{} (pos: {}, {})  {:+.1}% × {:+.1}%",
                    o.label(),
                    f.data_size.0,
                    f.data_size.1,
                    f.data_pos.0,
                    f.data_pos.1,
                    o.extra_w_pct,
                    o.extra_h_pct,
                ),
                egui::FontId::proportional(12.0),
                orange,
            );
        }

        // Annotation overlay + the in-place text field (#45 / #99 Slice 3d). Drawn here
        // rather than at the end of the function so the Diff path — which emits an
        // immediate callback and so leaves `ocio_draws` empty, taking the early return
        // below — still gets them. Order is safe either way: the composite paints into
        // the slot reserved above, so anything appended after that sits on top.
        self.draw_annotations(&painter, image_rect, view_scale);
        self.annotation_text_popup(ui, image_rect, view_scale);

        let ocio_draws = std::mem::take(&mut *ctx.ocio_draws.borrow_mut());
        if ocio_draws.is_empty() {
            // `force_accumulate` means every drawable layer accumulated; empty here
            // only if `draws` had no bindable layer — nothing to composite.
            return;
        }

        // Side B (the current layer) is a *single* layer, so it needs no ping-pong: emit
        // it as an independent placed draw after taking the accumulate group, and the
        // callback lays it into the scene beside side A with `LoadOp::Load`. It gets the
        // global view ops like any standalone image (`neutral_view_ops` stays false), so
        // both panes are exposed and display-transformed identically.
        // Side-by-Side only — `rect_b` is `None` elsewhere, so Wipe/Diff (which already
        // consumed pane B as `tex_b` above) can't also place it as a second pane.
        let overlay_draws = match (panes.rect_b, side_b.as_ref()) {
            (Some(rect_b), Some(b)) => {
                ctx.draw(
                    &painter,
                    b.draw.bind_group.clone(),
                    None,
                    rect,
                    rect_b,
                    false, // is_diff
                    false, // is_composite — a plain placed copy, not an accumulate fold
                    b.draw.opacity,
                );
                let taken = std::mem::take(&mut *ctx.ocio_draws.borrow_mut());
                // The divider, drawn over both panes like the A/B path's.
                if let Some(x) = panes.divider_x {
                    painter.line_segment(
                        [
                            egui::pos2(x, disp_rect.min.y),
                            egui::pos2(x, disp_rect.max.y),
                        ],
                        (2.0, egui::Color32::GRAY),
                    );
                }
                taken
            }
            _ => Vec::new(),
        };
        // The region the display stage covers: the panes plus whatever the layers
        // actually reach (#254 — a layer can now be wider than the canvas). `disp_rect`
        // alone would clip the background gradient's normalization to the base layer
        // in Stacked; `union_rect` alone would drop pane B in Side-by-Side, where the
        // union only spans the stack draws. Both, so neither case loses.
        let covered = disp_rect.union(union_rect.unwrap_or(disp_rect));
        let blit_uniforms = crate::gpu::BlitUniforms {
            display_min: [covered.min.x, covered.min.y],
            display_max: [covered.max.x, covered.max.y],
            // The overscan dim (#251) lives *here*, not in `fs_main`: that path
            // computes an `eff_opacity` from `Uniforms::overscan_factor`, but
            // `out_a = select(eff_opacity, a, skip_checker == 1u)` discards it for the
            // comp layers, which all run with `skip_checker == 1`. Under OCIO the dim
            // has to be applied post-transform anyway, which is why it was put in the
            // blit — so setting the per-draw factor alone dimmed nothing at all.
            dim_min: [disp_rect.min.x, disp_rect.min.y],
            dim_max: [disp_rect.max.x, disp_rect.max.y],
            screen_size: [content.width(), content.height()],
            overscan_factor: if canvas_overscanned {
                self.overscan_opacity
            } else {
                1.0
            },
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
        // Salt the arrangement in too, so switching Stacked↔Side-by-Side always
        // re-renders even if the per-draw uniforms happened to hash the same.
        let arrangement_salt = if is_sbs { 0x517C_C1B7_2722_0A95 } else { 0 };
        let render_sig =
            (ctx.ocio_sig.get() ^ self.ocio_render_gen ^ display_stage_salt ^ arrangement_salt)
                .wrapping_mul(0x100000001b3);
        // Side-by-Side spans the canvas with two panes, so the display transform runs
        // unscissored rather than over just the composite's rect (the A/B path does the
        // same); otherwise scissor to the drawn image region.
        //
        // That region is the UNION of the layer rects, not the base layer's (#257). The
        // display stage clears to transparent and only writes inside the scissor, so a
        // layer reaching past the base rect would otherwise be accumulated correctly and
        // then dropped on the floor — black, exactly like the accumulate clip this pairs
        // with. Fixing one without the other fixes nothing. Falls back to `image_rect`
        // when no layer accumulated (Wipe / Diff / Blink-B took their own path).
        let scissor_rect = union_rect.unwrap_or(image_rect);
        let scissor_pts = (!is_sbs).then_some([
            scissor_rect.min.x,
            scissor_rect.min.y,
            scissor_rect.max.x,
            scissor_rect.max.y,
        ]);
        let callback = crate::gpu::ocio_pass::OcioCallback {
            draws: ocio_draws,
            accumulate: true,
            // OCIO off → the display stage is the sRGB display-encode pass (R2).
            use_display_encode: !self.ocio_active,
            overlay_draws,
            display_format: render_state.target_format,
            blit_uniforms,
            scissor_pts,
            render_sig,
        };
        painter.set(
            slot,
            eframe::egui_wgpu::Callback::new_paint_callback(painter.clip_rect(), callback),
        );
    }

    /// Build a GPU texture + bind group for one layer of an `ExrData`, returning
    /// the [`LayerTexture`] (which owns the `Texture` handle so the VRAM is released
    /// when it drops — see the type docs for why eviction is drop-only rather than an
    /// explicit `Texture::destroy()`). The shader applies channel isolation, exposure,
    /// gamma, sRGB and every arrangement, so this one generator serves them all.
    ///
    /// **Not UI-thread bound.** It takes a [`crate::gpu::TexBuildCtx`] rather than
    /// the whole `GpuResources` precisely so it can run on the upload worker
    /// ([`crate::tex_upload`], #202) — every wgpu handle it touches is `Send`, and
    /// `queue.write_texture` is safe from any thread.
    fn build_layer_texture(
        ctx: &crate::gpu::TexBuildCtx,
        exr_data: &ExrData,
        layer_index: usize,
    ) -> Option<LayerTexture> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
        let width = layer.size.0;
        let height = layer.size.1;

        use eframe::egui_wgpu::wgpu;
        let device = &ctx.device;
        let queue = &ctx.queue;

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
        // Phase timing (#202). `t` is advanced by each `lap()`, so the four phases
        // partition this function's wall time without double-counting.
        let mut t = std::time::Instant::now();
        let lap = |t: &mut std::time::Instant| {
            let now = std::time::Instant::now();
            let d = now.duration_since(*t);
            *t = now;
            d
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
        let alloc_dur = lap(&mut t);

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
        // Interleave into the thread's reusable scratch, then hand it to wgpu.
        //
        // The scratch is **thread-local and reused** (#202). It used to be a fresh
        // `Vec` per call on the comp path, which meant every build allocated *and
        // zero-filled* a buffer — 111 MB on 4.6K plate footage — immediately before
        // overwriting every byte of it. Removing that redundant memset alone took
        // the interleave from ~31 ms to ~5 ms per build and roughly tripled
        // displayed throughput. Thread-local rather than passed in, because the
        // builders now run on the upload pool as well as the UI thread.
        let (pack_dur, upload_bytes) = STAGING.with_borrow_mut(|staging| {
            // Pack rows in parallel (a 4K layer is ~8M pixels — single-threaded was
            // a noticeable stall on layer switch).
            if use_f16 {
                // Fast path (#142 U2): copy half bit-patterns straight from the
                // source slices — `to_bits` is a reinterpret, so this skips the
                // per-pixel `to_f32` widening the F32 path pays. Absent channels
                // default: rgb → 0 (half `0x0000`), alpha → 1.0.
                let r_s = crate::pixels::f16_slice(r_chan);
                let g_s = crate::pixels::f16_slice(g_chan);
                let b_s = crate::pixels::f16_slice(b_chan);
                let a_s = crate::pixels::f16_slice(a_chan);
                let one = f16::from_f32(1.0).to_bits();
                // Two `u16` per `f32`, so half as many elements back the same
                // number of texels. The narrowing cast is always aligned.
                staging.resize(width * height * 2, 0.0);
                let pixels: &mut [u16] = bytemuck::cast_slice_mut(staging.as_mut_slice());
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
                let pack_dur = lap(&mut t);
                queue.write_texture(dst, bytemuck::cast_slice(&*pixels), buf_layout(2), extent);
                (pack_dur, (width * height * 4 * 2) as u64)
            } else {
                // F32/U32 sources: hoist the F32 slices (direct index) and widen the
                // rest per pixel via `sample_channel`.
                let r_s = sample_channel_f32(r_chan);
                let g_s = sample_channel_f32(g_chan);
                let b_s = sample_channel_f32(b_chan);
                let a_s = sample_channel_f32(a_chan);
                // Same buffer as the F16 path, used at its native type — no cast,
                // so no alignment question.
                staging.resize(width * height * 4, 0.0);
                let pixels = staging.as_mut_slice();
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
                let pack_dur = lap(&mut t);
                queue.write_texture(dst, bytemuck::cast_slice(&*pixels), buf_layout(4), extent);
                (pack_dur, (width * height * 4 * 4) as u64)
            }
        });
        let write_dur = lap(&mut t);

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // GpuState is app-owned (#54) — read it directly off the build context
        // instead of the renderer typemap lookup.
        let gpu_state = ctx.gpu_state.as_ref();

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
        tex_build_stats::record(alloc_dur, pack_dur, write_dur, lap(&mut t), upload_bytes);

        Some(LayerTexture {
            texture,
            bind_group: std::sync::Arc::new(bind_group),
        })
    }

    /// Build a standalone GPU texture + bind group for one AOV of an `ExrData`,
    /// for the Layers-panel composite sources (#99 PR-B.2). Wraps
    /// [`Self::build_layer_texture`] (all the F16/F32 packing logic) and hands back
    /// just the two GPU handles the caller owns — the `Texture` (kept to own the
    /// VRAM) and the `Arc<BindGroup>` (bound as a composite layer). `None` if the
    /// AOV is out of range or the upload fails.
    ///
    /// Safe to call from any thread, and normally called from
    /// [`crate::tex_upload`]'s worker rather than the paint thread (#202).
    pub(crate) fn build_source_texture(
        ctx: &crate::gpu::TexBuildCtx,
        exr_data: &ExrData,
        aov: usize,
    ) -> Option<(
        eframe::egui_wgpu::wgpu::Texture,
        std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>,
    )> {
        let t = Self::build_layer_texture(ctx, exr_data, aov)?;
        Some((t.texture, t.bind_group))
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

    /// Drop the cached image-A contact-sheet **thumbnails** (CPU + GPU) so the
    /// sheet re-bakes from the newly swapped data. Skipped while the transport is
    /// busy (`ExrApp::thumbs_suppressed`) and run once on settle (#144), so the
    /// sheet freezes during playback instead of re-baking every layer per frame.
    pub fn invalidate_active_thumbnails(&mut self) {
        self.thumbnails.fill(None);
        self.invalidate_gpu_thumbnails();
    }

    /// Apply the canvas zoom/pan interaction for one frame from `response`:
    /// first-frame fit-to-view, cursor-centered wheel/pinch zoom, and drag pan
    /// (suppressed while an annotation tool is active). Extracted from
    /// the comp path so the proxy first-paint path shares
    /// the exact same interaction model — the handoff from proxy to full-res is
    /// visually continuous because zoom/pan state is identical.
    fn handle_canvas_interaction(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        tex_size: egui::Vec2,
        tex_size_b: Option<egui::Vec2>,
        side_by_side: bool,
    ) {
        if self.first_frame {
            // Fit the whole visible layout, not just A: Side-by-Side is wider than
            // the A image (A + B), so framing on `tex_size` alone clips B.
            let fit = framing_bounds(
                side_by_side,
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

    /// Compute the 256-bin luminance histogram of `data`'s logical layer `layer_idx`
    /// (`None` if that layer has no resolvable RGB). Parallelized per-row: each
    /// thread accumulates its own `[u32; 256]`, then reduce by summing — for a 4K
    /// layer this is ~8M iterations, a noticeable single-threaded stall on every
    /// layer / log-scale change. Shared by the classic A/B and comp-layer entry
    /// points below.
    fn histogram_bins(data: &ExrData, layer_idx: usize, log_histogram: bool) -> Option<[u32; 256]> {
        let (layer, r_chan, g_chan, b_chan, _) = data.logical_channels(layer_idx)?;
        let width = layer.size.0;
        let height = layer.size.1;

        // Hoist F32 slices (common case) for direct indexing.
        let r_s = sample_channel_f32(r_chan);
        let g_s = sample_channel_f32(g_chan);
        let b_s = sample_channel_f32(b_chan);

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
    }

    /// Comp-path histogram (#99 R4): the current layer's source at `layer_idx`
    /// (its AOV), keyed by `disc` (its `SourceId`) so switching layers/AOVs
    /// recomputes. Mirrors
    /// [`Self::calculate_histogram`]'s cache gate.
    pub fn calculate_histogram_for(&mut self, exr_data: &ExrData, layer_idx: usize, disc: u64) {
        let key = (disc, layer_idx, self.log_histogram);
        if self.histogram_key == Some(key) {
            return;
        }
        self.histogram = Self::histogram_bins(exr_data, layer_idx, self.log_histogram);
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

/// One value block of the pixel tooltip: the per-channel coloured RGBA numbers, a
/// solid colour patch of the value, and its HSVL line. Factored out of the classic
/// tooltip so the comp path can reuse it verbatim (#99 Slice 3d).
fn pixel_value_block(ui: &mut egui::Ui, prefix: &str, val: [f32; 4]) {
    ui.horizontal(|ui| {
        colored_rgba_label(ui, prefix, val);
        let (r, g, b) = (
            (val[0].clamp(0.0, 1.0) * 255.0) as u8,
            (val[1].clamp(0.0, 1.0) * 255.0) as u8,
            (val[2].clamp(0.0, 1.0) * 255.0) as u8,
        );
        let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_rgb(r, g, b));
    });
    let (h, s, v, l) = rgb_to_hsvl(val[0], val[1], val[2]);
    ui.label(
        egui::RichText::new(format!("H:{h:.0} S:{s:.2} V:{v:.2} L:{l:.5}"))
            .color(egui::Color32::LIGHT_GRAY),
    );
}

/// The floating pixel-readout tooltip that follows the cursor, showing the sampled
/// coordinate and value(s). `val_b` is the second value in a two-input compare (the
/// classic A/B path); when both are present a `Diff:` row is appended. Shared by the
/// classic viewport and the comp path (#99 Slice 3d), which passes `val_b = None`
/// because its readout samples whichever single pane the cursor is over.
pub(crate) fn pixel_tooltip_window(
    ctx: &egui::Context,
    pos: egui::Pos2,
    x: usize,
    y: usize,
    val_a: Option<[f32; 4]>,
    val_b: Option<[f32; 4]>,
) {
    if val_a.is_none() && val_b.is_none() {
        return;
    }
    egui::Window::new("Pixel Tooltip")
        .fixed_pos(pos + egui::vec2(15.0, 15.0))
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.label(format!("x={x} y={y}"));
            if let Some(a) = val_a {
                pixel_value_block(ui, if val_b.is_some() { "A:" } else { "" }, a);
            }
            if let Some(b) = val_b {
                pixel_value_block(ui, "B:", b);
            }
            if let (Some(a), Some(b)) = (val_a, val_b) {
                let diff = [
                    (b[0] - a[0]).abs(),
                    (b[1] - a[1]).abs(),
                    (b[2] - a[2]).abs(),
                    (b[3] - a[3]).abs(),
                ];
                colored_rgba_label(ui, "Diff:", diff);
            }
        });
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

/// Stroke a dashed rectangle. Restored with the overscan overlay (#251) — it went out
/// with the A/B render path and has no other caller.
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
    //! drives the comp-path entry points headlessly (no GPU `gpu_resources`)
    //! to guard the hotkey / contact-sheet / geometry seams.
    use super::{ChannelMode, ExrViewer};
    use crate::annotation::{Annotation, AnnotationKind, AnnotationTool};
    use crate::exr_loader::ExrData;
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

    /// A realistic overscanned render: 1920×1080 format with a 140px margin all
    /// round, so the data window is 2200×1300 at origin (-140, -110).
    fn overscan_fixture() -> super::CompFormat {
        super::CompFormat {
            disp_pos: (0, 0),
            disp_size: (1920, 1080),
            data_pos: (-140, -110),
            data_size: (2200, 1300),
        }
    }

    /// The percentage is against the **display window** and totals both sides, so a
    /// symmetric 140px margin on a 1920 format reads +14.6%, not +7.3%. Getting this
    /// backwards would halve every figure and still look plausible, which is why the
    /// arithmetic is pinned rather than eyeballed on screen.
    #[test]
    fn overscan_is_measured_against_the_format_across_both_sides() {
        let o = super::overscan_of(overscan_fixture());
        assert!(o.overscanned);
        assert!(!o.cropped);
        assert_eq!((o.left, o.right, o.top, o.bottom), (140, 140, 110, 110));
        // 280 extra px on 1920 = 14.583%, 220 on 1080 = 20.370%.
        assert!((o.extra_w_pct - 14.583_334).abs() < 1e-3, "{o:?}");
        assert!((o.extra_h_pct - 20.370_37).abs() < 1e-3, "{o:?}");
    }

    /// A data window *inside* the display window is a crop, not an overscan, and the
    /// overlay says so — the original only ever had one word for both, and a cropped
    /// render labelled "Overscan: …" reads as the opposite of what happened.
    #[test]
    fn a_data_window_inside_the_format_reads_as_a_crop() {
        let o = super::overscan_of(super::CompFormat {
            disp_pos: (0, 0),
            disp_size: (1920, 1080),
            data_pos: (100, 50),
            data_size: (1000, 500),
        });
        assert!(o.cropped);
        assert!(!o.overscanned);
        assert!(o.extra_w_pct < 0.0 && o.extra_h_pct < 0.0);
        // Margins are negative on every side: the pixels fall short all round.
        assert_eq!((o.left, o.right, o.top, o.bottom), (-100, -820, -50, -530));

        // Equal windows are neither, so the overlay draws nothing — the common case,
        // where two identical boxes would just be visual noise.
        let none = super::overscan_of(super::CompFormat {
            disp_pos: (0, 0),
            disp_size: (64, 64),
            data_pos: (0, 0),
            data_size: (64, 64),
        });
        assert!(!none.overscanned && !none.cropped);

        // A degenerate format yields no percentage rather than a division by zero;
        // the pixel margins are still well defined and still reported.
        let zero = super::overscan_of(super::CompFormat {
            disp_pos: (0, 0),
            disp_size: (0, 0),
            data_pos: (0, 0),
            data_size: (8, 8),
        });
        assert_eq!((zero.extra_w_pct, zero.extra_h_pct), (0.0, 0.0));
        assert_eq!((zero.right, zero.bottom), (8, 8));
    }

    /// A real 40% overscan render (the `redSea` beauty from a dogfood session): 3225×2215
    /// of pixels in a 2304×1582 format, the data window starting at (-460, -317).
    ///
    /// Round numbers on real footage are the useful check here — a 40% overscan should
    /// read as 40%, and any confusion between "per side" and "across both sides" would
    /// show up immediately as 20%.
    #[test]
    fn a_real_render_reports_its_overscan_as_the_delivery_spec_states_it() {
        let o = super::overscan_of(super::CompFormat {
            disp_pos: (0, 0),
            disp_size: (2304, 1582),
            data_pos: (-460, -317),
            data_size: (3225, 2215),
        });
        assert!(o.overscanned && !o.cropped);
        assert!((o.extra_w_pct - 40.0).abs() < 0.05, "{o:?}");
        assert!((o.extra_h_pct - 40.0).abs() < 0.05, "{o:?}");
        // Margins are very nearly symmetric, as a render with uniform overscan is.
        assert_eq!((o.left, o.top), (460, 317));
        assert_eq!((o.right, o.bottom), (461, 316));
    }

    /// A data window that is *offset* as well as differently sized overruns the format
    /// on one side and falls short on another, so it is both overscanned and cropped —
    /// and the caption has to say so.
    ///
    /// The overlay used to pick its word from `overscanned` alone, which named the
    /// half that costs nothing and hid the half that costs you: a side with no pixels
    /// where the format expects them, reported as "Overscan".
    #[test]
    fn a_window_that_overruns_one_side_and_falls_short_of_another_says_both() {
        let o = super::overscan_of(super::CompFormat {
            disp_pos: (0, 0),
            disp_size: (1920, 1080),
            // Shifted right and down: pixels past the right/bottom edges, none along
            // the left/top ones.
            data_pos: (200, 100),
            data_size: (1920, 1080),
        });
        assert!(o.overscanned, "overruns the right and bottom");
        assert!(o.cropped, "falls short along the left and top");
        assert_eq!(o.label(), "Overscan + crop");
        assert_eq!((o.left, o.top), (-200, -100));
        assert_eq!((o.right, o.bottom), (200, 100));
        // Same size as the format, so neither axis gained area — the percentage is
        // about size and the margins are about placement; a caption showing 0% beside
        // non-zero margins is correct, not a contradiction.
        assert_eq!((o.extra_w_pct, o.extra_h_pct), (0.0, 0.0));

        // The single-sided cases keep their plain labels.
        assert_eq!(super::overscan_of(overscan_fixture()).label(), "Overscan");
        assert_eq!(
            super::overscan_of(super::CompFormat {
                disp_pos: (0, 0),
                disp_size: (1920, 1080),
                data_pos: (100, 50),
                data_size: (1000, 500),
            })
            .label(),
            "Crop"
        );
    }

    /// The display-window box is placed from the layer's own rect, so restoring the
    /// overlay moves nothing: the data window stays exactly where #254 put it.
    #[test]
    fn the_display_box_is_positioned_relative_to_the_data_window() {
        let f = overscan_fixture();
        // Unzoomed, square pixels: 1px of image is 1px of screen.
        let layer = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2200.0, 1300.0));
        let disp = super::comp_display_rect(layer, egui::vec2(1.0, 1.0), f);
        // The data window starts 140px left and 110px above the format, so the format
        // box sits that far *inside* the image rect.
        assert_eq!(disp.min, egui::pos2(140.0, 110.0));
        assert_eq!(disp.size(), egui::vec2(1920.0, 1080.0));
        assert!(
            layer.contains_rect(disp),
            "an overscanned render's format box lies inside its pixels"
        );

        // Zoom and the anamorphic unsqueeze both ride on `view_scale`, so the box
        // tracks a squeezed image instead of drifting off it — x carries the PAR.
        let disp2 = super::comp_display_rect(layer, egui::vec2(4.0, 2.0), f);
        assert_eq!(disp2.min, egui::pos2(560.0, 220.0));
        assert_eq!(disp2.size(), egui::vec2(7680.0, 2160.0));
    }

    #[test]
    fn comp_layer_rect_gives_each_layer_its_own_display_aspect() {
        use super::comp_layer_rect;
        let center = egui::pos2(500.0, 300.0);

        // The #254 case: a 2:1 anamorphic plate (PAR 2.0) and a square-pixel previs of
        // the same pixel dimensions, in one stack. The plate must be twice as wide on
        // screen. Before this, both got the bottom layer's rect and the plate was
        // squeezed back to 1:1.
        let tex = egui::vec2(1000.0, 500.0);
        let plate = comp_layer_rect(center, 1.0, tex, 2.0);
        let previs = comp_layer_rect(center, 1.0, tex, 1.0);
        assert_eq!(plate.width(), 2000.0, "PAR 2.0 doubles the on-screen width");
        assert_eq!(previs.width(), 1000.0, "PAR 1.0 is a no-op");
        assert_eq!(
            plate.height(),
            previs.height(),
            "the unsqueeze is horizontal only — height never changes"
        );

        // Concentric, so the composite stays registered about the canvas centre and
        // the wider layer overhangs symmetrically rather than shifting the picture.
        assert_eq!(plate.center(), previs.center());
        assert_eq!(plate.center(), center);

        // Zoom multiplies both axes; the aspect ratio is scale-invariant.
        let zoomed = comp_layer_rect(center, 0.25, tex, 2.0);
        assert_eq!(zoomed.width(), 500.0);
        assert_eq!(zoomed.height(), 125.0);
        assert!(
            (zoomed.width() / zoomed.height() - plate.width() / plate.height()).abs() < 1e-6,
            "display aspect must not depend on zoom"
        );

        // Layers of *different* pixel dimensions keep their own size too — placement
        // reads the layer's texture, never the canvas.
        let small = comp_layer_rect(center, 1.0, egui::vec2(200.0, 100.0), 1.0);
        assert_eq!(small.width(), 200.0);
        assert_eq!(small.height(), 100.0);
    }

    #[test]
    fn side_by_side_layout_places_a_left_b_right_with_divider() {
        use super::side_by_side_layout;
        let center = egui::pos2(500.0, 300.0);
        let z = egui::Vec2::ZERO;
        let a = egui::vec2(200.0, 100.0);
        let b_tex = egui::vec2(100.0, 100.0);

        // No normalize, scale 1, par 1: B keeps its native size. A left, B right,
        // divider at A's right edge; combined width = 300, so left edge = 350.
        let l = side_by_side_layout(center, z, 1.0, a, b_tex, 1.0, false);
        assert_eq!(l.rect_a.min.x, 350.0, "A starts at the combined left edge");
        assert_eq!(l.rect_a.width(), 200.0);
        assert_eq!(l.divider_x, l.rect_b.min.x, "divider sits at B's left edge");
        assert_eq!(l.rect_b.min.x, l.rect_a.max.x, "B abuts A, no gap");
        assert_eq!(l.rect_b.width(), 100.0);
        // Both vertically centered on the same row.
        assert_eq!(l.rect_a.center().y, l.rect_b.center().y);
        assert_eq!(l.rect_a.center().y, center.y);

        // Normalize scales B's height to A's (100), so its 1:1 native becomes 100×100
        // — here already equal, but a taller B would shrink to match.
        let b_tall = egui::vec2(50.0, 200.0);
        let ln = side_by_side_layout(center, z, 1.0, a, b_tall, 1.0, true);
        assert_eq!(ln.rect_b.height(), 100.0, "B normalized to A's height");
        assert_eq!(ln.rect_b.width(), 25.0, "B width scaled proportionally");

        // Translation shifts the whole combined rect.
        let lt = side_by_side_layout(center, egui::vec2(10.0, -20.0), 1.0, a, b_tex, 1.0, false);
        assert_eq!(lt.rect_a.min.x, 360.0);
        assert_eq!(lt.rect_a.center().y, center.y - 20.0);
    }

    #[test]
    fn pick_comp_side_routes_the_cursor_to_the_pane_under_it() {
        use super::{CompSide, pick_comp_side, side_by_side_layout};
        let center = egui::pos2(500.0, 300.0);
        let l = side_by_side_layout(
            center,
            egui::Vec2::ZERO,
            1.0,
            egui::vec2(200.0, 100.0),
            egui::vec2(100.0, 100.0),
            1.0,
            false,
        );

        // Inside each pane → that pane.
        assert_eq!(
            pick_comp_side(egui::pos2(400.0, 300.0), l.rect_a, Some(l.rect_b)),
            Some(CompSide::A)
        );
        assert_eq!(
            pick_comp_side(egui::pos2(600.0, 300.0), l.rect_a, Some(l.rect_b)),
            Some(CompSide::B)
        );
        // Outside both → None (the readout blanks rather than reporting a lie).
        assert_eq!(
            pick_comp_side(egui::pos2(100.0, 300.0), l.rect_a, Some(l.rect_b)),
            None
        );
        assert_eq!(
            pick_comp_side(egui::pos2(400.0, 900.0), l.rect_a, Some(l.rect_b)),
            None
        );
        // The rects abut exactly; the shared edge belongs to B, matching the render
        // (B's overlay draw is laid down last and wins that column).
        assert_eq!(
            pick_comp_side(egui::pos2(l.divider_x, 300.0), l.rect_a, Some(l.rect_b)),
            Some(CompSide::B)
        );
        // No B pane (any non-side-by-side arrangement) → the composite owns the rect.
        assert_eq!(
            pick_comp_side(egui::pos2(400.0, 300.0), l.rect_a, None),
            Some(CompSide::A)
        );
        assert_eq!(
            pick_comp_side(egui::pos2(600.0, 300.0), l.rect_a, None),
            None
        );
    }

    #[test]
    fn comp_pane_layout_splits_only_in_side_by_side() {
        use super::comp_pane_layout;
        use crate::layer::Arrangement;
        let center = egui::pos2(500.0, 300.0);
        let z = egui::Vec2::ZERO;
        let size_a = egui::vec2(200.0, 100.0);
        let b = Some((egui::vec2(100.0, 100.0), 1.0));

        // Side-by-Side: two abutting panes, a divider, and a display region spanning
        // both.
        let sbs = comp_pane_layout(Arrangement::SideBySide, center, z, 1.0, size_a, b, false);
        let rect_b = sbs.rect_b.expect("side-by-side has a second pane");
        assert_eq!(sbs.image_rect.width(), 200.0);
        assert_eq!(rect_b.min.x, sbs.image_rect.max.x, "panes abut");
        assert_eq!(sbs.divider_x, Some(rect_b.min.x));
        assert_eq!(sbs.disp_rect, sbs.image_rect.union(rect_b));

        // Wipe and Diff combine both layers in ONE rect: pane A owns the whole image,
        // there is no second rect, and no divider. Regression guard — keying the split
        // on "side B exists" (it does for every compare) squeezed these into the left
        // half and drew a stray second pane beside them.
        for arr in [Arrangement::Wipe { position: 0.5 }, Arrangement::Diff] {
            let p = comp_pane_layout(arr, center, z, 1.0, size_a, b, false);
            assert_eq!(p.rect_b, None, "{arr:?} has no second pane");
            assert_eq!(p.divider_x, None, "{arr:?} has no divider");
            assert_eq!(p.image_rect.width(), 200.0, "{arr:?} keeps the full rect");
            assert_eq!(p.image_rect.center(), center, "{arr:?} stays centered");
            assert_eq!(p.disp_rect, p.image_rect, "{arr:?} display == image");
        }

        // Stacked ignores side B entirely, present or not.
        let st = comp_pane_layout(Arrangement::Stacked, center, z, 1.0, size_a, b, false);
        assert_eq!(st.rect_b, None);
        assert_eq!(st.image_rect.center(), center);
        // Side-by-Side with no resolvable pane B falls back to the single rect.
        let none = comp_pane_layout(Arrangement::SideBySide, center, z, 1.0, size_a, None, false);
        assert_eq!(none.rect_b, None);
        assert_eq!(none.image_rect.center(), center);
    }

    #[test]
    fn blink_phase_alternates_and_reports_the_next_flip() {
        use super::blink_phase;
        // 1s interval: [0,1) shows A, [1,2) shows B, and so on.
        assert_eq!(blink_phase(0.0, 1.0), (false, 1.0));
        assert_eq!(blink_phase(0.5, 1.0), (false, 1.0));
        assert_eq!(blink_phase(1.0, 1.0), (true, 2.0));
        assert!(blink_phase(1.9, 1.0).0);
        assert!(!blink_phase(2.0, 1.0).0, "flips back on the third beat");

        // A sub-second interval still alternates, and the next flip is ahead of now.
        let (_, next) = blink_phase(10.02, 0.25);
        assert!(next > 10.02 && next <= 10.27, "next flip {next} is ahead");
        assert_ne!(
            blink_phase(10.02, 0.25).0,
            blink_phase(10.30, 0.25).0,
            "adjacent quarter-second beats show different panes"
        );

        // Degenerate intervals must not divide by zero or panic — they pin to pane A.
        assert_eq!(blink_phase(5.0, 0.0), (false, 5.0));
        assert_eq!(blink_phase(5.0, -1.0), (false, 5.0));
        assert_eq!(blink_phase(5.0, f32::NAN), (false, 5.0));
        assert!(!blink_phase(f64::NAN, 1.0).0);
    }

    #[test]
    fn comp_hover_side_follows_the_blink_phase_not_the_cursor() {
        use super::{CompSide, comp_hover_side};
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0));

        // Blink fills the one rect with a single pane, so the phase decides — the same
        // cursor position reports A or B depending on which is on screen.
        let pos = egui::pos2(150.0, 100.0);
        assert_eq!(
            comp_hover_side(pos, rect, None, None, Some(false)),
            Some(CompSide::A)
        );
        assert_eq!(
            comp_hover_side(pos, rect, None, None, Some(true)),
            Some(CompSide::B)
        );
        // Still nothing outside the image.
        assert_eq!(
            comp_hover_side(egui::pos2(10.0, 100.0), rect, None, None, Some(true)),
            None
        );
    }

    #[test]
    fn wipe_line_endpoints_centers_and_spans_the_image() {
        use super::wipe_line_endpoints;
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0));

        // Centered, 0°: a vertical line through the rect's middle.
        let w = wipe_line_endpoints(rect, [0.5, 0.5], 0.0);
        assert_eq!(w.center, egui::pos2(200.0, 100.0));
        assert!((w.p1.x - w.center.x).abs() < 1e-3, "vertical at 0°: {w:?}");
        assert!((w.p2.x - w.center.x).abs() < 1e-3, "vertical at 0°: {w:?}");
        // Extended past the diagonal, so it always spans the image.
        let diag = rect.width().hypot(rect.height());
        assert!((w.p1 - w.center).length() >= diag - 1e-3);

        // The normalized centre maps across the rect.
        let off = wipe_line_endpoints(rect, [0.25, 0.75], 0.0);
        assert_eq!(off.center, egui::pos2(150.0, 125.0));

        // 90°: the line runs horizontally (perpendicular to the (cos, sin) normal).
        let w90 = wipe_line_endpoints(rect, [0.5, 0.5], 90.0);
        assert!(
            (w90.p1.y - w90.center.y).abs() < 1e-3,
            "horizontal: {w90:?}"
        );
    }

    #[test]
    fn comp_hover_side_splits_on_the_wipe_line_and_falls_back_to_rects() {
        use super::{CompSide, comp_hover_side};
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0));
        let wipe = Some(([0.5f32, 0.5f32], 0.0f32));

        // At 0° the normal is +x, so right of centre is tex_b (pane B) — matching
        // `fs_main`'s `dist >= 0` test.
        assert_eq!(
            comp_hover_side(egui::pos2(250.0, 100.0), rect, None, wipe, None),
            Some(CompSide::B)
        );
        assert_eq!(
            comp_hover_side(egui::pos2(150.0, 100.0), rect, None, wipe, None),
            Some(CompSide::A)
        );
        // Outside the image → no readout at all.
        assert_eq!(
            comp_hover_side(egui::pos2(50.0, 100.0), rect, None, wipe, None),
            None
        );
        // No wipe → the Side-by-Side two-rect split.
        let rect_b = egui::Rect::from_min_size(egui::pos2(300.0, 50.0), egui::vec2(200.0, 100.0));
        assert_eq!(
            comp_hover_side(egui::pos2(350.0, 100.0), rect, Some(rect_b), None, None),
            Some(CompSide::B)
        );
        assert_eq!(
            comp_hover_side(egui::pos2(150.0, 100.0), rect, Some(rect_b), None, None),
            Some(CompSide::A)
        );
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
    }

    fn harness() -> Harness<'static, State> {
        Harness::new_ui_state(
            |ui, s: &mut State| s.viewer.handle_channel_hotkeys(ui),
            State {
                viewer: ExrViewer::default(),
            },
        )
    }

    #[test]
    fn channel_keys_isolate_and_reset() {
        let mut h = harness();

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
        let mut h = harness();
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
        let mut h = harness();
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
        let mut h = harness();
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
    fn channel_hotkeys_work_via_comp_entry_point() {
        // The comp path never runs the full viewer `ui` (and thus `handle_hotkeys`),
        // so it calls `handle_channel_hotkeys` directly (#192). Driving that entry
        // point alone must still isolate channels — and stay inert with the contact
        // sheet open, matching the full hotkey path.
        let mut h = Harness::new_ui_state(
            |ui, s: &mut State| s.viewer.handle_channel_hotkeys(ui),
            State {
                viewer: ExrViewer::default(),
            },
        );

        h.key_press(egui::Key::B);
        h.run();
        assert_eq!(
            h.state().viewer.channel_mode,
            ChannelMode::B,
            "the comp-path channel entry point must isolate B"
        );

        h.state_mut().viewer.show_contact_sheet = true;
        h.key_press(egui::Key::R);
        h.run();
        assert_eq!(
            h.state().viewer.channel_mode,
            ChannelMode::B,
            "channel hotkeys must stay inert in contact-sheet mode via the comp entry point"
        );
    }

    #[test]
    fn tone_reset_hotkeys_work_via_comp_entry_point() {
        // E / Shift+G moved from `handle_hotkeys` (unreachable since the R4 collapse)
        // onto the comp entry point (#99 Slice 3a), because exposure/gamma still drive
        // the composite. Plain G must keep isolating the green channel, not reset gamma.
        let mut h = Harness::new_ui_state(
            |ui, s: &mut State| s.viewer.handle_channel_hotkeys(ui),
            State {
                viewer: ExrViewer {
                    exposure: 2.0,
                    gamma: 2.2,
                    ..ExrViewer::default()
                },
            },
        );

        h.key_press(egui::Key::E);
        h.run();
        assert_eq!(h.state().viewer.exposure, 0.0, "E resets exposure");
        assert_eq!(h.state().viewer.gamma, 2.2, "E leaves gamma alone");

        // Plain G is channel isolation, NOT a gamma reset.
        h.key_press(egui::Key::G);
        h.run();
        assert_eq!(h.state().viewer.gamma, 2.2, "plain G must not reset gamma");
        assert_eq!(
            h.state().viewer.channel_mode,
            ChannelMode::G,
            "plain G isolates the green channel"
        );

        h.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::G);
        h.run();
        assert_eq!(h.state().viewer.gamma, 1.0, "Shift+G resets gamma");

        // Inert with the contact sheet open, matching the channel keys.
        h.state_mut().viewer.exposure = 3.0;
        h.state_mut().viewer.show_contact_sheet = true;
        h.key_press(egui::Key::E);
        h.run();
        assert_eq!(
            h.state().viewer.exposure,
            3.0,
            "tone resets stay inert in contact-sheet mode"
        );
    }

    #[test]
    fn t_toggles_the_contact_sheet_both_ways() {
        // `T` sits above the suppression gate for the same reason F11/Esc do: the
        // gate keys off `show_contact_sheet`, so handling it below would let `T`
        // open the sheet but never close it.
        let mut h = harness();

        h.key_press(egui::Key::T);
        h.run();
        assert!(h.state().viewer.show_contact_sheet, "T opens the sheet");

        h.key_press(egui::Key::T);
        h.run();
        assert!(
            !h.state().viewer.show_contact_sheet,
            "T must also close the sheet — the gate would otherwise swallow it"
        );

        // The channel keys stay inert while the sheet is open, unlike T.
        h.state_mut().viewer.show_contact_sheet = true;
        let before = h.state().viewer.channel_mode;
        h.key_press(egui::Key::R);
        h.run();
        assert_eq!(h.state().viewer.channel_mode, before);
    }

    #[test]
    fn fullscreen_and_esc_work_via_comp_entry_point() {
        // F11 / Esc moved onto the comp entry point (#99 Slice 3f) — `fullscreen` still
        // gates the menu bar / timeline / side panel, but nothing could set it once
        // `handle_hotkeys` went unreachable.
        let mut h = Harness::new_ui_state(
            |ui, s: &mut State| s.viewer.handle_channel_hotkeys(ui),
            State {
                viewer: ExrViewer::default(),
            },
        );

        h.key_press(egui::Key::F11);
        h.run();
        assert!(h.state().viewer.fullscreen, "F11 enters fullscreen");

        // Esc leaves fullscreen when there's no annotation to cancel.
        h.key_press(egui::Key::Escape);
        h.run();
        assert!(!h.state().viewer.fullscreen, "Esc exits fullscreen");

        // F11 toggles back off, rather than latching.
        h.key_press(egui::Key::F11);
        h.run();
        assert!(h.state().viewer.fullscreen);
        h.key_press(egui::Key::F11);
        h.run();
        assert!(!h.state().viewer.fullscreen, "F11 toggles");

        // Esc is consumed by an in-flight annotation first: fullscreen must survive.
        h.state_mut().viewer.fullscreen = true;
        h.state_mut().viewer.anno_tool = crate::annotation::AnnotationTool::Arrow;
        h.key_press(egui::Key::Escape);
        h.run();
        assert!(
            h.state().viewer.fullscreen,
            "Esc cancelling an annotation must not also exit fullscreen"
        );

        // Still live in contact-sheet mode, unlike the channel keys.
        h.state_mut().viewer.show_contact_sheet = true;
        h.key_press(egui::Key::F11);
        h.run();
        assert!(
            !h.state().viewer.fullscreen,
            "F11 stays live with the contact sheet open"
        );
    }

    struct SmokeState {
        viewer: ExrViewer,
        a: ExrData,
    }

    /// Drive the contact sheet the way `ExrApp::draw_comp_contact_sheet` does
    /// (#99 Slice 3h): size the caches first — `draw_contact_sheet` indexes them
    /// unguarded — then lay the sheet out.
    fn sheet_harness(a: ExrData) -> Harness<'static, SmokeState> {
        Harness::new_ui_state(
            |ui, s: &mut SmokeState| {
                let SmokeState { viewer, a } = s;
                viewer.sync_texture_caches(a.logical_layers.len());
                if viewer.show_contact_sheet {
                    viewer.draw_contact_sheet(ui, a, None, None);
                }
            },
            SmokeState {
                viewer: ExrViewer::default(),
                a,
            },
        )
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

        let mut h = sheet_harness(a);

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

        let mut h = sheet_harness(a);

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
