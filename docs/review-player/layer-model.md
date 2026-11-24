# Layer model — the review-player spine (#103)

> **Status:** shipped on `main`. The model in [`src/layer.rs`](../../src/layer.rs) is
> what the app, the GPU composite, and the timeline UI render and play. Viewport
> presentation is [`Arrangement`](../../src/layer.rs) (Stacked, Wipe, Side-by-Side,
> Diff, Blink). `src/render_program.rs` and `CompareMode` are gone — see
> [`unification-plan.md`](./unification-plan.md).
> Parent epic: [#99](https://github.com/byvfx/floki/issues/99).

This is the one structural decision the review-player roadmap calls highest-leverage
(see [`README.md`](./README.md#the-structural-insight-a-layer-model-is-the-spine)). The
ordered stack of placed sources is what `main` renders and plays. The remaining gaps —
N-way compare, locked-step A/B, per-layer transform, adjustment/brush/text layers,
Back-to-Beauty — are arrangements of that model rather than separate one-offs.

---

## Terminology: two kinds of "layer"

floki already uses "layer" for **`LogicalLayer`** — an AOV / channel-group *within* one EXR
(beauty, diffuse, specular…), selected today by `ExrViewer::active_layer`. That meaning is
unchanged.

The review-player **comp layer** (`layer::Layer`) is different: a *placed source* in an
ordered stack, à la Chaos Player / Nuke. A comp layer *references* a source and *selects one
of its AOVs*. In code and docs: unqualified "layer" = comp layer; the EXR AOV is always
"logical layer".

---

## The core idea: separate the two axes `CompareMode` conflated

The old `Slot::{A,B}` slots and `CompareMode` hardcoded a two-input pipeline, and
`CompareMode` mixed two unrelated concerns into one enum. Both are deleted. The split
that replaced them:

- **Arrangement** — *where* the visible inputs are placed (one viewport, a wipe split, a grid).
- **Blend** — *how* stacked inputs combine (Over/Add/Multiply…).

The model splits them:

| Concern      | Model type                          | Notes                                                |
|--------------|-------------------------------------|------------------------------------------------------|
| Arrangement  | `layer::Layout` (Stack/Wipe/Grid)   | stack-wide; independent of blend                     |
| Blend        | `viewer::BlendMode` (**reused**)    | per layer; **single source of truth** for the shader |

`BlendMode` is deliberately *not* redefined — its integer encoding is shared with
`gpu/shader.wgsl`, so the generalized N-input compositor reuses the exact same operators.

---

## Data model (`src/layer.rs`)

```text
LayerStack { layers: Vec<Layer> (bottom→top), layout: Layout, next_id }

Layer {
    id: LayerId,                 // stable; survives reordering → caches don't churn
    name: String,
    source: LayerSource,         // Image { source: SourceId, aov } | Adjustment
    transform: Transform,        // infinite-canvas: position + independent x/y scale
    blend: BlendMode,            // reused from the viewer
    opacity: f32,
    enabled: bool,
    solo: bool,                  // solo overrides enabled (the SingleA/B isolation)
    trim: Trim,                  // { in_point, out_point, offset } in source-frame space
}
```

Key pure operations (all unit-tested):

- **`Trim::source_frame(global) -> Option<u32>`** — maps a global timeline frame to this
  layer's source frame (`source = global + offset`), or `None` when the layer is blank there.
  Locked-step A/B is `offset == 0` on both layers; a staggered compare uses a nonzero offset.
  Out-of-span returns `None` instead of an out-of-range fetch (no underflow, no bad decode).
- **`LayerStack::visible()`** — the layers that render: only soloed layers if any is soloed,
  else every enabled layer.
- **`LayerStack::composite_at(global) -> Vec<Step>`** — the resolved, bottom-to-top render
  program for a frame. `Step` is `Draw { source, aov, source_frame, transform, blend, opacity }`
  for image layers, or `Adjust { id, opacity }` for adjustment layers. Blank layers are dropped.
- **`Layer::cache_key(source_frame) -> (LayerId, u32)`** — the generalization of the cache key
  (see below).

`SourceId` / `LayerId` are opaque handles. The model **never decodes or touches disk** — it
references sources owned by the app's loader/cache and returns *what to fetch*, leaving the how
to the existing decode/upload pipeline.

---

## How the old A/B modes map onto the model

`CompareMode` is deleted. The live viewport axis is `Arrangement` in `src/layer.rs`
(Stacked, Wipe, Side-by-Side, Diff, Blink), not `Layout` (kept, unused). The table is
the mapping the model was designed against:

| Old A/B mode                   | Layer-model expression                              |
|--------------------------------|-----------------------------------------------------|
| `Slot::A` / `Slot::B`          | stack layers 0 / 1                                  |
| `active_layer`                 | the active layer's `LayerSource::Image.aov`         |
| `CompareMode::SingleA/SingleB` | one `solo` layer                                    |
| `CompareMode::Composite`       | ≥2 enabled layers, `Layout::Stack`, per-layer blend |
| `CompareMode::Wipe`            | 2 visible layers, `Layout::Wipe { position }`       |
| `CompareMode::SideBySide`      | 2 visible layers, `Layout::Grid { cols: 2 }`        |
| `CompareMode::DiffMatte`       | a diff *operator* over 2 visible layers (unchanged) |
| N-way compare (#104)           | N visible layers, `Layout::Grid`                    |
| Locked-step A/B (#98)          | 2 layers, equal `offset` (advance together)         |
| Back-to-Beauty                 | additive AOV layers of one `SourceId`               |

`DiffMatte` stays a dedicated operator (it is not a blend); it simply consumes the two visible
layers the arrangement exposes.

---

## Cache keying generalization

The T1 ring (`src/cache.rs`) keys on `(SourceId, frame)`. (The T2 GPU ring was removed
in #299.) The model's cache key is **`(LayerId, source_frame)`**:

- `Slot::A`/`B` → `LayerId` (now N inputs, not 2).
- `frame` → the layer's *resolved source frame* (post-trim/offset), so two layers pointing at
  the same source at the same source frame share a cache entry, and a per-layer trim/offset
  decode is keyed correctly.
- `LayerId` is **stable across reordering**, so moving a layer in the stack never invalidates
  its decoded/uploaded frames.

The per-layer thumbnail cache in the contact-sheet overhaul (#112) keys the same way, which is
why that work and this model are designed together rather than as one-offs.

The VRAM/RAM budgets (`src/budget.rs`) split across visible layers: `max_t2` per-frame cost
becomes per-(layer,frame); the scheduler's look-ahead (`src/scheduler.rs::want_list`) runs per
visible source.

---

## Render-graph generalization

The viewport is GPU-only (`viewer.rs` / `gpu/shader.wgsl`; `main.rs` exits when no
adapter offers `FLOAT32_FILTERABLE`). The model turns rendering into: **walk `composite_at(frame)` bottom-to-top, fetch
each `Draw`'s `(source, aov, source_frame)` texture, place it by `transform`, blend it by
`blend`/`opacity`; apply each `Adjust` to the accumulated result; finally lay the visible
groups out per `Layout`.** Two visible layers + `Layout::Wipe`/`Grid` reproduce the existing
wipe/side-by-side exactly, so the generalization is additive, not a rewrite.

---

## Build phases (filed from this spike)

The spike started as the model only. Phase 1 has since been replaced by the unification
on `main` (see the status note). Later phases are still ahead:

1. **Adopt the model behind the UI.** **Landed, and since superseded by the unification
   on `main`** ([#114](https://github.com/byvfx/floki/issues/114), then the #99 render
   retire). The intermediate `src/render_program.rs` module is deleted. What remains is
   [`Arrangement`](../../src/layer.rs) next to [`LayerStack::composite_at`](../../src/layer.rs):
   the comp path walks that program, and wipe / side-by-side / diff / blink geometry stays
   in `viewer.rs`. The N-input render is still #104.
2. **N-way compare — [#104](https://github.com/byvfx/floki/issues/104).** N source layers +
   `Layout::Grid`; generalize the cache key and the scheduler look-ahead across visible layers.
3. **Per-layer transform + infinite canvas**, then the **adjustment / brush / text** layer
   types (the latter filed as follow-ups once the CC suite #102 lands the adjustment params).
4. **Locked-step A/B — [#98](https://github.com/byvfx/floki/issues/98).** Two layers with equal
   `offset`, advancing together.
5. **Contact-sheet per-layer thumbnail cache — [#112](https://github.com/byvfx/floki/issues/112)**
   keyed `(LayerId, source_frame)`.

---

## Sources / related docs

- [`README.md`](./README.md) — Chaos Player parity roadmap and gap analysis.
- [`docs/playback/`](../playback/) — the same pure-logic-first discipline for #56/#57/#7.
