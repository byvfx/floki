//! Benchmarks for the EXR decode + logical-layer grouping hot path
//! ([`ExrData::load`]) — the headline number from the June 2026 audit (#27).
//!
//! Two tiers (see `assets/perf/README.md`):
//! - **Synthetic** fixtures written to a temp dir at startup. Reproducible,
//!   committed-binary-free, run everywhere including CI. They isolate the two
//!   cost drivers: pixel decode (resolution / compression) and channel grouping
//!   (channel count).
//! - **Local real renders** discovered in `$FLOKI_PERF_FIXTURES` (default
//!   `assets/perf/`). Skipped with a notice when the dir is absent or empty, so
//!   `cargo bench` still runs clean on a fresh clone.

use std::path::{Path, PathBuf};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use exr::prelude::*;
use floki::exr_loader::ExrData;

/// Write a flat RGBA EXR of the given size and compression.
fn write_rgba(path: &Path, w: usize, h: usize, encoding: Encoding) {
    let mut list = smallvec::SmallVec::new();
    for (i, name) in ["R", "G", "B", "A"].into_iter().enumerate() {
        // Deterministic, non-constant data so compression has realistic work.
        let base = 0.1 * (i as f32 + 1.0);
        let samples: Vec<f32> = (0..w * h)
            .map(|p| base + ((p % 97) as f32) / 97.0)
            .collect();
        list.push(AnyChannel::new(Text::from(name), FlatSamples::F32(samples)));
    }
    let layer = Layer::new(
        (w, h),
        LayerAttributes::default(),
        encoding,
        AnyChannels::sort(list),
    );
    Image::from_layer(layer)
        .write()
        .to_file(path)
        .expect("write rgba exr fixture");
}

/// Write a Blender-style single-part EXR whose channels encode `passes` render
/// passes by dotted prefix (`ViewLayer.Pass{i}.R/G/B/A`) — `passes * 4`
/// channels. Small resolution: this fixture stresses channel grouping, not decode.
fn write_blender_wide(path: &Path, w: usize, h: usize, passes: usize, encoding: Encoding) {
    let mut list = smallvec::SmallVec::new();
    for p in 0..passes {
        for comp in ["R", "G", "B", "A"] {
            let name = format!("ViewLayer.Pass{p}.{comp}");
            list.push(AnyChannel::new(
                Text::from(name.as_str()),
                FlatSamples::F32(vec![0.5; w * h]),
            ));
        }
    }
    let layer = Layer::new(
        (w, h),
        LayerAttributes::default(),
        encoding,
        AnyChannels::sort(list),
    );
    Image::from_layer(layer)
        .write()
        .to_file(path)
        .expect("write blender-wide exr fixture");
}

fn bench_synthetic(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("temp dir for synthetic fixtures");

    // (label, path, optional decoded byte size for throughput, writer)
    let rgba_512 = dir.path().join("rgba_512_zip.exr");
    write_rgba(&rgba_512, 512, 512, Encoding::FAST_LOSSLESS);

    let rgba_512_raw = dir.path().join("rgba_512_uncompressed.exr");
    write_rgba(&rgba_512_raw, 512, 512, Encoding::UNCOMPRESSED);

    let rgba_1080p = dir.path().join("rgba_1080p_zip.exr");
    write_rgba(&rgba_1080p, 1920, 1080, Encoding::FAST_LOSSLESS);

    let blender_wide = dir.path().join("blender_120ch_zip.exr");
    write_blender_wide(&blender_wide, 128, 128, 30, Encoding::FAST_LOSSLESS);

    let mut group = c.benchmark_group("exr_load/synthetic");

    let rgba_bytes = |w: usize, h: usize| (w * h * 4 * 4) as u64; // 4 ch * f32

    group.throughput(Throughput::Bytes(rgba_bytes(512, 512)));
    group.bench_function("rgba_512_zip", |b| {
        b.iter(|| ExrData::load(std::hint::black_box(&rgba_512)).unwrap())
    });
    group.bench_function("rgba_512_uncompressed", |b| {
        b.iter(|| ExrData::load(std::hint::black_box(&rgba_512_raw)).unwrap())
    });

    group.throughput(Throughput::Bytes(rgba_bytes(1920, 1080)));
    group.bench_function("rgba_1080p_zip", |b| {
        b.iter(|| ExrData::load(std::hint::black_box(&rgba_1080p)).unwrap())
    });

    // Grouping-bound: 120 channels, tiny pixels. No byte-throughput (not decode-bound).
    group.throughput(Throughput::Elements(120));
    group.bench_function("blender_120ch_zip", |b| {
        b.iter(|| ExrData::load(std::hint::black_box(&blender_wide)).unwrap())
    });

    group.finish();
}

/// Real-render fixtures: every `*.exr` under `$FLOKI_PERF_FIXTURES` (default
/// `assets/perf/`). Skipped with a notice when absent so CI / fresh clones still
/// run the synthetic suite clean.
fn bench_local(c: &mut Criterion) {
    let dir = std::env::var_os("FLOKI_PERF_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/perf"));

    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("exr")))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();

    if files.is_empty() {
        eprintln!(
            "exr_load/local: no fixtures in {} — skipping real-render benches. \
             Drop .exr files there (or set FLOKI_PERF_FIXTURES) to enable them.",
            dir.display()
        );
        return;
    }

    let mut group = c.benchmark_group("exr_load/local");
    for path in &files {
        let label = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string();
        // Sanity-load once so a bad fixture fails loudly rather than mid-measure.
        if let Err(e) = ExrData::load(path) {
            eprintln!("exr_load/local: skipping {label} — load failed: {e}");
            continue;
        }
        group.bench_function(label, |b| {
            b.iter(|| ExrData::load(std::hint::black_box(path)).unwrap())
        });
    }
    group.finish();
}

criterion_group!(benches, bench_synthetic, bench_local);
criterion_main!(benches);
