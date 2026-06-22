# #7 — Sequence playback

> Status: design contract, built on the [memory](memory-contract.md) (#56) and
> [concurrency](concurrency-contract.md) (#57) contracts. Detection lands in Phase 1; transport +
> clock in Phase 2; caching/decode-ahead in Phases 3–4; A/B + polish in Phase 5.

## Detection — `src/sequence.rs` (pure logic)

Reuse the `tools.rs` enumeration shape (`read_dir` + case-insensitive ext filter). Add frame parsing
that does not exist today:

- Take the file **stem**, find the **last contiguous digit run** = the frame field →
  `(prefix, digits, suffix)`. Handles `name.0001.exr`, `name_0001.exr`, `name0001.exr`.
- **Group** by `(prefix, suffix)` within a directory; tolerate mixed padding (`9 → 10 → 100`).
- **Sort numerically** — the existing lexical `sort()` is wrong for unpadded frames
  (`frame9 < frame10 < frame100`).
- **Detect holes:** sorted numbers vs `[min..=max]`. The player holds the previous frame / shows a
  placeholder on a hole — it **never stalls**.

**Entry contract:** opening a single EXR with **≥2 matching siblings** enables sequence mode; a lone
image → today's behavior, unchanged (no regression).

**Output:** `Sequence { frames: Vec<PathBuf>, range: (min, max), holes: Vec<u32> }`. Fully
tempfile-unit-testable, no GPU.

## App-level `Playback` state

Lives on `ExrApp` (the viewer stays per-image and is **never reset** by playback — playback drives
`swap_image_data`, **not** `reset_viewer_session`):

```
Playback {
    sequence, current_frame, frame_range (in/out), fps_target,
    state { Stopped | Playing | Paused }, direction { Fwd | Rev },
    loop_mode { Once | Loop | PingPong }, epoch: Arc<AtomicU64>,
    last_tick, intent (for #56 eviction), sampling_suppressed (for INV-SAMPLE),
}
```

**serde split** (like the existing `ExrApp` split): persist prefs (`fps` / `loop` / `direction` /
`pacing`); `#[serde(skip)]` the runtime `sequence` / handles / `current_frame`.

## Frame clock

egui is idle-driven → use **`ctx.request_repaint_after(period)`** (the same pattern as load polling
and the 1 s status refresh in `app.rs`). Correct drift by computing the next deadline from an
**absolute frame-start anchor**, not "now + period." Track rolling actual-fps; show target-vs-actual.

## Pacing when decode can't keep up (selectable)

- **Stutter / play-every-frame — default.** A review tool wants every frame: advance only when the
  next frame's T2 is resident; effective fps drops; **nothing is skipped**.
- **Drop-frames — toggle.** The clock advances on wall-time; skip to the latest resident frame.

The clock **never blocks the UI thread** either way.

## Transport UI (app-level, near the status bar)

- Scrubber / timeline over `frame_range`, with **holes rendered distinctly** and in/out handles
  (drag = scrub = **P0** + epoch bump).
- play / pause / stop / step ±1 / jump-in-out / reverse / loop-cycle.
- Frame counter; editable **target fps** + **measured fps**.

## Driving the swap

Advancing = take the resident `Arc<ExrData>` for the target frame and call
`swap_image_data(data, /*is_b=*/false)` (`app.rs`) — which already preserves
zoom / pan / exposure / channel / compare / annotations, invalidates the right caches, and is
unit-tested. The cache holds **`Arc<ExrData>`** so a frame is both active (T3) and resident (T1)
without cloning 600 MB → this is why `self.exr_data` becomes `Option<Arc<ExrData>>` (Phase 0).

## A/B compare

Ship **A-plays / B-holds first:** the sequence drives slot **A**; **B** is a fixed reference, never
touched (the swap-A-keeps-B semantics already hold). Cache keys on `(frame_index, slot)` so
**locked-step A/B** is a later extension, not a rewrite.

## Scrub vs play

- **Linear play:** one-directional ring + prefetch ahead.
- **Scrub:** epoch bump, bidirectional eviction, paint T2 if resident else **T0 proxy** for instant
  feedback (T0's first real use as a scrub tier); on scrub-settle, **ensure T1 residency**
  (INV-SAMPLE) before re-enabling sampling.

## Decisions baked in (override on review)

1. **Space-bar:** context-gate — sequence loaded → space = play/pause; otherwise space =
   blink-compare (today's behavior). Revisit if blink-during-sequence is wanted (remap blink).
2. **Default pacing:** stutter (play every frame); drop-frames is a toggle.
3. **Persisted prefs:** `fps_target` / `loop_mode` / `direction` / `pacing` persist;
   `sequence` / `current_frame` / handles reset per open.
