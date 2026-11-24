Open one frame of a numbered sequence (for example `render.0101.exr`) and Floki plays the shot. A file with at least two matching siblings becomes a sequence. A lone image stays a still.

## Transport

Play, pause, step, reverse, and stop-in-place. The scrubber highlights the trimmed region and marks missing frames (holes). The player holds the previous frame on a hole; it does not stall. Target fps is editable, and a measured-fps readout sits next to it.

| Control | Behavior |
|---|---|
| Loop modes | Loop, Once, or Ping-Pong |
| **Stutter** (default) | Play every frame. fps drops under load. Nothing is skipped. |
| **Drop-frames** | Hold the wall-clock rate and skip frames |
| In / out | The cache and playback respect the trim |
| `Space`, `←` / `→`, `I` / `O` | Play/pause, step, set in/out — only once a sequence is loaded. See [Keyboard shortcuts](Keyboard-shortcuts). |

The clock never blocks the UI thread on a decode. While the clock is advancing, the pixel readout is served from the proxy or suppressed. Pausing re-decodes the playhead in full so the readout and an AOV switch stay accurate. That is the INV-SAMPLE rule in the [memory contract](Memory-contract).

## What gets decoded

Playback and scrub decode the pass currently on screen, not always the beauty. The beauty or first pass still uses the beauty or proxy path. Any other pass that lives in its own EXR part is decoded with `load_layer` (or a proxy of that pass). A full decode is used when the layers on one source want different passes, or when those passes share a single part.

That fast path only helps **multi-part** files (one AOV per part). Single-part files with prefixed channels keep every pass in the same blocks, so part selection skips nothing. See [Patched exr fork](Patched-exr-fork).

## Cache

A byte-budgeted ring keeps decoded frames in RAM inside a live memory budget, so scrub-backs and loops are instant and playback degrades instead of crashing. The budget is a slice of *free* RAM, not total RAM, with the ring's own residency added back. The user's RAM setting is a ceiling. Details, including the three sizing rules that have each been broken before, are the [memory contract](Memory-contract). How the single decode worker is fed is the [concurrency contract](Concurrency-contract).

**Eager precache** optionally caches the whole in/out range up front. A green fill bar under the scrubber shows what is resident, and the span then plays and loops with the decoder idle. The persisted precache default is on for fresh installs. Existing saved state is kept.

**Live render-watch** watches the sequence folder and picks up frames as a render writes them. New frames extend the range. Re-rendered frames refresh. It can park the playhead on the newest frame.

Proxies are a geometry-preserving downsample of the fully decoded frame (`load_beauty`, then `downsampled`), not a reduced-resolution EXR read. A reduced-res read flattened the data window and display window and mis-placed tight-window renders. f16 sources stay f16 through the downsample. Repeat views of a proxy can come from the on-disk cache at `~/.floki/proxy-cache`, keyed by path, mtime, size, pixel size, and AOV.

The design that this behavior shipped from is [Sequence playback](Sequence-playback). The performance levers around it are the [perf roadmap](Perf-roadmap).
