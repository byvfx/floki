`exr` is not the stock crates.io crate. [`Cargo.toml`](https://github.com/byvfx/floki/blob/main/Cargo.toml) has a `[patch.crates-io]` pointing at a public fork:

```toml
[patch.crates-io]
exr = { git = "https://github.com/byvfx/exrs", branch = "miniz-inflate-1.74.1" }
```

The branch name carries the upstream version it is rebased onto, so it changes on every bump. Check `Cargo.toml` and `Cargo.lock` rather than this snippet. `Cargo.lock` pins the exact commit. CI and fresh clones fetch the fork automatically. Keep `byvfx/exrs` public — a private fork breaks every build.

The fork carries two independent changes. Both have to survive a rebase. Do not push either change upstream to `johannesvollmer/exrs`.

## miniz_oxide instead of zune-inflate

Stock `exr` decompresses ZIP and PXR24 blocks with `zune-inflate`, which panics (`assertion failed: bits_left >= LOOKAHEAD`) on some valid streams. The decode runs on a detached rayon worker, so the panic hits rayon's `AbortIfPanic` and `process::abort()` — an uncatchable crash of the whole app.

The fork swaps both call sites to `miniz_oxide` (already used for compression), which returns an `Err` instead of panicking. With that, `ExrData::load` runs parallel decompression again and keeps a `catch_unwind` only as cheap insurance for calling-thread parse panics.

The change is two call sites, in the fork's `src/compression/zip.rs` and `src/compression/pxr24.rs`: replace `zune_inflate::DeflateDecoder` / `.decode_zlib()` with `miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, expected_byte_size)`, then drop the `zune-inflate` dependency from the fork's `Cargo.toml`.

## specific_layer — per-AOV decode

The high-level API offers `first_valid_layer()` and `all_layers()` and nothing between, so there was no way to decode one named part. `load_beauty` could accelerate part 0 and nothing else. Inspecting any other AOV fell back to a full all-parts decode: **260 ms vs 12 ms** on a 16-part / 421 MB render. Against a 24 fps clock, the awaited frame is always superseded before it lands.

`ReadSpecificLayer` in the fork's `src/image/read/layers.rs` is 53 added lines reusing `FirstValidLayerReader`, which already carries a `layer_index` and filters every block against it. The other parts are never decompressed. Floki calls it from `ExrData::load_layer` / `load_layer_proxy_into` (#217).

This only helps **multi-part** files (Karma / Houdini, one AOV per part). Single-part files with prefixed channels (Blender) keep every pass in the same blocks, so part selection skips nothing. `ExrApp::single_layer_part` refuses the fast path there. Playback behavior is on [Playback](Playback).

## Bumping exr

1. In the `byvfx/exrs` clone, branch `miniz-inflate-<newver>` off the new `vX.Y.Z` tag and rebase. Re-apply both changes above if they conflict, then push.
2. In this repo, point `[patch.crates-io]` at the new branch and run `cargo update -p exr` to re-pin `Cargo.lock`. Build and test.
3. Do not push either change upstream. Fork only.

The measured reason not to replace the fork with a C++ OpenEXR rewrite is on the [perf roadmap](Perf-roadmap): on multi-part files the fork is competitive or faster, and a rewrite would drop the miniz panic-safety fix.
