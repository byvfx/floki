# Cross-player review benchmark — Floki vs DJV vs Chaos Player

A reusable protocol for comparing Floki's review-playback performance against
other **dedicated sequence players** (DJV, Chaos Player) on the same footage.
Deliberately excludes Nuke: Nuke is a compositor with a Viewer bolted on, not a
review player, so its numbers aren't a fair baseline for this class of tool —
see the discussion this doc grew out of. Re-run this whenever a playback-perf
change lands (#33, #94, #165, #166, ...) to see if the gap to the field moved.

## Metrics

| Metric | What it tells you | Floki source |
|--------|--------------------|---------------|
| **Time-to-first-frame** | cold-open latency: file → pixels on screen | stopwatch, or `RUST_LOG=floki=debug` timestamps around `ExrData::load` |
| **Steady-state playback fps** | measured vs target once the cache is warm | `View → Playback Debug` overlay, `fps` row |
| **Time-to-smooth** | frames from playback start until `measured fps` stabilizes at target | `Playback Debug`, watch `fps` settle |
| **Memory footprint at steady state** | RAM/VRAM once the play range is cached | `Playback Debug`, `RAM` / `VRAM` rows |
| **Scrub responsiveness** | latency between scrubber release and correct frame paint | stopwatch / screen recording, both apps |

Playback fps and time-to-smooth are the headline numbers — they're what a
comp/lighting TD actually feels. Memory footprint matters mainly to explain
*why* one tool is smoother on the 4K case.

## Footage matrix

Reuse the [soak-checklist](../playback/soak-checklist.md) footage axes so this
benchmark and the internal soak pass share fixtures:

- **2K multi-AOV** — the "should just be smooth" baseline.
- **4K multi-AOV** — the stress case (~0.5–1.3 GB/frame); this is where players
  actually differentiate.
- Optional: a **single-part Blender-style** file (all passes in one part) vs a
  **multi-part Houdini/Karma** file (one AOV per part) — floki's #217 per-AOV
  decode only helps the multi-part shape, so this axis is worth separating out
  if any player claims an AOV-switch speed edge.

`assets/perf/` already has two real multi-part renders usable as a starting
point (`TPLS2_...redSea_bty.1078.exr`, `alien_v02.karmarendersettings.1001.exr`)
— for a *playback* comparison you need an actual **sequence** (a frame range),
not single frames, so point each player at the same numbered sequence on disk.
`.exr` is gitignored repo-wide, so drop sequences under `assets/perf/` locally;
they never get committed.

## Per-player measurement

- **Floki**: `View → Playback Debug` overlay gives `fps`, cache occupancy,
  eviction rate, and `RAM`/`VRAM` live — see the [soak checklist](../playback/soak-checklist.md)
  field map for what each row means. For time-to-first-frame, `RUST_LOG=floki=debug`
  and diff the timestamps bracketing `ExrData::load`.
- **DJV**: has a HUD/stats overlay (menu location has moved across DJV
  versions — check `View` for a HUD toggle) reporting frame number and
  playback speed; confirm what your installed version exposes before relying
  on it.
- **Chaos Player**: proprietary; built specifically for real-time multi-layer
  EXR scrubbing so it likely surfaces an fps/stats readout in its viewer, but
  verify in-app — don't assume a specific menu path.

## Protocol

1. **Same machine, same run session.** Don't compare a run from last week to
   one today; OS memory pressure and thermal state drift.
2. **Cold vs warm, kept separate.** Quit/relaunch each app between cold runs
   (clears its cache); for warm runs, play the sequence once first to prime
   the cache, then measure the second pass. Never average cold and warm
   together.
3. **3 runs per (player × sequence × cache-state) cell**, record all three and
   the median — first-run variance is common (OS disk cache, JIT/shader warm-up).
4. **Record raw numbers, not just "felt smooth."** For Floki, snapshot the
   overlay row verbatim (mirrors the soak checklist's failure-report format);
   for DJV/Chaos Player, whatever their HUD shows, plus a stopwatch reading
   where no HUD exists.
5. **Note config that affects fairness**: resolution/proxy mode, color
   management on/off (OCIO has a real cost), cache size limits, whether the
   player is CPU- or GPU-decoding.

## Results template

| Sequence | Player | Cache state | Time-to-first-frame | Steady fps (measured/target) | RAM | VRAM | Notes |
|----------|--------|--------------|----------------------|-------------------------------|-----|------|-------|
| 2K multi-AOV | Floki | cold | | | | | |
| 2K multi-AOV | Floki | warm | | | | | |
| 2K multi-AOV | DJV | cold | | | | | |
| 2K multi-AOV | DJV | warm | | | | | |
| 2K multi-AOV | Chaos Player | cold | | | | | |
| 2K multi-AOV | Chaos Player | warm | | | | | |
| 4K multi-AOV | Floki | cold | | | | | |
| 4K multi-AOV | Floki | warm | | | | | |
| 4K multi-AOV | DJV | cold | | | | | |
| 4K multi-AOV | DJV | warm | | | | | |
| 4K multi-AOV | Chaos Player | cold | | | | | |
| 4K multi-AOV | Chaos Player | warm | | | | | |

## Caveats

- This is a user-facing feel comparison, not a controlled decoder benchmark —
  for decoder-only numbers (isolating floki's `exr` fork from OpenEXR without
  any UI/cache variables) see [`dwa-arm-decode-investigation.md`](dwa-arm-decode-investigation.md)
  and the `#33` section of [`perf-roadmap.md`](../perf-roadmap.md) instead.
- DJV and Chaos Player versions matter — record the exact version tested, since
  overlay availability and defaults change across releases.
- A result here motivates filing a specific floki issue (footprint, pipelining,
  or feature gap per [`perf-roadmap.md`](../perf-roadmap.md)'s lever taxonomy) —
  it isn't itself a verdict on which tool is "better."
