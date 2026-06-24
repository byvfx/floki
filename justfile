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
    cargo run --release --no-default-features --features vendored

# Build (no run) floki with vendored OCIO.
ocio-build:
    git submodule update --init --recursive
    cargo build --release --no-default-features --features vendored

# Run the floki-ocio static-link smoke test against vendored OCIO.
ocio-test:
    git submodule update --init --recursive
    cargo test --no-default-features --features vendored

# Apply the auto-fixable pedantic/style lints (use_self, uninlined_format_args, etc.),
# then reformat. Zero-behavior-risk cleanup; run `cargo test` afterwards. See issue #68.
lint-fix:
    cargo clippy --fix --allow-dirty --allow-staged --all-targets -- \
        -W clippy::use_self \
        -W clippy::uninlined_format_args \
        -W clippy::redundant_clone \
        -W clippy::format_push_string
    cargo fmt
