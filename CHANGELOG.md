# Changelog

All notable changes to Floki are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- Vendored OCIO build (`--features ocio-vendored`) now links on Windows machines
  that have vcpkg's user-wide MSBuild integration (`vcpkg integrate install`).
  That integration silently injected vcpkg's headers (including a different
  yaml-cpp ABI) into OCIO's Visual Studio build, producing `LNK2019`
  unresolved `__imp_*` symbols against OCIO's own statically built yaml-cpp.
  `build.rs` now builds OCIO hermetically (disables the vcpkg MSBuild hooks),
  so the vendored build is reproducible regardless of the host's vcpkg state.

### Added
- Convenience build wrappers for OCIO: `cargo ocio-run` / `cargo ocio-build` /
  `cargo ocio-test` (cargo aliases, zero install) and a `justfile` (`just ocio`)
  that also inits the OCIO submodule. The self-contained vendored build is now
  the documented, recommended cross-platform path.

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

[Unreleased]: https://github.com/byvfx/floki/compare/v1.4.4...HEAD
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
