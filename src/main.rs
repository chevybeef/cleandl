use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Utc};
use clap::Parser;
use filetime::FileTime;
use log::{LevelFilter, debug, info, warn};
use rayon::ThreadPoolBuilder;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use regex::Regex;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use simplelog::{ColorChoice, CombinedLogger, Config, TermLogger, TerminalMode, WriteLogger};
use std::fs::{self, File, Metadata};
use std::path::{Path, PathBuf};
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
    destination: Option<Vec<String>>,
    #[serde(default)]
    default_destination: Vec<String>,
    #[serde(default)]
    append_year_month: bool,
    #[serde(default)]
    rules: Vec<ApiRule>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiConfig {
    settings: std::collections::HashMap<String, String>,
    #[serde(default)]
    ignore_rules: Vec<String>,
    #[serde(default)]
    delete_rules: Vec<String>,
    #[serde(default)]
    extensions: Vec<ApiExtension>,
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
    #[arg(short = 's', long)]
    source: Option<String>,

    /// OS prefix override
    #[arg(short = 'p', long)]
    prefix: Option<String>,

    /// Dry run override
    #[arg(short = 'r', long)]
    dry_run: Option<bool>,

    /// Debug logging override
    #[arg(short = 'd', long)]
    debug: Option<bool>,

    /// Number of threads override
    #[arg(short = 't', long)]
    threads: Option<usize>,

    /// API base URL
    #[arg(long, default_value = "https://cleandl.chevybeef.com/api/v1/config")]
    api_url: String,

    /// API Key for central control plane authentication.
    /// Falls back to reading `cleandl.key` in the current directory when omitted.
    #[arg(long, env = "CLEANDL_API_KEY")]
    api_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    let args = Cli::parse();

    let api_key = if let Some(key) = args.api_key.filter(|k| !k.trim().is_empty()) {
        key
    } else {
        let mut key_path = std::env::current_exe().context("Failed to determine exe directory")?;
        key_path.pop();
        key_path = key_path.join("cleandl.key");

        if key_path.exists() {
            let raw = fs::read_to_string(&key_path)
                .with_context(|| format!("Failed to read {}", key_path.display()))?;
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                eprintln!(
                    "Error: {} exists but is empty. \
                     Provide an API key via --api-key, CLEANDL_API_KEY, or that file.",
                    key_path.display()
                );
                std::process::exit(1);
            }
            trimmed
        } else {
            let _ = File::create(&key_path);
            eprintln!(
                "Error: No API key supplied. \
                 Edit {} that I created for you.",
                key_path.display()
            );
            std::process::exit(1);
        }
    };

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

    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let mut config = match fetch_config(&client, &args.api_url, &api_key) {
        Ok(fetched_config) => {
            println!("Configuration successfully synchronized!");
            fetched_config
        }
        Err(err) => {
            eprintln!("Error syncing with control plane: {err}");
            std::process::exit(1);
        }
    };

    if let Some(s) = args.source {
        config.settings.insert("source_folder".into(), s);
    }
    if let Some(p) = args.prefix {
        config.settings.insert("prefix_override".into(), p);
    }
    if let Some(t) = args.dry_run {
        config.settings.insert("trial_run".into(), t.to_string());
    }
    if let Some(d) = args.debug {
        config.settings.insert("debug".into(), d.to_string());
    }
    if let Some(r) = args.threads {
        config.settings.insert("threads".into(), r.to_string());
    }

    // Fetch and display user information from GET api/v1/account/me
    let user = fetch_user_me(&client, &args.api_url, &api_key).unwrap();
    println!(
        "Configuration fetched for id: {}, username: {}, role: {}",
        user.id, user.username, user.role
    );

    info!("Config loaded successfully. Starting run...");
    process_directory(config, None)?;
    info!("Run complete.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Config fetch
// ---------------------------------------------------------------------------

fn fetch_config(
    client: &Client,
    api_url: &str,
    api_key: &str,
) -> Result<ApiConfig, Box<dyn std::error::Error>> {
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    println!("Syncing configuration from control plane...");
    let response = client.get(api_url).headers(headers).send()?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Authentication failed: Invalid or missing API Key!".into());
    } else if !response.status().is_success() {
        return Err(format!("Server returned error status: {}", response.status()).into());
    }

    let config: ApiConfig = response.json()?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// Main processing engine
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn process_directory(cfg: ApiConfig, base_home: Option<&Path>) -> Result<()> {
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

    let resolve_home = || -> Result<PathBuf> {
        base_home.map_or_else(
            || dirs_next::home_dir().context("Failed to get home directory"),
            |h| Ok(h.to_path_buf()),
        )
    };

    let source_path = if Path::new(&source_name).is_absolute() {
        PathBuf::from(&source_name)
    } else {
        resolve_home()?.join(&source_name)
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

        let default_destination = ext.destination.unwrap_or(ext.default_destination);

        compiled_exts.insert(
            ext.name.to_lowercase(),
            CompiledExtension {
                default_destination,
                append_year_month: ext.append_year_month,
                rules: comp_rules,
            },
        );
    }

    info!("Scanning directory: {}", source_path.display());

    let entries = fs::read_dir(&source_path)
        .with_context(|| format!("Failed to read source directory: {}", source_path.display()))?;

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
            if debug {
                debug!("Skipping file matching global ignore rule: {filename}");
            }
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
                (&rule.destination, rule.append_year_month)
            },
        );

        if destination.is_empty() {
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
            if debug {
                debug!(
                    "File {} is already at its destination: {}",
                    filename,
                    dest_path.display()
                );
            }
            continue;
        }

        if dest_path.exists() {
            warn!("{} already exists, skipping!", dest_path.display());
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
                if debug {
                    debug!("Creating destination folder: {}", dest_dir.display());
                }
                fs::create_dir_all(&dest_dir).with_context(|| {
                    format!("Failed to create directory: {}", dest_dir.display())
                })?;
            }
        }

        let metadata = from
            .metadata()
            .with_context(|| format!("Failed to get metadata for: {}", from.display()))?;

        fromtos.push((from, dest_path, metadata));
    }

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
            info!(
                "Would move: {} -> {}",
                src.file_name().unwrap_or_default().to_string_lossy(),
                dst.display()
            );
        }
    } else {
        let _ = ThreadPoolBuilder::new().num_threads(threads).build_global();

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

fn fetch_user_me(
    client: &Client,
    api_url: &str,
    api_key: &str,
) -> Result<ApiUser, Box<dyn std::error::Error>> {
    let me_url = api_url.replace("/config", "/account/me");
    let mut headers = HeaderMap::new();
    let mut key_header_val = HeaderValue::from_str(api_key)?;
    key_header_val.set_sensitive(true);
    headers.insert("X-API-Key", key_header_val);

    let response = client.get(&me_url).headers(headers).send()?;
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
fn calculate_mbps(file_size_bytes: u64, duration: Duration) -> f64 {
    if duration.as_secs_f64() == 0.0 {
        return 0.0;
    }
    file_size_bytes as f64 / duration.as_secs_f64() / 1_000_000.0
}

// ---------------------------------------------------------------------------
// Automated Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Category 1: Unit Tests
    #[test]
    fn test_calculate_mbps_normal() {
        let bytes = 10_000_000;
        let duration = Duration::from_secs(2);
        let speed = calculate_mbps(bytes, duration);
        assert!((speed - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_mbps_zero_duration() {
        let bytes = 10_000_000;
        let duration = Duration::from_secs(0);
        let speed = calculate_mbps(bytes, duration);
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
            destination: None,
            default_destination: vec!["Downloads".to_string()],
            append_year_month: false,
            rules: vec![ApiRule {
                regex: r"^Scan ".to_string(),
                destination: vec!["Documents".to_string(), "Scanned".to_string()],
                order: 1,
                append_year_month: true, // <-- Rule level override enabled
            }],
        };

        let config = ApiConfig {
            settings,
            ignore_rules: vec![r"^\.".to_string()],
            delete_rules: vec!["sync-conflict".to_string()],
            extensions: vec![pdf_ext],
        };

        process_directory(config, None).unwrap();

        assert!(!delete_file.exists());
        assert!(ignore_file.exists());
        assert!(!sample_file.exists());

        // Calculate expected date folders based on sample file's mtime
        let metadata = fs::metadata(&sample_file).unwrap_or_else(|_| {
            // If file was moved, grab metadata from destination or current local time
            dest_prefix.join("Documents/Scanned").metadata().unwrap()
        });
        let mtime = FileTime::from_last_modification_time(&metadata);
        let dt: DateTime<Utc> =
            DateTime::from_timestamp(mtime.unix_seconds(), mtime.nanoseconds()).unwrap();
        let year = dt.year().to_string();
        let month = format!("{:02}", dt.month());

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
            destination: None,
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

        process_directory(config, None).unwrap();

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

    // Category 3: Network & Config Resolution Tests
    #[test]
    fn test_fetch_config_success() {
        let (url, _server_guard) = std::thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mock_server = MockServer::start().await;

                let response_json = r#"{
                    "settings": {"source_folder": "Downloads"},
                    "ignore_rules": [],
                    "delete_rules": [],
                    "extensions": []
                }"#;

                Mock::given(method("GET"))
                    .and(path("/api/v1/config"))
                    .and(header("X-API-Key", "valid-secret-key"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(response_json))
                    .mount(&mock_server)
                    .await;

                (format!("{}/api/v1/config", mock_server.uri()), mock_server)
            })
        })
        .join()
        .unwrap();

        let client = Client::new();
        let result = fetch_config(&client, &url, "valid-secret-key");

        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.settings.get("source_folder").unwrap(), "Downloads");
    }

    #[test]
    fn test_fetch_config_unauthorized() {
        let (url, _server_guard) = std::thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mock_server = MockServer::start().await;

                Mock::given(method("GET"))
                    .and(path("/api/v1/config"))
                    .respond_with(ResponseTemplate::new(401))
                    .mount(&mock_server)
                    .await;

                (format!("{}/api/v1/config", mock_server.uri()), mock_server)
            })
        })
        .join()
        .unwrap();

        let client = Client::new();
        let result = fetch_config(&client, &url, "invalid-key");

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("Authentication failed"));
    }
}
