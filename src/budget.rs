//! Byte-budget math for the playback ring cache (#56).
//!
//! Pure, side-effect-free helpers that turn a [`Sample`] of current memory usage
//! plus a frame's dimensions / decoded size into how many frames may be held at
//! each cache tier. The two budgets bind different tiers from different sources:
//! VRAM bounds the T2 GPU-texture ring, system RAM bounds the T1 CPU-frame ring.
//!
//! See `docs/playback/memory-contract.md` for the full contract. Callers
//! recompute periodically (the live `Sample` shifts as textures and frames are
//! built and evicted), so each function reports how many frames fit the budget
//! that *remains* after current usage — return `0` when not even one fits, which
//! is the signal to degrade to decode-on-demand rather than crash.

use crate::resource_monitor::Sample;

/// Percent of the reported VRAM working-set budget the T2 ring may claim,
/// leaving headroom for the rest of the app and allocator slop. Conservative;
/// to be exposed in the tools window later. wgpu can *abort the process* on a
/// GPU OOM, so we stay well under the reported budget proactively.
pub const VRAM_HEADROOM_PCT: u64 = 80;

/// Percent of *currently-free* system RAM the T1 ring may claim, leaving the
/// rest as headroom for the OS, other apps, and floki's own non-cache memory.
///
/// Sized from *free* RAM rather than total deliberately: a "% of total minus
/// used" model collapses to near-zero the moment other apps push total usage
/// past the ceiling — on a loaded workstation (e.g. 80+ GB held by other DCC
/// apps) the cache cratered to ~3 frames while tens of GB sat physically free.
/// Sizing from free RAM scales the ring with what is actually available and
/// degrades smoothly under external pressure instead of falling off a cliff.
pub const RAM_FREE_PCT: u64 = 60;

/// Conservative VRAM budget (bytes) assumed when the platform can't report a GPU
/// working-set size (`Sample::gpu_budget == None` — non-Metal backends). 1 GiB
/// keeps a handful of 4K textures resident without risking an OOM on unknown
/// hardware. Playback still runs off-Metal; it just caps the texture ring lower.
pub const FALLBACK_VRAM_BUDGET: u64 = 1 << 30;

/// VRAM one T2 frame texture occupies: `Rgba32Float` is 16 bytes/pixel, active
/// layer only.
#[must_use]
pub fn t2_frame_bytes(width: usize, height: usize) -> u64 {
    // Saturating so a pathological/huge dimension can't wrap to a *small* size
    // (which would over-allocate); an overflow becomes "too big to fit" -> 0 frames.
    (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(16)
}

/// Apply an integer-percent headroom to a budget. Integer math keeps results
/// deterministic (no float rounding surprises) and is exact for realistic
/// memory sizes.
fn with_headroom(total: u64, pct: u64) -> u64 {
    total.saturating_mul(pct) / 100
}

/// Max number of T2 GPU textures that fit the VRAM budget remaining after
/// current allocation. One texture per frame, active layer only.
///
/// Uses `Sample::gpu_budget` when available, else [`FALLBACK_VRAM_BUDGET`].
/// Returns `0` if not even one frame fits (caller disables pre-upload and
/// decodes on demand — see the failure modes in the memory contract).
#[must_use]
pub fn max_t2(sample: &Sample, width: usize, height: usize) -> usize {
    let per_frame = t2_frame_bytes(width, height);
    if per_frame == 0 {
        return 0;
    }
    let total = sample.gpu_budget.unwrap_or(FALLBACK_VRAM_BUDGET);
    let used = sample.gpu_used.unwrap_or(0);
    let available = with_headroom(total, VRAM_HEADROOM_PCT).saturating_sub(used);
    usize::try_from(available / per_frame).unwrap_or(usize::MAX)
}

/// Max number of T1 CPU frames (full `ExrData`, all layers) that fit a
/// [`RAM_FREE_PCT`] slice of *currently-free* system RAM, given one frame's
/// measured size (`ExrData::approx_bytes()`). Sequences are homogeneous, so a
/// single measurement sizes the ring.
///
/// Free RAM is `sys_total - sys_used`; claiming a fraction of it (rather than a
/// fraction of *total* minus used) keeps the ring usable when external apps hold
/// most of the machine — see [`RAM_FREE_PCT`] for why.
///
/// Returns `0` if not even one frame fits (caller refuses sequence mode and
/// shows a single frame — see the memory contract).
#[must_use]
pub fn max_t1(sample: &Sample, frame_bytes: usize) -> usize {
    if frame_bytes == 0 {
        return 0;
    }
    let free = sample.sys_total.saturating_sub(sample.sys_used);
    let available = with_headroom(free, RAM_FREE_PCT);
    usize::try_from(available / frame_bytes as u64).unwrap_or(usize::MAX)
}

/// Total T1 ring capacity (frames) for a live cache: like [`max_t1`] but counts
/// the cache's own resident bytes (`cache_bytes`) as *not* using budget, so the
/// figure is the total the ring may hold rather than how many more would fit.
/// This keeps the capacity stable as the ring fills (otherwise `sys_used` would
/// include the cache and the budget would chase its own tail), while still
/// shrinking when *other* memory pressure rises. Recompute periodically.
#[must_use]
pub fn t1_capacity(sample: &Sample, frame_bytes: usize, cache_bytes: u64) -> usize {
    let adjusted = Sample {
        sys_used: sample.sys_used.saturating_sub(cache_bytes),
        ..*sample
    };
    max_t1(&adjusted, frame_bytes)
}

/// Cap an auto-derived T1 frame count by an optional **user-assigned RAM budget**
/// (bytes). The user budget is a *ceiling*, not an override: the effective cap is
/// the smaller of the auto (free-RAM) figure and what the user budget affords, so
/// a generous setting can never push the ring past free RAM and risk an OOM,
/// while a small setting deliberately constrains it — useful for capping RAM on a
/// shared workstation and for dogfooding the eviction/degradation paths on a
/// machine (e.g. Apple unified memory) that otherwise never feels the pressure.
///
/// `None` (or a zero/degenerate frame size) leaves the auto figure untouched.
#[must_use]
pub fn apply_user_ram_cap(
    auto_frames: usize,
    user_budget: Option<u64>,
    frame_bytes: usize,
) -> usize {
    match user_budget {
        Some(budget) if frame_bytes > 0 => {
            let user_frames = usize::try_from(budget / frame_bytes as u64).unwrap_or(usize::MAX);
            auto_frames.min(user_frames)
        }
        _ => auto_frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        sys_total: u64,
        sys_used: u64,
        gpu_budget: Option<u64>,
        gpu_used: Option<u64>,
    ) -> Sample {
        Sample {
            proc_bytes: 0,
            sys_used,
            sys_total,
            gpu_used,
            gpu_budget,
        }
    }

    #[test]
    fn t2_frame_bytes_is_16_per_pixel() {
        assert_eq!(t2_frame_bytes(1920, 1080), 1920 * 1080 * 16);
        // A 4K frame is ~126.5 MiB.
        assert_eq!(t2_frame_bytes(3840, 2160), 132_710_400);
        assert_eq!(t2_frame_bytes(0, 1080), 0);
    }

    #[test]
    fn max_t2_divides_headroomed_budget_by_frame() {
        // 2 GB budget, nothing used, 80% headroom = 1.6 GB; 1000x1000 = 16 MB.
        let s = sample(0, 0, Some(2_000_000_000), Some(0));
        assert_eq!(max_t2(&s, 1000, 1000), 100);
    }

    #[test]
    fn max_t2_subtracts_current_allocation() {
        // 1.6 GB headroomed, 800 MB already allocated -> 800 MB free / 16 MB = 50.
        let s = sample(0, 0, Some(2_000_000_000), Some(800_000_000));
        assert_eq!(max_t2(&s, 1000, 1000), 50);
    }

    #[test]
    fn max_t2_uses_fallback_budget_when_gpu_budget_unknown() {
        // Off-Metal: gpu_budget None -> 1 GiB * 80% = 858_993_459; /16 MB = 53.
        let s = sample(0, 0, None, None);
        assert_eq!(max_t2(&s, 1000, 1000), 53);
    }

    #[test]
    fn max_t2_zero_when_nothing_fits() {
        // Budget fully consumed.
        let s = sample(0, 0, Some(2_000_000_000), Some(2_000_000_000));
        assert_eq!(max_t2(&s, 1000, 1000), 0);
        // Degenerate frame size.
        let s2 = sample(0, 0, Some(2_000_000_000), Some(0));
        assert_eq!(max_t2(&s2, 0, 1000), 0);
    }

    #[test]
    fn max_t1_takes_a_slice_of_free_ram() {
        // 20 GB total, 4 GB used -> 16 GB free; 60% of free = 9.6 GB;
        // 1 GB/frame -> 9 frames.
        let s = sample(20_000_000_000, 4_000_000_000, None, None);
        assert_eq!(max_t1(&s, 1_000_000_000), 9);
    }

    #[test]
    fn max_t1_sizes_from_free_ram_not_a_total_ceiling() {
        // Regression for the loaded-workstation cliff: 128 GB machine with
        // ~89.7 GB held by *other* apps still has ~38.2 GB physically free.
        // The old "70% of total - used" model returned ~0 here (89.7 GB is past
        // the 89.5 GB ceiling), collapsing the ring to a handful of frames.
        // Sizing from free RAM keeps a real read-ahead window: 38.2 GB free *
        // 60% = 22.92 GB; 1.3 GB/frame -> 17 frames.
        let s = sample(127_900_000_000, 89_700_000_000, None, None);
        assert_eq!(max_t1(&s, 1_300_000_000), 17);
    }

    #[test]
    fn t1_capacity_is_stable_as_the_cache_fills() {
        // 20 GB total; 1 GB/frame. With 2 GB of *other* usage, free is 18 GB
        // (the cache's own bytes are added back so they don't count against it);
        // 60% of 18 GB = 10.8 GB -> 10 frames — unchanged whether the cache
        // currently holds 0 or 5 of those frames.
        let frame = 1_000_000_000usize;
        let empty = sample(20_000_000_000, 2_000_000_000, None, None);
        assert_eq!(t1_capacity(&empty, frame, 0), 10);
        let half_full = sample(20_000_000_000, 2_000_000_000 + 5 * frame as u64, None, None);
        assert_eq!(
            t1_capacity(&half_full, frame, 5 * frame as u64),
            10,
            "capacity doesn't chase the cache's own growth"
        );
    }

    #[test]
    fn max_t1_zero_when_nothing_fits() {
        // Almost no free RAM: 0.5 GB free, 60% = 0.3 GB < one 1 GB frame -> 0.
        let s = sample(20_000_000_000, 19_500_000_000, None, None);
        assert_eq!(max_t1(&s, 1_000_000_000), 0);
        // Degenerate frame size.
        let s2 = sample(20_000_000_000, 0, None, None);
        assert_eq!(max_t1(&s2, 0), 0);
    }

    #[test]
    fn user_ram_cap_is_a_ceiling_never_an_override() {
        let frame = 1_000_000_000usize; // 1 GB/frame
        // A small user budget deliberately constrains a generous auto figure:
        // 4 GB / 1 GB = 4 frames < the 30 the machine could hold.
        assert_eq!(
            apply_user_ram_cap(30, Some(4_000_000_000), frame),
            4,
            "user budget caps below the auto figure"
        );
        // A generous user budget can't push *past* the auto (free-RAM) figure —
        // that's what protects against OOM.
        assert_eq!(
            apply_user_ram_cap(10, Some(64_000_000_000), frame),
            10,
            "user budget never exceeds the auto free-RAM cap"
        );
        // No user budget → auto untouched.
        assert_eq!(apply_user_ram_cap(12, None, frame), 12);
        // Degenerate frame size → auto untouched (no divide).
        assert_eq!(apply_user_ram_cap(12, Some(4_000_000_000), 0), 12);
    }
}
