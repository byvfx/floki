//! Low-resolution first-paint proxy images (#58 / #33).
//!
//! While the full-res `ExrData` decode is still in flight on the worker thread
//! (#28), the viewport can paint a cheap low-res [`ProxyImage`] almost instantly
//! so the image appears instead of a spinner. When the full decode lands,
//! [`crate::app::ExrApp::swap_image_data`] (#55) invalidates the full-res
//! textures and the proxy is cleared — the viewport swaps to full-res with the
//! same zoom/pan (session state is preserved across the handoff).
//!
//! A `ProxyImage` is a **standalone RGBA32Float buffer plus the full image
//! dimensions**, not an `ExrData`. It carries the full pixel dimensions so the
//! viewport can lay out the image rect identically to the upcoming full-res
//! render (the proxy texture is sampled with linear filtering and upscales into
//! that rect). Decoupling it from `ExrData` lets #33 later produce a proxy via a
//! true low-res EXR read (decoding only every Nth block) without touching the
//! render side — [`ProxyImage::from_exr_data_downsampled`] is the testable seam
//! that path will replace.

use crate::exr_loader::ExrData;

/// A low-resolution first-paint image. See the module docs.
///
/// `#[allow(dead_code)]`: the render-side path is wired (#58) but the producer
/// of proxy data is the #33 decode path, which hasn't landed yet. The unit
/// tests below exercise the downsample seam; `ExrApp::set_proxy` is the entry
/// point #33 will call from the decode worker.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProxyImage {
    /// Full-resolution pixel dimensions — the display window of the full image.
    /// Used for viewport layout so the proxy lands in the same rect the full-res
    /// render will occupy, making the handoff visually continuous.
    pub full_width: usize,
    pub full_height: usize,
    /// Proxy pixel dimensions (`<= full`). The texture is sampled with linear
    /// filtering, so it upscales smoothly to the full image rect.
    pub proxy_width: usize,
    pub proxy_height: usize,
    /// RGBA32Float, row-major, length `proxy_width * proxy_height * 4`.
    pub pixels: Vec<f32>,
}

impl ProxyImage {
    /// Build a proxy by box-filter downsampling a loaded `ExrData` layer to fit
    /// within `max_dim` on its long side (no upscaling — a layer already smaller
    /// than `max_dim` is returned at native resolution).
    ///
    /// This is the **testable seam** #33 will replace with a true low-res EXR
    /// read; the render side ([`crate::viewer::ExrViewer`] proxy path) is
    /// identical either way. Kept here so the downsample math is unit-testable
    /// without a GPU.
    ///
    /// Returns `None` if the layer index is invalid.
    #[allow(dead_code)]
    #[must_use]
    pub fn from_exr_data_downsampled(
        data: &ExrData,
        layer_index: usize,
        max_dim: usize,
    ) -> Option<Self> {
        let (layer, r_chan, g_chan, b_chan, a_chan) = data.logical_channels(layer_index)?;
        let full_width = layer.size.0;
        let full_height = layer.size.1;
        if full_width == 0 || full_height == 0 {
            return None;
        }

        // Downsample factor: the long side fits within max_dim. At least 1:1
        // (no upscale); cap the factor so a tiny max_dim still yields >=1px.
        let long_side = full_width.max(full_height);
        let factor = if long_side <= max_dim {
            1
        } else {
            long_side.div_ceil(max_dim)
        };
        let proxy_width = full_width.div_ceil(factor).max(1);
        let proxy_height = full_height.div_ceil(factor).max(1);

        // Pack the full-res layer to RGBA32Float first (mirrors
        // `ExrViewer::generate_gpu_texture`'s packing). For a true low-res read
        // (#33) this full pack goes away — the whole point — but for the
        // downsample seam we need the source pixels.
        let mut full = vec![0.0f32; full_width * full_height * 4];
        for y in 0..full_height {
            for x in 0..full_width {
                let i = (y * full_width + x) * 4;
                full[i] = crate::viewer::sample_channel(r_chan, x, y, full_width);
                full[i + 1] = crate::viewer::sample_channel(g_chan, x, y, full_width);
                full[i + 2] = crate::viewer::sample_channel(b_chan, x, y, full_width);
                full[i + 3] = a_chan
                    .map(|c| crate::viewer::sample_channel(Some(c), x, y, full_width))
                    .unwrap_or(1.0);
            }
        }

        // Box-filter: average each `factor × factor` block (clamped to the image
        // edge) into one proxy pixel. Box filtering is resolution-independent
        // and cheap — good enough for a first paint; the full-res decode
        // replaces it moments later.
        let mut pixels = vec![0.0f32; proxy_width * proxy_height * 4];
        for py in 0..proxy_height {
            for px in 0..proxy_width {
                let x0 = px * factor;
                let y0 = py * factor;
                let x1 = ((px + 1) * factor).min(full_width);
                let y1 = ((py + 1) * factor).min(full_height);
                let mut acc = [0.0f32; 4];
                let mut n = 0u32;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = (y * full_width + x) * 4;
                        acc[0] += full[i];
                        acc[1] += full[i + 1];
                        acc[2] += full[i + 2];
                        acc[3] += full[i + 3];
                        n += 1;
                    }
                }
                let n = n.max(1) as f32;
                let o = (py * proxy_width + px) * 4;
                pixels[o] = acc[0] / n;
                pixels[o + 1] = acc[1] / n;
                pixels[o + 2] = acc[2] / n;
                pixels[o + 3] = acc[3] / n;
            }
        }

        Some(Self {
            full_width,
            full_height,
            proxy_width,
            proxy_height,
            pixels,
        })
    }
}

// Channel sampling reuses [`crate::viewer::sample_channel`] (the single,
// tested implementation that handles F32/F16/U32 `FlatSamples`) rather than
// duplicating the enum match here. It's `pub(crate)` for that reason.

#[cfg(test)]
mod tests {
    use super::*;
    use exr::prelude::*;

    /// 4×4 RGBA EXR with a known gradient so downsampling is verifiable.
    fn write_gradient_exr(path: &std::path::Path) {
        const W: usize = 4;
        const H: usize = 4;
        // R increases with x, G with y, B constant, A constant 1.
        let r: Vec<f32> = (0..W * H).map(|i| (i % W) as f32 / 3.0).collect();
        let g: Vec<f32> = (0..W * H).map(|i| (i / W) as f32 / 3.0).collect();
        let b: Vec<f32> = vec![0.25; W * H];
        let a: Vec<f32> = vec![1.0; W * H];
        let mut list = smallvec::SmallVec::new();
        for (name, vals) in [("R", r), ("G", g), ("B", b), ("A", a)] {
            list.push(AnyChannel::new(Text::from(name), FlatSamples::F32(vals)));
        }
        Image::from_layer(Layer::new(
            (W, H),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(path)
        .expect("write gradient exr");
    }

    #[test]
    fn downsample_halves_dimensions_and_averages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grad.exr");
        write_gradient_exr(&path);
        let data = ExrData::load(&path).unwrap();

        // 4×4, max_dim=2 → factor 2 → 2×2 proxy.
        let proxy = ProxyImage::from_exr_data_downsampled(&data, 0, 2).unwrap();
        assert_eq!(proxy.full_width, 4);
        assert_eq!(proxy.full_height, 4);
        assert_eq!(proxy.proxy_width, 2);
        assert_eq!(proxy.proxy_height, 2);
        assert_eq!(proxy.pixels.len(), 2 * 2 * 4);

        // Top-left proxy pixel averages the 4×4's top-left 2×2 block:
        // R: (0/3 + 1/3 + 0/3 + 1/3)/4 = (2/3)/4 = 1/6 ≈ 0.1667
        // G: (0/3 + 0/3 + 1/3 + 1/3)/4 = (2/3)/4 = 1/6
        // B: 0.25 ; A: 1.0
        let px = &proxy.pixels[0..4];
        approx::assert_abs_diff_eq!(px[0], 1.0 / 6.0, epsilon = 1e-5);
        approx::assert_abs_diff_eq!(px[1], 1.0 / 6.0, epsilon = 1e-5);
        approx::assert_abs_diff_eq!(px[2], 0.25, epsilon = 1e-5);
        approx::assert_abs_diff_eq!(px[3], 1.0, epsilon = 1e-5);
    }

    #[test]
    fn no_upscale_when_already_within_max_dim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grad.exr");
        write_gradient_exr(&path);
        let data = ExrData::load(&path).unwrap();

        // max_dim >= long side → native resolution (factor 1).
        let proxy = ProxyImage::from_exr_data_downsampled(&data, 0, 8).unwrap();
        assert_eq!((proxy.proxy_width, proxy.proxy_height), (4, 4));
        // And the pixels match the source R channel at (0,0) = 0.0.
        approx::assert_abs_diff_eq!(proxy.pixels[0], 0.0, epsilon = 1e-6);
    }

    #[test]
    fn invalid_layer_index_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grad.exr");
        write_gradient_exr(&path);
        let data = ExrData::load(&path).unwrap();
        assert!(ProxyImage::from_exr_data_downsampled(&data, 99, 2).is_none());
    }
}
