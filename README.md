# EXR Analyzer

A Rust-based GUI application for analyzing and viewing multi-layer OpenEXR files. 

## Features
- **EXR Viewing**: Display EXR images with support for zooming and panning.
- **Tone Mapping**: Toggle between Linear and sRGB viewing spaces.
- **Pixel Sampling**: Hover over pixels to see their exact underlying floating-point values from the EXR data.
- **Metadata Inspection**: View detailed metadata, channels, and layers within the EXR file.

## Tech Stack
- **Rust**
- **egui / eframe**: For the immediate mode GUI.
- **exr**: Pure Rust crate for reading OpenEXR files safely.

## Build and Run
Ensure you have Rust and Cargo installed.
```bash
cargo run --release
```

## License
MIT
