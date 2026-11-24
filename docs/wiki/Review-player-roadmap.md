Floki is already a color-managed GPU viewer with sequence playback. This roadmap is the gap between that and a Chaos Player-class review tool. The file in the repo is [`docs/review-player/README.md`](https://github.com/byvfx/floki/blob/main/docs/review-player/README.md). Tracking epic: [#99](https://github.com/byvfx/floki/issues/99). The spine under it is the [layer model](Layer-model).

Decision recorded 2026-06-22: build the core review features in egui first. The Qt port ([#44](https://github.com/byvfx/floki/issues/44)) stays deferred. Porting onto a moving architecture means porting it twice.

The layer stack has shipped. Open and drop add a layer. The five arrangements (Stacked, Wipe, Side-by-Side, Diff, Blink) are comp-path native, and the stack persists. The record of that unification is [`docs/review-player/unification-plan.md`](https://github.com/byvfx/floki/blob/main/docs/review-player/unification-plan.md), which is not on this site's nav.

## Where things stand

The table matches the roadmap's own inventory. A check is shipped. A warning is partial. A cross is not in the app.

| Area | State | Notes |
|---|---|---|
| Smart cache and hi-res playback | Shipped | T1 RAM ring, decode-ahead, on-disk proxy cache. The T2 GPU pre-upload ring was removed in #299. |
| OCIO 2 and LUT | Shipped | Display transform, optionally baked to a 3D LUT. |
| EXR multi-channel, multi-part, AOV | Shipped | |
| Stacked, Wipe, Side-by-Side, Diff, Blink, per-layer blend | Shipped | `Arrangement` in `src/layer.rs`. |
| Sampler, histogram, contact sheet | Shipped | |
| Exposure and gamma, including in the OCIO view | Shipped | Gamma-in-OCIO is #93 / #95. |
| Transport: play, pause, stop, step, loop, ping-pong, in/out, stutter and drop-frames | Shipped | |
| Live render-watch | Shipped | #101, off-thread scan #145. |
| `floki <path>` | Shipped | Same entry as File ▸ Open and drag-and-drop. |
| Ordered layer stack | Shipped | Blend, opacity, solo, trim, AOV, pixel aspect, timeline tracks. Brush, text, adjustment types, and infinite-canvas editing are still open. |
| Contrast, hue, saturation, white balance, soft-clip | Partial | Only exposure and gamma today. [#102](https://github.com/byvfx/floki/issues/102). |
| Annotations | Partial | Static arrows, boxes, pen, and text. Animated brush-sequence notes are not layers yet. |
| Compare count | Partial | Any number of layers in the stack. A compare is still two panes (current vs `vs:`). N-way, up to 4, is [#104](https://github.com/byvfx/floki/issues/104). |
| MP4 / ProRes export | Not shipped | [#8](https://github.com/byvfx/floki/issues/8). |
| N-way compare | Not shipped | #104. |
| Stereoscopic playback, VRIMG / DPX / HDR / video input, HTTP broadcast, macros, keying, Nuke / AE export | Not shipped | Backlog [#105](https://github.com/byvfx/floki/issues/105). |

## Phases

Ordered so each one can ship without regressing stills or playback.

1. **Playback hardening — #100.** Validate playback on real Windows and macOS footage. Folds in proxy controls (#94) and a loading-full-res indicator (#75).
2. **Live render-watch — #101.** Shipped.
3. **MP4 / ProRes export — #8.** Encode the trimmed, color-managed sequence. Not shipped.
4. **Color-correction suite — #102.** Contrast, hue, saturation, white balance, soft-clip, in-shader during playback, placed correctly against the OCIO pass.
5. **Layer stack — #103.** Shipped. Still to build on it: N-way compare (#104), per-layer transform and infinite canvas, adjustment / brush / text layers, and locked-step A/B of two layers with equal offsets (#98).

Most of the remaining gaps are arrangements of the layer stack, not separate viewers. N-way compare is N source layers plus a grid. Back-to-Beauty is additive AOV layers of one source. An adjustment is a non-source layer over the stack.

Chaos Player is the north star (sequence review, OCIO, layers, wipe, contact sheet, live render-watch). The product page and the Chaos docs are linked from the repo file. A few Chaos specifics, including the exact blend-mode list, are marked unconfirmed there and are not repeated here as Floki features.
