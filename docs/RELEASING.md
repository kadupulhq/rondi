# Releasing

The three crates (`rondi`, `rondi-server`, `rondi-cli`) share one version and are released together, as [VERSIONING.md](../VERSIONING.md) describes. A release is a signed `vX.Y.Z` tag, a GitHub release, and the three crates on crates.io. Nothing publishes to crates.io automatically.

## What the release workflow does

[`.github/workflows/release.yml`](../.github/workflows/release.yml) runs when a tag matching `v*` is pushed. It calls the organisation's reusable workflow `kadupulhq/.github/.github/workflows/release.yml`, pinned to a full commit SHA, with `generate-notes: true`. That workflow:

1. Checks out the tag and rejects it unless the part after `v` is a Semantic Versioning 2.0.0 version. A version containing `-` (for example `0.2.0-rc.1`) is marked as a prerelease.
2. Runs `gh release create <tag> --title <tag> --generate-notes`, adding `--prerelease` when needed. The notes are generated from merged pull requests and grouped by the labels in [`.github/release.yml`](../.github/release.yml).

It does not read `CHANGELOG.md` (that path is only used when `generate-notes` is false), does not check that the tag matches the version in `Cargo.toml`, does not build or attach binaries (the `tarball` and `image` inputs are off), and does not publish to crates.io. Those checks are manual steps below.

## Checklist

Replace `X.Y.Z` with the new version throughout.

1. **Choose the version.** Apply [VERSIONING.md](../VERSIONING.md) to the changes listed under `## [Unreleased]` in `CHANGELOG.md`. Before 1.0, breaking changes go in the minor number.

2. **Open a release pull request** from a branch such as `release/vX.Y.Z` with these edits:
   - In `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` (UTC date) and add a new empty `## [Unreleased]` above it.
   - In the root `Cargo.toml`, set `workspace.package.version` to `X.Y.Z`, and set the `version` of the `rondi` and `rondi-server` entries under `workspace.dependencies` to the same value. crates.io records that requirement in place of the path, so a stale value makes a published crate depend on the previous release. Cargo refuses to build when the requirement no longer matches the path crate's version, which catches a missed edit for a minor or major bump.
   - Run `mise exec -- cargo update --workspace` so `Cargo.lock` records the new member versions.

3. **Verify the release commit** with the pinned toolchain:

   ```sh
   mise exec -- cargo fmt --all -- --check
   mise exec -- cargo clippy --workspace --all-targets --locked -- -D warnings
   RUSTDOCFLAGS='-D warnings' mise exec -- cargo doc --workspace --no-deps --locked
   mise exec -- cargo publish --workspace --dry-run --locked
   ```

   CI only builds with the toolchain pinned in `.mise.toml`, so also check that the declared `rust-version` still builds, replacing `<rust-version>` with the value in `Cargo.toml`:

   ```sh
   rustup toolchain install <rust-version> --profile minimal
   rustup run <rust-version> cargo check --workspace --locked
   ```

   `cargo publish --workspace --dry-run` packages all three crates, builds `rondi-server` and `rondi-cli` against the packaged `rondi` through a temporary local registry, and stops before uploading. Read its output for unexpected files; `cargo package -p <crate> --list` shows the exact contents. The full test suite with the RRDtool oracle runs in CI (`rust.yml`); wait for it to pass on the pull request.

4. **Merge** the pull request once CI passes and review is complete.

5. **Tag the merge commit** on `main` and push the tag:

   ```sh
   git fetch origin
   git checkout --detach origin/main
   git tag -s vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```

   Confirm the Release workflow run for the tag succeeded and that the GitHub release exists with the generated notes. Edit the release text if the generated notes miss something the changelog states.

6. **Publish to crates.io** from a clean checkout of the tag, in dependency order:

   ```sh
   mise exec -- cargo publish -p rondi --locked
   mise exec -- cargo publish -p rondi-server --locked
   mise exec -- cargo publish -p rondi-cli --locked
   ```

   `mise exec -- cargo publish --workspace --locked` does the same in the same order. A publish cannot be undone, so do this only after step 5 succeeded. Use a crates.io API token limited to the publish scopes for these three crates.

7. **Check crates.io and docs.rs.**
   - Each crate page on crates.io shows the version, the crate README, the license `MIT OR Apache-2.0`, the repository link, keywords, and categories.
   - `https://docs.rs/crate/<crate>/X.Y.Z/builds` shows a successful build for `rondi` and `rondi-server`. `rondi-cli` has no library, so docs.rs has no API pages for it.
   - The `rondi` and `rondi-server` documentation pages list the expected public items and no broken intra-doc links.
   - `cargo install rondi-cli --version X.Y.Z` in a clean environment installs a `rondi` binary that reports `X.Y.Z` with `rondi --version`.

## Yanking

`cargo yank` does not delete a release. It stops new lockfiles from selecting the version while existing lockfiles keep working.

Yank a version only when using it is harmful: it corrupts or silently misreads `.rrd` or `.rondi` data, has a security vulnerability, or cannot build. Publish the fixed release first, then yank each affected crate:

```sh
cargo yank --version X.Y.Z rondi
```

Record the yank and its reason under the fixed release in `CHANGELOG.md` and in the GitHub release notes. Do not yank for ordinary bugs or compatibility differences; release a fix instead. Undo a mistaken yank with `cargo yank --version X.Y.Z --undo <crate>`.
