Windows builds go through [`scripts/run-windows.ps1`](https://github.com/byvfx/floki/blob/main/scripts/run-windows.ps1). The shaderc and OCIO build scripts need `cmake`, `ninja`, and `python`, which on a typical dev box are bundled inside Visual Studio and Anaconda rather than on `PATH`. A bare `cargo build` dies with `couldn't find required command: "cmake"`.

The wrapper imports `vcvars64`, puts those tools on `PATH`, points the default `system-ocio` backend at a vcpkg OpenColorIO, then shells out to cargo:

```powershell
scripts\run-windows.ps1 run
scripts\run-windows.ps1 test
scripts\run-windows.ps1 build
scripts\run-windows.ps1 soak -- <file.exr>
```

| Task | What it runs |
|---|---|
| `run` (the default) | `cargo run --release` |
| `test` | `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test --all-targets` against the default `system-ocio` backend |
| `build` | `cargo build --release` |
| `soak -- <file.exr>` | Playback capture. See [`docs/playback/soak-checklist.md`](https://github.com/byvfx/floki/blob/main/docs/playback/soak-checklist.md). |

`run` forwards extra arguments:

```powershell
scripts\run-windows.ps1 run -- "D:\shots\comp.exr"
```

Paths default to a common layout and are overridable via `FLOKI_VCPKG`, `FLOKI_ANACONDA`, and `FLOKI_VSPATH`. The script is a **local dev convenience only** — CI does not use it. Its `test` task checks the default `system-ocio` backend. CI's Test & Lint job passes `--no-default-features` instead. See [Testing](Testing).

> [!WARNING]
> Close Floki before building. A running instance holds `floki.exe` and the link step fails with `Access is denied. (os error 5)`.

The three OCIO tiers are the same as on every other OS. [Install](Install#the-three-build-tiers) lists them. The vendored tier needs the MSVC "Desktop development with C++" workload, `cmake` ≥ 3.14, `ninja` or `make`, `python3`, and the OCIO submodule. The system tier links the vcpkg OCIO the script points at; it does not cmake-build OCIO.

Logging:

```powershell
$env:RUST_LOG = "floki=debug"
scripts\run-windows.ps1 run
```

`WGPU_BACKEND` and a missing `FLOAT32_FILTERABLE` adapter are covered in [Troubleshooting](Troubleshooting). The status-bar VRAM figure is macOS/Metal only; Windows shows RAM.
