//! Lightweight RAM / GPU-memory sampler for the bottom status bar (#51).
//!
//! Keeps a throttled snapshot of floki's own memory footprint, system memory
//! pressure, and (on macOS) the wgpu Metal device's allocated/budget VRAM, so the
//! status bar can render a discrete readout without doing per-frame work.

use eframe::egui_wgpu::wgpu;
use std::time::{Duration, Instant};

/// How often the underlying OS / GPU queries actually run. Between refreshes the
/// status bar reads the cached [`Sample`].
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// One sampled snapshot of memory usage, all values in bytes.
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

/// Throttled sampler. Holds a `sysinfo::System` and reuses it across refreshes so
/// we only pay for the memory/process collectors we asked for.
pub struct ResourceMonitor {
    sys: sysinfo::System,
    pid: Option<sysinfo::Pid>,
    last: Option<Sample>,
    last_at: Option<Instant>,
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceMonitor {
    pub fn new() -> Self {
        Self {
            sys: sysinfo::System::new(),
            pid: sysinfo::get_current_pid().ok(),
            last: None,
            last_at: None,
        }
    }

    /// Return the current snapshot, refreshing the underlying queries at most once
    /// per [`REFRESH_INTERVAL`]. `device` is the live wgpu device used for the GPU
    /// memory query; pass the one from `RenderState`.
    pub fn sample(&mut self, device: &wgpu::Device) -> Sample {
        let now = Instant::now();
        let due = self
            .last_at
            .is_none_or(|t| now.duration_since(t) >= REFRESH_INTERVAL);
        if due {
            self.last = Some(self.refresh(device));
            self.last_at = Some(now);
        }
        // `due` is true on the first call, so `last` is always populated here.
        self.last.expect("sample populated on first refresh")
    }

    fn refresh(&mut self, device: &wgpu::Device) -> Sample {
        self.sys.refresh_memory();

        let proc_bytes = self
            .pid
            .map(|pid| {
                self.sys.refresh_processes_specifics(
                    sysinfo::ProcessesToUpdate::Some(&[pid]),
                    false,
                    sysinfo::ProcessRefreshKind::nothing().with_memory(),
                );
                self.sys.process(pid).map(|p| p.memory()).unwrap_or(0)
            })
            .unwrap_or(0);

        let (gpu_used, gpu_budget) = gpu_memory(device);

        Sample {
            proc_bytes,
            sys_used: self.sys.used_memory(),
            sys_total: self.sys.total_memory(),
            gpu_used,
            gpu_budget,
        }
    }
}

/// Per-process GPU memory via the Metal device backing the wgpu device.
///
/// Returns `(used, budget)` in bytes, or `(None, None)` when the value can't be
/// obtained (non-macOS, or the running backend is not Metal).
#[cfg(target_os = "macos")]
fn gpu_memory(device: &wgpu::Device) -> (Option<u64>, Option<u64>) {
    use objc2_metal::MTLDevice;

    // SAFETY: `as_hal` hands us the underlying Metal device only while the guard is
    // alive; we just read two scalar properties off it and copy them out. We never
    // retain the raw handle or mutate device state.
    unsafe {
        match device.as_hal::<wgpu::hal::api::Metal>() {
            Some(hal_device) => {
                let mtl = hal_device.raw_device();
                (
                    Some(mtl.currentAllocatedSize() as u64),
                    Some(mtl.recommendedMaxWorkingSetSize()),
                )
            }
            None => (None, None),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn gpu_memory(_device: &wgpu::Device) -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// Format a byte count as a compact human-readable string for the status bar:
/// `"2.1 GB"`, `"640 MB"`, `"512 KB"`. Uses 1024-based units.
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_picks_sensible_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2 * 1024), "2 KB");
        assert_eq!(fmt_bytes(640 * 1024 * 1024), "640 MB");
        // 2.1 GB rounds to one decimal.
        assert_eq!(fmt_bytes(2_254_857_830), "2.1 GB");
    }

    #[test]
    fn monitor_reports_finite_system_memory() {
        // No GPU needed here (sample() needs a wgpu device, exercised at runtime):
        // constructing the monitor and reading system memory must not panic and must
        // return a non-zero total on any host the tests run on.
        let _monitor = ResourceMonitor::new();
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        assert!(sys.total_memory() > 0, "total memory should be readable");
    }
}
