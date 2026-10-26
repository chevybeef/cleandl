# Changelog

All notable changes to cleandl will be documented in this file.

## Unreleased

## 0.7.0

- improve progress remove pause due 2 file buffering
- validate destinations, use osstring, fix os error 45
- fixed per-file races abort the whole run
- fix 401s are swallowed in three of the fetch paths
- fixed Year/month bucketing uses UTC
- five copy-pasted X-API-Key blocks are gone, key attached once when client built
- refactored the long process_directory function
- settings simplified, parsed once, apiconfig is typed
- fix extension names lower-cased some places not others
- a script to aid in testing by moving files back
- same size files get number append, moved instead of left in the source folder
- append, not truncate log, show path/size after run
- implement log compession and rotation
- implement --filter=ext,ext to process only those
- added exceptions for clippy tests
- rename fast path when src & dst on same filesystem
- fix potential log lines tearing the progress bar.

## 0.6.0

- Show per-file copy progress with the filename, bytes transferred, transfer
  speed, and estimated time remaining. Progress bars are hidden in quiet mode
  and when standard error is not a terminal.
- Track bytes successfully written while retaining buffered copies and retries
  for interrupted reads. Clear the progress bar when a copy succeeds or fails.

## 0.5.0

- Replace parallel file moves with sequential buffered copies, improving macOS
  SMB performance in manual testing. Remove Rayon; legacy `--threads` options
  and server settings remain accepted but are ignored.
- Resolve destination collisions immediately before each move and exclusively
  create destination files to prevent overwrites.
- Retain sources on copy failures, attempt to remove partial destinations, and
  return a nonzero exit status after processing the remaining files when moves
  fail. A forced termination can still leave a partial destination.
- Require `--option=value` syntax for source, prefix, threads, API URL, and API
  key options. **Breaking change:** space-separated values are no longer
  accepted; update scripts to use forms such as `--source=/path/to/downloads`.
  Environment variables and configuration files are unaffected.

## 0.4.0

- Add `check-update` and approval-based `update` commands backed by stable
  GitHub Releases.
- Offer updates at most once per day during interactive runs while leaving
  quiet, scheduled, and non-interactive runs unchanged.
- Verify release archives against both cargo-dist checksum files and GitHub's
  asset digest before replacing the executable.

## 0.3.1

- Streamline first-run setup by saving the hosted API URL automatically,
  linking to the sign-in page, and prompting securely for a connection key.
- Finish setup immediately after saving a new key, leaving file processing for
  the next run.

## 0.3.0

- Save the last successful settings and sorting rules so CleanDL can continue
  when the server is temporarily unavailable.
- Check the server's configuration version before downloading settings, which
  avoids unnecessary requests when nothing has changed.
- Ask for confirmation before an offline run that can move or delete files;
  offline trial runs continue without interruption.
- Keep saved settings separate for each server and API key.
- Download rules for newly encountered file types without refreshing all
  settings.
- Rewrite the README with clearer installation, setup, demo, and offline-use
  instructions.

## 0.2.0

- Add a safe, offline `--demo` mode with built-in example sorting rules.
- Remove the unused `cleandl-rs` state-directory migration.
- Run release checksum normalization under Bash on Windows.
- Add request diagnostics around every control-plane API fetch.
- Publish clean build identifiers and strictly formatted checksum files.
- Prepare the first public, cross-platform client release.
