# floki as a Review Player — Chaos Player parity roadmap

> **Status:** the layer-stack unification has shipped on `main`. Tracking epic:
> [#99](https://github.com/byvfx/floki/issues/99). The `LayerStack` is the only model
> floki renders and plays — open and drop add a layer, the five arrangements
> (Stacked, Wipe, Side-by-Side, Diff, Blink) are comp-path native, and the stack
> persists. The record of that work is [`unification-plan.md`](./unification-plan.md).
> **Decision (2026-06-22):** build the core review features **in egui first**; the Qt
> port ([#44](https://github.com/byvfx/floki/issues/44)) stays deferred. Porting onto a
> moving architecture means re-porting it.

floki is already a fast, color-managed GPU EXR viewer with sequence playback. The next
arc turns it into a **Chaos Player-class review tool**. This document is the durable
reference: the north-star feature set, an honest gap analysis against what floki has
today, the one structural idea that ties most of it together, and the phased plan.

---

## North star: Chaos Player

[Chaos Player](https://www.chaos.com/player) (the successor to *Pdplayer*) is a
professional image-sequence review player. Feature inventory compiled from the product
page, [Chaos docs](https://documentation.chaos.com/space/PLAYER), and third-party
coverage ([CG Channel](https://www.cgchannel.com/2022/11/chaos-releases-chaos-player/)).
Where the official docs are JS-rendered, a few specifics (e.g. the exact blend-mode
list) are unconfirmed and marked as such.

**Playback & cache**
- High-perf playback of high-res 64-bit (32-bit-float) sequences, **mono or stereoscopic**.
- Smart cache → instant hi-res playback; **live render-watch**: new/updated frames load
  instantly while a render is still writing them.
- J/K/L transport, frame-by-frame, timeline.

**Formats**
- OpenEXR (**multi-channel + multi-part**), **.VRIMG**, **HDR**, **DPX**, standard image
  + **video** formats. Render-element / AOV display; **Back-to-Beauty** composite from
  multi-channel EXR.
- Export: **MP4**; export the comp to **Nuke `.nk`** and **After Effects `.jsx`**.

**Color**
- **OCIO v2** + LUTs.
- Real-time corrections *while playing*: exposure, gamma, contrast, hue, saturation,
  color balance, **soft-clip for HDR**.
- **Adjustment Layer** — apply one correction across many layers; per-format color mapping.

**Compare**
- A/B **wipe**, compare **up to 4 versions/inputs**, swipe + zoom to tech-check.
- Side-by-side; **Contact Sheet** grid of variants/tracks.

**Layers / compositing (the spine of the app)**
- Stack multiple sequences; composite / edit / cut **in real time during playback**.
- Layer types: sequence/image, **Brush**, **Brush Sequence** (animated paint), **Text**,
  **Adjustment**.
- **Per-layer transform**: scale independently, position, aspect ratio (per-layer or
  global), **infinite workspace** canvas. Blend modes (additive Back-to-Beauty + more,
  exact set unconfirmed), animated fades, trim/cut.
- Basic **keying** toolset for on-set comp.

**Review / pipeline**
- Brush + text markup, animated brush-sequence notes, remote collaboration.
- Built-in **HTTP server** to broadcast to team/clients.
- Extensive **command-line** + macro/hotkey command binding; workspace layouts, HiDPI.

**Platforms / licensing** — Win10+, Linux (CentOS 7+), macOS 10.15+; subscription.

---

## Where floki stands today

| Area | floki | Notes |
|------|-------|-------|
| Smart cache, instant hi-res playback | ✅ | #7 Phases 0–5: T1 RAM ring, decode-ahead (#57), on-disk proxy cache. The T2 GPU pre-upload ring was removed in #299 |
| OCIO 2 + LUT | ✅ | #24 / #76 (display transform baked to a 3D LUT) |
| EXR multi-channel / multi-part / AOV / channels | ✅ | core strength |
| Compare: **Stacked / Wipe / Side-by-Side / Diff / Blink** + per-layer blend modes | ✅ | `Arrangement` in `src/layer.rs` |
| Color sampler, histogram, contact-sheet thumbnails | ✅ | |
| Exposure + gamma (incl. in OCIO view) | ✅ | gamma-in-OCIO fixed in #93/#95 |
| Transport: play/pause/stop/step, loop, ping-pong, in/out trim, stutter **+ drop-frames** pacing | ✅ | #7 Phase 5 (PR #97) |
| **Live render-watch** | ✅ | #101 (off-thread scan #145) |
| Color corrections: contrast / hue / saturation / white balance / **soft-clip** | ⚠️ | only exposure + gamma today → **#102** |
| Annotations: animated **brush-sequence** + **text** layers | ⚠️ | static annotations/swatches exist → layer model (**#103**) |
| Compare count | ⚠️ | Any number of layers in the stack; a compare is still two panes (current vs `vs:`). N-way (up to 4) is **#104** |
| **MP4 / ProRes export** | ❌ | **#8** |
| **N-way compare** (up to 4) | ❌ | **#104** |
| **Layer / composite stack** | ✅ | Ordered stack on `main`: per-layer blend, opacity, solo, trim, AOV, pixel aspect, timeline tracks. Brush / text / adjustment types and infinite-canvas editing are still open (**#103** follow-ups) |
| Stereoscopic playback | ❌ | backlog **#105** |
| Format breadth: .VRIMG / DPX / HDR / video in | ❌ | backlog **#105** |
| Opening a path from the CLI (`floki <path>`) | ✅ | Same entry as File ▸ Open and drag-and-drop |
| HTTP-server broadcast; macros | ❌ | backlog **#105** |
| Basic keying; export comp → Nuke/AE | ❌ | backlog **#105** |

floki already covers a lot of the surface — caching, OCIO, channels, multi-mode compare,
color tools. The deltas that make it *feel* like Chaos Player are concentrated in a
handful of areas below.

---

## The structural insight: a layer model is the spine

Most of the remaining ❌ / ⚠️ rows still collapse into **one abstraction — a layer
stack**. That stack has shipped: the old `(Slot::A / Slot::B)` slots and `CompareMode`
are gone, and an ordered `Vec<Layer>` is the model (`src/layer.rs`). The shape:

```
Layer = {
    source: Sequence | Image,
    transform: { scale, position, aspect },   // infinite-canvas placement
    blend: BlendMode,
    opacity: f32,
    enabled / solo: bool,
    trim: (in, out),                           // per-layer, lengths may differ
}
```

The composite / wipe / diff shaders are arrangements of that stack. What is still
ahead of the model:

- **N-way compare** (up to 4) = N stacked source layers + a grid/wipe layout (**#104**).
- **Locked-step A/B** = 2 layers with locked playheads (**#98**).
- **Back-to-Beauty** = additive AOV layers from a multi-part EXR.
- **Adjustment / Brush / Text** layers = non-source layer types over the stack.

The layer stack itself is no longer a one-off beside compare: it is the model. N-way
compare and the non-image layer types are the pieces still to build on it.

> **The model is what `main` runs** — see [`layer-model.md`](./layer-model.md) and
> [`unification-plan.md`](./unification-plan.md). `src/layer.rs` splits arrangement
> (`Arrangement`) from blend, and the app, GPU path, and timeline UI all consume that
> stack. N-way compare and the non-image layer types are still ahead of it.

---

## Phased roadmap (egui-first)

Ordered lowest-risk-highest-value, each shippable on its own without regressing the
single-image or playback paths. Epic: **#99**.

1. **Playback hardening — #100.** Validate #7 Phases 0–5 on real Win + Mac footage
   (the "make sure playback works well" gate). Folds in **#94** (proxy controls + on-disk
   cache) and **#75** (loading-full-res indicator).
2. **Live render-watch — #101.** ✅ **Shipped.** Auto-loads new/updated frames as a
   render writes them (off-thread directory scan, #145); invalidates cache on mtime/size
   change; live timeline range. *Toolkit-independent.*
3. **MP4 / ProRes export — #8.** Encode the trimmed, color-managed sequence out.
   *Toolkit-independent.*
4. **Real-time color-correction suite — #102.** contrast / hue / saturation / white
   balance / soft-clip, in-shader during playback; correct placement vs the OCIO pass;
   CC params as a reusable struct (groundwork for the Adjustment Layer).
5. **Layer stack — #103.** ✅ **Shipped** on `main` (see the status note above). Still
   to build on it:
   - **N-way compare — #104.**
   - per-layer transform + infinite canvas, **adjustment / brush / text** layer types.
   - **Locked-step A/B — #98.**

**Parity backlog — #105** (parked until promoted): stereoscopic, .VRIMG / DPX / HDR /
video input, HTTP-server broadcast, macros, keying, Nuke/AE comp export. Opening a
path from the CLI has shipped (`floki <path>`).

---

## Relationship to the Qt port (#44)

The earlier roadmap framed the Qt port as the gate for everything. That is reversed here:
**ship review value in egui first, port later.** Items 1–4 above are entirely
toolkit-independent. Items under the layer model (5) are UI-heavy and are exactly the
pieces a Qt port would benefit from — but the *model* is toolkit-independent and should be
designed and unit-tested in egui regardless, so the port (if/when it happens) carries a
stable architecture rather than chasing one. See the playback contracts in
[`docs/playback/`](../playback/) for the same pure-logic-first discipline.

---

## Sources

- [Chaos Player — product page](https://www.chaos.com/player)
- [Chaos Player — documentation](https://documentation.chaos.com/space/PLAYER)
- [Chaos Player — Layers (docs)](https://docs.chaos.com/display/PLAYER/Layers)
- [CG Channel — Chaos releases Chaos Player](https://www.cgchannel.com/2022/11/chaos-releases-chaos-player/)
