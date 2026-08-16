//! Dump an EXR's part / channel / compression layout.
//!
//! The #100 soak manifest instrument: a p99 frame time is uninterpretable
//! without knowing whether the source is 2K zip or 4K DWA across 30 parts, so
//! run this against frame 1 of each sequence before soaking. Takes paths on the
//! command line; with no arguments it falls back to the scratch paths below.

use exr::prelude::*;

/// Scratch paths used when no argument is given (this started as a dev tool).
const FALLBACK: [&str; 2] = [
    r"X:\SuplexFX\TPLS2\206_206-0390\houdini\render\redSea_bty\v003\TPLS2_206_206-0390_render_v003.redSea_bty.1001.exr",
    r"X:\SuplexFX\TPLS2\206_206-0390\houdini\render\redSea_bty\v003\converted\TPLS2_206_206-0390_render_v003.redSea_bty.1001.exr",
];

fn inspect_file(path: &str) {
    println!("Inspecting: {path}");
    match std::fs::metadata(path) {
        Ok(m) => println!("  File size: {} bytes", m.len()),
        Err(e) => println!("  File size: <{e}>"),
    }
    match MetaData::read_from_file(path, false) {
        Ok(meta) => {
            // Part count + total channel count are the two manifest numbers that
            // decide whether a slow frame is a decode or a bandwidth problem.
            let total_channels: usize = meta.headers.iter().map(|h| h.channels.list.len()).sum();
            println!(
                "  Parts: {}  ·  Channels (all parts): {total_channels}",
                meta.headers.len()
            );
            for (i, header) in meta.headers.iter().enumerate() {
                println!("  Header {i}:");
                println!("    Layer Name: {:?}", header.own_attributes.layer_name);
                println!("    Data Window: {:?}", header.data_window());
                println!(
                    "    Display Window: {:?}",
                    header.shared_attributes.display_window
                );
                println!("    Compression: {:?}", header.compression);
                println!(
                    "    Channels ({}): {:?}",
                    header.channels.list.len(),
                    header
                        .channels
                        .list
                        .iter()
                        .map(|c| &c.name)
                        .collect::<Vec<_>>()
                );
            }
        }
        Err(e) => {
            println!("Error: {e}");
        }
    }
    println!("--------------------------------------------------");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        for path in FALLBACK {
            inspect_file(path);
        }
    } else {
        for path in &args {
            inspect_file(path);
        }
    }
}
