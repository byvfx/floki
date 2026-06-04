use exr::prelude::*;
use std::fs::File;
use std::io::{BufReader, BufWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = "X:/SuplexFX/TPLS2/206_206-0370/houdini/render/redSea_bty/v006/TPLS2_206_206-0370_render_v006.redSea_bty.1018.exr";
    let output = "C:/Users/brandon/test_out_fast.exr";
    
    let file = BufReader::new(File::open(input)?);
    let mut reader = exr::block::read(file, false)?;
    let mut meta = reader.meta_data().clone();
    
    // Modify headers
    let channel_remap = ["R", "G", "B", "A"];
    for (layer_idx, layer) in meta.headers.iter_mut().enumerate() {
        if layer_idx == 0 {
            layer.attributes.layer_name = Some(Text::from("rgba"));
        } else if let Some(name) = &layer.attributes.layer_name {
            // Keep existing layer name
        } else {
            layer.attributes.layer_name = Some(Text::from(format!("layer_{}", layer_idx).as_str()));
        }

        let prefix = if layer_idx == 0 {
            "rgba".to_string()
        } else if let Some(name) = &layer.attributes.layer_name {
            name.to_string()
        } else {
            format!("layer_{}", layer_idx)
        };

        for (ch_idx, channel) in layer.channels.list.iter_mut().enumerate() {
            let suffix = if ch_idx < channel_remap.len() {
                channel_remap[ch_idx]
            } else {
                "A"
            };
            channel.name = Text::from(format!("{}.{}", prefix, suffix).as_str());
        }
        layer.channels.list.sort_by(|a, b| a.name.to_string().cmp(&b.name.to_string()));
    }

    // Build block index maps for each layer
    let mut block_indices = Vec::new();
    for header in &meta.headers {
        let mut map = std::collections::HashMap::new();
        // How to iterate over blocks_increasing_y_order?
        for (idx, tile) in header.blocks_increasing_y_order().enumerate() {
            // tile is what type? exr::meta::header::Tile? Or TileDescription?
            // Actually, tile.location is exr::block::chunk::TileCoordinates
            map.insert(tile.location, idx);
        }
        block_indices.push(map);
    }
    
    let out_file = BufWriter::new(File::create(output)?);
    exr::block::writer::write_chunks_with(out_file, meta.headers.clone(), false, |_, mut chunk_writer| {
        let chunks_reader = reader.all_chunks(false).unwrap();
        for chunk_result in chunks_reader {
            let chunk = chunk_result.unwrap();
            let layer_idx = chunk.layer_index;
            
            // Extract the location
            let location = match &chunk.compressed_block {
                exr::block::chunk::CompressedBlock::ScanLine(b) => {
                    // For scanlines, what is TileCoordinates?
                    // Let's use exr::meta::header::BlockIndex or similar.
                    // Wait, what does header.blocks_increasing_y_order() yield?
                    // Let's just panic here and see what the compiler says.
                    panic!("need location");
                },
                _ => panic!("other"),
            };
        }
        Ok(())
    })?;
    
    Ok(())
}
