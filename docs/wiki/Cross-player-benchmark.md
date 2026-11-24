A protocol for comparing Floki's review playback with other dedicated sequence players (DJV, Chaos Player) on the same footage. The file in the repo is [`docs/perf/cross-player-benchmark.md`](https://github.com/byvfx/floki/blob/main/docs/perf/cross-player-benchmark.md). Nuke is deliberately not in the set: it is a compositor with a viewer, not a review player, so its numbers are not the baseline for this class of tool.

Re-run it when a playback-performance change lands (#33, #94, #165, #166, and the rest of the [perf roadmap](Perf-roadmap)).

## Metrics

| Metric | What it tells you | Where Floki shows it |
|---|---|---|
| Time to first frame | Cold-open latency, file to pixels | A stopwatch, or `RUST_LOG=floki=debug` timestamps around `ExrData::load` |
| Steady-state playback fps | Measured vs target once the cache is warm | **View → Playback Debug**, `fps` row |
| Time to smooth | Frames from playback start until measured fps sits on the target | Playback Debug, watch `fps` settle |
| Memory at steady state | RAM and VRAM once the play range is cached | Playback Debug, `RAM` / `VRAM` rows |
| Scrub responsiveness | Scrubber release to the correct frame | Stopwatch or a screen recording, both apps |

fps and time-to-smooth are the headline numbers. Footprint explains why one tool is smoother on the 4K case.

## Footage

Reuse the soak-checklist axes so this benchmark and the internal soak pass share fixtures. The checklist itself is [`docs/playback/soak-checklist.md`](https://github.com/byvfx/floki/blob/main/docs/playback/soak-checklist.md) and is not on this site's nav.

- **2K multi-AOV** — the case that should already be smooth.
- **4K multi-AOV** — the stress case (about 0.5–1.3 GB/frame in the playback docs). This is where players actually differ.
- Optional: a single-part file (all passes in one part) against a multi-part file (one AOV per part). The #217 per-AOV decode only helps the multi-part shape.

`assets/perf/` has two real multi-part renders as a starting point. A playback comparison needs a numbered sequence, not a single frame. `.exr` is gitignored, so sequences dropped under `assets/perf/` stay local.

## Protocol

1. Same machine, same session. Don't compare last week's run with today's. Memory pressure and thermals drift.
2. Keep cold and warm separate. Quit and relaunch between cold runs. For warm runs, play the sequence once, then measure the second pass. Never average cold and warm together.
3. Three runs per player, sequence, and cache state. Record all three and the median.
4. Record raw numbers. For Floki, snapshot the overlay row. For DJV and Chaos Player, record whatever their HUD shows, plus a stopwatch where no HUD exists. DJV's HUD has moved across versions. Chaos Player's menu path is not assumed here — check the installed build.
5. Note proxy mode, OCIO on or off, cache limits, and whether the player is decoding on the CPU or the GPU.

The results table in the repo file is empty on purpose. This page does not fill it in.

This is a feel comparison, not a controlled decoder benchmark. Decoder-only numbers (the `exr` fork against OpenEXR, without the UI and the cache) are the DWA study linked from the [perf roadmap](Perf-roadmap). A result here is a reason to file an issue. It is not a verdict on which tool is better.
