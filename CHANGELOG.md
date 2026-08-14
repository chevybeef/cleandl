# Changelog

All notable changes to cleandl will be documented in this file.

## Unreleased

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
