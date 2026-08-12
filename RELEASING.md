# Releasing cleandl

Releases are built from public source by GitHub Actions using `dist`. The
workflow assembles and verifies the artifacts first, then creates the GitHub
release only after every required build succeeds.

The workflow contains four small workarounds for `dist` 0.32.0: it corrects
the SBOM upload expression to `steps.cargo-cyclonedx.outputs.paths`, uses
`cargo-cyclonedx` 0.5.9 because the generated 0.5.5 bootstrap cannot read Cargo
lockfile format 4, removes blank lines from generated checksum files, and runs
that normalization under Bash on Windows. Consequently,
`allow-dirty = ["ci"]` is set in `dist-workspace.toml`. When upgrading `dist`,
regenerate and retest the workflow before removing the exception.

## Prepare a release

For the next release, `0.3.0`:

1. Set the package version in `Cargo.toml` to `0.3.0`, run `cargo check` to
   update `Cargo.lock`, then run `cargo check --locked` to verify it. Commit the
   updated lockfile with the release changes.
2. Move the relevant entries in `CHANGELOG.md` from `Unreleased` into a
   `0.3.0` section and commit the changes.
3. Validate the release plan:

   Install dist first using:

   ```bash
   curl --proto '=https' --tlsv1.2 -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh | sh
   ```

   ```bash
   dist plan --tag v0.3.0
   ```

4. Tag that exact commit and push the tag:

   ```bash
   git tag v0.3.0
   git push origin v0.3.0
   ```

The tag must match the version in `Cargo.toml`. Do not reuse or move an existing
release tag. Add a suffix such as `-rc.1` to both the package version and tag if
a prerelease is needed; GitHub will then publish it as a prerelease rather than
the latest stable version.

## What the workflow publishes

- macOS archives for Apple Silicon and Intel
- Linux archives for ARM64 and x86-64
- a Windows x86-64 ZIP archive
- SHA-256 files for each archive and a unified `sha256.sum`
- a source archive and release manifest
- a standalone CycloneDX `cleandl.cdx.xml` SBOM
- GitHub artifact attestations for release artifacts

The archives contain the `cleandl` executable, `README.md`, `CHANGELOG.md`, and
`LICENSE`. Builds are currently unsigned.

## Verification

Download an archive and its adjacent checksum, then verify it. On macOS or
Linux:

```bash
shasum -a 256 -c cleandl-aarch64-apple-darwin.tar.gz.sha256
```

The command must report `OK` without a checksum-file formatting warning.

Verify its GitHub provenance with GitHub CLI:

```bash
gh attestation verify cleandl-aarch64-apple-darwin.tar.gz \
  --repo chevybeef/cleandl
```

Extract the archive and confirm the client identifies itself correctly:

```bash
tar -xzf cleandl-aarch64-apple-darwin.tar.gz
./cleandl-aarch64-apple-darwin/cleandl --version
```

The version must contain the tagged commit and must not end in `-dirty`.
