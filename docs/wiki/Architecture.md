The app is one binary backed by a library crate, so benches and integration tests can reach the internals, plus a standalone `floki-ocio` crate. Entry is [`src/main.rs`](https://github.com/byvfx/floki/blob/main/src/main.rs): it builds the eframe app and enables the wgpu `FLOAT32_FILTERABLE` device feature. `ExrApp::new` constructs `GpuState` once and stashes it in `render_state.renderer.callback_resources`.

Data flow: [`src/app.rs`](https://github.com/byvfx/floki/blob/main/src/app.rs) (state, menus, persistence) → [`src/exr_loader.rs`](https://github.com/byvfx/floki/blob/main/src/exr_loader.rs) (parse → `ExrData`) → [`src/viewer.rs`](https://github.com/byvfx/floki/blob/main/src/viewer.rs) (canvas, sampling, per-frame uniforms) → [`src/gpu/mod.rs`](https://github.com/byvfx/floki/blob/main/src/gpu/mod.rs) and [`src/gpu/shader.wgsl`](https://github.com/byvfx/floki/blob/main/src/gpu/shader.wgsl).

## Rendering

One WGSL shader runs through an `egui_wgpu::CallbackTrait` (`ExrCallback`). It binds four bind groups in fixed order: `tex_a`, `tex_b`, `uniforms`, `lut`. Exposure, gamma, sRGB, channel isolation, the absolute difference, and the LUT run on the GPU. `viewer.rs` has a CPU path for contact-sheet thumbnails and headless tests. It is not a missing-GPU fallback. Launch exits if no adapter offers `FLOAT32_FILTERABLE`.

Five things stay in lockstep:

1. The `Uniforms` struct in `src/gpu/mod.rs` and the WGSL uniform struct, including `pad*` fields. `uniforms_size_is_16_byte_aligned` asserts the size. Bump it when the struct grows.
2. `channel_mode` integers (`RGB=0, R=1, G=2, B=3, A=4`). The source of truth is `ChannelMode::as_u32` in `viewer.rs`. The shader mirrors it.
3. `ExrCallback::paint` force-resets the wgpu viewport to the full physical screen. egui_wgpu otherwise clips the quad to the primitive's box and the screen-space math breaks.
4. The accumulate fold (#257). Each layer-stack fold is its own render pass over a full-target clear, so a fold that misses a pixel loses it. `vs_main` rasterizes `fold_min..fold_max` (the running union of the layer rects, from `DrawCtx::fold_rect`), and `fs_main` re-emits the prior accumulation outside the layer. Pass 2's scissor covers that same union. Get any one wrong and the composite clips to one layer's rect — everything outside reads back as the α=−1 sentinel (black). `accumulate_preserves_the_union_of_disjoint_layer_rects` is the regression test.
5. `BlitUniforms` and its WGSL mirror inside the `BLIT_SHADER` string. `blit_uniforms_size_is_16_byte_aligned` asserts the size. `display_min` / `display_max` is everything the display stage covers (the display window unioned with every layer rect) and normalizes the background gradient. `dim_min` / `dim_max` is the display window alone and gates the overscan dim (#251). The overscan dim is applied in the blit, not `fs_main`: under OCIO the dim has to happen after the transform, and `fs_main` discards `overscan_factor` for comp layers.

## Modules

**App shell.** `main.rs` (eframe / wgpu init). `app.rs` (window, menus, async EXR and LUT loading, snapshots, persistence, the panel layout). `resource_monitor.rs` (throttled RAM / GPU-VRAM sampler for the status bar; VRAM is macOS/Metal only).

**Image data.** `exr_loader.rs` — threaded decode and logical-layer grouping. `LogicalLayer` regroups a flat channel list by dotted-name prefix (`diffuse.R` / `diffuse.G` → pass `diffuse`), because Blender writes every render pass into a single EXR part. R/G/B/A slot indices are resolved at load time.

**Viewer.** `viewer.rs` — pan/zoom, the five arrangements, sampling, histograms, contact sheets, the GPU path. `layer.rs` — the ordered stack (blend, opacity, solo, trim, per-layer AOV, pixel aspect) and `Arrangement` (Stacked, Wipe, Side-by-Side, Diff, Blink).

**GPU.** `gpu/mod.rs` (pipelines, uniforms, bind groups), `shader.wgsl`, `ocio_pass.rs` (two-pass OCIO display and blit), `thumbnail.rs`, `resources.rs` (`GpuState` and the OCIO pass publisher).

**Color and overlays.** `render_math.rs` (exposure / gamma / sRGB). `color/cube.rs` (`.cube` LUTs). `gradient.rs` (diff colormaps and the gradient background). `background.rs` (checker / solid / gradient, sampled the same on every path). `annotation.rs`. `snapshot.rs`. `floki-ocio/` — OCIO over FFI and a GLSL→WGSL transpiler. Always linked. See [Color management](Color-management).

**Playback.** `sequence.rs` (detection, numeric sort, holes). `playback.rs` (transport). `scheduler.rs` (want-list). `cache.rs` (T1 ring). `budget.rs` (RAM budget). `pixels.rs`. `prefetch.rs` (read-ahead into the OS page cache). `proxy_cache.rs` (`~/.floki/proxy-cache`). `tex_upload.rs` (off-thread comp-texture upload). The contracts are [Memory](Memory-contract) and [Concurrency](Concurrency-contract).

**Tools.** `tools.rs` (batch channel rename). `src/bin/` — `convert_dir`, `inspect_exr`, and the scratch `check_types`. See [Batch tools](Batch-tools).

## Persistence

`ExrApp` derives serde `(default)`. Recent files, LUT path, `enable_lut`, and the OCIO path are plain fields. Loaded data, GPU handles, conversion progress, and window-open flags are `#[serde(skip)]`. eframe storage writes the rest.

The layer stack is not saved as `LayerStack`. `LayerId` and `SourceId` are session-scoped. `save()` flattens the live stack into `persisted_layers` (path, name, AOV, blend, opacity, enabled, solo, trim in/out/offset, pixel-aspect override). `restore_comp_layers` replays that list through `add_comp_source`, then writes the per-layer fields back. A missing file is skipped.

Tone math lives in `render_math.rs`. The longer contributor notes that this page is drawn from are [`CLAUDE.md`](https://github.com/byvfx/floki/blob/main/CLAUDE.md) and the architecture section of [`README.md`](https://github.com/byvfx/floki/blob/main/README.md).
