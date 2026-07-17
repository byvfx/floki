# Layer-Stack Unification — making the layer model core to floki

> Status: **proposed plan** (2026-07-16). Supersedes the ad-hoc PR-C/PR-D framing.
> Builds on the shipped additive Layers panel (epic #99, PR-B) and the layer model
> spine (`src/layer.rs`, #103). See `layer-model.md` for the model itself.

## The goal

Make the **`LayerStack` the single core model** floki renders and plays. Today there
are *two parallel systems*:

1. The **A/B compare** path — `Slot::{A,B}`, `is_b: bool`, the `exr_data`/`exr_data_b`
   field pair, `gpu_textures`/`gpu_textures_b`, `t2`/`t2_b`, `CompareMode`. This is the
   "core" the app is built around today, hardwired to exactly two inputs.
2. The **additive Layers panel** (PR-B) — an N-layer `comp_stack: LayerStack` +
   `comp_sources: HashMap<SourceId, CompSource>`, composited via the OCIO accumulate
   ping-pong. Still-only, OCIO-only, edit-side.

The end state is **one** system: open-file adds/sets a layer on the one stack; Single /
Composite / Wipe / Side-by-Side / Diff become the ≤2-layer special case rendered through
the same per-`SourceId` machinery the panel already uses; every layer is sequence-aware
and plays on the shared global playhead.

This is not scope creep — it is the explicit vision in `src/layer.rs`'s own docs
("Slot A/B is a hardcoded two-input special case … Compare modes become `Layout`
arrangements of the one model"). The additive panel was a deliberate de-risking stepping
stone; this plan makes it central.

## Key insight — PR-B already built the *render* half

The costly, risky part (N-layer premultiplied compositing on the GPU) is **done and
validated on-device**:

- Per-`SourceId` textures: `comp_sources: HashMap<SourceId, CompSource>` (`app.rs`).
- N-fold composite: `comp_layer_flags(i, n)` + `DrawCtx.blend_override` +
  `draw_comp_composite` iterating an arbitrary `&[CompDraw]` through the OCIO ping-pong
  (`viewer.rs`), proven by `accumulate_composite_on_device` (4-layer + opacity).
- Live-frame resolve: `comp_stack.composite_at(playback.current_frame)` (`app.rs`).

So unification is mostly **backend plumbing** (decode/cache/playback: `Slot`→`SourceId`)
plus **routing the A/B render through the panel's proven loop**. The exhaustive seam map
lives in the phase sections below.

## What is already generic (reused unchanged — do NOT rewrite)

Confirmed by code audit:

- `src/scheduler.rs` — `want_list` / `next_want` / `read_behind` are pure over frame
  numbers; **zero** `Slot`/A/B references. Already invoked once per slot in that slot's
  frame-space.
- `src/prefetch.rs` — keyed on `PathBuf`; epoch-agnostic; no slot coupling.
- `src/playback.rs` `advance(...)` — pure frame-number step rule; the master transport
  (`current_frame`, `in`/`out`, `anchor`, `epoch`) has **no `_b` fields** (B lives in
  `app.rs`).
- `src/viewer.rs` `T2Ring<T>` — frame-keyed GPU ring, generic over payload; policy
  (`t2_victim`, `evict_to_cap`, `ensure_layer`) has no slot concept. A single instance is
  already source-agnostic.
- All of `src/layer.rs` — `composite_at`, `Trim::source_frame`, `Step`/`Draw`,
  `LayerSource`, stable `LayerId`/`SourceId`. Nothing capped at 2.
- `src/budget.rs` — frame-count math is slot-agnostic (only its *input* `frame_bytes` is
  measured from Slot A today).

## Two semantic decisions to lock first

Locked in by the user (2026-07-16):

1. **Blank outside a layer's range** (DECIDED). A layer's sequence not covering the
   current global frame shows nothing (transparent) — model-native `Trim::source_frame`
   → `None`. This replaces today's B `map_b_frame` clamp/hold. Per-layer "hold" is a
   possible later option.
2. **The master clock does NOT stall on lagging layers** (DECIDED). The playhead advances;
   a layer whose frame isn't resident shows its last-resident frame for that beat.
   Smoothness over per-frame N-way sync; footprint controls (proxy / beauty-only / cap)
   keep sources decodable.

## Phases

Each phase is an independently shippable PR (or small stack), CI-green, with the app
usable throughout. Order is chosen so the **user-visible playback gap is fixed early** and
no throwaway parallel code is built.

### Phase 1 — `SourceId`-keyed decode / cache / playback backend  *(the foundation)*

Generalize the two-slot backend to N sources. This is the biggest phase but is
**mechanical** (a key swap + a state-struct fold); the pure helpers above don't change.

- **`src/cache.rs`**: key `(Slot, u32)` → `(SourceId, u32)`; delete the `Slot` enum;
  `evict_to`/`pick_victim`'s hardcoded `(playhead, playhead_b)` pair → a per-source
  protected-playhead map + a per-source policy flavor ("directional primary" vs
  "locked-step follower").
- **`src/app.rs` decode worker**: `is_b: bool` on `LoadJob`/`LoadResult` → `source:
  SourceId`; every `if res.is_b` / `match Slot::{A,B}` fork in `submit_seq` /
  `apply_load_result` / `next_want_slot` / `warm_ahead` → keyed on `source`.
- **Fold the `_b` mirror fields** (`exr_data_b`, `sequence_b`, `current_frame_b`,
  `pending_b`, `inflight_b`, `loading_b`, `open_gen_a`, `ab_offset`, …) into a
  `HashMap<SourceId, SourceState>` where `SourceState { sequence, current_frame, pending,
  inflight, loading, exr_data, open_gen }`. A single "primary source" id keeps the
  A-only special-cases (budget sizing, first-paint proxy, T2 evict).
- **The three `/2` budget splits** (`pump_decode` prefetch depth, `read_behind_depth`,
  T2 VRAM byte-split) → `/n_active`. The literal `[(A,0),(B,0),(A,d),(B,d)]` pump list →
  an N-source loop (primary playheads P0, then prefetch P1).
- **`src/viewer.rs` T2 rings**: `t2`/`t2_b` + the `_b` twin methods + two paint blocks →
  a per-`SourceId` `T2Ring` collection (a `HashMap<SourceId, T2Ring<T2Texture>>`).
- Resolve the **two semantic decisions** above here.

**De-risking:** keep the existing A/B UI driving exactly two `SourceId`s during this phase
so behavior is observable/unchanged; it's a refactor, not a feature. Add an on-device /
headless test that a 3-source cache evicts per-source correctly.

### Phase 2 — sequence-aware, playable comp layers  *(the visible payoff)*

Wire the Layers panel onto the Phase-1 backend.

- `add_comp_source` detects the source's sequence (as `detect_sequence` does for Slot A),
  registers a `SourceState`, and stores its frame range.
- Each visible comp layer decodes its `Trim::source_frame(global)` off the shared
  playhead; `draw_comp_central` binds the resident frame per source.
- Result: **added layers play** on the transport — the exact gap surfaced in the B.2–B.4
  smoke test.

### Phase 3 — unify the *render* (A/B through the comp machinery)

Route `emit_mode_draws` through the panel's proven loop.

- `render_program.rs`: `ProgramInput{A,B}` → a `SourceId`/`LayerId` handle; delete
  `input_of`'s A/B collapse; `new_ab_stack`'s 2-tuple → the shared `LayerStack`; thread
  the **live frame** into `resolve` (currently fixed at `0`); replace the single
  `is_composite` bool with `comp_layer_flags`-style per-index folding.
- `viewer.rs`: replace `gpu_textures`/`gpu_textures_b` + `pick_b` with the per-`SourceId`
  texture map; make `emit_mode_draws` **iterate `program.draws`**, binding each from the
  map (like `draw_comp_composite`) instead of the arrangement-hardcoded `bg_a`+`pick_b`.
- Generalize `active_layer` (one global AOV) → per-layer `aov` (the panel already stores
  it per source).
- Keep `Arrangement` as the viewport axis; lift Wipe / Side-by-Side / Diff geometry out
  of the A/B-only `emit_mode_draws` arms into arrangement-generic code (the #104 work).
  The OCIO-off display pass for N-layer non-OCIO composite lands here too.

### Phase 4 — collapse to one model

- Open-file **adds/sets a layer** on the one stack; the A/B compare UI and the Layers
  panel become two views over the same `LayerStack`.
- Delete the now-dead `_b`/`comp_*` duplication and the `CompareMode`→`configure` shim,
  replaced by direct stack editing + an `Arrangement` selector.
- Persistence (the deferred B.5) applies to the one stack: nested `Vec<LayerPersist>`
  (path + name/blend/opacity/enabled/solo/aov/trim), re-decoded on load.

## Risks & de-risking

- **Wide, mechanical refactor (Phase 1).** Mitigate: keep the A/B UI pinned to two
  `SourceId`s so behavior is unchanged and diff-observable; land it as a pure refactor PR
  with no feature change; lean on the existing playback contract tests + a new
  3-source cache test.
- **Footprint (N sequences in RAM).** Deferred but real; the cap-6 + proxy/beauty-only +
  on-disk proxy cache (#94/#56/#165) machinery already exists. Phase 1's `/n` budget
  splits are where this is enforced. A dedicated footprint pass follows Phase 4 if needed.
- **Behavior change for existing A/B users** (blank-vs-hold decision). Surface it; default
  to model-native blank; offer per-layer hold later.
- **Qt port (#44) interaction.** This unification *is* the "foundational architecture" the
  Qt port was blocked on — doing it first means porting one model, not the old A/B UI +
  then ripping it out.

## Relationship to the existing roadmap

- Absorbs the old **PR-C (N-source playback)** = Phases 1–2, and **PR-D (footprint)** into
  the footprint mitigations above.
- **PR-B.5 persistence** folds into Phase 4 (persist the one stack) — no reason to persist
  the additive panel separately if it's about to merge.
- Unblocks **#44 (Qt port)** and the reframing of compare modes as global effects on the
  composited framebuffer.

## Recommended first step

**Phase 1**, landed as a behavior-preserving refactor PR (A/B UI pinned to two
`SourceId`s). It fixes nothing user-visible on its own, but it is the foundation that makes
Phase 2 (playable layers — the smoke-test gap) a small follow-up, and it converts the
whole decode/cache/playback backend to the model the render half already speaks.

---

## Progress log

Branch `feat/layer-stack-panel` (stacked on PR-A #188). Each slice is CI-green
(`cargo test --lib` + `cargo clippy --lib` default & `--features system-ocio` + `cargo build`).

- **PR-B.1–B.4** — additive Layers panel (scaffold, decode-on-demand, ping-pong render,
  per-row vis/solo/blend/opacity/reorder/AOV). The N-layer render half.
- **P1.1** `31c2809` — `cache.rs` → `SourceId` key + N-playhead eviction (`playheads:
  &[(SourceId,u32)]` + `primary`); `Slot: Into<SourceId>` bridge; `ExrApp::cache_playheads()`.
- **P1.2** `e03b0a8` — `LoadJob`/`LoadResult` `is_b:bool` → `source:SourceId`; worker routes
  by source; `apply_load_result` derives `is_b` from `res.source` for the still-forked routing.
- **P1.4a** `a06c80b` — fold the eight scattered `*_b` follower fields
  (`loaded_file_b`/`sequence_b`/`current_frame_b`/`ab_offset`/`pending_b`/`inflight_b`/`loading_b`)
  into a single `SourceState` struct on `ExrApp` as `self.b`. Pure mechanical fold.
- **P1.4b** `963e77a` — generalize that single `b` follower into
  `followers: BTreeMap<SourceId, SourceState>` (+ `b()`/`b_mut()`/`active_followers()`/`n_active_sources()`);
  B pinned as the sole entry at `Slot::B.into()`. The **source-agnostic** aggregates now iterate the
  map: `cache_playheads`, the `pump_decode` busy-gate, the repaint poll, the watchdog/trace
  "outstanding" checks, `invalidate_inflight`, and the `/2`→`/n_active_sources()` budget split.
  Behavior-preserving (one follower). The `Slot::B`/`_b`-viewer-coupled decode/render/routing still
  addresses the sole follower via `b()`/`b_mut()`.

- **P1.3** `5fd2e94` (landed *after* P1.4a/b) — viewer `t2`/`t2_b` twins → one
  `t2_rings: BTreeMap<SourceId, T2Ring<T2Texture>>`; the seven `*_b` twin methods + `layer_b_for`
  collapse into source-keyed `set_t2_cap`/`set_t2_frame`/`t2_cap`/`t2_len`/`prebuild_t2`/
  `evict_t2_frame`/`clear_t2` (+ the general `t2_layer_for`); the two paint blocks bind `ring_a`/
  `ring_b` by entry-or-insert (`gpu_textures`/`_b` stay — Phase 3). App call sites pass
  `Self::{A,B}_SOURCE`; `tick_budgets` VRAM split `/2`→`/n_active_sources()`. `pump_t2`/`pump_t2_b`
  stay two funcs (now source-keyed). Behavior-preserving. **The T1 cache (P1.1) and T2 rings (P1.3)
  are now both `SourceId`-keyed** — the remaining P1.4 TODOs (N-source pump list, `cache::Slot`
  deletion) are unblocked.

## Execution checklist — remaining Phase 1 (resume here)

Symbols are stable; line numbers drift (grep the name). Keep A/B pinned to
`SourceId(0)`/`SourceId(1)` throughout so behavior is observable/unchanged.

### P1.3 — per-source T2 GPU rings (mostly `viewer.rs`, some `app.rs`)  **[DONE `5fd2e94`]**

- **[DONE]** `ExrViewer.t2` / `t2_b` → one `BTreeMap<SourceId, T2Ring<T2Texture>>` (chose
  `BTreeMap` over `HashMap` for a deterministic per-source order; the `T2Ring<T>` policy reused
  unchanged).
- **[DONE]** Fold the `_b` twin methods into source-keyed ones (each takes `source: SourceId`):
  `set_t2_cap`, `set_t2_frame`, `t2_cap`, `t2_len`, `prebuild_t2`, `clear_t2`, `evict_t2_frame`;
  the seven `*_b` variants + `layer_b_for` are gone (`layer_b_for` → the general `t2_layer_for`).
- **[DONE]** `tick_budgets` T2 VRAM byte-split `avail/2` → `avail / n_active_sources()`.
- **[DONE]** The two paint blocks bind `ring_a`/`ring_b` (entry-or-insert); `gpu_textures`/
  `gpu_textures_b` kept as-is (they unify in Phase 3), only the ring source changed.
- **[DEFERRED]** `ExrApp::pump_t2` / `pump_t2_b` stay *two* source-keyed functions — the "one
  per-source pump loop" rides with the `pump_decode` list unification below (same primary-vs-
  follower frame-space asymmetry).

### P1.4 — fold the `_b` app state; delete `Slot` (`app.rs`, `playback.rs`)

- **[DONE P1.4a]** Fold the scattered `*_b` follower fields into a `SourceState` struct
  (`self.b`). **[DONE P1.4b]** Generalize it to `followers: BTreeMap<SourceId, SourceState>`;
  B pinned as the sole entry. (`exr_data_b` stays the display slot; `open_gen_a` stays the
  primary generation; A's decode state stays the top-level `inflight`/`loading_a` for now — a
  full `primary: SourceId` fold with A *in* the map waits until A's transport-owned frame space
  is generalized.)
- **[DONE P1.4b]** The **source-agnostic aggregates** iterate `followers`: `cache_playheads`,
  the `pump_decode` busy-gate, the repaint poll, the watchdog/trace outstanding checks,
  `invalidate_inflight`, and the `/2`→`/n_active_sources()` budget splits.
- **[TODO — now unblocked, P1.3 done]** Source-key the A/B *decode* forks (`submit_seq`,
  `next_want_slot`, `warm_ahead`, `apply_load_result`'s derived `is_b`/`slot`) and the app-level
  `is_b:bool` params (`open_file`, `swap_image_data`, `swap_image_arc`, `unload`,
  `route_dropped_exrs`). Today these reach the sole follower through `b()`/`b_mut()`; the T1 cache
  (P1.1) + T2 rings (P1.3) are now `SourceId`-native, so the fork body no longer *has* to be
  `Slot::B`/`_b`-hardcoded. Fold `pump_t2`/`pump_t2_b` into the one per-source loop here too.
- **[TODO — now unblocked, P1.3 done]** `pump_decode`'s literal `[(A,0),(B,0),(A,d),(B,d)]`
  priority list → an N-source loop (primary playheads P0, then prefetch P1).
- **[TODO — behavior change]** Replace `sync_b_to_a` + `request_b_frame` +
  `map_b_frame`/`map_b_frame_offset` (`playback.rs`) with `Trim::source_frame` per source.
  **DECIDED semantics: BLANK outside a layer's range** (not the old clamp/hold), and **the master
  clock does NOT stall** on a lagging follower (keep `tick_stutter` gating on the primary only).
  Land as its own slice (it changes B's edge behavior + needs the display path to handle a blank
  follower) — not bundled with the mechanical folds.
- **[TODO — now unblocked, P1.3 done]** Delete `cache::Slot` + the `Slot::A.into()`/`Slot::B.into()`
  bridge sites once the decode forks above are source-keyed and nothing needs the two-value enum.
  (The cache & viewer are already `SourceId`-native; `Slot` now survives only as the app-side A/B
  label in the decode forks.)
- `budget.rs` needs no change (already slot-agnostic); `scheduler.rs`, `prefetch.rs`,
  `playback::advance` unchanged.

**Resume next at P1.4c** — source-key the decode forks (`submit_seq`/`next_want_slot`/`warm_ahead`/
`apply_load_result`), turn the literal A/B pump list into an N-source loop, fold `pump_t2`/`pump_t2_b`
together, and delete `cache::Slot`. Now unblocked (T1 cache + T2 rings are both `SourceId`-native).
The `map_b_frame`→`Trim::source_frame` swap lands independently whenever, as its own behavior-change
slice (BLANK outside range).

### After Phase 1

Phase 2 (make Layers-panel `add_comp_source` detect a sequence + register a `SourceState`
so comp layers play) becomes a small follow-up. Then Phase 3 (render unify) and Phase 4
(collapse + persistence). See the per-Phase sections above.
