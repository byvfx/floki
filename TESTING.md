# Testing

Floki's test suite covers parsing/import logic, the batch converter, color/tone
math, and headless GUI interaction. It runs on a plain CI runner. Fixtures are
generated into a temp dir rather than committed — with one deliberate exception,
the CI-gated DWA pair under `tests/fixtures/`, which cannot be produced
synthetically (see the compression note below).

> This document is the **automated** suite. For hands-on testing of a shipped
> build — what to exercise, known caveats, what to report — see
> [`docs/release-testing/`](docs/release-testing/), which carries one field-test
> guide per release. The two are complements: the suite is structurally blind to
> anything whose trigger is human timing, which is most of what field testing
> finds.

Almost all of it is **GPU-free** — that is the default and the rule for new
tests. The exception is a small set of **on-device** tests that validate the
render seams a CPU cannot stand in for (`gpu::ocio_pass::device_tests`,
`gpu::thumbnail`). Those acquire a real device and **skip** when the runner
can't provide one, so a GPU-less CI job still goes green; see
[On-device tests](#on-device-tests).

## Running the tests

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --all-targets --no-default-features
```

`--no-default-features` selects the pure-Rust OCIO stub. It is not optional if you
want to reproduce CI: the runner has no OpenColorIO installed, so that is the
feature set CI lints and tests. Dropping the flag locally checks something the
gate never does — and, without an OCIO install, fails to build at all. (See the
build tiers in [README](README.md#color-management-opencolorio).)

CI runs all three in a `Test & Lint` job that **gates** the build/release matrix
(see `.github/workflows/build.yml`). Every pull request that touches code runs it,
**including one stacked on another feature branch** — the trigger deliberately carries
no `branches:` filter, because `branches:` matches a PR's *base* and once meant a
stacked PR ran no CI at all while still reporting mergeable (#265).

A docs-only PR is the exception, and runs **nothing**: `paths-ignore` skips `**.md`,
`docs/**`, `assets/**`, `LICENSE` and `.gitignore`. A green PR with no checks at all is
expected there — and is worth telling apart from the failure mode in #256/#265, where a
*code* PR reported `CLEAN` having run nothing and looked identical.

## What is covered
| Area | Location | Notes |
|------|----------|-------|
| EXR channel regrouping (`LogicalLayer`) | `src/exr_loader.rs` | Pure helpers **and** full `ExrData::load` integration on a generated Blender-style EXR. |
| Batch channel-rename converter | `src/tools.rs` | `canonical_rgba` aliases, sort-safety skip, and `run_conversion_task` over a temp dir (progress monotonicity + cancellation). |
| `.cube` 3D LUT parser | `src/color/cube.rs` | Valid parse, domain handling, comment skipping, and every error path. |
| Tone / color math | `src/render_math.rs` | Exposure, gamma, sRGB transfer (round-trips). Shared by the CPU fallback and mirrored by `gpu/shader.wgsl`. |
| GPU uniform layout | `src/gpu/mod.rs` | `Uniforms` size/alignment + `Pod` round-trip, and the `ChannelMode` → `u32` encoding contract. |
| Composite layer placement | `src/viewer.rs` (`gui_tests`) | `comp_layer_rect` / `comp_pane_layout` / `side_by_side_layout` — each layer at its own `tex_size × PAR` (#254). Pure, no device. |
| Per-layer pixel aspect | `src/layer.rs` | `Draw::effective_par` — the override-vs-header precedence, and that one layer's override leaves its siblings alone (#263). |
| Format & overscan geometry | `src/viewer.rs` (`gui_tests`), `src/app.rs` | `comp_display_rect` / `overscan_of` / `Overscan::label` — the display box placed from the data window, overscan as a percentage of the format across both sides, and crop vs overscan vs both (#251). `comp_format_of` is checked against an overscanned fixture, since a version reading the *data* window passes every square-window test and misreports exactly the renders the readout exists for. |
| **Playback: transport** | `src/playback.rs` (30) | State machine, drift-corrected clock, loop/ping-pong wrap, trim, epoch, and the frame-time percentile ring (#236 — an idle span must not be filed as one frame). |
| **Playback: T1 ring** | `src/cache.rs` (24) | `(SourceId, frame)` keying, directional-ring + LRU eviction, protected playheads, and the byte/count `Bound` (#232). |
| **Playback: sequences** | `src/sequence.rs` (20) | Detection, numeric sort, holes, and path-for-frame resolution. |
| **Playback: scheduler** | `src/scheduler.rs` (18) | Pure want-list priority (P1 playhead → P2 ahead → P3 read-behind), plus `want_first_n`'s bounded walk asserted against `want_list`'s prefix. |
| **Playback: budgets** | `src/budget.rs` (6), `src/app.rs` | The bytes→count conversion, and — since the #288 T1/T2 split — the **cap arithmetic itself** via `tick_budgets_t1(&Sample)`, where #215 / #230 / #232 / #233 / #322 actually landed. |
| **Playback: disk proxy cache** | `src/proxy_cache.rs` (8) | Key derivation (path + mtime + size + px + aov), filename hashing, and blob round-trip. |
| **Playback: read-ahead warmer** | `src/prefetch.rs` (4) | Dedupe ring bounds, survival of missing files, and the gated `WarmJob` hand-off (#309). |
| DWA decode (committed fixture) | `src/exr_loader.rs` | The one CI-gated binary fixture pair — DWA output cannot be produced synthetically. |
| Render seams (on device) | `src/gpu/ocio_pass.rs` (`device_tests`), `src/gpu/thumbnail.rs` | Accumulate ping-pong blend vs a CPU reference, the layer-rect union (#257), blit coverage/checker, sRGB display-encode. Skips without a capable GPU — see [On-device tests](#on-device-tests). |
| GUI interaction (headless) | `src/viewer.rs` (`gui_tests`) | Drives `ExrViewer::handle_hotkeys` through `egui_kittest` — channel keys, compare modes, contact-sheet gating, B-image gating. |

## On-device tests

A handful of tests acquire a real GPU. They live in
`gpu::ocio_pass::device_tests` (the accumulate ping-pong, the blit, the
sRGB display-encode) and `gpu::thumbnail`. They exist because these are
render seams with no CPU equivalent to assert against: the ping-pong's
premultiplied-alpha blend, the α=−1 "no image" sentinel, and whether
`shader.wgsl` even compiles.

**They skip rather than fail** when the machine can't supply a device, so a
GPU-less CI runner still goes green. There are **two** ways to come up short
and both must be checked:
| Condition | Where it bites |
|---|---|
| No adapter at all | headless runners with no Vulkan/Metal/DX |
| An adapter without `FLOAT32_FILTERABLE` | **`Test & Lint` on ubuntu-latest**, which has llvmpipe in software |

The second one is the trap. An adapter *is* found on ubuntu, so an
adapter-only guard sails past it and then panics in `request_device` —
`GpuState` requires `FLOAT32_FILTERABLE` for the f32 3D LUT. That is exactly
how the ungated tests failed CI the first time they ran there.

**Any new on-device test must check both — by calling `crate::gpu::test_device`**
(`src/gpu/mod.rs`), which is the one shared guard. It used to be module-private,
with `gpu::thumbnail`'s tests carrying their own inline copies; three copies of one
guard is the shape that let the `FLOAT32_FILTERABLE` condition go missing from all of
them at once, so #266/#268 hoisted it somewhere every module can reach. Don't write a
fourth copy.

Two consequences worth knowing:

- **A skip reads as a pass.** `Test & Lint` reports these green having run
  none of them. Treat CI as covering the GPU-free suite only, and a real GPU
  (any dev machine; macOS for the OCIO ones) as where the render seams are
  actually verified.
- **macOS with an OCIO feature runs the most.** `metal_tests` additionally
  needs `target_os = "macos"` and `system-ocio` or `vendored`, so
  `cargo test --features vendored` on a Mac is the only configuration that
  runs every test in the repo.
- **Compiled ≠ run.** Those two macOS+OCIO modules (`ocio_pass::metal_tests`,
  `thumbnail::ocio_tests`) were once built by *no* CI job at all — every job was
  either the wrong `target_os`, feature-less, or a `cargo build` that skips test
  code — so they could rot undetected. Since #269 the macOS OCIO job runs the
  vendored clippy with `--all-targets`, which type-checks them on every PR. That
  catches a module that stopped compiling; it still does not execute them, and
  GitHub's macOS runners have no device that clears the guard above.

## Conventions

- **Generate fixtures in a temp dir** (`tempfile`) the way the existing
  `tools.rs` tests do — do not commit `.exr` binaries (`*.exr` is gitignored;
  the few files under `assets/` are small, deliberately force-added smoke
  fixtures).
- **No live GPU in tests, by default.** `viewer::ui` accepts
  `render_state: Option<&RenderState>`; tests pass `None` and assert on state.
  Reach for an on-device test only when the thing under test *is* the render
  seam and no CPU stand-in can observe it — see below.
- **GUI tests target a rendering-free seam.** `ExrViewer::handle_hotkeys` holds
  the keyboard-driven state changes so `egui_kittest` can exercise the real egui
  input pipeline without building the full canvas. GUI tests live in an inline
  `#[cfg(test)]` module because `ExrViewer::handle_hotkeys` is crate-private —
  that is the rule generally: **inline for crate-private items, `tests/` or
  `benches/` for the public surface**. `src/lib.rs` exists precisely so the
  public surface is reachable from separate crates (the `exr_load` benches use
  it), so "a binary crate can't be tested from `tests/`" is not the reason.
- **The `channel_mode` encoding has one source of truth:**
  `ChannelMode::as_u32` in `src/viewer.rs`. `gpu/shader.wgsl` must match it; a
  test in `gpu/mod.rs` locks the values.

## OpenEXR reference corpus (local, not committed)

The official test images from <https://openexr.com/en/latest/test_images/> — the
canonical set every EXR reader is expected to survive. Cloned locally rather than
vendored; ~500 MB on disk, gitignored wholesale:

```bash
git clone --depth 1 https://github.com/AcademySoftwareFoundation/openexr-images.git assets/openexr-images
```

97 `.exr` files across 11 categories, plus a 186-file damaged corpus. BSD-3-Clause
(ILM 2004). Nothing in the suite depends on it — it is a manual bench, and the
committed fixtures in `tests/fixtures/` stay the CI-gated ones.

What each category is worth *here*, as opposed to in general:
| Category | Why it matters to floki |
|---|---|
| `Damaged/` | 186 fuzz crashers (ASAN heap-OOB, SIGSEGV, truncation) that broke OpenEXR. Directly on point: the `byvfx/exrs` fork exists because a decompressor **panicked**, and on a rayon worker that is an uncatchable `process::abort()`. `ExrData::load`'s `catch_unwind` has never been tried against real malformed input. Note these deliberately have **no `.exr` extension** — they are not valid files. |
| `TestImages/` | NaN, infinity and denormalized pixel values — the tone-map, histogram and pixel-readout edge cases. |
| `MultiView/`, `v2/` | Multi-part and multi-view files: the shape `specific_layer` / `single_layer_part` (#217) reasons about, from a source other than one studio's renders. |
| `MultiResolution/` | Mip/ripmapped EXRs. `largest_resolution_level()` is asserted everywhere; this is what proves it. Also the reduced-res read mrv2 has and floki does not (see `docs/perf-roadmap.md`). |
| `DisplayWindow/` | Data vs display window mismatches — the geometry the viewport frames by, and the geometry `write_proxy_blob` explicitly preserves (#163). |
| `Chromaticities/`, `LuminanceChroma/` | Colour path. `LuminanceChroma` is subsampled Y/RY/BY, which floki has never been tested against and may not handle at all. |
| `ScanLines/`, `Tiles/` | Scanline vs tiled layout across compression types. |

**`Beachball/` is the #217 test case**, and better than the studio renders it was
built against, because it is a controlled pair:
| | frames | layout |
|---|---|---|
| `multipart.0001–0008.exr` | 8 | 10 parts, **one** logical layer each |
| `singlepart.0001–0008.exr` | 8 | **1** part, the same 10 passes as channel prefixes |

Identical content, both playable sequences, differing only on the axis the scope
limit is about. `single_layer_part` must take the fast path on every AOV of the
first and refuse every non-zero AOV of the second — with the picture identical
either way, just slower. Independent of any one studio's naming, which the
`byvfx` fixtures are not.

A caveat on what this corpus does *not* cover: no DWAA/DWAB anywhere in it, so it
says nothing about the codec `docs/perf/dwa-arm-decode-investigation.md` is about.
It does hit ZIP1/ZIP16 and PXR24 heavily, which are exactly the two call sites the
`byvfx/exrs` fork patches — so it exercises the fork's changed paths, just not the
ARM question.

### Beachball soak results (2026-08-18, #217 merged)

Three 12 s soaks, comp layer on the named AOV, **proxy off** so the only cheap
path is `load_beauty`/`load_layer`:
| footage | AOV | build failures | `soft=` playing | settles |
|---|---|---|---|---|
| `multipart` | 1 — `depth_left` | 0 | **1** | `s2:8full` |
| `multipart` | 7 — `disparityL` | 0 | **1** | `s2:8full` |
| `singlepart` | 1 | 0 | **0** | `s2:8full` |

Why that is decisive rather than merely green. With the proxy off, `soft=1` means
a *partial* decode is on screen. A `load_beauty` frame carries only logical layer
0, and `resolve_logical` is strict — it returns `None` for any other index — so
such a frame would have failed every build. Zero failures at AOV 1 and 7 therefore
means the partial frame genuinely carried those layers, which only `load_layer`
produces. `singlepart` sitting at `soft=0` throughout is the scope limit working:
same passes, one part, so the gate refuses and takes the full decode.

AOV **7** is checked deliberately. Index 1 is the weakest possible test of an
index remap, since 0-vs-1 confusions pass by luck; 7 is a high index *and* a
2-channel `x/y` vector pass, where 1 is a single `Z` channel.

**The fps figures in these runs mean nothing.** Every configuration held ~24–26
fps including `singlepart`, because a 2.9 MB F16 frame decodes trivially and the
full-decode path is indistinguishable from the fast one at that size. This is a
correctness bench; the perf case stays a real render, where the same refusal is
the difference between playing and freezing.

**AOV indices do not correspond between the two files.** `multipart` takes its
layer order from part order (`rgba_right`, `depth_left`, `forward_left`, …);
`singlepart` takes it from channel-prefix grouping (root `RGBA`, `Z`,
`disparityL`, `disparityR`, `forward.left`, …, `left`), so index *n* is a
different pass in each, and most names differ too (`depth_left` vs `left.Z`).
Compare across the pair **by name, not by index** — `disparityL` / `disparityR`
are named identically in both and are the clean cross-file pair.
