use std::fs::File;
use std::io::{BufReader, BufWriter};
use exr::block::writer::ChunksWriter;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let input = "X:/SuplexFX/TPLS2/206_206-0370/houdini/render/redSea_bty/v006/TPLS2_206_206-0370_render_v006.redSea_bty.1018.exr";
    let output = "C:/Users/brandon/test_out_fast.exr";
    
    let file = BufReader::new(File::open(input)?);
    let reader = exr::block::read(file, false)?;
    let mut meta = reader.meta_data().clone();

    // Build block index maps for each layer
    let mut block_indices = Vec::new();
    for header in &meta.headers {
        let mut map = std::collections::HashMap::new();
        for (idx, tile) in header.blocks_increasing_y_order().enumerate() {
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
            let header = &meta.headers[layer_idx];
            
            let location = header.get_block_data_indices(&chunk.compressed_block).unwrap();
            let index_in_header = block_indices[layer_idx][&location];
            chunk_writer.write_chunk(index_in_header, chunk).unwrap();
        }
        Ok(())
    })?;
    
    Ok(())
}
