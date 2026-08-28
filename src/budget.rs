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

/// One sampled snapshot of memory usage, all values in bytes.
///
/// Lives here rather than beside the sampler that produces it
/// ([`crate::resource_monitor::ResourceMonitor`]): the sampler needs a live
/// `wgpu::Device` for the Metal VRAM query, and pulling that dependency in
/// through the type would make this whole module — and every caller that only
/// wants to *do the arithmetic* — reachable only with a GPU. As a plain POD it
/// can be constructed by hand, which is what lets the cap arithmetic in
/// `ExrApp::tick_budgets_t1` be tested headlessly (#288).
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Resident set size of this process.
    pub proc_bytes: u64,
    /// System-wide memory in use.
    pub sys_used: u64,
    /// Total system memory.
    pub sys_total: u64,
    /// GPU memory currently allocated by this process. `None` when unavailable
    /// (non-macOS, or the active backend is not Metal).
    pub gpu_used: Option<u64>,
    /// Recommended GPU working-set budget. `None` when unavailable.
    pub gpu_budget: Option<u64>,
}

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

/// VRAM bytes the T2 ring(s) may claim: the headroomed working-set budget minus
/// what is already allocated. Uses `Sample::gpu_budget` when available, else
/// [`FALLBACK_VRAM_BUDGET`]. This is the pool the caller splits across the A and
/// B rings in locked-step compare (#166) — compute it once, then slice it.
#[must_use]
pub fn vram_available(sample: &Sample) -> u64 {
    let total = sample.gpu_budget.unwrap_or(FALLBACK_VRAM_BUDGET);
    let used = sample.gpu_used.unwrap_or(0);
    with_headroom(total, VRAM_HEADROOM_PCT).saturating_sub(used)
}

/// How many T2 textures of the given dimensions fit `available` VRAM bytes. One
/// texture per frame, active layer only. Returns `0` for a degenerate frame size
/// or when not even one fits (caller disables pre-upload and decodes on demand).
#[must_use]
pub fn frames_for(available: u64, width: usize, height: usize) -> usize {
    let per_frame = t2_frame_bytes(width, height);
    if per_frame == 0 {
        return 0;
    }
    usize::try_from(available / per_frame).unwrap_or(usize::MAX)
}

/// The T1 **byte** budget: how many bytes the ring may hold (#232).
///
/// This is the primary figure — the one eviction enforces — and every T1 frame
/// count is derived from it via [`frames_in`], so a count and a byte bound can
/// never disagree about the same budget.
///
/// `cache_bytes` (the ring's measured residency, #230) is added back to free RAM
/// so the budget does not chase its own tail as the ring fills, while still
/// shrinking when *other* memory pressure rises. Recompute periodically.
///
/// `user_budget` is a **ceiling, not an override**: the result is the smaller of
/// the free-RAM slice and the user's setting, so a generous setting can never
/// push the ring past free RAM and risk an OOM, while a small one deliberately
/// constrains it — useful for capping RAM on a shared workstation and for
/// dogfooding the eviction/degradation paths on a machine (e.g. Apple unified
/// memory) that otherwise never feels the pressure.
#[must_use]
pub fn t1_budget_bytes(sample: &Sample, cache_bytes: u64, user_budget: Option<u64>) -> u64 {
    let free = sample
        .sys_total
        .saturating_sub(sample.sys_used.saturating_sub(cache_bytes));
    let available = with_headroom(free, RAM_FREE_PCT);
    user_budget.map_or(available, |u| available.min(u))
}

/// Whole frames of `frame_bytes` that fit in a byte budget. `0` for a degenerate
/// frame size, and `0` when not even one fits (the caller refuses sequence mode
/// and shows a single frame — see the memory contract).
#[must_use]
pub fn frames_in(budget_bytes: u64, frame_bytes: usize) -> usize {
    if frame_bytes == 0 {
        return 0;
    }
    usize::try_from(budget_bytes / frame_bytes as u64).unwrap_or(usize::MAX)
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
    fn vram_available_divides_headroomed_budget_by_frame() {
        // 2 GB budget, nothing used, 80% headroom = 1.6 GB; 1000x1000 = 16 MB.
        let s = sample(0, 0, Some(2_000_000_000), Some(0));
        assert_eq!(frames_for(vram_available(&s), 1000, 1000), 100);
    }

    #[test]
    fn vram_available_subtracts_current_allocation() {
        // 1.6 GB headroomed, 800 MB already allocated -> 800 MB free / 16 MB = 50.
        let s = sample(0, 0, Some(2_000_000_000), Some(800_000_000));
        assert_eq!(vram_available(&s), 800_000_000);
        assert_eq!(frames_for(vram_available(&s), 1000, 1000), 50);
    }

    #[test]
    fn vram_available_uses_fallback_budget_when_gpu_budget_unknown() {
        // Off-Metal: gpu_budget None -> 1 GiB * 80% = 858_993_459; /16 MB = 53.
        let s = sample(0, 0, None, None);
        assert_eq!(vram_available(&s), 858_993_459);
        assert_eq!(frames_for(vram_available(&s), 1000, 1000), 53);
    }

    #[test]
    fn frames_for_zero_when_nothing_fits() {
        // Budget fully consumed.
        let s = sample(0, 0, Some(2_000_000_000), Some(2_000_000_000));
        assert_eq!(vram_available(&s), 0);
        assert_eq!(frames_for(vram_available(&s), 1000, 1000), 0);
        // Degenerate frame size.
        let s2 = sample(0, 0, Some(2_000_000_000), Some(0));
        assert_eq!(frames_for(vram_available(&s2), 0, 1000), 0);
    }

    #[test]
    fn frames_for_splits_evenly_across_equal_dims() {
        // 1.6 GB available; 1000x1000 = 16 MB -> 100 frames whole, 50 each half.
        let s = sample(0, 0, Some(2_000_000_000), Some(0));
        let avail = vram_available(&s);
        assert_eq!(frames_for(avail, 1000, 1000), 100);
        assert_eq!(frames_for(avail / 2, 1000, 1000), 50);
    }

    #[test]
    fn frames_for_gives_a_larger_slot_proportionally_fewer() {
        // Same half-budget, B twice A's area -> B fits ~half as many as A.
        let s = sample(0, 0, Some(2_000_000_000), Some(0));
        let half = vram_available(&s) / 2;
        let a = frames_for(half, 1000, 1000); // 16 MB/frame
        let b = frames_for(half, 1000, 2000); // 32 MB/frame
        assert_eq!(a, 50);
        assert_eq!(b, 25);
    }

    #[test]
    fn frames_for_zero_for_degenerate_or_empty_budget() {
        assert_eq!(frames_for(1_000_000_000, 0, 1000), 0);
        assert_eq!(frames_for(0, 1000, 1000), 0);
    }

    #[test]
    fn t1_budget_takes_a_slice_of_free_ram() {
        // 20 GB total, 4 GB used -> 16 GB free; 60% of free = 9.6 GB;
        // 1 GB/frame -> 9 frames.
        let s = sample(20_000_000_000, 4_000_000_000, None, None);
        assert_eq!(frames_in(t1_budget_bytes(&s, 0, None), 1_000_000_000), 9);
    }

    #[test]
    fn t1_budget_sizes_from_free_ram_not_a_total_ceiling() {
        // Regression for the loaded-workstation cliff: 128 GB machine with
        // ~89.7 GB held by *other* apps still has ~38.2 GB physically free.
        // The old "70% of total - used" model returned ~0 here (89.7 GB is past
        // the 89.5 GB ceiling), collapsing the ring to a handful of frames.
        // Sizing from free RAM keeps a real read-ahead window: 38.2 GB free *
        // 60% = 22.92 GB; 1.3 GB/frame -> 17 frames.
        let s = sample(127_900_000_000, 89_700_000_000, None, None);
        assert_eq!(frames_in(t1_budget_bytes(&s, 0, None), 1_300_000_000), 17);
    }

    #[test]
    fn t1_budget_is_stable_as_the_cache_fills() {
        // 20 GB total; 1 GB/frame. With 2 GB of *other* usage, free is 18 GB
        // (the cache's own bytes are added back so they don't count against it);
        // 60% of 18 GB = 10.8 GB -> 10 frames — unchanged whether the cache
        // currently holds 0 or 5 of those frames.
        let frame = 1_000_000_000usize;
        let empty = sample(20_000_000_000, 2_000_000_000, None, None);
        assert_eq!(frames_in(t1_budget_bytes(&empty, 0, None), frame), 10);
        let half_full = sample(20_000_000_000, 2_000_000_000 + 5 * frame as u64, None, None);
        assert_eq!(
            frames_in(t1_budget_bytes(&half_full, 5 * frame as u64, None), frame),
            10,
            "capacity doesn't chase the cache's own growth"
        );
    }

    #[test]
    fn t1_budget_holds_nothing_when_nothing_fits() {
        // Almost no free RAM: 0.5 GB free, 60% = 0.3 GB < one 1 GB frame -> 0.
        let s = sample(20_000_000_000, 19_500_000_000, None, None);
        assert_eq!(frames_in(t1_budget_bytes(&s, 0, None), 1_000_000_000), 0);
        // Degenerate frame size.
        let s2 = sample(20_000_000_000, 0, None, None);
        assert_eq!(frames_in(t1_budget_bytes(&s2, 0, None), 0), 0);
    }

    #[test]
    fn user_ram_cap_is_a_ceiling_never_an_override() {
        // 20 GB total, 2 GB used → 60% of 18 GB free = 10.8 GB auto.
        let s = sample(20_000_000_000, 2_000_000_000, None, None);
        let auto = t1_budget_bytes(&s, 0, None);
        assert_eq!(auto, 10_800_000_000);

        // A small user budget deliberately constrains the auto figure.
        assert_eq!(
            t1_budget_bytes(&s, 0, Some(4_000_000_000)),
            4_000_000_000,
            "user budget caps below the auto figure"
        );
        // A generous one can't push *past* it — that's what protects against OOM.
        assert_eq!(
            t1_budget_bytes(&s, 0, Some(64_000_000_000)),
            auto,
            "user budget never exceeds the auto free-RAM cap"
        );
        // No user budget → auto untouched.
        assert_eq!(t1_budget_bytes(&s, 0, None), auto);
    }

    #[test]
    fn frames_in_is_the_only_bytes_to_count_conversion() {
        // #232: every T1 frame count is this function over a byte budget, so a
        // count and a byte bound derived from the same budget cannot disagree.
        assert_eq!(frames_in(10_800_000_000, 1_000_000_000), 10);
        assert_eq!(frames_in(999, 1_000), 0, "not even one fits");
        assert_eq!(frames_in(1_000, 0), 0, "degenerate frame size, no divide");
    }
}
