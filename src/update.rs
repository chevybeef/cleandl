use anyhow::{Context, Result, bail};
use chrono::Utc;
use flate2::read::GzDecoder;
use reqwest::blocking::{Client, Response};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Component, Path};
use std::time::Duration;
use tempfile::TempDir;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/chevybeef/cleandl/releases/latest";
const UPDATE_CACHE_FILE: &str = "cleandl.update-check.json";
const UPDATE_CHECK_INTERVAL_SECONDS: i64 = 24 * 60 * 60;
const MAX_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 4 * 1024;

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Debug)]
pub struct UpdateInfo {
    current_version: Version,
    latest_version: Version,
    release_url: String,
    archive: GitHubAsset,
    checksum: GitHubAsset,
}

#[derive(Debug)]
enum UpdateStatus {
    Current { version: Version },
    Available(Box<UpdateInfo>),
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdateCheckCache {
    checked_at: i64,
}

pub fn run_check_command() -> Result<()> {
    let client = update_client()?;
    let status = check_for_update(&client, LATEST_RELEASE_API, super::PKG_VERSION)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    report_status(&status, &mut output)
}

pub fn run_update_command() -> Result<()> {
    if cfg!(debug_assertions) {
        bail!("self-update is disabled in development builds; use a release build");
    }

    let client = update_client()?;
    let status = check_for_update(&client, LATEST_RELEASE_API, super::PKG_VERSION)?;
    let UpdateStatus::Available(update) = status else {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        return report_status(&status, &mut output);
    };

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stderr = io::stderr();
    let mut output = stderr.lock();
    if !confirm_update(&update, &mut input, &mut output)? {
        writeln!(output, "Update cancelled.")?;
        return Ok(());
    }

    install_update(&client, &update, &mut output)
}

/// Checks at most once per day on ordinary interactive release-build runs.
/// Returns `true` when an update was installed and the caller should exit.
pub fn maybe_offer_automatic_update(log_dir: &Path, quiet: bool) -> Result<bool> {
    if quiet || cfg!(debug_assertions) || !io::stdin().is_terminal() || !io::stderr().is_terminal()
    {
        return Ok(false);
    }

    let cache_path = log_dir.join(UPDATE_CACHE_FILE);
    let now = Utc::now().timestamp();
    if checked_recently(&cache_path, now) {
        return Ok(false);
    }
    save_update_check_time(&cache_path, now)?;

    let client = update_client()?;
    let status = check_for_update(&client, LATEST_RELEASE_API, super::PKG_VERSION)?;

    let UpdateStatus::Available(update) = status else {
        return Ok(false);
    };

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stderr = io::stderr();
    let mut output = stderr.lock();
    if !confirm_update(&update, &mut input, &mut output)? {
        writeln!(output, "Continuing without updating.")?;
        return Ok(false);
    }

    install_update(&client, &update, &mut output)?;
    Ok(true)
}

fn update_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("cleandl/{}", super::PKG_VERSION))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to create the update client")
}

fn check_for_update(client: &Client, endpoint: &str, current: &str) -> Result<UpdateStatus> {
    let current_version = Version::parse(current).context("Invalid installed version")?;
    let response = client
        .get(endpoint)
        .send()
        .context("Failed to check GitHub for updates")?
        .error_for_status()
        .context("GitHub returned an error while checking for updates")?;
    let release: GitHubRelease = response
        .json()
        .context("Failed to decode the latest GitHub release")?;

    if release.draft || release.prerelease {
        bail!("GitHub returned a draft or prerelease as the latest stable release");
    }

    let latest_version = parse_release_version(&release.tag_name)?;
    if latest_version <= current_version {
        return Ok(UpdateStatus::Current {
            version: current_version,
        });
    }

    let asset_name = platform_asset_name(std::env::consts::OS, std::env::consts::ARCH)?;
    let checksum_name = format!("{asset_name}.sha256");
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .cloned()
        .with_context(|| {
            format!(
                "Release {} has no asset for this platform",
                release.tag_name
            )
        })?;
    let checksum = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .cloned()
        .with_context(|| {
            format!(
                "Release {} has no checksum for this platform",
                release.tag_name
            )
        })?;

    Ok(UpdateStatus::Available(Box::new(UpdateInfo {
        current_version,
        latest_version,
        release_url: release.html_url,
        archive,
        checksum,
    })))
}

fn parse_release_version(tag: &str) -> Result<Version> {
    let raw = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(raw).with_context(|| format!("Invalid release tag: {tag}"))
}

fn platform_asset_name(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("cleandl-aarch64-apple-darwin.tar.gz"),
        ("macos", "x86_64") => Ok("cleandl-x86_64-apple-darwin.tar.gz"),
        ("linux", "aarch64") => Ok("cleandl-aarch64-unknown-linux-gnu.tar.gz"),
        ("linux", "x86_64") => Ok("cleandl-x86_64-unknown-linux-gnu.tar.gz"),
        ("windows", "x86_64") => Ok("cleandl-x86_64-pc-windows-msvc.zip"),
        _ => bail!("Automatic updates are not available for {os}/{arch}"),
    }
}

fn report_status(status: &UpdateStatus, output: &mut impl Write) -> Result<()> {
    match status {
        UpdateStatus::Current { version } => {
            writeln!(output, "CleanDL {version} is up to date.")?;
        }
        UpdateStatus::Available(update) => {
            writeln!(
                output,
                "CleanDL {} is available (currently {}).",
                update.latest_version, update.current_version
            )?;
            writeln!(output, "Release: {}", update.release_url)?;
        }
    }
    Ok(())
}

fn confirm_update(
    update: &UpdateInfo,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<bool> {
    writeln!(
        output,
        "CleanDL {} is available (currently {}).",
        update.latest_version, update.current_version
    )?;
    writeln!(output, "Release: {}", update.release_url)?;
    writeln!(output, "Download: {}", update.archive.name)?;
    write!(output, "Update now? [y/N] ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn install_update(client: &Client, update: &UpdateInfo, output: &mut impl Write) -> Result<()> {
    writeln!(output, "Downloading {}...", update.archive.name)?;
    let temp_dir = TempDir::new().context("Failed to create a temporary update directory")?;
    let archive_path = temp_dir.path().join(&update.archive.name);
    let checksum_path = temp_dir.path().join(&update.checksum.name);

    download_to(
        client.get(&update.checksum.browser_download_url).send(),
        &checksum_path,
        MAX_CHECKSUM_BYTES,
        "checksum",
    )?;
    download_to(
        client.get(&update.archive.browser_download_url).send(),
        &archive_path,
        MAX_ARCHIVE_BYTES,
        "release archive",
    )?;

    verify_archive(&archive_path, &checksum_path, &update.archive)?;
    writeln!(output, "Checksum verified.")?;

    let staged_binary = temp_dir.path().join(binary_name());
    extract_binary(&archive_path, &staged_binary)?;
    if let Some(warning) = replace_executable(&staged_binary)? {
        writeln!(output, "Warning: {warning}")?;
    }
    writeln!(
        output,
        "Updated CleanDL to {}. Run CleanDL again to continue.",
        update.latest_version
    )?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_executable(new_binary: &Path) -> Result<Option<String>> {
    self_replace::self_replace(new_binary).context("Failed to replace the current executable")?;
    Ok(None)
}

#[cfg(windows)]
fn replace_executable(new_binary: &Path) -> Result<Option<String>> {
    let executable = std::env::current_exe()
        .context("Failed to locate the current executable")?
        .canonicalize()
        .context("Failed to resolve the current executable")?;
    let parent = executable
        .parent()
        .context("The current executable has no parent directory")?;
    let old_permissions = executable
        .metadata()
        .context("Failed to read executable permissions")?
        .permissions();

    let staged = tempfile::Builder::new()
        .prefix(".cleandl-update-")
        .suffix(".exe")
        .tempfile_in(parent)
        .context("The CleanDL installation folder is not writable")?;
    fs::copy(new_binary, staged.path()).context("Failed to stage the new executable")?;
    fs::set_permissions(staged.path(), old_permissions)
        .context("Failed to prepare the new executable")?;
    staged
        .as_file()
        .sync_all()
        .context("Failed to save the new executable")?;
    let (_, staged_path) = staged
        .keep()
        .context("Failed to preserve the new executable")?;

    let backup = parent.join(format!(".cleandl-{}-backup.exe", std::process::id()));
    if backup.exists() {
        fs::remove_file(&staged_path).ok();
        bail!("Cannot create a temporary backup at {}", backup.display());
    }
    swap_files_with_rollback(&staged_path, &executable, &backup)?;

    let warning = self_replace::self_delete_at(&backup).err().map(|error| {
        format!(
            "the old executable could not be scheduled for cleanup ({error}); a temporary backup may remain"
        )
    });
    Ok(warning)
}

#[cfg(any(windows, test))]
fn swap_files_with_rollback(staged: &Path, current: &Path, backup: &Path) -> Result<()> {
    fs::rename(current, backup).context("Failed to create an executable backup")?;
    if let Err(install_error) = fs::rename(staged, current) {
        if let Err(rollback_error) = fs::rename(backup, current) {
            bail!(
                "Failed to install the update ({install_error}) and failed to restore the previous executable ({rollback_error})"
            );
        }
        return Err(install_error)
            .context("Failed to install the update; the previous executable was restored");
    }
    Ok(())
}

fn download_to(
    response: reqwest::Result<Response>,
    destination: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<()> {
    let response = response
        .with_context(|| format!("Failed to download the {description}"))?
        .error_for_status()
        .with_context(|| format!("GitHub returned an error downloading the {description}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes)
    {
        bail!("The {description} is larger than the allowed limit");
    }

    let mut output = File::create(destination)
        .with_context(|| format!("Failed to create {}", destination.display()))?;
    let mut limited = response.take(maximum_bytes.saturating_add(1));
    let copied = io::copy(&mut limited, &mut output)
        .with_context(|| format!("Failed to save the {description}"))?;
    if copied > maximum_bytes {
        bail!("The {description} is larger than the allowed limit");
    }
    output.sync_all().context("Failed to save update data")?;
    Ok(())
}

fn verify_archive(archive_path: &Path, checksum_path: &Path, asset: &GitHubAsset) -> Result<()> {
    let checksum_text = fs::read_to_string(checksum_path).context("Failed to read the checksum")?;
    let mut fields = checksum_text.split_whitespace();
    let published = fields.next().context("The checksum file is empty")?;
    let published_name = fields
        .next()
        .context("The checksum file does not name the release archive")?
        .trim_start_matches('*');
    if fields.next().is_some() || published_name != asset.name {
        bail!("The checksum file does not match {}", asset.name);
    }
    validate_sha256(published)?;

    let api_digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("GitHub did not provide a SHA-256 digest for the release archive")?;
    validate_sha256(api_digest)?;
    if !published.eq_ignore_ascii_case(api_digest) {
        bail!("The release checksum disagrees with GitHub's asset digest");
    }

    let actual = sha256_file(archive_path)?;
    if !actual.eq_ignore_ascii_case(published) {
        bail!("The downloaded release archive failed SHA-256 verification");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Invalid SHA-256 digest in release metadata");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("Failed to open the release archive")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("Failed to verify the release archive")?;
        if read == 0 {
            break;
        }
        digest.update(
            buffer
                .get(..read)
                .context("Invalid checksum buffer length")?,
        );
    }
    Ok(format!("{:x}", digest.finalize()))
}

const fn binary_name() -> &'static str {
    if cfg!(windows) {
        "cleandl.exe"
    } else {
        "cleandl"
    }
}

fn extract_binary(archive_path: &Path, destination: &Path) -> Result<()> {
    let name = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid release archive name")?;
    let lowercase_name = name.to_ascii_lowercase();
    if archive_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        extract_from_zip(archive_path, destination)
    } else if lowercase_name.ends_with(".tar.gz") {
        extract_from_tar_gz(archive_path, destination)
    } else {
        bail!("Unsupported release archive format");
    }
}

fn extract_from_tar_gz(archive_path: &Path, destination: &Path) -> Result<()> {
    let archive_file = File::open(archive_path).context("Failed to open the release archive")?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(decoder);
    let mut found = false;

    for entry in archive
        .entries()
        .context("Failed to read the release archive")?
    {
        let mut entry = entry.context("Failed to read an archive entry")?;
        let path = entry.path().context("Invalid path in release archive")?;
        validate_archive_path(&path)?;
        if entry.header().entry_type().is_file()
            && path.file_name().is_some_and(|name| name == binary_name())
        {
            if found {
                bail!("The release archive contains more than one CleanDL binary");
            }
            let mut output = File::create(destination).context("Failed to stage the new binary")?;
            io::copy(&mut entry, &mut output).context("Failed to extract the new binary")?;
            output.sync_all().context("Failed to save the new binary")?;
            found = true;
        }
    }

    if !found {
        bail!("The release archive does not contain {}", binary_name());
    }
    Ok(())
}

fn extract_from_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let archive_file = File::open(archive_path).context("Failed to open the release archive")?;
    let mut archive =
        zip::ZipArchive::new(archive_file).context("Failed to read the ZIP archive")?;
    let mut found = false;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("Failed to read a ZIP archive entry")?;
        let path = entry
            .enclosed_name()
            .context("Unsafe path in ZIP release archive")?;
        validate_archive_path(&path)?;
        if !entry.is_dir() && path.file_name().is_some_and(|name| name == binary_name()) {
            if found {
                bail!("The release archive contains more than one CleanDL binary");
            }
            let mut output = File::create(destination).context("Failed to stage the new binary")?;
            io::copy(&mut entry, &mut output).context("Failed to extract the new binary")?;
            output.sync_all().context("Failed to save the new binary")?;
            found = true;
        }
    }

    if !found {
        bail!("The release archive does not contain {}", binary_name());
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("Unsafe path in release archive: {}", path.display());
    }
    Ok(())
}

fn checked_recently(path: &Path, now: i64) -> bool {
    let Ok(raw) = fs::read(path) else {
        return false;
    };
    let Ok(cache) = serde_json::from_slice::<UpdateCheckCache>(&raw) else {
        return false;
    };
    cache.checked_at <= now && now.saturating_sub(cache.checked_at) < UPDATE_CHECK_INTERVAL_SECONDS
}

fn save_update_check_time(path: &Path, now: i64) -> Result<()> {
    let cache = UpdateCheckCache { checked_at: now };
    let encoded = serde_json::to_vec_pretty(&cache).context("Failed to encode update cache")?;
    fs::write(path, encoded).context("Failed to save update check time")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use flate2::Compression;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn release_versions_allow_v_prefix_and_compare_semantically() {
        assert_eq!(
            parse_release_version("v1.10.0").unwrap(),
            Version::new(1, 10, 0)
        );
        assert!(parse_release_version("release-one").is_err());
    }

    #[test]
    fn platform_assets_match_cargo_dist_names() {
        assert_eq!(
            platform_asset_name("macos", "aarch64").unwrap(),
            "cleandl-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            platform_asset_name("windows", "x86_64").unwrap(),
            "cleandl-x86_64-pc-windows-msvc.zip"
        );
        assert!(platform_asset_name("freebsd", "x86_64").is_err());
    }

    #[test]
    fn approval_requires_explicit_yes() {
        let update = sample_update();
        for answer in ["y\n", "YES\n"] {
            let mut input = answer.as_bytes();
            let mut output = Vec::new();
            assert!(confirm_update(&update, &mut input, &mut output).unwrap());
        }
        for answer in ["\n", "no\n", "anything\n"] {
            let mut input = answer.as_bytes();
            let mut output = Vec::new();
            assert!(!confirm_update(&update, &mut input, &mut output).unwrap());
        }
    }

    #[test]
    fn checksum_requires_api_and_sidecar_to_agree() {
        let directory = TempDir::new().unwrap();
        let archive_path = directory.path().join("cleandl-test.tar.gz");
        fs::write(&archive_path, b"archive bytes").unwrap();
        let digest = sha256_file(&archive_path).unwrap();
        let checksum_path = directory.path().join("checksum");
        fs::write(&checksum_path, format!("{digest}  cleandl-test.tar.gz\n")).unwrap();
        let asset = GitHubAsset {
            name: "cleandl-test.tar.gz".to_owned(),
            browser_download_url: String::new(),
            digest: Some(format!("sha256:{digest}")),
        };

        verify_archive(&archive_path, &checksum_path, &asset).unwrap();
        fs::write(&archive_path, b"tampered").unwrap();
        assert!(verify_archive(&archive_path, &checksum_path, &asset).is_err());
    }

    #[test]
    fn cached_checks_expire_after_one_day() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(UPDATE_CACHE_FILE);
        let cache = UpdateCheckCache { checked_at: 1_000 };
        fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();

        assert!(checked_recently(&path, 1_001));
        assert!(!checked_recently(&path, 999));
        assert!(!checked_recently(
            &path,
            1_000 + UPDATE_CHECK_INTERVAL_SECONDS
        ));
    }

    #[tokio::test]
    async fn latest_release_check_selects_the_host_asset() -> Result<()> {
        let server = MockServer::start().await;
        let asset_name = platform_asset_name(std::env::consts::OS, std::env::consts::ARCH)
            .unwrap()
            .to_owned();
        Mock::given(method("GET"))
            .and(path("/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tag_name": "v9.1.0",
                "html_url": "https://example.test/releases/v9.1.0",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {
                        "name": asset_name,
                        "browser_download_url": "https://example.test/archive",
                        "digest": format!("sha256:{}", "a".repeat(64))
                    },
                    {
                        "name": format!("{asset_name}.sha256"),
                        "browser_download_url": "https://example.test/checksum",
                        "digest": format!("sha256:{}", "b".repeat(64))
                    }
                ]
            })))
            .mount(&server)
            .await;

        let endpoint = format!("{}/latest", server.uri());
        let status = tokio::task::spawn_blocking(move || {
            check_for_update(&update_client().unwrap(), &endpoint, "0.3.1")
        })
        .await
        .unwrap()
        .unwrap();

        let UpdateStatus::Available(update) = status else {
            bail!("expected an available update");
        };
        if update.latest_version != Version::new(9, 1, 0) {
            bail!("unexpected latest version: {}", update.latest_version);
        }
        if update.archive.name != asset_name {
            bail!("unexpected archive: {}", update.archive.name);
        }
        Ok(())
    }

    #[test]
    fn tar_and_zip_extract_only_the_expected_binary() {
        let directory = TempDir::new().unwrap();
        let tar_path = directory.path().join("release.tar.gz");
        let tar_file = File::create(&tar_path).unwrap();
        let encoder = flate2::write::GzEncoder::new(tar_file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let payload = b"new tar binary";
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(payload.len()).unwrap());
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("release/{}", binary_name()),
                payload.as_slice(),
            )
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let tar_output = directory.path().join("from-tar");
        extract_binary(&tar_path, &tar_output).unwrap();
        assert_eq!(fs::read(tar_output).unwrap(), payload);

        let zip_path = directory.path().join("release.zip");
        let zip_file = File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer
            .start_file(format!("release/{}", binary_name()), options)
            .unwrap();
        writer.write_all(b"new zip binary").unwrap();
        writer.finish().unwrap();
        let zip_output = directory.path().join("from-zip");
        extract_binary(&zip_path, &zip_output).unwrap();
        assert_eq!(fs::read(zip_output).unwrap(), b"new zip binary");
    }

    #[test]
    fn archive_paths_must_be_relative_and_normal() {
        assert!(validate_archive_path(Path::new("release/cleandl")).is_ok());
        assert!(validate_archive_path(Path::new("../cleandl")).is_err());
        assert!(validate_archive_path(Path::new("/cleandl")).is_err());
    }

    #[test]
    fn failed_final_swap_restores_the_previous_executable() {
        let directory = TempDir::new().unwrap();
        let current = directory.path().join("cleandl.exe");
        let missing_staged = directory.path().join("missing-update.exe");
        let backup = directory.path().join("backup.exe");
        fs::write(&current, b"previous binary").unwrap();

        let error = swap_files_with_rollback(&missing_staged, &current, &backup).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("previous executable was restored")
        );
        assert_eq!(fs::read(&current).unwrap(), b"previous binary");
        assert!(!backup.exists());
    }

    fn sample_update() -> UpdateInfo {
        UpdateInfo {
            current_version: Version::new(0, 3, 1),
            latest_version: Version::new(0, 4, 0),
            release_url: "https://example.test/release".to_owned(),
            archive: GitHubAsset {
                name: "cleandl-test.tar.gz".to_owned(),
                browser_download_url: String::new(),
                digest: None,
            },
            checksum: GitHubAsset {
                name: "cleandl-test.tar.gz.sha256".to_owned(),
                browser_download_url: String::new(),
                digest: None,
            },
        }
    }
}
