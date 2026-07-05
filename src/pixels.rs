//! Pure pixel access over decoded EXR channels + thumbnail decimation math.
//!
//! These helpers are consumed by the viewer's CPU bake paths, the proxy
//! downsampler, and the GPU thumbnail generator. They live here — not in
//! `viewer` — so `gpu/` never has to import from the UI layer (#153): the
//! module dependency direction stays `viewer → gpu`, never the reverse, which
//! keeps the UI module portable for the Qt port (#44).

/// Read one float component from a channel at `(x, y)`, handling F32 (fast
/// path), F16, and U32 `FlatSamples`. Returns 0.0 for a missing channel.
/// The `sample_data` match is invariant for the whole channel — in hot pixel
/// loops, prefer pre-extracting the F32 slice (the common case) via
/// [`sample_channel_f32`] + [`pixel_val`] to avoid the per-pixel enum
/// dispatch. This is the single source of truth for the sampling logic
/// (previously duplicated 8× as inline `get_val` closures).
pub(crate) fn sample_channel(
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
    x: usize,
    y: usize,
    width: usize,
) -> f32 {
    if let Some(c) = chan {
        let index = y * width + x;
        match &c.sample_data {
            exr::image::FlatSamples::F16(s) => s[index].to_f32(),
            exr::image::FlatSamples::F32(s) => s[index],
            exr::image::FlatSamples::U32(s) => s[index] as f32 / u32::MAX as f32,
        }
    } else {
        0.0
    }
}

/// If the channel is F32 (the common EXR case), return its slice for direct
/// indexing — eliminates the per-pixel `FlatSamples` enum match in hot loops.
/// Non-F32 channels return `None`; fall back to [`sample_channel`] for those.
pub(crate) fn sample_channel_f32(
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
) -> Option<&[f32]> {
    chan.and_then(|c| match &c.sample_data {
        exr::image::FlatSamples::F32(s) => Some(s.as_slice()),
        _ => None,
    })
}

/// Whether every *present* channel is F16 (absent channels are defaulted — rgb
/// to 0, alpha to 1 — so they don't force a wider format). EXR beauty data is
/// overwhelmingly F16, so this is the common case: the layer can pack + upload as
/// `Rgba16Float` — lossless, and half the bandwidth and VRAM of `Rgba32Float`
/// (#142). A present F32/U32 channel returns `false` so the caller keeps full
/// precision.
pub(crate) fn all_channels_f16(
    channels: [Option<&exr::image::AnyChannel<exr::image::FlatSamples>>; 4],
) -> bool {
    channels
        .iter()
        .all(|c| c.is_none_or(|ch| matches!(&ch.sample_data, exr::image::FlatSamples::F16(_))))
}

/// The F16 sample slice, for a direct half-bit-pattern copy in the `Rgba16Float`
/// pack (#142) — the half-float analogue of [`sample_channel_f32`]. Copying
/// `f16::to_bits()` is a pure reinterpret, skipping the per-pixel `to_f32`
/// widening the F32 path would otherwise pay for a half-float source.
pub(crate) fn f16_slice(
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
) -> Option<&[exr::prelude::f16]> {
    chan.and_then(|c| match &c.sample_data {
        exr::image::FlatSamples::F16(s) => Some(s.as_slice()),
        _ => None,
    })
}

/// Read a pixel from a pre-extracted F32 slice, falling back to
/// [`sample_channel`] for non-F32 channels. Used in hot pixel loops to skip
/// the enum match on the F32 fast path.
#[inline]
pub(crate) fn pixel_val(
    f32_slice: Option<&[f32]>,
    chan: Option<&exr::image::AnyChannel<exr::image::FlatSamples>>,
    x: usize,
    y: usize,
    width: usize,
) -> f32 {
    if let Some(s) = f32_slice {
        s[y * width + x]
    } else {
        sample_channel(chan, x, y, width)
    }
}

/// Output dimensions and source stride for a CPU texture bake. With `max_dim ==
/// None` (the full-res CPU-display fallback) this is the source size at stride 1.
/// With `Some(d)` (contact-sheet thumbnails) the source is point-decimated so the
/// longest edge is at most `d` — the per-pixel tone pipeline then runs over the
/// small output instead of the full frame, which is the difference between
/// processing ~34k pixels and ~8M for a 4K layer (re-baked on every frame swap
/// while the sheet is open). Aspect is preserved within rounding.
pub(crate) fn thumb_dims(
    width: usize,
    height: usize,
    max_dim: Option<usize>,
) -> (usize, usize, usize) {
    match max_dim {
        Some(d) if d > 0 && width.max(height) > d => {
            let stride = width.max(height).div_ceil(d);
            (width.div_ceil(stride), height.div_ceil(stride), stride)
        }
        _ => (width.max(1), height.max(1), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::thumb_dims;

    #[test]
    fn thumb_dims_decimates_to_the_box_and_preserves_aspect() {
        // No cap (CPU-display fallback): full res, stride 1.
        assert_eq!(thumb_dims(4096, 2160, None), (4096, 2160, 1));
        // Image already within the box: untouched.
        assert_eq!(thumb_dims(200, 100, Some(256)), (200, 100, 1));
        // 4K landscape -> longest edge capped at the box, aspect preserved.
        let (w, h, stride) = thumb_dims(4096, 2160, Some(256));
        assert!(w <= 256 && h <= 256, "longest edge within the box: {w}x{h}");
        assert_eq!(stride, 16, "4096.div_ceil(256)");
        assert!(
            (w as f32 / h as f32 - 4096.0 / 2160.0).abs() < 0.05,
            "aspect kept"
        );
        // Portrait caps the height instead.
        let (w, h, _) = thumb_dims(1080, 1920, Some(256));
        assert!(
            w <= 256 && h <= 256 && h >= w,
            "portrait stays portrait: {w}x{h}"
        );
        // Degenerate: never produces a zero dimension.
        assert_eq!(thumb_dims(0, 0, Some(256)), (1, 1, 1));
    }

    #[test]
    fn all_channels_f16_gates_the_16f_pack() {
        use super::all_channels_f16;
        use exr::image::{AnyChannel, FlatSamples};
        use exr::prelude::{Text, f16};
        let f16c = || {
            AnyChannel::new(
                Text::from("X"),
                FlatSamples::F16(vec![f16::from_f32(0.5); 4]),
            )
        };
        let f32c = || AnyChannel::new(Text::from("X"), FlatSamples::F32(vec![0.5; 4]));
        let (r, g, b) = (f16c(), f16c(), f16c());
        // All present channels F16 (alpha absent → defaulted, doesn't force 32F).
        assert!(all_channels_f16([Some(&r), Some(&g), Some(&b), None]));
        // A single F32 channel forces 32F.
        let a32 = f32c();
        assert!(!all_channels_f16([
            Some(&r),
            Some(&g),
            Some(&b),
            Some(&a32)
        ]));
        // Degenerate all-absent is trivially "f16".
        assert!(all_channels_f16([None, None, None, None]));
    }

    #[test]
    fn f16_slice_extracts_only_f16_channels() {
        use super::f16_slice;
        use exr::image::{AnyChannel, FlatSamples};
        use exr::prelude::{Text, f16};
        let f16c = AnyChannel::new(
            Text::from("X"),
            FlatSamples::F16(vec![f16::from_f32(1.0); 3]),
        );
        let f32c = AnyChannel::new(Text::from("X"), FlatSamples::F32(vec![1.0; 3]));
        assert_eq!(f16_slice(Some(&f16c)).map(<[_]>::len), Some(3));
        assert!(f16_slice(Some(&f32c)).is_none()); // F32 → not the fast path
        assert!(f16_slice(None).is_none());
    }
}
