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
would just contend for the same cores (this is exactly what OpenRV concluded in
[AcademySoftwareFoundation/OpenRV#404](https://github.com/AcademySoftwareFoundation/OpenRV/issues/404)).

**The levers that actually move playback are footprint and pipelining, not raw
decode threads:**

1. **Shrink the per-frame footprint** so the range fits RAM and plays *from* RAM
   (resolution and/or bit-depth reduction).
2. **Pipeline the I/O** so the read never blocks the decode cores.
3. **Amortize the decode across sessions** so a repeat pass pays nothing.

---

## What OpenRV does (the reference model)

OpenRV solves the same problem this way:

- **Shrink footprint to fit RAM, then play from RAM.** Resolution reduction is a
  **post-decode resample on the way into the cache** — *not* a fast reduced-res
  read. It also reduces **bit-depth** of cache entries (e.g. 32f → 8-bit) to fit
  roughly twice as many frames.
- **Separate reader threads from decoder threads** and pipeline them, so file I/O
  overlaps decode instead of serializing with it.
- **Region cache vs look-ahead cache**: a bounded resident region the user is
  reviewing, distinct from a look-ahead prefetch window.

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

Source pointers: OpenRV on
[DeepWiki](https://deepwiki.com/AcademySoftwareFoundation/OpenRV), the RV
manual's caching chapter, issue
[#404](https://github.com/AcademySoftwareFoundation/OpenRV/issues/404) (decode
thread contention), and its PBO-based texture-upload path (floki already does the
equivalent with wgpu — see the #142 upload-path batch, so GPU upload is *not* a
current bottleneck).

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
*Lever: pipelining.* Overlap the file **read** with **decode** via a byte-prefetch
ring filled by a dedicated prefetch thread + a decode-from-bytes path (`exr`
`from_buffered`), fed by the existing pump want-list. Epoch-agnostic, so the #98
pump/supersession logic is untouched. Biggest **general** win and the single best
lever for **networked / slow-storage** media. Modest on local heavy footage
(~15–20% faster fill); large where reads are slow. mmap+`madvise` noted as an
alternative to prototype.

### 2. Persistent on-disk proxy cache — [#165](https://github.com/byvfx/floki/issues/165)
*Lever: amortize decode.* Persist the downsampled proxies to disk (keyed
path+mtime+size+proxy-px) so the first-touch decode is paid **once, ever** — a
repeat pass or a later session loads proxies from disk instead of re-decoding.
Extracts the on-disk-cache half of #94 now that Phase 1 (#163) shipped; #94 can
close once this lands. Huge for networked media and repeated review (dailies, shot
iteration).

### 3. Faster first-pass decode — [#33](https://github.com/byvfx/floki/issues/33)
*Lever: throughput (the one place it helps).* Speed up `ExrData::load` itself:
lazy per-layer/channel decode (only decode what's shown), confirm the parallel
decompression is fully engaged, proxy first-paint. This is the only decode-speed
lever that pays off — it shortens the *first* touch of every frame, which the
prefetch and on-disk-cache levers then hide or eliminate on subsequent passes.
*(Pre-existing issue — linked, not re-filed.)*

### 4. B-side T2 GPU ring — [#166](https://github.com/byvfx/floki/issues/166) (#98 Phase 2)
*Lever: footprint/pipelining on the GPU side.* B currently uploads a texture
per-frame (`build_layer_texture`) while A renders from the pre-uploaded T2 VRAM
ring, so locked-step B stutters on heavy footage. Add a second T2 ring for slot B
+ a two-slot VRAM budget split + the ±offset transport control. Refs #98/#162.

### 5. Color-depth cache packing — [#167](https://github.com/byvfx/floki/issues/167)
*Lever: footprint (second axis).* Pack T1 cache entries at reduced bit-depth
(floki already caches at f16; explore f16 → 8-bit/packed-half behind a quality
toggle) → ~2× more frames in the same RAM, multiplying with #94 resolution
proxies. Opt-in and quality-sensitive; lowest priority — ship after the I/O and
on-disk-cache work.

---

## How the levers compose

| Pass | Without roadmap | With it |
|------|-----------------|---------|
| First touch of a frame | full decode (~220 ms) blocks the pipeline | prefetch (#164) hides the read; #33 shortens the decode |
| Fitting a large range in RAM | ~16 full frames only | #94 (resolution) × #167 (bit-depth) fit many more |
| Repeat pass / new session | full decode again | #165 loads proxies from disk — no decode |
| Locked-step A/B | B stutters (per-frame upload) | #166 gives B its own VRAM ring |

Resolution proxies (#94, shipped) and bit-depth packing (#167) are **independent
footprint axes on the same RAM budget** and combine multiplicatively. Prefetch
(#164) and faster decode (#33) are **independent throughput/pipeline axes** and
compound. The on-disk cache (#165) removes the decode entirely on repeat passes,
which is where review time is actually spent.
