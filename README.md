# EXR Analyzer

[![Rust](https://img.shields.io/badge/Rust-1.70+-orange.svg)](https://www.rust-lang.org)
[![egui](https://img.shields.io/badge/GUI-egui-blue)](https://github.com/emilk/egui)
[![wgpu](https://img.shields.io/badge/wgpu-Native-green.svg)](https://wgpu.rs)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

**EXR Analyzer** is a lightning-fast, hardware-accelerated Rust GUI application tailored for Technical Directors, Compositors, and LookDev Artists who need to deeply inspect and compare multi-layered OpenEXR files. 

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
* **Wipe:** Classic split-screen slider for side-by-side boundary checks.
* **Side-by-Side:** View Image A and Image B glued together in a continuous panorama. They share the same camera for synchronized panning and zooming.
* **Diff Matte:** Renders `|A - B| * multiplier` to easily spot fractional floating-point discrepancies in your render pipelines.
* **Blink Mode:** Hold `Spacebar` to rapidly strobe between Image A and Image B.

### Image Analysis
* **Dynamic Luminance Histogram:** Real-time histogram mapped to Exposure Values (EV stops). Effortlessly spot floating-point highlights over `1.0` using the Logarithmic view.
* **Dual Histogram Mode:** When comparing two images, the histogram overlays a translucent Red graph (Image B) on top of the White graph (Image A) so you can visually align black levels.
* **Channel Isolation:** Quickly isolate `R`, `G`, `B`, `A`, or view `RGB` composite with single-key shortcuts.

### High-Performance UI
* **Immediate-Mode UI:** Built on `egui` for a responsive, minimal-overhead interface.
* **Persistent State:** Remembers your UI layout, recent files list, and preferences across sessions.
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
git clone https://github.com/yourusername/exr-analyzer.git
cd exr-analyzer

# Build and run the app in release mode (Highly recommended for EXR parsing speed)
cargo run --release
```

## Architecture

- **`main.rs`**: Application entry point and `eframe` initialization.
- **`app.rs`**: Core application state, menu bars, persistence logic, and layout scaffolding.
- **`exr_loader.rs`**: Background threading and parsing of `OpenEXR` data structures using the `exr` crate.
- **`gpu/mod.rs`**: Hardware-accelerated drawing backend leveraging `wgpu` pipelines and WGSL shaders.
- **`viewer.rs`**: The heavy lifter. Handles canvas drawing, image scaling, pixel sampling, UI interaction, and falling back between GPU and CPU paths.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
