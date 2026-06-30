# Playback hardening plan (post-ship #7 → #100)

> Status: planning doc, written after the playback groundwork (#7 Phases 0–5) shipped. Sequences
> the remaining work — gap-closing, real-footage validation (#100), and the feature follow-ons —
> so it doesn't drift. Builds on the [memory](memory-contract.md) (#56), [concurrency](concurrency-contract.md)
> (#57), and [sequence-playback](sequence-playback.md) (#7) contracts.

## Where playback actually stands

**#7 Phases 0–5 are implemented and well-tested.** This is no longer a "build playback" effort — it's
hardening a working sequence player. Verified present and matching the contracts:

| Area | Status | Home |
|------|--------|------|
| Sequence detection (parse / group / numeric sort / holes) | DONE | `src/sequence.rs` |
| `Playback` state machine (state / direction / loop / trim / epoch / serde split) | DONE | `src/playback.rs` |
| Drift-corrected frame clock; stutter (default) + drop-frames pacing | DONE | `app.rs` `tick_playback` |
| T1 ring cache `(Slot, frame) → Arc<ExrData>`, directional-ring + LRU eviction | DONE | `src/cache.rs` |
| Budget math (`approx_bytes`, `max_t1`/`max_t2`, VRAM/RAM headroom, off-Metal fixed cap) | DONE | `src/budget.rs` |
| Pure scheduler want-list (P0/P1/P2, back-pressure = T1 budget) | DONE | `src/scheduler.rs` |
| Single-worker decode-ahead + epoch supersession (`pump_decode`/`pump_t2`) | DONE | `app.rs` |
| Transport UI (timeline w/ holes, in/out, play/step/reverse/loop, fps, T2 kill-switch) | DONE | `app.rs` `draw_transport_bar` |
| `swap_image_arc` Arc refactor; A-plays / B-holds | DONE | `app.rs` |

~77 tests (52 headless across `sequence`/`playback`/`cache`/`scheduler`/`budget`, ~25 app-level).

So the remaining work is: **close the one real gap → validate on real footage → layer on the feel
features → the one big net-new feature (locked-step A/B).**

## The two "gaps", honestly scoped

The issue list implies two missing pieces (INV-SAMPLE, scrub-proxy). On close reading they are smaller
and more entangled than the labels suggest:

- **INV-SAMPLE is ~80% already satisfied.** Every displayed frame is swapped into `self.exr_data`
  (T3) by `swap_image_arc`, and the sampler reads exactly that (`viewer.rs` `sample_pixel(exr_data, …)`),
  so the pixel probe always reflects what is on screen — never random evicted memory. The contract's
  "re-decode if evicted on pause" is also already handled: a scrub to an evicted frame goes through
  `request_sequence_frame`'s miss path (`pending = Some`, `pump_decode`). What is genuinely missing is
  narrow — **suppression while the clock advances**: hovering during play samples a ~600 MB `ExrData`
  on every mouse-move, and during a *pending* scrub the playhead label and the (held previous) pixels
  momentarily disagree.

- **The scrub-proxy "fallback" is not a tiny gap — it is the front half of #94.** The worker
  *deliberately skips* the proxy for sequence frames (`app.rs`, `seq_frame` ⇒ no `Proxy` message), so
  there is no T0 to paint during a scrub. Doing it means *producing* proxies for sequence frames,
  which is exactly #94's scope. It is tracked there, not mislabelled as a gap-close.

## Sequenced plan

Each step is independently shippable and never regresses the single-image app (the standing rule from
the contracts). Pure-logic-first where possible.

### 1. INV-SAMPLE suppression + coherence — `the` gap-close (do first; small, headless)

- Compute `suppress_sampling = playback.is_playing() || playback.pending.is_some()` and thread it into
  `viewer.ui` (same shape as `ocio_active` / `ocio_render_gen`).
- When suppressed, **skip `sample_pixel`** and show `—` / "playing" in the readout instead of sampling
  — no stale label/pixel disagreement, no per-hover full-frame scan during play.
- Tests (headless): playing ⇒ suppressed; pending ⇒ suppressed; paused+resident ⇒ live. Add a test
  asserting the existing ensure-T1-on-settle path (no new machinery — `request_sequence_frame`'s miss
  path already re-decodes an evicted target).
- No GPU dependency. ~1 focused PR.

### 2. #100 — real-footage validation (Mac + Windows footage is ready)

Validation is mostly a *soak*, but unproductive without instrumentation first. The instrumentation
shipped (#128); the soak is now a runnable checklist: **[soak-checklist.md](soak-checklist.md)**.

- **Instrumentation:** a toggleable cache-state debug overlay — T1/T2 residency counts, the live
  `max_t1`/`max_t2` caps and the budget inputs, measured-vs-target fps, evictions/sec, dropped-epoch
  count, in-flight frame. Makes the soak observable instead of guesswork.
- **Soak matrix:** 2K/4K multi-AOV sequences × {loop, ping-pong, scrub, stutter, drop-frames, in/out
  trim, A/B-hold} on **Metal (Mac)** and **off-Metal fixed-cap (Windows)**.
- **Watch the flagged risks:**
  - VRAM stays under budget — wgpu **aborts the process** on OOM, so the budget must hold proactively.
  - Rapid scrub↔play cycles — epoch correctness under path recurrence (loop / ping-pong / scrub-back).
  - Heterogeneous frame sizes vs the "sequences are homogeneous" budget assumption (re-measure on
    large deviation, or at least log it).
  - Single global rayon pool — decode vs UI-thread texture packing starvation under heavy T2 builds.
- File every bug as a **#99 blocker**.

**Findings so far (off-Metal Windows, ~300–600 MB on-disk all-AOV frames decoding to ~1.3 GB
resident):**
- **Decode-bound, not broken.** Stutter held `0.6 / 24` fps, drop-frames `6.2 / 24` — pacing path
  confirmed correct (drop-frames stops the clock waiting on decode), but neither nears target because
  single-worker all-AOV decode throughput is the wall. No OOM, epochs healthy.
- **The RAM budget was a cliff.** `T1` collapsed `7 → 3` frames when external usage rose just 1.4 GB,
  because the old `70% of total − used` model throttles to ~0 once other apps cross the ceiling — even
  with ~40 GB physically free. **Fixed:** `max_t1` now takes a slice of *free* RAM (`budget.rs`,
  `RAM_FREE_PCT`); the same machine would hold ~17–18 frames and shrink smoothly under pressure.
- **Fix confirmed on Metal (8 GB Mac, ~95 MB/frame light sequence):** `T1 27 / 27`, decoder idle,
  smooth playback. The free-RAM model yields 27 frames where the old total-ceiling model gave ~20 —
  graceful degradation on a small machine, as intended.

### 3. Beauty-only fast decode for playback T1 (decode-wall lever; likely do before #94)

The single biggest cost in both decode time *and* T1 size is that every frame decodes **all AOVs**
(~1.3 GB resident for a 500 MB file). For *playback preview* only the beauty/active layer is shown.

- Use the `exr` crate's **partial-channel read** to decode just the beauty (RGBA / active) layer for
  the playback ring — potentially a **5–10× cut in decode time and resident bytes at once**, hitting
  both the decode wall and the cache cap. Full-res (unlike #94's downscaled proxy), single-layer.
- **Coheres with #127:** sampling is already suppressed during play, so the other AOVs aren't needed
  while moving. On **pause**, the existing ensure-T1-on-settle path does a *full* all-AOV re-decode of
  the settled frame, restoring sampling and AOV-switching. Beauty-while-moving, full-when-stopped.
- Open questions: how "beauty" is identified (RGBA / named layer / first-layer heuristic — VFX EXRs
  vary); whether this is a new **decode mode** on T1 or a distinct preview tier; relation to #94 (they
  are complementary speed/quality points — beauty-full-res vs. tiny-downscaled).
- Pure-ish: the partial-read path is unit-testable on a fixture EXR; the play/pause decode-mode switch
  needs manual verification. Arguably **higher-leverage and lower-effort than #94** — sequence it first.

### 4. Eager precache + cache-fill indicator (PDplayer / OpenRV-style)

Today the scheduler prefetches a *sliding window* ahead of the playhead (#57 decode-ahead). Review
tools also offer an **eager precache**: fill the whole in/out range into the cache up front, so once
it's green the entire span plays — and loops — glass-smooth with the decoder idle (exactly the Mac
result above, but for the *whole* range rather than a window).

- **Eager fill:** on sequence load or an explicit "cache" action, request *every* in-range frame into
  T1 (and T2 up to VRAM budget) at a background priority, instead of only the decode-ahead window.
  Expressible on the existing want-list (all in-range frames at P1) — mechanism, not a rewrite.
- **Cache-fill indicator:** the classic colored bar under the scrubber showing per-frame residency
  (T1 / T2), filling as it caches. User-facing transport feature; the #128 debug overlay already has
  the residency data, this surfaces it on the timeline.
- **Budget-bounded, honestly:** if the range exceeds `max_t1`, precache fills to the cap and the bar
  shows the resident span — it does not pretend to hold what won't fit. This is precisely why the
  budget fix and **beauty-only decode (step 3)** matter: together they decide how much of a shot
  actually fits in RAM, i.e. how much of the bar can go green.
- **Interaction with pacing/loop:** a fully-green range needs no re-decode at the loop wrap — the
  stutter-vs-drop-frames distinction collapses to "always real-time" within the cached span.
- Open questions: auto-precache-on-open vs explicit button; whether precache targets in/out or the
  full range; eviction policy while precaching (don't evict already-green frames the user is about to
  loop over). Pure-ish: the want-list shape is headless-testable; the bar + fill UX needs manual check.

### 5. #94 — user-controllable scrub proxies + on-disk proxy cache

- Produce T0 proxies for sequence frames (the real scrub-proxy work); paint the proxy on a fast scrub
  when the full frame isn't resident, for instant feedback.
- User **enable/disable toggle** + **size/resolution** setting (mirror the existing T2 GPU kill-switch
  in the transport bar). Directly addresses the slow/networked-EXR (wifi share) case.
- Future: persistent on-disk proxy cache (keyed by path + mtime + size params) so a re-opened sequence
  doesn't re-downsample.
- Then **#75** — "loading full resolution" indicator while the proxy shows.

### 6. #112 — contact-sheet-during-playback — **re-measure before designing**

The issue text is **stale**: #59 deleted the `textures.fill(None)` full-invalidation and the CPU OCIO
bake it blames, and #67 added GPU thumbnails (its option D). The per-frame freeze may already be
largely gone. Re-measure with the sheet open over a playing sequence first; the remaining work is
likely a **frame-keyed GPU thumbnail cache** + not invalidating the whole sheet on every swap.

### 7. #98 — locked-step A/B sequence playback (largest net-new; last)

Two sequences in lockstep (frame N of A alongside frame N of B, with a user offset) for side-by-side /
wipe / diff review over time. The groundwork was built for this (`(Slot, frame)` cache keys,
`swap_image_arc(_, is_b)`, `exr_data_b: Option<Arc<ExrData>>`), so it is an extension, not a rewrite.
Net-new: B sequence detection + state, two-slot scheduling/decode interleave (still one decode at a
time — #57), per-slot T2 pre-upload, the two-slot VRAM/RAM budget split (`max_t1` halves per slot), and
the frame-offset alignment control. Pure-logic-first: the A/B want-list and offset mapping are
headless-testable; only the live two-slot T2 interleave needs manual GPU verification.

## Parking lot (back-burner — interesting, not scheduled)

Ideas worth keeping but explicitly *not* on the critical path. Recorded so they aren't lost.

- **Offline proxy-bake tool.** A standalone tool/command that pre-generates on-disk proxies for a
  sequence ahead of time (batch, not lazy) — the eager sibling of step 5's lazy on-disk proxy cache.
  Re-open a baked sequence and it scrubs instantly with zero re-downsample. Shares the cache key
  (path + mtime + size params) so the live cache and the baked files interoperate.
- **Proxies embedded *in* the EXR.** OpenEXR already supports multi-resolution images natively —
  **mipmapped/ripmapped tiled EXRs** carry multiple LOD levels in one standard file, and multi-part
  EXRs can hold a separate low-res part. So a proxy could live as the smallest mip level of a *standard*
  EXR (no format fork — any compliant reader sees it). The real blocker is the Rust **`exr` crate**:
  cheaply reading *only* a low LOD level (without decoding full-res) may not be exposed → an upstream
  contribution or a reader fork, not a new format. Upside: one file, no sidecar proxies to manage,
  proxy travels with the frame. Revisit once #94 / the offline bake tool prove the proxy UX is worth it.

## Open questions to settle in-flight

1. **Suppressed-readout presentation:** blank `—`, the last sampled value greyed, or a proxy-sampled
   approximation? (Start with `—`/"playing"; revisit if a live-ish readout during play is wanted.)
2. **#94 proxy persistence:** in-memory T0 ring first, or go straight to the on-disk cache? (Lean
   in-memory first; disk cache is the bigger, separable follow-on.)
3. **Heterogeneous-frame budget:** re-measure `approx_bytes` on first >N% deviation vs. log-and-accept?
   (Decide during #100 once real footage shows whether it actually varies.)
4. **Dedicated decode rayon pool:** still a measure-first lever (out of scope unless #100 shows
   UI-thread starvation).

## Cross-references

[README](README.md) · [memory-contract](memory-contract.md) · [concurrency-contract](concurrency-contract.md)
· [sequence-playback](sequence-playback.md) · issues #7, #56, #57, #75, #94, #98, #99, #100, #112.
