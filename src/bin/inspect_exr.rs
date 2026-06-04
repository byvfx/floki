use exr::prelude::*;

fn inspect_file(path: &str) {
    println!("Inspecting: {}", path);
    match MetaData::read_from_file(path, false) {
        Ok(meta) => {
            for (i, header) in meta.headers.iter().enumerate() {
                println!("  Header {}:", i);
                println!("    Layer Name: {:?}", header.own_attributes.layer_name);
                println!("    Data Window: {:?}", header.data_window());
                println!("    Display Window: {:?}", header.shared_attributes.display_window);
                println!("    Channels: {:?}", header.channels.list.iter().map(|c| &c.name).collect::<Vec<_>>());
            }
        }
        Err(e) => {
            println!("Error: {}", e);
        }
    }
    println!("--------------------------------------------------");
}

fn main() {
    let path1 = r"X:\SuplexFX\TPLS2\206_206-0390\houdini\render\redSea_bty\v003\TPLS2_206_206-0390_render_v003.redSea_bty.1001.exr";
    let path2 = r"X:\SuplexFX\TPLS2\206_206-0390\houdini\render\redSea_bty\v003\converted\TPLS2_206_206-0390_render_v003.redSea_bty.1001.exr";
    inspect_file(path1);
    inspect_file(path2);
}
