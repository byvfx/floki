//! T1 ring cache for sequence playback (#56).
//!
//! Holds decoded frames as `Arc<ExrData>` keyed by `(SourceId, frame number)`, so
//! a frame can be the active image (T3) and stay resident (T1) at once without
//! cloning its pixel buffers, and a scrub-back or loop replay is an instant cache
//! hit instead of a re-decode. Bounded by a frame-count capacity derived from the
//! RAM budget (`crate::budget::max_t1`); eviction is **directional-ring + LRU**:
//! during linear play drop frames behind the playhead first (measured around the
//! loop when `LoopMode::Loop` wraps, #140) — except a small **read-behind
//! window** of just-shown frames (#169, matching the scheduler's reservation) —
//! while scrubbing drop by absolute distance — LRU breaks ties. See
//! `docs/playback/memory-contract.md`.
//!
//! Keyed on `(SourceId, frame)` so every source rides the same ring and eviction
//! (#99 unification): today's locked-step A/B (#98) is two sources — the primary
//! (directional playhead) plus one follower — but the key + the N-playhead
//! eviction below already generalize to the N-source comp stack. The A/B decode
//! slots reach this via [`Slot`] → [`SourceId`] (a bridge that dissolves once the
//! app's per-slot state folds into a per-source map).

use std::collections::HashMap;
use std::sync::Arc;

use crate::exr_loader::ExrData;
use crate::layer::SourceId;
use crate::playback::Direction;

/// Which image slot a cached frame belongs to — the A/B decode path's two-source
/// identity (#98). A transitional bridge (#99 unification): it maps to a fixed
/// [`SourceId`] so the cache is already source-native, and is removed once the
/// app's `_b` state generalizes to a per-`SourceId` map.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Slot {
    A,
    B,
}

impl From<Slot> for SourceId {
    fn from(slot: Slot) -> Self {
        SourceId(match slot {
            Slot::A => 0,
            Slot::B => 1,
        })
    }
}

struct Entry {
    data: Arc<ExrData>,
    /// Monotonic access stamp for the LRU tiebreak.
    last_used: u64,
}

/// A byte-budgeted ring of decoded frames.
#[derive(Default)]
pub struct FrameCache {
    entries: HashMap<(SourceId, u32), Entry>,
    tick: u64,
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

    /// Fetch a resident frame **without** touching LRU order — used by the T2
    /// pre-upload to read a cached frame's pixels without keeping it warm in T1.
    pub fn peek(&self, source: impl Into<SourceId>, frame: u32) -> Option<Arc<ExrData>> {
        self.entries.get(&(source.into(), frame)).map(|e| e.data.clone())
    }

    /// Insert (or replace) a frame, marking it most-recently-used. Does not evict;
    /// call [`FrameCache::evict_to`] afterward to enforce the budget.
    pub fn insert(&mut self, source: impl Into<SourceId>, frame: u32, data: Arc<ExrData>) {
        self.tick += 1;
        self.entries.insert(
            (source.into(), frame),
            Entry {
                data,
                last_used: self.tick,
            },
        );
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
    }

    /// Drop every frame of one source, keeping the others. Used when locked-step B
    /// (#98) is dropped or replaced so a new B sequence (which reuses frame
    /// numbers) can't hit a stale entry.
    pub fn clear_slot(&mut self, source: impl Into<SourceId>) {
        let source = source.into();
        self.entries.retain(|(s, _), _| *s != source);
    }

    /// Drop a single frame, returning whether it was resident. Used by the
    /// render-watch (#101) when a frame is re-rendered or removed on disk, so the
    /// stale pixels don't survive in the ring.
    pub fn remove(&mut self, source: impl Into<SourceId>, frame: u32) -> bool {
        self.entries.remove(&(source.into(), frame)).is_some()
    }

    /// Evict frames until at most `capacity` remain (capacity is floored at 1),
    /// protecting each source's on-screen frame in `playheads` (every
    /// `(source, playhead)` pair is kept). Victim selection is the directional-ring
    /// + LRU policy in [`FrameCache::pick_victim`].
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
        capacity: usize,
        playheads: &[(SourceId, u32)],
        primary: SourceId,
        direction: Direction,
        playing: bool,
        loop_wrap: Option<(u32, u32)>,
        read_behind: usize,
    ) -> usize {
        let cap = capacity.max(1);
        let mut evicted = 0;
        while self.entries.len() > cap {
            match self.pick_victim(playheads, primary, direction, playing, loop_wrap, read_behind) {
                Some(victim) => {
                    self.entries.remove(&victim);
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

        let playhead_of =
            |src: SourceId| -> Option<u32> { playheads.iter().find(|(s, _)| *s == src).map(|(_, p)| *p) };
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
                (*source, *frame, evictability(*source, *frame), entry.last_used)
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

    fn fill(cache: &mut FrameCache, slot: Slot, frames: &[u32]) {
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
        c.insert(Slot::A, 1, frame()); // full
        c.insert(Slot::A, 2, proxy_frame()); // proxy — excluded from "full"
        c.insert(Slot::A, 3, frame()); // full
        let mut full: Vec<u32> = c.resident_full_frames(Slot::A).collect();
        full.sort_unstable();
        assert_eq!(full, vec![1, 3], "proxy frame excluded from full residency");
        // ...but it's still resident for the base cache-fill tone.
        let mut all: Vec<u32> = c.resident_frames(Slot::A).collect();
        all.sort_unstable();
        assert_eq!(all, vec![1, 2, 3]);
    }

    #[test]
    fn get_hits_resident_and_misses_absent() {
        let mut c = FrameCache::new();
        c.insert(Slot::A, 5, frame());
        assert!(c.get(Slot::A, 5).is_some());
        assert!(c.get(Slot::A, 6).is_none());
        // Source is part of the key.
        assert!(c.get(Slot::B, 5).is_none());
    }

    #[test]
    fn remove_drops_a_single_frame_and_reports_residency() {
        let mut c = FrameCache::new();
        fill(&mut c, Slot::A, &[1, 2, 3]);
        assert!(c.remove(Slot::A, 2), "resident frame removed");
        assert!(!c.contains(Slot::A, 2));
        assert!(
            c.contains(Slot::A, 1) && c.contains(Slot::A, 3),
            "others kept"
        );
        assert!(!c.remove(Slot::A, 2), "second remove reports absent");
        assert!(!c.remove(Slot::B, 1), "source is part of the key");
    }

    #[test]
    fn resident_lists_only_the_requested_slot() {
        let mut c = FrameCache::new();
        fill(&mut c, Slot::A, &[1, 2, 3]);
        fill(&mut c, Slot::B, &[9]);
        let mut a: Vec<u32> = c.resident_frames(Slot::A).collect();
        a.sort_unstable();
        assert_eq!(a, vec![1, 2, 3]);
        assert_eq!(c.resident_frames(Slot::B).collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn evict_protects_the_playhead_frame() {
        let mut c = FrameCache::new();
        fill(&mut c, Slot::A, &[10, 11, 12]);
        // Capacity 1 with playhead on 11: everything else goes, 11 stays.
        c.evict_to(1, &[(A, 11)], A, Direction::Forward, true, None, 0);
        assert_eq!(c.len(), 1);
        assert!(c.contains(Slot::A, 11), "active frame is never evicted");
    }

    #[test]
    fn evict_protects_both_playheads_in_locked_step() {
        // Locked-step A/B (#98): A and B live in different frame ranges and both
        // have an on-screen frame that must survive a hard squeeze.
        let mut c = FrameCache::new();
        fill(&mut c, Slot::A, &[3, 4, 5, 6]);
        fill(&mut c, Slot::B, &[103, 104, 105, 106]);
        c.evict_to(2, &[(A, 5), (B, 105)], A, Direction::Forward, true, None, 0);
        assert_eq!(c.len(), 2, "everything but the two playheads is evicted");
        assert!(c.contains(Slot::A, 5), "A playhead protected");
        assert!(c.contains(Slot::B, 105), "B playhead protected");
    }

    #[test]
    fn playing_forward_evicts_behind_before_ahead() {
        let mut c = FrameCache::new();
        // Playhead 5; 3,4 are behind, 6,7 ahead.
        fill(&mut c, Slot::A, &[3, 4, 5, 6, 7]);
        // Trim to 3: the two behind frames (furthest first: 3 then 4) are dropped.
        // The returned count (used by the #100 debug overlay) reflects that.
        assert_eq!(
            c.evict_to(3, &[(A, 5)], A, Direction::Forward, true, None, 0),
            2,
            "evicted count"
        );
        assert!(c.contains(Slot::A, 5));
        assert!(
            c.contains(Slot::A, 6) && c.contains(Slot::A, 7),
            "ahead kept"
        );
        assert!(
            !c.contains(Slot::A, 3) && !c.contains(Slot::A, 4),
            "behind evicted"
        );
    }

    #[test]
    fn playing_drops_furthest_ahead_when_nothing_is_behind() {
        let mut c = FrameCache::new();
        // All ahead of playhead 5.
        fill(&mut c, Slot::A, &[5, 6, 7, 8]);
        c.evict_to(2, &[(A, 5)], A, Direction::Forward, true, None, 0);
        // Keep playhead + nearest ahead; drop the furthest-ahead (8 then 7).
        assert!(c.contains(Slot::A, 5) && c.contains(Slot::A, 6));
        assert!(!c.contains(Slot::A, 7) && !c.contains(Slot::A, 8));
    }

    #[test]
    fn reverse_play_treats_higher_frames_as_behind() {
        let mut c = FrameCache::new();
        // Playhead 5, playing in reverse: 6,7 are "behind", 3,4 ahead.
        fill(&mut c, Slot::A, &[3, 4, 5, 6, 7]);
        c.evict_to(3, &[(A, 5)], A, Direction::Reverse, true, None, 0);
        assert!(c.contains(Slot::A, 5));
        assert!(
            c.contains(Slot::A, 3) && c.contains(Slot::A, 4),
            "ahead (lower) kept"
        );
        assert!(
            !c.contains(Slot::A, 6) && !c.contains(Slot::A, 7),
            "behind (higher) evicted"
        );
    }

    #[test]
    fn scrubbing_evicts_by_absolute_distance_either_side() {
        let mut c = FrameCache::new();
        // Playhead 5; not playing -> bidirectional distance.
        fill(&mut c, Slot::A, &[1, 4, 5, 6, 9]);
        c.evict_to(3, &[(A, 5)], A, Direction::Forward, false, None, 0);
        // Furthest from 5 are 1 (dist 4) and 9 (dist 4); both dropped.
        assert!(c.contains(Slot::A, 5));
        assert!(
            c.contains(Slot::A, 4) && c.contains(Slot::A, 6),
            "nearest kept"
        );
        assert!(
            !c.contains(Slot::A, 1) && !c.contains(Slot::A, 9),
            "furthest evicted"
        );
    }

    #[test]
    fn lru_breaks_ties_at_equal_distance() {
        let mut c = FrameCache::new();
        // Equidistant from playhead 5: frames 4 and 6 (dist 1).
        c.insert(Slot::A, 5, frame());
        c.insert(Slot::A, 4, frame()); // inserted earlier
        c.insert(Slot::A, 6, frame()); // inserted later
        c.get(Slot::A, 6); // touch 6 so 4 is least-recently-used
        c.evict_to(2, &[(A, 5)], A, Direction::Forward, false, None, 0);
        assert!(c.contains(Slot::A, 6), "recently used survives the tie");
        assert!(!c.contains(Slot::A, 4), "LRU loses the tie");
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
        fill(&mut c, Slot::A, &[7, 8, 9, 10, 1, 2]);
        c.evict_to(4, &[(A, 9)], A, Direction::Forward, true, Some((1, 10)), 0);
        assert!(c.contains(Slot::A, 9), "playhead protected");
        assert!(
            c.contains(Slot::A, 10) && c.contains(Slot::A, 1) && c.contains(Slot::A, 2),
            "wrapped prefetch is 'just ahead' around the loop — kept"
        );
        assert!(
            !c.contains(Slot::A, 7) && !c.contains(Slot::A, 8),
            "just-behind frames are a full lap away — evicted first"
        );
    }

    /// Reverse Loop play mirrors the wrap: prefetch below the in point wraps to
    /// the out point and must survive over frames just above the playhead.
    #[test]
    fn reverse_loop_play_keeps_prefetch_wrapped_to_the_out_point() {
        let mut c = FrameCache::new();
        // Loop [1,10], playhead 2 reverse. Prefetch 1,10,9 (wrapping); 3,4 just shown.
        fill(&mut c, Slot::A, &[4, 3, 2, 1, 10, 9]);
        c.evict_to(4, &[(A, 2)], A, Direction::Reverse, true, Some((1, 10)), 0);
        assert!(c.contains(Slot::A, 2), "playhead protected");
        assert!(
            c.contains(Slot::A, 1) && c.contains(Slot::A, 10) && c.contains(Slot::A, 9),
            "prefetch wrapped past the in point kept"
        );
        assert!(
            !c.contains(Slot::A, 3) && !c.contains(Slot::A, 4),
            "just-behind (higher) frames evicted first"
        );
    }

    /// Frames left outside the loop range by a trim are never shown again on
    /// this lap: they outrank everything in range for eviction.
    #[test]
    fn loop_play_evicts_out_of_range_leftovers_first() {
        let mut c = FrameCache::new();
        // Loop trimmed to [1,10]; frame 12 is a leftover from the old range.
        fill(&mut c, Slot::A, &[12, 8, 9, 10, 1]);
        c.evict_to(4, &[(A, 9)], A, Direction::Forward, true, Some((1, 10)), 0);
        assert!(
            !c.contains(Slot::A, 12),
            "out-of-range leftover evicted before any in-range frame"
        );
        assert!(
            c.contains(Slot::A, 8) && c.contains(Slot::A, 10) && c.contains(Slot::A, 1),
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
        fill(&mut c, Slot::A, &[7, 8, 9, 10, 11, 12, 13]);
        c.evict_to(5, &[(A, 10)], A, Direction::Forward, true, None, 2);
        assert!(
            !c.contains(Slot::A, 7),
            "beyond the window: biased, evicted first"
        );
        assert!(
            !c.contains(Slot::A, 13),
            "furthest-ahead evicts before the reserved window"
        );
        assert!(
            c.contains(Slot::A, 8) && c.contains(Slot::A, 9),
            "just-shown frames survive (#169)"
        );
        assert!(
            c.contains(Slot::A, 11) && c.contains(Slot::A, 12),
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
        fill(&mut c, Slot::A, &[7, 8, 9, 10, 1]);
        c.evict_to(3, &[(A, 9)], A, Direction::Forward, true, Some((1, 10)), 1);
        assert!(c.contains(Slot::A, 9), "playhead protected");
        assert!(c.contains(Slot::A, 8), "just-shown frame survives (#169)");
        assert!(c.contains(Slot::A, 10), "nearest ahead kept");
        assert!(
            !c.contains(Slot::A, 7),
            "beyond the window: nearly a full lap away, evicted first"
        );
        assert!(
            !c.contains(Slot::A, 1),
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
        c.insert(Slot::A, 5, frame());
        for f in [8, 9, 10, 11, 12] {
            c.insert(s2, f, frame());
        }
        // Primary A on 5 (protected); source 2's playhead is 10. Squeeze to 3:
        // keep A/5 + s2/10 + the nearest s2 frame; drop s2's furthest (12 or 8).
        c.evict_to(3, &[(A, 5), (s2, 10)], A, Direction::Forward, true, None, 0);
        assert_eq!(c.len(), 3);
        assert!(c.contains(Slot::A, 5), "primary playhead protected");
        assert!(c.contains(s2, 10), "third source's playhead protected");
        // Its furthest-by-distance frame goes first (12 and 8 are dist 2; LRU tie →
        // 8 inserted earliest loses). Either way one of the far ones is gone.
        assert!(
            !c.contains(s2, 8) || !c.contains(s2, 12),
            "a far frame of the third source is evicted by its own distance"
        );
    }
}
