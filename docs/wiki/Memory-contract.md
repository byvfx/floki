The byte-budgeted ring is the playback cache. The full contract, including the retired T2 passages kept as history, is [`docs/playback/memory-contract.md`](https://github.com/byvfx/floki/blob/main/docs/playback/memory-contract.md). Read that file before changing `budget.rs`, `cache.rs`, or the sizing gate in `app.rs`. The arithmetic has been wrong four separate times (#215, #230, #232, #233).

A frame can be resident at more than one tier.

| Tier | What | Size (4K), from the contract | Where |
|---|---|---|---|
| **T0 Proxy** | Downsampled `ExrData`, `proxy: true`, f16 where the source is | 5–20 MB | RAM. Scrub preview and fallback paint. |
| **T1 CPU frame** | Full `ExrData`, all layers | 0.6–1.3 GB | RAM. The only sampling source. |
| **T2 GPU texture** | Retired in #299. The pre-upload ring had no consumer and built a full-frame texture every playback frame on the UI thread. Textures now live one-per-source in `ExrApp::comp_sources`, built on the upload worker pool (#202). | — | — |
| **T3 active** | The `ExrData` promoted into the active image | One T1 | What the renderer and the sampler see this frame. |

T1 frames are `Arc<ExrData>`, so a frame can be active and resident without cloning hundreds of megabytes.

## The RAM budget

`budget::t1_budget_bytes` is the single source of truth: a slice of *free* RAM (not total), with the ring's own residency added back so it does not chase its own tail, and the user's RAM setting as a ceiling.

```text
ram_budget = min((sys_total − sys_used + cache_bytes) × free_pct, user_budget)
t1_cap     = floor(ram_budget / sizing_bytes)
```

`budget::frames_in` is the only bytes-to-count conversion, so the byte bound and the frame count cannot drift. `FrameCache::evict_to` enforces both via `cache::Bound`, whichever binds first. `FrameCache::bytes()` measures real residency. The ring is heterogeneous: playback caches beauty-only or proxy frames, and a settle upgrade replaces one with a full frame at the same key, so `len × one frame` is the wrong size.

Both bounds are floored at two frames, so playback still double-buffers when the budget says fewer would fit. That floor is a deliberate override, which is why it applies in bytes too.

**Protected playheads outrank both bounds.** When only the frames on screen remain, `pick_victim` returns `None` and eviction stops over budget. That is the contract's degradation, not a leak.

`frame_cache_cap` is also the prefetch window (`prefetch_depth = min(cap - 1, span)`) and the read-behind reservation. Mis-sizing the cap mis-schedules decoding.

## Three rules

1. **Size on the same predicate the decode path uses.** `ExrApp::sizing_frame_bytes` calls `decode_proxy_target_for` / `decode_beauty_only_for` directly, on the clock source. A gate that drifts fills the ring with full frames under a cheap cap — the OOM direction. It drifted twice (#213 / #217 moved decode onto `displayed_aov` while sizing still asked `viewer.active_layer`).
2. **Every fallback ends at the dearest measured figure, never smaller, and never at `None`.** A too-small cap is wasteful and self-corrects. A too-large cap risks OOM. `None` makes `tick_budgets` skip its whole T1 branch and freeze the cap at the constructed default.
3. **What the pump asks for is only a prediction of what lands (#233).** A cheap decode can fall back, up to a full `load`. While the clock source's last decode fell back, sizing takes the dearest *measured* figure (`frame_bytes`, else `beauty_bytes`, else `proxy_bytes`) and clears when a cheap decode succeeds.

`cache_bytes` is a measurement (`FrameCache::bytes()`). `sizing_bytes` is a latch for a frame that does not exist yet, chosen proxy over beauty over full. They answer different questions. Putting `len × sizing_bytes` on both sides was #230.

## Eviction and sampling

Linear play evicts opposite the play direction. In Loop, "behind" wraps the in/out span (#140). About 25% of the prefetch depth is reserved for frames just shown (`scheduler::read_behind`, #169), ranked by plain backward distance so play-then-step-back is a hit. The scheduler fetches that window at P3, and the pump's forward depth shrinks by the reservation. Scrub weights by absolute distance from the playhead.

> When the clock is not advancing (paused, stopped, or scrubbing settled), the frame at `current_frame` is resident at T1 and promoted to T3, so `sample_pixel` has pixel-accurate CPU data. While the clock is advancing, the readout comes from the T0 proxy or is suppressed. Entering a stopped state re-decodes the playhead if it was evicted before sampling is enabled again.

A blocking beat on pause is acceptable. A wrong sample is not.

The 1 Hz `floki::playback` trace carries `t1=`, `size_bytes=`, `t1_bytes=<resident>/<budget>`, `evict=`, and `fallback=`. Frame-time percentiles are recorded only while `PlayState::Playing` (#236). An interval measured while the clock is stopped is thinking time, not a hitch.

wgpu can abort the process on OOM. Stay under the budget before the upload. Don't plan on catching the failure. The worker side of this budget is the [concurrency contract](Concurrency-contract).
