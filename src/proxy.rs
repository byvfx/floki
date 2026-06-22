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
//! that rect). Decoupling it from `ExrData` lets the proxy come from a true
//! low-res EXR read ([`ProxyImage::from_exr_fast_read`], #33) that decompresses
//! only every Nth scanline block, without touching the render side.
//! [`ProxyImage::from_exr_data_downsampled`] remains as a GPU-free,
//! unit-testable downsample reference.

use crate::exr_loader::ExrData;
use exr::block::{self, UncompressedBlock};
use exr::meta::BlockDescription;
use exr::meta::attribute::SampleType;
use exr::prelude::f16;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Target number of scanline blocks to decompress for a fast-read proxy. Bounding
/// the work to a constant (independent of resolution) is what makes the first
/// paint fast: [`ProxyImage::from_exr_fast_read`] decompresses only every Nth
/// block, and bails when that wouldn't skip anything.
const PROXY_TARGET_BLOCKS: usize = 48;

/// A low-resolution first-paint image. See the module docs.
///
/// Produced on the decode worker by [`ProxyImage::from_exr_fast_read`] (#33) and
/// shown via the viewer's proxy path (#58) until the full-res decode lands.
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
        // `max_dim` is the downsample divisor below (`div_ceil(max_dim)`); guard
        // zero here so a 0 from any caller returns `None` instead of panicking.
        if full_width == 0 || full_height == 0 || max_dim == 0 {
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
        // `ExrViewer::build_layer_texture`'s packing). For a true low-res read
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

    /// Fast low-resolution first-paint proxy: decompress only every Nth scanline
    /// block of the file's first layer, so a downsampled image is ready in a
    /// fraction of the full-decode time. The viewer upscales it into the full
    /// image rect (#58). This is the production path #33 wires into the decode
    /// worker; [`Self::from_exr_data_downsampled`] is the GPU-free reference.
    ///
    /// Returns `None` — leaving the normal spinner-then-full-decode path — when a
    /// proxy would not help or is unsupported:
    /// - tiled or deep images (only flat scanline images are handled here);
    /// - images too short, or with compression blocks too large, to skip any
    ///   blocks, so the proxy read would cost about as much as the full decode;
    /// - files without R/G/B channels, or that fail to open / parse.
    ///
    /// Approximates the active layer by matching the first `R`/`G`/`B`(`/A`)
    /// channels (correct for the common single-layer beauty case); the full-res
    /// render, which honours the selected layer, replaces it moments later.
    #[must_use]
    pub fn from_exr_fast_read(path: &Path) -> Option<Self> {
        let file = BufReader::with_capacity(1 << 20, File::open(path).ok()?);
        let reader = block::read(file, false).ok()?;
        let meta = reader.meta_data().clone();
        let header = meta.headers.first()?;

        // Only flat scanline images. Tiled / deep go straight to the full decode.
        if header.deep || !matches!(header.blocks, BlockDescription::ScanLines) {
            return None;
        }
        let full_width = header.layer_size.0;
        let full_height = header.layer_size.1;
        if full_width == 0 || full_height == 0 {
            return None;
        }

        // First R/G/B(/A) channels by suffix. Channels are name-sorted, so
        // first-match is deterministic. RGB required; A optional (opaque if absent).
        let (mut r_idx, mut g_idx, mut b_idx, mut a_idx) = (None, None, None, None);
        for (idx, ch) in header.channels.list.iter().enumerate() {
            let name = ch.name.to_string();
            let suffix = name.rsplit('.').next().unwrap_or(name.as_str());
            if r_idx.is_none() && suffix.eq_ignore_ascii_case("R") {
                r_idx = Some(idx);
            } else if g_idx.is_none() && suffix.eq_ignore_ascii_case("G") {
                g_idx = Some(idx);
            } else if b_idx.is_none() && suffix.eq_ignore_ascii_case("B") {
                b_idx = Some(idx);
            } else if a_idx.is_none() && suffix.eq_ignore_ascii_case("A") {
                a_idx = Some(idx);
            }
        }
        let (r_idx, g_idx, b_idx) = (r_idx?, g_idx?, b_idx?);

        // Bound the work: decompress only every `block_stride`-th scanline block.
        // If that wouldn't skip anything (short image or large compression
        // blocks), the full decode is already fast — bail rather than read twice.
        let block_h = header.compression.scan_lines_per_block().max(1);
        let num_blocks = full_height.div_ceil(block_h);
        let block_stride = num_blocks / PROXY_TARGET_BLOCKS;
        if block_stride < 2 {
            return None;
        }
        // Match horizontal and vertical sampling periods so proxy pixels stay
        // roughly square (the proxy is upscaled uniformly to the full rect).
        let col_stride = (block_h * block_stride).max(1);
        let proxy_height = num_blocks.div_ceil(block_stride);
        let proxy_width = full_width.div_ceil(col_stride).max(1);

        // Byte offset of each channel's row within one (native-endian) scanline of
        // a decompressed block. Block layout is row-major: each scanline stores
        // every channel's samples contiguously, in channel-list order. The proxy
        // samples only the top row of each kept block.
        let sample_types: Vec<SampleType> =
            header.channels.list.iter().map(|c| c.sample_type).collect();
        let mut row_offsets = vec![0usize; sample_types.len()];
        let mut acc = 0usize;
        for (i, ty) in sample_types.iter().enumerate() {
            row_offsets[i] = acc;
            acc += full_width * ty.bytes_per_sample();
        }

        let mut pixels = vec![0.0f32; proxy_width * proxy_height * 4];
        // Default alpha to opaque (overwritten below if an A channel exists).
        for px in pixels.chunks_exact_mut(4) {
            px[3] = 1.0;
        }

        // Decompress only the kept blocks (every `block_stride`-th, layer 0).
        let chunks = reader
            .filter_chunks(false, move |_meta, _tile, block| {
                block.layer == 0 && (block.pixel_position.1 / block_h) % block_stride == 0
            })
            .ok()?;

        for chunk in chunks {
            let block = UncompressedBlock::decompress_chunk(chunk.ok()?, &meta, false).ok()?;
            let ordinal = block.index.pixel_position.1 / block_h;
            let proxy_y = ordinal / block_stride;
            if ordinal % block_stride != 0 || proxy_y >= proxy_height {
                continue;
            }
            let data = &block.data;
            for (slot, channel) in [Some(r_idx), Some(g_idx), Some(b_idx), a_idx]
                .into_iter()
                .enumerate()
            {
                let Some(ch) = channel else { continue };
                let ty = sample_types[ch];
                let bps = ty.bytes_per_sample();
                let row_start = row_offsets[ch];
                let row = data.get(row_start..row_start + full_width * bps)?;
                let out_row = proxy_y * proxy_width;
                for px in 0..proxy_width {
                    let x = (px * col_stride).min(full_width - 1);
                    pixels[(out_row + px) * 4 + slot] =
                        read_sample(&row[x * bps..x * bps + bps], ty);
                }
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

/// Read one sample from native-endian block bytes, normalising to `f32` exactly
/// as [`crate::viewer::sample_channel`] does (U32 scaled by `u32::MAX`). Returns
/// `0.0` if the slice is too short (the caller already bounds-checks the row).
fn read_sample(bytes: &[u8], ty: SampleType) -> f32 {
    match ty {
        SampleType::F16 => bytes
            .get(..2)
            .and_then(|b| b.try_into().ok())
            .map_or(0.0, |b| f16::from_ne_bytes(b).to_f32()),
        SampleType::F32 => bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map_or(0.0, f32::from_ne_bytes),
        SampleType::U32 => bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map_or(0.0, |b| u32::from_ne_bytes(b) as f32 / u32::MAX as f32),
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

    #[test]
    fn zero_max_dim_returns_none() {
        // `max_dim` is the downsample divisor; 0 must return None, not panic on
        // the `div_ceil(0)` divide-by-zero.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grad.exr");
        write_gradient_exr(&path);
        let data = ExrData::load(&path).unwrap();
        assert!(ProxyImage::from_exr_data_downsampled(&data, 0, 0).is_none());
    }

    /// `w × h` uncompressed (1 scanline/block) RGBA gradient: R increases with x,
    /// G with y, B constant 0.25, A constant 1. `half` writes F16 vs F32 samples.
    fn write_gradient(path: &std::path::Path, w: usize, h: usize, half: bool) {
        let xden = (w.max(2) - 1) as f32;
        let yden = (h.max(2) - 1) as f32;
        let r: Vec<f32> = (0..w * h).map(|i| (i % w) as f32 / xden).collect();
        let g: Vec<f32> = (0..w * h).map(|i| (i / w) as f32 / yden).collect();
        let b = vec![0.25f32; w * h];
        let a = vec![1.0f32; w * h];
        let mut list = smallvec::SmallVec::new();
        for (name, vals) in [("R", r), ("G", g), ("B", b), ("A", a)] {
            let samples = if half {
                FlatSamples::F16(vals.iter().map(|&v| f16::from_f32(v)).collect())
            } else {
                FlatSamples::F32(vals)
            };
            list.push(AnyChannel::new(Text::from(name), samples));
        }
        Image::from_layer(Layer::new(
            (w, h),
            LayerAttributes::default(),
            Encoding::UNCOMPRESSED,
            AnyChannels::sort(list),
        ))
        .write()
        .to_file(path)
        .expect("write gradient exr");
    }

    #[test]
    fn fast_read_downsamples_tall_scanline_image() {
        // Uncompressed → 1 scanline/block → num_blocks = 200; block_stride =
        // 200 / 48 = 4; col_stride = 4 → 4×50 proxy. Verify for both F16 and F32.
        for half in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tall.exr");
            write_gradient(&path, 16, 200, half);

            let proxy = ProxyImage::from_exr_fast_read(&path).expect("proxy for tall image");
            assert_eq!(
                (proxy.full_width, proxy.full_height),
                (16, 200),
                "half={half}"
            );
            assert_eq!(
                (proxy.proxy_width, proxy.proxy_height),
                (4, 50),
                "half={half}"
            );
            assert_eq!(proxy.pixels.len(), 4 * 50 * 4);

            let eps = if half { 1e-3 } else { 1e-6 };
            // Top-left proxy pixel = image (0,0): R=0, G=0, B=0.25, A=1.
            approx::assert_abs_diff_eq!(proxy.pixels[0], 0.0, epsilon = eps);
            approx::assert_abs_diff_eq!(proxy.pixels[1], 0.0, epsilon = eps);
            approx::assert_abs_diff_eq!(proxy.pixels[2], 0.25, epsilon = eps);
            approx::assert_abs_diff_eq!(proxy.pixels[3], 1.0, epsilon = eps);
            // Constant channels hold everywhere, independent of sample positions.
            for px in proxy.pixels.chunks_exact(4) {
                approx::assert_abs_diff_eq!(px[2], 0.25, epsilon = eps);
                approx::assert_abs_diff_eq!(px[3], 1.0, epsilon = eps);
            }
        }
    }

    #[test]
    fn fast_read_returns_none_for_short_image() {
        // num_blocks (== height, uncompressed) < 2 × PROXY_TARGET_BLOCKS, so no
        // blocks can be skipped → no proxy (the full decode is already fast).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.exr");
        write_gradient(&path, 8, 40, false);
        assert!(ProxyImage::from_exr_fast_read(&path).is_none());
    }

    #[test]
    fn fast_read_returns_none_for_missing_file() {
        assert!(
            ProxyImage::from_exr_fast_read(std::path::Path::new("/no/such/file.exr")).is_none()
        );
    }
}
