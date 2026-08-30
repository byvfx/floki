//! Read-ahead file warmer for sequence playback (#164).
//!
//! A single background thread that pulls upcoming frame files through the OS
//! page cache *before* the decode worker opens them, so the worker's read runs
//! at memory speed instead of stalling the (internally rayon-parallel) decode
//! behind storage — the read of frame N+1 overlaps the decode of frame N. This
//! is the pipelining lever of the perf roadmap; biggest on networked / slow
//! storage, where the read dominates the decode.
//!
//! Design constraints (see `docs/perf-roadmap.md`):
//! - **Epoch-agnostic**: warming a superseded frame wastes bandwidth, never
//!   correctness — zero coupling to the #98 pump/supersession logic.
//! - **Best-effort**: bounded queue with drop-on-full; a missing or slow file
//!   costs only warmer-thread time. Playback never waits on this thread.
//! - **No owned bytes**: files are read through one small reusable buffer and
//!   discarded — residency lives in the page cache (evictable by the OS under
//!   pressure), so the #56 RAM budget and the free-RAM-based T1 cap are
//!   untouched. The mmap decode path (`exr_loader::map_file`) then parses
//!   straight from those warm pages without a copy.

use crate::proxy_cache::ProxyCache;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

/// Queue depth: at most this many files awaiting warming. The pump only sends
/// a frame or two per decode, so anything deeper means the warmer is behind
/// the decoder — newer sends are then dropped (the decode itself will read the
/// file soon anyway; queueing more would only warm frames further out at the
/// cost of the nearer ones).
const QUEUE_DEPTH: usize = 4;

/// How many recently queued paths to remember for dedupe. Covers the pump
/// re-offering the same want across a few decode cycles without re-reading a
/// file that was just warmed.
const RECENT_DEPTH: usize = 16;

/// Chunk size for the warming read. Large enough to run the disk sequentially
/// at full bandwidth, small enough to keep the warmer's resident footprint
/// negligible (one buffer, reused across files).
const CHUNK: usize = 1 << 20;

/// One warm request: the file to pull through the page cache, plus what the
/// decode would ask the disk proxy cache (#165) for — the gate the warmer
/// applies before reading (#309).
///
/// The gate travels *with* the job rather than being answered before the send
/// because answering it costs two stats of the source (`fs::metadata` +
/// `fs::canonicalize`) and one of the cache file. On the networked storage this
/// warmer exists for, that is milliseconds of blocking I/O — paid on the UI
/// thread, per candidate, per decode.
struct WarmJob {
    path: PathBuf,
    /// Proxy long-side cap the decode would request, or `None` when it won't
    /// take the proxy path at all (nothing to gate on then — warm it).
    proxy_px: Option<usize>,
    /// Logical layer (AOV) the decode would request (#217).
    aov: usize,
}

pub struct Prefetcher {
    /// `None` until the first [`Self::warm`] spawns the thread, or again if the
    /// thread ever dies (warming then stays off — it is an optimization, not a
    /// dependency, so there is no respawn dance like the decode worker's).
    tx: Option<SyncSender<WarmJob>>,
    /// Ring of recently queued paths for dedupe.
    recent: VecDeque<PathBuf>,
}

impl Default for Prefetcher {
    fn default() -> Self {
        Self {
            tx: None,
            recent: VecDeque::with_capacity(RECENT_DEPTH),
        }
    }
}

impl Prefetcher {
    /// Queue `path` for background warming, to be skipped by the warmer if the
    /// disk proxy cache already holds it at `proxy_px` / `aov`. Returns whether
    /// it was queued — `false` for a recently queued duplicate, a full queue
    /// (warmer busy; dropped by design), or a dead warmer thread.
    ///
    /// `cache` is only read when the thread is spawned; the warmer then owns its
    /// own handle. Queuing a job the gate will reject is deliberate (#309): it
    /// costs one channel slot and no I/O on this thread, and it means a
    /// cache-hit path enters `recent` like any other, so the next pump stops
    /// re-offering it. Gating before the send instead left cache hits out of the
    /// dedupe ring entirely — the same frame was re-stat'd on every pump until
    /// it decoded.
    pub fn warm(
        &mut self,
        path: PathBuf,
        proxy_px: Option<usize>,
        aov: usize,
        cache: &Arc<ProxyCache>,
    ) -> bool {
        if self.recent.contains(&path) {
            return false;
        }
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<WarmJob>(QUEUE_DEPTH);
            let cache = Arc::clone(cache);
            // A failed spawn (resource exhaustion) just leaves warming off —
            // never a panic on the UI thread for an optional optimization.
            if std::thread::Builder::new()
                .name("floki-prefetch".into())
                .spawn(move || warm_loop(&rx, &cache))
                .is_ok()
            {
                self.tx = Some(tx);
            }
        }
        let Some(tx) = &self.tx else {
            return false;
        };
        match tx.try_send(WarmJob {
            path: path.clone(),
            proxy_px,
            aov,
        }) {
            Ok(()) => {
                if self.recent.len() == RECENT_DEPTH {
                    self.recent.pop_front();
                }
                self.recent.push_back(path);
                true
            }
            // Full: the warmer is saturated — drop, don't block the UI thread.
            Err(TrySendError::Full(_)) => false,
            // Thread died (should not happen — `warm_loop` has no panicking
            // paths): disable warming rather than respawn-loop.
            Err(TrySendError::Disconnected(_)) => {
                self.tx = None;
                false
            }
        }
    }
}

/// The warmer thread: read each queued file front to back through one reusable
/// buffer, populating the page cache, and discard the bytes. Errors (file
/// vanished, permission) are ignored — the decode path reports them properly
/// if the frame is ever actually decoded.
///
/// Skips a file the disk proxy cache (#165) already holds at the size and AOV
/// the decode will ask for: the worker will hit that cache and never open the
/// source, so pulling it through the page cache is wasted bandwidth — exactly
/// the networked-storage cost the cache exists to remove. `contains` self-gates
/// to `false` when the disk cache is off, and its stats are paid here rather
/// than on the UI thread (#309).
fn warm_loop(rx: &Receiver<WarmJob>, cache: &ProxyCache) {
    let mut buf = vec![0u8; CHUNK];
    for job in rx {
        if job
            .proxy_px
            .is_some_and(|px| cache.contains(&job.path, px, job.aov))
        {
            continue;
        }
        let Ok(mut file) = std::fs::File::open(&job.path) else {
            continue;
        };
        while matches!(file.read(&mut buf), Ok(n) if n > 0) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The inert cache every test warms against: zero I/O, no writer thread, and
    /// `contains` self-gates to `false`, so the warmer reads whatever it is given.
    fn no_cache() -> Arc<ProxyCache> {
        Arc::new(ProxyCache::disabled())
    }

    #[test]
    fn warm_dedupes_recent_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("frame.exr");
        std::fs::File::create(&p)
            .unwrap()
            .write_all(b"not really an exr; the warmer doesn't care")
            .unwrap();

        let mut pf = Prefetcher::default();
        assert!(
            pf.warm(p.clone(), None, 0, &no_cache()),
            "first offer queues"
        );
        assert!(
            !pf.warm(p.clone(), None, 0, &no_cache()),
            "immediate duplicate is skipped"
        );
    }

    #[test]
    fn a_proxy_gated_path_still_enters_the_dedupe_ring() {
        // #309: the disk-cache gate used to run *before* the send, so a
        // cache-hit path took an early `continue` and never reached the recent
        // ring — the pump then re-offered it, and re-paid its stats, on every
        // pump until the frame decoded. The gate now lives on the warmer
        // thread, so every offered path is remembered whether or not the warmer
        // ends up reading it.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("gated.exr");
        std::fs::write(&p, vec![0u8; 4096]).unwrap();

        let mut pf = Prefetcher::default();
        assert!(
            pf.warm(p.clone(), Some(1024), 2, &no_cache()),
            "a gate-carrying job queues like any other"
        );
        assert!(
            !pf.warm(p, Some(1024), 2, &no_cache()),
            "and is deduped on the next offer"
        );
    }

    #[test]
    fn warm_survives_missing_files() {
        let mut pf = Prefetcher::default();
        // A path that does not exist: queued fine, warmer skips it silently.
        assert!(pf.warm(
            PathBuf::from("/definitely/not/a/real/file.exr"),
            None,
            0,
            &no_cache()
        ));
        // The thread must still be alive for further sends afterwards.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("real.exr");
        std::fs::write(&p, vec![0u8; 4096]).unwrap();
        assert!(pf.warm(p, None, 0, &no_cache()));
    }

    #[test]
    fn recent_ring_is_bounded() {
        let mut pf = Prefetcher::default();
        for i in 0..(RECENT_DEPTH * 2) {
            // Missing paths are fine; only the dedupe ring matters here. Sends
            // may drop when the queue is momentarily full — the ring still
            // never grows past its cap.
            let _ = pf.warm(
                PathBuf::from(format!("/nope/{i}.exr")),
                None,
                0,
                &no_cache(),
            );
        }
        assert!(pf.recent.len() <= RECENT_DEPTH);
    }
}
