This is the design contract for sequence playback (#7), written before the code and kept so the target does not drift. The file in the repo is [`docs/playback/sequence-playback.md`](https://github.com/byvfx/floki/blob/main/docs/playback/sequence-playback.md). What the app does today is [Playback](Playback). The two contracts underneath are [Memory](Memory-contract) and [Concurrency](Concurrency-contract).

The parent index, phase plan, and the thread-boundary fact (the worker returns CPU `ExrData`; a frame is not playback-ready until a texture exists) are [`docs/playback/README.md`](https://github.com/byvfx/floki/blob/main/docs/playback/README.md). The hardening plan and the soak checklist stayed off this site's nav; they are still in [`docs/playback/`](https://github.com/byvfx/floki/tree/main/docs/playback).

## Detection

`src/sequence.rs` takes the file stem, finds the last contiguous digit run, and groups by `(prefix, suffix)` inside one directory. Mixed padding is allowed (`9`, `10`, `100`). Sort is numeric, not lexical. Holes are the missing numbers in `[min..=max]`. The player holds the previous frame on a hole and never stalls.

Opening one EXR that has at least two matching siblings turns sequence mode on. A lone image stays a still.

## Transport the contract specified

State lives on `ExrApp`. Playback drives a session-preserving swap. It does not reset the viewer: zoom, pan, exposure, channel, compare, and annotations carry across frames.

Persisted prefs are target fps, loop mode, direction, and pacing. The sequence, the current frame, and the in/out handles reset per open.

The clock uses `ctx.request_repaint_after` and corrects drift from an absolute frame-start anchor, not "now plus period". Target fps and measured fps are both shown.

Pacing, when decode falls behind:

- **Stutter / play-every-frame — the default.** Advance only when the next frame is resident. Effective fps drops. Nothing is skipped.
- **Drop-frames — a toggle.** The clock advances on wall time and skips to the latest resident frame.

The UI is a scrubber over the in/out range, holes drawn distinctly, play / pause / stop / step / reverse / loop, and an editable target fps next to the measured fps.

## What moved after the contract was written

The contract's compare section ships "A plays, B holds" on the old two-slot model, with cache keys on `(frame, slot)`, and treats a GPU texture ring (T2) as the thing the clock waits for. Both of those have since changed, and the updated docs are the ones to follow:

- The slot-A / slot-B display path is gone (#277, retired in the layer-stack unification). A compare is the current layer against the `vs:` layer. See [Comparison](Comparison) and [Layer model](Layer-model).
- The T2 pre-upload ring was removed in #299. Textures are one per source, built on the upload worker pool (#202). The clock still must not block the UI thread on a decode. See [Memory contract](Memory-contract).
- Space is context-gated, as the contract decided: a sequence loaded means Space is play/pause; otherwise Space stays blink. See [Keyboard shortcuts](Keyboard-shortcuts).

Scrub still bumps the epoch, paints a proxy when the full frame is not resident, and ensures T1 residency before sampling comes back (INV-SAMPLE).
