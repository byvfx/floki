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
| **T2 GPU texture** | `Rgba16Float` for f16 sources (`Rgba32Float` for f32/u32), **active layer only** | ~66 MB (16F) / ~133 MB (32F) | UI thread (`build_layer_texture`) | VRAM | instant paint on `swap_image_data` |
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
per_frame_t2 = w × h × 16            # 16 B/px: conservatively budgets for Rgba32Float.
                                     # f16 sources upload as Rgba16Float (8 B/px), so the
                                     # ring stays well under the real VRAM cost (#142).
max_t2       = floor(budget / per_frame_t2)
```

Off-Metal, `recommendedMaxWorkingSetSize` is `None` → use a conservative fixed/config cap.

### CPU RAM budget — binds T0 + T1
From `sysinfo` `sys_total` / `sys_used` in `Sample`, against **`ExrData::approx_bytes()`**
(sum of physical channel buffers × sample size).

```
ram_budget = (sys_total − sys_used + cache_bytes) × free_pct   # a slice of *free* RAM, not of total
max_t1     = floor(ram_budget / sizing_bytes)
```

Sized from *free* RAM (not `total × headroom − used`) on purpose: when other apps hold most of the
machine, a total-based ceiling collapses the ring to near-zero even with tens of GB physically free.
A free-relative slice keeps a usable read-ahead window and shrinks smoothly under external pressure.

**The two figures are asymmetric on purpose (#230).** They answer different questions, so neither
one scalar nor one measurement serves both:

- `cache_bytes` — what the ring *is holding*. A **measurement**: `FrameCache::bytes()`, the live sum
  of each resident entry's `approx_bytes()` snapshotted at insert. It is added back to free RAM so
  capacity does not chase the cache's own growth. It has to be measured because the ring is
  genuinely heterogeneous — playback fills it with beauty-only or proxy frames and each settle
  upgrade replaces one with a full frame at the same key — so no single scalar describes it. It was
  synthesized as `len × sizing_bytes` until #230, which put the same possibly-wrong scalar on both
  sides of the budget at once.
- `sizing_bytes` — what one *newly decoded* frame will cost. A **latch**, because a frame that does
  not exist yet cannot be measured: one per fidelity (`frame_bytes` / `beauty_bytes` /
  `proxy_bytes`), selected by `ExrApp::sizing_frame_bytes` in the decode path's own precedence,
  proxy over beauty over full. Sequences are homogeneous *at a given fidelity*, which is what makes
  a latch sound here; they are not homogeneous across fidelities, which is what made a single
  `frame_bytes` wrong.

Two rules keep the selection safe:

1. **Gate on the same predicate the decode path uses**, never a restatement of it —
   `sizing_frame_bytes` calls `decode_proxy_target_for` / `decode_beauty_only_for` directly. A
   sizing gate that drifts from the decode gate fills the ring with full frames under a cheap-sized
   cap: #215's OOM direction. The gate #230 replaced had drifted exactly that way, still asking
   `viewer.active_layer` after #213/#217 moved the decode path onto `displayed_aov`.
2. **Every fallback chain ends at `frame_bytes`**, never at something smaller. An unmeasured cheap
   fidelity yields a cap that is too *small* — wasteful, and self-correcting on the next decode —
   rather than one too large.

A fidelity change is safe without any re-measure: the cap recomputes every tick, and `tick_budgets`
force-evicts in the same tick when the ring exceeds it (#146), so beauty → full raises the divisor,
drops the cap, and shrinks the ring together.

## Windows differ — T1 vs T2

- **T1 window** = the decode-ahead horizon ahead of the playhead (RAM-budgeted).
- **T2 window** = a smaller texture ring around the playhead (VRAM-budgeted).

A T1 frame **behind** the playhead whose T2 is already built is evicted first (its pixels are in
VRAM; only the *active* frame needs CPU for sampling).

> **Implemented deviation (#153):** T1 eviction is T2-blind — `FrameCache::pick_victim`
> ranks purely by direction/distance/LRU and never consults the T2 ring. At T2's cap (≤ 8
> frames, all inside the prefetch window the T1 policy already protects last) the refinement
> wasn't worth coupling the tiers; revisit if T2 grows.

### Eviction = directional-ring + LRU tiebreak
- **Linear play:** evict opposite the play direction. In **Loop** mode "behind" is measured
  *around* the loop (distance in the play direction modulo the in/out span, #140), so
  prefetch wrapped past the out point ranks just-ahead instead of furthest-behind; trim
  leftovers outside the in/out range evict first.
- **Read-behind window (#169):** ~25% of the prefetch depth (`scheduler::read_behind`,
  OpenRV's `-lookback` model) is reserved for the frames *just shown*: within the window
  they rank by plain backward distance instead of the behind bias, so play-then-step-back
  is a cache hit. The scheduler fetches the same window (P3, after the forward window) and
  the pump's forward depth shrinks by the reservation, so fetch and eviction never fight
  over the last slots.
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
| `budget < 1 × T1` | Implemented as: **floor the T1 cap at 2** (`tick_budgets`, `app.rs`) so playback still runs — degraded to a 2-frame ring — rather than refusing sequence mode outright (#153). |
| 8K / huge frames | Windows collapse to 1–2; stutter, not crash. |
| Live VRAM pressure | Recompute the budget each second; shrink the T2 ring **before** the next upload. |

> **wgpu can abort the process on OOM.** Stay under the reported budget **proactively** — never rely
> on catching an allocation failure. The budget math runs *before* each upload, not after a failure.
