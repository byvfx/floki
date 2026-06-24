# Layer model — the review-player spine (#103)

> **Status:** design spike landed. The pure model lives in [`src/layer.rs`](../../src/layer.rs)
> with unit tests; no app/GPU/UI wiring yet. Consumers land in the build phases below.
> Parent epic: [#99](https://github.com/byvfx/floki/issues/99).

This is the one structural decision the review-player roadmap calls highest-leverage
(see [`README.md`](./README.md#the-structural-insight-a-layer-model-is-the-spine)). Most of
the ❌/⚠️ gaps — N-way compare, locked-step A/B, per-layer transform, adjustment/brush/text
layers, Back-to-Beauty — collapse into **one abstraction: an ordered stack of placed
sources**. We design and unit-test that model once, headless, before building any of the
features on top of it, so each feature becomes an *arrangement* of the model rather than a
one-off.

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

## The core idea: separate the two axes `CompareMode` conflates

Today, `Slot::{A,B}` + [`CompareMode`](../../src/viewer.rs) hardcode a two-input pipeline, and
`CompareMode` mixes two unrelated concerns into one enum:

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

## How today's behavior maps onto the model

| Today                          | Layer-model expression                              |
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

The T1 ring (`src/cache.rs`) and T2 GPU ring (`src/viewer.rs`) key on `(Slot, frame)`. With the
model they key on **`(LayerId, source_frame)`**:

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

The current diff/composite/wipe paths (GPU shader + CPU fallbacks in `viewer.rs`) take exactly
two inputs. The model turns rendering into: **walk `composite_at(frame)` bottom-to-top, fetch
each `Draw`'s `(source, aov, source_frame)` texture, place it by `transform`, blend it by
`blend`/`opacity`; apply each `Adjust` to the accumulated result; finally lay the visible
groups out per `Layout`.** Two visible layers + `Layout::Wipe`/`Grid` reproduce the existing
wipe/side-by-side exactly, so the generalization is additive, not a rewrite.

---

## Build phases (filed from this spike)

The spike is the model only. Wiring is sequenced so each step ships without regressing the
single-image or A/B paths:

1. **Adopt the model behind today's UI** — represent the current A/B + `CompareMode` as a
   2-layer `LayerStack` internally, keeping the existing controls. Pure refactor, no new UX.
   **Landed ([#114](https://github.com/byvfx/floki/issues/114)).** `src/render_program.rs`
   reconfigures the viewer's held 2-layer stack from `compare_mode` / `blend_mode` /
   `active_layer` each frame and resolves it via `LayerStack::composite_at` into a
   `RenderProgram`; both the GPU and CPU draw paths now dispatch on that program instead of
   matching `CompareMode` inline. The two layers carry stable `LayerId`s (the `Slot ⇄ LayerId`
   cache-key seam). Arrangement geometry (wipe/side-by-side) and the diff-matte inspection stay
   viewer-side this phase; the N-input render lands with #104.
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
