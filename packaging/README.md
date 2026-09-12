# packaging/

Distribution metadata and the release process for rift.

## Layout

| Path | Purpose |
|---|---|
| `homebrew/rift.rb` | Homebrew formula that installs the prebuilt release binary. Copy into a tap (`homebrew-rift/Formula/rift.rb`) or `brew install --formula` it directly. |
| `mise/.mise.toml` | Mise tool config; installs the release binary via the `ubi` backend (or `cargo:` from source). |

## Release process

1. Bump the version everywhere and sync `Cargo.lock`:

   ```bash
   scripts/bump-version.sh 0.2.0
   ```

2. Commit, tag, and push the tag:

   ```bash
   git commit -am "release: v0.2.0"
   git tag v0.2.0
   git push origin main --tags
   ```

3. Pushing the `v*` tag triggers `.github/workflows/release.yml`, which builds
   the four target tarballs (`rift-<version>-<target>.tar.gz`) plus `.sha256`
   sidecars and publishes them as a GitHub Release.

4. Once the release is live, refresh the Homebrew formula checksums and commit:

   ```bash
   scripts/bump-version.sh 0.2.0 --shasums
   git commit -am "release: homebrew checksums for v0.2.0"
   git push
   ```

## Build targets

| Target | Runner |
|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-unknown-linux-gnu` | `ubuntu-latest` (cross linker) |
| `x86_64-apple-darwin` | `macos-latest` |
| `aarch64-apple-darwin` | `macos-latest` |

Artifact naming (`rift-<version>-<target>.tar.gz`) is shared by the release
workflow, the Homebrew formula URLs, the Mise `ubi` auto-detection, and the aqua
`asset` template — keep them in sync if you rename anything.
