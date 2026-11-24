The suite covers parsing, the batch converter, color and tone math, playback, and headless GUI interaction. It runs on a plain CI runner. The full write-up, including the OpenEXR corpus notes, is [`TESTING.md`](https://github.com/byvfx/floki/blob/main/TESTING.md).

Hands-on testing of a shipped build is [`docs/release-testing/`](https://github.com/byvfx/floki/blob/main/docs/release-testing/) (currently [`v1.13.3.md`](https://github.com/byvfx/floki/blob/main/docs/release-testing/v1.13.3.md)). That folder is not a guide for every past tag, and it is not on this site's nav. The automated suite is structurally blind to anything whose trigger is human timing.

## Commands

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --all-targets --no-default-features
```

`--no-default-features` selects the pure-Rust OCIO stub. That is the feature set CI lints and tests. Dropping the flag locally checks something the gate never does and, without an OCIO install, fails to build.

CI runs all three in a Test & Lint job that **gates** the build matrix (see [`.github/workflows/build.yml`](https://github.com/byvfx/floki/blob/main/.github/workflows/build.yml)). Every pull request that touches code runs it, **including one stacked on another feature branch** — the trigger has no `branches:` filter, because `branches:` matches a PR's base and once meant a stacked PR ran no CI while still reporting mergeable (#265).

A docs-only PR runs **nothing**: `paths-ignore` skips `**.md`, `docs/**`, `assets/**`, `LICENSE`, and `.gitignore`. A green PR with no checks is expected there. Tell it apart from #256 / #265, where a *code* PR reported `CLEAN` having run nothing.

## What is covered

| Area | Where |
|---|---|
| `LogicalLayer` regrouping, including a generated Blender-style EXR | `src/exr_loader.rs` |
| Batch channel rename, monotonic progress, cancellation | `src/tools.rs` |
| `.cube` parser, including every error path | `src/color/cube.rs` |
| Exposure, gamma, sRGB | `src/render_math.rs` |
| `Uniforms` size and alignment, `ChannelMode` → `u32` | `src/gpu/mod.rs` |
| Layer rects, panes, display window, overscan | `src/viewer.rs`, `src/app.rs`, `src/layer.rs` |
| Playback transport, T1 ring, sequences, scheduler, budgets, disk proxy, read-ahead | `src/playback.rs`, `src/cache.rs`, `src/sequence.rs`, `src/scheduler.rs`, `src/budget.rs`, `src/proxy_cache.rs`, `src/prefetch.rs` |
| Headless keyboard and compare layout | `src/viewer.rs` (`gui_tests`), via `egui_kittest` |

The T1 cap arithmetic is reachable: `tick_budgets` delegates to `tick_budgets_t1(&Sample)`. `Sample` lives in `budget.rs` so a test can build one. The T2 VRAM ring was removed in #299. Counts in `TESTING.md` (transport 30, ring 24, sequences 20, scheduler 18, budgets 6, proxy 8, warmer 4) are the inventory there; treat that file as the list that moves.

DWA fixtures under `tests/fixtures/` are the one committed binary pair. `exr` can decode DWAA/DWAB and cannot write them, so they cannot be generated in the test. Everything else is built with `tempfile` in a temp dir. `*.exr` is gitignored apart from that exception.

## On-device tests

`gpu::ocio_pass::device_tests` and `gpu::thumbnail` acquire a real device. They exist because the accumulate blend, the α=−1 "no image" sentinel, and whether `shader.wgsl` compiles have no CPU stand-in.

They **skip** rather than fail when the machine cannot supply a device. Both of these must be checked, and the shared guard is `crate::gpu::test_device`:

| Condition | Where it bites |
|---|---|
| No adapter at all | Headless runners with no Vulkan, Metal, or DX |
| An adapter without `FLOAT32_FILTERABLE` | Test & Lint on `ubuntu-latest`, which has llvmpipe |

An adapter-only guard passes on Ubuntu and then panics in `request_device`. Don't write another copy of the guard.

A skip reads as a pass. CI covers the GPU-free suite. A real GPU is where the render seams are verified. `cargo test --no-default-features --features vendored` on a Mac is the only configuration that runs every test, because `metal_tests` needs macOS and an OCIO feature. Since #269 the macOS OCIO job's vendored clippy (`--all-targets`) type-checks those modules. It does not execute them. GitHub's macOS runners have no device that clears the guard.

## Conventions

- Generate fixtures in a temp dir. Don't commit `.exr` binaries.
- Ordinary tests do not construct a wgpu device.
- GUI tests call `ExrViewer::handle_channel_hotkeys` so `egui_kittest` can drive egui without building the canvas. Inline `#[cfg(test)]` modules are for crate-private items. `tests/` and `benches/` are for the public surface `src/lib.rs` exposes.
- `ChannelMode::as_u32` in `src/viewer.rs` is the one encoding (`RGB=0, R=1, G=2, B=3, A=4`). `gpu/shader.wgsl` must match it. A test in `gpu/mod.rs` locks the values.

The official OpenEXR images can be cloned locally and are gitignored (`assets/openexr-images/`, about 500 MB). Nothing in CI depends on them. `Beachball/` is the #217 pair: eight multipart frames (10 parts, one logical layer each) against eight single-part frames (the same 10 passes as channel prefixes). With the proxy off, a 12 s soak showed `soft=1` on the multipart AOVs and `soft=0` on the single-part file — the fast path carrying the requested layer, and the scope limit refusing it when every pass shares a part. The fps figures in those runs mean nothing: a 2.9 MB F16 frame decodes either way. Compare the two files by pass name, not by index.

For playback changes, a real interactive session is a merge gate. The soak harness auto-plays and never idles, so a bug whose trigger is a human pause is invisible to it.
