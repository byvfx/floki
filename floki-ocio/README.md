# floki-ocio

UI- and graphics-API-agnostic [OpenColorIO](https://opencolorio.org/) (OCIO v2) bindings.
Consumed by **floki** (the EXR viewer) and, later, the USD scene assembler.

It exposes three things, none of which depend on egui/wgpu:

- **Config + enumeration** (`OcioConfig`) — load from a file, a built-in (`ocio://default`),
  or `$OCIO`; list color spaces / displays / views.
- **CPU processing** (`CpuProcessor`) — apply a display transform to RGBA f32 in place
  (thumbnails, batch baking, texture color-space resolution).
- **GPU shader bundle** (`GpuShaderBundle`) — OCIO-generated fragment shader as SPIR-V
  (+ optional WGSL) with LUT textures and binding reflection.

## Backends

A native OCIO must be linked via exactly one cargo feature. With neither, the crate compiles
to pure-Rust stubs and every OCIO call returns `OcioError::NotCompiled` (so it never forces a
C++ toolchain on unrelated builds).

| Feature         | What it does                                                                      |
|-----------------|-----------------------------------------------------------------------------------|
| `system-ocio`   | Links an externally-provided OCIO (Homebrew / `OPENCOLORIO_ROOT`). **Dev default** — fast, no cmake. |
| `vendored-ocio` | Statically builds OCIO 2.4.2 from the vendored submodule via cmake. **Distributable** — the binary needs no installed OCIO. |

If both are enabled, `vendored-ocio` wins (a self-contained build never falls back to a system OCIO).

## Build prerequisites (for the native backends)

- A C++ toolchain: clang+libc++ (macOS), MSVC (Windows), gcc/clang+libstdc++ (Linux).
- `python3` and a build tool (`make` or `ninja`) on PATH — needed by the GLSL→SPIR-V (shaderc) step.
- **`vendored-ocio` additionally needs `cmake`** to build OCIO. CMake ≥ 3.14; CMake 4 works
  (the build sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` as a var **and** an env var so OCIO's bundled
  ext deps — which still declare a pre-3.5 minimum — configure under it).

## Vendored OCIO

The OCIO source is a git submodule pinned to **v2.4.2** (matched to the USD app's VFX Reference
Platform year, so both link the same OCIO). After cloning floki:

```sh
git submodule update --init --recursive
```

`build.rs` then cmake-builds it static (`BUILD_SHARED_LIBS=OFF`) with apps/tests/python/docs off
and `OCIO_INSTALL_EXT_PACKAGES=ALL` (OCIO builds its own Imath/yaml-cpp/expat/pystring/minizip-ng),
discovers the installed static archives, and links them (OpenColorIO first) plus the C++ runtime.
The first build is slow (it compiles OCIO + all ext deps); subsequent builds are incremental.

Build the app against it from the workspace root:

```sh
cargo build --release --features ocio-vendored
```

Or use the convenience wrappers (defined in the workspace `.cargo/config.toml` and `justfile`):

```sh
cargo ocio-run     # run floki with vendored OCIO (cargo alias; submodule must be checked out)
cargo ocio-build   # build only
cargo ocio-test    # static-link smoke test

just ocio          # inits the submodule first, then runs (one-shot from a fresh clone; needs `just`)
```
