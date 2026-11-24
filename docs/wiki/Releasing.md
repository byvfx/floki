Releases are tag-driven. The Build workflow compiles the cross-platform binaries and fills the GitHub Release notes from the matching `CHANGELOG.md` section. **The release job fails if that `## [X.Y.Z]` section is missing**, so the changelog entry has to exist before the tag.

To cut one:

1. Move the `## [Unreleased]` entries in [`CHANGELOG.md`](https://github.com/byvfx/floki/blob/main/CHANGELOG.md) into a new `## [X.Y.Z] - DATE` section.
2. Add the compare-link line at the bottom of the changelog.
3. Bump `version` in `Cargo.toml`. Build once so `Cargo.lock` picks up the same version.
4. Commit.
5. Push the tag `vX.Y.Z`.

The release binaries are the self-contained vendored-OCIO builds. See [Install](Install#the-three-build-tiers).

Hands-on checks for a shipped build live in [`docs/release-testing/`](https://github.com/byvfx/floki/blob/main/docs/release-testing/). That checklist is not part of this site's nav. The automated suite will not catch a bug whose trigger is a person pausing playback; a real session is the merge gate for playback changes. See [Testing](Testing).
