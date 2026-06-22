# floki as a Review Player — Chaos Player parity roadmap

> **Status:** planning. Tracking epic: [#99](https://github.com/byvfx/floki/issues/99).
> **Decision (2026-06-22):** build the core review features **in egui first**; the Qt
> port ([#44](https://github.com/byvfx/floki/issues/44)) is deferred until this scope
> ships. Porting onto a moving architecture means re-porting it.

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
| Smart cache, instant hi-res playback | ✅ | #7 Phases 0–5: T0–T3 ring, decode-ahead (#57), GPU pre-upload (#56) |
| OCIO 2 + LUT | ✅ | #24 / #76 (display transform baked to a 3D LUT) |
| EXR multi-channel / multi-part / AOV / channels | ✅ | core strength |
| Compare: **wipe / side-by-side / diff / composite** + blend modes | ✅ | `CompareMode` in `viewer.rs` |
| Color sampler, histogram, contact-sheet thumbnails | ✅ | |
| Exposure + gamma (incl. in OCIO view) | ✅ | gamma-in-OCIO fixed in #93/#95 |
| Transport: play/pause/stop/step, loop, ping-pong, in/out trim, stutter **+ drop-frames** pacing | ✅ | #7 Phase 5 (PR #97) |
| Color corrections: contrast / hue / saturation / white balance / **soft-clip** | ⚠️ | only exposure + gamma today → **#102** |
| Annotations: animated **brush-sequence** + **text** layers | ⚠️ | static annotations/swatches exist → layer model (**#103**) |
| Compare count | ⚠️ | 2-slot (A/B); Chaos does up to 4 → **#104** |
| **Live render-watch** | ❌ | **#101** |
| **MP4 / ProRes export** | ❌ | **#8** |
| **N-way compare** (up to 4) | ❌ | **#104** |
| **Layer / composite stack** (per-layer xform, infinite canvas, layer types) | ❌ | **#103** |
| Stereoscopic playback | ❌ | backlog **#105** |
| Format breadth: .VRIMG / DPX / HDR / video in | ❌ | backlog **#105** |
| HTTP-server broadcast; CLI / macros | ❌ | backlog **#105** |
| Basic keying; export comp → Nuke/AE | ❌ | backlog **#105** |

floki already covers a lot of the surface — caching, OCIO, channels, multi-mode compare,
color tools. The deltas that make it *feel* like Chaos Player are concentrated in a
handful of areas below.

---

## The structural insight: a layer model is the spine

Most of the ❌ / ⚠️ rows collapse into **one abstraction — a layer stack**. floki's
current `(Slot::A / Slot::B)` slots plus `CompareMode` / `Composite` are a hardcoded
**two-layer special case**. Generalized to an ordered `Vec<Layer>`:

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

The existing composite / wipe / diff shaders generalize from 2 inputs to N. Once the
layer model exists, these stop being separate features and become **arrangements**:

- **N-way compare** (up to 4) = N stacked source layers + a grid/wipe layout (**#104**).
- **Locked-step A/B** = 2 layers with locked playheads (**#98**).
- **Back-to-Beauty** = additive AOV layers from a multi-part EXR.
- **Adjustment / Brush / Text** layers = non-source layer types over the stack.

So we **design the layer model once** (data model, render-graph generalization, cache
keying `(layer, frame)`, budget split across layers) and avoid building N-way compare and
the layer stack as independent one-offs. This is the highest-leverage item, and like the
playback phases it is pure-logic-first / headless-testable before any UI.

---

## Phased roadmap (egui-first)

Ordered lowest-risk-highest-value, each shippable on its own without regressing the
single-image or playback paths. Epic: **#99**.

1. **Playback hardening — #100.** Validate #7 Phases 0–5 on real Win + Mac footage
   (the "make sure playback works well" gate). Folds in **#94** (proxy controls + on-disk
   cache) and **#75** (loading-full-res indicator).
2. **Live render-watch — #101.** Auto-load new/updated frames as a render writes them;
   invalidate cache on mtime/size change; live timeline range. *Toolkit-independent.*
3. **MP4 / ProRes export — #8.** Encode the trimmed, color-managed sequence out.
   *Toolkit-independent.*
4. **Real-time color-correction suite — #102.** contrast / hue / saturation / white
   balance / soft-clip, in-shader during playback; correct placement vs the OCIO pass;
   CC params as a reusable struct (groundwork for the Adjustment Layer).
5. **Layer-model design spike — #103**, then build on it:
   - **N-way compare — #104.**
   - per-layer transform + infinite canvas, **adjustment / brush / text** layer types
     (filed from the spike).
   - **Locked-step A/B — #98.**

**Parity backlog — #105** (parked until promoted): stereoscopic, .VRIMG / DPX / HDR /
video input, HTTP-server broadcast, CLI + macros, keying, Nuke/AE comp export.

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
