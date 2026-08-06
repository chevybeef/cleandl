use anyhow::{Context, Result, bail};
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
use std::collections::HashSet;
use std::fs::DirEntry;
use std::fs::{self, File, Metadata};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// API response types — mirror the JSON shape served by cleandloapi
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

/// cleandl: move downloaded files to sorted folders, config from API
#[derive(Parser, Debug)]
#[command(
    author,
    version = cli_version(),
    about = "CleanDL - Cleanup your downloads folder"
)]
struct Cli {
    /// Source folder override (default: "Downloads")
    #[arg(long)]
    source: Option<String>,

    /// OS prefix override (default: platform prefix from the API)
    #[arg(long)]
    prefix: Option<String>,

    /// Dry run override
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    dry_run: Option<bool>,

    /// Debug logging override
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
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

fn cli_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            let dirty = if GIT_DIRTY == "true" { "-dirty" } else { "" };
            format!("{PKG_VERSION} ({GIT_SHA}{dirty})")
        })
        .as_str()
}

fn main() -> Result<()> {
    // 1. Parse first so --help and --version do not touch the filesystem.
    let args = Cli::parse();

    // 2. Set up logging before performing application work.
    let log_dir = setup_logging_dir()?;
    let cli_debug_flag = args.debug.unwrap_or_default() || args.trace;
    let debug_flag = setup_logging(cli_debug_flag, args.quiet, args.trace, &log_dir)?;
    debug!("Will log to {}", log_dir.display());

    // 3. display the program's version string
    let now = chrono::Local::now();
    let dirty = if GIT_DIRTY == "true" { "-dirty" } else { "" };
    info!(
        "{PKG_NAME} v{PKG_VERSION} ({GIT_SHA}{dirty}) on {} on {}",
        now.format("%Y-%m-%d at %H:%M:%S"),
        std::env::consts::OS,
    );

    // 4. get the API key from environment variable or key file
    let api_key = load_api_key(args.api_key.as_deref(), &log_dir)?;

    // 5. get API URL from environment variable or url file
    let api_url = load_api_url(args.api_url.as_deref(), &log_dir)?;

    // 6. fetch the settings so that we have the source folder
    debug!("Fetching settings from {api_url}");
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;

    let settings_map = load_settings(&client, &api_url, &api_key)?;

    let settings = resolve_settings(&settings_map, &args, &debug_flag);

    let source_folder = settings
        .get("source_folder")
        .context("missing 'source_folder' setting")?;

    let (entries, found_extensions) = scan_source_directory(source_folder)?;
    if setting_is_true(&settings, "trial_run") {
        info!("Trial run enabled; files will not actually be moved.");
    }

    // 9. Fetch and display user information from GET api/v1/account/me
    if setting_is_true(&settings, "debug") {
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

fn scan_source_directory(
    source_folder: &str,
) -> Result<(Vec<std::io::Result<DirEntry>>, HashSet<String>)> {
    let source_path = if Path::new(source_folder).is_absolute() {
        PathBuf::from(source_folder)
    } else {
        dirs_next::home_dir()
            .context("Failed to get home directory")?
            .join(source_folder)
    };

    info!("Scanning directory: {}", source_path.display());

    // Collect once: ReadDir is a single-pass iterator, and we need to walk it
    // twice — once here to work out which extensions are present, and again
    // in process_directory to actually move the files.
    let entries: Vec<std::io::Result<DirEntry>> = fs::read_dir(&source_path)
        .with_context(|| format!("Failed to read source directory: {}", source_path.display()))?
        .collect();

    let mut found_extensions = HashSet::new();
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

    Ok((entries, found_extensions))
}

fn resolve_settings(
    map: &HashMap<String, String>,
    args: &Cli,
    debug_flag: &AtomicBool,
) -> HashMap<std::string::String, std::string::String> {
    let mut settings = HashMap::new();

    // 1. Determine platform-specific prefix default from remote map
    let prefix_key = if cfg!(target_os = "windows") {
        "windows_prefix"
    } else if cfg!(target_os = "macos") {
        "macos_prefix"
    } else {
        "unix_prefix"
    };

    let prefix_default = map.get(prefix_key).map_or("", String::as_str);
    let dest_base = args
        .prefix
        .clone()
        .unwrap_or_else(|| prefix_default.to_string());

    // 2. Populate settings map (CLI args take precedence)
    let source_folder = args
        .source
        .clone()
        .or_else(|| map.get("source_folder").cloned())
        .unwrap_or_else(|| "Downloads".to_string());

    let trial_run = args
        .dry_run
        .map(|t| t.to_string())
        .or_else(|| map.get("trial_run").cloned())
        .unwrap_or_else(|| "false".to_string());

    let debug_setting = args
        .debug
        .map(|d| d.to_string())
        .or_else(|| map.get("debug").cloned())
        .unwrap_or_else(|| "false".to_string());

    let threads = args
        .threads
        .map(|r| r.to_string())
        .or_else(|| map.get("threads").cloned())
        .unwrap_or_else(|| "6".to_string());

    settings.insert("source_folder".to_string(), source_folder);
    settings.insert("prefix_override".to_string(), dest_base.clone());
    settings.insert("trial_run".to_string(), trial_run);
    settings.insert("debug".to_string(), debug_setting);
    settings.insert("threads".to_string(), threads);

    if let Some(sort_by_size) = map.get("sort_by_size") {
        settings.insert("sort_by_size".to_string(), sort_by_size.clone());
    }

    // 3. Set the atomic debug flag
    let api_debug_flag = setting_is_true(&settings, "debug") || args.trace;
    debug_flag.store(api_debug_flag, Ordering::Relaxed);

    // 4. Log settings
    debug!("Settings are as follows:");
    debug!("source_folder: {:?}", settings.get("source_folder"));
    debug!("destination_prefix: {dest_base}");
    debug!("trial_run: {:?}", settings.get("trial_run"));
    debug!("debug: {:?}", settings.get("debug"));
    debug!("threads: {:?}", settings.get("threads"));

    settings
}

struct ConfigSource<'a> {
    name: &'a str,         // e.g., "API key" or "API URL"
    filename: &'a str,     // e.g., "cleandl.key" or "cleandl.url"
    cli_env_desc: &'a str, // e.g., "CLEANDL_API_KEY" or "CLI / CLEANDL_API_URL"
    cli_flag: &'a str,     // e.g., "--api-key" or "--api-url"
    example_hint: Option<&'a str>,
    sensitive: bool,
}

fn create_empty_config_file(path: &Path, sensitive: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    if sensitive {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    #[cfg(not(unix))]
    let _ = sensitive;

    options
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    Ok(())
}

fn load_config_val(
    arg_val: Option<&str>,
    base_dir: &Path,
    source: &ConfigSource,
) -> Result<String> {
    if let Some(val) = arg_val.filter(|v| !v.trim().is_empty()) {
        debug!("{} loaded from {}", source.name, source.cli_env_desc);
        return Ok(val.to_string());
    }

    let path = base_dir.join(source.filename);

    if path.exists() {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let trimmed = raw.trim().to_owned();

        if trimmed.is_empty() {
            error!(
                "Error: {} exists but is empty. Provide an {} via {}, {}, or that file.",
                path.display(),
                source.name,
                source.cli_flag,
                source.cli_env_desc
            );
            bail!("{} file {} is empty", source.name, path.display());
        }

        debug!("{} loaded from {}", source.name, path.display());
        Ok(trimmed)
    } else {
        create_empty_config_file(&path, source.sensitive)?;
        let hint = source
            .example_hint
            .map(|h| format!(" (example: {h})"))
            .unwrap_or_default();

        error!(
            "Error: No {} supplied. Edit {} that I created for you{}.",
            source.name,
            path.display(),
            hint
        );
        bail!(
            "No {} supplied; created empty file at {}",
            source.name,
            path.display()
        );
    }
}

fn load_api_key(api_key_arg: Option<&str>, base_dir: &Path) -> Result<String> {
    load_config_val(
        api_key_arg,
        base_dir,
        &ConfigSource {
            name: "API key",
            filename: "cleandl.key",
            cli_env_desc: "CLEANDL_API_KEY",
            cli_flag: "--api-key",
            example_hint: None,
            sensitive: true,
        },
    )
}

fn load_api_url(api_url_arg: Option<&str>, base_dir: &Path) -> Result<String> {
    let raw_url = load_config_val(
        api_url_arg,
        base_dir,
        &ConfigSource {
            name: "API URL",
            filename: "cleandl.url",
            cli_env_desc: "CLI / CLEANDL_API_URL",
            cli_flag: "--api-url",
            example_hint: Some("https://cleandl.example.com/api/v1"),
            sensitive: false,
        },
    )?;
    normalize_api_url(&raw_url)
}

fn normalize_api_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("API URL cannot be empty");
    }
    if trimmed.ends_with("/config") {
        bail!("use the API root ending in /api/v1, not the old /config URL");
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        bail!("API URL must start with http:// or https://");
    }
    if !trimmed.ends_with("/api/v1") {
        bail!("API URL must end in /api/v1");
    }
    Ok(trimmed.to_owned())
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
    // The global-rules endpoint returns every rule, so we filter active rules here.
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
fn load_settings(client: &Client, api_url: &str, api_key: &str) -> Result<HashMap<String, String>> {
    let app_settings_json_value = fetch_settings(client, api_url, api_key)?;
    debug!("Configuration successfully synchronized!");
    info!("Settings loaded successfully.");

    let array = app_settings_json_value
        .as_array()
        .context("Invalid settings response: expected a JSON array")?;
    let mut map = HashMap::new();
    for item in array {
        if let (Some(key), Some(value)) = (item["key"].as_str(), item["value"].as_str()) {
            map.insert(key.to_string(), value.to_string());
        }
    }

    Ok(map)
}

fn fetch_settings(client: &Client, api_url: &str, api_key: &str) -> Result<serde_json::Value> {
    let fetch_url = format!("{api_url}/settings");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    debug!("Syncing settings from control plane...");
    let response = client
        .get(fetch_url)
        .headers(headers)
        .send()
        .context("Failed to fetch settings from the control plane")?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Authentication failed: Invalid or missing API Key!");
    } else if !response.status().is_success() {
        bail!("Server returned error status: {}", response.status());
    }

    let app_settings_json: serde_json::Value = response
        .json()
        .context("Failed to decode settings returned by the control plane")?;
    Ok(app_settings_json)
}

// ---------------------------------------------------------------------------
// Main processing engine
// ---------------------------------------------------------------------------

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

    let mut compiled_exts: HashMap<String, CompiledExtension> = HashMap::new();

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

    let mut created_or_logged_dirs: HashSet<PathBuf> = HashSet::new();

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
            warn!("Extension inactive or not handled for: {filename}");
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

                let mut found_unique = false;
                for counter in 1..=10_000 {
                    let candidate = dest_dir.join(format!("{stem}-{counter}{ext}"));
                    if !candidate.exists() {
                        warn!(
                            "Collision (different size): renaming {} → {}",
                            final_dest.file_name().unwrap_or_default().to_string_lossy(),
                            candidate.file_name().unwrap_or_default().to_string_lossy()
                        );
                        final_dest = candidate;
                        found_unique = true;
                        break;
                    }
                }

                if !found_unique {
                    error!("Too many collisions for {}", final_dest.display());
                    skip_file = true;
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

#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
fn calculate_mibps(file_size_bytes: u64, duration: Duration) -> f64 {
    if duration.as_secs_f64() == 0.0 {
        return 0.0;
    }
    let bytes_f64 = file_size_bytes as f64;
    bytes_f64 / duration.as_secs_f64() / 1_048_576.0
}

fn setting_is_true(settings: &HashMap<String, String>, key: &str) -> bool {
    settings
        .get(key)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
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
        File::create(log_dir.join("cleandl.log")).context("Failed to open log file")?,
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

fn setup_logging_dir() -> Result<PathBuf> {
    let (app_dir, legacy_app_dir) = if cfg!(target_os = "windows") {
        let data_dir =
            dirs_next::data_local_dir().context("unable to get local app data directory")?;
        (data_dir.join("cleandl"), data_dir.join("cleandl-rs"))
    } else {
        let state_dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs_next::home_dir().map(|h| h.join(".local/state")))
            .context("unable to get home directory")?;
        (state_dir.join("cleandl"), state_dir.join("cleandl-rs"))
    };

    migrate_legacy_state_dir(&legacy_app_dir, &app_dir)?;

    let log_dir = if cfg!(target_os = "windows") {
        app_dir.join("Logs")
    } else {
        app_dir
    };

    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    Ok(log_dir)
}

fn migrate_legacy_state_dir(legacy_dir: &Path, current_dir: &Path) -> Result<()> {
    if !current_dir.exists() && legacy_dir.exists() {
        fs::rename(legacy_dir, current_dir).with_context(|| {
            format!(
                "Failed to migrate state directory from {} to {}",
                legacy_dir.display(),
                current_dir.display()
            )
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Automated Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
