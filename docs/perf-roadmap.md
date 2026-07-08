# Floki Playback Performance Roadmap

The single source that the playback-performance issues reference. Companion to the
codebase audits ([audit-2026-06](audit-2026-06.md), [audit-2026-07](audit-2026-07.md));
those cover general health, this one is specifically the **heavy-footage review
playback** strategy and the ordered backlog behind it.

---

## The core insight: floki is footprint-bound, not throughput-bound

Heavy-4K review playback stalls because of **memory footprint**, not decode
throughput:

- ~**178 MB/frame**, ~**220 ms** to decode, and only ~**16 frames fit in 8 GB**.
- A review range larger than ~16 frames therefore **can never cache**, so playback
  falls back to decode-on-demand and **thrashes** — worst in locked-step A/B
  (#98), which needs two windows resident at once.

The tempting fix — "add more decode workers" — **does not help**. floki's `exr`
decode is already internally parallel (rayon, via the `byvfx/exrs` `miniz-inflate`
patch), so a **single frame already saturates the cores**. More decode workers
would just contend for the same cores. OpenRV's config agrees: it defaults the
EXR decoder pool to `cores − 1` (`-exrcpus`, `src/bin/apps/rv/main.cpp`), i.e. one
decode already owns the machine, and its manual warns total threads should stay
below logical cores.

> **Citation correction (2026-07):** an earlier revision cited
> [OpenRV#404](https://github.com/AcademySoftwareFoundation/OpenRV/issues/404) as
> "more EXR decode workers contend for cores". #404 is actually about **ProRes**:
> a seek-limiting bug (FFmpeg reports `gop_size=12` even for intra-frame codecs)
> made every reader thread linearly re-decode frames — "with 8 reader threads,
> each frame ended up being decoded 8×" — fixed in
> [PR #405](https://github.com/AcademySoftwareFoundation/OpenRV/pull/405). It says
> nothing about EXR core saturation.

Worth knowing: the tlRender family takes the **opposite** approach — it sets
`Imf::setGlobalThreadCount(0)` ("we multithread frames") and decodes N frames
concurrently (xStudio: 8 reader actors per EXR source). Both models saturate the
cores; frame-parallelism also hides file I/O behind decode for free. floki keeps
intra-frame parallelism (lowest latency for the frame the user is waiting on) and
covers the I/O bubble explicitly with the prefetch pipeline (#164).

**The levers that actually move playback are footprint and pipelining, not raw
decode threads:**

1. **Shrink the per-frame footprint** so the range fits RAM and plays *from* RAM
   (resolution and/or bit-depth reduction).
2. **Pipeline the I/O** so the read never blocks the decode cores.
3. **Amortize the decode across sessions** so a repeat pass pays nothing.

---

## What OpenRV does (the reference model)

OpenRV solves the same problem this way (all verified in source, 2026-07):

- **Shrink footprint to fit RAM, then play from RAM.** Resolution reduction is a
  **post-decode resample on the way into the cache** — *not* a fast reduced-res
  read. The node order is hardwired `FileSource → cacheLUT → Format → Cache`
  (`SourceGroupIPNode.cpp`); `FormatIPNode` software-resizes the decoded
  framebuffer (`-s`, `-resampleMethod`, default "area"). No reduced-res EXR read
  path exists in RV at all.
- **Reduce bit-depth of cache entries** (`-maxbits 8/16/32`, `-nofloat`) to fit
  more frames. Crucially, a **LUT stage sits before the cache**
  (`CacheLUTIPNode`), so 8-bit entries can be display/log-referred instead of
  banding in linear — #167 must copy this.
- **Frame-level read parallelism + mmap I/O**, *not* a reader→decoder pipeline.
  `-rthreads` eval threads (default `min(cores/4, 4)`) each read+decode whole
  frames via cloned reader instances; intra-frame parallelism is the format
  library's own pool (`-exrcpus`, default `cores − 1`). I/O–decode overlap comes
  from per-format I/O methods — **EXR defaults to memory-mapped I/O on all
  platforms** ("Based on performance tuning", `Options.cpp`), with buffered /
  unbuffered / async variants selectable.
- **Region cache vs look-ahead cache**: `IPGraph::GreedyCache` vs `BufferCache`,
  two explicit branches in `FBCache::utility()`. The look-ahead cache reserves
  **25% for frames behind the playhead** (`-lookback`), applied to audio too.
- **Utility-function eviction**: cache/free by weighted distance from the display
  frame, direction-aware, with modulo wrap-around distances for looping — the
  same shape as floki's `pick_victim`.
- **Pacing is a user choice**: realtime mode drops frames against the clock
  (skips marked with red circles on the timeline); play-all-frames mode retimes
  instead. Look-ahead mode adds a bounded "buffer wait" (default 5 s) that pauses
  until the cache refills — floki's Stutter/DropFrames pair matches this model.

### The one hard-won lesson (don't repeat it)

floki's first proxy attempt used a **fast reduced-res read** (fewer scanline
blocks), which **flattened the EXR data-window / display-window geometry** the
viewport positions by (`src/viewer.rs` viewport geometry reads `display_window` +
layer `layer_position` + `logical_size`). The result was a mis-positioned "empty
quadrant" on tight-data-window renders.

**Downsample the fully-decoded frame instead** — the full decode gets geometry
right for free. `ExrData::load_proxy` = `load_beauty` then
`ExrData::downsampled(max_dim)`, which box-filters down while **preserving**
`display_window` + `layer_position` and stashing the full size as `display_size`
so `logical_size` frames full-res while the small texture upscales. This is the
model all future footprint work must follow.

Source pointers: RV user manual chapters
[3](https://aswf-openrv.readthedocs.io/en/latest/rv-manuals/rv-user-manual/rv-user-manual-chapter-three.html) (CLI),
[4](https://aswf-openrv.readthedocs.io/en/latest/rv-manuals/rv-user-manual/rv-user-manual-chapter-four.html) (caching modes) and
[14](https://aswf-openrv.readthedocs.io/en/latest/rv-manuals/rv-user-manual/rv-user-manual-chapter-fourteen.html) (performance);
in the repo: `FBCache.cpp` (utility eviction), `FormatIPNode.cpp` (pre-cache
resize/bit-depth), `SourceGroupIPNode.cpp` (node order), `Options.cpp` (I/O-method
defaults). RV also pre-uploads the *next* display frame's textures while the
current one draws (`prefetch=1`, at the cost of 2× VRAM) — precedent for #166 —
and its PBO upload path is the GL equivalent of what floki already does with wgpu
(#142 batch, so GPU upload is *not* a current bottleneck).

---

## Field survey (2026-07): DJV/tlRender, mrv2, xStudio

Source-level survey of the rest of the open-source field, run alongside the
OpenRV verification above. What each adds beyond the RV model:

**tlRender / DJV (all generations)** — GB budget converted to a frame window via
bytes-per-frame of the actual media; **direction-aware, loop-aware cache window
with an explicit read-behind** (tlRender `readBehind = 0.5 s`; DJV 2.x hardcodes
10 frames behind; classic DJV 1.x had cache-side **proxy scale 1/2–1/8 + force
8-bit ingest** — the direct precedent for #94 × #167). EXR reads go through
memory-mapped `Imf::IStream`s with `Imf::setGlobalThreadCount(0)` and per-frame
parallelism (default `cores/2` in-flight decodes). Pacing: free-running clock,
frames drop on display lag, but a **cache miss rewinds the clock** (stall) with a
500 ms audio-mute timeout. DJV 2.x splits the cache budget evenly across open
files (precedent for #166's two-slot split). Two-tone timeline cache bar
(planned range vs actually-cached frames).

**mrv2** — tlRender fork. Flushes the *entire* cache on layer switch (floki's
all-layers T1 is strictly better here). Has the field's only true reduced-res
EXR read: **mip/ripmap level reads for tiled EXRs** — geometry-safe by
construction since the display window is constant across levels (rare in render
output, though; mipped EXRs are a texture-pipeline artifact). Auto-disables OCIO
when no input colorspace is set, as a playback-speed fix. Documented heavy-EXR
workflow: "wait for the cache bar to fill, then play."

**xStudio** — actor-model pipeline, the most sophisticated of the set:
- **Next-needed-timestamp eviction**: playheads continually tell the cache when
  each buffer will next be displayed; past-timestamp buffers evict first (vs
  floki/RV distance heuristics).
- **Vsync-predictive presentation**: predicts the playhead position *at the next
  display refresh* (measured refresh rate, proper pulldown math) and shows the
  nearest-behind frame; late frames are skipped, never waited for.
- **Adaptive throttle**: frames arriving > 2 periods late multiply playback
  velocity by 0.8 (floor 0.1) until decode catches up — graceful middle ground
  between floki's Stutter and DropFrames.
- **`ImageBufferRecyclerCache`**: 0.5 GB bin of freed aligned allocations reused
  for new frames — no allocator churn in the hot loop (RV has the same idea via a
  framebuffer trash list).
- Caches **file-native buffers** (half-float EXR, YUV) and lets per-reader GLSL
  unpack on the GPU; colour LUT/shader data is cached and **prefetched alongside
  frames** (CPU OCIO only for thumbnails) — design input for floki's OCIO work.
- Idle full-timeline precache 500 ms after the playhead stops; failed reads are
  cached as error buffers so a bad frame is never re-decoded in a loop.

**Where floki already leads the field**: budget sizing off *free* RAM (everyone
else uses a fixed GB pref; mrv2 only clamps to RAM/2); the directional
loop-modulo eviction in `pick_victim` (parity with RV's utility function, ahead
of tlRender's erase-outside-window); epoch supersession with the worker-side
pre-decode skip (tlRender can only cancel still-queued requests); all-layers T1
(mrv2 flushes on layer switch).

**Confirmed differentiator**: **no open-source player persists decoded frames to
disk** — RV, DJV, tlRender, mrv2 and xStudio are all RAM-only. The closest prior
art is manual pre-rendering (RVIO remasters, Nuke proxy files, OIIO `maketx`
`.tx` textures). #165 has no direct competition.

---

## Shipped (this wave)

| PR | Issue | What it did |
|----|-------|-------------|
| [#160](https://github.com/byvfx/floki/pull/160) | [#144](https://github.com/byvfx/floki/issues/144) | Contact-sheet thumbnail re-bake perf — freeze during scrub, refresh on settle, amortized bakes |
| [#161](https://github.com/byvfx/floki/pull/161) | [#151](https://github.com/byvfx/floki/issues/151) | Single-owner viewer prefs (removed the duplicate-state stutter source) |
| [#162](https://github.com/byvfx/floki/pull/162) | [#98](https://github.com/byvfx/floki/issues/98) Ph.1 | Locked-step A/B playback engine — B slaved to A, both play in wipe/compare |
| [#163](https://github.com/byvfx/floki/pull/163) | [#94](https://github.com/byvfx/floki/issues/94) Ph.1 | In-RAM scrub proxies — geometry-preserving post-decode `downsampled()`, so the range fits RAM and replays smoothly |

The proxies (#94/#163) are the footprint lever #1 in action; the rest of this
roadmap is the remaining levers.

---

## Backlog (ranked)

Ordered highest-payoff first. Each is a tracked issue; this doc is the context
those issues share.

### 1. I/O prefetch pipeline — [#164](https://github.com/byvfx/floki/issues/164)
*Lever: pipelining.* Overlap the file **read** with **decode**, fed by the
existing pump want-list. Epoch-agnostic, so the #98 pump/supersession logic is
untouched. Biggest **general** win and the single best lever for **networked /
slow-storage** media. Modest on local heavy footage (~15–20% faster fill); large
where reads are slow. Two candidate designs, and the field survey **promotes
mmap to the primary candidate**: OpenRV defaults EXR I/O to memory-mapped on all
platforms ("Based on performance tuning"), and tlRender/mrv2 read EXR through
mmap'd `Imf::IStream`s. Fallback/alternative: a byte-prefetch ring filled by a
dedicated prefetch thread + a decode-from-bytes path (`exr` `from_buffered`).

### 2. Prefetch read-behind window — [#169](https://github.com/byvfx/floki/issues/169)
*Lever: pipelining (the other direction).* The scheduler (`src/scheduler.rs`)
only ever walks **ahead** in the play direction, and `pick_victim` evicts
behind-frames *first* during play — so play-then-step-back, the most common
review gesture there is, is a guaranteed cache miss on exactly the frame the
user just watched. Every surveyed player keeps a behind window: OpenRV reserves
**25% of the look-ahead cache** for frames behind the playhead (`-lookback`),
tlRender keeps `readBehind = 0.5 s`, xStudio starts its prefetch 0.5 s behind
the playhead ("in case the user stops playback and steps back a couple of
frames"). Small scheduler + eviction-rank change, outsized review-feel payoff.
Companion to #164.

### 3. Persistent on-disk proxy cache — [#165](https://github.com/byvfx/floki/issues/165)
*Lever: amortize decode.* Persist the downsampled proxies to disk (keyed
path+mtime+size+proxy-px) so the first-touch decode is paid **once, ever** — a
repeat pass or a later session loads proxies from disk instead of re-decoding.
Extracts the on-disk-cache half of #94 now that Phase 1 (#163) shipped; #94 can
close once this lands. Huge for networked media and repeated review (dailies, shot
iteration). **No open-source player has this** (field survey above) — RV, DJV,
mrv2, xStudio are all RAM-only; the prior art is manual pre-rendering (RVIO,
Nuke proxy files, OIIO `.tx`). **Format: raw f16 dumps, mmap-able.** At proxy
resolution the bandwidth is trivial (1024-wide RGBA f16 ≈ 4.4 MB/frame ≈
106 MB/s @ 24 fps), and decode cost is zero. Optional zstd for network shares
(filtered zstd: ~2.3× ratio at ~20× faster decode than EXR/ZIP, per Aras
Pranckevičius 2025). Avoid DWA for the cache itself — compact but the slowest
decode of the options (~60 ms @ 8K with 16 threads, OpenEXR #1755).

### 4. Faster first-pass decode — [#33](https://github.com/byvfx/floki/issues/33)
*Lever: throughput (the one place it helps).* Speed up `ExrData::load` itself:
lazy per-layer/channel decode (only decode what's shown), confirm the parallel
decompression is fully engaged, proxy first-paint. This is the only decode-speed
lever that pays off — it shortens the *first* touch of every frame, which the
prefetch and on-disk-cache levers then hide or eliminate on subsequent passes.
*(Pre-existing issue — linked, not re-filed.)* Constraints from the 2026-07
survey: **(a)** lazy per-layer decode only truly pays for **multi-part** files —
in single-part multi-layer EXRs all channels of a chunk decompress together and
only the unpack/copy stage is skippable, which `load_beauty` already exploits;
**(b)** the `exrs` maintainer identifies **f32↔f16 channel conversion** as the
crate's bottleneck, not inflate; **(c)** `exrs` v1.74.1 (2026-07) added **SIMD
DWA decode** — check what the `byvfx/exrs` fork is based on, since before this
the crate couldn't decode DWAA/DWAB at all; **(d)** no published exrs-vs-OpenEXR
benchmark exists — bench the fork against `openexr-sys`/OpenEXRCore on our own
corpus before any rewrite (OpenEXR 3.3's Core-backed rewrite initially
*regressed* DWA decode 12–17%, OpenEXR #1915 — Core is not automatically faster).

### 5. B-side T2 GPU ring — [#166](https://github.com/byvfx/floki/issues/166) (#98 Phase 2)
*Lever: footprint/pipelining on the GPU side.* B currently uploads a texture
per-frame (`build_layer_texture`) while A renders from the pre-uploaded T2 VRAM
ring, so locked-step B stutters on heavy footage. Add a second T2 ring for slot B
+ a two-slot VRAM budget split + the ±offset transport control. Refs #98/#162.
Precedents: DJV 2.x divides its cache budget evenly across open files; tlRender
counts all A/B-compare timelines into its bytes-per-frame; xStudio jitters
per-playhead prefetch refresh so compared sources don't re-request in lockstep.

### 6. Color-depth cache packing — [#167](https://github.com/byvfx/floki/issues/167)
*Lever: footprint (second axis).* Pack T1 cache entries at reduced bit-depth
(floki already caches at f16; explore f16 → 8-bit/packed-half behind a quality
toggle) → ~2× more frames in the same RAM, multiplying with #94 resolution
proxies. Opt-in and quality-sensitive; lowest priority — ship after the I/O and
on-disk-cache work. **RV lesson**: RV's LUT stage sits *before* its cache
(`CacheLUTIPNode`), so 8-bit entries are display/log-referred — packing linear
float straight to 8-bit bands; the 8-bit tier needs a pre-cache transform baked
in. Classic DJV 1.x shipped exactly this combo (proxy scale × force-8-bit
ingest). Note the first slice — f16 proxy packing — is extracted as its own
quick fix below.

---

## Quick wins & small items (2026-07 survey)

Smaller than the ranked levers, filed as their own issues:

- **f16 proxy packing — [#170](https://github.com/byvfx/floki/issues/170).** `ExrData::downsampled()` emits **F32**
  buffers (`exr_loader.rs`), so proxies from f16 sources are 2× bigger than
  needed *and* defeat the T2 fast path — `build_layer_texture` only picks
  `Rgba16Float` when all channels are F16, so F32 proxies upload as
  `Rgba32Float` at 2× VRAM + bandwidth. Accumulate the box filter in f32, store
  f16: proxy RAM halves and the f16 upload path comes back. Effectively the
  first slice of #167, nearly free.
- **Decode-buffer reuse pool — [#171](https://github.com/byvfx/floki/issues/171).** floki allocates a fresh buffer per
  decode (~178 MB full-res, or proxy-size); at playback rates the allocator
  churn + page-zeroing is real hot-loop cost. xStudio keeps a 0.5 GB
  `ImageBufferRecyclerCache` of freed aligned allocations; RV recycles freed
  framebuffers via a trash list. Proxies are uniform-size, so a pool is trivial
  there. (T2 already does this with the reused `t2_staging` buffer.)
- **Cache & drop visibility — [#172](https://github.com/byvfx/floki/issues/172).** Every surveyed player ships a
  timeline cache bar (RV's green/blue stripe, DJV's two-tone planned-vs-cached,
  mrv2, xStudio) and drop feedback (RV marks skipped frames with red circles,
  DJV 1.x showed *achieved* FPS, mrv2/DJV3 show dropped-frame HUDs). The
  documented heavy-EXR workflow everywhere is "wait for the bar, then play" —
  floki's cache machinery needs to be legible for users to trust it.
- **Idle precache default-on when proxying.** `tick_precache` exists but
  defaults off; DJV 1.x idle-preloaded by default and xStudio starts
  whole-timeline background caching 500 ms after the playhead stops. Proxies
  (#94) are exactly what makes this affordable. (Fold into the cache-visibility
  issue or flip the default with #165.)
- **Adaptive-throttle pacing (future).** xStudio's middle ground between
  Stutter and DropFrames: frames > 2 periods late multiply velocity by 0.8 until
  decode catches up, restoring after. Revisit once the levers above land.

---

## How the levers compose

| Pass | Without roadmap | With it |
|------|-----------------|---------|
| First touch of a frame | full decode (~220 ms) blocks the pipeline | prefetch (#164) hides the read; #33 shortens the decode |
| Fitting a large range in RAM | ~16 full frames only | #94 (resolution) × #167 (bit-depth) fit many more |
| Play, stop, step back | behind-frames evicted first → guaranteed miss | #169 read-behind window keeps the just-watched frames resident |
| Repeat pass / new session | full decode again | #165 loads proxies from disk — no decode |
| Locked-step A/B | B stutters (per-frame upload) | #166 gives B its own VRAM ring |

Resolution proxies (#94, shipped) and bit-depth packing (#167) are **independent
footprint axes on the same RAM budget** and combine multiplicatively. Prefetch
(#164) and faster decode (#33) are **independent throughput/pipeline axes** and
compound. The on-disk cache (#165) removes the decode entirely on repeat passes,
which is where review time is actually spent.
