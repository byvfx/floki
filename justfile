# One-shot wrappers for the self-contained (vendored) OpenColorIO build.
#
# These init the OCIO submodule first, so they work from a fresh clone in a
# single command. `just` is optional — the same builds are available with no
# extra tooling via the cargo aliases in .cargo/config.toml (e.g. `cargo ocio-run`).
#
# Prerequisites: a C++ toolchain (MSVC "Desktop development with C++" on Windows;
# clang/gcc elsewhere), cmake >= 3.14, ninja, python3.

# List available recipes.
default:
    @just --list

# Build + run floki with self-contained (vendored) OCIO. Inits the submodule first.
ocio:
    git submodule update --init --recursive
    cargo run --release --features ocio-vendored

# Build (no run) floki with vendored OCIO.
ocio-build:
    git submodule update --init --recursive
    cargo build --release --features ocio-vendored

# Run the floki-ocio static-link smoke test against vendored OCIO.
ocio-test:
    git submodule update --init --recursive
    cargo test --features ocio-vendored
