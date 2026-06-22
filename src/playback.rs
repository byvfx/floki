//! Sequence-playback state and the pure frame-advance logic (#7, Phase 2).
//!
//! This module owns the *what* of playback (the playhead, transport state, loop
//! semantics) but not the *how* of decoding — `app.rs` drives the on-demand load
//! of each frame and the egui clock. The advance rule ([`advance`]) is pure and
//! exhaustively unit-tested; the [`Playback`] struct is plain data plus a few
//! helpers. See `docs/playback/sequence-playback.md`.
//!
//! Phase 2 is decode-per-frame with no cache: stepping or playing issues a normal
//! load and swaps on arrival. The ring cache (#56) and decode-ahead worker (#57)
//! arrive in later phases; the contracts here (frame clock, loop modes, in/out)
//! are designed not to change when they do.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::sequence::Sequence;

/// Play direction. Persisted as the user's chosen base direction; mutated in
/// place by [`LoopMode::PingPong`] as it bounces.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
}

/// What happens when the playhead reaches the in/out boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum LoopMode {
    /// Play once and stop at the boundary.
    Once,
    /// Wrap around to the opposite boundary.
    #[default]
    Loop,
    /// Reverse direction at each boundary.
    PingPong,
}

/// Pacing policy when decode can't keep up with the target fps.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Pacing {
    /// Play every frame; effective fps drops, nothing is skipped. A review tool
    /// default. (In Phase 2 — decode-per-frame — this is the only behavior; the
    /// toggle is wired through for the cached phases.)
    #[default]
    Stutter,
    /// Advance on wall-time, skipping to the latest ready frame.
    DropFrames,
}

/// Transport state. Runtime-only (never persisted).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// The next frame number and (possibly flipped) direction after advancing one
/// step from `current` within the inclusive `[in_pt, out_pt]` range, or `None`
/// when [`LoopMode::Once`] has reached the boundary and playback should stop.
///
/// Pure over frame *numbers* — holes are not considered here; the caller checks
/// whether the resulting number has a file and holds the previous frame if not.
/// Assumes `in_pt <= out_pt`.
#[must_use]
pub fn advance(
    current: u32,
    in_pt: u32,
    out_pt: u32,
    dir: Direction,
    mode: LoopMode,
) -> Option<(u32, Direction)> {
    match dir {
        Direction::Forward => {
            if current < out_pt {
                Some((current + 1, Direction::Forward))
            } else {
                match mode {
                    LoopMode::Once => None,
                    LoopMode::Loop => Some((in_pt, Direction::Forward)),
                    // Bounce: step back inside the range, now reversing. A
                    // single-frame range (in == out) has nowhere to go.
                    LoopMode::PingPong if out_pt > in_pt => Some((out_pt - 1, Direction::Reverse)),
                    LoopMode::PingPong => Some((in_pt, Direction::Reverse)),
                }
            }
        }
        Direction::Reverse => {
            if current > in_pt {
                Some((current - 1, Direction::Reverse))
            } else {
                match mode {
                    LoopMode::Once => None,
                    LoopMode::Loop => Some((out_pt, Direction::Reverse)),
                    LoopMode::PingPong if out_pt > in_pt => Some((in_pt + 1, Direction::Forward)),
                    LoopMode::PingPong => Some((out_pt, Direction::Forward)),
                }
            }
        }
    }
}

/// Playback state attached to the app. Prefs (fps / loop / direction / pacing)
/// persist; the runtime playhead, loaded sequence, clock anchor, and in-flight
/// request do not (`#[serde(skip)]`) and reset on each open.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Playback {
    /// Target frames per second.
    pub fps_target: f32,
    pub loop_mode: LoopMode,
    pub direction: Direction,
    pub pacing: Pacing,

    /// The detected sequence, or `None` when a lone image is loaded.
    #[serde(skip)]
    pub sequence: Option<Sequence>,
    /// The playhead — a frame *number* in `[range.0, range.1]` (may sit on a hole).
    #[serde(skip)]
    pub current_frame: u32,
    /// In/out points (inclusive); default to the full range, user-trimmable.
    #[serde(skip)]
    pub in_point: u32,
    #[serde(skip)]
    pub out_point: u32,
    #[serde(skip)]
    pub state: PlayState,
    /// Absolute clock anchor for drift-free pacing (frame N is due at
    /// `anchor + N * period`). `None` until playback starts.
    #[serde(skip)]
    pub anchor: Option<Instant>,
    /// Frames advanced since `anchor` was (re)set.
    #[serde(skip)]
    pub frames_since_anchor: u32,
    /// Frame number whose decode is currently in flight (one at a time in Phase 2).
    #[serde(skip)]
    pub pending: Option<u32>,
    /// Smoothed measured fps for the readout.
    #[serde(skip)]
    pub measured_fps: f32,
    #[serde(skip)]
    last_shown: Option<Instant>,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            fps_target: 24.0,
            loop_mode: LoopMode::Loop,
            direction: Direction::Forward,
            pacing: Pacing::Stutter,
            sequence: None,
            current_frame: 0,
            in_point: 0,
            out_point: 0,
            state: PlayState::Stopped,
            anchor: None,
            frames_since_anchor: 0,
            pending: None,
            measured_fps: 0.0,
            last_shown: None,
        }
    }
}

impl Playback {
    /// Whether a sequence is loaded (transport UI + keys are active).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.sequence.is_some()
    }

    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.state == PlayState::Playing
    }

    /// Enter sequence mode: adopt `seq`, reset in/out to the full range, place
    /// the playhead at `start` (clamped), and reset the clock. Prefs are kept.
    pub fn enter(&mut self, seq: Sequence, start: u32) {
        let (lo, hi) = seq.range;
        self.in_point = lo;
        self.out_point = hi;
        self.current_frame = start.clamp(lo, hi);
        self.state = PlayState::Stopped;
        self.anchor = None;
        self.frames_since_anchor = 0;
        self.pending = None;
        self.measured_fps = 0.0;
        self.last_shown = None;
        self.sequence = Some(seq);
    }

    /// Leave sequence mode (a lone image was opened).
    pub fn clear(&mut self) {
        self.sequence = None;
        self.state = PlayState::Stopped;
        self.pending = None;
        self.anchor = None;
    }

    /// Begin playing from the current playhead, anchoring the clock to now.
    pub fn start_playing(&mut self, now: Instant) {
        self.state = PlayState::Playing;
        self.anchor = Some(now);
        self.frames_since_anchor = 0;
    }

    /// Frame period for the target fps (clamped so fps can't be ≤ 0).
    #[must_use]
    pub fn period(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.fps_target.max(1.0))
    }

    /// Path of the frame with the given number, or `None` for a hole / no sequence.
    #[must_use]
    pub fn frame_path(&self, number: u32) -> Option<&Path> {
        self.sequence.as_ref()?.path_for(number)
    }

    /// Record that a frame was shown, updating the smoothed measured fps.
    pub fn note_shown(&mut self, now: Instant) {
        if let Some(prev) = self.last_shown {
            let dt = now.duration_since(prev).as_secs_f32();
            if dt > 0.0 {
                let inst = 1.0 / dt;
                self.measured_fps = if self.measured_fps > 0.0 {
                    self.measured_fps * 0.8 + inst * 0.2
                } else {
                    inst
                };
            }
        }
        self.last_shown = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_advances_until_out_then_loops() {
        // 1..=3, forward, loop.
        assert_eq!(
            advance(1, 1, 3, Direction::Forward, LoopMode::Loop),
            Some((2, Direction::Forward))
        );
        assert_eq!(
            advance(2, 1, 3, Direction::Forward, LoopMode::Loop),
            Some((3, Direction::Forward))
        );
        // At out -> wrap to in.
        assert_eq!(
            advance(3, 1, 3, Direction::Forward, LoopMode::Loop),
            Some((1, Direction::Forward))
        );
    }

    #[test]
    fn reverse_advances_until_in_then_loops() {
        assert_eq!(
            advance(3, 1, 3, Direction::Reverse, LoopMode::Loop),
            Some((2, Direction::Reverse))
        );
        assert_eq!(
            advance(2, 1, 3, Direction::Reverse, LoopMode::Loop),
            Some((1, Direction::Reverse))
        );
        // At in -> wrap to out.
        assert_eq!(
            advance(1, 1, 3, Direction::Reverse, LoopMode::Loop),
            Some((3, Direction::Reverse))
        );
    }

    #[test]
    fn once_stops_at_each_boundary() {
        assert_eq!(advance(3, 1, 3, Direction::Forward, LoopMode::Once), None);
        assert_eq!(advance(1, 1, 3, Direction::Reverse, LoopMode::Once), None);
        // Mid-range still advances.
        assert_eq!(
            advance(2, 1, 3, Direction::Forward, LoopMode::Once),
            Some((3, Direction::Forward))
        );
    }

    #[test]
    fn pingpong_reverses_at_boundaries() {
        // Hitting out flips to reverse, stepping back inside.
        assert_eq!(
            advance(3, 1, 3, Direction::Forward, LoopMode::PingPong),
            Some((2, Direction::Reverse))
        );
        // Hitting in flips to forward.
        assert_eq!(
            advance(1, 1, 3, Direction::Reverse, LoopMode::PingPong),
            Some((2, Direction::Forward))
        );
        // Full bounce cycle: 1,2,3,2,1,2,...
        let (mut f, mut d) = (1u32, Direction::Forward);
        let mut seen = vec![f];
        for _ in 0..5 {
            let (nf, nd) = advance(f, 1, 3, d, LoopMode::PingPong).unwrap();
            f = nf;
            d = nd;
            seen.push(f);
        }
        assert_eq!(seen, vec![1, 2, 3, 2, 1, 2]);
    }

    #[test]
    fn single_frame_range_is_stable() {
        // in == out: loop and pingpong stay put rather than under/overflowing.
        assert_eq!(
            advance(5, 5, 5, Direction::Forward, LoopMode::Loop),
            Some((5, Direction::Forward))
        );
        assert_eq!(
            advance(5, 5, 5, Direction::Forward, LoopMode::PingPong),
            Some((5, Direction::Reverse))
        );
        assert_eq!(
            advance(5, 5, 5, Direction::Reverse, LoopMode::PingPong),
            Some((5, Direction::Forward))
        );
        assert_eq!(advance(5, 5, 5, Direction::Forward, LoopMode::Once), None);
    }

    #[test]
    fn period_is_inverse_fps_and_guards_zero() {
        let p = Playback {
            fps_target: 24.0,
            ..Default::default()
        };
        assert!((p.period().as_secs_f32() - 1.0 / 24.0).abs() < 1e-6);
        // Guarded to 1 fps, not a divide-by-zero.
        let p0 = Playback {
            fps_target: 0.0,
            ..Default::default()
        };
        assert_eq!(p0.period(), Duration::from_secs(1));
    }
}
