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
/// A stale result from a superseded request is discarded; which field is the
/// supersession key depends on the request kind: a **seq-frame** by `epoch`
/// (#57) and an explicit open by `open_gen` (#109).
/// How expensive a decode is, cheapest first: proxy < beauty-only < full (#233).
///
/// A proxy carries `beauty_only` too (it is a downsampled beauty decode), so the
/// proxy test has to come first. Ordering these is what lets a **fallback** — the
/// worker returning something dearer than the job asked for — be told apart from
/// a decode that simply is what was requested.
fn fidelity_rank(proxy: bool, beauty_only: bool) -> u8 {
    if proxy {
        0
    } else if beauty_only {
        1
    } else {
        2
    }
}

struct LoadResult {
    /// Which source this decode is for (#99 unification): the A/B compare slots are
    /// `ExrApp::{A,B}_SOURCE` (`SourceId` 0/1). For an explicit open it selects
    /// B-open vs A-open (path/generation supersession); for a **seq frame**
    /// (`seq_frame`) it is the cache source (locked-step A/B, #98).
    source: crate::layer::SourceId,
    /// True for an image-sequence frame (#7): apply via `swap_image_data` to
    /// preserve the viewer session, rather than starting a fresh session.
    seq_frame: bool,
    /// Playback frame number (meaningful only when `seq_frame`); the cache key.
    frame: u32,
    /// Supersession epoch at issue time (#57).
    epoch: u64,
    /// Slot-A explicit-open generation at issue time (#109). Supersedes by a
    /// *later open*, independent of `loaded_file` — which playback churns to the
    /// current frame's path (`request_sequence_frame`), so it can't be the
    /// supersession key for the open. Unused for seq-frames (epoch-keyed) and B
    /// (its `b.loaded_file` is stable, still path-keyed).
    open_gen: u64,
    /// The worker returned a decode **dearer than this job asked for** (#233):
    /// a cheap decode's `or_else` fallback fired, most consequentially all the way
    /// to a full all-parts `load`.
    ///
    /// The worker is the only place both halves are known — the job's requested
    /// fidelity and what actually came back — so it reports rather than the UI
    /// re-deriving it from state that may have moved on since submit.
    fell_back: bool,
    result: Result<ExrData, String>,
}

/// serde default for `proxy_size` (#94): the scrub-proxy long-side pixel target
/// (~half of 1080p → ~4× more frames fit, per the OpenRV model). Scrub only —
/// playback derives its own target from the viewport (#209).
fn default_proxy_size() -> usize {
    1024
}

/// What resolution playback should decode a source at (#209).
///
/// Three states, not two, because "I can't tell" and "full resolution" want
/// opposite fallbacks and collapsing them breaks one path or the other: treating
/// unknown as full silently disables the proxy for classic (non-comp) sequences,
/// while treating full as unknown puts a 256 px scrub proxy on screen at 1:1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlaybackProxy {
    /// Decimate to this long-side pixel target.
    Px(usize),
    /// Decode full — the view needs every source pixel, so proxying would cost a
    /// pointless factor-1 "downsample" and a full-size on-disk proxy blob.
    Full,
    /// No registered source or no sane view transform yet. Falls back to the fixed
    /// `proxy_size` knob, which is what this path did before the target was derived.
    Unknown,
}

/// Floor for the viewport-derived playback proxy (#209). A pathological zoom-out
/// (or a tiny window) shouldn't drive the decode down to a thumbnail — below this
/// the saving is irrelevant anyway, and the cost is a visibly mushy frame the
/// instant the user zooms back in.
const MIN_PROXY_PX: usize = 256;

/// Hard cap on Layers-panel composite sources (#99 PR-B). Playback footprint is
/// bounded on 8 GB (see the roadmap); the panel disables Add at this many layers.
const COMP_LAYER_CAP: usize = 6;

/// First `SourceId` the Layers panel allocates for its sources (#99 Phase 2).
/// `SourceId(0)` is the base-plate slot ([`ExrApp::A_SOURCE`]) and `SourceId(1)`
/// was the old locked-step B follower (deleted in Slice 3h.2), so comp sources
/// start at 2 and never alias either in the shared T1 cache / T2 rings /
/// `followers` map once they decode as sequence followers.
const COMP_SOURCE_BASE: u64 = 2;

/// Width of the timeline panel's track gutter (#99): the fixed-width left column
/// holding a layer's visibility / solo / name / menu. Everything right of it is
/// the shared time axis, so the frame ruler and every clip bar map frames to the
/// same x.
const TIMELINE_GUTTER_W: f32 = 220.0;

/// Height of one row in the timeline panel (#99) — the ruler and each layer track.
const TIMELINE_ROW_H: f32 = 22.0;

/// Cache-fill strip colors (#172), shared by the ruler's bar and the per-track
/// strips (#99): proxy/beauty-resident frames in a dim green, full-res on top in a
/// brighter green — so a range that's only proxy-cached (it sharpens to full on
/// pause) reads differently from a fully-cached one.
const CACHE_PROXY_FILL: egui::Color32 = egui::Color32::from_rgb(52, 104, 72);
const CACHE_FULL_FILL: egui::Color32 = egui::Color32::from_rgb(64, 168, 96);

/// Frame ↔ x mapping for the timeline panel (#99). Built per row from that row's
/// axis rect (every row allocates identically, so they all agree) and shared by
/// the ruler and the layer clip bars, which is what makes a bar line up with the
/// frame numbers above it. `f64` throughout: frame numbers on long sequences
/// exceed f32-exact range, and the mapping has to stay monotonic.
#[derive(Clone, Copy, Debug)]
struct TimeAxis {
    left: f32,
    width: f32,
    lo: u32,
    hi: u32,
}

impl TimeAxis {
    fn new(rect: egui::Rect, lo: u32, hi: u32) -> Self {
        Self {
            left: rect.left(),
            width: rect.width(),
            lo,
            hi,
        }
    }

    /// Frames spanned; `0` for a single-frame sequence (which maps to the center).
    fn span(self) -> f64 {
        f64::from(self.hi.saturating_sub(self.lo))
    }

    /// x of a global frame. Frames outside `[lo, hi]` clamp to the axis edges, so
    /// a layer offset off the end of the timeline shows as a bar against the edge
    /// rather than painting outside the panel.
    fn x_of(self, frame: i64) -> f32 {
        let span = self.span();
        let t = if span == 0.0 {
            0.5
        } else {
            (((frame - i64::from(self.lo)) as f64) / span) as f32
        };
        self.left + t.clamp(0.0, 1.0) * self.width
    }

    /// x of the *left edge of a frame's slot*, for strips where each frame owns an
    /// equal-width cell over `lo..=hi` inclusive (the cache-fill bars) rather than
    /// sitting on a tick.
    fn slot_x(self, frame: i64) -> f32 {
        let nslots = self.span() + 1.0;
        let t = (((frame - i64::from(self.lo)) as f64) / nslots) as f32;
        self.left + t.clamp(0.0, 1.0) * self.width
    }

    /// The global frame under `x` (clamped to the axis).
    fn frame_at(self, x: f32) -> i64 {
        let t = f64::from((x - self.left) / self.width.max(1.0)).clamp(0.0, 1.0);
        i64::from(self.lo) + (t * self.span()).round() as i64
    }
}

/// Coalesce a sorted frame set into contiguous runs, handing each `[start, end]`
/// to `run` (#146): with a large RAM budget the ring holds thousands of frames,
/// and painting one rect per frame tessellated thousands of shapes per repaint.
/// Shared by the ruler's cache-fill bar and the per-track strips.
fn for_each_frame_run(frames: &mut [u32], mut run: impl FnMut(u32, u32)) {
    frames.sort_unstable();
    let mut i = 0;
    while i < frames.len() {
        let start = frames[i];
        let mut end = start;
        while i + 1 < frames.len() && frames[i + 1] == end + 1 {
            i += 1;
            end = frames[i];
        }
        i += 1;
        run(start, end);
    }
}

/// Allocate one full-width timeline row and split it into `(gutter, axis)`.
/// Allocating the row in a single call — rather than as two adjacent widgets —
/// is what keeps the axis x-origin identical on every row: egui inserts
/// `item_spacing` between separate allocations, which would stagger the rows.
fn alloc_timeline_row(ui: &mut egui::Ui, axis_w: f32, h: f32) -> (egui::Rect, egui::Rect) {
    let (row, _) = ui.allocate_exact_size(
        egui::vec2(TIMELINE_GUTTER_W + axis_w, h),
        egui::Sense::hover(),
    );
    let split = row.left() + TIMELINE_GUTTER_W;
    (
        egui::Rect::from_min_max(row.min, egui::pos2(split, row.bottom())),
        egui::Rect::from_min_max(egui::pos2(split, row.top()), row.max),
    )
}

/// A layer clip bar's span on the *global* timeline, from its source-space trim.
/// `source = global + offset`, so `global = source - offset`. Returns `None` when
/// the layer falls entirely outside `[lo, hi]` (it is blank across the whole
/// visible timeline).
fn track_span(trim: crate::layer::Trim, lo: u32, hi: u32) -> Option<(i64, i64)> {
    let g_lo = i64::from(trim.in_point).saturating_sub(trim.offset);
    let g_hi = i64::from(trim.out_point).saturating_sub(trim.offset);
    if g_hi < i64::from(lo) || g_lo > i64::from(hi) {
        return None;
    }
    Some((g_lo, g_hi))
}

/// The layer offset a clip drag lands on (#99): the bar is anchored to the frame
/// the pointer grabbed, so the result is a function of the pointer's *absolute*
/// position rather than an accumulation of per-event deltas (which drifts).
/// Dragging right moves the layer later on the global timeline, which *decreases*
/// the offset (`global = source - offset`).
fn offset_after_drag(start_offset: i64, grab: i64, now: i64) -> i64 {
    start_offset - (now - grab)
}

/// An in-progress clip-bar drag (#99). Holds the frame the pointer grabbed and
/// the layer's offset at grab time, so every drag event re-derives the offset
/// from scratch (see [`offset_after_drag`]).
#[derive(Clone, Copy, Debug)]
struct TrackDrag {
    id: crate::layer::LayerId,
    grab: i64,
    start_offset: i64,
}

/// A decoded Layers-panel source (#99 PR-B.2): its pixels plus the GPU texture the
/// composite ping-pong (PR-B.3) binds as a layer. One per `SourceId`, held in
/// [`ExrApp::comp_sources`]. The `texture` handle is kept purely to own the VRAM
/// for the source's lifetime — dropping this `CompSource` (on layer removal)
/// releases it (drop-only free; wgpu reclaims once no bind group is bound). GPU
/// fields are `None` on the headless / CPU-only path (no `gpu_resources`), where
/// the model layer is still valid but nothing renders.
struct CompSource {
    // Every field is written by `add_comp_source` (PR-B.2) but first *read* by the
    // composite ping-pong / row controls in PR-B.3–B.4 — hence the item-level
    // `#[allow(dead_code)]`s (the sanctioned "landed ahead of its consumer" pattern,
    // #153), not a struct-wide allow that would also mask accidental dead fields.
    /// The file this source decoded from — the one thing persistence needs to
    /// rebuild the layer next session (#99 PR-B.5). The layer's `name` is only the
    /// file *name*, so it can't reopen the file on its own.
    path: std::path::PathBuf,
    /// Full decode, kept for pixel sampling, AOV metadata, and re-uploads (an AOV
    /// switch in PR-B.4 rebuilds the texture from this without touching disk).
    #[allow(dead_code)]
    exr_data: std::sync::Arc<ExrData>,
    /// Full-res pixel dimensions of `aov`, for per-layer placement in PR-B.3.
    #[allow(dead_code)]
    size: (usize, usize),
    /// The logical layer (AOV) `bind_group` was built for. A layer whose `aov`
    /// diverges from this needs a rebuild (PR-B.4); today every source is added at
    /// AOV 0, so this is always 0.
    #[allow(dead_code)]
    aov: usize,
    /// The layer's GPU bind group (binding0 = texture view, binding1 = sampler,
    /// under `bind_group_layout_tex`) — directly bindable as `tex_a`/`tex_b` in the
    /// composite shader. `None` headless. Set together with `texture`.
    #[allow(dead_code)]
    bind_group: Option<std::sync::Arc<eframe::egui_wgpu::wgpu::BindGroup>>,
    /// Owns the texture's VRAM; not read directly (the bind group holds the view).
    #[allow(dead_code)]
    texture: Option<eframe::egui_wgpu::wgpu::Texture>,
    /// The sequence frame `texture`/`bind_group` currently hold, for a **sequence**
    /// source (#99 Phase 2). `None` for a still (one fixed texture) or before the
    /// first per-frame build. `draw_comp_central` rebuilds from the T1 cache when
    /// the resolved source frame moves off this.
    cur_frame: Option<u32>,
    /// Whether the bound texture was built from a **full** decode, as opposed to a
    /// proxy or beauty-only one (#212).
    ///
    /// Frame number alone can't answer "is what's on screen the final image?".
    /// Settling re-decodes the *same* frame at full fidelity, so without this
    /// `ensure_comp_frame` sees `cur_frame` unchanged, early-returns, and the layer
    /// keeps displaying the proxy texture forever even though the correct pixels
    /// are sitting in the cache.
    cur_full: bool,
}

/// One comp layer, flattened for persistence (#99 PR-B.5). Written at `save()` from
/// `comp_stack` + `comp_sources`, replayed by [`ExrApp::restore_comp_layers`] on the
/// next launch.
///
/// **Flat fields, not the model types.** `LayerStack`/`Layer`/`Trim` are deliberately
/// not `Serialize`: the model is pure and free to change shape, and `LayerId`/`SourceId`
/// are session-scoped (re-allocated on load, so a persisted id would be a lie). This
/// struct is the versioned boundary between the two — every field has a serde default,
/// so an `app.ron` written by an older build still loads.
///
/// **Never `#[serde(flatten)]` this into `ExrApp`** — a flattened map swallows unknown
/// keys and has repeatedly wiped `app.ron` on this project.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct LayerPersist {
    /// The source file. Re-decoded on load; a layer whose file has moved or been
    /// deleted is skipped (not an error — the rest of the stack still restores).
    path: std::path::PathBuf,
    /// The layer's display name. Usually the file name, but the user may have
    /// renamed it, so persist it rather than re-deriving.
    name: String,
    /// Which AOV / logical layer of the source is shown.
    aov: usize,
    blend: crate::viewer::BlendMode,
    opacity: f32,
    enabled: bool,
    solo: bool,
    /// `Trim`, flattened — see the struct note on why the model type isn't used.
    trim_in: u32,
    trim_out: u32,
    trim_offset: i64,
}

/// Hand-written rather than derived, because `#[serde(default)]` fills **missing**
/// fields from here: a derived `Default` would give `opacity: 0.0` / `enabled: false`,
/// so an `app.ron` from a build that predates either field would restore a stack of
/// invisible layers. These match [`crate::layer::LayerStack::push_image`]'s defaults.
impl Default for LayerPersist {
    fn default() -> Self {
        Self {
            path: std::path::PathBuf::new(),
            name: String::new(),
            aov: 0,
            blend: crate::viewer::BlendMode::default(),
            opacity: 1.0,
            enabled: true,
            solo: false,
            trim_in: 0,
            trim_out: u32::MAX,
            trim_offset: 0,
        }
    }
}

/// serde default for `proxy_cache_gb` (#165): the on-disk proxy-cache size budget
/// in GiB. ~10 GiB ≈ 2.5–5k f16 proxy frames — plenty for a shot, trivial on a
/// modern disk. A ceiling, LRU-evicted; the actual footprint is only what's cached.
fn default_proxy_cache_gb() -> f32 {
    10.0
}

/// serde default for `t2_enabled` (bool's own default is `false`).
fn ret_true() -> bool {
    true
}

/// A `ram_budget_gb` below this counts as **Auto** (no cap), both in the display
/// and in [`ExrApp::ram_budget_bytes`]. It matches the control's one-decimal
/// formatting: any value that would render as `0.0 GB` instead reads `Auto` and
/// applies no cap, so the label never disagrees with the behavior.
const RAM_BUDGET_AUTO_BELOW_GB: f32 = 0.05;

/// Job sent to the dedicated EXR worker thread via `load_tx`.
struct LoadJob {
    path: PathBuf,
    /// Which source this decode is for (#99 unification): the A/B compare slots are
    /// `ExrApp::{A,B}_SOURCE` (`SourceId` 0/1). Selects B-open vs A-open for an
    /// explicit open; the cache source for a **seq frame** (`seq_frame`, #98).
    source: crate::layer::SourceId,
    /// True when this is a playback frame: skip the first-paint proxy and apply
    /// as a session-preserving swap on arrival (#7).
    seq_frame: bool,
    /// Playback frame number (meaningful only when `seq_frame`); the cache key.
    frame: u32,
    /// Supersession epoch at issue time (#57); the result is dropped if it no
    /// longer matches `Playback::epoch` on arrival.
    epoch: u64,
    /// Slot-A explicit-open generation at issue time (#109); see [`LoadResult`].
    open_gen: u64,
    /// Decode beauty-only (a single layer) rather than all AOVs (#56, hardening
    /// step 3). Set for playback prefetch while the clock advances; cleared for
    /// explicit opens and the full re-decode on settle. See [`ExrData::load_beauty`].
    beauty_only: bool,
    /// Decode a **downsampled scrub proxy** at this `max_dim` (long-side px) size
    /// (#94) instead of a full/beauty frame — for cheap playback of heavy footage.
    /// `None` = no proxy. Takes precedence over `beauty_only`, which is kept as the
    /// fallback if the decode errors. See [`ExrData::load_proxy`] (a normal decode
    /// then a post-decode box-filter).
    proxy_target: Option<usize>,
    /// Which single AOV a cheap decode must carry, as `(physical part index,
    /// logical layer index)` (#217) — the generalisation that lets a non-beauty
    /// pass play. `None` means logical layer 0, decoded by the `first_valid_layer`
    /// path (`load_beauty` / `load_proxy`) exactly as before.
    ///
    /// Only read when `beauty_only` or `proxy_target` is set; a full decode carries
    /// every AOV and has nothing to select. Set by [`ExrApp::cheap_decode_layer`],
    /// which guarantees the named part holds exactly one logical layer.
    aov_layer: Option<(usize, usize)>,
}

/// Message from the decode worker to the UI thread, delivered over `load_rx`. A
/// slot-A load first sends a `Proxy` (a fast low-res first paint, #33) when one
/// is available, then always sends `Loaded` with the full decode.
enum LoadMsg {
    /// Boxed: `LoadResult` holds a full `ExrData` inline.
    Loaded(Box<LoadResult>),
}

/// Result of an off-thread `.cube` LUT parse. The GPU bind group is created
/// on the UI thread in [`ExrApp::apply_lut_load_result`] (wgpu device access);
/// only the file I/O + parsing runs off-thread.
struct LutLoadResult {
    path: String,
    result: Result<crate::color::cube::CubeLut, String>,
}

/// A completed off-thread render-watch scan (#145). The `read_dir` + per-frame
/// `stat` (a multi-hundred-ms blocking call on a network share) runs on a worker
/// thread; the UI thread diffs the signatures against its baseline and applies
/// via [`ExrApp::apply_scan`], which mutates playback state and so must stay
/// on-thread.
/// The worker sends `None` when the scan produced nothing — `scan_group` failed
/// (the directory was briefly unreachable on a share) or the scan panicked: the
/// UI clears the in-flight flag and keeps its baseline, so a transient hiccup
/// never drops the cache or reports spurious removals.
struct ScanResult {
    /// Path of the lowest-numbered frame the scan was launched from. The UI
    /// discards a result whose anchor no longer matches the live sequence's first
    /// frame — a scan that finished after the user opened a different sequence.
    anchor: PathBuf,
    group: std::collections::BTreeMap<u32, PathBuf>,
    sigs: Vec<(u32, crate::sequence::FrameSig)>,
}

/// Edge length of the baked OCIO 3D LUT (#24). 65³ keeps saturated-highlight error well under
/// 0.02 vs the analytic ACES transform (33³ measured ~0.04 there); ~4.4 MB as RGBA f32, a
/// trivial VRAM cost for a viewer.
const OCIO_BAKE_LUT_SIZE: u32 = 65;

/// One image source's decode + playback state (#99 unification). One of these
/// lives in `ExrApp::followers` per non-primary `SourceId`; today the only entry
/// is the single locked-step **B** follower (#98), and Phase 2 adds the comp
/// stack's N sources. The master clock (`ExrApp::playback`) drives the primary
/// source; a follower's `current_frame` is a slaved function of the global
/// playhead, and it decodes into the T1 cache under its own `SourceId`. All fields
/// default to the "no follower" state so a lone-image B (or no B) behaves exactly
/// as the old defaulted `*_b`.
#[derive(Default)]
struct SourceState {
    /// The follower's detected sequence (`None` → a lone image that holds).
    sequence: Option<crate::sequence::Sequence>,
    /// The follower's current frame (position-aligned to the global playhead).
    current_frame: u32,
    /// The follower frame the transport is awaiting (`None` when resident).
    pending: Option<u32>,
    /// Follower seq frames submitted-but-not-returned — a separate set from the
    /// primary's `inflight` so the primary's heavily-tested path is untouched.
    inflight: std::collections::HashSet<u32>,
    /// Whether an explicit follower open is decoding.
    loading: bool,
}

/// One `sources` row in the playback debug overlay (#100).
///
/// `displayed` is the source's `CompSource::cur_frame` — the frame whose pixels are
/// actually on screen. Every other field is playhead-derived, so it is the only one
/// that can show the picture failing to keep up with the clock (#204).
/// Field order is the sort order: by source id.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DbgFollowerRow {
    id: u64,
    playhead: u32,
    displayed: Option<u32>,
    pending: Option<u32>,
    inflight: usize,
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
    /// Monotonic slot-A explicit-open generation (#109). Bumped on every slot-A
    /// `open_file` and on unload; a load result whose `open_gen` no longer matches
    /// was superseded by a later open. Decouples open-supersession from
    /// `loaded_file` (which playback rewrites to the current frame's path).
    #[serde(skip)]
    open_gen_a: u64,
    // `Arc` so a decoded frame can be the active image (tier T3) and stay
    // resident in the playback ring cache (tier T1) at once, without cloning the
    // (often 600 MB+) pixel buffers. See docs/playback/memory-contract.md.
    #[serde(skip)]
    exr_data: Option<std::sync::Arc<ExrData>>,
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
    /// Read-ahead file warmer (#164): pulls the next wanted frames' files
    /// through the page cache while the current one decodes, so the decode
    /// worker's read never waits on storage.
    #[serde(skip)]
    prefetcher: crate::prefetch::Prefetcher,
    /// Resident-frame budget for `frame_cache`, recomputed from the RAM budget
    /// (`budget::frames_in` of the byte budget) each status tick once a frame's
    /// size is measured.
    #[serde(skip)]
    frame_cache_cap: usize,
    /// The T1 **byte** ceiling the ring is evicted to (#232), recomputed each
    /// status tick from `budget::t1_budget_bytes`.
    ///
    /// This is the authoritative bound; `frame_cache_cap` is `frames_in` of it,
    /// carried because the decode scheduler needs a count. Eviction was
    /// count-only until #232, which is exact only while every resident frame is
    /// the same size — and #230 established that they are not.
    #[serde(skip)]
    frame_cache_budget: u64,
    /// One **full** frame's measured `approx_bytes()`, captured on the first full
    /// decode (a sequence is homogeneous at a given fidelity). Sizes the cache
    /// budget when full frames are what the pump is issuing.
    #[serde(skip)]
    frame_bytes: Option<usize>,
    /// One **proxy** frame's measured `approx_bytes()` (#94), captured on the first
    /// proxy decode. Sizes the T1 cap while proxying so hundreds of the tiny
    /// frames fit instead of ~16 full ones.
    #[serde(skip)]
    proxy_bytes: Option<usize>,
    /// One **beauty-only** frame's measured `approx_bytes()` (#230) — the third
    /// fidelity, and the one that had no latch at all.
    ///
    /// Without it, `beauty_preview` playback (proxy off) cached beauty-only frames
    /// while sizing the ring off a full 23-part decode: measured `t1=23/23` and
    /// `evict=726` in 45 s on a 1035 MB/frame Karma render against a 24 GB budget,
    /// a ~10x under-use of the configured budget on exactly the footage where cache
    /// depth matters most. Read through [`Self::sizing_frame_bytes`].
    #[serde(skip)]
    beauty_bytes: Option<usize>,
    /// Sequence frame numbers submitted to the worker but not yet returned (#57).
    /// Bounds decode-ahead concurrency and prevents re-requesting an in-flight
    /// frame; cleared on every seek so superseded decodes can't be miscounted.
    #[serde(skip)]
    inflight: std::collections::HashSet<u32>,

    /// Per-`SourceId` follower decode+playback state (#99 unification). Each
    /// non-primary source is a *slaved* function of the master playhead. Holds the
    /// comp stack's sequence sources so their playheads,
    /// in-flight sets, and per-source budget shares are tracked uniformly. The
    /// primary (Slot A) still lives in the top-level `inflight` / `loading_a` and
    /// on `playback`. `BTreeMap` gives a deterministic pump/eviction order by id.
    #[serde(skip)]
    followers: std::collections::BTreeMap<crate::layer::SourceId, SourceState>,
    /// Latest playback epoch, shared with the decode worker so it can skip a
    /// sequence job a newer seek/scrub already superseded **before** paying the
    /// decode. Rapid scrubbing otherwise floods the worker's FIFO channel with
    /// soon-stale jobs; draining them one full decode at a time (each then dropped
    /// by [`Self::apply_load_result`]'s epoch check) strands the awaited frame
    /// behind the backlog for seconds — the scrub freeze.
    #[serde(skip)]
    epoch_signal: std::sync::Arc<std::sync::atomic::AtomicU64>,

    /// Playback debug overlay (#100): a toggleable window showing live cache /
    /// budget / pacing state, so real-footage soak testing is observable rather
    /// than guesswork. Transient, off by default.
    #[serde(skip)]
    show_playback_debug: bool,
    /// User-facing playback HUD (#172): a compact viewport overlay with achieved
    /// vs target fps, the dropped/held-frame count for the current play run, and
    /// T1 cache occupancy — so the cache/pacing machinery is legible during
    /// review. Persisted (a review preference), off by default.
    show_playback_hud: bool,
    /// Last `ResourceMonitor` sample, stashed each status tick for the overlay
    /// (the budget inputs: sys + VRAM used/total).
    #[serde(skip)]
    dbg_last_sample: Option<crate::resource_monitor::Sample>,
    /// Cumulative T1 evictions and dropped stale-epoch results, for the overlay.
    #[serde(skip)]
    dbg_evictions: u64,
    #[serde(skip)]
    dbg_dropped_epoch: u64,
    /// Cumulative cheap decodes the worker had to satisfy with something dearer
    /// (#233) — see `LoadResult::fell_back`.
    ///
    /// A silent mode until now: nothing distinguished "this footage plays cheap"
    /// from "every cheap decode is failing and we are quietly decoding full",
    /// because the fallback is *correct* behaviour (slow beats stuck, #213) and
    /// left no trace. Counting it is most of the fix.
    #[serde(skip)]
    dbg_fallbacks: u64,
    /// Whether the clock source's most recent sequence decode fell back. Makes
    /// [`Self::sizing_frame_bytes`] size off full frames while a cheap path is
    /// failing, instead of off the fidelity that was merely *asked* for.
    #[serde(skip)]
    decode_fell_back: bool,
    /// Frames the pacer skipped (DropFrames) or held-late (Stutter) during the
    /// current play run (#172), for the HUD + debug overlay. Reset when play
    /// (re)starts. `dbg_dropped_epoch` above is unrelated (stale-decode discards).
    #[serde(skip)]
    run_dropped: u32,
    #[serde(skip)]
    run_held: u32,

    /// Wall-clock instant the most recent **sequence** decode job was submitted
    /// to the worker. Anchors the decode stall watchdog ([`Self::tick_decode_watchdog`]):
    /// with playback work outstanding and no result past an adaptive timeout,
    /// playback is force-recovered instead of freezing until the file is reopened.
    #[serde(skip)]
    decode_submit_at: Option<std::time::Instant>,
    /// Turnaround of the last completed sequence decode. Scales the stall
    /// watchdog's timeout so a genuinely slow big-frame decode never trips it.
    #[serde(skip)]
    last_decode_dur: Option<std::time::Duration>,
    /// Throttle for the once-per-second playback state trace ([`Self::trace_playback_state`]).
    #[serde(skip)]
    dbg_last_trace: Option<std::time::Instant>,
    /// Round-robin cursor for [`Self::pump_decode`]'s source order (#204), so the
    /// single decode slot is shared rather than always going to whichever source
    /// sorts first and always wants something.
    #[serde(skip)]
    pump_rotation: usize,
    /// Whether the last trace tick saw the clock running — the edge detector that
    /// lets [`Self::trace_playback_state`] emit one line on play→pause/stop (#100).
    /// That transition is exactly when INV-SAMPLE is decided, and the trace's
    /// "something is outstanding" gate would otherwise swallow it.
    #[serde(skip)]
    dbg_was_playing: bool,

    recent_files: Vec<PathBuf>,
    theme: ThemeChoice,

    /// Persistence bridge for the viewer's display prefs (#151): diff controls,
    /// custom gradients, background + presets. The runtime single owner is
    /// `self.viewer.prefs` — this mirror is written from it in `save()` and read
    /// back into it in `new()`, i.e. only at persist boundaries, never per frame.
    ///
    /// Nested (not `#[serde(flatten)]`): eframe persists via RON, and flatten
    /// forces the whole app struct to serialize as a map, which fails to parse
    /// against an existing struct-syntax `app.ron` and would wipe *all* settings.
    /// The cost of nesting is a one-time reset of just these six display prefs on
    /// upgrade (the old top-level keys are ignored; every other setting survives);
    /// schema-versioned migration is tracked separately in #65.
    #[serde(default)]
    persisted_prefs: crate::viewer::ViewerPrefs,

    /// The comp stack, flattened for persistence (#99 PR-B.5). Mirrored from
    /// `comp_stack` + `comp_sources` in [`eframe::App::save`] — the same persist-time
    /// bridge `persisted_prefs` uses — and replayed by [`Self::restore_comp_layers`]
    /// on the next launch. Nested, **never `#[serde(flatten)]`** (see the note above).
    #[serde(default)]
    persisted_layers: Vec<LayerPersist>,

    /// The comp-texture upload pool (#202) — `None` headless. Owns the worker
    /// threads that interleave and upload frames off the paint thread.
    #[serde(skip)]
    tex_uploader: Option<crate::tex_upload::TexUploader>,

    /// Per-source proxy target held for the duration of a play run (#209), so the
    /// T1 ring can't end up holding frames decoded at several different zoom
    /// levels. Empty outside a run, and re-latched by [`Self::relatch_proxy_targets`]
    /// each time playback starts. Runtime-only — a view transform doesn't outlive
    /// the session.
    #[serde(skip)]
    proxy_target_latch: std::collections::HashMap<crate::layer::SourceId, PlaybackProxy>,

    /// First frame of an env-gated soak run — the clock the warm-up and run window
    /// in [`Self::tick_soak_harness`] are measured from. `None` outside a soak.
    #[serde(skip)]
    soak_started_at: Option<std::time::Instant>,
    /// Whether the soak harness has already pressed Play (it must fire once, not
    /// every frame past the warm-up).
    #[serde(skip)]
    soak_play_sent: bool,

    /// Whether the soak harness has pressed Pause, entering its settle-hold phase.
    #[serde(skip)]
    soak_paused: bool,

    /// Snapshot to clipboard (issue #19): when true, each snapshot also writes a
    /// timestamped PNG to `~/.floki/snapshots/`. The clipboard copy always happens.
    #[serde(default)]
    save_snapshots: bool,

    /// Pre-upload sequence frames to GPU textures ahead of the playhead (#56, the
    /// T2 ring) for smoother playback. On by default; a kill-switch back to the
    /// lazy per-swap path if it misbehaves on a given GPU. Persisted.
    #[serde(default = "ret_true")]
    t2_enabled: bool,

    /// Decode only the beauty/first layer for the playback ring while the clock
    /// is advancing (#56, hardening step 3): cheaper decode + smaller resident
    /// frames for multi-part AOV EXRs, so playback feeds the decode wall faster.
    /// On settle the playhead is re-decoded in full so the readout + AOV switch
    /// see every channel (INV-SAMPLE, #7). On by default; a kill-switch back to
    /// always-full decode if a file's first layer isn't its beauty. Persisted.
    #[serde(default = "ret_true")]
    beauty_preview: bool,

    /// Scrub proxies (#94): while the playhead moves (play / scrub / precache),
    /// decode a tiny **downsampled** proxy instead of a full/beauty frame, so
    /// heavy footage plays fast and hundreds of frames fit RAM (vs ~16 full); the
    /// settled playhead is always re-decoded full-res. On by default (self-gates
    /// to a full decode on small/tiled/deep files); a transport kill-switch.
    /// Persisted.
    #[serde(default = "ret_true")]
    proxy_enabled: bool,
    /// Scrub-proxy size — the downsampled **long-side pixel** cap passed to
    /// [`crate::exr_loader::ExrData::load_proxy`] (`max_dim`): higher = sharper but
    /// larger + fewer frames cached. Persisted.
    #[serde(default = "default_proxy_size")]
    proxy_size: usize,

    /// Persist the downsampled scrub proxies to disk (#165) so the first-touch
    /// decode of a frame is paid **once, ever** — a repeat pass / later session
    /// loads proxies from `~/.floki/proxy-cache` instead of re-decoding. Keyed by
    /// source path+mtime+size+proxy-px (a re-render auto-invalidates); LRU-evicted
    /// to [`Self::proxy_cache_gb`]. On by default (bounded + self-invalidating);
    /// a kill-switch. Persisted. The runtime cache lives in [`Self::proxy_cache`].
    #[serde(default = "ret_true")]
    proxy_disk_cache: bool,
    /// On-disk proxy-cache size budget in **gibibytes** (1024³, matching the RAM
    /// budget's unit). LRU ceiling, not a reservation. Persisted.
    #[serde(default = "default_proxy_cache_gb")]
    proxy_cache_gb: f32,
    /// Runtime handle to the persistent proxy cache (#165). Shared (`Arc`) with
    /// the decode worker, which reads-through it (hit → skip decode) and
    /// write-throughs misses. Not persisted — rebuilt each session from
    /// `proxy_disk_cache` / `proxy_cache_gb` in [`Self::new`]; the inert default
    /// does zero I/O until `configure`d, so headless tests pay nothing.
    #[serde(skip)]
    proxy_cache: std::sync::Arc<crate::proxy_cache::ProxyCache>,

    /// Eager precache (#56, hardening step 4): fill the whole in/out range into
    /// the T1 ring up front — not just the decode-ahead window — so the cached
    /// span plays *and loops* with the decoder idle (PDplayer / OpenRV-style).
    /// Bounded by the live RAM budget: it fills to `frame_cache_cap` and the
    /// cache-fill bar under the scrubber shows the resident span; it never
    /// pretends to hold what won't fit. On by default (#165): scrub proxies keep
    /// the per-frame footprint small and the disk cache makes a repeat fill cheap,
    /// so eagerly warming the range is affordable now. An explicit transport
    /// toggle. Persisted (existing saved state is respected; only fresh installs
    /// get the new default).
    #[serde(default = "ret_true")]
    precache: bool,

    /// User-assigned cap on the RAM the T1 ring may use. The unit is **gibibytes**
    /// (1024³ bytes) — labeled `GB` in the UI to match floki's other memory
    /// readouts (`resource_monitor::fmt_bytes` is also 1024-based). Below
    /// [`RAM_BUDGET_AUTO_BELOW_GB`] means "auto" — size purely from the live
    /// free-RAM budget (#56). A larger value is a *ceiling* applied on top of the
    /// auto figure (`budget::apply_user_ram_cap`), never an override, so it can
    /// only shrink the ring, not push it past free RAM. Handy for bounding RAM on
    /// a shared box and for dogfooding the eviction paths on a unified-memory Mac.
    /// Persisted.
    #[serde(default)]
    ram_budget_gb: f32,

    /// Runtime latch: set once eager precache (#56, step 4) has filled the in/out
    /// range as far as the RAM budget allows, so `tick_precache` stops re-pumping.
    /// Without it, a range larger than the budget churns — the live cap wobbles by
    /// a frame at the edge and the evicted frame is re-requested every tick
    /// (decode→evict→repeat). Reset whenever the playhead moves or the range
    /// changes (`invalidate_inflight`, `advance_playhead`, enabling precache), so
    /// the new window refills. Never persisted.
    #[serde(skip)]
    precache_filled: bool,
    /// The playhead the T2 ring was last pumped for (#142 U4). A full ring plus
    /// an unchanged playhead means every slot is already built for this frame, so
    /// the pump skips its want-list allocation — the common paused / settled case.
    #[serde(skip)]
    last_t2_pump: Option<u32>,
    /// A timeline drag is in progress (#143). While held, seeks decode
    /// beauty-only like playback does — the readout is suppressed or showing
    /// the beauty layer anyway — and the release settles the landing frame to
    /// a full all-AOV decode via `settle_to_full` (INV-SAMPLE, #7).
    #[serde(skip)]
    scrub_active: bool,

    /// Render-watch (#101): poll the sequence directory and pick up frames as a
    /// render writes them — new frames extend the range, re-rendered frames drop
    /// from cache and re-decode. Off by default (it costs a periodic `read_dir`,
    /// wasteful on a static dir or a slow network share); the user turns it on for
    /// a live render. Persisted.
    #[serde(default)]
    watch_enabled: bool,
    /// When watching, park the playhead on the newest frame as it arrives (a
    /// live "follow the render" view). Persisted.
    #[serde(default)]
    watch_follow: bool,
    /// Last directory scan `(number, signature)`, the baseline the next poll
    /// diffs against. Empty until the first poll (re)baselines. Runtime-only.
    #[serde(skip)]
    watch_sigs: Vec<(u32, crate::sequence::FrameSig)>,
    /// When the render-watch last polled, to throttle the `read_dir` cadence.
    #[serde(skip)]
    last_watch_poll: Option<std::time::Instant>,
    /// Result channel for the off-thread render-watch scan (#145). The scan
    /// (`read_dir` + a `stat` per frame) blocks — hundreds of ms on a network
    /// share — so it runs on a spawned thread and delivers its result here for
    /// the UI thread to diff + apply. Persistent (mirrors `snapshot_tx`).
    #[serde(skip)]
    scan_tx: Option<std::sync::mpsc::Sender<Option<ScanResult>>>,
    #[serde(skip)]
    scan_rx: Option<std::sync::mpsc::Receiver<Option<ScanResult>>>,
    /// A watch scan is on a worker thread; suppresses launching another until it
    /// lands, so a slow share can't stack scans (#145).
    #[serde(skip)]
    scan_in_flight: bool,

    /// A framebuffer screenshot has been requested and we're awaiting its
    /// `Event::Screenshot` reply (transient).
    #[serde(skip)]
    snapshot_pending: bool,
    /// Last snapshot outcome, shown briefly in the status bar (transient).
    #[serde(skip)]
    snapshot_status: Option<String>,
    /// Receives the finalize outcome from the snapshot worker threads (#146):
    /// clipboard copy + PNG encode/write run off the UI thread. The channel is
    /// persistent (workers clone `snapshot_tx`), so overlapping snapshots each
    /// deliver their status in order.
    #[serde(skip)]
    snapshot_tx: Option<std::sync::mpsc::Sender<String>>,
    #[serde(skip)]
    snapshot_rx: Option<std::sync::mpsc::Receiver<String>>,

    /// Throttled RAM/GPU-memory sampler for the bottom-bar readout (#51).
    #[serde(skip)]
    resource_monitor: crate::resource_monitor::ResourceMonitor,

    #[serde(skip)]
    show_help: bool,
    #[serde(skip)]
    show_settings: bool,
    /// Whether the **Layers** panel is shown (#99). On by default now that the
    /// layer stack is becoming the primary model (the A/B compare is on its way
    /// out); still menu-toggleable.
    #[serde(skip)]
    show_layers_panel: bool,
    /// Whether the left **Info** side panel (EXR Info / Color Sampler / Histogram)
    /// is shown. On by default; menu-toggleable via View ▸ Info panel. Transient
    /// like `show_layers_panel` — resets to shown each session.
    #[serde(skip)]
    show_side_panel: bool,
    /// An in-progress timeline clip-bar drag (#99), or `None`. Purely transient
    /// interaction state — the edit it produces lands in the layer's `Trim.offset`.
    #[serde(skip)]
    track_drag: Option<TrackDrag>,
    /// The comp source driving the global transport (#99 R4-lite), or `None` when
    /// slot A owns the clock. Set when the first added comp *sequence* establishes
    /// the playhead (there's no slot-A open), so the timeline + playback keys light
    /// up. While set, the slot-A decode path is bypassed (`request_sequence_frame` /
    /// `next_want` / the budget split) so the clock-driving sequence isn't also
    /// decoded as the primary. A real slot-A open reclaims the transport (clears it).
    ///
    /// This holds the **id**, not just a flag, because every transport-level gate
    /// and instrument in this file was written against `A_SOURCE` and goes dead the
    /// moment the primary stops decoding (#100): pacing (`note_shown`), the T1
    /// sizing seed (`frame_bytes`), and the stutter hold gate all need to know
    /// *which* follower is the clock. `Self::comp_drives_transport()` keeps the
    /// original boolean reading for the sites that only care that it isn't A.
    #[serde(skip)]
    transport_source: Option<crate::layer::SourceId>,
    /// The Layers panel's composite stack: a viewer-independent N-layer stack the
    /// panel edits and (PR-B.3) composites via the PR-A accumulate ping-pong,
    /// separate from the A/B compare stack so the compare modes stay untouched.
    /// Runtime-only; its layers persist via a `LayerPersist` list (PR-B.5), not
    /// this field (`LayerStack` isn't `Serialize`, and ids are re-allocated on load).
    #[serde(skip)]
    comp_stack: crate::layer::LayerStack,
    /// Monotonic allocator for the panel's `SourceId`s (#99 PR-B). Starts at
    /// [`COMP_SOURCE_BASE`] (2) so comp sources never alias the A/B slots (0/1);
    /// never reused, so a removed-then-added source can't alias a stale
    /// decode/texture (PR-B.2).
    #[serde(skip)]
    comp_next_source: u64,
    /// Decoded pixels + GPU texture for each Layers-panel source, keyed by the
    /// `SourceId` its layer references (#99 PR-B.2). Populated by
    /// [`Self::add_comp_source`] (a synchronous decode-on-add — the panel is a
    /// paused / cap-6 workflow, so it reuses [`ExrData::load`] rather than the
    /// A/B decode worker) and consumed by the composite ping-pong (PR-B.3).
    /// Runtime-only: GPU handles aren't serializable, so PR-B.5 re-decodes each
    /// source from its persisted path on load. Sources are dropped (freeing VRAM)
    /// when the last layer referencing them is removed — see
    /// [`Self::remove_comp_layer`].
    #[serde(skip)]
    comp_sources: std::collections::HashMap<crate::layer::SourceId, CompSource>,

    /// The topmost drawable comp layer under the cursor — `(source, aov)` — for the
    /// status-bar pixel readout (#99 R4). Set each frame by [`Self::draw_comp_central`]
    /// and read the next frame by [`Self::draw_status_bar`] (which runs first — the same
    /// one-frame lag the A/B readout has). Runtime-only.
    #[serde(skip)]
    comp_readout: Option<(crate::layer::SourceId, usize)>,

    /// Which layer fills the compare pane (side B) in a non-`Stacked` arrangement
    /// (#99 Slice 2a) — Nuke's second viewer input, chosen explicitly via the `vs:`
    /// picker rather than tied to the "current" layer. `None` = use the default from
    /// [`default_compare_b`] (the topmost layer that isn't current), so the two panes
    /// differ out of the box instead of side B duplicating the top of the composite.
    /// Runtime-only.
    #[serde(skip)]
    compare_b_layer: Option<crate::layer::LayerId>,

    /// The compare layer's `(source, aov)` — side B of a Side-by-Side (#99 Slice 2a).
    /// When the cursor is over that pane, [`Self::sample_comp_readout`] promotes this
    /// into `comp_readout` so the status-bar row names and samples the pane actually
    /// under the cursor. `None` outside Side-by-Side. Runtime-only.
    #[serde(skip)]
    comp_readout_b: Option<(crate::layer::SourceId, usize)>,

    /// The comp layer the viewer's AOV / channel controls + EXR Info act on —
    /// Nuke's "current" layer (#99 R4 follow-up). Set to the newest layer on
    /// open/add and by clicking a layer in the viewport bar or a timeline track;
    /// resolved (with a top-of-stack fallback for a stale/empty selection) by
    /// [`Self::active_comp_layer`]. Runtime-only.
    #[serde(skip)]
    selected_comp_layer: Option<crate::layer::LayerId>,

    /// Viewport arrangement for the comp path (#99 render-retire, Slice 2): how the
    /// composite is presented against the **current layer** (`active_comp_layer`).
    /// `Stacked` = the plain composite; `SideBySide`/`Wipe`/`Diff` compare the
    /// composite (side A) against the current layer (side B). The comp-path analogue
    /// of the A/B `compare_mode`. Runtime-only.
    #[serde(skip)]
    comp_arrangement: crate::layer::Arrangement,

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

    #[serde(default)]
    ocio_display: String,
    #[serde(default)]
    ocio_view: String,
    #[serde(default)]
    ocio_input_cs: String,
    #[serde(default)]
    pub ocio_enabled: bool,
    /// Bake the OCIO display transform to a 3D LUT (#24): trades a tiny amount of accuracy for
    /// a cheap per-pixel texture lookup instead of the analytic ACES ALU — smoother pan/zoom
    /// on weak GPUs. Off by default (analytic is the reference).
    #[serde(default)]
    pub ocio_bake_lut: bool,
    #[serde(skip)]
    ocio_config: Option<floki_ocio::OcioConfig>,
    #[serde(skip)]
    ocio_displays: Vec<floki_ocio::Display>,
    #[serde(skip)]
    ocio_colorspaces: Vec<String>,
    #[serde(skip)]
    ocio_error: Option<String>,
    #[serde(skip)]
    ocio_ready: bool,
    /// Monotonic generation of the OCIO display transform, bumped on every
    /// `rebuild_ocio_pass` (config / display / view change). Pushed into the
    /// viewer each frame so it can invalidate contact-sheet thumbnails and force
    /// OCIO re-renders when the managed look changes (#59 replaced the old
    /// `ocio_cpu` Rc-pointer identity used for this).
    #[serde(skip)]
    ocio_render_gen: u64,

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
    /// Job queue sender: send `LoadJob { path, source, .. }` to the single worker thread.
    #[serde(skip)]
    load_tx: Option<std::sync::mpsc::Sender<LoadJob>>,
    /// Result receiver: the worker sends completed `LoadResult`s back here.
    #[serde(skip)]
    load_rx: Option<std::sync::mpsc::Receiver<LoadMsg>>,
    /// Cloned into the decode worker so it can wake the UI the moment a result
    /// lands (#137) instead of the result waiting out the 50 ms in-flight poll.
    /// `None` only in tests, which drain the channel directly.
    #[serde(skip)]
    repaint_ctx: Option<egui::Context>,

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
            open_gen_a: 0,
            exr_data: None,
            error_msg: None,
            viewer: ExrViewer::default(),
            playback: crate::playback::Playback::default(),
            frame_cache: crate::cache::FrameCache::new(),
            prefetcher: crate::prefetch::Prefetcher::default(),
            // Conservative starting budget until the first frame is measured and
            // `budget::t1_budget_bytes` recomputes it from a slice of free RAM.
            frame_cache_cap: 8,
            // Unbounded until a decode measures a frame and `tick_budgets` sizes
            // it — matching `frame_cache_cap`'s placeholder, which is likewise not
            // a real budget until then.
            frame_cache_budget: u64::MAX,
            frame_bytes: None,
            proxy_bytes: None,
            beauty_bytes: None,
            inflight: std::collections::HashSet::new(),
            followers: std::collections::BTreeMap::new(),
            epoch_signal: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            show_playback_debug: false,
            show_playback_hud: false,
            dbg_last_sample: None,
            dbg_evictions: 0,
            dbg_fallbacks: 0,
            decode_fell_back: false,
            dbg_dropped_epoch: 0,
            run_dropped: 0,
            run_held: 0,
            decode_submit_at: None,
            last_decode_dur: None,
            dbg_last_trace: None,
            pump_rotation: 0,
            dbg_was_playing: false,
            recent_files: Vec::new(),
            theme: ThemeChoice::default(),
            persisted_prefs: crate::viewer::ViewerPrefs::default(),
            persisted_layers: Vec::new(),
            save_snapshots: false,
            t2_enabled: true,
            beauty_preview: true,
            proxy_enabled: true,
            proxy_size: default_proxy_size(),
            proxy_disk_cache: true,
            proxy_cache_gb: default_proxy_cache_gb(),
            // Inert until `new` calls `configure`: zero I/O, no writer thread —
            // so the `Default` app (every headless test) pays nothing.
            proxy_cache: std::sync::Arc::new(crate::proxy_cache::ProxyCache::disabled()),
            precache: true,
            ram_budget_gb: 0.0,
            precache_filled: false,
            last_t2_pump: None,
            scrub_active: false,
            watch_enabled: false,
            watch_follow: false,
            watch_sigs: Vec::new(),
            last_watch_poll: None,
            scan_tx: None,
            scan_rx: None,
            scan_in_flight: false,
            snapshot_pending: false,
            snapshot_status: None,
            snapshot_tx: None,
            snapshot_rx: None,
            resource_monitor: crate::resource_monitor::ResourceMonitor::default(),
            show_help: false,
            show_settings: false,
            show_layers_panel: true,
            show_side_panel: true,
            track_drag: None,
            transport_source: None,
            comp_stack: crate::layer::LayerStack::new(),
            proxy_target_latch: std::collections::HashMap::new(),
            tex_uploader: None,
            soak_started_at: None,
            soak_play_sent: false,
            soak_paused: false,
            comp_next_source: COMP_SOURCE_BASE,
            comp_sources: std::collections::HashMap::new(),
            comp_readout: None,
            comp_readout_b: None,
            compare_b_layer: None,
            selected_comp_layer: None,
            comp_arrangement: crate::layer::Arrangement::Stacked,
            gpu_resources: None,
            ocio_path: String::new(),
            lut_path: String::new(),
            enable_lut: false,
            lut_bg: None,
            lut_texture: None,
            lut_error: None,
            lut_domain_min: [0.0, 0.0, 0.0, 0.0],
            lut_domain_max: [1.0, 1.0, 1.0, 0.0],
            ocio_display: String::new(),
            ocio_view: String::new(),
            ocio_input_cs: String::new(),
            ocio_enabled: false,
            ocio_bake_lut: false,
            ocio_config: None,
            ocio_displays: Vec::new(),
            ocio_colorspaces: Vec::new(),
            ocio_error: None,
            ocio_ready: false,
            ocio_render_gen: 0,
            show_tools_window: false,
            tools_input_dir: String::new(),
            tools_output_dir: String::new(),
            conversion_progress: None,
            conversion_status: String::new(),
            conversion_receiver: None,
            conversion_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            loading_a: false,
            load_tx: None,
            load_rx: None,
            repaint_ctx: None,
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

        // Move the persisted viewer prefs into their runtime owner, the viewer
        // (#151). From here on `self.viewer.prefs` is the single source of truth;
        // `persisted_prefs` stays dormant until `save()` re-mirrors it.
        app.viewer.prefs = std::mem::take(&mut app.persisted_prefs);

        // Rebuild the comp stack from the persisted layer list (#99 PR-B.5). Deferred
        // until after the prefs move (so a restored layer's decode sees the right
        // settings) but before the GPU/OCIO wiring below — `add_comp_source` decodes
        // synchronously and only builds textures when `gpu_resources` is set, which
        // `draw_comp_central`'s `ensure_comp_*` then does on the first frame anyway.
        app.restore_comp_layers();

        // Wire the repaint handle before anything can spawn the decode worker,
        // so the worker can wake the UI when a result lands (#137).
        app.repaint_ctx = Some(cc.egui_ctx.clone());

        app.gpu_resources = cc
            .wgpu_render_state
            .clone()
            .map(crate::gpu::GpuResources::new);

        // The comp-texture upload pool (#202). Spawned with the GPU and kept for the
        // process lifetime; absent a GPU there is nothing to upload to, and every
        // texture path is already a no-op.
        app.tex_uploader = app
            .gpu_resources
            .as_ref()
            .map(|g| crate::tex_upload::TexUploader::new(&g.tex_build_ctx(), cc.egui_ctx.clone()));

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
        if app.ocio_enabled {
            app.reload_ocio();
            if !app.ocio_ready {
                app.ocio_enabled = false;
            }
        }

        // Activate the persistent proxy cache (#165) from the restored settings.
        // Off the UI thread: the first enable spawns the writer, which does the
        // one-time directory scan + eviction.
        app.proxy_cache.configure(
            app.proxy_disk_cache,
            crate::proxy_cache::gib_to_bytes(app.proxy_cache_gb),
        );

        app
    }

    /// Load the OCIO config (from `ocio_path`, or built-in `ocio://default` if empty),
    /// enumerate its color spaces/displays/views, pick sensible defaults, and build the GPU
    /// pass. Errors land in `ocio_error` and clear `ocio_ready`.
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
    fn rebuild_ocio_pass(&mut self) {
        use floki_ocio::DisplayTransformRequest;

        // Bump the OCIO generation so the viewer re-renders thumbnails and the
        // OCIO callback on any config / display / view change (#59). `saturating`,
        // not `wrapping`: the viewer treats 0 as the OCIO-off sentinel and relies
        // on the generation being non-zero (>= 1) whenever OCIO is active, so this
        // must never wrap back to 0.
        self.ocio_render_gen = self.ocio_render_gen.saturating_add(1);
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

                // Second pass built for the `Rgba8Unorm` contact-sheet thumbnail
                // target (#67 Phase 2): same bundle, different output format. A
                // failure here is non-fatal — the viewport stays OCIO-managed and
                // the contact sheet falls back to the CPU thumbnail path.
                match crate::gpu::ocio_pass::OcioGpuPass::from_bundle(
                    &rs.device,
                    &rs.queue,
                    &bundle,
                    eframe::egui_wgpu::wgpu::TextureFormat::Rgba8Unorm,
                ) {
                    Ok(thumb_pass) => gpu.publish_ocio_thumbnail_pass(thumb_pass),
                    Err(_) => gpu.clear_ocio_thumbnail_pass(),
                }
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
        let repaint_ctx = self.repaint_ctx.clone();
        std::thread::spawn(move || {
            let result = crate::color::cube::CubeLut::load(&path)
                .map_err(|e| format!("Failed to load LUT: {e}"));
            let _ = tx.send(LutLoadResult { path, result });
            // Wake the UI so the parsed LUT applies immediately (#137) instead
            // of waiting for the next input-driven repaint.
            if let Some(ctx) = &repaint_ctx {
                ctx.request_repaint();
            }
        });
    }

    /// Snapshot to clipboard (#19): drive the hotkey trigger and consume the
    /// `Event::Screenshot` reply. Called once per frame from [`Self::ui`].
    fn process_snapshot(&mut self, ctx: &egui::Context) {
        // Deliver the off-thread finalize outcomes (#146); with overlapping
        // snapshots the latest status wins.
        if let Some(rx) = &self.snapshot_rx {
            while let Ok(status) = rx.try_recv() {
                self.snapshot_status = Some(status);
            }
        }

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

    /// Crop the captured framebuffer to the image canvas, then copy it to the
    /// clipboard and (when enabled) save a timestamped PNG **off-thread** —
    /// arboard + PNG encode/write are a several-hundred-ms stall at 4K, and this
    /// runs in the frame that receives `Event::Screenshot`, mid-playback if
    /// playing (#146). The outcome arrives over `snapshot_rx`.
    fn finish_snapshot(&mut self, image: &egui::ColorImage, pixels_per_point: f32) {
        // Crop to the active image area (#52), falling back to the full canvas.
        //
        // The clamp to the canvas happens here rather than at the `last_image_rect`
        // write site (#99 Slice 3a): the comp path reuses that rect for the cursor→
        // pixel readout (`comp_hover_pixel` normalizes across it), so storing a
        // canvas-clipped rect would skew the readout whenever the image is zoomed
        // past the viewport. The legacy path clamped on write and had separate hover
        // geometry; doing it here keeps both consumers correct. Two panes span the
        // canvas, so Side-by-Side takes the whole canvas.
        let Some(rect) = self
            .viewer
            .last_image_rect
            .zip(self.viewer.last_canvas_rect)
            .map(|(img, canvas)| {
                crate::snapshot::active_area_rect(
                    canvas,
                    img,
                    self.viewer.last_image_rect_b.is_some(),
                )
            })
            .or(self.viewer.last_canvas_rect)
        else {
            return;
        };
        let cropped = crate::snapshot::crop_to_rect(image, rect, pixels_per_point);

        let save = self.save_snapshots;
        // One persistent channel; workers clone the sender. Replacing the
        // receiver per snapshot would silently drop the status of a finalize
        // still in flight when a second snapshot fires.
        if self.snapshot_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.snapshot_tx = Some(tx);
            self.snapshot_rx = Some(rx);
        }
        let tx = self
            .snapshot_tx
            .clone()
            .expect("snapshot channel initialized above");
        self.snapshot_status = Some("Snapshot: saving…".to_string());
        let repaint_ctx = self.repaint_ctx.clone();
        std::thread::spawn(move || {
            let mut parts = Vec::new();
            match crate::snapshot::copy_to_clipboard(&cropped) {
                Ok(()) => parts.push("copied to clipboard".to_string()),
                Err(e) => parts.push(format!("clipboard failed: {e}")),
            }
            if save {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                match crate::snapshot::save_png(&cropped, secs) {
                    Ok(path) => parts.push(format!("saved {}", path.display())),
                    Err(e) => parts.push(format!("save failed: {e}")),
                }
            }
            let _ = tx.send(format!("Snapshot: {}", parts.join(", ")));
            if let Some(ctx) = &repaint_ctx {
                ctx.request_repaint();
            }
        });
    }

    /// Apply a completed [`LutLoadResult`] from the worker thread: create the
    /// GPU bind group, capture the domain bounds, and update `lut_bg` /
    /// `lut_error` / `enable_lut`. Ignores stale results (a newer reload of a
    /// different path superseded this one).
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
                    // Drop-only, never `destroy()` (#120): a recorded-but-
                    // unsubmitted draw can still reference the old LUT bind
                    // group when the load lands mid-frame, and destroying an
                    // in-flight texture aborts the submit on Vulkan. Dropping
                    // the handle lets wgpu reclaim it when the last reference
                    // ends — a .cube LUT is well under a megabyte, so deferred
                    // reclaim costs nothing.
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
                // Drop-only, same #120 hazard as the success arm.
                self.lut_texture = None;
                self.enable_lut = false;
                self.lut_domain_min = [0.0, 0.0, 0.0, 0.0];
                self.lut_domain_max = [1.0, 1.0, 1.0, 0.0];
            }
        }
        // Both arms change what an enabled LUT renders as (new contents, or
        // force-disabled on error); cached thumbnails baked the old state (#147).
        self.viewer.invalidate_tone();
    }

    /// Arm slot-A sequence playback from `path` — **test fixture only** (#99
    /// Slice 3h.2). Production arms the transport through `add_comp_source`, which
    /// uses `sequence::detect_from_file` + `playback.enter` directly; this survived
    /// its production caller (`open_file`) because ~45 playback tests use it to set
    /// up a sequence before exercising advance / loop / scrub / cache / pump — all
    /// of which are live. Rewriting those onto `add_comp_source` is a test-refactor
    /// worth doing separately, not deletion fallout.
    /// Drops the frame cache — it is keyed by frame number, which a different
    /// sequence reuses.
    #[cfg(test)]
    fn detect_sequence(&mut self, path: &std::path::Path) {
        self.frame_cache.clear();
        self.frame_bytes = None;
        // Reset the debug-overlay counters so each opened sequence's soak run
        // starts clean (#100).
        self.dbg_evictions = 0;
        self.dbg_fallbacks = 0;
        self.dbg_dropped_epoch = 0;
        // A different sequence reuses frame numbers, so drop the T2 GPU ring too
        // (and reset the on-screen frame; the first show re-sets it).
        self.viewer.clear_t2(Self::A_SOURCE);
        self.viewer.set_t2_frame(Self::A_SOURCE, None);
        // Drop any prior sequence's in-flight frames (a different sequence reuses
        // frame numbers); `enter`/`clear` bump the epoch so their results are
        // dropped. `loading_a` is left to the caller.
        self.inflight.clear();
        // A new sequence resets the precache fill latch (#56, step 4).
        self.precache_filled = false;
        // A new sequence (or a lone image) invalidates the watch baseline; the
        // next poll re-baselines against the freshly-opened group.
        self.watch_sigs.clear();
        self.last_watch_poll = None;
        match crate::sequence::detect_from_file(path) {
            Some(seq) => {
                let start = seq.number_of(path).unwrap_or(seq.range.0);
                self.playback.enter(seq, start);
            }
            None => self.playback.clear(),
        }
    }

    /// Whether two paths name the same file on disk.
    ///
    /// Cheap comparison first, then `canonicalize` — the same file reaches the app
    /// spelled differently all the time: a relative path on the command line versus
    /// the absolute one a file picker returns, `..` segments, or Windows' `/` and
    /// `\` mixed in a dropped path. Raw `PathBuf` equality misses every one of
    /// those, which would silently reintroduce the duplicate decode #242 removes.
    ///
    /// Deliberately used **only for comparison**, never to rewrite what is stored.
    /// On Windows `canonicalize` returns a `\\?\`-prefixed UNC path, which would
    /// leak into layer names, the recent-files menu and `app.ron` if it were
    /// adopted as the path of record.
    ///
    /// Falls back to raw inequality when either side cannot be resolved (a deleted
    /// or unreachable file): unknown is not the same as equal.
    fn same_file(a: &Path, b: &Path) -> bool {
        a == b
            || match (a.canonicalize(), b.canonicalize()) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
    }

    /// The layer already drawing `path`, if the stack has one (#242).
    ///
    /// Matches on the `CompSource` path rather than the layer name, which the user
    /// can rename. The first match wins; with re-open focusing instead of adding,
    /// the only way to get two layers on one path is [`Self::duplicate_comp_layer`],
    /// and either of that pair is a correct answer to "where is this file".
    fn layer_for_path(&self, path: &Path) -> Option<crate::layer::LayerId> {
        self.comp_stack.iter().find_map(|l| match &l.source {
            crate::layer::LayerSource::Image { source, .. }
                if self
                    .comp_sources
                    .get(source)
                    .is_some_and(|cs| Self::same_file(&cs.path, path)) =>
            {
                Some(l.id)
            }
            _ => None,
        })
    }

    /// Bring a file into the stack as a new layer — the unified open/drop entry
    /// (#99 R4). Records recent files, ensures the Layers panel is visible, then
    /// decodes + adds the layer via [`Self::add_comp_source`] (a synchronous
    /// decode: it promotes slot A to a base track on the first add, drives the
    /// transport for the first sequence, and caps at [`COMP_LAYER_CAP`]).
    ///
    /// **A path already in the stack is focused, not added again** (#242). Opening
    /// a file means "show me this", and it is already shown; adding a second copy
    /// stacked exactly on the first is invisible — the picture does not change —
    /// while costing a full synchronous decode, another decode follower dividing
    /// the single worker's turns, and another share of the T1 budget via
    /// `n_active_sources`. That made a stack grow silently across sessions and the
    /// app get slower with no apparent cause. Deliberate duplication is
    /// [`Self::duplicate_comp_layer`], which is explicit, instant, and shares the
    /// already-decoded source.
    ///
    /// Recent files are still recorded, and the panel still shown: the user asked
    /// for this file, and both are true whether or not a layer had to be created.
    fn open_layer(&mut self, path: PathBuf) {
        // Same-file, not same-spelling (#242 review): otherwise the relative and
        // absolute forms of one file both sit in the menu as separate entries.
        self.recent_files.retain(|p| !Self::same_file(p, &path));
        self.recent_files.insert(0, path.clone());
        self.recent_files.truncate(10);
        self.show_layers_panel = true;
        if let Some(id) = self.layer_for_path(&path) {
            log::debug!(
                target: "floki::playback",
                "open: {} is already {id:?} — focusing it", path.display()
            );
            self.selected_comp_layer = Some(id);
            self.error_msg = None;
            return;
        }
        self.add_comp_source(path);
    }

    /// Add a second layer drawing the same source as `id`, and make it current
    /// (#242) — the explicit way to get what re-opening a file used to do by
    /// accident.
    ///
    /// Costs no decode: the copy shares the original's `SourceId`, so the pixels it
    /// needs are the pixels already cached under that key. It is a second view of
    /// one source — retime it, re-trim it, or point it at another AOV — rather than
    /// a second copy of the footage.
    ///
    /// Named `foo.exr (2)`, counting the layers already on that source, so the pair
    /// is distinguishable in the panel where two identical names would not be. The
    /// user can rename it; nothing keys off the name.
    ///
    /// **The slot-A base track cannot be duplicated** (#242 review). It is the sole
    /// layer allowed to reference `A_SOURCE` — [`Self::base_layer_id`] resolves it
    /// with a `find_map` on exactly that assumption, and
    /// [`Self::remove_base_layer`]'s teardown is written for one. It also would not
    /// survive a restart: [`Self::comp_layers_persist`] skips `A_SOURCE` layers
    /// (slot A is restored through its own path, not the layer list), so the copy
    /// would vanish on the next launch having quietly broken the invariant in the
    /// meantime.
    fn duplicate_comp_layer(&mut self, id: crate::layer::LayerId) {
        if self.comp_stack.len() >= COMP_LAYER_CAP {
            self.error_msg = Some(format!(
                "Layer limit reached ({COMP_LAYER_CAP}) — remove a layer to add more."
            ));
            return;
        }
        let Some(src) = self.comp_stack.get(id).and_then(|l| match &l.source {
            crate::layer::LayerSource::Image { source, .. } => Some(*source),
            crate::layer::LayerSource::Adjustment => None,
        }) else {
            return;
        };
        if src == Self::A_SOURCE {
            return;
        }
        let n = self
            .comp_stack
            .iter()
            .filter(|l| {
                matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == src)
            })
            .count();
        let Some(new_id) = self.comp_stack.duplicate(id) else {
            return;
        };
        if let Some(l) = self.comp_stack.get_mut(new_id) {
            // Re-number an existing `(N)` rather than stacking suffixes, but only
            // when the tail really is one (#242 review). A bare `rsplit_once(" (")`
            // treats any parenthesis as the marker, so `plate (final).exr` would be
            // truncated to `plate` — losing the part of the name that identified it.
            let base = l
                .name
                .rsplit_once(" (")
                .filter(|(_, tail)| {
                    tail.strip_suffix(')')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
                .map_or(l.name.as_str(), |(stem, _)| stem)
                .to_string();
            l.name = format!("{base} ({})", n + 1);
        }
        self.selected_comp_layer = Some(new_id);
    }

    /// Open a path given on the command line, through the **same entry as
    /// File ▸ Open and drag-drop** ([`Self::open_layer`]) — so launching with a
    /// path exercises the real default path (add-a-layer, comp-drives-transport)
    /// rather than a special startup route that the soak wouldn't be testing.
    ///
    /// Called from `main` right after construction. Safe there for the same reason
    /// `restore_comp_layers` is: `add_comp_source` decodes synchronously and only
    /// builds textures when `gpu_resources` is set, which `draw_comp_central`'s
    /// `ensure_comp_*` does on the first paint anyway.
    ///
    /// A missing path is reported into `error_msg` (via `add_comp_source`'s load
    /// failure) rather than silently ignored — a typo'd path in a soak command
    /// must be loud.
    pub fn open_cli_path(&mut self, path: PathBuf) {
        if !path.exists() {
            self.error_msg = Some(format!("No such file: {}", path.display()));
            log::error!(target: "floki::playback", "cli path does not exist: {}", path.display());
            return;
        }
        self.open_layer(path);
    }

    /// Send a decode job to the worker, **respawning it if it has died** (#…).
    /// A dead worker's `job_rx` has been dropped, so `send` fails and hands the
    /// job back inside the `SendError`; drop the stale channels so the next
    /// [`Self::ensure_worker`] spawns a fresh thread, then resend. Without this a
    /// crashed/wedged worker silently swallows every subsequent job and playback
    /// (and even reopening) would decode nothing — the unrecoverable-freeze class.
    fn submit_job(&mut self, job: LoadJob) {
        let tx = self.ensure_worker();
        if let Err(err) = tx.send(job) {
            log::warn!(
                target: "floki::playback",
                "decode worker channel closed; respawning worker and resending job"
            );
            self.load_tx = None;
            self.load_rx = None;
            let tx = self.ensure_worker();
            let _ = tx.send(err.0);
        }
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
            let epoch_signal = std::sync::Arc::clone(&self.epoch_signal);
            // Persistent proxy cache (#165), shared with the worker: read-through
            // (hit → skip decode) + write-through on a proxy miss. Cloned before
            // the spawn like `epoch_signal`; a respawn re-clones from `self`.
            let proxy_cache = std::sync::Arc::clone(&self.proxy_cache);
            let repaint_ctx = self.repaint_ctx.clone();
            // Wake the UI as soon as a result is queued (#137). Without this the
            // result waits for the next scheduled repaint — up to the full 50 ms
            // in-flight poll — on top of decode time, and the one-outstanding
            // pump leaves the worker idle for that window too.
            let wake_ui = move || {
                if let Some(ctx) = &repaint_ctx {
                    ctx.request_repaint();
                }
            };
            std::thread::spawn(move || {
                // Reused box-filter accumulator for proxy downsampling (#171),
                // owned by the worker for its whole life. Proxies are uniform-size
                // per sequence, so this allocates + zeroes once instead of per
                // channel per frame during scrub/playback (the decode-side analogue
                // of the T2 `t2_staging` reuse).
                let mut proxy_scratch: Vec<f32> = Vec::new();
                for job in job_rx {
                    // Drop a sequence job a newer seek/scrub already superseded,
                    // **before** paying the decode (#…). Rapid scrubbing queues
                    // many soon-stale jobs in this FIFO channel; decoding each one
                    // (only for `apply_load_result` to drop it by epoch) strands
                    // the awaited frame behind the backlog for seconds. Skipping
                    // drains the backlog in microseconds. Opens are generation-
                    // keyed, not epoch-keyed, so they are never skipped here.
                    if job.seq_frame
                        && job.epoch < epoch_signal.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        continue;
                    }
                    // Decode mode while the playhead moves, cheapest first (#94/#56):
                    // - a **scrub proxy** (downsampled, tiny + fast) when requested,
                    //   falling back to beauty/full if the fast read isn't available;
                    // - **beauty-only** (one layer) otherwise while moving (#56 step 3);
                    // - full all-AOV for opens and the settle re-decode.
                    // #165: a proxy job checks the on-disk cache first — a hit is a
                    // raw f16 read (~zero decode); a miss decodes then write-throughs.
                    // #217: `aov_layer` names a single non-beauty pass to decode; the
                    // AOV is part of the cache key, or layer 1's proxy would be a
                    // verified hit for layer 2.
                    let mut store_blob: Option<(crate::proxy_cache::ProxyKey, Vec<u8>)> = None;
                    let aov = job.aov_layer.map_or(0, |(_, logical)| logical);
                    let result = match job.proxy_target {
                        Some(tb) => {
                            if let Some(cached) = proxy_cache.read(&job.path, tb, aov) {
                                Ok(cached)
                            } else {
                                let decoded = match job.aov_layer {
                                    Some((phys, logical)) => ExrData::load_layer_proxy_into(
                                        &job.path,
                                        phys,
                                        logical,
                                        tb,
                                        &mut proxy_scratch,
                                    ),
                                    None => {
                                        ExrData::load_proxy_into(&job.path, tb, &mut proxy_scratch)
                                    }
                                }
                                // A full decode is the fallback for *any* failure,
                                // including a single-layer decode this file's part
                                // layout can't support: it always carries the AOV,
                                // so the texture build can't fail and spin the
                                // evict-and-retry loop (#213). Slow beats stuck.
                                .or_else(|_| {
                                    if job.beauty_only && job.aov_layer.is_none() {
                                        ExrData::load_beauty(&job.path)
                                    } else {
                                        ExrData::load(&job.path)
                                    }
                                });
                                // Cache only a genuine proxy (never a fallback
                                // beauty/full). Serialize here (miss-only, <1% of
                                // the decode just paid) so the borrow ends before
                                // `decoded` is moved into the message; the disk
                                // write happens off-thread in the cache's writer.
                                if let Ok(data) = &decoded
                                    && data.proxy
                                    && let Some(key) =
                                        crate::proxy_cache::ProxyKey::for_source(&job.path, tb, aov)
                                {
                                    let blob = data.write_proxy_blob(&key);
                                    store_blob = Some((key, blob));
                                }
                                decoded
                            }
                        }
                        None if job.beauty_only => match job.aov_layer {
                            Some((phys, logical)) => ExrData::load_layer(&job.path, phys, logical)
                                .or_else(|_| ExrData::load(&job.path)),
                            None => ExrData::load_beauty(&job.path),
                        },
                        None => ExrData::load(&job.path),
                    };
                    // Did an `or_else` above fire? Compare what came back against
                    // what was asked for (#233). Every fallback here is deliberate
                    // — slow beats stuck — but it was also *silent*, and a job
                    // budgeted as a 9 MB proxy that returns a 1035 MB full frame is
                    // an 18x mis-budget the caller had no way to see.
                    let requested = fidelity_rank(job.proxy_target.is_some(), job.beauty_only);
                    let fell_back = result
                        .as_ref()
                        .is_ok_and(|d| fidelity_rank(d.proxy, d.beauty_only) > requested);
                    let _ = result_tx.send(LoadMsg::Loaded(Box::new(LoadResult {
                        source: job.source,
                        seq_frame: job.seq_frame,
                        frame: job.frame,
                        epoch: job.epoch,
                        open_gen: job.open_gen,
                        fell_back,
                        result,
                    })));
                    wake_ui();
                    // Queue the proxy write-through *after* the UI has the frame, so
                    // the awaited frame never waits on the write path (#165).
                    if let Some((key, blob)) = store_blob {
                        proxy_cache.store(&key, blob);
                    }
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
    fn request_sequence_frame(&mut self, frame: u32) {
        self.error_msg = None;
        // Comp stack drives the transport (#99 R4-lite): no slot-A sequence, so skip
        // the slot-A request entirely and just advance the comp followers to the new
        // playhead. (`self.playback.frame_path` points at the clock-driving comp
        // sequence, but decoding it as A_SOURCE would double-decode + steal priority.)
        if self.comp_drives_transport() {
            self.sync_comp_followers();
            self.pump_decode();
            return;
        }
        let Some(path) = self.playback.frame_path(frame).map(Path::to_path_buf) else {
            // Hole: keep showing the last real frame; prefetch may still run.
            self.playback.pending = None;
            self.pump_decode();
            return;
        };
        self.loaded_file = Some(path);

        if let Some(data) = self.frame_cache.get(Self::A_SOURCE, frame) {
            // A beauty-only ring frame (#56, step 3) is fine to *display* while
            // moving, but on settle it must be upgraded to a full all-AOV decode
            // so the readout + AOV switch are correct (INV-SAMPLE, #7). Show it
            // now for instant feedback, but keep the playhead awaited so sampling
            // stays suppressed until the full frame lands. An active timeline
            // drag counts as moving (#143): upgrading every touched frame
            // mid-drag would spam full decodes; the release settles instead.
            let needs_full = !self.playback.is_playing()
                && !self.scrub_active
                && (data.beauty_only || data.proxy);
            self.loading_a = false;
            self.viewer.set_t2_frame(Self::A_SOURCE, Some(frame)); // bind this frame's T2 texture
            self.swap_image_arc(data);
            if needs_full {
                self.playback.pending = Some(frame);
            } else {
                // Cache hit: show immediately, no decode round-trip.
                self.playback.pending = None;
                self.playback.note_shown(std::time::Instant::now(), frame);
            }
        } else {
            // Miss: mark the playhead as awaited; `pump_decode` submits it (the
            // want-list puts the playhead first, so it beats any prefetch).
            self.playback.pending = Some(frame);
        }
        // Comp-panel sequence layers (#99 Phase 2) also follow the shared playhead —
        // each maps the global frame through its `Trim` and requests it, so the pump
        // decodes it alongside the primary. No-op with no comp sequence layers.
        self.sync_comp_followers();
        self.pump_decode();
    }

    /// Advance each comp-panel **sequence** layer to its `Trim`-mapped source frame
    /// for the current global playhead and request it (#99 Phase 2) — the comp
    /// for the current global playhead and request it (#99 Phase 2). A layer whose trim does not cover the
    /// current global frame is blank (holds its last frame); a still (no follower)
    /// is skipped. No-op with no comp sequence layers.
    fn sync_comp_followers(&mut self) {
        let global = self.playback.current_frame;
        // Snapshot (source, source_frame) for each in-range sequence layer before
        // mutating followers / requesting (avoids aliasing the `comp_stack` borrow).
        let wants: Vec<(crate::layer::SourceId, u32)> = self
            .comp_stack
            .iter()
            .filter_map(|l| match &l.source {
                crate::layer::LayerSource::Image { source, .. }
                    if self.followers.contains_key(source) =>
                {
                    l.trim.source_frame(global).map(|sf| (*source, sf))
                }
                _ => None,
            })
            .collect();
        for (source, sf) in wants {
            if let Some(st) = self.followers.get_mut(&source) {
                st.current_frame = sf;
            }
            // A hidden layer keeps tracking the playhead but asks for nothing
            // (#211): un-hiding then lands on the right frame and re-warms like any
            // seek, instead of resuming from wherever it was when it went dark.
            // Requesting is what costs — decode turns, ring slots, eviction
            // protection — and none of that should be spent on pixels nobody sees.
            if !self.source_is_visible(source) {
                if let Some(st) = self.followers.get_mut(&source) {
                    st.pending = None;
                    st.loading = false;
                }
                continue;
            }
            // Pacing is *not* recorded here. Residency at advance time says the
            // pixels are available, not that they were painted — and through the
            // runs where the picture was actually frozen this fired for frames that
            // never reached the screen (#204). `ensure_comp_frame` counts the texture
            // swap instead, which is the display itself.
            self.request_comp_frame(source, sf);
        }
    }

    /// Request a comp source's `frame` for the pump (#99 Phase 2) — the comp
    /// counterpart of [`Self::request_b_frame`]. A comp follower has no display slot
    /// (`draw_comp_central` binds it from the T1 cache), so this only clears the
    /// stale wait state and marks the frame awaited when it isn't already resident.
    fn request_comp_frame(&mut self, source: crate::layer::SourceId, frame: u32) {
        // A resident *cheap* frame still needs upgrading once we settle (#212),
        // exactly as slot A does in `request_sequence_frame`. `next_want` treats
        // `pending` as "not resident", so marking it here is what gets the frame
        // re-decoded at full fidelity; without it, a proxy cached during a scrub is
        // resident forever and the layer never sharpens. Mid-move this stays off —
        // upgrading every frame the playhead touches during a drag would spam full
        // decodes, which is why the release is what settles.
        let resident_final = self
            .frame_cache
            .peek(source, frame)
            .map(|d| !d.proxy && !d.beauty_only);
        let settled = !self.playback.is_playing() && !self.scrub_active;
        let resident = match resident_final {
            None => false,
            Some(true) => true,
            Some(false) => !settled, // cheap copy: good enough only while moving
        };
        let is_hole = self
            .followers
            .get(&source)
            .and_then(|st| st.sequence.as_ref())
            .is_none_or(|s| s.path_for(frame).is_none());
        if let Some(st) = self.followers.get_mut(&source) {
            st.loading = false;
            st.pending = if is_hole || resident {
                None
            } else {
                Some(frame)
            };
        }
        self.pump_decode();
    }

    /// The total prefetch window around the playhead (ahead + the #169
    /// read-behind reservation, split in [`Self::next_want`]) — bounded by the
    /// T1 ring (`frame_cache_cap − 1`, tying #57 back-pressure to the #56 budget) and by
    /// the transport range, so a huge sequence can't queue the world.
    ///
    /// This is the **single** window figure: playing and idle precache both ask
    /// for it (#207). They used to disagree — idle took the whole budget while
    /// playing was clamped to a `MAX_PREFETCH = 16` constant — so pressing Play
    /// *shrank* the lookahead, which is backwards: play is when read-ahead
    /// matters most. The constant was picked while `frame_cache_cap` was frozen at
    /// its constructed default of 8 (#199), where `min(7, 16)` never bound on
    /// anything. Once the cap became a real measured budget the constant became
    /// the binding constraint instead of memory: on a 6-layer stack at `cap = 121`
    /// it cut the per-source window from 20 frames to 2, and since
    /// [`crate::scheduler::read_behind`] floors to zero below a depth of 4, the
    /// RV-style lookback window silently stopped existing along with it.
    ///
    /// Deepening the window does **not** deepen decode concurrency: `pump_decode`
    /// still submits one job at a time across all sources and returns early while
    /// anything is in flight. It changes which frame the single worker picks next,
    /// not how many run — so it does not reopen the #204 finding that decoding
    /// faster than frames can be painted *lowers* the displayed rate.
    fn prefetch_depth(&self) -> usize {
        // Never walk more positions than the range holds. Past the loop wrap
        // `want_list` only re-lists frames already on the list, so the extra
        // positions are pure iteration — and this is what bounds the window on a
        // short range now that the constant is gone: an 8-frame Beachball against
        // a 734-frame budget walks 7 positions, not 733.
        let span = usize::try_from(
            self.playback
                .out_point
                .saturating_sub(self.playback.in_point),
        )
        .unwrap_or(usize::MAX);
        self.frame_cache_cap.saturating_sub(1).min(span)
    }

    /// The source id of the **primary** compare slot (A) — `SourceId(0)`. The
    /// master clock's source; keys A's T1 cache + T2 ring (#99).
    const A_SOURCE: crate::layer::SourceId = crate::layer::SourceId(0);

    /// Whether the comp stack (rather than slot A) owns the global clock — the
    /// boolean reading of [`Self::transport_source`], for the many sites that only
    /// need "the primary isn't decoding".
    fn comp_drives_transport(&self) -> bool {
        self.transport_source.is_some()
    }

    /// The source whose decodes the transport is actually paced by: the
    /// clock-driving comp follower, or [`Self::A_SOURCE`] when slot A owns the
    /// clock. Every per-frame transport instrument keys off this rather than
    /// `A_SOURCE` directly, so it keeps working in both worlds (#100).
    fn clock_source(&self) -> crate::layer::SourceId {
        let claimed = self.transport_source.unwrap_or(Self::A_SOURCE);
        // A hidden layer must not drive the clock (#211). Pacing is recorded only
        // for the clock source, and a hidden source never paints — so with one
        // holding the clock, `fps=` reads 0.0 through a run that is playing
        // perfectly, and every pacing percentile goes with it. Observed live:
        // `settled=[s2:-SOFT, s3:-SOFT, s4:1045full] fps=0.0/24`, where s2 held the
        // clock while hidden and s4 was the one actually on screen.
        if self.source_is_visible(claimed) {
            return claimed;
        }
        // Fall back to the lowest-numbered visible sequence follower — id order, so
        // the choice is stable frame to frame rather than HashMap-random.
        self.followers
            .iter()
            .filter(|(id, s)| s.sequence.is_some() && self.source_is_visible(**id))
            .map(|(id, _)| *id)
            .min()
            .unwrap_or(claimed)
    }

    /// Whether the transport is still waiting on the frame it means to display —
    /// the readiness question the pacer asks, answered for **whichever source
    /// drives the clock** (#100).
    ///
    /// The slot-A fields alone are not that answer: once the comp stack owns the
    /// transport they are empty by construction (`request_sequence_frame` returns
    /// before touching them) and a follower carries its own wait state. Reading only
    /// them made Stutter advance past undecoded frames — silently behaving as
    /// DropFrames, with `run_held` never accruing and `ensure_comp_frame` holding a
    /// stale texture (#200).
    ///
    /// Deliberately only the clock source: a *trailing* layer that is behind must
    /// not stall the transport, or an N-layer comp would play at the speed of its
    /// slowest layer.
    fn transport_awaiting(&self) -> bool {
        if self.playback.pending.is_some() || self.loading_a {
            return true;
        }
        // `clock_source()`, not `transport_source` (#211). Since the clock can now
        // move off a hidden layer, the two differ exactly when the claimed source is
        // invisible — and a hidden source requests nothing, so its `pending` is
        // always `None`. Asking it whether the transport is ready would answer "yes,
        // always", letting Stutter advance past undecoded frames: #200 reopened by a
        // different route, and silently, since Stutter would just behave as
        // DropFrames. The readiness predicate has to track whichever source the
        // clock logic actually selected.
        let clock = self.clock_source();
        if clock == Self::A_SOURCE {
            return false; // slot A's wait state is the `pending`/`loading_a` check above
        }
        self.followers
            .get(&clock)
            .is_some_and(|st| st.pending.is_some() || st.loading)
    }

    /// Point the transport at `source` (or release it with `None`), dropping the
    /// measured frame sizing when the clock actually moves to a different source.
    ///
    /// `frame_bytes` is a `get_or_insert_with` latch, so without this a re-point to
    /// a **different-resolution** follower (#98) would keep sizing the ring off the
    /// old source forever — the same class of staleness as never seeding it at all.
    fn set_transport_source(&mut self, source: Option<crate::layer::SourceId>) {
        if self.transport_source == source {
            return;
        }
        self.transport_source = source;
        self.frame_bytes = None;
        self.proxy_bytes = None;
        self.beauty_bytes = None;
        self.decode_fell_back = false;
    }

    /// The *active* followers — those with a detected sequence (the N-source
    /// generalization of "B is playing"). A lone-image / absent follower holds a
    /// default `SourceState` and is skipped.
    fn active_followers(&self) -> impl Iterator<Item = (&crate::layer::SourceId, &SourceState)> {
        self.followers
            .iter()
            .filter(|(id, s)| s.sequence.is_some() && self.source_is_visible(**id))
    }

    /// Whether any **visible** comp layer draws `source` (#211).
    ///
    /// Hiding a layer used to stop it rendering but not working: it kept decoding,
    /// kept taking decode turns, kept dividing the T1 budget and the prefetch
    /// window, and kept its frames protected from eviction. Hiding a layer is the
    /// first thing a user reaches for when playback is slow, and it did nothing for
    /// throughput — the pool was still split N ways.
    ///
    /// Routed through [`crate::layer::LayerStack::visible`] rather than checking
    /// `enabled` directly, so solo is handled by the same rule the compositor
    /// renders by: with anything soloed, every non-soloed layer is invisible and
    /// should stop working too.
    ///
    /// A source no comp layer references at all counts as visible — that is the
    /// classic slot-A path, which has no layer to consult.
    fn source_is_visible(&self, source: crate::layer::SourceId) -> bool {
        let mut referenced = false;
        for l in self.comp_stack.iter() {
            if let crate::layer::LayerSource::Image { source: s, .. } = &l.source
                && *s == source
            {
                referenced = true;
                break;
            }
        }
        if !referenced {
            return true;
        }
        self.comp_stack.visible().any(|l| {
            matches!(&l.source, crate::layer::LayerSource::Image { source: s, .. } if *s == source)
        })
    }

    /// Resident source count for the per-source budget splits (#99): the primary
    /// (Slot A) plus every active follower. Replaces the hardcoded `/2` A+B split
    /// so N playing sources each get their fair T1/decode-ahead share. The primary
    /// isn't counted when the comp stack drives the transport (#99 R4-lite) — it
    /// doesn't decode then — so the followers get the whole budget. Floored at 1.
    fn n_active_sources(&self) -> usize {
        let primary = usize::from(!self.comp_drives_transport());
        (primary + self.active_followers().count()).max(1)
    }

    /// The protected on-screen playheads for T1 eviction (#99 unification): the
    /// primary (Slot A) always, plus every active follower's playhead.
    fn cache_playheads(&self) -> Vec<(crate::layer::SourceId, u32)> {
        let mut playheads = vec![(Self::A_SOURCE, self.playback.current_frame)];
        for (id, st) in self.active_followers() {
            playheads.push((*id, st.current_frame));
        }
        playheads
    }

    /// The read-behind reservation (#169) T1 eviction must protect — mirrors the
    /// decode pump's slot-A depth exactly (`pump_decode`'s playing depth,
    /// including the locked-step halving), so the evictor reserves precisely the
    /// window the scheduler maintains: a larger value would protect frames the
    /// pump can't fit and the two would churn. Zero while not playing —
    /// paused/scrub eviction is bidirectional-distance and needs no carve-out.
    /// The bound T1 eviction enforces (#232): the byte ceiling, plus the frame
    /// count derived from it. One place, so the two `evict_to` call sites — the
    /// #146 pressure shrink and the post-insert trim — can't be given different
    /// budgets.
    fn cache_bound(&self) -> crate::cache::Bound {
        crate::cache::Bound::new(self.frame_cache_cap, self.frame_cache_budget)
    }

    /// Whether the ring has taken all the budget allows, in either unit — the
    /// precache latch and the pump's back-pressure check.
    ///
    /// Both units matter: with the count alone, a ring full of frames dearer than
    /// the sizing figure sits under `cap` while over budget, so precache never
    /// latches and churns decode -> evict forever. That is the same failure the
    /// count check was added to fix (#56), in the other unit.
    fn cache_is_full(&self) -> bool {
        self.frame_cache.len() >= self.frame_cache_cap
            || self.frame_cache.bytes() >= self.frame_cache_budget
    }

    fn read_behind_depth(&self) -> usize {
        if !self.playback.is_playing() {
            return 0;
        }
        let full = self.prefetch_depth();
        // Split the reservation per active source (#99): the primary plus each
        // active follower, so it mirrors the pump's per-source decode-ahead depth
        // exactly. One follower → `/2` as before.
        let depth = full / self.n_active_sources();
        crate::scheduler::read_behind(depth)
    }

    /// The user-assigned T1 RAM budget in bytes, or `None` for "auto" (the live
    /// free-RAM budget). Applied as a ceiling in the cap computation (#56).
    ///
    /// Values below the control's display threshold (i.e. anything that renders
    /// as `Auto`) return `None`, so what the UI shows always matches what is
    /// applied — a hand-edited or float-rounded tiny positive can't silently
    /// impose an ultra-low cap while displaying `Auto`.
    fn ram_budget_bytes(&self) -> Option<u64> {
        if self.ram_budget_gb < RAM_BUDGET_AUTO_BELOW_GB {
            None
        } else {
            Some((f64::from(self.ram_budget_gb) * (1u64 << 30) as f64) as u64)
        }
    }

    /// Whether decoding `frame` should be **beauty-only** (#56, hardening step 3).
    /// Requires the `beauty_preview` kill-switch on and the viewer showing the
    /// beauty/first layer — a beauty-only frame holds just that layer, so it would
    /// be wrong to serve a different active AOV from it. Then:
    ///
    /// - **Playing or an active timeline drag** (#143) → every ring frame is
    ///   beauty (the readout is suppressed while moving — or, for a landed scrub
    ///   frame, correct for the beauty layer that's showing — INV-SAMPLE, #7).
    ///   The drag release settles the landing frame to full (`settle_to_full`).
    /// - **Settled + precache** (#56, step 4) → the *prefetched* frames are beauty
    ///   for future playback, but the playhead itself stays **full** so its
    ///   sampling + AOV switch are correct.
    /// - Otherwise (a plain paused seek) → full.
    fn decode_beauty_only(&self, frame: u32) -> bool {
        self.decode_beauty_only_at(Self::A_SOURCE, frame, self.playback.current_frame)
    }

    /// A source's current playhead (#99): the master transport for the primary
    /// (Slot A), else the follower's slaved frame. Drives the per-source cheap-
    /// decode gate below and the settle checks (each source's "settled playhead" is
    /// its own frame). `0` for an unknown source (never happens for a live source).
    fn source_playhead(&self, source: crate::layer::SourceId) -> u32 {
        if source == Self::A_SOURCE {
            self.playback.current_frame
        } else {
            self.followers.get(&source).map_or(0, |st| st.current_frame)
        }
    }

    /// The `Trim::offset` of the layer drawing `source` — how far that source's
    /// own frame numbering is shifted from the global timeline (`source = global +
    /// offset`), so a source frame maps back with `global = source - offset`.
    ///
    /// `0` for slot A (the master transport is the global numbering by definition)
    /// and for a source no layer draws. A source backing more than one layer is
    /// answered by the first, which is exact while the Add flow makes one source
    /// per file — the same assumption [`Self::ensure_comp_aov`] already documents.
    fn source_trim_offset(&self, source: crate::layer::SourceId) -> i64 {
        if source == Self::A_SOURCE {
            return 0;
        }
        self.comp_stack
            .iter()
            .find_map(|l| match &l.source {
                crate::layer::LayerSource::Image { source: s, .. } if *s == source => {
                    Some(l.trim.offset)
                }
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Resident frames for the ruler's cache-fill bar (#245), in **global**
    /// timeline numbers and clipped to `[in_pt, out_pt]` — as
    /// `(resident, full_fidelity)`, the two tones the bar shades.
    ///
    /// Keyed on [`Self::clock_source`], not `A_SOURCE`. The bar used to read slot A
    /// unconditionally, and under `comp_drives_transport` slot A holds no frames at
    /// all — so the bar was empty on the default path however full the ring was,
    /// reading as "the precache isn't working" on a session with hundreds of
    /// resident frames. Same `A_SOURCE`-keyed blind spot as #199/#200/#201, in the
    /// UI this time. With slot A driving, `clock_source()` *is* `A_SOURCE`, so the
    /// classic path is unchanged.
    ///
    /// The trim mapping matters as soon as the answer isn't slot A: a follower's
    /// ring is keyed by its **own** frame numbers, and a retimed layer's frame 12
    /// is not global frame 12. Painting them unmapped would put the fill under the
    /// wrong part of the ruler — subtly, and only for retimed layers.
    fn cache_bar_frames(&self, in_pt: u32, out_pt: u32) -> (Vec<u32>, Vec<u32>) {
        let source = self.clock_source();
        let offset = self.source_trim_offset(source);
        let to_global = |f: u32| {
            let g = i64::from(f).saturating_sub(offset);
            (g >= i64::from(in_pt) && g <= i64::from(out_pt)).then_some(g as u32)
        };
        (
            self.frame_cache
                .resident_frames(source)
                .filter_map(to_global)
                .collect(),
            self.frame_cache
                .resident_full_frames(source)
                .filter_map(to_global)
                .collect(),
        )
    }

    /// Per-source counterpart of [`Self::decode_beauty_only`] (#99): the
    /// settled-precache "keep the playhead full" exception measures against the
    /// source's own current frame (its numbers live in its own range, not A's), so
    /// a paused compared frame is decoded full for correct compare-mode sampling.
    /// The primary delegates to its canonical (tested) gate.
    fn decode_beauty_only_for(&self, source: crate::layer::SourceId, frame: u32) -> bool {
        if source == Self::A_SOURCE {
            self.decode_beauty_only(frame)
        } else {
            self.decode_beauty_only_at(source, frame, self.source_playhead(source))
        }
    }

    /// Whether a cheaper decode can even represent what `source` is showing (#213).
    ///
    /// Beauty-only (#56) and proxy (#94) decodes both carry **only logical layer
    /// 0**, so they are safe only when nothing on screen needs another AOV.
    ///
    /// This used to ask `viewer.active_layer`, which is right for the classic
    /// single-image path — there, the viewer's active layer *is* what's displayed —
    /// and wrong for a comp stack, where every layer carries its own `aov`. A layer
    /// on AOV 1 with the viewer on 0 passed the gate, got a frame containing only
    /// layer 0, and then `logical_channels(1)` returned `None` in
    /// `build_layer_texture`. The build failed silently, `cur_frame` never advanced,
    /// and the layer froze **permanently** — while `t1`, `last_decode` and every
    /// other decode-side metric reported perfect health.
    ///
    /// Checks *every* layer drawing this source, not just visible ones: a hidden
    /// layer on a non-zero AOV would otherwise be served cheap frames it can't
    /// display and appear frozen the moment it was un-hidden.
    fn cheap_decode_fits_aov(&self, source: crate::layer::SourceId) -> bool {
        self.cheap_decode_layer(source).is_some()
    }

    /// The one AOV every consumer of `source` is displaying, or `None` if they
    /// disagree (#217).
    ///
    /// A cheap decode carries a *single* logical layer, so it can only stand in
    /// when there is a single answer to "which pass is on screen". Two comp layers
    /// on the same source at different AOVs is the disagreeing case: no one-layer
    /// decode serves both, and the full decode is the only correct answer.
    ///
    /// Checks *every* layer drawing this source, not just visible ones: a hidden
    /// layer on a different AOV would otherwise be served frames it can't display
    /// and appear frozen the moment it was un-hidden.
    fn displayed_aov(&self, source: crate::layer::SourceId) -> Option<usize> {
        let mut want: Option<usize> = None;
        for l in self.comp_stack.iter() {
            if let crate::layer::LayerSource::Image { source: s, aov } = &l.source
                && *s == source
            {
                match want {
                    Some(w) if w != *aov => return None, // two passes, one decode
                    _ => want = Some(*aov),
                }
            }
        }
        // No comp layer draws it: the classic path, where the viewer's active layer
        // is what's on screen.
        want.or(Some(self.viewer.active_layer))
    }

    /// The layer a cheap decode of `source` must carry — `(physical part index,
    /// logical AOV index)` — or `None` if no cheap decode can represent what's on
    /// screen (#213/#217).
    ///
    /// This is the #213 invariant, generalised rather than weakened: a decode must
    /// never be cheaper than what the displayed AOV needs. What changed is *how
    /// cheap* is achievable. Beauty-only (#56) and proxy (#94) decodes carry
    /// logical layer 0 and nothing else, so before #217 the only safe answer for a
    /// non-zero AOV was "decode everything" — a cliff, not a slope: a layer on AOV
    /// 0 played, the same layer on AOV 1 fell back to a 260 ms all-parts decode.
    /// `ExrData::load_layer` can now decode any *one* part, so a non-zero AOV takes
    /// the cheap path too — but only when the part it lives in holds exactly one
    /// logical layer, which is what [`Self::single_layer_part`] decides.
    ///
    /// **The single-layer condition is not a nicety.** A one-layer decode of a part
    /// holding several passes cannot answer `logical_channels` for any of them
    /// (`ExrData::resolve_logical` refuses rather than guess), so every texture
    /// build fails — and #213's evict-and-retry hardening then re-decodes the same
    /// frame forever. Refusing here keeps that unreachable; `load_layer`'s own
    /// check is the second line, for a sequence whose part layout changes mid-shot.
    fn cheap_decode_layer(&self, source: crate::layer::SourceId) -> Option<(usize, usize)> {
        let aov = self.displayed_aov(source)?;
        // AOV 0 keeps the proven `first_valid_layer` path (`load_beauty` /
        // `load_proxy`), which needs no layout knowledge at all — so a source whose
        // table we can't read still plays as fast as it did before #217.
        if aov == 0 {
            return Some((0, 0));
        }
        Some((self.single_layer_part(source, aov)?, aov))
    }

    /// The physical part logical layer `aov` of `source` lives in, **if** that part
    /// holds exactly one logical layer (#217). `None` otherwise, including when the
    /// layer table isn't known.
    ///
    /// This is the multi-part-render shape — Karma/Houdini write one AOV per part,
    /// which is what makes part selection able to skip work at all. A single-part
    /// file with prefixed channels (Blender's `ViewLayer.Combined.R` style) puts
    /// every pass in one part, so the answer here is `None` for every non-zero AOV
    /// and those files keep the full decode. That is correct, not a gap: their
    /// passes share the same compressed blocks, so there is nothing for a
    /// part-level filter to skip. Channel-level filtering is a separate mechanism.
    fn single_layer_part(&self, source: crate::layer::SourceId, aov: usize) -> Option<usize> {
        let data = self.full_layer_table(source)?;
        let phys = data.logical_layers.get(aov)?.physical_index;
        let siblings = data
            .logical_layers
            .iter()
            .filter(|l| l.physical_index == phys)
            .count();
        (siblings == 1).then_some(phys)
    }

    /// A **full** decode of `source`, for reading its layer table (#217).
    ///
    /// Deliberately not "whatever is on screen": `self.exr_data` is swapped to every
    /// frame the transport lands, including the cheap ones, and a one-layer decode's
    /// table is a single renumbered entry — reading part layout from it would answer
    /// "yes, one layer per part" about every file. A comp source's `exr_data` is the
    /// open-time decode and is never replaced, so it is the reliable source.
    ///
    /// `None` (⇒ no fast path, full decode, i.e. exactly the pre-#217 behaviour) if
    /// nothing full is at hand. Fail-safe by construction: the fast path is only
    /// ever taken on a layout we have actually read.
    fn full_layer_table(&self, source: crate::layer::SourceId) -> Option<&ExrData> {
        let full = |d: &ExrData| !d.proxy && !d.beauty_only && d.only_layer.is_none();
        if let Some(cs) = self.comp_sources.get(&source)
            && full(&cs.exr_data)
        {
            return Some(&cs.exr_data);
        }
        self.exr_data
            .as_deref()
            .filter(|d| source == Self::A_SOURCE && full(d))
    }

    /// The shared "decode something cheaper while the playhead moves" condition for
    /// beauty (#56) and proxy (#94): a cheap decode can represent what `source`
    /// shows AND the frame is playing, being dragged, or a precache prefetch (not
    /// the settled playhead). The respective `beauty_preview` / `proxy_enabled`
    /// kill-switches decide *which* cheaper decode is used.
    fn wants_cheap_decode_at(
        &self,
        source: crate::layer::SourceId,
        frame: u32,
        playhead: u32,
    ) -> bool {
        if !self.cheap_decode_fits_aov(source) {
            return false;
        }
        if self.playback.is_playing() || self.scrub_active {
            return true;
        }
        self.precache && frame != playhead
    }

    fn decode_beauty_only_at(
        &self,
        source: crate::layer::SourceId,
        frame: u32,
        playhead: u32,
    ) -> bool {
        self.beauty_preview && self.wants_cheap_decode_at(source, frame, playhead)
    }

    /// The proxy resolution *playback* needs for `source` (#209): the number of
    /// physical screen pixels its long side currently occupies.
    ///
    /// `viewer.scale` is points per source pixel, so `long × scale ×
    /// pixels_per_point` is exactly the on-screen footprint in real pixels — the
    /// point past which more source resolution cannot be seen. Fit-to-window on a
    /// 1200-point canvas needs ~1200; zooming to 2:1 needs twice that, and asks for
    /// it automatically. Pausing is unaffected: `settle_to_full` still brings the
    /// playhead back to full resolution for sampling (INV-SAMPLE).
    ///
    /// This replaces `proxy_size` for playback because that knob is the *scrub*
    /// target — deliberately aggressive for dragging, and a number the user
    /// otherwise has to trade off against playback sharpness. Derived, there is no
    /// tradeoff to get wrong: at fit-to-window it is visually lossless and still
    /// ~9× cheaper than full res on 4.6K footage.
    ///
    /// **Sized from `exr_data`, deliberately not from `cs.size`.** Today `cs.size`
    /// would also work: it comes from `logical_size`, which returns the proxy's
    /// recorded `display_size` — the *full-res* display window — so it does not
    /// shrink when a proxy texture is bound. But that is a property of how proxies
    /// carry geometry, not of what `cs.size` means, and anything that ever set it
    /// from real buffer dimensions would create a feedback loop: a smaller bound
    /// texture asking for a smaller next target, every frame, until the picture
    /// collapsed to the floor. `exr_data` is the full-resolution frame by
    /// construction and cannot express that bug, so the target is derived from it
    /// and a test pins the invariant.
    ///
    /// `None` means **decode full, no proxy at all** — either because the view
    /// needs every source pixel (zoomed to 1:1 or past it), or because there is no
    /// sane view transform yet and guessing would be worse than decoding full.
    ///
    /// That distinction matters beyond sharpness. A "proxy" at the source's own
    /// resolution is a decimation by a factor of 1 — a full-size copy — which the
    /// on-disk proxy cache then stores *as a proxy*, at ~83 MB per frame on 4.6K
    /// plate footage. It caches the full frame under a key that only a matching
    /// zoom will ever hit, burning a 50 GB budget in ~600 frames to save nothing.
    /// Above the top decimation band the proxy path must be off, not maximal.
    ///
    /// Callers during playback want [`Self::latched_proxy_target`], not this — see
    /// there for why the live value must not drive decodes mid-run.
    fn viewport_proxy_target(&self, source: crate::layer::SourceId) -> PlaybackProxy {
        let Some(cs) = self.comp_sources.get(&source) else {
            return PlaybackProxy::Unknown;
        };
        let Some((w, h)) = cs.exr_data.logical_size(cs.aov) else {
            return PlaybackProxy::Unknown;
        };
        let long = w.max(h);
        let ppp = self
            .repaint_ctx
            .as_ref()
            .map_or(1.0, eframe::egui::Context::pixels_per_point);
        let scale = self.viewer.scale;
        if !scale.is_finite() || scale <= 0.0 || !ppp.is_finite() || ppp <= 0.0 {
            return PlaybackProxy::Unknown; // no view transform yet — keep the old behaviour
        }
        let needed = (long as f32 * scale * ppp).ceil();
        if !needed.is_finite() || needed <= 0.0 {
            return PlaybackProxy::Unknown;
        }
        // Never above the source (no gain past 1:1), and never below the floor —
        // except that the floor itself cannot exceed the source, or a sub-256px
        // image inverts the bounds and `clamp` panics. That is not hypothetical:
        // it took out seven tests on a 2×2 fixture, and would have been a crash on
        // any small source in the wild.
        let floor = MIN_PROXY_PX.min(long);
        let needed = (needed as usize).clamp(floor, long);

        // Quantize to the decimation the decoder will actually apply.
        // `downsampled_into` reduces by an integer `factor = long.div_ceil(max_dim)`,
        // so a whole band of targets produces a byte-identical decode. Snapping to
        // the canonical target for that band means an ordinary zoom nudge does not
        // move this value at all — which is what makes re-latching cheap enough to
        // invalidate on (see `relatch_proxy_targets`).
        //
        // **Round the factor down, never up.** Rounding up picks *more* decimation
        // than the view asked for, so the proxy is then upscaled onto the screen —
        // and the shortfall is worst exactly where it is most visible. Just under
        // 1:1, `ceil` jumps to factor 2 and delivers **half** the resolution the
        // viewer needs, a 1.8× upscale, precisely when the user has zoomed in to
        // inspect detail. Rounding down guarantees `target >= needed`: always
        // oversampled, never blurred, so "visually lossless at the current view" is
        // a property rather than an approximation. It costs one decimation band
        // (~2304 rather than ~1536 on 4.6K plate at fit — about 22 MB per build,
        // ~1.05 GB/s at 48 builds/s, well inside the ~6 GB/s this path sustains).
        if long <= needed {
            return PlaybackProxy::Full; // factor would be 1: a copy, not a proxy
        }
        let factor = long / needed.max(1); // floor
        if factor <= 1 {
            return PlaybackProxy::Full;
        }
        PlaybackProxy::Px(long.div_ceil(factor).max(1))
    }

    /// `source`'s proxy target for the current play run, **latched** at the moment
    /// playback started (#209).
    ///
    /// The T1 ring is keyed on `(source, frame)` — resolution is not part of the
    /// key. The settings knob knows this and calls `frame_cache.clear()` whenever
    /// `proxy_size` changes, because the cached ring is "the wrong decode mode/size"
    /// afterwards. Deriving the target from zoom made that same value change
    /// *continuously*, with no such invalidation, so frames decoded at different
    /// zoom levels would coexist in the ring and play back as **sharpness flickering
    /// frame to frame**. Uniform softness reads as a proxy; inconsistent sharpness
    /// reads as a broken player.
    ///
    /// Latching per run fixes it by construction rather than by invalidation: the
    /// target cannot change while frames are being decoded into the ring, so the
    /// ring is always internally consistent and no precached range is ever thrown
    /// away for a zoom nudge. Zooming mid-play shows the latched resolution until
    /// you pause — and pausing settles the playhead to full res anyway, so the next
    /// run re-latches to whatever the view has become.
    ///
    /// The latch stores the whole decision, `None` (decode full) included — not
    /// just a size. Recording only sizes would let a source latched at full res
    /// fall through to the live value and start proxying mid-run the moment the
    /// user zoomed out, which is the mismatch this exists to prevent.
    fn latched_proxy_target(&self, source: crate::layer::SourceId) -> PlaybackProxy {
        match self.proxy_target_latch.get(&source) {
            Some(latched) => *latched,
            None => self.viewport_proxy_target(source), // no run in progress
        }
    }

    /// Re-latch every source's proxy target to the current view (#209). Called when
    /// a play run begins, which is the only moment the target may safely move: the
    /// ring is about to be filled, and anything stale in it is superseded.
    ///
    /// If the target genuinely changed — the user zoomed far enough to cross a
    /// decimation band between runs — the cached frames are the wrong resolution
    /// and are dropped, exactly as the `proxy_size` knob does for the same reason.
    /// Because the target is quantized to the decimation factor, this fires on a
    /// real change of detail level and not on every small zoom, so a precached
    /// range survives ordinary framing adjustments.
    fn relatch_proxy_targets(&mut self) {
        let next: std::collections::HashMap<_, _> = self
            .comp_sources
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|s| (s, self.viewport_proxy_target(s)))
            .collect();

        // Only on a change *between* runs. A first play has no previous latch, and
        // whatever precache put in the ring was decoded at this same live target.
        let changed = !self.proxy_target_latch.is_empty() && next != self.proxy_target_latch;
        self.proxy_target_latch = next;
        if changed {
            self.frame_cache.clear();
            // Re-measure: a different proxy size means a different per-frame
            // footprint, and the T1 budget is sized from it.
            self.proxy_bytes = None;
            self.invalidate_inflight();
        }
    }

    /// The proxy `target_blocks` to decode `frame` at (#94/#209), or `None` for a
    /// full/beauty decode. Same cheap-while-moving gate as beauty, behind the
    /// `proxy_enabled` kill-switch; the worker falls back to beauty/full if the
    /// fast proxy read isn't available for the file.
    ///
    /// Scrubbing keeps the fixed `proxy_size` knob — while dragging, latency beats
    /// sharpness and aggressive decimation is the point. Playback and precache use
    /// the viewport-derived target instead.
    fn decode_proxy_target_at(&self, frame: u32, playhead: u32) -> Option<usize> {
        self.decode_proxy_target_at_for(Self::A_SOURCE, frame, playhead)
    }

    /// [`Self::decode_proxy_target_at`] for an explicit source, so a follower sizes
    /// from its own resolution rather than the primary's.
    fn decode_proxy_target_at_for(
        &self,
        source: crate::layer::SourceId,
        frame: u32,
        playhead: u32,
    ) -> Option<usize> {
        if !self.proxy_enabled || !self.wants_cheap_decode_at(source, frame, playhead) {
            return None;
        }
        if self.scrub_active {
            return Some(self.proxy_size);
        }
        match self.latched_proxy_target(source) {
            PlaybackProxy::Px(px) => Some(px),
            // A decision, not an absence: substituting the 256 px scrub knob here
            // would put a deliberately-soft frame on screen at 1:1.
            PlaybackProxy::Full => None,
            PlaybackProxy::Unknown => Some(self.proxy_size),
        }
    }

    fn decode_proxy_target(&self, frame: u32) -> Option<usize> {
        self.decode_proxy_target_at(frame, self.playback.current_frame)
    }

    /// What one **newly decoded** frame will cost, at the fidelity the decode pump
    /// is currently issuing — the divisor the T1 cap is computed from (#230).
    ///
    /// #215's lesson generalised: sizing must be gated on the *same* condition the
    /// decode path uses, or the ring fills with full frames under a cheap-sized cap
    /// and the failure mode flips from wasteful to OOM. So this asks `submit_seq`'s
    /// own predicates rather than restating them. The gate it replaces —
    /// `proxy_enabled && is_active() && viewer.active_layer == 0` — claimed in its
    /// comment to be that same condition, and had silently stopped being it at
    /// #213/#217, which moved the decode path onto `displayed_aov` /
    /// `cheap_decode_layer` (the whole comp stack, not the viewer's layer).
    ///
    /// Probed one frame off the playhead because **prefetch is what fills the
    /// ring**: the cheap gates differ from the settled playhead only by
    /// `frame != playhead` (`wants_cheap_decode_at`), so this answers "what does the
    /// next prefetch cost" — the marginal frame the cap is counting. While playing
    /// or scrubbing the probe is irrelevant (those short-circuit true), so it only
    /// matters for the settled-precache case, where the playhead is deliberately
    /// full and everything around it is not.
    ///
    /// Every fallback chain ends at `frame_bytes`, never at something smaller: a
    /// cheap mode whose bytes have not been measured yet errs conservative — a cap
    /// that is too small, which is merely wasteful — instead of toward OOM.
    fn sizing_frame_bytes(&self) -> Option<usize> {
        // The **clock source**, matching `apply_load_result`'s latch condition
        // (`res.source == self.clock_source()`) exactly. Probing `A_SOURCE` would
        // read a different source's gate than the one whose bytes were measured:
        // under `comp_drives_transport` slot A never decodes at all, and
        // `displayed_aov(A_SOURCE)` then falls through to `viewer.active_layer` —
        // reintroducing the very predicate #213/#217 moved the decode path off.
        // What the pump *asks* for is only a prediction of what lands. When the
        // last decode fell back (#233), size off full frames: the cheap path is
        // failing for this footage, so full frames are what the ring is actually
        // taking. Deliberately one-directional — this can only make the divisor
        // *dearer* and so the cap smaller, which is the safe error (#215's OOM is
        // the other one) — and self-clearing, since the next cheap decode that
        // succeeds resets it.
        //
        // Without this the cap stays sized for 9 MB proxies while 1035 MB full
        // frames land, and `prefetch_depth` — `cap - 1` — sends the scheduler
        // fetching far into a ring that evicts each arrival on contact.
        //
        // **Dearest measured, not `frame_bytes` outright.** A fallback is not
        // always all the way to full: a proxy job whose fast read fails returns a
        // *beauty* frame, which latches `beauty_bytes` and leaves `frame_bytes`
        // `None` if no full decode has happened yet. Returning `frame_bytes`
        // there hands `tick_budgets` a `None`, which skips its entire T1 branch —
        // cap frozen at its constructed 8, no budget check, and the #146 pressure
        // shrink unable to fire. Falling through to the dearest figure that *has*
        // been measured keeps the override's one-directional guarantee (every arm
        // is >= what the requested-mode chains below would return) without the
        // hole.
        if self.decode_fell_back {
            return self.frame_bytes.or(self.beauty_bytes).or(self.proxy_bytes);
        }
        let src = self.clock_source();
        let probe = self.source_playhead(src).wrapping_add(1);
        if self.decode_proxy_target_for(src, probe).is_some() {
            return self.proxy_bytes.or(self.frame_bytes);
        }
        if self.decode_beauty_only_for(src, probe) {
            return self.beauty_bytes.or(self.frame_bytes);
        }
        self.frame_bytes
    }

    /// Per-source counterpart of [`Self::decode_proxy_target`] (#99). The primary
    /// delegates to its canonical (tested) gate.
    fn decode_proxy_target_for(&self, source: crate::layer::SourceId, frame: u32) -> Option<usize> {
        if source == Self::A_SOURCE {
            self.decode_proxy_target(frame)
        } else {
            self.decode_proxy_target_at_for(source, frame, self.source_playhead(source))
        }
    }

    /// Decode-ahead pump (#57): with at most one sequence decode outstanding,
    /// submit the highest-priority frame the scheduler wants — the awaited
    /// playhead first, then prefetch ahead in the play direction. Called after
    /// the playhead moves, after each result lands (the worker just freed up), and
    /// each playing tick. A no-op while a decode is in flight or a non-sequence
    /// load is busy, which is what keeps it to one outstanding job.
    fn pump_decode(&mut self) {
        // One shared worker, one decode at a time across ALL sources (#98/#99):
        // block while the primary or any follower has an outstanding job or awaited
        // playhead.
        if !self.playback.is_active()
            || !self.inflight.is_empty()
            || self.loading_a
            || self
                .followers
                .values()
                .any(|s| !s.inflight.is_empty() || s.loading)
        {
            return;
        }
        // Depth priority:
        // - **Playing** → the sliding prefetch window ahead of the playhead, even
        //   with precache on.
        // - **Idle + precache, not yet filled** (#56, step 4) → the same window, so
        //   the range goes as resident as it fits for instant scrubbing.
        //   Gated on `!precache_filled`: once latched (cache full / nothing more
        //   fits), the window keeps asking for far frames that `evict_to`
        //   immediately drops — and because `apply_load_result` re-pumps after every
        //   result, that decode→evict churn runs forever while idle, independent of
        //   the `tick_precache` latch. A `pending` playhead still gets through at
        //   depth 0 (its P1 slot in `next_want`).
        // - **Idle otherwise** → just the playhead.
        //
        // The two used to take *different* figures — playing the `MAX_PREFETCH`-
        // clamped one, idle the raw budget — which is what #207 is about; they now
        // share [`Self::prefetch_depth`]. The old note here warned that a whole-
        // budget window while playing loop-wraps to the far side and burns the
        // single worker on frames *behind* the playhead.
        //
        // The walk still wraps — with the playhead near the out point and Loop or
        // PingPong on, it must, and should: after the wrap come the frames about to
        // be displayed. What the span bound in `prefetch_depth` removes is the
        // *second* lap. At `depth <= out - in` the walk covers at most every other
        // frame in the range exactly once, so it can never lap around to re-list a
        // frame, and the frames it reaches "behind" the playhead in frame-number
        // terms are ahead of it in play order — fetched last, since `want_list`
        // orders nearest-first.
        let full_depth = if self.playback.is_playing() || (self.precache && !self.precache_filled) {
            self.prefetch_depth()
        } else {
            0
        };
        // With followers also playing, T1 holds one resident window per active
        // source, so split the decode-ahead per source (#98/#99) — otherwise A's
        // window alone would fill the budget and starve them. One follower → `/2`.
        let depth = full_depth / self.n_active_sources();

        // Ordered sources, one job submitted per pump: the primary (A) first, then
        // each active follower (deterministic by `SourceId` — `followers` is a
        // BTreeMap). Two priority passes across all sources: P0 the awaited
        // playheads (depth 0), then P1 the prefetch window (`depth`) — so a lagging
        // follower's playhead still beats the primary's prefetch. `depth 0` asks
        // `next_want` for the playhead only (its P1 slot).
        let sources: Vec<crate::layer::SourceId> = std::iter::once(Self::A_SOURCE)
            .chain(self.active_followers().map(|(id, _)| *id))
            .collect();
        // Rotate the starting point each pump so the P0 turn is shared (#204).
        //
        // A fixed order starves everything after the first source that always wants
        // something. There is one decode in flight globally and the loop returns
        // after submitting one job, so with the clock source first — its playhead
        // advancing every ~42 ms while a decode takes ~270 ms — it wanted a frame on
        // every single pump and consumed every slot. Measured with 5 layers: four of
        // them never reached the P0 pass at all and displayed frame 1 for 30 seconds
        // while their playheads swept the range twice, compositing a temporally
        // wrong image.
        //
        // Rotation makes the starvation impossible rather than merely unlikely: over
        // N pumps every source leads once. Priority *between* the passes is
        // unchanged — every source's playhead (P0) still beats every source's
        // prefetch (P1), so a lagging layer's current frame outranks the clock
        // source's read-ahead.
        let lead = self.pump_rotation % sources.len().max(1);
        self.pump_rotation = self.pump_rotation.wrapping_add(1);
        for d in [0, depth] {
            for i in 0..sources.len() {
                let source = sources[(lead + i) % sources.len()];
                if let Some(w) = self.next_want(source, d) {
                    self.submit_seq(source, w);
                    // Overlap I/O with decode (#164): while the worker decodes `w`,
                    // a background thread pulls the *next* wanted frames' files
                    // through the page cache, so the worker's next read is a
                    // memory-speed pointer walk (the decode maps the file) instead
                    // of a storage stall. Only while a prefetch window is active —
                    // warming on a bare playhead seek would race the user's intent.
                    if depth > 0 {
                        self.warm_ahead(source, w, depth);
                    }
                    return;
                }
            }
        }
    }

    /// Queue the next few non-resident frames after `submitted` for background
    /// file warming (#164) — the same order `next_want` will request them, so the
    /// warmer stays exactly ahead of the decoder. Best-effort and epoch-agnostic: a
    /// superseded warm wastes bandwidth, never correctness. Runs in `source`'s own
    /// frame space (#99).
    fn warm_ahead(&mut self, source: crate::layer::SourceId, submitted: u32, depth: usize) {
        /// Files to keep warm ahead of the decoder. One hides the read of the
        /// very next frame; a second gives slack for a fast decode (proxies)
        /// outpacing a slow warm. More would only evict-race the page cache.
        const WARM_AHEAD: usize = 2;
        let behind = crate::scheduler::read_behind(depth);
        let ahead = depth - behind;
        let mut resident: std::collections::HashSet<u32> =
            self.frame_cache.resident_frames(source).collect();
        resident.insert(submitted);
        let wants = self.want_list_for(source, &resident, ahead, behind);
        for w in wants.into_iter().take(WARM_AHEAD) {
            if let Some(path) = self.frame_path_for(source, w) {
                // Skip the read-ahead warm when the on-disk proxy cache (#165)
                // already holds this frame at the size it would decode: the worker
                // will hit the cache and never open the source, so pulling it
                // through the page cache is wasted bandwidth — exactly the
                // networked-storage cost the cache exists to remove. `contains`
                // self-gates to `false` when the disk cache is off.
                let px = self.decode_proxy_target_for(source, w);
                // Same AOV the decode will ask the cache for (#217) — checking
                // layer 0 while the decode wants layer 2 would skip the warm on a
                // frame that is about to miss and open the source anyway.
                let aov = self
                    .cheap_decode_layer(source)
                    .map_or(0, |(_, logical)| logical);
                if px.is_some_and(|px| self.proxy_cache.contains(&path, px, aov)) {
                    continue;
                }
                self.prefetcher.warm(path);
            }
        }
    }

    /// One source's decode-ahead want-list in its own frame space (#99): the
    /// primary's transport in/out range, else the follower's sequence range; both
    /// share the master direction / loop. Empty for an inactive / unknown follower.
    fn want_list_for(
        &self,
        source: crate::layer::SourceId,
        resident: &std::collections::HashSet<u32>,
        ahead: usize,
        behind: usize,
    ) -> Vec<u32> {
        let (playhead, lo, hi) = if source == Self::A_SOURCE {
            (
                self.playback.current_frame,
                self.playback.in_point,
                self.playback.out_point,
            )
        } else if let Some(st) = self.followers.get(&source) {
            match st.sequence.as_ref() {
                Some(seq) => (st.current_frame, seq.range.0, seq.range.1),
                None => return Vec::new(),
            }
        } else {
            return Vec::new();
        };
        crate::scheduler::want_list(
            playhead,
            lo,
            hi,
            self.playback.direction,
            self.playback.loop_mode,
            resident,
            ahead,
            behind,
        )
    }

    /// The on-disk path of `source`'s frame `w`, or `None` if absent (a hole or an
    /// inactive follower). Primary → the transport's sequence; follower → its own.
    fn frame_path_for(&self, source: crate::layer::SourceId, w: u32) -> Option<PathBuf> {
        if source == Self::A_SOURCE {
            self.playback.frame_path(w).map(Path::to_path_buf)
        } else {
            self.followers
                .get(&source)
                .and_then(|st| st.sequence.as_ref())
                .and_then(|s| s.path_for(w))
                .map(Path::to_path_buf)
        }
    }

    /// Whether `source` has a decodable file for frame `f` — the `next_want`
    /// path-existence predicate, without `frame_path_for`'s allocation in the hot
    /// scheduler closure.
    fn has_source_frame(&self, source: crate::layer::SourceId, f: u32) -> bool {
        if source == Self::A_SOURCE {
            self.playback.frame_path(f).is_some()
        } else {
            self.followers
                .get(&source)
                .and_then(|st| st.sequence.as_ref())
                .is_some_and(|s| s.path_for(f).is_some())
        }
    }

    /// The next frame to fetch for `source` at the given decode-ahead `depth`, or
    /// `None` if its window is fully resident (#98/#99). A frame we are explicitly
    /// `pending` on counts as **not** resident even if a *beauty-only* copy is
    /// cached, so the full all-AOV upgrade on settle still gets submitted
    /// (INV-SAMPLE, #7; `contains` is fidelity-blind). A follower runs the same pure
    /// scheduler in its own frame space (its range as in/out, its playhead), with
    /// the master direction / loop.
    fn next_want(&self, source: crate::layer::SourceId, depth: usize) -> Option<u32> {
        // No primary (slot-A) decode when the comp stack drives the transport
        // (#99 R4-lite): `playback` holds a comp sequence for the clock, but that
        // sequence decodes as its own follower, not as A_SOURCE.
        if source == Self::A_SOURCE && self.comp_drives_transport() {
            return None;
        }
        // Split the window RV-style (#169): ~25% reserved behind the playhead,
        // the rest ahead — so forward window + behind reservation together never
        // over-ask the ring (the #57 back-pressure contract holds for the sum).
        let behind = crate::scheduler::read_behind(depth);
        let ahead = depth - behind;
        let (playhead, lo, hi, pending) = if source == Self::A_SOURCE {
            (
                self.playback.current_frame,
                self.playback.in_point,
                self.playback.out_point,
                self.playback.pending,
            )
        } else {
            let st = self.followers.get(&source)?;
            let seq = st.sequence.as_ref()?;
            (st.current_frame, seq.range.0, seq.range.1, st.pending)
        };
        crate::scheduler::next_want(
            playhead,
            lo,
            hi,
            self.playback.direction,
            self.playback.loop_mode,
            ahead,
            behind,
            |f| self.frame_cache.contains(source, f) && Some(f) != pending,
            |f| self.has_source_frame(source, f),
        )
    }

    /// Submit one sequence decode for `source` at frame `w` (#98/#99). Tags the job
    /// with `source`, marks it in-flight, and drives that source's "loading" state
    /// when it's the awaited playhead.
    fn submit_seq(&mut self, source: crate::layer::SourceId, w: u32) {
        let path = self
            .frame_path_for(source, w)
            .expect("next_want only returns decodable frames");
        // The awaited playhead drives the "loading" state; prefetch is silent.
        if source == Self::A_SOURCE {
            self.inflight.insert(w);
            if Some(w) == self.playback.pending {
                self.loading_a = true;
            }
        } else if let Some(st) = self.followers.get_mut(&source) {
            st.inflight.insert(w);
            if Some(w) == st.pending {
                st.loading = true;
            }
        }
        let beauty_only = self.decode_beauty_only_for(source, w);
        // Scrub proxy takes precedence over beauty while moving (#94); `beauty_only`
        // stays as the worker's fallback if the fast proxy read isn't available.
        let proxy_target = self.decode_proxy_target_for(source, w);
        // Which pass that cheap decode has to carry (#217). Both cheap modes gate on
        // the same `cheap_decode_layer`, so it answers whenever either is on; layer
        // 0 stays `None` so the beauty path is byte-for-byte what it was.
        let aov_layer = (beauty_only || proxy_target.is_some())
            .then(|| self.cheap_decode_layer(source))
            .flatten()
            .filter(|&(_, logical)| logical != 0);
        self.submit_job(LoadJob {
            path,
            source,
            seq_frame: true,
            frame: w,
            epoch: self.playback.epoch,
            // Seq-frames supersede by epoch, not open generation.
            open_gen: 0,
            beauty_only,
            proxy_target,
            aov_layer,
        });
        // Anchor the stall watchdog: one decode is now outstanding.
        self.decode_submit_at = Some(std::time::Instant::now());
    }

    /// Pre-upload T2 GPU textures (#56) for the on-screen frame and the next few
    /// T1-cached frames ahead of the playhead, within the VRAM budget. Builds at
    /// most a couple per call to amortize the upload across UI frames; only
    /// touches frames already resident in T1 (never decodes). UI-thread only.
    fn pump_t2(&mut self) {
        if !self.playback.is_active() || self.viewer.t2_cap(Self::A_SOURCE) == 0 {
            return;
        }
        // Nothing to do when the ring is full and the playhead hasn't moved since
        // the last pump: every slot is already built for this frame. Skips the
        // want-list allocation in the paused / settled case (#142 U4). A playhead
        // move, or a shrunk/evicted ring, drops one of these conditions and pumps.
        if self.viewer.t2_len(Self::A_SOURCE) >= self.viewer.t2_cap(Self::A_SOURCE)
            && self.last_t2_pump == Some(self.playback.current_frame)
        {
            return;
        }
        let Some(gpu) = self.gpu_resources.as_ref() else {
            return;
        };
        let depth = self.viewer.t2_cap(Self::A_SOURCE).saturating_sub(1);
        // Empty resident set -> want_list returns the playhead + the window ahead;
        // we then keep only frames actually cached in T1. No read-behind here
        // (#169): the T2 VRAM ring is tiny (≤ 8) and strictly forward — behind
        // textures would displace the upcoming frames it exists to have ready.
        let wants = crate::scheduler::want_list(
            self.playback.current_frame,
            self.playback.in_point,
            self.playback.out_point,
            self.playback.direction,
            self.playback.loop_mode,
            &std::collections::HashSet::new(),
            depth,
            0,
        );
        self.last_t2_pump = Some(self.playback.current_frame);
        // Budget by time, not a fixed count (#142 U4): one 4K build is 20-60ms on
        // the UI thread, so a flat "2 builds/frame" is a hiccup generator at 4K
        // yet leaves throughput unused at 2K. Always allow the first build (the
        // ring must make progress even when a single build exceeds the slice),
        // then stop once this pump has spent its budget — so 4K does ~1 build per
        // frame and lower resolutions amortize more.
        const PUMP_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);
        let start = std::time::Instant::now();
        let mut built = 0;
        for w in std::iter::once(self.playback.current_frame).chain(wants) {
            if built > 0 && start.elapsed() >= PUMP_BUDGET {
                break;
            }
            if let Some(arc) = self.frame_cache.peek(Self::A_SOURCE, w)
                && self.viewer.prebuild_t2(Self::A_SOURCE, gpu, &arc, w)
            {
                built += 1;
            }
        }
    }

    fn invalidate_inflight(&mut self) {
        self.playback.bump_epoch();
        // Publish the new epoch so the worker skips the jobs this just superseded
        // (a scrub backlog) instead of decoding each one only to drop it.
        self.epoch_signal
            .store(self.playback.epoch, std::sync::atomic::Ordering::Relaxed);
        self.inflight.clear();
        self.loading_a = false;
        self.playback.pending = None;
        // Every follower (#98/#99) is superseded by the same epoch; clear each's
        // seq state too so a dropped decode can't leave a `loading` latched (gating
        // the pump). `sync_comp_followers` re-requests their new frames right after.
        for st in self.followers.values_mut() {
            st.inflight.clear();
            st.pending = None;
            st.loading = false;
        }
        // The playhead/range moved — the precache window shifts, so let it refill.
        self.precache_filled = false;
    }

    /// Step the playhead by `delta` frames, clamped to the in/out range (no
    /// wrap), and pause. Drives the back/forward transport buttons and arrow keys.
    fn playback_step(&mut self, delta: i32) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.pause();
        let (lo, hi) = (self.playback.in_point, self.playback.out_point);
        let next = (i64::from(self.playback.current_frame) + i64::from(delta))
            .clamp(i64::from(lo), i64::from(hi)) as u32;
        // Clamped at a range boundary: a held arrow key would otherwise re-seek
        // the same frame every key-repeat, superseding its own decode (#139).
        // Same narrowing as `playback_scrub_to`: an errored frame (not pending,
        // not resident) still falls through so the step retries it.
        if next == self.playback.current_frame
            && (self.playback.pending == Some(next)
                || self.frame_cache.contains(Self::A_SOURCE, next))
        {
            return;
        }
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
        self.playback.pause();
        let next = frame.clamp(self.playback.in_point, self.playback.out_point);
        // A held-but-stationary drag re-lands on the current frame every UI
        // frame (`dragged()` is true even with zero pointer movement). Re-running
        // the seek would bump the epoch each time, so the held frame's decode is
        // dropped on arrival and resubmitted forever — never displayed or cached
        // until release (#139). A same-frame scrub is a no-op while the frame is
        // in flight or already resident; a frame that is neither (its decode
        // errored — errors clear `pending` without a T1 insert) falls through so
        // the seek doubles as a retry.
        if next == self.playback.current_frame
            && (self.playback.pending == Some(next)
                || self.frame_cache.contains(Self::A_SOURCE, next))
        {
            return;
        }
        self.playback.current_frame = next;
        self.invalidate_inflight(); // a scrub supersedes any in-flight decode
        self.request_sequence_frame(next);
    }

    /// Unattended soak driver (#100): press Play once the stack is ready, then quit
    /// after a fixed wall-clock window. Entirely env-gated — absent
    /// `FLOKI_SOAK_SECS` this is a bool check per frame and nothing else.
    ///
    /// Exists because a soak comparison has to be *repeatable*. Hand-clicking Play
    /// and closing the window varies the run length, the warm-up, and (via the
    /// persisted stack) even the layer count, which is why earlier before/after
    /// numbers on this issue were only ever approximate. With this the whole
    /// measurement is one command.
    ///
    /// `FLOKI_SOAK_SECS=<n>`  run for n seconds after Play, then exit.
    /// `FLOKI_SOAK_WARMUP=<n>` seconds to wait before pressing Play (default 3), so
    ///                         the restored layers finish their first decode and the
    ///                         window isn't measuring the open.
    fn tick_soak_harness(&mut self, ctx: &egui::Context) {
        let Some(secs) = std::env::var("FLOKI_SOAK_SECS")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
        else {
            return;
        };
        let now = std::time::Instant::now();
        let started = *self.soak_started_at.get_or_insert(now);
        let warmup = std::env::var("FLOKI_SOAK_WARMUP")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(3.0);
        let elapsed = now.duration_since(started).as_secs_f32();

        if !self.soak_play_sent && elapsed >= warmup {
            self.soak_play_sent = true;
            log::debug!(target: "floki::playback", "evt=soak_play nsrc={}", self.n_active_sources());
            if self.playback.state != crate::playback::PlayState::Playing {
                self.playback_toggle();
            }
        }
        // Pause before quitting, then hold, so the run actually exercises the
        // settle path (#212) — full-res upgrade of every layer's playhead. Without
        // this the harness only ever measured playback and quit mid-run, which is
        // precisely the half where the "stuck in proxy" bug lived.
        const SETTLE_HOLD: f32 = 4.0;
        if self.soak_play_sent && !self.soak_paused && elapsed >= warmup + secs {
            self.soak_paused = true;
            log::debug!(target: "floki::playback", "evt=soak_pause elapsed={elapsed:.1}");
            if self.playback.state == crate::playback::PlayState::Playing {
                self.playback_toggle();
            }
        }
        if self.soak_paused && elapsed >= warmup + secs + SETTLE_HOLD {
            // Report what each layer ended up displaying. The 1 Hz trace goes quiet
            // once nothing is outstanding, so the *result* of the settle — the
            // moment that matters for #212 — otherwise leaves no record at all.
            let mut ids: Vec<_> = self.comp_sources.keys().copied().collect();
            ids.sort_unstable();
            let final_state = ids
                .iter()
                .map(|id| {
                    let cs = &self.comp_sources[id];
                    format!(
                        "s{}:{}{}",
                        id.0,
                        cs.cur_frame
                            .map_or_else(|| "-".to_string(), |f| f.to_string()),
                        if cs.cur_full { "full" } else { "SOFT" },
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            log::debug!(
                target: "floki::playback",
                "evt=soak_done elapsed={elapsed:.1} settled=[{final_state}]"
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // The soak drives itself, so keep the loop hot rather than waiting on input.
        ctx.request_repaint();
    }

    /// Toggle play/pause. Starting playback anchors the frame clock to now.
    fn playback_toggle(&mut self) {
        use crate::playback::PlayState;
        if !self.playback.is_active() {
            return;
        }
        if self.playback.state == PlayState::Playing {
            self.playback.pause();
            self.settle_to_full();
        } else {
            // Fresh play run → reset the HUD's dropped/held counters (#172), and
            // latch the proxy target to the view as it is now (#209). Play start is
            // the one safe moment to move it: the ring is about to fill, so nothing
            // decoded at the previous target survives to mismatch what follows.
            self.run_dropped = 0;
            self.run_held = 0;
            self.relatch_proxy_targets();
            self.playback.start_playing(std::time::Instant::now());
        }
    }

    /// Contact-sheet thumbnails freeze while the transport is busy (#144),
    /// mirroring the histogram suppression (#141). Otherwise a 40-AOV sheet
    /// re-bakes every layer on every playback frame swap. This folds an active
    /// timeline drag (`scrub_active`) into `sampling_suppressed()`, which already
    /// covers playing and pending; the sheet refreshes once on settle.
    fn thumbs_suppressed(&self) -> bool {
        self.playback.sampling_suppressed() || self.scrub_active
    }

    /// On settling from playback (pause / stop / boundary), ensure the playhead
    /// is resident at **full** resolution so the readout + AOV switcher see every
    /// channel (INV-SAMPLE, #7).
    ///
    /// - Already full-resident → nothing to do; the displayed frame is
    ///   sampling-ready and no decode is scheduled.
    /// - Beauty-only (#56, step 3) or missing → supersede in-flight beauty
    ///   prefetch (so its now-stale result can't clear the awaited upgrade) and
    ///   re-decode the playhead in full. A beauty-only frame stays on screen
    ///   instantly while the full frame decodes; sampling re-enables when it lands.
    ///
    /// The fullness question is asked of the **clock source**, not of `A_SOURCE`
    /// unconditionally (#201). Under `comp_drives_transport` slot A holds no cached
    /// frames at all, so the A-keyed test answered "not full" on every settle and
    /// the comp path took the re-decode branch every time — dumping the entire
    /// in-flight prefetch backlog (`invalidate_inflight` also resets
    /// `precache_filled`) on a pause where the displayed frame was already final,
    /// then re-requesting a slot-A frame that has nothing to decode. Same
    /// `A_SOURCE`-keyed blind spot as #199/#200.
    fn settle_to_full(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        let clock = self.clock_source();
        let frame = self.source_playhead(clock);
        let clock_full = self
            .frame_cache
            .peek(clock, frame)
            .is_some_and(|d| !d.beauty_only && !d.proxy);
        if clock_full {
            // The clock source is full-res and displayed; no decode of its will land
            // to trigger it, so refresh the contact sheet now (frozen during play,
            // #144).
            self.viewer.invalidate_active_thumbnails();
        } else {
            // Supersede in-flight beauty prefetch, then re-request the playhead at
            // full fidelity (`request_*` upgrades a beauty ring hit; #7).
            // Already-full frames need nothing — the slot-B re-sync that used to run
            // in that case went with the locked-step B path (#99 Slice 3h.2).
            //
            // The supersede is **not** slot-A-only: a follower's in-flight cheap
            // prefetch for the settled frame lands on `res.frame ==
            // source_playhead(source)` and clears that follower's `pending`, which
            // is exactly the awaited upgrade `settle_followers_to_full` is about to
            // set — symptom 1 of #201 by another route. The comp path got this for
            // free while it was falling into the A branch on every settle; keep it
            // once the branch is properly gated. The A re-request itself stays
            // gated, since off the A path it decodes nothing.
            self.invalidate_inflight();
            if clock == Self::A_SOURCE {
                self.request_sequence_frame(frame);
            }
        }
        // Always, even when the clock source needed nothing (#212). This used to sit
        // after an `if a_full { return }`, so slot A being full skipped the comp
        // layers entirely — and A is full on the classic transport path, which is
        // exactly where a comp stack can also be present.
        self.settle_followers_to_full();
    }

    /// The comp-path half of [`Self::settle_to_full`] (#212).
    ///
    /// Settling used to consider **only** `A_SOURCE`. With the comp stack driving
    /// the transport that source has no cached frames and no sequence, so the
    /// re-request went to something with nothing to decode and the layers actually
    /// on screen were never asked about at all — they kept whatever proxy frame was
    /// cached, indefinitely. That is the "stuck in proxy mode if I scrub some"
    /// report, and it is the same `A_SOURCE`-keyed blind spot as #199/#200/#201.
    ///
    /// Clock source first: with one decode worker the settles serialize, and the
    /// layer driving the transport is the one the user is looking at.
    fn settle_followers_to_full(&mut self) {
        let clock = self.clock_source();
        let mut wants: Vec<(crate::layer::SourceId, u32)> = self
            .active_followers()
            .map(|(id, st)| (*id, st.current_frame))
            .collect();
        wants.sort_by_key(|(id, _)| (*id != clock, id.0));
        for (source, frame) in wants {
            // Only the ones that need it — an already-full playhead costs nothing
            // and must not be re-decoded on every pause.
            let needs = self
                .frame_cache
                .peek(source, frame)
                .is_none_or(|d| d.proxy || d.beauty_only);
            if needs {
                self.request_comp_frame(source, frame);
            }
        }
    }

    /// Stop playback, halting **in place** — the playhead stays on the frame the
    /// user stopped on (rewind is the dedicated `|<` jump-to-in button). Like a
    /// pause but also resets the pacing clock. Re-decodes the settled frame in
    /// full so the readout + AOV switch see every channel (INV-SAMPLE, #7).
    fn playback_stop(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        self.playback.stop();
        self.settle_to_full();
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
            // The playhead advanced — the precache window shifts ahead of it.
            self.precache_filled = false;
            self.request_sequence_frame(self.playback.current_frame);
            true
        } else {
            false
        }
    }

    /// Ask the UI for another frame, now (#149).
    ///
    /// The immediate half of the seam described on [`Self::request_repaint_after`].
    fn request_repaint(&self) {
        if let Some(ctx) = &self.repaint_ctx {
            ctx.request_repaint();
        }
    }

    /// Ask the UI for another frame after `after` (#149).
    ///
    /// **This and [`Self::request_repaint`] are the playback engine's entire
    /// on-thread dependency on the UI framework.** The engine — `pump_*`, `tick_*`,
    /// `playback_*`, `apply_load_result`, `invalidate_inflight` — is otherwise
    /// egui-free, so these two are the seam the Qt port (#44) re-points: the engine
    /// ships unchanged and only these bodies change. The one other touchpoint is
    /// off-thread, where the decode worker and the LUT/scan loads wake the UI
    /// through a **cloned** `repaint_ctx` (they outlive the borrow, so they cannot
    /// come through here).
    ///
    /// Deliberately reads `repaint_ctx` rather than taking a context parameter.
    /// Threading `&egui::Context` through the engine put the framework in the
    /// *signatures* of functions that never used it for anything else, which made
    /// the boundary look bigger than it is — and forced headless tests to construct
    /// a `Context` purely to satisfy the type.
    ///
    /// A `None` context is a no-op, which is the headless case: the same tolerance
    /// the decode-worker wake already relies on.
    fn request_repaint_after(&self, after: std::time::Duration) {
        if let Some(ctx) = &self.repaint_ctx {
            ctx.request_repaint_after(after);
        }
    }

    /// Drive eager precache (#56, step 4) while idle. `pump_decode` already fills
    /// the whole budget when `precache` is on (playing or not), and chains itself
    /// from `apply_load_result` as each frame lands; this kicks that chain when
    /// the app is otherwise idle (paused) and keeps the frame loop alive until the
    /// resident span covers the budget. A no-op once the range is fully cached.
    fn tick_precache(&mut self) {
        if !self.precache || !self.playback.is_active() || self.precache_filled {
            return;
        }
        self.pump_decode();
        // Latch (stop re-pumping) once the resident span is as full as it fits:
        // either nothing more is wanted, **or** the cache is already at capacity.
        // The capacity check is essential when the in/out range exceeds the RAM
        // budget: there `next_want` always finds a non-resident frame (it loop-
        // wraps to the far side), so `inflight` is never empty and the old
        // nothing-wanted latch never fired — precache churned decode→evict forever.
        if self.inflight.is_empty() || self.cache_is_full() {
            self.precache_filled = true;
        } else {
            self.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    /// Per-frame playback clock. While playing and no decode is in flight, advance
    /// to the next frame once its absolute deadline (`anchor + n·period`) passes —
    /// drift-free pacing. Decode-bound playback (stutter) naturally drops the
    /// effective fps: the next request waits for the previous frame to land.
    fn tick_playback(&mut self) {
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
        // Wake at the next absolute deadline (`anchor + n·period`), not a full
        // period from now: `request_repaint_after(period)` added the wake-up
        // slop (timer/vsync quantization) to every frame, capping effective
        // fps below target no matter how fast decode was (#138). While a
        // decode holds the stutter clock, the worker wake (#137) and the
        // in-flight poll drive repaints instead — schedule the period only as
        // a lazy fallback rather than spinning on the overdue deadline.
        let decode_bound =
            matches!(self.playback.pacing, Pacing::Stutter) && self.transport_awaiting();
        let wait = if decode_bound {
            period
        } else {
            let now = std::time::Instant::now();
            self.playback.anchor.map_or(period, |anchor| {
                (anchor + period * self.playback.frames_since_anchor).saturating_duration_since(now)
            })
        };
        self.request_repaint_after(wait);
    }

    /// Stutter pacing: advance only when the playhead frame is ready (not awaiting
    /// a decode). With decode-ahead the next frame is usually already resident, so
    /// this advances smoothly; when decode falls behind it holds, dropping the
    /// effective fps without skipping frames. A review tool's default.
    fn tick_stutter(&mut self, period: std::time::Duration) {
        if self.transport_awaiting() {
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
                // The lag we're forgiving is the number of frame-periods the
                // display spent behind schedule — count it as held frames (#172).
                let lag = now.duration_since(due).as_nanos() / period.as_nanos().max(1);
                self.run_held = self
                    .run_held
                    .saturating_add(u32::try_from(lag).unwrap_or(u32::MAX));
                self.playback.anchor = Some(now);
                self.playback.frames_since_anchor = 0;
            }
        } else {
            self.playback.pause();
            self.settle_to_full();
        }
    }

    /// Drop-frames pacing: the clock advances on wall-time regardless of decode
    /// readiness, skipping straight to the latest due frame. Intermediate frames
    /// are stepped over with [`Self::step_playhead`] (no decode) and only the
    /// landing frame is requested — so when decode can't keep up you see skipped
    /// frames at a steady wall-clock rate instead of a slowing stutter. A hard
    /// per-tick cap re-anchors after a long stall so the catch-up can't spiral.
    fn tick_drop_frames(&mut self, period: std::time::Duration) {
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
            // Only the landing frame is displayed; the `steps - 1` frames stepped
            // over are skipped/dropped — count them for the HUD (#172).
            self.run_dropped = self.run_dropped.saturating_add(steps.saturating_sub(1));
            // The playhead skipped past any frame we were awaiting, so that
            // in-flight decode is no longer the one to show. Clear the awaited
            // state *before* requesting the new frame: otherwise `loading_a`
            // stays set for a frame we moved past, and once the stale job lands
            // (its frame != current) `apply_load_result` leaves the flag up,
            // permanently gating `pump_decode`. The in-flight job still completes
            // and fills the cache; the pump then resumes for the new playhead.
            // No epoch bump — the stale result is a valid cache fill.
            self.loading_a = false;
            self.playback.pending = None;
            // Show whatever the playhead landed on; if it isn't resident yet the
            // display holds until it decodes while the clock keeps moving.
            self.request_sequence_frame(self.playback.current_frame);
        }
        if hit_boundary {
            self.playback.pause();
            self.settle_to_full();
        }
    }

    /// Render-watch poll (#101). While enabled and a sequence is loaded, re-scan
    /// the directory on a throttled cadence and fold in what changed: new frames
    /// extend the range, re-rendered frames drop from cache and re-decode. Cheap
    /// when nothing changed (one `read_dir` + a stat per frame, per interval).
    fn tick_render_watch(&mut self, ctx: &egui::Context) {
        /// How often the watch re-scans the sequence directory.
        const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

        if !self.watch_enabled || !self.playback.is_active() {
            return;
        }
        // Fold in whatever the scan worker finished since the last frame first
        // (clears `scan_in_flight`), then decide whether to launch another.
        self.drain_scan_results();

        let now = std::time::Instant::now();
        if let Some(last) = self.last_watch_poll {
            let since = now.duration_since(last);
            if since < WATCH_INTERVAL {
                ctx.request_repaint_after(WATCH_INTERVAL - since);
                return;
            }
        }
        // Don't stack a second scan on a slow share; retry on the next cadence
        // tick once the outstanding one lands.
        if self.scan_in_flight {
            ctx.request_repaint_after(WATCH_INTERVAL);
            return;
        }
        self.last_watch_poll = Some(now);
        ctx.request_repaint_after(WATCH_INTERVAL); // keep the cadence ticking
        self.spawn_scan();
    }

    /// Launch the render-watch directory scan on a worker thread (#145): the
    /// `read_dir` + per-frame `stat` blocks (hundreds of ms on a network share)
    /// and must not run in `update()`. The result is delivered over `scan_rx`
    /// and folded in by [`Self::drain_scan_results`] on the UI thread — the diff
    /// and [`Self::apply_scan`] mutate playback state. One scan at a time.
    fn spawn_scan(&mut self) {
        let Some(anchor) = self
            .playback
            .sequence
            .as_ref()
            .and_then(|s| s.frames.first())
            .cloned()
        else {
            return;
        };
        // One persistent channel; the worker clones the sender (mirrors
        // `snapshot_tx`) so a scan still in flight isn't orphaned by a new one.
        if self.scan_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.scan_tx = Some(tx);
            self.scan_rx = Some(rx);
        }
        let tx = self
            .scan_tx
            .clone()
            .expect("scan channel initialized above");
        let repaint_ctx = self.repaint_ctx.clone();
        self.scan_in_flight = true;
        std::thread::spawn(move || {
            // Always deliver exactly one message so the UI can clear
            // `scan_in_flight`. A panic in the scan (a pathological path) must not
            // strand the flag `true` forever — killing render-watch for the
            // session — so it's caught and treated like a failed scan (`None`).
            let msg = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::sequence::scan_group(&anchor).map(|group| {
                    let sigs = crate::sequence::sigs_of(&group);
                    ScanResult {
                        anchor,
                        group,
                        sigs,
                    }
                })
            }))
            .unwrap_or(None);
            let _ = tx.send(msg);
            // Wake the UI to drain the result (mirrors the decode-worker wake).
            if let Some(ctx) = &repaint_ctx {
                ctx.request_repaint();
            }
        });
    }

    /// Fold in any render-watch scan the worker finished (#145), clearing the
    /// in-flight flag. At most one scan is ever outstanding, so this applies the
    /// newest and drops the rest. A failed scan (empty `scan`) keeps the baseline;
    /// a scan whose anchor no longer matches the live sequence — it finished after
    /// the user opened a different one — is discarded.
    fn drain_scan_results(&mut self) {
        let mut latest = None;
        let mut got = false;
        if let Some(rx) = self.scan_rx.as_ref() {
            while let Ok(msg) = rx.try_recv() {
                got = true;
                latest = Some(msg);
            }
        }
        if got {
            self.scan_in_flight = false;
        }
        let Some(msg) = latest else {
            return;
        };
        let Some(res) = msg else {
            return; // scan failed — keep the baseline, no cache drop.
        };
        let anchor_matches = self
            .playback
            .sequence
            .as_ref()
            .and_then(|s| s.frames.first())
            == Some(&res.anchor);
        if anchor_matches {
            self.apply_scan_result(res.group, res.sigs);
        }
    }

    /// Resize the T1 (RAM) and T2 (VRAM) rings to the live resource budgets
    /// (#56). Runs every frame from `ui()` — `ResourceMonitor::sample` is
    /// internally throttled so the cost is a struct copy — and lives here, not
    /// in a draw method: the caps are overcommit protection, and must keep
    /// running through any UI restructure (#150).
    fn tick_budgets(&mut self) {
        let Some(gpu) = &self.gpu_resources else {
            return;
        };
        let sample = self.resource_monitor.sample(&gpu.render_state().device);
        self.dbg_last_sample = Some(sample); // stash for the status bar + debug overlay

        // T1: recomputed each tick; shrinks under other memory pressure. The
        // divisor is what one *new* frame costs at the fidelity the pump is
        // currently issuing ([`Self::sizing_frame_bytes`]); the resident side is
        // the ring's own measured bytes rather than `len * divisor` — with a
        // heterogeneous ring that synthesized figure put the same possibly-wrong
        // scalar on both sides of the budget at once (#230).
        if let Some(bytes) = self.sizing_frame_bytes() {
            // One byte budget, stated in two units (#232). The user's RAM setting
            // is folded in as a ceiling, never an override. Both are floored at two
            // frames' worth so playback still double-buffers when the budget says
            // fewer would fit — the floor is a deliberate override of the budget,
            // so it has to apply in bytes too or the byte bound would undo it.
            let budget = crate::budget::t1_budget_bytes(
                &sample,
                self.frame_cache.bytes(),
                self.ram_budget_bytes(),
            );
            self.frame_cache_budget = budget.max((bytes as u64).saturating_mul(2));
            self.frame_cache_cap = crate::budget::frames_in(self.frame_cache_budget, bytes).max(2);
            // Enforce a shrink now, not on the next decode: eviction otherwise
            // only runs on insert, so with precache latched (nothing in flight)
            // external memory pressure lowered the cap while the ring kept every
            // frame indefinitely — the memory contract's live-pressure
            // degradation never fired (#146).
            if self.frame_cache.len() > self.frame_cache_cap
                || self.frame_cache.bytes() > self.frame_cache_budget
            {
                let loop_wrap = (self.playback.loop_mode == crate::playback::LoopMode::Loop)
                    .then_some((self.playback.in_point, self.playback.out_point));
                let playheads = self.cache_playheads();
                self.dbg_evictions = self.dbg_evictions.saturating_add(self.frame_cache.evict_to(
                    self.cache_bound(),
                    &playheads,
                    Self::A_SOURCE,
                    self.playback.direction,
                    self.playback.is_playing(),
                    loop_wrap,
                    self.read_behind_depth(),
                ) as u64);
            }
        }

        // T2: conservative — capped low, and disabled (→ lazy path) unless at
        // least a couple of frames comfortably fit, since a wgpu OOM aborts
        // the process. Off entirely when the user disables it or no sequence
        // is loaded. In locked-step compare (#166) the VRAM budget is split
        // across the A and B rings so neither can push VRAM over the ceiling.
        const T2_HARD_CAP: usize = 8;
        // One texture per frame maps `available` bytes → a capped, ≥2-or-off count.
        let cap_from = |available: u64, dims: Option<(usize, usize)>| -> usize {
            dims.map_or(0, |(w, h)| {
                let fits = crate::budget::frames_for(available, w, h);
                if fits < 2 { 0 } else { fits.min(T2_HARD_CAP) }
            })
        };
        let t2_on = self.t2_enabled && self.playback.is_active();
        let avail = crate::budget::vram_available(&sample);
        // Split the pool in *bytes* (not frame counts) across the active sources
        // (#99) so each derives its own count from its own resolution when they
        // differ. One active follower (B) → the same A/B halving as before.
        let per_source = avail / self.n_active_sources() as u64;
        let a_avail = per_source;
        let a_dims = t2_on
            .then(|| {
                self.exr_data
                    .as_ref()
                    .and_then(|d| d.logical_size(self.viewer.active_layer))
            })
            .flatten();
        self.viewer
            .set_t2_cap(Self::A_SOURCE, cap_from(a_avail, a_dims));
    }

    /// The ctx-free, synchronous core of the render-watch: re-scan the group,
    /// diff against the baseline, and apply. Returns whether a change was applied
    /// (the first call only baselines). Runs the FS work inline — production goes
    /// through the off-thread [`Self::spawn_scan`] path — so the render-watch
    /// tests exercise the same baseline/diff/apply seam without an egui context,
    /// a worker thread, or wall-clock throttling.
    #[cfg(test)]
    fn rescan_and_apply(&mut self) -> bool {
        // Re-scan from any present frame — the group identity (dir / prefix /
        // suffix / extension) is shared by every member.
        let Some(anchor) = self
            .playback
            .sequence
            .as_ref()
            .and_then(|s| s.frames.first())
            .cloned()
        else {
            return false;
        };
        let Some(group) = crate::sequence::scan_group(&anchor) else {
            return false;
        };
        let sigs_new = crate::sequence::sigs_of(&group);
        self.apply_scan_result(group, sigs_new)
    }

    /// Baseline (first scan) or diff-and-apply a completed scan — the shared tail
    /// of the synchronous [`Self::rescan_and_apply`] (tests) and the async
    /// [`Self::drain_scan_results`]. Returns whether a change was applied.
    fn apply_scan_result(
        &mut self,
        group: std::collections::BTreeMap<u32, std::path::PathBuf>,
        sigs_new: Vec<(u32, crate::sequence::FrameSig)>,
    ) -> bool {
        if self.watch_sigs.is_empty() {
            self.watch_sigs = sigs_new; // first poll: baseline only, nothing to diff.
            return false;
        }
        let diff = crate::sequence::diff_scans(&self.watch_sigs, &sigs_new);
        self.watch_sigs = sigs_new;
        if diff.is_empty() {
            return false;
        }
        self.apply_scan(group, &diff);
        true
    }

    /// Fold a render-watch scan diff into live playback state (#101): drop cache
    /// for frames that changed or vanished, rebuild the sequence range/holes from
    /// the fresh scan, grow the out-point if it tracked the end, and refresh the
    /// displayed frame when it changed (or follow the newest).
    fn apply_scan(
        &mut self,
        group: std::collections::BTreeMap<u32, std::path::PathBuf>,
        diff: &crate::sequence::ScanDiff,
    ) {
        // 1. A re-rendered or removed frame's cached pixels are stale — drop T1+T2.
        for &f in diff.changed.iter().chain(&diff.removed) {
            self.frame_cache.remove(Self::A_SOURCE, f);
            self.viewer.evict_t2_frame(Self::A_SOURCE, f);
            self.inflight.remove(&f);
        }

        // 2. Rebuild the sequence. Keep the in-point; grow the out-point only if it
        //    sat at the old end (an untrimmed tail follows the render) or we're
        //    following. A scan that fell below a sequence (≥2 frames) is ignored.
        let Some(new_seq) = crate::sequence::Sequence::from_group(group) else {
            return;
        };
        let was_full_out = self
            .playback
            .sequence
            .as_ref()
            .is_some_and(|s| self.playback.out_point == s.range.1);
        let new_hi = new_seq.range.1;
        self.playback.sequence = Some(new_seq);
        if was_full_out || self.watch_follow {
            self.playback.out_point = new_hi;
        }
        self.playback.current_frame = self
            .playback
            .current_frame
            .clamp(self.playback.in_point, self.playback.out_point);

        // 3. Follow the newest frame, or refresh the displayed frame if it changed
        //    (added covers a hole the playhead sat on that just filled in).
        let cur = self.playback.current_frame;
        let refresh_current =
            diff.changed.contains(&cur) || diff.added.contains(&cur) || diff.removed.contains(&cur);

        if self.watch_follow {
            let newest = self
                .playback
                .sequence
                .as_ref()
                .and_then(|s| s.numbers().last().copied());
            if let Some(n) = newest {
                let target = n.clamp(self.playback.in_point, self.playback.out_point);
                if target != cur || refresh_current {
                    self.playback.current_frame = target;
                    self.invalidate_inflight();
                    self.request_sequence_frame(target);
                }
            }
        } else if refresh_current {
            self.invalidate_inflight();
            self.request_sequence_frame(cur);
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
                self.dbg_dropped_epoch = self.dbg_dropped_epoch.saturating_add(1);
                return; // a seek/scrub/direction change superseded this decode.
            }
            // Route by source (#99): the primary (A) drives the master transport; a
            // follower decodes under its own `SourceId` into the shared T1 cache.
            let is_primary = res.source == Self::A_SOURCE;
            if is_primary {
                self.inflight.remove(&res.frame);
            } else if let Some(st) = self.followers.get_mut(&res.source) {
                st.inflight.remove(&res.frame);
            }
            // The worker delivered a matching result — record turnaround so the
            // stall watchdog can scale its timeout off real decode cost.
            if let Some(t) = self.decode_submit_at {
                self.last_decode_dur = Some(std::time::Instant::now().duration_since(t));
            }
            match res.result {
                Ok(data) => {
                    let arc = std::sync::Arc::new(data);
                    // Measure one **clock-driving** frame to size the shared cache
                    // budget (homogeneous seq): another source may be a different
                    // resolution (#98), so sizing off an arbitrary one would
                    // mis-size the ring. A full frame seeds `frame_bytes`; a proxy
                    // one seeds `proxy_bytes` (#94).
                    //
                    // Keyed on the clock source rather than the primary (#100/#199):
                    // under `comp_drives_transport` slot A never decodes, so this
                    // never fired, `sizing_bytes` stayed `None`, and `tick_budgets`
                    // skipped its whole T1 branch — leaving `frame_cache_cap` at its
                    // constructed default of 8 with no budget check at all (~3.4 GB
                    // on real footage) and the #146 pressure shrink unable to fire.
                    // With slot A driving, `clock_source()` *is* `A_SOURCE`, so the
                    // A/B behaviour is unchanged.
                    // One branch per fidelity, in `submit_seq`'s own precedence
                    // (proxy over beauty over full) so the latch a decode writes is
                    // the latch `sizing_frame_bytes` will read back for it. The two
                    // used to be a two-way `if`, which left a plain beauty-only frame
                    // matching *neither* arm: it filled the ring and sized nothing
                    // (#230).
                    // A cheap decode the worker had to satisfy with something dearer
                    // (#233). Counted for every source — a frozen follower is worth
                    // seeing — but only the clock source's result steers sizing,
                    // matching which decodes the latches below are measured from.
                    if res.fell_back {
                        self.dbg_fallbacks = self.dbg_fallbacks.saturating_add(1);
                    }
                    if res.seq_frame && res.source == self.clock_source() {
                        self.decode_fell_back = res.fell_back;
                    }
                    if res.source == self.clock_source() {
                        if arc.proxy {
                            // **Most recent, not first.** `get_or_insert` latched
                            // whichever proxy happened to decode first and never
                            // moved, which was fine while scrub and playback shared
                            // one `proxy_size`. Since #209 they don't: playback
                            // derives its target from the viewport while scrubbing
                            // keeps the knob, so a 256 px scrub proxy could size the
                            // whole T1 ring for 9 MB playback proxies — measured
                            // `t1=278/55314`, a cap 34x too large, where eviction by
                            // count can never fire before RAM runs out.
                            self.proxy_bytes = Some(arc.approx_bytes());
                        } else if arc.beauty_only {
                            // Most-recent, not first, for the proxy reason amplified
                            // by #217: a beauty-only decode now carries whichever
                            // single AOV is on screen (`aov_layer` in `submit_seq`),
                            // so its size changes with the displayed pass. A
                            // `get_or_insert` would pin the first AOV's size for the
                            // rest of the session.
                            self.beauty_bytes = Some(arc.approx_bytes());
                        } else {
                            self.frame_bytes.get_or_insert_with(|| arc.approx_bytes());
                        }
                    }
                    self.frame_cache.insert(res.source, res.frame, arc.clone());
                    // A full-res frame landing (the settle upgrade, #94/#56)
                    // replaces a proxy/beauty frame, so its pre-built T2 GPU
                    // texture is now stale — evict it or the viewport keeps binding
                    // the blurry proxy texture ("stuck in proxy"). Primary only for
                    // now (the compare follower's settle-evict is a follow-up);
                    // fires only for full frames (settle), not the proxy/beauty
                    // frames decoded while moving.
                    if is_primary && !arc.beauty_only {
                        self.viewer.evict_t2_frame(Self::A_SOURCE, res.frame);
                    }
                    // In Loop mode eviction distance follows the play direction
                    // around the loop, so prefetch wrapped past the out point
                    // isn't classified "behind" and evicted on arrival (#140).
                    let loop_wrap = (self.playback.loop_mode == crate::playback::LoopMode::Loop)
                        .then_some((self.playback.in_point, self.playback.out_point));
                    // Protect every active source's on-screen frame (#98/#99).
                    let playheads = self.cache_playheads();
                    self.dbg_evictions =
                        self.dbg_evictions.saturating_add(self.frame_cache.evict_to(
                            self.cache_bound(),
                            &playheads,
                            Self::A_SOURCE,
                            self.playback.direction,
                            self.playback.is_playing(),
                            loop_wrap,
                            self.read_behind_depth(),
                        ) as u64);
                    // Show it only if it's the frame that source's playhead awaits;
                    // a prefetched frame ahead of the playhead is just cached.
                    if is_primary {
                        if res.frame == self.playback.current_frame {
                            self.loading_a = false;
                            self.playback.pending = None;
                            self.viewer.set_t2_frame(Self::A_SOURCE, Some(res.frame));
                            self.swap_image_arc(arc);
                            self.playback
                                .note_shown(std::time::Instant::now(), res.frame);
                        }
                    } else if res.frame == self.source_playhead(res.source) {
                        // The awaited follower frame landed (already in the T1 cache,
                        // inserted above). Clear its wait state. A comp follower has
                        // no display slot — `draw_comp_central` binds it from the
                        // cache — so just nudge a repaint so the new frame paints.
                        if let Some(st) = self.followers.get_mut(&res.source) {
                            st.loading = false;
                            st.pending = None;
                        }
                        // Not paced here either: an arriving decode has not been
                        // painted yet, and under load frequently never is — the
                        // playhead has moved on by the time it lands (#204).
                        // `ensure_comp_frame` records the swap that actually shows it.
                        self.request_repaint();
                    }
                    self.error_msg = None;
                }
                Err(e) => {
                    // Clear the awaited state for whichever source's playhead errored.
                    if is_primary {
                        if res.frame == self.playback.current_frame {
                            self.loading_a = false;
                            self.playback.pending = None;
                            self.error_msg = Some(e);
                        }
                    } else if res.frame == self.source_playhead(res.source) {
                        if let Some(st) = self.followers.get_mut(&res.source) {
                            st.loading = false;
                            st.pending = None;
                        }
                        self.error_msg = Some(e);
                    }
                }
            }
            // The worker just freed up — submit the next wanted frame.
            self.pump_decode();
            return;
        }

        // Supersession for an explicit open. The primary slot is
        // **generation**-keyed (#109): `loaded_file` is rewritten to the current
        // frame's path during playback, so a path check could drop a still-current
        // open's result — and dropping it here returns before clearing `loading_a`,
        // permanently gating `pump_decode`. The generation is bumped only by a
        // later open or an unload.
        if res.open_gen != self.open_gen_a {
            return;
        }
        self.loading_a = false;

        match res.result {
            Ok(data) => {
                {
                    // The full decode is this image's first paint — reset the
                    // viewer so it fits the new image.
                    self.exr_data = Some(std::sync::Arc::new(data));
                    self.reset_viewer_session();
                    // If this open started a sequence, seed the T1 ring with the
                    // opened frame so a scrub-back to it is an instant hit (#56).
                    if self.playback.is_active()
                        && let Some(arc) = &self.exr_data
                    {
                        self.frame_bytes.get_or_insert_with(|| arc.approx_bytes());
                        self.frame_cache.insert(
                            Self::A_SOURCE,
                            self.playback.current_frame,
                            arc.clone(),
                        );
                    }
                }
                self.error_msg = None;
            }
            Err(e) => {
                self.exr_data = None;
                self.error_msg = Some(e);
            }
        }
    }

    /// As [`Self::swap_image_data`], but takes an already-`Arc`'d image so a
    /// playback cache hit (#56) can show a resident frame without cloning its
    /// pixel buffers — the same `Arc` is held by the T1 ring and the active slot.
    fn swap_image_arc(&mut self, data: std::sync::Arc<ExrData>) {
        // Same Arc as already displayed (scrub-return, settle onto the shown
        // frame): the pixels are identical, so skip the invalidations — on a T2
        // miss they'd force a full re-pack + re-upload of the same data (#146).
        let same = self
            .exr_data
            .as_ref()
            .is_some_and(|cur| std::sync::Arc::ptr_eq(cur, &data));
        if same {
            self.error_msg = None;
            return;
        }
        {
            // Clamp the active layer to the new image's last valid index. A
            // sequence normally has identical structure frame-to-frame, but guard
            // against a frame with fewer layers so the per-layer texture index
            // stays valid (sync_texture_caches resizes the cache but does not
            // clamp). A true clamp (not reset-to-0) keeps the user's selection
            // when the new image still has that index in range.
            //
            // **Only for a whole-image decode** (#217). A cheap decode's table is a
            // deliberate subset — a per-AOV decode holds exactly one entry — so
            // clamping to it would drag the user's selection to 0 on the first
            // played frame. The gate would then agree the source is "on AOV 0",
            // the fast path would keep working, and the viewer would show the
            // wrong pass with every metric healthy. Nothing about a partial frame
            // is evidence the image lost layers.
            let partial = data.proxy || data.beauty_only || data.only_layer.is_some();
            let layer_count = data.logical_layers.len();
            self.exr_data = Some(data);
            if !partial {
                self.viewer.active_layer =
                    self.viewer.active_layer.min(layer_count.saturating_sub(1));
            }
            // The viewport must rebuild every swap (that's how the next frame
            // paints), but the contact-sheet thumbnails freeze while the transport
            // is busy — otherwise a sheet open over a sequence re-bakes every layer
            // per frame swap (#144). `settle_to_full` / the settle landing refresh
            // them once the playhead stops.
            self.viewer.invalidate_active_viewport();
            if !self.thumbs_suppressed() {
                self.viewer.invalidate_active_thumbnails();
            }
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
    ///
    /// Persisted display prefs (diff controls, custom gradients, background +
    /// presets) are single-owned by the viewer now (#151), so carry them across
    /// the reset — opening a new image must not wipe the user's background or
    /// gradient settings (previously the app re-pushed them each frame).
    fn reset_viewer_session(&mut self) {
        let prefs = std::mem::take(&mut self.viewer.prefs);
        self.viewer = ExrViewer::default();
        self.viewer.prefs = prefs;
    }

    fn tracked_image_bytes(&self) -> u64 {
        // For a sequence the active frame is one of the resident T1 frames (a
        // shared `Arc`), so the cache already accounts for slot A — don't also add
        // `exr_data`, which would double-count it.
        if self.playback.is_active() {
            // The ring's own measurement (#230). This used to be
            // `len * frame_bytes`, which reported every beauty-only or proxy frame
            // as a full one — on a 1 GB/frame render that overstated tracked RAM by
            // an order of magnitude in exactly the mode playback runs in.
            self.frame_cache.bytes()
        } else {
            self.exr_data
                .as_ref()
                .map_or(0, |d| d.approx_bytes() as u64)
        }
    }

    /// Load EXR files dragged onto the window as new layers (#99 R4). While files
    /// are dragged over the window a single "Drop to add layer" overlay is drawn;
    /// on drop every `.exr` is added to the stack via [`Self::open_layer`], in
    /// drop order (up to the layer cap). Non-EXR drops are ignored.
    fn handle_drag_and_drop(&mut self, ctx: &egui::Context) {
        // Hover preview while files are dragged in (before release). Dropping is
        // no longer position-dependent — the whole window is one drop target.
        let hovered = ctx.input(|i| i.raw.hovered_files.len());
        if hovered > 0 {
            let screen = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("dnd_overlay"),
            ));
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(150));
            painter.rect_stroke(
                screen,
                0.0,
                egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(90, 160, 240)),
                egui::StrokeKind::Inside,
            );
            let label = if hovered > 1 {
                format!("Drop to add {hovered} layers")
            } else {
                "Drop to add layer".to_string()
            };
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(28.0),
                egui::Color32::WHITE,
            );
            // Keep repainting so the overlay shows promptly during the drag.
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
        for path in exr_paths {
            self.open_layer(path);
        }
    }
}

/// True if `path` has a (case-insensitive) `.exr` extension.
fn is_exr_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exr"))
}

/// The topmost drawable layer in a resolved composite — the last `Step::Draw`
/// whose source is drawable (per `drawable`) → `(source, aov)`. This is the layer
/// the comp-path pixel readout samples (top-of-stack under the cursor); `None` when
/// nothing drawable is present. Mirrors `draw_comp_central`'s draw-list skip rule,
/// factored out pure (`drawable` is the caller's `comp_sources` texture check).
fn top_sample_source(
    steps: &[crate::layer::Step],
    drawable: impl Fn(crate::layer::SourceId) -> bool,
) -> Option<(crate::layer::SourceId, usize)> {
    steps.iter().rev().find_map(|step| match step {
        crate::layer::Step::Draw(d) if drawable(d.source) => Some((d.source, d.aov)),
        _ => None,
    })
}

/// How many characters of a layer's file name the comp viewport bar's pickers show
/// before eliding. Two of them share that row with every other control.
const COMP_BAR_NAME_CHARS: usize = 24;

/// Shorten a label to at most `max` characters, eliding the middle with `…` (#99
/// Slice 3d follow-up). EXR layer names are full file names — often 50+ characters
/// like `ESTU0001_gloomWatcher_v001.karmarendersettings.1001.exr` — and two of them
/// sit in the comp viewport bar, which pushed the controls to its right off-screen
/// even maximized. Keeping both ends preserves the distinguishing parts (the shot
/// prefix and the version/frame suffix), which a plain truncation would lose. Callers
/// pair it with a hover tooltip carrying the full name. Pure.
fn elide_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 3 {
        return s.to_string();
    }
    // Split the budget around the ellipsis, biasing the tail (version/frame) when odd.
    let keep = max - 1;
    let head = keep / 2 + keep % 2;
    let tail = keep - head;
    let chars: Vec<char> = s.chars().collect();
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[n - tail..]);
    out
}

/// The default compare pane (side B) for a stack whose layers are `ids` **bottom→top**
/// and whose current layer is `current` (#99 Slice 2a). Picks the topmost layer that
/// *isn't* the current one, so the two panes differ without the user choosing anything
/// — side A is the whole composite, so defaulting B to the current layer (which is
/// itself top-of-stack by default) would make the compare look redundant. Falls back to
/// the only layer when there is just one, and `None` for an empty stack. Pure.
fn default_compare_b(
    ids: &[crate::layer::LayerId],
    current: Option<crate::layer::LayerId>,
) -> Option<crate::layer::LayerId> {
    ids.iter()
        .rev()
        .find(|id| Some(**id) != current)
        .or_else(|| ids.last())
        .copied()
}

/// One named layer's draw in a resolved composite — the `Step::Draw` carrying `layer`,
/// if that layer is both present at this frame (`composite_at` drops hidden / soloed-out
/// / trimmed-blank layers) and drawable (per `drawable`). Resolves **both** panes of a
/// comp Side-by-Side (#99 Slice 2a), which shows two individual layers rather than the
/// composite; `None` for either pane means the compare can't be drawn and the caller
/// falls back to `Stacked`. Pure.
fn comp_layer_draw(
    steps: &[crate::layer::Step],
    layer: Option<crate::layer::LayerId>,
    drawable: impl Fn(crate::layer::SourceId) -> bool,
) -> Option<&crate::layer::Draw> {
    let layer = layer?;
    steps.iter().find_map(|step| match step {
        crate::layer::Step::Draw(d) if d.id == layer && drawable(d.source) => Some(d),
        _ => None,
    })
}

/// Map a screen position over the composite `image_rect` to a source pixel of a
/// layer sized `size`. Normalized (fraction across the rect × source size) so a
/// layer whose size differs from the base still maps correctly under the
/// stretch-to-base display. `None` outside `[0, 1)` on either axis, or for a
/// zero-sized / zero-area rect. Pure / testable.
fn comp_hover_pixel(
    pos: egui::Pos2,
    image_rect: egui::Rect,
    size: (usize, usize),
) -> Option<(usize, usize)> {
    let (w, h) = size;
    if w == 0 || h == 0 || image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let u = (pos.x - image_rect.min.x) / image_rect.width();
    let v = (pos.y - image_rect.min.y) / image_rect.height();
    if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
        return None;
    }
    Some(((u * w as f32) as usize, (v * h as f32) as usize))
}

impl eframe::App for ExrApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        // Mirror the viewer-owned display prefs (#151) into the serde bridge so the
        // whole-app serialize below persists them. Done here, at persist time only,
        // instead of the old per-frame round-trip around `viewer.ui`.
        self.persisted_prefs = self.viewer.prefs.clone();
        // Same persist-time bridge for the comp stack (#99 PR-B.5): flatten the live
        // layers + their source paths into the serde-able list.
        self.persisted_layers = self.comp_layers_persist();
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply the persisted theme preference. Idempotent per frame; `System`
        // tracks the OS light/dark setting via egui's input each frame.
        ui.ctx().set_theme(self.theme);

        self.tick_soak_harness(ui.ctx());

        // Load EXR files dragged onto the window (and draw the drag-over overlay).
        self.handle_drag_and_drop(ui.ctx());

        self.poll_async_loads(ui.ctx());

        // Bind any comp textures the upload workers finished (#202). Before the
        // transport ticks and well before the canvas draws, so a frame that landed
        // since the last paint is on screen *this* frame rather than next.
        self.collect_comp_textures();

        // Sequence playback (#7): consume transport keys (Space/←/→) before the
        // viewer sees them, then run the frame clock. Both are no-ops unless a
        // sequence is loaded, so single-image behavior is unchanged.
        self.handle_playback_keys(ui.ctx());
        self.tick_playback();
        // Eager precache (#56, step 4): fill the in/out range while idle when the
        // user has enabled it. No-op unless precache is on and a sequence loaded.
        self.tick_precache();
        // Pick up frames a render writes while we're open (#101); no-op unless the
        // user enabled Watch and a sequence is loaded.
        self.tick_render_watch(ui.ctx());
        // Keep the T1/T2 rings sized to the live RAM/VRAM budgets (#56, #150).
        self.tick_budgets();

        // Snapshot to clipboard (#19): request a framebuffer screenshot on the
        // hotkey and consume the reply when it arrives.
        self.process_snapshot(ui.ctx());

        self.draw_help_window(ui.ctx());
        self.draw_tools_window(ui.ctx());
        self.draw_color_management_window(ui.ctx());
        // Viewer-owned windows, driven from here rather than from `ExrViewer::ui`
        // (#99 Slice 3b): `ui` has been unreachable since the R4 collapse, which left
        // View ▸ "Viewport Background…" setting a flag whose window nothing drew. Both
        // early-return unless their flag is set.
        self.viewer.background_window(ui.ctx());
        self.viewer.gradient_editor_window(ui.ctx());
        self.draw_playback_debug(ui.ctx());
        self.draw_playback_hud(ui.ctx());
        self.draw_menu_bar(ui);
        self.draw_status_bar(ui);
        // The merged bottom timeline panel (#99, Chaos-Player layout): transport
        // controls, the frame ruler, and the layer tracks — one panel so the bars
        // share the ruler's x axis. Added after the status bar, so it sits just
        // above it. A no-op unless a sequence is loaded or the tracks are shown.
        self.draw_timeline_panel(ui);
        self.draw_side_panel(ui);
        self.draw_central_canvas(ui);
        // Pre-upload T2 GPU textures ahead of the playhead (#56). After the canvas
        // so the on-screen frame's texture exists; self-gates when T2 is off.
        self.pump_t2();
    }
}

impl ExrApp {
    fn poll_async_loads(&mut self, ctx: &egui::Context) {
        // Keep the worker's epoch signal current each frame so any queued seq job
        // superseded by a seek/scrub is skipped before its decode (belt-and-braces
        // alongside the immediate publish in `invalidate_inflight`).
        self.epoch_signal
            .store(self.playback.epoch, std::sync::atomic::Ordering::Relaxed);
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
                LoadMsg::Loaded(res) => self.apply_load_result(*res),
            }
        }
        // Recover a permanently stuck playback decode instead of freezing.
        self.tick_decode_watchdog();
        // Once-per-second decode trace for RUST_LOG=floki=debug diagnosis.
        self.trace_playback_state();
        if self.loading_a
            || !self.inflight.is_empty()
            || self
                .followers
                .values()
                .any(|s| s.loading || !s.inflight.is_empty())
        {
            // Fallback only: the worker wakes the UI directly when a result
            // lands (#137). This poll covers a worker that dies mid-decode (the
            // watchdog path) and tests that run without a live worker wake.
            // `inflight` covers silent precache prefetch (#56, step 4), which
            // doesn't set `loading_a` but still has results to drain.
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

    /// Recover from a permanently stuck playback decode. Liveness assumes every
    /// submitted sequence frame's result comes back; if the worker dies or wedges
    /// mid-decode, or a result is otherwise lost, `inflight`/`pending` stay set,
    /// `pump_decode` is gated forever, and playback freezes until the file is
    /// reopened (the reported Windows symptom: frozen image, live UI, unrecovered
    /// by Stop/Play/scrub). This detects "work outstanding but no progress past an
    /// adaptive timeout" and force-recovers: respawn the worker, supersede the
    /// stale state, and re-request the playhead.
    ///
    /// The timeout scales off the last good decode (with a generous floor), so a
    /// slow big-frame decode never trips it — it fires only on a true stall. If a
    /// specific frame keeps wedging the decoder it will re-fire each timeout
    /// (leaking the old wedged thread), but the UI stays responsive and playback
    /// self-heals rather than dying — strictly better than the hard freeze.
    fn tick_decode_watchdog(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        // Only sequence work counts: `loading_a` is also set by a non-seq open,
        // which is not a playback stall. `inflight`/`pending` cover every seq case
        // (awaited playhead and silent precache prefetch).
        let outstanding = !self.inflight.is_empty()
            || self.playback.pending.is_some()
            || self
                .followers
                .values()
                .any(|s| !s.inflight.is_empty() || s.pending.is_some());
        let Some(submitted) = self.decode_submit_at.filter(|_| outstanding) else {
            return;
        };
        // Generous and decode-scaled: only a genuine wedge waits this long.
        const FLOOR: std::time::Duration = std::time::Duration::from_secs(10);
        let timeout = self.last_decode_dur.map_or(FLOOR, |d| (d * 6).max(FLOOR));
        let now = std::time::Instant::now();
        let waited = now.duration_since(submitted);
        if waited < timeout {
            return;
        }
        log::warn!(
            target: "floki::playback",
            "decode stall watchdog: no result for {waited:?} with work outstanding \
             (inflight={:?}, pending={:?}, loading_a={}, epoch={}); respawning worker \
             and re-requesting frame {}",
            self.inflight,
            self.playback.pending,
            self.loading_a,
            self.playback.epoch,
            self.playback.current_frame,
        );
        // The worker may be dead or wedged mid-decode: drop the channels so the
        // next submit spawns a fresh thread. A wedged old thread finishes its
        // decode, fails to send (result_rx dropped), and exits.
        self.load_tx = None;
        self.load_rx = None;
        // Supersede the stuck state (bumps the epoch so any late result is
        // dropped) and re-drive the pump for the current playhead.
        self.invalidate_inflight();
        self.request_sequence_frame(self.playback.current_frame);
        // Reset the stall clock so a recovery that (for any reason) still can't
        // submit waits a full timeout before firing again, rather than spinning
        // every frame — bumping the epoch and dropping the worker on each tick.
        self.decode_submit_at = Some(now);
    }

    /// Throttled (once/second) debug trace of the playback decode state — the
    /// exact fields the stall watchdog acts on. Lets a freeze be diagnosed from
    /// `RUST_LOG=floki=debug` even when the watchdog's own recovery doesn't fire,
    /// which is the case if the stuck state doesn't match its trigger condition.
    /// Zero-cost when debug logging is disabled.
    ///
    /// This is also the #100 soak capture (`run-windows.ps1 soak`), so the format
    /// is stable `key=value` pairs — grep-able and paste-able into an issue — and
    /// it carries the budget / pacing / memory fields the checklist reads, not just
    /// the watchdog's. One line is also emitted on the play→pause/stop transition,
    /// which the `outstanding` early return would otherwise swallow: that is
    /// precisely the moment INV-SAMPLE (#7) is decided.
    fn trace_playback_state(&mut self) {
        use crate::playback::PlayState;
        // Check the target this actually logs to. A bare `log_enabled!` uses
        // `module_path!()` — `floki::app` — so with the soak's narrow
        // `RUST_LOG=floki::playback=debug` the gate evaluated a target that isn't
        // enabled and returned before emitting anything: the capture produced an
        // empty log while playback ran normally.
        if !self.playback.is_active()
            || !log::log_enabled!(target: "floki::playback", log::Level::Debug)
        {
            return;
        }
        let outstanding = !self.inflight.is_empty()
            || self.playback.pending.is_some()
            || self
                .followers
                .values()
                .any(|s| !s.inflight.is_empty() || s.pending.is_some());
        let playing = self.playback.state == PlayState::Playing;
        // The play→settle edge: trace it once even though nothing is outstanding.
        let settled = self.dbg_was_playing && !playing;
        self.dbg_was_playing = playing;
        // Only trace while there is something that *should* be progressing.
        if !playing && !outstanding && !self.loading_a && !settled {
            return;
        }
        let now = std::time::Instant::now();
        // The settle edge bypasses the throttle — it happens once and is the line
        // the INV-SAMPLE checks are read from.
        if !settled
            && self
                .dbg_last_trace
                .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(1))
        {
            return;
        }
        self.dbg_last_trace = Some(now);

        let mut inflight: Vec<u32> = self.inflight.iter().copied().collect();
        inflight.sort_unstable();
        // Followers, in id order, as
        // `s<id>[*]:<playhead>@<displayed>/<pending>/<inflight>` — `*` marks the
        // clock-driving source. The primary's `pending`/`inflight` are empty by
        // construction once the comp stack owns the transport, so this is the field
        // that actually moves during a comp soak (#100).
        //
        // `@<displayed>` is `cs.cur_frame`: the frame whose pixels are on screen.
        // Every other field here is playhead-derived, so without it "playing" and
        // "the clock is advancing over a frozen picture" are indistinguishable in a
        // log — `ensure_comp_frame` holds the last-built texture whenever the wanted
        // frame isn't resident, and that divergence *is* #204.
        let clock = self.clock_source();
        let followers = {
            let mut v: Vec<_> = self.active_followers().collect();
            v.sort_unstable_by_key(|(id, _)| **id);
            v.iter()
                .map(|(id, st)| {
                    format!(
                        "s{}{}:{}@{}/{}/{}",
                        id.0,
                        if **id == clock { "*" } else { "" },
                        st.current_frame,
                        self.comp_sources
                            .get(id)
                            .and_then(|cs| cs.cur_frame)
                            .map_or_else(|| "-".to_string(), |f| f.to_string()),
                        st.pending
                            .map_or_else(|| "-".to_string(), |f| f.to_string()),
                        st.inflight.len()
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        // Active sources painting a frame other than the one their playhead is on —
        // the headline "is the picture keeping up with the clock" number.
        let stale = self
            .active_followers()
            .filter(|(id, st)| {
                self.comp_sources
                    .get(id)
                    .is_some_and(|cs| cs.cur_frame != Some(st.current_frame))
            })
            .count();
        // Active sources whose bound texture came from a proxy/beauty decode (#212)
        // — "the picture is on the right frame but is not the final image". `stale=`
        // cannot see this: a layer stuck on a proxy of the *correct* frame reads as
        // perfectly healthy there, which is exactly how the settle bug survived. On
        // a settled transport this should be 0.
        let soft = self
            .active_followers()
            .filter(|(id, _)| self.comp_sources.get(id).is_some_and(|cs| !cs.cur_full))
            .count();
        // Sequence layers deliberately excluded from every budget and metric above
        // because they're hidden (#211). Reported so "why is nsrc 1 when I have 3
        // layers" has an answer in the log rather than looking like a bug.
        let hidden = self
            .followers
            .iter()
            .filter(|(id, s)| s.sequence.is_some() && !self.source_is_visible(**id))
            .count();
        // `-1` for "no data", not `0`. `frame_time_pcts` deliberately returns `None`
        // until an interval exists, and zeroing that in a stable `key=value` line
        // reads as a perfect 0 ms frame time — the opposite of what it means. It
        // misled me while reading these very logs: `p50=0.0` through a cold pass
        // looks like flawless playback when it means nothing has been displayed yet.
        // `ft_n` gives the sample count alongside.
        // Texture-build cost over the last second (#202), drained here so the phase
        // sums are per-trace-window. These now accumulate on the upload workers, so
        // the total is across the pool and may exceed 1000 ms — it is throughput,
        // not UI-thread occupancy. `texq` is the UI-side number.
        let (texb_n, texb_alloc, texb_pack, texb_write, texb_bind, texb_max, texb_mb) =
            crate::viewer::tex_build_stats::drain();
        let texb_tot = texb_alloc + texb_pack + texb_write + texb_bind;
        // Builds in flight on the upload workers (#202). Bounded by the number of
        // active sources — one per source — so a value pinned at `nsrc` means the
        // workers are the constraint, and a value at 0 means they are keeping up.
        let texq = self.tex_uploader.as_ref().map_or(0, |u| u.inflight_len());

        const NO_DATA: f32 = -1.0;
        let (p50, p95, p99, pmax) = self
            .playback
            .frame_time_pcts()
            .unwrap_or((NO_DATA, NO_DATA, NO_DATA, NO_DATA));
        let (ram_used, ram_total, vram_used, vram_budget) =
            self.dbg_last_sample.map_or((0, 0, 0, 0), |s| {
                (
                    s.sys_used,
                    s.sys_total,
                    s.gpu_used.unwrap_or(0),
                    s.gpu_budget.unwrap_or(0),
                )
            });
        // `secs_f32` rather than Duration's Debug so the numbers are comparable and
        // sortable in a log rather than mixing `ms`/`s`/`µs` units per line.
        let age = |d: Option<std::time::Duration>| d.map_or(-1.0, |d| d.as_secs_f32());

        log::debug!(
            target: "floki::playback",
            "evt={evt} state={state:?} frame={frame} epoch={epoch} pacing={pacing:?} \
             loop={loop_mode:?} dir={dir:?} in={in_pt} out={out_pt} \
             pending={pending} loading_a={loading_a} inflight={inflight:?} \
             clock=s{clock_id} followers=[{followers}] nsrc={nsrc} \
             win={win_full}/{win_src}+{win_behind} \
             stale={stale} soft={soft} hidden={hidden} \
             texb_n={texb_n} texb_tot={texb_tot:.1} texb_alloc={texb_alloc:.1} \
             texb_pack={texb_pack:.1} texb_write={texb_write:.1} texb_bind={texb_bind:.1} \
             texb_max={texb_max:.1} texb_mb={texb_mb:.1} texq={texq} \
             worker={worker} submit_age={submit_age:.2} last_decode={last_decode:.2} \
             precache={precache}/{precache_filled} \
             t1={t1_len}/{t1_cap} frame_bytes={frame_bytes} \
             size_bytes={size_bytes} t1_bytes={t1_bytes}/{t1_budget} \
             t2={t2_len}/{t2_cap} \
             evict={evict} fallback={fallback} drop_epoch={drop_epoch} run_dropped={run_dropped} run_held={run_held} \
             fps={fps:.1}/{fps_target:.0} ft_n={ft_n} \
             p50={p50:.1} p95={p95:.1} p99={p99:.1} ft_max={pmax:.1} \
             ram={ram_used}/{ram_total} vram={vram_used}/{vram_budget}",
            evt = if settled { "settle" } else { "tick" },
            state = self.playback.state,
            frame = self.playback.current_frame,
            epoch = self.playback.epoch,
            pacing = self.playback.pacing,
            loop_mode = self.playback.loop_mode,
            dir = self.playback.direction,
            in_pt = self.playback.in_point,
            out_pt = self.playback.out_point,
            pending = self
                .playback
                .pending
                .map_or_else(|| "-".to_string(), |f| f.to_string()),
            loading_a = self.loading_a,
            clock_id = clock.0,
            nsrc = self.n_active_sources(),
            // The prefetch window, total and per-source (#207). `t1=` alone can't
            // answer whether the window is the constraint — the ring read
            // `121/121` while only ~18 frames were actively wanted — so the figure
            // the scheduler actually uses is logged rather than inferred.
            win_full = self.prefetch_depth(),
            win_src = self.prefetch_depth() / self.n_active_sources(),
            win_behind = self.read_behind_depth(),
            stale = stale,
            soft = soft,
            hidden = hidden,
            worker = if self.load_rx.is_some() { "alive" } else { "dead" },
            submit_age = age(self.decode_submit_at.map(|t| now.duration_since(t))),
            last_decode = age(self.last_decode_dur),
            precache = self.precache,
            precache_filled = self.precache_filled,
            t1_len = self.frame_cache.len(),
            t1_cap = self.frame_cache_cap,
            // `none` is the finding, not a missing field: the T1 cap is only sized
            // once a decode measures a frame (#100).
            frame_bytes = self
                .frame_bytes
                .map_or_else(|| "none".to_string(), |b| b.to_string()),
            // The two figures the cap is *actually* computed from (#230). Without
            // them the trace could not distinguish "the budget is small" from "the
            // budget is being divided by the wrong frame": `frame_bytes` read
            // 1035 MB while the ring held ~80 MB beauty-only frames, and nothing in
            // the line said so. `size_bytes` is the divisor for the fidelity the
            // pump is issuing; `t1_bytes` is what the ring measurably holds — so
            // `t1_bytes / t1` against `size_bytes` is a direct sanity check.
            size_bytes = self
                .sizing_frame_bytes()
                .map_or_else(|| "none".to_string(), |b| b.to_string()),
            t1_bytes = self.frame_cache.bytes(),
            // The bound eviction actually enforces (#232). `t1_bytes` up against
            // this while `t1` sits well under `t1_cap` is the byte bound binding —
            // the case a count-only evictor could not see at all.
            t1_budget = self.frame_cache_budget,
            t2_len = self.viewer.t2_len(Self::A_SOURCE),
            t2_cap = self.viewer.t2_cap(Self::A_SOURCE),
            evict = self.dbg_evictions,
            // Cheap decodes satisfied with something dearer (#233). Climbing while
            // `soft=1` means the fidelity the trace reports asking for is not the
            // fidelity being delivered — the one state this line could not show.
            fallback = self.dbg_fallbacks,
            drop_epoch = self.dbg_dropped_epoch,
            run_dropped = self.run_dropped,
            run_held = self.run_held,
            fps = self.playback.measured_fps,
            fps_target = self.playback.fps_target,
            ft_n = self.playback.frame_time_samples(),
        );
    }

    fn draw_help_window(&mut self, ctx: &egui::Context) {
        if self.show_help {
            egui::Window::new("Help & Shortcuts")
                .open(&mut self.show_help)
                .show(ctx, |ui| {
                    ui.heading("Keyboard Shortcuts");
                    ui.label("R / G / B / A - Isolate specific channel");
                    ui.label("C - Return to full color composite");
                    ui.label("F - Frame image to fit the window");
                    ui.label("T - Toggle the contact sheet for the current layer");
                    ui.label("F11 - Toggle full-screen (ESC or F11 to exit)");
                    ui.label("ESC - Cancel an in-progress annotation, else exit full-screen");
                    ui.label("E - Reset exposure to 0.0");
                    ui.label("Shift+G - Reset gamma to 1.0");
                    ui.label("(or right-click the EV / γ boxes to reset)");

                    ui.add_space(5.0);
                    ui.heading("Mouse Controls");
                    ui.label("Left Click + Drag - Pan image");
                    ui.label("Scroll Wheel - Zoom in and out");
                    ui.label("Shift + Left Click - Sample pixel color and save to swatches");

                    ui.add_space(10.0);
                    ui.heading("Features");
                    ui.label("• Contact Sheet: View ▸ Contact Sheet shows every pass of the current layer as a grid; click one to switch the layer to it.");
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
            let mut ocio_load_requested = false;
            let mut ocio_rebuild_requested = false;
            // Snapshot the enumerations so the combos can read them while the closure holds
            // a mutable borrow of `self` for the selections.
            let ocio_displays = self.ocio_displays.clone();
            let ocio_colorspaces = self.ocio_colorspaces.clone();
            egui::Window::new("Color Management")
                .open(&mut self.show_settings)
                .show(ctx, |ui| {
                    ui.heading("Settings");
                    ui.add_space(5.0);

                    {
                        ui.label(egui::RichText::new("OCIO color config").strong());
                        ui.label(
                            "Leave empty and click Load to use the built-in ACES config \
                             bundled with Floki, or Browse to a .ocio for your own.",
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
                                    Some(v) => format!("Using $OCIO: {v} — no file needed"),
                                    None => "Using built-in ACES (ocio://default) — no file needed"
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
                    // Thumbnails bake the LUT into their cached pixels like
                    // exposure/gamma, so the toggle must invalidate them (#147).
                    if ui
                        .checkbox(&mut self.enable_lut, "Enable Custom LUT")
                        .changed()
                    {
                        self.viewer.invalidate_tone();
                    }
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
                                self.open_layer(path);
                            }
                            ui.close();
                        }
                        ui.menu_button("Open Recent", |ui| {
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
                                    self.open_layer(path);
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("Quit").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });

                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut self.viewer.show_contact_sheet, "Contact Sheet");
                        ui.checkbox(&mut self.show_layers_panel, "Layer tracks")
                            .on_hover_text(
                                "Compositing layer stack as timeline tracks: stack N \
                                 sources as layers with per-layer blend / opacity / \
                                 visibility, and drag a layer's clip to retime it (#99)",
                            );
                        ui.checkbox(&mut self.show_side_panel, "Info panel")
                            .on_hover_text(
                                "Left panel: EXR Info, Color Sampler, and Histogram for \
                                 the current layer",
                            );
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
                        ui.separator();
                        ui.checkbox(&mut self.show_playback_hud, "Playback HUD")
                            .on_hover_text(
                                "Compact on-viewport readout while playing: achieved / \
                                 target fps, dropped/held frames, and cache occupancy (#172)",
                            );
                        ui.checkbox(&mut self.show_playback_debug, "Playback Debug")
                            .on_hover_text(
                                "Live cache / budget / pacing readout for playback soak testing (#100)",
                            );
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
            // Channel isolation now lives in the comp viewport's top control bar
            // (`draw_comp_layer_bar`, #99 R4) and the classic viewer's own control
            // row, so the bottom-bar quick toggle (#192) was retired to avoid a
            // duplicate "Channel:" row.
            if let Some(status) = &self.snapshot_status {
                ui.label(egui::RichText::new(status).weak());
            }

            // Discrete RAM/VRAM readout, right-aligned (#51). The sample is taken
            // (and the T1/T2 budgets recomputed) by `tick_budgets` each frame;
            // request a slow repaint so the numbers keep ticking while the app
            // is otherwise idle.
            if let Some(sample) = self.dbg_last_sample {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
                use crate::resource_monitor::fmt_bytes;
                // floki's tracked image footprint leads the readout: it drops to 0
                // on unload, whereas process RSS lags (the allocator keeps freed
                // pages mapped). #117.
                let mut text = format!(
                    "img {} · RAM {} · sys {}/{}",
                    fmt_bytes(self.tracked_image_bytes()),
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

                // `logical_layer`, not `logical_layers.get` — during playback the
                // live frame may be a per-AOV decode whose own table is renumbered
                // (#217), and a raw index into it names the wrong pass.
                let ll_a = self
                    .exr_data
                    .as_ref()
                    .and_then(|d| d.logical_layer(self.viewer.active_layer));
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

                // Comp-path readout (#99 R4): the topmost layer under the cursor.
                // In comp mode `exr_data` is None, so the row above renders nothing
                // and this is the sole readout row.
                if let Some((src, aov)) = self.comp_readout
                    && let Some(cs) = self.comp_sources.get(&src)
                {
                    let ll = cs.exr_data.logical_layer(aov);
                    let phys_idx = ll.map(|l| l.physical_index).unwrap_or(0);
                    let layer_name = ll.map(|l| l.name.as_str()).unwrap_or("");
                    // Row label = the comp layer's name (which of N is on top).
                    let prefix = self
                        .comp_stack
                        .iter()
                        .find(|l| {
                            matches!(l.source,
                                crate::layer::LayerSource::Image { source, .. } if source == src)
                        })
                        .map(|l| l.name.as_str())
                        .unwrap_or("Layer");
                    draw_nuke_status_line(
                        ui,
                        prefix,
                        Some(cs.exr_data.as_ref()),
                        self.viewer.last_hover_pos_img,
                        self.viewer.last_sampled_val_a,
                        phys_idx,
                        layer_name,
                    );
                }
            });
        });
    }

    /// Compact on-viewport playback HUD (#172): achieved vs target fps (green when
    /// keeping up, amber when lagging), the dropped (DropFrames) or held (Stutter)
    /// frame count for the current play run, and T1 cache occupancy. Toggled from
    /// View ▸ Playback HUD; shown only while a sequence is loaded. A chromeless,
    /// non-interactive window anchored top-right so it never eats canvas clicks.
    fn draw_playback_hud(&mut self, ctx: &egui::Context) {
        if !self.show_playback_hud || !self.playback.is_active() {
            return;
        }
        use crate::playback::Pacing;
        let playing = self.playback.is_playing();
        let (measured, target) = (self.playback.measured_fps, self.playback.fps_target);
        // Skipped frames only accrue in DropFrames; held frames only in Stutter —
        // label by the active pacing so the number always means what it says.
        let (drop_label, drop_n) = match self.playback.pacing {
            Pacing::DropFrames => ("dropped", self.run_dropped),
            Pacing::Stutter => ("held", self.run_held),
        };
        let (t1_len, t1_cap) = (self.frame_cache.len(), self.frame_cache_cap);
        egui::Window::new("playback_hud")
            .title_bar(false)
            .resizable(false)
            .movable(false)
            .interactable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let fps_color = if !playing {
                    ui.visuals().weak_text_color()
                } else if measured >= target * 0.95 {
                    egui::Color32::from_rgb(120, 200, 140)
                } else {
                    egui::Color32::from_rgb(220, 170, 80)
                };
                ui.colored_label(fps_color, format!("{measured:.1} / {target:.0} fps"));
                ui.label(format!("{drop_label} {drop_n}"));
                ui.label(format!("cache {t1_len}/{t1_cap} frames"));
            });
    }

    /// Live playback debug overlay (#100): cache / budget / pacing readout for
    /// real-footage soak testing. Toggled from View ▸ Playback Debug. Read-only;
    /// values are snapshotted into locals first so the `Window::open` borrow of
    /// `show_playback_debug` doesn't clash with reading the rest of `self`.
    fn draw_playback_debug(&mut self, ctx: &egui::Context) {
        if !self.show_playback_debug {
            return;
        }
        use crate::resource_monitor::fmt_bytes;

        let pb = &self.playback;
        let active = pb.is_active();
        let full_range = pb.full_range();
        let (in_pt, out_pt, frame, epoch) = (pb.in_point, pb.out_point, pb.current_frame, pb.epoch);
        let state = format!("{:?}", pb.state);
        let dir = format!("{:?}", pb.direction);
        let loop_mode = format!("{:?}", pb.loop_mode);
        let pacing = format!("{:?}", pb.pacing);
        let (fps_target, fps_measured) = (pb.fps_target, pb.measured_fps);
        let pending = pb.pending;

        let pcts = pb.frame_time_pcts();
        let pct_n = pb.frame_time_samples();

        let t1_len = self.frame_cache.len();
        let t1_cap = self.frame_cache_cap;
        let t2_len = self.viewer.t2_len(Self::A_SOURCE);
        let t2_cap = self.viewer.t2_cap(Self::A_SOURCE);
        // The divisor the cap is actually computed from, and what the ring measurably
        // holds (#230) — not the raw `frame_bytes` latch, which is only one of three
        // fidelities and is the wrong one during beauty/proxy playback.
        let sizing_bytes = self.sizing_frame_bytes();
        let t1_bytes = self.frame_cache.bytes();
        let t1_budget = self.frame_cache_budget;
        let (dbg_fallbacks, decode_fell_back) = (self.dbg_fallbacks, self.decode_fell_back);
        // The comp path holds one texture per source, rebuilt on the UI thread by
        // `ensure_comp_frame` — with T2 structurally off there (every ring call site
        // passes `A_SOURCE`), this is the VRAM the player actually occupies (#100).
        let comp_tex: Vec<(u64, (usize, usize))> = {
            let mut v: Vec<_> = self
                .comp_sources
                .iter()
                .filter(|(_, cs)| cs.texture.is_some())
                .map(|(id, cs)| (id.0, cs.size))
                .collect();
            v.sort_unstable();
            v
        };
        // Per-follower transport state. The `worker` row below reads `self.inflight`
        // and `loading_a`, which are permanently empty once the comp stack drives
        // the transport — this row is what's actually moving.
        let clock_src = self.clock_source().0;
        let n_sources = self.n_active_sources();
        let follower_state: Vec<DbgFollowerRow> = {
            let mut v: Vec<_> = self
                .active_followers()
                .map(|(id, st)| DbgFollowerRow {
                    id: id.0,
                    playhead: st.current_frame,
                    displayed: self.comp_sources.get(id).and_then(|cs| cs.cur_frame),
                    pending: st.pending,
                    inflight: st.inflight.len(),
                })
                .collect();
            v.sort_unstable();
            v
        };
        let proxy_state = (self.proxy_enabled, self.proxy_size, self.proxy_bytes);
        let mut inflight: Vec<u32> = self.inflight.iter().copied().collect();
        inflight.sort_unstable();
        let sample = self.dbg_last_sample;
        let (evictions, dropped) = (self.dbg_evictions, self.dbg_dropped_epoch);
        let loading_a = self.loading_a;
        let worker_alive = self.load_rx.is_some();
        let since_submit = self
            .decode_submit_at
            .map(|t| std::time::Instant::now().duration_since(t));
        let last_decode = self.last_decode_dur;
        let thumb_bakes = self.viewer.dbg_thumb_bakes;

        let mut open = true;
        egui::Window::new("Playback debug")
            .open(&mut open)
            .resizable(false)
            .default_width(260.0)
            .show(ctx, |ui| {
                if !active {
                    ui.label("No sequence loaded.");
                    return;
                }
                egui::Grid::new("pb_dbg_grid")
                    .num_columns(2)
                    .spacing([12.0, 2.0])
                    .show(ui, |ui| {
                        let (lo, hi) = full_range.unwrap_or((0, 0));
                        ui.label("range");
                        ui.label(format!("{lo}–{hi}  (in/out {in_pt}–{out_pt})"));
                        ui.end_row();

                        ui.label("frame");
                        ui.label(format!("{frame}  ·  {state}  ·  {dir}"));
                        ui.end_row();

                        ui.label("mode");
                        ui.label(format!("{loop_mode}  ·  {pacing}  ·  epoch {epoch}"));
                        ui.end_row();

                        ui.label("fps");
                        ui.label(format!("{fps_measured:.1} / {fps_target:.0} target"));
                        ui.end_row();

                        // The EWMA above has a ~5-frame time constant, so a single
                        // long hitch is smeared into a dip that can't be quantified.
                        // The tail is the actual "stutter vs drop-frames" evidence
                        // (#100); `n` is shown so a p99 over three samples reads as
                        // the non-answer it is.
                        ui.label("frame time");
                        ui.label(match pcts {
                            Some((p50, p95, p99, max)) => format!(
                                "p50 {p50:.0}  p95 {p95:.0}  p99 {p99:.0}  max {max:.0} ms  (n={pct_n})"
                            ),
                            None => "— (no frames shown yet)".to_string(),
                        });
                        ui.end_row();

                        ui.label("T1 (CPU)");
                        // Sizing provenance, not just occupancy: the sizing latches
                        // are seeded only from a *primary* decode, so with the comp
                        // stack driving the transport none is ever measured and the
                        // cap sits frozen at its constructed default (#100). Say so
                        // in the row rather than presenting 8/8 as a live budget.
                        //
                        // Held vs. sizing are shown side by side because they answer
                        // different questions and used to be the same number (#230):
                        // a held/frame far below the sizing figure is the ring being
                        // budgeted for frames it isn't holding.
                        let t1_sizing = sizing_bytes.map_or_else(
                            || "cap frozen (no sizing decode)".to_string(),
                            |b| format!("sizing ~{}/frame", fmt_bytes(b as u64)),
                        );
                        // Held is shown against the byte budget, not bare (#232):
                        // that pair is the bound eviction enforces, and the ring can
                        // be up against it while the frame count still looks roomy.
                        let t1_held = if t1_len == 0 {
                            "held —".to_string()
                        } else {
                            format!(
                                "held {} / {} (~{}/frame)",
                                fmt_bytes(t1_bytes),
                                fmt_bytes(t1_budget),
                                fmt_bytes(t1_bytes / t1_len as u64)
                            )
                        };
                        ui.label(format!(
                            "{t1_len} / {t1_cap} frames  ·  {t1_sizing}  ·  {t1_held}"
                        ));
                        ui.end_row();

                        // Scrub proxy (#94): whether it's on, the size knob, and the
                        // measured per-proxy bytes once one has decoded (proves
                        // proxies are actually being produced for this footage).
                        ui.label("proxy");
                        let (px_on, px_size, px_bytes) = proxy_state;
                        let px = if px_on {
                            match px_bytes {
                                Some(b) => format!("on · {px_size} px · ~{}/frame", fmt_bytes(b as u64)),
                                None => format!("on · {px_size} px · (full — none produced)"),
                            }
                        } else {
                            "off".to_string()
                        };
                        ui.label(px);
                        ui.end_row();

                        // Cheap decodes the worker had to satisfy with something
                        // dearer (#233). The fallback is correct — slow beats a
                        // frozen layer (#213) — but it was invisible, and it is the
                        // difference between "this footage plays cheap" and "every
                        // cheap decode is failing and we are quietly decoding full".
                        ui.label("fallbacks");
                        ui.label(if dbg_fallbacks == 0 {
                            "none — decodes are the fidelity asked for".to_string()
                        } else {
                            format!(
                                "{dbg_fallbacks} · sizing off {}",
                                if decode_fell_back {
                                    "full frames (last decode fell back)"
                                } else {
                                    "the requested fidelity (last decode was clean)"
                                }
                            )
                        });
                        ui.end_row();

                        // Labelled A-only because it *is* A-only: every ring call
                        // site passes `A_SOURCE`, so in the comp path this reads
                        // `off` by construction and is not the VRAM instrument.
                        // `comp tex` below is (#100).
                        ui.label("T2 (GPU, A only)");
                        let t2 = if t2_cap == 0 {
                            "off".to_string()
                        } else {
                            format!("{t2_len} / {t2_cap} frames")
                        };
                        ui.label(t2);
                        ui.end_row();

                        ui.label("comp tex");
                        ui.label(if comp_tex.is_empty() {
                            "—".to_string()
                        } else {
                            let dims = comp_tex
                                .iter()
                                .map(|(id, (w, h))| format!("s{id} {w}×{h}"))
                                .collect::<Vec<_>>()
                                .join("  ·  ");
                            format!("{} live  ·  {dims}", comp_tex.len())
                        });
                        ui.end_row();

                        ui.label("sources");
                        ui.label(if follower_state.is_empty() {
                            format!("{n_sources} active  ·  no followers")
                        } else {
                            let each = follower_state
                                .iter()
                                .map(|r| {
                                    let (id, cur, nfl) = (r.id, r.playhead, r.inflight);
                                    let clock = if id == clock_src { "*" } else { "" };
                                    let p =
                                        r.pending.map_or_else(|| "—".to_string(), |f| f.to_string());
                                    // `f<playhead>→<displayed>`; the arrow only shows
                                    // when they differ, so a healthy row stays quiet
                                    // and a frozen layer is obvious at a glance.
                                    let d = match r.displayed {
                                        Some(s) if s != cur => format!("→{s} STALE"),
                                        Some(_) => String::new(),
                                        None => "→— none".to_string(),
                                    };
                                    format!("s{id}{clock} f{cur}{d} pend {p} fly {nfl}")
                                })
                                .collect::<Vec<_>>()
                                .join("  ·  ");
                            format!("{n_sources} active  ·  {each}")
                        });
                        ui.end_row();

                        // Slot-A only, like the fields it reads — empty by
                        // construction in the comp path; `sources` above is the
                        // live one there.
                        ui.label("worker (A)");
                        let pend = pending.map_or_else(|| "—".to_string(), |f| f.to_string());
                        ui.label(format!(
                            "in-flight {inflight:?}  ·  pending {pend}  ·  loading_a {loading_a}  ·  {}",
                            if worker_alive { "alive" } else { "dead" }
                        ));
                        ui.end_row();

                        ui.label("decode");
                        let submit = since_submit.map_or_else(
                            || "idle".to_string(),
                            |d| format!("{:.1}s ago", d.as_secs_f32()),
                        );
                        let last = last_decode
                            .map_or_else(|| "—".to_string(), |d| format!("{:.2}s", d.as_secs_f32()));
                        ui.label(format!("submitted {submit}  ·  last {last}"));
                        ui.end_row();

                        if let Some(s) = sample {
                            ui.label("RAM");
                            ui.label(format!(
                                "{} / {}",
                                fmt_bytes(s.sys_used),
                                fmt_bytes(s.sys_total)
                            ));
                            ui.end_row();

                            ui.label("VRAM");
                            let vram = match (s.gpu_used, s.gpu_budget) {
                                (Some(u), Some(b)) => {
                                    format!("{} / {}", fmt_bytes(u), fmt_bytes(b))
                                }
                                _ => "n/a (off-Metal fixed cap)".to_string(),
                            };
                            ui.label(vram);
                            ui.end_row();
                        }

                        ui.label("evictions");
                        ui.label(evictions.to_string());
                        ui.end_row();

                        ui.label("dropped-epoch");
                        ui.label(dropped.to_string());
                        ui.end_row();

                        // Flat while the sheet is frozen during playback; steps up
                        // once per settle. Climbing per frame ⇒ #144 has regressed.
                        ui.label("sheet bakes");
                        ui.label(thumb_bakes.to_string());
                        ui.end_row();
                    });
            });
        self.show_playback_debug = open;
    }

    /// The merged bottom **timeline panel** (#99, Chaos-Player layout): transport
    /// controls, the frame ruler, and one track per composite layer — all sharing a
    /// single x axis, so a layer's clip bar lines up with the frame numbers above
    /// it. Replaces the separate transport and Layers panels (aligning bars to a
    /// ruler in a *different* panel would mean matching gutter widths by hand).
    ///
    /// Each section self-gates: the controls and ruler need an active transport,
    /// the tracks need the Layers toggle. With neither, the panel doesn't show.
    fn draw_timeline_panel(&mut self, ui: &mut egui::Ui) {
        let transport = self.playback.is_active();
        if self.viewer.fullscreen || (!transport && !self.show_layers_panel) {
            return;
        }
        egui::Panel::bottom("timeline_panel")
            .resizable(true)
            .show_inside(ui, |ui| {
                // One axis width for every row in the panel, measured before any
                // row is added (`available_width` is horizontal, so it's stable
                // down the vertical layout).
                let axis_w = (ui.available_width() - TIMELINE_GUTTER_W).max(64.0);
                // The visible frame range comes from the driving sequence. `None`
                // (stills-only stack, no transport) ⇒ no axis: tracks render
                // gutter-only, as the old flat list did.
                let range = self.playback.full_range();

                if transport {
                    self.draw_transport_controls(ui);
                    self.draw_ruler_row(ui, axis_w, range);
                }
                if self.show_layers_panel {
                    self.draw_layer_tracks(ui, axis_w, range);
                }
            });
    }

    /// The frame-ruler row of the timeline panel: the frame readout in the gutter,
    /// the scrubbable timeline over the shared axis.
    fn draw_ruler_row(&mut self, ui: &mut egui::Ui, axis_w: f32, range: Option<(u32, u32)>) {
        let (gutter, axis_rect) = alloc_timeline_row(ui, axis_w, TIMELINE_ROW_H);
        let cur = self.playback.current_frame;
        let (in_pt, out_pt) = (self.playback.in_point, self.playback.out_point);
        // A hole holds the previous frame; flag it so the readout isn't mistaken
        // for a decoded frame.
        let hole = self.playback.frame_path(cur).is_none();
        let mut g = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(gutter)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        g.set_clip_rect(gutter);
        g.label(format!("{cur}  [{in_pt}–{out_pt}]"));
        if hole {
            g.label(egui::RichText::new("(hole)").weak());
        }
        if let Some((lo, hi)) = range {
            self.draw_timeline(ui, axis_rect, lo, hi);
        }
    }

    /// Transport controls for image-sequence playback (#7): play/pause/stop/step/
    /// jump + reverse + loop-mode + pacing + in/out trim + editable target fps and
    /// measured fps + the decode/cache toggles. The top row of the timeline panel
    /// (#99); the caller gates on an active transport.
    fn draw_transport_controls(&mut self, ui: &mut egui::Ui) {
        use crate::playback::{Direction, LoopMode, Pacing};
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
            if ui
                .button("Stop")
                .on_hover_text("Stop (halt in place; |< rewinds to in)")
                .clicked()
            {
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
                self.viewer.clear_t2(Self::A_SOURCE);
            }

            ui.separator();

            // Beauty-only playback decode kill-switch (#56, step 3). On → the
            // ring decodes just the beauty/first layer while moving (faster
            // decode, smaller frames for multi-part AOV EXRs); the settled
            // frame is always re-decoded in full. Off → always decode all AOVs.
            if ui
                .checkbox(&mut self.beauty_preview, "Beauty preview")
                .on_hover_text(
                    "While playing, decode only the beauty/first layer for a \
                         faster, lighter cache; the paused frame is always full. \
                         Turn off if a file's first layer isn't its beauty.",
                )
                .changed()
            {
                // Switching modes makes the cached ring (now the wrong decode
                // mode) stale; drop it so frames re-decode under the new mode.
                self.frame_cache.clear();
                self.invalidate_inflight();
                self.request_sequence_frame(self.playback.current_frame);
            }

            ui.separator();

            // Scrub-proxy kill-switch + size (#94). On → while moving, decode a
            // tiny downsampled proxy so heavy footage plays and far more frames
            // fit RAM; the paused frame is always re-decoded full-res. Self-gates
            // to a full decode on small/tiled/deep files.
            let proxy_changed = ui
                .checkbox(&mut self.proxy_enabled, "Scrub proxy")
                .on_hover_text(
                    "While playing/scrubbing, decode a small downsampled proxy so \
                         heavy footage plays smoothly and far more frames fit in RAM; \
                         the paused frame sharpens to full res. Great for slow or \
                         networked media.",
                )
                .changed();
            let size_changed = self.proxy_enabled
                && ui
                    .add(
                        egui::DragValue::new(&mut self.proxy_size)
                            .range(256..=4096)
                            .speed(16.0)
                            .suffix(" px"),
                    )
                    .on_hover_text(
                        "Proxy size — the downsampled long-side resolution: higher \
                             = sharper but larger + fewer frames cached. The frame is \
                             decoded full (correct geometry) then box-filtered to this \
                             size on the way into the cache.",
                    )
                    .changed();
            if proxy_changed || size_changed {
                // The cached ring is now the wrong decode mode/size — drop it so
                // frames re-decode (mirrors the Beauty-preview toggle). Also
                // forget the measured proxy byte-size so a new size re-measures
                // it (otherwise a larger proxy is budgeted at the old, smaller
                // size until restart).
                self.frame_cache.clear();
                self.proxy_bytes = None;
                self.invalidate_inflight();
                self.request_sequence_frame(self.playback.current_frame);
            }

            ui.separator();

            // On-disk proxy cache (#165). On → persist scrub proxies to
            // ~/.floki/proxy-cache so a repeat pass / later session loads them
            // instead of re-decoding; LRU-bounded by the GB budget beside it.
            let disk_changed = ui
                .checkbox(&mut self.proxy_disk_cache, "Disk cache")
                .on_hover_text(
                    "Persist scrub proxies to disk (~/.floki/proxy-cache) so a \
                         repeat pass or a later session loads them instantly instead \
                         of re-decoding — auto-invalidated when a frame is re-rendered. \
                         Huge for networked media and repeated review (dailies, shot \
                         iteration).",
                )
                .changed();
            let budget_changed = self.proxy_disk_cache
                && ui
                    .add(
                        egui::DragValue::new(&mut self.proxy_cache_gb)
                            .range(1.0..=200.0)
                            .speed(1.0)
                            .suffix(" GB"),
                    )
                    .on_hover_text(
                        "On-disk proxy-cache size budget in GB (gibibytes, 1024³ — \
                             same unit as the RAM budget). A ceiling, not a reservation: \
                             the least-recently-used proxies are evicted first once it \
                             fills.",
                    )
                    .changed();
            if disk_changed || budget_changed {
                self.proxy_cache.configure(
                    self.proxy_disk_cache,
                    crate::proxy_cache::gib_to_bytes(self.proxy_cache_gb),
                );
            }
            if self.proxy_disk_cache
                && ui
                    .button("Clear")
                    .on_hover_text("Delete all cached proxies from ~/.floki/proxy-cache.")
                    .clicked()
            {
                self.proxy_cache.clear();
            }

            ui.separator();

            // Eager precache kill-switch (#56, step 4). On → fill the whole
            // in/out range into the ring up front (bounded by RAM); the
            // cache-fill bar under the scrubber shows how much is resident. On
            // by default (#165): scrub proxies keep the footprint small and the
            // disk cache makes a repeat fill cheap, so warming the range is cheap.
            if ui
                .checkbox(&mut self.precache, "Precache")
                .on_hover_text(
                    "Cache the whole in/out range up front so it plays and \
                         loops with no decoding. Fills to the RAM budget; the bar \
                         under the scrubber shows the cached span. Cheap with scrub \
                         proxies + the disk cache on, so it's on by default.",
                )
                .changed()
                && self.precache
            {
                // Kick the fill immediately; the chain self-sustains from
                // `apply_load_result` as each frame lands.
                self.precache_filled = false;
                self.pump_decode();
                ui.ctx().request_repaint();
            }

            ui.separator();

            // User-assigned RAM budget for the T1 ring (#56). 0 = Auto (size
            // from free RAM). A ceiling only — capped by the auto figure so it
            // can't OOM; lets you bound RAM on a shared box or dogfood eviction.
            ui.label("RAM");
            ui.add(
                egui::DragValue::new(&mut self.ram_budget_gb)
                    .range(0.0..=256.0)
                    .speed(0.25)
                    .custom_formatter(|n, _| {
                        if n < f64::from(RAM_BUDGET_AUTO_BELOW_GB) {
                            "Auto".to_string()
                        } else {
                            format!("{n:.1} GB")
                        }
                    }),
            )
            .on_hover_text(
                "Cap the RAM the frame cache may use (0 = Auto, sized from free \
                     RAM). A ceiling only — it can't exceed what's free. Lower it to \
                     bound RAM on a shared box, or to dogfood the eviction paths.",
            );

            ui.separator();

            // Render-watch (#101): pick up frames as a render writes them.
            if ui
                .checkbox(&mut self.watch_enabled, "Watch")
                .on_hover_text(
                    "Watch the sequence folder and load frames as a render \
                         writes them (new frames extend the range; re-rendered \
                         frames refresh).",
                )
                .changed()
            {
                // (Re)baseline on the next poll so existing frames aren't
                // mistaken for newly-arrived ones.
                self.watch_sigs.clear();
                self.last_watch_poll = None;
            }
            if self.watch_enabled {
                ui.checkbox(&mut self.watch_follow, "Follow")
                    .on_hover_text("Park the playhead on the newest frame as it arrives.");
            }
        });
    }

    /// Draw the playback timeline over the full sequence span: the trimmed
    /// `[in, out]` region is highlighted, holes are marked distinctly, the in/out
    /// edges and playhead are drawn as vertical ticks. Click or drag scrubs to the
    /// frame under the cursor (a P0 seek, clamped to the trim).
    ///
    /// `rect` is the row's axis area, already allocated by the caller (#99) so the
    /// ruler and every layer clip bar below it share one frame↔x mapping.
    fn draw_timeline(&mut self, ui: &mut egui::Ui, rect: egui::Rect, lo: u32, hi: u32) {
        let axis = TimeAxis::new(rect, lo, hi);
        let (in_pt, out_pt) = (self.playback.in_point, self.playback.out_point);
        let cur = self.playback.current_frame;

        let resp = ui.interact(
            rect,
            ui.id().with("timeline_ruler"),
            egui::Sense::click_and_drag(),
        );
        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            let visuals = ui.visuals();
            let x_of = |f: u32| axis.x_of(i64::from(f));

            // Track background.
            painter.rect_filled(rect, 3.0, visuals.extreme_bg_color);
            // Trimmed [in, out] region.
            let trim = egui::Rect::from_min_max(
                egui::pos2(x_of(in_pt), rect.top()),
                egui::pos2(x_of(out_pt), rect.bottom()),
            );
            painter.rect_filled(trim, 3.0, visuals.selection.bg_fill.gamma_multiply(0.4));
            // Cache-fill bar (#56, step 4): a thin green strip along the bottom of
            // the track marking which frames are resident in the T1 ring. Each
            // resident frame fills its own equal slot (`[f, f+1)` over `span + 1`
            // slots, so the last frame `hi` gets a real slot too). Contiguous
            // runs are coalesced into single rects before painting: with a large
            // RAM budget the ring holds thousands of frames, and one rect per
            // frame tessellated thousands of shapes per repaint (#146) — a small
            // sort is far cheaper. The gap to a full green bar is the part of
            // the range that doesn't fit the RAM budget (or hasn't decoded yet).
            let strip_top = rect.bottom() - 4.0;
            let paint_runs = |frames: &mut Vec<u32>, color: egui::Color32| {
                for_each_frame_run(frames, |start, end| {
                    let seg = egui::Rect::from_min_max(
                        egui::pos2(axis.slot_x(i64::from(start)), strip_top),
                        egui::pos2(
                            axis.slot_x(i64::from(end) + 1).min(rect.right()),
                            rect.bottom(),
                        ),
                    );
                    painter.rect_filled(seg, 0.0, color);
                });
            };
            // The clock source's ring, in global numbers and clipped to the trimmed
            // range (the precache target) — see `cache_bar_frames` for why it is not
            // `A_SOURCE` (#245).
            let (mut resident, mut full) = self.cache_bar_frames(in_pt, out_pt);
            paint_runs(&mut resident, CACHE_PROXY_FILL);
            paint_runs(&mut full, CACHE_FULL_FILL);
            // Holes: distinct vertical marks across the full span.
            if let Some(seq) = self.playback.sequence.as_ref() {
                let hole_color = egui::Color32::from_rgb(206, 92, 60);
                for &h in &seq.holes {
                    let x = x_of(h);
                    painter.line_segment(
                        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                        egui::Stroke::new(1.5_f32, hole_color),
                    );
                }
            }
            // In/out edges.
            for f in [in_pt, out_pt] {
                let x = x_of(f);
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(2.0_f32, visuals.widgets.active.fg_stroke.color),
                );
            }
            // Playhead (drawn last, on top).
            let px = x_of(cur);
            painter.line_segment(
                [
                    egui::pos2(px, rect.top() - 2.0),
                    egui::pos2(px, rect.bottom() + 2.0),
                ],
                egui::Stroke::new(2.0_f32, visuals.strong_text_color()),
            );
            painter.rect_stroke(
                rect,
                3.0,
                egui::Stroke::new(1.0_f32, visuals.widgets.noninteractive.bg_stroke.color),
                egui::StrokeKind::Inside,
            );
        }

        // Scrub on click or drag, clamped to the trim by `playback_scrub_to`.
        // While the drag is held, seeks decode beauty-only for responsiveness
        // (#143); the release settles the landing frame to a full decode.
        if resp.drag_started() {
            self.scrub_active = true;
        }
        if (resp.clicked() || resp.dragged())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            // `frame_at` clamps to `[lo, hi]`, so the cast is always in range.
            self.playback_scrub_to(axis.frame_at(pos.x).max(0) as u32);
        }
        if resp.drag_stopped() {
            self.scrub_active = false;
            self.settle_to_full();
        }
    }

    fn draw_side_panel(&mut self, ui: &mut egui::Ui) {
        if !self.viewer.fullscreen && self.show_side_panel {
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

                        // (section label, display filename, decoded data, comp layer
                        // id). `comp_layer` is `Some` for a layer-stack source — its
                        // pass list drives that layer's AOV; `None` for the classic
                        // A/B slots, whose pass list drives `viewer.active_layer`.
                        // Owned `Arc` clones (cheap) so nothing borrows `self` across
                        // the render loop (which mutates `self` on a pass click).
                        type InfoEntry = (
                            String,
                            String,
                            std::sync::Arc<ExrData>,
                            Option<crate::layer::LayerId>,
                        );
                        let mut files_to_show: Vec<InfoEntry> = vec![];
                        if let (Some(path), Some(data)) = (&self.loaded_file, &self.exr_data) {
                            files_to_show.push((
                                "Image A".to_string(),
                                path.file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                                data.clone(),
                                None,
                            ));
                        }
                        // Comp / layer-stack path (#99 R4): `exr_data` is None, so show
                        // the current layer's source metadata first (Nuke-style), then
                        // the rest of the stack top→bottom.
                        if files_to_show.is_empty() {
                            let cur = self.active_comp_layer();
                            let ids: Vec<crate::layer::LayerId> = {
                                let mut v: Vec<_> =
                                    self.comp_stack.iter().rev().map(|l| l.id).collect();
                                if let Some(c) = cur {
                                    v.retain(|&id| id != c);
                                    v.insert(0, c);
                                }
                                v
                            };
                            for id in ids {
                                let Some(l) = self.comp_stack.get(id) else {
                                    continue;
                                };
                                let source = match &l.source {
                                    crate::layer::LayerSource::Image { source, .. } => {
                                        Some(*source)
                                    }
                                    crate::layer::LayerSource::Adjustment => None,
                                };
                                if let Some(cs) = source.and_then(|s| self.comp_sources.get(&s)) {
                                    let label = if cur == Some(id) {
                                        "Current Layer"
                                    } else {
                                        "Layer"
                                    };
                                    files_to_show.push((
                                        label.to_string(),
                                        l.name.clone(),
                                        cs.exr_data.clone(),
                                        Some(id),
                                    ));
                                }
                            }
                        }

                        if !files_to_show.is_empty() {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for (idx, (label, name, exr_data, comp_layer)) in
                                    files_to_show.iter().enumerate()
                                {
                                    if idx > 0 {
                                        ui.separator();
                                        ui.add_space(10.0);
                                    }
                                    ui.heading(format!("{label}: {name}"));
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
                                            // Flag anamorphic footage (#179); the
                                            // unsqueeze toggle lives in the viewer's
                                            // "Display ▾" menu.
                                            let par_note = if attrs.pixel_aspect != 1.0 {
                                                " (anamorphic)"
                                            } else {
                                                ""
                                            };
                                            ui.label(format!(
                                                "Pixel Aspect: {}{par_note}",
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

                                    // Which pass is active for this entry, and where a
                                    // click retargets it: a comp layer drives its own
                                    // AOV (`LayerSource::Image.aov`); the classic A/B
                                    // slots drive the shared `viewer.active_layer`.
                                    let active_pass = match comp_layer {
                                        Some(id) => self
                                            .comp_stack
                                            .get(*id)
                                            .and_then(|l| match &l.source {
                                                crate::layer::LayerSource::Image {
                                                    aov, ..
                                                } => Some(*aov),
                                                crate::layer::LayerSource::Adjustment => None,
                                            })
                                            .unwrap_or(0),
                                        None => self.viewer.active_layer,
                                    };
                                    for (i, ll) in exr_data.logical_layers.iter().enumerate() {
                                        let is_selected = active_pass == i;

                                        if ui.selectable_label(is_selected, &ll.name).clicked() {
                                            match comp_layer {
                                                Some(id) => {
                                                    if let Some(l) = self.comp_stack.get_mut(*id)
                                                        && let crate::layer::LayerSource::Image {
                                                            aov,
                                                            ..
                                                        } = &mut l.source
                                                    {
                                                        *aov = i;
                                                    }
                                                }
                                                None => self.viewer.active_layer = i,
                                            }
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

                        // Color Sampler + Histogram source (#99 R4): the current
                        // layer's source at its AOV. Owned Arc clones so the block can
                        // call the &mut viewer histogram methods without holding a
                        // borrow of `self`. Tuple = (data, AOV idx, source discriminator).
                        type HistPanel = (std::sync::Arc<ExrData>, usize, u64);
                        let panel_hist: Option<HistPanel> =
                            if let Some(id) = self.active_comp_layer() {
                                match self.comp_stack.get(id).map(|l| &l.source) {
                                    Some(crate::layer::LayerSource::Image { source, aov }) => self
                                        .comp_sources
                                        .get(source)
                                        .map(|cs| (cs.exr_data.clone(), *aov, source.0)),
                                    _ => None,
                                }
                            } else {
                                None
                            };

                        if let Some((exr_data, hist_layer, hist_disc)) = &panel_hist {
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
                                        let exp_mult = crate::render_math::exposure_to_multiplier(
                                            self.viewer.exposure,
                                        );
                                        for (i, swatch) in self.viewer.swatches.iter().enumerate() {
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
                                                    disp_r =
                                                        crate::render_math::linear_to_srgb(disp_r);
                                                    disp_g =
                                                        crate::render_math::linear_to_srgb(disp_g);
                                                    disp_b =
                                                        crate::render_math::linear_to_srgb(disp_b);
                                                }

                                                let r_u8 = (disp_r.clamp(0.0, 1.0) * 255.0) as u8;
                                                let g_u8 = (disp_g.clamp(0.0, 1.0) * 255.0) as u8;
                                                let b_u8 = (disp_b.clamp(0.0, 1.0) * 255.0) as u8;

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
                                                    let s = if max == 0.0 { 0.0 } else { c / max };
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

                            // Recomputing full-res bins on every frame swap
                            // would block the UI thread at playback rate and
                            // contend with decode for the rayon pool (#141).
                            // INV-SAMPLE already suppresses the pixel readout
                            // while playing/pending — mirror it: hold the last
                            // computed bins and recompute once on settle (the
                            // swap invalidated the key).
                            if !self.playback.sampling_suppressed() {
                                self.viewer.calculate_histogram_for(
                                    exr_data,
                                    *hist_layer,
                                    *hist_disc,
                                );
                            }

                            if let Some(bins) = &self.viewer.histogram {
                                let (rect, _resp) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 80.0),
                                    egui::Sense::hover(),
                                );
                                let max_val = (*bins.iter().max().unwrap_or(&1) as f32).max(1.0);

                                // 256 bins; reserve to avoid reallocation.
                                let mut shapes = Vec::with_capacity(256);
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
                                ui.painter().extend(shapes);
                            }
                        } else {
                            ui.label("No file loaded.");
                        }
                    });
                });
        }
    }

    /// The `LayerId` of the slot-A **base track** (#99 R3), if the composite holds
    /// one. The base is the sole layer referencing [`Self::A_SOURCE`] — the opened
    /// image, rendered as the bottom of the stack while compositing.
    fn base_layer_id(&self) -> Option<crate::layer::LayerId> {
        self.comp_stack.iter().find_map(|l| match l.source {
            crate::layer::LayerSource::Image { source, .. } if source == Self::A_SOURCE => {
                Some(l.id)
            }
            _ => None,
        })
    }

    /// Push the opened image (slot A) as the **bottom track** of the composite
    /// (#99 R3), so once you add a comp layer the base plate is *in* the stack
    /// rather than vanishing. A real `LayerSource::Image { source: A_SOURCE }`
    /// layer: `draw_comp_composite` binds it like any other, and it shows as a
    /// (non-removable, clock-pinned) track. Registers a `CompSource` at `A_SOURCE`
    /// whose texture is built lazily each paint from the live A frame
    /// ([`Self::ensure_base_frame`]). Caller ensures a file is open.
    fn add_base_layer(&mut self) {
        let Some(path) = self.loaded_file.clone() else {
            return;
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "base".to_string());
        let Some(exr_data) = self.exr_data.clone() else {
            return;
        };
        // A's range drives the clock at offset 0 (source frame == global frame); a
        // lone still spans all frames.
        let trim = match self.playback.full_range() {
            Some((lo, hi)) => crate::layer::Trim {
                in_point: lo,
                out_point: hi,
                offset: 0,
            },
            None => crate::layer::Trim::full(0, u32::MAX),
        };
        let aov = self.viewer.active_layer;
        let id = self.comp_stack.push_image(name, Self::A_SOURCE, aov, trim);
        self.comp_stack.move_to(id, 0); // bottom of the stack
        let size = exr_data.logical_size(aov).unwrap_or((0, 0));
        self.comp_sources.insert(
            Self::A_SOURCE,
            CompSource {
                path,
                exr_data,
                size,
                aov,
                bind_group: None,
                texture: None,
                cur_frame: None,
                cur_full: false,
            },
        );
    }

    /// Re-point the base track at a freshly-opened slot A (#99 R3): its name +
    /// range, and force a texture rebuild next paint (the new image may reuse the
    /// old frame number, which would otherwise skip the rebuild). No-op if there is
    /// no base track. Called from `open_file`, after `detect_sequence` has set the
    /// new range.
    ///
    /// Reached only from the legacy [`Self::open_file`], now unreachable via the
    /// unified open/drop flow (#99 R4) — kept for the compare path until
    /// render-retire (Phase 3).
    #[allow(dead_code)]
    fn update_base_layer(&mut self, path: &std::path::Path) {
        let Some(id) = self.base_layer_id() else {
            return;
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "base".to_string());
        let trim = match self.playback.full_range() {
            Some((lo, hi)) => crate::layer::Trim {
                in_point: lo,
                out_point: hi,
                offset: 0,
            },
            None => crate::layer::Trim::full(0, u32::MAX),
        };
        if let Some(l) = self.comp_stack.get_mut(id) {
            l.name = name;
            l.trim = trim;
        }
        if let Some(cs) = self.comp_sources.get_mut(&Self::A_SOURCE) {
            cs.cur_frame = None;
        }
    }

    /// Remove the slot-A base track + its comp source (#99 R3), if present. Unlike
    /// a comp source, `A_SOURCE` is A's *own* transport cache / T2 ring, not a
    /// follower — so this drops only the model layer + the `comp_sources` entry and
    /// must NEVER `clear_slot`/touch `followers` for it. Called when the last comp
    /// source is removed and when A is unloaded.
    fn remove_base_layer(&mut self) {
        if let Some(id) = self.base_layer_id() {
            self.comp_stack.remove(id);
            self.comp_sources.remove(&Self::A_SOURCE);
        }
    }

    /// Add a source to the Layers-panel stack (#99 PR-B.2): **decode-on-demand**.
    /// Synchronously decodes `path` (the panel is a paused, cap-6 workflow, so it
    /// reuses [`ExrData::load`] directly rather than the A/B decode worker), then —
    /// when a GPU is present — uploads AOV 0 into a texture keyed by a fresh
    /// `SourceId` in [`Self::comp_sources`], so the composite ping-pong (PR-B.3)
    /// can bind it. On a decode error nothing is added; the error surfaces in the
    /// status bar. Headless (no `gpu_resources`) still registers the model layer +
    /// pixels, just without a texture.
    ///
    /// When this is the *first* comp source and a file is open as slot A, the
    /// opened image is pushed first as the bottom **base track** (#99 R3) so the
    /// plate composites underneath the added layers.
    /// Flatten the live comp stack into the persistable list (#99 PR-B.5), bottom→top
    /// so a replay rebuilds the same order. The **base plate is skipped**: it is
    /// `A_SOURCE`, owned by the slot-A open path rather than the panel, and
    /// `add_comp_source` re-promotes it on its own. A layer whose source is missing
    /// (never possible today, but cheap to tolerate) is skipped rather than persisted
    /// with an empty path that would fail to restore.
    fn comp_layers_persist(&self) -> Vec<LayerPersist> {
        self.comp_stack
            .iter()
            .filter_map(|l| {
                let crate::layer::LayerSource::Image { source, aov } = &l.source else {
                    return None; // adjustment layers (#102) carry no file
                };
                if *source == Self::A_SOURCE {
                    return None;
                }
                let cs = self.comp_sources.get(source)?;
                Some(LayerPersist {
                    path: cs.path.clone(),
                    name: l.name.clone(),
                    aov: *aov,
                    blend: l.blend,
                    opacity: l.opacity,
                    enabled: l.enabled,
                    solo: l.solo,
                    trim_in: l.trim.in_point,
                    trim_out: l.trim.out_point,
                    trim_offset: l.trim.offset,
                })
            })
            .collect()
    }

    /// Rebuild the comp stack from the persisted list (#99 PR-B.5) — called once at
    /// startup, after storage is read.
    ///
    /// Each entry is replayed through [`Self::add_comp_source`], so restore takes the
    /// *same* path as a normal open: sequence detection, follower registration,
    /// transport entry, and the layer cap all behave identically. The per-layer state
    /// the model owns (`aov` / blend / opacity / enabled / solo / trim) is then applied
    /// on top, since `add_comp_source` always adds at defaults.
    ///
    /// A file that has moved or been deleted is **skipped silently** — the rest of the
    /// stack still restores, and a startup error box for a stale session would be
    /// noise. `add_comp_source` sets `error_msg` on a failed decode, so that is cleared
    /// afterwards for the same reason.
    fn restore_comp_layers(&mut self) {
        let saved = std::mem::take(&mut self.persisted_layers);
        if saved.is_empty() {
            return;
        }
        for entry in saved {
            if !entry.path.is_file() {
                continue;
            }
            // Two persisted layers on one path share a source rather than decoding it
            // twice (#242). That is what a duplicate *is* in a live session
            // (`duplicate_comp_layer` clones the layer against the same `SourceId`),
            // so restoring one as two independent sources would silently undo the
            // sharing on the next launch — the same double-decode, one restart later.
            //
            // It also repairs stacks built before the fix: a session that accumulated
            // N copies of a path by re-opening it collapses to one source and one
            // decode on load, instead of paying for N.
            //
            // `push_image`'s trim is a placeholder — the entry's own trim is applied
            // below, like every other field.
            let before = self.comp_stack.len();
            if let Some(existing) = self.layer_for_path(&entry.path) {
                let Some((source, trim)) =
                    self.comp_stack.get(existing).and_then(|l| match &l.source {
                        crate::layer::LayerSource::Image { source, .. } => Some((*source, l.trim)),
                        crate::layer::LayerSource::Adjustment => None,
                    })
                else {
                    continue;
                };
                self.comp_stack
                    .push_image(entry.name.clone(), source, entry.aov, trim);
            } else {
                self.add_comp_source(entry.path);
            }
            // A rejected add (cap reached / decode failure) leaves the stack
            // unchanged — so `iter().last()` is then the *previous* layer, and
            // configuring it would overwrite a successfully restored layer's name,
            // blend, trim and AOV with this entry's. Length is the reliable "did
            // anything land" test; the id alone can't tell.
            if self.comp_stack.len() == before {
                continue;
            }
            // The just-restored layer is the last one either way: `add_comp_source`
            // and `push_image` both push on top.
            let Some(id) = self.comp_stack.iter().last().map(|l| l.id) else {
                continue;
            };
            let Some(layer) = self.comp_stack.get_mut(id) else {
                continue;
            };
            if let crate::layer::LayerSource::Image { aov, .. } = &mut layer.source {
                *aov = entry.aov;
            }
            layer.name = entry.name;
            layer.blend = entry.blend;
            layer.opacity = entry.opacity;
            layer.enabled = entry.enabled;
            layer.solo = entry.solo;
            layer.trim = crate::layer::Trim {
                in_point: entry.trim_in,
                out_point: entry.trim_out,
                offset: entry.trim_offset,
            };
        }
        // A skipped/failed entry must not greet the user with a stale error box.
        self.error_msg = None;
    }

    fn add_comp_source(&mut self, path: std::path::PathBuf) {
        if self.comp_stack.len() >= COMP_LAYER_CAP {
            // Report rather than silently drop — a multi-file drop past the cap
            // otherwise reads as if every file landed (#99 R4, no-silent-caps).
            self.error_msg = Some(format!(
                "Layer limit reached ({COMP_LAYER_CAP}) — remove a layer to add more."
            ));
            return;
        }
        // Decode first: a failed load must leave the stack untouched (no dangling
        // layer pointing at a source with no pixels).
        let exr_data = match ExrData::load(&path) {
            Ok(d) => std::sync::Arc::new(d),
            Err(e) => {
                self.error_msg = Some(format!("Failed to load layer '{}': {e}", path.display()));
                return;
            }
        };
        // A file can decode yet expose no renderable image layer (no channels /
        // groups, or a deep/unsupported part). Reject it like a load error — before
        // consuming a `SourceId` — so we never push a permanently non-renderable
        // layer (size (0,0), no texture) with a misleading hint (#189 review).
        if exr_data.logical_channels(0).is_none() {
            self.error_msg = Some(format!(
                "Layer '{}' has no renderable image layers.",
                path.display()
            ));
            return;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "layer".to_string());

        // Detect whether the file is part of an image sequence (#99 Phase 2). A
        // sequence source becomes a decoding **follower** on the shared playhead,
        // and its layer's `Trim` spans the sequence range (blank outside it); a
        // lone still keeps the all-frames trim and its single decoded texture.
        let sequence = crate::sequence::detect_from_file(&path);
        // The opened frame of the sequence (its "current" frame).
        let cf = sequence
            .as_ref()
            .map(|seq| seq.number_of(&path).unwrap_or(seq.range.0));
        // The first comp sequence with no transport loaded drives the global clock
        // (#99 R4-lite): enter it so the timeline + playback keys light up (mirrors
        // opening a slot-A sequence). A base plate already active keeps its clock;
        // added comp layers then align to it.
        let claims_transport = if let (Some(seq), Some(cf)) = (&sequence, cf)
            && !self.playback.is_active()
        {
            self.playback.enter(seq.clone(), cf);
            true
        } else {
            false
        };
        let trim = match (&sequence, cf) {
            // Align the opened frame to the current playhead so the just-added layer
            // is visible *immediately*: `offset = cf - global` makes
            // `source_frame(global) == cf` right now, then it plays 1:1 with the
            // transport and is blank outside `[lo, hi]`. Without a base plate the
            // playhead is 0, so this shows the opened frame instead of dropping the
            // layer as out-of-range (the bug where an added sequence went blank).
            (Some(seq), Some(cf)) => crate::layer::Trim {
                in_point: seq.range.0,
                out_point: seq.range.1,
                offset: i64::from(cf) - i64::from(self.playback.current_frame),
            },
            // A lone still spans all frames.
            _ => crate::layer::Trim::full(0, u32::MAX),
        };

        // The first comp source, with a plate open as slot A, promotes that plate
        // to the bottom base track first (#99 R3) — so it's beneath the layer we're
        // about to add. `comp_drives_transport` (no slot A) has no plate to add.
        if self.comp_stack.is_empty() && self.loaded_file.is_some() && self.exr_data.is_some() {
            self.add_base_layer();
        }

        let source = crate::layer::SourceId(self.comp_next_source);
        self.comp_next_source += 1;
        // Record *which* source took the clock, not just that one did: pacing and
        // the T1 sizing seed have to attribute a decode to the transport-driving
        // follower (#100). The id only exists here, after allocation.
        if claims_transport {
            self.set_transport_source(Some(source));
        }
        let layer_id = self.comp_stack.push_image(name, source, 0, trim);
        // A freshly added layer becomes the "current" layer the viewport bar's AOV /
        // channel controls + EXR Info act on (Nuke-style, #99 R4 follow-up).
        self.selected_comp_layer = Some(layer_id);

        // Build the GPU texture for AOV 0 (matching `push_image`'s aov). Absent a
        // GPU device (headless / CPU-only) the source is stored pixels-only.
        let aov = 0;
        let size = exr_data.logical_size(aov).unwrap_or((0, 0));
        let (texture, bind_group) = match self.gpu_resources.as_ref() {
            Some(gpu) => {
                crate::viewer::ExrViewer::build_source_texture(&gpu.tex_build_ctx(), &exr_data, aov)
                    .map_or((None, None), |(t, bg)| (Some(t), Some(bg)))
            }
            None => (None, None),
        };

        // A sequence source: register a follower so the shared decode pump / T1
        // cache / eviction treat it like the A/B slots, and seed the cache with the
        // opened frame (its full decode) for an instant first hit — mirrors
        // `detect_sequence`. Stills register no follower (nothing to play).
        if let (Some(seq), Some(cf)) = (sequence, cf) {
            self.frame_cache.insert(source, cf, exr_data.clone());
            self.followers.insert(
                source,
                SourceState {
                    sequence: Some(seq),
                    current_frame: cf,
                    ..Default::default()
                },
            );
            // Size the T1 budget from this decode when it drives the clock
            // (#100/#199). This path decodes **synchronously** and inserts straight
            // into the cache, so it never reaches `apply_load_result`'s seed — and
            // it is how every sequence enters the app now that open/drop means "add
            // a layer". Without it the budget stays unsized until the *next* decode
            // happens to land, which on a paused first open is never.
            if source == self.clock_source() {
                self.frame_bytes
                    .get_or_insert_with(|| exr_data.approx_bytes());
            }
        }

        self.comp_sources.insert(
            source,
            CompSource {
                path,
                exr_data,
                size,
                aov,
                bind_group,
                texture,
                cur_frame: None,
                cur_full: false,
            },
        );
    }

    /// Ensure a comp source's GPU texture holds the requested AOV (#99 PR-B.4.3),
    /// rebuilding it if the layer's AOV changed. A source stores one texture, so
    /// this is exact while each source backs a single layer (every UI-reachable
    /// state today — the Add flow makes one source per file). Sharing a source
    /// across layers at different AOVs (back-to-beauty) would need per-`(SourceId,
    /// aov)` textures — a follow-up. No-op if already current, headless, or the
    /// rebuild fails (the stale texture is kept).
    fn ensure_comp_aov(&mut self, source: crate::layer::SourceId, aov: usize) {
        let Some(cs) = self.comp_sources.get(&source) else {
            return;
        };
        if cs.aov == aov {
            return;
        }
        // Clone the Arc so the immutable `comp_sources` borrow ends before the
        // `gpu_resources` read + the `get_mut` write below (disjoint fields).
        let exr_data = cs.exr_data.clone();
        let Some(gpu) = self.gpu_resources.as_ref() else {
            return;
        };
        let Some((texture, bind_group)) =
            crate::viewer::ExrViewer::build_source_texture(&gpu.tex_build_ctx(), &exr_data, aov)
        else {
            return;
        };
        let size = exr_data.logical_size(aov).unwrap_or((0, 0));
        if let Some(cs) = self.comp_sources.get_mut(&source) {
            cs.texture = Some(texture);
            cs.bind_group = Some(bind_group);
            cs.aov = aov;
            cs.size = size;
        }
    }

    /// Request a **sequence** comp source's texture for `(source_frame, aov)` from
    /// the T1-cached frame (#99 Phase 2), when the playhead or AOV moved off what it
    /// currently holds. No-op if already current, headless, the frame isn't resident
    /// (the last-built frame is held), or a build for this source is already in
    /// flight. This is what makes an added comp layer *play*: each paint,
    /// `draw_comp_central` resolves the layer's source frame and rebinds the decoded
    /// pixels for it.
    ///
    /// The build itself is **asynchronous** (#202): this hands the work to
    /// [`crate::tex_upload::TexUploader`] and returns, and
    /// [`Self::collect_comp_textures`] swaps the result in on a later paint. It used
    /// to interleave and upload the whole frame inline, which measured at ~940 ms of
    /// every 1000 ms of UI-thread time on 4.6K footage with two layers — the thread
    /// that has to paint was spending essentially all of itself on `memcpy`, which
    /// is why displayed throughput sat at ~13 fps against a 24 fps clock however
    /// fast frames decoded.
    fn ensure_comp_frame(&mut self, source: crate::layer::SourceId, source_frame: u32, aov: usize) {
        let Some(cs) = self.comp_sources.get(&source) else {
            return;
        };
        // Steady state: this exact frame is bound at full fidelity, so there is
        // nothing better to show and no need to touch the cache.
        let on_frame = cs.cur_frame == Some(source_frame) && cs.aov == aov;
        if on_frame && cs.cur_full {
            return;
        }
        // Hold the current texture until the frame is actually resident in T1.
        let Some(arc) = self.frame_cache.peek(source, source_frame) else {
            return;
        };
        // Rebuild the *same* frame when the cache has upgraded underneath us
        // (#212). Settling re-decodes the playhead at full fidelity without moving
        // its frame number, so keying only on the number left the layer showing the
        // proxy texture forever while the full pixels sat in the cache — the
        // "stuck in proxy after scrubbing" report.
        if on_frame && (arc.proxy || arc.beauty_only) {
            return; // resident copy is no better than what's bound
        }
        // A cheap frame decoded for a *different* pass cannot serve this one
        // (#217). Switching a layer's AOV mid-playback leaves exactly these frames
        // resident — T1 is keyed `(source, frame)`, with no AOV in the key — so
        // every one of them is asked for the new pass and cannot answer.
        //
        // The build would fail and `collect_comp_textures` would evict it, which is
        // correct and self-healing; doing it here instead just skips a guaranteed-
        // dead uploader round trip per frame. The re-decode is not avoidable — the
        // pixels genuinely are not there — so this does not remove the switch hitch,
        // only the wasted work inside it.
        //
        // The real point is the alarm. That build-failed WARN is #213's, and an
        // ordinary AOV switch fired it 102 times in one dogfood session; an alarm
        // that routine is one you learn to scroll past. Restricted to *partial*
        // frames, so a **full** decode that still cannot serve the AOV falls
        // through to the failing build and the warning — because that case really
        // is the bug the alarm is for, and re-decoding it would loop.
        if (arc.proxy || arc.beauty_only || arc.only_layer.is_some())
            && arc.logical_channels(aov).is_none()
        {
            self.frame_cache.remove(source, source_frame);
            return;
        }
        let Some(up) = self.tex_uploader.as_mut() else {
            return;
        };
        // Already building exactly this — waiting is the whole point of the gate.
        if up.pending_for(source) == Some((source_frame, aov)) {
            return;
        }
        up.try_submit(source, source_frame, aov, arc);
    }

    /// Bind every texture the upload workers finished since the last paint (#202),
    /// and pace off the swap.
    ///
    /// A result whose frame the playhead has already passed is still applied. It is
    /// newer than what is on screen, so it moves the picture forward; the next
    /// `ensure_comp_frame` immediately asks for the now-current frame. Dropping it
    /// would hold the layer on an older frame for another full round trip — the
    /// freeze #204 is about.
    fn collect_comp_textures(&mut self) {
        let Some(up) = self.tex_uploader.as_mut() else {
            return;
        };
        let built = up.drain();
        for b in built {
            let Some((texture, bind_group)) = b.texture else {
                // A failed build used to be indistinguishable from "nothing to do",
                // which is how #213 hid: every build failed, the layer froze, and no
                // metric said so. Evict the frame that couldn't be built so the next
                // request re-decodes it rather than re-failing on the same cached
                // bytes forever — the usual cause is a cheap decode that doesn't
                // carry the AOV this layer needs.
                log::warn!(
                    target: "floki::playback",
                    "texture build failed: s{} f{} aov{} — evicting to force a re-decode",
                    b.source.0, b.frame, b.aov
                );
                self.frame_cache.remove(b.source, b.frame);
                continue;
            };
            // The layer may have been removed while its build was in flight.
            let swapped = self.comp_sources.get_mut(&b.source).is_some_and(|cs| {
                let changed = cs.cur_frame != Some(b.frame);
                cs.texture = Some(texture);
                cs.bind_group = Some(bind_group);
                cs.aov = b.aov;
                cs.size = b.size;
                cs.cur_frame = Some(b.frame);
                cs.cur_full = b.full;
                changed
            });
            if swapped {
                self.note_display(b.source, b.frame);
            }
        }
    }

    /// Record that `source` swapped its displayed frame to `source_frame` — the
    /// pacing measurement's single source of truth (#100/#204).
    ///
    /// Called from [`Self::ensure_comp_frame`] on an actual texture swap, because
    /// **that** is the display event. Residency and decode-arrival are both proxies
    /// for it and both wrong: a frame can be resident, or can land, without ever
    /// being painted (the playhead has moved on by then), and a frame can be painted
    /// long after either. Pacing off those proxies reported the same displayed frame
    /// twice microseconds apart — and, worse, recorded nothing at all through the
    /// runs where the picture was genuinely frozen, which is precisely when the
    /// number matters.
    ///
    /// Only the clock source paces: a trailing layer painting late is not a
    /// transport tick, and counting it would make the frame-time ring report layer
    /// count as speed.
    fn note_display(&mut self, source: crate::layer::SourceId, source_frame: u32) {
        if source == self.clock_source() {
            self.playback
                .note_shown(std::time::Instant::now(), source_frame);
        }
    }

    /// Bring the slot-A **base track**'s texture current for the on-screen frame
    /// (#99 R3) — the base-plate analogue of [`Self::ensure_comp_frame`]. Slot A is
    /// the master transport, not a follower, so its pixels come straight from the
    /// live `self.exr_data` (already swapped to each frame by the decode path)
    /// rather than the T1 cache. Rebuilds only when the playhead or AOV moved off
    /// what the texture holds; a still keeps `current_frame == 0` and builds once.
    /// No-op headless or if the build fails (the last texture is held).
    fn ensure_base_frame(&mut self, aov: usize) {
        let cur = self.playback.current_frame;
        let Some(cs) = self.comp_sources.get(&Self::A_SOURCE) else {
            return;
        };
        if cs.cur_frame == Some(cur) && cs.aov == aov {
            return;
        }
        let Some(data) = self.exr_data.clone() else {
            return;
        };
        let Some(gpu) = self.gpu_resources.as_ref() else {
            return;
        };
        let Some((texture, bind_group)) =
            crate::viewer::ExrViewer::build_source_texture(&gpu.tex_build_ctx(), &data, aov)
        else {
            return;
        };
        let size = data.logical_size(aov).unwrap_or((0, 0));
        if let Some(cs) = self.comp_sources.get_mut(&Self::A_SOURCE) {
            cs.texture = Some(texture);
            cs.bind_group = Some(bind_group);
            cs.aov = aov;
            cs.size = size;
            cs.cur_frame = Some(cur);
        }
    }

    /// Remove a Layers-panel layer and free its source's pixels/VRAM once no other
    /// layer references it (#99 PR-B.2). A source can be shared by several layers
    /// (e.g. back-to-beauty AOVs of one file), so its [`CompSource`] is dropped
    /// only when the last referencing layer goes — dropping it releases the GPU
    /// texture (drop-only free; wgpu reclaims after the bind group unbinds).
    fn remove_comp_layer(&mut self, id: crate::layer::LayerId) {
        // Note the source this layer referenced before removing it.
        let source = self.comp_stack.get(id).and_then(|l| match &l.source {
            crate::layer::LayerSource::Image { source, .. } => Some(*source),
            crate::layer::LayerSource::Adjustment => None,
        });
        self.comp_stack.remove(id);
        // Drop a stale selection so `active_comp_layer` falls back to the top layer.
        if self.selected_comp_layer == Some(id) {
            self.selected_comp_layer = None;
        }
        // GC the source if the removed layer was its last reference.
        if let Some(source) = source
            && !self.comp_stack.iter().any(|l| {
                matches!(&l.source, crate::layer::LayerSource::Image { source: s, .. } if *s == source)
            })
        {
            self.comp_sources.remove(&source);
            // A sequence comp source is also a decode follower (#99 Phase 2): drop
            // its follower state and every cached frame so a removed-then-re-added
            // source can't hit a stale follower / cache entry. No-op for a still
            // (no follower registered).
            self.followers.remove(&source);
            // Release the upload gate too (#202), or a source removed mid-build
            // would leave a permanently-held in-flight slot behind. A result may
            // still land for it; `collect_comp_textures` drops results for sources
            // that no longer exist.
            if let Some(up) = self.tex_uploader.as_mut() {
                up.forget(source);
            }
            self.frame_cache.clear_slot(source);
        }
        // Removing the last comp source leaves only the slot-A base track (#99 R3);
        // drop it too so the composite empties and the classic viewer takes back
        // over (readout / histogram / compare modes). `remove_base_layer` is careful
        // not to touch A's real cache / transport.
        if self.comp_stack.len() == 1 && self.base_layer_id().is_some() {
            self.remove_base_layer();
        }
        // If the comp stack drove the transport and its last sequence is gone,
        // release the clock (#99 R4-lite) so the timeline doesn't outlive its
        // source.
        if self.comp_drives_transport() && self.active_followers().next().is_none() {
            self.playback.clear();
            self.set_transport_source(None);
        } else if self
            .transport_source
            .is_some_and(|s| !self.followers.contains_key(&s))
        {
            // The clock-driving source itself was removed but others remain. The
            // transport keeps running off `playback.sequence` (an owned clone), as
            // it did when this was a bare flag — but the id must not dangle, or the
            // pacing / sizing instruments keyed on it (#100) go silent. Re-point at
            // a surviving follower.
            let next = self.active_followers().next().map(|(s, _)| *s);
            self.set_transport_source(next);
        }
    }

    /// The comp layer the viewport bar's AOV / channel controls + EXR Info operate
    /// on — Nuke's "current" layer. Prefers the explicit
    /// [`Self::selected_comp_layer`]; when unset or stale (its layer was removed),
    /// falls back to the **topmost** layer so there is always a current layer while
    /// the stack is non-empty. `None` only for an empty stack.
    fn active_comp_layer(&self) -> Option<crate::layer::LayerId> {
        if let Some(id) = self.selected_comp_layer
            && self.comp_stack.get(id).is_some()
        {
            return Some(id);
        }
        // `iter()` is bottom→top, so the last layer is the top of the stack.
        self.comp_stack.iter().last().map(|l| l.id)
    }

    /// The layer filling the compare pane (side B) — Nuke's second viewer input
    /// (#99 Slice 2a). Prefers the explicit [`Self::compare_b_layer`]; when unset or
    /// stale (its layer was removed) falls back to [`default_compare_b`], which avoids
    /// the current layer so the panes differ by default. `None` only for an empty stack.
    fn compare_b(&self) -> Option<crate::layer::LayerId> {
        if let Some(id) = self.compare_b_layer
            && self.comp_stack.get(id).is_some()
        {
            return Some(id);
        }
        let ids: Vec<_> = self.comp_stack.iter().map(|l| l.id).collect();
        default_compare_b(&ids, self.active_comp_layer())
    }

    /// Nuke-style "current layer" control bar for the comp viewport (#99 R4
    /// follow-up): a layer picker (which of the stack is current), that layer's
    /// AOV / pass pulldown, and R/G/B/A channel isolation — the classic
    /// single-image control row, restored for the layer-stack path so you can load
    /// a layer and look through its AOVs / channels. Reuses the per-track AOV combo
    /// (`aov_names`) + [`crate::viewer::ExrViewer::set_channel_mode`].
    fn draw_comp_layer_bar(&mut self, ui: &mut egui::Ui) {
        let Some(layer_id) = self.active_comp_layer() else {
            return;
        };
        // Snapshot the current layer's source + AOV + the whole stack's names up
        // front, so the combo closures below only mutate simple fields (no aliasing
        // borrow of `self.comp_stack` while a picker is open).
        let (source, aov) = match self.comp_stack.get(layer_id).map(|l| &l.source) {
            Some(crate::layer::LayerSource::Image { source, aov }) => (Some(*source), *aov),
            _ => (None, 0),
        };
        // Top→bottom for the picker, matching the stack's visual order.
        let layer_list: Vec<(crate::layer::LayerId, String)> = self
            .comp_stack
            .iter()
            .rev()
            .map(|l| (l.id, l.name.clone()))
            .collect();
        let cur_name = self
            .comp_stack
            .get(layer_id)
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "—".to_string());
        let aov_names: Vec<String> = source
            .and_then(|s| self.comp_sources.get(&s))
            .map(|cs| {
                cs.exr_data
                    .logical_layers
                    .iter()
                    .enumerate()
                    .map(|(i, ll)| {
                        if ll.name.is_empty() {
                            format!("layer {i}")
                        } else {
                            ll.name.clone()
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The current layer's header pixel aspect, to gate the anamorphic toggle
        // below (#194) — only worth showing for non-square-pixel footage.
        let cur_par = source
            .and_then(|s| self.comp_sources.get(&s))
            .map(|cs| cs.exr_data.image.attributes.pixel_aspect)
            .unwrap_or(1.0);

        // Wrapped, so a long layer name pushes the trailing controls onto a second line
        // instead of clipping them off the right edge.
        ui.horizontal_wrapped(|ui| {
            // Which layer is current (Nuke's input selector). Only worth showing
            // with more than one layer to choose from.
            if layer_list.len() > 1 {
                ui.label("Layer:");
                egui::ComboBox::from_id_salt("comp_current_layer")
                    .selected_text(elide_middle(&cur_name, COMP_BAR_NAME_CHARS))
                    .show_ui(ui, |ui| {
                        for (id, nm) in &layer_list {
                            if ui.selectable_label(*id == layer_id, nm).clicked() {
                                self.selected_comp_layer = Some(*id);
                            }
                        }
                    })
                    .response
                    .on_hover_text(&cur_name);
                ui.separator();
            }

            // The current layer's AOV / pass pulldown (multi-pass EXRs only).
            if aov_names.len() > 1 {
                ui.label("Pass:");
                let mut new_aov = aov.min(aov_names.len() - 1);
                egui::ComboBox::from_id_salt("comp_current_aov")
                    .selected_text(aov_names[new_aov].clone())
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for (idx, nm) in aov_names.iter().enumerate() {
                            ui.selectable_value(&mut new_aov, idx, nm);
                        }
                    });
                if new_aov != aov
                    && let Some(l) = self.comp_stack.get_mut(layer_id)
                    && let crate::layer::LayerSource::Image { aov: a, .. } = &mut l.source
                {
                    *a = new_aov;
                }
                ui.separator();
            }

            // Contact Sheet, beside the pass control because that is what it is — a
            // visual pass-picker for the current layer. Always available (single or
            // multi load), unlike the `Pass:` combo, which needs a multi-pass source.
            // Only shows the *current* layer's passes, so it follows the `Layer:`
            // selection; the View-menu item and `T` toggle the same flag.
            ui.toggle_value(&mut self.viewer.show_contact_sheet, "▦ Sheet")
                .on_hover_text("Contact sheet of the current layer's passes (T)");
            ui.separator();

            // Tone controls (#99 Slice 3a). These lived only in the classic viewer's
            // `primary_row`, which the R4 collapse made unreachable — yet
            // `draw_comp_composite` still applies `exposure` / `gamma` / `srgb` to the
            // composite, so they were frozen at whatever persisted. Same widgets and
            // same right-click-to-reset behaviour as the row they replace.
            let exp = ui
                .add(
                    egui::DragValue::new(&mut self.viewer.exposure)
                        .speed(0.01)
                        .range(-5.0..=5.0)
                        .prefix("EV ")
                        .fixed_decimals(2),
                )
                .on_hover_text("Drag to adjust • right-click resets to 0.0 (key: E)");
            if exp.changed() {
                self.viewer.invalidate_tone();
            }
            if exp.secondary_clicked() {
                self.viewer.reset_exposure();
            }
            let gam = ui
                .add(
                    egui::DragValue::new(&mut self.viewer.gamma)
                        .speed(0.01)
                        .range(0.1..=5.0)
                        .prefix("γ ")
                        .fixed_decimals(2),
                )
                .on_hover_text("Drag to adjust • right-click resets to 1.0 (key: Shift+G)");
            if gam.changed() {
                self.viewer.invalidate_tone();
            }
            if gam.secondary_clicked() {
                self.viewer.reset_gamma();
            }
            if ui
                .button("⟲")
                .on_hover_text("Reset exposure (0.0) & gamma (1.0)")
                .clicked()
            {
                self.viewer.reset_exposure();
                self.viewer.reset_gamma();
            }
            if ui.checkbox(&mut self.viewer.srgb, "sRGB").changed() {
                self.viewer.invalidate_tone();
            }
            ui.separator();

            // Channel isolation — the same C/R/G/B/A the classic top row had; the
            // composite honors `channel_mode` on its top layer.
            ui.label("Channel:");
            use crate::viewer::ChannelMode;
            let mut mode = self.viewer.channel_mode;
            ui.selectable_value(&mut mode, ChannelMode::RGB, "RGB")
                .on_hover_text("Show all channels (C)");
            ui.selectable_value(&mut mode, ChannelMode::R, "R");
            ui.selectable_value(&mut mode, ChannelMode::G, "G");
            ui.selectable_value(&mut mode, ChannelMode::B, "B");
            ui.selectable_value(&mut mode, ChannelMode::A, "A");
            self.viewer.set_channel_mode(mode);

            // Compare arrangement (#99 render-retire, Slice 2): how the composite
            // (side A) is presented against the current layer (side B). Only
            // meaningful with ≥2 layers. Wipe/Diff arrive in Slice 2b.
            if layer_list.len() > 1 {
                ui.separator();
                ui.label("Compare:");
                use crate::layer::Arrangement;
                let mut arr = self.comp_arrangement;
                egui::ComboBox::from_id_salt("comp_arrangement")
                    .selected_text(match arr {
                        Arrangement::Stacked => "Stacked",
                        Arrangement::SideBySide => "Side by Side",
                        Arrangement::Wipe { .. } => "Wipe",
                        Arrangement::Diff => "Diff",
                        Arrangement::Blink => "Blink",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut arr, Arrangement::Stacked, "Stacked");
                        ui.selectable_value(&mut arr, Arrangement::SideBySide, "Side by Side");
                        // Wipe carries a position, but the live geometry comes from the
                        // viewer's `wipe_center` / `wipe_angle` (dragged on the handle),
                        // so this literal is just the selector value.
                        if ui
                            .selectable_label(matches!(arr, Arrangement::Wipe { .. }), "Wipe")
                            .clicked()
                        {
                            arr = Arrangement::Wipe { position: 0.5 };
                        }
                        ui.selectable_value(&mut arr, Arrangement::Diff, "Diff");
                        ui.selectable_value(&mut arr, Arrangement::Blink, "Blink");
                    });
                self.comp_arrangement = arr;

                if arr != Arrangement::Stacked {
                    // Which layer fills the compare pane — Nuke's second viewer input.
                    // Side A is always the whole composite; this picks what it is shown
                    // against, independent of the "current" layer above.
                    let cmp_b = self.compare_b();
                    let cmp_b_name = cmp_b
                        .and_then(|id| self.comp_stack.get(id))
                        .map(|l| l.name.clone())
                        .unwrap_or_else(|| "—".to_string());
                    ui.label("vs:");
                    egui::ComboBox::from_id_salt("comp_compare_b")
                        .selected_text(elide_middle(&cmp_b_name, COMP_BAR_NAME_CHARS))
                        .show_ui(ui, |ui| {
                            for (id, nm) in &layer_list {
                                if ui.selectable_label(Some(*id) == cmp_b, nm).clicked() {
                                    self.compare_b_layer = Some(*id);
                                }
                            }
                        })
                        .response
                        .on_hover_text(&cmp_b_name);
                    // Per-arrangement parameters (#99 Slice 3c). These lived in the
                    // classic `mode_param_row` — a second toolbar row that slid in — but
                    // the comp bar is one row, so they go behind a menu. Shared with
                    // that row via `wipe_params_ui` / `diff_params_ui`.
                    match arr {
                        // Only Side-by-Side lays the panes out separately; Wipe and Diff
                        // overlay both layers in one rect, where normalizing is
                        // meaningless.
                        Arrangement::SideBySide => {
                            ui.checkbox(&mut self.viewer.normalize_side_by_side, "Normalize Size")
                                .on_hover_text("Match the compare layer's height to pane A's");
                        }
                        Arrangement::Wipe { .. } => {
                            ui.menu_button("Wipe ▾", |ui| self.viewer.wipe_params_ui(ui))
                                .response
                                .on_hover_text(
                                    "Split centre, angle, and divider opacity \
                                     (or drag the on-image handle / scroll to rotate)",
                                );
                        }
                        Arrangement::Diff => {
                            ui.menu_button("Diff ▾", |ui| self.viewer.diff_params_ui(ui))
                                .response
                                .on_hover_text("Gain, colormap, metric, and noise floor");
                        }
                        Arrangement::Blink => {
                            ui.label("Speed");
                            ui.add(
                                egui::Slider::new(&mut self.viewer.blink_interval, 0.05..=5.0)
                                    .suffix("s"),
                            )
                            .on_hover_text("How long each pane is shown before flipping");
                        }
                        Arrangement::Stacked => {}
                    }
                }
            }

            // Annotations (#45 / #99 Slice 3d). Hiding the bar also drops the active
            // tool, so a hidden bar can never leave a tool armed — which would suppress
            // canvas pan (`handle_canvas_interaction` checks `anno_tool`).
            ui.separator();
            if ui
                .toggle_value(&mut self.viewer.show_annotation_bar, "✎ Annotate")
                .on_hover_text("Draw arrows / boxes / freehand / text over the image")
                .changed()
                && !self.viewer.show_annotation_bar
            {
                self.viewer.anno_tool = crate::annotation::AnnotationTool::None;
            }

            // The rarely-touched controls, behind a menu as the classic row had them
            // (#99 Slice 3a). The standalone Unsqueeze checkbox (#194) folds in here.
            ui.separator();
            ui.menu_button("Display ▾", |ui| {
                // Readout aperture. `sample_pixel` (shared with the comp readout) has
                // always honored this; only its control was unreachable.
                ui.label("Sample:");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.viewer.sample_aperture, 1, "1px");
                    ui.selectable_value(&mut self.viewer.sample_aperture, 3, "3×3");
                    ui.selectable_value(&mut self.viewer.sample_aperture, 9, "9×9");
                });
                ui.checkbox(&mut self.viewer.show_tooltip, "Show Pixel Tooltip")
                    .on_hover_text("Float the sampled value next to the cursor");

                ui.separator();
                // Anamorphic unsqueeze (#179 / #194): the master toggle persists via
                // `ViewerPrefs`; the optional custom factor overrides the header PAR.
                ui.checkbox(
                    &mut self.viewer.prefs.anamorphic_unsqueeze,
                    "Unsqueeze anamorphic",
                )
                .on_hover_text(
                    "Stretch non-square-pixel (anamorphic) footage to its display aspect",
                );
                let unsqueeze = self.viewer.prefs.anamorphic_unsqueeze;
                ui.add_enabled_ui(unsqueeze, |ui| {
                    let mut custom = self.viewer.pixel_aspect_override.is_some();
                    if ui
                        .checkbox(&mut custom, "Custom factor")
                        .on_hover_text("Override the header pixel aspect ratio")
                        .changed()
                    {
                        // Seed from the current layer's header PAR, or a common 2×
                        // squeeze when the header is square/absent.
                        let seed = if cur_par > 0.0 && (cur_par - 1.0).abs() > f32::EPSILON {
                            cur_par
                        } else {
                            2.0
                        };
                        self.viewer.pixel_aspect_override = custom.then_some(seed);
                    }
                    if let Some(factor) = self.viewer.pixel_aspect_override.as_mut() {
                        ui.add(egui::DragValue::new(factor).speed(0.01).range(0.1..=4.0));
                    } else {
                        ui.label(format!("Header PAR: {cur_par}"));
                    }
                });
            });
        });
        // The annotation tool row, on its own line below the bar so the main row keeps
        // its width (the classic UI gave it a separate toolbar row too).
        if self.viewer.show_annotation_bar {
            self.viewer.annotation_toolbar(ui);
        }
        ui.separator();
    }

    /// The **layer tracks** section of the timeline panel (#99, Chaos-Player
    /// layout): one row per composite layer, top-of-stack first. The gutter holds
    /// visibility / solo / name plus a `⋮` menu with the rest of the layer's
    /// controls; right of it the layer's clip bar sits on the panel's shared time
    /// axis, showing its `[in, out] + offset` span and draggable to retime it.
    ///
    /// `range` is the visible global frame range, or `None` when no transport is
    /// loaded (a stills-only stack) — the rows are then gutter-only, as the old
    /// flat list was.
    fn draw_layer_tracks(&mut self, ui: &mut egui::Ui, axis_w: f32, range: Option<(u32, u32)>) {
        ui.horizontal(|ui| {
            ui.heading("Layers");
            ui.label(
                egui::RichText::new(format!("{}/{}", self.comp_stack.len(), COMP_LAYER_CAP)).weak(),
            );
            let at_cap = self.comp_stack.len() >= COMP_LAYER_CAP;
            ui.add_enabled_ui(!at_cap, |ui| {
                if ui
                    .button("➕  Add source…")
                    .on_disabled_hover_text("Layer cap reached")
                    .clicked()
                    && let Some(path) = FileDialog::new()
                        .add_filter("EXR Image", &["exr"])
                        .pick_file()
                {
                    self.open_layer(path);
                }
            });
        });
        ui.separator();

        if self.comp_stack.is_empty() {
            ui.weak("No layers yet. Add a source to begin.");
            return;
        }
        {
            // Per-row controls (#99 PR-B.4): visibility / solo / blend / reorder /
            // remove. Snapshot each row's id + display state up front (bottom→top)
            // so the widgets can mutate `comp_stack` without aliasing an `iter()`
            // borrow; the index `i` in this Vec is the layer's stack index (0 =
            // bottom). Deferred edits (reorder / remove restructure the stack) are
            // recorded and applied once after the loop.
            struct Row {
                id: crate::layer::LayerId,
                name: String,
                enabled: bool,
                solo: bool,
                blend: crate::viewer::BlendMode,
                opacity: f32,
                /// Current AOV index + the source's AOV names, for the per-row AOV
                /// picker (#99 PR-B.4.3). Empty / single-entry ⇒ no picker shown.
                aov: usize,
                aov_names: Vec<String>,
                /// This layer's time mapping + whether it's a sequence (#99): the
                /// clip bar's span comes from the trim, and only a sequence layer is
                /// retimeable (a still spans all frames, so an offset is a no-op).
                trim: crate::layer::Trim,
                is_sequence: bool,
                /// The slot-A base track (#99 R3): the opened plate as the bottom
                /// layer. Clock-pinned (no retime) and non-removable via the panel.
                is_base: bool,
                /// The layer's pixel source, for the per-track cache-fill strip.
                source: Option<crate::layer::SourceId>,
            }
            let rows: Vec<Row> = self
                .comp_stack
                .iter()
                .map(|l| {
                    let (source, aov) = match &l.source {
                        crate::layer::LayerSource::Image { source, aov } => (Some(*source), *aov),
                        crate::layer::LayerSource::Adjustment => (None, 0),
                    };
                    // The base track is slot A itself (the master transport), not a
                    // decode follower, so its sequence-ness comes from the transport;
                    // a comp source's comes from its follower registration.
                    let is_base = source == Some(Self::A_SOURCE);
                    let is_sequence = if is_base {
                        self.playback.sequence.is_some()
                    } else {
                        source.is_some_and(|s| self.followers.contains_key(&s))
                    };
                    let aov_names = source
                        .and_then(|s| self.comp_sources.get(&s))
                        .map(|cs| {
                            cs.exr_data
                                .logical_layers
                                .iter()
                                .enumerate()
                                .map(|(i, ll)| {
                                    if ll.name.is_empty() {
                                        format!("layer {i}")
                                    } else {
                                        ll.name.clone()
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Row {
                        id: l.id,
                        name: l.name.clone(),
                        enabled: l.enabled,
                        solo: l.solo,
                        blend: l.blend,
                        opacity: l.opacity,
                        aov,
                        aov_names,
                        trim: l.trim,
                        is_sequence,
                        is_base,
                        source,
                    }
                })
                .collect();
            let count = rows.len();
            let solo_active = self.comp_stack.solo_active();
            let mut remove: Option<crate::layer::LayerId> = None;
            let mut duplicate: Option<crate::layer::LayerId> = None;
            let mut reorder: Option<(crate::layer::LayerId, usize)> = None;
            // A time-offset edit re-maps a layer's source frame, so re-request the
            // comp followers after the loop (the pump fetches the newly-needed frame).
            let mut offset_changed = false;

            // Vertical extent of the track lanes, for the playhead drawn across
            // all of them once the rows are laid out.
            let mut lanes: Option<egui::Rect> = None;

            // Display top-of-stack first: iterate high stack index → low.
            for (i, row) in rows.iter().enumerate().rev() {
                let (gutter, lane) = alloc_timeline_row(ui, axis_w, TIMELINE_ROW_H);
                lanes = Some(lanes.map_or(lane, |r: egui::Rect| r.union(lane)));
                // Whether this layer reaches the composite at all (disabled, or
                // hidden by a solo elsewhere) — greys both the name and the bar.
                let renders = if solo_active { row.solo } else { row.enabled };
                // A layer can be slid in time only if it's a sequence AND not the
                // base track (#99 R3): the base is the master clock, pinned at
                // offset 0 — retiming it would desync everything below.
                let retimeable = row.is_sequence && !row.is_base;

                // ── Gutter: visibility / solo / name / the ⋮ menu ──────────────
                let mut g = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(gutter)
                        .layout(egui::Layout::right_to_left(egui::Align::Center)),
                );
                g.set_clip_rect(gutter);
                // The ⋮ menu holds every control that doesn't earn permanent
                // gutter space; laid out right-to-left so it pins to the gutter's
                // right edge and the name gets whatever is left.
                g.menu_button("⋮", |ui| {
                    // Blend (unused for the bottom layer, which is a plain copy —
                    // the base of the composite has nothing beneath it).
                    ui.add_enabled_ui(i > 0, |ui| {
                        let mut blend = row.blend;
                        egui::ComboBox::from_id_salt((row.id, "blend"))
                            .selected_text(blend.label())
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                for mode in crate::viewer::BlendMode::ALL {
                                    ui.selectable_value(&mut blend, mode, mode.label());
                                }
                            });
                        if blend != row.blend
                            && let Some(l) = self.comp_stack.get_mut(row.id)
                        {
                            l.blend = blend;
                        }
                    });
                    // Opacity 0–100% (applies to every layer, including the bottom
                    // — the shader premultiplies its color by this).
                    ui.horizontal(|ui| {
                        ui.label("Opacity");
                        let mut opacity = row.opacity;
                        if ui
                            .add(
                                egui::DragValue::new(&mut opacity)
                                    .range(0.0..=1.0)
                                    .speed(0.01)
                                    .fixed_decimals(2),
                            )
                            .changed()
                            && let Some(l) = self.comp_stack.get_mut(row.id)
                        {
                            l.opacity = opacity;
                        }
                    });
                    // AOV picker: which logical layer (pass) of the source to show.
                    // Only for multi-layer EXRs — a single-beauty source has nothing
                    // to choose. Changing it rebuilds the source texture next frame
                    // (`ensure_comp_aov`).
                    if row.aov_names.len() > 1 {
                        let mut aov = row.aov.min(row.aov_names.len() - 1);
                        egui::ComboBox::from_id_salt((row.id, "aov"))
                            .selected_text(row.aov_names[aov].clone())
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                for (idx, nm) in row.aov_names.iter().enumerate() {
                                    ui.selectable_value(&mut aov, idx, nm);
                                }
                            });
                        if aov != row.aov
                            && let Some(l) = self.comp_stack.get_mut(row.id)
                            && let crate::layer::LayerSource::Image { aov: a, .. } = &mut l.source
                        {
                            *a = aov;
                        }
                    }
                    // Precise time offset, for typing an exact value where dragging
                    // the clip bar is the coarse gesture. Omitted where retiming is a
                    // no-op or forbidden: a still (all frames) or the base track (the
                    // clock, pinned at 0).
                    if retimeable {
                        ui.horizontal(|ui| {
                            ui.label("Offset");
                            let mut offset = row.trim.offset;
                            if ui
                                .add(egui::DragValue::new(&mut offset).speed(0.25).suffix(" f"))
                                .on_hover_text("Slide this layer along the timeline")
                                .changed()
                                && let Some(l) = self.comp_stack.get_mut(row.id)
                            {
                                l.trim.offset = offset;
                                offset_changed = true;
                            }
                        });
                    }
                    ui.separator();
                    // ⬆ moves toward the top of the composite (higher index); ⬇
                    // toward the bottom. Disabled at the ends.
                    ui.add_enabled_ui(i + 1 < count, |ui| {
                        if ui.button("⬆  Move up").clicked() {
                            reorder = Some((row.id, i + 1));
                            ui.close();
                        }
                    });
                    ui.add_enabled_ui(i > 0, |ui| {
                        if ui.button("⬇  Move down").clicked() {
                            reorder = Some((row.id, i - 1));
                            ui.close();
                        }
                    });
                    // The base track (the opened plate) isn't removed here — its
                    // close path is revived in the render-retire step (#99 R3); the
                    // File ▸ Close Image A menu that closed it was removed in R4.
                    // A second view of the same source (#242) — no decode, since the
                    // copy shares the original's `SourceId`. This is the explicit
                    // route to what re-opening a file used to do by accident.
                    //
                    // Not offered for the base track: it is the sole layer allowed to
                    // reference `A_SOURCE`, and a copy would not persist. Absent
                    // rather than disabled, matching how Remove layer treats the same
                    // row — a control the base track never has.
                    if !row.is_base
                        && ui
                            .button("⧉  Duplicate layer")
                            .on_hover_text(
                                "Add another layer on the same source — retime or \
                                 re-trim it independently. Costs no decode.",
                            )
                            .clicked()
                    {
                        duplicate = Some(row.id);
                        ui.close();
                    }
                    if !row.is_base && ui.button("✕  Remove layer").clicked() {
                        remove = Some(row.id);
                        ui.close();
                    }
                });
                // The remaining gutter width, left-to-right: visibility, solo, name.
                g.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // Visibility. Dimmed (but not disabled) while a solo is active,
                    // since solo overrides `enabled` in the composite.
                    let mut enabled = row.enabled;
                    if ui
                        .checkbox(&mut enabled, "")
                        .on_hover_text("Visible")
                        .changed()
                        && let Some(l) = self.comp_stack.get_mut(row.id)
                    {
                        l.enabled = enabled;
                    }
                    // Solo: isolate this layer (any solo hides non-soloed layers).
                    if ui
                        .selectable_label(row.solo, "S")
                        .on_hover_text("Solo")
                        .clicked()
                        && let Some(l) = self.comp_stack.get_mut(row.id)
                    {
                        l.solo = !row.solo;
                    }
                    // Name, greyed when it won't render. Truncated rather than
                    // wrapped: the gutter is a fixed width shared with every row.
                    // Clicking it makes this the "current" layer the viewport bar's
                    // AOV / channel controls + EXR Info act on (#99 R4); the current
                    // layer's name is bold.
                    let is_current = self.selected_comp_layer == Some(row.id);
                    let mut text = if renders {
                        egui::RichText::new(&row.name)
                    } else {
                        egui::RichText::new(&row.name).weak()
                    };
                    if is_current {
                        text = text.strong();
                    }
                    if ui
                        .add(
                            egui::Label::new(text)
                                .truncate()
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Click to make this the current layer")
                        .clicked()
                    {
                        self.selected_comp_layer = Some(row.id);
                    }
                });
                drop(g);

                // ── Clip bar on the shared time axis ──────────────────────────
                let Some((lo, hi)) = range else { continue };
                let axis = TimeAxis::new(lane, lo, hi);
                let painter = ui.painter_at(lane);
                let visuals = ui.visuals();
                let lane_bg = lane.shrink2(egui::vec2(0.0, 2.0));
                painter.rect_filled(lane_bg, 2.0, visuals.extreme_bg_color);

                let Some((g_lo, g_hi)) = track_span(row.trim, lo, hi) else {
                    // The layer is blank across the whole visible timeline. Mark
                    // the edge it ran off so it isn't silently missing.
                    let ran_off_the_start = i64::from(row.trim.out_point)
                        .saturating_sub(row.trim.offset)
                        < i64::from(lo);
                    let x = if ran_off_the_start {
                        lane.left() + 1.5
                    } else {
                        lane.right() - 1.5
                    };
                    painter.line_segment(
                        [
                            egui::pos2(x, lane_bg.top()),
                            egui::pos2(x, lane_bg.bottom()),
                        ],
                        egui::Stroke::new(3.0_f32, visuals.warn_fg_color),
                    );
                    continue;
                };

                // Clip extents use slot mapping (each frame owns an equal cell)
                // so a one-frame layer still has visible width; `max` keeps a
                // clip clamped to a sliver of the axis from vanishing entirely.
                let (x0, x1) = (axis.slot_x(g_lo), axis.slot_x(g_hi + 1));
                let bar = egui::Rect::from_min_max(
                    egui::pos2(x0, lane_bg.top()),
                    egui::pos2(x1.max(x0 + 3.0).min(lane.right()), lane_bg.bottom()),
                );
                let fill = if renders {
                    visuals.selection.bg_fill
                } else {
                    visuals.selection.bg_fill.gamma_multiply(0.35)
                };
                painter.rect_filled(bar, 2.0, fill);

                // Per-track cache-fill strip (#99): which of *this* layer's frames
                // are resident, in its own place on the global timeline. The T1
                // cache is `SourceId`-keyed, so this is the same two-tone readout
                // the ruler gives slot A — per layer. Source frames map to global
                // by `global = source - offset`, so a contiguous source run stays
                // contiguous. Sequence layers only (a still has no ring); the base
                // track reads slot A's own `frame_cache` (#99 R3).
                if row.is_sequence
                    && let Some(src) = row.source
                {
                    let strip_top = bar.bottom() - 3.0;
                    let clip = ui.painter_at(bar);
                    let to_global = |f: u32| {
                        let g = i64::from(f).saturating_sub(row.trim.offset);
                        (g >= i64::from(lo) && g <= i64::from(hi)).then_some(g as u32)
                    };
                    let paint_runs = |frames: &mut Vec<u32>, color: egui::Color32| {
                        for_each_frame_run(frames, |start, end| {
                            let seg = egui::Rect::from_min_max(
                                egui::pos2(axis.slot_x(i64::from(start)), strip_top),
                                egui::pos2(
                                    axis.slot_x(i64::from(end) + 1).min(bar.right()),
                                    bar.bottom(),
                                ),
                            );
                            clip.rect_filled(seg, 0.0, color);
                        });
                    };
                    let mut resident: Vec<u32> = self
                        .frame_cache
                        .resident_frames(src)
                        .filter_map(to_global)
                        .collect();
                    paint_runs(&mut resident, CACHE_PROXY_FILL);
                    let mut full: Vec<u32> = self
                        .frame_cache
                        .resident_full_frames(src)
                        .filter_map(to_global)
                        .collect();
                    paint_runs(&mut full, CACHE_FULL_FILL);
                }

                // ── Drag the clip to retime the layer ─────────────────────────
                // Only a retimeable clip drags; a still or the base track is hover-
                // only (offset is a no-op / forbidden).
                let resp = ui.interact(
                    bar,
                    ui.id().with(("track", row.id)),
                    if retimeable {
                        egui::Sense::click_and_drag()
                    } else {
                        egui::Sense::hover()
                    },
                );
                let resp = resp.on_hover_text(format!(
                    "{}\nframes {g_lo}–{g_hi}   offset {} f{}",
                    row.name,
                    row.trim.offset,
                    if retimeable {
                        "\ndrag to slide this layer along the timeline"
                    } else if row.is_base {
                        "\nbase plate — pinned to the transport"
                    } else {
                        ""
                    }
                ));
                if retimeable {
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                    }
                    if resp.drag_started()
                        && let Some(p) = resp.interact_pointer_pos()
                    {
                        self.track_drag = Some(TrackDrag {
                            id: row.id,
                            grab: axis.frame_at(p.x),
                            start_offset: row.trim.offset,
                        });
                    }
                    if resp.dragged()
                        && let Some(p) = resp.interact_pointer_pos()
                        && let Some(d) = self.track_drag
                        && d.id == row.id
                    {
                        let offset = offset_after_drag(d.start_offset, d.grab, axis.frame_at(p.x));
                        if let Some(l) = self.comp_stack.get_mut(row.id)
                            && l.trim.offset != offset
                        {
                            l.trim.offset = offset;
                            offset_changed = true;
                        }
                    }
                    if resp.drag_stopped() {
                        self.track_drag = None;
                        // Settle: the pump fetches whatever the final offset needs.
                        offset_changed = true;
                    }
                }
            }

            // Playhead across every lane (drawn last, on top of the bars) so the
            // tracks read as one timeline with the ruler above them.
            if let (Some(lanes), Some((lo, hi))) = (lanes, range) {
                let x = TimeAxis::new(lanes, lo, hi).x_of(i64::from(self.playback.current_frame));
                ui.painter().line_segment(
                    [egui::pos2(x, lanes.top()), egui::pos2(x, lanes.bottom())],
                    egui::Stroke::new(1.5_f32, ui.visuals().strong_text_color()),
                );
            }

            // Apply the structural edits after the loop (each restructures the Vec).
            if let Some((id, to)) = reorder {
                self.comp_stack.move_to(id, to);
            }
            if let Some(id) = duplicate {
                self.duplicate_comp_layer(id);
            }
            if let Some(id) = remove {
                self.remove_comp_layer(id);
            }
            // A time-offset edit changed a layer's mapping — re-request each comp
            // follower's newly-needed frame so the pump fetches it (#99).
            if offset_changed {
                self.sync_comp_followers();
            }
        }
    }

    /// Render the Layers-panel composite in the central canvas (#99 PR-B.3):
    /// resolve `comp_stack.composite_at` at the shared global frame, look up each
    /// draw's decoded source texture in `comp_sources`, and fold them bottom→top
    /// through the viewer's OCIO accumulate ping-pong. Requires OCIO active + a GPU
    /// (the ping-pong lives on the OCIO path); otherwise shows a hint — the N-layer
    /// non-OCIO composite is a follow-up. Assumes the panel is shown with a
    /// non-empty stack (the caller gates on that).
    fn draw_comp_central(&mut self, ui: &mut egui::Ui) {
        // Nuke-style current-layer control bar at the top of the viewport (#99 R4
        // follow-up): pick the current layer, page through its AOVs/passes, and
        // isolate channels — the classic single-image control row, restored for the
        // layer-stack path. Drawn before the composite so it sits above it.
        self.draw_comp_layer_bar(ui);

        // Mirror the per-frame viewer state the slot-A path sets before `viewer.ui`,
        // so the composite honors the same tone / OCIO / LUT settings.
        self.viewer.enable_lut = self.enable_lut && self.lut_bg.is_some();
        self.viewer.lut_domain_min = self.lut_domain_min;
        self.viewer.lut_domain_max = self.lut_domain_max;
        self.viewer.ocio_active = self.ocio_enabled && self.ocio_ready;
        self.viewer.ocio_render_gen = self.ocio_render_gen;

        // Resolve the model → concrete draws at the shared global playhead. A still
        // spans all frames (`Trim::full`) and binds its one texture; a sequence layer
        // (#99 Phase 2) resolves its `source_frame` here and is dropped by
        // `composite_at` outside its trim (blank there).
        let steps = self.comp_stack.composite_at(self.playback.current_frame);

        // Bring each source's texture current for what its draw wants: a sequence
        // layer rebinds its resolved source frame from the T1 cache (this is what
        // makes it *play*); a still just tracks AOV switches (#99 PR-B.4.3). Done
        // before the draw list so the bind below is up to date.
        for step in &steps {
            if let crate::layer::Step::Draw(d) = step {
                if d.source == Self::A_SOURCE {
                    // The slot-A base track (#99 R3): pixels from the live A frame.
                    self.ensure_base_frame(d.aov);
                } else if self.followers.contains_key(&d.source) {
                    self.ensure_comp_frame(d.source, d.source_frame, d.aov);
                } else {
                    self.ensure_comp_aov(d.source, d.aov);
                }
            }
        }

        // Bottom→top draw list from the decoded sources. A step whose source has no
        // texture (headless, or a not-yet-built AOV) is skipped; the bottom drawable
        // layer defines the shared canvas size.
        let mut draws: Vec<crate::viewer::CompDraw> = Vec::new();
        let mut base_size = (0usize, 0usize);
        // The bottom drawable layer defines the canvas, so its header pixel aspect
        // drives the anamorphic unsqueeze for the whole composite (#194 / #179).
        let mut base_par = 1.0_f32;
        for step in &steps {
            let crate::layer::Step::Draw(d) = step else {
                continue; // Adjustment layers (#102) don't render yet.
            };
            let Some(cs) = self.comp_sources.get(&d.source) else {
                continue;
            };
            let Some(bind_group) = cs.bind_group.clone() else {
                continue;
            };
            if draws.is_empty() {
                base_size = cs.size;
                base_par = cs.exr_data.image.attributes.pixel_aspect;
            }
            draws.push(crate::viewer::CompDraw {
                bind_group,
                blend: d.blend,
                opacity: d.opacity,
                par: cs.exr_data.image.attributes.pixel_aspect,
            });
        }

        // Pixel readout (#99 R4) targets the topmost drawable layer under the cursor
        // — the honest analogue of the A/B path, which samples raw source pixels, not
        // the composited result. Record it independent of hover (so the status-bar
        // row persists) and clone its pixels (cheap Arc) so the sample below doesn't
        // borrow `self` across the GPU draw.
        let top = top_sample_source(&steps, |s| {
            self.comp_sources
                .get(&s)
                .is_some_and(|c| c.bind_group.is_some())
        });
        let top_exr =
            top.and_then(|(s, aov)| self.comp_sources.get(&s).map(|c| (c.exr_data.clone(), aov)));

        // A compare arrangement (#99 Slice 2a) shows the **two layers themselves**, not
        // the composite: pane A is the `Layer:` (current) layer, pane B the `vs:` one,
        // each drawn alone. Comparing a layer against a composite that already contains
        // it just showed the same content twice. Both must be drawable at this frame
        // (not hidden / soloed out / trimmed blank / textureless) or we fall back to
        // `Stacked` — better the plain composite than an empty pane.
        let drawable = |s: crate::layer::SourceId| {
            self.comp_sources
                .get(&s)
                .is_some_and(|c| c.bind_group.is_some())
        };
        let compare = self.comp_arrangement != crate::layer::Arrangement::Stacked;
        let side_a_draw = compare
            .then(|| comp_layer_draw(&steps, self.active_comp_layer(), drawable))
            .flatten();
        let side_b_draw = compare
            .then(|| comp_layer_draw(&steps, self.compare_b(), drawable))
            .flatten()
            // Both panes or neither: a compare with only one resolvable side is a
            // fallback to `Stacked`, not a half-drawn split.
            .filter(|_| side_a_draw.is_some());
        let side_b = side_b_draw.and_then(|d| {
            let cs = self.comp_sources.get(&d.source)?;
            Some(crate::viewer::CompSideB {
                draw: crate::viewer::CompDraw {
                    bind_group: cs.bind_group.clone()?,
                    blend: d.blend,
                    opacity: d.opacity,
                    // Pane B is placed by `CompSideB::par` below, not through the
                    // accumulate loop's relative correction — carried for consistency.
                    par: cs.exr_data.image.attributes.pixel_aspect,
                },
                tex_size: egui::vec2(cs.size.0.max(1) as f32, cs.size.1.max(1) as f32),
                par: cs.exr_data.image.attributes.pixel_aspect,
            })
        });
        // The compare layer's own pixels, for the per-pane readout.
        self.comp_readout_b = side_b_draw.map(|d| (d.source, d.aov));
        let side_b_exr = side_b_draw.and_then(|d| {
            self.comp_sources
                .get(&d.source)
                .map(|c| (c.exr_data.clone(), d.aov))
        });

        // With the compare live, pane A is the current layer *alone*, so replace the
        // composite draw list with that one layer (and take the canvas size / PAR from
        // it). Side B is already a lone placed draw, so both panes are single layers.
        let arrangement = match (side_a_draw, side_b.is_some()) {
            (Some(a), true) => {
                if let Some(cs) = self.comp_sources.get(&a.source)
                    && let Some(bind_group) = cs.bind_group.clone()
                {
                    draws = vec![crate::viewer::CompDraw {
                        bind_group,
                        blend: a.blend,
                        opacity: a.opacity,
                        // Pane A *is* the base in a compare, so its relative
                        // correction is 1 by construction — same PAR both sides.
                        par: cs.exr_data.image.attributes.pixel_aspect,
                    }];
                    base_size = cs.size;
                    base_par = cs.exr_data.image.attributes.pixel_aspect;
                }
                self.comp_arrangement
            }
            _ => crate::layer::Arrangement::Stacked,
        };
        // Pane A's readout follows pane A: the current layer in compare mode, the
        // composite's topmost drawable layer otherwise.
        let (top, top_exr) = match (side_a_draw, arrangement) {
            (Some(a), arr) if arr != crate::layer::Arrangement::Stacked => (
                Some((a.source, a.aov)),
                self.comp_sources
                    .get(&a.source)
                    .map(|c| (c.exr_data.clone(), a.aov)),
            ),
            _ => (top, top_exr),
        };
        self.comp_readout = top;

        // The composite renders in any color mode (#99 R2): OCIO on → the OCIO
        // display transform; OCIO off → the sRGB display-encode pass. Only a GPU
        // and a drawable layer are required.
        if !draws.is_empty()
            && let Some(gpu) = self.gpu_resources.as_ref()
        {
            let lut = self.lut_bg.clone();
            self.viewer.draw_comp_composite(
                ui,
                base_size,
                base_par,
                &draws,
                arrangement,
                side_b,
                gpu,
                lut,
            );
        } else {
            // An empty composite has several causes and they need different actions,
            // so say which one it is. Reporting "add a source" while the panel shows
            // "Layers 2/6" is actively misleading — it sent a dogfood session hunting
            // a decode regression when both layers had simply been toggled off.
            let image_layers = |it: &mut dyn Iterator<Item = &crate::layer::Layer>| {
                it.filter(|l| matches!(l.source, crate::layer::LayerSource::Image { .. }))
                    .count()
            };
            let total = image_layers(&mut self.comp_stack.iter());
            let visible = image_layers(&mut self.comp_stack.visible());
            let owned;
            let msg: &str = if self.gpu_resources.is_none() {
                "No GPU: the compositing viewport is unavailable."
            } else if total == 0 {
                "Add a source to the Layers panel to begin."
            } else if visible == 0 {
                owned = if self.comp_stack.solo_active() {
                    "Every layer with a source is hidden by a solo. Clear the solo to \
                     see the composite."
                        .to_string()
                } else {
                    format!(
                        "All {total} layers are hidden. Toggle one visible in the \
                         Layers panel to see the composite."
                    )
                };
                &owned
            } else {
                // Visible layers exist but none covers this frame — a trim or time
                // offset, which is invisible on screen unless said aloud.
                owned = format!(
                    "No visible layer covers frame {}. Move the playhead into a \
                     layer's span, or clear its time offset.",
                    self.playback.current_frame
                );
                &owned
            };
            ui.centered_and_justified(|ui| {
                ui.label(msg);
            });
        }

        // Sample after the draw so `last_image_rect` is this frame's, and after the
        // `gpu_resources` borrow above is released. Unconditional: no drawable / no
        // hover / suppressed clears the readout (status bar then shows `x=--`).
        self.sample_comp_readout(ui, top_exr, side_b_exr);
    }

    /// Populate the pixel-readout fields (`last_hover_pos_img` / `last_sampled_val_a`)
    /// from the cursor over the composite (#99 R4). `top` is the topmost drawable
    /// layer's pixels + aov; `side_b` is the current layer's, sampled instead when the
    /// cursor is over the Side-by-Side compare pane (#99 Slice 2a) so each pane reports
    /// its *own* values. Clears the readout when playback suppresses sampling, the
    /// pointer is off the image, or there is no drawable layer.
    fn sample_comp_readout(
        &mut self,
        ui: &egui::Ui,
        top: Option<(std::sync::Arc<ExrData>, usize)>,
        side_b: Option<(std::sync::Arc<ExrData>, usize)>,
    ) {
        let suppressed = self.playback.sampling_suppressed();
        let hover = if suppressed {
            None
        } else {
            ui.input(|i| i.pointer.hover_pos())
        };
        // Resolve which pane the cursor is over, then sample that pane's own source
        // against its own rect. Outside Side-by-Side `last_image_rect_b` is `None`, so
        // this collapses to the single-composite case.
        let rect_a = self.viewer.last_image_rect;
        let rect_b = self.viewer.last_image_rect_b;
        let wipe = self.viewer.last_wipe;
        let blink_b = self.viewer.last_blink_b;
        let picked = hover
            .zip(rect_a)
            .and_then(|(pos, ir)| crate::viewer::comp_hover_side(pos, ir, rect_b, wipe, blink_b));
        // Name the status-bar row after the pane actually sampled, not always the
        // composite's top layer.
        if picked == Some(crate::viewer::CompSide::B)
            && let Some(b) = self.comp_readout_b
        {
            self.comp_readout = Some(b);
        }
        // The `bool` is "this pane went through the accumulate loop's per-layer
        // unsqueeze" (#254). Only pane A does: pane B is placed by `CompSideB::par`,
        // already at its own aspect, and Wipe binds both layers to pane A's rect
        // without a per-layer correction. Applying the ratio there would
        // double-correct.
        let side = match picked {
            Some(crate::viewer::CompSide::A) => rect_a.map(|ir| (ir, &top, true)),
            // Side-by-Side gives pane B its own rect; Wipe overlays both layers in
            // pane A's rect, so B is sampled against that one.
            Some(crate::viewer::CompSide::B) => rect_b.or(rect_a).map(|ir| (ir, &side_b, false)),
            None => None,
        };
        let sampled = match (side, hover) {
            (Some((ir, Some((exr, aov)), relative)), Some(pos)) => {
                let size = exr.logical_size(*aov).unwrap_or((0, 0));
                // Sample against the rect this layer actually drew at. The canvas is
                // unsqueezed for the base layer, so a differently-squeezed layer sits
                // in a horizontally scaled rect — normalizing across the canvas
                // instead would slide the readout off the cursor by that ratio.
                let ir = if relative {
                    let rel = crate::viewer::relative_unsqueeze(
                        self.viewer
                            .sanitize_unsqueeze(exr.image.attributes.pixel_aspect),
                        self.viewer.sanitize_unsqueeze(self.viewer.last_base_par),
                    );
                    crate::viewer::layer_draw_rect(ir, rel)
                } else {
                    ir
                };
                comp_hover_pixel(pos, ir, size)
                    .map(|(x, y)| (x, y, self.viewer.sample_pixel(exr, *aov, x, y)))
            }
            _ => None,
        };
        match sampled {
            Some((x, y, v)) => {
                self.viewer.last_hover_pos_img = Some((x, y));
                self.viewer.last_sampled_val_a = v;
                // The floating cursor tooltip (#99 Slice 3d): another casualty of the
                // R4 collapse — it lived in `viewer.ui`'s `handle_pixel_sampling`.
                // Reuses that window verbatim via `pixel_tooltip_window`, with no B
                // value: the comp readout samples whichever single pane the cursor is
                // over, so a two-value A/B block would be meaningless here.
                if self.viewer.show_tooltip
                    && let Some(pos) = hover
                {
                    crate::viewer::pixel_tooltip_window(ui.ctx(), pos, x, y, v, None);
                }
                // Shift+Click saves the sampled pixel as a persistent swatch — the
                // comp-path analogue of the classic sampler (#99 R4), which lived in
                // `viewer.ui` and so never fired here. `v` is the raw source value
                // under the cursor; checks `shift` only, so Shift+Ctrl+Click works too.
                if let Some(rgba) = v
                    && ui.input(|i| i.modifiers.shift && i.pointer.primary_clicked())
                {
                    self.viewer.swatches.push(rgba);
                }
            }
            None => {
                self.viewer.last_hover_pos_img = None;
                self.viewer.last_sampled_val_a = None;
            }
        }
    }

    /// a grid of that source's AOVs/passes, where clicking one points the layer's
    /// `aov` at it — the comp analogue of the classic sheet's "select this layer".
    /// Returns `false` when there is nothing to show, so the caller falls through to
    /// the composite rather than rendering a blank viewport.
    ///
    /// Owns the per-frame thumbnail housekeeping that `ExrViewer::ui` used to run:
    /// `sync_texture_caches` (which sizes the caches the sheet indexes **unguarded**),
    /// the deferred GPU-id drain, and the OCIO-change invalidation.
    fn draw_comp_contact_sheet(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(layer_id) = self.active_comp_layer() else {
            return false;
        };
        let Some(crate::layer::LayerSource::Image { source, .. }) =
            self.comp_stack.get(layer_id).map(|l| l.source.clone())
        else {
            return false;
        };
        let Some(exr) = self.comp_sources.get(&source).map(|cs| cs.exr_data.clone()) else {
            return false;
        };

        // The per-frame app→viewer state the sheet's tone snapshot reads.
        self.viewer.enable_lut = self.enable_lut && self.lut_bg.is_some();
        self.viewer.lut_domain_min = self.lut_domain_min;
        self.viewer.lut_domain_max = self.lut_domain_max;
        self.viewer.ocio_active = self.ocio_enabled && self.ocio_ready;
        self.viewer.ocio_render_gen = self.ocio_render_gen;
        self.viewer.suppress_sampling = self.playback.sampling_suppressed();

        self.viewer.invalidate_thumbnails_on_ocio_change();
        self.viewer.drain_thumb_frees(self.gpu_resources.as_ref());
        self.viewer.sync_texture_caches(exr.logical_layers.len());

        // Header: which source is on show, and an explicit way out. The sheet replaces
        // the whole viewport (the comp bar that hosts the toggle isn't drawn), so
        // without this the only exits are clicking a cell or finding the View menu.
        let name = self
            .comp_stack
            .get(layer_id)
            .map(|l| l.name.clone())
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.heading(&name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("✕ Close")
                    .on_hover_text("Back to the composite")
                    .clicked()
                {
                    self.viewer.show_contact_sheet = false;
                }
            });
        });
        ui.separator();

        let lut = self.lut_bg.clone();
        let picked =
            self.viewer
                .draw_contact_sheet(ui, &exr, self.gpu_resources.as_ref(), lut.as_ref());

        // Point the current layer at the chosen AOV, the comp-model equivalent of the
        // classic sheet's `active_layer` write.
        if let Some(aov) = picked
            && let Some(l) = self.comp_stack.get_mut(layer_id)
            && let crate::layer::LayerSource::Image { aov: a, .. } = &mut l.source
        {
            *a = aov;
        }
        true
    }

    fn draw_central_canvas(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            // Viewer shortcuts run here, *before* the branch, so they work on the
            // contact-sheet path too — otherwise `T` could open the sheet but never
            // close it, and F11 / Esc would die at its edge. Exactly one call site:
            // servicing them in both branches would toggle each key twice per press.
            self.viewer.handle_channel_hotkeys(ui);

            // The comp stack IS the viewport (#99 Slice 3h): the unified open/drop flow
            // adds every file to it, so this is the only path. The `show_layers_panel`
            // toggle hides only the timeline *tracks* (see `draw_timeline_panel`), not
            // the composite, so toggling it off shows the composite full-screen.
            //
            // Contact Sheet (#191) is a single-source, per-layer/pass grid over the
            // current layer, so it takes priority over the composite.
            if self.viewer.show_contact_sheet && self.draw_comp_contact_sheet(ui) {
                return;
            }
            if !self.comp_stack.is_empty() {
                self.draw_comp_central(ui);
                return;
            }
            ui.centered_and_justified(|ui| {
                ui.label("Open an EXR file to begin.");
            });
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

    // --- Timeline panel: the shared frame↔x axis (#99) -----------------------
    // The ruler and every layer clip bar map frames through one `TimeAxis`, so
    // these are the guarantees that make a bar line up with the ruler above it.

    fn axis(lo: u32, hi: u32) -> TimeAxis {
        TimeAxis::new(
            egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(200.0, 22.0)),
            lo,
            hi,
        )
    }

    #[test]
    fn time_axis_maps_the_range_across_the_rect_and_clamps_outside_it() {
        let a = axis(1000, 1100);
        assert!((a.x_of(1000) - 100.0).abs() < 0.01, "lo at the left edge");
        assert!((a.x_of(1100) - 300.0).abs() < 0.01, "hi at the right edge");
        assert!(
            (a.x_of(1050) - 200.0).abs() < 0.01,
            "midpoint at the center"
        );
        // A layer dragged off either end must paint against the edge, never
        // outside the panel.
        assert!((a.x_of(500) - 100.0).abs() < 0.01, "before lo clamps left");
        assert!((a.x_of(9999) - 300.0).abs() < 0.01, "after hi clamps right");
        // A single-frame sequence has no span to divide by: it sits centered.
        assert!((axis(7, 7).x_of(7) - 200.0).abs() < 0.01);
    }

    #[test]
    fn time_axis_frame_at_round_trips_x_of() {
        let a = axis(1000, 1100);
        for f in [1000, 1001, 1037, 1099, 1100] {
            assert_eq!(a.frame_at(a.x_of(f)), f, "round trip at {f}");
        }
        // Scrubbing past either end lands on the end frame, not out of range —
        // `draw_timeline` casts the result to `u32` on that guarantee.
        assert_eq!(a.frame_at(-500.0), 1000);
        assert_eq!(a.frame_at(9999.0), 1100);
    }

    #[test]
    fn slot_x_gives_the_last_frame_a_real_cell() {
        // Cache-fill strips and clip bars give each frame an equal cell over
        // `lo..=hi` *inclusive*, so a one-frame clip still has visible width and
        // frame `hi`'s cell reaches the right edge.
        let a = axis(0, 3); // 4 frames ⇒ 4 cells of 50 px
        assert!((a.slot_x(0) - 100.0).abs() < 0.01);
        assert!((a.slot_x(1) - 150.0).abs() < 0.01);
        assert!((a.slot_x(3) - 250.0).abs() < 0.01);
        assert!(
            (a.slot_x(4) - 300.0).abs() < 0.01,
            "hi's cell ends at the edge"
        );
    }

    // --- Timeline panel: clip spans + drag arithmetic (#99) ------------------

    #[test]
    fn track_span_maps_the_trim_into_global_frames() {
        use crate::layer::Trim;
        // `source = global + offset`, so the bar sits at `source - offset`.
        let t = Trim {
            in_point: 10,
            out_point: 20,
            offset: 0,
        };
        assert_eq!(track_span(t, 0, 100), Some((10, 20)));
        // A positive offset makes the layer *lead* the playhead, so its clip
        // moves earlier on the global timeline; a negative one moves it later.
        assert_eq!(track_span(Trim { offset: 5, ..t }, 0, 100), Some((5, 15)));
        assert_eq!(track_span(Trim { offset: -5, ..t }, 0, 100), Some((15, 25)));
        // Partly visible still draws (the bar clamps at the edge)...
        assert_eq!(track_span(t, 15, 100), Some((10, 20)));
        // ...but a layer entirely off the visible timeline has no bar at all.
        assert_eq!(track_span(t, 50, 100), None, "ends before the range");
        assert_eq!(track_span(t, 0, 5), None, "starts after the range");
    }

    #[test]
    fn track_span_handles_a_stills_all_frames_trim() {
        // A still is `Trim::full(0, u32::MAX)`: its bar spans the whole timeline
        // and the i64 arithmetic must not overflow at the u32 ceiling.
        let t = crate::layer::Trim::full(0, u32::MAX);
        let (lo, hi) = track_span(t, 1000, 1100).expect("a still is always visible");
        assert!(lo <= 1000 && hi >= 1100);
    }

    #[test]
    fn dragging_a_clip_right_moves_it_later_on_the_timeline() {
        // Grab frame 20 on a layer at offset 0 and drop it on frame 30: the clip
        // moved 10 frames later, so the offset drops by 10 (`global = source -
        // offset`). Dragging back left is the exact inverse.
        assert_eq!(offset_after_drag(0, 20, 30), -10);
        assert_eq!(offset_after_drag(0, 20, 10), 10);
        // Re-deriving from the grab frame (rather than accumulating per-event
        // deltas) means a jittery drag that returns to where it started restores
        // the original offset exactly — no drift.
        let start = 7;
        for now in [20, 25, 31, 24, 20] {
            let landed = offset_after_drag(start, 20, now);
            assert_eq!(landed, start - (now - 20));
        }
        assert_eq!(offset_after_drag(start, 20, 20), start);
    }

    // --- Timeline panel: layout (#99) ----------------------------------------

    #[test]
    fn timeline_panel_lays_out_tracks_for_sequence_and_still_layers() {
        use egui_kittest::Harness;
        let dir = tempfile::tempdir().unwrap();
        // The two track shapes the panel has to lay out: a comp *sequence* (two
        // numbered frames ⇒ `detect_from_file` finds a range, so the layer gets a
        // real `[in, out]` clip) and a lone *still* (`Trim::full`, spanning the
        // whole axis).
        let s1 = dir.path().join("seq.0001.exr");
        let s2 = dir.path().join("seq.0002.exr");
        let still = dir.path().join("still.exr");
        for p in [&s1, &s2, &still] {
            write_rgba_exr(p);
        }

        let mut app = ExrApp::default();
        app.add_comp_source(s1);
        app.add_comp_source(still);
        assert_eq!(app.comp_stack.len(), 2);
        assert!(
            app.playback.is_active(),
            "the first comp sequence takes the clock (#99 R4-lite), so the panel \
             draws its transport + ruler as well as the tracks"
        );

        let mut h = Harness::new_ui_state(|ui, app: &mut ExrApp| app.draw_timeline_panel(ui), app);
        h.run_steps(1);
        let app = std::mem::take(h.state_mut());
        // Laying out the tracks must not disturb the model.
        assert_eq!(app.comp_stack.len(), 2, "drawing is not an edit");
    }

    #[test]
    fn timeline_panel_draws_gutter_only_tracks_without_a_transport() {
        // A stills-only stack has no sequence, so `full_range()` is `None` and
        // there is no time axis to place clips on. The rows must still lay out
        // (gutter-only, as the old flat list did) rather than panicking on a
        // missing range.
        use egui_kittest::Harness;
        let dir = tempfile::tempdir().unwrap();
        let still = dir.path().join("still.exr");
        write_rgba_exr(&still);

        let mut app = ExrApp::default();
        app.add_comp_source(still);
        assert!(
            !app.playback.is_active(),
            "a still establishes no transport"
        );
        assert!(app.playback.full_range().is_none());

        let mut h = Harness::new_ui_state(|ui, app: &mut ExrApp| app.draw_timeline_panel(ui), app);
        h.run_steps(1);
        assert_eq!(h.state().comp_stack.len(), 1);
    }

    // --- Layers panel: decode-on-add (#99 PR-B.2) ----------------------------
    // Headless (no `gpu_resources`): the model layer + decoded pixels register,
    // but no GPU texture is built. The composite-render half is on-device (PR-B.3).

    #[test]
    fn add_comp_source_decodes_and_registers_a_source() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("layer.exr");
        write_rgba_exr(&f);

        let mut app = ExrApp::default();
        app.add_comp_source(f);

        // One model layer, at AOV 0, referencing the first comp source id
        // (COMP_SOURCE_BASE = 2; ids 0/1 are reserved for the A/B compare slots).
        assert_eq!(app.comp_stack.len(), 1, "layer registered");
        assert_eq!(
            app.comp_next_source,
            COMP_SOURCE_BASE + 1,
            "SourceId allocator advanced"
        );
        let layer = app.comp_stack.iter().next().unwrap();
        let crate::layer::LayerSource::Image { source, aov } = layer.source else {
            panic!("expected an image layer");
        };
        assert_eq!((source, aov), (crate::layer::SourceId(COMP_SOURCE_BASE), 0));
        assert!(
            source != ExrApp::A_SOURCE,
            "comp source must not alias the base-plate slot"
        );

        // The decoded pixels are stored keyed by that source; no GPU texture
        // headless, but the model + size are populated.
        let cs = app
            .comp_sources
            .get(&source)
            .expect("source decoded + stored");
        assert_eq!(cs.size, (2, 2), "2×2 fixture dims");
        assert_eq!(cs.aov, 0);
        assert_eq!(cs.exr_data.logical_layers.len(), 1, "beauty layer decoded");
        assert!(cs.bind_group.is_none(), "no GPU texture headless");
    }

    #[test]
    fn add_comp_source_rejects_a_bad_path_without_touching_the_stack() {
        let mut app = ExrApp::default();
        // A path that can't decode: nothing is added and the allocator is untouched,
        // so no layer is left dangling at a source with no pixels.
        app.add_comp_source(std::path::PathBuf::from("/nonexistent/does-not-exist.exr"));
        assert!(app.comp_stack.is_empty(), "no layer on decode failure");
        assert!(app.comp_sources.is_empty(), "no source stored");
        assert_eq!(
            app.comp_next_source, COMP_SOURCE_BASE,
            "SourceId not consumed"
        );
        assert!(app.error_msg.is_some(), "error surfaced to the status bar");
    }

    #[test]
    fn remove_comp_layer_frees_its_orphaned_source() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("layer.exr");
        write_rgba_exr(&f);

        let mut app = ExrApp::default();
        app.add_comp_source(f);
        let id = app.comp_stack.iter().next().unwrap().id;
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        assert!(app.comp_sources.contains_key(&source));

        app.remove_comp_layer(id);
        assert!(app.comp_stack.is_empty(), "layer removed");
        assert!(
            !app.comp_sources.contains_key(&source),
            "orphaned source freed"
        );
        // Ids are never reused, so a re-add can't alias the freed source.
        assert_eq!(
            app.comp_next_source,
            COMP_SOURCE_BASE + 1,
            "allocator not rewound"
        );
    }

    #[test]
    fn active_comp_layer_tracks_selection_with_a_top_fallback() {
        // The Nuke-style "current layer" (#99 R4): adding a layer selects it; an
        // explicit selection wins; removing the selected layer falls back to the
        // top of the stack; an empty stack has no current layer.
        let dir = tempfile::tempdir().unwrap();
        let (f0, f1) = (dir.path().join("a.exr"), dir.path().join("b.exr"));
        write_rgba_exr(&f0);
        write_rgba_exr(&f1);

        let mut app = ExrApp::default();
        assert_eq!(
            app.active_comp_layer(),
            None,
            "empty stack → no current layer"
        );

        app.add_comp_source(f0);
        let bottom = app.comp_stack.iter().next().unwrap().id;
        assert_eq!(
            app.selected_comp_layer,
            Some(bottom),
            "adding a layer selects it"
        );

        app.add_comp_source(f1);
        // `iter()` is bottom→top, so the second add is the top (last) layer.
        let top = app.comp_stack.iter().last().unwrap().id;
        assert_eq!(
            app.active_comp_layer(),
            Some(top),
            "the newest layer is current"
        );

        // An explicit selection of the bottom layer wins over the top fallback.
        app.selected_comp_layer = Some(bottom);
        assert_eq!(app.active_comp_layer(), Some(bottom));

        // Removing the selected layer drops the selection; the resolver then
        // falls back to the topmost remaining layer.
        app.remove_comp_layer(bottom);
        assert_eq!(app.selected_comp_layer, None, "stale selection cleared");
        assert_eq!(
            app.active_comp_layer(),
            Some(top),
            "falls back to the top of the stack"
        );
    }

    #[test]
    fn comp_histogram_entry_point_computes_bins() {
        // The comp-path histogram (#99 R4) computes the current layer's bins keyed
        // by its SourceId, and — unlike the classic A/B path — never populates B.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("layer.exr");
        write_rgba_exr(&f);
        let data = std::sync::Arc::new(ExrData::load(&f).unwrap());

        let mut v = crate::viewer::ExrViewer::default();
        v.calculate_histogram_for(&data, 0, 2);

        let bins = v.histogram.expect("comp bins computed");
        assert_eq!(
            bins.iter().sum::<u32>(),
            4,
            "every pixel of the 2×2 fixture lands in exactly one bin"
        );
    }

    #[test]
    fn add_comp_source_registers_a_sequence_follower() {
        // A numbered file is detected as a sequence: the comp source becomes a
        // decode follower on the shared playhead (#99 Phase 2), its layer's Trim
        // spans the sequence range, and the opened frame is seeded into the T1
        // cache under the comp source id — never the A/B slots.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());

        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        let layer = app.comp_stack.iter().next().unwrap();
        assert_eq!(
            (layer.trim.in_point, layer.trim.out_point),
            (1, 5),
            "trim spans the detected sequence range, not the still's all-frames trim"
        );

        let f = app
            .followers
            .get(&source)
            .expect("sequence source registered as a follower");
        assert!(
            f.sequence.is_some(),
            "follower carries the detected sequence"
        );
        assert_eq!(
            f.current_frame, 1,
            "follower opens on the sequence's frame 1"
        );

        assert!(
            app.frame_cache.contains(source, 1),
            "opened frame seeded under the comp source id"
        );
        assert!(
            !app.frame_cache.contains(ExrApp::A_SOURCE, 1),
            "comp frame must not land in the base-plate slot"
        );
    }

    /// Deliver a frame to an arbitrary source as the worker would, at the live
    /// epoch — the comp-follower counterpart of `deliver_frame` (which is hardwired
    /// to `A_SOURCE`).
    fn deliver_source_frame(
        app: &mut ExrApp,
        source: crate::layer::SourceId,
        path: &std::path::Path,
        frame: u32,
    ) {
        let data = ExrData::load(path).unwrap();
        app.apply_load_result(LoadResult {
            source,
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: false,
            result: Ok(data),
        });
    }

    #[test]
    fn comp_transport_paces_off_the_clock_driving_follower() {
        // #100 finding E: `note_shown` fired only under `is_primary`, and the A-path
        // cache-hit call sits *after* `request_sequence_frame`'s comp early return.
        // So in 1.12.0's default path (open/drop = add a layer → the comp stack owns
        // the clock) nothing ever recorded a shown frame: `measured_fps` — the HUD's
        // headline number and the input to the frame-time percentiles — stayed 0.0.
        //
        // Driven through `note_display`, the seam `ensure_comp_frame` calls on a
        // texture swap. The swap itself needs a device, so under the GPU-free test
        // convention the *wiring* into the paint path is covered by inspection, not
        // here; what this pins is the routing — which source paces and which doesn't.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());

        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        assert_eq!(
            app.transport_source,
            Some(source),
            "the added sequence claimed the clock, and by id"
        );
        assert_eq!(app.clock_source(), source);

        // Playing, not stepping. Pacing measures the *clock's* rate, so a stepped
        // frame is deliberately not a frame time (#236) — the interval between two
        // steps is however long the user looked at the first one. What is under
        // test here is that a comp source's `note_display` reaches the transport
        // at all, which the play path exercises just as well.
        app.playback_toggle();
        assert!(app.playback.is_playing());
        for frame in 2..=5u32 {
            app.note_display(source, frame);
        }

        assert!(
            app.playback.measured_fps > 0.0,
            "the comp transport is paced, not stuck at 0.0"
        );
        assert_eq!(
            app.playback.frame_time_samples(),
            3,
            "four displayed frames are three intervals — no double counting"
        );
        assert!(app.playback.frame_time_pcts().is_some());
    }

    #[test]
    fn pacing_ignores_a_resident_frame_that_was_never_displayed() {
        // The defect that made the metric lie (#204): pacing used to fire when a
        // frame was resident at advance time, or when a decode landed. Under load
        // neither implies the pixels ever reached the screen — the playhead moves on
        // and `ensure_comp_frame` keeps holding the old texture. That reported
        // healthy frame times through runs where the picture was frozen for seconds.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        // Walk the playhead and land decodes for every frame — residency and
        // arrival both happen, display does not.
        for (i, path) in paths.iter().enumerate().skip(1) {
            app.playback_step(1);
            deliver_source_frame(&mut app, source, path, u32::try_from(i).unwrap() + 1);
            assert!(
                app.frame_cache
                    .contains(source, u32::try_from(i).unwrap() + 1)
            );
        }

        assert_eq!(
            app.playback.frame_time_samples(),
            0,
            "nothing was painted, so nothing is paced"
        );
        assert_eq!(app.playback.measured_fps, 0.0);
    }

    #[test]
    fn the_decode_slot_rotates_across_sources_instead_of_starving_them() {
        // #204: there is one decode in flight globally and `pump_decode` returns
        // after submitting one job, so a fixed source order starved everything after
        // the first source that always wants something. The clock source's playhead
        // advances every ~42 ms while a decode takes ~270 ms, so it wanted a frame on
        // every pump and took every slot — measured with 5 layers, four of them never
        // reached the P0 pass and displayed frame 1 for 30 seconds.
        let (_dir, paths) = write_sequence(10);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());
        let sources: Vec<_> = (0..3)
            .map(|i| crate::layer::SourceId(COMP_SOURCE_BASE + i))
            .collect();

        app.playback.state = PlayState::Playing;
        app.playback_step(1); // every follower now wants a non-resident frame

        // Drain the single slot repeatedly, recording which source each job went to.
        // Completing with an *error* clears the wait state without caching anything,
        // so every source keeps wanting a frame — the starvation regime exactly.
        let mut served: std::collections::HashMap<crate::layer::SourceId, usize> =
            std::collections::HashMap::new();
        for _ in 0..30 {
            app.pump_decode();
            let Some((src, frame)) = app
                .followers
                .iter()
                .find_map(|(id, st)| st.inflight.iter().next().map(|f| (*id, *f)))
            else {
                break;
            };
            *served.entry(src).or_default() += 1;
            app.apply_load_result(LoadResult {
                source: src,
                seq_frame: true,
                frame,
                epoch: app.playback.epoch,
                open_gen: 0,
                fell_back: false,
                result: Err("stub".to_string()),
            });
        }

        for s in &sources {
            assert!(
                served.get(s).copied().unwrap_or(0) > 0,
                "every source must get decode turns, but {s:?} got none: {served:?}"
            );
        }
    }

    #[test]
    fn a_trailing_follower_does_not_pace_the_transport() {
        // Only the clock source counts as a transport tick — a second layer painting
        // is not a displayed transport frame, and letting it pace would make the
        // frame-time ring report layer count as speed.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());

        let clock = crate::layer::SourceId(COMP_SOURCE_BASE);
        let trailing = crate::layer::SourceId(COMP_SOURCE_BASE + 1);
        assert_eq!(
            app.clock_source(),
            clock,
            "the *first* sequence is the clock"
        );

        app.playback_step(1);
        app.note_display(trailing, 2);
        app.note_display(trailing, 3);
        assert_eq!(
            app.playback.frame_time_samples(),
            0,
            "a trailing layer's paints are not transport ticks"
        );
    }

    #[test]
    fn removing_the_clock_source_repoints_the_transport() {
        // With the transport identified by id rather than a bare flag, removing the
        // clock-driving layer while others remain must re-point it — a dangling id
        // would silently kill pacing and T1 sizing (#100).
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());

        let clock = crate::layer::SourceId(COMP_SOURCE_BASE);
        let other = crate::layer::SourceId(COMP_SOURCE_BASE + 1);
        assert_eq!(app.clock_source(), clock);

        // Remove the layer backed by the clock source.
        let id = app
            .comp_stack
            .iter()
            .find(
                |l| matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == clock),
            )
            .map(|l| l.id)
            .expect("the clock source's layer");
        app.remove_comp_layer(id);

        assert_eq!(
            app.transport_source,
            Some(other),
            "the clock re-points at a surviving follower"
        );
        assert!(app.playback.is_active(), "the transport keeps running");
    }

    #[test]
    fn removing_the_last_comp_sequence_releases_the_clock() {
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        assert!(app.comp_drives_transport());

        let id = app.comp_stack.iter().next().map(|l| l.id).unwrap();
        app.remove_comp_layer(id);

        assert_eq!(app.transport_source, None, "the clock is released");
        assert!(!app.playback.is_active(), "and the timeline with it");
    }

    // --- #100 comp-path pins ---------------------------------------------------
    //
    // Each of these asserts the behaviour the comp path *should* have. All are
    // GPU-free, and all are live — the sizing (#199), Stutter-hold (#200) and
    // INV-SAMPLE (#201) fixes have all landed, so each pin now guards its fix
    // rather than documenting an open bug.
    //
    // Common root cause: when the comp stack drives the transport, the primary slot
    // (`A_SOURCE`) never decodes, and every transport-level gate keyed on it goes
    // dead.

    /// `deliver_source_frame` with the fidelity flags a playback decode would carry
    /// (a beauty-only or downsampled proxy frame), set before the `Arc` wrap.
    fn deliver_source_frame_as(
        app: &mut ExrApp,
        source: crate::layer::SourceId,
        path: &std::path::Path,
        frame: u32,
        proxy: bool,
        beauty_only: bool,
    ) {
        let mut data = ExrData::load(path).unwrap();
        data.proxy = proxy;
        data.beauty_only = beauty_only;
        app.apply_load_result(LoadResult {
            source,
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: false,
            result: Ok(data),
        });
    }

    #[test]
    fn comp_transport_seeds_the_t1_sizing_bytes() {
        // `frame_bytes` is the sole input to `tick_budgets`' T1 branch. Unseeded, the
        // branch is skipped entirely: `frame_cache_cap` keeps its constructed default
        // of 8 and the #146 live-pressure shrink can never fire. On real footage that
        // is ~3.4 GB held with no budget check at all.
        //
        // The cap itself isn't asserted here — `tick_budgets` early-returns without a
        // GPU device, so it is unreachable under the GPU-free test convention. The
        // seeding is the defect; the cap follows from it mechanically.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        assert_eq!(
            app.clock_source(),
            source,
            "the comp source drives the clock"
        );

        app.playback_step(1);
        deliver_source_frame(&mut app, source, &paths[1], 2);

        let expected = ExrData::load(&paths[1]).unwrap().approx_bytes();
        assert_eq!(
            app.frame_bytes,
            Some(expected),
            "a full frame from the clock-driving source must size the ring"
        );
    }

    // --- #230: which fidelity's bytes size the T1 cap ------------------------

    #[test]
    fn a_beauty_only_frame_latches_its_own_size() {
        // The defect in one assertion. The latch was a two-way `if` —
        // `arc.proxy` -> `proxy_bytes`, `!arc.beauty_only` -> `frame_bytes` — so a
        // plain beauty-only frame matched *neither* arm. With `beauty_preview` on
        // and `proxy_enabled` off, playback then cached beauty-only frames while
        // sizing the ring off a full all-parts decode: `t1=23/23`, `evict=726` in
        // 45 s on a 1035 MB/frame render against a 24 GB budget.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        app.frame_bytes = None; // the open seeded it; start from unmeasured
        app.beauty_bytes = None;

        app.playback_step(1);
        deliver_source_frame_as(&mut app, source, &paths[1], 2, false, true);

        let expected = ExrData::load(&paths[1]).unwrap().approx_bytes();
        assert_eq!(
            app.beauty_bytes,
            Some(expected),
            "a beauty-only frame sizes the beauty latch"
        );
        assert_eq!(
            app.frame_bytes, None,
            "and must not masquerade as a full frame — it carries one layer"
        );
    }

    /// A playing slot-A sequence with the cheap-decode AOV gate satisfied — the
    /// state `sizing_frame_bytes` is read in. Latches are set to distinct
    /// sentinels so each assertion names exactly one of them.
    fn app_playing_with_all_three_latches() -> (tempfile::TempDir, ExrApp) {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 50);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.playback_toggle();
        assert!(app.playback.is_playing());
        app.frame_bytes = Some(1_000_000);
        app.beauty_bytes = Some(80_000);
        app.proxy_bytes = Some(9_000);
        (dir, app)
    }

    #[test]
    fn sizing_bytes_follow_the_fidelity_the_pump_is_issuing() {
        let (_dir, mut app) = app_playing_with_all_three_latches();

        // Proxy outranks beauty, exactly as `submit_seq` resolves the two: the
        // scrub proxy is what actually lands in the ring when both are enabled.
        assert!(app.proxy_enabled && app.beauty_preview);
        assert_eq!(app.sizing_frame_bytes(), Some(9_000));

        // Proxy off, beauty on — the configuration this machine runs, and the one
        // that was sizing off `frame_bytes`.
        app.proxy_enabled = false;
        assert_eq!(app.sizing_frame_bytes(), Some(80_000));

        // Both cheap modes off: full frames land, so full bytes size the ring.
        app.beauty_preview = false;
        assert_eq!(app.sizing_frame_bytes(), Some(1_000_000));
    }

    #[test]
    fn an_unmeasured_cheap_mode_falls_back_to_full_bytes() {
        // Every fallback chain ends at `frame_bytes`, never at something smaller.
        // A cheap mode whose bytes haven't been measured yet must err toward a cap
        // that is too *small* — merely wasteful — not toward one too large, which
        // is #215's OOM direction.
        let (_dir, mut app) = app_playing_with_all_three_latches();
        app.proxy_enabled = false;
        app.beauty_bytes = None;
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(1_000_000),
            "unmeasured beauty falls back to full, not to the proxy figure"
        );

        app.proxy_enabled = true;
        app.proxy_bytes = None;
        assert_eq!(app.sizing_frame_bytes(), Some(1_000_000));

        app.frame_bytes = None;
        assert_eq!(
            app.sizing_frame_bytes(),
            None,
            "nothing measured at all -> no cap computed, as before"
        );
    }

    #[test]
    fn a_settled_ring_with_precache_off_sizes_off_full_frames() {
        // Not playing, not scrubbing, no precache: `wants_cheap_decode_at` is false
        // for every frame, so every decode is full and the divisor must be too.
        let (_dir, mut app) = app_playing_with_all_three_latches();
        app.playback_stop();
        app.precache = false;
        assert!(!app.playback.is_playing() && !app.scrub_active);
        assert_eq!(app.sizing_frame_bytes(), Some(1_000_000));

        // With precache back on the ring *is* filled by cheap prefetch frames even
        // while settled, so the cheap figure is the honest one.
        app.precache = true;
        assert_eq!(app.sizing_frame_bytes(), Some(9_000));
    }

    #[test]
    fn sizing_refuses_the_cheap_figure_when_the_aov_needs_a_full_decode() {
        // The gate this replaced asked `viewer.active_layer == 0`, which stopped
        // tracking the decode path at #213/#217 — those moved it onto
        // `displayed_aov` over the whole comp stack. Sharing the predicate is the
        // point: a non-zero AOV whose part holds several passes can't be served by
        // a one-layer decode, so full frames land and full bytes must size them.
        let (_dir, mut app) = app_playing_with_all_three_latches();
        assert_eq!(app.sizing_frame_bytes(), Some(9_000), "AOV 0: cheap");

        app.viewer.active_layer = 3; // no layer table -> `single_layer_part` refuses
        assert!(!app.cheap_decode_fits_aov(ExrApp::A_SOURCE));
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(1_000_000),
            "no cheap decode can represent this AOV, so none may size the ring"
        );
    }

    #[test]
    fn repointing_the_transport_drops_every_measured_size() {
        // `frame_bytes` is a one-shot latch, so a re-point to a different-resolution
        // source would otherwise size the ring off the old one forever. The beauty
        // latch joins the other two rather than surviving the switch.
        let mut app = ExrApp {
            frame_bytes: Some(1),
            proxy_bytes: Some(2),
            beauty_bytes: Some(3),
            ..ExrApp::default()
        };
        app.set_transport_source(Some(crate::layer::SourceId(COMP_SOURCE_BASE)));
        assert_eq!(
            (app.frame_bytes, app.proxy_bytes, app.beauty_bytes),
            (None, None, None),
            "all three measured sizes are dropped with the transport"
        );
    }

    // --- #233: cheap decodes satisfied with something dearer ------------------

    #[test]
    fn fidelity_rank_orders_the_three_decode_modes() {
        // A proxy carries `beauty_only` too — it is a downsampled beauty decode —
        // so the proxy test has to come first or every proxy would rank as beauty
        // and a genuine proxy -> beauty fallback would read as clean.
        assert_eq!(fidelity_rank(true, true), 0, "proxy");
        assert_eq!(fidelity_rank(false, true), 1, "beauty-only");
        assert_eq!(fidelity_rank(false, false), 2, "full");
        assert!(fidelity_rank(false, false) > fidelity_rank(false, true));
        assert!(fidelity_rank(false, true) > fidelity_rank(true, true));
        // #217's per-AOV decode sets `beauty_only`, so a *successful* one-layer
        // decode of a non-beauty pass ranks cheap and is never a false fallback.
        assert_eq!(fidelity_rank(false, true), 1);
    }

    /// Deliver a frame that reports whether the worker had to fall back.
    fn deliver_frame_falling_back(
        app: &mut ExrApp,
        path: &std::path::Path,
        frame: u32,
        fell_back: bool,
    ) {
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back,
            result: Ok(ExrData::load(path).unwrap()),
        });
    }

    #[test]
    fn a_fallback_is_counted_and_steers_sizing_to_full_frames() {
        // The silent mode: the fallback is deliberate (slow beats stuck, #213) but
        // left no trace, so a run where every cheap decode failed looked identical
        // to one where the footage simply plays cheap — while the cap stayed sized
        // for 9 MB proxies as 1035 MB frames landed.
        let (_dir, paths) = write_sequence(3);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        app.playback_toggle();
        app.frame_bytes = Some(1_000_000);
        app.beauty_bytes = Some(80_000);
        app.proxy_bytes = Some(9_000);
        assert_eq!(app.dbg_fallbacks, 0);
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(9_000),
            "a clean proxy run sizes off proxy bytes"
        );

        deliver_frame_falling_back(&mut app, &paths[1], 2, true);
        assert_eq!(app.dbg_fallbacks, 1, "counted");
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(1_000_000),
            "a failing cheap path sizes off what actually lands"
        );

        // Self-clearing: the divisor is not latched to the bad news forever.
        deliver_frame_falling_back(&mut app, &paths[2], 3, false);
        assert_eq!(app.dbg_fallbacks, 1, "the counter is cumulative");
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(9_000),
            "a clean decode restores the requested fidelity's figure"
        );
    }

    #[test]
    fn the_fallback_override_can_only_shrink_the_cap() {
        // One-directional on purpose. Sizing too *small* is merely wasteful and
        // self-corrects; sizing too large is #215's OOM direction, so the override
        // is only ever allowed to move the divisor dearer.
        let (_dir, paths) = write_sequence(2);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        app.playback_toggle();
        app.frame_bytes = Some(1_000_000);
        app.beauty_bytes = Some(80_000);
        app.proxy_bytes = Some(9_000);

        for (requested, label) in [(true, "proxy"), (false, "beauty")] {
            app.proxy_enabled = requested;
            let clean = app.sizing_frame_bytes().unwrap();
            deliver_frame_falling_back(&mut app, &paths[1], 2, true);
            let after = app.sizing_frame_bytes().unwrap();
            assert!(
                after >= clean,
                "{label}: the override must never pick a cheaper figure ({after} < {clean})"
            );
            assert_eq!(after, 1_000_000);
            deliver_frame_falling_back(&mut app, &paths[1], 2, false);
        }
    }

    #[test]
    fn repointing_the_transport_forgets_a_stale_fallback() {
        // `decode_fell_back` describes one source's decode path. Carrying it to a
        // different sequence would size the new one off full frames on evidence
        // that says nothing about it.
        let (_dir, paths) = write_sequence(2);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        deliver_frame_falling_back(&mut app, &paths[1], 2, true);
        assert!(app.decode_fell_back);
        app.set_transport_source(Some(crate::layer::SourceId(COMP_SOURCE_BASE)));
        assert!(!app.decode_fell_back, "dropped with the other sizing state");
    }

    #[test]
    fn a_partial_fallback_still_sizes_off_something() {
        // A fallback is not always all the way to full. A proxy job whose fast
        // read fails returns a *beauty* frame — dearer than asked, so `fell_back`,
        // but it latches `beauty_bytes` and leaves `frame_bytes` unmeasured. An
        // override that reached straight for `frame_bytes` returned `None` here,
        // and `tick_budgets` skips its whole T1 branch on `None`: cap frozen at
        // its constructed 8, no budget check, #146's pressure shrink dead.
        let (_dir, paths) = write_sequence(2);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        app.playback_toggle();

        // The frame that *lands* is beauty-only — dearer than the proxy asked
        // for, so `fell_back`, but it latches `beauty_bytes`, not `frame_bytes`.
        let mut data = ExrData::load(&paths[1]).unwrap();
        data.beauty_only = true;
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 2,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: true,
            result: Ok(data),
        });
        assert!(app.decode_fell_back);
        assert_eq!(app.frame_bytes, None, "no full decode has happened yet");
        let beauty = app.beauty_bytes.expect("the landed frame latched beauty");

        // A definitively cheaper proxy figure, so the comparison below is real.
        app.proxy_bytes = Some(beauty / 2);
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(beauty),
            "falls through to the dearest measured figure, never to None"
        );

        // Still one-directional: the beauty figure is dearer than the proxy one
        // the request alone would have picked, so the cap only shrank.
        app.decode_fell_back = false;
        assert_eq!(app.sizing_frame_bytes(), Some(beauty / 2));
        assert!(
            beauty / 2 < beauty,
            "the override was the dearer of the two"
        );

        // And with nothing at all measured it is `None` exactly as before — that
        // is the honest "no sizing decode yet" state, not a hole.
        app.decode_fell_back = true;
        app.beauty_bytes = None;
        app.proxy_bytes = None;
        assert_eq!(app.sizing_frame_bytes(), None);
    }

    #[test]
    fn the_cache_reads_full_on_either_bound() {
        // #232: `precache_filled` and the pump's back-pressure both ask this. With
        // the count alone, a ring of frames dearer than the sizing figure sits
        // under `cap` while over budget — precache never latches and churns
        // decode -> evict forever, which is the exact failure the count check was
        // added to fix (#56), in the other unit.
        let (_dir, paths) = write_sequence(3);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]);
        for (i, p) in paths.iter().enumerate() {
            let data = std::sync::Arc::new(ExrData::load(p).unwrap());
            app.frame_cache.insert(ExrApp::A_SOURCE, i as u32 + 1, data);
        }
        let resident = app.frame_cache.bytes();
        assert!(resident > 0 && app.frame_cache.len() == 3);

        app.frame_cache_cap = 100;
        app.frame_cache_budget = resident * 2;
        assert!(!app.cache_is_full(), "room by both bounds");

        // Room by count (3 of 100), out of room by bytes.
        app.frame_cache_budget = resident;
        assert!(
            app.cache_is_full(),
            "a ring up against its byte budget is full however few frames it holds"
        );

        // And the count still latches on its own when bytes are slack.
        app.frame_cache_budget = u64::MAX;
        app.frame_cache_cap = 0;
        assert!(app.cache_is_full());
    }

    #[test]
    fn the_evict_bound_states_one_budget_in_two_units() {
        // Both `evict_to` call sites go through `cache_bound`, so the #146 pressure
        // shrink and the post-insert trim can't be handed different budgets.
        let app = ExrApp {
            frame_cache_cap: 424,
            frame_cache_budget: 24_000_000_000,
            ..ExrApp::default()
        };
        assert_eq!(
            app.cache_bound(),
            crate::cache::Bound::new(424, 24_000_000_000)
        );
    }

    #[test]
    fn stutter_holds_while_the_clock_source_frame_is_undecoded() {
        // Stutter's contract is "play every frame; drop the effective fps rather than
        // skip". With the gate blind to follower state the playhead advances on the
        // wall clock regardless, `run_held` never accrues, and `ensure_comp_frame`
        // early-returns on an unchanged source frame — so the *stale* texture stays on
        // screen while the frame counter moves. Stutter silently becomes drop-frames.
        use crate::playback::Pacing;
        let (_dir, paths) = write_sequence(10);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        app.playback.pacing = Pacing::Stutter;
        app.playback.loop_mode = LoopMode::Once;
        app.playback.state = PlayState::Playing;
        app.playback.fps_target = 24.0;
        let period = app.playback.period();

        // The clock source is awaiting a frame that has not decoded.
        app.followers.get_mut(&source).unwrap().pending = Some(2);
        let before = app.playback.current_frame;

        // Backdate the anchor so the next frame is due this tick.
        app.playback.anchor = Some(std::time::Instant::now() - period.mul_f32(1.5));
        app.playback.frames_since_anchor = 0;
        app.tick_stutter(period);

        assert_eq!(
            app.playback.current_frame, before,
            "stutter must hold on the frame the clock source is still decoding"
        );
    }

    #[test]
    fn settling_upgrades_a_proxy_comp_frame_to_full() {
        // INV-SAMPLE (#7): on settle the displayed frame must be full-fidelity, or the
        // readout and AOV switcher see a frame that doesn't carry every channel. The
        // slot-A path computes this explicitly (`needs_full`); `request_comp_frame`
        // grew the same fidelity test in #212, and this holds it there — a residency-
        // only predicate counts a proxy as satisfied and the blurry frame stays up
        // indefinitely.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        app.playback.state = PlayState::Playing;
        app.playback_step(1);
        assert_eq!(app.playback.current_frame, 2);

        // Frame 2 arrives as a scrub proxy — fine to display while moving.
        deliver_source_frame_as(&mut app, source, &paths[1], 2, true, true);
        assert!(app.frame_cache.contains(source, 2), "the proxy is resident");

        app.playback_stop();

        assert_eq!(
            app.followers.get(&source).unwrap().pending,
            Some(2),
            "settling must re-request the clock source's frame at full fidelity"
        );
    }

    #[test]
    fn settling_on_a_full_comp_frame_requests_nothing() {
        // The other half of the settle contract, and what makes the check above a real
        // fidelity test rather than an unconditional re-request: an already-full frame
        // needs no decode. Only meaningful paired with the proxy case above — a blind
        // path that always cleared `pending` would "pass" this one on its own.
        //
        // NOTE: the contact-sheet half of #201 is *not* pinned here, and turned out
        // not to be the bug the issue describes. `invalidate_active_thumbnails` was
        // genuinely unreachable on the comp path (fixed above — the fullness test is
        // keyed on the clock source now), but the sheet it would refresh bakes from
        // `CompSource::exr_data`, which `full_layer_table` (#217) deliberately pins to
        // the open-time decode and never replaces. So the sheet is frozen on frame one
        // by construction, and the invalidation re-bakes identical pixels. Tracked
        // separately; it also needs a `gui_tests`-level test either way, since the
        // thumbnail vector holds `egui::TextureHandle`s that cannot be constructed
        // without a device.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        app.playback.state = PlayState::Playing;
        app.playback_step(1);
        deliver_source_frame(&mut app, source, &paths[1], 2); // full decode

        app.playback_stop();

        assert_eq!(
            app.followers.get(&source).unwrap().pending,
            None,
            "an already-full frame needs no re-decode on settle"
        );
    }

    #[test]
    fn settling_on_a_full_comp_frame_keeps_the_prefetch_backlog() {
        // The cost side of the same `A_SOURCE`-keyed test (#201). `settle_to_full`
        // asked whether *slot A* was full, and under `comp_drives_transport` slot A
        // has nothing cached, so the answer was "no" on every settle — including the
        // settles where the displayed frame was already final. That took the
        // re-decode branch, and `invalidate_inflight` does not discriminate: it bumps
        // the epoch (superseding every worker job in flight, for every source),
        // clears each follower's `inflight`, and drops `precache_filled`. So pausing
        // on a fully-decoded frame threw away the read-ahead that pause is exactly
        // when you want kept, and the ring had to refill from scratch.
        //
        // Pinned on the epoch rather than on cache contents: the epoch *is* the
        // supersession mechanism (`apply_load_result` drops any result whose epoch no
        // longer matches), so it is what says the in-flight work survived.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        app.playback.state = PlayState::Playing;
        app.playback_step(1);
        deliver_source_frame(&mut app, source, &paths[1], 2); // the playhead, full

        // Read-ahead in flight for the frames after it, and the precache satisfied.
        app.followers.get_mut(&source).unwrap().inflight.insert(3);
        app.precache_filled = true;
        let epoch = app.playback.epoch;

        app.playback_stop();

        assert_eq!(
            app.playback.epoch, epoch,
            "settling on a final frame must not supersede in-flight decodes"
        );
        assert!(
            app.followers.get(&source).unwrap().inflight.contains(&3),
            "nor drop the read-ahead already submitted for the frames after it"
        );
        assert!(
            app.precache_filled,
            "nor force the precache window to refill from scratch"
        );
    }

    #[test]
    fn the_cache_bar_reads_the_clock_source_not_slot_a() {
        // #245: the ruler's cache-fill bar read `resident_frames(A_SOURCE)`
        // unconditionally. Under `comp_drives_transport` slot A never decodes, so
        // the bar was empty on the default path however full the ring actually was
        // — which reads as "the precache isn't working" on a session holding
        // hundreds of frames. Same `A_SOURCE`-keyed blind spot as #199/#200/#201.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        assert!(app.comp_drives_transport(), "the comp stack owns the clock");

        deliver_source_frame(&mut app, source, &paths[1], 2);
        deliver_source_frame(&mut app, source, &paths[2], 3);

        assert!(
            app.frame_cache.resident_frames(ExrApp::A_SOURCE).count() == 0,
            "slot A holds nothing on the comp path — the premise of the bug"
        );
        let (mut resident, mut full) = app.cache_bar_frames(1, 5);
        resident.sort_unstable();
        full.sort_unstable();
        assert!(
            resident.contains(&2) && resident.contains(&3),
            "the bar must show the clock source's ring, got {resident:?}"
        );
        assert_eq!(full, resident, "full decodes shade as full");
    }

    #[test]
    fn the_cache_bar_maps_a_retimed_layer_into_global_frames() {
        // A follower's ring is keyed by its *own* frame numbers. With a trim offset
        // (`source = global + offset`) painting them unmapped would put the fill
        // under the wrong part of the ruler — wrong only for retimed layers, and
        // quietly.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        let layer = app.comp_stack.iter().next().map(|l| l.id).unwrap();
        // Source frame 3 now sits at global frame 1.
        app.comp_stack.get_mut(layer).unwrap().trim.offset = 2;

        deliver_source_frame(&mut app, source, &paths[2], 3);

        let (resident, _) = app.cache_bar_frames(1, 5);
        assert_eq!(
            resident,
            vec![1],
            "source frame 3 at offset 2 paints at global frame 1"
        );
    }

    #[test]
    fn the_cache_bar_separates_cheap_frames_from_full_ones_and_clips_to_the_range() {
        // The bar is two-tone (#172): every resident frame in the base shade, the
        // full-fidelity ones brighter. A proxy must appear in the first and not the
        // second, or a ring full of scrub proxies would read as fully cached.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        // Frame 1 is already resident and full: `add_comp_source` decodes the opened
        // frame synchronously and seeds the ring with it for an instant first hit.
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        deliver_source_frame(&mut app, source, &paths[1], 2); // full
        deliver_source_frame_as(&mut app, source, &paths[2], 3, true, true); // proxy
        deliver_source_frame(&mut app, source, &paths[4], 5); // full, outside the clip below

        let (mut resident, mut full) = app.cache_bar_frames(1, 4);
        resident.sort_unstable();
        full.sort_unstable();
        assert_eq!(
            resident,
            vec![1, 2, 3],
            "frame 5 is outside [1, 4] and clipped"
        );
        assert_eq!(
            full,
            vec![1, 2],
            "frame 3 is resident but a proxy, so it never shades as full"
        );
    }

    #[test]
    fn add_comp_source_still_registers_no_follower() {
        // A lone (unnumbered) file stays a still: no follower, nothing to play.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("still.exr");
        write_rgba_exr(&f);
        let mut app = ExrApp::default();
        app.add_comp_source(f);

        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        assert!(
            !app.followers.contains_key(&source),
            "a still registers no decode follower"
        );
        assert!(
            app.comp_sources.contains_key(&source),
            "the still source is still stored (its single texture)"
        );
    }

    // --- R3: slot A joins the composite as the bottom base track (#99) --------

    /// Open a still as slot A directly (headless: `open_file` is async), then add a
    /// comp source. Returns the app with A + one comp layer.
    fn app_with_open_a_still() -> (tempfile::TempDir, ExrApp) {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("plate.exr");
        let comp = dir.path().join("over.exr");
        write_rgba_exr(&a);
        write_rgba_exr(&comp);
        let mut app = ExrApp {
            loaded_file: Some(a.clone()),
            exr_data: Some(std::sync::Arc::new(ExrData::load(&a).unwrap())),
            ..Default::default()
        };
        app.add_comp_source(comp);
        (dir, app)
    }

    #[test]
    fn adding_the_first_comp_source_promotes_open_a_to_the_base_track() {
        let (_dir, app) = app_with_open_a_still();
        assert_eq!(
            app.comp_stack.len(),
            2,
            "the opened plate + the added layer"
        );
        // Bottom of the stack (index 0) is slot A.
        let bottom = app.comp_stack.iter().next().unwrap();
        assert!(
            matches!(bottom.source, crate::layer::LayerSource::Image { source, .. } if source == ExrApp::A_SOURCE),
            "the base track references A_SOURCE"
        );
        assert_eq!(
            app.base_layer_id(),
            Some(bottom.id),
            "the base is discoverable as the A_SOURCE layer"
        );
        assert!(
            app.comp_sources.contains_key(&ExrApp::A_SOURCE),
            "a CompSource is registered for the base plate"
        );
    }

    #[test]
    fn removing_the_last_comp_source_drops_the_base_and_leaves_a_untouched() {
        let (_dir, mut app) = app_with_open_a_still();
        // Seed A's own T1 cache to prove base removal never clears it (the
        // `clear_slot(A_SOURCE)` footgun).
        app.frame_cache
            .insert(ExrApp::A_SOURCE, 1, app.exr_data.clone().unwrap());

        // Remove the added comp source (the top layer).
        let comp_id = app.comp_stack.iter().last().unwrap().id;
        assert_ne!(Some(comp_id), app.base_layer_id());
        app.remove_comp_layer(comp_id);

        assert!(
            app.comp_stack.is_empty(),
            "base auto-removed with the last comp source → classic viewer takes over"
        );
        assert!(app.base_layer_id().is_none());
        assert!(!app.comp_sources.contains_key(&ExrApp::A_SOURCE));
        // Slot A itself is untouched: still open, cache intact.
        assert!(app.loaded_file.is_some() && app.exr_data.is_some());
        assert!(
            app.frame_cache.contains(ExrApp::A_SOURCE, 1),
            "base removal must not clear A's real transport cache"
        );
    }

    #[test]
    fn base_track_trim_spans_the_a_sequence_range_at_offset_zero() {
        let (dir, paths) = write_sequence(5);
        let comp = dir.path().join("over.exr");
        write_rgba_exr(&comp);
        let mut app = ExrApp {
            loaded_file: Some(paths[0].clone()),
            exr_data: Some(std::sync::Arc::new(ExrData::load(&paths[0]).unwrap())),
            ..Default::default()
        };
        app.detect_sequence(&paths[0]); // playback range (1, 5)
        app.add_comp_source(comp);

        let base = app.comp_stack.get(app.base_layer_id().unwrap()).unwrap();
        assert_eq!(
            (base.trim.in_point, base.trim.out_point, base.trim.offset),
            (1, 5, 0),
            "the base spans A's range at offset 0 — it drives the clock"
        );
    }

    #[test]
    fn add_comp_source_without_an_open_a_adds_no_base() {
        // No plate open: the comp sequence drives the transport itself (#99 R4-lite)
        // and there is no slot A to promote — so no base track.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        assert!(app.base_layer_id().is_none(), "no plate → no base track");
        assert_eq!(app.comp_stack.len(), 1, "only the comp source");
        assert!(app.comp_drives_transport());
    }

    #[test]
    fn sync_comp_followers_maps_the_global_playhead_through_the_trim() {
        // A comp sequence layer follows the shared playhead (#99). Added with no base
        // plate, the first sequence *drives the transport* (enter at its opened frame
        // 1), so its Trim aligns 1:1: source_frame(g) == g, blank outside [1, 5].
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        // The first comp sequence established the transport at its opened frame (R4-lite).
        assert!(
            app.playback.is_active(),
            "the comp stack drives the transport"
        );
        assert_eq!(app.playback.current_frame, 1);
        assert!(app.comp_drives_transport());
        assert_eq!(app.followers.get(&source).unwrap().current_frame, 1);
        assert!(app.frame_cache.contains(source, 1));

        // Global 3 → source frame 3; not resident, so it's awaited for the pump.
        app.playback.current_frame = 3;
        app.sync_comp_followers();
        let f = app.followers.get(&source).unwrap();
        assert_eq!(f.current_frame, 3, "follower maps global 1:1");
        assert_eq!(f.pending, Some(3), "non-resident mapped frame is awaited");

        // Global 6 → source 6, past the range [1, 5]: blank, follower holds.
        app.playback.current_frame = 6;
        app.sync_comp_followers();
        assert_eq!(
            app.followers.get(&source).unwrap().current_frame,
            3,
            "out-of-range global holds the last frame (blank layer)"
        );
    }

    #[test]
    fn per_layer_time_offset_reshifts_the_follower() {
        // Editing a comp sequence layer's Trim offset (what the per-row time control
        // does) slides it along the timeline: the follower maps the global frame
        // through the new offset on the next sync (#99).
        let (_dir, paths) = write_sequence(10);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        let id = app.comp_stack.iter().next().unwrap().id;

        // Transport enters at frame 1, offset 0 → source_frame(global) == global.
        app.playback.current_frame = 3;
        app.sync_comp_followers();
        assert_eq!(app.followers.get(&source).unwrap().current_frame, 3);

        // Nudge the layer +2 frames (what the row DragValue writes), then re-sync.
        app.comp_stack.get_mut(id).unwrap().trim.offset = 2;
        app.sync_comp_followers();
        assert_eq!(
            app.followers.get(&source).unwrap().current_frame,
            5,
            "offset +2 shifts the follower's source frame (3 + 2)"
        );
    }

    #[test]
    fn comp_source_carries_all_aovs_for_the_picker() {
        // A multi-pass source stores every logical layer, so the per-row AOV picker
        // (#99 PR-B.4.3) has entries; the layer starts on AOV 0.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("passes.exr");
        write_multi_pass_exr(&f, 3);

        let mut app = ExrApp::default();
        app.add_comp_source(f);
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);
        let cs = app.comp_sources.get(&source).expect("source stored");
        assert_eq!(cs.exr_data.logical_layers.len(), 3, "3 passes → 3 AOVs");
        assert_eq!(cs.aov, 0, "starts on AOV 0");

        // ensure_comp_aov is a no-op when the stored AOV already matches (headless has
        // no GPU, but the early `aov == aov` return fires before that matters).
        app.ensure_comp_aov(source, 0);
        assert_eq!(app.comp_sources.get(&source).unwrap().aov, 0);
    }

    #[test]
    fn add_comp_source_stops_at_the_layer_cap() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("layer.exr");
        write_rgba_exr(&f);

        let mut app = ExrApp::default();
        for _ in 0..(COMP_LAYER_CAP + 2) {
            app.add_comp_source(f.clone());
        }
        assert_eq!(app.comp_stack.len(), COMP_LAYER_CAP, "capped");
        assert_eq!(
            app.comp_sources.len(),
            COMP_LAYER_CAP,
            "one source per layer"
        );
    }

    #[test]
    fn stale_load_result_is_ignored() {
        // A result from an open the user has since superseded (a later open bumped
        // the slot-A generation) must not clobber state or clear the in-flight flag
        // for the current request (#109).
        let mut app = ExrApp {
            loaded_file: Some(PathBuf::from("current.exr")),
            open_gen_a: 2, // a newer open is in flight
            loading_a: true,
            ..Default::default()
        };

        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 1, // the older, superseded open
            fell_back: false,
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

    /// #109: opening a new EXR while the timeline plays must load it. Playback
    /// rewrites `loaded_file` to the current frame's path
    /// (`request_sequence_frame`), so superseding the explicit open by *path*
    /// dropped its still-current result — and that drop returned before clearing
    /// `loading_a`, permanently gating `pump_decode` ("doesn't load until you stop
    /// and reopen"). The open is now superseded by **generation**, immune to the
    /// `loaded_file` churn.
    #[test]
    fn open_result_applies_despite_loaded_file_churn_during_playback() {
        let dir = tempfile::tempdir().unwrap();
        let newf = dir.path().join("new.exr");
        write_rgba_exr(&newf);
        let data = ExrData::load(&newf).unwrap();

        // An explicit open of `new.exr` is in flight at generation 5.
        let mut app = ExrApp {
            loaded_file: Some(newf.clone()),
            open_gen_a: 5,
            loading_a: true,
            ..Default::default()
        };
        // Playback then rewrites `loaded_file` to the frame on screen — the churn
        // that used to make the open's result look superseded.
        app.loaded_file = Some(dir.path().join("seq.0007.exr"));

        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 5, // still the current open
            fell_back: false,
            result: Ok(data),
        });

        assert!(
            app.exr_data.is_some(),
            "the open is applied despite the loaded_file churn"
        );
        assert!(
            !app.loading_a,
            "loading flag cleared — pump_decode is not left gated"
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
            source: ExrApp::A_SOURCE,
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 0,
            fell_back: false,
            result: Err("bad exr".to_string()),
        });

        assert_eq!(app.error_msg.as_deref(), Some("bad exr"));
        assert!(!app.loading_a, "matching result clears the loading flag");
    }

    #[test]
    fn swap_image_data_a_preserves_viewer_state() {
        // The per-frame playback path (#7): a new A frame lands but the user's
        // view (zoom, pan, exposure, channel mode, swatches, annotations) must be
        // preserved. Contrast the open path, which resets the viewer.
        let dir = tempfile::tempdir().unwrap();
        let path_a0 = dir.path().join("a0.exr");
        let path_a1 = dir.path().join("a1.exr");
        write_rgba_exr(&path_a0);
        write_rgba_exr(&path_a1);
        let a1 = ExrData::load(&path_a1).unwrap();

        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&path_a0).unwrap())),
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

        app.swap_image_arc(std::sync::Arc::new(a1));

        assert!(app.exr_data.is_some(), "new A applied");
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

        app.swap_image_arc(std::sync::Arc::new(one));

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
        // #151: display prefs are viewer-owned now — a new-session reset must NOT
        // wipe them (opening a new file shouldn't lose your background / gradients).
        app.viewer.prefs.diff_floor = 0.2;
        app.viewer.prefs.background.checker_size = 77.0;
        app.viewer.prefs.custom_gradients.push((
            "keep".into(),
            crate::gradient::Colormap::default().gradient(),
        ));

        app.reset_viewer_session();

        assert_eq!(app.viewer.scale, 1.0, "zoom reset");
        assert_eq!(app.viewer.translation, egui::Vec2::ZERO, "pan reset");
        assert_eq!(app.viewer.exposure, 0.0, "exposure reset");
        assert!(app.viewer.swatches.is_empty(), "swatches cleared");
        assert!(app.viewer.annotations.is_empty(), "annotations cleared");
        // Prefs survive the reset.
        assert_eq!(app.viewer.prefs.diff_floor, 0.2, "diff_floor preserved");
        assert_eq!(
            app.viewer.prefs.background.checker_size, 77.0,
            "background preserved"
        );
        assert_eq!(
            app.viewer.prefs.custom_gradients.len(),
            1,
            "custom gradients preserved"
        );
    }

    #[test]
    fn viewer_prefs_persist_and_restore_through_ron_storage() {
        // Exercise the real save/load bridge (#151) through eframe's RON codec via
        // an in-memory Storage, so this guards the actual persistence path — not
        // just a serde round-trip.
        #[derive(Default)]
        struct MemStorage(std::collections::HashMap<String, String>);
        impl eframe::Storage for MemStorage {
            fn get_string(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
            fn set_string(&mut self, key: &str, value: String) {
                self.0.insert(key.to_owned(), value);
            }
            fn flush(&mut self) {}
        }

        let mut app = ExrApp::default();
        // Mutate the viewer-owned prefs (the single runtime owner).
        app.viewer.prefs.diff_floor = 0.125;
        app.viewer.prefs.background.checker_size = 42.0;
        app.viewer.prefs.background.gradient_angle = 90.0;
        app.viewer.prefs.custom_gradients.push((
            "mine".into(),
            crate::gradient::Colormap::default().gradient(),
        ));
        app.viewer
            .prefs
            .background_presets
            .push(("bg1".into(), crate::background::Background::default()));
        let expected = app.viewer.prefs.clone();

        // save(): mirror viewer.prefs into persisted_prefs, then RON-serialize.
        let mut storage = MemStorage::default();
        eframe::App::save(&mut app, &mut storage);

        // new(): RON-deserialize, then move persisted_prefs back into the viewer.
        let mut restored: ExrApp = eframe::get_value(&storage, eframe::APP_KEY)
            .expect("app state round-trips through eframe's RON codec");
        restored.viewer.prefs = std::mem::take(&mut restored.persisted_prefs);

        assert_eq!(
            restored.viewer.prefs, expected,
            "viewer prefs survive the save/load persistence bridge"
        );
    }

    #[test]
    fn comp_layers_persist_and_restore_through_ron_storage() {
        // #99 PR-B.5: the comp stack survives a restart. Exercises the real bridge —
        // `save()` flattens, eframe's RON codec round-trips, `restore_comp_layers`
        // replays through `add_comp_source` — not just a serde round-trip.
        #[derive(Default)]
        struct MemStorage(std::collections::HashMap<String, String>);
        impl eframe::Storage for MemStorage {
            fn get_string(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
            fn set_string(&mut self, key: &str, value: String) {
                self.0.insert(key.to_owned(), value);
            }
            fn flush(&mut self) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let f0 = dir.path().join("one.exr");
        let f1 = dir.path().join("two.exr");
        write_rgba_exr(&f0);
        write_rgba_exr(&f1);

        let mut app = ExrApp::default();
        app.add_comp_source(f0.clone());
        app.add_comp_source(f1.clone());
        assert_eq!(app.comp_stack.len(), 2, "two layers added");

        // Give the top layer non-default per-layer state, so the assertions below
        // prove the *state* round-trips and not just the paths.
        let top = app.comp_stack.iter().last().map(|l| l.id).unwrap();
        {
            let l = app.comp_stack.get_mut(top).unwrap();
            l.blend = crate::viewer::BlendMode::Screen;
            l.opacity = 0.25;
            l.solo = true;
            l.name = "renamed".to_string();
            l.trim = crate::layer::Trim {
                in_point: 3,
                out_point: 9,
                offset: -2,
            };
        }

        let mut storage = MemStorage::default();
        eframe::App::save(&mut app, &mut storage);

        let mut restored: ExrApp = eframe::get_value(&storage, eframe::APP_KEY)
            .expect("app state round-trips through eframe's RON codec");
        assert_eq!(
            restored.comp_stack.len(),
            0,
            "the stack itself is runtime-only — it arrives empty and is replayed"
        );
        restored.restore_comp_layers();

        assert_eq!(restored.comp_stack.len(), 2, "both layers restored");
        let names: Vec<_> = restored.comp_stack.iter().map(|l| l.name.clone()).collect();
        assert_eq!(
            names,
            vec!["one.exr".to_string(), "renamed".to_string()],
            "restored bottom→top in the saved order, keeping the renamed layer"
        );

        let t = restored.comp_stack.iter().last().unwrap();
        assert_eq!(t.blend, crate::viewer::BlendMode::Screen, "blend restored");
        assert!((t.opacity - 0.25).abs() < f32::EPSILON, "opacity restored");
        assert!(t.solo, "solo restored");
        assert_eq!(t.trim.in_point, 3);
        assert_eq!(t.trim.out_point, 9);
        assert_eq!(t.trim.offset, -2, "per-layer time offset restored");

        // Each restored layer decoded into its own source — no aliasing, and none
        // landing on the base-plate slot.
        let sources: Vec<_> = restored
            .comp_stack
            .iter()
            .filter_map(|l| match &l.source {
                crate::layer::LayerSource::Image { source, .. } => Some(*source),
                crate::layer::LayerSource::Adjustment => None,
            })
            .collect();
        assert_eq!(sources.len(), 2);
        assert_ne!(sources[0], sources[1], "distinct sources");
        assert!(
            sources.iter().all(|s| *s != ExrApp::A_SOURCE),
            "restored layers never alias the base-plate slot"
        );
        assert!(restored.error_msg.is_none(), "a clean restore is silent");
    }

    #[test]
    fn restore_skips_layers_whose_file_is_gone() {
        // A stale session must not fail wholesale, nor greet the user with an error
        // box: the missing layer is dropped and the rest of the stack restores.
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("here.exr");
        write_rgba_exr(&present);

        let mut app = ExrApp {
            persisted_layers: vec![
                LayerPersist {
                    path: dir.path().join("deleted.exr"),
                    name: "deleted.exr".into(),
                    ..Default::default()
                },
                LayerPersist {
                    path: present.clone(),
                    name: "here.exr".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        app.restore_comp_layers();

        assert_eq!(app.comp_stack.len(), 1, "only the present file restored");
        assert_eq!(app.comp_stack.iter().next().unwrap().name, "here.exr");
        assert!(
            app.error_msg.is_none(),
            "a missing file is not an error box"
        );
        assert!(
            app.persisted_layers.is_empty(),
            "the replayed list is consumed, so a later save re-derives it from the stack"
        );
    }

    #[test]
    fn reopening_a_loaded_path_focuses_its_layer_instead_of_duplicating() {
        // #242: opening a path already in the stack appended a second layer stacked
        // exactly on the first — invisible (the picture is identical) but costing a
        // full synchronous decode, another decode follower dividing the single
        // worker, and another share of the T1 budget. Five relaunches on one file
        // took a stack from 1 layer to 6.
        let (_dir, paths) = write_sequence(3);
        let mut app = ExrApp::default();
        app.open_layer(paths[0].clone());
        let before = app.comp_stack.len();
        let id = app.comp_stack.iter().next().map(|l| l.id).unwrap();
        app.selected_comp_layer = None;

        app.open_layer(paths[0].clone());

        assert_eq!(app.comp_stack.len(), before, "no second layer was added");
        assert_eq!(
            app.selected_comp_layer,
            Some(id),
            "the existing layer is focused instead — 'open' means 'show me this'"
        );
        assert_eq!(
            app.recent_files.first(),
            Some(&paths[0]),
            "the user still asked for this file, so it is still a recent file"
        );
    }

    #[test]
    fn duplicating_a_layer_shares_its_source_and_costs_no_decode() {
        // The explicit route to what re-opening used to do by accident — and the
        // difference that matters: the copy references the *same* `SourceId`, so its
        // pixels are the ones already cached under that key. A second decode, a
        // second follower and a second T1 share are exactly what #242 was about.
        let (_dir, paths) = write_sequence(3);
        let mut app = ExrApp::default();
        app.open_layer(paths[0].clone());
        let id = app.comp_stack.iter().next().map(|l| l.id).unwrap();
        let sources_before = app.comp_sources.len();
        let followers_before = app.followers.len();

        app.duplicate_comp_layer(id);

        assert_eq!(app.comp_stack.len(), 2, "a second layer exists");
        assert_eq!(
            app.comp_sources.len(),
            sources_before,
            "but no second source was decoded"
        );
        assert_eq!(
            app.followers.len(),
            followers_before,
            "and no second decode follower divides the worker"
        );
        let names: Vec<&str> = app.comp_stack.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names[1],
            format!("{} (2)", names[0]).as_str(),
            "the copy is distinguishable in the panel, and sits directly above"
        );
    }

    #[test]
    fn the_base_track_cannot_be_duplicated() {
        // #242 review: slot A is the sole layer allowed to reference `A_SOURCE` —
        // `base_layer_id` resolves it with a `find_map` on that assumption — and
        // `comp_layers_persist` skips `A_SOURCE` layers, so a copy would silently
        // vanish on the next launch having broken the invariant in the meantime.
        let (_dir, mut app) = app_with_open_a_still();
        let base = app.base_layer_id().expect("slot A is the base track");
        let before = app.comp_stack.len();

        app.duplicate_comp_layer(base);

        assert_eq!(app.comp_stack.len(), before, "the base track is not copied");
        assert_eq!(
            app.comp_stack
                .iter()
                .filter(|l| matches!(
                    l.source,
                    crate::layer::LayerSource::Image { source, .. } if source == ExrApp::A_SOURCE
                ))
                .count(),
            1,
            "exactly one layer references A_SOURCE, as base_layer_id assumes"
        );
    }

    #[test]
    fn duplicate_naming_only_renumbers_a_real_numeric_suffix() {
        // #242 review: a bare `rsplit_once(" (")` treats any parenthesis as the
        // copy marker, so `plate (final).exr` would be truncated to `plate` —
        // dropping the part of the name that identified it.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("plate (final).exr");
        write_rgba_exr(&f);
        let mut app = ExrApp::default();
        app.open_layer(f);
        let id = app.comp_stack.iter().next().map(|l| l.id).unwrap();

        app.duplicate_comp_layer(id);

        let names: Vec<&str> = app.comp_stack.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["plate (final).exr", "plate (final).exr (2)"],
            "a non-numeric parenthetical is part of the name, not a copy marker"
        );
    }

    #[test]
    fn reopening_the_same_file_by_a_different_path_spelling_still_focuses() {
        // #242 review: raw `PathBuf` equality misses the same file spelled two ways
        // — a relative path on the command line versus the absolute one a picker
        // returns — which would silently reintroduce the duplicate decode.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("plate.exr");
        write_rgba_exr(&f);
        let mut app = ExrApp::default();
        app.open_layer(f.clone());
        let before = app.comp_stack.len();
        let id = app.comp_stack.iter().next().map(|l| l.id).unwrap();
        app.selected_comp_layer = None;

        // The same file reached via a `..` round trip — a different `PathBuf`,
        // the same bytes on disk.
        let indirect = dir.path().join("sub").join("..").join("plate.exr");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        assert_ne!(indirect, f, "the two spellings really do differ");

        app.open_layer(indirect);

        assert_eq!(app.comp_stack.len(), before, "no duplicate was added");
        assert_eq!(
            app.selected_comp_layer,
            Some(id),
            "the existing layer is focused, whichever spelling asked for it"
        );
        assert_eq!(
            app.recent_files.len(),
            1,
            "and the two spellings are one recent-files entry, not two"
        );
    }

    #[test]
    fn removing_one_of_a_duplicated_pair_keeps_the_source_alive() {
        // `remove_comp_layer` frees a source when its last layer goes. With a
        // duplicate sharing the `SourceId`, freeing on the first removal would pull
        // the cache, the follower and the upload gate out from under the survivor.
        let (_dir, paths) = write_sequence(3);
        let mut app = ExrApp::default();
        app.open_layer(paths[0].clone());
        let id = app.comp_stack.iter().next().map(|l| l.id).unwrap();
        app.duplicate_comp_layer(id);
        let source = crate::layer::SourceId(COMP_SOURCE_BASE);

        app.remove_comp_layer(id);

        assert_eq!(app.comp_stack.len(), 1, "the copy survives");
        assert!(
            app.comp_sources.contains_key(&source),
            "and its source with it — the last reference has not gone"
        );
        assert!(
            app.followers.contains_key(&source),
            "including the decode follower the survivor still needs"
        );
    }

    #[test]
    fn restoring_two_layers_on_one_path_shares_a_single_source() {
        // A duplicate is one source and two layers in a live session, so restoring it
        // as two independent sources would silently undo the sharing on the next
        // launch — the same double-decode #242 is about, one restart later. This also
        // repairs stacks built before the fix: N accumulated copies collapse to one
        // source and one decode on load.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("plate.exr");
        write_rgba_exr(&f);

        let mut app = ExrApp {
            persisted_layers: vec![
                LayerPersist {
                    path: f.clone(),
                    name: "plate.exr".into(),
                    ..Default::default()
                },
                LayerPersist {
                    path: f.clone(),
                    name: "plate.exr (2)".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        app.restore_comp_layers();

        assert_eq!(app.comp_stack.len(), 2, "both layers restored");
        assert_eq!(app.comp_sources.len(), 1, "sharing one decoded source");
        let names: Vec<&str> = app.comp_stack.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["plate.exr", "plate.exr (2)"],
            "each layer keeps its own persisted name"
        );
    }

    #[test]
    fn layer_persist_defaults_keep_a_layer_visible() {
        // `#[serde(default)]` fills MISSING fields from `Default`, so an app.ron
        // written before a field existed must not restore an invisible layer — a
        // derived Default would give opacity 0.0 / enabled false.
        let d = LayerPersist::default();
        assert!((d.opacity - 1.0).abs() < f32::EPSILON, "fully opaque");
        assert!(d.enabled, "enabled");
        assert!(!d.solo);
        assert_eq!(d.trim_out, u32::MAX, "spans all frames");
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
    fn is_exr_path_is_case_insensitive_and_extension_only() {
        assert!(is_exr_path(std::path::Path::new("/a/b/shot.exr")));
        assert!(is_exr_path(std::path::Path::new("SHOT.EXR")));
        assert!(is_exr_path(std::path::Path::new("render.Exr")));
        assert!(!is_exr_path(std::path::Path::new("note.txt")));
        assert!(!is_exr_path(std::path::Path::new("exr"))); // bare name, no extension
        assert!(!is_exr_path(std::path::Path::new("archive.exr.zip")));
    }

    #[test]
    fn top_sample_source_picks_the_last_drawable_draw() {
        use crate::layer::{LayerStack, SourceId, Trim};
        let mut stack = LayerStack::new();
        let s_bottom = SourceId(2);
        let s_top = SourceId(3);
        stack.push_image("bottom", s_bottom, 0, Trim::full(0, u32::MAX));
        stack.push_image("top", s_top, 1, Trim::full(0, u32::MAX));
        let steps = stack.composite_at(0);

        // All drawable → the top (last) layer wins, carrying its own aov.
        assert_eq!(top_sample_source(&steps, |_| true), Some((s_top, 1)));
        // Top not drawable → the next drawable below it.
        assert_eq!(
            top_sample_source(&steps, |s| s == s_bottom),
            Some((s_bottom, 0))
        );
        // Nothing drawable → None.
        assert_eq!(top_sample_source(&steps, |_| false), None);
    }

    #[test]
    fn elide_middle_keeps_both_ends_within_budget() {
        // Short enough → untouched.
        assert_eq!(elide_middle("short.exr", 24), "short.exr");
        assert_eq!(
            elide_middle("exactly_24_chars_long.ex", 24),
            "exactly_24_chars_long.ex"
        );

        // A real offender: both the shot prefix and the version/frame tail survive,
        // which is what makes two similar names distinguishable in the picker.
        let long = "ESTU0001_gloomWatcher_v001.karmarendersettings.1001.exr";
        let out = elide_middle(long, 24);
        assert_eq!(out.chars().count(), 24, "stays within budget: {out:?}");
        assert!(out.contains('…'), "elided: {out:?}");
        assert!(out.starts_with("ESTU0001"), "keeps the head: {out:?}");
        assert!(out.ends_with(".exr"), "keeps the tail: {out:?}");

        // Multi-byte characters are counted as chars, not bytes (no panic on a
        // non-ASCII boundary).
        let uni = "日本語のとても長いファイル名です.exr";
        let out = elide_middle(uni, 10);
        assert_eq!(out.chars().count(), 10, "counts chars: {out:?}");

        // Degenerate budgets are passed through rather than panicking.
        assert_eq!(elide_middle("abcdef", 2), "abcdef");
    }

    #[test]
    fn default_compare_b_avoids_the_current_layer() {
        use crate::layer::{LayerStack, SourceId, Trim};
        // `LayerId` is opaque, so mint real ones from a stack (bottom→top).
        let mut stack = LayerStack::new();
        let a = stack.push_image("a", SourceId(2), 0, Trim::full(0, u32::MAX));
        let b = stack.push_image("b", SourceId(3), 0, Trim::full(0, u32::MAX));
        let c = stack.push_image("c", SourceId(4), 0, Trim::full(0, u32::MAX));
        let ids = [a, b, c];

        // Current is the top layer (the usual default) → compare against the next one
        // down, so the two panes aren't showing the same thing.
        assert_eq!(default_compare_b(&ids, Some(c)), Some(b));
        // Current is lower → the topmost layer is the natural counterpart.
        assert_eq!(default_compare_b(&ids, Some(a)), Some(c));
        assert_eq!(default_compare_b(&ids, Some(b)), Some(c));
        // No current layer → the top of the stack.
        assert_eq!(default_compare_b(&ids, None), Some(c));
        // A single layer can only compare against itself.
        assert_eq!(default_compare_b(&[a], Some(a)), Some(a));
        // Empty stack → nothing to compare.
        assert_eq!(default_compare_b(&[], Some(a)), None);
    }

    #[test]
    fn comp_layer_draw_resolves_a_named_layer_or_falls_back() {
        use crate::layer::{LayerStack, SourceId, Trim};
        let mut stack = LayerStack::new();
        let s_bottom = SourceId(2);
        let s_top = SourceId(3);
        let bottom = stack.push_image("bottom", s_bottom, 0, Trim::full(0, u32::MAX));
        let top = stack.push_image("top", s_top, 1, Trim::full(0, u32::MAX));
        let steps = stack.composite_at(0);

        // The current layer resolves to its own draw, wherever it sits in the stack.
        let d = comp_layer_draw(&steps, Some(bottom), |_| true).expect("bottom resolves");
        assert_eq!((d.source, d.aov), (s_bottom, 0));
        let d = comp_layer_draw(&steps, Some(top), |_| true).expect("top resolves");
        assert_eq!((d.source, d.aov), (s_top, 1));

        // Not drawable (no texture yet) → None, so the caller falls back to Stacked.
        assert!(comp_layer_draw(&steps, Some(top), |s| s == s_bottom).is_none());
        // No selection at all → None.
        assert!(comp_layer_draw(&steps, None, |_| true).is_none());
        // Selected but absent from this frame's composite (hidden / trimmed blank).
        let hidden = stack.push_image("hidden", SourceId(4), 0, Trim::full(0, u32::MAX));
        assert!(
            comp_layer_draw(&steps, Some(hidden), |_| true).is_none(),
            "a layer missing from these steps has no side-B draw"
        );
    }

    #[test]
    fn comp_hover_pixel_maps_normalized_and_rejects_outside() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(200.0, 100.0));
        // Center → center pixel of a 200×100 source.
        assert_eq!(
            comp_hover_pixel(egui::pos2(200.0, 100.0), rect, (200, 100)),
            Some((100, 50))
        );
        // Min corner → (0, 0).
        assert_eq!(
            comp_hover_pixel(egui::pos2(100.0, 50.0), rect, (200, 100)),
            Some((0, 0))
        );
        // Just outside (left / above) → None.
        assert_eq!(
            comp_hover_pixel(egui::pos2(99.0, 100.0), rect, (200, 100)),
            None
        );
        assert_eq!(
            comp_hover_pixel(egui::pos2(200.0, 49.0), rect, (200, 100)),
            None
        );
        // Far edge (u or v == 1.0) is excluded → None.
        assert_eq!(
            comp_hover_pixel(egui::pos2(300.0, 100.0), rect, (200, 100)),
            None
        );
        // Zero-sized source → None.
        assert_eq!(
            comp_hover_pixel(egui::pos2(150.0, 75.0), rect, (0, 0)),
            None
        );
        // Differing size maps by fraction (stretch-correct): a 50×25 top layer.
        assert_eq!(
            comp_hover_pixel(egui::pos2(200.0, 100.0), rect, (50, 25)),
            Some((25, 12))
        );
    }

    #[test]
    fn open_layer_adds_a_source_and_records_recent() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("layer.exr");
        write_rgba_exr(&f);

        // Prove the unified entry re-shows the panel even if it was toggled off.
        let mut app = ExrApp {
            show_layers_panel: false,
            ..Default::default()
        };
        app.open_layer(f.clone());

        assert_eq!(
            app.comp_stack.len(),
            1,
            "the dropped/opened file became a layer"
        );
        assert_eq!(app.comp_sources.len(), 1, "one source backs the layer");
        assert_eq!(
            app.recent_files.first(),
            Some(&f),
            "the file is recorded as most-recent"
        );
        assert!(
            app.show_layers_panel,
            "opening a layer shows the Layers panel"
        );
    }

    #[test]
    fn add_comp_source_past_cap_reports_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("layer.exr");
        write_rgba_exr(&f);

        let mut app = ExrApp::default();
        for _ in 0..COMP_LAYER_CAP {
            app.add_comp_source(f.clone());
        }
        assert_eq!(app.comp_stack.len(), COMP_LAYER_CAP);
        assert!(app.error_msg.is_none(), "no error while filling to the cap");

        // One past the cap stays capped and reports it, rather than silently
        // dropping the file (so a multi-file drop past the cap is visible).
        app.add_comp_source(f.clone());
        assert_eq!(app.comp_stack.len(), COMP_LAYER_CAP, "stays capped");
        assert!(
            app.error_msg
                .as_deref()
                .is_some_and(|m| m.contains("Layer limit")),
            "over-cap add reports the cap"
        );
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

    /// A held-but-stationary scrub re-lands on the same frame every UI frame
    /// (`dragged()` stays true with zero pointer movement). Each re-seek used to
    /// bump the epoch, so the held frame's decode was dropped on arrival and
    /// resubmitted forever — never displayed or cached until release (#139).
    /// Same-frame scrubs and boundary-clamped steps must be no-ops.
    #[test]
    fn stationary_scrub_hold_does_not_supersede_its_own_decode() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));

        // The drag lands on frame 3: a real seek — decode in flight.
        app.playback_scrub_to(3);
        let epoch = app.playback.epoch;
        assert_eq!(app.playback.pending, Some(3));

        // The user holds the drag without moving: the same frame re-lands on
        // every subsequent UI frame. The in-flight decode must survive.
        for _ in 0..3 {
            app.playback_scrub_to(3);
        }
        assert_eq!(
            app.playback.epoch, epoch,
            "stationary hold must not supersede the held frame's decode"
        );
        assert_eq!(app.playback.pending, Some(3), "still awaiting the decode");

        // Key-repeat at a range boundary clamps to the same frame — also a no-op.
        app.playback_scrub_to(99); // land on the out point (frame 5)
        let epoch = app.playback.epoch;
        app.playback_step(1);
        assert_eq!(app.playback.current_frame, 5);
        assert_eq!(
            app.playback.epoch, epoch,
            "boundary-clamped step must not supersede the out point's decode"
        );
    }

    /// The same-frame no-op must not swallow retries: a decode *error* clears
    /// `pending` without inserting into T1, so scrubbing onto the errored frame
    /// again has to fall through and re-request it.
    #[test]
    fn same_frame_scrub_retries_after_a_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));

        app.playback_scrub_to(3);
        assert_eq!(app.playback.pending, Some(3));

        // The decode fails: pending clears, nothing lands in T1.
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 3,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: false,
            result: Err("truncated exr".to_string()),
        });
        assert_eq!(app.playback.pending, None);
        assert_eq!(app.error_msg.as_deref(), Some("truncated exr"));

        // Scrubbing onto the same frame is a retry, not a no-op.
        app.playback_scrub_to(3);
        assert_eq!(
            app.playback.pending,
            Some(3),
            "errored frame re-requested by a same-frame scrub"
        );
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
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 2,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: false,
            result: Ok(data2),
        });

        assert!(app.exr_data.is_some(), "frame 2 applied");
        assert_eq!(app.viewer.scale, 3.0, "zoom preserved across the frame");
        assert_eq!(app.viewer.exposure, 1.5, "exposure preserved");
        assert!(!app.loading_a, "decode flag cleared");
        assert_eq!(app.playback.pending, None, "pending cleared on arrival");
    }

    // --- Beauty-only playback decode (#56, hardening step 3) -----------------

    #[test]
    fn beauty_only_decode_gated_on_playing_and_beauty_layer() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        // This case tests the no-precache path; precache now defaults on (#165),
        // so pin it off explicitly (precache-on makes prefetched-while-settled
        // frames beauty, covered separately).
        app.precache = false;

        let cur = app.playback.current_frame; // playhead (frame 1)
        let ahead = cur + 1; // a prefetch frame

        // Stopped, no precache → full decode for every frame (readout/AOV need
        // all channels).
        assert!(!app.decode_beauty_only(cur), "not playing ⇒ full decode");
        assert!(!app.decode_beauty_only(ahead), "not playing ⇒ full decode");

        app.playback_toggle(); // start playing
        assert!(app.playback.is_playing());
        app.viewer.active_layer = 0;
        assert!(
            app.decode_beauty_only(cur),
            "playing + beauty layer ⇒ beauty-only"
        );

        // Viewing a non-beauty AOV: a beauty-only frame wouldn't carry it, so
        // playback stays full-decode for correctness.
        app.viewer.active_layer = 2;
        assert!(
            !app.decode_beauty_only(cur),
            "non-beauty active layer ⇒ full"
        );

        // Kill-switch forces always-full.
        app.viewer.active_layer = 0;
        app.beauty_preview = false;
        assert!(!app.decode_beauty_only(cur), "kill-switch off ⇒ full");

        // Precache while *settled* (#56, step 4): prefetched frames decode beauty
        // for future playback, but the playhead itself stays full so its sampling
        // + AOV switch are correct.
        app.beauty_preview = true;
        app.playback.state = PlayState::Paused;
        app.precache = true;
        assert!(
            !app.decode_beauty_only(cur),
            "precache: the settled playhead stays full"
        );
        assert!(
            app.decode_beauty_only(ahead),
            "precache: prefetched frames are beauty for future playback"
        );

        // An active timeline drag behaves like playing (#143): the held seek
        // decodes beauty-only for responsiveness; the release settles to full.
        app.precache = false;
        app.scrub_active = true;
        assert!(
            app.decode_beauty_only(cur),
            "active scrub ⇒ beauty-only decode"
        );
        app.scrub_active = false;
        assert!(!app.decode_beauty_only(cur), "released ⇒ full decode again");
    }

    /// An EXR of an explicit size, for the viewport-proxy sizing tests (#209),
    /// which need a source whose long side is known and large enough to decimate.
    fn write_sized_exr(path: &std::path::Path, w: usize, h: usize) {
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F16(vec![f16::from_f32(0.5); w * h]),
            ));
        }
        Image::from_layer(Layer::new(
            (w, h),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(path)
        .expect("write sized exr fixture");
    }

    /// A `w × h` EXR in the Karma/Houdini shape the per-AOV decode path (#217)
    /// exists for: `parts` physical parts, each holding exactly **one** render
    /// pass, so logical layer *n* lives alone in part *n*.
    fn write_one_aov_per_part_exr(path: &std::path::Path, w: usize, h: usize, parts: usize) {
        let named = |name: &str| {
            let mut list = smallvec::SmallVec::new();
            for c in ["R", "G", "B"] {
                list.push(AnyChannel::new(
                    Text::from(c),
                    FlatSamples::F16(vec![f16::from_f32(0.5); w * h]),
                ));
            }
            Layer::new(
                (w, h),
                LayerAttributes {
                    layer_name: Some(Text::from(name)),
                    ..Default::default()
                },
                Encoding::FAST_LOSSLESS,
                AnyChannels::sort(list),
            )
        };
        let names = ["beauty", "diffuse", "specular", "normal"];
        let layers: smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]> =
            (0..parts).map(|i| named(names[i % names.len()])).collect();
        Image::from_layers(
            ImageAttributes::new(IntegerBounds::from_dimensions((w, h))),
            layers,
        )
        .write()
        .to_file(path)
        .expect("write one-aov-per-part exr fixture");
    }

    /// A `w × h` EXR in the Blender shape: **one** part whose channel-name
    /// prefixes encode several passes. Part selection can skip nothing here —
    /// every pass lives in the same compressed blocks.
    fn write_single_part_multipass_exr(path: &std::path::Path, w: usize, h: usize) {
        let mut list = smallvec::SmallVec::new();
        for name in [
            "ViewLayer.Combined.R",
            "ViewLayer.Combined.G",
            "ViewLayer.Combined.B",
            "ViewLayer.Depth.Z",
        ] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F16(vec![f16::from_f32(0.5); w * h]),
            ));
        }
        Image::from_layer(Layer::new(
            (w, h),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(path)
        .expect("write single-part multipass exr fixture");
    }

    /// Register `source` as a comp source backed by an already-written file, the
    /// way the Add flow would. The stored `exr_data` is a **full** decode, which is
    /// what `full_layer_table` reads the part layout from (#217).
    fn seed_comp_source_from(
        app: &mut ExrApp,
        source: crate::layer::SourceId,
        path: &std::path::Path,
    ) {
        let data = std::sync::Arc::new(ExrData::load(path).unwrap());
        let size = data.logical_size(0).unwrap_or((0, 0));
        app.comp_sources.insert(
            source,
            CompSource {
                path: path.to_path_buf(),
                exr_data: data,
                size,
                aov: 0,
                bind_group: None,
                texture: None,
                cur_frame: None,
                cur_full: false,
            },
        );
    }

    /// [`seed_comp_source_from`] over a plain single-layer `w × h` RGBA image.
    fn seed_comp_source(
        app: &mut ExrApp,
        source: crate::layer::SourceId,
        path: &std::path::Path,
        w: usize,
        h: usize,
    ) {
        write_sized_exr(path, w, h);
        seed_comp_source_from(app, source, path);
    }

    #[test]
    fn viewport_proxy_target_tracks_on_screen_pixels() {
        // #209: playback should decode the resolution it can actually show — the
        // image's on-screen footprint in physical pixels — rather than a number the
        // user typed. `scale` is points per source pixel, so a 4096-wide source at
        // scale 0.25 occupies 1024 points, and at 1 point-per-pixel that is 1024
        // real pixels of detail. Anything finer cannot be seen.
        let dir = tempfile::tempdir().unwrap();
        let mut app = ExrApp::default();
        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("v.exr"), 1024, 512);

        app.viewer.scale = 0.25;
        assert_eq!(
            app.viewport_proxy_target(s),
            PlaybackProxy::Px(256),
            "fit-to-window"
        );

        // Zooming in asks for more source pixels, automatically and in proportion —
        // the property a fixed knob cannot have.
        app.viewer.scale = 0.5;
        assert_eq!(
            app.viewport_proxy_target(s),
            PlaybackProxy::Px(512),
            "2x zoom => 2x detail"
        );

        // At (or past) 1:1 the proxy path switches OFF rather than asking for a
        // full-size "proxy". A decimation by 1 is a copy, and the on-disk proxy
        // cache would store that full frame as a proxy — ~83 MB each on 4.6K plate
        // footage, under a key only an identical zoom could ever hit.
        app.viewer.scale = 1.0;
        assert_eq!(
            app.viewport_proxy_target(s),
            PlaybackProxy::Full,
            "1:1 ⇒ decode full, not a factor-1 proxy"
        );
        app.viewer.scale = 4.0;
        assert_eq!(
            app.viewport_proxy_target(s),
            PlaybackProxy::Full,
            "past 1:1 ⇒ still full"
        );

        // And a pathological zoom-out floors rather than requesting a thumbnail.
        app.viewer.scale = 0.000_01;
        assert_eq!(
            app.viewport_proxy_target(s),
            PlaybackProxy::Px(MIN_PROXY_PX),
            "floored"
        );

        // A degenerate transform declines to guess: decode full rather than wrong.
        app.viewer.scale = 0.0;
        assert_eq!(app.viewport_proxy_target(s), PlaybackProxy::Unknown);
        app.viewer.scale = f32::NAN;
        assert_eq!(app.viewport_proxy_target(s), PlaybackProxy::Unknown);
    }

    #[test]
    fn hiding_a_layer_stops_it_consuming_the_transport() {
        // #211: hiding a layer stopped it *rendering* but not *working* — it kept
        // dividing the T1 budget and prefetch window, kept taking decode turns, and
        // kept eviction protection. Hiding a layer is the first thing a user reaches
        // for when playback is slow, and it did nothing for throughput.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());
        let a = crate::layer::SourceId(COMP_SOURCE_BASE);
        let b = crate::layer::SourceId(COMP_SOURCE_BASE + 1);

        assert_eq!(app.active_followers().count(), 2, "both visible to start");
        assert_eq!(app.n_active_sources(), 2, "budget split two ways");

        // Hide the *second* layer (the one that isn't driving the clock).
        let hidden_layer = app
            .comp_stack
            .iter()
            .find(|l| matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == b))
            .map(|l| l.id)
            .expect("layer for source b");
        if let Some(l) = app.comp_stack.get_mut(hidden_layer) {
            l.enabled = false;
        }

        assert!(!app.source_is_visible(b), "hidden");
        assert!(app.source_is_visible(a), "still visible");
        assert_eq!(
            app.active_followers().count(),
            1,
            "a hidden layer is not an active source"
        );
        assert_eq!(
            app.n_active_sources(),
            1,
            "hiding a layer must actually give the rest of the stack its budget back"
        );
        assert!(
            !app.cache_playheads().iter().any(|(s, _)| *s == b),
            "a hidden layer's frames lose eviction protection"
        );

        // It keeps tracking the playhead — so un-hiding lands on the right frame —
        // but asks for nothing while dark.
        app.playback.current_frame = 3;
        app.sync_comp_followers();
        assert_eq!(
            app.followers.get(&b).map(|st| st.current_frame),
            Some(3),
            "hidden layers still follow the playhead"
        );
        assert_eq!(
            app.followers.get(&b).and_then(|st| st.pending),
            None,
            "but request nothing while hidden"
        );

        // Un-hiding restores it.
        if let Some(l) = app.comp_stack.get_mut(hidden_layer) {
            l.enabled = true;
        }
        assert_eq!(
            app.active_followers().count(),
            2,
            "un-hidden is active again"
        );
    }

    #[test]
    fn stutter_readiness_follows_the_clock_when_it_moves_off_a_hidden_layer() {
        // Review catch. `transport_awaiting` keyed off `transport_source` while the
        // clock could move to a different source, and a hidden source requests
        // nothing — so its `pending` is always `None`. Asking it "are we ready?"
        // answers yes forever, and Stutter advances past undecoded frames: #200
        // reopened, and silently, since Stutter would just behave as DropFrames.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());
        let a = crate::layer::SourceId(COMP_SOURCE_BASE);
        let b = crate::layer::SourceId(COMP_SOURCE_BASE + 1);
        assert_eq!(app.clock_source(), a, "a claimed the transport");

        // Hide the claimed source; the clock moves to b.
        let a_layer = app
            .comp_stack
            .iter()
            .find(|l| matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == a))
            .map(|l| l.id)
            .expect("layer for a");
        if let Some(l) = app.comp_stack.get_mut(a_layer) {
            l.enabled = false;
        }
        assert_eq!(app.clock_source(), b);

        // b — the visible clock — is awaiting a frame. The transport must wait.
        if let Some(st) = app.followers.get_mut(&b) {
            st.pending = Some(4);
        }
        // The hidden ex-clock has nothing pending, which is what used to make this
        // read "ready".
        assert_eq!(app.followers.get(&a).and_then(|st| st.pending), None);
        assert!(
            app.transport_awaiting(),
            "readiness must track the visible clock, not the hidden claim"
        );

        if let Some(st) = app.followers.get_mut(&b) {
            st.pending = None;
        }
        assert!(!app.transport_awaiting(), "nothing awaited ⇒ not waiting");
    }

    #[test]
    fn solo_hides_the_others_for_the_transport_too() {
        // Visibility goes through `LayerStack::visible`, so solo uses the same rule
        // the compositor renders by: with anything soloed, non-soloed layers are
        // invisible and must stop consuming decode budget as well.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());
        let a = crate::layer::SourceId(COMP_SOURCE_BASE);
        let b = crate::layer::SourceId(COMP_SOURCE_BASE + 1);

        let solo_layer = app
            .comp_stack
            .iter()
            .find(|l| matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == b))
            .map(|l| l.id)
            .expect("layer for source b");
        if let Some(l) = app.comp_stack.get_mut(solo_layer) {
            l.solo = true;
        }

        assert!(app.source_is_visible(b), "soloed layer is visible");
        assert!(!app.source_is_visible(a), "everything else is not");
        assert_eq!(app.n_active_sources(), 1);
    }

    #[test]
    fn the_clock_never_sits_on_a_hidden_layer() {
        // Pacing is recorded only for the clock source, and a hidden source never
        // paints — so a hidden layer holding the clock makes `fps=` read 0.0 through
        // a run that is playing perfectly. Observed live as
        // `settled=[s2:-SOFT, s3:-SOFT, s4:1045full] fps=0.0/24`.
        let (_dir, paths) = write_sequence(5);
        let mut app = ExrApp::default();
        app.add_comp_source(paths[0].clone());
        app.add_comp_source(paths[0].clone());
        let a = crate::layer::SourceId(COMP_SOURCE_BASE);
        let b = crate::layer::SourceId(COMP_SOURCE_BASE + 1);

        // The first sequence claimed the transport.
        assert_eq!(app.clock_source(), a);

        let clock_layer = app
            .comp_stack
            .iter()
            .find(|l| matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == a))
            .map(|l| l.id)
            .expect("layer for source a");
        if let Some(l) = app.comp_stack.get_mut(clock_layer) {
            l.enabled = false;
        }

        assert_eq!(
            app.clock_source(),
            b,
            "the clock moves to a layer that can actually paint"
        );

        // With everything hidden there is nothing better — keep the claim rather
        // than inventing a source, so the transport stays coherent.
        for l in [clock_layer] {
            if let Some(l) = app.comp_stack.get_mut(l) {
                l.enabled = false;
            }
        }
        let other = app
            .comp_stack
            .iter()
            .find(|l| matches!(&l.source, crate::layer::LayerSource::Image { source, .. } if *source == b))
            .map(|l| l.id)
            .expect("layer for source b");
        if let Some(l) = app.comp_stack.get_mut(other) {
            l.enabled = false;
        }
        assert_eq!(
            app.clock_source(),
            a,
            "all hidden ⇒ keep the original claim"
        );
    }

    #[test]
    fn settling_upgrades_a_comp_layer_holding_a_proxy() {
        // #212: `settle_to_full` used to consider only A_SOURCE. With the comp stack
        // driving the transport that source has nothing cached and nothing to
        // decode, so the layers actually on screen were never asked about and kept
        // their proxy frame forever — "stuck in proxy mode if I scrub some".
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.0001.exr");
        // Two frames: `Sequence::from_group` applies a ≥2 rule, and a lone image
        // registers no follower at all — so a one-frame fixture would silently skip
        // the very loop under test.
        write_sized_exr(&src, 1024, 512);
        write_sized_exr(&dir.path().join("p.0002.exr"), 1024, 512);
        touch_sequence(dir.path(), 3);

        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("seed.exr"), 1024, 512);
        app.followers.insert(
            s,
            SourceState {
                sequence: crate::sequence::detect_from_file(&src),
                current_frame: 1,
                ..Default::default()
            },
        );

        // A proxy of the playhead frame is resident — what a scrub leaves behind.
        let proxy = std::sync::Arc::new(ExrData::load_proxy(&src, 128).unwrap());
        assert!(proxy.proxy, "fixture must actually be a proxy");
        app.frame_cache.insert(s, 1, proxy);

        // Settled: the layer must be marked for a full re-decode. `next_want`
        // treats `pending` as "not resident", so this is the upgrade lever.
        app.settle_to_full();
        assert_eq!(
            app.followers.get(&s).and_then(|st| st.pending),
            Some(1),
            "a settled comp layer holding a proxy must re-request it full"
        );

        // Once the full frame lands, settling again must be a no-op — a pause must
        // not re-decode an already-final playhead every time.
        let full = std::sync::Arc::new(ExrData::load(&src).unwrap());
        assert!(!full.proxy && !full.beauty_only);
        app.frame_cache.insert(s, 1, full);
        if let Some(st) = app.followers.get_mut(&s) {
            st.pending = None;
        }
        app.settle_to_full();
        assert_eq!(
            app.followers.get(&s).and_then(|st| st.pending),
            None,
            "an already-full playhead needs no re-decode"
        );
    }

    #[test]
    fn comp_layers_settle_even_when_slot_a_needs_nothing() {
        // Regression guard for a review catch. `settle_followers_to_full` sat after
        // an `if a_full { return }`, so slot A already holding a full frame skipped
        // the comp layers entirely — and A *is* full on the classic transport path,
        // which is precisely where a comp stack can also be present. The
        // comp-driven case has nothing cached for A, so `a_full` is false there and
        // every test and soak written against it passed regardless.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("q.0001.exr");
        write_sized_exr(&src, 256, 128);
        write_sized_exr(&dir.path().join("q.0002.exr"), 256, 128);
        touch_sequence(dir.path(), 3);

        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("seed.exr"), 256, 128);
        app.followers.insert(
            s,
            SourceState {
                sequence: crate::sequence::detect_from_file(&src),
                current_frame: 1,
                ..Default::default()
            },
        );

        // Slot A is resident at full fidelity — the condition that used to skip
        // everything below it.
        let full_a = std::sync::Arc::new(ExrData::load(&src).unwrap());
        assert!(!full_a.proxy && !full_a.beauty_only);
        app.frame_cache
            .insert(ExrApp::A_SOURCE, app.playback.current_frame, full_a);

        // The comp layer is on a proxy and must still be upgraded.
        app.frame_cache.insert(
            s,
            1,
            std::sync::Arc::new(ExrData::load_proxy(&src, 64).unwrap()),
        );

        app.settle_to_full();
        assert_eq!(
            app.followers.get(&s).and_then(|st| st.pending),
            Some(1),
            "a full slot A must not stop the comp layers settling"
        );
    }

    #[test]
    fn a_fidelity_upgrade_rebuilds_the_texture_on_the_same_frame() {
        // The second half of #212. Settling re-decodes the *same* frame number at
        // full fidelity, so `ensure_comp_frame` keyed only on `cur_frame` would
        // early-return and keep the proxy texture bound forever — the correct
        // pixels sitting in the cache, unused. Without this the decode-side fix
        // above changes nothing the user can see.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("u.0001.exr");
        write_sized_exr(&src, 1024, 512);

        let mut app = ExrApp::default();
        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("seed.exr"), 1024, 512);

        // Pretend a proxy of frame 1 is bound (what playback leaves on screen).
        if let Some(cs) = app.comp_sources.get_mut(&s) {
            cs.cur_frame = Some(1);
            cs.cur_full = false;
        }
        app.frame_cache.insert(
            s,
            1,
            std::sync::Arc::new(ExrData::load_proxy(&src, 128).unwrap()),
        );

        // Headless, so no upload can happen — assert on the decision, not the
        // texture: with only a proxy resident there is nothing better to show.
        app.ensure_comp_frame(s, 1, 0);
        assert!(
            !app.comp_sources.get(&s).is_some_and(|cs| cs.cur_full),
            "a proxy resident over a proxy bound is not an upgrade"
        );

        // Now the full frame lands under the same frame number. This must be
        // treated as work to do, not as "already on that frame".
        app.frame_cache
            .insert(s, 1, std::sync::Arc::new(ExrData::load(&src).unwrap()));
        let bound_before = app.comp_sources.get(&s).and_then(|cs| cs.cur_frame);
        app.ensure_comp_frame(s, 1, 0);
        assert_eq!(bound_before, Some(1), "same frame number throughout");
        // The uploader is absent headless, so the observable effect is that the
        // early-return no longer fires — verified by the source still being marked
        // not-full and therefore eligible on every subsequent pass.
        assert!(
            !app.comp_sources.get(&s).is_some_and(|cs| cs.cur_full),
            "still not full until a build lands, so it stays eligible"
        );
    }

    #[test]
    fn a_layer_on_a_non_zero_aov_never_takes_a_cheap_decode() {
        // #213: beauty-only and proxy decodes carry only logical layer 0. The gate
        // used to ask `viewer.active_layer`, which describes the classic path, not a
        // comp stack where each layer has its own AOV. A layer on AOV 1 with the
        // viewer on 0 passed the gate, got a frame without layer 1, and every
        // texture build then failed silently — the layer froze permanently while
        // every decode-side metric read healthy.
        //
        // #217 narrowed *why* this source is refused rather than repealing the
        // rule: the fixture is a single-layer image, so AOV 1 doesn't exist in it
        // and no decode can carry it. The invariant is unchanged — a decode must
        // never be cheaper than the displayed AOV needs. What changed is that a
        // non-zero AOV which *does* own its own part now has a cheap decode that
        // carries it (`a_non_zero_aov_takes_the_cheap_path_when_it_owns_its_part`).
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.viewer.active_layer = 0; // the viewer says "beauty" ...
        app.proxy_enabled = true;
        app.beauty_preview = true;

        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("aov.exr"), 4096, 2048);
        app.viewer.scale = 0.25;
        let f = app.playback.current_frame;
        app.playback_toggle(); // playing

        // ... while the layer drawing this source is on AOV 0: cheap is fine.
        let lid = app
            .comp_stack
            .push_image("l", s, 0, crate::layer::Trim::full(1, 3));
        assert!(
            app.decode_proxy_target_at_for(s, f, f).is_some(),
            "AOV 0 ⇒ a proxy can represent what's on screen"
        );
        assert!(app.decode_beauty_only_for(s, f), "AOV 0 ⇒ beauty is enough");

        // Move that layer to AOV 1 — now neither cheap decode can carry it.
        if let Some(l) = app.comp_stack.get_mut(lid)
            && let crate::layer::LayerSource::Image { aov, .. } = &mut l.source
        {
            *aov = 1;
        }
        assert_eq!(
            app.decode_proxy_target_at_for(s, f, f),
            None,
            "AOV 1 ⇒ full decode, or the texture build fails and the layer freezes"
        );
        assert!(
            !app.decode_beauty_only_for(s, f),
            "AOV 1 ⇒ beauty-only would omit the layer this source displays"
        );

        // The viewer's own active layer is irrelevant to a comp source — it is not
        // what that layer draws.
        app.viewer.active_layer = 3;
        assert_eq!(app.decode_proxy_target_at_for(s, f, f), None);
    }

    /// Stand a playing app up with one comp layer drawing `path` at AOV `aov`,
    /// returning `(source, layer, frame)` — the shape all three gate tests below
    /// share.
    fn playing_with_one_layer_on(
        app: &mut ExrApp,
        dir: &std::path::Path,
        path: &std::path::Path,
        aov: usize,
    ) -> (crate::layer::SourceId, crate::layer::LayerId, u32) {
        touch_sequence(dir, 3);
        app.detect_sequence(&dir.join("s.0001.exr"));
        app.proxy_enabled = true;
        app.beauty_preview = true;
        app.viewer.scale = 0.25;
        let s = crate::layer::SourceId(2);
        seed_comp_source_from(app, s, path);
        let f = app.playback.current_frame;
        app.playback_toggle(); // playing
        let lid = app
            .comp_stack
            .push_image("l", s, aov, crate::layer::Trim::full(1, 3));
        (s, lid, f)
    }

    #[test]
    fn a_non_zero_aov_takes_the_cheap_path_when_it_owns_its_part() {
        // #217: the cliff #213 left behind. A layer on AOV 0 played at proxy
        // resolution; the same layer on AOV 1 fell back to a full all-parts decode
        // — 260 ms against 12 ms on the 16-part reference render, the difference
        // between 24 fps and 3.8. Inspecting a pass is the workflow this app is
        // for, so it should not be the slow case.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("karma.exr");
        write_one_aov_per_part_exr(&src, 4096, 2048, 4);
        let mut app = ExrApp::default();
        let (s, _lid, f) = playing_with_one_layer_on(&mut app, dir.path(), &src, 1);

        // The decode is allowed to be cheap, and it names the part AOV 1 lives in.
        assert_eq!(
            app.cheap_decode_layer(s),
            Some((1, 1)),
            "one pass per part ⇒ decode part 1 alone and call it logical layer 1"
        );
        assert!(app.decode_beauty_only_for(s, f), "cheap decode allowed");
        assert!(app.decode_proxy_target_at_for(s, f, f).is_some());

        // Still the #213 invariant, not a hole in it: the decode carries exactly
        // the AOV on screen, so the texture build has what it needs.
        let decoded = ExrData::load_layer(&src, 1, 1).expect("the gate's chosen decode");
        assert!(
            decoded.logical_channels(1).is_some(),
            "the frame the gate authorised must be able to answer for the displayed AOV"
        );
    }

    #[test]
    fn a_single_part_file_keeps_the_full_decode_on_every_non_zero_aov() {
        // The scope limit, pinned. Blender writes one part with the passes encoded
        // as channel-name prefixes, so every AOV shares the same compressed blocks
        // and part selection can skip nothing. `load_layer` on that part would
        // decode a frame that can address neither pass — and #213's evict-and-retry
        // would re-decode it forever. So the honest claim for #217 is "fast AOV
        // playback on multi-part renders", not "on all renders".
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("blender.exr");
        write_single_part_multipass_exr(&src, 4096, 2048);
        let mut app = ExrApp::default();
        let (s, _lid, f) = playing_with_one_layer_on(&mut app, dir.path(), &src, 1);

        // Both passes live in part 0, so neither owns it.
        let table = app.full_layer_table(s).expect("a full decode is stored");
        assert_eq!(table.logical_layers.len(), 2, "Combined + Depth");
        assert!(
            table.logical_layers.iter().all(|l| l.physical_index == 0),
            "one part holds both"
        );

        assert_eq!(app.single_layer_part(s, 1), None);
        assert_eq!(app.cheap_decode_layer(s), None);
        assert!(
            !app.decode_beauty_only_for(s, f),
            "full decode, as before #217"
        );
        assert_eq!(app.decode_proxy_target_at_for(s, f, f), None);
    }

    #[test]
    fn two_layers_on_one_source_at_different_aovs_force_a_full_decode() {
        // A cheap decode carries a single logical layer, so it can only stand in
        // when there is a single answer to "which pass is on screen". Serving the
        // one that happens to be asked first would freeze the other exactly as #213
        // did — and hidden layers count, or un-hiding one would freeze it.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("karma.exr");
        write_one_aov_per_part_exr(&src, 4096, 2048, 4);
        let mut app = ExrApp::default();
        let (s, _lid, f) = playing_with_one_layer_on(&mut app, dir.path(), &src, 1);
        assert!(app.cheap_decode_layer(s).is_some(), "one AOV: fine");

        // A second layer on the same source at a different pass.
        let other = app
            .comp_stack
            .push_image("l2", s, 2, crate::layer::Trim::full(1, 3));
        assert_eq!(app.displayed_aov(s), None, "two passes, one decode");
        assert_eq!(app.cheap_decode_layer(s), None);
        assert!(!app.decode_beauty_only_for(s, f));
        assert_eq!(app.decode_proxy_target_at_for(s, f, f), None);

        // Hidden still counts — it must be ready the instant it is shown.
        if let Some(l) = app.comp_stack.get_mut(other) {
            l.enabled = false;
        }
        assert_eq!(
            app.cheap_decode_layer(s),
            None,
            "a hidden layer on another AOV would freeze the moment it was shown"
        );
    }

    #[test]
    fn sizing_asks_the_clock_source_not_slot_a() {
        // The latch measures frames from `clock_source()`, so the gate must read
        // that same source's fidelity. Probing `A_SOURCE` instead would consult a
        // source that, under `comp_drives_transport`, never decodes at all — and
        // `displayed_aov(A_SOURCE)` then falls through to `viewer.active_layer`,
        // reintroducing the exact predicate #213/#217 moved the decode path off.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("karma.exr");
        write_one_aov_per_part_exr(&src, 4096, 2048, 4);
        let mut app = ExrApp::default();
        let (s, _lid, _f) = playing_with_one_layer_on(&mut app, dir.path(), &src, 1);
        app.set_transport_source(Some(s));
        assert_eq!(app.clock_source(), s, "the comp source drives the clock");
        app.frame_bytes = Some(1_000_000);
        app.beauty_bytes = Some(80_000);
        app.proxy_bytes = Some(9_000);

        // One layer, one AOV, its own part: cheap, and the proxy figure sizes it.
        assert_eq!(app.sizing_frame_bytes(), Some(9_000));

        // A second layer on the same source at a different pass: no one-layer
        // decode serves both, so full frames land and full bytes must size them.
        // Slot A's gate is untouched by this and would still answer "cheap".
        app.comp_stack
            .push_image("l2", s, 2, crate::layer::Trim::full(1, 3));
        assert_eq!(app.cheap_decode_layer(s), None);
        assert!(
            app.decode_proxy_target_for(ExrApp::A_SOURCE, 2).is_some(),
            "slot A still reports cheap — which is why the probed source matters"
        );
        assert_eq!(
            app.sizing_frame_bytes(),
            Some(1_000_000),
            "the clock source needs full frames, so full bytes size the ring"
        );
    }

    #[test]
    fn the_fast_aov_path_is_refused_without_a_trusted_layer_table() {
        // `full_layer_table` deliberately will not read part layout from the live
        // frame: `exr_data` is swapped to every frame the transport lands, and a
        // per-AOV decode's table is a single renumbered entry — which would answer
        // "one pass per part" about *any* file, including the Blender one above.
        // With nothing full at hand the answer is the pre-#217 behaviour: decode
        // everything.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("karma.exr");
        write_one_aov_per_part_exr(&src, 4096, 2048, 4);
        let mut app = ExrApp::default();
        let (s, _lid, f) = playing_with_one_layer_on(&mut app, dir.path(), &src, 1);
        assert!(
            app.cheap_decode_layer(s).is_some(),
            "full table ⇒ fast path"
        );

        // Swap the source's stored decode for a one-layer one, as if the layout had
        // been read from a played frame.
        let partial = std::sync::Arc::new(ExrData::load_layer(&src, 1, 1).unwrap());
        assert_eq!(partial.logical_layers.len(), 1, "the misleading shape");
        if let Some(cs) = app.comp_sources.get_mut(&s) {
            cs.exr_data = partial;
        }
        assert!(
            app.full_layer_table(s).is_none(),
            "not a table we can trust"
        );
        assert_eq!(app.cheap_decode_layer(s), None);
        assert!(!app.decode_beauty_only_for(s, f));
    }

    #[test]
    fn switching_aov_drops_the_stale_cheap_frames_without_tripping_the_alarm() {
        // The dogfood finding: switching a layer's AOV mid-playback fired the
        // build-failed WARN 102 times in one session. T1 is keyed (source, frame)
        // with no AOV, so every frame decoded for the old pass is still resident
        // and cannot answer for the new one. Evicting on the failed build is
        // correct and self-healing, but it spends an uploader round trip per frame
        // to learn what is knowable up front — and it turns #213's alarm into
        // routine noise.
        //
        // The re-decode itself is not avoidable and this does not try to avoid it:
        // those pixels genuinely are not in the cached frames.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("karma.exr");
        write_one_aov_per_part_exr(&src, 64, 32, 4);
        let mut app = ExrApp::default();
        let (s, _lid, _f) = playing_with_one_layer_on(&mut app, dir.path(), &src, 1);

        // A frame decoded for AOV 1, resident, as an AOV switch would leave it.
        let stale = std::sync::Arc::new(ExrData::load_layer(&src, 1, 1).unwrap());
        app.frame_cache.insert(s, 7, stale);
        assert!(app.frame_cache.peek(s, 7).is_some());

        // Ask for AOV 2 from it: dropped immediately, so the next pump re-decodes.
        app.ensure_comp_frame(s, 7, 2);
        assert!(
            app.frame_cache.peek(s, 7).is_none(),
            "a frame that cannot serve the requested pass is evicted, not submitted"
        );

        // Its own AOV is untouched — this must not evict frames that are fine.
        let good = std::sync::Arc::new(ExrData::load_layer(&src, 1, 1).unwrap());
        app.frame_cache.insert(s, 8, good);
        app.ensure_comp_frame(s, 8, 1);
        assert!(
            app.frame_cache.peek(s, 8).is_some(),
            "the frame answers for AOV 1, so it stays"
        );

        // A **full** decode that still cannot serve the AOV is left alone: that is
        // the case the alarm is actually for, and re-decoding it would loop
        // forever, since the pass does not exist in the file at all.
        let full = std::sync::Arc::new(ExrData::load(&src).unwrap());
        app.frame_cache.insert(s, 9, full);
        app.ensure_comp_frame(s, 9, 99);
        assert!(
            app.frame_cache.peek(s, 9).is_some(),
            "a full frame is never fast-evicted — the failing build must raise it"
        );
    }

    #[test]
    fn a_played_frame_never_drags_the_active_layer_back_to_zero() {
        // `swap_image_arc` clamps `active_layer` to the incoming image's layer
        // count, to survive a frame with fewer layers. A per-AOV decode holds
        // exactly one entry, so clamping to it would reset the user's pass
        // selection on the first played frame — and the gate would then agree the
        // source is "on AOV 0" and keep the fast path running, showing the wrong
        // pass with every metric healthy.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("karma.exr");
        write_one_aov_per_part_exr(&src, 64, 32, 4);
        let mut app = ExrApp::default();
        app.viewer.active_layer = 2;

        let partial = std::sync::Arc::new(ExrData::load_layer(&src, 2, 2).unwrap());
        app.swap_image_arc(partial);
        assert_eq!(
            app.viewer.active_layer, 2,
            "a subset frame is not evidence the image lost layers"
        );

        // A genuinely smaller *full* image still clamps — that guard is why the
        // clamp is there.
        let small = dir.path().join("small.exr");
        write_sized_exr(&small, 64, 32);
        app.swap_image_arc(std::sync::Arc::new(ExrData::load(&small).unwrap()));
        assert_eq!(app.viewer.active_layer, 0, "one real layer ⇒ clamp");
    }

    #[test]
    fn classic_sequences_still_gate_on_the_viewer_layer() {
        // The fallback matters: with no comp layer drawing a source, the viewer's
        // active layer *is* what's displayed, and that path must keep its cheap
        // decodes. Collapsing the two would silently disable the proxy for every
        // plain sequence.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.proxy_enabled = true;
        app.beauty_preview = true;
        let f = app.playback.current_frame;
        app.playback_toggle();

        app.viewer.active_layer = 0;
        assert!(app.decode_beauty_only(f), "beauty layer ⇒ cheap decode");
        app.viewer.active_layer = 2;
        assert!(
            !app.decode_beauty_only(f),
            "non-beauty layer ⇒ full, as before"
        );
    }

    #[test]
    fn proxy_target_is_latched_for_the_duration_of_a_play_run() {
        // #209: the T1 ring is keyed on (source, frame) — resolution is not in the
        // key — so a target that moved mid-run would leave frames of several
        // resolutions in one ring, playing back as sharpness flickering frame to
        // frame. Latching at play start makes that impossible by construction.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.viewer.active_layer = 0;
        app.proxy_enabled = true;

        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("l.exr"), 1024, 512);
        app.viewer.scale = 0.25;
        let f = app.playback.current_frame;

        app.playback_toggle(); // play → latch 1024
        assert_eq!(app.decode_proxy_target_at_for(s, f, f), Some(256));

        // Zooming mid-run does NOT move the target — that is the whole point.
        app.viewer.scale = 1.0;
        assert_eq!(
            app.decode_proxy_target_at_for(s, f, f),
            Some(256),
            "target is held for the run even though the view changed"
        );

        // Stopping and starting again re-latches to the view as it now is — here
        // that means dropping the proxy entirely, since 1:1 needs every pixel.
        app.playback_toggle(); // pause
        app.playback_toggle(); // play again
        assert_eq!(
            app.decode_proxy_target_at_for(s, f, f),
            None,
            "a fresh run picks up the new zoom — at 1:1 that is full res"
        );

        // And the latched "full res" decision is itself held for the run: zooming
        // back out mid-run must not quietly start proxying into a ring already
        // holding full-res frames.
        app.viewer.scale = 0.25;
        assert_eq!(
            app.decode_proxy_target_at_for(s, f, f),
            None,
            "the 'no proxy' decision is latched too, not just a size"
        );
    }

    #[test]
    fn relatching_to_a_new_detail_level_drops_the_wrong_size_ring() {
        // A ring filled at one resolution is wrong for the next, the same way the
        // `proxy_size` knob's ring is — and that knob already clears the cache for
        // exactly this reason. But quantizing to the decimation factor means an
        // ordinary framing nudge does not count as a change, so a precached range
        // is not thrown away every time the user touches the zoom.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.viewer.active_layer = 0;
        app.proxy_enabled = true;

        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("r.exr"), 1024, 512);
        app.viewer.scale = 0.25;

        app.playback_toggle(); // latch 1024
        app.playback_toggle(); // pause

        // A nudge far too small to change the decimation factor: ring survives.
        // With the factor rounded *down* (so the proxy is never upscaled), factor 4
        // spans `needed` in (long/5, long/4] — scale 0.2 to 0.25 here — so 0.24 is
        // the same decode as 0.25 and must not invalidate anything.
        app.frame_cache.insert(
            s,
            1,
            std::sync::Arc::new(ExrData::load(dir.path().join("r.exr")).unwrap()),
        );
        app.viewer.scale = 0.24;
        app.playback_toggle();
        assert!(
            app.frame_cache.contains(s, 1),
            "a sub-band zoom nudge must not discard a precached ring"
        );
        app.playback_toggle();

        // A real change of detail level: the ring is the wrong resolution now.
        app.viewer.scale = 1.0;
        app.playback_toggle();
        assert!(
            !app.frame_cache.contains(s, 1),
            "crossing a decimation band drops frames decoded at the old size"
        );
    }

    #[test]
    fn viewport_proxy_target_handles_a_source_smaller_than_the_floor() {
        // The floor must not be able to exceed the source, or the clamp bounds
        // invert and panic. A 2×2 image is a legitimate input — it is what most of
        // this suite's fixtures are — and an icon-sized source in the wild would
        // have crashed the app on the first played frame.
        let dir = tempfile::tempdir().unwrap();
        let mut app = ExrApp::default();
        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("tiny.exr"), 2, 2);

        // There is nothing to decimate on a 2×2 source at any zoom, so the answer is
        // "decode full" rather than a proxy — which also keeps a pointless full-size
        // blob out of the on-disk proxy cache.
        app.viewer.scale = 0.25;
        assert_eq!(
            app.viewport_proxy_target(s),
            PlaybackProxy::Full,
            "nothing to decimate"
        );
        app.viewer.scale = 100.0;
        assert_eq!(app.viewport_proxy_target(s), PlaybackProxy::Full);
    }

    #[test]
    fn the_proxy_is_never_upscaled_onto_the_screen() {
        // The whole claim of #209 is "visually lossless at the current view", which
        // holds only if the proxy carries at least as many pixels as the screen
        // shows. Rounding the decimation factor up broke that just under 1:1 —
        // `ceil` jumps to factor 2 and delivers half the needed resolution, a 1.8x
        // upscale, exactly where the user has zoomed in to look closely.
        //
        // Swept rather than spot-checked: the failure was at a band boundary, and
        // hand-picked scales are precisely what misses those.
        let dir = tempfile::tempdir().unwrap();
        let mut app = ExrApp::default();
        let s = crate::layer::SourceId(2);
        let long = 1024;
        seed_comp_source(&mut app, s, &dir.path().join("sweep.exr"), long, 704);

        for step in 1..=400 {
            let scale = f64::from(step) * 0.0025; // 0.0025 ..= 1.0
            app.viewer.scale = scale as f32;
            let needed = (long as f64 * scale).ceil() as usize;
            match app.viewport_proxy_target(s) {
                PlaybackProxy::Full => {} // full res is never short
                PlaybackProxy::Px(px) => assert!(
                    px >= needed.min(long) || px == MIN_PROXY_PX,
                    "scale {scale:.4}: proxy {px}px is short of the {needed}px on \
                     screen — it would be upscaled and look soft"
                ),
                PlaybackProxy::Unknown => panic!("scale {scale:.4}: should be known"),
            }
        }
    }

    #[test]
    fn viewport_proxy_target_sizes_from_the_source_not_the_bound_texture() {
        // #209 invariant guard. `cs.size` happens to be safe today — it comes from
        // `logical_size`, which returns a proxy's recorded full-res `display_size`
        // — but the target must not depend on that staying true. Anything that set
        // `cs.size` from real buffer dimensions would feed back on itself: each
        // frame smaller than the last until the picture collapsed to the floor.
        // This pins that the target ignores `cs.size` entirely.
        let dir = tempfile::tempdir().unwrap();
        let mut app = ExrApp::default();
        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("fb.exr"), 1024, 512);
        app.viewer.scale = 0.25;

        let first = app.viewport_proxy_target(s);
        assert_eq!(first, PlaybackProxy::Px(256));

        // Simulate a proxy texture having been bound, as `ensure_comp_frame` does.
        if let Some(cs) = app.comp_sources.get_mut(&s) {
            cs.size = (1024, 512);
        }
        assert_eq!(
            app.viewport_proxy_target(s),
            first,
            "target must not shrink after a proxy texture is bound"
        );
    }

    #[test]
    fn scrub_keeps_the_fixed_knob_and_playback_derives() {
        // #209: the two uses are not the same job. Dragging wants aggressive
        // decimation for latency (the `proxy_size` knob, deliberately small);
        // playback wants "as sharp as the screen can show". One number cannot serve
        // both, and making the user trade them off is what gets the feature turned
        // off entirely.
        let dir = tempfile::tempdir().unwrap();
        let seq = dir.path().join("s.0001.exr");
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&seq);
        app.viewer.active_layer = 0;
        app.proxy_enabled = true;
        // Deliberately *not* the value the viewport derives (256 at this size and
        // zoom), or the assertions below could not tell the two paths apart.
        app.proxy_size = 512;

        let s = crate::layer::SourceId(2);
        seed_comp_source(&mut app, s, &dir.path().join("p.exr"), 1024, 512);
        app.viewer.scale = 0.25;
        let f = app.playback.current_frame;

        app.playback_toggle(); // playing
        assert_eq!(
            app.decode_proxy_target_at_for(s, f, f),
            Some(256),
            "playback derives from the viewport, ignoring the scrub knob"
        );

        app.scrub_active = true;
        assert_eq!(
            app.decode_proxy_target_at_for(s, f, f),
            Some(512),
            "scrubbing keeps the fixed knob, whatever it is set to"
        );
        app.scrub_active = false;

        // The kill-switch still wins over both.
        app.proxy_enabled = false;
        assert_eq!(app.decode_proxy_target_at_for(s, f, f), None);
    }

    #[test]
    fn proxy_target_gated_on_moving_and_toggle() {
        // #94: a scrub proxy is decoded on the same cheap-while-moving gate as
        // beauty (playing / drag / precache prefetch, beauty layer only), behind
        // the `proxy_enabled` kill-switch; the settled playhead is always full.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.viewer.active_layer = 0;
        let cur = app.playback.current_frame;
        let ahead = cur + 1;
        let size = app.proxy_size;

        assert_eq!(app.decode_proxy_target(cur), None, "settled ⇒ full");

        app.playback_toggle(); // playing
        assert_eq!(app.decode_proxy_target(cur), Some(size), "playing ⇒ proxy");

        app.viewer.active_layer = 2;
        assert_eq!(
            app.decode_proxy_target(cur),
            None,
            "non-beauty layer ⇒ full (proxy carries only the beauty layer)"
        );
        app.viewer.active_layer = 0;

        app.proxy_enabled = false;
        assert_eq!(app.decode_proxy_target(cur), None, "toggle off ⇒ full");
        app.proxy_enabled = true;

        // Settled + precache: prefetch proxies, the playhead itself stays full.
        app.playback.state = PlayState::Paused;
        app.precache = true;
        assert_eq!(
            app.decode_proxy_target(cur),
            None,
            "precache: the settled playhead stays full"
        );
        assert_eq!(
            app.decode_proxy_target(ahead),
            Some(size),
            "precache prefetch ⇒ proxy"
        );
    }

    /// #143: mid-drag, a resident beauty ring frame displays without being
    /// marked for a full upgrade (that would spam full decodes per touched
    /// frame); the drag release settles the landing frame to full instead.
    #[test]
    fn active_scrub_shows_resident_beauty_and_settles_full_on_release() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let f3 = dir.path().join("s.0003.exr");
        write_rgba_exr(&f3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));

        let beauty = std::sync::Arc::new(ExrData::load_beauty(&f3).unwrap());
        assert!(beauty.beauty_only);
        app.frame_cache.insert(ExrApp::A_SOURCE, 3, beauty);

        // Mid-drag: the resident beauty frame shows immediately, no upgrade.
        app.scrub_active = true;
        app.playback_scrub_to(3);
        assert_eq!(
            app.playback.pending, None,
            "beauty ring frame displays without a mid-drag full upgrade"
        );

        // Release: the landing frame settles to a full all-AOV decode.
        app.scrub_active = false;
        app.settle_to_full();
        assert_eq!(
            app.playback.pending,
            Some(3),
            "release re-requests the landing frame in full"
        );
    }

    /// #144: the contact sheet freezes (skips per-frame thumbnail invalidation)
    /// whenever the transport is busy — playing, a seek in flight, or a held
    /// timeline drag — and refreshes only once it settles. Gate mirrors the
    /// histogram suppression (#141) plus the explicit scrub flag.
    #[test]
    fn thumbs_suppressed_tracks_transport_and_scrub() {
        use crate::playback::PlayState;
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        assert!(app.playback.is_active(), "sequence loaded");

        // Settled: not playing, nothing pending, no drag → the sheet refreshes.
        app.playback.state = PlayState::Paused;
        app.playback.pending = None;
        app.scrub_active = false;
        assert!(!app.thumbs_suppressed(), "settled ⇒ not suppressed");

        // Playing ⇒ frozen.
        app.playback.start_playing(std::time::Instant::now());
        assert!(app.thumbs_suppressed(), "playing ⇒ suppressed");

        // Paused but a seek is still in flight ⇒ frozen until it lands.
        app.playback.state = PlayState::Paused;
        app.playback.pending = Some(3);
        assert!(app.thumbs_suppressed(), "pending decode ⇒ suppressed");

        // A held timeline drag ⇒ frozen even with nothing pending.
        app.playback.pending = None;
        app.scrub_active = true;
        assert!(app.thumbs_suppressed(), "active scrub ⇒ suppressed");

        // Drag released and settled ⇒ refreshes again.
        app.scrub_active = false;
        assert!(
            !app.thumbs_suppressed(),
            "released + settled ⇒ not suppressed"
        );
    }

    // --- Decode-stall recovery (Windows freeze, unrecoverable-hang class) -----

    /// Put `app` into sequence mode over `count` touched frames, playing, with a
    /// wedged decode: frame 1 submitted but never returned, so `inflight`/`pending`
    /// stay set and `pump_decode` is gated. Returns the pre-stall epoch.
    fn stuck_playing_app(dir: &std::path::Path, count: u32) -> ExrApp {
        touch_sequence(dir, count);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.join("s.0001.exr"));
        app.playback.start_playing(std::time::Instant::now());
        app.inflight.insert(1);
        app.playback.pending = Some(1);
        app.loading_a = true;
        app
    }

    #[test]
    fn watchdog_recovers_a_stuck_decode() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = stuck_playing_app(dir.path(), 3);
        let e0 = app.playback.epoch;
        // Backdate the submission past the floor timeout so the watchdog fires.
        app.decode_submit_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(30));
        app.last_decode_dur = None; // no measurement → 10s floor applies

        app.tick_decode_watchdog();

        assert_ne!(
            app.playback.epoch, e0,
            "watchdog supersedes the stuck decode (bumps the epoch)"
        );
        assert!(
            app.load_rx.is_some(),
            "worker is live again after recovery (respawned via re-request)"
        );
        assert!(
            app.inflight.contains(&1),
            "the playhead is re-requested, so the pump is unblocked again"
        );
        let waited = app.decode_submit_at.map(|t| t.elapsed()).unwrap();
        assert!(
            waited < std::time::Duration::from_secs(5),
            "the stall clock is reset on recovery"
        );
    }

    #[test]
    fn watchdog_holds_within_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = stuck_playing_app(dir.path(), 3);
        let e0 = app.playback.epoch;
        // A fresh submission: a genuinely slow decode must not be force-recovered.
        app.decode_submit_at = Some(std::time::Instant::now());
        app.last_decode_dur = None;

        app.tick_decode_watchdog();

        assert_eq!(app.playback.epoch, e0, "no recovery before the timeout");
        assert!(app.inflight.contains(&1), "in-flight decode left untouched");
        assert_eq!(app.playback.pending, Some(1), "still awaiting the frame");
    }

    #[test]
    fn watchdog_is_a_noop_without_outstanding_work() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        let e0 = app.playback.epoch;
        // Old submit clock but nothing outstanding (settled): must not fire.
        app.decode_submit_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));

        app.tick_decode_watchdog();

        assert_eq!(app.playback.epoch, e0, "idle transport is never recovered");
    }

    #[test]
    fn stale_epoch_result_leaves_the_live_inflight_intact() {
        // The epoch-mismatch drop in `apply_load_result` returns *before*
        // `inflight.remove`. That is correct precisely because a re-requested same
        // frame number under the new epoch must not be evicted by its own stale
        // predecessor — otherwise the pump would leak. This guards that invariant.
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        write_rgba_exr(&f1);
        for n in 2..=3 {
            write_rgba_exr(&dir.path().join(format!("s.{n:04}.exr")));
        }
        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&f1).unwrap())),
            ..Default::default()
        };
        app.detect_sequence(&f1);
        // Seek to frame 2: bumps the epoch and re-requests 2 at the live epoch.
        app.playback_step(1);
        let live_epoch = app.playback.epoch;
        assert!(
            app.inflight.contains(&2),
            "frame 2 in flight at the live epoch"
        );
        let dropped_before = app.dbg_dropped_epoch;

        // A stale result for frame 2 (a pre-seek decode) arrives late.
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 2,
            epoch: live_epoch.wrapping_sub(1),
            open_gen: 0,
            fell_back: false,
            result: Ok(ExrData::load(&f1).unwrap()),
        });
        assert_eq!(
            app.dbg_dropped_epoch,
            dropped_before + 1,
            "the stale result is dropped"
        );
        assert!(
            app.inflight.contains(&2),
            "the stale drop must not evict the live in-flight frame"
        );
        assert_eq!(
            app.playback.pending,
            Some(2),
            "still awaiting the live frame"
        );

        // The live-epoch result then lands and clears the in-flight entry — proof
        // there was no permanent leak.
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 2,
            epoch: live_epoch,
            open_gen: 0,
            fell_back: false,
            result: Ok(ExrData::load(&f1).unwrap()),
        });
        assert!(
            !app.inflight.contains(&2),
            "the live result clears the in-flight entry"
        );
        assert_eq!(
            app.playback.pending, None,
            "pending cleared on the live frame"
        );
    }

    #[test]
    fn submit_job_respawns_a_dead_worker() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        write_rgba_exr(&f1);
        let mut app = ExrApp::default();
        // Wire a dead job channel (receiver dropped) so the next send fails, as it
        // would if the worker thread had exited.
        let (dead_tx, dead_rx) = std::sync::mpsc::channel::<LoadJob>();
        drop(dead_rx);
        let (_orphan_tx, orphan_rx) = std::sync::mpsc::channel::<LoadMsg>();
        app.load_tx = Some(dead_tx);
        app.load_rx = Some(orphan_rx);

        app.submit_job(LoadJob {
            path: PathBuf::from("x.exr"),
            source: ExrApp::A_SOURCE,
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: app.open_gen_a,
            beauty_only: false,
            proxy_target: None,
            aov_layer: None,
        });

        // The dead channel was replaced and a fresh worker spawned; it processes
        // the resent job and delivers a result over the new receiver.
        let got = app
            .load_rx
            .as_ref()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10));
        assert!(
            got.is_ok(),
            "respawned worker delivered a result for the resent job"
        );
    }

    #[test]
    fn scrub_publishes_the_epoch_to_the_worker_signal() {
        // Rapid scrubbing floods the worker's FIFO queue with soon-stale jobs; the
        // worker skips any whose epoch is below this shared signal, so the backlog
        // drains without full decodes. Each scrub must publish the bumped epoch.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));

        let before = app.epoch_signal.load(std::sync::atomic::Ordering::Relaxed);
        app.playback_scrub_to(3);
        let after = app.epoch_signal.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after, app.playback.epoch,
            "a scrub publishes its bumped epoch to the worker signal"
        );
        assert_ne!(after, before, "the published epoch advanced");
    }

    #[test]
    fn ram_budget_bytes_maps_gb_with_zero_as_auto() {
        let mut app = ExrApp::default();
        assert_eq!(app.ram_budget_bytes(), None, "0 GB → auto (no cap)");
        app.ram_budget_gb = 4.0;
        assert_eq!(app.ram_budget_bytes(), Some(4 * (1 << 30)), "4 GB → 4 GiB");
        app.ram_budget_gb = 0.5;
        assert_eq!(app.ram_budget_bytes(), Some(1 << 29), "0.5 GB → 512 MiB");
        // A tiny positive (below the display threshold, renders as "Auto") must
        // behave as auto, not as an ultra-low cap.
        app.ram_budget_gb = 0.01;
        assert_eq!(
            app.ram_budget_bytes(),
            None,
            "sub-display-threshold value reads Auto and applies no cap"
        );
    }

    #[test]
    fn precache_prefetches_the_range_while_paused() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        write_rgba_exr(&f1);
        for n in 2..=5 {
            write_rgba_exr(&dir.path().join(format!("s.000{n}.exr")));
        }
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);
        app.frame_cache_cap = 8; // budget comfortably exceeds the 5-frame range
        // Playhead (frame 1) resident, so it isn't what gets pumped.
        app.frame_cache.insert(
            ExrApp::A_SOURCE,
            1,
            std::sync::Arc::new(ExrData::load(&f1).unwrap()),
        );
        assert_eq!(app.playback.current_frame, 1);
        app.playback.state = PlayState::Paused;

        // A plain paused decode prefetches nothing (just the playhead, already here).
        app.precache = false;
        app.pump_decode();
        assert!(
            app.inflight.is_empty(),
            "paused + no precache ⇒ no prefetch"
        );

        // Precache fills ahead into the range even while paused.
        app.precache = true;
        app.pump_decode();
        assert!(
            app.inflight.contains(&2),
            "precache prefetches the next in-range frame while paused"
        );
    }

    #[test]
    fn precache_latches_when_filled_and_resets_on_playhead_move() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        write_rgba_exr(&f1);
        for n in 2..=3 {
            write_rgba_exr(&dir.path().join(format!("s.000{n}.exr")));
        }
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);
        app.frame_cache_cap = 8; // the 3-frame range fits comfortably
        app.precache = true;

        // Whole range resident → tick_precache finds nothing to fetch and latches,
        // so it stops re-pumping (and the decode→evict churn stops).
        for n in 1..=3 {
            app.frame_cache.insert(
                ExrApp::A_SOURCE,
                n,
                std::sync::Arc::new(ExrData::load(&f1).unwrap()),
            );
        }
        app.tick_precache();
        assert!(
            app.precache_filled,
            "latches once the range is fully resident"
        );
        // Latched: a second tick submits nothing.
        app.tick_precache();
        assert!(app.inflight.is_empty(), "no churn while latched");

        // A scrub (playhead move) clears the latch so the new window refills.
        app.playback_scrub_to(2);
        assert!(!app.precache_filled, "playhead move clears the latch");
    }

    #[test]
    fn precache_latches_when_budget_full_even_if_range_exceeds_it() {
        // Regression for the Windows playback freeze: with the in/out range larger
        // than the RAM budget, `next_want` always finds a non-resident frame (it
        // loop-wraps to the far side), so the old "latch when nothing is wanted"
        // never fired and precache churned decode→evict forever, starving the
        // playhead's own frames. The latch must also fire when the cache is full.
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        write_rgba_exr(&f1);
        for n in 2..=5 {
            write_rgba_exr(&dir.path().join(format!("s.{n:04}.exr")));
        }
        let mut app = ExrApp::default();
        app.detect_sequence(&f1); // playhead = 1, paused
        app.frame_cache_cap = 3; // budget (3) < 5-frame range
        app.precache = true;

        // Cache at capacity, but holding frames *outside* the playhead's prefetch
        // window (window ahead of frame 1 is 2,3; we hold 1,4,5), so `next_want`
        // still wants an in-window frame. Pre-fix this state churned forever.
        for n in [1u32, 4, 5] {
            app.frame_cache.insert(
                ExrApp::A_SOURCE,
                n,
                std::sync::Arc::new(ExrData::load(&f1).unwrap()),
            );
        }
        assert_eq!(
            app.frame_cache.len(),
            app.frame_cache_cap,
            "cache at capacity"
        );
        app.tick_precache();
        assert!(
            app.precache_filled,
            "latches when the budget is full even though the range isn't fully resident"
        );
        assert!(
            !app.inflight.is_empty(),
            "latched on the capacity check (a frame was still wanted), not nothing-wanted"
        );
    }

    #[test]
    fn latched_precache_does_not_churn_via_the_apply_pump_loop() {
        // Once precache has latched (cache full, range > budget), `pump_decode`
        // must submit nothing — otherwise the apply→pump loop (which re-pumps after
        // every result, bypassing the `tick_precache` latch) keeps fetching far
        // frames that `evict_to` immediately drops: decode→evict churn forever
        // while stopped, pegging the worker. Regression for the stopped-churn hang.
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        write_rgba_exr(&f1);
        for n in 2..=5 {
            write_rgba_exr(&dir.path().join(format!("s.{n:04}.exr")));
        }
        let mut app = ExrApp::default();
        app.detect_sequence(&f1); // playhead 1, stopped
        app.frame_cache_cap = 3; // budget < 5-frame range
        app.precache = true;
        app.precache_filled = true; // already latched by an earlier fill
        for n in [1u32, 4, 5] {
            app.frame_cache.insert(
                ExrApp::A_SOURCE,
                n,
                std::sync::Arc::new(ExrData::load(&f1).unwrap()),
            );
        }

        app.pump_decode();
        assert!(
            app.inflight.is_empty(),
            "a latched precache submits nothing on re-pump (no decode/evict churn)"
        );
    }

    #[test]
    fn settling_on_a_beauty_only_frame_awaits_a_full_redecode() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        let f2 = dir.path().join("s.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2); // a second frame makes it a real sequence
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);
        app.playback.current_frame = 1;

        // Simulate the play-time ring: a beauty-only frame resident at the playhead.
        let beauty = std::sync::Arc::new(ExrData::load_beauty(&f1).unwrap());
        assert!(beauty.beauty_only);
        app.frame_cache.insert(ExrApp::A_SOURCE, 1, beauty);
        app.playback.state = PlayState::Paused; // settled

        app.request_sequence_frame(1);
        // The beauty frame is shown instantly, but the playhead stays awaited so
        // the readout is suppressed until the full all-AOV frame lands.
        assert!(app.exr_data.is_some(), "beauty frame shown immediately");
        assert_eq!(app.playback.pending, Some(1), "full re-decode awaited");
        // Regression: the upgrade must be *submitted*, not merely marked pending.
        // `contains` is fidelity-blind, so a beauty-resident playhead must still be
        // pumped — otherwise `pending` sticks forever and playback freezes.
        assert!(
            app.inflight.contains(&1),
            "the full re-decode is actually submitted to the worker"
        );
        assert!(
            app.loading_a,
            "the awaited playhead drives the loading state"
        );
        assert!(
            app.playback.sampling_suppressed(),
            "readout suppressed until the full frame lands"
        );
        // And the re-decode it pumped is a *full* decode, not beauty-only.
        assert!(!app.decode_beauty_only(app.playback.current_frame));

        // Deliver the full frame: cache upgrades, sampling re-enables.
        let full = ExrData::load(&f1).unwrap();
        assert!(!full.beauty_only);
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 1,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: false,
            result: Ok(full),
        });
        assert_eq!(
            app.playback.pending, None,
            "settled once the full frame lands"
        );
        assert!(
            !app.frame_cache
                .get(ExrApp::A_SOURCE, 1)
                .unwrap()
                .beauty_only,
            "ring entry upgraded to the full decode"
        );
        assert!(!app.playback.sampling_suppressed(), "readout live again");
    }

    #[test]
    fn stop_halts_in_place_and_does_not_rewind() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 5);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));

        // Play forward to frame 3, then stop.
        app.playback_toggle();
        app.advance_playhead();
        app.advance_playhead();
        assert_eq!(app.playback.current_frame, 3);

        app.playback_stop();
        assert_eq!(app.playback.state, PlayState::Stopped);
        assert_eq!(
            app.playback.current_frame, 3,
            "stop halts on the current frame, not the in-point"
        );

        // The |< button still rewinds to the in-point.
        app.playback_scrub_to(app.playback.in_point);
        assert_eq!(app.playback.current_frame, 1);
    }

    #[test]
    fn settle_is_a_noop_when_the_playhead_is_already_full_resident() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        let f2 = dir.path().join("s.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2);
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);
        app.playback.current_frame = 1;

        // A full (all-AOV) frame already resident at the playhead.
        let full = std::sync::Arc::new(ExrData::load(&f1).unwrap());
        assert!(!full.beauty_only);
        app.frame_cache.insert(ExrApp::A_SOURCE, 1, full);
        app.playback.state = PlayState::Paused;
        let epoch = app.playback.epoch;

        app.settle_to_full();
        assert_eq!(app.playback.pending, None, "no re-decode scheduled");
        assert!(app.inflight.is_empty(), "no in-flight work");
        assert_eq!(
            app.playback.epoch, epoch,
            "epoch untouched — no supersession when the frame is already full"
        );
    }

    #[test]
    fn pausing_from_play_settles_the_beauty_frame_to_full() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        let f2 = dir.path().join("s.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2); // a second frame makes it a real sequence
        let mut app = ExrApp::default();
        app.detect_sequence(&f1);
        app.playback.current_frame = 1;

        let beauty = std::sync::Arc::new(ExrData::load_beauty(&f1).unwrap());
        app.frame_cache.insert(ExrApp::A_SOURCE, 1, beauty);
        app.exr_data = app.frame_cache.peek(ExrApp::A_SOURCE, 1);
        app.playback.start_playing(std::time::Instant::now());

        app.playback_toggle(); // play → pause triggers the settle
        assert_eq!(app.playback.state, PlayState::Paused);
        assert_eq!(
            app.playback.pending,
            Some(1),
            "pausing on a beauty frame awaits its full re-decode"
        );
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
    }

    // --- T1 ring cache + epoch (#56/#57, Phase 3) ----------------------------

    /// Deliver a sequence frame to the app as the worker would, at the live epoch.
    fn deliver_frame(app: &mut ExrApp, path: &std::path::Path, frame: u32) {
        let data = ExrData::load(path).unwrap();
        app.apply_load_result(LoadResult {
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            open_gen: 0,
            fell_back: false,
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
        assert!(app.frame_cache.contains(ExrApp::A_SOURCE, 2));

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
            source: ExrApp::A_SOURCE,
            seq_frame: true,
            frame: 2,
            epoch: stale_epoch,
            open_gen: 0,
            fell_back: false,
            result: Ok(data2),
        });
        assert!(
            !app.frame_cache.contains(ExrApp::A_SOURCE, 2),
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
        assert!(app.frame_cache.contains(ExrApp::A_SOURCE, 1));
        assert!(app.inflight.contains(&2), "prefetching frame 2 ahead");

        // Frame 2 is ahead of the playhead: cached but NOT shown; prefetch rolls on.
        deliver_frame(&mut app, &paths[1], 2);
        assert!(app.frame_cache.contains(ExrApp::A_SOURCE, 2));
        assert!(app.inflight.contains(&3));
        let _ = dir;
        assert_eq!(
            app.playback.current_frame, 1,
            "playhead unmoved — only the clock advances it, not prefetch"
        );
    }

    // --- Prefetch window sizing (#207/#216) ----------------------------------

    #[test]
    fn play_does_not_shrink_the_prefetch_window() {
        // The #207 defect in one assertion: the window used to be the whole budget
        // while idle and `min(budget, MAX_PREFETCH = 16)` once playing, so pressing
        // Play *shrank* the lookahead — backwards, since play is when read-ahead
        // matters most. Both states must now read the same figure.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 300);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.frame_cache_cap = 121; // a realistic measured budget (#199)

        let idle = app.prefetch_depth();
        app.playback_toggle();
        assert!(app.playback.is_playing());
        assert_eq!(
            app.prefetch_depth(),
            idle,
            "the window must not depend on whether the transport is running"
        );
    }

    #[test]
    fn prefetch_window_scales_with_the_budget_past_the_old_constant() {
        // `MAX_PREFETCH = 16` was chosen while `frame_cache_cap` was frozen at its
        // constructed default of 8 (#199), where it never bound. With a real budget
        // it became the binding constraint instead of memory: 121 slots yielded a
        // 16-frame window, and 2 frames per source across a 6-layer stack.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 300);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.playback_toggle();

        app.frame_cache_cap = 121;
        assert_eq!(app.prefetch_depth(), 120, "the budget sizes the window");

        // And the lookback window (#169) comes back with it. `read_behind` floors to
        // zero below a depth of 4, so at 6 sources the old 16-frame window left
        // `16 / 6 = 2` each and no read-behind at all — scrubbing backwards during
        // play always missed. This is the arithmetic that fixes that, without
        // touching `read_behind` itself.
        let per_source = app.prefetch_depth() / 6;
        assert_eq!(per_source, 20);
        assert!(
            crate::scheduler::read_behind(per_source) > 0,
            "a 6-layer stack keeps a lookback window"
        );
    }

    #[test]
    fn prefetch_window_never_walks_past_the_range() {
        // What bounds the window on a short range now that the constant is gone.
        // Past the loop wrap `want_list` only re-lists frames already on the list,
        // so walking further is pure iteration: an 8-frame Beachball against a
        // 734-frame budget must walk 7 positions, not 733.
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 8);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.playback_toggle();
        app.frame_cache_cap = 734;

        assert_eq!((app.playback.in_point, app.playback.out_point), (1, 8));
        assert_eq!(
            app.prefetch_depth(),
            7,
            "bounded by the span, not the budget"
        );

        // Trimming the range tightens it further — the in/out points are the range,
        // not the file count.
        app.playback_scrub_to(3);
        app.playback_set_in();
        app.playback_scrub_to(5);
        app.playback_set_out();
        assert_eq!(app.prefetch_depth(), 2, "a 3-frame trim walks 2 positions");
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
        assert!(app.frame_cache.contains(ExrApp::A_SOURCE, 1));
        assert!(app.frame_cache.contains(ExrApp::A_SOURCE, 2));
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
    fn drop_frames_resumes_decode_after_skipping_past_an_inflight_frame() {
        use crate::playback::Pacing;
        let (_dir, paths) = write_sequence(10);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]); // playhead 1
        app.frame_cache_cap = 4;
        app.playback.pacing = Pacing::DropFrames;
        app.playback.loop_mode = LoopMode::Once;
        app.playback.state = PlayState::Playing;
        app.playback.fps_target = 24.0;

        // Frame 1's decode is in flight (the awaited playhead).
        app.request_sequence_frame(1);
        assert!(app.inflight.contains(&1) && app.loading_a);

        // Wall-time jumps: drop-frames skips the playhead past the awaited frame.
        let period = app.playback.period();
        app.playback.anchor = Some(std::time::Instant::now() - period.mul_f32(3.5));
        app.playback.frames_since_anchor = 0;
        app.tick_drop_frames(period);
        let cur = app.playback.current_frame;
        assert!(cur > 1, "skipped ahead of the in-flight frame");
        assert!(
            !app.loading_a,
            "the awaited flag is cleared for the frame we skipped past"
        );

        // The stale frame-1 decode lands (still a valid cache fill). The pump must
        // resume for the new playhead instead of staying gated on `loading_a`.
        deliver_frame(&mut app, &paths[0], 1);
        assert!(
            app.frame_cache.contains(ExrApp::A_SOURCE, 1),
            "stale result is cached, not discarded"
        );
        assert!(
            app.inflight.contains(&cur),
            "pump resumed for the new playhead once the worker freed up"
        );
    }

    // --- render-watch (#101) -------------------------------------------------

    #[test]
    fn render_watch_picks_up_new_frames_and_extends_the_range() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.watch_enabled = true;
        assert_eq!((app.playback.in_point, app.playback.out_point), (1, 3));

        assert!(!app.rescan_and_apply(), "first call only baselines");

        // A render writes two more frames onto the end.
        std::fs::write(dir.path().join("s.0004.exr"), b"").unwrap();
        std::fs::write(dir.path().join("s.0005.exr"), b"").unwrap();
        assert!(app.rescan_and_apply(), "new frames applied");
        assert_eq!(app.playback.sequence.as_ref().unwrap().range, (1, 5));
        assert_eq!(
            app.playback.out_point, 5,
            "untrimmed out-point follows growth"
        );
    }

    #[test]
    fn render_watch_respects_a_trimmed_out_point() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr"));
        app.watch_enabled = true;
        app.rescan_and_apply(); // baseline

        // User trims the out-point in to frame 2.
        app.playback.current_frame = 2;
        app.playback_set_out();
        assert_eq!(app.playback.out_point, 2);

        std::fs::write(dir.path().join("s.0004.exr"), b"").unwrap();
        app.rescan_and_apply();
        assert_eq!(
            app.playback.sequence.as_ref().unwrap().range,
            (1, 4),
            "range still tracks disk"
        );
        assert_eq!(app.playback.out_point, 2, "user trim preserved, not grown");
    }

    #[test]
    fn render_watch_evicts_and_refreshes_a_rerendered_frame() {
        let (dir, paths) = write_sequence(3);
        let mut app = ExrApp::default();
        app.detect_sequence(&paths[0]); // playhead on frame 1
        app.watch_enabled = true;
        app.rescan_and_apply(); // baseline against the original files

        // Frame 1 is cached (it's the on-screen frame).
        deliver_frame(&mut app, &paths[0], 1);
        assert!(app.frame_cache.contains(ExrApp::A_SOURCE, 1));

        // The render rewrites frame 1 with different content (size changes -> the
        // signature changes even if mtime resolution is coarse).
        std::fs::write(&paths[0], vec![0u8; 4096]).unwrap();
        assert!(app.rescan_and_apply(), "re-rendered frame applied");

        assert!(
            !app.frame_cache.contains(ExrApp::A_SOURCE, 1),
            "stale cached pixels dropped for the re-rendered frame"
        );
        assert_eq!(
            app.playback.pending,
            Some(1),
            "the re-rendered on-screen frame is re-requested"
        );
        let _ = dir;
    }

    #[test]
    fn render_watch_follow_parks_on_the_newest_frame() {
        let dir = tempfile::tempdir().unwrap();
        touch_sequence(dir.path(), 3);
        let mut app = ExrApp::default();
        app.detect_sequence(&dir.path().join("s.0001.exr")); // playhead 1
        app.watch_enabled = true;
        app.watch_follow = true;
        app.rescan_and_apply(); // baseline

        std::fs::write(dir.path().join("s.0004.exr"), b"").unwrap();
        app.rescan_and_apply();
        assert_eq!(
            app.playback.current_frame, 4,
            "follow jumps the playhead to the newest frame"
        );
    }
}
