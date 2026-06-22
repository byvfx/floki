//! T1 ring cache for sequence playback (#56).
//!
//! Holds decoded frames as `Arc<ExrData>` keyed by `(slot, frame number)`, so a
//! frame can be the active image (T3) and stay resident (T1) at once without
//! cloning its pixel buffers, and a scrub-back or loop replay is an instant cache
//! hit instead of a re-decode. Bounded by a frame-count capacity derived from the
//! RAM budget (`crate::budget::max_t1`); eviction is **directional-ring + LRU**:
//! during linear play drop frames behind the playhead first, while scrubbing drop
//! by absolute distance — LRU breaks ties. See `docs/playback/memory-contract.md`.
//!
//! Keyed on `(Slot, frame)` so locked-step A/B (#7, Phase 5) is an extension, not
//! a rewrite; Phase 3 only ever stores `Slot::A` frames.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::exr_loader::ExrData;
use crate::playback::Direction;

/// Which image slot a cached frame belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Slot {
    A,
    B,
}

struct Entry {
    data: Arc<ExrData>,
    /// Monotonic access stamp for the LRU tiebreak.
    last_used: u64,
}

/// A byte-budgeted ring of decoded frames.
#[derive(Default)]
pub struct FrameCache {
    entries: HashMap<(Slot, u32), Entry>,
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

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn contains(&self, slot: Slot, frame: u32) -> bool {
        self.entries.contains_key(&(slot, frame))
    }

    /// Fetch a resident frame, bumping it as most-recently-used.
    pub fn get(&mut self, slot: Slot, frame: u32) -> Option<Arc<ExrData>> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(&(slot, frame))?;
        entry.last_used = tick;
        Some(entry.data.clone())
    }

    /// Insert (or replace) a frame, marking it most-recently-used. Does not evict;
    /// call [`FrameCache::evict_to`] afterward to enforce the budget.
    pub fn insert(&mut self, slot: Slot, frame: u32, data: Arc<ExrData>) {
        self.tick += 1;
        self.entries.insert(
            (slot, frame),
            Entry {
                data,
                last_used: self.tick,
            },
        );
    }

    /// The set of resident frame numbers for a slot (input to the scheduler).
    #[must_use]
    pub fn resident(&self, slot: Slot) -> HashSet<u32> {
        self.entries
            .keys()
            .filter(|(s, _)| *s == slot)
            .map(|(_, f)| *f)
            .collect()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Evict frames until at most `capacity` remain (capacity is floored at 1),
    /// protecting the active frame `(Slot::A, playhead)`. Victim selection is the
    /// directional-ring + LRU policy in [`FrameCache::pick_victim`].
    pub fn evict_to(
        &mut self,
        capacity: usize,
        playhead: u32,
        direction: Direction,
        playing: bool,
    ) {
        let cap = capacity.max(1);
        while self.entries.len() > cap {
            match self.pick_victim(playhead, direction, playing) {
                Some(victim) => {
                    self.entries.remove(&victim);
                }
                // Only the protected active frame remains — stop.
                None => break,
            }
        }
    }

    /// Choose the next frame to evict, or `None` if only the protected active
    /// frame is left. Higher "evictability" wins; LRU (smaller `last_used`)
    /// breaks ties.
    ///
    /// - **Playing** (linear): frames *behind* the playhead in the play direction
    ///   are evicted first (furthest-behind first); frames *ahead* are kept until
    ///   no behind frames remain, then furthest-ahead goes first.
    /// - **Scrubbing/paused**: evict by absolute distance from the playhead
    ///   (bidirectional), furthest first.
    fn pick_victim(
        &self,
        playhead: u32,
        direction: Direction,
        playing: bool,
    ) -> Option<(Slot, u32)> {
        // Rank "behind" frames above "ahead" frames during play by adding this
        // offset, so any behind frame outranks every ahead frame.
        const BEHIND_BIAS: u64 = 1 << 40;

        let evictability = |frame: u32| -> u64 {
            let dist = u64::from(frame.abs_diff(playhead));
            if !playing {
                return dist; // scrub: pure distance, either side.
            }
            let behind = match direction {
                Direction::Forward => frame < playhead,
                Direction::Reverse => frame > playhead,
            };
            if behind { BEHIND_BIAS + dist } else { dist }
        };

        self.entries
            .iter()
            // Never evict the frame currently on screen.
            .filter(|((slot, frame), _)| !(*slot == Slot::A && *frame == playhead))
            .map(|((slot, frame), entry)| (*slot, *frame, evictability(*frame), entry.last_used))
            // Max evictability; on a tie, the least-recently-used (smallest
            // last_used) frame is the victim.
            .max_by(|a, b| a.2.cmp(&b.2).then(b.3.cmp(&a.3)))
            .map(|(slot, frame, _, _)| (slot, frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn get_hits_resident_and_misses_absent() {
        let mut c = FrameCache::new();
        c.insert(Slot::A, 5, frame());
        assert!(c.get(Slot::A, 5).is_some());
        assert!(c.get(Slot::A, 6).is_none());
        // Slot is part of the key.
        assert!(c.get(Slot::B, 5).is_none());
    }

    #[test]
    fn resident_lists_only_the_requested_slot() {
        let mut c = FrameCache::new();
        fill(&mut c, Slot::A, &[1, 2, 3]);
        fill(&mut c, Slot::B, &[9]);
        let mut a: Vec<u32> = c.resident(Slot::A).into_iter().collect();
        a.sort_unstable();
        assert_eq!(a, vec![1, 2, 3]);
        assert_eq!(c.resident(Slot::B).into_iter().collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn evict_protects_the_playhead_frame() {
        let mut c = FrameCache::new();
        fill(&mut c, Slot::A, &[10, 11, 12]);
        // Capacity 1 with playhead on 11: everything else goes, 11 stays.
        c.evict_to(1, 11, Direction::Forward, true);
        assert_eq!(c.len(), 1);
        assert!(c.contains(Slot::A, 11), "active frame is never evicted");
    }

    #[test]
    fn playing_forward_evicts_behind_before_ahead() {
        let mut c = FrameCache::new();
        // Playhead 5; 3,4 are behind, 6,7 ahead.
        fill(&mut c, Slot::A, &[3, 4, 5, 6, 7]);
        // Trim to 3: the two behind frames (furthest first: 3 then 4) are dropped.
        c.evict_to(3, 5, Direction::Forward, true);
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
        c.evict_to(2, 5, Direction::Forward, true);
        // Keep playhead + nearest ahead; drop the furthest-ahead (8 then 7).
        assert!(c.contains(Slot::A, 5) && c.contains(Slot::A, 6));
        assert!(!c.contains(Slot::A, 7) && !c.contains(Slot::A, 8));
    }

    #[test]
    fn reverse_play_treats_higher_frames_as_behind() {
        let mut c = FrameCache::new();
        // Playhead 5, playing in reverse: 6,7 are "behind", 3,4 ahead.
        fill(&mut c, Slot::A, &[3, 4, 5, 6, 7]);
        c.evict_to(3, 5, Direction::Reverse, true);
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
        c.evict_to(3, 5, Direction::Forward, false);
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
        c.evict_to(2, 5, Direction::Forward, false);
        assert!(c.contains(Slot::A, 6), "recently used survives the tie");
        assert!(!c.contains(Slot::A, 4), "LRU loses the tie");
    }
}
