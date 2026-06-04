use exr::prelude::*;
use exr::block::reader::read_meta_data;
use exr::meta::header::Header;

fn main() {
    let input = "X:/SuplexFX/TPLS2/206_206-0370/houdini/render/redSea_bty/v006/TPLS2_206_206-0370_render_v006.redSea_bty.1018.exr";
    let meta_data = read_meta_data(input).unwrap();
    println!("{:#?}", meta_data.headers);
}
