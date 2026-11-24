A system OCIO on macOS is Homebrew's `opencolorio` when `OPENCOLORIO_ROOT` is unset. `floki-ocio` falls back to `brew --prefix opencolorio`. Imath headers come from `IMATH_ROOT` (the build adds `$IMATH_ROOT/include`) or `brew --prefix imath`. If neither is set, that extra include is omitted. `build.rs` reruns when `OPENCOLORIO_ROOT` or `IMATH_ROOT` changes.

```bash
brew install opencolorio imath cmake ninja python
cargo run --release
```

That is the **system** tier: OCIO is linked, not cmake-built. shaderc still needs `cmake`, `python3`, and `make` or `ninja`, plus a C++ toolchain (clang and libc++).

The **vendored** tier is the same command as on Linux and Windows, and it is what the macOS release binary ships:

```bash
git submodule update --init --recursive
cargo ocio-run
```

`cargo ocio-run` is the alias in [`.cargo/config.toml`](https://github.com/byvfx/floki/blob/main/.cargo/config.toml): `run --release --no-default-features --features vendored`. `--no-default-features` is required so the default `system-ocio` feature is not enabled beside `vendored`. `just ocio` inits the submodule first, then runs that build. `just` is optional.

On a machine with no OCIO install, the stub build still compiles:

```bash
cargo run --release --no-default-features
```

Colour management is then the built-in transforms and `.cube` LUTs. See [Color management](Color-management).

macOS with an OCIO feature is the configuration that runs the most tests. `metal_tests` needs `target_os = "macos"` and `system-ocio` or `vendored`, so `cargo test --no-default-features --features vendored` on a Mac is the only configuration that runs every test in the repo. Details are on [Testing](Testing).

The status-bar **VRAM** readout uses Metal (`recommendedMaxWorkingSetSize` / `currentAllocatedSize`). It is macOS-only; Windows and Linux show RAM.

Launch still needs an adapter with `FLOAT32_FILTERABLE`. If the one eframe picks does not have it, set `WGPU_BACKEND=metal` or select the discrete GPU. See [Troubleshooting](Troubleshooting).
