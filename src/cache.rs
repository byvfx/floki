//! T1 ring cache for sequence playback (#56).
//!
//! Holds decoded frames as `Arc<ExrData>` keyed by `(SourceId, frame number)`, so
//! a frame can be the active image (T3) and stay resident (T1) at once without
//! cloning its pixel buffers, and a scrub-back or loop replay is an instant cache
//! hit instead of a re-decode. Bounded by a [`Bound`] — a byte ceiling and a
//! frame count, both derived from the one RAM budget
//! (`crate::budget::t1_budget_bytes`); eviction is **directional-ring + LRU**:
//! during linear play drop frames behind the playhead first (measured around the
//! loop when `LoopMode::Loop` wraps, #140) — except a small **read-behind
//! window** of just-shown frames (#169, matching the scheduler's reservation) —
//! while scrubbing drop by absolute distance — LRU breaks ties. See
//! `docs/playback/memory-contract.md`.
//!
//! Keyed on `(SourceId, frame)` so every source rides the same ring and eviction
//! (#99 unification): today's locked-step A/B (#98) is two sources — the primary
//! (directional playhead) plus one follower — but the key + the N-playhead
//! eviction below already generalize to the N-source comp stack. Callers pass a
//! [`SourceId`] directly (`ExrApp::{A,B}_SOURCE` for the compare slots).

use std::collections::HashMap;
use std::sync::Arc;

use crate::exr_loader::ExrData;
use crate::layer::SourceId;
use crate::playback::Direction;

/// What the ring is held under (#232): a byte ceiling and a frame count,
/// enforced together by [`FrameCache::evict_to`] — whichever binds first.
///
/// **Bytes are the authority.** The budget is expressed in bytes and the ring's
/// residency is measured in bytes (#230), so a count alone can only approximate
/// it, and does so badly on a mixed ring: playback's beauty-only frames and the
/// full frame at each settled playhead differ by ~18x on a heavy multi-part
/// render, so `frames × one frame's size` is wrong by whatever the mix happens to
/// be. `frames` is carried alongside rather than dropped because the decode
/// scheduler needs a count — it sizes the #207 prefetch window — and because it
/// keeps entry count bounded when `approx_bytes` (pixel buffers only, a
/// documented lower bound) understates a frame's true cost.
///
/// Both are derived from one byte budget via `budget::frames_in`, so they state
/// the same limit in two units rather than two limits that can drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    pub frames: usize,
    pub bytes: u64,
}

impl Bound {
    #[must_use]
    pub fn new(frames: usize, bytes: u64) -> Self {
        Self { frames, bytes }
    }

    /// A count-only bound — for tests exercising the *policy* (which frame is
    /// picked) rather than the budget (how many are kept).
    #[cfg(test)]
    #[must_use]
    pub fn frames(frames: usize) -> Self {
        Self {
            frames,
            bytes: u64::MAX,
        }
    }

    /// A byte-only bound, for the same reason in the other unit.
    #[cfg(test)]
    #[must_use]
    pub fn bytes(bytes: u64) -> Self {
        Self {
            frames: usize::MAX,
            bytes,
        }
    }
}

struct Entry {
    data: Arc<ExrData>,
    /// Monotonic access stamp for the LRU tiebreak.
    last_used: u64,
    /// This frame's `approx_bytes()`, **snapshotted at insert** rather than
    /// recomputed on demand, so the running sum in [`FrameCache::bytes`] can
    /// never drift from what was actually added.
    bytes: usize,
}

/// A byte-budgeted ring of decoded frames.
#[derive(Default)]
pub struct FrameCache {
    entries: HashMap<(SourceId, u32), Entry>,
    tick: u64,
    /// Live sum of every resident entry's `bytes` (#230).
    ///
    /// The T1 budget used to *synthesize* this figure at the call site as
    /// `len * frame_bytes` — the same scalar it then divided by — so one
    /// mis-measured scalar was wrong on both sides of the budget at once. The
    /// ring is genuinely heterogeneous: playback fills it with beauty-only or
    /// proxy frames and each settle upgrade replaces one with a full frame at
    /// the same key, so no single scalar describes the resident set. Measuring
    /// it costs one add per mutation.
    bytes: u64,
}

impl FrameCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Total resident bytes across every source (#230) — the measured figure the
    /// T1 budget adds back to free RAM so capacity doesn't chase the cache's own
    /// growth (`budget::t1_budget_bytes`).
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether `arc` is one of the resident entries, by **pointer identity** rather
    /// than value.
    ///
    /// A decode can be reachable from two places at once — a `CompSource` holds its
    /// open-time `Arc<ExrData>` and the same allocation may also sit in the ring — so
    /// anything summing RAM across both has to know which is which or it counts the
    /// bytes twice (#342). Linear over the entries, which number in the tens.
    #[must_use]
    pub fn holds(&self, arc: &Arc<ExrData>) -> bool {
        self.entries.values().any(|e| Arc::ptr_eq(&e.data, arc))
    }

    /// API completeness alongside [`FrameCache::len`] (and keeps
    /// `clippy::len_without_is_empty` satisfied); no runtime caller today.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn contains(&self, source: impl Into<SourceId>, frame: u32) -> bool {
        self.entries.contains_key(&(source.into(), frame))
    }

    /// Fetch a resident frame, bumping it as most-recently-used.
    pub fn get(&mut self, source: impl Into<SourceId>, frame: u32) -> Option<Arc<ExrData>> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(&(source.into(), frame))?;
        entry.last_used = tick;
        Some(entry.data.clone())
    }

    /// Fetch a resident frame **without** touching LRU order — for readers that
    /// want a cached frame's pixels without keeping it warm in T1.
    pub fn peek(&self, source: impl Into<SourceId>, frame: u32) -> Option<Arc<ExrData>> {
        self.entries
            .get(&(source.into(), frame))
            .map(|e| e.data.clone())
    }

    /// Insert (or replace) a frame, marking it most-recently-used. Does not evict;
    /// call [`FrameCache::evict_to`] afterward to enforce the budget.
    pub fn insert(&mut self, source: impl Into<SourceId>, frame: u32, data: Arc<ExrData>) {
        self.tick += 1;
        let bytes = data.approx_bytes();
        let replaced = self.entries.insert(
            (source.into(), frame),
            Entry {
                data,
                last_used: self.tick,
                bytes,
            },
        );
        // Replacement is the common case here, not the rare one: the settle
        // upgrade swaps a beauty/proxy frame for a full one at the *same* key.
        // Crediting the new bytes without debiting the old leaks the difference
        // once per upgrade, which on a played-through range is every frame.
        self.bytes = self
            .bytes
            .saturating_sub(replaced.map_or(0, |e| e.bytes as u64))
            .saturating_add(bytes as u64);
    }

    /// Resident frame numbers for a source, **without allocating** — for callers
    /// that only iterate (the timeline cache-fill bar, #56 step 4, on every
    /// repaint). Order is unspecified.
    pub fn resident_frames(&self, source: impl Into<SourceId>) -> impl Iterator<Item = u32> + '_ {
        let source = source.into();
        self.entries
            .keys()
            .filter(move |(s, _)| *s == source)
            .map(|(_, f)| *f)
    }

    /// Resident frame numbers held at **full** fidelity (not a scrub proxy or a
    /// beauty-only decode), non-allocating — for the two-tone timeline cache bar
    /// (#172), which shades full-res frames brighter than proxy/beauty frames.
    pub fn resident_full_frames(
        &self,
        source: impl Into<SourceId>,
    ) -> impl Iterator<Item = u32> + '_ {
        let source = source.into();
        self.entries
            .iter()
            .filter(move |((s, _), e)| *s == source && !e.data.proxy && !e.data.beauty_only)
            .map(|((_, f), _)| *f)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Drop every frame of one source, keeping the others. Used when locked-step B
    /// (#98) is dropped or replaced so a new B sequence (which reuses frame
    /// numbers) can't hit a stale entry.
    pub fn clear_slot(&mut self, source: impl Into<SourceId>) {
        let source = source.into();
        let mut freed = 0u64;
        self.entries.retain(|(s, _), e| {
            let keep = *s != source;
            if !keep {
                freed = freed.saturating_add(e.bytes as u64);
            }
            keep
        });
        self.bytes = self.bytes.saturating_sub(freed);
    }

    /// Drop a single frame, returning whether it was resident. Used by the
    /// render-watch (#101) when a frame is re-rendered or removed on disk, so the
    /// stale pixels don't survive in the ring.
    pub fn remove(&mut self, source: impl Into<SourceId>, frame: u32) -> bool {
        match self.entries.remove(&(source.into(), frame)) {
            Some(e) => {
                self.bytes = self.bytes.saturating_sub(e.bytes as u64);
                true
            }
            None => false,
        }
    }

    /// Evict frames until the ring fits `bound` — **both** its frame count and its
    /// byte ceiling, whichever binds first — protecting each source's on-screen
    /// frame in `playheads` (every `(source, playhead)` pair is kept). Victim
    /// selection is the directional-ring + LRU policy in
    /// [`FrameCache::pick_victim`], unchanged by which bound is doing the binding.
    ///
    /// The protected playheads outrank both bounds: when only they remain,
    /// `pick_victim` returns `None` and this stops, over budget. That is the
    /// memory contract's degradation, not a leak — a ring that evicted the frame
    /// on screen would decode it again immediately.
    ///
    /// `primary` is the source whose playhead drives the *directional* policy
    /// (loop-wrap distance, behind-bias, read-behind carve-out); every other
    /// source is a follower whose frames rank by plain distance from its own
    /// playhead (its numbers live in its own range, not the primary's trim).
    ///
    /// `loop_wrap` is the primary's in/out range when playing with
    /// `LoopMode::Loop`, so eviction distance follows the play direction *around*
    /// the loop; pass `None` for Once/PingPong and while paused.
    ///
    /// `read_behind` (#169) is the reserved trailing window: the last that-many
    /// frames behind the playhead rank by plain distance instead of behind-bias,
    /// so play-then-step-back hits cache. Must match the scheduler's reservation
    /// (`scheduler::read_behind` of the pump depth) — a larger value here would
    /// protect frames the pump then can't fit, and the two would churn.
    /// Returns the number of frames evicted (for instrumentation).
    #[allow(clippy::too_many_arguments)]
    pub fn evict_to(
        &mut self,
        bound: Bound,
        playheads: &[(SourceId, u32)],
        primary: SourceId,
        direction: Direction,
        playing: bool,
        loop_wrap: Option<(u32, u32)>,
        read_behind: usize,
    ) -> usize {
        let cap = bound.frames.max(1);
        let mut evicted = 0;
        while self.entries.len() > cap || self.bytes > bound.bytes {
            match self.pick_victim(
                playheads,
                primary,
                direction,
                playing,
                loop_wrap,
                read_behind,
            ) {
                Some(victim) => {
                    if let Some(e) = self.entries.remove(&victim) {
                        self.bytes = self.bytes.saturating_sub(e.bytes as u64);
                    }
                    evicted += 1;
                }
                // Only the protected active frame(s) remain — stop.
                None => break,
            }
        }
        evicted
    }

    /// Choose the next frame to evict, or `None` if only protected active frame(s)
    /// remain. Higher "evictability" wins; LRU (smaller `last_used`) breaks ties.
    ///
    /// - **Playing, Loop** (`loop_wrap = Some((in, out))`): distance is measured
    ///   in the play direction *around* the loop (modulo the span). A frame just
    ///   behind the playhead is nearly a full lap from being shown again, so it
    ///   is evicted first, while prefetch wrapped past the out point ranks "just
    ///   ahead" and survives. The plain behind test below used to classify
    ///   wrapped prefetch as furthest-behind and evict it the moment it landed —
    ///   decode churn near the out point and a guaranteed miss at the wrap (#140).
    /// - **Playing** (linear — Once/PingPong): frames *behind* the playhead in
    ///   the play direction are evicted first (furthest-behind first); frames
    ///   *ahead* are kept until no behind frames remain, then furthest-ahead
    ///   goes first.
    /// - **Scrubbing/paused**: evict by absolute distance from the playhead
    ///   (bidirectional), furthest first.
    /// - **Read-behind carve-out (#169)**, both playing modes: the
    ///   `read_behind` frames just shown rank by plain (backward) distance —
    ///   they compete with the near-ahead window instead of evicting first, so
    ///   stepping back after play is a cache hit. `0` restores the pure policy.
    ///
    /// The directional policy applies only to `primary`'s frames; follower
    /// sources (locked-step B, #98; the N-source comp stack) rank by plain
    /// distance from their own playhead.
    fn pick_victim(
        &self,
        playheads: &[(SourceId, u32)],
        primary: SourceId,
        direction: Direction,
        playing: bool,
        loop_wrap: Option<(u32, u32)>,
        read_behind: usize,
    ) -> Option<(SourceId, u32)> {
        // Rank "behind" frames above "ahead" frames during play by adding this
        // offset, so any behind frame outranks every ahead frame.
        const BEHIND_BIAS: u64 = 1 << 40;

        let playhead_of = |src: SourceId| -> Option<u32> {
            playheads.iter().find(|(s, _)| *s == src).map(|(_, p)| *p)
        };
        let primary_playhead = playhead_of(primary);

        let evictability = |source: SourceId, frame: u32| -> u64 {
            // Follower sources rank by plain distance from their own playhead: their
            // frames live in their own range, so the primary's loop-wrap / behind
            // logic below doesn't apply. The primary keeps the tuned directional policy.
            if source != primary {
                return playhead_of(source).map_or(0, |pb| u64::from(frame.abs_diff(pb)));
            }
            let Some(playhead) = primary_playhead else {
                return 0;
            };
            let dist = u64::from(frame.abs_diff(playhead));
            if !playing {
                return dist; // scrub: pure distance, either side.
            }
            if let Some((lo, hi)) = loop_wrap
                && (lo..=hi).contains(&playhead)
            {
                // A leftover from an old trim, outside the loop: never shown
                // again on this lap — evict before anything in range.
                if !(lo..=hi).contains(&frame) {
                    return BEHIND_BIAS + dist;
                }
                let len = i64::from(hi) - i64::from(lo) + 1;
                let delta = i64::from(frame) - i64::from(playhead);
                let fwd = match direction {
                    Direction::Forward => delta.rem_euclid(len),
                    Direction::Reverse => (-delta).rem_euclid(len),
                };
                // Read-behind carve-out (#169): a just-shown frame is nearly a
                // full lap away in ring distance; within the reserved window it
                // ranks by its backward distance instead, so it survives over
                // further-ahead prefetch. (fwd >= 1 for any frame != playhead.)
                let back = len - fwd;
                if back <= read_behind as i64 {
                    return back as u64;
                }
                return fwd as u64;
            }
            let behind = match direction {
                Direction::Forward => frame < playhead,
                Direction::Reverse => frame > playhead,
            };
            // Read-behind carve-out (#169): within the window, rank like an
            // ahead frame at the same distance instead of biasing.
            if behind && dist > read_behind as u64 {
                BEHIND_BIAS + dist
            } else {
                dist
            }
        };

        self.entries
            .iter()
            // Never evict a frame currently on screen — any source's playhead (#98).
            .filter(|((source, frame), _)| {
                !playheads.iter().any(|(s, p)| s == source && p == frame)
            })
            .map(|((source, frame), entry)| {
                (
                    *source,
                    *frame,
                    evictability(*source, *frame),
                    entry.last_used,
                )
            })
            // Max evictability; on a tie, the least-recently-used (smallest
            // last_used) frame is the victim.
            .max_by(|a, b| a.2.cmp(&b.2).then(b.3.cmp(&a.3)))
            .map(|(source, frame, _, _)| (source, frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two A/B decode slots as `SourceId`s, for the eviction call sites.
    const A: SourceId = SourceId(0);
    const B: SourceId = SourceId(1);

    /// A tiny valid `ExrData` so the cache holds real `Arc`s. The pixel content
    /// is irrelevant to cache behavior; we test keys/eviction, not pixels.
    fn frame() -> Arc<ExrData> {
        use exr::prelude::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.exr");
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F32(vec![0.0; 4]),
            ));
        }
        let layer = Layer::new(
            (2, 2),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer).write().to_file(&path).unwrap();
        Arc::new(ExrData::load(&path).unwrap())
    }

    fn fill(cache: &mut FrameCache, slot: SourceId, frames: &[u32]) {
        for &f in frames {
            cache.insert(slot, f, frame());
        }
    }

    /// A proxy (downsampled) frame — its `proxy` flag is set, so the two-tone
    /// cache bar (#172) counts it as *not* full-res.
    fn proxy_frame() -> Arc<ExrData> {
        Arc::new(frame().downsampled(1))
    }

    #[test]
    fn resident_full_frames_excludes_proxy() {
        let mut c = FrameCache::new();
        c.insert(SourceId(0), 1, frame()); // full
        c.insert(SourceId(0), 2, proxy_frame()); // proxy — excluded from "full"
        c.insert(SourceId(0), 3, frame()); // full
        let mut full: Vec<u32> = c.resident_full_frames(SourceId(0)).collect();
        full.sort_unstable();
        assert_eq!(full, vec![1, 3], "proxy frame excluded from full residency");
        // ...but it's still resident for the base cache-fill tone.
        let mut all: Vec<u32> = c.resident_frames(SourceId(0)).collect();
        all.sort_unstable();
        assert_eq!(all, vec![1, 2, 3]);
    }

    #[test]
    fn get_hits_resident_and_misses_absent() {
        let mut c = FrameCache::new();
        c.insert(SourceId(0), 5, frame());
        assert!(c.get(SourceId(0), 5).is_some());
        assert!(c.get(SourceId(0), 6).is_none());
        // Source is part of the key.
        assert!(c.get(SourceId(1), 5).is_none());
    }

    #[test]
    fn remove_drops_a_single_frame_and_reports_residency() {
        let mut c = FrameCache::new();
        fill(&mut c, SourceId(0), &[1, 2, 3]);
        assert!(c.remove(SourceId(0), 2), "resident frame removed");
        assert!(!c.contains(SourceId(0), 2));
        assert!(
            c.contains(SourceId(0), 1) && c.contains(SourceId(0), 3),
            "others kept"
        );
        assert!(!c.remove(SourceId(0), 2), "second remove reports absent");
        assert!(!c.remove(SourceId(1), 1), "source is part of the key");
    }

    #[test]
    fn resident_lists_only_the_requested_slot() {
        let mut c = FrameCache::new();
        fill(&mut c, SourceId(0), &[1, 2, 3]);
        fill(&mut c, SourceId(1), &[9]);
        let mut a: Vec<u32> = c.resident_frames(SourceId(0)).collect();
        a.sort_unstable();
        assert_eq!(a, vec![1, 2, 3]);
        assert_eq!(c.resident_frames(SourceId(1)).collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn evict_protects_the_playhead_frame() {
        let mut c = FrameCache::new();
        fill(&mut c, SourceId(0), &[10, 11, 12]);
        // Capacity 1 with playhead on 11: everything else goes, 11 stays.
        c.evict_to(
            Bound::frames(1),
            &[(A, 11)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert_eq!(c.len(), 1);
        assert!(c.contains(SourceId(0), 11), "active frame is never evicted");
    }

    #[test]
    fn evict_protects_both_playheads_in_locked_step() {
        // Locked-step A/B (#98): A and B live in different frame ranges and both
        // have an on-screen frame that must survive a hard squeeze.
        let mut c = FrameCache::new();
        fill(&mut c, SourceId(0), &[3, 4, 5, 6]);
        fill(&mut c, SourceId(1), &[103, 104, 105, 106]);
        c.evict_to(
            Bound::frames(2),
            &[(A, 5), (B, 105)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert_eq!(c.len(), 2, "everything but the two playheads is evicted");
        assert!(c.contains(SourceId(0), 5), "A playhead protected");
        assert!(c.contains(SourceId(1), 105), "B playhead protected");
    }

    #[test]
    fn playing_forward_evicts_behind_before_ahead() {
        let mut c = FrameCache::new();
        // Playhead 5; 3,4 are behind, 6,7 ahead.
        fill(&mut c, SourceId(0), &[3, 4, 5, 6, 7]);
        // Trim to 3: the two behind frames (furthest first: 3 then 4) are dropped.
        // The returned count (used by the #100 debug overlay) reflects that.
        assert_eq!(
            c.evict_to(
                Bound::frames(3),
                &[(A, 5)],
                A,
                Direction::Forward,
                true,
                None,
                0
            ),
            2,
            "evicted count"
        );
        assert!(c.contains(SourceId(0), 5));
        assert!(
            c.contains(SourceId(0), 6) && c.contains(SourceId(0), 7),
            "ahead kept"
        );
        assert!(
            !c.contains(SourceId(0), 3) && !c.contains(SourceId(0), 4),
            "behind evicted"
        );
    }

    #[test]
    fn playing_drops_furthest_ahead_when_nothing_is_behind() {
        let mut c = FrameCache::new();
        // All ahead of playhead 5.
        fill(&mut c, SourceId(0), &[5, 6, 7, 8]);
        c.evict_to(
            Bound::frames(2),
            &[(A, 5)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        // Keep playhead + nearest ahead; drop the furthest-ahead (8 then 7).
        assert!(c.contains(SourceId(0), 5) && c.contains(SourceId(0), 6));
        assert!(!c.contains(SourceId(0), 7) && !c.contains(SourceId(0), 8));
    }

    #[test]
    fn reverse_play_treats_higher_frames_as_behind() {
        let mut c = FrameCache::new();
        // Playhead 5, playing in reverse: 6,7 are "behind", 3,4 ahead.
        fill(&mut c, SourceId(0), &[3, 4, 5, 6, 7]);
        c.evict_to(
            Bound::frames(3),
            &[(A, 5)],
            A,
            Direction::Reverse,
            true,
            None,
            0,
        );
        assert!(c.contains(SourceId(0), 5));
        assert!(
            c.contains(SourceId(0), 3) && c.contains(SourceId(0), 4),
            "ahead (lower) kept"
        );
        assert!(
            !c.contains(SourceId(0), 6) && !c.contains(SourceId(0), 7),
            "behind (higher) evicted"
        );
    }

    #[test]
    fn scrubbing_evicts_by_absolute_distance_either_side() {
        let mut c = FrameCache::new();
        // Playhead 5; not playing -> bidirectional distance.
        fill(&mut c, SourceId(0), &[1, 4, 5, 6, 9]);
        c.evict_to(
            Bound::frames(3),
            &[(A, 5)],
            A,
            Direction::Forward,
            false,
            None,
            0,
        );
        // Furthest from 5 are 1 (dist 4) and 9 (dist 4); both dropped.
        assert!(c.contains(SourceId(0), 5));
        assert!(
            c.contains(SourceId(0), 4) && c.contains(SourceId(0), 6),
            "nearest kept"
        );
        assert!(
            !c.contains(SourceId(0), 1) && !c.contains(SourceId(0), 9),
            "furthest evicted"
        );
    }

    #[test]
    fn lru_breaks_ties_at_equal_distance() {
        let mut c = FrameCache::new();
        // Equidistant from playhead 5: frames 4 and 6 (dist 1).
        c.insert(SourceId(0), 5, frame());
        c.insert(SourceId(0), 4, frame()); // inserted earlier
        c.insert(SourceId(0), 6, frame()); // inserted later
        c.get(SourceId(0), 6); // touch 6 so 4 is least-recently-used
        c.evict_to(
            Bound::frames(2),
            &[(A, 5)],
            A,
            Direction::Forward,
            false,
            None,
            0,
        );
        assert!(c.contains(SourceId(0), 6), "recently used survives the tie");
        assert!(!c.contains(SourceId(0), 4), "LRU loses the tie");
    }

    /// #140 regression: with the playhead near the out point in Loop mode, the
    /// scheduler wraps the prefetch window past the boundary. Those wrapped
    /// frames used to be classified "behind" (plain `frame < playhead`) and
    /// evicted the moment they landed — decode churn near the out point and a
    /// guaranteed miss at every wrap. In ring distance they are "just ahead"
    /// and must survive; the frames just behind the playhead go first.
    #[test]
    fn loop_play_keeps_wrapped_prefetch_and_evicts_just_behind() {
        let mut c = FrameCache::new();
        // Loop [1,10], playhead 9 forward. Wrapped prefetch 1,2 has landed;
        // 7,8 were just shown. Ring distances from 9: 10→1, 1→2, 2→3, 7→8, 8→9.
        fill(&mut c, SourceId(0), &[7, 8, 9, 10, 1, 2]);
        c.evict_to(
            Bound::frames(4),
            &[(A, 9)],
            A,
            Direction::Forward,
            true,
            Some((1, 10)),
            0,
        );
        assert!(c.contains(SourceId(0), 9), "playhead protected");
        assert!(
            c.contains(SourceId(0), 10) && c.contains(SourceId(0), 1) && c.contains(SourceId(0), 2),
            "wrapped prefetch is 'just ahead' around the loop — kept"
        );
        assert!(
            !c.contains(SourceId(0), 7) && !c.contains(SourceId(0), 8),
            "just-behind frames are a full lap away — evicted first"
        );
    }

    /// Reverse Loop play mirrors the wrap: prefetch below the in point wraps to
    /// the out point and must survive over frames just above the playhead.
    #[test]
    fn reverse_loop_play_keeps_prefetch_wrapped_to_the_out_point() {
        let mut c = FrameCache::new();
        // Loop [1,10], playhead 2 reverse. Prefetch 1,10,9 (wrapping); 3,4 just shown.
        fill(&mut c, SourceId(0), &[4, 3, 2, 1, 10, 9]);
        c.evict_to(
            Bound::frames(4),
            &[(A, 2)],
            A,
            Direction::Reverse,
            true,
            Some((1, 10)),
            0,
        );
        assert!(c.contains(SourceId(0), 2), "playhead protected");
        assert!(
            c.contains(SourceId(0), 1) && c.contains(SourceId(0), 10) && c.contains(SourceId(0), 9),
            "prefetch wrapped past the in point kept"
        );
        assert!(
            !c.contains(SourceId(0), 3) && !c.contains(SourceId(0), 4),
            "just-behind (higher) frames evicted first"
        );
    }

    /// Frames left outside the loop range by a trim are never shown again on
    /// this lap: they outrank everything in range for eviction.
    #[test]
    fn loop_play_evicts_out_of_range_leftovers_first() {
        let mut c = FrameCache::new();
        // Loop trimmed to [1,10]; frame 12 is a leftover from the old range.
        fill(&mut c, SourceId(0), &[12, 8, 9, 10, 1]);
        c.evict_to(
            Bound::frames(4),
            &[(A, 9)],
            A,
            Direction::Forward,
            true,
            Some((1, 10)),
            0,
        );
        assert!(
            !c.contains(SourceId(0), 12),
            "out-of-range leftover evicted before any in-range frame"
        );
        assert!(
            c.contains(SourceId(0), 8) && c.contains(SourceId(0), 10) && c.contains(SourceId(0), 1),
            "in-range frames kept"
        );
    }

    /// #169: during linear play the reserved read-behind window ranks by plain
    /// distance instead of the behind bias, so play-then-step-back hits cache;
    /// beyond-window behind frames still evict first, then furthest-ahead.
    #[test]
    fn read_behind_window_survives_linear_play() {
        let mut c = FrameCache::new();
        // Playhead 10 forward, window 2: 9,8 reserved; 7 beyond the window.
        fill(&mut c, SourceId(0), &[7, 8, 9, 10, 11, 12, 13]);
        c.evict_to(
            Bound::frames(5),
            &[(A, 10)],
            A,
            Direction::Forward,
            true,
            None,
            2,
        );
        assert!(
            !c.contains(SourceId(0), 7),
            "beyond the window: biased, evicted first"
        );
        assert!(
            !c.contains(SourceId(0), 13),
            "furthest-ahead evicts before the reserved window"
        );
        assert!(
            c.contains(SourceId(0), 8) && c.contains(SourceId(0), 9),
            "just-shown frames survive (#169)"
        );
        assert!(
            c.contains(SourceId(0), 11) && c.contains(SourceId(0), 12),
            "near-ahead prefetch kept"
        );
    }

    /// #169 in Loop mode: a just-shown frame is nearly a full lap away in ring
    /// distance (#140); within the reserved window it ranks by its backward
    /// distance instead, while frames beyond the window keep the ring rank.
    #[test]
    fn read_behind_window_survives_loop_play() {
        let mut c = FrameCache::new();
        // Loop [1,10], playhead 9 forward, window 1: 8 reserved; 7 beyond it.
        // Ring ranks: 10→1, 1→2, 8→back 1 (carve-out), 7→fwd 8.
        fill(&mut c, SourceId(0), &[7, 8, 9, 10, 1]);
        c.evict_to(
            Bound::frames(3),
            &[(A, 9)],
            A,
            Direction::Forward,
            true,
            Some((1, 10)),
            1,
        );
        assert!(c.contains(SourceId(0), 9), "playhead protected");
        assert!(
            c.contains(SourceId(0), 8),
            "just-shown frame survives (#169)"
        );
        assert!(c.contains(SourceId(0), 10), "nearest ahead kept");
        assert!(
            !c.contains(SourceId(0), 7),
            "beyond the window: nearly a full lap away, evicted first"
        );
        assert!(
            !c.contains(SourceId(0), 1),
            "wrapped prefetch further out evicts before the reserved frame"
        );
    }

    /// #99 unification: a third source evicts independently — its frames are
    /// protected at its own playhead and rank by plain distance from it, never
    /// perturbed by the primary's directional policy.
    #[test]
    fn evicts_a_third_source_by_its_own_playhead() {
        let mut c = FrameCache::new();
        let s2 = SourceId(2);
        c.insert(SourceId(0), 5, frame());
        for f in [8, 9, 10, 11, 12] {
            c.insert(s2, f, frame());
        }
        // Primary A on 5 (protected); source 2's playhead is 10. Squeeze to 3:
        // keep A/5 + s2/10 + the nearest s2 frame; drop s2's furthest (12 or 8).
        c.evict_to(
            Bound::frames(3),
            &[(A, 5), (s2, 10)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert_eq!(c.len(), 3);
        assert!(c.contains(SourceId(0), 5), "primary playhead protected");
        assert!(c.contains(s2, 10), "third source's playhead protected");
        // Its furthest-by-distance frame goes first (12 and 8 are dist 2; LRU tie →
        // 8 inserted earliest loses). Either way one of the far ones is gone.
        assert!(
            !c.contains(s2, 8) || !c.contains(s2, 12),
            "a far frame of the third source is evicted by its own distance"
        );
    }

    // --- #230: measured resident bytes -------------------------------------
    //
    // The T1 budget used to synthesize the ring's resident bytes at the call
    // site as `len * frame_bytes`, so it reported a beauty-only or proxy frame
    // as costing a full one. These pin the real sum across every mutation.

    /// A **bigger** full frame (4x4 rather than 2x2), so a replacement at the
    /// same key changes the resident total — this is the settle upgrade's shape.
    fn big_frame() -> Arc<ExrData> {
        use exr::prelude::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.exr");
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F32(vec![0.0; 16]),
            ));
        }
        let layer = Layer::new(
            (4, 4),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer).write().to_file(&path).unwrap();
        Arc::new(ExrData::load(&path).unwrap())
    }

    #[test]
    fn bytes_sums_what_is_actually_resident() {
        let mut c = FrameCache::new();
        assert_eq!(c.bytes(), 0, "empty ring holds nothing");

        let small = frame().approx_bytes() as u64;
        let big = big_frame().approx_bytes() as u64;
        assert!(
            big > small,
            "fixtures must differ or the test proves nothing"
        );

        c.insert(A, 1, frame());
        c.insert(A, 2, big_frame());
        assert_eq!(
            c.bytes(),
            small + big,
            "a mixed ring sums its members, not len * one scalar"
        );
        // The figure `len * scalar` would have produced for the same ring, off by
        // the whole spread between the two fidelities.
        assert_ne!(c.bytes(), c.len() as u64 * small);
    }

    #[test]
    fn replacing_a_frame_debits_the_one_it_replaced() {
        // The settle upgrade (#94/#56) writes a full frame over a beauty/proxy
        // frame at the *same* key. Crediting the new bytes without debiting the
        // old leaks the difference once per upgraded frame — i.e. every frame of
        // a played-through range.
        let mut c = FrameCache::new();
        c.insert(A, 1, proxy_frame());
        let proxy = c.bytes();
        c.insert(A, 1, big_frame());
        assert_eq!(c.len(), 1, "still one frame");
        assert_eq!(
            c.bytes(),
            big_frame().approx_bytes() as u64,
            "the replaced frame's bytes are debited, not stacked"
        );
        assert!(c.bytes() > proxy);
    }

    #[test]
    fn every_removal_path_returns_the_bytes() {
        let mut c = FrameCache::new();

        // remove()
        c.insert(A, 1, frame());
        assert!(c.remove(A, 1));
        assert_eq!(c.bytes(), 0, "remove");
        assert!(!c.remove(A, 1), "a miss removes nothing...");
        assert_eq!(c.bytes(), 0, "...and debits nothing");

        // evict_to()
        fill(&mut c, A, &[1, 2, 3, 4]);
        let each = frame().approx_bytes() as u64;
        assert_eq!(c.bytes(), 4 * each);
        c.evict_to(
            Bound::frames(2),
            &[(A, 1)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c.bytes(), 2 * each, "eviction");

        // clear_slot() — only the named source's bytes go
        c.clear();
        c.insert(A, 1, frame());
        c.insert(B, 1, big_frame());
        c.clear_slot(B);
        assert_eq!(c.bytes(), each, "clear_slot debits only that source");

        // clear()
        c.clear();
        assert_eq!(c.bytes(), 0, "clear");
    }

    // --- #232: the byte bound ------------------------------------------------

    #[test]
    fn the_byte_bound_evicts_when_the_count_alone_would_not() {
        // The defect: eviction was count-only against a budget expressed in bytes.
        // A ring comfortably under its frame count can still be over the budget it
        // is meant to enforce, and nothing brought it back down.
        let each = frame().approx_bytes() as u64;
        let mut c = FrameCache::new();
        fill(&mut c, A, &[1, 2, 3, 4, 5]);
        assert_eq!(c.bytes(), 5 * each);

        // Frames say "room for 100"; bytes say "room for 3".
        let bound = Bound::new(100, 3 * each);
        let evicted = c.evict_to(bound, &[(A, 1)], A, Direction::Forward, true, None, 0);
        assert_eq!(evicted, 2);
        assert_eq!(c.len(), 3, "the byte bound bound it, not the count");
        assert!(c.bytes() <= bound.bytes);
        assert!(c.contains(A, 1), "the playhead is still protected");
    }

    #[test]
    fn one_dear_frame_displaces_several_cheap_ones() {
        // Why a count can't stand in for bytes: the settle upgrade replaces a
        // cheap frame with a full one at the same key, so the ring's cost changes
        // while its *length* does not. Only a byte bound notices.
        let small = frame().approx_bytes() as u64;
        let big = big_frame().approx_bytes() as u64;
        assert!(big > small);

        let mut c = FrameCache::new();
        fill(&mut c, A, &[1, 2, 3, 4]);
        let budget = c.bytes(); // exactly fits four cheap frames
        c.insert(A, 1, big_frame()); // frame 1 settles to full fidelity
        assert_eq!(c.len(), 4, "length unchanged — a count sees nothing");
        assert!(c.bytes() > budget, "but the ring is now over budget");

        c.evict_to(
            Bound::new(100, budget),
            &[(A, 1)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert!(c.bytes() <= budget);
        assert!(c.contains(A, 1), "the upgraded playhead frame stays");
        // It cost (big - small) more, so the cheap frames it displaced are however
        // many that difference buys.
        assert_eq!(c.len(), 4 - ((big - small).div_ceil(small)) as usize);
    }

    #[test]
    fn the_protected_playhead_outranks_the_byte_bound() {
        // Degradation, not a leak: a ring holding only the frames on screen has
        // nothing left it may evict, so it stops over budget rather than dropping
        // the frame being displayed (which it would decode again immediately).
        let mut c = FrameCache::new();
        c.insert(A, 5, frame());
        c.insert(B, 105, frame());
        let evicted = c.evict_to(
            Bound::bytes(1), // a budget nothing could satisfy
            &[(A, 5), (B, 105)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert_eq!(evicted, 0);
        assert_eq!(c.len(), 2, "both playheads survive an impossible budget");
        assert!(c.bytes() > 1, "and the ring reports itself honestly over");
    }

    #[test]
    fn the_count_still_binds_when_frames_are_cheaper_than_budgeted() {
        // Both bounds are live; the more conservative one wins. Frames cheaper
        // than the sizing figure leave bytes slack, so the count binds — which is
        // the pre-#232 behaviour, preserved.
        let mut c = FrameCache::new();
        fill(&mut c, A, &[1, 2, 3, 4, 5]);
        c.evict_to(
            Bound::new(2, u64::MAX),
            &[(A, 1)],
            A,
            Direction::Forward,
            true,
            None,
            0,
        );
        assert_eq!(c.len(), 2, "the count bound it, not the bytes");
    }
}
