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
            // Part count + total channel count decide whether a slow frame is a
            // decode or a bandwidth problem.
            let total_channels: usize = meta.headers.iter().map(|h| h.channels.list.len()).sum();
            // The number that actually sizes the T1 ring: `ExrData::approx_bytes`
            // sums the *decoded* sample buffers at their native EXR sample size,
            // so file size (compressed) says nothing about cache occupancy. Mirror
            // that here — half=2, float=4, uint=4 — per part, since parts can
            // carry different data windows.
            let decoded: u64 = meta
                .headers
                .iter()
                .map(|h| {
                    let px = h.data_window().size.area() as u64;
                    h.channels
                        .list
                        .iter()
                        .map(|c| px * u64::from(c.sample_type.bytes_per_sample() as u32))
                        .sum::<u64>()
                })
                .sum();
            println!(
                "  Parts: {}  ·  Channels (all parts): {total_channels}",
                meta.headers.len()
            );
            println!(
                "  Decoded size: {decoded} bytes ({:.1} MB) — this is what sizes the T1 ring",
                decoded as f64 / (1024.0 * 1024.0)
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
                // Sample type per channel, not just the name: a float32 crypto or
                // depth pass costs twice a half beauty channel in the ring.
                println!(
                    "    Channels ({}): {:?}",
                    header.channels.list.len(),
                    header
                        .channels
                        .list
                        .iter()
                        .map(|c| format!("{}:{:?}", c.name, c.sample_type))
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
