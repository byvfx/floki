//! Off-thread comp-texture upload (#202).
//!
//! # Why this exists
//!
//! `ensure_comp_frame` used to build each layer's GPU texture **synchronously on
//! the paint thread**: interleave the EXR's planar channels into an RGBA buffer,
//! then `queue.write_texture` the whole frame. Measured on 4.6K plate footage with
//! two layers, that consumed ~940 ms of every 1000 ms of wall clock — the UI thread
//! spent almost all of its time uploading and almost none painting, which capped
//! playback at ~13 of a 24 fps target no matter how fast frames decoded.
//!
//! That is also why the parallel-decode attempt on #204 backfired so badly: more
//! decode workers produced more frames for a paint thread that had no capacity to
//! display them, so displayed frames fell 240 → 3. Decode throughput is worthless
//! until the display path can keep up, which makes this the prerequisite.
//!
//! # Shape
//!
//! A small pool of worker threads, each owning a [`crate::gpu::TexBuildCtx`] (a
//! `Device`/`Queue`/`Arc<GpuState>` triple — all `Send`, since wgpu handles are
//! refcounted) and its own interleave scratch buffer. The paint thread submits
//! `(source, frame, aov, Arc<ExrData>)` and later collects a finished texture +
//! bind group, so its per-frame cost drops to a hash-map swap.
//!
//! # Backpressure — the part that matters
//!
//! **At most one build in flight per source**, enforced by [`Self::try_submit`].
//! This is deliberate and is what keeps the failure mode of the reverted decode
//! pool from reappearing here: the queue can never grow, because the next build for
//! a source is only submitted after the previous one has been *collected and
//! displayed*. Work is therefore generated at exactly the rate the screen consumes
//! it, and a slow GPU throttles the pipeline instead of flooding it.
//!
//! A late result is still applied rather than dropped. A texture that arrives after
//! the playhead moved on is newer than what is on screen, so showing it advances
//! the picture; the following paint immediately asks for the now-current frame.
//! Dropping it would leave the layer frozen for another whole round trip.

use crate::exr_loader::ExrData;
use crate::gpu::TexBuildCtx;
use crate::layer::SourceId;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

/// A texture build handed to a worker.
struct Job {
    source: SourceId,
    frame: u32,
    aov: usize,
    data: Arc<ExrData>,
}

/// A finished build, ready for the paint thread to bind.
pub struct Built {
    pub source: SourceId,
    pub frame: u32,
    pub aov: usize,
    pub size: (usize, usize),
    /// Whether the source frame was a **full** decode rather than a proxy or
    /// beauty-only one (#212). Read off the decoded data here rather than passed
    /// in, so it can never disagree with the pixels actually uploaded.
    pub full: bool,
    /// `None` if the build failed (AOV out of range, or the upload was rejected).
    /// Still delivered, so the in-flight slot is released rather than wedging the
    /// source forever.
    pub texture: Option<(
        eframe::egui_wgpu::wgpu::Texture,
        Arc<eframe::egui_wgpu::wgpu::BindGroup>,
    )>,
}

/// Worker-pool handle. Dropping it closes the job channel, which ends every
/// worker's `recv` loop and joins nothing — the threads exit on their own and hold
/// no resources the app needs back.
pub struct TexUploader {
    jobs: Option<Sender<Job>>,
    done: Receiver<Built>,
    /// `(frame, aov)` currently being built per source — the depth-1 gate.
    inflight: std::collections::HashMap<SourceId, (u32, usize)>,
}

/// Worker count. **One by default, because two measured worse.**
///
/// The intuition says a second worker should help — one is marginally short of
/// feeding two layers at 24 fps. It does not. Going 1 → 2 on 4.6K plate footage
/// left throughput flat (~28 builds/s either way) while making every build more
/// expensive, and the giveaway was `create_bind_group` going 0.03 ms → 10.3 ms:
/// that call moves no pixels, so a 340× slowdown is pure driver-lock contention.
/// wgpu serializes resource creation and queue writes internally, and the
/// interleave is already `rayon`-parallel across all cores, so a second worker
/// adds contention on both without adding a lane. It also competes with the paint
/// thread's own queue use — the shape of the reverted decode-pool regression
/// (#204), one layer down.
///
/// `FLOKI_TEX_WORKERS` overrides it, which is how the above was measured; keep it
/// for re-measuring on other GPUs before assuming this generalizes.
fn worker_count() -> usize {
    std::env::var("FLOKI_TEX_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 8)
}

impl TexUploader {
    /// Spawn the pool. `ctx` is cloned per worker.
    #[must_use]
    pub fn new(ctx: &TexBuildCtx) -> Self {
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Built>();
        let jobs_rx = Arc::new(std::sync::Mutex::new(jobs_rx));

        let n = worker_count();
        for i in 0..n {
            let ctx = ctx.clone();
            let jobs_rx = Arc::clone(&jobs_rx);
            let done_tx = done_tx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("floki-tex-upload-{i}"))
                .spawn(move || {
                    loop {
                        // Scoped so the queue lock is released before the build —
                        // otherwise the pool would serialize on it.
                        let job = {
                            let Ok(rx) = jobs_rx.lock() else { return };
                            match rx.recv() {
                                Ok(j) => j,
                                Err(_) => return, // sender dropped: app shutting down
                            }
                        };
                        let built = crate::viewer::ExrViewer::build_source_texture(
                            &ctx, &job.data, job.aov,
                        );
                        let size = job.data.logical_size(job.aov).unwrap_or((0, 0));
                        let full = !job.data.proxy && !job.data.beauty_only;
                        if done_tx
                            .send(Built {
                                source: job.source,
                                frame: job.frame,
                                aov: job.aov,
                                size,
                                full,
                                texture: built,
                            })
                            .is_err()
                        {
                            return; // receiver dropped
                        }
                    }
                });
            if let Err(e) = spawned {
                log::warn!(target: "floki::playback", "tex upload worker {i} failed to spawn: {e}");
            }
        }

        Self {
            jobs: Some(jobs_tx),
            done: done_rx,
            inflight: std::collections::HashMap::new(),
        }
    }

    /// Queue a build unless `source` already has one in flight (the depth-1 gate).
    /// Returns whether it was queued.
    pub fn try_submit(
        &mut self,
        source: SourceId,
        frame: u32,
        aov: usize,
        data: Arc<ExrData>,
    ) -> bool {
        if self.inflight.contains_key(&source) {
            return false;
        }
        let Some(tx) = self.jobs.as_ref() else {
            return false;
        };
        if tx
            .send(Job {
                source,
                frame,
                aov,
                data,
            })
            .is_err()
        {
            return false;
        }
        self.inflight.insert(source, (frame, aov));
        true
    }

    /// Collect every finished build, releasing each source's in-flight slot.
    /// Non-blocking.
    pub fn drain(&mut self) -> Vec<Built> {
        let mut out = Vec::new();
        while let Ok(b) = self.done.try_recv() {
            self.inflight.remove(&b.source);
            out.push(b);
        }
        out
    }

    /// What `source` is currently building, if anything. Lets the caller avoid
    /// re-requesting a frame that is already on its way.
    #[must_use]
    pub fn pending_for(&self, source: SourceId) -> Option<(u32, usize)> {
        self.inflight.get(&source).copied()
    }

    /// Number of builds in flight across all sources (instrumentation).
    #[must_use]
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// Forget `source`'s in-flight slot — for when the layer is removed. A result
    /// may still arrive afterwards; the caller drops it as unknown.
    pub fn forget(&mut self, source: SourceId) {
        self.inflight.remove(&source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exr::prelude::*;

    /// A minimal on-disk RGBA fixture, loaded back into an `ExrData` — the suite is
    /// GPU-free and keeps no committed binaries, so fixtures are generated per test
    /// (see TESTING.md).
    fn tiny_exr() -> (tempfile::TempDir, Arc<ExrData>) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.exr");
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F16(vec![f16::from_f32(0.5); 4 * 4]),
            ));
        }
        Image::from_layer(Layer::new(
            (4, 4),
            LayerAttributes::default(),
            Encoding::UNCOMPRESSED,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(&p)
        .expect("write exr fixture");
        let data = Arc::new(ExrData::load(&p).unwrap());
        (dir, data)
    }

    /// The depth-1 gate is the whole backpressure story, and it is pure bookkeeping
    /// — testable without a GPU, which the suite has no access to. A second submit
    /// for a source with one in flight must be refused, and accepted again only
    /// once that source's slot is released.
    #[test]
    fn one_build_in_flight_per_source() {
        // Channels only; no workers, so nothing ever completes on its own.
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (_done_tx, done_rx) = channel::<Built>();
        let mut up = TexUploader {
            jobs: Some(jobs_tx),
            done: done_rx,
            inflight: std::collections::HashMap::new(),
        };

        let (_dir, data) = tiny_exr();
        let a = SourceId(2);
        let b = SourceId(3);

        assert!(up.try_submit(a, 1, 0, Arc::clone(&data)), "first submit");
        assert!(
            !up.try_submit(a, 2, 0, Arc::clone(&data)),
            "same source must be gated while one is in flight"
        );
        assert!(
            up.try_submit(b, 1, 0, Arc::clone(&data)),
            "a different source is independent"
        );
        assert_eq!(up.inflight_len(), 2);
        assert_eq!(up.pending_for(a), Some((1, 0)));

        // Releasing the slot re-opens it — this is the step that makes the pipeline
        // self-clocking rather than unbounded.
        up.forget(a);
        assert!(up.try_submit(a, 2, 0, Arc::clone(&data)), "slot released");
        assert_eq!(up.pending_for(a), Some((2, 0)));

        // Three jobs reached the workers, not four: the gated submit is refused
        // outright rather than queued behind the one in flight. That is the property
        // that bounds the queue — a refused build is retried by the next paint,
        // against the frame that is wanted *then*, so no stale work accumulates.
        assert_eq!(jobs_rx.try_iter().count(), 3);
    }
}
