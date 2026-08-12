# cleandl

**CleanDL Edge Sync Agent** — a small Rust client that pulls sorting rules from a central control plane and moves downloaded files into organised folders.

It is designed to run on the machines where files land (laptops, NAS boxes, download servers). Configuration lives in the CleanDL API; the edge agent stays dumb and reliable.

## Features

- Checks `GET /api/v1/configuration-version` and fetches settings and rules only when the local cache is missing or stale
- Regex-based routing with ordered rules and optional year/month folders
- Global ignore and delete rules
- Parallel file moves (rayon) with configurable thread count
- Preserves modification and access timestamps
- Destination collision handling: same-size → skip, different-size → numeric rename (`file-1.ext`)
- Dry-run mode, debug logging, and quiet mode (for cron)
- Offline `--demo` mode with built-in example rules and no account required
- Normal mode can continue from its last synchronized configuration while the control plane is offline
- API key + API URL via CLI, environment variables, or files next to the logs
- Structured logging to both terminal (optional) and a rotating-friendly log file

## Requirements

- Rust toolchain (edition 2024 / recent stable)
- A running CleanDL control plane and a valid API key for normal mode

## Installation

### Release archives

Beginning with version 0.2.0, prebuilt archives for macOS, Linux, and Windows
will be published on the [GitHub Releases](https://github.com/chevybeef/cleandl/releases)
page. Every archive has a SHA-256 checksum. A CycloneDX software bill of
materials is published alongside the archives, and the release artifacts have
verifiable GitHub build provenance. These initial binaries are unsigned.

Extract the archive for your operating system, place `cleandl` (or
`cleandl.exe`) somewhere on your `PATH`, and run `cleandl --version`.

### From source (recommended while developing)

```bash
git clone https://github.com/chevybeef/cleandl.git
cd cleandl
cargo build --release
```

### Makefile (Linux / macOS)

```bash
make install
# installs to ~/.local/bin/cleandl
```

### PowerShell (Windows)

```powershell
.\install.ps1
# installs to %USERPROFILE%\.local\bin\cleandl.exe
```

### cargo install

```bash
cargo install --path .
# produces ~/.cargo/bin/cleandl
```

Add `~/.local/bin` or `~/.cargo/bin` to your `PATH` if needed.

## Try it without an account

Demo mode scans your Downloads folder using built-in example rules and shows
what CleanDL would do. It makes no network requests, needs no URL or API key,
and always forces a trial run so it cannot move or delete files:

```bash
cleandl --demo
```

Normal overrides still work, which makes it easy to try another folder and see
how destinations would be constructed:

```bash
cleandl --demo --source=/path/to/downloads --prefix=/example/destination --debug
```

The demo recognizes common documents, spreadsheets, images, audio, video,
archives, and installer formats. Example filename rules route invoices and
receipts to `Documents/Finance`, scans to `Documents/Scanned`, and screenshots
to `Pictures/Screenshots`. Supplying `--dry-run=false` cannot disable demo
safety. Suggested destinations use a realistic TrueNAS mount by default:
`/Volumes/truenas` on macOS, `/mnt/truenas` on Linux, and
`\\truenas\downloads` on Windows. Use `--prefix` to replace it.

For example, a macOS demo containing an invoice, screenshot, and ZIP archive
would include output like:

```text
Would move: invoice-123.pdf -> /Volumes/truenas/Documents/Finance/2026/08/invoice-123.pdf
Would move: Screenshot-example.png -> /Volumes/truenas/Pictures/Screenshots/2026/08/Screenshot-example.png
Would move: package.zip -> /Volumes/truenas/Archives/package.zip
```

## First-run configuration for normal mode

On first launch the agent creates two files under its log directory and exits if they are missing or empty:

| File            | Purpose                          | Example                                      |
|-----------------|----------------------------------|----------------------------------------------|
| `cleandl.key`   | API key                          | `cldl_live_…`                                |
| `cleandl.url`   | Control-plane API base URL      | `https://cleandl.chevybeef.com/api/v1`        |
| `cleandl.cache.json` | Automatically managed configuration cache | Created after the first successful sync |

**Log directory locations**

| OS      | Path                                              |
|---------|---------------------------------------------------|
| Linux   | `$XDG_STATE_HOME/cleandl` or `~/.local/state/cleandl` |
| macOS   | `~/.local/state/cleandl`                             |
| Windows | `%LOCALAPPDATA%\cleandl\Logs`                       |

For temporary automation, you can also supply the values through environment
variables:

```bash
export CLEANDL_API_KEY="…"
export CLEANDL_API_URL="https://cleandl.chevybeef.com/api/v1"
```

Prefer the `cleandl.key` file for routine use. Avoid putting an API key directly
in `--api-key`, because command-line arguments can be recorded in shell history
or exposed in process listings.

The cache is associated with both the API URL and a SHA-256 fingerprint of the
API key. When the control plane cannot be reached, a trial run proceeds normally
from the cache. A non-trial run pauses for explicit confirmation before cached
rules are allowed to move or delete files. If no matching cache exists, the run
fails without changing files.

## Usage

```text
cleandl [OPTIONS]
```

| Flag            | Description                                      |
|-----------------|--------------------------------------------------|
| `--demo`        | Use built-in rules offline; always a trial run   |
| `--source`      | Override source folder                           |
| `--prefix`      | Override destination prefix                      |
| `--dry-run`     | Simulate moves / deletes (no filesystem changes) |
| `--debug`       | Enable debug logging                             |
| `--quiet`       | Suppress terminal output (log file still written)|
| `--threads N`   | Number of parallel move threads (default from API or 6) |
| `--api-url`     | Control-plane config URL                         |
| `--api-key`     | API key                                          |

### Typical cron entry

```bash
0 3 * * * CLEANDL_API_KEY=… /home/user/.local/bin/cleandl --quiet
```

### One-off dry run with debug

```bash
cleandl --dry-run --debug
```

## How it works

1. In demo mode, load built-in rules and skip all account and network setup. Otherwise, resolve the API key and URL (CLI → env → files under the log directory).
2. In normal mode, compare the server's configuration timestamp with `cleandl.cache.json`. Fetch settings and rules only for a missing or stale cache; resolve newly encountered local extensions incrementally.
3. Apply any CLI overrides (`source`, `prefix`, `dry-run`, `debug`, `threads`).
4. Scan the source folder.
5. For each file:
   - Match global delete rules → remove (or log in dry-run).
   - Match global ignore rules → skip.
   - Look up extension → apply ordered regex rules or the extension default destination.
   - Optionally append `YYYY/MM` from the file’s mtime.
   - Handle collisions (size check + numeric suffix).
6. Move files in parallel, preserving timestamps, and log throughput in MiB/s.

The control plane remains the source of truth. The edge agent keeps only the
last successfully synchronized configuration as a resilience cache.

## Development

```bash
cargo test
cargo clippy
cargo build --release
```

Useful environment variables while developing:

```bash
export CLEANDL_API_KEY=…
export CLEANDL_API_URL=http://127.0.0.1:8080/api/v1
cargo run -- --dry-run --debug
```

## Related

- CleanDL control plane API (OpenAPI) — the service this agent talks to
- Optional future MCP server — manage rules and settings from an AI agent without opening the web UI

## License

MIT
