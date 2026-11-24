Install the same GUI libraries CI installs before the first build:

```bash
sudo apt-get install -y libxkbcommon-dev libwayland-dev libx11-dev libasound2-dev libudev-dev libgtk-3-dev
```

Rust 1.92 or newer, then the stub tier (no OpenColorIO, no C++ toolchain):

```bash
git clone https://github.com/byvfx/floki.git
cd floki
cargo run --release --no-default-features
```

The **system** tier (`cargo run --release`) links an OpenColorIO found via `OPENCOLORIO_ROOT`, an install prefix that contains `include/` and `lib/`. Imath headers come from `IMATH_ROOT` when that variable is set. This tier does not cmake-build OCIO. It still needs a C++ toolchain (gcc or clang with libstdc++), `python3`, `cmake`, and `make` or `ninja`, because shaderc's build script uses them.

The **vendored** tier builds OCIO 2.4.2 from the submodule. `cmake` ≥ 3.14 is required here (CMake 4 works: the build sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` as a variable and an environment variable). Same helpers as the other platforms:

```bash
git submodule update --init --recursive
cargo ocio-run
```

The three tiers, and what each one links, are in [Install](Install#the-three-build-tiers) and [Color management](Color-management).

> [!NOTE]
> `ubuntu-latest` CI has a software Vulkan adapter (llvmpipe) that does **not** advertise `FLOAT32_FILTERABLE`. The app will not launch on that adapter, and the on-device tests skip there. A real GPU is required to open a window. See [Troubleshooting](Troubleshooting).

There is no Windows-style wrapper script. Logging is the same `RUST_LOG=floki=debug` prefix. The VRAM figure in the status bar is macOS/Metal only; Linux shows RAM.
