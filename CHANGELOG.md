# Changelog

All notable changes to Floki are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Frame-time percentiles.** The playback debug overlay reports p50 / p95 / p99 /
  max frame time over the last 240 shown frames, alongside the existing smoothed
  fps. The smoothed number has a ~5-frame time constant, so it hides exactly the
  hitches a review player is judged on.
- **A soak capture task.** `scripts\run-windows.ps1 soak` runs the release build
  with the playback trace enabled and tees it to `soak-logs\soak-<timestamp>.log`.
  The trace is now stable `key=value` pairs and carries the budget, pacing, and
  memory fields, plus one line whenever playback settles — pause or stop.
- **`inspect_exr` takes paths on the command line** and reports part count,
  per-part compression, channel sample types, and the decoded size that sizes the
  playback cache — enough to characterize a sequence before soaking it. It no
  longer falls back to hardcoded absolute paths when given no arguments.

### Fixed
- **A cheap decode that quietly turns into an expensive one is now visible, and
  budgeted for.** When a proxy or single-pass decode can't be produced for a file,
  the worker falls back to a full all-parts decode so the layer never freezes —
  correct, but silent: nothing distinguished "this footage plays cheap" from
  "every cheap decode is failing and we are quietly decoding everything," and the
  cache stayed sized for 9 MB proxies while 1035 MB frames landed, sending the
  prefetch window far into a ring that evicted each arrival on contact. Fallbacks
  are now counted in the playback trace and the debug overlay, and while the cheap
  path is failing the cache sizes off full frames instead. The override only ever
  makes the cache *smaller*, and clears as soon as a cheap decode succeeds.
- **The frame cache is now held to its byte budget, not just a frame count.**
  Eviction counted frames while the budget it enforced was measured in bytes —
  exact only while every resident frame is the same size, which playback's frames
  are not: a settled frame upgraded from a 57 MB beauty decode to a 1035 MB full
  one changes the ring's cost without changing its length, so a count-based
  evictor saw nothing to do. The ring is now bounded by both, whichever binds
  first, from one budget stated in two units. The playing debug row and the
  playback trace show resident bytes against the budget so the byte bound binding
  is visible rather than inferred.
- **The RAM budget is no longer ~10x under-used on heavy multi-part renders.** The
  cache cap was sized by dividing the budget by a *full* frame's measured size, but
  the frames playback actually holds are cheaper: with beauty preview on, a
  beauty-only decode carries one pass, not all 23 parts. Beauty-only frames matched
  neither arm of the two-way sizing latch, so they filled the ring while sizing
  nothing — a 40-frame Karma render (1035 MB/frame, 24 GB budget) held 23 frames of
  maybe 80 MB each and evicted 726 times in 45 seconds. There is now a size latch
  per fidelity, picked by asking the decode path's own predicates which one it is
  issuing, so the divisor matches what lands in the ring. The prefetch window is
  derived from the cap, so it widens with it.
- **Resident cache bytes are measured rather than assumed.** The ring reported its
  size as "frame count x one frame's bytes", which counts every proxy or
  beauty-only frame as a full one — so the same wrong figure sat on both sides of
  the budget, and the status bar's tracked-memory readout overstated RAM by an
  order of magnitude during playback. The cache now keeps a live sum of what it is
  holding.
- **Playback caches far more of the sequence, and pressing Play no longer shrinks
  the lookahead.** The prefetch window was clamped to a 16-frame constant while
  playing but took the whole RAM budget while idle, so starting playback *cut* the
  read-ahead — on a six-layer stack from 20 frames per layer to 2, with the
  read-behind window collapsing to nothing so a backwards scrub during play always
  missed. The constant was sized for an era when the cache cap was frozen at 8 and
  never bound on anything; both paths now take the same figure, sized from the
  measured RAM budget and the in/out range.
- **The measured-fps readout no longer reads 0.0 during playback.** It was only
  updated from the primary slot, which stopped decoding when open/drop became "add
  a layer" in 1.12.0 — so the transport driven by a comp layer was never paced.
  Playback now paces off whichever source drives the clock.

### Changed
- The playback debug overlay names what each row actually measures: the T1 row
  shows whether its cap was sized from a measured frame or is sitting at the
  default, the T2 row is marked as slot-A only, and new `comp tex` and `sources`
  rows show the textures and per-layer decode state the comp path really holds.

## [1.12.0] - 2026-08-15

The layer-stack wave (#99). Floki's viewer was built around two hardcoded image
slots, A and B. It is now built around one **layer stack**: you add as many layers
as you like, they composite, they play together on a shared timeline, and the
compare modes became arrangements of that stack rather than a separate system.

### Added
- **N-layer compositing.** Open or drop any number of EXRs and they stack as
  layers, composited on the GPU. Each layer has its own blend mode, opacity,
  visibility, solo, and AOV/pass selection.
- **A timeline with layer tracks.** The bottom panel shows each layer as a clip bar
  on a shared frame ruler, with its own cache-fill strip. Drag a bar to retime that
  layer against the others.
- **Layers play.** Every sequence layer decodes and advances on the shared
  transport, so a multi-layer comp scrubs and plays as one.
- **Blink compare.** A new arrangement that alternates the two compared layers in
  place on a timer, with an adjustable speed.
- **The layer stack persists.** Your layers — paths, order, blend, opacity, solo,
  AOV, and time offset — come back when you relaunch. A layer whose file has moved
  is skipped without disturbing the rest.
- **Contact Sheet button and `T` hotkey.** The sheet now shows the *current
  layer's* passes as a grid; clicking one switches that layer to it.

### Changed
- **Open and drop add a layer** rather than filling slot A or B. The A/B reference
  slot, its menu items, and the "Drop for A / Drop for B" split are gone.
- **Compare modes are arrangements over the stack.** Side-by-Side, Wipe, Diff and
  Blink now compare the **current layer** (the `Layer:` picker) against a layer you
  choose (the `vs:` picker), instead of a fixed A-versus-B. Their parameters live in
  the viewport bar beside the mode selector.
- **The viewport controls moved into the comp bar** — exposure, gamma, sRGB,
  channel isolation, pass selection, the sample aperture, and the anamorphic
  unsqueeze/PAR override.

## [1.11.0] - 2026-07-14

### Added
- **Anamorphic unsqueeze (#179).** The EXR `pixelAspectRatio` is now applied to
  the viewport, so anamorphic footage (e.g. a 2× squeeze) displays at its correct
  wide aspect instead of horizontally compressed. On by default; toggle it off for
  raw square-pixel display, or set a custom squeeze factor for footage with a
  missing/wrong header PAR — both in the viewer's "Display ▾" menu. Pixel readout,
  annotations, "Frame (F)", and the display/data-window overlays all track the
  unsqueeze; the anamorphic state is flagged in the Image Metadata panel.

## [1.10.2] - 2026-07-09

A small follow-up to the 1.10.0 playback wave: the image resolution is now
always visible on the canvas, and the on-disk proxy cache reports its activity.

### Fixed
- **Resolution now always shown on the canvas.** The on-canvas resolution
  readout only appeared when the EXR had overscan (a data window that differs
  from the display window); a normal image showed none. The display-window
  resolution now renders for every image, at the bottom-right of the frame. The
  orange overscan/bbox annotation stays scoped to when the data window differs
  from the display window.

### Changed
- **The persistent on-disk proxy cache now logs its activity.** Hit / miss /
  write / eviction and enable / disable / clear are emitted under the
  `floki::proxy_cache` log target, so cache behaviour is observable at runtime
  (`RUST_LOG=floki::proxy_cache=debug`).

## [1.10.0] - 2026-07-08

The heavy-footage review-playback wave. Scrub proxies, an I/O prefetch pipeline,
and a persistent on-disk cache make 4K multi-AOV sequences play — and re-review —
smoothly, where they used to thrash. Plus DWA-compressed EXRs now open at all.

### Added
- **DWAA/DWAB (DWA-compressed) EXRs now open.** floki previously failed to open
  them (`pixels cannot be compressed (dwaa)`). The patched `exr` decoder is
  rebased onto upstream's DWA support (v1.74.1) while keeping the miniz-inflate
  fix that stops a decompressor panic from aborting the app.
- **Persistent on-disk proxy cache.** Downsampled scrub proxies are cached to
  `~/.floki/proxy-cache`, so a repeat pass or a later session loads them from disk
  instead of re-decoding the source — the first-touch decode is paid once, ever.
  Huge for networked media and repeated review (dailies, shot iteration). On by
  default, LRU-bounded (default 10 GB), with a transport toggle, a size budget,
  and a Clear button. The read-ahead warmer skips frames the cache already holds.
- **In-RAM scrub proxies for heavy footage.** While playing or scrubbing, floki
  decodes a small downsampled proxy so heavy footage plays smoothly and far more
  frames fit in RAM (hundreds vs ~16 full); the paused frame always sharpens to
  full res. Geometry-preserving, so tight-data-window renders still frame
  correctly. A transport toggle with an adjustable proxy size.
- **Locked-step A/B sequence playback.** In wipe/compare modes the B sequence
  plays slaved to A, so both advance together.

### Performance
- **I/O prefetch pipeline.** A background warmer pulls the next frame's file
  through the OS page cache while the current frame decodes (with a zero-copy
  memory-mapped decode), so the read never blocks the decode cores — biggest on
  slow or networked storage.
- **Read-behind window.** ~25% of the prefetch depth is reserved behind the
  playhead, so play-then-stop-then-step-back hits cache instead of re-decoding.
- **f16 proxies.** Proxies keep the source half-float bit depth instead of
  widening to f32 — half the proxy RAM, and the fast `Rgba16Float` upload path
  stays engaged.
- **Contact-sheet thumbnails freeze during playback** and refresh on settle,
  removing a re-bake hitch while scrubbing.

### Changed
- **Eager precache now defaults on.** With proxies and the disk cache making a
  full-range fill cheap, floki warms the whole in/out range up front by default
  (bounded by the RAM budget). Existing saved settings are respected — only fresh
  installs get the new default.
- The `exr` decoder pulls `pulp` (SIMD dispatch for DWA's inverse DCT) as a new
  transitive dependency.

## [1.9.3] - 2026-07-05

More of the July audit ([docs/audit-2026-07.md](docs/audit-2026-07.md)): the
per-frame texture-upload path is roughly halved for the common case, live
render-watch no longer hitches playback, and dark viewport gradients no longer
band.

### Fixed
- **Dark viewport background gradients banded**, showing visible steps across
  the gradient backdrop. The gradient LUT is now baked at full float precision
  instead of 8-bit, and the output is dithered before the 8-bit framebuffer
  write — on both the colour-managed (OCIO) and non-managed paths.
- **"Frame" (F) in Side-by-Side fit only the A image**, pushing the B image
  off-screen. It now frames the combined A+B layout, honouring the size-normalize
  toggle.

### Performance
- **The texture-upload path does about half the per-frame work for the common
  case.** f16 EXR sources — the overwhelming majority of beauty renders — now
  upload as `Rgba16Float` instead of `Rgba32Float`: lossless, and half the VRAM
  and bandwidth. A direct half-float fast path skips the per-pixel float
  widening, and a reused staging buffer replaces a ~66 MB allocation on every
  texture build during playback (a known stutter source on Windows).
- **The GPU pre-upload pump is time-budgeted** (~4 ms per frame) rather than a
  fixed two builds per frame, so a 4K sequence no longer hitches when two large
  uploads land in the same frame, while lighter footage fills the ring faster.
- **Render-watch scans the sequence directory off the UI thread.** The 2-second
  re-scan — a `read_dir` plus a stat per frame — previously ran inside the frame
  loop: a multi-hundred-millisecond hitch mid-playback on a network share, the
  actual use case for watching a live render.

### Changed
- Internal groundwork for the planned Qt port, with no behaviour change: the T2
  GPU-texture ring is now a pure, unit-tested `T2Ring`; the 490-line
  `draw_canvas_gpu` is split into focused helpers; and the render-watch
  scan/apply seam is cleanly separated.

## [1.9.2] - 2026-07-02

A playback-responsiveness patch, from a full codebase audit
([docs/audit-2026-07.md](docs/audit-2026-07.md)). Playback now paces at the
target frame rate, scrubbing heavy multi-AOV sequences is dramatically
snappier, and looped playback no longer stutters at the wrap.

### Fixed
- **Playback ran below the target fps even with every frame cached.** The frame
  clock re-armed a full period after each tick instead of at the next absolute
  deadline, so every frame paid the timer/vsync wake-up slop (24 fps paced out
  at ~20 on a 60 Hz display); and completed decodes sat unread for up to 50 ms
  because the worker couldn't wake the UI. The clock now schedules to the
  deadline and the decode worker requests a repaint the moment a result lands.
- **Holding the scrubber still re-decoded the held frame forever.** A
  stationary drag superseded its own decode every UI frame, so the frame under
  the cursor never displayed until release. Same-frame seeks (and held arrow
  keys at a range boundary) are no-ops now — while still retrying frames whose
  decode failed.
- **Looped playback stuttered at every wrap.** Prefetch wrapped past the out
  point was misclassified as "behind" and evicted the moment it landed, so the
  decoder churned near the out point and the wrap always landed on a cache
  miss. Loop eviction now measures distance around the loop.
- **Contact-sheet thumbnails went stale when the `.cube` LUT was toggled or
  reloaded**, showing pre-toggle pixels until an unrelated change forced a
  re-render.
- **A loading OCIO config could flash the background over the canvas** for a
  frame; the blit now waits for the first real render.
- **Reloading a `.cube` LUT mid-frame could crash on Vulkan** (the old LUT
  texture was destroyed while a recorded draw could still reference it).

### Performance
- **Scrubbing decodes only the beauty layer while the drag is held**, with the
  landing frame upgraded to a full all-AOV decode on release — heavy multi-AOV
  EXRs scrub at beauty-decode speed instead of full-frame speed.
- **The histogram no longer recomputes at full resolution on every playback
  frame** while the side panel is open; it refreshes when playback settles.
- **The overscan dim renders in a single draw.** The non-OCIO path previously
  drew the whole image twice per repaint (once dimmed, once clipped to the
  display window); the shader now dims outside the display window per fragment.
- Blink compare mode wakes exactly at the next A/B flip instead of repainting
  at the full refresh rate; snapshots finalize (clipboard + PNG) off the UI
  thread; the timeline's cache strip draws contiguous runs as single rects; and
  the RAM budget enforces cache shrinks immediately under memory pressure
  instead of waiting for the next decode.

## [1.9.1] - 2026-07-02

A stability patch for the 1.9.0 playback release: several ways sequence playback
could freeze mid-shot (image frozen, UI still responsive, only reopening the file
helped) are fixed, and playback now recovers on its own if a decode ever gets stuck.

### Fixed
- **Playback freezes during play and scrubbing.** Multiple causes in the playback
  cache and decode pipeline are addressed: eager precache no longer decode/evict
  churns forever when the in/out range is larger than the RAM budget (it stops once
  the cache is full); playing prefetches the window *ahead* of the playhead instead
  of wrapping to the far side and starving it; settling or scrubbing onto a
  beauty-only cached frame now actually submits its full re-decode instead of
  stalling; and rapid scrubbing no longer floods the decode worker with soon-stale
  jobs — the worker skips superseded frames before decoding, so the frame you land
  on appears right away.
- **Self-recovery.** A decode-stall watchdog and automatic decode-worker respawn
  recover a stuck or crashed decode on their own, instead of freezing until the file
  is reopened. `Stop` also clears any awaited decode so the readout and the next
  `Play` aren't left waiting on a frame that never lands.

## [1.9.0] - 2026-06-30

The **image-sequence playback** release. Floki becomes a sequence review player:
open one frame of a numbered sequence and play the whole shot, with a smart
cache that keeps playback real-time and degrades instead of crashing under
memory pressure.

> **Raised system requirements.** Color management is now always-on (OpenColorIO
> is mandatory, #121) and the viewport is GPU-only (the CPU render path and CPU
> OCIO processor were removed, #59) — Floki now requires a GPU supporting
> `FLOAT32_FILTERABLE`. There is no CPU fallback.

### Added
- **Image-sequence playback (#7).** Open one frame of a numbered sequence to
  play the whole shot. A drift-corrected frame clock with editable **target fps**
  and a live **measured fps** readout, **Loop / Once / Ping-Pong**, **reverse**,
  **in/out trim** (Set In / Set Out / Reset), and **Stutter vs Drop-frames**
  pacing. A transport bar with a scrubber/timeline that marks the trimmed region
  and missing frames (holes); Space toggles play/pause, ←/→ step, Stop halts in
  place. Sequence detection handles numeric sort and gaps (#83).
- **Byte-budgeted playback cache (#56/#57/#92).** A four-tier cache keeps frames
  resident across the CPU/GPU boundary: a **T1 CPU ring** fed by a **decode-ahead
  prefetch worker** and a **T2 GPU-texture pre-upload ring**, each sized live from
  the RAM/VRAM budget so a scrub-back or loop is an instant hit and playback
  degrades (fewer frames / lower fps) instead of crashing under pressure.
- **Beauty-only fast decode (#132).** While playing, decode just the beauty/first
  layer — multi-part AOV EXRs decode several times faster and use far less RAM —
  then re-decode the settled frame in full so the pixel readout and AOV switch
  stay correct. A "Beauty preview" toggle; on by default.
- **Eager precache + cache-fill bar (#133).** A "Precache" toggle fills the whole
  in/out range into the cache up front, so once the green residency bar under the
  scrubber is full the span plays and loops with the decoder idle. Bounded by the
  RAM budget — it caches what fits and shows it honestly.
- **Pixel-readout correctness during playback (#131).** The color sampler is
  suppressed while the clock advances (it would lag the playhead or cost a full
  frame scan per hover) and re-enabled on settle, re-decoding the frame in full so
  the probe is always pixel-accurate when you stop to inspect.
- **Live render-watch (#101).** Watch the sequence folder and pick up frames as a
  render writes them — new frames extend the range, re-rendered frames refresh —
  with an optional "Follow" to park the playhead on the newest frame.
- **Playback debug overlay (#100).** A live readout (residency, fps, worker
  in-flight/pending, RAM/VRAM, evictions) for soak-testing playback on real
  footage, with a runnable soak checklist (#129).
- **GPU contact-sheet thumbnails (#67).** The contact sheet now renders thumbnails
  on the GPU through the OCIO display transform, replacing the CPU thumbnail bake.

### Changed
- **OCIO is now mandatory (#121).** The non-OCIO build is gone; color management
  is always on, so what you see matches the configured display transform by
  default.
- **Internal: a comp layer-stack model now backs the A/B compare UI (#103/#114).**
  The pure, headless layer model is the spine for upcoming N-way compare and
  locked-step A/B work; adopting it behind today's compare UI is a no-behavior
  refactor.

### Removed
- **CPU viewport render path + CPU OCIO processor (#59).** The viewport renders
  exclusively on the GPU now (see the raised-requirements note above).

### Fixed
- Open a new EXR while a sequence is playing (#109).
- Release the frame cache and stop the decode pump when slot A is unloaded, so
  decoded frames don't leak and the pump doesn't keep re-issuing jobs (#117).
- Apply the user gamma control in the OCIO view path (#93).
- Dedicate a contact-sheet thumbnail cache so it doesn't fight the main texture
  cache (#115).
- Index-based dashes in the dashed-rectangle overlay (#71).

## [1.8.0] - 2026-06-21

### Added
- **Instant first paint for large EXRs (#33).** Opening a big EXR now shows a
  low-resolution proxy in tens of milliseconds — the 669 MB redSea render goes
  from a ~1.3 s blank wait to a ~30 ms first paint (~43×) — then sharpens to
  full resolution once the decode finishes, with zoom/pan preserved across the
  handoff. The proxy comes from a fast subsampled read (decompressing only every
  Nth scanline block, so the work is bounded regardless of resolution) on the
  decode worker; tiled/deep images and small files fall back to the existing
  spinner-then-full path. Builds on the #58 render path and the #55 swap seam.
- **OCIO: bake the display transform to a 3D LUT (#24).** A new **Bake to 3D
  LUT** toggle under **Color Management** replaces the per-pixel analytic ACES
  math (~875-line shader, 15 `pow()`) with a cheap 3D-LUT lookup, for much
  smoother pan/zoom on weaker GPUs. Off by default — the analytic transform
  stays the accuracy reference — and visually indistinguishable for SDR when on
  (65³ tetrahedral LUT fronted by a log2 shaper). The setting persists.
- **Internal: render-side proxy first-paint path (#58).** Adds a low-res
  `ProxyImage` (standalone RGBA32Float buffer + full image dimensions) and a
  viewer proxy texture slot with a tone-baked upload (exposure/gamma/sRGB +
  background, mirroring the CPU `generate_texture` path). While the full-res
  `ExrData` decode is in flight, the loading branch renders the proxy instead
  of a spinner; when the full decode lands, `swap_image_data` (#55) clears the
  proxy and the viewport swaps to full-res with zoom/pan preserved. The proxy
  uses the non-OCIO tone pipeline even when OCIO is active (transient stand-in;
  OCIO-accurate proxy is a follow-up). The decode-side producer (a true low-res
  EXR read) is #33 — `ExrApp::set_proxy` is the seam it will call from the
  worker. No user-facing change yet (nothing produces a proxy).

### Changed
- **Internal: decouple GPU core from `egui_wgpu::Renderer` ownership (#54).**
  Introduces an app-owned `GpuResources` (`src/gpu/resources.rs`) as the single
  home for the persistent `GpuState`; the application is now the source of
  truth, with egui's `callback_resources` holding only an `Arc<GpuState>` clone
  for the `CallbackTrait` paint callbacks. The viewer and app read `GpuState`
  directly off `GpuResources` instead of a per-frame `renderer.read()` typemap
  lookup. OCIO pass/targets still live in `callback_resources` (the OCIO
  callback's `prepare` mutates `OcioTargets` through the typemap), but their
  lifecycle is centralized behind `GpuResources::publish_ocio_pass` /
  `invalidate_ocio_targets`, replacing the hand-rolled `insert` +
  `remove::<OcioTargets>()` footgun in `rebuild_ocio_pass`. Prerequisite for
  the Qt port (#44) and clean resource management for #7 / #24 / #33. No
  user-facing change.
- **Internal: split image-data swap from viewer session-state reset (#55).**
  Extracted `swap_image_data` (replaces the pixel source for A or B while
  preserving zoom, pan, compare mode, channel mode, annotations, swatches, and
  tone/OCIO/LUT state) and `reset_viewer_session` (the full reset used on an
  explicit open / new session) as named seams. The open path still resets the
  viewer and drops B exactly as before; the new swap path is the contract
  image-sequence playback (#7) will use for per-frame loads so a frame change
  doesn't wipe the user's view. Also clamps `active_layer` to the new image's
  layer count on swap so a frame with fewer passes can't index out of bounds.
  No user-facing change.

### Fixed
- **Docs: corrected the README's CPU-fallback claim (#63).** The README stated
  the app "automatically drops down to multithreaded CPU rendering if a graphics
  card or driver is unavailable" — it does not; floki requires a working GPU to
  launch (the internal CPU path is for contact-sheet thumbnails and headless
  tests). The docs now describe the GPU requirement. Shipped alongside an
  internal auto-fixable lint cleanup and annotations marking the June 2026 audit
  items as completed (#73).

### Notes
- **macOS: the downloaded binary is not yet code-signed or notarized (#64).** On
  first launch macOS Gatekeeper will report *"floki cannot be opened because the
  developer cannot be verified."* Clear the quarantine flag with
  `xattr -d com.apple.quarantine ./floki` (or right-click the binary → **Open**
  the first time). Signing/notarization is tracked as #64.

## [1.7.2] - 2026-06-20

### Changed
- **Internal maintainability only — no user-facing changes.** Acted on the
  remaining items from the June 2026 codebase audit: broke up the ~1100-line
  `ExrApp::ui` per-frame entry point into focused per-panel methods, documented
  the `floki-ocio` public API (`#[must_use]` + `# Errors`), added a
  `.clang-format` for the C++ OCIO shim, and cleared a batch of lint findings
  (signature cleanups, struct field order). Behaviour is unchanged; the full
  test suite and `clippy -D warnings` pass.

## [1.7.1] - 2026-06-20

### Added
- **Resource monitor.** A discrete status-bar readout shows floki's own memory
  footprint and system RAM, plus live GPU **VRAM** usage on macOS (Metal) — handy
  when loading heavy EXRs or sequences. It samples about once a second and tucks
  into the bottom-right. Windows and Linux show RAM only for now.

### Fixed
- **Snapshots crop to the active image area.** Saved snapshots and clipboard
  copies now contain just the displayed image (the display window, clamped to the
  visible canvas) instead of the entire viewer canvas including the surrounding
  background. Side-by-side still captures the full canvas. (#52)

## [1.7.0] - 2026-06-19

### Added
- **Diff heat-map visualization controls.** The Diff/Matte view's fixed black-body
  ramp is now configurable: choose a colormap (black-body, grayscale, turbo,
  viridis, magma, inferno) or build a custom multi-stop gradient in the editor;
  pick the magnitude metric (max channel, Rec.709 luminance, or per-channel RGB);
  set a noise-floor threshold; and read the gain-to-colour mapping off a legend.
  The colormap is shared by the GPU and CPU paths through a reusable gradient
  module.
- **Customizable viewport background.** The transparency backdrop can now be a
  checkerboard (configurable cell colours and size), a solid colour, or a
  multi-stop gradient at any angle, set from **View ▸ Viewport Background**. Named
  presets are saved and persist across sessions, and the background composites
  consistently across the GPU, CPU, and OCIO paths.
- **Snapshot to clipboard.** Copy the current view to the system clipboard with
  **Cmd/Ctrl+Shift+S** (or **View ▸ Snapshot to Clipboard**). The capture is
  exactly what's on screen — background, compare mode, OCIO, and any annotations.
  An optional toggle also writes a timestamped PNG to `~/.floki/snapshots/`.
- **Annotation overlay.** Mark up the view before snapshotting with arrows, boxes,
  a freehand pen, and text labels, each with adjustable colour and stroke width.
  Annotations anchor to image pixels (so they track pan/zoom), support undo/redo
  and clear-all, and are flattened into the snapshot automatically.

All four features reproduce prior behaviour by default, so existing workflows are
unchanged.

## [1.6.0] - 2026-06-18

### Added
- **Drag-and-drop loading.** Drop an EXR onto the window to load it — the left
  half loads it as Image A, the right half as the reference Image B. While you
  drag, a live overlay highlights the half that will receive the drop. Dropping
  two files at once loads the first as A and the second as B. (Because the
  windowing layer discards the OS drop position, floki queries the system cursor
  directly so the left/right split works on macOS and Windows.)

### Fixed
- Wipe compare-mode controls now use consistent left-aligned labels across all
  four sliders (Center X, Center Y, Angle, Line Opacity); the previously
  unlabeled center slider is now named.

## [1.5.3] - 2026-06-17

### Fixed
- The OCIO CPU display transform (and CPU composite path) now show nothing
  rather than wrong colors when the transform fails: previously the
  untransformed scene-linear buffer was clamped to [0,1] and displayed, silently
  presenting incorrect color with no indication the transform never ran.

### Changed
- Internal: hardened a set of panic-prone `unwrap()`s across the GPU/CPU canvas
  render paths (side-by-side draws, egui paint/prepare callbacks, and GPU
  resource lookups) into clean early-returns. These crossed function-call
  boundaries where the upholding invariant wasn't locally visible. No user-facing
  behavior change in the normal case — the app degrades gracefully instead of
  crashing if an invariant is ever violated.

## [1.5.2] - 2026-06-17

### Fixed
- Changing the OCIO config no longer produces a wgpu validation error (or
  silent black frame) when the window has not been resized since the previous
  config load. The cached scene bind group is now invalidated whenever the OCIO
  pipeline is rebuilt, so it is always created against the current pipeline
  layout.
- Clicking **Browse** for a LUT, then immediately browsing a second path, no
  longer auto-enables the LUT for the second load. The auto-enable flag is now
  cleared when a superseded (stale) LUT result is discarded.
- If the GPU or render state is unavailable when a LUT finishes loading, the
  LUT is now correctly disabled (`Enable LUT` unchecked) rather than left
  enabled with mismatched domain bounds — which previously caused the shader to
  apply a non-identity coordinate remap while sampling the fallback identity
  texture.

## [1.5.1] - 2026-06-16

### Performance
- EXR files now decode on a background thread, so opening a large multi-layer
  render no longer freezes the window — a loading spinner shows while it loads.
  (The decode itself is unchanged; the UI just stays responsive during it.)

### Changed
- Internal: split the monolithic viewer `ui()` into focused units (contact
  sheet, pixel sampling, and the GPU/CPU canvas render paths), added a
  `criterion` benchmark harness for EXR load, and expanded API docs and tests.
  No user-facing behavior change.

## [1.5.0] - 2026-06-16

### Added
- **Color management ships by default.** Release binaries now statically bundle
  OpenColorIO (vendored OCIO 2.4.2), so OCIO color management works out of the
  box with no install or C++ toolchain on the user's machine. Previously OCIO
  was a manual opt-in build.
- Convenience build wrappers for OCIO: `cargo ocio-run` / `cargo ocio-build` /
  `cargo ocio-test` (cargo aliases, zero install) and a `justfile` (`just ocio`)
  that also inits the OCIO submodule. The self-contained vendored build is now
  the documented, recommended cross-platform path.

### Fixed
- Vendored OCIO build (`--features ocio-vendored`) now links on Windows machines
  that have vcpkg's user-wide MSBuild integration (`vcpkg integrate install`).
  That integration silently injected vcpkg's headers (including a different
  yaml-cpp ABI) into OCIO's Visual Studio build, producing `LNK2019`
  unresolved `__imp_*` symbols against OCIO's own statically built yaml-cpp.
  `build.rs` now builds OCIO hermetically (disables the vcpkg MSBuild hooks),
  so the vendored build is reproducible regardless of the host's vcpkg state.
- `cargo run` is no longer ambiguous against the `src/bin` helper binaries
  (`default-run = "floki"`).

## [1.4.4] - 2026-06-14

### Fixed
- A depth `Z` channel packed alongside an unprefixed `R,G,B,A` beauty pass no
  longer overwrites the Blue channel (it rendered pure white). Channel-to-slot
  resolution now prioritizes canonical color names (`R/G/B/A`) over geometric
  aliases (`X/Y/Z`), so a real Blue is never clobbered by depth.
- Leftover non-color channels (e.g. depth `Z`) in an RGBA group are now surfaced
  as their own grayscale layer instead of being silently dropped.

## [1.4.3] - 2026-06-14

### Performance
- Acquire the renderer read-lock once per frame in the GPU draw path instead of
  on every draw (2–4× per frame with overscan / Side-by-Side).
- Channel grouping on load is now O(n) in channel count (was O(n²)), which
  matters for Blender EXRs that pack 100+ channels into one part.
- Status-bar channel summary builds with an early length cap instead of joining
  every layer name and then truncating.

### Fixed
- Rebuild the LUT bind group on startup so a persisted **Enable LUT** actually
  applies instead of silently doing nothing until the file is re-browsed.
- Histogram cache now self-validates on `(active layer, log scale)`, fixing stale
  bins when toggling log scale and a missing Image B histogram after loading B.
- `.cube` parser rejects malformed and non-finite (`NaN`/`inf`) rows instead of
  silently dropping them or uploading garbage into the LUT texture.
- Disabled the inert OCIO **Browse** button and marked it "Coming soon".

## [1.4.2] - 2026-06-14

### Added
- Two-tier viewer toolbar, theme picker, and recent A/B file lists.

## [1.4.1] - 2026-06-12

### Added
- Rotatable wipe compare mode with adjustable center, angle, and line opacity.

## [1.4.0] - 2026-06-11

### Added
- Missing comparison shortcuts and an adjustable blink-speed control.

### Changed
- Bumped CI runners to Node.js 24.

## [1.3.1] - 2026-06-10

### Changed
- Renamed the project from "EXR Analyzer" to **Floki**.

## [1.3.0] - 2026-06-10

### Added
- Viewer quality-of-life features: fullscreen, unload, pixel sampling, tone
  reset, and compositing.

## [1.2.3] - 2026-06-07

- Maintenance release.

## [1.2.2] - 2026-06-06

### Added
- Floating tooltip toggle and color swatch.
- Overscan opacity slider.
- Nuke-style RGBA-colored text, HSVL readout, data-window dashed boxes, and a
  bottom status bar mirroring the A/B info.
- Swatch, HSVL, and layer name for Image B in the status bar.
- Mouse controls in the help menu.

### Changed
- Redesigned the status bar to match Nuke's layout with multiline A and B info.
- Left sidebar now scrolls so its content can't overflow the window.
- Overscan is hidden by default and indicated by the bounding box.

### Fixed
- Prevent an app crash when EXR block decompression panics.
- Switched EXR decompression off `zune-inflate` to fix load crashes.
- Status bar disappearing when Image B is loaded.
- Clamp the active layer when sampling Image B pixels to prevent data from
  disappearing.
- Missing status bar and Side-by-Side hover coordinates.
- Contact-sheet row alignment.
- Compiler warnings.

## [1.2.1] - 2026-06-05

### Added
- `convert_dir` CLI for headless batch EXR conversion.
- Group Blender single-part EXR channels into selectable passes.

### Fixed
- Contact-sheet clicks, wipe/diff in the contact sheet, and normalized
  Side-by-Side sizes.

## [1.2.0] - 2026-06-04

### Added
- `RUST_LOG` logging to the EXR converter.
- Converted/total summary (and failure count) shown when conversion finishes.

### Changed
- Parallelized and optimized viewer texture generation.
- Optimized EXR header conversion using fast chunk copying.

### Fixed
- EXR converter corrupting non-RGBA passes (P/N xyz swap).
- EXR converter UI freeze and non-monotonic progress.
- Converter cancel button and assorted bugs.

## [1.1.0] - 2026-06-03

### Added
- Multi-threaded EXR header converter tool.
- Dual contact sheets for Image B and an Image B info panel.
- Tooltip displaying values and diff for both A and B.

### Fixed
- GPU viewport squishing in the paint callback.
- GPU screen-size uniform for correct UV mapping.
- Wipe clip-rect bounds.
- `exr_data_b` reference in the tooltip.

## [1.0.1] - 2026-06-03

### Fixed
- Release workflow permissions and paths.

## [1.0.0] - 2026-06-03

Initial release.

### Added
- GPU-accelerated multi-layer OpenEXR viewer with A/B comparison.
- 3D LUT (`.cube`) color management.
- Channel isolation, alpha checkerboard, and contact sheet.
- Persistent color-sampler swatches and a pixel tooltip.
- Advanced metadata header inspector.
- Cross-platform GitHub Actions builds (Linux, Windows, macOS).

[Unreleased]: https://github.com/byvfx/floki/compare/v1.12.0...HEAD
[1.12.0]: https://github.com/byvfx/floki/compare/v1.11.0...v1.12.0
[1.11.0]: https://github.com/byvfx/floki/compare/v1.10.2...v1.11.0
[1.10.2]: https://github.com/byvfx/floki/compare/v1.10.0...v1.10.2
[1.10.0]: https://github.com/byvfx/floki/compare/v1.9.3...v1.10.0
[1.9.3]: https://github.com/byvfx/floki/compare/v1.9.2...v1.9.3
[1.9.2]: https://github.com/byvfx/floki/compare/v1.9.1...v1.9.2
[1.9.1]: https://github.com/byvfx/floki/compare/v1.9.0...v1.9.1
[1.9.0]: https://github.com/byvfx/floki/compare/v1.8.0...v1.9.0
[1.8.0]: https://github.com/byvfx/floki/compare/v1.7.2...v1.8.0
[1.7.2]: https://github.com/byvfx/floki/compare/v1.7.1...v1.7.2
[1.7.1]: https://github.com/byvfx/floki/compare/v1.7.0...v1.7.1
[1.7.0]: https://github.com/byvfx/floki/compare/v1.6.0...v1.7.0
[1.6.0]: https://github.com/byvfx/floki/compare/v1.5.3...v1.6.0
[1.5.3]: https://github.com/byvfx/floki/compare/v1.5.2...v1.5.3
[1.5.2]: https://github.com/byvfx/floki/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/byvfx/floki/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/byvfx/floki/compare/v1.4.4...v1.5.0
[1.4.4]: https://github.com/byvfx/floki/compare/v1.4.3...v1.4.4
[1.4.3]: https://github.com/byvfx/floki/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/byvfx/floki/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/byvfx/floki/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/byvfx/floki/compare/v1.3.1...v1.4.0
[1.3.1]: https://github.com/byvfx/floki/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/byvfx/floki/compare/v1.2.3...v1.3.0
[1.2.3]: https://github.com/byvfx/floki/compare/v1.2.2...v1.2.3
[1.2.2]: https://github.com/byvfx/floki/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/byvfx/floki/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/byvfx/floki/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/byvfx/floki/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/byvfx/floki/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/byvfx/floki/releases/tag/v1.0.0
