The comp-layer stack is what Floki renders and plays. The note in the repo is [`docs/review-player/layer-model.md`](https://github.com/byvfx/floki/blob/main/docs/review-player/layer-model.md). Status there: shipped on `main`. Parent epic: [#99](https://github.com/byvfx/floki/issues/99). The roadmap around it is [Review player roadmap](Review-player-roadmap).

## Two meanings of layer

A **logical layer** is an AOV inside one EXR (beauty, diffuse, specular), grouped by channel-name prefix. That meaning did not change.

A **comp layer** (`layer::Layer` in [`src/layer.rs`](https://github.com/byvfx/floki/blob/main/src/layer.rs)) is a placed source in an ordered stack. It references a source and selects one of that source's AOVs. In the code and in these docs, an unqualified "layer" is a comp layer. The EXR AOV is a logical layer.

## Arrangement is not blend

The old `CompareMode` mixed two questions into one enum, and `Slot::A` / `Slot::B` hardcoded two inputs. Both are gone.

| Concern | Type | Notes |
|---|---|---|
| Where the inputs sit | `Arrangement` in `src/layer.rs` — Stacked, Wipe, Side-by-Side, Diff, Blink | This is the live viewport axis. `Layout` (Stack / Wipe / Grid) is kept and unused. |
| How stacked inputs combine | `BlendMode` (Over, Under, Add, Multiply, Screen) | Per layer. The integer encoding is shared with `gpu/shader.wgsl`. Not redefined. |

Diff stays its own operator. It is not a blend. It consumes the two visible layers the arrangement exposes. See [Comparison](Comparison).

## What a layer holds

`LayerStack` is a `Vec<Layer>` from bottom to top, plus `next_id`. Each `Layer` has a stable `LayerId` (reordering does not churn caches), a name, a source (`Image { source, aov }` or `Adjustment`), a transform (position and independent x/y scale), blend, opacity, enabled, solo, and a trim (`in_point`, `out_point`, `offset` in source-frame space).

Pure operations, all unit-tested:

- `Trim::source_frame(global)` maps a global frame to this layer's source frame (`source = global + offset`), or `None` when the layer is blank there. Out of span returns `None` instead of an out-of-range fetch.
- `LayerStack::visible()` is the soloed layers if any is soloed, otherwise every enabled layer.
- `LayerStack::composite_at(global)` is the bottom-to-top program for a frame. A step is either a `Draw` (source, aov, source frame, transform, blend, opacity) or an `Adjust`. Blank layers are dropped.
- `Layer::cache_key(source_frame)` is `(LayerId, source_frame)`.

The model never decodes and never touches disk. It says what to fetch. The existing decode and upload path does the fetch.

The T1 ring keys on `(SourceId, frame)`. Two layers that point at the same source frame share a cache entry. `LayerId` is stable across a reorder, so moving a row does not throw away decoded frames. The contact-sheet thumbnail cache keys the same way.

Rendering walks `composite_at` bottom to top, places each draw by its transform, and blends by blend and opacity. The viewport is GPU-only.

## Still ahead of the model

The unification on `main` replaced the intermediate `render_program.rs` path. Still filed, not built:

1. N-way compare — [#104](https://github.com/byvfx/floki/issues/104). N source layers and a grid.
2. Per-layer transform editing on an infinite canvas, then adjustment, brush, and text layer types.
3. Locked-step playback of two layers with equal `offset` — [#98](https://github.com/byvfx/floki/issues/98).
4. The contact-sheet cache keyed `(LayerId, source_frame)` as [#112](https://github.com/byvfx/floki/issues/112) described it. The overhaul is designed against this key.

The user-facing controls that already sit on this model are [Layers and compositing](Layers-and-compositing).
