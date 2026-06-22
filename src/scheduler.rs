//! Decode want-list scheduler for sequence playback (#57).
//!
//! Pure: given the playhead, play direction, in/out range, loop mode, the set of
//! resident frames, and how far to decode ahead, produce the ordered list of
//! frame numbers worth decoding next. Priority order:
//!
//! - **P1** — the playhead frame, if it isn't resident (we fell behind).
//! - **P2** — frames ahead in the play direction, nearest first, up to
//!   `decode_ahead`, skipping anything already resident.
//!
//! (P0 — an explicit user seek — is handled by the app directly bumping the epoch
//! and requesting the frame; it isn't part of the prefetch want-list.)
//!
//! Back-pressure is the cache budget: the caller passes
//! `decode_ahead = min(configured, max_t1 - 1)`, so a want-list never asks for
//! more than the T1 ring can hold (this ties #57 to #56). The decode-ahead worker
//! that consumes this lands in Phase 4; Phase 3 ships it pure and tested.

use std::collections::HashSet;

use crate::playback::{Direction, LoopMode, advance};

/// Ordered frame numbers to decode next (highest priority first), excluding any
/// already in `resident`. At most `decode_ahead` prefetch frames follow the
/// (optional) playhead frame.
#[must_use]
pub fn want_list(
    playhead: u32,
    in_pt: u32,
    out_pt: u32,
    direction: Direction,
    mode: LoopMode,
    resident: &HashSet<u32>,
    decode_ahead: usize,
) -> Vec<u32> {
    let mut wants = Vec::new();

    // P1: the playhead itself, if we don't already have it.
    if !resident.contains(&playhead) {
        wants.push(playhead);
    }

    // P2: the prefetch *window* — the next `decode_ahead` positions in the play
    // direction (the same step rule the clock uses, so loop/ping-pong wrap is
    // consistent). Return the non-resident ones, nearest first. Walking a fixed
    // number of *positions* (not "until N non-resident found") bounds the window
    // to what the ring keeps: it never reaches past the cache horizon to a frame
    // that would be evicted and re-requested forever. The playhead and duplicates
    // (ping-pong revisits) are skipped, not pushed.
    let mut frame = playhead;
    let mut dir = direction;
    for _ in 0..decode_ahead {
        let Some((next, next_dir)) = advance(frame, in_pt, out_pt, dir, mode) else {
            break; // Once reached the boundary.
        };
        frame = next;
        dir = next_dir;
        if frame != playhead && !resident.contains(&frame) && !wants.contains(&frame) {
            wants.push(frame);
        }
    }
    wants
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resident(frames: &[u32]) -> HashSet<u32> {
        frames.iter().copied().collect()
    }

    #[test]
    fn playhead_comes_first_when_not_resident() {
        let r = resident(&[]);
        let got = want_list(5, 1, 10, Direction::Forward, LoopMode::Loop, &r, 2);
        assert_eq!(got, vec![5, 6, 7], "playhead, then prefetch ahead");
    }

    #[test]
    fn playhead_omitted_when_already_resident() {
        let r = resident(&[5]);
        let got = want_list(5, 1, 10, Direction::Forward, LoopMode::Loop, &r, 2);
        assert_eq!(got, vec![6, 7], "only prefetch ahead");
    }

    #[test]
    fn prefetches_in_reverse_direction() {
        let r = resident(&[5]);
        let got = want_list(5, 1, 10, Direction::Reverse, LoopMode::Loop, &r, 3);
        assert_eq!(got, vec![4, 3, 2]);
    }

    #[test]
    fn skips_resident_frames_in_the_prefetch_window() {
        let r = resident(&[5, 6, 8]);
        // Window = the next 3 positions (6, 7, 8); 6 and 8 are cached, so only 7
        // is wanted. The window does not reach past position 3 to 9/10.
        let got = want_list(5, 1, 10, Direction::Forward, LoopMode::Loop, &r, 3);
        assert_eq!(got, vec![7]);
    }

    #[test]
    fn loops_around_the_out_point() {
        let r = resident(&[3]);
        // Forward from 3 with out=3: wrap to in=1, then 2.
        let got = want_list(3, 1, 3, Direction::Forward, LoopMode::Loop, &r, 5);
        assert_eq!(got, vec![1, 2], "stops after covering the whole range once");
    }

    #[test]
    fn once_stops_at_the_boundary() {
        let r = resident(&[]);
        // Forward, Once, playhead at out point: only the playhead is wanted.
        let got = want_list(10, 1, 10, Direction::Forward, LoopMode::Once, &r, 4);
        assert_eq!(got, vec![10]);
    }

    #[test]
    fn pingpong_prefetch_follows_the_bounce() {
        let r = resident(&[]);
        // Window of 3 positions from playhead 9: 10, (bounce to 9 = playhead,
        // skipped), 8. Plus the playhead itself (not resident) first.
        let got = want_list(9, 1, 10, Direction::Forward, LoopMode::PingPong, &r, 3);
        assert_eq!(got, vec![9, 10, 8]);
    }

    #[test]
    fn decode_ahead_zero_yields_only_the_playhead() {
        let r = resident(&[]);
        assert_eq!(
            want_list(5, 1, 10, Direction::Forward, LoopMode::Loop, &r, 0),
            vec![5]
        );
        // ...and nothing at all when the playhead is already resident.
        let r2 = resident(&[5]);
        assert!(want_list(5, 1, 10, Direction::Forward, LoopMode::Loop, &r2, 0).is_empty());
    }
}
