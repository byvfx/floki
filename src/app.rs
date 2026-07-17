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
/// (#57), a **slot-A explicit open** by `open_gen` (#109), and a **slot-B
/// open** by `path` (its `b.loaded_file` is never rewritten by playback).
struct LoadResult {
    path: PathBuf,
    /// Which source this decode is for (#99 unification): the A/B slots map to
    /// fixed `SourceId`s via `cache::Slot` (A→0, B→1). For an explicit open it
    /// selects B-open vs A-open (path/generation supersession); for a **seq frame**
    /// (`seq_frame`) it is the cache slot (locked-step A/B, #98).
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
    result: Result<ExrData, String>,
}

/// serde default for `proxy_size` (#94): the scrub-proxy long-side pixel target
/// (~half of 1080p → ~4× more frames fit, per the OpenRV model).
fn default_proxy_size() -> usize {
    1024
}

/// Hard cap on Layers-panel composite sources (#99 PR-B). Playback footprint is
/// bounded on 8 GB (see the roadmap); the panel disables Add at this many layers.
const COMP_LAYER_CAP: usize = 6;

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
    /// Which source this decode is for (#99 unification): A/B slots map to fixed
    /// `SourceId`s via `cache::Slot` (A→0, B→1). Selects B-open vs A-open for an
    /// explicit open; the cache slot for a **seq frame** (`seq_frame`, #98).
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

/// One image source's decode + playback state (#99 unification). Today this holds
/// the single locked-step **B** follower (#98); P1.4b generalizes it to a
/// per-`SourceId` collection so the comp stack's N sources each carry their own.
/// The master clock (`ExrApp::playback`) drives the primary source; a follower's
/// `current_frame` is a slaved function of the global playhead, and it decodes into
/// the T1 cache under its own `SourceId`. All fields default to the "no follower"
/// state so a lone-image B (or no B) behaves exactly as the old defaulted `*_b`.
#[derive(Default)]
struct SourceState {
    /// The follower's opened path — path-keyed explicit-open supersession.
    loaded_file: Option<PathBuf>,
    /// The follower's detected sequence (`None` → a lone image that holds).
    sequence: Option<crate::sequence::Sequence>,
    /// The follower's current frame (position-aligned to the global playhead).
    current_frame: u32,
    /// User A/B frame offset (#166): the follower shows its aligned frame + this,
    /// clamped to its range. Runtime, footage-specific; resets to 0 each session.
    offset: i32,
    /// The follower frame the transport is awaiting (`None` when resident).
    pending: Option<u32>,
    /// Follower seq frames submitted-but-not-returned — a separate set from the
    /// primary's `inflight` so the primary's heavily-tested path is untouched.
    inflight: std::collections::HashSet<u32>,
    /// Whether an explicit follower open is decoding.
    loading: bool,
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
    /// Read-ahead file warmer (#164): pulls the next wanted frames' files
    /// through the page cache while the current one decodes, so the decode
    /// worker's read never waits on storage.
    #[serde(skip)]
    prefetcher: crate::prefetch::Prefetcher,
    /// Resident-frame budget for `frame_cache`, recomputed from the RAM budget
    /// (`budget::max_t1`) each status tick once a frame's size is measured.
    #[serde(skip)]
    frame_cache_cap: usize,
    /// One frame's measured `approx_bytes()`, captured on the first decode (a
    /// sequence is homogeneous). Sizes the cache budget.
    #[serde(skip)]
    frame_bytes: Option<usize>,
    /// One **proxy** frame's measured `approx_bytes()` (#94), captured on the first
    /// proxy decode. Sizes the T1 cap while proxying so hundreds of the tiny
    /// frames fit instead of ~16 full ones.
    #[serde(skip)]
    proxy_bytes: Option<usize>,
    /// Sequence frame numbers submitted to the worker but not yet returned (#57).
    /// Bounds decode-ahead concurrency and prevents re-requesting an in-flight
    /// frame; cleared on every seek so superseded decodes can't be miscounted.
    #[serde(skip)]
    inflight: std::collections::HashSet<u32>,

    /// Locked-step A/B playback (#98), now grouped as one **follower** source
    /// (#99 unification): B is a *slaved* function of A's playhead. All fields
    /// reset on each open like the A playback state. P1.4b generalizes this single
    /// `b` into a per-`SourceId` collection so the comp stack's N sources each carry
    /// their own follower state.
    #[serde(skip)]
    b: SourceState,
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
    /// The B playhead the B T2 ring was last pumped for (#166) — the `last_t2_pump`
    /// counterpart for slot B, so a full B ring on an unmoved B frame skips its
    /// want-list allocation.
    #[serde(skip)]
    last_t2_pump_b: Option<u32>,

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
    /// Whether the additive **Layers** panel is shown (#99 PR-B). Menu-toggled.
    #[serde(skip)]
    show_layers_panel: bool,
    /// The Layers panel's composite stack: a viewer-independent N-layer stack the
    /// panel edits and (PR-B.3) composites via the PR-A accumulate ping-pong,
    /// separate from the A/B compare stack so the compare modes stay untouched.
    /// Runtime-only; its layers persist via a `LayerPersist` list (PR-B.5), not
    /// this field (`LayerStack` isn't `Serialize`, and ids are re-allocated on load).
    #[serde(skip)]
    comp_stack: crate::layer::LayerStack,
    /// Monotonic allocator for the panel's `SourceId`s (#99 PR-B). Never reused, so
    /// a removed-then-added source can't alias a stale decode/texture (PR-B.2).
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
            exr_data_b: None,
            error_msg: None,
            viewer: ExrViewer::default(),
            playback: crate::playback::Playback::default(),
            frame_cache: crate::cache::FrameCache::new(),
            prefetcher: crate::prefetch::Prefetcher::default(),
            // Conservative starting budget until the first frame is measured and
            // `budget::max_t1` recomputes it from a slice of free RAM.
            frame_cache_cap: 8,
            frame_bytes: None,
            proxy_bytes: None,
            inflight: std::collections::HashSet::new(),
            b: SourceState::default(),
            epoch_signal: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            show_playback_debug: false,
            show_playback_hud: false,
            dbg_last_sample: None,
            dbg_evictions: 0,
            dbg_dropped_epoch: 0,
            run_dropped: 0,
            run_held: 0,
            decode_submit_at: None,
            last_decode_dur: None,
            dbg_last_trace: None,
            recent_files: Vec::new(),
            theme: ThemeChoice::default(),
            persisted_prefs: crate::viewer::ViewerPrefs::default(),
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
            last_t2_pump_b: None,
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
            show_layers_panel: false,
            comp_stack: crate::layer::LayerStack::new(),
            comp_next_source: 0,
            comp_sources: std::collections::HashMap::new(),
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

        // Wire the repaint handle before anything can spawn the decode worker,
        // so the worker can wake the UI when a result lands (#137).
        app.repaint_ctx = Some(cc.egui_ctx.clone());

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
        let Some(rect) = self.viewer.last_image_rect.or(self.viewer.last_canvas_rect) else {
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
            // New slot-A open: bump the generation so any in-flight open's result
            // is superseded, even though playback may have rewritten `loaded_file`
            // to a frame path in the meantime (#109).
            self.open_gen_a += 1;
            self.loading_a = true;
            // An explicit slot-A open (re)evaluates sequence mode: opening one
            // frame of a numbered sequence enables playback over its siblings; a
            // lone image leaves single-image behavior unchanged (#7).
            self.detect_sequence(&path);
        } else {
            self.b.loaded_file = Some(path.clone());
            self.b.loading = true;
        }
        self.error_msg = None;

        self.submit_job(LoadJob {
            path,
            source: if is_b {
                crate::cache::Slot::B.into()
            } else {
                crate::cache::Slot::A.into()
            },
            seq_frame: false,
            frame: 0,
            epoch: self.playback.epoch,
            // Only slot-A opens supersede by generation; B is path-keyed, so its
            // job carries no meaningful generation.
            open_gen: if is_b { 0 } else { self.open_gen_a },
            // An explicit open always decodes in full (it seeds the session and
            // the T1 RAM budget; AOVs must be present).
            beauty_only: false,
            proxy_target: None,
        });
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
                    // Slot-A first-paint proxy (#33): a fast low-res read so the
                    // image appears before the full decode lands. Skipped for
                    // slot B (a reference) and for playback frames (#7), which
                    // swap straight to full-res. `from_exr_fast_read` returns None
                    // for small / tiled / deep files anyway.
                    if job.source == crate::cache::Slot::A.into()
                        && !job.seq_frame
                        && let Some(proxy) = crate::proxy::ProxyImage::from_exr_fast_read(
                            &job.path,
                            crate::proxy::PROXY_TARGET_BLOCKS,
                        )
                    {
                        let _ = result_tx.send(LoadMsg::Proxy {
                            path: job.path.clone(),
                            proxy,
                        });
                        wake_ui();
                    }
                    // Decode mode while the playhead moves, cheapest first (#94/#56):
                    // - a **scrub proxy** (downsampled, tiny + fast) when requested,
                    //   falling back to beauty/full if the fast read isn't available;
                    // - **beauty-only** (one layer) otherwise while moving (#56 step 3);
                    // - full all-AOV for opens and the settle re-decode.
                    // #165: a proxy job checks the on-disk cache first — a hit is a
                    // raw f16 read (~zero decode); a miss decodes then write-throughs.
                    let mut store_blob: Option<(crate::proxy_cache::ProxyKey, Vec<u8>)> = None;
                    let result = match job.proxy_target {
                        Some(tb) => {
                            if let Some(cached) = proxy_cache.read(&job.path, tb) {
                                Ok(cached)
                            } else {
                                let decoded =
                                    ExrData::load_proxy_into(&job.path, tb, &mut proxy_scratch)
                                        .or_else(|_| {
                                            if job.beauty_only {
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
                                        crate::proxy_cache::ProxyKey::for_source(&job.path, tb)
                                {
                                    let blob = data.write_proxy_blob(&key);
                                    store_blob = Some((key, blob));
                                }
                                decoded
                            }
                        }
                        None if job.beauty_only => ExrData::load_beauty(&job.path),
                        None => ExrData::load(&job.path),
                    };
                    let _ = result_tx.send(LoadMsg::Loaded(Box::new(LoadResult {
                        path: job.path,
                        source: job.source,
                        seq_frame: job.seq_frame,
                        frame: job.frame,
                        epoch: job.epoch,
                        open_gen: job.open_gen,
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

    /// Evaluate sequence mode for a freshly opened slot-A `path`: enter playback
    /// over the detected siblings (placing the playhead on the opened frame), or
    /// clear playback for a lone image. Either way the frame cache is dropped —
    /// it is keyed by frame number, which a different sequence reuses.
    fn detect_sequence(&mut self, path: &std::path::Path) {
        self.frame_cache.clear();
        self.frame_bytes = None;
        // Reset the debug-overlay counters so each opened sequence's soak run
        // starts clean (#100).
        self.dbg_evictions = 0;
        self.dbg_dropped_epoch = 0;
        // A different sequence reuses frame numbers, so drop the T2 GPU ring too
        // (and reset the on-screen frame; the first show re-sets it).
        self.viewer.clear_t2();
        self.viewer.set_t2_frame(None);
        // Drop any prior sequence's in-flight frames (a different sequence reuses
        // frame numbers); `enter`/`clear` bump the epoch so their results are
        // dropped. `loading_a` is left to `open_file`, which owns this open.
        self.inflight.clear();
        // A new sequence resets the precache fill latch (#56, step 4).
        self.precache_filled = false;
        // A new sequence (or a lone image) invalidates the watch baseline; the
        // next poll re-baselines against the freshly-opened group.
        self.watch_sigs.clear();
        self.last_watch_poll = None;
        // A new A session drops any locked-step B (a lone reference is meaningless
        // on its own, #98); the B result-apply path re-detects if a B is reopened.
        self.clear_b_sequence();
        match crate::sequence::detect_from_file(path) {
            Some(seq) => {
                let start = seq.number_of(path).unwrap_or(seq.range.0);
                self.playback.enter(seq, start);
            }
            None => self.playback.clear(),
        }
    }

    /// Drop all locked-step B state (#98): the B sequence, its playhead, awaited
    /// and in-flight frames, and every `Slot::B` ring entry. After this, B is a
    /// lone static reference again (or absent).
    fn clear_b_sequence(&mut self) {
        self.b.sequence = None;
        self.b.current_frame = 0;
        self.b.offset = 0;
        self.b.pending = None;
        self.b.loading = false;
        self.b.inflight.clear();
        self.frame_cache.clear_slot(crate::cache::Slot::B);
        // Drop the B T2 GPU ring too (#166): its frame keys belong to the sequence
        // being cleared; the next B show re-sets the on-screen frame.
        self.viewer.clear_t2_b();
        self.viewer.set_t2_frame_b(None);
    }

    /// Arm locked-step B (#98) if the just-opened B file is part of an image
    /// sequence: store `b.sequence`, seed `b.current_frame` + the `Slot::B` ring
    /// with the opened frame, and re-align to A on the next playhead move. A lone
    /// image leaves `sequence_b = None`, so B stays the static reference it was.
    fn detect_sequence_b(&mut self, path: &std::path::Path) {
        self.clear_b_sequence();
        let Some(seq) = crate::sequence::detect_from_file(path) else {
            return;
        };
        let bf = seq.number_of(path).unwrap_or(seq.range.0);
        self.b.current_frame = bf;
        // The opened B frame is on screen; mark it for the B ring so its pump
        // protects it from eviction (#166) even before the first locked-step swap.
        self.viewer.set_t2_frame_b(Some(bf));
        // Seed the ring with the opened B frame (mirror the A open, #56) so it's an
        // instant hit and the first locked-step advance has a cached neighbour.
        if let Some(arc) = self.exr_data_b.clone() {
            self.frame_cache.insert(crate::cache::Slot::B, bf, arc);
        }
        self.b.sequence = Some(seq);
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
            self.viewer.set_t2_frame(Some(frame)); // bind this frame's T2 texture
            self.swap_image_arc(data, false);
            if needs_full {
                self.playback.pending = Some(frame);
            } else {
                // Cache hit: show immediately, no decode round-trip.
                self.playback.pending = None;
                self.playback.note_shown(std::time::Instant::now());
            }
        } else {
            // Miss: mark the playhead as awaited; `pump_decode` submits it (the
            // want-list puts the playhead first, so it beats any prefetch).
            self.playback.pending = Some(frame);
        }
        // Locked-step B follows A here — this is the single chokepoint every A
        // playhead move funnels through (advance / step / scrub / drop-frames /
        // render-watch), so B never falls out of step (#98). No-op without a B
        // sequence. Ordered after A's own request so `pump_decode` sees A's want
        // first (A-playhead priority).
        self.sync_b_to_a();
        self.pump_decode();
    }

    /// Locked-step A/B (#98): drive B's playhead from A's. B is a *slaved*,
    /// position-aligned function of A (no clock of its own), so whenever A's
    /// playhead moves, recompute B's frame and request it. No-op when B isn't a
    /// sequence (a lone-image B just holds, as before).
    fn sync_b_to_a(&mut self) {
        let Some(range) = self.b.sequence.as_ref().map(|s| s.range) else {
            return;
        };
        // B follows A by position, plus the user's A/B offset (#166), clamped to
        // B's range so the nudge can never point outside the compared sequence.
        let b_frame = crate::playback::map_b_frame_offset(
            self.playback.current_frame,
            self.playback.in_point,
            range,
            self.b.offset,
        );
        self.b.current_frame = b_frame;
        self.request_b_frame(b_frame);
    }

    /// The slot-B counterpart of [`Self::request_sequence_frame`] (#98): show B's
    /// frame from the T1 ring if resident (upgrading a beauty copy to full on
    /// settle), else mark it awaited for the pump. `b.loaded_file` is *not*
    /// rewritten — B seq frames supersede by the shared epoch (like A), so the
    /// B-open path keeps its stable path-key.
    fn request_b_frame(&mut self, frame: u32) {
        // B is slaved to A, so it can move to a new frame while a previous B
        // decode is still awaited — the old `b.loading` is now stale, and its
        // frame (if it lands) won't match `b.current_frame`, so nothing else would
        // clear it. Reset it on every B move: `submit_seq` re-sets it if this frame
        // is submitted, and `apply_load_result` clears it when B's current frame
        // lands. Without this, mapping B onto a hole (or advancing past an in-flight
        // B frame) latches `b.loading` and gates `pump_decode` forever (freeze).
        self.b.loading = false;
        let is_hole = self
            .b.sequence
            .as_ref()
            .is_none_or(|s| s.path_for(frame).is_none());
        if is_hole {
            // Hole in B: hold the last real B frame; A drives on.
            self.b.pending = None;
            self.pump_decode();
            return;
        }
        if let Some(data) = self.frame_cache.get(crate::cache::Slot::B, frame) {
            let needs_full = !self.playback.is_playing()
                && !self.scrub_active
                && (data.beauty_only || data.proxy);
            self.swap_b_frame(data);
            self.b.pending = if needs_full { Some(frame) } else { None };
        } else {
            self.b.pending = Some(frame);
        }
        self.pump_decode();
    }

    /// The total prefetch window around the playhead (ahead + the #169
    /// read-behind reservation, split in [`Self::next_want_slot`]) — bounded by
    /// the T1 ring (`min(configured, max_t1 − 1)`, tying #57 back-pressure to
    /// the #56 budget) and a hard cap so a huge sequence can't queue the world.
    fn prefetch_depth(&self) -> usize {
        const MAX_PREFETCH: usize = 16;
        self.frame_cache_cap.saturating_sub(1).min(MAX_PREFETCH)
    }

    /// The protected on-screen playheads for T1 eviction (#99 unification): the
    /// primary (Slot A) always, plus the locked-step B playhead when a B sequence
    /// is active. As the app's `_b` state folds into a per-`SourceId` map (Phase
    /// 1.4) this grows to the full active-source set.
    fn cache_playheads(&self) -> Vec<(crate::layer::SourceId, u32)> {
        let mut playheads = vec![(
            crate::cache::Slot::A.into(),
            self.playback.current_frame,
        )];
        if self.b.sequence.is_some() {
            playheads.push((crate::cache::Slot::B.into(), self.b.current_frame));
        }
        playheads
    }

    /// The read-behind reservation (#169) T1 eviction must protect — mirrors the
    /// decode pump's slot-A depth exactly (`pump_decode`'s playing depth,
    /// including the locked-step halving), so the evictor reserves precisely the
    /// window the scheduler maintains: a larger value would protect frames the
    /// pump can't fit and the two would churn. Zero while not playing —
    /// paused/scrub eviction is bidirectional-distance and needs no carve-out.
    fn read_behind_depth(&self) -> usize {
        if !self.playback.is_playing() {
            return 0;
        }
        let full = self.prefetch_depth();
        let depth = if self.b.sequence.is_some() {
            full / 2
        } else {
            full
        };
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
        self.decode_beauty_only_at(frame, self.playback.current_frame)
    }

    /// Locked-step B (#98) counterpart: same policy, but the settled-precache
    /// "keep the playhead full" exception measures against B's own current frame
    /// (B's numbers live in B's range, not A's), so a paused B frame is decoded
    /// full for correct compare-mode sampling/AOV.
    fn decode_beauty_only_b(&self, frame: u32) -> bool {
        self.decode_beauty_only_at(frame, self.b.current_frame)
    }

    /// The shared "decode something cheaper while the playhead moves" condition for
    /// beauty (#56) and proxy (#94): the viewer is on the beauty layer (layer 0 —
    /// the only layer both cheaper decodes carry) AND the frame is playing, being
    /// dragged, or a precache prefetch (not the settled playhead). The respective
    /// `beauty_preview` / `proxy_enabled` kill-switches decide *which* cheaper
    /// decode is used.
    fn wants_cheap_decode_at(&self, frame: u32, playhead: u32) -> bool {
        if self.viewer.active_layer != 0 {
            return false;
        }
        if self.playback.is_playing() || self.scrub_active {
            return true;
        }
        self.precache && frame != playhead
    }

    fn decode_beauty_only_at(&self, frame: u32, playhead: u32) -> bool {
        self.beauty_preview && self.wants_cheap_decode_at(frame, playhead)
    }

    /// The scrub-proxy `target_blocks` to decode `frame` at (#94), or `None` for a
    /// full/beauty decode. Same cheap-while-moving gate as beauty, behind the
    /// `proxy_enabled` kill-switch; the worker falls back to beauty/full if the
    /// fast proxy read isn't available for the file.
    fn decode_proxy_target_at(&self, frame: u32, playhead: u32) -> Option<usize> {
        (self.proxy_enabled && self.wants_cheap_decode_at(frame, playhead))
            .then_some(self.proxy_size)
    }

    fn decode_proxy_target(&self, frame: u32) -> Option<usize> {
        self.decode_proxy_target_at(frame, self.playback.current_frame)
    }

    /// Locked-step B (#98) counterpart of [`Self::decode_proxy_target`].
    fn decode_proxy_target_b(&self, frame: u32) -> Option<usize> {
        self.decode_proxy_target_at(frame, self.b.current_frame)
    }

    /// Decode-ahead pump (#57): with at most one sequence decode outstanding,
    /// submit the highest-priority frame the scheduler wants — the awaited
    /// playhead first, then prefetch ahead in the play direction. Called after
    /// the playhead moves, after each result lands (the worker just freed up), and
    /// each playing tick. A no-op while a decode is in flight or a non-sequence
    /// load is busy, which is what keeps it to one outstanding job.
    fn pump_decode(&mut self) {
        // One shared worker, one decode at a time across BOTH slots (#98): block
        // while either slot has an outstanding job or awaited playhead.
        if !self.playback.is_active()
            || !self.inflight.is_empty()
            || !self.b.inflight.is_empty()
            || self.loading_a
            || self.b.loading
        {
            return;
        }
        // Depth priority:
        // - **Playing** → the sliding prefetch window ahead of the playhead, even
        //   with precache on. Whole-budget depth here is actively harmful when the
        //   range exceeds the budget: `next_want` loop-wraps to the far side and
        //   the single worker burns its bandwidth decoding frames *behind* the
        //   playhead (evict-churn) instead of the ones just ahead, so play goes
        //   decode-bound and stalls.
        // - **Idle + precache, not yet filled** (#56, step 4) → fill the whole
        //   budget so the range goes as resident as it fits, for instant scrubbing.
        //   Gated on `!precache_filled`: once latched (cache full / nothing more
        //   fits), a whole-budget window keeps asking for far frames that
        //   `evict_to` immediately drops — and because `apply_load_result` re-pumps
        //   after every result, that decode→evict churn runs forever while idle,
        //   independent of the `tick_precache` latch. A `pending` playhead still
        //   gets through at depth 0 (its P1 slot in `next_want`).
        // - **Idle otherwise** → just the playhead.
        let full_depth = if self.playback.is_playing() {
            self.prefetch_depth()
        } else if self.precache && !self.precache_filled {
            self.frame_cache_cap.saturating_sub(1)
        } else {
            0
        };
        // With B also playing, T1 holds two resident windows, so split the
        // decode-ahead per slot (#98) — otherwise A's window alone would fill the
        // budget and starve B.
        let b_active = self.b.sequence.is_some();
        let depth = if b_active { full_depth / 2 } else { full_depth };

        // Priority across the two slots, one job submitted per pump:
        // P0 awaited playheads (A before B), then P1 prefetch (A before B).
        // depth 0 asks `next_want` for the playhead only (its P1 slot).
        for (slot, d) in [
            (crate::cache::Slot::A, 0),
            (crate::cache::Slot::B, 0),
            (crate::cache::Slot::A, depth),
            (crate::cache::Slot::B, depth),
        ] {
            if slot == crate::cache::Slot::B && !b_active {
                continue;
            }
            if let Some(w) = self.next_want_slot(slot, d) {
                self.submit_seq(slot, w);
                // Overlap I/O with decode (#164): while the worker decodes `w`,
                // a background thread pulls the *next* wanted frames' files
                // through the page cache, so the worker's next read is a
                // memory-speed pointer walk (the decode maps the file) instead
                // of a storage stall. Only while a prefetch window is active —
                // warming on a bare playhead seek would race the user's intent.
                if depth > 0 {
                    self.warm_ahead(slot, w, depth);
                }
                return;
            }
        }
    }

    /// Queue the next few non-resident frames after `submitted` for background
    /// file warming (#164) — the same order `next_want_slot` will request them,
    /// so the warmer stays exactly ahead of the decoder. Best-effort and
    /// epoch-agnostic: a superseded warm wastes bandwidth, never correctness.
    fn warm_ahead(&mut self, slot: crate::cache::Slot, submitted: u32, depth: usize) {
        /// Files to keep warm ahead of the decoder. One hides the read of the
        /// very next frame; a second gives slack for a fast decode (proxies)
        /// outpacing a slow warm. More would only evict-race the page cache.
        const WARM_AHEAD: usize = 2;
        let behind = crate::scheduler::read_behind(depth);
        let ahead = depth - behind;
        let mut resident: std::collections::HashSet<u32> =
            self.frame_cache.resident_frames(slot).collect();
        resident.insert(submitted);
        let wants = match slot {
            crate::cache::Slot::A => crate::scheduler::want_list(
                self.playback.current_frame,
                self.playback.in_point,
                self.playback.out_point,
                self.playback.direction,
                self.playback.loop_mode,
                &resident,
                ahead,
                behind,
            ),
            crate::cache::Slot::B => {
                let Some(seq_b) = self.b.sequence.as_ref() else {
                    return;
                };
                crate::scheduler::want_list(
                    self.b.current_frame,
                    seq_b.range.0,
                    seq_b.range.1,
                    self.playback.direction,
                    self.playback.loop_mode,
                    &resident,
                    ahead,
                    behind,
                )
            }
        };
        for w in wants.into_iter().take(WARM_AHEAD) {
            let path = match slot {
                crate::cache::Slot::A => self.playback.frame_path(w).map(Path::to_path_buf),
                crate::cache::Slot::B => self
                    .b.sequence
                    .as_ref()
                    .and_then(|s| s.path_for(w))
                    .map(Path::to_path_buf),
            };
            if let Some(path) = path {
                // Skip the read-ahead warm when the on-disk proxy cache (#165)
                // already holds this frame at the size it would decode: the worker
                // will hit the cache and never open the source, so pulling it
                // through the page cache is wasted bandwidth — exactly the
                // networked-storage cost the cache exists to remove. `contains`
                // self-gates to `false` when the disk cache is off.
                let px = match slot {
                    crate::cache::Slot::A => self.decode_proxy_target(w),
                    crate::cache::Slot::B => self.decode_proxy_target_b(w),
                };
                if px.is_some_and(|px| self.proxy_cache.contains(&path, px)) {
                    continue;
                }
                self.prefetcher.warm(path);
            }
        }
    }

    /// The next frame to fetch for one slot at the given decode-ahead `depth`, or
    /// `None` if that slot's window is fully resident (#98). A frame we are
    /// explicitly `pending` on counts as **not** resident even if a *beauty-only*
    /// copy is cached, so the full all-AOV upgrade on settle still gets submitted
    /// (INV-SAMPLE, #7; `contains` is fidelity-blind). B runs the same pure
    /// scheduler in B's own frame space (its range as in/out, its playhead),
    /// with A's direction / loop.
    fn next_want_slot(&self, slot: crate::cache::Slot, depth: usize) -> Option<u32> {
        // Split the window RV-style (#169): ~25% reserved behind the playhead,
        // the rest ahead — so forward window + behind reservation together never
        // over-ask the ring (the #57 back-pressure contract holds for the sum).
        let behind = crate::scheduler::read_behind(depth);
        let ahead = depth - behind;
        match slot {
            crate::cache::Slot::A => {
                let pending = self.playback.pending;
                crate::scheduler::next_want(
                    self.playback.current_frame,
                    self.playback.in_point,
                    self.playback.out_point,
                    self.playback.direction,
                    self.playback.loop_mode,
                    ahead,
                    behind,
                    |f| self.frame_cache.contains(crate::cache::Slot::A, f) && Some(f) != pending,
                    |f| self.playback.frame_path(f).is_some(),
                )
            }
            crate::cache::Slot::B => {
                let seq_b = self.b.sequence.as_ref()?;
                let pending = self.b.pending;
                crate::scheduler::next_want(
                    self.b.current_frame,
                    seq_b.range.0,
                    seq_b.range.1,
                    self.playback.direction,
                    self.playback.loop_mode,
                    ahead,
                    behind,
                    |f| self.frame_cache.contains(crate::cache::Slot::B, f) && Some(f) != pending,
                    |f| seq_b.path_for(f).is_some(),
                )
            }
        }
    }

    /// Submit one sequence decode for `slot` at frame `w` (#98). Tags the job with
    /// its cache slot via `source` (`slot.into()`), marks it in-flight, and drives
    /// that slot's "loading" state when it's the awaited playhead.
    fn submit_seq(&mut self, slot: crate::cache::Slot, w: u32) {
        let path = match slot {
            crate::cache::Slot::A => self.playback.frame_path(w).map(Path::to_path_buf),
            crate::cache::Slot::B => self
                .b.sequence
                .as_ref()
                .and_then(|s| s.path_for(w))
                .map(Path::to_path_buf),
        }
        .expect("next_want only returns decodable frames");
        // The awaited playhead drives the "loading" state; prefetch is silent.
        match slot {
            crate::cache::Slot::A => {
                self.inflight.insert(w);
                if Some(w) == self.playback.pending {
                    self.loading_a = true;
                }
            }
            crate::cache::Slot::B => {
                self.b.inflight.insert(w);
                if Some(w) == self.b.pending {
                    self.b.loading = true;
                }
            }
        }
        let beauty_only = match slot {
            crate::cache::Slot::A => self.decode_beauty_only(w),
            crate::cache::Slot::B => self.decode_beauty_only_b(w),
        };
        // Scrub proxy takes precedence over beauty while moving (#94); `beauty_only`
        // stays as the worker's fallback if the fast proxy read isn't available.
        let proxy_target = match slot {
            crate::cache::Slot::A => self.decode_proxy_target(w),
            crate::cache::Slot::B => self.decode_proxy_target_b(w),
        };
        self.submit_job(LoadJob {
            path,
            source: slot.into(),
            seq_frame: true,
            frame: w,
            epoch: self.playback.epoch,
            // Seq-frames supersede by epoch, not open generation.
            open_gen: 0,
            beauty_only,
            proxy_target,
        });
        // Anchor the stall watchdog: one decode is now outstanding.
        self.decode_submit_at = Some(std::time::Instant::now());
    }

    /// Pre-upload T2 GPU textures (#56) for the on-screen frame and the next few
    /// T1-cached frames ahead of the playhead, within the VRAM budget. Builds at
    /// most a couple per call to amortize the upload across UI frames; only
    /// touches frames already resident in T1 (never decodes). UI-thread only.
    fn pump_t2(&mut self) {
        if !self.playback.is_active() || self.viewer.t2_cap() == 0 {
            return;
        }
        // Nothing to do when the ring is full and the playhead hasn't moved since
        // the last pump: every slot is already built for this frame. Skips the
        // want-list allocation in the paused / settled case (#142 U4). A playhead
        // move, or a shrunk/evicted ring, drops one of these conditions and pumps.
        if self.viewer.t2_len() >= self.viewer.t2_cap()
            && self.last_t2_pump == Some(self.playback.current_frame)
        {
            return;
        }
        let Some(gpu) = self.gpu_resources.as_ref() else {
            return;
        };
        let depth = self.viewer.t2_cap().saturating_sub(1);
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
            if let Some(arc) = self.frame_cache.peek(crate::cache::Slot::A, w)
                && self.viewer.prebuild_t2(gpu, &arc, w)
            {
                built += 1;
            }
        }
    }

    /// Pre-upload **slot-B** T2 GPU textures (#166, #98 Phase 2) ahead of B's
    /// playhead in locked-step compare, so a compared sequence binds an
    /// already-uploaded texture each frame instead of re-packing + re-uploading on
    /// the UI thread — the per-frame B upload that made B stutter while A stayed
    /// smooth. Mirrors [`Self::pump_t2`] in B's own frame space; no-op unless B is
    /// a sequence with a non-zero B ring cap. Only touches frames already resident
    /// in the `Slot::B` T1 cache (never decodes). UI-thread only.
    fn pump_t2_b(&mut self) {
        let Some(range) = self.b.sequence.as_ref().map(|s| s.range) else {
            return;
        };
        if !self.playback.is_active() || self.viewer.t2_cap_b() == 0 {
            return;
        }
        // Full B ring on an unmoved B playhead: every slot is built for this frame.
        if self.viewer.t2_len_b() >= self.viewer.t2_cap_b()
            && self.last_t2_pump_b == Some(self.b.current_frame)
        {
            return;
        }
        let Some(gpu) = self.gpu_resources.as_ref() else {
            return;
        };
        let depth = self.viewer.t2_cap_b().saturating_sub(1);
        // B's want-list in B's own frame space (its range as in/out, its playhead),
        // sharing A's direction/loop — the same shape `next_want_slot`/`warm_ahead`
        // use for B. Forward-only (no read-behind), like the A ring.
        let wants = crate::scheduler::want_list(
            self.b.current_frame,
            range.0,
            range.1,
            self.playback.direction,
            self.playback.loop_mode,
            &std::collections::HashSet::new(),
            depth,
            0,
        );
        self.last_t2_pump_b = Some(self.b.current_frame);
        const PUMP_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);
        let start = std::time::Instant::now();
        let mut built = 0;
        for w in std::iter::once(self.b.current_frame).chain(wants) {
            if built > 0 && start.elapsed() >= PUMP_BUDGET {
                break;
            }
            if let Some(arc) = self.frame_cache.peek(crate::cache::Slot::B, w)
                && self.viewer.prebuild_t2_b(gpu, &arc, w)
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
        // Publish the new epoch so the worker skips the jobs this just superseded
        // (a scrub backlog) instead of decoding each one only to drop it.
        self.epoch_signal
            .store(self.playback.epoch, std::sync::atomic::Ordering::Relaxed);
        self.inflight.clear();
        self.loading_a = false;
        self.playback.pending = None;
        // Locked-step B (#98) is superseded by the same epoch; clear its seq state
        // too so a dropped B decode can't leave `b.loading` latched (gating the
        // pump). `sync_b_to_a` re-requests B's new frame right after.
        self.b.inflight.clear();
        self.b.pending = None;
        self.b.loading = false;
        // The playhead/range moved — the precache window shifts, so let it refill.
        self.precache_filled = false;
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
        // Clamped at a range boundary: a held arrow key would otherwise re-seek
        // the same frame every key-repeat, superseding its own decode (#139).
        // Same narrowing as `playback_scrub_to`: an errored frame (not pending,
        // not resident) still falls through so the step retries it.
        if next == self.playback.current_frame
            && (self.playback.pending == Some(next)
                || self.frame_cache.contains(crate::cache::Slot::A, next))
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
        self.playback.state = crate::playback::PlayState::Paused;
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
                || self.frame_cache.contains(crate::cache::Slot::A, next))
        {
            return;
        }
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
            self.settle_to_full();
        } else {
            // Fresh play run → reset the HUD's dropped/held counters (#172).
            self.run_dropped = 0;
            self.run_held = 0;
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
    fn settle_to_full(&mut self) {
        if !self.playback.is_active() {
            return;
        }
        let a_frame = self.playback.current_frame;
        let a_full = self
            .frame_cache
            .peek(crate::cache::Slot::A, a_frame)
            .is_some_and(|d| !d.beauty_only && !d.proxy);
        // Locked-step B (#98): its settled frame must be full-resident too so
        // compare-mode sampling/AOV on B is correct. A hole holds the previous B
        // frame — nothing to fetch, so treat it as settled.
        let b_full = match self.b.sequence.as_ref() {
            None => true,
            Some(s) => {
                let bf = self.b.current_frame;
                s.path_for(bf).is_none()
                    || self
                        .frame_cache
                        .peek(crate::cache::Slot::B, bf)
                        .is_some_and(|d| !d.beauty_only && !d.proxy)
            }
        };
        if a_full {
            // A is full-res and displayed; no A decode will land to trigger it,
            // so refresh the contact sheet now (frozen during play, #144).
            self.viewer.invalidate_active_thumbnails();
        }
        if a_full && b_full {
            return;
        }
        // Supersede in-flight beauty prefetch, then re-request each not-yet-full
        // playhead at full fidelity (`request_*` upgrades a beauty ring hit; #7).
        self.invalidate_inflight();
        if a_full {
            // A needs no re-decode; `request_sequence_frame` (which would sync B)
            // is skipped, so upgrade/re-sync B directly.
            self.sync_b_to_a();
        } else {
            // Re-decodes A at full fidelity and syncs B via the chokepoint.
            self.request_sequence_frame(a_frame);
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

    /// Per-frame playback clock. While playing and no decode is in flight, advance
    /// to the next frame once its absolute deadline (`anchor + n·period`) passes —
    /// drift-free pacing. Decode-bound playback (stutter) naturally drops the
    /// effective fps: the next request waits for the previous frame to land.
    /// Drive eager precache (#56, step 4) while idle. `pump_decode` already fills
    /// the whole budget when `precache` is on (playing or not), and chains itself
    /// from `apply_load_result` as each frame lands; this kicks that chain when
    /// the app is otherwise idle (paused) and keeps the frame loop alive until the
    /// resident span covers the budget. A no-op once the range is fully cached.
    fn tick_precache(&mut self, ctx: &egui::Context) {
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
        if self.inflight.is_empty() || self.frame_cache.len() >= self.frame_cache_cap {
            self.precache_filled = true;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

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
        // Wake at the next absolute deadline (`anchor + n·period`), not a full
        // period from now: `request_repaint_after(period)` added the wake-up
        // slop (timer/vsync quantization) to every frame, capping effective
        // fps below target no matter how fast decode was (#138). While a
        // decode holds the stutter clock, the worker wake (#137) and the
        // in-flight poll drive repaints instead — schedule the period only as
        // a lazy fallback rather than spinning on the overdue deadline.
        let decode_bound = matches!(self.playback.pacing, Pacing::Stutter)
            && (self.playback.pending.is_some() || self.loading_a);
        let wait = if decode_bound {
            period
        } else {
            let now = std::time::Instant::now();
            self.playback.anchor.map_or(period, |anchor| {
                (anchor + period * self.playback.frames_since_anchor).saturating_duration_since(now)
            })
        };
        ctx.request_repaint_after(wait);
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
            self.playback.state = PlayState::Paused;
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
            self.playback.state = PlayState::Paused;
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

        // T1: recomputed each tick; shrinks under other memory pressure.
        // While scrub proxies are actually being produced (#94) the resident
        // frames are tiny downsampled proxies, so size the cap off proxy bytes —
        // hundreds fit instead of the ~16 that full 178 MB frames allow. Gated on
        // the SAME condition proxies decode under (`proxy_enabled` + beauty layer):
        // on a non-beauty AOV the ring fills with *full* frames, so a proxy-sized
        // cap would over-count and risk OOM. Falls back to full-frame bytes
        // otherwise (or when proxy_bytes hasn't been measured yet).
        let proxying =
            self.proxy_enabled && self.playback.is_active() && self.viewer.active_layer == 0;
        let sizing_bytes = if proxying {
            self.proxy_bytes.or(self.frame_bytes)
        } else {
            self.frame_bytes
        };
        if let Some(bytes) = sizing_bytes {
            let cache_bytes = self.frame_cache.len() as u64 * bytes as u64;
            let auto = crate::budget::t1_capacity(&sample, bytes, cache_bytes);
            // A user-assigned RAM budget (if any) caps the auto figure —
            // never raises it — then floor at 2 so playback still runs.
            self.frame_cache_cap =
                crate::budget::apply_user_ram_cap(auto, self.ram_budget_bytes(), bytes).max(2);
            // Enforce a shrink now, not on the next decode: eviction otherwise
            // only runs on insert, so with precache latched (nothing in flight)
            // external memory pressure lowered the cap while the ring kept every
            // frame indefinitely — the memory contract's live-pressure
            // degradation never fired (#146).
            if self.frame_cache.len() > self.frame_cache_cap {
                let loop_wrap = (self.playback.loop_mode == crate::playback::LoopMode::Loop)
                    .then_some((self.playback.in_point, self.playback.out_point));
                let playheads = self.cache_playheads();
                self.dbg_evictions = self.dbg_evictions.saturating_add(self.frame_cache.evict_to(
                    self.frame_cache_cap,
                    &playheads,
                    crate::cache::Slot::A.into(),
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
        let b_active = t2_on && self.b.sequence.is_some();
        let avail = crate::budget::vram_available(&sample);
        // Split the pool in *bytes* (not frame counts) so A and B each derive
        // their own count from their own resolution when the two differ.
        let a_avail = if b_active { avail / 2 } else { avail };
        let a_dims = t2_on
            .then(|| {
                self.exr_data
                    .as_ref()
                    .and_then(|d| d.logical_size(self.viewer.active_layer))
            })
            .flatten();
        self.viewer.set_t2_cap(cap_from(a_avail, a_dims));

        // B ring: same half-budget, sized from B's own dims at the B layer (active
        // layer clamped to B's layer count). Disabled unless B is a live sequence.
        let b_dims = b_active
            .then(|| {
                self.exr_data_b.as_ref().and_then(|d| {
                    let layer_b = self
                        .viewer
                        .active_layer
                        .min(d.logical_layers.len().saturating_sub(1));
                    d.logical_size(layer_b)
                })
            })
            .flatten();
        let t2_cap_b = if b_active {
            cap_from(avail / 2, b_dims)
        } else {
            0
        };
        self.viewer.set_t2_cap_b(t2_cap_b);
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
        use crate::cache::Slot;

        // 1. A re-rendered or removed frame's cached pixels are stale — drop T1+T2.
        for &f in diff.changed.iter().chain(&diff.removed) {
            self.frame_cache.remove(Slot::A, f);
            self.viewer.evict_t2_frame(f);
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
        // The A/B routing below still forks on slot; derive it from the result's
        // source (B → the follower slot) until the `_b` state folds to a per-source
        // map (#99 P1.4).
        let is_b = res.source == crate::cache::Slot::B.into();
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
            // Route by cache slot (#98): `is_b` is the slot for a seq frame.
            let slot = if is_b {
                crate::cache::Slot::B
            } else {
                crate::cache::Slot::A
            };
            match slot {
                crate::cache::Slot::A => self.inflight.remove(&res.frame),
                crate::cache::Slot::B => self.b.inflight.remove(&res.frame),
            };
            // The worker delivered a matching result — record turnaround so the
            // stall watchdog can scale its timeout off real decode cost.
            if let Some(t) = self.decode_submit_at {
                self.last_decode_dur = Some(std::time::Instant::now().duration_since(t));
            }
            match res.result {
                Ok(data) => {
                    let arc = std::sync::Arc::new(data);
                    // Measure one **Slot::A** frame to size the shared cache budget
                    // (homogeneous seq): B may be a different resolution (#98), so
                    // sizing off a B frame would mis-size the ring. A full A frame
                    // seeds `frame_bytes`; a proxy A frame seeds `proxy_bytes` (#94).
                    if slot == crate::cache::Slot::A {
                        if arc.proxy {
                            self.proxy_bytes.get_or_insert_with(|| arc.approx_bytes());
                        } else if !arc.beauty_only {
                            self.frame_bytes.get_or_insert_with(|| arc.approx_bytes());
                        }
                    }
                    self.frame_cache.insert(slot, res.frame, arc.clone());
                    // A full-res frame landing (the settle upgrade, #94/#56)
                    // replaces a proxy/beauty frame, so its pre-built T2 GPU
                    // texture is now stale — evict it or the viewport keeps binding
                    // the blurry proxy texture ("stuck in proxy"). Slot::A only (the
                    // T2 ring is A); fires only for full frames (settle), not the
                    // proxy/beauty frames decoded while moving.
                    if slot == crate::cache::Slot::A && !arc.beauty_only {
                        self.viewer.evict_t2_frame(res.frame);
                    }
                    // In Loop mode eviction distance follows the play direction
                    // around the loop, so prefetch wrapped past the out point
                    // isn't classified "behind" and evicted on arrival (#140).
                    let loop_wrap = (self.playback.loop_mode == crate::playback::LoopMode::Loop)
                        .then_some((self.playback.in_point, self.playback.out_point));
                    // Protect B's on-screen frame too when it's locked-step (#98).
                    let playheads = self.cache_playheads();
                    self.dbg_evictions =
                        self.dbg_evictions.saturating_add(self.frame_cache.evict_to(
                            self.frame_cache_cap,
                            &playheads,
                            crate::cache::Slot::A.into(),
                            self.playback.direction,
                            self.playback.is_playing(),
                            loop_wrap,
                            self.read_behind_depth(),
                        ) as u64);
                    // Show it only if it's the frame that slot's playhead awaits;
                    // a prefetched frame ahead of the playhead is just cached.
                    match slot {
                        crate::cache::Slot::A if res.frame == self.playback.current_frame => {
                            self.loading_a = false;
                            self.playback.pending = None;
                            self.viewer.set_t2_frame(Some(res.frame));
                            self.swap_image_arc(arc, false);
                            self.playback.note_shown(std::time::Instant::now());
                        }
                        crate::cache::Slot::B if res.frame == self.b.current_frame => {
                            self.b.loading = false;
                            self.b.pending = None;
                            self.swap_b_frame(arc);
                        }
                        _ => {}
                    }
                    self.error_msg = None;
                }
                Err(e) => {
                    // Clear the awaited state for whichever slot's playhead errored.
                    match slot {
                        crate::cache::Slot::A if res.frame == self.playback.current_frame => {
                            self.loading_a = false;
                            self.playback.pending = None;
                            self.error_msg = Some(e);
                        }
                        crate::cache::Slot::B if res.frame == self.b.current_frame => {
                            self.b.loading = false;
                            self.b.pending = None;
                            self.error_msg = Some(e);
                        }
                        _ => {}
                    }
                }
            }
            // The worker just freed up — submit the next wanted frame.
            self.pump_decode();
            return;
        }

        // Supersession for an explicit open. Slot B is path-keyed (its
        // `b.loaded_file` is only set by an explicit B open / unload, never by
        // playback). Slot A is **generation**-keyed (#109): `loaded_file` is
        // rewritten to the current frame's path during playback, so a path check
        // could drop a still-current open's result — and dropping it here returns
        // before clearing `loading_a`, permanently gating `pump_decode`. The
        // generation is bumped only by a later open or an unload.
        let superseded = if is_b {
            self.b.loaded_file.as_ref() != Some(&res.path)
        } else {
            res.open_gen != self.open_gen_a
        };
        if superseded {
            return;
        }

        if is_b {
            self.b.loading = false;
        } else {
            self.loading_a = false;
        }

        match res.result {
            Ok(data) => {
                if is_b {
                    // B is a reference slot, not a new session: swap the pixel
                    // source while preserving the viewer's session state.
                    self.swap_image_data(data, true);
                    // Locked-step A/B (#98): if this B open is part of a sequence,
                    // arm B so it advances with A. A lone image just holds.
                    self.detect_sequence_b(&res.path);
                } else {
                    // An explicit open of a new A starts a fresh session: drop the
                    // reference (meaningless on its own) in both paths below.
                    self.exr_data_b = None; // Reset B when A changes
                    self.b.loaded_file = None;
                    self.clear_b_sequence(); // and any locked-step B (#98)
                    self.b.loading = false; // A discards any in-flight B load
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
                if !is_b {
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
        // Same Arc as already displayed (scrub-return, settle onto the shown
        // frame): the pixels are identical, so skip the invalidations — on a T2
        // miss they'd force a full re-pack + re-upload of the same data (#146).
        let same = if is_b {
            self.exr_data_b
                .as_ref()
                .is_some_and(|cur| std::sync::Arc::ptr_eq(cur, &data))
        } else {
            self.exr_data
                .as_ref()
                .is_some_and(|cur| std::sync::Arc::ptr_eq(cur, &data))
        };
        if same {
            self.error_msg = None;
            return;
        }
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

    /// Swap slot B to a new locked-step playback frame (#98) — the per-frame B
    /// counterpart of `swap_image_arc(_, is_b=true)`, without the reference-*open*
    /// semantics. Rebuilds only B's viewport bind groups (so the compare draws
    /// show the new B frame); B thumbnails stay frozen while the transport is busy
    /// and refresh on settle (mirror the A #144/#151 split). No histogram reset —
    /// it's suppressed during play anyway (#141). Skips a same-`Arc` swap so a
    /// settle onto the shown frame doesn't force a needless B re-pack.
    fn swap_b_frame(&mut self, data: std::sync::Arc<ExrData>) {
        if self
            .exr_data_b
            .as_ref()
            .is_some_and(|cur| std::sync::Arc::ptr_eq(cur, &data))
        {
            return;
        }
        self.exr_data_b = Some(data);
        // The B ring binds this frame's pre-built texture (#166); tell it which B
        // frame is on screen (mirrors A's `set_t2_frame`).
        self.viewer.set_t2_frame_b(Some(self.b.current_frame));
        self.viewer.invalidate_reference_viewport();
        if !self.thumbs_suppressed() {
            self.viewer.invalidate_reference_thumbnails();
        }
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
        // The viewer keeps its background across the reset above (#151), so the
        // proxy already renders with the persisted background — no re-sync needed.
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
            self.b.loaded_file = None;
            // Drop any in-flight B load so a late decode can't resurrect B and
            // `b.loading` doesn't stick (the path-supersession guard in
            // `apply_load_result` returns before clearing it). #117.
            self.b.loading = false;
            self.clear_b_sequence(); // and any locked-step B state (#98)
            // B-only compare modes are meaningless without B.
            self.viewer.compare_mode = crate::viewer::CompareMode::SingleA;
            self.viewer.blink_state = false;
            // Drop B's histogram (not part of the cache key).
            self.viewer.invalidate_histogram();
        } else {
            self.exr_data = None;
            self.loaded_file = None;
            self.exr_data_b = None;
            self.b.loaded_file = None;
            // Supersede any in-flight slot-A open so its late result can't
            // resurrect the released image (#109 / #117).
            self.open_gen_a += 1;
            // Tear down the app-owned playback + decode state too. Without this the
            // T1 ring keeps every decoded frame resident (the memory leak) and the
            // decode-ahead pump keeps re-issuing `LoadJob`s for the now-unloaded
            // sequence — a late sequence-frame result (guarded by epoch, not path)
            // could even resurrect the image. Mirrors the load-side reset in
            // `detect_sequence`; `playback.clear()` bumps the epoch so any in-flight
            // decode is dropped on arrival. #117.
            self.frame_cache.clear();
            self.frame_bytes = None;
            self.inflight.clear();
            self.watch_sigs.clear();
            self.last_watch_poll = None;
            self.loading_a = false;
            self.b.loading = false;
            self.clear_b_sequence(); // locked-step B goes with A (#98)
            self.playback.clear();
            self.reset_viewer_session();
        }
        self.error_msg = None;
    }

    /// floki's own resident image footprint in bytes: the decoded pixel buffers it
    /// holds — slot A (or, for a sequence, the resident T1 frames) plus slot B.
    /// Shown in the status bar beside process RSS. Unlike RSS — which the allocator
    /// keeps mapped after a free, so it doesn't budge on unload — this drops
    /// immediately when an image is released, reflecting what floki actually holds
    /// (#117).
    fn tracked_image_bytes(&self) -> u64 {
        // For a sequence the active frame is one of the resident T1 frames (a
        // shared `Arc`), so the cache already accounts for slot A — don't also add
        // `exr_data`, which would double-count it.
        let a = if self.playback.is_active() {
            self.frame_bytes
                .map_or(0, |b| self.frame_cache.len() as u64 * b as u64)
        } else {
            self.exr_data
                .as_ref()
                .map_or(0, |d| d.approx_bytes() as u64)
        };
        let b = self
            .exr_data_b
            .as_ref()
            .map_or(0, |d| d.approx_bytes() as u64);
        a + b
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
                egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(90, 160, 240)),
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
        // Mirror the viewer-owned display prefs (#151) into the serde bridge so the
        // whole-app serialize below persists them. Done here, at persist time only,
        // instead of the old per-frame round-trip around `viewer.ui`.
        self.persisted_prefs = self.viewer.prefs.clone();
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
        // Eager precache (#56, step 4): fill the in/out range while idle when the
        // user has enabled it. No-op unless precache is on and a sequence loaded.
        self.tick_precache(ui.ctx());
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
        self.draw_playback_debug(ui.ctx());
        self.draw_playback_hud(ui.ctx());
        self.draw_menu_bar(ui);
        self.draw_status_bar(ui);
        // Transport bar sits just above the status bar (added after it, so it
        // stacks above); a no-op panel unless a sequence is loaded.
        self.draw_transport_bar(ui);
        self.draw_side_panel(ui);
        self.draw_layers_panel(ui);
        self.draw_central_canvas(ui);
        // Pre-upload T2 GPU textures ahead of the playhead (#56). After the canvas
        // so the on-screen frame's texture exists; self-gates when T2 is off.
        self.pump_t2();
        // ...and the slot-B ring in locked-step compare (#166); self-gates unless
        // B is a sequence with a non-zero B ring cap.
        self.pump_t2_b();
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
                LoadMsg::Proxy { path, proxy } => self.apply_proxy(ctx, &path, proxy),
                LoadMsg::Loaded(res) => self.apply_load_result(*res),
            }
        }
        // Recover a permanently stuck playback decode instead of freezing.
        self.tick_decode_watchdog();
        // Once-per-second decode trace for RUST_LOG=floki=debug diagnosis.
        self.trace_playback_state();
        if self.loading_a
            || self.b.loading
            || !self.inflight.is_empty()
            || !self.b.inflight.is_empty()
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
            || !self.b.inflight.is_empty()
            || self.b.pending.is_some();
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
    fn trace_playback_state(&mut self) {
        use crate::playback::PlayState;
        if !self.playback.is_active() || !log::log_enabled!(log::Level::Debug) {
            return;
        }
        let outstanding = !self.inflight.is_empty()
            || self.playback.pending.is_some()
            || !self.b.inflight.is_empty()
            || self.b.pending.is_some();
        // Only trace while there is something that *should* be progressing.
        if self.playback.state != PlayState::Playing && !outstanding && !self.loading_a {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .dbg_last_trace
            .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(1))
        {
            return;
        }
        self.dbg_last_trace = Some(now);
        let mut inflight: Vec<u32> = self.inflight.iter().copied().collect();
        inflight.sort_unstable();
        log::debug!(
            target: "floki::playback",
            "state={:?} frame={} pending={:?} loading_a={} inflight={inflight:?} epoch={} \
             worker={} submit_age={:?} last_decode={:?} precache={}/{} t1={}/{}",
            self.playback.state,
            self.playback.current_frame,
            self.playback.pending,
            self.loading_a,
            self.playback.epoch,
            if self.load_rx.is_some() { "alive" } else { "dead" },
            self.decode_submit_at.map(|t| now.duration_since(t)),
            self.last_decode_dur,
            self.precache,
            self.precache_filled,
            self.frame_cache.len(),
            self.frame_cache_cap,
        );
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
                        ui.checkbox(&mut self.show_layers_panel, "Layers")
                            .on_hover_text(
                                "Compositing layer stack: stack N sources as layers with \
                                 per-layer blend / opacity / visibility (#99)",
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

        let t1_len = self.frame_cache.len();
        let t1_cap = self.frame_cache_cap;
        let t2_len = self.viewer.t2_len();
        let t2_cap = self.viewer.t2_cap();
        let t2_len_b = self.viewer.t2_len_b();
        let t2_cap_b = self.viewer.t2_cap_b();
        let frame_bytes = self.frame_bytes;
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

                        ui.label("T1 (CPU)");
                        let t1_frame = frame_bytes
                            .map(|b| fmt_bytes(b as u64))
                            .unwrap_or_else(|| "—".into());
                        ui.label(format!("{t1_len} / {t1_cap} frames  ·  ~{t1_frame}/frame"));
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

                        ui.label("T2 (GPU)");
                        let t2 = if t2_cap == 0 {
                            "off".to_string()
                        } else {
                            format!("{t2_len} / {t2_cap} frames")
                        };
                        ui.label(t2);
                        ui.end_row();

                        // Slot-B T2 ring (#166) — only meaningful in locked-step
                        // compare; shows "off" otherwise.
                        ui.label("T2-B (GPU)");
                        let t2b = if t2_cap_b == 0 {
                            "off".to_string()
                        } else {
                            format!("{t2_len_b} / {t2_cap_b} frames")
                        };
                        ui.label(t2b);
                        ui.end_row();

                        ui.label("worker");
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
                    self.viewer.clear_t2();
                }

                // A/B frame offset (#166): nudge the compared (B) sequence relative
                // to A. Only shown when B is a locked-step sequence.
                if self.b.sequence.is_some() {
                    ui.separator();
                    ui.label("A/B offset");
                    let mut off = self.b.offset;
                    if ui
                        .add(
                            egui::DragValue::new(&mut off)
                                .range(-9999..=9999)
                                .speed(0.1)
                                .suffix(" f"),
                        )
                        .on_hover_text(
                            "Frame offset of the compared (B) sequence relative to A: \
                             nudge B ahead (+) or behind (−) to line up takes that \
                             start on different frames. Clamped to B's range.",
                        )
                        .changed()
                    {
                        self.b.offset = off;
                        // Re-align B to the new offset immediately.
                        self.sync_b_to_a();
                    }
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
                // f64 throughout so the mapping stays monotonic for long
                // sequences (frame offsets can exceed u16/f32-exact ranges).
                let t = if span == 0 {
                    0.5
                } else {
                    (f64::from(f.saturating_sub(lo)) / f64::from(span)) as f32
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
            let nslots = f64::from(span) + 1.0; // frames lo..=hi inclusive
            let slot_x = |f: u32| {
                rect.left() + ((f64::from(f.saturating_sub(lo)) / nslots) as f32) * rect.width()
            };
            // Two-tone (#172): proxy/beauty-resident frames in a dim green, full-res
            // on top in a brighter green — so a range that's only proxy-cached (it
            // sharpens to full on pause) reads differently from a fully-cached one.
            let proxy_fill = egui::Color32::from_rgb(52, 104, 72);
            let full_fill = egui::Color32::from_rgb(64, 168, 96);
            // Coalesce a sorted frame set into contiguous runs and paint each as one
            // rect (#146 — one shape per frame tessellated thousands per repaint).
            let paint_runs = |frames: &mut Vec<u32>, color: egui::Color32| {
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
                    let seg = egui::Rect::from_min_max(
                        egui::pos2(slot_x(start), strip_top),
                        egui::pos2(slot_x(end + 1).min(rect.right()), rect.bottom()),
                    );
                    painter.rect_filled(seg, 0.0, color);
                }
            };
            // only the trimmed range is the precache target
            let in_range = |f: u32| f >= in_pt && f <= out_pt;
            let mut resident: Vec<u32> = self
                .frame_cache
                .resident_frames(crate::cache::Slot::A)
                .filter(|&f| in_range(f))
                .collect();
            paint_runs(&mut resident, proxy_fill);
            let mut full: Vec<u32> = self
                .frame_cache
                .resident_full_frames(crate::cache::Slot::A)
                .filter(|&f| in_range(f))
                .collect();
            paint_runs(&mut full, full_fill);
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
            let t = f64::from((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            let frame = lo + (t * f64::from(span)).round() as u32;
            self.playback_scrub_to(frame);
        }
        if resp.drag_stopped() {
            self.scrub_active = false;
            self.settle_to_full();
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
                        if let (Some(path), Some(data)) = (&self.b.loaded_file, &self.exr_data_b) {
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

                                // Recomputing full-res bins on every frame swap
                                // would block the UI thread at playback rate and
                                // contend with decode for the rayon pool (#141).
                                // INV-SAMPLE already suppresses the pixel readout
                                // while playing/pending — mirror it: hold the last
                                // computed bins and recompute once on settle (the
                                // swap invalidated the key).
                                if !self.playback.sampling_suppressed() {
                                    self.viewer
                                        .calculate_histogram(exr_data, self.exr_data_b.as_deref());
                                }

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

    /// Add a source to the Layers-panel stack (#99 PR-B.2): **decode-on-demand**.
    /// Synchronously decodes `path` (the panel is a paused, cap-6 workflow, so it
    /// reuses [`ExrData::load`] directly rather than the A/B decode worker), then —
    /// when a GPU is present — uploads AOV 0 into a texture keyed by a fresh
    /// `SourceId` in [`Self::comp_sources`], so the composite ping-pong (PR-B.3)
    /// can bind it. On a decode error nothing is added; the error surfaces in the
    /// status bar. Headless (no `gpu_resources`) still registers the model layer +
    /// pixels, just without a texture.
    fn add_comp_source(&mut self, path: std::path::PathBuf) {
        if self.comp_stack.len() >= COMP_LAYER_CAP {
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
        let source = crate::layer::SourceId(self.comp_next_source);
        self.comp_next_source += 1;
        self.comp_stack
            .push_image(name, source, 0, crate::layer::Trim::full(0, u32::MAX));

        // Build the GPU texture for AOV 0 (matching `push_image`'s aov). Absent a
        // GPU device (headless / CPU-only) the source is stored pixels-only.
        let aov = 0;
        let size = exr_data.logical_size(aov).unwrap_or((0, 0));
        let (texture, bind_group) = match self.gpu_resources.as_ref() {
            Some(gpu) => crate::viewer::ExrViewer::build_source_texture(gpu, &exr_data, aov)
                .map_or((None, None), |(t, bg)| (Some(t), Some(bg))),
            None => (None, None),
        };
        self.comp_sources.insert(
            source,
            CompSource {
                exr_data,
                size,
                aov,
                bind_group,
                texture,
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
            crate::viewer::ExrViewer::build_source_texture(gpu, &exr_data, aov)
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
        // GC the source if the removed layer was its last reference.
        if let Some(source) = source
            && !self.comp_stack.iter().any(|l| {
                matches!(&l.source, crate::layer::LayerSource::Image { source: s, .. } if *s == source)
            })
        {
            self.comp_sources.remove(&source);
        }
    }

    /// The additive **Layers** panel (#99 PR-B): a right dock listing the panel's
    /// composite stack top-to-bottom, with Add / remove. Per-layer blend / opacity /
    /// visibility / solo / AOV controls and reorder land in PR-B.4; rendering the
    /// stack via the accumulate ping-pong lands in PR-B.3.
    fn draw_layers_panel(&mut self, ui: &mut egui::Ui) {
        if !self.show_layers_panel || self.viewer.fullscreen {
            return;
        }
        egui::Panel::right("layers_panel")
            .resizable(true)
            .min_size(220.0)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Layers");
                    ui.label(
                        egui::RichText::new(format!(
                            "{}/{}",
                            self.comp_stack.len(),
                            COMP_LAYER_CAP
                        ))
                        .weak(),
                    );
                });
                ui.separator();

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
                        self.add_comp_source(path);
                    }
                });
                ui.separator();

                if self.comp_stack.is_empty() {
                    ui.weak("No layers yet. Add a source to begin.");
                    return;
                }

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
                }
                let rows: Vec<Row> = self
                    .comp_stack
                    .iter()
                    .map(|l| {
                        let (source, aov) = match &l.source {
                            crate::layer::LayerSource::Image { source, aov } => (Some(*source), *aov),
                            crate::layer::LayerSource::Adjustment => (None, 0),
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
                        }
                    })
                    .collect();
                let count = rows.len();
                let solo_active = self.comp_stack.solo_active();
                let mut remove: Option<crate::layer::LayerId> = None;
                let mut reorder: Option<(crate::layer::LayerId, usize)> = None;

                // Display top-of-stack first: iterate high stack index → low.
                for (i, row) in rows.iter().enumerate().rev() {
                    ui.horizontal(|ui| {
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

                        // Name, greyed when it won't render (disabled, or hidden by a
                        // solo elsewhere).
                        let renders = if solo_active { row.solo } else { row.enabled };
                        ui.add(egui::Label::new(if renders {
                            egui::RichText::new(&row.name)
                        } else {
                            egui::RichText::new(&row.name).weak()
                        }));

                        // Right-aligned per-row controls: remove, reorder, blend.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("✕").on_hover_text("Remove layer").clicked() {
                                remove = Some(row.id);
                            }
                            // ⬆ moves toward the top of the composite (higher index);
                            // ⬇ toward the bottom. Disabled at the ends.
                            ui.add_enabled_ui(i + 1 < count, |ui| {
                                if ui.small_button("⬆").on_hover_text("Move up").clicked() {
                                    reorder = Some((row.id, i + 1));
                                }
                            });
                            ui.add_enabled_ui(i > 0, |ui| {
                                if ui.small_button("⬇").on_hover_text("Move down").clicked() {
                                    reorder = Some((row.id, i - 1));
                                }
                            });
                            // Opacity 0–100% (applies to every layer, including the
                            // bottom — the shader premultiplies its color by this).
                            let mut opacity = row.opacity;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut opacity)
                                        .range(0.0..=1.0)
                                        .speed(0.01)
                                        .fixed_decimals(2),
                                )
                                .on_hover_text("Opacity")
                                .changed()
                                && let Some(l) = self.comp_stack.get_mut(row.id)
                            {
                                l.opacity = opacity;
                            }
                            // Blend (unused for the bottom layer, which is a plain
                            // copy — the base of the composite has nothing beneath it).
                            ui.add_enabled_ui(i > 0, |ui| {
                                let mut blend = row.blend;
                                egui::ComboBox::from_id_salt((row.id, "blend"))
                                    .selected_text(blend.label())
                                    .width(90.0)
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
                            // AOV picker: which logical layer (pass) of the source to
                            // show. Only for multi-layer EXRs — a single-beauty source
                            // has nothing to choose. Changing it rebuilds the source
                            // texture next frame (`ensure_comp_aov`).
                            if row.aov_names.len() > 1 {
                                let mut aov = row.aov.min(row.aov_names.len() - 1);
                                egui::ComboBox::from_id_salt((row.id, "aov"))
                                    .selected_text(row.aov_names[aov].clone())
                                    .width(90.0)
                                    .show_ui(ui, |ui| {
                                        for (idx, nm) in row.aov_names.iter().enumerate() {
                                            ui.selectable_value(&mut aov, idx, nm);
                                        }
                                    });
                                if aov != row.aov
                                    && let Some(l) = self.comp_stack.get_mut(row.id)
                                    && let crate::layer::LayerSource::Image { aov: a, .. } =
                                        &mut l.source
                                {
                                    *a = aov;
                                }
                            }
                        });
                    });
                }
                // Apply the structural edits after the loop (each restructures the Vec).
                if let Some((id, to)) = reorder {
                    self.comp_stack.move_to(id, to);
                }
                if let Some(id) = remove {
                    self.remove_comp_layer(id);
                }
            });
    }

    /// Render the Layers-panel composite in the central canvas (#99 PR-B.3):
    /// resolve `comp_stack.composite_at` at the shared global frame, look up each
    /// draw's decoded source texture in `comp_sources`, and fold them bottom→top
    /// through the viewer's OCIO accumulate ping-pong. Requires OCIO active + a GPU
    /// (the ping-pong lives on the OCIO path); otherwise shows a hint — the N-layer
    /// non-OCIO composite is a follow-up. Assumes the panel is shown with a
    /// non-empty stack (the caller gates on that).
    fn draw_comp_central(&mut self, ui: &mut egui::Ui) {
        // Mirror the per-frame viewer state the slot-A path sets before `viewer.ui`,
        // so the composite honors the same tone / OCIO / LUT settings.
        self.viewer.enable_lut = self.enable_lut && self.lut_bg.is_some();
        self.viewer.lut_domain_min = self.lut_domain_min;
        self.viewer.lut_domain_max = self.lut_domain_max;
        self.viewer.ocio_active = self.ocio_enabled && self.ocio_ready;
        self.viewer.ocio_render_gen = self.ocio_render_gen;

        // Resolve the model → concrete draws at the shared global playhead (stills
        // sit at frame 0 via `Trim::full`; per-source sequence frames arrive w/ PR-C).
        let steps = self.comp_stack.composite_at(self.playback.current_frame);

        // Rebuild any source texture whose layer switched AOV since it was last built
        // (#99 PR-B.4.3), so the draw below binds the requested pass.
        for step in &steps {
            if let crate::layer::Step::Draw(d) = step {
                self.ensure_comp_aov(d.source, d.aov);
            }
        }

        // Bottom→top draw list from the decoded sources. A step whose source has no
        // texture (headless, or a not-yet-built AOV) is skipped; the bottom drawable
        // layer defines the shared canvas size.
        let mut draws: Vec<crate::viewer::CompDraw> = Vec::new();
        let mut base_size = (0usize, 0usize);
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
            }
            draws.push(crate::viewer::CompDraw {
                bind_group,
                blend: d.blend,
                opacity: d.opacity,
            });
        }

        if self.viewer.ocio_active
            && !draws.is_empty()
            && let Some(gpu) = self.gpu_resources.as_ref()
        {
            let lut = self.lut_bg.clone();
            self.viewer
                .draw_comp_composite(ui, base_size, &draws, gpu, lut);
        } else {
            let msg = if self.gpu_resources.is_none() {
                "No GPU: the compositing viewport is unavailable."
            } else if !self.viewer.ocio_active {
                "Enable OCIO (Color Management) to view the layer composite."
            } else {
                "Add a source to the Layers panel to begin."
            };
            ui.centered_and_justified(|ui| {
                ui.label(msg);
            });
        }
    }

    fn draw_central_canvas(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            // The Layers-panel composite takes over the viewport when the panel is
            // active with sources (#99 PR-B.3), separate from the A/B path below
            // (which is untouched — the compare modes are unaffected).
            if self.show_layers_panel && !self.comp_stack.is_empty() {
                self.draw_comp_central(ui);
                return;
            }
            if self.loaded_file.is_some() {
                if let Some(data) = &self.exr_data {
                    self.viewer.enable_lut = self.enable_lut && self.lut_bg.is_some();
                    self.viewer.lut_domain_min = self.lut_domain_min;
                    self.viewer.lut_domain_max = self.lut_domain_max;
                    self.viewer.ocio_active = self.ocio_enabled && self.ocio_ready;
                    self.viewer.ocio_render_gen = self.ocio_render_gen;
                    // INV-SAMPLE (#7): suppress the pixel readout while a sequence
                    // is advancing or a seek's frame is still in flight.
                    self.viewer.suppress_sampling = self.playback.sampling_suppressed();
                    // Diff controls, custom gradients, and background + presets are
                    // single-owned by the viewer now (#151): the UI mutates them in
                    // place and `save()` persists them straight from the viewer — no
                    // per-frame push/read-back or `mem::take` shuffle.
                    self.viewer.ui(
                        ui,
                        data,
                        self.exr_data_b.as_deref(),
                        self.gpu_resources.as_ref(),
                        self.lut_bg.clone(),
                    );
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

        // One model layer, at AOV 0, referencing source id 0.
        assert_eq!(app.comp_stack.len(), 1, "layer registered");
        assert_eq!(app.comp_next_source, 1, "SourceId allocator advanced");
        let layer = app.comp_stack.iter().next().unwrap();
        let crate::layer::LayerSource::Image { source, aov } = layer.source else {
            panic!("expected an image layer");
        };
        assert_eq!((source, aov), (crate::layer::SourceId(0), 0));

        // The decoded pixels are stored keyed by that source; no GPU texture
        // headless, but the model + size are populated.
        let cs = app.comp_sources.get(&source).expect("source decoded + stored");
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
        assert_eq!(app.comp_next_source, 0, "SourceId not consumed");
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
        let source = crate::layer::SourceId(0);
        assert!(app.comp_sources.contains_key(&source));

        app.remove_comp_layer(id);
        assert!(app.comp_stack.is_empty(), "layer removed");
        assert!(
            !app.comp_sources.contains_key(&source),
            "orphaned source freed"
        );
        // Ids are never reused, so a re-add can't alias the freed source.
        assert_eq!(app.comp_next_source, 1, "allocator not rewound");
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
        let source = crate::layer::SourceId(0);
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
        assert_eq!(app.comp_sources.len(), COMP_LAYER_CAP, "one source per layer");
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
            path: PathBuf::from("superseded.exr"),
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 1, // the older, superseded open
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
            path: newf,
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 5, // still the current open
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

    /// #109/#117: unloading bumps the open generation, so a slot-A open still in
    /// flight when the user unloads can't resurrect the released image on arrival.
    #[test]
    fn unload_supersedes_an_in_flight_open() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.exr");
        write_rgba_exr(&f);
        let data = ExrData::load(&f).unwrap();

        // An open is in flight (gen 1, loading).
        let mut app = ExrApp {
            loaded_file: Some(f.clone()),
            open_gen_a: 1,
            loading_a: true,
            ..Default::default()
        };
        app.unload(false); // bumps the generation; releases the slot
        assert!(app.open_gen_a > 1, "unload advances the open generation");

        // The now-stale open result lands.
        app.apply_load_result(LoadResult {
            path: f,
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 1,
            result: Ok(data),
        });

        assert!(
            app.exr_data.is_none(),
            "a late open must not resurrect the unloaded slot"
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
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 0,
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
            exr_data_b: Some(std::sync::Arc::new(data_b)),
            loading_a: true,
            b: SourceState {
                loaded_file: Some(PathBuf::from("b.exr")),
                loading: true,
                ..Default::default()
            },
            ..Default::default()
        };

        app.apply_load_result(LoadResult {
            path,
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 0,
            result: Ok(data_a),
        });

        assert!(app.exr_data.is_some(), "A data applied");
        assert!(app.exr_data_b.is_none(), "B reset when A changes");
        assert!(app.b.loaded_file.is_none(), "B path cleared when A changes");
        assert!(
            !app.loading_a && !app.b.loading,
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
            exr_data_b: Some(std::sync::Arc::new(data_b)),
            loading_a: true, // full decode in flight
            b: SourceState {
                loaded_file: Some(PathBuf::from("b.exr")),
                loading: true,
                ..Default::default()
            },
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
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: 0,
            result: Ok(data),
        });

        assert!(app.exr_data.is_some(), "A data applied");
        assert!(!app.viewer.has_proxy(), "proxy cleared on handoff");
        assert_eq!(app.viewer.scale, 2.5, "view preserved across handoff");
        assert!(app.exr_data_b.is_none(), "B dropped on explicit new-A open");
        assert!(app.b.loaded_file.is_none(), "B path cleared");
        assert!(!app.loading_a && !app.b.loading, "loading flags cleared");
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

    /// #117: unloading slot A must release the app-owned T1 ring (the leak) and
    /// tear down the decode pump, or the decoded frames stay resident and the
    /// pump keeps re-issuing `LoadJob`s for the unloaded sequence (a late
    /// epoch-guarded frame could even resurrect the image).
    #[test]
    fn unload_a_releases_the_frame_cache_and_stops_the_decode_pump() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("s.0001.exr");
        let f2 = dir.path().join("s.0002.exr");
        write_rgba_exr(&f1);
        write_rgba_exr(&f2);

        let mut app = ExrApp {
            exr_data: Some(std::sync::Arc::new(ExrData::load(&f1).unwrap())),
            loaded_file: Some(f1.clone()),
            ..Default::default()
        };
        app.detect_sequence(&f1);
        assert!(app.playback.is_active(), "sequence mode entered");

        // Simulate a populated T1 ring + an in-flight decode + a stuck load flag.
        // `frame_bytes` is captured on the first real decode; set it so the
        // tracked-footprint accounting has a per-frame size to multiply by.
        let frame = std::sync::Arc::new(ExrData::load(&f1).unwrap());
        app.frame_bytes = Some(frame.approx_bytes());
        app.frame_cache.insert(crate::cache::Slot::A, 1, frame);
        app.inflight.insert(2);
        app.loading_a = true;
        let epoch_before = app.playback.epoch;
        assert!(
            app.tracked_image_bytes() > 0,
            "footprint reflects the resident frame"
        );

        app.unload(false);

        assert!(app.exr_data.is_none(), "image released");
        assert_eq!(
            app.tracked_image_bytes(),
            0,
            "tracked footprint drops to zero on unload"
        );
        assert!(app.frame_cache.is_empty(), "T1 ring freed — no leak");
        assert!(app.inflight.is_empty(), "no in-flight decodes survive");
        assert!(!app.playback.is_active(), "sequence torn down");
        assert!(!app.loading_a, "loading flag cleared");
        assert_ne!(
            app.playback.epoch, epoch_before,
            "epoch bumped so any late decode is dropped on arrival"
        );

        // The decode-ahead pump issues nothing for an unloaded sequence.
        app.pump_decode();
        assert!(app.inflight.is_empty(), "pump stays idle after unload");
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
            path: dir.path().join("s.0003.exr"),
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame: 3,
            epoch: app.playback.epoch,
            open_gen: 0,
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
            path: f2,
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame: 2,
            epoch: app.playback.epoch,
            open_gen: 0,
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
        app.frame_cache.insert(crate::cache::Slot::A, 3, beauty);

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
            path: dir.path().join("s.0002.exr"),
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame: 2,
            epoch: live_epoch.wrapping_sub(1),
            open_gen: 0,
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
            path: dir.path().join("s.0002.exr"),
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame: 2,
            epoch: live_epoch,
            open_gen: 0,
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
            path: f1,
            source: crate::cache::Slot::A.into(),
            seq_frame: false,
            frame: 0,
            epoch: 0,
            open_gen: app.open_gen_a,
            beauty_only: false,
            proxy_target: None,
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
            crate::cache::Slot::A,
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
                crate::cache::Slot::A,
                n,
                std::sync::Arc::new(ExrData::load(&f1).unwrap()),
            );
        }
        let ctx = egui::Context::default();
        app.tick_precache(&ctx);
        assert!(
            app.precache_filled,
            "latches once the range is fully resident"
        );
        // Latched: a second tick submits nothing.
        app.tick_precache(&ctx);
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
                crate::cache::Slot::A,
                n,
                std::sync::Arc::new(ExrData::load(&f1).unwrap()),
            );
        }
        assert_eq!(
            app.frame_cache.len(),
            app.frame_cache_cap,
            "cache at capacity"
        );

        let ctx = egui::Context::default();
        app.tick_precache(&ctx);
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
                crate::cache::Slot::A,
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
        app.frame_cache.insert(crate::cache::Slot::A, 1, beauty);
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
            path: f1,
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame: 1,
            epoch: app.playback.epoch,
            open_gen: 0,
            result: Ok(full),
        });
        assert_eq!(
            app.playback.pending, None,
            "settled once the full frame lands"
        );
        assert!(
            !app.frame_cache
                .get(crate::cache::Slot::A, 1)
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
        app.frame_cache.insert(crate::cache::Slot::A, 1, full);
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
        app.frame_cache.insert(crate::cache::Slot::A, 1, beauty);
        app.exr_data = app.frame_cache.peek(crate::cache::Slot::A, 1);
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
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            open_gen: 0,
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
            source: crate::cache::Slot::A.into(),
            seq_frame: true,
            frame: 2,
            epoch: stale_epoch,
            open_gen: 0,
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
            app.frame_cache.contains(crate::cache::Slot::A, 1),
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
        assert!(app.frame_cache.contains(crate::cache::Slot::A, 1));

        // The render rewrites frame 1 with different content (size changes -> the
        // signature changes even if mtime resolution is coarse).
        std::fs::write(&paths[0], vec![0u8; 4096]).unwrap();
        assert!(app.rescan_and_apply(), "re-rendered frame applied");

        assert!(
            !app.frame_cache.contains(crate::cache::Slot::A, 1),
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
        app.b.loaded_file = Some(bpath.clone());

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
            app.b.loaded_file.as_deref(),
            Some(bpath.as_path()),
            "B's loaded path is unchanged"
        );
    }

    /// Deliver a decoded **slot-B** sequence frame (#98): the B `source` tags the
    /// cache slot on a seq frame.
    fn deliver_b_frame(app: &mut ExrApp, path: &std::path::Path, frame: u32) {
        let data = ExrData::load(path).unwrap();
        app.apply_load_result(LoadResult {
            path: path.to_path_buf(),
            source: crate::cache::Slot::B.into(),
            seq_frame: true,
            frame,
            epoch: app.playback.epoch,
            open_gen: 0,
            result: Ok(data),
        });
    }

    /// Build a B sequence in its own dir and arm locked-step B on `app` as if the
    /// user opened B's first frame while A is loaded (#98).
    fn arm_b_sequence(app: &mut ExrApp, count: u32) -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = (1..=count)
            .map(|n| {
                let p = dir.path().join(format!("bref.{n:04}.exr"));
                write_rgba_exr(&p);
                p
            })
            .collect();
        app.exr_data_b = Some(std::sync::Arc::new(ExrData::load(&paths[0]).unwrap()));
        app.b.loaded_file = Some(paths[0].clone());
        app.detect_sequence_b(&paths[0]);
        (dir, paths)
    }

    #[test]
    fn sequence_b_advances_in_lockstep_with_a() {
        let (_adir, a) = write_sequence(3);
        let mut app = ExrApp::default();
        app.detect_sequence(&a[0]);
        app.frame_cache_cap = 8; // room for both slots' windows
        let (_bdir, b) = arm_b_sequence(&mut app, 3);
        assert!(app.b.sequence.is_some(), "B armed as a sequence");
        assert_eq!(app.b.current_frame, 1, "B starts on its opened frame");
        let b1 = app.exr_data_b.clone().unwrap();

        // Play A one frame; B is a slaved, position-aligned function of A.
        app.playback_toggle();
        app.advance_playhead();
        assert_eq!(app.playback.current_frame, 2, "A advanced");
        assert_eq!(app.b.current_frame, 2, "B tracks A");

        // One worker, one decode at a time: A's playhead lands first and frees the
        // worker, then B's frame is pumped and delivered.
        deliver_frame(&mut app, &a[1], 2);
        deliver_b_frame(&mut app, &b[1], 2);
        assert!(
            app.frame_cache.contains(crate::cache::Slot::B, 2),
            "B frame decoded into Slot::B"
        );
        assert!(
            app.frame_cache.contains(crate::cache::Slot::A, 2),
            "A frame still in Slot::A (B didn't clobber it)"
        );
        assert!(
            app.exr_data_b
                .as_ref()
                .is_some_and(|d| !std::sync::Arc::ptr_eq(d, &b1)),
            "B display swapped off its opened frame"
        );
    }

    #[test]
    fn apply_load_result_routes_seq_frame_by_slot() {
        let (_adir, a) = write_sequence(2);
        let mut app = ExrApp::default();
        app.detect_sequence(&a[0]);
        app.frame_cache_cap = 8;
        let (_bdir, b) = arm_b_sequence(&mut app, 2);
        // A B seq frame (B `source`) is filed under Slot::B, never Slot::A.
        deliver_b_frame(&mut app, &b[1], 2);
        assert!(
            app.frame_cache.contains(crate::cache::Slot::B, 2),
            "B seq frame routed to Slot::B"
        );
        assert!(
            !app.frame_cache.contains(crate::cache::Slot::A, 2),
            "not misfiled into Slot::A"
        );
    }

    #[test]
    fn b_move_to_hole_clears_loading_b_so_pump_is_not_gated() {
        // #98 regression (Copilot): B is slaved to A, so it can move to a new frame
        // — or a hole — while a previous B decode is still awaited. If `b.loading`
        // isn't reset on the move, its stale-frame decode never matches
        // `b.current_frame`, `b.loading` latches true, and `pump_decode` is gated
        // forever → playback freeze.
        let (_adir, a) = write_sequence(4);
        let mut app = ExrApp::default();
        app.detect_sequence(&a[0]);
        app.frame_cache_cap = 8;

        // A B sequence with a hole at frame 3 (files 1, 2, 4).
        let bdir = tempfile::tempdir().unwrap();
        for n in [1u32, 2, 4] {
            write_rgba_exr(&bdir.path().join(format!("bref.{n:04}.exr")));
        }
        let b1 = bdir.path().join("bref.0001.exr");
        app.exr_data_b = Some(std::sync::Arc::new(ExrData::load(&b1).unwrap()));
        app.b.loaded_file = Some(b1.clone());
        app.detect_sequence_b(&b1);
        assert!(
            app.b.sequence
                .as_ref()
                .is_some_and(|s| s.holes.contains(&3)),
            "B has a hole at 3"
        );

        // Simulate B awaiting a frame (a decode in flight), then move B onto the
        // hole — as a locked-step advance would.
        app.b.loading = true;
        app.b.pending = Some(2);
        app.request_b_frame(3);
        assert!(!app.b.loading, "hole clears loading_b (pump stays ungated)");
        assert_eq!(app.b.pending, None, "no B frame awaited on a hole");
    }

    #[test]
    fn opening_a_new_a_drops_locked_step_b() {
        let (_adir, a) = write_sequence(2);
        let mut app = ExrApp::default();
        app.detect_sequence(&a[0]);
        let (_bdir, _b) = arm_b_sequence(&mut app, 2);
        assert!(app.b.sequence.is_some());
        // Opening a different A resets the session — B (a lone reference on its
        // own) goes with it.
        let (_a2dir, a2) = write_sequence(2);
        app.detect_sequence(&a2[0]);
        assert!(
            app.b.sequence.is_none(),
            "locked-step B dropped with the A open"
        );
        assert!(
            !app.frame_cache.contains(crate::cache::Slot::B, 1),
            "Slot::B cleared"
        );
    }
}
