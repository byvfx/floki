use exr::prelude::*;
use std::path::Path;

/// The decoded-image type the viewer works with: a (small) list of physical
/// layers, each a flat `AnyChannels` buffer. Both the full read (`all_layers`)
/// and the beauty-only read (a single layer, re-wrapped) produce this shape.
type FlatImage = Image<smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]>>;

/// Error returned when the synchronous parse path panics (see [`ExrData::load`]).
const DECODE_PANIC_MSG: &str = "Failed to decode EXR: the decompressor panicked. The file may use an \
     unsupported compression variant, be corrupted, or trigger a bug in the \
     exr crate's miniz_oxide decompressor.";

/// On-disk proxy-cache blob format (#165). Magic + version prefix a raw f16/f32
/// dump of a downsampled proxy plus the geometry the viewport frames by, so a
/// repeat pass reconstructs the proxy without re-decoding the source EXR. The
/// magic doubles as the version tag (`FPX1` = format v1); bump it on any layout
/// change so a stale blob is rejected as a miss rather than mis-parsed. See
/// [`ExrData::write_proxy_blob`] / [`ExrData::from_proxy_blob`].
const PROXY_BLOB_MAGIC: &[u8] = b"FPX1";
/// v2 (#217) adds the echoed `key.aov` and the [`ExrData::only_layer`] tag, so a
/// per-AOV proxy is neither served for the wrong pass nor reconstructed as if it
/// were the beauty layer. A v1 blob is rejected as a miss and re-decoded.
const PROXY_BLOB_VERSION: u8 = 2;
/// Per-channel sample-type tags in the blob. Proxies only ever carry F16 or F32
/// (`downsampled` maps U32 → F32), so those are the only two tags.
const SAMPLE_TAG_F16: u8 = 0;
const SAMPLE_TAG_F32: u8 = 1;
/// Sanity bounds for reconstructing a proxy from an on-disk blob (#165): a
/// corrupt or truncated `.fpx` must fail as a cache miss, never a huge allocation
/// or a runaway loop. Real proxies are a handful of layers / channels; the
/// per-channel sample buffer is separately bounded to the blob's own remaining
/// bytes before it is allocated.
const MAX_PROXY_LAYERS: usize = 64;
const MAX_PROXY_CHANNELS: usize = 1024;

pub struct ExrData {
    pub image: FlatImage,
    /// Displayable passes derived by grouping channels by their dotted name
    /// prefix. See [`LogicalLayer`].
    pub logical_layers: Vec<LogicalLayer>,
    /// Truncated, comma-joined list of logical-layer names, computed once at
    /// load time. Shown in the status bar; rebuilding it every frame for
    /// Blender EXRs with 100+ layers was wasteful.
    pub channels_str: String,
    /// True when this frame was decoded **beauty-only** (a single layer, via
    /// [`ExrData::load_beauty`]) for the playback ring (#56, hardening step 3)
    /// rather than the full all-AOV [`ExrData::load`]. Such a frame is correct
    /// for the displayed beauty/active layer but carries no other AOVs and is
    /// not a valid sampling source — `app.rs` re-decodes it in full on settle so
    /// the readout + AOV switcher see every channel (INV-SAMPLE, #7).
    pub beauty_only: bool,
    /// True when this is a **downsampled scrub proxy** (#94), built by
    /// [`ExrData::load_proxy`] — a normal decode box-filtered down
    /// ([`Self::downsampled`]) — for cheap playback of heavy footage. Not a valid
    /// full/sampling source, so callers treat a proxy as **not-full** (settle
    /// re-decodes it) regardless of `beauty_only`; the distinct flag also lets the
    /// cache budget size off the tiny proxy bytes so hundreds of frames fit, and
    /// marks it in the debug overlay. (In practice a proxy is also `beauty_only`,
    /// since `load_proxy` downsamples a `load_beauty` result — but `downsampled`
    /// itself preserves the source flag, so don't rely on that coupling.)
    pub proxy: bool,
    /// For a **proxy** (#94), the *full-resolution* display dimensions — the pixel
    /// buffers are downsampled, but the viewport must frame the image at full size
    /// (and preserve zoom/pan across the proxy↔full swap), or a tiny proxy would
    /// render tiny. [`Self::logical_size`] returns this when set; the texture is
    /// still built from the small buffers (and upscaled). `None` for real frames,
    /// where the buffer dims *are* the display dims.
    pub display_size: Option<(usize, usize)>,
    /// For a single-layer decode ([`Self::load_layer`], #217): which index **in the
    /// full image's layer table** this data represents.
    ///
    /// The decode carries one physical layer, so its own `logical_layers` are
    /// renumbered from 0 — but callers hold indices from the full image and would
    /// otherwise ask for, say, layer 5 of a table with one entry and get `None`.
    /// That is not a soft failure: it is #213's silent freeze, where every texture
    /// build fails and the layer never displays a frame.
    ///
    /// [`Self::resolve_logical`] maps the caller's index onto the decoded one. The
    /// mapping is only unambiguous when the decoded part holds exactly **one**
    /// logical layer, so the caller must check that before requesting this path —
    /// see `ExrApp::single_layer_part`.
    pub only_layer: Option<usize>,
}

/// A displayable "layer" (render pass) derived from grouping a physical EXR
/// layer's channels by their dotted name prefix.
///
/// OpenEXR stores a layer's channels as a flat list; the convention is that a
/// channel name like `diffuse.R` means channel `R` of layer `diffuse`. Blender
/// in particular writes a *single* EXR part whose channels encode every render
/// pass as a prefix (`ViewLayer.Combined.R`, `ViewLayer.Normal.X`, ...). The
/// `exr` crate exposes that as one unnamed `Layer`, so without regrouping the
/// passes are invisible. This type is that regrouping: one entry per pass.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalLayer {
    /// Smart display name, e.g. `Combined`, `Normal`, or `RGBA` for root channels.
    pub name: String,
    /// Full group key (the channel-name prefix, plus any physical layer name),
    /// e.g. `ViewLayer.Combined`; empty for unprefixed root channels.
    pub group_key: String,
    /// Index into [`ExrData::image`]`.layer_data`.
    pub physical_index: usize,
    /// Indices into the physical layer's `channel_data.list` for each slot.
    pub r: Option<usize>,
    pub g: Option<usize>,
    pub b: Option<usize>,
    pub a: Option<usize>,
}

impl ExrData {
    /// Decode an EXR file and group its channels into logical layers.
    ///
    /// # Errors
    /// Returns `Err` with a human-readable message if the file cannot be read or parsed,
    /// or if decoding panics (caught via `catch_unwind`).
    pub fn load(path: impl AsRef<Path>) -> std::result::Result<Self, String> {
        let path_ref = path.as_ref();

        // The patched `exr` (see [patch.crates-io] in Cargo.toml) decompresses via
        // miniz_oxide, which returns an error instead of panicking on bad data, so
        // parallel decompression is safe again. catch_unwind is kept as cheap
        // insurance against panics in the synchronous (calling-thread) parsing
        // path; it can't catch a rayon-worker panic, but miniz_oxide removes the
        // one decompression panic that used to abort the app.
        let read_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let builder = read()
                .no_deep_data()
                .largest_resolution_level()
                .all_channels()
                .all_layers()
                .all_attributes();
            match map_file(path_ref) {
                Some(map) => builder.from_buffered(std::io::Cursor::new(&map[..])),
                None => builder.from_file(path_ref),
            }
        }))
        .map_err(|_| DECODE_PANIC_MSG.to_string())?;

        let image = map_read_err(read_result, path_ref)?;
        Ok(Self::from_image(image, false, false))
    }

    /// Decode only the **physical layer at `physical_index`** — the AOV
    /// generalisation of [`Self::load_beauty`] (#217).
    ///
    /// `load_beauty` accelerates the beauty pass and nothing else, so inspecting
    /// any other AOV fell off a cliff: the gate refused every cheap decode and
    /// playback paid a full all-parts decode. Measured on a 16-part / 421 MB
    /// render, all layers is **264 ms** against **21 ms** for one — so the
    /// difference between an AOV that plays and one that crawls is entirely
    /// whether the other parts get decompressed.
    ///
    /// Uses `specific_layer`, added to the `byvfx/exrs` fork for this: the block
    /// reader rejects every block not belonging to `physical_index`, exactly as
    /// `first_valid_layer` does for part 0.
    ///
    /// `logical_index` is the caller's index **in the full image's** layer table,
    /// recorded as [`only_layer`](Self::only_layer) so the resulting one-layer
    /// `ExrData` can still answer `logical_channels(logical_index)`. See that field
    /// for why the caller must not ask for a part holding more than one logical
    /// layer.
    ///
    /// The result is flagged [`beauty_only`](Self::beauty_only) — not because it is
    /// the beauty pass, but because it carries a *subset* of the image and so is
    /// not a valid sampling or AOV-switch source; the settle re-decodes it full.
    ///
    /// # Errors
    /// Same contract as [`Self::load`], plus an error if `physical_index` is out of
    /// range, or if the decoded part turns out to hold more than one logical layer
    /// (see below).
    ///
    /// # Refusing a multi-pass part
    /// The `only_layer` remap is only unambiguous for a part holding exactly one
    /// logical layer. `ExrApp::single_layer_part` is what decides that, from the
    /// full layer table — but it decides *before* the decode, from a table that
    /// could in principle disagree with this file (a re-rendered frame mid-sequence
    /// with a different part layout). If it does, every texture build fails, and
    /// #213's evict-and-retry hardening re-decodes the same wrong thing forever: an
    /// unbounded loop, not a slow path. Erroring here costs one wasted decode and
    /// falls the caller back to a full one.
    pub fn load_layer(
        path: impl AsRef<Path>,
        physical_index: usize,
        logical_index: usize,
    ) -> std::result::Result<Self, String> {
        let path_ref = path.as_ref();

        let read_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let builder = read()
                .no_deep_data()
                .largest_resolution_level()
                .all_channels()
                .specific_layer(physical_index)
                .all_attributes();
            match map_file(path_ref) {
                Some(map) => builder.from_buffered(std::io::Cursor::new(&map[..])),
                None => builder.from_file(path_ref),
            }
        }))
        .map_err(|_| DECODE_PANIC_MSG.to_string())?;

        let single = map_read_err(read_result, path_ref)?;
        let image = FlatImage {
            attributes: single.attributes,
            layer_data: smallvec::smallvec![single.layer_data],
        };
        let mut data = Self::from_image(image, true, false);
        if data.logical_layers.len() != 1 {
            return Err(format!(
                "{}: part {physical_index} holds {} render passes, not one — \
                 a single-layer decode cannot address them",
                path_ref.display(),
                data.logical_layers.len()
            ));
        }
        data.only_layer = Some(logical_index);
        Ok(data)
    }

    /// [`Self::load_layer`] followed by the same box-filter downsample
    /// [`Self::load_proxy`] applies (#217). Only the requested layer is decimated —
    /// doing all of them would cost ~16x the filter time on a 16-part render for
    /// pixels nothing is going to display.
    ///
    /// # Errors
    /// Same contract as [`Self::load_layer`].
    pub fn load_layer_proxy_into(
        path: impl AsRef<Path>,
        physical_index: usize,
        logical_index: usize,
        max_dim: usize,
        scratch: &mut Vec<f32>,
    ) -> std::result::Result<Self, String> {
        let full = Self::load_layer(path, physical_index, logical_index)?;
        let mut small = full.downsampled_into(max_dim, scratch);
        small.only_layer = Some(logical_index);
        Ok(small)
    }

    /// Decode only the **first valid layer** of an EXR — the beauty/main pass by
    /// the usual VFX convention — for the playback ring cache (#56, hardening
    /// step 3). Cheaper than [`Self::load`] in both decode time and resident
    /// bytes for **multi-part** EXRs (one part per AOV): the block reader filters
    /// out every other part, so their scanlines are never decompressed. A
    /// single-part multichannel file stores all AOVs interleaved in one part's
    /// blocks, so the whole part still decompresses — the saving there is only
    /// the discarded per-channel copy, not the inflate.
    ///
    /// The result is flagged [`beauty_only`](Self::beauty_only): it is correct
    /// for the displayed beauty/active layer but is **not** a valid sampling or
    /// AOV-switch source, so the caller re-decodes the settled frame in full
    /// (INV-SAMPLE, #7) when the clock stops.
    ///
    /// # Errors
    /// Same contract as [`Self::load`].
    pub fn load_beauty(path: impl AsRef<Path>) -> std::result::Result<Self, String> {
        let path_ref = path.as_ref();

        // `first_valid_layer` yields a single `Layer`; re-wrap it in the
        // one-element layer list the rest of the app expects, so beauty-only and
        // full frames are the same `ExrData` shape downstream.
        let read_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let builder = read()
                .no_deep_data()
                .largest_resolution_level()
                .all_channels()
                .first_valid_layer()
                .all_attributes();
            match map_file(path_ref) {
                Some(map) => builder.from_buffered(std::io::Cursor::new(&map[..])),
                None => builder.from_file(path_ref),
            }
        }))
        .map_err(|_| DECODE_PANIC_MSG.to_string())?;

        let single = map_read_err(read_result, path_ref)?;
        let image = FlatImage {
            attributes: single.attributes,
            layer_data: smallvec::smallvec![single.layer_data],
        };
        Ok(Self::from_image(image, true, false))
    }

    /// Decode a **downsampled scrub proxy** (#94), OpenRV "region cache" style: a
    /// normal beauty decode, then box-filtered down to `<= max_dim` on the long
    /// side ([`Self::downsampled`]). The full decode keeps the EXR
    /// display/data-window **geometry** correct — the reason this replaced the old
    /// fast-read, which flattened it and mis-positioned tight-data-window renders.
    /// Only the pixel buffers shrink, so heavy footage **fits** in RAM and
    /// replays/loops/scrubs play smooth from cache, in every compare mode. Flagged
    /// `proxy` (and `beauty_only`) so settle re-decodes full. It does *not* cut the
    /// first decode — a heavy frame still decodes once, then downsamples in.
    ///
    /// # Errors
    /// Same contract as [`Self::load_beauty`].
    pub fn load_proxy(path: impl AsRef<Path>, max_dim: usize) -> std::result::Result<Self, String> {
        Self::load_proxy_into(path, max_dim, &mut Vec::new())
    }

    /// [`Self::load_proxy`] reusing a caller-owned `f32` scratch buffer for the
    /// box-filter accumulator (#171). The decode worker owns one `scratch` across
    /// the whole session, so proxy playback pays the accumulator allocation +
    /// page-zeroing **once** instead of per channel per frame — the decode-side
    /// analogue of the T2 `t2_staging` reuse. Proxies are uniform-size per
    /// sequence, so the buffer never reallocates after the first frame.
    pub fn load_proxy_into(
        path: impl AsRef<Path>,
        max_dim: usize,
        scratch: &mut Vec<f32>,
    ) -> std::result::Result<Self, String> {
        Ok(Self::load_beauty(path)?.downsampled_into(max_dim, scratch))
    }

    /// Box-filter this frame down so its long side is `<= max_dim` (#94). Preserves
    /// the EXR geometry — `image.attributes` (incl. `display_window`) and each
    /// layer's `attributes` (incl. `layer_position`) are kept, and the original
    /// (full) layer size is recorded as [`Self::display_size`] — so the viewport
    /// frames it exactly like the full frame (`viewer.rs:2323`); only the pixel
    /// buffers (→ the GPU texture) shrink and upscale. Output channels keep the
    /// source bit-depth for `F16` (#170) — half the proxy RAM, and an all-f16
    /// layer stays on the `Rgba16Float` upload path (`build_layer_texture`) —
    /// while `F32`/`U32` sources land as `F32` (the filter accumulates in f32
    /// either way). Flagged `proxy`. (Single-layer in practice — the caller
    /// downsamples a `load_beauty` result.)
    #[must_use]
    pub fn downsampled(&self, max_dim: usize) -> Self {
        self.downsampled_into(max_dim, &mut Vec::new())
    }

    /// [`Self::downsampled`] reusing a caller-owned `f32` accumulator (#171), so
    /// the per-channel box-filter scratch is allocated + zeroed once for the whole
    /// session rather than on every proxy decode. See [`Self::load_proxy_into`].
    pub fn downsampled_into(&self, max_dim: usize, scratch: &mut Vec<f32>) -> Self {
        let mut layers: smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]> =
            smallvec::SmallVec::new();
        // The displayed (first/beauty) layer's full data-window size drives framing.
        let mut display_size = None;
        for layer in &self.image.layer_data {
            let (fw, fh) = (layer.size.0, layer.size.1);
            let long = fw.max(fh);
            let factor = if long == 0 || long <= max_dim {
                1
            } else {
                long.div_ceil(max_dim.max(1))
            };
            let dw = fw.div_ceil(factor).max(1);
            let dh = fh.div_ceil(factor).max(1);
            display_size.get_or_insert((fw, fh));

            let mut chans: smallvec::SmallVec<[AnyChannel<FlatSamples>; 4]> =
                smallvec::SmallVec::new();
            for ch in &layer.channel_data.list {
                // Box-filter: average each `factor × factor` block (clamped to the
                // edge) into one output sample. Read via `sample_channel` so the
                // source's F16/F32/U32 type is handled uniformly. The accumulator
                // is the reused `scratch` (#171): `resize` is a no-op after the
                // first uniform-size channel/frame, and every element is fully
                // overwritten below so the fill value is inert.
                scratch.clear();
                scratch.resize(dw * dh, 0.0);
                for py in 0..dh {
                    for px in 0..dw {
                        let (x0, y0) = (px * factor, py * factor);
                        let (x1, y1) = (((px + 1) * factor).min(fw), ((py + 1) * factor).min(fh));
                        let mut acc = 0.0f32;
                        let mut n = 0u32;
                        for y in y0..y1 {
                            for x in x0..x1 {
                                acc += crate::pixels::sample_channel(Some(ch), x, y, fw);
                                n += 1;
                            }
                        }
                        scratch[py * dw + px] = acc / n.max(1) as f32;
                    }
                }
                // Store at the source depth for f16 (#170): the accumulation
                // above already filtered at f32 precision, and rounding the
                // result back to half loses nothing the source ever had. U32
                // stays F32 — a box-filtered ID channel is meaningless either
                // way, and proxies are never a sampling source. The f32 branch
                // must copy out of the shared scratch (it can't be moved).
                let samples = match &ch.sample_data {
                    FlatSamples::F16(_) => {
                        FlatSamples::F16(scratch.iter().map(|&v| f16::from_f32(v)).collect())
                    }
                    FlatSamples::F32(_) | FlatSamples::U32(_) => FlatSamples::F32(scratch.clone()),
                };
                chans.push(AnyChannel::new(ch.name.clone(), samples));
            }
            layers.push(Layer::new(
                (dw, dh),
                layer.attributes.clone(), // preserves layer_position
                layer.encoding,
                AnyChannels::sort(chans),
            ));
        }
        let image = FlatImage {
            attributes: self.image.attributes.clone(), // preserves display_window
            layer_data: layers,
        };
        let mut data = Self::from_image(image, self.beauty_only, true);
        data.display_size = display_size;
        data
    }

    /// Build an [`ExrData`] from a decoded image: regroup channels into logical
    /// layers and precompute the status-bar string. Shared by [`Self::load`],
    /// [`Self::load_beauty`], and [`Self::load_proxy`].
    fn from_image(image: FlatImage, beauty_only: bool, proxy: bool) -> Self {
        let logical_layers = build_logical_layers(&image);
        let channels_str = build_channels_str(&logical_layers);
        Self {
            image,
            logical_layers,
            channels_str,
            beauty_only,
            proxy,
            display_size: None,
            only_layer: None,
        }
    }

    /// Serialize this **proxy** frame to the on-disk-cache blob format (#165) so a
    /// later pass / session can skip the decode. The blob is a raw f16/f32 sample
    /// dump (mmap-able, ~zero decode cost) plus exactly the geometry the viewport
    /// frames by — `display_window`, `pixel_aspect`, per-layer `layer_position` +
    /// `layer_name` + size, and [`Self::display_size`] — so [`Self::from_proxy_blob`]
    /// reconstitutes what [`Self::downsampled`] produced (the #163 invariant). The
    /// `key` (source path + mtime + size + proxy px) is echoed into the header so
    /// the reader can reject a hash collision or a stale entry. Custom `other`
    /// attributes and chromaticities are intentionally dropped — nothing in the
    /// render/geometry path reads them, and the settle re-decode restores them.
    ///
    /// Intended for a proxy (`self.proxy`); serializing a full frame this way is
    /// well-formed but pointless (the caller only caches proxies).
    #[must_use]
    pub fn write_proxy_blob(&self, key: &crate::proxy_cache::ProxyKey) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(PROXY_BLOB_MAGIC);
        out.push(PROXY_BLOB_VERSION);
        out.push((self.beauty_only as u8) | ((self.proxy as u8) << 1));
        // Which AOV this proxy *is* (#217). Restored on read so the reconstructed
        // proxy answers for that AOV and refuses every other one, exactly as the
        // decode it stands in for does.
        match self.only_layer {
            Some(n) => {
                out.push(1);
                out.extend_from_slice(&(n as u32).to_le_bytes());
            }
            None => out.push(0),
        }

        // Echoed key tuple — verified on read to defeat 64-bit filename
        // collisions and catch a stale entry the key hash didn't already exclude.
        out.extend_from_slice(&key.mtime_ns.to_le_bytes());
        out.extend_from_slice(&key.size.to_le_bytes());
        out.extend_from_slice(&key.proxy_px.to_le_bytes());
        out.extend_from_slice(&key.aov.to_le_bytes());
        write_blob_str(&mut out, &key.path);

        // Image geometry: display window + pixel aspect frame the viewport.
        let dw = self.image.attributes.display_window;
        out.extend_from_slice(&dw.position.x().to_le_bytes());
        out.extend_from_slice(&dw.position.y().to_le_bytes());
        out.extend_from_slice(&(dw.size.x() as u32).to_le_bytes());
        out.extend_from_slice(&(dw.size.y() as u32).to_le_bytes());
        out.extend_from_slice(&self.image.attributes.pixel_aspect.to_le_bytes());
        match self.display_size {
            Some((w, h)) => {
                out.push(1);
                out.extend_from_slice(&(w as u32).to_le_bytes());
                out.extend_from_slice(&(h as u32).to_le_bytes());
            }
            None => out.push(0),
        }

        out.extend_from_slice(&(self.image.layer_data.len() as u16).to_le_bytes());
        for layer in &self.image.layer_data {
            out.extend_from_slice(&layer.attributes.layer_position.x().to_le_bytes());
            out.extend_from_slice(&layer.attributes.layer_position.y().to_le_bytes());
            out.extend_from_slice(&(layer.size.0 as u32).to_le_bytes());
            out.extend_from_slice(&(layer.size.1 as u32).to_le_bytes());
            match &layer.attributes.layer_name {
                Some(name) => {
                    out.push(1);
                    write_blob_str(&mut out, &name.to_string());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&(layer.channel_data.list.len() as u16).to_le_bytes());
            for ch in &layer.channel_data.list {
                write_blob_str(&mut out, &ch.name.to_string());
                match &ch.sample_data {
                    FlatSamples::F16(v) => {
                        out.push(SAMPLE_TAG_F16);
                        for s in v {
                            out.extend_from_slice(&s.to_bits().to_le_bytes());
                        }
                    }
                    FlatSamples::F32(v) => {
                        out.push(SAMPLE_TAG_F32);
                        for s in v {
                            out.extend_from_slice(&s.to_le_bytes());
                        }
                    }
                    // A proxy never carries U32 (downsampled maps it to F32);
                    // handled defensively so this stays total — normalize like
                    // `pixels::sample_channel` and store as F32.
                    FlatSamples::U32(v) => {
                        out.push(SAMPLE_TAG_F32);
                        for s in v {
                            out.extend_from_slice(&(*s as f32 / u32::MAX as f32).to_le_bytes());
                        }
                    }
                }
            }
        }
        out
    }

    /// Reconstruct a proxy [`ExrData`] from an on-disk-cache blob (#165), the
    /// inverse of [`Self::write_proxy_blob`]. Returns `None` on any mismatch —
    /// bad magic, wrong version, a truncated / corrupt body, or an echoed `key`
    /// that doesn't match the request (a hash collision or a re-rendered source)
    /// — so the caller treats it as a cache miss and decodes normally. Never
    /// panics and never yields wrong pixels for the wrong request.
    #[must_use]
    pub fn from_proxy_blob(bytes: &[u8], key: &crate::proxy_cache::ProxyKey) -> Option<Self> {
        let mut r = BlobReader::new(bytes);
        if r.take(PROXY_BLOB_MAGIC.len())? != PROXY_BLOB_MAGIC {
            return None;
        }
        if r.u8()? != PROXY_BLOB_VERSION {
            return None;
        }
        let flags = r.u8()?;
        let beauty_only = flags & 1 != 0;
        let proxy = flags & 2 != 0;
        let only_layer = match r.u8()? {
            0 => None,
            _ => Some(r.u32()? as usize),
        };

        // Reject a collision / stale entry: the echoed tuple must match the key
        // the caller hashed to find this file. `aov` is in here for #217: without
        // it, layer 1's cached proxy is a verified "hit" for layer 2 and the wrong
        // pass renders with nothing failing.
        if r.u64()? != key.mtime_ns
            || r.u64()? != key.size
            || r.u32()? != key.proxy_px
            || r.u32()? != key.aov
            || r.str()? != key.path
        {
            return None;
        }

        let dw = IntegerBounds::new((r.i32()?, r.i32()?), (r.u32()? as usize, r.u32()? as usize));
        let pixel_aspect = r.f32()?;
        let display_size = match r.u8()? {
            0 => None,
            _ => Some((r.u32()? as usize, r.u32()? as usize)),
        };

        let layer_count = r.u16()? as usize;
        if layer_count > MAX_PROXY_LAYERS {
            return None;
        }
        let mut layer_data: smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]> =
            smallvec::SmallVec::new();
        for _ in 0..layer_count {
            let layer_position = Vec2(r.i32()?, r.i32()?);
            let (lw, lh) = (r.u32()? as usize, r.u32()? as usize);
            let n = lw.checked_mul(lh)?;
            let layer_name = match r.u8()? {
                0 => None,
                _ => Some(Text::from(r.str()?.as_str())),
            };
            let chan_count = r.u16()? as usize;
            if chan_count > MAX_PROXY_CHANNELS {
                return None;
            }
            let mut chans: smallvec::SmallVec<[AnyChannel<FlatSamples>; 4]> =
                smallvec::SmallVec::new();
            for _ in 0..chan_count {
                let name = r.str()?;
                let samples = match r.u8()? {
                    SAMPLE_TAG_F16 => {
                        // Reject before allocating if the claimed sample count
                        // can't fit in the bytes left — a corrupt `n` can't force
                        // an allocation larger than the blob itself.
                        if n.checked_mul(2)? > r.remaining() {
                            return None;
                        }
                        let mut v = Vec::with_capacity(n);
                        for _ in 0..n {
                            v.push(f16::from_bits(r.u16()?));
                        }
                        FlatSamples::F16(v)
                    }
                    SAMPLE_TAG_F32 => {
                        if n.checked_mul(4)? > r.remaining() {
                            return None;
                        }
                        let mut v = Vec::with_capacity(n);
                        for _ in 0..n {
                            v.push(r.f32()?);
                        }
                        FlatSamples::F32(v)
                    }
                    _ => return None,
                };
                chans.push(AnyChannel::new(Text::from(name.as_str()), samples));
            }
            let attributes = LayerAttributes {
                layer_position,
                layer_name,
                ..LayerAttributes::default()
            };
            layer_data.push(Layer::new(
                (lw, lh),
                attributes,
                Encoding::UNCOMPRESSED,
                AnyChannels::sort(chans),
            ));
        }

        let mut attributes = ImageAttributes::new(dw);
        attributes.pixel_aspect = pixel_aspect;
        let image = FlatImage {
            attributes,
            layer_data,
        };
        let mut data = Self::from_image(image, beauty_only, proxy);
        data.display_size = display_size;
        // A per-AOV proxy (#217) must claim the same AOV it was written for, and —
        // via `resolve_logical` — refuse every other. A blob claiming an AOV it
        // can't address is rejected outright rather than restored into the state
        // `load_layer` itself refuses to produce.
        if let Some(n) = only_layer {
            if data.logical_layers.len() != 1 {
                return None;
            }
            data.only_layer = Some(n);
        }
        Some(data)
    }

    /// Map a caller's logical index (in the *full* image's table) onto this data's
    /// own, which differ for a single-layer decode (#217). Identity for a decode
    /// that carries the whole image; `None` when this data **cannot** answer for
    /// `idx` at all.
    ///
    /// Refusing is the important half. A single-layer decode holds one entry, so a
    /// lenient fallthrough would hand `logical_channels(0)` the entry it *does*
    /// have — which is AOV 3's pixels — and the caller would render them as AOV 0
    /// without a single failed call. The T1 ring is keyed `(source, frame)` with no
    /// AOV in the key, so switching a layer's AOV leaves exactly those stale frames
    /// resident and asks them for the new index. Wrong pixels, silently, is worse
    /// than the freeze #213 was about; returning `None` instead makes the texture
    /// build fail, which `collect_comp_textures` already turns into an evict +
    /// re-decode.
    ///
    /// Only remaps when this data holds exactly one logical layer — the
    /// multi-part-with-one-AOV-per-part case. A single-part file decodes its whole
    /// part, so every logical layer is present and already correctly numbered.
    #[must_use]
    fn resolve_logical(&self, idx: usize) -> Option<usize> {
        match self.only_layer {
            // A whole-image decode: the caller's numbering is ours.
            None => Some(idx),
            Some(n) if n == idx && self.logical_layers.len() == 1 => Some(0),
            // A partial decode asked for an AOV it does not carry.
            Some(_) => None,
        }
    }

    /// The [`LogicalLayer`] a caller's index names, honouring the single-layer
    /// remap above (#217). Prefer this over indexing `logical_layers` directly
    /// wherever `idx` is a *caller's* AOV rather than a position in this data's own
    /// table — during playback the displayed frame may be a one-layer decode, and a
    /// raw `logical_layers.get(aov)` then reads the wrong entry or none at all.
    #[must_use]
    pub fn logical_layer(&self, idx: usize) -> Option<&LogicalLayer> {
        self.logical_layers.get(self.resolve_logical(idx)?)
    }

    /// `(width, height)` of the given logical layer **for display/framing**. For a
    /// proxy (#94) this is the full-res [`Self::display_size`] (so the viewport
    /// frames it at full size and the small texture upscales); otherwise it's the
    /// physical layer's pixel dims. Note: texture packing reads the real buffer
    /// dims via [`Self::logical_channels`] (`layer.size`) directly, so it is
    /// unaffected by this override.
    #[must_use]
    pub fn logical_size(&self, idx: usize) -> Option<(usize, usize)> {
        let ll = self.logical_layer(idx)?;
        if let Some(ds) = self.display_size {
            return Some(ds);
        }
        let layer = self.image.layer_data.get(ll.physical_index)?;
        Some((layer.size.0, layer.size.1))
    }

    /// Resolve a logical layer to its physical layer plus the `(r, g, b, a)`
    /// channels it maps to. Channels are looked up by the indices recorded at
    /// load time, so no per-call name matching is needed.
    #[allow(clippy::type_complexity)]
    #[must_use]
    pub fn logical_channels(
        &self,
        idx: usize,
    ) -> Option<(
        &Layer<AnyChannels<FlatSamples>>,
        Option<&exr::image::AnyChannel<FlatSamples>>,
        Option<&exr::image::AnyChannel<FlatSamples>>,
        Option<&exr::image::AnyChannel<FlatSamples>>,
        Option<&exr::image::AnyChannel<FlatSamples>>,
    )> {
        let ll = self.logical_layer(idx)?;
        let layer = self.image.layer_data.get(ll.physical_index)?;
        let list = &layer.channel_data.list;
        let get = |o: Option<usize>| o.and_then(|i| list.get(i));
        Some((layer, get(ll.r), get(ll.g), get(ll.b), get(ll.a)))
    }

    /// Approximate resident size in bytes: the sum of every physical channel
    /// buffer across every physical layer (sample count × sample size).
    ///
    /// This is the per-frame RAM-accounting primitive the playback ring cache
    /// (#56) budgets against (tier T1). It counts the decoded pixel buffers only
    /// — the dominant cost — and ignores small bookkeeping (`logical_layers`,
    /// channel names, attributes), so it is a lower bound on true heap use, but
    /// a tight one for the sequences playback cares about. Cheap: it reads
    /// `Vec::len`, it does not walk samples.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        // Saturating so a malformed/huge image can't wrap to a tiny figure and
        // under-budget the cache; it pins at usize::MAX (treated as "won't fit").
        self.image
            .layer_data
            .iter()
            .flat_map(|layer| layer.channel_data.list.iter())
            .map(|channel| match &channel.sample_data {
                // OpenEXR sample sizes are fixed by the format: half=2, float=4, uint=4.
                FlatSamples::F16(s) => s.len().saturating_mul(2),
                FlatSamples::F32(s) => s.len().saturating_mul(4),
                FlatSamples::U32(s) => s.len().saturating_mul(4),
            })
            .fold(0usize, usize::saturating_add)
    }
}

/// Memory-map `path` for zero-copy decode (#164). `from_file` streams through a
/// `BufReader`, paying a copy per read syscall; mapping lets the parse pull
/// straight from the page cache — which the prefetch warmer
/// (`crate::prefetch`) has ideally already filled from a background thread, so
/// the decode's "read" is a pointer walk over warm pages. Cold files still
/// win: `Advice::Sequential` turns page faults into kernel readahead. This is
/// what OpenRV ships as its EXR default ("Memory Map. Based on performance
/// tuning") and how tlRender/mrv2 read EXR (mmap'd `Imf::IStream`s).
///
/// Returns `None` when the file can't be opened or mapped (empty file, exotic
/// filesystem) — callers fall back to the streaming `from_file` path, which
/// also produces the friendlier open-error messages.
///
/// Safety: the mapping is read-only but **shared** (not copy-on-write), so
/// concurrent in-place writes to the file are visible through it — a torn
/// read mid-parse yields garbage pixels or a decode error, not UB. The abort
/// risk is another process *truncating* the file mid-parse (SIGBUS).
/// Renderers conventionally write-to-temp-then-rename — the mapped (old)
/// inode stays valid — and the render-watch (#101) invalidates re-rendered
/// frames; an in-place writer truncating during the parse remains a
/// theoretical abort, the same trade OpenRV and tlRender accept for their
/// mmap'd EXR reads.
fn map_file(path: &Path) -> Option<memmap2::Mmap> {
    let file = std::fs::File::open(path).ok()?;
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    // Best-effort readahead hint; `advise` (madvise) is Unix-only in memmap2 —
    // Windows' own mapped-file readahead heuristics apply there.
    #[cfg(unix)]
    let _ = map.advise(memmap2::Advice::Sequential);
    Some(map)
}

/// Map an `exr` read error to a human-readable string, with a friendlier
/// message (and a hex/ASCII dump of the first four bytes) when the file is
/// missing the OpenEXR magic number. Generic over the read result so both the
/// full (`all_layers`) and beauty-only (`first_valid_layer`) reads share it.
fn map_read_err<T>(
    read_result: std::result::Result<T, exr::error::Error>,
    path: &Path,
) -> std::result::Result<T, String> {
    read_result.map_err(|e| {
        let err_str = e.to_string();
        if err_str.contains("file identifier missing") {
            // Try to read the first 4 bytes to help the user.
            if let Ok(mut f) = std::fs::File::open(path) {
                use std::io::Read;
                let mut buf = [0u8; 4];
                if f.read_exact(&mut buf).is_ok() {
                    let hex_str =
                        format!("{:02X} {:02X} {:02X} {:02X}", buf[0], buf[1], buf[2], buf[3]);
                    let ascii_str: String = buf
                        .iter()
                        .map(|&b| if (32..=126).contains(&b) { b as char } else { '.' })
                        .collect();
                    return format!(
                        "Not a valid EXR file (magic number missing).\nFirst 4 bytes: [{hex_str}] ('{ascii_str}')\nMake sure this is actually an OpenEXR file and not a renamed PNG, JPG, or corrupted file."
                    );
                }
            }
            "Not a valid EXR file (magic number missing). The file might be corrupted or in another format.".to_string()
        } else {
            err_str
        }
    })
}

/// Append a length-prefixed (`u32` LE) UTF-8 string to a proxy-cache blob.
fn write_blob_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Bounds-checked little-endian cursor over a proxy-cache blob (#165). Every
/// accessor returns `Option`, so a truncated or corrupt blob short-circuits to a
/// cache miss ([`ExrData::from_proxy_blob`]) instead of panicking.
struct BlobReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BlobReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        // `take` bounds `len` to the bytes left, so a corrupt length can't
        // allocate past the blob's own size.
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
    /// Bytes not yet consumed — used to reject a claimed sample count that
    /// couldn't possibly fit before allocating a buffer for it.
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }
}

/// Build the truncated, comma-joined display string of logical-layer names.
/// Computed once at load time and cached on `ExrData` so the status bar
/// doesn't rebuild it every frame (Blender EXRs can have 100+ layers).
fn build_channels_str(layers: &[LogicalLayer]) -> String {
    let mut s = String::new();
    for name in layers.iter().map(|l| l.name.as_str()) {
        if !s.is_empty() {
            s.push(',');
        }
        s.push_str(name);
        if s.len() > 50 {
            s.truncate(50);
            s.push_str("...");
            break;
        }
    }
    s
}

/// Build the logical-layer list for a loaded image by grouping each physical
/// layer's channels on their dotted prefix, then applying smart display names.
fn build_logical_layers(
    image: &Image<smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]>>,
) -> Vec<LogicalLayer> {
    let mut result = Vec::new();
    for (physical_index, layer) in image.layer_data.iter().enumerate() {
        let phys_name = layer.attributes.layer_name.as_ref().map(|t| t.to_string());
        let names: Vec<String> = layer
            .channel_data
            .list
            .iter()
            .map(|c| c.name.to_string())
            .collect();
        for raw in group_channels(&names) {
            result.push(LogicalLayer {
                name: String::new(),
                group_key: combine_key(phys_name.as_deref(), &raw.group_key),
                physical_index,
                r: raw.r,
                g: raw.g,
                b: raw.b,
                a: raw.a,
            });
        }
    }
    apply_smart_names(&mut result);
    result
}

/// A channel group before display naming: the prefix and the resolved slots.
struct RawGroup {
    group_key: String,
    r: Option<usize>,
    g: Option<usize>,
    b: Option<usize>,
    a: Option<usize>,
}

/// Split a channel name into `(prefix, component_token)` on the last `.`.
/// `"ViewLayer.Combined.R"` -> `("ViewLayer.Combined", "R")`; `"R"` -> `("", "R")`.
fn split_channel_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((prefix, token)) => (prefix, token),
        None => ("", name),
    }
}

/// Map a canonical color token to an RGBA slot (`0=r, 1=g, 2=b, 3=a`), or `None`.
/// This is the high-priority tier: a real `R/G/B/A` always wins its slot.
fn color_slot(token: &str) -> Option<usize> {
    let eq = |s: &str| token.eq_ignore_ascii_case(s);
    if eq("R") || eq("red") {
        Some(0)
    } else if eq("G") || eq("green") {
        Some(1)
    } else if eq("B") || eq("blue") {
        Some(2)
    } else if eq("A") || eq("alpha") {
        Some(3)
    } else {
        None
    }
}

/// Map a geometric/vector token (normal, position, depth) to a display slot:
/// `X->r, Y->g, Z->b`. This is the low-priority tier and only fills slots a
/// `color_slot` match left empty, so a depth `Z` never overwrites a real Blue.
fn vector_slot(token: &str) -> Option<usize> {
    let eq = |s: &str| token.eq_ignore_ascii_case(s);
    if eq("X") {
        Some(0)
    } else if eq("Y") {
        Some(1)
    } else if eq("Z") {
        Some(2)
    } else {
        None
    }
}

/// Group channel names by prefix, preserving first-seen order, and assign each
/// group's channels to r/g/b/a slots. A single-channel group renders as
/// grayscale (r=g=b); a multi-channel group with no recognizable color
/// component falls back to replicating its first channel.
fn group_channels(names: &[String]) -> Vec<RawGroup> {
    use std::collections::HashMap;

    // First-seen prefix order with a side index map so lookup is O(1): grouping
    // stays O(n) on channel count rather than O(n^2). Blender EXRs routinely have
    // 50-150+ channels, so this runs on the load hot path. Prefixes and tokens are
    // borrowed `&str` from `names` to avoid a `String` allocation per channel; the
    // only owned allocation is the final `group_key` (one per group).
    let mut order: Vec<&str> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut members: Vec<Vec<(usize, &str)>> = Vec::new();

    for (idx, name) in names.iter().enumerate() {
        let (prefix, token) = split_channel_name(name);
        let pos = *index.entry(prefix).or_insert_with(|| {
            order.push(prefix);
            members.push(Vec::new());
            order.len() - 1
        });
        members[pos].push((idx, token));
    }

    let mut groups: Vec<RawGroup> = Vec::new();
    for (group_key, mem) in order.into_iter().zip(members) {
        let (mut r, mut g, mut b, mut a) = (None, None, None, None);

        if mem.len() == 1 {
            // Single-channel pass (Z-depth, mist, a mask): show as grayscale.
            let only = mem[0].0;
            groups.push(RawGroup {
                group_key: group_key.to_string(),
                r: Some(only),
                g: Some(only),
                b: Some(only),
                a: None,
            });
            continue;
        }

        // Pass 1: canonical colors (R/G/B/A) claim their slot outright.
        for &(ci, token) in &mem {
            match color_slot(token) {
                Some(0) => r = Some(ci),
                Some(1) => g = Some(ci),
                Some(2) => b = Some(ci),
                Some(3) => a = Some(ci),
                _ => {}
            }
        }
        // Pass 2: vector aliases (X/Y/Z) fill only still-empty slots, so a depth
        // `Z` packed next to RGBA can never overwrite the Blue pass 1 resolved.
        for &(ci, token) in &mem {
            match vector_slot(token) {
                Some(0) if r.is_none() => r = Some(ci),
                Some(1) if g.is_none() => g = Some(ci),
                Some(2) if b.is_none() => b = Some(ci),
                _ => {}
            }
        }

        if r.is_none() && g.is_none() && b.is_none() {
            // Unrecognized multi-channel pass: replicate the first channel and
            // keep it as one group (no per-channel splitting of e.g. UV/chroma).
            let first = mem[0].0;
            groups.push(RawGroup {
                group_key: group_key.to_string(),
                r: Some(first),
                g: Some(first),
                b: Some(first),
                a,
            });
            continue;
        }

        // A genuine color group: emit it, then surface any leftover member
        // channels (e.g. a depth `Z` stored alongside RGBA) as their own
        // grayscale layers so they stay viewable instead of being dropped.
        let claimed = [r, g, b, a];
        groups.push(RawGroup {
            group_key: group_key.to_string(),
            r,
            g,
            b,
            a,
        });
        for &(ci, _token) in &mem {
            if claimed.contains(&Some(ci)) {
                continue;
            }
            groups.push(RawGroup {
                group_key: names[ci].clone(),
                r: Some(ci),
                g: Some(ci),
                b: Some(ci),
                a: None,
            });
        }
    }
    groups
}

/// Combine a physical layer name with a channel-prefix into one full group key.
fn combine_key(phys: Option<&str>, group_key: &str) -> String {
    match (phys, group_key.is_empty()) {
        (Some(p), false) => format!("{p}.{group_key}"),
        (Some(p), true) => p.to_string(),
        (None, false) => group_key.to_string(),
        (None, true) => String::new(),
    }
}

/// Fill in each layer's display `name`: strip a leading dotted prefix shared by
/// every keyed layer (e.g. the Blender view-layer name), keep the remainder,
/// and label unprefixed root channels `RGBA`.
fn apply_smart_names(layers: &mut [LogicalLayer]) {
    let keys: Vec<&str> = layers
        .iter()
        .map(|l| l.group_key.as_str())
        .filter(|k| !k.is_empty())
        .collect();
    let prefix = longest_common_dotted_prefix(&keys);

    for l in layers.iter_mut() {
        let mut name = if l.group_key.is_empty() {
            "RGBA".to_string()
        } else if !prefix.is_empty() && l.group_key.len() > prefix.len() {
            l.group_key[prefix.len()..].to_string()
        } else {
            l.group_key.clone()
        };

        // If the resulting name is exactly `part.part` (like `combineddiffuse.combineddiffuse`),
        // deduplicate it to just `part` to match standard Nuke formatting.
        if let Some((left, right)) = name.split_once('.')
            && left == right
        {
            name = left.to_string();
        }

        l.name = name;
    }
}

/// Longest prefix of whole dotted segments shared by all keys, including the
/// trailing dot, never consuming a key's final segment. Empty if fewer than two
/// keys or nothing is shared. `["ViewLayer.Combined", "ViewLayer.Normal"]` ->
/// `"ViewLayer."`.
fn longest_common_dotted_prefix(keys: &[&str]) -> String {
    if keys.len() < 2 {
        return String::new();
    }
    let split: Vec<Vec<&str>> = keys.iter().map(|k| k.split('.').collect()).collect();
    let min_segs = split.iter().map(|s| s.len()).min().unwrap_or(0);
    let max_take = min_segs.saturating_sub(1); // keep at least one segment as the name
    let mut k = 0;
    while k < max_take {
        let seg = split[0][k];
        if split.iter().all(|s| s[k] == seg) {
            k += 1;
        } else {
            break;
        }
    }
    if k == 0 {
        String::new()
    } else {
        format!("{}.", split[0][..k].join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_dotted_and_bare_names() {
        assert_eq!(
            split_channel_name("ViewLayer.Combined.R"),
            ("ViewLayer.Combined", "R")
        );
        assert_eq!(split_channel_name("R"), ("", "R"));
        assert_eq!(split_channel_name("Depth.Z"), ("Depth", "Z"));
    }

    #[test]
    fn maps_color_and_vector_components() {
        // Canonical colors are the high-priority tier.
        assert_eq!(color_slot("R"), Some(0));
        assert_eq!(color_slot("g"), Some(1));
        assert_eq!(color_slot("BLUE"), Some(2));
        assert_eq!(color_slot("A"), Some(3));
        assert_eq!(color_slot("mask"), None);
        // Vector aliases are a separate, lower-priority tier and do NOT match
        // color names (so a Blue is never resolved via the X/Y/Z path).
        assert_eq!(vector_slot("X"), Some(0));
        assert_eq!(vector_slot("Y"), Some(1));
        assert_eq!(vector_slot("Z"), Some(2));
        assert_eq!(vector_slot("R"), None);
        assert_eq!(vector_slot("B"), None);
    }

    #[test]
    fn groups_blender_passes_regardless_of_channel_order() {
        // Blender commonly stores channels in B,G,R,A order.
        let g = group_channels(&names(&[
            "ViewLayer.Combined.B",
            "ViewLayer.Combined.G",
            "ViewLayer.Combined.R",
            "ViewLayer.Combined.A",
            "ViewLayer.Depth.Z",
            "ViewLayer.Normal.X",
            "ViewLayer.Normal.Y",
            "ViewLayer.Normal.Z",
        ]));
        assert_eq!(g.len(), 3);

        let combined = &g[0];
        assert_eq!(combined.group_key, "ViewLayer.Combined");
        assert_eq!(
            (combined.r, combined.g, combined.b, combined.a),
            (Some(2), Some(1), Some(0), Some(3))
        );

        let depth = &g[1];
        assert_eq!(depth.group_key, "ViewLayer.Depth");
        // Single channel -> grayscale, no alpha.
        assert_eq!(
            (depth.r, depth.g, depth.b, depth.a),
            (Some(4), Some(4), Some(4), None)
        );

        let normal = &g[2];
        assert_eq!(normal.group_key, "ViewLayer.Normal");
        assert_eq!(
            (normal.r, normal.g, normal.b, normal.a),
            (Some(5), Some(6), Some(7), None)
        );
    }

    #[test]
    fn groups_unprefixed_root_channels() {
        let g = group_channels(&names(&["R", "G", "B", "A"]));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].group_key, "");
        assert_eq!(
            (g[0].r, g[0].g, g[0].b, g[0].a),
            (Some(0), Some(1), Some(2), Some(3))
        );
    }

    #[test]
    fn depth_z_does_not_clobber_blue_in_rgba_group() {
        // Regression: a Blender beauty pass with an unprefixed RGBA layer plus a
        // depth `Z` channel. EXR stores channels alphabetically on disk, so the
        // list order is A, B, G, R, Z (indices 0..=4). The Blue slot must resolve
        // to the real `B` channel (index 1), NOT the `Z` depth channel (index 4),
        // which previously overwrote it and rendered pure white.
        let g = group_channels(&names(&["A", "B", "G", "R", "Z"]));

        // The RGBA color group comes first, then the leftover depth channel.
        assert_eq!(g.len(), 2);

        let rgba = &g[0];
        assert_eq!(rgba.group_key, "");
        assert_eq!(
            (rgba.r, rgba.g, rgba.b, rgba.a),
            (Some(3), Some(2), Some(1), Some(0)),
            "blue must point at B (idx 1), not Z (idx 4)"
        );

        // The depth Z surfaces as its own grayscale layer.
        let depth = &g[1];
        assert_eq!(depth.group_key, "Z");
        assert_eq!(
            (depth.r, depth.g, depth.b, depth.a),
            (Some(4), Some(4), Some(4), None)
        );
    }

    #[test]
    fn vector_pass_maps_xyz_without_leftovers() {
        // A pure normal pass (no R/G/B) still maps X/Y/Z -> r/g/b and emits no
        // extra grayscale layer.
        let g = group_channels(&names(&["N.X", "N.Y", "N.Z"]));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].group_key, "N");
        assert_eq!(
            (g[0].r, g[0].g, g[0].b, g[0].a),
            (Some(0), Some(1), Some(2), None)
        );
    }

    #[test]
    fn finds_common_view_layer_prefix() {
        assert_eq!(
            longest_common_dotted_prefix(&["ViewLayer.Combined", "ViewLayer.Normal"]),
            "ViewLayer."
        );
        // No shared leading segment.
        assert_eq!(longest_common_dotted_prefix(&["A.R", "B.R"]), "");
        // Fewer than two keys.
        assert_eq!(longest_common_dotted_prefix(&["ViewLayer.Combined"]), "");
    }

    #[test]
    fn smart_names_strip_shared_prefix_and_label_root() {
        let mut layers = vec![
            LogicalLayer {
                name: String::new(),
                group_key: "ViewLayer.Combined".into(),
                physical_index: 0,
                r: None,
                g: None,
                b: None,
                a: None,
            },
            LogicalLayer {
                name: String::new(),
                group_key: "ViewLayer.Normal".into(),
                physical_index: 0,
                r: None,
                g: None,
                b: None,
                a: None,
            },
        ];
        apply_smart_names(&mut layers);
        assert_eq!(layers[0].name, "Combined");
        assert_eq!(layers[1].name, "Normal");

        let mut root = vec![LogicalLayer {
            name: String::new(),
            group_key: String::new(),
            physical_index: 0,
            r: Some(0),
            g: Some(1),
            b: Some(2),
            a: Some(3),
        }];
        apply_smart_names(&mut root);
        assert_eq!(root[0].name, "RGBA");
    }

    // --- End-to-end `ExrData::load` integration tests -----------------------
    // These generate tiny EXRs in a temp dir (no committed binaries) and drive
    // the full parse + regrouping path, complementing the pure-helper tests above.

    use std::collections::HashMap;

    /// Write a Blender-style single-part EXR: one unnamed physical layer whose
    /// channel names encode multiple passes by dotted prefix.
    fn write_blender_exr(path: &Path) {
        const W: usize = 2;
        const H: usize = 2;
        let mut list = smallvec::SmallVec::new();
        for name in [
            "ViewLayer.Combined.R",
            "ViewLayer.Combined.G",
            "ViewLayer.Combined.B",
            "ViewLayer.Combined.A",
            "ViewLayer.Depth.Z",
        ] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F32(vec![0.5; W * H]),
            ));
        }
        let layer = Layer::new(
            (W, H),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer)
            .write()
            .to_file(path)
            .expect("write blender-style exr");
    }

    #[test]
    fn load_regroups_blender_passes_into_logical_layers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blender.exr");
        write_blender_exr(&path);

        let data = ExrData::load(&path).expect("load generated exr");

        // Two passes recovered from the single physical layer: Combined + Depth.
        assert_eq!(data.logical_layers.len(), 2, "expected Combined + Depth");
        let by_name: HashMap<&str, &LogicalLayer> = data
            .logical_layers
            .iter()
            .map(|l| (l.name.as_str(), l))
            .collect();

        let combined = by_name.get("Combined").expect("Combined pass present");
        assert!(
            combined.r.is_some()
                && combined.g.is_some()
                && combined.b.is_some()
                && combined.a.is_some(),
            "Combined must resolve all four RGBA slots"
        );

        let depth = by_name.get("Depth").expect("Depth pass present");
        // Single channel renders as grayscale (r=g=b) with no alpha.
        assert_eq!(depth.r, depth.g);
        assert_eq!(depth.g, depth.b);
        assert_eq!(depth.a, None);
    }

    #[test]
    fn logical_size_and_channels_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blender.exr");
        write_blender_exr(&path);
        let data = ExrData::load(&path).unwrap();

        // Layer 0 is the Combined (RGBA) pass.
        assert_eq!(data.logical_size(0), Some((2, 2)));
        let (_layer, r, g, b, a) = data.logical_channels(0).expect("channels for layer 0");
        assert!(r.is_some() && g.is_some() && b.is_some() && a.is_some());

        // Out-of-range indices are handled gracefully.
        assert!(data.logical_size(99).is_none());
        assert!(data.logical_channels(99).is_none());
    }

    #[test]
    fn load_rejects_non_exr_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_an.exr");
        std::fs::write(&path, b"this is plainly not an exr file").unwrap();

        // `ExrData` isn't `Debug`, so match rather than `expect_err`.
        let err = match ExrData::load(&path) {
            Ok(_) => panic!("garbage must not parse as EXR"),
            Err(e) => e,
        };
        let lower = err.to_lowercase();
        assert!(
            lower.contains("magic number")
                || lower.contains("valid exr")
                || lower.contains("identifier"),
            "error should explain the bad EXR, got: {err}"
        );
    }

    // --- `approx_bytes` (the #56 T1 RAM-accounting primitive) ----------------

    /// Write a single-part EXR whose channels are `(name, is_half)` pairs, so a
    /// test can pin the exact decoded buffer sizes. Half channels round-trip as
    /// `F16` (2 bytes/sample), float channels as `F32` (4 bytes/sample).
    fn write_typed_exr(path: &Path, w: usize, h: usize, channels: &[(&str, bool)]) {
        let mut list = smallvec::SmallVec::new();
        for (name, is_half) in channels {
            let samples = if *is_half {
                FlatSamples::F16(vec![f16::from_f32(0.5); w * h])
            } else {
                FlatSamples::F32(vec![0.5; w * h])
            };
            list.push(AnyChannel::new(Text::from(*name), samples));
        }
        let layer = Layer::new(
            (w, h),
            LayerAttributes::default(),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer)
            .write()
            .to_file(path)
            .expect("write typed exr fixture");
    }

    #[test]
    fn approx_bytes_sums_all_channel_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let rgba = &[("R", false), ("G", false), ("B", false), ("A", false)];

        // 2x2 RGBA F32: 4 channels × 4 px × 4 bytes = 64.
        let p = dir.path().join("rgba.exr");
        write_typed_exr(&p, 2, 2, rgba);
        assert_eq!(ExrData::load(&p).unwrap().approx_bytes(), 4 * 4 * 4);

        // Larger frame scales linearly with pixel count: 8×4 px = 32.
        let big = dir.path().join("big.exr");
        write_typed_exr(&big, 8, 4, rgba);
        assert_eq!(ExrData::load(&big).unwrap().approx_bytes(), 4 * 32 * 4);
    }

    #[test]
    fn approx_bytes_counts_per_sample_type() {
        let dir = tempfile::tempdir().unwrap();
        // 2×2 with two half + two float channels:
        // half  = 2 × (4 px × 2 bytes) = 16
        // float = 2 × (4 px × 4 bytes) = 32  -> 48 total.
        let p = dir.path().join("mixed.exr");
        write_typed_exr(
            &p,
            2,
            2,
            &[("R", true), ("G", true), ("B", false), ("A", false)],
        );
        assert_eq!(ExrData::load(&p).unwrap().approx_bytes(), 16 + 32);
    }

    // --- Scrub proxy (#94) ---------------------------------------------------

    /// Write a tall **uncompressed** RGBA EXR (1 scanline per block) so the
    /// fast-read proxy has enough blocks to subsample (`block_stride >= 2`).
    fn write_tall_rgba(path: &Path, w: usize, h: usize) {
        let mut list = smallvec::SmallVec::new();
        for name in ["R", "G", "B", "A"] {
            list.push(AnyChannel::new(
                Text::from(name),
                FlatSamples::F32(vec![0.5; w * h]),
            ));
        }
        let layer = Layer::new(
            (w, h),
            LayerAttributes::default(),
            Encoding::UNCOMPRESSED,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer)
            .write()
            .to_file(path)
            .expect("write tall exr fixture");
    }

    #[test]
    fn load_proxy_builds_a_tiny_beauty_frame() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("tall.exr");
        write_tall_rgba(&p, 16, 200); // uncompressed → 200 blocks

        let full = ExrData::load(&p).unwrap();
        let proxy = ExrData::load_proxy(&p, 48).expect("proxy for a tall image");

        assert!(proxy.proxy, "flagged as a proxy");
        assert!(
            proxy.beauty_only,
            "proxy implies beauty-only so settle re-decodes it full (#94/#7)"
        );
        assert_eq!(proxy.logical_layers.len(), 1, "a single beauty layer");
        // Frames at FULL res so it upscales into the full image rect (#94)...
        assert_eq!(
            proxy.display_size,
            Some((16, 200)),
            "logical_size reports the full display dims"
        );
        assert_eq!(proxy.logical_size(0), Some((16, 200)));
        // ...while the actual decoded buffer is downsampled far smaller.
        let (layer, ..) = proxy.logical_channels(0).unwrap();
        assert!(
            layer.size.0 <= 16 && layer.size.1 < 200,
            "buffer downsampled: {}x{}",
            layer.size.0,
            layer.size.1
        );
        assert!(
            proxy.approx_bytes() < full.approx_bytes(),
            "proxy is much smaller than the full frame"
        );
    }

    #[test]
    fn load_proxy_of_a_small_file_is_a_no_op_downsample() {
        // An image already `<= max_dim` on its long side needs no downsampling:
        // load_proxy still succeeds (post-decode model, unlike the old fast read
        // which bailed) — flagged proxy, dims unchanged.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("small.exr");
        write_typed_exr(
            &p,
            2,
            2,
            &[("R", false), ("G", false), ("B", false), ("A", false)],
        );
        let proxy = ExrData::load_proxy(&p, 1024).expect("small file still decodes");
        assert!(proxy.proxy && proxy.beauty_only);
        assert_eq!(
            proxy.logical_size(0),
            Some((2, 2)),
            "no downsample under max_dim"
        );
    }

    #[test]
    fn downsampled_preserves_display_and_data_window() {
        // The whole reason for the post-decode rework (#94): downsampling must keep
        // the EXR geometry the viewport positions by, so a proxy frames like the
        // full frame instead of the old fast-read's mis-positioned quadrant.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("g.exr");
        write_tall_rgba(&p, 16, 200);
        let full = ExrData::load(&p).unwrap();
        let proxy = full.downsampled(48);

        assert_eq!(
            proxy.image.attributes.display_window, full.image.attributes.display_window,
            "display window preserved"
        );
        assert_eq!(
            proxy.image.layer_data[0].attributes.layer_position,
            full.image.layer_data[0].attributes.layer_position,
            "data-window (layer) position preserved"
        );
        // Framing reports the FULL size, while the pixel buffer is downsampled.
        assert_eq!(proxy.logical_size(0), Some((16, 200)));
        assert!(proxy.image.layer_data[0].size.1 < 200, "buffer shrank");
    }

    #[test]
    fn downsampled_preserves_f16_bit_depth() {
        // #170: an f16 source channel must stay f16 through the proxy downsample
        // — half the proxy RAM, and an all-f16 layer keeps the `Rgba16Float`
        // upload path — while an f32 channel stays f32. Previously every proxy
        // channel came out F32, doubling both.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mixed_depth.exr");
        // 64×64, R/G/A half + B float; max_dim 16 → factor 4 → exactly 16×16.
        write_typed_exr(
            &p,
            64,
            64,
            &[("R", true), ("G", true), ("B", false), ("A", true)],
        );
        let full = ExrData::load(&p).unwrap();
        let proxy = full.downsampled(16);

        let full_chans = &full.image.layer_data[0].channel_data.list;
        let proxy_chans = &proxy.image.layer_data[0].channel_data.list;
        assert_eq!(full_chans.len(), proxy_chans.len());
        for (src, out) in full_chans.iter().zip(proxy_chans) {
            assert_eq!(src.name, out.name, "channel order preserved by sort");
            // Depth matches the source, and the constant 0.5 fixture survives
            // the filter + (for halves) the f32→f16 round-trip exactly.
            match (&src.sample_data, &out.sample_data) {
                (FlatSamples::F16(_), FlatSamples::F16(s)) => {
                    assert_eq!(s[0].to_f32(), 0.5, "f16 {} value intact", src.name);
                }
                (FlatSamples::F32(_), FlatSamples::F32(s)) => {
                    assert_eq!(s[0], 0.5, "f32 {} value intact", src.name);
                }
                _ => panic!("proxy channel {} changed bit depth", src.name),
            }
        }

        // The footprint win is exact: 3 half channels (16×16×2B) + 1 float
        // channel (16×16×4B) = 2560, vs 4096 if everything widened to F32.
        assert_eq!(proxy.approx_bytes(), 3 * 256 * 2 + 256 * 4);

        // An all-f16 source must produce a proxy that still satisfies the
        // `Rgba16Float` upload gate (`all_channels_f16`, #142/#170).
        let p2 = dir.path().join("all_half.exr");
        write_typed_exr(
            &p2,
            64,
            64,
            &[("R", true), ("G", true), ("B", true), ("A", true)],
        );
        let proxy2 = ExrData::load(&p2).unwrap().downsampled(16);
        let (_, r, g, b, a) = proxy2.logical_channels(0).unwrap();
        assert!(
            crate::pixels::all_channels_f16([r, g, b, a]),
            "all-f16 source proxy keeps the f16 upload path"
        );
    }

    #[test]
    fn downsampled_into_reuses_scratch_and_matches() {
        // #171: `downsampled_into` with a reused accumulator must produce the exact
        // same result as `downsampled`, regardless of the scratch's prior contents,
        // and reuse the buffer (no realloc) across same-size frames.
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.exr");
        let small = dir.path().join("small.exr");
        write_typed_exr(&big, 64, 64, &[("R", true), ("G", true), ("B", false)]);
        write_typed_exr(&small, 32, 32, &[("R", true), ("G", true), ("B", false)]);
        let big_src = ExrData::load(&big).unwrap();
        let small_src = ExrData::load(&small).unwrap();

        let mut scratch = Vec::new();
        // A differently-sized frame first exercises the resize path; then the same
        // size twice must hit the no-realloc fast path with an identical result.
        let _ = small_src.downsampled_into(16, &mut scratch);
        let a = big_src.downsampled_into(16, &mut scratch);
        let cap_after_first = scratch.capacity();
        let b = big_src.downsampled_into(16, &mut scratch);
        assert_eq!(
            scratch.capacity(),
            cap_after_first,
            "same-size reuse must not reallocate the scratch"
        );

        // Equal to the throwaway-buffer path, byte for byte.
        let want = big_src.downsampled(16);
        for got in [&a, &b] {
            let want_ch = &want.image.layer_data[0].channel_data.list;
            let got_ch = &got.image.layer_data[0].channel_data.list;
            assert_eq!(want_ch.len(), got_ch.len());
            for (w, g) in want_ch.iter().zip(got_ch) {
                assert_eq!(w.name, g.name);
                match (&w.sample_data, &g.sample_data) {
                    (FlatSamples::F16(ws), FlatSamples::F16(gs)) => assert_eq!(ws, gs),
                    (FlatSamples::F32(ws), FlatSamples::F32(gs)) => assert_eq!(ws, gs),
                    _ => panic!("bit depth mismatch for {}", w.name),
                }
            }
        }
    }

    // --- On-disk proxy-cache blob (#165) -------------------------------------

    /// A throwaway key for blob tests (the on-disk cache echoes + verifies it).
    fn blob_key() -> crate::proxy_cache::ProxyKey {
        crate::proxy_cache::ProxyKey {
            path: "/shots/sh010/beauty.0001.exr".into(),
            mtime_ns: 1_700_000_000_123_456_789,
            size: 178_000_000,
            proxy_px: 1024,
            aov: 0,
        }
    }

    #[test]
    fn proxy_blob_round_trips_geometry_depth_and_layer_name() {
        // Build a proxy-shaped frame directly so the geometry the viewport frames
        // by is non-trivial: a shifted display window, a **named** beauty layer at
        // a non-zero data-window origin, all-f16 channels, a custom pixel aspect,
        // and a full-res `display_size` distinct from the (small) buffer.
        let mut chans: smallvec::SmallVec<[AnyChannel<FlatSamples>; 4]> = smallvec::SmallVec::new();
        for (i, name) in ["R", "G", "B", "A"].iter().enumerate() {
            let vals: Vec<f16> = (0..3 * 2)
                .map(|p| f16::from_f32((i as f32 * 10.0 + p as f32) * 0.01))
                .collect();
            chans.push(AnyChannel::new(Text::from(*name), FlatSamples::F16(vals)));
        }
        let attributes = LayerAttributes {
            layer_position: Vec2(7, -3),
            layer_name: Some(Text::from("beauty")),
            ..LayerAttributes::default()
        };
        let layer = Layer::new(
            (3usize, 2usize),
            attributes,
            Encoding::UNCOMPRESSED,
            AnyChannels::sort(chans),
        );
        let mut img_attrs = ImageAttributes::new(IntegerBounds::new((5, 9), (40usize, 30usize)));
        img_attrs.pixel_aspect = 2.0;
        let image = FlatImage {
            attributes: img_attrs,
            layer_data: smallvec::smallvec![layer],
        };
        let mut proxy = ExrData::from_image(image, true, true);
        proxy.display_size = Some((40, 30));

        let key = blob_key();
        let blob = proxy.write_proxy_blob(&key);
        let back = ExrData::from_proxy_blob(&blob, &key).expect("valid blob round-trips");

        assert!(back.proxy && back.beauty_only, "flags preserved");
        assert_eq!(back.display_size, Some((40, 30)), "full-res framing size");
        assert_eq!(
            back.image.attributes.display_window, proxy.image.attributes.display_window,
            "display window (viewport framing) preserved"
        );
        assert_eq!(back.image.attributes.pixel_aspect, 2.0);
        let bl = &back.image.layer_data[0];
        assert_eq!(
            bl.attributes.layer_position,
            Vec2(7, -3),
            "data-window origin"
        );
        assert_eq!(
            bl.attributes.layer_name.as_ref().map(ToString::to_string),
            Some("beauty".to_string()),
            "named beauty layer survives (logical-layer name fidelity)"
        );
        // Every channel stays F16 (buffer bit-for-bit) → keeps the Rgba16Float path.
        let (_, r, g, b, a) = back.logical_channels(0).unwrap();
        assert!(
            crate::pixels::all_channels_f16([r, g, b, a]),
            "f16 preserved"
        );
        for (src, out) in proxy.image.layer_data[0]
            .channel_data
            .list
            .iter()
            .zip(&bl.channel_data.list)
        {
            match (&src.sample_data, &out.sample_data) {
                (FlatSamples::F16(a), FlatSamples::F16(b)) => {
                    assert_eq!(a, b, "{} samples", src.name)
                }
                _ => panic!("channel {} depth changed", src.name),
            }
        }
    }

    #[test]
    fn proxy_blob_round_trips_a_decoded_multipart_beauty() {
        // End-to-end: the multi-part beauty proxy the cache actually stores.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mp.exr");
        write_multipart_exr(&p);
        let proxy = ExrData::load_proxy(&p, 1).expect("proxy");
        let key = blob_key();
        let back =
            ExrData::from_proxy_blob(&proxy.write_proxy_blob(&key), &key).expect("round-trips");
        assert_eq!(back.logical_layers.len(), proxy.logical_layers.len());
        assert_eq!(back.channels_str, proxy.channels_str);
        assert_eq!(back.approx_bytes(), proxy.approx_bytes());
        assert_eq!(back.logical_size(0), proxy.logical_size(0));
    }

    #[test]
    fn from_proxy_blob_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.exr");
        write_tall_rgba(&p, 16, 200);
        let key = blob_key();
        let good = ExrData::load_proxy(&p, 48).unwrap().write_proxy_blob(&key);

        // Sanity: the untouched blob does round-trip.
        assert!(ExrData::from_proxy_blob(&good, &key).is_some());

        // Bad magic.
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(ExrData::from_proxy_blob(&bad_magic, &key).is_none());

        // Wrong version byte (right after the 4-byte magic).
        let mut bad_ver = good.clone();
        bad_ver[4] = 0xFF;
        assert!(ExrData::from_proxy_blob(&bad_ver, &key).is_none());

        // Truncated body.
        assert!(ExrData::from_proxy_blob(&good[..good.len() / 2], &key).is_none());
        assert!(ExrData::from_proxy_blob(&[], &key).is_none());

        // Echoed key mismatch (a hash collision or a re-rendered source): the
        // stored key no longer matches the request → treated as a miss.
        let other = crate::proxy_cache::ProxyKey {
            mtime_ns: key.mtime_ns + 1,
            ..key.clone()
        };
        assert!(ExrData::from_proxy_blob(&good, &other).is_none());
    }

    #[test]
    fn a_cached_aov_proxy_is_never_served_for_a_different_aov() {
        // #217, the one that has to be right: a per-AOV proxy of `shot.0007.exr`
        // shares path, mtime, size and pixel cap with every *other* AOV of the same
        // frame. With `aov` out of the key they hash to one filename and the header
        // check passes, so layer 1's pixels come back as a verified hit for layer 2
        // — the wrong pass on screen, silently, which is worse than the slow decode
        // the cache exists to avoid.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.exr");
        write_multipart_exr(&path);

        // (That the two keys also hash to different *filenames* is pinned next to
        // the hash itself, in `proxy_cache`.)
        let k1 = crate::proxy_cache::ProxyKey {
            aov: 1,
            ..blob_key()
        };
        let k2 = crate::proxy_cache::ProxyKey {
            aov: 2,
            ..blob_key()
        };

        let mut scratch = Vec::new();
        let proxy = ExrData::load_layer_proxy_into(&path, 1, 1, 8, &mut scratch).unwrap();
        assert_eq!(proxy.only_layer, Some(1));
        let blob = proxy.write_proxy_blob(&k1);

        // Its own key round-trips, AOV identity intact — so the restored proxy
        // refuses layer 0 exactly as the decode it stands in for does.
        let back = ExrData::from_proxy_blob(&blob, &k1).expect("round-trips under its own key");
        assert_eq!(back.only_layer, Some(1));
        assert!(back.logical_channels(1).is_some());
        assert!(back.logical_channels(0).is_none());

        // A different AOV's key is a miss even though everything else matches.
        assert!(
            ExrData::from_proxy_blob(&blob, &k2).is_none(),
            "layer 1's blob must not verify as layer 2"
        );
        assert!(
            ExrData::from_proxy_blob(&blob, &blob_key()).is_none(),
            "nor as the beauty layer"
        );
    }

    #[test]
    fn from_proxy_blob_rejects_absurd_counts_without_allocating() {
        // A corrupt/tampered blob with a valid header + matching key but a layer
        // claiming a 65535×65535 buffer must return None (a cache miss) — the
        // per-channel allocation is bounded to the blob's own remaining bytes, so
        // a bogus dimension can never force a multi-GB allocation. (If this ever
        // regressed it would OOM the test process, not just fail an assert.)
        let key = blob_key();
        let mut b = Vec::new();
        b.extend_from_slice(PROXY_BLOB_MAGIC);
        b.push(PROXY_BLOB_VERSION);
        b.push(0b10); // proxy flag
        b.extend_from_slice(&key.mtime_ns.to_le_bytes());
        b.extend_from_slice(&key.size.to_le_bytes());
        b.extend_from_slice(&key.proxy_px.to_le_bytes());
        write_blob_str(&mut b, &key.path);
        b.extend_from_slice(&0i32.to_le_bytes()); // display_window pos x
        b.extend_from_slice(&0i32.to_le_bytes()); // display_window pos y
        b.extend_from_slice(&64u32.to_le_bytes()); // display_window size x
        b.extend_from_slice(&64u32.to_le_bytes()); // display_window size y
        b.extend_from_slice(&1.0f32.to_le_bytes()); // pixel_aspect
        b.push(0); // display_size: None
        b.extend_from_slice(&1u16.to_le_bytes()); // layer_count = 1
        b.extend_from_slice(&0i32.to_le_bytes()); // layer_position x
        b.extend_from_slice(&0i32.to_le_bytes()); // layer_position y
        b.extend_from_slice(&65535u32.to_le_bytes()); // lw  → n = 65535²
        b.extend_from_slice(&65535u32.to_le_bytes()); // lh
        b.push(0); // layer_name: None
        b.extend_from_slice(&1u16.to_le_bytes()); // chan_count = 1
        write_blob_str(&mut b, "R");
        b.push(SAMPLE_TAG_F16); // ~8.6 GB of samples would follow — but don't
        assert!(
            ExrData::from_proxy_blob(&b, &key).is_none(),
            "absurd dims must miss, not allocate"
        );
    }

    // --- Beauty-only decode (#56, hardening step 3) --------------------------

    /// Write a **multi-part** EXR: a named `beauty` part (RGBA) followed by a
    /// named `normal` part (X/Y/Z). Multi-part is exactly the case beauty-only
    /// decode wins on — `first_valid_layer` filters the second part's blocks.
    fn write_multipart_exr(path: &Path) {
        const W: usize = 2;
        const H: usize = 2;
        let named = |name: &str, chans: &[&str]| {
            let mut list = smallvec::SmallVec::new();
            for c in chans {
                list.push(AnyChannel::new(
                    Text::from(*c),
                    FlatSamples::F32(vec![0.5; W * H]),
                ));
            }
            let attrs = LayerAttributes {
                layer_name: Some(Text::from(name)),
                ..Default::default()
            };
            Layer::new(
                (W, H),
                attrs,
                Encoding::FAST_LOSSLESS,
                AnyChannels::sort(list),
            )
        };
        let layers: smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]> = smallvec::smallvec![
            named("beauty", &["R", "G", "B", "A"]),
            named("normal", &["X", "Y", "Z"]),
        ];
        Image::from_layers(
            ImageAttributes::new(IntegerBounds::from_dimensions((W, H))),
            layers,
        )
        .write()
        .to_file(path)
        .expect("write multipart exr");
    }

    /// Write a multi-part EXR whose **second part carries two render passes**
    /// (`diffuse.*` + `specular.*`). The Karma shape `load_layer` is built for is
    /// one AOV per part; this is the shape it must refuse, because a decode of
    /// part 1 alone cannot say which of the two logical layers it is.
    fn write_multipass_part_exr(path: &Path) {
        const W: usize = 2;
        const H: usize = 2;
        let named = |name: &str, chans: &[&str]| {
            let mut list = smallvec::SmallVec::new();
            for c in chans {
                list.push(AnyChannel::new(
                    Text::from(*c),
                    FlatSamples::F32(vec![0.5; W * H]),
                ));
            }
            Layer::new(
                (W, H),
                LayerAttributes {
                    layer_name: Some(Text::from(name)),
                    ..Default::default()
                },
                Encoding::FAST_LOSSLESS,
                AnyChannels::sort(list),
            )
        };
        let layers: smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]> = smallvec::smallvec![
            named("beauty", &["R", "G", "B", "A"]),
            named(
                "passes",
                &[
                    "diffuse.R",
                    "diffuse.G",
                    "diffuse.B",
                    "specular.R",
                    "specular.G",
                    "specular.B",
                ],
            ),
        ];
        Image::from_layers(
            ImageAttributes::new(IntegerBounds::from_dimensions((W, H))),
            layers,
        )
        .write()
        .to_file(path)
        .expect("write multipass-part exr");
    }

    #[test]
    fn load_layer_decodes_one_aov_and_answers_for_that_index_only() {
        // #217: the whole point is that a caller holding full-image indices can ask
        // a one-part decode for *its* layer. The refusals matter as much as the
        // hit: the T1 ring is keyed (source, frame) with no AOV, so an AOV switch
        // leaves these frames resident and asks them for the new index.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.exr");
        write_multipart_exr(&path);

        let full = ExrData::load(&path).expect("full load");
        assert_eq!(full.logical_layers.len(), 2, "beauty + normal");
        assert_eq!(full.logical_layers[1].physical_index, 1);

        let one = ExrData::load_layer(&path, 1, 1).expect("layer 1 decode");
        assert_eq!(one.only_layer, Some(1));
        assert_eq!(one.image.layer_data.len(), 1, "only part 1 decoded");
        assert!(one.beauty_only, "a subset frame: settle re-decodes it full");

        // It answers for the index the caller asked for ...
        assert!(
            one.logical_channels(1).is_some(),
            "the decoded AOV resolves under the caller's own index"
        );
        assert_eq!(one.logical_size(1), full.logical_size(1));
        assert_eq!(
            one.logical_layer(1).map(|l| l.name.clone()),
            Some("normal".into())
        );

        // ... and for no other. Layer 0 is the dangerous one: a lenient remap would
        // hand back the single entry it holds — the *normal* pass — and it would
        // render as beauty with nothing failing anywhere.
        assert!(
            one.logical_channels(0).is_none(),
            "a layer-1 decode must not answer as layer 0"
        );
        assert!(one.logical_size(0).is_none());
        assert!(one.logical_channels(2).is_none(), "out of range stays None");

        // Cheaper than the full decode, which is the entire justification.
        assert!(one.approx_bytes() < full.approx_bytes());
    }

    #[test]
    fn load_layer_refuses_a_part_holding_several_passes() {
        // The gate (`ExrApp::single_layer_part`) should never route here, but it
        // decides from a layer table read at open time — a sequence whose part
        // layout changes mid-shot would slip through. Without this check that frame
        // decodes into an ExrData that can address *nothing*, every texture build
        // fails, and #213's evict-and-retry re-decodes it forever: an unbounded
        // loop, not a slow path. An Err costs one decode and falls back to full.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipass.exr");
        write_multipass_part_exr(&path);

        let full = ExrData::load(&path).expect("full load");
        assert_eq!(full.logical_layers.len(), 3, "beauty + diffuse + specular");
        assert_eq!(full.logical_layers[1].physical_index, 1);
        assert_eq!(
            full.logical_layers[2].physical_index, 1,
            "both passes share part 1 — the ambiguous case"
        );

        let err = match ExrData::load_layer(&path, 1, 1) {
            Ok(_) => panic!("a part holding two passes must not decode as one layer"),
            Err(e) => e,
        };
        assert!(
            err.contains("render passes"),
            "error should explain the ambiguity, got: {err}"
        );

        // Part 0 holds exactly one pass, so it is still addressable.
        assert!(ExrData::load_layer(&path, 0, 0).is_ok());
    }

    #[test]
    fn full_load_sees_every_part_beauty_load_sees_only_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.exr");
        write_multipart_exr(&path);

        // Full decode: both physical parts present, flagged all-AOV.
        let full = ExrData::load(&path).expect("full load");
        assert!(!full.beauty_only, "load() is a full all-AOV decode");
        assert_eq!(full.image.layer_data.len(), 2, "both parts decoded");

        // Beauty-only decode: just the first part (beauty/RGBA), flagged.
        let beauty = ExrData::load_beauty(&path).expect("beauty load");
        assert!(beauty.beauty_only, "load_beauty() sets the flag");
        assert_eq!(
            beauty.image.layer_data.len(),
            1,
            "only the first part is decoded"
        );
        // That single part is the RGBA beauty, fully resolved.
        assert_eq!(beauty.logical_layers.len(), 1);
        let ll = &beauty.logical_layers[0];
        assert!(
            ll.r.is_some() && ll.g.is_some() && ll.b.is_some() && ll.a.is_some(),
            "beauty part resolves all four RGBA slots"
        );

        // The beauty-only frame is strictly lighter in RAM (one part vs two).
        assert!(
            beauty.approx_bytes() < full.approx_bytes(),
            "beauty-only frame is smaller: {} vs {}",
            beauty.approx_bytes(),
            full.approx_bytes()
        );
    }

    #[test]
    fn beauty_load_rejects_non_exr_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_an.exr");
        std::fs::write(&path, b"definitely not an exr").unwrap();
        // Shares the same friendly error path as `load`.
        let err = match ExrData::load_beauty(&path) {
            Ok(_) => panic!("garbage must not parse as EXR"),
            Err(e) => e,
        };
        let lower = err.to_lowercase();
        assert!(
            lower.contains("magic number")
                || lower.contains("valid exr")
                || lower.contains("identifier"),
            "error should explain the bad EXR, got: {err}"
        );
    }

    // --- DWA decode (exrs v1.74.1 rebase) ------------------------------------

    /// DWAA/DWAB are lossy DCT codecs the `miniz-inflate` fork couldn't decode
    /// before the v1.74.1 rebase — opening one used to fail with
    /// `unsupported("pixels cannot be compressed (dwaa)")`. A committed fixture is
    /// the only way to regression-test this: exrs can *decode* DWA but not *write*
    /// it, so it can't be generated synthetically. Both fixtures are a 64×64
    /// RGBA-half flat fill `(0.25, 0.5, 0.75, 1.0)`, so a correct decode
    /// reproduces that within DWA's lossy tolerance.
    #[test]
    fn loads_dwaa_and_dwab_compressed_exrs() {
        for name in ["dwaa", "dwab"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("{name}.exr"));
            let data = ExrData::load(&path)
                .unwrap_or_else(|e| panic!("{name} fixture must decode after the rebase: {e}"));
            assert_eq!(data.logical_size(0), Some((64, 64)), "{name} dims");
            let (layer, r, g, b, a) = data.logical_channels(0).expect("beauty layer");
            let w = layer.size.0;
            let at = |c| crate::pixels::sample_channel(c, 32, 32, w);
            // RGB go through DWA's lossy DCT → loose ε. Alpha isn't in a CSC set,
            // so it's stored losslessly → assert it exactly (below).
            approx::assert_abs_diff_eq!(at(r), 0.25, epsilon = 0.02);
            approx::assert_abs_diff_eq!(at(g), 0.50, epsilon = 0.02);
            approx::assert_abs_diff_eq!(at(b), 0.75, epsilon = 0.02);
            // Lossless: 1.0 is exact in f16, so no DWA error here.
            approx::assert_abs_diff_eq!(at(a), 1.00, epsilon = 1e-6);
        }
    }
}
