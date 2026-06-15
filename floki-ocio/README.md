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

| Feature         | What it does                                                             |
|-----------------|--------------------------------------------------------------------------|
| `vendored-ocio` | Builds OCIO from the vendored submodule via cmake (reproducible).         |
| `system-ocio`   | Links an externally-provided OCIO (e.g. the one the USD app builds).      |

## Build prerequisites (for the native backends)

- `cmake`, `ninja`, `python3` on PATH (OCIO via cmake; the GLSL→SPIR-V step needs ninja+python).
- A C++ toolchain: clang+libc++ (macOS), MSVC (Windows), gcc/clang+libstdc++ (Linux).

## Vendored OCIO (Stage 1)

The OCIO source is a git submodule pinned to a release tag (matched to the USD app's
VFX Reference Platform year; default 2.4.x):

```sh
git submodule add -b v2.4.2 https://github.com/AcademySoftwareFoundation/OpenColorIO \
    floki-ocio/vendor/OCIO
```

`build.rs` then cmake-builds it with apps/tests/python off and `OCIO_INSTALL_EXT_PACKAGES=ALL`
(OCIO builds its own Imath/yaml-cpp/expat/pystring/minizip), static-linked.

## Status

Scaffolding only. `build.rs` is a no-op without a backend feature and panics (intentionally)
if a backend is enabled before the Stage 1 cmake/cxx wiring lands. See
`plans/i-want-to-map-modular-hinton.md`.
