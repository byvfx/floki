//! Customizable viewport background (issue #18).
//!
//! The transparency backdrop — shown wherever the image's premultiplied alpha is
//! below 1 — used to be a fixed 16px grey checkerboard hardcoded in five places
//! (the main GPU shader, the OCIO blit pass, and three CPU composite paths). This
//! module centralises the *configuration* (mode + colours + gradient) and the CPU
//! *sampling* so all paths agree.
//!
//! Like the diff colormap, the background is **not** colour-managed: in the
//! non-OCIO path it is composited in scene-linear space (then tone-mapped with the
//! image, exactly as the old checker was); in the OCIO path it is composited in
//! display space by the blit pass. Colours here are therefore linear values; the
//! defaults reproduce the historical `0.1 / 0.2` grey checker exactly.
//!
//! The gradient mode reuses [`crate::gradient::Gradient`] (and its editor / preset
//! library), which is why the gradient work for the diff heat map (#15) was built
//! as a shared module.

use crate::gradient::Gradient;
use serde::{Deserialize, Serialize};

/// Which backdrop is composited behind transparent pixels. Encoded for the GPU
/// via [`Self::as_u32`]; the `background_color` helpers in `gpu/shader.wgsl` and
/// the blit shader must use the same values.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub enum BackgroundMode {
    #[default]
    Checkerboard,
    Solid,
    Gradient,
}

impl BackgroundMode {
    /// Integer encoding shared with the GPU (`Checkerboard=0`, `Solid=1`,
    /// `Gradient=2`). Single source of truth for the shader mapping.
    pub fn as_u32(self) -> u32 {
        match self {
            BackgroundMode::Checkerboard => 0,
            BackgroundMode::Solid => 1,
            BackgroundMode::Gradient => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BackgroundMode::Checkerboard => "Checkerboard",
            BackgroundMode::Solid => "Solid",
            BackgroundMode::Gradient => "Gradient",
        }
    }

    pub const ALL: [BackgroundMode; 3] = [
        BackgroundMode::Checkerboard,
        BackgroundMode::Solid,
        BackgroundMode::Gradient,
    ];
}

/// Full viewport-background configuration. Linear-space colours; see the module
/// docs for the colour-management rationale.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Background {
    pub mode: BackgroundMode,
    /// Checkerboard cell colours (the two alternating squares) and cell size in
    /// screen pixels.
    pub checker_dark: [f32; 3],
    pub checker_light: [f32; 3],
    pub checker_size: f32,
    /// Solid-mode fill colour.
    pub solid: [f32; 3],
    /// Gradient-mode ramp + its direction in degrees (0 = left→right, 90 =
    /// top→bottom), spanning the composited rect corner-to-corner.
    pub gradient: Gradient,
    pub gradient_angle: f32,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            mode: BackgroundMode::Checkerboard,
            checker_dark: [0.1, 0.1, 0.1],
            checker_light: [0.2, 0.2, 0.2],
            checker_size: 16.0,
            solid: [0.1, 0.1, 0.1],
            gradient: default_gradient(),
            gradient_angle: 90.0,
        }
    }
}

/// A neutral dark→light grey vertical ramp, used as the default gradient and as
/// the seed when the user first switches to gradient mode.
pub fn default_gradient() -> Gradient {
    use crate::gradient::GradientStop;
    Gradient::new(vec![
        GradientStop::new(0.0, [0.05, 0.05, 0.05]),
        GradientStop::new(1.0, [0.3, 0.3, 0.3]),
    ])
}

impl Background {
    /// Map a gradient direction (degrees) and normalized coords `uv ∈ [0,1]²` to a
    /// position `t ∈ [0,1]` along the ramp, spanning the rect corner-to-corner so
    /// the full gradient is always visible regardless of angle. Shared formula with
    /// the GPU shaders (keep in lockstep).
    pub fn gradient_t(angle_deg: f32, u: f32, v: f32) -> f32 {
        let a = angle_deg.to_radians();
        let (dx, dy) = (a.cos(), a.sin());
        let pmin = dx.min(0.0) + dy.min(0.0);
        let pmax = dx.max(0.0) + dy.max(0.0);
        let p = u * dx + v * dy;
        ((p - pmin) / (pmax - pmin).max(1e-4)).clamp(0.0, 1.0)
    }

    /// The linear background colour at pixel `(px, py)` within a `w × h` rect.
    /// Used by every CPU composite path; the GPU paths reimplement the same logic
    /// in WGSL.
    pub fn sample_linear(&self, px: f32, py: f32, w: f32, h: f32) -> [f32; 3] {
        match self.mode {
            BackgroundMode::Solid => self.solid,
            BackgroundMode::Checkerboard => {
                let size = self.checker_size.max(1.0);
                let cx = (px / size).floor() as i64;
                let cy = (py / size).floor() as i64;
                if (cx + cy).rem_euclid(2) == 0 {
                    self.checker_dark
                } else {
                    self.checker_light
                }
            }
            BackgroundMode::Gradient => {
                let u = (px / w.max(1.0)).clamp(0.0, 1.0);
                let v = (py / h.max(1.0)).clamp(0.0, 1.0);
                self.gradient
                    .sample(Self::gradient_t(self.gradient_angle, u, v))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_reproduces_legacy_checker() {
        let bg = Background::default();
        // (0,0) cell is "dark" 0.1; the neighbouring cell is "light" 0.2 — matching
        // the old `((x/16)+(y/16)) % 2 == 0 -> 0.1 else 0.2`.
        assert_eq!(bg.sample_linear(0.0, 0.0, 100.0, 100.0), [0.1, 0.1, 0.1]);
        assert_eq!(bg.sample_linear(16.0, 0.0, 100.0, 100.0), [0.2, 0.2, 0.2]);
        assert_eq!(bg.sample_linear(16.0, 16.0, 100.0, 100.0), [0.1, 0.1, 0.1]);
    }

    #[test]
    fn solid_is_constant() {
        let bg = Background {
            mode: BackgroundMode::Solid,
            solid: [0.18, 0.2, 0.22],
            ..Default::default()
        };
        assert_eq!(bg.sample_linear(3.0, 7.0, 50.0, 50.0), [0.18, 0.2, 0.22]);
        assert_eq!(bg.sample_linear(40.0, 40.0, 50.0, 50.0), [0.18, 0.2, 0.22]);
    }

    #[test]
    fn gradient_spans_corner_to_corner() {
        let bg = Background {
            mode: BackgroundMode::Gradient,
            gradient_angle: 90.0, // top->bottom
            ..Default::default()
        };
        let top = bg.sample_linear(50.0, 0.0, 100.0, 100.0);
        let bot = bg.sample_linear(50.0, 100.0, 100.0, 100.0);
        // Vertical ramp: bottom strictly brighter than top.
        assert!(bot[0] > top[0]);
        assert!((top[0] - 0.05).abs() < 1e-3);
        assert!((bot[0] - 0.3).abs() < 1e-3);
    }

    #[test]
    fn gradient_angle_zero_is_horizontal() {
        // 0° = left→right: t depends on x, not y.
        let t_left = Background::gradient_t(0.0, 0.0, 0.5);
        let t_right = Background::gradient_t(0.0, 1.0, 0.5);
        assert!((t_left - 0.0).abs() < 1e-4);
        assert!((t_right - 1.0).abs() < 1e-4);
        // Moving in y does nothing at 0°.
        assert!(
            (Background::gradient_t(0.0, 0.3, 0.0) - Background::gradient_t(0.0, 0.3, 1.0)).abs()
                < 1e-4
        );
    }
}
