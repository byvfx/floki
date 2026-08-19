# Testing

Floki ships a **GPU-free** test suite that covers parsing/import logic,
the batch converter, color/tone math, and headless GUI interaction. Everything
runs on a plain CI runner — no graphics device, no committed binary fixtures.

## Running the tests

```bash
cargo test                 # all tests (debug)
cargo fmt --all -- --check # formatting gate
cargo clippy --all-targets -- -D warnings  # lint gate (warnings are errors)
```

CI runs all three in a `Test & Lint` job that **gates** the build/release matrix
(see `.github/workflows/build.yml`).

## What is covered

| Area | Location | Notes |
|------|----------|-------|
| EXR channel regrouping (`LogicalLayer`) | `src/exr_loader.rs` | Pure helpers **and** full `ExrData::load` integration on a generated Blender-style EXR. |
| Batch channel-rename converter | `src/tools.rs` | `canonical_rgba` aliases, sort-safety skip, and `run_conversion_task` over a temp dir (progress monotonicity + cancellation). |
| `.cube` 3D LUT parser | `src/color/cube.rs` | Valid parse, domain handling, comment skipping, and every error path. |
| Tone / color math | `src/render_math.rs` | Exposure, gamma, sRGB transfer (round-trips). Shared by the CPU fallback and mirrored by `gpu/shader.wgsl`. |
| GPU uniform layout | `src/gpu/mod.rs` | `Uniforms` size/alignment + `Pod` round-trip, and the `ChannelMode` → `u32` encoding contract. |
| GUI interaction (headless) | `src/viewer.rs` (`gui_tests`) | Drives `ExrViewer::handle_hotkeys` through `egui_kittest` — channel keys, compare modes, contact-sheet gating, B-image gating. |

## Conventions

- **Generate fixtures in a temp dir** (`tempfile`) the way the existing
  `tools.rs` tests do — do not commit `.exr` binaries (`*.exr` is gitignored;
  the few files under `assets/` are small, deliberately force-added smoke
  fixtures).
- **No live GPU in tests.** `viewer::ui` already accepts
  `render_state: Option<&RenderState>`; tests pass `None` and assert on state.
  Render-only logic that genuinely needs a device is out of scope for the suite
  and is validated by the build step / manual `cargo run --release`.
- **GUI tests target a rendering-free seam.** `ExrViewer::handle_hotkeys` holds
  the keyboard-driven state changes so `egui_kittest` can exercise the real egui
  input pipeline without building the full canvas. This is a binary crate, so
  GUI tests live in an inline `#[cfg(test)]` module (a `tests/` integration
  crate can't reach binary internals).
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
