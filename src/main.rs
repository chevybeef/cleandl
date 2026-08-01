use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use clap::Parser;
use filetime::FileTime;
use log::Record;
use log::{debug, error, info, trace, warn};
use rayon::ThreadPoolBuilder;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use simplelog::{
    ColorChoice, CombinedLogger, Config, ConfigBuilder, LevelFilter, SharedLogger, TermLogger,
    TerminalMode, WriteLogger,
};

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// API response types — mirror the JSON shape served by cleandl-api
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct ApiRule {
    regex: String,
    destination: Vec<String>,
    #[serde(default)]
    order: i64,
    #[serde(default)]
    append_year_month: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiUser {
    id: i64,
    username: String,
    role: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiExtension {
    name: String,
    #[serde(default)]
    default_destination: Vec<String>,
    #[serde(default)]
    append_year_month: bool,
    #[serde(default)]
    rules: Vec<ApiRule>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiGlobalRule {
    action: String,
    regex: String,
    #[serde(default)]
    is_active: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiConfig {
    settings: HashMap<String, String>,
    #[serde(default)]
    ignore_rules: Vec<String>,
    #[serde(default)]
    delete_rules: Vec<String>,
    #[serde(default)]
    extensions: Vec<ApiExtension>,
}

// source_folder = 'Downloads'
// macos_prefix = '/Volumes/storage'
// unix_prefix = '/media/storage'
// windows_prefix = '\\nas\storage'
// trial_run = true
// debug = true
// sort_by_size = 'No' # Sort file size, No, Ascending or Descending
// threads = 6
#[derive(Debug, Clone, Deserialize)]
struct AppSettings {
    source_folder: String,
    macos_prefix: String,
    unix_prefix: String,
    windows_prefix: String,
    trial_run: bool,
    debug: bool,
    threads: u64,
}

// ---------------------------------------------------------------------------
// Compiled (ready-to-use) types
// ---------------------------------------------------------------------------

struct CompiledRule {
    regex: Regex,
    destination: Vec<String>,
    append_year_month: bool,
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
#[derive(Parser, Debug)]
#[command(author, version, about = "CleanDL Edge Sync Agent")]
struct Cli {
    /// Source folder override
    #[arg(long)]
    source: Option<String>,

    /// OS prefix override
    #[arg(long)]
    prefix: Option<String>,

    /// Dry run override
    #[arg(long)]
    dry_run: Option<bool>,

    /// Debug logging override
    #[arg(long)]
    debug: Option<bool>,

    /// Number of threads override
    #[arg(long)]
    threads: Option<usize>,

    /// Enable trace logging
    #[arg(long, action = clap::ArgAction::SetTrue)]
    trace: bool,

    /// API base URL
    /// Falls back to `CLEANDL_API_URL` or the file `cleandl.url` next to the key.
    #[arg(long, env = "CLEANDL_API_URL")]
    api_url: Option<String>,

    /// API Key for central control plane authentication.
    /// Falls back to reading `cleandl.key` in the current directory when omitted.
    #[arg(long, env = "CLEANDL_API_KEY")]
    api_key: Option<String>,

    /// Suppress terminal output (log file is still written)
    #[arg(long, action = clap::ArgAction::SetTrue)]
    quiet: bool,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const GIT_SHA: &str = env!("VERGEN_GIT_SHA");
const GIT_DIRTY: &str = env!("VERGEN_GIT_DIRTY");

fn main() -> Result<()> {
    // 1. setup logging first
    let log_dir = if cfg!(target_os = "windows") {
        dirs_next::data_local_dir()
            .expect("unable to get local app data directory")
            .join("cleandl-rs")
            .join("Logs")
    } else {
        std::env::var("XDG_STATE_HOME").map_or_else(
            |_| {
                dirs_next::home_dir()
                    .expect("unable to get home directory")
                    .join(".local/state/cleandl-rs")
            },
            |base| std::path::PathBuf::from(base).join("cleandl-rs"),
        )
    };
    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    // 2. parse the cli arguments
    let args = Cli::parse();
    let cli_debug_flag = args.debug.unwrap_or_default() || args.trace;
    let debug_flag = setup_logging(cli_debug_flag, args.quiet, args.trace, &log_dir)?;
    debug!("Will log to {}", log_dir.display());

    // 3. display the program's version string
    let now = chrono::Local::now();
    let dirty = if GIT_DIRTY == "true" { "-dirty" } else { "" };
    info!(
        "{PKG_NAME} v{PKG_VERSION} ({GIT_SHA}{dirty}) at {} on {}",
        now.format("%Y-%m-%d %H:%M:%S"),
        std::env::consts::OS,
    );

    // 4. get the API key from environment variable or key file
    let api_key = if let Some(key) = args.api_key.filter(|k| !k.trim().is_empty()) {
        debug!("API key loaded from CLEANDL_API_KEY");
        key
    } else {
        let key_path = log_dir.join("cleandl.key");

        if key_path.exists() {
            let raw = fs::read_to_string(&key_path)
                .with_context(|| format!("Failed to read {}", key_path.display()))?;
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                error!(
                    "Error: {} exists but is empty. \
                     Provide an API key via --api-key, CLEANDL_API_KEY, or that file.",
                    key_path.display()
                );
                std::process::exit(1);
            }
            debug!("API key loaded from {}", key_path.display());
            trimmed
        } else {
            let _ = File::create(&key_path);
            error!(
                "Error: No API key supplied. \
                 Edit {} that I created for you.",
                key_path.display()
            );
            std::process::exit(1);
        }
    };

    // 5. get API URL from environment variable or url file
    let api_url = if let Some(url) = args.api_url.filter(|u| !u.trim().is_empty()) {
        debug!("API URL loaded from CLI / CLEANDL_API_URL");
        url
    } else {
        let url_path = log_dir.join("cleandl.url");

        if url_path.exists() {
            let raw = fs::read_to_string(&url_path)
                .with_context(|| format!("Failed to read {}", url_path.display()))?;
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                error!(
                    "Error: {} exists but is empty. \
                 Provide an API URL via --api-url, CLEANDL_API_URL, or that file.",
                    url_path.display()
                );
                std::process::exit(1);
            }
            debug!("API URL loaded from {}", url_path.display());
            trimmed
        } else {
            let _ = File::create(&url_path);
            error!(
                "Error: No API URL supplied. \
             Edit {} that I created for you \
             (example: https://cleandl.example.com/api/v1/config).",
                url_path.display()
            );
            std::process::exit(1);
        }
    };

    // 6. fetch the settings so that we have the source folder
    debug!("Fetching settings from {api_url}");
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;

    let app_settings_json_value = match fetch_settings(&client, &api_url, &api_key) {
        Ok(fetched_settings) => {
            debug!("Configuration successfully synchronized!");
            fetched_settings
        }
        Err(err) => {
            error!("Error syncing with control plane: {err}");
            std::process::exit(1);
        }
    };
    info!("Settings loaded successfully.");
    let mut map = HashMap::new();
    if let Some(array) = app_settings_json_value.as_array() {
        for item in array {
            if let (Some(k), Some(v)) = (item["key"].as_str(), item["value"].as_str()) {
                map.insert(k, v);
            }
        }
    }

    let mut app_settings = AppSettings {
        source_folder: map.get("source_folder").unwrap_or(&"").to_string(),
        macos_prefix: map.get("macos_prefix").unwrap_or(&"").to_string(),
        unix_prefix: map.get("unix_prefix").unwrap_or(&"").to_string(),
        windows_prefix: map.get("windows_prefix").unwrap_or(&"").to_string(),
        trial_run: map
            .get("trial_run")
            .and_then(|v| v.parse().ok())
            .unwrap_or(false),
        debug: map
            .get("debug")
            .and_then(|v| v.parse().ok())
            .unwrap_or(false),
        threads: map.get("threads").and_then(|v| v.parse().ok()).unwrap_or(6),
    };

    // 7. Set the flags, passed arguments take precedence
    if let Some(s) = args.source {
        app_settings.source_folder = s;
    }
    let mut dest_base = if cfg!(target_os = "windows") {
        app_settings.windows_prefix
    } else if cfg!(target_os = "macos") {
        app_settings.macos_prefix
    } else {
        app_settings.unix_prefix
    };
    if let Some(p) = args.prefix {
        dest_base = p;
    }
    if let Some(t) = args.dry_run {
        app_settings.trial_run = t;
    }
    if let Some(d) = args.debug {
        app_settings.debug = d;
    }
    if let Some(r) = args.threads {
        app_settings.threads = r as u64;
    }
    // Keep debug enabled if either the API says so or --trace was passed on CLI
    let api_debug_flag = app_settings.debug || args.trace;
    debug_flag.store(api_debug_flag, Ordering::Relaxed);
    // Now we can print the debug info of all settings (move from process_directory)
    debug!("Settings are as follows:");
    debug!("source_folder: {}", app_settings.source_folder);
    debug!("destination_prefix: {dest_base}");
    debug!("trial_run: {}", app_settings.trial_run);
    debug!("debug: {}", app_settings.debug);
    debug!("threads: {}", app_settings.threads);

    // 8. get the list of files actually in the source folder
    let resolve_home =
        || -> Result<PathBuf> { dirs_next::home_dir().context("Failed to get home directory") };
    let source_path = if Path::new(&app_settings.source_folder).is_absolute() {
        PathBuf::from(&app_settings.source_folder)
    } else {
        resolve_home()?.join(&app_settings.source_folder)
    };
    info!("Scanning directory: {}", source_path.display());
    // Collect once: ReadDir is a single-pass iterator, and we need to walk it
    // twice — once here to work out which extensions are present, and again
    // in process_directory to actually move the files.
    let entries: Vec<std::io::Result<fs::DirEntry>> = fs::read_dir(&source_path)
        .with_context(|| format!("Failed to read source directory: {}", source_path.display()))?
        .collect();

    let mut found_extensions: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in &entries {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        if let Some(ext) = entry.path().extension().and_then(|e| e.to_str()) {
            found_extensions.insert(ext.to_lowercase());
        }
    }
    info!("Extensions found in source folder: {found_extensions:?}");

    // 9. Fetch and display user information from GET api/v1/account/me
    if app_settings.debug {
        let user = fetch_user_me(&client, &api_url, &api_key).map_err(|e| {
            error!("Could not fetch account info: {e}");
            anyhow::anyhow!("{e}")
        })?;
        debug!(
            "Configuration fetched for id: {}, username: {}, role: {}",
            user.id, user.username, user.role
        );
    }

    // 10. Fetch global ignore/delete rules (these apply to every filename
    // regardless of extension, so there's nothing to prune here).
    let global_rules = fetch_global_rules(&client, &api_url, &api_key).map_err(|e| {
        error!("Could not fetch global rules: {e}");
        anyhow::anyhow!("{e}")
    })?;
    let mut ignore_rules = Vec::new();
    let mut delete_rules = Vec::new();
    for rule in global_rules {
        if !rule.is_active {
            continue;
        }
        match rule.action.as_str() {
            "IGNORE" => ignore_rules.push(rule.regex),
            "DELETE" => delete_rules.push(rule.regex),
            other => warn!("Unrecognised global rule action '{other}', skipping"),
        }
    }

    // 11. Resolve full extension config (destination + rules) for only the
    // extensions actually present in the source folder — this is the whole
    // point of the exercise, so a folder with e.g. just .pdf and .zip files
    // never pulls rules for the other 40 extensions someone has configured.
    let extensions = if found_extensions.is_empty() {
        info!("No files found in source folder, nothing to resolve.");
        Vec::new()
    } else {
        let names: Vec<String> = found_extensions.into_iter().collect();
        fetch_resolve_extensions(&client, &api_url, &api_key, &names).map_err(|e| {
            error!("Could not resolve extensions: {e}");
            anyhow::anyhow!("{e}")
        })?
    };
    trace!("Resolved extensions {extensions:#?}");

    // 12. Process the files
    let mut settings: HashMap<String, String> = HashMap::new();
    settings.insert(
        "source_folder".to_string(),
        app_settings.source_folder.clone(),
    );
    settings.insert("prefix_override".to_string(), dest_base.clone());
    settings.insert("trial_run".to_string(), app_settings.trial_run.to_string());
    settings.insert("debug".to_string(), app_settings.debug.to_string());
    settings.insert("threads".to_string(), app_settings.threads.to_string());
    if let Some(sort_by_size) = map.get("sort_by_size") {
        settings.insert("sort_by_size".to_string(), (*sort_by_size).to_string());
    }

    let config = ApiConfig {
        settings,
        ignore_rules,
        delete_rules,
        extensions,
    };

    process_directory(config, None, entries)?;

    info!("Run complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Fetch global ignore/delete rules
// ---------------------------------------------------------------------------
fn fetch_global_rules(
    client: &Client,
    api_url: &str,
    api_key: &str,
) -> Result<Vec<ApiGlobalRule>, Box<dyn std::error::Error>> {
    let fetch_url = format!("{api_url}/global-rules");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    debug!("Fetching global rules from control plane...");
    let response = client.get(fetch_url).headers(headers).send()?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed: Invalid or missing API Key!".into());
    } else if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    // Note: /global-rules returns every rule regardless of is_active, unlike
    // /config's inline query which filters server-side — so we filter here.
    let rules: Vec<ApiGlobalRule> = response.json()?;
    Ok(rules)
}

// ---------------------------------------------------------------------------
// Resolve full config for only the extensions present locally
// ---------------------------------------------------------------------------
fn fetch_resolve_extensions(
    client: &Client,
    api_url: &str,
    api_key: &str,
    names: &[String],
) -> Result<Vec<ApiExtension>, Box<dyn std::error::Error>> {
    let fetch_url = format!("{api_url}/extensions/resolve");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    debug!("Resolving extension configuration for: {names:?}");
    let response = client
        .post(fetch_url)
        .headers(headers)
        .json(&serde_json::json!({ "names": names }))
        .send()?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed: Invalid or missing API Key!".into());
    } else if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    let extensions: Vec<ApiExtension> = response.json()?;
    Ok(extensions)
}

// ---------------------------------------------------------------------------
// Fetch Settings
// ---------------------------------------------------------------------------
fn fetch_settings(
    client: &Client,
    api_url: &str,
    api_key: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let fetch_url = format!("{api_url}/settings");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    debug!("Syncing settings from control plane...");
    let response = client.get(fetch_url).headers(headers).send()?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed: Invalid or missing API Key!".into());
    } else if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    let app_settings_json: serde_json::Value = response.json()?;
    Ok(app_settings_json)
}

// ---------------------------------------------------------------------------
// Main processing engine
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn process_directory(
    cfg: ApiConfig,
    base_home: Option<&Path>,
    entries: Vec<std::io::Result<fs::DirEntry>>,
) -> Result<()> {
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

    let resolve_home = || -> Result<PathBuf> {
        base_home.map_or_else(
            || dirs_next::home_dir().context("Failed to get home directory"),
            |h| Ok(h.to_path_buf()),
        )
    };

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
        cfg.settings.get("unix_prefix").cloned().unwrap_or_default()
    };

    let dest_base: PathBuf = if prefix.is_empty() {
        resolve_home()?
    } else {
        PathBuf::from(&prefix)
    };

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

    let mut compiled_exts: std::collections::HashMap<String, CompiledExtension> =
        std::collections::HashMap::new();

    for ext in cfg.extensions {
        let mut api_rules = ext.rules;
        api_rules.sort_by_key(|r| r.order);

        let mut comp_rules = Vec::with_capacity(api_rules.len());
        for rule in api_rules {
            match Regex::new(&rule.regex) {
                Ok(re) => comp_rules.push(CompiledRule {
                    regex: re,
                    destination: rule.destination,
                    append_year_month: rule.append_year_month,
                }),
                Err(e) => {
                    warn!(
                        "Error compiling regex '{}' for extension '{}': {e}",
                        rule.regex, ext.name
                    );
                }
            }
        }

        let default_destination = ext.default_destination;

        compiled_exts.insert(
            ext.name.to_lowercase(),
            CompiledExtension {
                default_destination,
                append_year_month: ext.append_year_month,
                rules: comp_rules,
            },
        );
    }

    let mut fromtos: Vec<(PathBuf, PathBuf, Metadata)> = Vec::new();

    let mut created_or_logged_dirs: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();

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

        if compiled_ignore.iter().any(|re| re.is_match(&filename)) {
            debug!("Skipping file matching global ignore rule: {filename}");
            continue;
        }

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

        let matched = mapping.rules.iter().find(|r| r.regex.is_match(&filename));
        let (destination, append_year_month) = matched.map_or_else(
            || {
                debug!(
                    "No regex match for: {filename}, using default: {}",
                    mapping
                        .default_destination
                        .iter()
                        .collect::<PathBuf>()
                        .display()
                );
                (&mapping.default_destination, mapping.append_year_month)
            },
            |rule| {
                debug!(
                    "Regex matched for: {filename} destination is now: {}",
                    rule.destination.iter().collect::<PathBuf>().display()
                );
                (&rule.destination, rule.append_year_month)
            },
        );

        if destination.is_empty() {
            warn!(
                "Empty destination configured for '{filename}' (extension '.{ext_str}'), skipping"
            );
            continue;
        }

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

        let dest_path = dest_dir.join(&*filename);

        if dest_path == from {
            debug!(
                "File {} is already at its destination: {}",
                filename,
                dest_path.display()
            );
            continue;
        }

        // Fetch metadata once – needed for size check, timestamps and speed calc
        let metadata = from
            .metadata()
            .with_context(|| format!("Failed to get metadata for: {}", from.display()))?;

        let mut final_dest = dest_path;
        let mut skip_file = false;

        if final_dest.exists() {
            let dest_len = final_dest.metadata().map_or(u64::MAX, |m| m.len());

            if dest_len == metadata.len() {
                warn!(
                    "{} already exists with same size ({} bytes), skipping",
                    final_dest.display(),
                    dest_len
                );
                skip_file = true;
            } else {
                // Different size → find a free name with numeric suffix
                let stem = final_dest
                    .file_stem()
                    .map_or_else(|| "file".to_string(), |s| s.to_string_lossy().into_owned());

                let ext = final_dest
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();

                let mut counter = 1u32;
                loop {
                    let candidate = dest_dir.join(format!("{stem}-{counter}{ext}"));
                    if !candidate.exists() {
                        warn!(
                            "Collision (different size): renaming {} → {}",
                            final_dest.file_name().unwrap_or_default().to_string_lossy(),
                            candidate.file_name().unwrap_or_default().to_string_lossy()
                        );
                        final_dest = candidate;
                        break;
                    }
                    counter += 1;
                    if counter > 10_000 {
                        error!("Too many collisions for {}", final_dest.display());
                        skip_file = true;
                        break;
                    }
                }
            }
        }

        if skip_file {
            continue;
        }

        if !dest_dir.exists() && created_or_logged_dirs.insert(dest_dir.clone()) {
            if trial_run {
                info!(
                    "Would create folder: {} for '.{}'",
                    dest_dir.display(),
                    ext_str
                );
            } else {
                debug!("Creating destination folder: {}", dest_dir.display());
                fs::create_dir_all(&dest_dir).with_context(|| {
                    format!("Failed to create directory: {}", dest_dir.display())
                })?;
            }
        }

        fromtos.push((from, final_dest, metadata));
    }

    if trial_run {
        for (src, dst, _) in &fromtos {
            info!(
                "Would move: {} -> {}",
                src.file_name().unwrap_or_default().to_string_lossy(),
                dst.display()
            );
        }
    } else {
        let pool = ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .context("Failed to build rayon thread pool")?;
        pool.install(|| {
            fromtos.par_iter().for_each(|(src, dst, metadata)| {
                if let Err(e) = move_file(src, dst, metadata) {
                    error!("Error moving {}: {e}", src.display());
                }
            });
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
        error!("Setting file times failed for {}: {e}", dst.display());
    }

    fs::remove_file(src)
        .with_context(|| format!("Failed to remove source file {}", src.display()))?;

    let duration = start.elapsed();
    info!(
        "→ {:.2} MiB/s \t{}",
        calculate_mibps(metadata.len(), duration),
        dst.display()
    );

    Ok(())
}

fn fetch_user_me(
    client: &Client,
    api_url: &str,
    api_key: &str,
) -> Result<ApiUser, Box<dyn std::error::Error>> {
    let fetch_url = format!("{api_url}/account/me");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    let response = client.get(&fetch_url).headers(headers).send()?;
    if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    let user: ApiUser = response.json()?;
    Ok(user)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)]
fn calculate_mibps(file_size_bytes: u64, duration: Duration) -> f64 {
    if duration.as_secs_f64() == 0.0 {
        return 0.0;
    }
    // 1 MiB = 1 048 576 bytes
    file_size_bytes as f64 / duration.as_secs_f64() / 1_048_576.0
}

pub struct DynamicLogger {
    inner: Box<dyn SharedLogger>,
    debug_flag: Arc<AtomicBool>,
}

impl log::Log for DynamicLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        // Trace = 5, Debug = 4, Info = 3, Warn = 2, Error = 1
        // Filter out Debug/Trace unless debug_flag (or trace mode) is set to true
        if metadata.level() >= log::Level::Debug && !self.debug_flag.load(Ordering::Relaxed) {
            return false;
        }

        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

impl SharedLogger for DynamicLogger {
    fn level(&self) -> LevelFilter {
        self.inner.level()
    }

    fn config(&self) -> Option<&Config> {
        self.inner.config()
    }

    fn as_log(self: Box<Self>) -> Box<dyn log::Log> {
        self
    }
}

fn setup_logging(
    cli_debug: bool,
    cli_quiet: bool,
    cli_trace: bool,
    log_dir: &Path,
) -> anyhow::Result<Arc<AtomicBool>> {
    let debug_flag = Arc::new(AtomicBool::new(cli_debug));
    let mut loggers: Vec<Box<dyn SharedLogger>> = Vec::new();

    let level_filter = if cli_trace {
        LevelFilter::Trace
    } else {
        LevelFilter::Debug
    };

    // Terminal logger only when not quiet
    if !cli_quiet {
        let term_config = ConfigBuilder::new()
            .set_thread_level(LevelFilter::Off)
            .set_target_level(LevelFilter::Off)
            .add_filter_ignore_str("reqwest")
            .add_filter_ignore_str("hyper")
            .add_filter_ignore_str("rustls")
            .set_time_level(LevelFilter::Off)
            .build();

        loggers.push(TermLogger::new(
            level_filter,
            term_config,
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ));
    }

    // File logger is always present (captures up to Trace)
    let file_logger = WriteLogger::new(
        LevelFilter::Trace,
        Config::default(),
        File::create(log_dir.join("cleandl-rs.log")).context("Failed to open log file")?,
    );
    loggers.push(file_logger);

    let combined = CombinedLogger::new(loggers);

    let dynamic_logger = DynamicLogger {
        inner: combined,
        debug_flag: Arc::clone(&debug_flag),
    };

    log::set_boxed_logger(Box::new(dynamic_logger))
        .map(|()| log::set_max_level(LevelFilter::Trace))
        .context("Failed to initialize logger")?;

    Ok(debug_flag)
}

// ---------------------------------------------------------------------------
// Automated Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // Category 1: Unit Tests
    #[test]
    fn test_calculate_mibps_normal() {
        let bytes = 10_485_760; // exactly 10 MiB
        let duration = Duration::from_secs(2);
        let speed = calculate_mibps(bytes, duration);
        assert!((speed - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_mibps_zero_duration() {
        let bytes = 10_485_760;
        let duration = Duration::from_secs(0);
        let speed = calculate_mibps(bytes, duration);
        assert!((speed - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_json_deserialization_defaults() {
        let raw_json = r#"{
            "settings": {"source_folder": "Downloads"},
            "extensions": []
        }"#;

        let config: ApiConfig = serde_json::from_str(raw_json).expect("Deserialization failed");
        assert!(config.ignore_rules.is_empty());
        assert!(config.delete_rules.is_empty());
        assert_eq!(config.settings.get("source_folder").unwrap(), "Downloads");
    }

    #[test]
    fn test_rule_sorting_order() {
        let raw_json = r#"{
            "settings": {},
            "extensions": [{
                "name": "pdf",
                "default_destination": ["Downloads"],
                "append_year_month": false,
                "rules": [
                    {"regex": "second", "destination": ["2"], "order": 2},
                    {"regex": "first", "destination": ["1"], "order": 1}
                ]
            }]
        }"#;

        let mut config: ApiConfig = serde_json::from_str(raw_json).unwrap();
        let ext = config.extensions.remove(0);
        let mut rules = ext.rules;
        rules.sort_by_key(|r| r.order);

        assert_eq!(rules[0].order, 1);
        assert_eq!(rules[0].regex, "first");
        assert_eq!(rules[1].order, 2);
        assert_eq!(rules[1].regex, "second");
    }

    // Category 2: Integration Tests (Filesystem Sandbox)
    fn setup_test_environment() -> (TempDir, PathBuf, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let source_path = temp_dir.path().join("Downloads");
        let dest_prefix = temp_dir.path().join("NAS");

        fs::create_dir_all(&source_path).unwrap();
        fs::create_dir_all(&dest_prefix).unwrap();

        (temp_dir, source_path, dest_prefix)
    }

    #[test]
    fn test_directory_processing_and_routing() {
        let (_temp, source_path, dest_prefix) = setup_test_environment();

        let sample_file = source_path.join("Scan 0001.pdf");
        fs::write(&sample_file, "dummy content").unwrap();

        let ignore_file = source_path.join(".stignore");
        fs::write(&ignore_file, "ignore me").unwrap();

        let delete_file = source_path.join("file.sync-conflict-2026.pdf");
        fs::write(&delete_file, "conflict data").unwrap();

        // Capture expected year/month from the source file *before* it is moved
        let metadata = fs::metadata(&sample_file).unwrap();
        let mtime = FileTime::from_last_modification_time(&metadata);
        let dt = DateTime::from_timestamp(mtime.unix_seconds(), mtime.nanoseconds()).unwrap();
        let year = dt.year().to_string();
        let month = format!("{:02}", dt.month());

        let mut settings = HashMap::new();
        settings.insert(
            "source_folder".to_string(),
            source_path.to_str().unwrap().to_string(),
        );
        settings.insert(
            "prefix_override".to_string(),
            dest_prefix.to_str().unwrap().to_string(),
        );
        settings.insert("trial_run".to_string(), "false".to_string());

        let pdf_ext = ApiExtension {
            name: "pdf".to_string(),
            default_destination: vec!["Downloads".to_string()],
            append_year_month: false,
            rules: vec![ApiRule {
                regex: r"^Scan ".to_string(),
                destination: vec!["Documents".to_string(), "Scanned".to_string()],
                order: 1,
                append_year_month: true,
            }],
        };

        let config = ApiConfig {
            settings,
            ignore_rules: vec![r"^\.".to_string()],
            delete_rules: vec!["sync-conflict".to_string()],
            extensions: vec![pdf_ext],
        };

        let entries: Vec<std::io::Result<fs::DirEntry>> =
            fs::read_dir(&source_path).unwrap().collect();
        process_directory(config, None, entries).unwrap();

        assert!(!delete_file.exists());
        assert!(ignore_file.exists());
        assert!(!sample_file.exists());

        let expected_dest = dest_prefix
            .join("Documents/Scanned")
            .join(year)
            .join(month)
            .join("Scan 0001.pdf");

        assert!(expected_dest.exists());
    }

    #[test]
    fn test_trial_run_mode() {
        let (_temp, source_path, dest_prefix) = setup_test_environment();

        let sample_file = source_path.join("test.exe");
        fs::write(&sample_file, "exe payload").unwrap();

        let mut settings = HashMap::new();
        settings.insert(
            "source_folder".to_string(),
            source_path.to_str().unwrap().to_string(),
        );
        settings.insert(
            "prefix_override".to_string(),
            dest_prefix.to_str().unwrap().to_string(),
        );
        settings.insert("trial_run".to_string(), "true".to_string());

        let exe_ext = ApiExtension {
            name: "exe".to_string(),
            default_destination: vec!["Software".to_string()],
            append_year_month: false,
            rules: vec![],
        };

        let config = ApiConfig {
            settings,
            ignore_rules: vec![],
            delete_rules: vec![],
            extensions: vec![exe_ext],
        };

        let entries: Vec<std::io::Result<fs::DirEntry>> =
            fs::read_dir(&source_path).unwrap().collect();
        process_directory(config, None, entries).unwrap();

        assert!(sample_file.exists());
        let uncreated_dest = dest_prefix.join("Software/test.exe");
        assert!(!uncreated_dest.exists());
    }

    #[test]
    fn test_timestamp_preservation() {
        let temp_dir = TempDir::new().unwrap();
        let src_file = temp_dir.path().join("src.txt");
        let dst_file = temp_dir.path().join("dst.txt");

        fs::write(&src_file, "timestamp payload").unwrap();

        let past_time = FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_times(&src_file, past_time, past_time).unwrap();

        let metadata = fs::metadata(&src_file).unwrap();
        move_file(&src_file, &dst_file, &metadata).unwrap();

        assert!(!src_file.exists());
        assert!(dst_file.exists());

        let dst_metadata = fs::metadata(&dst_file).unwrap();
        let dst_mtime = FileTime::from_last_modification_time(&dst_metadata);

        assert_eq!(dst_mtime.unix_seconds(), past_time.unix_seconds());
    }
}
