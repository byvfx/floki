//! floki library crate.
//!
//! The app is shipped as a binary (`src/main.rs`), but the modules live here so
//! integration tests and benchmarks (`benches/`, which compile as separate
//! crates) can reach the public surface — notably [`exr_loader::ExrData::load`],
//! the EXR decode + logical-layer grouping hot path exercised by the benches.
//!
//! Only modules that need to be reached from outside the crate are `pub`
//! (`app` for the binary, `exr_loader` for the benches); the rest stay
//! crate-internal and are referenced via `crate::` as before.

pub mod app;
pub mod exr_loader;

mod annotation;
mod background;
mod color;
mod gpu;
mod gradient;
// Sequence-playback transport state + pure frame-advance logic (#7).
mod playback;
mod proxy;
mod render_math;
mod resource_monitor;
// Pure image-sequence detection (#7), consumed by playback/app.
pub mod sequence;
mod snapshot;
pub mod tools;
mod viewer;
