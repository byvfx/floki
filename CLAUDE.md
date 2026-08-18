# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Floki is a native GPU-accelerated GUI for inspecting and A/B-comparing multi-layer OpenEXR files (built for VFX/compositing TDs). Built on `eframe`/`egui` (immediate-mode UI) with a custom `wgpu` render pipeline, parsing via the pure-Rust `exr` crate.

## Commands

```bash
cargo run --release        # Run the app — ALWAYS use --release; debug EXR parsing is painfully slow
cargo build                # Debug build (compile check)
cargo fmt                  # Required before committing (see CONTRIBUTING.md)
cargo clippy               # Required; fix all warnings before committing
cargo test                 # Run tests
```

Logging is via `env_logger` to stderr; filter with the `floki=` target prefix to silence wgpu/eframe noise:

```powershell
$env:RUST_LOG = "floki=debug"; cargo run --release   # PowerShell
```

### Testing (see TESTING.md)
The suite is **GPU-free** and gated in CI (`Test & Lint` job → blocks `build`). Run
`cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test --all-targets` before committing. Conventions: generate EXR/`.cube`
fixtures in a temp dir via `tempfile` (no committed binaries); never create a wgpu
device in tests (`viewer::ui` takes `render_state: Option<&RenderState>` — pass
`None`). Headless GUI tests drive `ExrViewer::handle_hotkeys` through `egui_kittest`
(`gui_tests` in `viewer.rs`); because this is a binary crate, all tests live in
inline `#[cfg(test)]` modules. Tone/color math lives in `render_math.rs`; the
`channel_mode` integer encoding's single source of truth is `ChannelMode::as_u32`
(`viewer.rs`), mirrored by `gpu/shader.wgsl`.

### Auxiliary binaries (`src/bin/`)
- `cargo run --bin convert_dir -- <input_dir> [output_dir]` — headless batch channel-rename converter (wraps `tools::run_conversion_task`).
- `cargo run --bin inspect_exr -- <file.exr> [more.exr ...]` — dumps part / channel / compression layout. Paths are **required**: it used to fall back to hardcoded absolute paths on a production share, which is real internal location data in a public repo and in shipped build artifacts. Don't reintroduce a default.
- `check_types` is a throwaway scratch tool that reads `./test.exr` — not a general utility.

## Architecture

Entry: `main.rs` builds the `eframe` app and — critically — enables the wgpu `FLOAT32_FILTERABLE` device feature (needed to linearly sample the f32 3D LUT texture). `app.rs::ExrApp::new` constructs `GpuState` once and stashes it in `render_state.renderer.callback_resources`.

Data flow: `app.rs` (state + menus + persistence) → `exr_loader.rs` (parse file → `ExrData`) → `viewer.rs` (canvas, interaction, sampling, builds per-frame GPU uniforms or CPU fallback) → `gpu/mod.rs` + `gpu/shader.wgsl` (render).

### GPU rendering (the core mechanism)
Rendering is a single WGSL shader (`gpu/shader.wgsl`) invoked through an `egui_wgpu::CallbackTrait` impl (`ExrCallback` in `gpu/mod.rs`). It binds **4 bind groups in fixed order**: `tex_a`, `tex_b`, `uniforms`, `lut`. The shader does all exposure / gamma / sRGB / channel-isolation / `|A-B|` diff / LUT work on the GPU. `viewer.rs` has a parallel CPU fallback path for when no GPU is available.

Three things must stay in lockstep when touching rendering:
1. The `Uniforms` struct (`gpu/mod.rs`) and the matching WGSL uniform struct in `shader.wgsl` — including the explicit `pad*` fields for alignment.
2. The `channel_mode` integer mapping (`RGB=0, R=1, G=2, B=3, A=4`) is duplicated in `viewer.rs` and the shader.
3. `ExrCallback::paint` force-resets the wgpu viewport to the full physical screen — egui_wgpu otherwise clips the quad to the primitive's bounding box and the screen-space math breaks.

### LogicalLayer regrouping (non-obvious, key domain concept)
`exr_loader.rs` defines `LogicalLayer`, which regroups a physical EXR layer's flat channel list into displayable passes by **dotted-name prefix** (`diffuse.R`/`diffuse.G` → pass `diffuse`). This exists because Blender writes every render pass into a *single* EXR part as channel-name prefixes (`ViewLayer.Combined.R`, ...), which the `exr` crate surfaces as one unnamed layer. Without this regrouping the passes are invisible. R/G/B/A slot indices are resolved at load time so rendering never re-matches names.

### State & persistence
`ExrApp` derives serde `(default)`. Fields that should persist across sessions (recent files, LUT path, `enable_lut`, OCIO path) are plain fields; all transient/runtime state (loaded data, GPU handles, conversion progress, window-open flags) is marked `#[serde(skip)]`. Persistence is handled by `eframe` storage. Image B (reference image) is reset whenever Image A changes.

### Color / LUT
`color/cube.rs` parses Adobe `.cube` 3D LUTs into an RGBA `Vec<[f32;4]>`; `gpu/mod.rs::create_lut_bind_group` uploads it as an `Rgba32Float` 3D texture for in-shader color transforms.

### Batch converter (`tools.rs`)
`run_conversion_task` renames EXR channels across a directory in parallel via `rayon`. Progress is reported over an `mpsc` channel as `(completed, total, msg)` using a shared `AtomicUsize` so the count only moves forward despite out-of-order parallel completion; cancellation is an `Arc<AtomicBool>`. The GUI (Tools window in `app.rs`) and the `convert_dir` binary are two front-ends over this one function.

## Patched `exr` dependency (fork)

`exr` is **not** the stock crates.io build. `Cargo.toml` has a `[patch.crates-io]`
pointing at a fork:

```toml
[patch.crates-io]
exr = { git = "https://github.com/byvfx/exrs", branch = "miniz-inflate-1.74.1" }
```

The branch name carries the upstream version it is rebased onto, so it changes on
every bump — check `Cargo.toml`/`Cargo.lock` rather than trusting this snippet.

**Why:** stock `exr` decompresses ZIP/PXR24 blocks with `zune-inflate`, which panics
(`assertion failed: bits_left >= LOOKAHEAD`) on some *valid* streams. The decode runs on
a detached rayon worker, so the panic hits rayon's `AbortIfPanic` → `process::abort()` —
an **uncatchable** crash of the whole app. The fork swaps both decompression call sites
to `miniz_oxide` (already used for compression), which returns an `Err` instead of
panicking. With that, `ExrData::load` runs parallel decompression again and keeps a
`catch_unwind` only as cheap insurance for calling-thread parse panics.

The change is two call sites, in `src/compression/zip.rs` and `src/compression/pxr24.rs`:
replace the `zune_inflate::DeflateDecoder` / `.decode_zlib()` with
`miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, expected_byte_size)`, then
drop the `zune-inflate` dep from the fork's `Cargo.toml`.

**Operational notes:**
- `Cargo.lock` pins the exact fork commit, so builds are reproducible. CI / fresh clones
  fetch the fork automatically (it is **public** — keep it that way; do not delete or
  privatize `byvfx/exrs` or builds break).

**Upkeep when bumping `exr`:**
1. In the `byvfx/exrs` clone, branch `miniz-inflate-<newver>` off the new `vX.Y.Z` tag and
   rebase (re-apply the two-call-site change if it conflicts), then `git push`.
2. In this repo, point `[patch.crates-io]` at the new branch and `cargo update -p exr` to
   re-pin `Cargo.lock`, then build/test.
3. **Do not** push the change upstream to `johannesvollmer/exrs` — fork only.

## Releasing
Releases are tag-driven. To cut one: move the `## [Unreleased]` entries in
`CHANGELOG.md` into a new `## [X.Y.Z] - DATE` section (add the compare-link line
at the bottom), bump `version` in `Cargo.toml` (build once to sync `Cargo.lock`),
commit, then push tag `vX.Y.Z`. The `Build` workflow compiles the cross-platform
binaries and auto-fills the GitHub Release notes from the matching `CHANGELOG.md`
section. **The release job fails if that `## [X.Y.Z]` section is missing**, so the
changelog entry must exist before tagging.

## Conventions
- Edition 2024. Prefer the pure-Rust ecosystem for file formats and rendering; avoid heavy new dependencies (see CONTRIBUTING.md).
- Keep UI logic decoupled from parsing/processing logic.
- `*.exr` files are gitignored (large binaries); test assets live in `assets/`.
