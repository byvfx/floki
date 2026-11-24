# Playback soak checklist (#100 validation)

> Run this **before** building #94. It validates the sequence player (#7 Phases 0–5) plus the debug
> overlay (#128, shipped) on real footage, on both budget regimes. The **INV-SAMPLE suppression**
> checks below land with #127 (recovered as #131) — run that section once it's merged; until then the
> readout samples on every hover. The goal is to turn "playback works on my test clip" into "playback
> holds under the flagged risks" — and to file every failure as a **#99 blocker** before more features
> land on top.
>
> Sequenced from [hardening-plan.md](hardening-plan.md) step 2. Read [memory-contract](memory-contract.md)
> (budgets / INV-SAMPLE) and [concurrency-contract](concurrency-contract.md) (epochs) for the *why*
> behind each "watch".

## Setup

1. Build the release binary you actually review with (`system-ocio` or `vendored`), not the
   stub. The app will not launch without a GPU that supports `FLOAT32_FILTERABLE`. The T2
   VRAM ring was removed in #299; the RAM budget is the T1 ring.
2. **View → Playback Debug** (the #128 overlay). Every check below reads off it. Field map
   (labels as drawn in `ExrApp::draw_playback_debug`):

   | Overlay row | What to watch |
   |-------------|---------------|
   | `range` | full range, then `(in/out …)` — confirms trimmed playback honors the window |
   | `frame` | playhead · `Playing`/`Paused`/`Stopped` · `Forward`/`Reverse` |
   | `mode` | loop mode · pacing · **epoch** (must climb on every scrub/loop turn, never stall) |
   | `fps` | **measured / target** — the headline smoothness number |
   | `frame time` | p50 / p95 / p99 / max ms, and `n`. `—` until a frame has been shown |
   | `T1 (CPU)` | `len / cap frames`, sizing per frame, and held bytes / budget |
   | `proxy` | on/off, long-side px, and measured bytes/frame once a proxy has decoded |
   | `fallbacks` | cheap decodes the worker had to satisfy with a dearer frame |
   | `comp tex` | live comp textures (`s{id} {w}×{h}`). This is the GPU-resident set now that the T2 ring is gone |
   | `sources` | per-follower playhead, displayed frame, pending, in-flight. The live row when the comp stack drives the clock |
   | `worker (A)` | slot-A `in-flight`, `pending`, `loading_a`, alive/dead. Empty by construction on the comp path; read `sources` there |
   | `decode` | `submitted`, `cost`, `apply`, `round trip` — three timings, not one (#350) |
   | `RAM` | system `used / total` |
   | `VRAM` | `used / budget`, or `n/a (off-Metal fixed cap)` |
   | `evictions` | cumulative T1 evictions; **rate** matters — a high steady rate during smooth play = thrash |
   | `dropped-epoch` | superseded decodes; a few per scrub is healthy, a flood = epoch storm |
   | `sheet bakes` | contact-sheet bakes; flat while playback is moving, steps once on settle |

3. The `evictions` / `dropped-epoch` counters reset when a **new sequence is detected** (each freshly
   opened sequence starts clean); `fps` resets on stop. To zero them mid-soak, re-open the sequence.

## Footage matrix

Run each axis on **both** regimes:

- **Metal (Mac)** — live `recommendedMaxWorkingSetSize` VRAM budget.
- **Off-Metal (Windows)** — fixed 1 GiB VRAM cap. This is the harsher regime; most budget bugs show here first.

Sequences (use real multi-AOV EXR, not synthetic):

- [ ] **2K multi-AOV** — comfortably within budget; the "should just be smooth" baseline.
- [ ] **4K multi-AOV** — the stress case; ~0.5–1.3 GB/frame, where T1 cap drops to single digits.
- [ ] **A sequence with holes** — confirms the timeline gaps render and stepping skips them cleanly.
- [ ] **Heterogeneous frame sizes** (mixed res within one sequence, if you have it) — the budget
      assumes homogeneity; see the dedicated check below.

## Per-mode checks

For each sequence above, exercise:

- [ ] **Stutter pacing** (default) — every frame shown. `measured fps` may sit below target on 4K;
      that's expected (it never *drops* frames). Confirm no freeze, no audio-less judder spikes.
- [ ] **Drop-frames pacing** — `measured` should track `target` closely; frames skipped, not stalled.
- [ ] **Loop** — at the wrap, `epoch` increments, `frame` jumps to in-point, no flash of a stale frame.
- [ ] **Ping-pong** — direction flips at both ends; `dir` flips; the just-played tail stays resident
      (eviction shouldn't dump the frames you're about to replay).
- [ ] **In/out trim** — set a tight window mid-sequence; playback stays inside it; `range` reflects it;
      caches don't prefetch far outside the window.
- [ ] **Scrub** — drag the scrubber fast across the whole range. Watch `pending`/`in-flight` on
      `sources` (or `worker (A)` when slot A owns the clock) and `dropped-epoch`: a fast scrub
      *should* generate dropped epochs (that's supersession working), but the playhead must always
      settle on the frame you released on, never a stale one.
- [ ] **Two layers** — add a second source. Both follow the shared playhead (their own trims
      included); the readout names the pane under the cursor. There is no separate B slot (#277).

## The flagged risks — verify each explicitly

These are the four risks called out in the hardening plan. Each gets a deliberate adversarial probe:

1. [ ] **VRAM never exceeds budget.** wgpu **aborts the process** on OOM — there is no soft failure.
       On the 4K sequence, off-Metal especially, watch `VRAM` and `comp tex`. There is no T2 ring
       and no T2 kill-switch (#299): playback does not pre-upload a ring of full-frame GPU textures.
       If the app vanishes, that's the bug.
2. [ ] **Epoch correctness under path recurrence.** Rapidly alternate scrub ↔ play ↔ loop-back ↔
       ping-pong on the same frames. `epoch` must climb monotonically; `dropped-epoch` rises but the
       *displayed* frame always matches the playhead. The failure signature is a frame from a previous
       pass painting after you've moved on (supersession leak).
3. [ ] **Heterogeneous frame size.** If frame bytes vary >~N% across the sequence, the `~bytes/frame`
       budget estimate is wrong → T1 over- or under-fills. Watch whether `T1 len` swings oddly or RAM
       creeps past budget. Decide here (open question #3): re-measure `approx_bytes` on deviation, or
       log-and-accept.
4. [ ] **UI-thread starvation.** Single global rayon pool runs decode *and* texture packing. On heavy
       4K playback, watch for an `in-flight` backlog that never drains and an unresponsive
       UI (laggy scrubber, stuttery menus *while frames are still decoding*). That's decode starving
       the UI thread — the trigger to consider a dedicated decode pool (open question #4).

## INV-SAMPLE (#127) confirmation

- [ ] **During play**, hover the image: the readout shows **no live pixel value** (a `—` / paused-
      readout state), and there's no per-hover hitch (no full-frame scan).
- [ ] **During a pending scrub**, the playhead label and the painted pixels never disagree visibly.
- [ ] **Paused on a resident frame**, hover: readout is **live** again and matches the displayed frame.
- [ ] **Pause on an evicted frame** (scrub far, pause): the frame re-decodes (`pending` → resident),
      then the readout goes live — never samples stale/freed memory.

## Pass criteria

- No process aborts (VRAM held proactively in every mode, both regimes).
- `epoch` strictly monotonic; no stale-frame paints under any scrub/loop/ping-pong sequence.
- `T1` `len ≤ cap` at all times; `RAM` / `VRAM` under budget. There is no T2 cap to check (#299).
- Eviction rate is bounded during steady play (no thrash).
- INV-SAMPLE: suppressed during play/seek, live + correct when paused-resident, re-decode on
  paused-evicted.
- UI stays responsive (no decode→UI starvation) under the 4K/fast-play stress.

## On failure

File each as a **#99 blocker**, with: regime (Metal / off-Metal), sequence (res + AOV count), mode,
the overlay readout at the moment of failure (fps / frame time / T1 / comp tex / epoch / dropped-epoch / RAM / VRAM), and repro
steps. A reproducible overlay snapshot is worth more than a description — note the exact numbers.

## Cross-references

[README](README.md) · [hardening-plan](hardening-plan.md) · [memory-contract](memory-contract.md)
· [concurrency-contract](concurrency-contract.md) · issues #7, #94, #99, #100, #112, #127, #128.
