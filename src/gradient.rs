//! Shared multi-stop colour gradients and the diff-visualisation colormap.
//!
//! A [`Gradient`] is an ordered list of colour stops sampled by linear
//! interpolation. It is the single source of truth that replaced the old
//! hardcoded `heat_ramp` (CPU) / inline `heat` vector (GPU) for the Diff/Matte
//! view: the CPU path calls [`Gradient::sample`] directly and the GPU path
//! uploads [`Gradient::bake`] as a 256×1 colormap LUT texture.
//!
//! The module is deliberately decoupled from the viewer so the planned
//! customizable viewport background (gradient mode) can reuse the same
//! [`Gradient`] type, the same editor, and the same preset library.

use serde::{Deserialize, Serialize};

/// One colour stop: `t` in `[0,1]` along the gradient, `color` is display-space
/// (non-colour-managed) RGB in `[0,1]`.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct GradientStop {
    pub t: f32,
    pub color: [f32; 3],
}

impl GradientStop {
    pub const fn new(t: f32, color: [f32; 3]) -> Self {
        Self { t, color }
    }
}

/// An ordered multi-stop gradient. Stops are kept sorted by `t`; [`Self::sample`]
/// clamps out-of-range `t` to the endpoint colours and linearly interpolates
/// between the two surrounding stops.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Gradient {
    pub stops: Vec<GradientStop>,
}

impl Gradient {
    /// Build from raw stops, sorting by `t`. An empty list is tolerated and
    /// samples to black so a malformed persisted gradient can't panic.
    pub fn new(mut stops: Vec<GradientStop>) -> Self {
        stops.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        Self { stops }
    }

    /// Sample the gradient at `t`, linearly interpolating between the two
    /// surrounding stops. `t` is clamped to `[0,1]`; values past the first/last
    /// stop return that stop's colour (clamp-to-edge).
    pub fn sample(&self, t: f32) -> [f32; 3] {
        match self.stops.as_slice() {
            [] => [0.0, 0.0, 0.0],
            [only] => only.color,
            stops => {
                let t = t.clamp(0.0, 1.0);
                if t <= stops[0].t {
                    return stops[0].color;
                }
                if t >= stops[stops.len() - 1].t {
                    return stops[stops.len() - 1].color;
                }
                // Find the segment [lo, hi] containing t.
                let hi = stops.iter().position(|s| s.t >= t).unwrap();
                let lo = hi - 1;
                let (a, b) = (stops[lo], stops[hi]);
                let span = (b.t - a.t).max(f32::EPSILON);
                let f = (t - a.t) / span;
                [
                    a.color[0] + (b.color[0] - a.color[0]) * f,
                    a.color[1] + (b.color[1] - a.color[1]) * f,
                    a.color[2] + (b.color[2] - a.color[2]) * f,
                ]
            }
        }
    }

    /// Bake the gradient to an `n×1` row of RGBA8 texels (alpha = 255) for upload
    /// as a 1-D colormap LUT. Index `i` samples `t = i / (n-1)`.
    pub fn bake(&self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n * 4);
        let denom = (n.saturating_sub(1)).max(1) as f32;
        for i in 0..n {
            let c = self.sample(i as f32 / denom);
            out.push((c[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            out.push((c[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            out.push((c[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            out.push(255);
        }
        out
    }
}

/// Width of the baked colormap LUT (matches the GPU `256×1` colormap texture).
pub const COLORMAP_LUT_SIZE: usize = 256;

fn rgb8(r: u8, g: u8, b: u8) -> [f32; 3] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}

/// The colormap applied to the scalar diff magnitude. The built-in presets are
/// the false-colour ramps offered in the UI; [`Colormap::Custom`] carries a
/// user-authored [`Gradient`] (from the gradient editor / preset library).
#[derive(Clone, PartialEq, Default, Debug, Serialize, Deserialize)]
pub enum Colormap {
    /// Black → red → yellow → white. Default; reproduces the historical
    /// `heat_ramp` exactly so the diff view is unchanged out of the box.
    #[default]
    BlackBody,
    /// Plain absolute-difference grayscale (the "raw abs-diff" option).
    Grayscale,
    Turbo,
    Viridis,
    Magma,
    Inferno,
    /// A user-authored gradient.
    Custom(Gradient),
}

impl Colormap {
    /// The built-in presets, in UI order (excludes `Custom`).
    pub const PRESETS: [Colormap; 6] = [
        Colormap::BlackBody,
        Colormap::Grayscale,
        Colormap::Turbo,
        Colormap::Viridis,
        Colormap::Magma,
        Colormap::Inferno,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Colormap::BlackBody => "Black-body",
            Colormap::Grayscale => "Grayscale",
            Colormap::Turbo => "Turbo",
            Colormap::Viridis => "Viridis",
            Colormap::Magma => "Magma",
            Colormap::Inferno => "Inferno",
            Colormap::Custom(_) => "Custom",
        }
    }

    /// Resolve to the concrete [`Gradient`] used for sampling/baking. The
    /// perceptual presets (Turbo/Viridis/Magma/Inferno) are stored as a handful
    /// of anchor stops; linear interpolation between them is ample fidelity for a
    /// false-colour diff overlay.
    pub fn gradient(&self) -> Gradient {
        match self {
            Colormap::BlackBody => Gradient::new(vec![
                GradientStop::new(0.0, [0.0, 0.0, 0.0]),
                GradientStop::new(1.0 / 3.0, [1.0, 0.0, 0.0]),
                GradientStop::new(2.0 / 3.0, [1.0, 1.0, 0.0]),
                GradientStop::new(1.0, [1.0, 1.0, 1.0]),
            ]),
            Colormap::Grayscale => Gradient::new(vec![
                GradientStop::new(0.0, [0.0, 0.0, 0.0]),
                GradientStop::new(1.0, [1.0, 1.0, 1.0]),
            ]),
            Colormap::Turbo => Gradient::new(vec![
                GradientStop::new(0.0, rgb8(48, 18, 59)),
                GradientStop::new(0.143, rgb8(65, 105, 225)),
                GradientStop::new(0.286, rgb8(32, 196, 205)),
                GradientStop::new(0.429, rgb8(95, 247, 110)),
                GradientStop::new(0.571, rgb8(200, 242, 52)),
                GradientStop::new(0.714, rgb8(251, 167, 42)),
                GradientStop::new(0.857, rgb8(231, 73, 17)),
                GradientStop::new(1.0, rgb8(122, 4, 3)),
            ]),
            Colormap::Viridis => Gradient::new(vec![
                GradientStop::new(0.0, rgb8(68, 1, 84)),
                GradientStop::new(0.13, rgb8(72, 40, 120)),
                GradientStop::new(0.25, rgb8(62, 74, 137)),
                GradientStop::new(0.38, rgb8(49, 104, 142)),
                GradientStop::new(0.5, rgb8(38, 130, 142)),
                GradientStop::new(0.63, rgb8(31, 158, 137)),
                GradientStop::new(0.75, rgb8(53, 183, 121)),
                GradientStop::new(0.88, rgb8(110, 206, 88)),
                GradientStop::new(1.0, rgb8(253, 231, 37)),
            ]),
            Colormap::Magma => Gradient::new(vec![
                GradientStop::new(0.0, rgb8(0, 0, 4)),
                GradientStop::new(0.13, rgb8(24, 15, 62)),
                GradientStop::new(0.25, rgb8(69, 16, 119)),
                GradientStop::new(0.38, rgb8(114, 31, 129)),
                GradientStop::new(0.5, rgb8(159, 47, 127)),
                GradientStop::new(0.63, rgb8(205, 64, 113)),
                GradientStop::new(0.75, rgb8(241, 96, 93)),
                GradientStop::new(0.88, rgb8(253, 149, 103)),
                GradientStop::new(1.0, rgb8(252, 253, 191)),
            ]),
            Colormap::Inferno => Gradient::new(vec![
                GradientStop::new(0.0, rgb8(0, 0, 4)),
                GradientStop::new(0.13, rgb8(27, 12, 65)),
                GradientStop::new(0.25, rgb8(74, 12, 107)),
                GradientStop::new(0.38, rgb8(120, 28, 109)),
                GradientStop::new(0.5, rgb8(165, 44, 96)),
                GradientStop::new(0.63, rgb8(207, 68, 70)),
                GradientStop::new(0.75, rgb8(237, 105, 37)),
                GradientStop::new(0.88, rgb8(251, 155, 6)),
                GradientStop::new(1.0, rgb8(252, 255, 164)),
            ]),
            Colormap::Custom(g) => g.clone(),
        }
    }
}

/// How the per-pixel diff magnitude is reduced to the scalar that drives the
/// colormap. Encoded for the GPU via [`Self::as_u32`]; the `is_diff_mode` branch
/// in `gpu/shader.wgsl` must use the same values.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub enum DiffMetric {
    /// `max(|Δr|, |Δg|, |Δb|)` — the historical metric.
    #[default]
    MaxChannel,
    /// Rec.709-luminance-weighted magnitude of the difference.
    Luminance,
    /// No reduction: each channel's gained `|Δ|` is shown directly as R/G/B
    /// (bypasses the colormap). Useful to see *which* channel differs.
    PerChannelRGB,
}

impl DiffMetric {
    /// Integer encoding shared with the GPU (`MaxChannel=0`, `Luminance=1`,
    /// `PerChannelRGB=2`). Single source of truth for the shader mapping.
    pub fn as_u32(self) -> u32 {
        match self {
            DiffMetric::MaxChannel => 0,
            DiffMetric::Luminance => 1,
            DiffMetric::PerChannelRGB => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DiffMetric::MaxChannel => "Max Channel",
            DiffMetric::Luminance => "Luminance",
            DiffMetric::PerChannelRGB => "Per-channel RGB",
        }
    }

    pub const ALL: [DiffMetric; 3] = [
        DiffMetric::MaxChannel,
        DiffMetric::Luminance,
        DiffMetric::PerChannelRGB,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: [f32; 3], b: [f32; 3]) {
        for i in 0..3 {
            assert!((a[i] - b[i]).abs() < 1e-4, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn sample_endpoints_and_clamp() {
        let g = Gradient::new(vec![
            GradientStop::new(0.0, [0.0, 0.0, 0.0]),
            GradientStop::new(1.0, [1.0, 1.0, 1.0]),
        ]);
        approx(g.sample(-1.0), [0.0, 0.0, 0.0]);
        approx(g.sample(0.0), [0.0, 0.0, 0.0]);
        approx(g.sample(1.0), [1.0, 1.0, 1.0]);
        approx(g.sample(2.0), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn sample_midpoint_interpolates() {
        let g = Gradient::new(vec![
            GradientStop::new(0.0, [0.0, 0.0, 0.0]),
            GradientStop::new(1.0, [1.0, 0.5, 0.0]),
        ]);
        approx(g.sample(0.5), [0.5, 0.25, 0.0]);
    }

    #[test]
    fn single_stop_is_constant() {
        let g = Gradient::new(vec![GradientStop::new(0.4, [0.2, 0.3, 0.4])]);
        approx(g.sample(0.0), [0.2, 0.3, 0.4]);
        approx(g.sample(1.0), [0.2, 0.3, 0.4]);
    }

    #[test]
    fn empty_gradient_is_black() {
        let g = Gradient::new(vec![]);
        approx(g.sample(0.5), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn new_sorts_unordered_stops() {
        let g = Gradient::new(vec![
            GradientStop::new(1.0, [1.0, 1.0, 1.0]),
            GradientStop::new(0.0, [0.0, 0.0, 0.0]),
        ]);
        approx(g.sample(0.25), [0.25, 0.25, 0.25]);
    }

    #[test]
    fn blackbody_matches_legacy_heat_ramp() {
        // Legacy ramp: (clamp(m*3), clamp(m*3-1), clamp(m*3-2)).
        let g = Colormap::BlackBody.gradient();
        for i in 0..=20 {
            let m = i as f32 / 20.0;
            let legacy = [
                (m * 3.0).clamp(0.0, 1.0),
                (m * 3.0 - 1.0).clamp(0.0, 1.0),
                (m * 3.0 - 2.0).clamp(0.0, 1.0),
            ];
            approx(g.sample(m), legacy);
        }
    }

    #[test]
    fn bake_length_and_alpha() {
        let baked = Colormap::Viridis.gradient().bake(COLORMAP_LUT_SIZE);
        assert_eq!(baked.len(), COLORMAP_LUT_SIZE * 4);
        assert!(baked.chunks(4).all(|px| px[3] == 255));
        // Endpoints match the gradient's first/last stop.
        assert_eq!(&baked[0..3], &[68, 1, 84]);
    }
}
