# Playback groundwork — design contracts (#56 / #57 → #7)

These documents are the durable design contracts for floki's v2.0 sequence-playback work.
Sequence playback (**#7**) is hard-blocked by two foundational contracts that must hold before any
caching or decode-ahead exists:

- **#56 — [memory contract](memory-contract.md):** a byte-budgeted, four-tier ring cache.
- **#57 — [concurrency contract](concurrency-contract.md):** a single-worker decode-ahead scheduler.

…and the feature that builds on them:

- **#7 — [sequence playback](sequence-playback.md):** detection, transport, clock, pacing, A/B.

…and, now that Phases 0–5 have shipped, the plan for what's left:

- **[hardening plan](hardening-plan.md) (#100):** post-ship status, the INV-SAMPLE gap-close,
  real-footage validation, and the feature follow-ons (#94 / #75 / #112 / #98), sequenced.

They are written **first** (before code) so the contracts can't drift, and so each implementation
phase can reference a fixed target. Every phase below is independently shippable and **never
regresses the single-image standalone app**.

---

## The one fact that shapes the whole design

GPU texture creation (`generate_gpu_texture`, `viewer.rs`) needs `gpu_resources` +
`queue.write_texture` → it is **UI-thread only**. The load worker can only produce CPU-side
`ExrData` (+ `ProxyImage`). The sampler (`sample_pixel`, `viewer.rs`, reached via
`self.viewer.ui(ui, data, …)` in `app.rs`, where `data` is the active slot's `&ExrData`) reads
the **active slot's `ExrData`** directly.

Therefore:

- A frame is not "playback-ready" until its `ExrData` has been uploaded to a GPU texture **on the
  UI thread**.
- The worker prefetches **CPU frames** only.
- The ring cache is **split across the thread boundary** (CPU tiers vs the VRAM tier).

Designing as if the worker could return GPU-ready frames would be wrong.

---

## The four tiers (per frame)

A single frame may be resident at several tiers at once.

| Tier | What | Size (4K) | Producer | Lives in | Purpose |
|------|------|-----------|----------|----------|---------|
| **T0 Proxy** | `ProxyImage` low-res RGBA32F (`proxy.rs`) | 5–20 MB | worker (`from_exr_fast_read`) | CPU | scrub preview / fallback paint |
| **T1 CPU frame** | full `ExrData`, ALL layers (`exr_loader.rs`) | 0.6–1.3 GB | worker (`ExrData::load`) | RAM | **only sampling source** + upload source for T2 |
| **T2 GPU texture** | `Rgba32Float`, **active layer only** | ~134 MB | UI thread (`generate_gpu_texture`) | VRAM | instant paint on `swap_image_data` |
| **T3 active** | the `ExrData` promoted into `self.exr_data` | (== one T1) | UI thread (swap) | — | what renderer + sampler see this frame |

See [memory-contract.md](memory-contract.md) for budgets, eviction, and the **INV-SAMPLE**
invariant that ties them together.

---

## Phase plan

Each phase ships independently; no phase regresses single-image.

- **Phase 0 — Accounting + ownership prep** (pure, low risk; first).
  `ExrData::approx_bytes()`; move `exr_data: Option<ExrData>` → `Option<Arc<ExrData>>`; budget-math
  module (`max_t1`/`max_t2`). All unit-tested, no GPU/UI.
- **Phase 1 — Sequence detection** (`src/sequence.rs`, pure, no GPU; zero risk).
  Frame parsing / grouping / numeric sort / hole detection. Tempfile-unit-tested. No UI yet.
- **Phase 2 — Manual transport over on-demand decode** (shippable, slow).
  `Playback` state + transport UI + frame clock; step/scrub issues a normal load and
  `swap_image_data` on arrival. Validates the whole UX before any caching.
- **Phase 3 — #56 T1 ring + epoch scheduler** (still lazy T2).
  `(frame, slot) → Arc<ExrData>` ring, budget-evicted; pure scheduler want-list; epoch (#57)
  replaces the path-check for the sequence path. Fully headless tests.
- **Phase 4 — #57 decode-ahead worker + #56 T2 pre-upload** (GPU; manual verification).
  Priority mailbox; UI-thread uploader pre-builds T2 within VRAM budget; pacing policy; layer-switch
  invalidation; scrub-proxy; pause-ensures-T1. Real smoothness appears here.
- **Phase 5 — A/B + ping-pong + polish.**
  A-plays/B-holds (mostly free), then optional locked-step; ping-pong; in/out tuning; drift tuning.

**Riskiest, flagged:** (1) the `Arc<ExrData>` refactor (Phase 0, many sites — done first).
(2) T2 pre-upload under live VRAM budget (Phase 4, can't unit-test, wgpu OOM aborts the process).
(3) epoch correctness under loop/ping-pong/scrub (Phase 3 — path recurrence is exactly where today's
supersession breaks). (4) UI-thread starvation (decode + texture-pack + clock on one global rayon
pool — measure in Phase 4 before partitioning).

---

## ⭐ On radar: the cache is the place to be clever

The four-tier model is the **baseline contract**, deliberately conservative — it is not the final
design. There is real opportunity in the cache itself. Directions to explore before/around Phase 3
(no need to dig yet):

- Smarter eviction than directional-ring + LRU — velocity/acceleration-aware prefetch shaping;
  drop behind aggressively on fast play but keep a sparse keyframe set for instant scrub-back.
- A persistent **T0 proxy ring** that survives eviction (cheap, ~5–20 MB/frame) so a *whole sequence*
  stays scrubbable at proxy res while only a window is full-res.
- An intermediate **half-float T2** tier to roughly double the VRAM frame count.
- Reusing a **second EXR-half** decode as a sampling tier.
- Possibly a small on-disk decoded cache.

Treat #56 as the contract and these as the cleverness layered on top — revisit at Phase 3.

---

## Open questions

1. **Mixed-padding grouping:** group on `(prefix, suffix)` within a dir, tolerate mixed padding
   (`9 → 10 → 100`) — the VFX norm. (Baked into Phase 1.)
2. **Default pacing:** **stutter** (play every frame); drop-frames is a toggle. A review tool wants
   every frame.
3. **Space-bar:** context-gate — sequence loaded → space = play/pause; otherwise space = blink-compare
   (today's behavior). Revisit if blink-during-sequence is wanted.
4. **Off-Metal VRAM:** `recommendedMaxWorkingSetSize` is macOS/Metal-only → conservative fixed/config
   cap elsewhere; don't block playback off-Metal.
5. **Budget defaults:** start with hardcoded headroom fractions + default `decode_ahead`; expose in
   the tools window later.
6. **Dedicated decode rayon pool:** out of scope for v2.0; a measure-first lever, noted not built.
7. **Persisted prefs:** `fps_target` / `loop_mode` / `direction` / `pacing` persist (serde);
   `sequence` / `current_frame` / handles reset per open.
