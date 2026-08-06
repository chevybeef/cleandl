#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use super::*;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn request_settings(
    api_url: String,
    api_key: &'static str,
) -> Result<HashMap<String, String>> {
    tokio::task::spawn_blocking(move || {
        let client = Client::builder().timeout(Duration::from_secs(2)).build()?;
        load_settings(&client, &api_url, api_key)
    })
    .await
    .context("Settings request task failed")?
}

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
fn test_calculate_mibps_large_file() {
    let bytes = 8_589_934_592; // 8 GiB
    let speed = calculate_mibps(bytes, Duration::from_secs(2));

    assert!((speed - 4096.0).abs() < f64::EPSILON);
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
fn test_cli_boolean_flags_accept_bare_and_explicit_values() {
    let bare = Cli::try_parse_from(["cleandl", "--dry-run", "--debug"]).unwrap();
    assert_eq!(bare.dry_run, Some(true));
    assert_eq!(bare.debug, Some(true));

    let explicit = Cli::try_parse_from(["cleandl", "--dry-run=false", "--debug=false"]).unwrap();
    assert_eq!(explicit.dry_run, Some(false));
    assert_eq!(explicit.debug, Some(false));
}

#[test]
fn test_cli_version_includes_git_revision() {
    let output = Cli::try_parse_from(["cleandl", "--version"])
        .unwrap_err()
        .to_string();
    assert_eq!(output, format!("cleandl {}\n", cli_version()));
}

#[test]
fn test_normalize_api_url_requires_api_v1_root() {
    assert_eq!(
        normalize_api_url(" https://example.test/api/v1/ ").unwrap(),
        "https://example.test/api/v1"
    );
    assert!(normalize_api_url("https://example.test/config").is_err());
    assert!(normalize_api_url("https://example.test").is_err());
}

#[cfg(unix)]
#[test]
fn test_new_api_key_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp_dir = TempDir::new().unwrap();
    let error = load_api_key(None, temp_dir.path()).unwrap_err();
    let key_path = temp_dir.path().join("cleandl.key");
    let mode = fs::metadata(key_path).unwrap().permissions().mode() & 0o777;

    assert!(error.to_string().contains("created empty file"));
    assert_eq!(mode, 0o600);
}

#[test]
fn test_legacy_state_directory_is_migrated() {
    let temp_dir = TempDir::new().unwrap();
    let legacy_dir = temp_dir.path().join("cleandl-rs");
    let current_dir = temp_dir.path().join("cleandl");
    fs::create_dir(&legacy_dir).unwrap();
    fs::write(legacy_dir.join("cleandl.key"), "test-key").unwrap();

    migrate_legacy_state_dir(&legacy_dir, &current_dir).unwrap();

    assert!(!legacy_dir.exists());
    assert_eq!(
        fs::read_to_string(current_dir.join("cleandl.key")).unwrap(),
        "test-key"
    );
}

#[test]
fn test_resolve_settings_applies_defaults() {
    let prefix_key = if cfg!(target_os = "windows") {
        "windows_prefix"
    } else if cfg!(target_os = "macos") {
        "macos_prefix"
    } else {
        "unix_prefix"
    };
    let map = HashMap::from([(prefix_key.to_string(), "/api-prefix".to_string())]);
    let args = Cli::try_parse_from(["cleandl"]).unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&map, &args, &debug_flag);

    assert_eq!(settings["source_folder"], "Downloads");
    assert_eq!(settings["prefix_override"], "/api-prefix");
    assert_eq!(settings["trial_run"], "false");
    assert_eq!(settings["debug"], "false");
    assert_eq!(settings["threads"], "6");
    assert!(!settings.contains_key("sort_by_size"));
    assert!(!debug_flag.load(Ordering::Relaxed));
}

#[test]
fn test_resolve_settings_cli_overrides_api_values() {
    let map = HashMap::from([
        ("source_folder".to_string(), "ApiDownloads".to_string()),
        ("trial_run".to_string(), "true".to_string()),
        ("debug".to_string(), "true".to_string()),
        ("threads".to_string(), "4".to_string()),
        ("sort_by_size".to_string(), "Descending".to_string()),
    ]);
    let args = Cli::try_parse_from([
        "cleandl",
        "--source=CliDownloads",
        "--prefix=/cli-prefix",
        "--dry-run=false",
        "--debug=false",
        "--threads=12",
        "--trace",
    ])
    .unwrap();
    let debug_flag = AtomicBool::new(false);

    let settings = resolve_settings(&map, &args, &debug_flag);

    assert_eq!(settings["source_folder"], "CliDownloads");
    assert_eq!(settings["prefix_override"], "/cli-prefix");
    assert_eq!(settings["trial_run"], "false");
    assert_eq!(settings["debug"], "false");
    assert_eq!(settings["threads"], "12");
    assert_eq!(settings["sort_by_size"], "Descending");
    assert!(debug_flag.load(Ordering::Relaxed));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_settings_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/settings"))
        .and(header("X-API-Key", "secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"key": "source_folder", "value": "Downloads"},
            {"key": "threads", "value": "8"}
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let settings = request_settings(server.uri(), "secret-key").await.unwrap();

    assert_eq!(settings["source_folder"], "Downloads");
    assert_eq!(settings["threads"], "8");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_settings_rejects_unauthorized_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/settings"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let error = request_settings(server.uri(), "invalid-key")
        .await
        .expect_err("an unauthorized response should fail");

    assert!(error.to_string().contains("Authentication failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_load_settings_rejects_unexpected_json_shape() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"source_folder": "Downloads"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let error = request_settings(server.uri(), "secret-key")
        .await
        .expect_err("a non-array settings response should fail");

    assert!(error.to_string().contains("expected a JSON array"));
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

    let entries: Vec<std::io::Result<fs::DirEntry>> = fs::read_dir(&source_path).unwrap().collect();
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

    let entries: Vec<std::io::Result<fs::DirEntry>> = fs::read_dir(&source_path).unwrap().collect();
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
