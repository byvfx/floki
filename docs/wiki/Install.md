Prebuilt binaries for Linux, Windows, and macOS are on the [GitHub Releases](https://github.com/byvfx/floki/releases) page. Those artifacts are the self-contained vendored-OCIO builds. Building from source needs [Rust and Cargo](https://rustup.rs/) (Rust 1.92 or newer — `egui` 0.34.3 requires it) and a GPU that supports `FLOAT32_FILTERABLE` to launch.

Platform notes: [Windows](Install-Windows), [macOS](Install-macOS), [Linux](Install-Linux). Color backends: [Color management](Color-management).

## The three build tiers

`floki-ocio` is always linked. The cargo features choose its backend; they do not omit the crate. A bare `cargo run` enables the default `system-ocio` feature and fails at build time without an installed OpenColorIO. `--no-default-features` compiles the pure-Rust stub: the crate still builds, and every OCIO call returns `OcioError::NotCompiled`.

| Tier | Command | Needs | Gets you |
|---|---|---|---|
| **Stub** | `cargo run --release --no-default-features` | Rust, and a GPU with `FLOAT32_FILTERABLE` to launch | No OpenColorIO and no C++ toolchain. Built-in transforms and `.cube` LUTs only. **This is what CI builds, lints, and tests.** |
| **System** (default) | `cargo run --release` | An installed OpenColorIO (`OPENCOLORIO_ROOT`, or Homebrew on macOS), a C++ toolchain for the cxx shim and shaderc, `python3`, `make` or `ninja`, and `cmake` (shaderc's build script uses it; `floki-ocio` does not cmake-build OCIO on this tier). Imath headers from `IMATH_ROOT` or Homebrew `imath` when present. | Full OCIO. OCIO is linked, not built from source. |
| **Vendored** | `cargo ocio-run` | A C++ toolchain, `cmake` ≥ 3.14, `make` or `ninja`, `python3`, and the OCIO submodule | Full OCIO, self-contained — what the [release binaries](https://github.com/byvfx/floki/releases) ship. `cmake` builds OCIO here, as well as shaderc. |

> [!NOTE]
> On a clean machine, use `--no-default-features`. The default build links an *installed* OpenColorIO and fails at build time if it can't find one. The flag selects the pure-Rust stub backend — no native OCIO, no C++ toolchain — which is also what CI builds and lints. Launching still needs a GPU that supports `FLOAT32_FILTERABLE`.

```bash
git clone https://github.com/byvfx/floki.git
cd floki
cargo run --release --no-default-features
```

**System (the default).** A plain `cargo run --release` links an OCIO found via `OPENCOLORIO_ROOT` (any OS) or Homebrew (macOS). Imath headers come from `IMATH_ROOT` if it is set, otherwise from `brew --prefix imath`. This tier does not cmake-build OCIO; shaderc still needs `cmake`, `python3`, and `make` or `ninja`.

**Vendored (what the release binaries ship).** Statically builds OCIO 2.4.2 from the vendored submodule, so nothing needs to be installed system-wide. The first build is slow (minutes); then it is cached.

```bash
cargo ocio-run

# …or, to also init the submodule in one shot (requires `just`):
just ocio
```

Prerequisites: a C++ toolchain (MSVC "Desktop development with C++" on Windows; clang/gcc elsewhere), **cmake ≥ 3.14**, **make or ninja**, **python3**, and the OCIO submodule checked out (`git submodule update --init --recursive` — done automatically by `just ocio`).

If both native features are enabled, `vendored-ocio` wins. See [Color management](Color-management) and [`floki-ocio/README.md`](https://github.com/byvfx/floki/blob/main/floki-ocio/README.md).

## Launching

```bash
floki path/to/frame.exr
```

The first argument is opened through the same path as **File ▸ Open EXR…** and a drag-and-drop, after any saved layer stack has been restored. The file is added as a layer, or the existing layer is focused if that path is already in the stack. A path that does not exist is shown as `No such file: …` in the EXR Info panel and logged; nothing is opened. Further arguments are ignored.

Always build and run with `--release`. Debug EXR parsing is painfully slow.
