# Floki

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org)
[![egui](https://img.shields.io/badge/GUI-egui-blue)](https://github.com/emilk/egui)
[![wgpu](https://img.shields.io/badge/wgpu-Native-green.svg)](https://wgpu.rs)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

**Floki** is a fast, hardware-accelerated Rust GUI application tailored for Technical Directors, Compositors, and LookDev Artists who need to deeply inspect and compare multi-layered OpenEXR files. 

Powered by `egui`, `wgpu`, and the pure-Rust `exr` crate, it allows you to instantly view dense pixel data, isolate color channels, explore unbounded floating-point histograms, and perform pixel-perfect A/B diffing natively on your GPU.

---

## Key Features

### Hardware-Accelerated Rendering
* **Vulkan/Metal/DX12 Backend:** All image exposure scaling, gamma correction, sRGB mapping, and A/B difference matte compositing are executed via custom WGSL shaders on the GPU.
* **CPU Fallback:** Automatically drops down to multithreaded CPU rendering if a graphics card or driver is unavailable.

### Deep Inspection
* **Precision Pixel Sampling:** Hover over any pixel to reveal exact floating-point values (R, G, B, A) regardless of bit-depth (F16, F32, U32).
* **Persistent Swatches:** `Shift + Click` on the image to drop a permanent color swatch into your toolbelt for cross-referencing.
* **Metadata Explorer:** Cleanly displays embedded EXR attributes (like V-Ray/Arnold custom tags), layers, and bounding box data in collapsible panels for both Image A and Image B simultaneously.
* **Dual Contact Sheets:** Instantly view all AOVs (Arbitrary Output Variables) and layers as a scrollable grid of thumbnails. If a second image is loaded, view dual synchronized contact sheets side-by-side or seamlessly toggle between them.

### Advanced A/B Comparison
Load a Reference Image (Image B) to unlock advanced visual diffing:
* **Wipe:** Split-screen slider for boundary checks — with adjustable center, rotation angle, and divider-line opacity for wipes at any orientation.
* **Side-by-Side:** View Image A and Image B glued together in a continuous panorama. They share the same camera for synchronized panning and zooming.
* **Diff Matte:** Renders `|A - B| * multiplier` to easily spot fractional floating-point discrepancies in your render pipelines.
* **Composite:** Blend A over B directly in-viewport with selectable blend modes (Over, Under, Add, Multiply, Screen).
* **Blink Mode:** Press `Spacebar` to strobe between Image A and Image B at an adjustable interval.

Comparison controls follow a two-tier toolbar: the everyday controls stay on a single row, while the active mode's parameters slide into a contextual second row only when needed.

### Image Analysis
* **Dynamic Luminance Histogram:** Real-time histogram mapped to Exposure Values (EV stops). Effortlessly spot floating-point highlights over `1.0` using the Logarithmic view.
* **Dual Histogram Mode:** When comparing two images, the histogram overlays a translucent Red graph (Image B) on top of the White graph (Image A) so you can visually align black levels.
* **Channel Isolation:** Quickly isolate `R`, `G`, `B`, `A`, or view `RGB` composite with single-key shortcuts.

### Color Management
* **3D LUT Support:** Load Adobe `.cube` 3D LUTs and apply them in real time on the GPU as a display transform, alongside the built-in Exposure/Gamma/sRGB controls (OCIO config path is also configurable).

### Batch Tooling
* **EXR Header Converter:** Bulk-rename channels across an entire directory to standard RGBA — available both as an in-app Tools window and a headless `convert_dir` CLI, parallelized across CPU cores via `rayon` with live progress and cancellation.

### High-Performance UI
* **Immediate-Mode UI:** Built on `egui` for a responsive, minimal-overhead interface.
* **Light / Dark / System Themes:** Switch the interface theme from the **Theme** menu; the `System` option tracks your OS light/dark setting live. Your choice persists across sessions.
* **Recent Files for A & B:** `File ▸ Open Recent A` / `Open Recent B` reload a recent EXR straight into the main or reference slot.
* **Persistent State:** Remembers your UI layout, recent files list, theme, and preferences across sessions.
* **Software Tone Mapping:** Apply Exposure, Gamma, and sRGB transforms instantly without altering the underlying raw data.

---

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `F` | Frame Image (Reset Zoom & Pan to fit screen) |
| `R` | Isolate Red Channel |
| `G` | Isolate Green Channel |
| `B` | Isolate Blue Channel |
| `A` | Isolate Alpha Channel |
| `C` | View Full RGB (Color) |
| `1` | View Image A (when B is loaded) |
| `2` | View Image B (when B is loaded) |
| `Space` | Toggle Blink Mode (Strobes between A and B) |
| `Shift + Click` | Sample pixel and save to swatch palette |
| `Scroll Wheel` | Zoom in/out at cursor |
| `Click + Drag` | Pan Image |

---

## Installation & Building

Make sure you have [Rust and Cargo](https://rustup.rs/) installed on your system.

```bash
# Clone the repository
git clone https://github.com/byvfx/floki.git
cd floki

# Build and run the app in release mode (Highly recommended for EXR parsing speed)
cargo run --release
```

## Debugging & Logging

The app initializes [`env_logger`](https://docs.rs/env_logger), so runtime logging is
controlled by the `RUST_LOG` environment variable. Launch the app from a terminal so
log output (written to `stderr`) is visible.

```powershell
# PowerShell — watch the EXR Header Converter work through a batch
$env:RUST_LOG = "floki=debug"
cargo run --release
```

```bash
# bash / zsh
RUST_LOG=floki=debug cargo run --release
```

Useful levels (prefix the target with `floki=` to filter out noisy `wgpu`/`eframe` logs):

| `RUST_LOG` value | What you see |
|------------------|--------------|
| `floki=info` | Conversion start line, final summary (`N of X files converted`), and any errors |
| `floki=debug` | The above plus a line per converted file and any layer left unchanged by the rename guard |
| `info` | Everything at info level, including `wgpu`/`eframe` startup |
| `floki=info,wgpu=warn` | App info logs while silencing graphics-backend chatter |

> **Note:** During batch conversion, files are processed in parallel across CPU cores, so
> per-file log lines appear interleaved/out of order. The count in the final summary is
> authoritative.

## Architecture

- **`main.rs`**: Application entry point and `eframe` initialization.
- **`app.rs`**: Core application state, menu bars, persistence logic, and layout scaffolding.
- **`exr_loader.rs`**: Background threading and parsing of `OpenEXR` data structures using the `exr` crate.
- **`gpu/mod.rs`**: Hardware-accelerated drawing backend leveraging `wgpu` pipelines and WGSL shaders.
- **`viewer.rs`**: The heavy lifter. Handles canvas drawing, image scaling, pixel sampling, UI interaction, and falling back between GPU and CPU paths.
- **`tools.rs`**: The multi-threaded EXR Header Converter (batch channel renaming via `rayon`), with progress reporting and `RUST_LOG` logging.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
