use exr::prelude::*;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

/// Aggregate result of a `run_conversion_task` call. The per-file progress
/// messages still flow through the `mpsc` channel (the GUI relies on them for
/// live updates); this struct is the return value so the `convert_dir` binary
/// can set a correct exit code without parsing message strings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConversionSummary {
    pub converted: usize,
    pub failed: usize,
    pub total: usize,
    pub cancelled: bool,
}

impl ConversionSummary {
    /// `true` if the run had no failures and was not cancelled.
    // Only called from `src/bin/convert_dir.rs`, which re-includes this file
    // via `#[path]` (separate compilation) — the lib crate doesn't see the use.
    #[allow(dead_code)]
    pub fn is_success(&self) -> bool {
        !self.cancelled && self.failed == 0
    }
}

pub fn run_conversion_task(
    input_dir: PathBuf,
    output_dir: PathBuf,
    sender: Sender<(usize, usize, String)>,
    cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> ConversionSummary {
    let mut files_to_process = Vec::new();

    // Read all .exr files in the input directory
    match std::fs::read_dir(&input_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && let Some(ext) = path.extension()
                    && ext.eq_ignore_ascii_case("exr")
                {
                    files_to_process.push(path);
                }
            }
        }
        Err(e) => {
            log::error!("Failed to read input directory {:?}: {}", input_dir, e);
            let _ = sender.send((0, 0, format!("Failed to read directory: {}", e)));
            return ConversionSummary::default();
        }
    }

    let total = files_to_process.len();
    if total == 0 {
        log::warn!("No EXR files found in {:?}", input_dir);
        let _ = sender.send((0, 0, "No EXR files found in directory.".to_string()));
        return ConversionSummary::default();
    }

    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        log::error!("Failed to create output directory {:?}: {}", output_dir, e);
        let _ = sender.send((0, 0, format!("Failed to create output directory: {}", e)));
        return ConversionSummary::default();
    }

    log::info!(
        "EXR convert: {} file(s) from {:?} -> {:?}",
        total,
        input_dir,
        output_dir
    );

    // Shared monotonic counter: files convert in parallel and finish out of
    // order, but progress must only ever move forward. Each file emits exactly
    // one message (on completion) carrying the cumulative completed count.
    let completed = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));

    files_to_process
        .into_par_iter()
        .for_each_with(sender.clone(), |s, path| {
            if cancel_flag.load(Ordering::Relaxed) {
                return;
            }
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let out_path = output_dir.join(&file_name);

            let msg = match convert_exr(&path, &out_path) {
                Ok(_) => {
                    log::debug!("converted {}", file_name);
                    format!("Converted: {}", file_name)
                }
                Err(e) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                    log::error!("convert failed for {}: {}", file_name, e);
                    format!("Error on {}: {}", file_name, e)
                }
            };
            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = s.send((done, total, msg));
        });

    // Terminal message, then `sender` drops and the channel disconnects — the UI
    // uses that disconnect to detect completion (also covers cancellation).
    let done = completed.load(Ordering::Relaxed);
    let failed = errors.load(Ordering::Relaxed);
    let converted = done - failed;
    let cancelled = cancel_flag.load(Ordering::Relaxed);

    let mut final_msg = if cancelled {
        format!("Cancelled — {} of {} files converted", converted, total)
    } else {
        format!("Complete — {} of {} files converted", converted, total)
    };
    if failed > 0 {
        final_msg.push_str(&format!(" ({} failed)", failed));
    }
    log::info!("EXR convert finished: {}", final_msg);
    let count = if cancelled { done } else { total };
    let _ = sender.send((count, total, final_msg));

    ConversionSummary {
        converted,
        failed,
        total,
        cancelled,
    }
}

/// Maps an RGBA-style channel suffix to its canonical single letter, or `None`
/// for anything that is not a colour channel (x/y/z, depth, custom data, ...).
fn canonical_rgba(suffix: &str) -> Option<&'static str> {
    if suffix.eq_ignore_ascii_case("R") || suffix.eq_ignore_ascii_case("RED") {
        Some("R")
    } else if suffix.eq_ignore_ascii_case("G") || suffix.eq_ignore_ascii_case("GREEN") {
        Some("G")
    } else if suffix.eq_ignore_ascii_case("B") || suffix.eq_ignore_ascii_case("BLUE") {
        Some("B")
    } else if suffix.eq_ignore_ascii_case("A") || suffix.eq_ignore_ascii_case("ALPHA") {
        Some("A")
    } else {
        None
    }
}

fn convert_exr(
    in_path: &Path,
    out_path: &Path,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    use exr::block::writer::ChunksWriter;
    use std::fs::File;
    use std::io::{BufReader, BufWriter};

    // 1 MiB buffers: EXR chunks are large, so the default 8 KiB buffer turns a
    // sequential copy into many syscalls. Bigger buffers cut that dramatically.
    let file = BufReader::with_capacity(1 << 20, File::open(in_path)?);
    let reader = exr::block::read(file, false)?;
    let mut meta = reader.meta_data().clone();

    for (layer_idx, layer) in meta.headers.iter_mut().enumerate() {
        // Determine the layer name / channel prefix.
        let prefix = if layer_idx == 0 {
            layer.own_attributes.layer_name = Some(Text::from("rgba"));
            "rgba".to_string()
        } else if let Some(name) = &layer.own_attributes.layer_name {
            name.to_string()
        } else {
            let name = format!("layer_{}", layer_idx);
            layer.own_attributes.layer_name = Some(Text::from(name.as_str()));
            name
        };

        // Rename ONLY channels whose suffix is already R/G/B/A (case-insensitive).
        // Everything else — x/y/z vector passes (P, N), depth, single-channel
        // data like cusp/height/thickness — keeps its original name so its pixel
        // data is never mis-mapped. (Forcing those to R/G/B/A by position is what
        // swapped X/Z on the position and normal passes.)
        let proposed: Vec<String> = layer
            .channels
            .list
            .iter()
            .map(|c| {
                let name = c.name.to_string();
                let suffix = name.rsplit('.').next().unwrap_or(&name);
                match canonical_rgba(suffix) {
                    Some(canon) => format!("{}.{}", prefix, canon),
                    None => name,
                }
            })
            .collect();

        // Pixel blocks are copied verbatim in the original (already alphabetical)
        // channel order. Renaming is therefore only safe if the proposed names
        // stay in that same order; if a rename would reorder channels, skip this
        // layer entirely rather than scramble its data.
        let stays_sorted = proposed.windows(2).all(|w| w[0] <= w[1]);
        if stays_sorted {
            for (channel, new_name) in layer.channels.list.iter_mut().zip(proposed.iter()) {
                channel.name = Text::from(new_name.as_str());
            }
        } else {
            log::warn!(
                "{:?}: layer {} left unchanged (renaming would reorder channels)",
                in_path,
                layer_idx
            );
        }
    }

    let mut block_indices = Vec::new();
    for header in &meta.headers {
        let mut map = std::collections::HashMap::new();
        for (idx, tile) in header.blocks_increasing_y_order().enumerate() {
            map.insert(tile.location, idx);
        }
        block_indices.push(map);
    }

    let out_file = BufWriter::with_capacity(1 << 20, File::create(out_path)?);
    exr::block::writer::write_chunks_with(
        out_file,
        meta.headers.clone(),
        false,
        |_, chunk_writer| {
            let chunks_reader = reader.all_chunks(false)?;
            for chunk_result in chunks_reader {
                let chunk = chunk_result?;
                let layer_idx = chunk.layer_index;
                let header = &meta.headers[layer_idx];

                let location = header.get_block_data_indices(&chunk.compressed_block)?;
                let index_in_header = block_indices[layer_idx][&location];
                chunk_writer.write_chunk(index_in_header, chunk)?;
            }
            Ok(())
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{canonical_rgba, convert_exr, run_conversion_task};
    use exr::prelude::*;
    use std::path::Path;

    const W: usize = 2;
    const H: usize = 2;

    fn write_single_layer(path: &Path, layer_name: &str, channels: &[(&str, f32)]) {
        let mut list = exr::prelude::SmallVec::new();
        for (name, value) in channels {
            list.push(AnyChannel::new(
                Text::from(*name),
                FlatSamples::F32(vec![*value; W * H]),
            ));
        }
        let layer = Layer::new(
            (W, H),
            LayerAttributes::named(Text::from(layer_name)),
            Encoding::FAST_LOSSLESS,
            AnyChannels::sort(list),
        );
        Image::from_layer(layer)
            .write()
            .to_file(path)
            .expect("write test exr");
    }

    /// Returns (channel name -> first sample value) for layer 0.
    fn read_back(path: &Path) -> Vec<(String, f32)> {
        let image = exr::prelude::read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .all_layers()
            .all_attributes()
            .from_file(path)
            .expect("read converted exr");
        image.layer_data[0]
            .channel_data
            .list
            .iter()
            .map(|c| {
                let v = match &c.sample_data {
                    FlatSamples::F16(s) => s[0].to_f32(),
                    FlatSamples::F32(s) => s[0],
                    FlatSamples::U32(s) => s[0] as f32,
                };
                (c.name.to_string(), v)
            })
            .collect()
    }

    #[test]
    fn beauty_rgba_is_renamed_and_data_follows() {
        let dir = std::env::temp_dir();
        let src = dir.join("exr_test_beauty_src.exr");
        let dst = dir.join("exr_test_beauty_dst.exr");
        // Distinct value per channel so any scramble is detectable.
        write_single_layer(&src, "C", &[("R", 1.0), ("G", 2.0), ("B", 3.0), ("A", 4.0)]);

        convert_exr(&src, &dst).expect("convert");
        let out: std::collections::HashMap<String, f32> = read_back(&dst).into_iter().collect();

        assert_eq!(out.get("rgba.R"), Some(&1.0), "R data must stay under .R");
        assert_eq!(out.get("rgba.G"), Some(&2.0), "G data must stay under .G");
        assert_eq!(out.get("rgba.B"), Some(&3.0), "B data must stay under .B");
        assert_eq!(out.get("rgba.A"), Some(&4.0), "A data must stay under .A");
    }

    #[test]
    fn xyz_pass_channels_are_left_untouched() {
        let dir = std::env::temp_dir();
        let src = dir.join("exr_test_xyz_src.exr");
        let dst = dir.join("exr_test_xyz_dst.exr");
        // Position-style pass; x/y/z must NOT be forced to R/G/B (that swapped X/Z).
        write_single_layer(&src, "P", &[("x", 10.0), ("y", 20.0), ("z", 30.0)]);

        convert_exr(&src, &dst).expect("convert");
        let out: std::collections::HashMap<String, f32> = read_back(&dst).into_iter().collect();

        assert_eq!(out.get("x"), Some(&10.0), "x must keep its name and data");
        assert_eq!(out.get("y"), Some(&20.0), "y must keep its name and data");
        assert_eq!(out.get("z"), Some(&30.0), "z must keep its name and data");
        assert!(
            !out.keys()
                .any(|k| k.ends_with(".R") || k.ends_with(".G") || k.ends_with(".B")),
            "no x/y/z channel should have been renamed to a colour channel: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn canonical_rgba_maps_aliases_and_rejects_others() {
        for s in ["R", "r", "RED", "red", "Red"] {
            assert_eq!(canonical_rgba(s), Some("R"), "{s} -> R");
        }
        for s in ["G", "g", "green", "GREEN"] {
            assert_eq!(canonical_rgba(s), Some("G"), "{s} -> G");
        }
        for s in ["B", "b", "blue", "BLUE"] {
            assert_eq!(canonical_rgba(s), Some("B"), "{s} -> B");
        }
        for s in ["A", "a", "alpha", "ALPHA"] {
            assert_eq!(canonical_rgba(s), Some("A"), "{s} -> A");
        }
        for s in ["X", "x", "y", "z", "Z", "depth", "height", "mask", ""] {
            assert_eq!(canonical_rgba(s), None, "{s} must not be a colour channel");
        }
    }

    #[test]
    fn rename_skipped_when_it_would_reorder_channels() {
        // A layer mixing a colour channel (R) with a later-sorting data channel
        // (Z): prefixing R into "rgba.R" would push it after "Z" alphabetically,
        // so the converter must leave the layer untouched rather than scramble
        // the verbatim-copied pixel blocks.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("reorder_src.exr");
        let dst = dir.path().join("reorder_dst.exr");
        write_single_layer(&src, "C", &[("R", 7.0), ("Z", 9.0)]);

        convert_exr(&src, &dst).expect("convert");
        let out: std::collections::HashMap<String, f32> = read_back(&dst).into_iter().collect();

        assert_eq!(out.get("R"), Some(&7.0), "R must keep its name and data");
        assert_eq!(out.get("Z"), Some(&9.0), "Z must keep its name and data");
        assert!(
            !out.keys().any(|k| k.contains('.')),
            "no channel should have been renamed: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn run_conversion_task_processes_all_files() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::channel;

        let in_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();
        const N: usize = 4;
        for i in 0..N {
            write_single_layer(
                &in_dir.path().join(format!("img_{i}.exr")),
                "C",
                &[("R", 1.0), ("G", 2.0), ("B", 3.0), ("A", 4.0)],
            );
        }

        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let summary = run_conversion_task(
            in_dir.path().to_path_buf(),
            out_dir.path().to_path_buf(),
            tx,
            cancel,
        );

        // The returned summary is authoritative for success/failure.
        assert!(
            summary.is_success(),
            "non-cancelled run with all files converting must succeed: {summary:?}"
        );
        assert_eq!(summary.converted, N, "all {N} files must be converted");
        assert_eq!(summary.failed, 0, "no failures expected");
        assert_eq!(summary.total, N, "total must match file count");
        assert!(!summary.cancelled, "must not be cancelled");

        let msgs: Vec<(usize, usize, String)> = rx.iter().collect();
        assert!(!msgs.is_empty(), "must emit progress");
        assert!(
            msgs.iter().all(|(_, total, _)| *total == N),
            "every message reports the same total"
        );
        let max_done = msgs.iter().map(|(d, _, _)| *d).max().unwrap();
        assert_eq!(max_done, N, "all {N} files must complete");

        // The monotonic counter visits 1..=N exactly across per-file messages.
        let mut dones: Vec<usize> = msgs
            .iter()
            .map(|(d, _, _)| *d)
            .filter(|&d| (1..=N).contains(&d))
            .collect();
        dones.sort_unstable();
        dones.dedup();
        assert_eq!(dones, (1..=N).collect::<Vec<_>>());

        // Outputs exist and were renamed to the canonical rgba.* scheme.
        for i in 0..N {
            assert!(
                out_dir.path().join(format!("img_{i}.exr")).exists(),
                "output {i} must exist"
            );
        }
        let sample: std::collections::HashMap<String, f32> =
            read_back(&out_dir.path().join("img_0.exr"))
                .into_iter()
                .collect();
        assert_eq!(sample.get("rgba.R"), Some(&1.0));
        assert_eq!(sample.get("rgba.A"), Some(&4.0));
    }

    #[test]
    fn run_conversion_task_respects_cancellation() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::channel;

        let in_dir = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            write_single_layer(
                &in_dir.path().join(format!("img_{i}.exr")),
                "C",
                &[("R", 1.0), ("G", 2.0), ("B", 3.0)],
            );
        }

        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(true)); // cancelled before it starts
        let summary = run_conversion_task(
            in_dir.path().to_path_buf(),
            out_dir.path().to_path_buf(),
            tx,
            cancel,
        );
        let _drain: Vec<_> = rx.iter().collect();

        // The summary must reflect cancellation.
        assert!(summary.cancelled, "summary must report cancelled");
        assert!(
            !summary.is_success(),
            "cancelled run must not be a success: {summary:?}"
        );
        assert_eq!(summary.total, 3, "total must still report the file count");

        let produced = std::fs::read_dir(out_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("exr"))
            })
            .count();
        assert_eq!(produced, 0, "cancelled run must not write outputs");
    }
}
