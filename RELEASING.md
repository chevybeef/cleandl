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

In the steps below, replace `X.Y.Z` with the version being released (for
example `0.7.1`).

**The release workflow does not wait for the lint and test checks.** Pushing a
tag starts the release build on its own, and a release can be published even
while CI on the same commit is red. Everything that CI checks must therefore be
verified locally, and CI on `main` must be green, before the tag is pushed.

1. Set the package version in `Cargo.toml` to `X.Y.Z`, run `cargo check` to
   update `Cargo.lock`, then run `cargo check --locked` to verify it. Commit the
   updated lockfile with the release changes.
2. Move the relevant entries in `CHANGELOG.md` from `Unreleased` into an
   `X.Y.Z` section and commit the changes.
3. Run the same checks CI runs, with the same flags, and fix anything they
   report. Test code is linted too (`--all-targets`), so a warning in
   `src/tests.rs` fails CI just like one in the application code:

   ```bash
   cargo clippy --all-targets --all-features --locked -- -D warnings
   cargo test --locked
   ```

4. Validate the release plan:

   Install dist first using:

   ```bash
   curl --proto '=https' --tlsv1.2 -LsSf https://github.com/axodotdev/cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh | sh
   ```

   ```bash
   dist plan --tag vX.Y.Z
   ```

5. Push `main` on its own first and wait for CI to pass on that commit. A tag
   push uploads only the tag and the commits it points to; it never moves
   `origin/main`.

   ```bash
   git push origin main
   ```

6. Tag that exact commit, confirm the tag points at `HEAD`, and push the tag:

   ```bash
   git tag vX.Y.Z
   git rev-parse vX.Y.Z^{commit}
   git rev-parse HEAD
   git push origin vX.Y.Z
   ```

   The two `rev-parse` commands must print the same hash. If a tag is pushed
   before its commits are on `main`, `git status` will report the local branch
   as *ahead of* `origin/main`; run `git push` to publish the branch. (*Ahead*
   means you have commits GitHub lacks; *behind* means GitHub has commits you
   have not pulled.)

   To push the branch and tag together, use `git push origin main vX.Y.Z`.
   `git push --follow-tags` is not a substitute here, because it skips
   lightweight tags such as the ones created above; it only pushes annotated
   tags (`git tag -a`).

The tag must match the version in `Cargo.toml`. Do not reuse or move an existing
release tag. Add a suffix such as `-rc.1` to both the package version and tag if
a prerelease is needed; GitHub will then publish it as a prerelease rather than
the latest stable version.

## If something fails after tagging

Do not move or re-push a tag once its release is public; anyone who has already
downloaded or cached the release would be looking at a tag that changed. Decide
by what failed:

- **Lint or test-only failure** (for example a clippy warning in
  `src/tests.rs`): the published binaries are unaffected, because lints do not
  change generated code. Commit the fix to `main` and carry on. The tag stays on
  a commit with red CI, which is cosmetic.
- **A defect in shipped code or in the release artifacts**: cut a new patch
  release (`X.Y.Z+1`) rather than editing the old one. Bump the version in
  `Cargo.toml` and `CHANGELOG.md`, and follow the steps above again.
- **The release workflow itself failed before publishing anything**: fix the
  cause, then delete the incomplete release and tag on GitHub before retrying.
  Only reuse the version number if nothing was ever published.

To stop a red CI run from slipping through again, make the release job depend on
the lint and test jobs (`needs: [...]`), or have the release workflow wait for
CI. Note that `dist-workspace.toml` uses `allow-dirty = ["ci"]`, so this change
has to be made by hand in the checked-in workflow and re-applied when
regenerating it.

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

After publishing, confirm the update endpoint reports the new stable release
and all platform archives have both a `digest` value and an adjacent `.sha256`
asset:

```bash
curl --fail --silent --show-error --location \
  https://api.github.com/repos/chevybeef/cleandl/releases/latest
```
