Floki does not open a window unless wgpu can create a device with `FLOAT32_FILTERABLE`. That feature is what linearly samples the f32 3D LUT. There is no runtime CPU fallback for a missing GPU. An internal CPU path exists for contact-sheet thumbnails and headless tests; it is not a substitute display.

## FLOAT32_FILTERABLE

Startup lists adapters the same way wgpu does, including the `WGPU_BACKEND` filter, and keeps going only if one of them has `FLOAT32_FILTERABLE`. If none qualifies, Floki prints the reason, shows an error dialog, and exits.

The message depends on what the probe found:

* **No adapter at all.** Unset `WGPU_BACKEND` and try again. A value naming a backend this machine does not have leaves the list empty.
* **Adapters, but none with `FLOAT32_FILTERABLE`.** Unset `WGPU_BACKEND` if it is set — it may be hiding a capable adapter. If it is not set, try forcing one with `vulkan`, `dx12`, or `metal`, or launch Floki on the discrete GPU.

If the probe found a capable adapter but the one eframe then picks lacks the feature, the log says to set `WGPU_BACKEND` or select the discrete GPU.

`ubuntu-latest` CI has llvmpipe. An adapter *is* found, and it does not advertise `FLOAT32_FILTERABLE`, so an adapter-only check passes and then `request_device` panics. On-device tests call `crate::gpu::test_device` in [`src/gpu/mod.rs`](https://github.com/byvfx/floki/blob/main/src/gpu/mod.rs), which checks both conditions and skips. See [Testing](Testing#on-device-tests).

## WGPU_BACKEND

wgpu reads `WGPU_BACKEND` when it lists adapters. Typical values are `vulkan`, `dx12`, and `metal`. Leave it unset unless you are steering the probe. A wrong value looks the same as having no GPU.

## RUST_LOG

The app initializes [`env_logger`](https://docs.rs/env_logger). Log lines go to stderr, so launch from a terminal. Prefix the target with `floki=` to drop `wgpu` and `eframe` noise.

```bash
RUST_LOG=floki=debug cargo run --release
```

```powershell
$env:RUST_LOG = "floki=debug"
cargo run --release
```

| `RUST_LOG` value | What you see |
|---|---|
| `floki=info` | Conversion start line, final summary (`N of X files converted`), and any errors |
| `floki=debug` | The above plus a line per converted file and any layer left unchanged by the rename guard |
| `info` | Everything at info level, including `wgpu` / `eframe` startup |
| `floki=info,wgpu=warn` | App info logs while silencing graphics-backend chatter |

> [!NOTE]
> During batch conversion, files run in parallel, so per-file log lines are interleaved. The count in the final summary is authoritative.

Playback logs a 1 Hz `floki::playback` trace (`t1=`, `size_bytes=`, `t1_bytes=<resident>/<budget>`, `evict=`, `fallback=`). A decode is three figures, not one: `decode_cost=` is worker-measured decompress, `apply_lag=` is how long the finished frame waited for the UI thread, and `turnaround=` is the whole submit-to-apply span. Only `turnaround=` feeds the stall watchdog. Frame-time percentiles are recorded only while playback is actually playing.

## Build failures

| Symptom | What it is |
|---|---|
| `couldn't find required command: "cmake"` | shaderc (and the vendored OCIO tier) needs `cmake` on `PATH`. On Windows use [`scripts/run-windows.ps1`](https://github.com/byvfx/floki/blob/main/scripts/run-windows.ps1) — Visual Studio's cmake and ninja are not on `PATH` until `vcvars64` is imported. See [Windows](Install-Windows). |
| Default `cargo build` / `cargo run` fails looking for OpenColorIO | The default feature is `system-ocio`. Use `--no-default-features` for the stub, or install OCIO / set `OPENCOLORIO_ROOT`. See [Install](Install#the-three-build-tiers). |
| `Access is denied. (os error 5)` linking `floki.exe` | The app is still running and holding the binary. Close it, then rebuild. |
| Linux link errors for `xkbcommon`, Wayland, GTK, or ALSA | Install the packages in [Linux](Install-Linux). |
| Vendored build cannot find the OCIO sources | Check out the submodule: `git submodule update --init --recursive`, or `just ocio`. |
| Both `system-ocio` and `vendored` enabled | `vendored-ocio` wins. Don't enable the default feature beside `vendored`. The cargo aliases already pass `--no-default-features --features vendored`. |

Debug builds parse EXRs very slowly. Use `--release`.
