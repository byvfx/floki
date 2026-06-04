use exr::prelude::*;
use std::fs::File;
use std::io::{BufReader, BufWriter};

fn main() {
    let input = "X:/SuplexFX/TPLS2/206_206-0370/houdini/render/redSea_bty/v006/TPLS2_206_206-0370_render_v006.redSea_bty.1018.exr";
    
    // Read headers and chunks
    let file = BufReader::new(File::open(input).unwrap());
    let reader = exr::block::read(file, false).unwrap();
    let meta_data = reader.meta_data().clone();
    
    let () = reader; // Cause an error to see what methods reader has
}
