# Releasing

A release is a `vX.Y.Z` tag on `main`. The `release` workflow builds the archives, writes `SHA256SUMS`, attests the archives and creates the GitHub release.

1. Bump `version` in the workspace `Cargo.toml` and refresh `Cargo.lock`.
2. In `CHANGELOG.md`, move the `[Unreleased]` entries into a `## [X.Y.Z] - YYYY-MM-DD` section (a section still marked `Unreleased` fails the run) and fix the compare links at the bottom.
3. Set `DEFAULT_VERSION` in `scripts/install.sh` and the tag in the README Install section to `X.Y.Z`.
4. Merge the change to `main`.
5. Tag the merge commit and push the tag: `git tag -s vX.Y.Z && git push origin vX.Y.Z`.
6. Watch the `release` run. `verify` fails if the tag differs from the Cargo version, is not on `main`, or has no changelog section.
7. Check the release page for four archives and `SHA256SUMS`, and run `gh attestation verify` on one archive.

To test the workflow without a release, run it by hand from the Actions tab (`workflow_dispatch`). A dry run builds the archives and keeps them as workflow artifacts. It publishes nothing, even when started on a tag: only a pushed tag publishes.

`install.sh` accepts `CODA_RELEASE_BASE` (a `file://` URL) only when `CODA_INSTALL_TEST=1`. It exists for the installer tests and is not a user feature.
