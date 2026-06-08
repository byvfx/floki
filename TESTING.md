# Testing

EXR Analyzer ships a **GPU-free** test suite that covers parsing/import logic,
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
