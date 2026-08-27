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

// The startup GPU preflight (#247). Re-exported rather than making `gpu` public:
// `main.rs` enumerates the adapters (that needs a live wgpu instance) and this
// crate decides what to say about them, so only what that split needs crosses the
// boundary — the summary type, the requirement to test each adapter against, and
// the verdict.
pub use gpu::{AdapterSummary, REQUIRED_DEVICE_FEATURES, gpu_preflight_error};

mod annotation;
mod background;
// Pure ring-cache budget math (#56): `t1_budget_bytes` sizes the T1 ring,
// `frames_for(vram_available(..))` the T2 pre-upload ring — split across the A
// and B rings in locked-step compare (#166; both live in `tick_budgets`).
mod budget;
// T1 ring cache for sequence playback (#56).
mod cache;
mod color;
mod gpu;
mod gradient;
// Comp layer model — the review-player spine (#103), consumed by the viewer via
// the render program (#114); N-way compare (#104) extends it.
mod layer;
// Sequence-playback transport state + pure frame-advance logic (#7).
mod playback;
// Pure pixel access over decoded EXR channels + thumbnail decimation (#153).
mod pixels;
// Read-ahead file warmer (#164) — overlaps the next frame's file read with the
// current decode via the OS page cache.
mod prefetch;
// Persistent on-disk proxy cache (#165) — amortizes the first-touch decode of a
// scrub proxy across passes/sessions (`~/.floki/proxy-cache`).
mod proxy_cache;
mod render_math;
// Resolves the A/B viewer state into a render program via the layer model (#114).
mod resource_monitor;
// Pure decode want-list scheduler (#57), driving the decode-ahead pump.
mod scheduler;
// Pure image-sequence detection (#7) — a crate-internal building block consumed
// by playback/app.
mod sequence;
mod snapshot;
// Off-thread comp-texture upload (#202) — the paint thread submits a frame and
// collects a finished GPU texture instead of interleaving and uploading inline.
mod tex_upload;
pub mod tools;
mod viewer;
