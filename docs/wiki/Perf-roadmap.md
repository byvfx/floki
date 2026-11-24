Heavy-4K review playback stalls because of memory footprint, not decode throughput. The full strategy, the OpenRV / DJV / mrv2 / xStudio survey, and the ranked backlog are [`docs/perf-roadmap.md`](https://github.com/byvfx/floki/blob/main/docs/perf-roadmap.md). The figures below are that document's, from the 2026-07 field survey and the benches it cites.

The motivating shape: about **178 MB/frame**, about **220 ms** to decode, and only about **16 frames in 8 GB**. A review range larger than that cannot stay cached, so playback falls back to decode-on-demand and thrashes. Adding decode workers does not help. One `ExrData::load` already saturates the cores through the patched `exr` crate. See [Concurrency contract](Concurrency-contract).

The levers that move playback:

1. Shrink the per-frame footprint so the range fits in RAM.
2. Pipeline the I/O so the read does not block the decode cores.
3. Amortize the decode across sessions so a repeat pass pays nothing.

## What shipped in that wave

| Change | What it did |
|---|---|
| [#160](https://github.com/byvfx/floki/pull/160) / #144 | Contact-sheet thumbnail re-bake: freeze during scrub, refresh on settle. |
| [#161](https://github.com/byvfx/floki/pull/161) / #151 | One owner for viewer prefs. Removed a duplicate-state stutter. |
| [#162](https://github.com/byvfx/floki/pull/162) / #98 phase 1 | Locked-step engine slice: B slaved to A, both play in wipe and compare. Equal-offset locked step on the layer model is still [#98](https://github.com/byvfx/floki/issues/98). See [Layer model](Layer-model). |
| [#163](https://github.com/byvfx/floki/pull/163) / #94 phase 1 | In-RAM scrub proxies. `downsampled()` runs after a full decode and keeps the data window and display window. |
| [#174](https://github.com/byvfx/floki/pull/174) / #170 | f16 proxies. The box filter stores at the source depth, so an f16 source is not promoted to f32. |
| [#175](https://github.com/byvfx/floki/pull/175) / #169 | Read-behind window, about 25% of the prefetch depth. |
| [#176](https://github.com/byvfx/floki/pull/176) / #164 | mmap decode plus a page-cache warm-ahead thread (`src/prefetch.rs`). |
| [#177](https://github.com/byvfx/floki/pull/177) / #165 | On-disk proxy cache (`src/proxy_cache.rs`), LRU-evicted to a GB budget. Idle precache default flipped on for fresh installs. |

The first proxy attempt used a reduced-resolution EXR read and flattened display-window geometry. Downsample the fully decoded frame instead. That is the rule for later footprint work.

## What the benches decided

A 2026-07-14 bench of `load` / `load_beauty` / `load_proxy` against an eager OpenEXR 3.4 reference, on 24 real renders, warm cache, local disk:

- `load_beauty` was 4–41 ms and `load_proxy` was 8–56 ms. Full `load` scales with part count. On the 30-part redSea set, beauty isolation was **28.8×**.
- On multi-part files the fork was competitive or faster than that OpenEXR reference (the reference reads parts sequentially; Floki's cross-part rayon pool is a real edge).
- The gap that showed up was single-part DWA (about 2.6–2.9×) and PIZ (about 2.5–2.7×), and a later three-machine study found it is Apple-Silicon-broad, not DWA-only. On an i9-13980HX every codec closed to 1.05–1.59× OpenEXR. The DWA f16 SIMD-batch fix (fork `a368469`, #184) measured about 4–9% on DWA. A NEON IDCT tier was left unbuilt.

The verdict in the roadmap: do not rewrite the decoder. A rewrite drops the miniz panic-safety fix, adds a C++ dependency, and a three-platform static build, to lose on the common multi-part case. The study is [`docs/perf/dwa-arm-decode-investigation.md`](https://github.com/byvfx/floki/blob/main/docs/perf/dwa-arm-decode-investigation.md), which is not on this site's nav.

## Still ranked

1. **Faster first-pass decode — [#33](https://github.com/byvfx/floki/issues/33).** The one throughput lever. Shorten the first touch. Later passes are hidden by prefetch and the disk cache. Lazy per-layer decode only really pays on multi-part files.
2. **B-side T2 GPU ring — [#166](https://github.com/byvfx/floki/issues/166) (#98 phase 2).** The roadmap's text: B uploaded a texture per frame while A rendered from the pre-uploaded T2 ring, so locked-step B stuttered on heavy footage. The proposed fix there is a second T2 ring, a two-slot VRAM split, and the ±offset transport control. The T2 ring was later removed in #299. See [Memory contract](Memory-contract).
3. **Color-depth cache packing — [#167](https://github.com/byvfx/floki/issues/167).** Optional, after the I/O and disk-cache work. Packing linear float straight to 8-bit bands; a pre-cache transform has to be baked in, which is the OpenRV lesson the roadmap records. The f16 proxy slice of this already shipped as #170.

Smaller follow-ons still open in the same doc: a decode-buffer reuse pool (#171) and clearer cache and drop-frame feedback (#172). An adaptive throttle between Stutter and Drop-frames is noted as future, not built.

How to measure a change against DJV and Chaos Player is the [cross-player benchmark](Cross-player-benchmark).
