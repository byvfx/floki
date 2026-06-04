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
            let _ = sender.send((0, 0, format!("Failed to read directory: {}", e)));
            return;
        }
    }

    let total = files_to_process.len();
    if total == 0 {
        let _ = sender.send((0, 0, "No EXR files found in directory.".to_string()));
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        let _ = sender.send((0, 0, format!("Failed to create output directory: {}", e)));
        return;
    }

    // Shared monotonic counter: files convert in parallel and finish out of
    // order, but progress must only ever move forward. Each file emits exactly
    // one message (on completion) carrying the cumulative completed count.
    let completed = Arc::new(AtomicUsize::new(0));

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
                Ok(_) => format!("Converted: {}", file_name),
                Err(e) => format!("Error on {}: {}", file_name, e),
            };
            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = s.send((done, total, msg));
        });

    // Terminal message, then `sender` drops and the channel disconnects — the UI
    // uses that disconnect to detect completion (also covers cancellation).
    let done = completed.load(Ordering::Relaxed);
    let cancelled = cancel_flag.load(Ordering::Relaxed);
    let (count, final_msg) = if cancelled {
        (done, format!("Cancelled — {}/{} files processed", done, total))
    } else {
        (total, format!("Complete — {} file(s) converted", done))
    };
    let _ = sender.send((count, total, final_msg));
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

    let channel_remap = ["R", "G", "B", "A"];

    for (layer_idx, layer) in meta.headers.iter_mut().enumerate() {
        if layer_idx == 0 {
            layer.own_attributes.layer_name = Some(Text::from("rgba"));
        } else if let Some(_name) = &layer.own_attributes.layer_name {
            // Keep existing layer name
        } else {
            layer.own_attributes.layer_name = Some(Text::from(format!("layer_{}", layer_idx).as_str()));
        }

        let prefix = if layer_idx == 0 {
            "rgba".to_string()
        } else if let Some(name) = &layer.own_attributes.layer_name {
            name.to_string()
        } else {
            format!("layer_{}", layer_idx)
        };

        for (ch_idx, channel) in layer.channels.list.iter_mut().enumerate() {
            let suffix = if ch_idx < channel_remap.len() {
                channel_remap[ch_idx]
            } else {
                "A" // fallback
            };
            channel.name = Text::from(format!("{}.{}", prefix, suffix).as_str());
        }

        layer.channels.list.sort_by(|a, b| a.name.to_string().cmp(&b.name.to_string()));
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
