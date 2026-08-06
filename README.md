# cleandl

**CleanDL Edge Sync Agent** — a small Rust client that pulls sorting rules from a central control plane and moves downloaded files into organised folders.

It is designed to run on the machines where files land (laptops, NAS boxes, download servers). Configuration lives in the CleanDL API; the edge agent stays dumb and reliable.

## Features

- Fetches settings from `GET /api/v1/settings`, global rules from `GET /api/v1/global-rules`, and resolves only locally present extensions through `POST /api/v1/extensions/resolve`
- Regex-based routing with ordered rules and optional year/month folders
- Global ignore and delete rules
- Parallel file moves (rayon) with configurable thread count
- Preserves modification and access timestamps
- Destination collision handling: same-size → skip, different-size → numeric rename (`file-1.ext`)
- Dry-run mode, debug logging, and quiet mode (for cron)
- API key + API URL via CLI, environment variables, or files next to the logs
- Structured logging to both terminal (optional) and a rotating-friendly log file

## Requirements

- Rust toolchain (edition 2024 / recent stable)
- A running CleanDL control plane and a valid API key

## Installation

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

## First-run configuration

On first launch the agent creates two files under its log directory and exits if they are missing or empty:

| File            | Purpose                          | Example                                      |
|-----------------|----------------------------------|----------------------------------------------|
| `cleandl.key`   | API key                          | `cldl_live_…`                                |
| `cleandl.url`   | Control-plane API base URL      | `https://cleandl.example.com/api/v1`        |

**Log directory locations**

| OS      | Path                                              |
|---------|---------------------------------------------------|
| Linux   | `$XDG_STATE_HOME/cleandl` or `~/.local/state/cleandl` |
| macOS   | `~/.local/state/cleandl`                             |
| Windows | `%LOCALAPPDATA%\cleandl\Logs`                       |

On first launch after upgrading from `cleandl-rs`, the existing state directory
is moved to the new `cleandl` location automatically.

For temporary automation, you can also supply the values through environment
variables:

```bash
export CLEANDL_API_KEY="…"
export CLEANDL_API_URL="https://cleandl.chevybeef.com/api/v1"
```

Prefer the `cleandl.key` file for routine use. Avoid putting an API key directly
in `--api-key`, because command-line arguments can be recorded in shell history
or exposed in process listings.

## Usage

```text
cleandl [OPTIONS]
```

| Flag            | Description                                      |
|-----------------|--------------------------------------------------|
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

1. Resolve API key and URL (CLI → env → files under the log directory).
2. `GET /api/v1/settings`, `GET /api/v1/global-rules`, `POST /api/v1/extensions/resolve`, and optionally `GET /api/v1/account/me` for debug logging.
3. Apply any CLI overrides (`source`, `prefix`, `dry-run`, `debug`, `threads`).
4. Scan the source folder.
5. For each file:
   - Match global delete rules → remove (or log in dry-run).
   - Match global ignore rules → skip.
   - Look up extension → apply ordered regex rules or the extension default destination.
   - Optionally append `YYYY/MM` from the file’s mtime.
   - Handle collisions (size check + numeric suffix).
6. Move files in parallel, preserving timestamps, and log throughput in MiB/s.

The control plane remains the single source of truth for rules; the edge agent never stores a local rule database.

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
