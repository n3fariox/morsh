# Releasing morsh

All crates share the version in the root `Cargo.toml` (`[workspace.package]`).
Releases are cut from `main` with
[cargo-release](https://github.com/crate-ci/cargo-release), configured in
`release.toml`.

## One-time setup

```sh
cargo binstall cargo-release   # or: cargo install cargo-release
```

## Cutting a release

```sh
git checkout main && git pull
cargo release patch            # dry run: shows the bump, commit, and tag
cargo release patch --execute  # bump, commit "chore: release vX.Y.Z", tag vX.Y.Z, push
```

Use `minor` or `major` instead of `patch` as appropriate, or an explicit
version such as `cargo release 0.1.0 --execute`.

cargo-release refuses to run from a branch other than `main` or with
uncommitted changes. Nothing is published to crates.io.

## What happens next

The pushed `vX.Y.Z` tag triggers `.github/workflows/release.yml`, which:

1. Fails early if the tag does not match the `Cargo.toml` version.
2. Builds the binaries for Linux, Windows, and macOS (plus a universal macOS
   archive).
3. Creates the GitHub release with those archives attached.

Once the release is published, `mise` can install it (see the README).
