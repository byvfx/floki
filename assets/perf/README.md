# Local performance fixtures

Drop real `.exr` renders in this directory to benchmark `ExrData::load` against
them. Run with:

```sh
cargo bench --bench exr_load
```

Every `*.exr` here is picked up automatically and benched under the
`exr_load/local` group (one entry per file, labelled by file stem). Point the
benches at a different directory with:

```sh
FLOKI_PERF_FIXTURES=/path/to/renders cargo bench --bench exr_load
```

## Not committed

`.exr` files are gitignored repo-wide (`*.exr`), so anything you put here stays
local — it never bloats the clone. The benches **skip this tier with a notice**
when the directory is empty, so `cargo bench` still runs the synthetic suite
clean on a fresh checkout and in CI.

Good fixtures to keep here: a few representative real renders (e.g. a heavy
multi-pass Blender/Houdini EXR, a large single-layer beauty, different
compression schemes) so the numbers reflect production files rather than only
the synthetic shapes.
