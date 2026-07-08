use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use clap::Parser;
use filetime::FileTime;
use log::{LevelFilter, debug, info, warn};
use rayon::ThreadPoolBuilder;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use regex::Regex;
use serde::Deserialize;
use simplelog::{ColorChoice, CombinedLogger, Config, TermLogger, TerminalMode, WriteLogger};
use std::fs::{self, File, Metadata};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// API response types — mirror the JSON shape served by cleandl-api
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiRule {
    regex: String,
    destination: Vec<String>,
    order: i64,
}

#[derive(Debug, Deserialize)]
struct ApiExtension {
    name: String,
    default_destination: Vec<String>,
    append_year_month: bool,
    // #[serde(default, deserialize_with = "null_to_default")]
    rules: Vec<ApiRule>,
}

#[derive(Debug, Deserialize)]
struct ApiConfig {
    settings: std::collections::HashMap<String, String>,
    #[serde(default)]
    ignore_rules: Vec<String>,
    #[serde(default)]
    delete_rules: Vec<String>,
    extensions: Vec<ApiExtension>,
}

// ---------------------------------------------------------------------------
// Compiled (ready-to-use) types
// ---------------------------------------------------------------------------

struct CompiledRule {
    regex: Regex,
    destination: Vec<String>,
    // order field consumed during sort; not stored here
}

struct CompiledExtension {
    default_destination: Vec<String>,
    append_year_month: bool,
    rules: Vec<CompiledRule>,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// cleandl-rs: move downloaded files to sorted folders, config from API
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Source folder override
    #[arg(short = 's', long)]
    source: Option<String>,

    /// OS prefix override
    #[arg(short = 'p', long)]
    prefix: Option<String>,

    /// Trial run (dry-run) override
    #[arg(short = 't', long)]
    trial: Option<bool>,

    /// Debug logging override
    #[arg(short = 'd', long)]
    debug: Option<bool>,

    /// Number of threads override
    #[arg(short = 'r', long)]
    threads: Option<usize>,

    /// API base URL
    #[arg(long, default_value = "https://cleandl.example.com/api/config")]
    api_url: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    let args = Cli::parse();

    // Log file lives alongside the cleandl-og log so both implementations
    // write into the same synced ~/Sync/bin directory.
    let log_dir = dirs_next::home_dir()
        .expect("unable to get home directory")
        .join("Sync/bin");

    let _ = CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Debug,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            LevelFilter::Debug,
            Config::default(),
            File::create(log_dir.join("cleandl-rs.log")).context("Failed to open log file")?,
        ),
    ]);

    info!("Fetching configuration from {}", args.api_url);
    let mut cfg = fetch_config(&args.api_url)?;

    // Apply CLI overrides into the settings map (mirrors cleandl-go behaviour)
    if let Some(s) = args.source {
        cfg.settings.insert("source_folder".into(), s);
    }
    if let Some(p) = args.prefix {
        cfg.settings.insert("prefix_override".into(), p);
    }
    if let Some(t) = args.trial {
        cfg.settings.insert("trial_run".into(), t.to_string());
    }
    if let Some(d) = args.debug {
        cfg.settings.insert("debug".into(), d.to_string());
    }
    if let Some(r) = args.threads {
        cfg.settings.insert("threads".into(), r.to_string());
    }

    info!("Config loaded successfully. Starting run...");
    process_directory(cfg)?;
    info!("Run complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Config fetch
// ---------------------------------------------------------------------------

fn fetch_config(url: &str) -> Result<ApiConfig> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(url)
        .send()
        .context("Failed to fetch config from API")?;

    if !resp.status().is_success() {
        anyhow::bail!("API returned status: {}", resp.status());
    }

    resp.json::<ApiConfig>()
        .context("Failed to decode API JSON")
}

// ---------------------------------------------------------------------------
// Main processing engine
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn process_directory(cfg: ApiConfig) -> Result<()> {
    // ---- Parse settings ----
    let debug = cfg
        .settings
        .get("debug")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));

    let trial_run = cfg
        .settings
        .get("trial_run")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));

    let threads: usize = cfg
        .settings
        .get("threads")
        .and_then(|v| v.parse().ok())
        .filter(|&t| t > 0)
        .unwrap_or(6);

    let sort_by_size = cfg
        .settings
        .get("sort_by_size")
        .map_or("", String::as_str)
        .to_owned();

    let source_name = cfg
        .settings
        .get("source_folder")
        .cloned()
        .filter(|s| !s.is_empty())
        .context("'source_folder' setting is missing from API configuration")?;

    // Resolve source path — relative paths are joined to home directory
    let source_path = if Path::new(&source_name).is_absolute() {
        PathBuf::from(&source_name)
    } else {
        dirs_next::home_dir()
            .context("Failed to get home directory")?
            .join(&source_name)
    };

    // Resolve destination prefix (OS-specific, overridable via CLI)
    let prefix = if let Some(po) = cfg
        .settings
        .get("prefix_override")
        .filter(|v| !v.is_empty())
    {
        po.clone()
    } else if cfg!(target_os = "windows") {
        cfg.settings
            .get("windows_prefix")
            .cloned()
            .unwrap_or_default()
    } else if cfg!(target_os = "macos") {
        cfg.settings
            .get("macos_prefix")
            .cloned()
            .unwrap_or_default()
    } else {
        // linux / freebsd / openbsd / netbsd
        cfg.settings.get("unix_prefix").cloned().unwrap_or_default()
    };

    let dest_base: PathBuf = if prefix.is_empty() {
        dirs_next::home_dir().context("Failed to get home directory")?
    } else {
        PathBuf::from(&prefix)
    };

    let now = chrono::Local::now();
    info!(
        "cleandl-rs v{VERSION} at {} on {}",
        now.format("%Y-%m-%d %H:%M:%S"),
        std::env::consts::OS,
    );

    if debug {
        info!("Settings are as follows:");
        info!("source_folder: {source_name}");
        info!("destination_prefix: {}", dest_base.display());
        info!("trial_run: {trial_run}");
        info!("debug: {debug}");
        info!("sort_by_size: {sort_by_size}");
        info!("threads: {threads}");
    }

    // ---- Pre-compile global ignore rules ----
    let compiled_ignore: Vec<Regex> = cfg
        .ignore_rules
        .iter()
        .filter_map(|pat| match Regex::new(pat) {
            Ok(re) => Some(re),
            Err(e) => {
                warn!("Error compiling global ignore regex '{pat}': {e}");
                None
            }
        })
        .collect();

    // ---- Pre-compile global delete rules ----
    let compiled_delete: Vec<Regex> = cfg
        .delete_rules
        .iter()
        .filter_map(|pat| match Regex::new(pat) {
            Ok(re) => Some(re),
            Err(e) => {
                warn!("Error compiling global delete regex '{pat}': {e}");
                None
            }
        })
        .collect();

    // ---- Pre-compile extension rules ----
    // Build a HashMap keyed by lowercase extension name
    let mut compiled_exts: std::collections::HashMap<String, CompiledExtension> =
        std::collections::HashMap::new();

    for ext in cfg.extensions {
        let mut api_rules = ext.rules;
        // Sort rules by their order field ascending (mirrors cleandl-go)
        api_rules.sort_by_key(|r| r.order);

        let mut comp_rules = Vec::with_capacity(api_rules.len());
        for rule in api_rules {
            match Regex::new(&rule.regex) {
                Ok(re) => comp_rules.push(CompiledRule {
                    regex: re,
                    destination: rule.destination,
                }),
                Err(e) => {
                    warn!(
                        "Error compiling regex '{}' for extension '{}': {e}",
                        rule.regex, ext.name
                    );
                }
            }
        }

        compiled_exts.insert(
            ext.name.to_lowercase(),
            CompiledExtension {
                default_destination: ext.default_destination,
                append_year_month: ext.append_year_month,
                rules: comp_rules,
            },
        );
    }

    // ---- Walk source directory ----
    info!("Scanning directory: {}", source_path.display());

    let entries = fs::read_dir(&source_path)
        .with_context(|| format!("Failed to read source directory: {}", source_path.display()))?;

    // (source, destination, metadata) triples for parallel move
    let mut fromtos: Vec<(PathBuf, PathBuf, Metadata)> = Vec::new();

    for entry in entries {
        let Ok(entry) = entry else { continue };

        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        let from = entry.path();
        let entry_file_name = entry.file_name();
        let filename = entry_file_name.to_string_lossy();

        // 1. Check delete rules
        if compiled_delete.iter().any(|re| re.is_match(&filename)) {
            if trial_run {
                info!(
                    "Would delete file matching global delete rule: {}",
                    from.display()
                );
            } else {
                fs::remove_file(&from).context("Failed to delete file")?;
                info!("Deleted file matching global rule: {filename}");
            }
            continue;
        }

        // 2. Check ignore rules
        if compiled_ignore.iter().any(|re| re.is_match(&filename)) {
            if debug {
                debug!("Skipping file matching global ignore rule: {filename}");
            }
            continue;
        }

        // 3. Extension lookup
        let ext_str = if let Some(e) = from.extension() {
            e.to_string_lossy().to_lowercase()
        } else {
            warn!("No extension found for: {filename}.");
            continue;
        };

        let Some(mapping) = compiled_exts.get(&ext_str) else {
            warn!("Extension not handled for: {filename}");
            continue;
        };

        // 4. Find destination via rules, fall back to default
        let matched = mapping.rules.iter().find(|r| r.regex.is_match(&filename));
        let (destination, append_year_month) = matched.map_or_else(
            || {
                if debug {
                    debug!(
                        "No regex match for: {filename}, using default: {}",
                        mapping
                            .default_destination
                            .iter()
                            .collect::<PathBuf>()
                            .display()
                    );
                }
                (&mapping.default_destination, mapping.append_year_month)
            },
            |rule| {
                if debug {
                    debug!(
                        "Regex matched for: {filename} destination is now: {}",
                        rule.destination.iter().collect::<PathBuf>().display()
                    );
                }
                (&rule.destination, mapping.append_year_month)
            },
        );

        if destination.is_empty() {
            continue;
        }

        // 5. Resolve full destination directory
        let mut dest_dir: PathBuf = dest_base.clone();
        dest_dir.push(destination.iter().collect::<PathBuf>());

        if append_year_month {
            let metadata = from
                .metadata()
                .with_context(|| format!("Failed to get metadata for: {}", from.display()))?;
            let mtime = FileTime::from_last_modification_time(&metadata);
            let dt: DateTime<Utc> =
                DateTime::from_timestamp(mtime.unix_seconds(), mtime.nanoseconds())
                    .context("Invalid timestamp from file metadata")?;
            dest_dir.push(dt.year().to_string());
            dest_dir.push(format!("{:02}", dt.month()));
        }

        // 6. Ensure destination directory exists
        if !dest_dir.exists() {
            if trial_run {
                info!("Would create folder: {}", dest_dir.display());
            } else {
                if debug {
                    debug!("Creating destination folder: {}", dest_dir.display());
                }
                fs::create_dir_all(&dest_dir).with_context(|| {
                    format!("Failed to create directory: {}", dest_dir.display())
                })?;
            }
        }

        let dest_path = dest_dir.join(&*filename);

        // 7. Skip if already at destination
        if dest_path == from {
            if debug {
                debug!(
                    "File {} is already at its destination: {}",
                    filename,
                    dest_path.display()
                );
            }
            continue;
        }

        // 8. Skip if destination already exists
        if dest_path.exists() {
            warn!("{} already exists, skipping!", dest_path.display());
            continue;
        }

        let metadata = from
            .metadata()
            .with_context(|| format!("Failed to get metadata for: {}", from.display()))?;

        fromtos.push((from, dest_path, metadata));
    }

    // ---- Sort jobs by size if requested ----
    match sort_by_size.as_str() {
        "Ascending" => fromtos.sort_by_key(|k| k.2.len()),
        "Descending" => {
            fromtos.sort_by_key(|k| k.2.len());
            fromtos.reverse();
        }
        _ => (),
    }

    if debug {
        debug!("{fromtos:#?}");
    }

    if trial_run {
        for (src, dst, _) in &fromtos {
            info!("Would move: {} -> {}", src.display(), dst.display());
        }
    } else {
        // Build thread pool from config, then move files in parallel
        ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .context("Failed to build rayon thread pool")?;

        fromtos.par_iter().for_each(|(src, dst, metadata)| {
            if let Err(e) = move_file(src, dst, metadata) {
                warn!("Error moving {}: {e}", src.display());
            }
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// File move (copy + preserve timestamps + delete source)
// ---------------------------------------------------------------------------

fn move_file(src: &Path, dst: &Path, metadata: &Metadata) -> Result<()> {
    let mtime = FileTime::from_last_modification_time(metadata);
    let atime = FileTime::from_last_access_time(metadata);

    let start = Instant::now();

    fs::copy(src, dst)
        .with_context(|| format!("Failed to copy {} to {}", src.display(), dst.display()))?;

    if let Err(e) = filetime::set_file_times(dst, atime, mtime) {
        warn!("Setting file times failed for {}: {e}", dst.display());
    }

    fs::remove_file(src)
        .with_context(|| format!("Failed to remove source file {}", src.display()))?;

    let duration = start.elapsed();
    info!(
        "→ {:.2} MB/s \t{}",
        calculate_mbps(metadata.len(), duration),
        dst.display()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)]
fn calculate_mbps(file_size_bytes: u64, duration: Duration) -> f64 {
    file_size_bytes as f64 / duration.as_secs_f64() / 1_000_000.0
}

// leave this here in case needed in future, see ApiExtension for use (also commented out)
// fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
// where
//     D: serde::Deserializer<'de>,
//     T: Default + serde::Deserialize<'de>,
// {
//     let opt = Option::deserialize(deserializer)?;
//     Ok(opt.unwrap_or_default())
// }
