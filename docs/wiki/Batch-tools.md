## Open a file from the shell

```bash
floki path/to/frame.exr
```

The first argument uses the same path as **File ▸ Open EXR…** and a drag-and-drop, after any saved layer stack has been restored. The file is added as a layer, or the existing layer is focused if that path is already in the stack. A missing path is shown as `No such file: …` in the EXR Info panel and logged. Further arguments are ignored.

## convert_dir

`convert_dir` bulk-renames EXR channels across a directory to standard RGBA. The same work is the in-app Tools window. Both call `run_conversion_task` in [`src/tools.rs`](https://github.com/byvfx/floki/blob/main/src/tools.rs), which parallelizes across files with `rayon`.

```bash
cargo run --bin convert_dir -- <input_dir> [output_dir]
```

`output_dir` defaults to `<input_dir>/converted`. Progress prints as `[done/total] message`. The process exits non-zero if any file failed or the run was cancelled. The summary counts are authoritative; the channel messages are for humans. Because files finish out of order, per-file `RUST_LOG` lines are interleaved. See [Troubleshooting](Troubleshooting#rust_log).

Cancellation is an `Arc<AtomicBool>`. The GUI and this binary are two front-ends over one function. A shared `AtomicUsize` keeps the completed count moving only forward.

Do not reuse this `into_par_iter`-over-files shape for playback prefetch. One `ExrData::load` already saturates the cores via the `exr` crate's internal rayon pool, and nesting another pool oversubscribes the machine. That constraint is the [concurrency contract](Concurrency-contract).

## inspect_exr

`inspect_exr` dumps part count, per-part compression, channel sample types, data and display windows, and the decoded size that sizes the T1 playback cache. A path is required. It used to fall back to hardcoded absolute paths on a production share; do not put a default back.

```bash
cargo run --bin inspect_exr -- <file.exr> [more.exr ...]
```

With no arguments it prints usage and exits 2:

```text
usage: inspect_exr <file.exr> [file.exr ...]
```

Decoded size sums each channel's data-window pixel count times its sample size (half = 2, float = 4, uint = 4). That is the figure `ExrData::approx_bytes` uses. Compressed file size is not cache occupancy.

`check_types` is a scratch binary that reads `./test.exr`. It is not a general tool.
