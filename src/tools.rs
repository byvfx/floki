use exr::prelude::*;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub fn run_conversion_task(
    input_dir: PathBuf,
    output_dir: PathBuf,
    sender: Sender<(usize, usize, String)>,
    cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut files_to_process = Vec::new();
    
    // Read all .exr files in the input directory
    match std::fs::read_dir(&input_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext.to_ascii_lowercase() == "exr" {
                            files_to_process.push(path);
                        }
                    }
                }
            }
        }
        Err(e) => {
            log::error!("Failed to read input directory {:?}: {}", input_dir, e);
            let _ = sender.send((0, 0, format!("Failed to read directory: {}", e)));
            return;
        }
    }

    let total = files_to_process.len();
    if total == 0 {
        log::warn!("No EXR files found in {:?}", input_dir);
        let _ = sender.send((0, 0, "No EXR files found in directory.".to_string()));
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        log::error!("Failed to create output directory {:?}: {}", output_dir, e);
        let _ = sender.send((0, 0, format!("Failed to create output directory: {}", e)));
        return;
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

fn convert_exr(in_path: &Path, out_path: &Path) -> std::result::Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufReader, BufWriter};
    use std::fs::File;
    use exr::block::writer::ChunksWriter;

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
            log::debug!(
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
    exr::block::writer::write_chunks_with(out_file, meta.headers.clone(), false, |_, chunk_writer| {
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
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::convert_exr;
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
            !out.keys().any(|k| k.ends_with(".R") || k.ends_with(".G") || k.ends_with(".B")),
            "no x/y/z channel should have been renamed to a colour channel: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }
}
