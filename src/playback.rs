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

impl Direction {
    /// The other direction — the read-behind walk (#169) steps *against* play
    /// with the same `advance` rule the clock uses.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Forward => Self::Reverse,
            Self::Reverse => Self::Forward,
        }
    }
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
    /// The frame [`Self::note_shown`] last recorded, so a repeat report of the same
    /// displayed frame is ignored rather than logged as a ~0 ms interval.
    #[serde(skip)]
    last_shown_frame: Option<u32>,
    /// The last [`FRAME_TIME_WINDOW`] inter-shown intervals, oldest first (#100).
    /// `measured_fps` is an EWMA with a ~5-frame time constant, so a single long
    /// hitch is smeared into a dip that can't be quantified after the fact — but
    /// "stutter vs drop-frames" is exactly a tail question. This ring keeps the
    /// raw deltas so [`Self::frame_time_pcts`] can report the tail.
    #[serde(skip)]
    frame_times: std::collections::VecDeque<f32>,
    /// Supersession counter (#57). Bumped on every seek / scrub / direction or
    /// sequence change; each decode request and result carries the epoch at issue
    /// time, and the UI drops any result whose epoch no longer matches. Required
    /// because sequences recur the same paths (loop / ping-pong / scrub-back), so
    /// the `(path, is_b)` check alone can mistake a stale frame for the current one.
    #[serde(skip)]
    pub epoch: u64,
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
            last_shown_frame: None,
            frame_times: std::collections::VecDeque::with_capacity(FRAME_TIME_WINDOW),
            epoch: 0,
        }
    }
}

/// How many inter-shown intervals the percentile ring keeps — 10 s at 24 fps,
/// long enough for a tail to mean something and short enough that the numbers
/// still describe *now* rather than the whole session.
const FRAME_TIME_WINDOW: usize = 240;

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

    /// Whether the pixel readout / color sampler should be suppressed
    /// (INV-SAMPLE, #7): true while a sequence is loaded **and** either the clock
    /// is advancing or a seek's frame is still in flight. In both cases the
    /// displayed frame can lag the playhead, so a sample would either disagree
    /// with the playhead label or cost a full ~600 MB `ExrData` scan on every
    /// hover. The readout re-enables once the clock stops and the awaited frame
    /// has landed (`pending` cleared on swap). Gated on `is_active()` so
    /// single-image sampling stays live **by construction**.
    #[must_use]
    pub fn sampling_suppressed(&self) -> bool {
        self.is_active() && (self.is_playing() || self.pending.is_some())
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
        self.last_shown_frame = None;
        self.frame_times.clear();
        self.sequence = Some(seq);
        self.bump_epoch();
    }

    /// Leave sequence mode (a lone image was opened).
    pub fn clear(&mut self) {
        self.sequence = None;
        self.state = PlayState::Stopped;
        self.pending = None;
        self.anchor = None;
        self.bump_epoch();
    }

    /// Invalidate any in-flight decode: a stale result whose epoch differs from
    /// the new value is dropped on arrival. Call on every seek / scrub /
    /// direction or sequence change (#57).
    pub fn bump_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Begin playing from the current playhead. The clock anchor is set one
    /// period out so the current frame gets a full period of dwell before the
    /// first advance (otherwise `tick_playback`'s `anchor + n·period` deadline is
    /// already due on the very frame Play was pressed, skipping the start frame).
    /// Also resets the pacing baseline. A displayed frame is recorded whenever the
    /// picture changes, including the first paint after a file is opened — so
    /// without clearing here, the first interval of a run spans however long the app
    /// sat *idle* before Play was pressed and is filed as a frame time. Observed: a
    /// 240-second "frame" on a 40-second run, which then dominates `max` and `p99`.
    /// Pause → resume is the same hazard, and this covers it: the gap while paused
    /// is not a frame time either.
    pub fn start_playing(&mut self, now: Instant) {
        self.state = PlayState::Playing;
        self.anchor = Some(now + self.period());
        self.frames_since_anchor = 0;
        self.last_shown = None;
        self.last_shown_frame = None;
    }

    /// Halt the clock without leaving sequence mode — the mirror of
    /// [`Self::start_playing`], and the counterpart [`Self::stop`] is for a full
    /// reset.
    ///
    /// Exists because pause was the one transport transition with no method: five
    /// call sites assigned `state = Paused` directly, and every one of them
    /// therefore skipped the shown-frame bookkeeping that `start_playing` does.
    /// A pause, a long idle, then a step filed the whole idle span as one frame
    /// time — 20.5 s and 27.2 s samples observed in a single session (#236).
    ///
    /// `measured_fps` is deliberately *not* cleared: it reads as the rate of the
    /// run just paused, which is what the HUD should show. `stop` clears it.
    pub fn pause(&mut self) {
        self.state = PlayState::Paused;
        self.last_shown = None;
        self.last_shown_frame = None;
    }

    /// Stop the clock and reset the pacing measurement. The smoothed
    /// `measured_fps` reflects the *last* playback, so it's cleared here — a
    /// stopped transport reads `0.0` rather than a stale rate. The caller sets the
    /// playhead (stop rewinds to the in point).
    ///
    /// Also drops any awaited decode (`pending`): a stopped transport must not
    /// stay sampling-suppressed (INV-SAMPLE, #7), and a lingering `pending` would
    /// gate the next Play's first advance (`tick_stutter` holds while
    /// `pending.is_some()`) on a frame whose result may never arrive.
    pub fn stop(&mut self) {
        self.state = PlayState::Stopped;
        self.anchor = None;
        self.frames_since_anchor = 0;
        self.measured_fps = 0.0;
        self.last_shown = None;
        self.last_shown_frame = None;
        self.frame_times.clear();
        self.pending = None;
    }

    /// Drop the pacing **statistics** without disturbing the transport (#329).
    ///
    /// For when the source being measured changes underneath a running clock:
    /// `note_shown` records only the frames of whichever source drives the clock,
    /// and `clock_source()` can re-point itself when a layer is hidden or a solo
    /// excludes it (#211). The ring then holds two sources' cadences mixed, and
    /// p50/p95/p99 describe neither — the same way an idle span filed as one
    /// enormous frame time made the percentiles describe nothing in #236.
    ///
    /// Deliberately narrower than [`Self::stop`]. `anchor` and
    /// `frames_since_anchor` stay, so the drift-corrected clock keeps its
    /// reference and playback does not hitch when a layer is hidden.
    ///
    /// **`measured_fps` stays too.** The ring holds a hundred samples, so it
    /// carries the old source's cadence for seconds and has to go; the EWMA has a
    /// ~5-frame time constant and re-converges in about a fifth of a second by
    /// itself, so zeroing it buys nothing and costs a readout that says `0.0`
    /// during real playback — which is the exact misleading signal #249 was
    /// written to get rid of ("the one number that should have said slow said
    /// 0.0"). Dogfooding caught this: hiding a layer mid-run made the fps display
    /// read zero.
    ///
    /// `last_shown` does go, or the first interval after the change would span
    /// both sources and land in the ring as one oversized sample.
    pub fn reset_pacing_stats(&mut self) {
        self.last_shown = None;
        self.last_shown_frame = None;
        self.frame_times.clear();
    }

    /// Frame period for the target fps (clamped so fps can't be ≤ 0).
    #[must_use]
    pub fn period(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.fps_target.max(1.0))
    }

    /// Set the in point to the current playhead (a P0 trim). Clamped so it never
    /// passes the out point; the playhead already sits at the new in point, so the
    /// range stays valid. Bumps the epoch (a trim supersedes prefetch, which may
    /// have run past the new boundary).
    pub fn set_in(&mut self) {
        self.in_point = self.current_frame.min(self.out_point);
        self.bump_epoch();
    }

    /// Set the out point to the current playhead (a P0 trim). Clamped so it never
    /// precedes the in point. Bumps the epoch.
    pub fn set_out(&mut self) {
        self.out_point = self.current_frame.max(self.in_point);
        self.bump_epoch();
    }

    /// Reset the in/out trim to the full sequence range. No-op without a sequence.
    pub fn reset_trim(&mut self) {
        if let Some(seq) = &self.sequence {
            let (lo, hi) = seq.range;
            self.in_point = lo;
            self.out_point = hi;
            self.bump_epoch();
        }
    }

    /// The full sequence range `(min, max)`, or `None` without a sequence — the
    /// span the timeline draws, independent of the in/out trim.
    #[must_use]
    pub fn full_range(&self) -> Option<(u32, u32)> {
        self.sequence.as_ref().map(|s| s.range)
    }

    /// Path of the frame with the given number, or `None` for a hole / no sequence.
    #[must_use]
    pub fn frame_path(&self, number: u32) -> Option<&Path> {
        self.sequence.as_ref()?.path_for(number)
    }

    /// Record that `frame` was shown, updating the smoothed measured fps and the
    /// percentile ring.
    ///
    /// **Idempotent per frame**: repeating the frame already recorded is ignored.
    /// The same displayed frame can be reported by more than one path in a single
    /// update (a cache-residency hit and an arriving decode, say), and the two calls
    /// land microseconds apart — `1.0 / dt` then produces an instantaneous rate in
    /// the thousands, which the EWMA happily folds into `measured_fps` (observed:
    /// 14150 fps on a 24 fps target). A display can only show a given frame once, so
    /// the second report is never new information.
    pub fn note_shown(&mut self, now: Instant, frame: u32) {
        if self.last_shown_frame == Some(frame) {
            return;
        }
        self.last_shown_frame = Some(frame);
        // Only *playback* is measured (#236). Both figures below describe the
        // clock's pacing, so an interval recorded while the clock isn't running
        // has no meaning — and a paused one is unbounded, being however long the
        // user looked at the frame. `pause` clearing `last_shown` already stops
        // the known paths; this is the invariant stated where the measurement
        // happens, so a pause path added later can't reopen it.
        if !matches!(self.state, PlayState::Playing) {
            self.last_shown = Some(now);
            return;
        }
        if let Some(prev) = self.last_shown {
            let dt = now.duration_since(prev).as_secs_f32();
            if dt > 0.0 {
                let inst = 1.0 / dt;
                self.measured_fps = if self.measured_fps > 0.0 {
                    self.measured_fps * 0.8 + inst * 0.2
                } else {
                    inst
                };
                if self.frame_times.len() == FRAME_TIME_WINDOW {
                    self.frame_times.pop_front();
                }
                self.frame_times.push_back(dt * 1000.0);
            }
        }
        self.last_shown = Some(now);
    }

    /// Frame-time tail over the ring, in **milliseconds**: `(p50, p95, p99, max)`.
    /// `None` until at least one interval has been recorded — an empty ring reads
    /// as "no data", never as a zeroed-out perfect score.
    ///
    /// Nearest-rank percentiles over a sorted copy. The ring is bounded at
    /// [`FRAME_TIME_WINDOW`], so this is a fixed ~240-element sort — cheap enough
    /// for the 1 Hz trace and the (opt-in) debug overlay.
    #[must_use]
    pub fn frame_time_pcts(&self) -> Option<(f32, f32, f32, f32)> {
        if self.frame_times.is_empty() {
            return None;
        }
        let mut v: Vec<f32> = self.frame_times.iter().copied().collect();
        v.sort_unstable_by(f32::total_cmp);
        // Nearest-rank: the ceil(p·n)-th value, 1-indexed.
        let at = |p: f32| -> f32 {
            let n = v.len();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let rank = (p * n as f32).ceil() as usize;
            v[rank.clamp(1, n) - 1]
        };
        Some((at(0.50), at(0.95), at(0.99), v[v.len() - 1]))
    }

    /// How many intervals the percentile ring currently holds — the sample count
    /// behind [`Self::frame_time_pcts`], so a soak log can tell a real p99 from
    /// one computed over three frames.
    #[must_use]
    pub fn frame_time_samples(&self) -> usize {
        self.frame_times.len()
    }
}

/// One sample of the playback pipeline for [`DecodeBound`] (#249).
#[derive(Clone, Copy, Debug)]
pub struct DecodeBoundSample {
    pub playing: bool,
    /// Supersession epoch. Bumped on every seek / scrub / direction change, so a
    /// change restarts the window — which is how scrubbing is excluded without a
    /// separate "am I scrubbing" flag: dragging the playhead is *expected* to be
    /// decode-bound and must never raise the hint.
    pub epoch: u64,
    /// Turnaround of the last completed sequence decode.
    pub last_decode: Option<std::time::Duration>,
    /// One frame's wall clock at the target rate.
    pub frame_period: std::time::Duration,
    /// Frames the pacer held late or dropped during this play run — the picture
    /// failing to keep up, counted. Held *and* dropped because which one grows
    /// depends on the pacing mode.
    pub behind: u32,
}

/// What to tell the user, with the figures that justify it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodeBoundHint {
    pub decode_ms: u32,
    pub budget_ms: u32,
}

/// Detects a *sustained* decode-bound transport (#249): playback that presents as
/// frozen rather than slow, because decode turnaround is many times the frame
/// period and the displayed frame never advances.
///
/// **Keyed on `last_decode`, deliberately not on `stale`.** #249 proposed
/// `stale > 0` sustained, on the strength of that field's own comment calling it
/// the headline "is the picture keeping up" number. Measured on a 1.03 GB/frame
/// render it reaches **2 while playing at 25.6 fps** — comfortably keeping up — so a
/// hint built on it would fire during healthy playback. `stale` counts sources
/// painting a frame other than their playhead, and at 24 fps with prefetch in
/// flight that is momentarily true all the time. It reads well in a trace beside
/// its neighbours; it is not a predicate.
///
/// `last_decode` separates cleanly on the same footage: 0.03–0.05 s healthy against
/// 0.56–0.80 s bound, with nothing in between. [`BOUND_FACTOR`] sits an order of
/// magnitude clear of both.
#[derive(Default, Debug)]
pub struct DecodeBound {
    /// When the condition began holding continuously; `None` while it does not.
    since: Option<std::time::Instant>,
    /// `behind` when it began, so the hint also requires the count to be *growing*
    /// — a slow decode that is nonetheless keeping the picture moving is not this.
    behind_at_start: u32,
    /// Epoch the window belongs to; a change restarts it.
    epoch: u64,
    /// Epoch at which the user dismissed the hint. Any seek re-arms it, so
    /// dismissal silences the current condition rather than the feature.
    dismissed_at: Option<u64>,
}

/// Decode turnaround must exceed this multiple of the frame period. At 24 fps that
/// is 83 ms, against 30–50 ms measured healthy and 560–800 ms measured bound.
const BOUND_FACTOR: u32 = 2;

/// How long the condition must hold before the hint appears. A seek or a loop wrap
/// costs one slow decode; this is about the state that does not recover.
const HOLD: std::time::Duration = std::time::Duration::from_secs(2);

impl DecodeBound {
    /// Feed one sample and get the hint to show, if any. `now` is passed in rather
    /// than read so the whole thing is testable without a clock.
    pub fn update(
        &mut self,
        now: std::time::Instant,
        s: &DecodeBoundSample,
    ) -> Option<DecodeBoundHint> {
        // A seek restarts everything, including a dismissal: the user asked about a
        // different part of the timeline, and the old verdict no longer applies.
        if s.epoch != self.epoch {
            self.epoch = s.epoch;
            self.since = None;
            self.dismissed_at = None;
        }
        let budget = s.frame_period;
        let bound = s.playing
            && !budget.is_zero()
            && s.last_decode.is_some_and(|d| d > budget * BOUND_FACTOR);
        if !bound {
            // Clears immediately rather than lingering: the condition ending is the
            // good news, and a stale warning is the thing that makes warnings
            // ignorable. Stopping also lands here, since `playing` goes false —
            // which matters, because `last_decode` keeps its bound value after a
            // stopped run and would otherwise pin the hint up forever.
            self.since = None;
            return None;
        }
        let started = *self.since.get_or_insert_with(|| {
            self.behind_at_start = s.behind;
            now
        });
        if now.duration_since(started) < HOLD {
            return None;
        }
        // Still falling behind, not merely slow. In the healthy capture this count
        // rose during spin-up and then flatlined; in the bound one it climbed for
        // the whole run.
        if s.behind <= self.behind_at_start {
            return None;
        }
        if self.dismissed_at == Some(s.epoch) {
            return None;
        }
        let ms = |d: std::time::Duration| u32::try_from(d.as_millis()).unwrap_or(u32::MAX);
        Some(DecodeBoundHint {
            decode_ms: ms(s.last_decode?),
            budget_ms: ms(budget),
        })
    }

    /// Dismiss the hint for the current condition. Re-armed by the next seek.
    pub fn dismiss(&mut self) {
        self.dismissed_at = Some(self.epoch);
    }
}

#[cfg(test)]
mod decode_bound_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 24 fps.
    const BUDGET: Duration = Duration::from_micros(41_667);

    /// Measured on redSea (4K, 1.03 GB/frame) with beauty preview and proxy off:
    /// 560–800 ms turnaround, `run_held` climbing the whole run.
    fn bound(behind: u32) -> DecodeBoundSample {
        DecodeBoundSample {
            playing: true,
            epoch: 1,
            last_decode: Some(Duration::from_millis(650)),
            frame_period: BUDGET,
            behind,
        }
    }

    /// The same footage at defaults: 30–50 ms turnaround, 25.6 fps, `run_held` flat.
    fn healthy(behind: u32) -> DecodeBoundSample {
        DecodeBoundSample {
            playing: true,
            epoch: 1,
            last_decode: Some(Duration::from_millis(50)),
            frame_period: BUDGET,
            behind,
        }
    }

    #[test]
    fn fires_only_after_the_condition_is_sustained() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();

        // One slow decode is a seek or a loop wrap, not this.
        assert!(d.update(t0, &bound(0)).is_none());
        assert!(
            d.update(t0 + Duration::from_millis(1_500), &bound(30))
                .is_none()
        );

        let hint = d
            .update(t0 + Duration::from_millis(2_100), &bound(60))
            .expect("sustained past the hold");
        assert_eq!(hint.decode_ms, 650);
        assert_eq!(hint.budget_ms, 41);
    }

    /// The regression this whole detector is shaped around: healthy playback of the
    /// *same* footage must stay silent, however long it runs. `stale` reached 2 here
    /// while keeping up at 25.6 fps, which is why it isn't the signal.
    #[test]
    fn never_fires_on_healthy_playback() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();
        for i in 0..60 {
            let at = t0 + Duration::from_millis(i * 500);
            // `behind` climbs during spin-up then flatlines, as measured.
            let behind = if i < 6 { i as u32 * 9 } else { 52 };
            assert!(
                d.update(at, &healthy(behind)).is_none(),
                "healthy playback raised the hint at sample {i}"
            );
        }
    }

    /// A decode slower than the budget that is nonetheless keeping the picture
    /// moving is slow, not stuck — the hint is about a transport that has stopped
    /// advancing, and `behind` is what says so.
    #[test]
    fn a_slow_decode_that_keeps_up_is_not_decode_bound() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();
        for i in 0..10 {
            let at = t0 + Duration::from_millis(i * 500);
            assert!(
                d.update(at, &bound(7)).is_none(),
                "`behind` never grew, so nothing is falling behind"
            );
        }
    }

    /// Stopping must clear it. `last_decode` keeps its bound value after the run
    /// ends — measured at 0.70 s on a stopped transport — so without the `playing`
    /// gate the hint would stay up on a stopped app forever.
    #[test]
    fn clears_when_playback_stops_even_though_last_decode_is_still_slow() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();
        d.update(t0, &bound(0));
        assert!(d.update(t0 + Duration::from_secs(3), &bound(90)).is_some());

        let stopped = DecodeBoundSample {
            playing: false,
            ..bound(90)
        };
        assert!(d.update(t0 + Duration::from_secs(4), &stopped).is_none());
        // And it does not come straight back on resume without re-earning the hold.
        assert!(d.update(t0 + Duration::from_secs(5), &bound(95)).is_none());
    }

    /// Scrubbing is expected to be decode-bound. A seek bumps the epoch, which
    /// restarts the window, so dragging the playhead can never accumulate one.
    #[test]
    fn a_seek_restarts_the_window() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();
        d.update(t0, &bound(0));
        // Two seconds in, still bound — but the user seeked, so the clock restarts.
        let seeked = DecodeBoundSample {
            epoch: 2,
            ..bound(60)
        };
        assert!(d.update(t0 + Duration::from_secs(2), &seeked).is_none());
        // It fires two seconds after the *seek*, not after the original start.
        let later = DecodeBoundSample {
            epoch: 2,
            ..bound(120)
        };
        assert!(
            d.update(t0 + Duration::from_millis(4_100), &later)
                .is_some()
        );
    }

    #[test]
    fn dismissal_holds_until_the_next_seek() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();
        d.update(t0, &bound(0));
        assert!(d.update(t0 + Duration::from_secs(3), &bound(90)).is_some());

        d.dismiss();
        assert!(d.update(t0 + Duration::from_secs(4), &bound(120)).is_none());

        // A seek re-arms: a new part of the timeline is a new question.
        let seeked = DecodeBoundSample {
            epoch: 2,
            ..bound(150)
        };
        assert!(d.update(t0 + Duration::from_secs(5), &seeked).is_none());
        let later = DecodeBoundSample {
            epoch: 2,
            ..bound(200)
        };
        assert!(d.update(t0 + Duration::from_secs(8), &later).is_some());
    }

    /// No measurement yet, or a nonsense frame period, must not be read as trouble.
    #[test]
    fn is_inert_without_a_usable_measurement() {
        let mut d = DecodeBound::default();
        let t0 = Instant::now();
        let no_decode = DecodeBoundSample {
            last_decode: None,
            ..bound(90)
        };
        let zero_budget = DecodeBoundSample {
            frame_period: Duration::ZERO,
            ..bound(90)
        };
        for i in 0..10 {
            let at = t0 + Duration::from_millis(i * 500);
            assert!(d.update(at, &no_decode).is_none());
            assert!(d.update(at, &zero_budget).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    #[test]
    fn stop_resets_the_fps_measurement() {
        let mut pb = Playback::default();
        pb.start_playing(Instant::now());
        // Two shown frames give a non-zero measured fps.
        pb.note_shown(Instant::now(), 1);
        pb.note_shown(Instant::now(), 2);
        // (measured_fps may be 0 if the two `now`s coincide; the contract under
        // test is that stop() zeroes it regardless.)
        pb.measured_fps = 24.0;
        pb.stop();
        assert_eq!(pb.state, PlayState::Stopped);
        assert_eq!(pb.measured_fps, 0.0, "stop clears the stale rate");
        assert!(pb.anchor.is_none());
    }

    /// Feed the ring a run of exact intervals, bypassing the wall clock. Each call
    /// uses a distinct frame number so the per-frame dedupe never suppresses one.
    ///
    /// Starts the clock first: only playback is measured (#236), so a run fed to a
    /// stopped transport would record nothing at all.
    fn shown_after(pb: &mut Playback, gaps_ms: &[u64]) {
        let base = Instant::now();
        let mut t = base;
        let mut frame = 0u32;
        pb.start_playing(t);
        pb.note_shown(t, frame);
        for ms in gaps_ms {
            t += Duration::from_millis(*ms);
            frame += 1;
            pb.note_shown(t, frame);
        }
    }

    #[test]
    fn note_shown_ignores_a_repeat_of_the_same_frame() {
        // A displayed frame can be reported by more than one path in a single update
        // (cache-residency hit and arriving decode), microseconds apart. Recorded as
        // an interval that is a rate in the thousands, which the EWMA folds into
        // `measured_fps` — 14150 fps was observed on a 24 fps target (#100). A
        // display shows a given frame once, so the repeat is never new information.
        let mut pb = Playback::default();
        let t = Instant::now();
        pb.start_playing(t); // only playback is measured (#236)
        pb.note_shown(t, 1);
        pb.note_shown(t + Duration::from_millis(40), 2);
        assert_eq!(pb.frame_time_samples(), 1);
        let fps_after_two = pb.measured_fps;

        // Same frame again, a hair later — must not record.
        pb.note_shown(t + Duration::from_micros(40_070), 2);
        assert_eq!(
            pb.frame_time_samples(),
            1,
            "a repeat of frame 2 records no interval"
        );
        assert_eq!(
            pb.measured_fps, fps_after_two,
            "and does not disturb the smoothed rate"
        );

        // A genuinely new frame still records.
        pb.note_shown(t + Duration::from_millis(80), 3);
        assert_eq!(pb.frame_time_samples(), 2);
    }

    #[test]
    fn starting_playback_does_not_record_the_idle_gap_as_a_frame_time() {
        // The first paint after an open records a displayed frame, so the next one —
        // whenever the user finally presses Play — would otherwise be filed as an
        // interval covering all the idle time in between. Observed as a 240-second
        // "frame" on a 40-second run, dominating `max` and `p99`.
        let mut pb = Playback::default();
        let t = Instant::now();
        pb.note_shown(t, 1); // first paint on open

        // ...a long idle, then Play.
        pb.start_playing(t + Duration::from_secs(240));
        pb.note_shown(t + Duration::from_secs(240), 2);
        assert_eq!(
            pb.frame_time_samples(),
            0,
            "the idle span before Play is not a frame time"
        );

        // Frames shown while actually playing measure normally.
        pb.note_shown(t + Duration::from_secs(240) + Duration::from_millis(40), 3);
        assert_eq!(pb.frame_time_samples(), 1);
        let (_, _, _, max) = pb.frame_time_pcts().unwrap();
        assert!((max - 40.0).abs() < 1.0, "got {max}");
    }

    #[test]
    fn pausing_then_stepping_after_an_idle_does_not_record_the_gap() {
        // #236, found dogfooding. `start_playing` nulls the shown marker so the
        // idle *before* Play isn't filed as a frame time — but pause had no
        // method at all, five call sites assigned `state = Paused` raw, and none
        // did the same. Pause, look at the frame for twenty seconds, step: the
        // whole idle span landed in the ring as one sample. Observed at 20567 ms
        // and 27220 ms in a single session, against a real worst frame of ~55 ms.
        let mut pb = Playback::default();
        let t = Instant::now();
        pb.start_playing(t);
        pb.note_shown(t, 1);
        pb.note_shown(t + Duration::from_millis(40), 2);
        assert_eq!(pb.frame_time_samples(), 1);
        let (_, _, _, max) = pb.frame_time_pcts().unwrap();
        assert!((max - 40.0).abs() < 1.0, "got {max}");

        // Pause, a long look, then step to the next frame.
        pb.pause();
        pb.note_shown(t + Duration::from_secs(20), 3);
        assert_eq!(
            pb.frame_time_samples(),
            1,
            "the idle span while paused is not a frame time"
        );
        let (_, _, _, max) = pb.frame_time_pcts().unwrap();
        assert!((max - 40.0).abs() < 1.0, "tail unpoisoned, got {max}");

        // Stepping *again* after another long look is equally not a frame time —
        // every step re-enters pause, so no paused interval is ever recorded.
        pb.note_shown(t + Duration::from_secs(40), 4);
        assert_eq!(pb.frame_time_samples(), 1);
    }

    #[test]
    fn only_playback_is_measured() {
        // The invariant stated where the measurement happens, so a pause path
        // added later cannot reopen #236 the way the five raw assignments did.
        // Both figures describe the clock's pacing, so neither means anything
        // while the clock is not running.
        let t = Instant::now();
        for state in [PlayState::Stopped, PlayState::Paused] {
            let mut pb = Playback {
                state,
                ..Playback::default()
            };
            pb.note_shown(t, 1);
            pb.note_shown(t + Duration::from_millis(40), 2);
            assert_eq!(
                pb.frame_time_samples(),
                0,
                "{state:?}: the clock is not running, so there is no frame time"
            );
            assert_eq!(pb.measured_fps, 0.0, "{state:?}: nor a rate");
        }
    }

    #[test]
    fn pausing_keeps_the_rate_of_the_run_it_paused() {
        // `measured_fps` is deliberately not cleared on pause: the HUD should read
        // the rate of the run just paused, not zero. `stop` is the one that resets.
        let mut pb = Playback::default();
        let t = Instant::now();
        pb.start_playing(t);
        for (i, ms) in [0u64, 40, 80, 120].iter().enumerate() {
            pb.note_shown(t + Duration::from_millis(*ms), i as u32 + 1);
        }
        let playing_fps = pb.measured_fps;
        assert!(playing_fps > 0.0);

        pb.pause();
        assert_eq!(pb.measured_fps, playing_fps, "pause holds the rate");
        pb.stop();
        assert_eq!(pb.measured_fps, 0.0, "stop resets it");
    }

    #[test]
    fn note_shown_records_a_frame_shown_again_after_a_loop() {
        // (Playing throughout — only playback is measured, #236.)
        // The dedupe is against the *immediately* preceding frame, not a history:
        // looping back onto a frame is a real display event and must count, or a
        // short in/out range would stop being measured entirely.
        let mut pb = Playback::default();
        let t = Instant::now();
        pb.start_playing(t);
        pb.note_shown(t, 1);
        pb.note_shown(t + Duration::from_millis(40), 2);
        pb.note_shown(t + Duration::from_millis(80), 1);
        assert_eq!(
            pb.frame_time_samples(),
            2,
            "1 → 2 → 1 is two intervals; the wrap back to 1 is a real frame"
        );
    }

    #[test]
    fn reset_pacing_stats_drops_the_measurements_and_keeps_the_clock() {
        // #329: `note_shown` records only the clock source's frames, and the
        // effective clock re-points itself when a layer is hidden (#211). Without
        // a reset the ring holds two sources' cadences mixed and the percentiles
        // describe neither — the #236 failure in a different disguise.
        let mut pb = Playback::default();
        shown_after(&mut pb, &[40u64; 30]);
        assert_eq!(pb.frame_time_samples(), 30);
        assert!(pb.measured_fps > 0.0);
        let (anchor, since) = (pb.anchor, pb.frames_since_anchor);

        pb.reset_pacing_stats();

        assert_eq!(pb.frame_time_samples(), 0, "the ring is dropped");
        assert!(pb.frame_time_pcts().is_none(), "and reports no percentiles");
        assert!(
            pb.measured_fps > 0.0,
            "but the EWMA stays: it re-converges in ~5 frames on its own, and \
             zeroing it makes the readout say 0.0 during real playback (#249)"
        );
        assert_eq!(
            (pb.anchor, pb.frames_since_anchor),
            (anchor, since),
            "but the drift-corrected clock keeps its reference — hiding a layer \
             must not hitch playback"
        );
    }

    #[test]
    fn frame_time_pcts_reports_the_tail_the_ewma_hides() {
        // 2 of 100 frames hitch at 400 ms, early in the run, then 78 clean frames.
        // The EWMA has a ~5-frame time constant, so `measured_fps` has fully
        // recovered by the end and reports the run as healthy — the percentile
        // ring is what still shows the hitches (#100).
        let mut pb = Playback::default();
        let mut gaps = vec![40u64; 20];
        gaps.extend([400, 400]);
        gaps.extend(std::iter::repeat_n(40u64, 78));
        shown_after(&mut pb, &gaps);

        assert_eq!(pb.frame_time_samples(), 100);
        assert!(
            pb.measured_fps > 24.0,
            "the EWMA has forgotten the hitches, reading {:.1} fps",
            pb.measured_fps
        );

        let (p50, p95, p99, max) = pb.frame_time_pcts().expect("100 intervals recorded");
        assert!(
            (p50 - 40.0).abs() < 1.0,
            "p50 is the nominal 40 ms, got {p50}"
        );
        assert!((p95 - 40.0).abs() < 1.0, "p95 still nominal, got {p95}");
        assert!(
            (p99 - 400.0).abs() < 1.0,
            "2% of frames hitched, so the p99 is a hitch, got {p99}"
        );
        assert!(
            (max - 400.0).abs() < 1.0,
            "max is the hitch itself, got {max}"
        );
    }

    #[test]
    fn frame_time_ring_is_bounded_and_drops_the_oldest() {
        // Overfill with a leading hitch: once it slides out of the window the
        // tail must return to nominal rather than reporting a stale spike.
        let mut pb = Playback::default();
        let mut gaps = vec![500u64];
        gaps.extend(std::iter::repeat_n(40u64, FRAME_TIME_WINDOW + 10));
        shown_after(&mut pb, &gaps);

        assert_eq!(
            pb.frame_time_samples(),
            FRAME_TIME_WINDOW,
            "the ring is bounded"
        );
        let (_, _, _, max) = pb.frame_time_pcts().unwrap();
        assert!(
            (max - 40.0).abs() < 1.0,
            "the evicted 500 ms hitch no longer shows, got {max}"
        );
    }

    #[test]
    fn frame_time_pcts_is_none_before_any_interval() {
        // One shown frame is a timestamp, not an interval — an empty ring must
        // read "no data", never a zeroed-out perfect score.
        let mut pb = Playback::default();
        assert_eq!(pb.frame_time_pcts(), None, "nothing shown yet");
        pb.note_shown(Instant::now(), 1);
        assert_eq!(pb.frame_time_pcts(), None, "one frame is no interval");
    }

    #[test]
    fn enter_and_stop_clear_the_percentile_ring() {
        // The ring describes the *current* run; a stale tail carried across a
        // stop or a new sequence would misreport the next soak segment.
        let mut pb = Playback::default();
        shown_after(&mut pb, &[40, 40, 40]);
        assert!(pb.frame_time_samples() > 0);
        pb.stop();
        assert_eq!(pb.frame_time_samples(), 0, "stop clears the ring");

        shown_after(&mut pb, &[40, 40, 40]);
        assert!(pb.frame_time_samples() > 0);
        pb.enter(seq(1, 3), 1);
        assert_eq!(pb.frame_time_samples(), 0, "a new sequence clears the ring");
    }

    #[test]
    fn stop_clears_the_awaited_frame() {
        // A lingering `pending` after Stop would keep the readout suppressed and
        // gate the next Play's first advance (`tick_stutter` holds while
        // `pending.is_some()`) on a frame whose result may never arrive.
        let mut pb = Playback::default();
        pb.start_playing(Instant::now());
        pb.pending = Some(7);
        pb.stop();
        assert_eq!(pb.pending, None, "stop drops the awaited decode");
    }

    #[test]
    fn sampling_suppressed_tracks_play_and_pending() {
        // No sequence loaded → never suppressed, even with state/pending set
        // (single-image sampling stays live by construction).
        let mut pb = Playback {
            state: PlayState::Playing,
            pending: Some(7),
            ..Default::default()
        };
        assert!(
            !pb.sampling_suppressed(),
            "no sequence ⇒ readout always live"
        );

        // Enter a 2-frame sequence; stopped → live readout.
        let seq = Sequence {
            frames: vec![PathBuf::from("f.0001.exr"), PathBuf::from("f.0002.exr")],
            range: (1, 2),
            holes: vec![],
        };
        pb.enter(seq, 1);
        assert!(!pb.sampling_suppressed());

        // Advancing clock → suppressed.
        pb.start_playing(Instant::now());
        assert!(pb.is_playing());
        assert!(pb.sampling_suppressed());

        // Paused but a seek's frame is still decoding → still suppressed (the
        // displayed frame can lag the playhead until it lands).
        pb.state = PlayState::Paused;
        pb.pending = Some(2);
        assert!(pb.sampling_suppressed());

        // Settled: paused and the awaited frame landed (`pending` cleared on swap)
        // → readout re-enabled.
        pb.pending = None;
        assert!(!pb.sampling_suppressed());
    }

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

    fn seq(lo: u32, hi: u32) -> Sequence {
        Sequence {
            frames: Vec::new(),
            range: (lo, hi),
            holes: Vec::new(),
        }
    }

    #[test]
    fn set_in_out_trims_around_the_playhead() {
        let mut p = Playback {
            in_point: 1,
            out_point: 10,
            current_frame: 4,
            ..Default::default()
        };
        let e0 = p.epoch;
        p.set_in(); // in moves up to the playhead
        assert_eq!((p.in_point, p.out_point), (4, 10));
        assert_ne!(p.epoch, e0, "a trim bumps the epoch");

        p.current_frame = 8;
        p.set_out(); // out moves down to the playhead
        assert_eq!((p.in_point, p.out_point), (4, 8));
    }

    #[test]
    fn set_in_never_passes_out_and_set_out_never_precedes_in() {
        let mut p = Playback {
            in_point: 3,
            out_point: 6,
            current_frame: 9, // pathological: clamp keeps in <= out
            ..Default::default()
        };
        p.set_in();
        assert_eq!(p.in_point, 6, "in clamped to out");

        let mut q = Playback {
            in_point: 5,
            out_point: 8,
            current_frame: 2,
            ..Default::default()
        };
        q.set_out();
        assert_eq!(q.out_point, 5, "out clamped to in");
    }

    #[test]
    fn reset_trim_restores_the_full_range() {
        let mut p = Playback {
            sequence: Some(seq(10, 50)),
            in_point: 20,
            out_point: 40,
            ..Default::default()
        };
        let e0 = p.epoch;
        p.reset_trim();
        assert_eq!((p.in_point, p.out_point), (10, 50));
        assert_ne!(p.epoch, e0);

        // No sequence -> no-op (and no panic).
        let mut none = Playback::default();
        none.reset_trim();
        assert_eq!((none.in_point, none.out_point), (0, 0));
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
