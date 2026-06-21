# Changelog

All notable changes to Floki are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Internal: render-side proxy first-paint path (#58).** Adds a low-res
  `ProxyImage` (standalone RGBA32Float buffer + full image dimensions) and a
  viewer proxy texture slot with a tone-baked upload (exposure/gamma/sRGB +
  background, mirroring the CPU `generate_texture` path). While the full-res
  `ExrData` decode is in flight, the loading branch renders the proxy instead
  of a spinner; when the full decode lands, `swap_image_data` (#55) clears the
  proxy and the viewport swaps to full-res with zoom/pan preserved. The proxy
  uses the non-OCIO tone pipeline even when OCIO is active (transient stand-in;
  OCIO-accurate proxy is a follow-up). The decode-side producer (a true low-res
  EXR read) is #33 — `ExrApp::set_proxy` is the seam it will call from the
  worker. No user-facing change yet (nothing produces a proxy).

### Changed
- **Internal: decouple GPU core from `egui_wgpu::Renderer` ownership (#54).**
  Introduces an app-owned `GpuResources` (`src/gpu/resources.rs`) as the single
  home for the persistent `GpuState`; the application is now the source of
  truth, with egui's `callback_resources` holding only an `Arc<GpuState>` clone
  for the `CallbackTrait` paint callbacks. The viewer and app read `GpuState`
  directly off `GpuResources` instead of a per-frame `renderer.read()` typemap
  lookup. OCIO pass/targets still live in `callback_resources` (the OCIO
  callback's `prepare` mutates `OcioTargets` through the typemap), but their
  lifecycle is centralized behind `GpuResources::publish_ocio_pass` /
  `invalidate_ocio_targets`, replacing the hand-rolled `insert` +
  `remove::<OcioTargets>()` footgun in `rebuild_ocio_pass`. Prerequisite for
  the Qt port (#44) and clean resource management for #7 / #24 / #33. No
  user-facing change.
- **Internal: split image-data swap from viewer session-state reset (#55).**
  Extracted `swap_image_data` (replaces the pixel source for A or B while
  preserving zoom, pan, compare mode, channel mode, annotations, swatches, and
  tone/OCIO/LUT state) and `reset_viewer_session` (the full reset used on an
  explicit open / new session) as named seams. The open path still resets the
  viewer and drops B exactly as before; the new swap path is the contract
  image-sequence playback (#7) will use for per-frame loads so a frame change
  doesn't wipe the user's view. Also clamps `active_layer` to the new image's
  layer count on swap so a frame with fewer passes can't index out of bounds.
  No user-facing change.

## [1.7.2] - 2026-06-20

### Changed
- **Internal maintainability only — no user-facing changes.** Acted on the
  remaining items from the June 2026 codebase audit: broke up the ~1100-line
  `ExrApp::ui` per-frame entry point into focused per-panel methods, documented
  the `floki-ocio` public API (`#[must_use]` + `# Errors`), added a
  `.clang-format` for the C++ OCIO shim, and cleared a batch of lint findings
  (signature cleanups, struct field order). Behaviour is unchanged; the full
  test suite and `clippy -D warnings` pass.

## [1.7.1] - 2026-06-20

### Added
- **Resource monitor.** A discrete status-bar readout shows floki's own memory
  footprint and system RAM, plus live GPU **VRAM** usage on macOS (Metal) — handy
  when loading heavy EXRs or sequences. It samples about once a second and tucks
  into the bottom-right. Windows and Linux show RAM only for now.

### Fixed
- **Snapshots crop to the active image area.** Saved snapshots and clipboard
  copies now contain just the displayed image (the display window, clamped to the
  visible canvas) instead of the entire viewer canvas including the surrounding
  background. Side-by-side still captures the full canvas. (#52)

## [1.7.0] - 2026-06-19

### Added
- **Diff heat-map visualization controls.** The Diff/Matte view's fixed black-body
  ramp is now configurable: choose a colormap (black-body, grayscale, turbo,
  viridis, magma, inferno) or build a custom multi-stop gradient in the editor;
  pick the magnitude metric (max channel, Rec.709 luminance, or per-channel RGB);
  set a noise-floor threshold; and read the gain-to-colour mapping off a legend.
  The colormap is shared by the GPU and CPU paths through a reusable gradient
  module.
- **Customizable viewport background.** The transparency backdrop can now be a
  checkerboard (configurable cell colours and size), a solid colour, or a
  multi-stop gradient at any angle, set from **View ▸ Viewport Background**. Named
  presets are saved and persist across sessions, and the background composites
  consistently across the GPU, CPU, and OCIO paths.
- **Snapshot to clipboard.** Copy the current view to the system clipboard with
  **Cmd/Ctrl+Shift+S** (or **View ▸ Snapshot to Clipboard**). The capture is
  exactly what's on screen — background, compare mode, OCIO, and any annotations.
  An optional toggle also writes a timestamped PNG to `~/.floki/snapshots/`.
- **Annotation overlay.** Mark up the view before snapshotting with arrows, boxes,
  a freehand pen, and text labels, each with adjustable colour and stroke width.
  Annotations anchor to image pixels (so they track pan/zoom), support undo/redo
  and clear-all, and are flattened into the snapshot automatically.

All four features reproduce prior behaviour by default, so existing workflows are
unchanged.

## [1.6.0] - 2026-06-18

### Added
- **Drag-and-drop loading.** Drop an EXR onto the window to load it — the left
  half loads it as Image A, the right half as the reference Image B. While you
  drag, a live overlay highlights the half that will receive the drop. Dropping
  two files at once loads the first as A and the second as B. (Because the
  windowing layer discards the OS drop position, floki queries the system cursor
  directly so the left/right split works on macOS and Windows.)

### Fixed
- Wipe compare-mode controls now use consistent left-aligned labels across all
  four sliders (Center X, Center Y, Angle, Line Opacity); the previously
  unlabeled center slider is now named.

## [1.5.3] - 2026-06-17

### Fixed
- The OCIO CPU display transform (and CPU composite path) now show nothing
  rather than wrong colors when the transform fails: previously the
  untransformed scene-linear buffer was clamped to [0,1] and displayed, silently
  presenting incorrect color with no indication the transform never ran.

### Changed
- Internal: hardened a set of panic-prone `unwrap()`s across the GPU/CPU canvas
  render paths (side-by-side draws, egui paint/prepare callbacks, and GPU
  resource lookups) into clean early-returns. These crossed function-call
  boundaries where the upholding invariant wasn't locally visible. No user-facing
  behavior change in the normal case — the app degrades gracefully instead of
  crashing if an invariant is ever violated.

## [1.5.2] - 2026-06-17

### Fixed
- Changing the OCIO config no longer produces a wgpu validation error (or
  silent black frame) when the window has not been resized since the previous
  config load. The cached scene bind group is now invalidated whenever the OCIO
  pipeline is rebuilt, so it is always created against the current pipeline
  layout.
- Clicking **Browse** for a LUT, then immediately browsing a second path, no
  longer auto-enables the LUT for the second load. The auto-enable flag is now
  cleared when a superseded (stale) LUT result is discarded.
- If the GPU or render state is unavailable when a LUT finishes loading, the
  LUT is now correctly disabled (`Enable LUT` unchecked) rather than left
  enabled with mismatched domain bounds — which previously caused the shader to
  apply a non-identity coordinate remap while sampling the fallback identity
  texture.

## [1.5.1] - 2026-06-16

### Performance
- EXR files now decode on a background thread, so opening a large multi-layer
  render no longer freezes the window — a loading spinner shows while it loads.
  (The decode itself is unchanged; the UI just stays responsive during it.)

### Changed
- Internal: split the monolithic viewer `ui()` into focused units (contact
  sheet, pixel sampling, and the GPU/CPU canvas render paths), added a
  `criterion` benchmark harness for EXR load, and expanded API docs and tests.
  No user-facing behavior change.

## [1.5.0] - 2026-06-16

### Added
- **Color management ships by default.** Release binaries now statically bundle
  OpenColorIO (vendored OCIO 2.4.2), so OCIO color management works out of the
  box with no install or C++ toolchain on the user's machine. Previously OCIO
  was a manual opt-in build.
- Convenience build wrappers for OCIO: `cargo ocio-run` / `cargo ocio-build` /
  `cargo ocio-test` (cargo aliases, zero install) and a `justfile` (`just ocio`)
  that also inits the OCIO submodule. The self-contained vendored build is now
  the documented, recommended cross-platform path.

### Fixed
- Vendored OCIO build (`--features ocio-vendored`) now links on Windows machines
  that have vcpkg's user-wide MSBuild integration (`vcpkg integrate install`).
  That integration silently injected vcpkg's headers (including a different
  yaml-cpp ABI) into OCIO's Visual Studio build, producing `LNK2019`
  unresolved `__imp_*` symbols against OCIO's own statically built yaml-cpp.
  `build.rs` now builds OCIO hermetically (disables the vcpkg MSBuild hooks),
  so the vendored build is reproducible regardless of the host's vcpkg state.
- `cargo run` is no longer ambiguous against the `src/bin` helper binaries
  (`default-run = "floki"`).

## [1.4.4] - 2026-06-14

### Fixed
- A depth `Z` channel packed alongside an unprefixed `R,G,B,A` beauty pass no
  longer overwrites the Blue channel (it rendered pure white). Channel-to-slot
  resolution now prioritizes canonical color names (`R/G/B/A`) over geometric
  aliases (`X/Y/Z`), so a real Blue is never clobbered by depth.
- Leftover non-color channels (e.g. depth `Z`) in an RGBA group are now surfaced
  as their own grayscale layer instead of being silently dropped.

## [1.4.3] - 2026-06-14

### Performance
- Acquire the renderer read-lock once per frame in the GPU draw path instead of
  on every draw (2–4× per frame with overscan / Side-by-Side).
- Channel grouping on load is now O(n) in channel count (was O(n²)), which
  matters for Blender EXRs that pack 100+ channels into one part.
- Status-bar channel summary builds with an early length cap instead of joining
  every layer name and then truncating.

### Fixed
- Rebuild the LUT bind group on startup so a persisted **Enable LUT** actually
  applies instead of silently doing nothing until the file is re-browsed.
- Histogram cache now self-validates on `(active layer, log scale)`, fixing stale
  bins when toggling log scale and a missing Image B histogram after loading B.
- `.cube` parser rejects malformed and non-finite (`NaN`/`inf`) rows instead of
  silently dropping them or uploading garbage into the LUT texture.
- Disabled the inert OCIO **Browse** button and marked it "Coming soon".

## [1.4.2] - 2026-06-14

### Added
- Two-tier viewer toolbar, theme picker, and recent A/B file lists.

## [1.4.1] - 2026-06-12

### Added
- Rotatable wipe compare mode with adjustable center, angle, and line opacity.

## [1.4.0] - 2026-06-11

### Added
- Missing comparison shortcuts and an adjustable blink-speed control.

### Changed
- Bumped CI runners to Node.js 24.

## [1.3.1] - 2026-06-10

### Changed
- Renamed the project from "EXR Analyzer" to **Floki**.

## [1.3.0] - 2026-06-10

### Added
- Viewer quality-of-life features: fullscreen, unload, pixel sampling, tone
  reset, and compositing.

## [1.2.3] - 2026-06-07

- Maintenance release.

## [1.2.2] - 2026-06-06

### Added
- Floating tooltip toggle and color swatch.
- Overscan opacity slider.
- Nuke-style RGBA-colored text, HSVL readout, data-window dashed boxes, and a
  bottom status bar mirroring the A/B info.
- Swatch, HSVL, and layer name for Image B in the status bar.
- Mouse controls in the help menu.

### Changed
- Redesigned the status bar to match Nuke's layout with multiline A and B info.
- Left sidebar now scrolls so its content can't overflow the window.
- Overscan is hidden by default and indicated by the bounding box.

### Fixed
- Prevent an app crash when EXR block decompression panics.
- Switched EXR decompression off `zune-inflate` to fix load crashes.
- Status bar disappearing when Image B is loaded.
- Clamp the active layer when sampling Image B pixels to prevent data from
  disappearing.
- Missing status bar and Side-by-Side hover coordinates.
- Contact-sheet row alignment.
- Compiler warnings.

## [1.2.1] - 2026-06-05

### Added
- `convert_dir` CLI for headless batch EXR conversion.
- Group Blender single-part EXR channels into selectable passes.

### Fixed
- Contact-sheet clicks, wipe/diff in the contact sheet, and normalized
  Side-by-Side sizes.

## [1.2.0] - 2026-06-04

### Added
- `RUST_LOG` logging to the EXR converter.
- Converted/total summary (and failure count) shown when conversion finishes.

### Changed
- Parallelized and optimized viewer texture generation.
- Optimized EXR header conversion using fast chunk copying.

### Fixed
- EXR converter corrupting non-RGBA passes (P/N xyz swap).
- EXR converter UI freeze and non-monotonic progress.
- Converter cancel button and assorted bugs.

## [1.1.0] - 2026-06-03

### Added
- Multi-threaded EXR header converter tool.
- Dual contact sheets for Image B and an Image B info panel.
- Tooltip displaying values and diff for both A and B.

### Fixed
- GPU viewport squishing in the paint callback.
- GPU screen-size uniform for correct UV mapping.
- Wipe clip-rect bounds.
- `exr_data_b` reference in the tooltip.

## [1.0.1] - 2026-06-03

### Fixed
- Release workflow permissions and paths.

## [1.0.0] - 2026-06-03

Initial release.

### Added
- GPU-accelerated multi-layer OpenEXR viewer with A/B comparison.
- 3D LUT (`.cube`) color management.
- Channel isolation, alpha checkerboard, and contact sheet.
- Persistent color-sampler swatches and a pixel tooltip.
- Advanced metadata header inspector.
- Cross-platform GitHub Actions builds (Linux, Windows, macOS).

[Unreleased]: https://github.com/byvfx/floki/compare/v1.5.2...HEAD
[1.5.2]: https://github.com/byvfx/floki/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/byvfx/floki/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/byvfx/floki/compare/v1.4.4...v1.5.0
[1.4.4]: https://github.com/byvfx/floki/compare/v1.4.3...v1.4.4
[1.4.3]: https://github.com/byvfx/floki/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/byvfx/floki/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/byvfx/floki/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/byvfx/floki/compare/v1.3.1...v1.4.0
[1.3.1]: https://github.com/byvfx/floki/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/byvfx/floki/compare/v1.2.3...v1.3.0
[1.2.3]: https://github.com/byvfx/floki/compare/v1.2.2...v1.2.3
[1.2.2]: https://github.com/byvfx/floki/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/byvfx/floki/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/byvfx/floki/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/byvfx/floki/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/byvfx/floki/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/byvfx/floki/releases/tag/v1.0.0
