Discuss a bug or a large change in an issue before spending time on a pull request.

## Setup

1. Install Rust and Cargo via [rustup](https://rustup.rs/). Rust 1.92 or newer.
2. Fork the repository and clone it.
3. `cargo build --no-default-features` checks the stub build. A plain `cargo build` enables `system-ocio` and fails without an installed OpenColorIO and the C++ toolchain that feature's shim and shaderc need.
4. OCIO work uses `cargo ocio-run` (vendored, self-contained). It needs cmake, make or ninja, python, and a C++ toolchain. See [Color management](Color-management).

On Windows, build through [`scripts/run-windows.ps1`](https://github.com/byvfx/floki/blob/main/scripts/run-windows.ps1) so cmake, ninja, and python are on `PATH`. Close the app first. See [Windows](Install-Windows).

## Checks before a pull request

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --no-default-features -- -D warnings
cargo test --all-targets --no-default-features
```

CI's Test & Lint job runs exactly these three and **blocks the build until they pass**. `--no-default-features` selects the pure-Rust OCIO stub. The runner has no OpenColorIO installed, so without the flag the gate would not build — and CI lints and tests *that* feature set. Running the commands without the flag locally checks something CI never does.

What the suite covers, and how to add a test, is [Testing](Testing).

The pull request itself:

1. Branch for the change.
2. Keep the code commented and split so UI logic stays decoupled from parsing and processing.
3. Run the three commands above.
4. `cargo fmt`, then `cargo clippy --all-targets --no-default-features` with warnings fixed.
5. Commit with a message that says what changed.
6. Push the fork and open the pull request.

Prefer the pure-Rust ecosystem for file formats and rendering. Avoid heavy new dependencies.

## Bug reports

Include a description, OS, Rust version, and steps. If an `.exr` caused it, link a file you have the right to share.

## Editing these docs

`docs/wiki/` is the source. GitHub's wiki is a mirror, and this site is another view of the same files.

- A push to `main` that touches `docs/wiki/**` runs [`.github/workflows/wiki.yml`](https://github.com/byvfx/floki/blob/main/.github/workflows/wiki.yml), which `rsync`s `docs/wiki/` to `byvfx/floki.wiki.git` with `GITHUB_TOKEN`. The wiki git repo exists only after a first page has been saved in the GitHub wiki UI. Edits made on the wiki itself are overwritten on the next sync.
- [`.github/workflows/docs.yml`](https://github.com/byvfx/floki/blob/main/.github/workflows/docs.yml) runs `pytest docs_site/tests` and `mkdocs build --strict`, and on a push to `main` deploys `./site` to GitHub Pages at <https://byvfx.github.io/floki/>. Pages has to be set to **GitHub Actions** once, under the repo's Settings.
- Page titles and the nav live in [`docs_site/convert.py`](https://github.com/byvfx/floki/blob/main/docs_site/convert.py) (`TITLES`) and [`mkdocs.yml`](https://github.com/byvfx/floki/blob/main/mkdocs.yml). Every non-underscore page must be in both, and in [`docs/wiki/_Sidebar.md`](https://github.com/byvfx/floki/blob/main/docs/wiki/_Sidebar.md).
- Wiki links have no `.md` (`[Install](Install)`). Source files are full GitHub URLs (`https://github.com/byvfx/floki/blob/main/...`), not relative paths out of `docs/wiki/`.
- A `<!-- wiki-only -->` … `<!-- /wiki-only -->` block stays on the GitHub wiki and is dropped from the MkDocs build. `_Sidebar.md` and `_Footer.md` are not copied into the site.
- `docs-site/*.md` is generated on each build and gitignored. Do not edit it. Do not commit the `site/` output.

Dated audits, lint reviews, hardening plans, the DWA investigation, and release-testing checklists stay in `docs/` and off this nav. Link them from a page when a reader needs the file; don't add them to `mkdocs.yml`.

The contributor map of the code is [Architecture](Architecture). Playback invariants are the [memory contract](Memory-contract).
