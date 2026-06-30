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

1. Build the release-ish OCIO binary you actually review with (`system-ocio` or `vendored`), not the
   stub — the budget math and T2 path only exist with a GPU.
2. **View → Playback Debug** (the #128 overlay). Every check below reads off it. Field map:

   | Overlay row | What to watch |
   |-------------|---------------|
   | `range` | in/out trim vs full range — confirms trimmed playback honors the window |
   | `frame · state · dir` | playhead, `Playing`/`Paused`/`Stopped`, `Forward`/`Reverse` |
   | `mode` | loop mode · pacing · **epoch** (must climb on every scrub/loop turn, never stall) |
   | `fps` | **measured / target** — the headline smoothness number |
   | `T1 (CPU)` | `len / cap frames · ~bytes/frame` — RAM ring occupancy vs budget |
   | `T2 (GPU)` | `len / cap frames` or `off` — VRAM ring occupancy vs budget |
   | `worker` | `in-flight [...] · pending N` — decode-ahead depth; persistent backlog = starvation |
   | `RAM` / `VRAM` | live `used / total` (VRAM = `n/a` off-Metal) — must stay under budget |
   | `evictions` | cumulative; **rate** matters — a high steady rate during smooth play = thrash |
   | `dropped-epoch` | superseded decodes; a few per scrub is healthy, a flood = epoch storm |

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
- [ ] **Scrub** — drag the scrubber fast across the whole range. Watch `pending`/`in-flight` and
      `dropped-epoch`: a fast scrub *should* generate dropped epochs (that's supersession working),
      but the playhead must always settle on the frame you released on, never a stale one.
- [ ] **A-plays / B-holds** — load a B slot; A plays, B stays put; sampler/readout reflects the right slot.

## The flagged risks — verify each explicitly

These are the four risks called out in the hardening plan. Each gets a deliberate adversarial probe:

1. [ ] **VRAM never exceeds budget.** wgpu **aborts the process** on OOM — there is no soft failure.
       On the 4K sequence, off-Metal especially, watch `T2 len/cap` and `VRAM used`: `len` must never
       exceed `cap`, and the cap must hold *proactively* (eviction before upload, not after). If the
       app vanishes, that's the bug. Toggle the **T2 kill-switch** and confirm playback degrades to
       T1-only instead of crashing.
2. [ ] **Epoch correctness under path recurrence.** Rapidly alternate scrub ↔ play ↔ loop-back ↔
       ping-pong on the same frames. `epoch` must climb monotonically; `dropped-epoch` rises but the
       *displayed* frame always matches the playhead. The failure signature is a frame from a previous
       pass painting after you've moved on (supersession leak).
3. [ ] **Heterogeneous frame size.** If frame bytes vary >~N% across the sequence, the `~bytes/frame`
       budget estimate is wrong → T1 over- or under-fills. Watch whether `T1 len` swings oddly or RAM
       creeps past budget. Decide here (open question #3): re-measure `approx_bytes` on deviation, or
       log-and-accept.
4. [ ] **UI-thread starvation.** Single global rayon pool runs decode *and* texture packing. On heavy
       T2 builds (4K, fast play), watch for `in-flight` backlog that never drains and an unresponsive
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
- `T1 len ≤ cap` and `T2 len ≤ cap` at all times; `RAM`/`VRAM used` under budget.
- Eviction rate is bounded during steady play (no thrash).
- INV-SAMPLE: suppressed during play/seek, live + correct when paused-resident, re-decode on
  paused-evicted.
- UI stays responsive (no decode→UI starvation) under the 4K/fast-play stress.

## On failure

File each as a **#99 blocker**, with: regime (Metal / off-Metal), sequence (res + AOV count), mode,
the overlay readout at the moment of failure (fps / T1 / T2 / epoch / dropped / RAM / VRAM), and repro
steps. A reproducible overlay snapshot is worth more than a description — note the exact numbers.

## Cross-references

[README](README.md) · [hardening-plan](hardening-plan.md) · [memory-contract](memory-contract.md)
· [concurrency-contract](concurrency-contract.md) · issues #7, #94, #99, #100, #112, #127, #128.
