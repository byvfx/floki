# #56 — Byte-budgeted memory contract (the ring cache)

> Status: design contract. Implemented incrementally (Phase 0 accounting/budget math → Phase 3 T1
> ring → Phase 4 T2 pre-upload). See [README](README.md) for the thread-boundary fact and tier table.

The ring cache holds decoded frames across the CPU/GPU thread boundary under two independent byte
budgets, so playback never exceeds available RAM or VRAM and **degrades rather than crashes** under
pressure.

## Four tiers per frame

A frame may be resident at several tiers at once.

| Tier | What | Size (4K) | Producer | Lives in | Purpose |
|------|------|-----------|----------|----------|---------|
| **T0 Proxy** | `ProxyImage` low-res RGBA32F (`proxy.rs`) | 5–20 MB | worker (`from_exr_fast_read`) | CPU | scrub preview / fallback paint |
| **T1 CPU frame** | full `ExrData`, ALL layers (`exr_loader.rs`) | 0.6–1.3 GB | worker (`ExrData::load`) | RAM | **only sampling source** + upload source for T2 |
| **T2 GPU texture** | `Rgba32Float`, **active layer only** | ~134 MB | UI thread (`generate_gpu_texture`) | VRAM | instant paint on `swap_image_data` |
| **T3 active** | the `ExrData` promoted into `self.exr_data` | (== one T1) | UI thread (swap) | — | what renderer + sampler see this frame |

T1 frames are held as `Arc<ExrData>` so a frame can be both **active (T3)** and **resident (T1)**
without cloning ~600 MB (this is why Phase 0 moves `exr_data` to `Option<Arc<ExrData>>`).

## Two independent budgets

They bind different tiers from different sources.

### VRAM budget — binds T2
`ResourceMonitor` already reads `recommendedMaxWorkingSetSize` / `currentAllocatedSize`
(`resource_monitor.rs`, Metal only).

```
budget       = recommendedMaxWorkingSetSize × headroom − baseline_vram
per_frame_t2 = w × h × 16            # Rgba32Float, 16 bytes/pixel
max_t2       = floor(budget / per_frame_t2)
```

Off-Metal, `recommendedMaxWorkingSetSize` is `None` → use a conservative fixed/config cap.

### CPU RAM budget — binds T0 + T1
From `sysinfo` `sys_total` / `sys_used` in `Sample`. Requires a new **`ExrData::approx_bytes()`**
(sum of physical channel buffers × sample size) — no per-frame accounting exists today.

```
ram_budget = sys_total × headroom − sys_used
max_t1     = floor(ram_budget / measured_t1)   # measure measured_t1 once on the first real frame;
                                               # sequences are homogeneous
```

## Windows differ — T1 vs T2

- **T1 window** = the decode-ahead horizon ahead of the playhead (RAM-budgeted).
- **T2 window** = a smaller texture ring around the playhead (VRAM-budgeted).

A T1 frame **behind** the playhead whose T2 is already built is evicted first (its pixels are in
VRAM; only the *active* frame needs CPU for sampling).

### Eviction = directional-ring + LRU tiebreak
- **Linear play:** evict opposite the play direction.
- **Scrub (random):** weight by absolute distance from the playhead (bidirectional).

## INV-SAMPLE — the single invariant everything protects

> When the clock is **not advancing** (Paused / Stopped / Scrubbing-settled), the frame at
> `current_frame` is resident at **T1** and promoted to **T3** (`self.exr_data == Some(it)`), so
> `sample_pixel` always has pixel-accurate CPU data.
>
> While the clock **is advancing**, the readout is served from **T0 (proxy)** or suppressed.
>
> On transition to a non-advancing state, the system must **ensure T1 residency for
> `current_frame`** (re-decode if evicted — a blocking beat is acceptable on pause) before
> re-enabling sampling.

This is what guarantees the pixel-probe / color-picker is never wrong when the user stops to inspect
a frame, regardless of what the cache evicted during playback.

## Multi-layer

A frame may have 100 layers. **T2 caches only the active layer**; **T1 holds all layers** (the
sampler and the layer-switcher need them). A **layer switch mid-sequence invalidates the whole T2
ring** (like `invalidate_active_textures`, `viewer.rs`): rebuild the playhead's T2 first, re-prime
the rest lazily; **T1 is untouched**.

## Failure modes — degrade, never crash

| Condition | Behavior |
|-----------|----------|
| `budget < 1 × T2` | Disable pre-upload; decode-on-demand (today's behavior, clock-driven); surface "insufficient VRAM, X fps". |
| `budget < 1 × T1` | Refuse sequence mode; show single frame + reason. |
| 8K / huge frames | Windows collapse to 1–2; stutter, not crash. |
| Live VRAM pressure | Recompute the budget each second; shrink the T2 ring **before** the next upload. |

> **wgpu can abort the process on OOM.** Stay under the reported budget **proactively** — never rely
> on catching an allocation failure. The budget math runs *before* each upload, not after a failure.
