//! Pure, GPU-free tone/color math.
//!
//! These functions are the CPU-side reference for the per-pixel work that
//! `gpu/shader.wgsl` performs on the GPU, and they back the viewer's CPU
//! fallback path. Keeping them here — dependency-free and side-effect-free —
//! makes the math trivially unit-testable without a wgpu device.

/// Convert exposure stops (EV) to a linear multiplier: `2^ev`.
/// EV 0 → 1.0, EV +1 → 2.0, EV −1 → 0.5.
pub fn exposure_to_multiplier(ev: f32) -> f32 {
    2.0_f32.powf(ev)
}

/// Apply display gamma to a value: `value^(1/gamma)`.
///
/// Non-positive inputs map to `0.0`, matching the viewer's CPU fallback (a
/// negative sample has no meaningful gamma-corrected display value).
pub fn apply_gamma(value: f32, gamma: f32) -> f32 {
    if value > 0.0 {
        value.powf(1.0 / gamma)
    } else {
        0.0
    }
}

/// Linear → sRGB opto-electronic transfer function (IEC 61966-2-1).
/// Mirrored by `linear_to_srgb` in `gpu/shader.wgsl`.
pub fn linear_to_srgb(l: f32) -> f32 {
    if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    /// sRGB → linear EOTF, the analytic inverse of [`linear_to_srgb`]. Defined
    /// here (test-only) so we can verify the forward transform round-trips.
    fn srgb_to_linear(s: f32) -> f32 {
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn exposure_multiplier_matches_powers_of_two() {
        assert_relative_eq!(exposure_to_multiplier(0.0), 1.0);
        assert_relative_eq!(exposure_to_multiplier(1.0), 2.0);
        assert_relative_eq!(exposure_to_multiplier(-1.0), 0.5);
        assert_relative_eq!(exposure_to_multiplier(3.0), 8.0);
    }

    #[test]
    fn gamma_is_identity_at_one() {
        for v in [0.05_f32, 0.25, 0.5, 0.9, 1.0, 4.0] {
            assert_relative_eq!(apply_gamma(v, 1.0), v, epsilon = 1e-6);
        }
    }

    #[test]
    fn gamma_22_brightens_midtones() {
        // 0.5 ^ (1/2.2) ≈ 0.7297; gamma should lift the midtone.
        let out = apply_gamma(0.5, 2.2);
        assert_relative_eq!(out, 0.5_f32.powf(1.0 / 2.2), epsilon = 1e-6);
        assert!(out > 0.5);
    }

    #[test]
    fn gamma_clamps_non_positive_to_zero() {
        assert_eq!(apply_gamma(0.0, 2.2), 0.0);
        assert_eq!(apply_gamma(-1.0, 2.2), 0.0);
    }

    #[test]
    fn srgb_is_linear_in_the_toe() {
        // Below the 0.0031308 break the transfer is exactly ×12.92.
        assert_relative_eq!(linear_to_srgb(0.0), 0.0);
        assert_relative_eq!(linear_to_srgb(0.001), 0.001 * 12.92, epsilon = 1e-9);
    }

    #[test]
    fn srgb_known_midpoint() {
        // Linear 0.5 encodes to ~0.7354 in sRGB — a well-known reference point.
        assert_relative_eq!(linear_to_srgb(0.5), 0.735_356_6, epsilon = 1e-4);
        assert_relative_eq!(linear_to_srgb(1.0), 1.0, epsilon = 1e-6);
    }

    #[test]
    fn srgb_round_trips_through_inverse() {
        for l in [0.0_f32, 0.002, 0.05, 0.18, 0.5, 0.9, 1.0] {
            let back = srgb_to_linear(linear_to_srgb(l));
            assert_relative_eq!(back, l, epsilon = 1e-5);
        }
    }
}
