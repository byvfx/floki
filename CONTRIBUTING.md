# Contributing to Floki

First off, thank you for considering contributing to Floki! It's people like you that make open-source software such a great community.

## Where do I go from here?

If you've noticed a bug or have a feature request, please make sure to open an issue before submitting a PR. It's best to discuss large changes with the maintainers before spending your time writing code.

## Setting up your environment

1. Ensure you have Rust and Cargo installed via [rustup](https://rustup.rs/).
2. Fork the repository and clone it locally.
3. Run `cargo build` to ensure everything compiles correctly on your system.

## Submitting a Pull Request (PR)

1. **Create a new branch** for your feature or bugfix (`git checkout -b feature/my-awesome-feature`).
2. **Make your changes**. Ensure your code is well-commented and clean.
3. **Run the tests** (`cargo test`) to ensure no existing functionality is broken. See [TESTING.md](TESTING.md) for what the suite covers and how to add tests (generate fixtures in a temp dir; keep tests GPU-free).
4. **Format your code** using `cargo fmt`.
5. **Lint your code** using `cargo clippy` and fix any warnings.

> CI runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --all-targets`, and **blocks the build until they pass** — so run them locally first.
6. **Commit your changes** with clear, descriptive commit messages.
7. **Push to your fork** and submit a Pull Request.

## Code Style Guide

- Follow standard Rust formatting guidelines (`cargo fmt` will handle most of this).
- Keep UI logic decoupled from parsing/processing logic where possible.
- Avoid introducing unnecessary heavy dependencies. We prefer the pure-Rust ecosystem when working with file formats and rendering.

## Reporting Bugs

When filing a bug report, please include:
- A clear description of the issue.
- Your OS and Rust version.
- Steps to reproduce the bug.
- If possible, a link to the specific `.exr` file that caused the crash or bug (please ensure you have the right to share the file).

Thanks again for helping make Floki better for everyone!
