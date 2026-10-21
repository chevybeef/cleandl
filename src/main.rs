use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Utc};
use clap::{Parser, Subcommand};
use filetime::FileTime;
use indicatif::{ProgressBar, ProgressStyle};
use log::Record;
use log::{debug, error, info, trace, warn};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use simplelog::{
    ColorChoice, CombinedLogger, Config, ConfigBuilder, LevelFilter, SharedLogger, TermLogger,
    TerminalMode, WriteLogger,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::DirEntry;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod update;

// ---------------------------------------------------------------------------
// API response types — mirror the JSON shape served by cleandloapi
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ApiExtension {
    name: String,
    #[serde(default)]
    default_destination: Vec<String>,
    #[serde(default)]
    append_year_month: bool,
    #[serde(default)]
    rules: Vec<ApiRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ApiGlobalRule {
    action: String,
    regex: String,
    #[serde(default)]
    is_active: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ApiConfig {
    settings: HashMap<String, String>,
    #[serde(default)]
    ignore_rules: Vec<String>,
    #[serde(default)]
    delete_rules: Vec<String>,
    #[serde(default)]
    extensions: Vec<ApiExtension>,
}

#[derive(Debug, Deserialize)]
struct ConfigurationVersion {
    updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CachedConfiguration {
    schema_version: u32,
    api_url: String,
    api_key_fingerprint: String,
    configuration_version: i64,
    settings: HashMap<String, String>,
    global_rules: Vec<ApiGlobalRule>,
    extensions: Vec<ApiExtension>,
    resolved_extensions: HashSet<String>,
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
    #[command(subcommand)]
    command: Option<Command>,

    /// Run offline with built-in example rules; always implies a dry run
    #[arg(long, action = clap::ArgAction::SetTrue)]
    demo: bool,

    /// Source folder override (default: "Downloads")
    #[arg(long, require_equals = true)]
    source: Option<String>,

    /// OS prefix override (default: platform prefix from the API)
    #[arg(long, require_equals = true)]
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

    /// Deprecated compatibility option; ignored (copies are sequential)
    #[arg(long, require_equals = true)]
    threads: Option<usize>,

    /// Enable trace logging
    #[arg(long, action = clap::ArgAction::SetTrue)]
    trace: bool,

    /// API base URL
    /// Falls back to `CLEANDL_API_URL` or the file `cleandl.url` next to the key.
    #[arg(long, env = "CLEANDL_API_URL", require_equals = true)]
    api_url: Option<String>,

    /// API Key for central control plane authentication.
    /// Falls back to reading `cleandl.key` in the current directory when omitted.
    #[arg(long, env = "CLEANDL_API_KEY", require_equals = true)]
    api_key: Option<String>,

    /// Suppress terminal output (log file is still written)
    #[arg(long, action = clap::ArgAction::SetTrue)]
    quiet: bool,
}

#[derive(Clone, Copy, Debug, Subcommand)]
enum Command {
    /// Check GitHub for a newer stable release without installing it
    CheckUpdate,
    /// Check for, confirm, and install the latest stable release
    Update,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const GIT_SHA: &str = env!("VERGEN_GIT_SHA");
const GIT_DIRTY: &str = env!("VERGEN_GIT_DIRTY");
const CACHE_FILE: &str = "cleandl.cache.json";
const CACHE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_API_URL: &str = "https://cleandl.chevybeef.com/api/v1";
const CHUNK: usize = 1024 * 1024;
const SYNC_EVERY: u64 = 16 * 1024 * 1024;

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

    if let Some(command) = args.command {
        return match command {
            Command::CheckUpdate => update::run_check_command(),
            Command::Update => update::run_update_command(),
        };
    }

    // 3. display the program's version string
    let now = chrono::Local::now();
    let dirty = if GIT_DIRTY == "true" { "-dirty" } else { "" };
    info!(
        "{PKG_NAME} v{PKG_VERSION} ({GIT_SHA}{dirty}) on {} on {}",
        now.format("%Y-%m-%d at %H:%M:%S"),
        std::env::consts::OS,
    );

    match update::maybe_offer_automatic_update(&log_dir, args.quiet) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => warn!("Could not check for updates: {error:#}"),
    }

    let prepared_run = if args.demo {
        Some(prepare_demo_run(&args, &debug_flag)?)
    } else {
        prepare_api_run(&args, &debug_flag, &log_dir)?
    };
    let Some((config, entries)) = prepared_run else {
        return Ok(());
    };

    if setting_is_true(&config.settings, "trial_run") {
        info!("Trial run enabled; files will not actually be moved.");
    }

    process_directory(config, None, entries)?;

    info!("Run complete.");
    Ok(())
}

fn prepare_demo_run(
    args: &Cli,
    debug_flag: &AtomicBool,
) -> Result<(ApiConfig, Vec<std::io::Result<DirEntry>>)> {
    info!("Demo mode: using built-in example rules with trial run forced on.");
    info!("Demo mode is offline and will not move or delete any source files.");
    if args.dry_run == Some(false) {
        warn!("Ignoring --dry-run=false because demo mode is always a trial run.");
    }

    let settings = resolve_settings(&demo_settings(), args, debug_flag);

    let source_folder = settings
        .get("source_folder")
        .context("missing 'source_folder' setting")?;

    let (entries, found_extensions) = scan_source_directory(source_folder)?;
    let config = demo_config(settings, &found_extensions);
    trace!("Demo configuration {config:#?}");
    Ok((config, entries))
}

fn demo_settings() -> HashMap<String, String> {
    if cfg!(target_os = "windows") {
        HashMap::from([(
            "windows_prefix".to_string(),
            r"\\truenas\downloads".to_string(),
        )])
    } else if cfg!(target_os = "macos") {
        HashMap::from([("macos_prefix".to_string(), "/Volumes/truenas".to_string())])
    } else {
        HashMap::from([("unix_prefix".to_string(), "/mnt/truenas".to_string())])
    }
}

fn prepare_api_run(
    args: &Cli,
    debug_flag: &AtomicBool,
    log_dir: &Path,
) -> Result<Option<(ApiConfig, Vec<std::io::Result<DirEntry>>)>> {
    let api_url = load_api_url(args.api_url.as_deref(), log_dir)?;
    let Some(api_key) = load_api_key(args.api_key.as_deref(), log_dir, &api_url)? else {
        return Ok(None);
    };
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let cache_path = log_dir.join(CACHE_FILE);
    let mut cached = load_cached_configuration(&cache_path, &api_url, &api_key);

    let remote_version = match fetch_configuration_version(&client, &api_url, &api_key) {
        Ok(version) => version.updated_at,
        Err(error) if !is_authentication_error(&error) => {
            debug!("Control plane unavailable; using cached configuration: {error:#}");
            return prepare_cached_run(
                cached.as_ref().context(
                    "Control plane is unavailable and no matching cached configuration exists",
                )?,
                args,
                debug_flag,
            )
            .map(Some);
        }
        Err(error) => return Err(error),
    };

    if cached
        .as_ref()
        .is_some_and(|cache| cache.configuration_version == remote_version)
    {
        let cache = cached
            .as_mut()
            .context("cached configuration disappeared")?;
        let settings = resolve_settings(&cache.settings, args, debug_flag);
        let source_folder = settings
            .get("source_folder")
            .context("missing 'source_folder' setting")?;
        let (entries, found_extensions) = scan_source_directory(source_folder)?;
        let missing: Vec<String> = found_extensions
            .difference(&cache.resolved_extensions)
            .cloned()
            .collect();

        if !missing.is_empty() {
            match fetch_resolve_extensions(&client, &api_url, &api_key, &missing) {
                Ok(mut extensions) => {
                    let missing_names: HashSet<&str> = missing.iter().map(String::as_str).collect();
                    cache
                        .extensions
                        .retain(|extension| !missing_names.contains(extension.name.as_str()));
                    cache.extensions.append(&mut extensions);
                    cache.resolved_extensions.extend(missing);
                    if let Err(error) = save_cached_configuration(&cache_path, cache) {
                        warn!("Could not update configuration cache: {error:#}");
                    }
                }
                Err(error) => {
                    debug!("Control plane became unavailable; using cached configuration: {error}");
                    return prepare_cached_run(cache, args, debug_flag).map(Some);
                }
            }
        }

        maybe_log_account(&client, &api_url, &api_key, &settings);
        let config = build_api_config(
            settings,
            &cache.global_rules,
            cache
                .extensions
                .iter()
                .filter(|extension| found_extensions.contains(&extension.name.to_lowercase()))
                .cloned()
                .collect(),
        );
        return Ok(Some((config, entries)));
    }

    let settings_map = match load_settings(&client, &api_url, &api_key) {
        Ok(settings) => settings,
        Err(error) => {
            debug!("Control plane became unavailable; using cached configuration: {error:#}");
            return prepare_cached_run(
                cached.as_ref().context(
                    "Control plane is unavailable and no matching cached configuration exists",
                )?,
                args,
                debug_flag,
            )
            .map(Some);
        }
    };
    let settings = resolve_settings(&settings_map, args, debug_flag);

    let source_folder = settings
        .get("source_folder")
        .context("missing 'source_folder' setting")?;
    let (entries, found_extensions) = scan_source_directory(source_folder)?;

    maybe_log_account(&client, &api_url, &api_key, &settings);

    // Fetch global ignore/delete rules (these apply to every filename
    // regardless of extension, so there's nothing to prune here).
    let global_rules = match fetch_global_rules(&client, &api_url, &api_key) {
        Ok(rules) => rules,
        Err(error) => {
            debug!("Control plane became unavailable; using cached configuration: {error}");
            return prepare_cached_run(
                cached.as_ref().context(
                    "Control plane is unavailable and no matching cached configuration exists",
                )?,
                args,
                debug_flag,
            )
            .map(Some);
        }
    };

    // Resolve full extension config (destination + rules) for only the
    // extensions actually present in the source folder — this is the whole
    // point of the exercise, so a folder with e.g. just .pdf and .zip files
    // never pulls rules for the other 40 extensions someone has configured.
    let extensions = if found_extensions.is_empty() {
        info!("No files found in source folder, nothing to resolve.");
        Vec::new()
    } else {
        let names: Vec<String> = found_extensions.iter().cloned().collect();
        match fetch_resolve_extensions(&client, &api_url, &api_key, &names) {
            Ok(extensions) => extensions,
            Err(error) => {
                debug!("Control plane became unavailable; using cached configuration: {error}");
                return prepare_cached_run(
                    cached.as_ref().context(
                        "Control plane is unavailable and no matching cached configuration exists",
                    )?,
                    args,
                    debug_flag,
                )
                .map(Some);
            }
        }
    };
    trace!("Resolved extensions {extensions:#?}");

    let mut cache_extensions = cached
        .as_ref()
        .map(|cache| cache.extensions.clone())
        .unwrap_or_default();
    cache_extensions.retain(|extension| !found_extensions.contains(&extension.name.to_lowercase()));
    cache_extensions.extend(extensions.iter().cloned());
    let cache = CachedConfiguration {
        schema_version: CACHE_SCHEMA_VERSION,
        api_url,
        api_key_fingerprint: api_key_fingerprint(&api_key),
        configuration_version: remote_version,
        settings: settings_map,
        global_rules: global_rules.clone(),
        extensions: cache_extensions,
        resolved_extensions: found_extensions,
    };
    if let Err(error) = save_cached_configuration(&cache_path, &cache) {
        warn!("Could not update configuration cache: {error:#}");
    }
    let config = build_api_config(settings, &global_rules, extensions);
    Ok(Some((config, entries)))
}

fn maybe_log_account(
    client: &Client,
    api_url: &str,
    api_key: &str,
    settings: &HashMap<String, String>,
) {
    if !setting_is_true(settings, "debug") {
        return;
    }
    match fetch_user_me(client, api_url, api_key) {
        Ok(user) => debug!(
            "Configuration fetched for id: {}, username: {}, role: {}",
            user.id, user.username, user.role
        ),
        Err(error) => debug!("Could not fetch account info: {error}"),
    }
}

fn build_api_config(
    settings: HashMap<String, String>,
    global_rules: &[ApiGlobalRule],
    extensions: Vec<ApiExtension>,
) -> ApiConfig {
    let mut ignore_rules = Vec::new();
    let mut delete_rules = Vec::new();
    for rule in global_rules.iter().filter(|rule| rule.is_active) {
        match rule.action.as_str() {
            "IGNORE" => ignore_rules.push(rule.regex.clone()),
            "DELETE" => delete_rules.push(rule.regex.clone()),
            other => warn!("Unrecognised global rule action '{other}', skipping"),
        }
    }
    ApiConfig {
        settings,
        ignore_rules,
        delete_rules,
        extensions,
    }
}

fn prepare_cached_run(
    cache: &CachedConfiguration,
    args: &Cli,
    debug_flag: &AtomicBool,
) -> Result<(ApiConfig, Vec<std::io::Result<DirEntry>>)> {
    let settings = resolve_settings(&cache.settings, args, debug_flag);
    let source_folder = settings
        .get("source_folder")
        .context("missing 'source_folder' setting")?;
    let (entries, found_extensions) = scan_source_directory(source_folder)?;
    if !setting_is_true(&settings, "trial_run") {
        confirm_cached_live_run()?;
    }
    let extensions = cache
        .extensions
        .iter()
        .filter(|extension| found_extensions.contains(&extension.name.to_lowercase()))
        .cloned()
        .collect();
    Ok((
        build_api_config(settings, &cache.global_rules, extensions),
        entries,
    ))
}

fn confirm_cached_live_run() -> Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    confirm_cached_live_run_with(&mut input, &mut output)
}

fn confirm_cached_live_run_with(input: &mut impl BufRead, output: &mut impl Write) -> Result<()> {
    write!(
        output,
        "The control plane is unavailable and trial_run is false. Continue using cached settings? [y/N] "
    )?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("Run cancelled; no files were changed")
    }
}

fn demo_config(settings: HashMap<String, String>, found: &HashSet<String>) -> ApiConfig {
    let mut extensions = Vec::new();

    let simple_groups: &[(&[&str], &[&str], bool)] = &[
        (&["doc", "docx", "odt", "rtf", "txt"], &["Documents"], false),
        (
            &["csv", "ods", "xls", "xlsx"],
            &["Documents", "Spreadsheets"],
            false,
        ),
        (&["mp3", "flac", "m4a", "ogg", "wav"], &["Music"], false),
        (&["mkv", "mov", "mp4", "webm"], &["Videos"], true),
        (
            &["7z", "bz2", "gz", "rar", "tar", "xz", "zip"],
            &["Archives"],
            false,
        ),
        (
            &["deb", "dmg", "exe", "iso", "msi", "pkg", "rpm"],
            &["Software"],
            false,
        ),
    ];

    for (names, destination, append_year_month) in simple_groups {
        for name in *names {
            if found.contains(*name) {
                extensions.push(ApiExtension {
                    name: (*name).to_string(),
                    default_destination: destination.iter().map(ToString::to_string).collect(),
                    append_year_month: *append_year_month,
                    rules: Vec::new(),
                });
            }
        }
    }

    for name in ["gif", "jpeg", "jpg", "png", "webp"] {
        if found.contains(name) {
            extensions.push(ApiExtension {
                name: name.to_string(),
                default_destination: vec!["Pictures".to_string()],
                append_year_month: false,
                rules: vec![ApiRule {
                    regex: "(?i)^screenshot".to_string(),
                    destination: vec!["Pictures".to_string(), "Screenshots".to_string()],
                    order: 1,
                    append_year_month: true,
                }],
            });
        }
    }

    if found.contains("pdf") {
        extensions.push(ApiExtension {
            name: "pdf".to_string(),
            default_destination: vec!["Documents".to_string(), "PDF".to_string()],
            append_year_month: false,
            rules: vec![
                ApiRule {
                    regex: "(?i)(invoice|receipt)".to_string(),
                    destination: vec!["Documents".to_string(), "Finance".to_string()],
                    order: 1,
                    append_year_month: true,
                },
                ApiRule {
                    regex: "(?i)^(scan|scanned)".to_string(),
                    destination: vec!["Documents".to_string(), "Scanned".to_string()],
                    order: 2,
                    append_year_month: true,
                },
            ],
        });
    }

    ApiConfig {
        settings,
        ignore_rules: vec!["(?i)\\.(crdownload|download|part)$".to_string()],
        delete_rules: vec!["(?i)\\.(temp|tmp)$".to_string()],
        extensions,
    }
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

    let trial_run = if args.demo {
        "true".to_string()
    } else {
        args.dry_run
            .map(|t| t.to_string())
            .or_else(|| map.get("trial_run").cloned())
            .unwrap_or_else(|| "false".to_string())
    };

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
    debug!("File copies are sequential; the threads setting is ignored");

    settings
}

fn write_new_config_file(path: &Path, value: &str, sensitive: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    if sensitive {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    #[cfg(not(unix))]
    let _ = sensitive;

    let mut file = options
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    writeln!(file, "{value}").with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn save_api_key(path: &Path, api_key: &str) -> Result<()> {
    if path.exists() {
        fs::write(path, format!("{api_key}\n"))
            .with_context(|| format!("Failed to write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to secure {}", path.display()))?;
        }
        Ok(())
    } else {
        write_new_config_file(path, api_key, true)
    }
}

fn login_url(api_url: &str) -> String {
    format!("{}/login", api_url.trim_end_matches("/api/v1"))
}

fn load_api_key(
    api_key_arg: Option<&str>,
    base_dir: &Path,
    api_url: &str,
) -> Result<Option<String>> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stderr();
    load_api_key_with(api_key_arg, base_dir, api_url, &mut input, &mut output)
}

fn load_api_key_with(
    api_key_arg: Option<&str>,
    base_dir: &Path,
    api_url: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<String>> {
    if let Some(api_key) = api_key_arg.map(str::trim).filter(|value| !value.is_empty()) {
        debug!("API key loaded from CLI / CLEANDL_API_KEY");
        return Ok(Some(api_key.to_owned()));
    }

    let path = base_dir.join("cleandl.key");
    match fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => {
            debug!("API key loaded from {}", path.display());
            return Ok(Some(raw.trim().to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    }

    writeln!(output, "No CleanDL API key was found.")?;
    writeln!(output, "Sign in here: {}", login_url(api_url))?;
    writeln!(
        output,
        "Under Connection keys, enter a name and choose Generate key."
    )?;
    let api_key = loop {
        write!(output, "Paste the generated key here (Ctrl+C to cancel): ")?;
        output.flush()?;

        let mut api_key = String::new();
        let bytes_read = input
            .read_line(&mut api_key)
            .context("API key input was cancelled")?;
        if bytes_read == 0 {
            bail!("API key input ended before a key was pasted; no key was saved");
        }
        let api_key = api_key.trim();
        if !api_key.is_empty() {
            break api_key.to_owned();
        }
        writeln!(output, "The key cannot be empty. Please try again.")?;
    };

    save_api_key(&path, &api_key)?;
    writeln!(output, "API key saved to {}.", path.display())?;
    writeln!(
        output,
        "Setup complete. Run CleanDL again to sort your downloads."
    )?;
    Ok(None)
}

fn load_api_url(api_url_arg: Option<&str>, base_dir: &Path) -> Result<String> {
    if let Some(api_url) = api_url_arg.map(str::trim).filter(|value| !value.is_empty()) {
        debug!("API URL loaded from CLI / CLEANDL_API_URL");
        return normalize_api_url(api_url);
    }

    let path = base_dir.join("cleandl.url");
    match fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => {
            debug!("API URL loaded from {}", path.display());
            normalize_api_url(&raw)
        }
        Ok(_) => {
            fs::write(&path, format!("{DEFAULT_API_URL}\n"))
                .with_context(|| format!("Failed to write {}", path.display()))?;
            info!("Saved default API URL to {}.", path.display());
            Ok(DEFAULT_API_URL.to_owned())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_new_config_file(&path, DEFAULT_API_URL, false)?;
            info!("Saved default API URL to {}.", path.display());
            Ok(DEFAULT_API_URL.to_owned())
        }
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
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

fn api_key_fingerprint(api_key: &str) -> String {
    format!("{:x}", Sha256::digest(api_key.as_bytes()))
}

fn load_cached_configuration(
    path: &Path,
    api_url: &str,
    api_key: &str,
) -> Option<CachedConfiguration> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            warn!("Could not read configuration cache: {error}");
            return None;
        }
    };
    let cache: CachedConfiguration = match serde_json::from_slice(&raw) {
        Ok(cache) => cache,
        Err(error) => {
            warn!("Ignoring invalid configuration cache: {error}");
            return None;
        }
    };
    if cache.schema_version != CACHE_SCHEMA_VERSION
        || cache.api_url != api_url
        || cache.api_key_fingerprint != api_key_fingerprint(api_key)
    {
        debug!("Ignoring configuration cache belonging to another client configuration");
        return None;
    }
    Some(cache)
}

fn save_cached_configuration(path: &Path, cache: &CachedConfiguration) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, cache)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn is_authentication_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("Authentication failed")
}

fn fetch_configuration_version(
    client: &Client,
    api_url: &str,
    api_key: &str,
) -> Result<ConfigurationVersion> {
    let fetch_url = format!("{api_url}/configuration-version");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    debug!("Checking configuration version: GET {fetch_url}");
    let response = client
        .get(&fetch_url)
        .headers(headers)
        .send()
        .context("Failed to check the control-plane configuration version")?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Authentication failed: Invalid or missing API Key!");
    } else if !response.status().is_success() {
        bail!("Server returned error status: {}", response.status());
    }
    response
        .json()
        .context("Failed to decode the control-plane configuration version")
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

    debug!("Fetching global rules: GET {fetch_url}");
    let started = Instant::now();
    let response = client.get(&fetch_url).headers(headers).send()?;
    debug!(
        "Global rules response: HTTP {} after {:?}",
        response.status(),
        started.elapsed()
    );

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed: Invalid or missing API Key!".into());
    } else if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    // Note: /global-rules returns every rule regardless of is_active, unlike
    // The global-rules endpoint returns every rule, so we filter active rules here.
    let rules: Vec<ApiGlobalRule> = response.json()?;
    debug!("Decoded {} global rules", rules.len());
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

    debug!(
        "Resolving {} extension configurations: POST {fetch_url}",
        names.len()
    );
    let started = Instant::now();
    let response = client
        .post(&fetch_url)
        .headers(headers)
        .json(&serde_json::json!({ "names": names }))
        .send()?;
    debug!(
        "Extension resolution response: HTTP {} after {:?}",
        response.status(),
        started.elapsed()
    );

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed: Invalid or missing API Key!".into());
    } else if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    let extensions: Vec<ApiExtension> = response.json()?;
    debug!("Decoded {} extension configurations", extensions.len());
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

    debug!("Fetching settings: GET {fetch_url}");
    let started = Instant::now();
    let response = client
        .get(&fetch_url)
        .headers(headers)
        .send()
        .context("Failed to fetch settings from the control plane")?;
    debug!(
        "Settings response: HTTP {} after {:?}",
        response.status(),
        started.elapsed()
    );

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("Authentication failed: Invalid or missing API Key!");
    } else if !response.status().is_success() {
        bail!("Server returned error status: {}", response.status());
    }

    let app_settings_json: serde_json::Value = response
        .json()
        .context("Failed to decode settings returned by the control plane")?;
    debug!(
        "Decoded {} settings",
        app_settings_json.as_array().map_or(0, Vec::len)
    );
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

    let mut failed_moves = 0_usize;
    // A dry run needs to simulate names occupied by earlier moves.
    let mut simulated_destinations = HashSet::new();

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

        if simulated_destinations.contains(&final_dest) || final_dest.try_exists()? {
            let dest_len = final_dest.metadata().map_or(u64::MAX, |m| m.len());

            if !simulated_destinations.contains(&final_dest) && dest_len == metadata.len() {
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
                    if !simulated_destinations.contains(&candidate) && !candidate.try_exists()? {
                        warn!(
                            "Destination collision: renaming {} → {}",
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

        if trial_run {
            info!(
                "Would move: {} -> {}",
                from.file_name().unwrap_or_default().to_string_lossy(),
                final_dest.display()
            );
            simulated_destinations.insert(final_dest);
        } else if let Err(error) = move_file(&from, &final_dest, &metadata) {
            error!("Error moving {}: {error:#}", from.display());
            failed_moves = failed_moves.saturating_add(1);
        }
    }
    if failed_moves > 0 {
        bail!("{failed_moves} file move(s) failed; see the log for details");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// File move (copy + preserve timestamps + delete source)
// ---------------------------------------------------------------------------

fn move_file(src: &Path, dst: &Path, metadata: &Metadata) -> Result<()> {
    move_file_with_copy(src, dst, metadata, |input, output| {
        copy_with_progress(input, output, src)
    })
}

fn move_file_with_copy(
    src: &Path,
    dst: &Path,
    metadata: &Metadata,
    copy: impl FnOnce(&mut File, &mut File) -> io::Result<u64>,
) -> Result<()> {
    let mtime = FileTime::from_last_modification_time(metadata);
    let atime = FileTime::from_last_access_time(metadata);

    let start = Instant::now();

    let mut input =
        File::open(src).with_context(|| format!("Failed to open source {}", src.display()))?;
    // Keep this handle for the entire copy: fs::copy would reopen and truncate
    // a destination another process created after our collision check.
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("Failed to exclusively create destination {}", dst.display()))?;
    let result = (|| -> Result<()> {
        let copied = copy(&mut input, &mut output)
            .with_context(|| format!("Failed to copy {} to {}", src.display(), dst.display()))?;

        if copied != metadata.len() {
            bail!(
                "Source size changed or copy was incomplete for {}",
                src.display()
            );
        }

        Ok(())
    })();
    drop(output);
    drop(input);
    if let Err(error) = result {
        if let Err(cleanup) = fs::remove_file(dst) {
            return Err(error.context(format!(
                "Could not remove partial destination {}: {cleanup}",
                dst.display()
            )));
        }
        return Err(error);
    }

    if let Err(e) = filetime::set_file_times(dst, atime, mtime) {
        warn!(
            "Copied data successfully, but setting file times failed for {}: {e}",
            dst.display()
        );
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

static SHOW_COPY_PROGRESS: AtomicBool = AtomicBool::new(false);

fn copy_progress_bar(size: u64, enabled: bool) -> ProgressBar {
    if enabled {
        ProgressBar::new(size)
    } else {
        ProgressBar::hidden()
    }
}

fn copy_with_progress(input: &mut File, output: &mut File, src: &Path) -> io::Result<u64> {
    let size = input.metadata()?.len();

    let pb = copy_progress_bar(size, SHOW_COPY_PROGRESS.load(Ordering::Relaxed));

    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {msg} [{bar:40.cyan/blue}] \
             {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
        )
        .map_err(io::Error::other)?
        .progress_chars("=>-"),
    );

    pb.set_message(
        src.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    );

    copy_with_progress_bar(input, output, &pb)
}

fn copy_with_progress_bar(
    mut input: impl Read,
    output: &mut File,
    pb: &ProgressBar,
) -> io::Result<u64> {
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    let mut since_sync = 0u64;

    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let chunk = buf
            .get(..n)
            .ok_or_else(|| io::Error::other("read returned more bytes than the buffer holds"))?;
        output.write_all(chunk)?;

        let n64 = u64::try_from(n).map_err(io::Error::other)?;
        total = total.saturating_add(n64);
        since_sync = since_sync.saturating_add(n64);

        if since_sync >= SYNC_EVERY {
            output.sync_data()?;
            since_sync = 0;
        }
        pb.set_position(total);
    }

    pb.set_message("syncing...");
    output.sync_all()?;
    pb.finish_and_clear();
    Ok(total)
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

    debug!("Fetching account details: GET {fetch_url}");
    let started = Instant::now();
    let response = client.get(&fetch_url).headers(headers).send()?;
    debug!(
        "Account details response: HTTP {} after {:?}",
        response.status(),
        started.elapsed()
    );
    if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    let user: ApiUser = response.json()?;
    debug!("Decoded account details");
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

    SHOW_COPY_PROGRESS.store(!cli_quiet, Ordering::Relaxed);

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
    let app_dir = if cfg!(target_os = "windows") {
        let data_dir =
            dirs_next::data_local_dir().context("unable to get local app data directory")?;
        data_dir.join("cleandl")
    } else {
        let state_dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs_next::home_dir().map(|h| h.join(".local/state")))
            .context("unable to get home directory")?;
        state_dir.join("cleandl")
    };

    let log_dir = if cfg!(target_os = "windows") {
        app_dir.join("Logs")
    } else {
        app_dir
    };

    std::fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
    Ok(log_dir)
}

// ---------------------------------------------------------------------------
// Automated Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
