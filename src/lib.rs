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
// Pure ring-cache budget math (#56). `max_t1` sizes the T1 ring (Phase 3);
// `max_t2` waits for the T2 pre-upload in Phase 4, hence the module-wide allow.
#[allow(dead_code)]
mod budget;
// T1 ring cache for sequence playback (#56). The core (get/insert/evict) is used
// now; `resident` feeds the Phase 4 scheduler and `Slot::B` the Phase 5 A/B work.
#[allow(dead_code)]
mod cache;
mod color;
mod gpu;
mod gradient;
// Sequence-playback transport state + pure frame-advance logic (#7).
mod playback;
mod proxy;
mod render_math;
mod resource_monitor;
// Pure decode want-list scheduler (#57). Consumed by the decode-ahead worker in
// Phase 4; until then only its own tests exercise it.
#[allow(dead_code)]
mod scheduler;
// Pure image-sequence detection (#7), consumed by playback/app.
pub mod sequence;
mod snapshot;
pub mod tools;
mod viewer;
