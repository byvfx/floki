use exr::prelude::*;
use std::path::Path;

pub struct ExrData {
    pub image: Image<smallvec::SmallVec<[Layer<AnyChannels<FlatSamples>>; 2]>>,
    /// Displayable passes derived by grouping channels by their dotted name
    /// prefix. See [`LogicalLayer`].
    pub logical_layers: Vec<LogicalLayer>,
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
    pub fn load(path: impl AsRef<Path>) -> std::result::Result<Self, String> {
        let path_ref = path.as_ref();

        // The patched `exr` (see [patch.crates-io] in Cargo.toml) decompresses via
        // miniz_oxide, which returns an error instead of panicking on bad data, so
        // parallel decompression is safe again. catch_unwind is kept as cheap
        // insurance against panics in the synchronous (calling-thread) parsing
        // path; it can't catch a rayon-worker panic, but miniz_oxide removes the
        // one decompression panic that used to abort the app.
        let read_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            read()
                .no_deep_data()
                .largest_resolution_level()
                .all_channels()
                .all_layers()
                .all_attributes()
                .from_file(path_ref)
        }))
        .map_err(|_| {
            "Failed to decode EXR: the decompressor panicked. The file may use an \
             unsupported compression variant or trigger a known decoder bug in the \
             exr/zune-inflate dependency."
                .to_string()
        })?;

        match read_result {
            Ok(image) => {
                let logical_layers = build_logical_layers(&image);
                Ok(Self {
                    image,
                    logical_layers,
                })
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("file identifier missing") {
                    // Try to read the first 4 bytes to help the user
                    if let Ok(mut f) = std::fs::File::open(path_ref) {
                        use std::io::Read;
                        let mut buf = [0u8; 4];
                        if f.read_exact(&mut buf).is_ok() {
                            let hex_str = format!(
                                "{:02X} {:02X} {:02X} {:02X}",
                                buf[0], buf[1], buf[2], buf[3]
                            );
                            let ascii_str: String = buf
                                .iter()
                                .map(|&b| {
                                    if (32..=126).contains(&b) {
                                        b as char
                                    } else {
                                        '.'
                                    }
                                })
                                .collect();
                            return Err(format!(
                                "Not a valid EXR file (magic number missing).\nFirst 4 bytes: [{}] ('{}')\nMake sure this is actually an OpenEXR file and not a renamed PNG, JPG, or corrupted file.",
                                hex_str, ascii_str
                            ));
                        }
                    }
                    Err("Not a valid EXR file (magic number missing). The file might be corrupted or in another format.".to_string())
                } else {
                    Err(err_str)
                }
            }
        }
    }

    /// `(width, height)` of the physical layer backing the given logical layer.
    pub fn logical_size(&self, idx: usize) -> Option<(usize, usize)> {
        let ll = self.logical_layers.get(idx)?;
        let layer = self.image.layer_data.get(ll.physical_index)?;
        Some((layer.size.0, layer.size.1))
    }

    /// Resolve a logical layer to its physical layer plus the `(r, g, b, a)`
    /// channels it maps to. Channels are looked up by the indices recorded at
    /// load time, so no per-call name matching is needed.
    #[allow(clippy::type_complexity)]
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
        let ll = self.logical_layers.get(idx)?;
        let layer = self.image.layer_data.get(ll.physical_index)?;
        let list = &layer.channel_data.list;
        let get = |o: Option<usize>| o.and_then(|i| list.get(i));
        Some((layer, get(ll.r), get(ll.g), get(ll.b), get(ll.a)))
    }
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

/// Map a component token to an RGBA slot (`0=r, 1=g, 2=b, 3=a`), or `None` if it
/// is not a recognized color/vector component. Vector passes map `X->r, Y->g, Z->b`.
fn component_slot(token: &str) -> Option<usize> {
    let eq = |s: &str| token.eq_ignore_ascii_case(s);
    if eq("R") || eq("red") || eq("X") {
        Some(0)
    } else if eq("G") || eq("green") || eq("Y") {
        Some(1)
    } else if eq("B") || eq("blue") || eq("Z") {
        Some(2)
    } else if eq("A") || eq("alpha") {
        Some(3)
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

    order
        .into_iter()
        .zip(members)
        .map(|(group_key, mem)| {
            let (mut r, mut g, mut b, mut a) = (None, None, None, None);
            if mem.len() == 1 {
                // Single-channel pass (Z-depth, mist, a mask): show as grayscale.
                let only = mem[0].0;
                r = Some(only);
                g = Some(only);
                b = Some(only);
            } else {
                for &(ci, token) in &mem {
                    match component_slot(token) {
                        Some(0) => r = Some(ci),
                        Some(1) => g = Some(ci),
                        Some(2) => b = Some(ci),
                        Some(3) => a = Some(ci),
                        _ => {}
                    }
                }
                if r.is_none() && g.is_none() && b.is_none() {
                    let first = mem[0].0;
                    r = Some(first);
                    g = Some(first);
                    b = Some(first);
                }
            }
            RawGroup {
                group_key: group_key.to_string(),
                r,
                g,
                b,
                a,
            }
        })
        .collect()
}

/// Combine a physical layer name with a channel-prefix into one full group key.
fn combine_key(phys: Option<&str>, group_key: &str) -> String {
    match (phys, group_key.is_empty()) {
        (Some(p), false) => format!("{}.{}", p, group_key),
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
        assert_eq!(component_slot("R"), Some(0));
        assert_eq!(component_slot("g"), Some(1));
        assert_eq!(component_slot("BLUE"), Some(2));
        assert_eq!(component_slot("A"), Some(3));
        assert_eq!(component_slot("X"), Some(0));
        assert_eq!(component_slot("Y"), Some(1));
        assert_eq!(component_slot("Z"), Some(2));
        assert_eq!(component_slot("mask"), None);
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
}
