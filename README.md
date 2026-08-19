# cleandl

CleanDL sorts downloaded files into organised folders. It gets your settings
from the [CleanDL Server](https://cleandl.chevybeef.com), then runs on the 
computer or NAS where your downloads are stored.

## Features

- Sorts files by extension and filename rules
- Can place files into year and month folders
- Supports rules that ignore or delete matching files
- Preserves file access and modification times
- Avoids overwriting existing files
- Includes a safe trial-run mode that shows what would happen without changing files
- Can continue using its saved settings when the CleanDL server is offline
- Includes a demo mode that works without an account or server
- Can run quietly as a scheduled task

## Requirements

To use your own settings, you need access to a CleanDL server and a valid API
key. You only need Rust if you want to build CleanDL yourself.

## Installation

### Release archives

Download the archive for your operating system from the
[GitHub Releases](https://github.com/chevybeef/cleandl/releases) page and
extract it.

You can run CleanDL directly from the folder where you extracted it:

```bash
# Linux or macOS
./cleandl --version

# Windows PowerShell
.\cleandl.exe --version
```

You can also move `cleandl` (or `cleandl.exe`) to a folder on your `PATH` if
you want to run it from anywhere. The downloadable programs are currently
unsigned, so your operating system may ask you to confirm that you want to run
them. Checksums are provided on the release page so you can verify your
download.

### Build from source

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

## Try CleanDL without an account

Demo mode scans your Downloads folder using example rules and shows what
CleanDL would do. It does not connect to a server, does not need an API key,
and cannot move or delete files:

```bash
cleandl --demo
```

You can choose a different source folder and destination:

```bash
cleandl --demo --source=/path/to/downloads --prefix=/example/destination --debug
```

The demo recognises common documents, spreadsheets, images, audio, video,
archives, and installer files. It places invoices and receipts in
`Documents/Finance`, scans in `Documents/Scanned`, and screenshots in
`Pictures/Screenshots`. Demo mode always remains a trial run, even if you use
`--dry-run=false`. Its example destination starts at `/Volumes/truenas` on
macOS, `/mnt/truenas` on Linux, and
`\\truenas\downloads` on Windows. Use `--prefix` to replace it.

For example, a macOS demo containing an invoice, screenshot, and ZIP archive
would include output like:

```text
Would move: invoice-123.pdf -> /Volumes/truenas/Documents/Finance/2026/08/invoice-123.pdf
Would move: Screenshot-example.png -> /Volumes/truenas/Pictures/Screenshots/2026/08/Screenshot-example.png
Would move: package.zip -> /Volumes/truenas/Archives/package.zip
```

## Connect to your CleanDL server

The first time you run CleanDL normally, it saves the hosted CleanDL API address
to `cleandl.url`. If no API key is configured, it shows a link to the CleanDL
sign-in page and explains how to generate a connection key. Paste that key into
the prompt and CleanDL saves it to `cleandl.key`, reports that setup is complete,
and exits. Run CleanDL again to sort your downloads. Press Ctrl+C at the prompt
to cancel without saving a key.

| File            | Purpose                          | Example                                      |
|-----------------|----------------------------------|----------------------------------------------|
| `cleandl.key`   | API key                          | `cldl_live_…`                                |
| `cleandl.url`   | CleanDL server URL               | `https://cleandl.chevybeef.com/api/v1`        |
| `cleandl.cache.json` | Saved copy of your settings | Created automatically after a successful connection |

CleanDL stores these files and its log in the following folder:

| OS      | Path                                              |
|---------|---------------------------------------------------|
| Linux   | `$XDG_STATE_HOME/cleandl` or `~/.local/state/cleandl` |
| macOS   | `~/.local/state/cleandl`                             |
| Windows | `%LOCALAPPDATA%\cleandl\Logs`                       |

You can also provide the API key and URL through environment variables. This is
useful for scripts and scheduled runs:

```bash
export CLEANDL_API_KEY="…"
export CLEANDL_API_URL="https://cleandl.chevybeef.com/api/v1"
```

For everyday use, putting the key in `cleandl.key` is safer than passing it with
`--api-key`, because commands may be saved in your shell history or visible to
other programs.

After a successful connection, CleanDL saves a local copy of your settings. If
the server is unavailable later:

- A trial run continues normally using the saved settings.
- A run that can move or delete files asks you to confirm before continuing.
- If no saved settings are available, CleanDL stops without changing anything.

CleanDL checks whether your server settings have changed before downloading
them again. This makes normal runs faster and reduces work on the server.

## Usage

```text
cleandl [OPTIONS] [COMMAND]
```

| Flag            | Description                                      |
|-----------------|--------------------------------------------------|
| `--demo`        | Use built-in rules offline; always a trial run   |
| `--source`      | Use a different source folder                    |
| `--prefix`      | Use a different base destination folder          |
| `--dry-run`     | Simulate moves / deletes (no filesystem changes) |
| `--debug`       | Enable debug logging                             |
| `--quiet`       | Suppress terminal output (log file still written)|
| `--threads N`   | Number of parallel move threads (default from API or 6) |
| `--api-url`     | CleanDL server URL                               |
| `--api-key`     | API key                                          |

| Command        | Description                                      |
|----------------|--------------------------------------------------|
| `check-update` | Check for a newer stable release without installing it |
| `update`       | Check for, confirm, and install a stable release |

## Updates

CleanDL checks GitHub for a newer stable release at most once every 24 hours
during ordinary interactive runs. If one is available, it shows the version,
release page, and platform download, then asks before installing anything. Only
`y` or `yes` approves the update.

Quiet, scheduled, and non-interactive runs never check or prompt automatically.
You can check explicitly at any time without changing the installed program:

```bash
cleandl check-update
```

To check and optionally install the latest stable release:

```bash
cleandl update
```

Before replacing the current executable, CleanDL verifies the downloaded
archive against both its adjacent SHA-256 file and the digest reported by
GitHub. The update is cancelled if either value is missing or they disagree.
After a successful update, run CleanDL again. If the executable's installation
folder is not writable, download the archive manually from the
[GitHub Releases](https://github.com/chevybeef/cleandl/releases) page instead.

### Typical cron entry

```bash
0 3 * * * CLEANDL_API_KEY=… /home/user/.local/bin/cleandl --quiet
```

### One-off dry run with debug

```bash
cleandl --dry-run --debug
```

## What happens during a run

1. CleanDL loads your server address and API key. Demo mode uses its built-in
   example settings instead.
2. It checks whether your saved settings are still current and downloads a new
   copy only when needed.
3. It applies any command-line options you supplied.
4. It scans your source folder.
5. For each file:
   - Delete it if it matches a delete rule.
   - Leave it alone if it matches an ignore rule.
   - Choose a destination from its extension and filename.
   - Add year and month folders when configured.
   - Avoid overwriting a file that already exists.
6. It moves the files while preserving their timestamps. In trial-run mode, it
   only reports what it would do.

The server remains the main copy of your settings. CleanDL only keeps the last
successful copy so it can continue when the server is temporarily unavailable.

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

## License

MIT
