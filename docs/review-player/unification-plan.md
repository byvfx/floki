# Layer-Stack Unification — making the layer model core to floki

> Status: **in progress** (updated 2026-08). Phases 1–2 + the R-series are done; the R4 collapse is
> underway — open/drop = add a layer, the comp-path readout / EXR Info / histogram, and **all four
> compare arrangements** (render-retire Slices 2a+2b) have landed, so nothing user-visible is left on
> the A/B path. Next: **Slice 3, the A/B retire**. Resume point and concrete next steps: the **R4
> handoff** section below. Supersedes the ad-hoc PR-C/PR-D framing.
> Builds on the shipped additive Layers panel (epic #99, PR-B) and the layer model spine
> (`src/layer.rs`, #103). See `layer-model.md` for the model itself.

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

### Phase 2 — sequence-aware, playable comp layers  *(the visible payoff)*  **[DONE]**

Wired the Layers panel onto the Phase-1 backend.

- **[DONE P2.1 `542c6c8`]** Reserve `SourceId` 0/1 for the A/B slots; comp sources allocate
  from `COMP_SOURCE_BASE = 2` (else the first two would alias A/B in the shared cache/rings).
- **[DONE P2.2 `d210383`]** `add_comp_source` runs `detect_from_file`; a numbered file registers
  a `SourceState` follower (sequence + opened frame), its layer's `Trim` spans the range (blank
  outside), and the opened frame seeds the T1 cache. `apply_load_result`'s follower branch now
  splits **B** (display slot → `swap_b_frame`) from a **comp** follower (cache + repaint);
  `remove_comp_layer` drops the follower + its cache.
- **[DONE P2.3 `9551e21`]** `sync_comp_followers` (comp `sync_b_to_a`) runs at the playhead
  chokepoint: each comp layer maps global→source via `Trim` and requests it; the pump decodes it
  (`active_followers()`). `draw_comp_central`'s `ensure_comp_frame` rebinds each layer's resolved
  source frame from the T1 cache (`CompSource.cur_frame` gates rebuilds); stills keep
  `ensure_comp_aov`.
- **Result: added comp layers PLAY** on the transport — the B.2–B.4 smoke-test gap closed.
- **Scope:** comp layers follow the **A-driven** transport (load a base plate as slot A to drive
  the clock); matching frame numbers play in lockstep, mismatched need a per-layer offset UI
  (follow-up, #102/#104). Standalone comp-only playback is Phase 4. GPU render binding is unit-
  tested at the sync layer; the OCIO+GPU composite reuses PR-B.3 infra — a manual smoke test
  (base plate A + 2 comp sequences + play) is the end-to-end check.

### Phase 3 — unify the *render* (A/B through the comp machinery)  *(partly done via the R-series)*

The comp path is now a full viewport — the **new-render half** of Phase 3 is built: the
accumulate ping-pong renders in any colour mode (**R1/R2**), threads the live frame + per-layer
`aov` (Phase 2), and folds the base plate in (**R3**). What is **not yet done** is retiring the
*old* A/B render path in favour of it:

- **[TODO]** `render_program.rs`: `ProgramInput{A,B}` → a `SourceId`/`LayerId` handle; delete
  `input_of`'s A/B collapse; `new_ab_stack`'s 2-tuple → the shared `LayerStack`; thread the **live
  frame** into `resolve` (currently fixed at `0`); the single `is_composite` bool → `comp_layer_
  flags`-style per-index folding (the pattern `draw_comp_composite` already uses).
- **[TODO]** `viewer.rs`: replace `gpu_textures`/`gpu_textures_b` + `pick_b` with the per-`SourceId`
  texture map; make `emit_mode_draws` **iterate `program.draws`**, binding each from the map (like
  `draw_comp_composite`) instead of the arrangement-hardcoded `bg_a`+`pick_b`.
- **[DONE via R1/R2]** the OCIO-off display pass for a non-OCIO composite.
- **[DONE `ab63b16`]** **pixel readout** ported into the comp path (`draw_comp_central` →
  `sample_comp_readout` + the pure `top_sample_source` / `comp_hover_pixel` helpers; the status-bar
  row reuses `draw_nuke_status_line`). Samples the **top layer's raw source pixels** (the A/B
  analogue), not the composited GPU result. The **histogram** has since landed too.
- **[PARTLY DONE]** keep `Arrangement` as the viewport axis; lift Wipe / Side-by-Side / Diff geometry
  out of the A/B-only `emit_mode_draws` arms into arrangement-generic code (#104). **This is where the
  compare modes come back** for the comp path. **Side-by-Side is done** (Slice 2a below — and *not*
  by making `emit_mode_draws` generic; the comp path grew its own geometry and the A/B arm stays put
  until Slice 3 deletes it). Wipe + Diff remain (Slice 2b).

> **CORRECTION (2026-08-14):** the first two TODO bullets above are **stale**. `Arrangement` already
> exists as a real enum (`render_program.rs`, `Stacked | Wipe{position} | SideBySide | Diff`) and is
> the live dispatch key in `emit_mode_draws`, and the comp path already resolves at the **live frame**
> (`composite_at(current_frame)`). So the end state **deletes** `render_program.rs` wholesale (its
> `resolve`/`configure`/`ProgramInput`/`new_ab_stack`/… all die), rather than threading a live frame
> into `resolve`. See the concrete sliced plan below.

#### Render-retire — concrete sliced plan  (locked 2026-08-14)

Two render paths exist: the **A/B path** (`draw_canvas_gpu` → `render_program::resolve` →
`emit_mode_draws`, ≤2 inputs via positional `gpu_textures`/`gpu_textures_b` + `pick_b`, geometry
hardcoded per-arm) and the **comp path** (`draw_comp_central` → `draw_comp_composite`, N-layer
accumulate at the live frame from `comp_sources`, but only ever `Stacked`). Retire the A/B path.

**Key architectural fact:** Wipe and Diff in the A/B path are **single-pass 2-input shader draws**
(`tex_a`+`tex_b`, `is_wipe_mode`/`is_diff` uniforms), **not** accumulate draws; Diff opts out of OCIO
(display-space heat map). SideBySide is two independent placed draws. The comp renderer today has
**only** the accumulate path — so compare modes are not a trivial addition; Wipe/Diff need the
non-accumulate 2-input pass reintroduced.

- ~~**Slice 1 — texture-map unification.**~~ **DROPPED (2026-08-14).** It refactored
  `gpu_textures`/`gpu_textures_b` + `pick_b`, which are used *only* in the A/B render path that Slice 3
  deletes — and Slice 2's "compose vs current layer" sources side B from `comp_sources`, never from
  that storage. It was a holdover from the "make `emit_mode_draws` generic" framing the "delete, not
  evolve" correction invalidated. Refactoring code we then delete is wasted motion; go straight to
  Slice 2.
- **Slice 2 — Arrangements in the comp path.** ~~**UX model (locked): compose vs current layer**~~ —
  A = the full composite, B = the `active_comp_layer` (the timeline-selected "current" layer); zero
  new selection UI. **REVISED TWICE on first use (2026-08-14), and the second revision is the model:**
  a compare shows the **two layers themselves** — pane A = the `Layer:` (current) layer, pane B = the
  `vs:` layer (`compare_b_layer`, Nuke's second viewer input, defaulting via the pure
  `default_compare_b` to the topmost non-current layer). *Any* "composite vs a layer" framing is
  wrong: side A contains side B by construction, so the compared content shows up twice — first as
  "B duplicates the top of the comp", then, after B became its own pick, as "the nebula still shows
  up on the left". Only when the compare is off (`Stacked`) does the viewport show the composite. New
  `ExrApp::comp_arrangement: Arrangement` (default `Stacked`) surfaced in
  `draw_comp_layer_bar`. Geometry helpers extracted **pure + unit-tested** (`side_by_side_layout`,
  `wipe_line_endpoints`), same pattern as `comp_hover_pixel`. Sub-slices: **2a SideBySide** (fits the
  accumulate model — two draw-groups at disjoint rects, ship first); **2b Wipe + Diff** (needs the
  non-accumulate 2-input pass — first cut restricts each side to a single texture, base-vs-current;
  full N-vs-N via offscreen-per-side is a fast-follow). `draw_comp_composite`'s hardwired
  `force_accumulate=true` / `is_wipe_mode=0` become arrangement-dependent; fold `comp_arrangement` +
  current-layer id into `render_sig`.

  > **CORRECTION (2026-08-14): "2a SxS fits the accumulate model" was WRONG.** The composite uses a
  > two-target **ping-pong**, and each accumulate draw does a whole-target `LoadOp::Clear` + only
  > rasterizes its own rect. So two disjoint-rect groups in one *accumulate sequence* can't coexist —
  > the final target keeps only the *last* group's rect (verified on-device: side A/composite drops to
  > a sliver, side B renders). **Landed as scaffolding** (`comp_arrangement` field + `Compare:`
  > selector + the pure `side_by_side_layout` helper + tests, `#[allow(dead_code)]` until wired); the
  > single-pass render was reverted.
  >
  > **SUPERSEDED (2026-08-14):** that correction went on to conclude SxS "requires the
  > offscreen-per-side path" — two new `compare_a`/`compare_b` targets, a new rect-placing pipeline +
  > shader, and a `two_group` mode on `OcioCallback`. **It doesn't.** See **2a [DONE]** below for what
  > actually shipped: one extra `LoadOp::Load` render pass, no new GPU resources at all.

- **[DONE] Slice 2a — SideBySide via a placed overlay pass.** The premise that made the
  offscreen-per-side build look necessary was that *both* sides are accumulate groups. Neither is: a
  compare draws **one layer per pane**, so pane B needs no ping-pong at all and pane A is a one-layer
  "composite". Pane A still goes through the accumulate path unchanged — every draw's `target_rect`
  is now `rect_a` instead of the full image rect, and the parity `start=(N-1)%2` still lands it in
  `scene_view` (so this stays correct if a pane ever becomes multi-layer again) — and pane B is
  emitted as one independent placed draw into a **second pass-1 render pass with
  `LoadOp::Load`** (`OcioCallback::overlay_draws`, "pass 1b"). `pipeline_linear` is `blend: None` +
  `ColorWrites::ALL`, and the two rects abut without overlapping, so B overwrites colour *and* alpha
  only under its own quad and leaves A untouched; everything outside both keeps the α=−1 no-image
  sentinel the blit discards on. Pass 2 (OCIO transform *or* the OCIO-off display-encode) and the
  final blit then run **once, unchanged**, over the combined scene — so both panes get the display
  transform and the view ops exactly once, which the two-offscreen design would have had to
  reconstruct by hand. Net: **zero** new textures, pipelines, or bind groups. The same overlay pass
  generalizes to N panes (contact-sheet / #104 N-way compare), since each is just another placed draw
  at a disjoint rect.
  - `draw_comp_composite` takes `arrangement` + a `CompSideB` (texture + native size + its own PAR).
    `draw_comp_central` resolves **both** panes with the pure `comp_layer_draw` and, when the compare
    is live, **replaces the composite draw list with pane A's single layer** (canvas size / PAR taken
    from it) — so neither pane is a composite. **Falls back to `Stacked`** unless *both* panes
    resolve (a layer can be hidden / soloed out / trimmed blank / textureless); a half-drawn split is
    never shown.
  - **Both panes are explicit picks:** pane A = the `Layer:` (current) layer, pane B = the new `vs:`
    picker (`compare_b_layer`), defaulting via the pure `default_compare_b` to the topmost non-current
    layer. See the Slice 2 revision note above for the two wrong models this replaced.
  - The GPU callback is installed into a painter slot **reserved before** the divider
    (`painter.add(Shape::Noop)` → `painter.set`), mirroring `draw_canvas_gpu`. Appending it last
    paints the composite quad straight over the divider line, which is invisible then.
  - `disp_rect` in SxS is the **union** of both panes, not `rect_a` — `display_min/max` gates the
    shader's overscan dim, so a `rect_a`-only value would silently force side B's layer opacity to 1.
  - `framing_bounds` now takes `side_by_side: bool` instead of `CompareMode` (so the comp path drives
    it from its own `Arrangement`, and one more `CompareMode` dependency dies ahead of Slice 3);
    `handle_canvas_interaction` gained the matching parameter, so first-paint fit spans both panes.
  - `scissor_pts` is `None` in SxS (mirroring the A/B path); `comp_arrangement` is salted into
    `render_sig` alongside the display-stage salt.
  - Per-pane pixel readout: new `last_image_rect_b` + the pure `pick_comp_side`, so hovering the
    compare pane samples *that* layer and renames the status-bar row, instead of reporting the
    composite's top layer. Hovering outside both panes blanks.
  - `Normalize Size` (the existing shared `normalize_side_by_side`) sits beside the `vs:` picker.
    New pure tests: `pick_comp_side`, `comp_layer_draw`, `default_compare_b`;
    `side_by_side_layout` stops being dead code and `framing_bounds`' test moves to the bool.
- **[DONE] Slice 2b — Wipe + Diff.** This section previously predicted 2b would need "a third scene
  target, or a wipe pass that samples the accumulation via `accum_tex_bg`", because side A lived *in*
  `scene_view` and a 2-input wipe would have to read and write it at once. **That was already stale
  when written:** the layer-vs-layer revision (Slice 2 note) made pane A a plain layer texture, so
  the hazard evaporated. Wipe and Diff are just what they are on the A/B path — a **single 2-input
  draw**, pane A as `tex_a` and pane B as `tex_b`, at one shared rect, with `is_wipe_mode` / `is_diff`
  doing the work in `fs_main`. `composite_accum` stays 0, so `tex_b` is sampled at image-local uv.
  **No new GPU resources here either.**
  - Wipe stays colour-managed: it is one accumulate draw (`n = 1`), so pass 1 binds pane B as its
    `tex_b` and the display stage runs normally. Diff keeps the A/B path's opt-out — `DrawCtx::draw`
    routes `is_diff` past the accumulate path to an immediate `ExrCallback`, so `ocio_draws` comes
    back empty and the existing early return *is* the exit. `is_wipe_mode = 0` /
    `is_diff = false` in `draw_comp_composite` became arrangement-dependent.
  - `wipe_line_endpoints` extracted **pure + unit-tested** and now shared by the A/B path, so the two
    can't drift; `handle_wipe_interaction` (drag the handle, scroll to rotate) is reused as-is.
  - Wipe overlays both layers in **one** rect, so the per-pane readout can't key on `last_image_rect_b`.
    New pure `wipe_side_at` mirrors `fs_main`'s split exactly (rect-relative pixels projected on the
    `(cos θ, sin θ)` normal, `dist >= 0` ⇒ `tex_b`), and `comp_hover_side` composes it with the
    Side-by-Side two-rect case behind one call. `ExrViewer::last_wipe` carries the frame's
    centre/angle to the readout.
  - **Follow-up:** the Diff controls (`diff_multiplier` / `diff_metric` / `diff_floor`) and the wipe
    angle / line-opacity sliders still live only in the A/B compare toolbar — the comp bar exposes
    neither, so comp Diff runs on defaults. Surface them in `draw_comp_layer_bar` (or fold that
    toolbar in) with the Slice 3 retire.
- **Slice 3 — retire the A/B path.** Delete `emit_mode_draws`, `gpu_textures_b`, `pick_b`,
  `render_program.rs` (keep/relocate only `Arrangement`, or fold into `layer::Layout`), `CompareMode`,
  the `configure` shim, and the `_b`/`comp_*` duplication; route `draw_central_canvas` through the
  comp path only (migrate the Contact-Sheet `viewer.ui` shim). `open_file` → **deleted**;
  `update_base_layer` → **revived** (live base-plate infra, drop the `#[allow(dead_code)]`). Grep each
  deleted symbol for hidden readers (status bar / EXR Info / histogram were re-homed into the comp
  path). Persistence (`Vec<LayerPersist>`, never `#[serde(flatten)]`) is an independent fast-follow.

**Order:** 2a **[DONE]** → 2b **[DONE]** → 3 (Slice 1 dropped). Each slice CI-green (default +
`system-ocio`), no throwaway code.

> **Retrospective on 2a/2b (2026-08-14).** Both slices were planned around GPU work that turned out
> to be unnecessary — two new render targets + a pipeline + a `two_group` callback mode for 2a, a
> third scene target for 2b. Both estimates came from the same wrong premise: that a compare pane is
> *the composite*. Once each pane is a single layer, 2a is one extra `LoadOp::Load` pass and 2b is
> zero new GPU code. **The expensive question was a UX question, not a rendering one** — worth
> settling what the panes actually show before designing how to render them.

### Phase 4 / R4 — collapse to one model  ← **RESUME HERE (handoff below)**

The R-series has already delivered the pieces R4 stands on: the base plate is a real layer (R3),
the comp path renders standalone (R1/R2) and drives the transport (R4-lite), and the UI is the
bottom timeline tracks. R4 finishes the collapse:

- **[DONE `03ec2cf`] Open / drop = add a layer (literal).** Every open/drop routes through the new
  `open_layer` → `add_comp_source` (records recents, shows the panel). `handle_drag_and_drop` lost
  the "Drop for A / Drop for B" split for a single "Drop to add layer" overlay; the four A/B-split
  drop helpers + the B-reference menu items are deleted. `draw_central_canvas` enters the comp path
  on a non-empty stack alone (the panel toggle now hides only the tracks, not the composite). The
  legacy `open_file` / `update_base_layer` are kept `#[allow(dead_code)]` until the render-retire
  step deletes or revives them. **Design decision (user): the literal variant** — a single image
  renders through the comp path and loses readout / histogram / compare until render-retire ports
  them in (readout has since landed, see Phase 3).
- **Delete the `_b`/`comp_*` duplication** and the `CompareMode`→`configure` shim, replaced by
  direct stack editing + an `Arrangement` selector. Depends on the Phase-3 `emit_mode_draws`
  retire + compare-modes-as-arrangements above (do that first, or R4 loses the compare modes).
- **Persistence (deferred B.5).** A nested `Vec<LayerPersist>` on `ExrApp` (path + name / blend /
  opacity / enabled / solo / aov / trim), re-decoded on load; re-alloc `LayerId`/`SourceId`. NEVER
  `#[serde(flatten)]` (wipes `app.ron`). `BlendMode` already has serde. Independent of the render
  retire — landable any time.

#### R4 handoff — state as of `cad0a56` (2026-08)

- **Branch:** `feat/layer-stack-play`, PR **#190** (ready for review), 17 ahead of `main`, CI green.
  Phase 1 + Phase 2 + the R-series + the R4 collapse-so-far are all on it.
- **Done since `7379fd5`:** open/drop = add a layer (`03ec2cf`, literal); **pixel readout** in the
  comp path (`ab63b16`); two Copilot review fixes on the R2 OCIO-off path (`cad0a56` — fold the
  display stage into the comp-path `render_sig` so an OCIO toggle re-renders; cache the display-encode
  scene bind group on `OcioTargets`).
- **Routing now:** `draw_central_canvas` runs the comp path whenever `comp_stack` is non-empty; the
  classic `viewer.ui` A/B path is only reachable with an **empty** stack, which no longer happens via
  the UI (open/drop always add a layer). It is *retained, not deleted* — the render-retire step below
  revives compare modes as `Arrangement`s and **then** the `_b`/`comp_*` deletion removes it.
- **RESUME AT — render-retire (Phase 3), Slice 3.** **All four arrangements are back in the comp
  path** (Stacked / Side-by-Side / Wipe / Diff, Slices 2a+2b) and the readout / EXR Info / histogram
  are ported, so nothing user-visible is left on the A/B path. Slice 3: delete `emit_mode_draws` /
  `gpu_textures_b` / `pick_b` / `render_program.rs` (keep `Arrangement`) / the `_b`/`comp_*`
  duplication + the `CompareMode`→`configure` shim, and drop the `#[allow(dead_code)]` on
  `open_file` / `update_base_layer`. Grep each deleted symbol for hidden readers first.
  **Carry over from 2b:** the Diff controls (`diff_multiplier`/`diff_metric`/`diff_floor`) and the
  wipe angle / line-opacity sliders currently exist *only* in the A/B compare toolbar, so they must
  be re-homed into `draw_comp_layer_bar` rather than deleted with it. Persistence
  (`Vec<LayerPersist>`, path + name/blend/opacity/enabled/solo/aov/trim; NEVER `#[serde(flatten)]`)
  is independent — landable any time.
- **Readout fast-follows:** the floating cursor tooltip (`viewer.rs` `Window::new("Pixel Tooltip")` —
  factor its swatch/HSVL block out and reuse); expose the aperture combo (1 / 3×3 / 9×9) in comp mode.
- **Non-blocking follow-ups:** reuse A's T2 ring in `ensure_base_frame`; a `.cube` LUT in the OCIO-off
  display-encode (R2 is sRGB-only); the lazy 2nd scene target (`ocio_pass.rs`, wasted for single-image
  OCIO); the ruler cache-fill bar reading only `A_SOURCE`; the `map_b_frame`→`Trim::source_frame` slice
  — now effectively folded into the `_b` deletion (B is unreachable under the literal collapse).

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

### Render foundation + timeline UI  (post-Phase-2, branch `feat/layer-stack-play`, PR #190)

The R-series makes the comp path a self-sufficient viewport (renders in any colour mode, drives
the transport, base plate included) and dresses the panel as Chaos-Player timeline tracks — the
groundwork the final collapse (**R4**) sits on. All in `src/app.rs` + the GPU render files.

- **R1** `7ebda89` — OCIO-**off** display-encode pipeline: `DISPLAY_ENCODE_SHADER` +
  `GpuState.display_encode_pipeline` (linear→sRGB), the OCIO-off twin of the OCIO pass-2 display
  transform. On-device test `display_encode_srgb_on_device`.
- **R2** `ff5f946` — the composite renders in **any** colour mode. `DrawCtx.force_accumulate`
  (comp always folds through the ping-pong), `OcioCallback.use_display_encode` (`= !ocio_active`,
  runs `display_encode_pipeline` in pass-2's slot); `draw_comp_central` drops its `ocio_active`
  gate. Composite no longer requires OCIO.
- **Visibility fix** `4d73e66` — an added comp *sequence* went blank (playhead 0 outside its
  range): `add_comp_source` sets `Trim.offset = cf - global` so the opened frame is visible at
  add-time.
- **R4-lite** `1b4fd03` — the comp stack drives the transport + the panel is on by default:
  `add_comp_source` `playback.enter`s the first comp sequence when there's no transport;
  `comp_drives_transport` gates the slot-A decode path off (no double-decode); `open_file`
  reclaims the clock; `remove_comp_layer` releases it with the last comp sequence.
- **Per-layer time offset** `2ef1102` + `47bd65d` — each sequence layer gets an editable
  `Trim.offset` (slide a layer in time), re-requested via `sync_comp_followers`.
- **Panel → bottom** `b163218` — `draw_layers_panel` becomes `egui::Panel::bottom` (Chaos-Player
  layout), reordered so top→bottom is viewport → transport → layers → status.
- **Timeline tracks** `374ff2c` — the plain-list rows become **clip bars on a shared frame
  ruler**. The transport + Layers panels merge into one `Panel::bottom("timeline_panel")`
  (`draw_timeline_panel`) so bars share the ruler's x; `draw_transport_bar` → `draw_transport_
  controls`, `draw_layers_panel` → `draw_layer_tracks`. New primitives (unit-tested headless):
  `TimeAxis` (frame↔x, clamps off-range), `for_each_frame_run` (shared run-coalescer, #146),
  `alloc_timeline_row`, `track_span`, `offset_after_drag` (grab-anchored, drift-free), `TrackDrag`.
  Each layer = narrow gutter (eye/solo/name + `⋮` menu) + a clip bar with its own two-tone
  cache-fill strip, draggable to retime. NLE drag convention (drag right → later).
- **R3 — base plate in the stack** `7379fd5` — slot A becomes a real bottom **base track**
  (`LayerSource::Image{source: A_SOURCE}`), so adding a comp layer no longer makes the plate
  vanish. Helpers `base_layer_id` / `add_base_layer` / `update_base_layer` / `remove_base_layer` /
  `ensure_base_frame` (rebuilds A's composite texture from the live `self.exr_data`, since A is the
  master transport, not a follower). Lifecycle hooks in `add_comp_source` / `remove_comp_layer` /
  `open_file` / `unload`. The base is clock-pinned (no retime) and non-removable via the panel;
  `remove_base_layer` touches only the model + `comp_sources`, never A's real `frame_cache` /
  `followers`. **Routing stays conservative** (user decision): 0 comp sources → classic `viewer.ui`
  (readout / histogram / compare modes intact); ≥1 comp source → comp path with the plate at bottom.

### R4 collapse — open/drop = a layer, comp-path readout  (branch `feat/layer-stack-play`, PR #190)

- **Open / drop = add a layer** `03ec2cf` — the literal collapse. New `open_layer` is the one entry
  (records recents, shows the panel, `add_comp_source`); it backs drag-drop, File > Open, Open Recent,
  and the panel Add button. `handle_drag_and_drop` → a single "Drop to add layer" overlay; the four
  A/B-split drop helpers (`route_dropped_exrs` / `live_dropped_right` / `cursor_targets_right` /
  `global_cursor_pos_points`) + the B-reference menu items removed. `draw_central_canvas`'s gate
  decoupled from `show_layers_panel`. Cap surfaced via `error_msg`. Legacy `open_file` /
  `update_base_layer` kept `#[allow(dead_code)]`. Net −106 lines.
- **Pixel readout in the comp path** `ab63b16` — `sample_pixel` → `pub(crate)`; `draw_comp_central`
  records the topmost drawable layer (`comp_readout`) and, after the draw, samples its source pixels
  under the cursor into the shared `last_hover_pos_img` / `last_sampled_val_a` (honors
  `suppress_sampling` so it blanks during playback); `draw_status_bar` gains a comp row reusing
  `draw_nuke_status_line`. Two pure helpers (`top_sample_source`, `comp_hover_pixel`) unit-tested.
- **Copilot review fixes** `cad0a56` — (1) fold a display-stage salt into the comp-path `render_sig`
  so toggling OCIO on a static frame re-renders (was leaving a stale `display_view`); (2) cache the
  OCIO-off display-encode scene bind group on `OcioTargets` (built in `new`), the twin of
  `scene_bind_group`, instead of a per-dirty-frame `create_bind_group`.
- **Nuke-style current-layer bar + EXR Info restore** — the literal collapse dropped the classic
  single-image control row / EXR Info in the comp path (readout since landed; these had not). Rather
  than reviving `open_file`, restore them *in* the comp path (user decision — Nuke model: load a
  layer, select it, page through its AOVs/channels). New `selected_comp_layer` (Nuke's "current"
  layer) — set on open/add + by clicking a timeline track name (bold when current), resolved with a
  top-of-stack fallback by `active_comp_layer`. `draw_comp_layer_bar` draws a viewport-top row:
  layer picker · the current layer's AOV/pass pulldown (reuses the per-track combo) · R/G/B/A
  isolation. `draw_side_panel` EXR Info now populates from the current comp layer's `CompSource`
  (then the rest of the stack); its pass list routes clicks to that layer's `aov` (not
  `viewer.active_layer`). The `#192` bottom-bar channel toggle is retired (the bar supersedes it) and
  the File ▸ Close Image A/B items removed (`unload` parked `#[allow(dead_code)]`). `LayerStack::iter`
  widened to `DoubleEndedIterator` for top→bottom walks.
- **Anamorphic unsqueeze in the comp path** (#194) — the collapse dropped the #179 unsqueeze (it
  lived only in `viewer::ui`), so anamorphic footage rendered squeezed. `draw_comp_composite` now
  applies `unsqueeze_factor(base_par)` as the same CPU-side horizontal geometry stretch the A/B path
  uses (framing fits the unsqueezed extents; the uniform stretch keeps `comp_hover_pixel` correct
  with no extra term). `base_par` is the bottom drawable layer's header PAR, threaded through
  `draw_comp_central`. An **Unsqueeze** checkbox is surfaced in `draw_comp_layer_bar` for anamorphic
  layers, since the comp path doesn't draw the classic Display ▾ menu.
- **Histogram + Color Sampler + swatch sampling in the comp path** — completes the inspection trilogy
  (readout ✓ / EXR Info ✓ / histogram ✓). `calculate_histogram`'s bin math is extracted to
  `histogram_bins`; a new `calculate_histogram_for(exr_data, layer_idx, disc)` is the comp entry point
  (clears B). The histogram cache key gains a **source discriminator** (`(disc, layer_idx, log)`;
  classic uses `HIST_DISC_CLASSIC`, comp passes its `SourceId`) so switching the current layer/AOV
  recomputes. `draw_side_panel` resolves a per-mode panel source (classic slot-A, or the current comp
  layer's `CompSource` at its AOV) so Color Sampler + Histogram render in comp mode. Shift+Click swatch
  sampling — which lived in `viewer::ui` — is ported into `sample_comp_readout` (checks `shift` only,
  so Shift+Ctrl+Click works). New **View ▸ Info panel** toggle (`show_side_panel`) shows/hides the left
  panel.
- **Side-by-Side in the comp path** (render-retire Slice 2a) — the first compare mode back after the
  collapse. Side A (the composite) accumulates into `rect_a`; side B (the current layer, a *single*
  layer, so no ping-pong needed) is one placed draw in a new `LoadOp::Load` pass-1b
  (`OcioCallback::overlay_draws`), and pass 2 + the blit run once over the combined scene, unchanged.
  **No new GPU targets, pipelines, or bind groups** — superseding the doc's offscreen-per-side plan,
  which assumed both sides were accumulate groups. `draw_comp_composite` takes `arrangement` +
  `CompSideB`; `disp_rect` is the union of both panes (it gates the shader's overscan dim, so
  `rect_a` alone would force side B's opacity to 1); `framing_bounds` /
  `handle_canvas_interaction` swap `CompareMode` for a `side_by_side: bool` so first-paint fit spans
  both panes; `scissor_pts` is `None` and the arrangement is salted into `render_sig`. Per-pane pixel
  readout via the new `last_image_rect_b` + pure `pick_comp_side` (hovering a pane samples *that*
  layer and renames the status row). `Normalize Size` surfaced beside the `Compare:` combo. New pure
  tests: `pick_comp_side`, `comp_side_b_draw`; `side_by_side_layout` is no longer dead code.

- **Wipe + Diff in the comp path** (render-retire Slice 2b) — completes the compare set, so the A/B
  path has nothing user-visible left. Both are **single 2-input draws** (pane A as `tex_a`, pane B as
  `tex_b`, one shared rect, `is_wipe_mode` / `is_diff` doing the work in `fs_main`), which needed
  **no new GPU code at all** — the layer-vs-layer model from 2a had already removed the read/write
  hazard this slice was budgeted for. Wipe rides the accumulate path as a single draw and stays
  colour-managed; Diff keeps the A/B opt-out (immediate `ExrCallback`, empty `ocio_draws`, existing
  early return). `wipe_line_endpoints` extracted pure and shared with the A/B path so they can't
  drift; `handle_wipe_interaction` reused. Because Wipe overlays both layers in one rect, the readout
  splits on the line via the new pure `wipe_side_at` (mirroring `fs_main` exactly) behind a single
  `comp_hover_side`, fed by `ExrViewer::last_wipe`. Follow-up: the Diff + wipe sliders still live
  only in the A/B toolbar, so Slice 3 must re-home rather than delete them.

## Execution checklist — Phase 1  *(complete; kept for reference — resume point is Phase 4 / R4 below)*

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
- **[DONE P1.4c]** `b3bd1d8` — source-key the *decode* forks: `next_want_slot(Slot)`→
  `next_want(SourceId)`, `submit_seq`/`warm_ahead` take `SourceId`, each branching `source ==
  A_SOURCE` (primary transport) vs a follower (`followers[&source]`). New per-source helpers
  (`source_playhead`, `frame_path_for`, `has_source_frame`, `want_list_for`,
  `decode_beauty_only_for`/`decode_proxy_target_for`) replace the `_b` twins. `apply_load_result`'s
  seq routing keys on `res.source`. (The app-level `is_b:bool` open params + `swap_b_frame` display
  stay — that's the A/B compare-UI open, folds in Phase 4.)
- **[DONE P1.4c]** `b3bd1d8` — `pump_decode`'s literal `[(A,0),(B,0),(A,d),(B,d)]` list → an
  ordered `[A] + active_followers` loop over two priority passes (P0 playheads, P1 prefetch).
  `pump_t2`/`pump_t2_b` stay two source-keyed funcs (their one-loop fold deferred — separate
  primary/follower `last_t2_pump` fields; low value).
- **[DONE P1.4c-2]** `8195ed9` — delete `cache::Slot` + the `From<Slot>` bridge. All ~150 sites
  (app.rs, cache.rs tests, the two `render_program` dead-code seams `slot_of`→`source_of`/
  `layer_id_for`) now pass a `SourceId`. The backend (T1 cache, T2 rings, decode/pump) is fully
  `SourceId`-native; A/B is the ≤2-source special case.
- **[TODO — behavior change, independent]** Replace `sync_b_to_a` + `request_b_frame` +
  `map_b_frame`/`map_b_frame_offset` (`playback.rs`) with `Trim::source_frame` per source.
  **DECIDED semantics: BLANK outside a layer's range** (not the old clamp/hold), and **the master
  clock does NOT stall** on a lagging follower (keep `tick_stutter` gating on the primary only).
  Land as its own slice (it changes B's edge behavior + needs the display path to handle a blank
  follower) — not bundled with the mechanical folds.
- `budget.rs` needs no change (already slot-agnostic); `scheduler.rs`, `prefetch.rs`,
  `playback::advance` unchanged.

**Phase 1 backend unification is essentially COMPLETE** (P1.1–P1.4c-2): the decode / T1 cache / T2
ring / pump path are all `SourceId`-native and N-source-shaped, with A/B pinned as the ≤2-source
special case. The only remaining Phase-1 item is the **`map_b_frame`→`Trim::source_frame`** swap — an
independent behavior-change slice (blank-outside-range), landable whenever.

**Phase 2 is DONE** (P2.1–P2.3): added comp sequence layers play on the A-driven transport. See the
Phase 2 section above for the slice-by-slice breakdown.

**The render foundation + timeline UI (R-series) is DONE** (`7ebda89`…`7379fd5`): the comp path
renders standalone in any colour mode, drives the transport, includes the base plate, and the panel
is the Chaos-Player bottom timeline tracks. See the "Render foundation" progress-log block above.

**Resume next → render-retire (Phase 3) Slice 3, the `_b`/`comp_*` deletion.** Open/drop = add a
layer, the comp-path pixel readout, EXR Info, the histogram, and **all four compare arrangements**
(Slices 2a+2b) have landed, so nothing user-visible remains on the A/B path; see the **R4 handoff**
above for the concrete resume steps. In short: delete `emit_mode_draws` / `render_program.rs` /
`CompareMode` / the `_b`/`comp_*` duplication and drop the `#[allow(dead_code)]` on `open_file` /
`update_base_layer` — re-homing the Diff + wipe sliders into the comp bar rather than deleting them.
Layer persistence (`Vec<LayerPersist>`) is independent. Readout fast-follows: the floating cursor
tooltip + the aperture combo in comp mode.
