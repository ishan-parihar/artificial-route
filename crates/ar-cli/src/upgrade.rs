//! Fetches and atomically installs the latest published aroute release.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "ishan-parihar/artificial-route";
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/ishan-parihar/artificial-route/releases/latest";
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: String,
}

/// Fetches the latest stable release and replaces the currently running file.
///
/// The process image is never restarted. A service manager must start a new
/// process before the daemon begins using the downloaded file.
pub(crate) async fn run() -> anyhow::Result<()> {
    let asset_name = release_asset_name()?;
    let current_version = env!("CARGO_PKG_VERSION");
    let current_key = parse_version(current_version)
        .with_context(|| format!("the installed version {current_version:?} is not semver"))?;

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("aroute/{current_version}"))
        .build()
        .context("could not initialize the HTTPS client")?;
    let release_response = client
        .get(LATEST_RELEASE_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("could not fetch the latest GitHub release")?;
    validate_api_url(release_response.url())?;
    let release = release_response
        .error_for_status()
        .context("GitHub rejected the latest-release request")?
        .json::<Release>()
        .await
        .context("GitHub returned an invalid latest-release response")?;

    ensure!(
        !release.draft && !release.prerelease,
        "GitHub's latest release is not a published stable release"
    );
    let latest_version = release
        .tag_name
        .strip_prefix('v')
        .with_context(|| format!("release tag {:?} must start with `v`", release.tag_name))?;
    let latest_key = parse_version(latest_version)
        .with_context(|| format!("release tag {:?} is not a stable version", release.tag_name))?;

    println!("aroute upgrade: current {current_version}; latest {latest_version}");
    if latest_key <= current_key {
        println!("aroute is already current; no binary was changed");
        return Ok(());
    }

    let asset = find_asset(&release, asset_name)
        .with_context(|| format!("release {:?} has no {asset_name} asset", release.tag_name))?;
    ensure!(
        asset.size > 0 && asset.size <= MAX_ASSET_BYTES,
        "release asset has an invalid size: {} bytes",
        asset.size
    );
    let expected_digest = parse_sha256_digest(&asset.digest)?;
    let asset_url = validate_asset_url(&asset.browser_download_url, &release.tag_name, asset_name)?;

    let executable =
        std::env::current_exe().context("could not locate the current aroute executable")?;
    let parent = executable
        .parent()
        .context("current executable path has no parent directory")?;
    let (candidate_path, mut candidate_file) = create_candidate(parent)?;
    let _cleanup = RemoveCandidate(candidate_path.clone());

    let mut response = client
        .get(asset_url)
        .send()
        .await
        .context("could not download the release binary")?
        .error_for_status()
        .context("GitHub rejected the release-asset download")?;
    validate_download_url(response.url())?;
    if let Some(content_length) = response.content_length() {
        ensure!(
            content_length == asset.size,
            "release asset length changed: metadata said {} bytes, download says {content_length}",
            asset.size
        );
    }

    let mut downloaded = 0_u64;
    let mut hasher = Sha256::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed while reading the release binary")?
    {
        downloaded = downloaded
            .checked_add(u64::try_from(chunk.len()).context("asset chunk size overflowed")?)
            .context("downloaded asset size overflowed")?;
        ensure!(
            downloaded <= asset.size && downloaded <= MAX_ASSET_BYTES,
            "release asset exceeded its declared or permitted size"
        );
        hasher.update(&chunk);
        candidate_file
            .write_all(&chunk)
            .context("could not write the temporary release binary")?;
    }
    ensure!(
        downloaded == asset.size,
        "release asset was truncated: expected {} bytes, received {downloaded}",
        asset.size
    );
    let actual_digest = hasher.finalize();
    ensure!(
        actual_digest.as_slice() == expected_digest,
        "release asset SHA-256 does not match GitHub's digest; the installed binary was left unchanged"
    );
    candidate_file
        .sync_all()
        .context("could not flush the temporary release binary")?;
    drop(candidate_file);

    let permissions = fs::metadata(&executable)
        .context("could not read the installed binary permissions")?
        .permissions();
    fs::set_permissions(&candidate_path, permissions)
        .context("could not preserve the installed binary permissions")?;
    verify_candidate_version(&candidate_path, latest_version)?;
    replace_executable(&candidate_path, &executable)?;

    println!(
        "updated aroute {current_version} -> {latest_version} at {}",
        executable.display()
    );
    println!("restart the aroute service separately to activate the new process");
    Ok(())
}

fn release_asset_name() -> anyhow::Result<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok("aroute-x86_64-unknown-linux-musl")
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        anyhow::bail!(
            "automatic upgrades are currently supported only on Linux x86_64; install another platform with install.sh"
        )
    }
}

fn parse_version(version: &str) -> anyhow::Result<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parse_version_part(
        parts
            .next()
            .context("version is missing its major number")?,
    )?;
    let minor = parse_version_part(
        parts
            .next()
            .context("version is missing its minor number")?,
    )?;
    let patch = parse_version_part(
        parts
            .next()
            .context("version is missing its patch number")?,
    )?;
    ensure!(
        parts.next().is_none(),
        "version must have exactly three numeric parts"
    );
    Ok((major, minor, patch))
}

fn parse_version_part(part: &str) -> anyhow::Result<u64> {
    ensure!(
        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()),
        "version components must contain only digits"
    );
    let number = part
        .parse::<u64>()
        .context("version component is outside the supported range")?;
    ensure!(
        part == number.to_string(),
        "version component has a leading zero"
    );
    Ok(number)
}

fn parse_sha256_digest(digest: &str) -> anyhow::Result<[u8; 32]> {
    let hex = digest
        .strip_prefix("sha256:")
        .context("GitHub release asset is missing a SHA-256 digest")?;
    ensure!(
        hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "GitHub returned a malformed SHA-256 digest"
    );
    let mut parsed = [0_u8; 32];
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    ensure!(remainder.is_empty(), "GitHub returned an incomplete digest");
    for (index, pair) in pairs.iter().enumerate() {
        let digits = std::str::from_utf8(pair).context("digest was not ASCII")?;
        parsed[index] = u8::from_str_radix(digits, 16).context("digest was not hexadecimal")?;
    }
    Ok(parsed)
}

fn find_asset<'a>(release: &'a Release, expected_name: &str) -> Option<&'a ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == expected_name)
}

fn validate_asset_url(raw_url: &str, tag: &str, asset_name: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw_url).context("GitHub returned an invalid release asset URL")?;
    let expected_path = format!("/{REPOSITORY}/releases/download/{tag}/{asset_name}");
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.path() == expected_path
            && url.query().is_none()
            && url.fragment().is_none(),
        "GitHub returned an untrusted release asset URL"
    );
    Ok(url)
}

fn validate_download_url(url: &Url) -> anyhow::Result<()> {
    let host = url.host_str().unwrap_or_default();
    ensure!(
        url.scheme() == "https"
            && (host == "github.com" || host.ends_with(".githubusercontent.com"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none(),
        "release asset redirected outside GitHub's HTTPS download hosts"
    );
    Ok(())
}

fn validate_api_url(url: &Url) -> anyhow::Result<()> {
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("api.github.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none(),
        "latest-release metadata redirected outside the GitHub API"
    );
    Ok(())
}

fn create_candidate(parent: &Path) -> anyhow::Result<(PathBuf, File)> {
    static NEXT_CANDIDATE: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        let sequence = NEXT_CANDIDATE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".aroute-upgrade-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "could not create a temporary executable beside {}",
                        parent.display()
                    )
                });
            }
        }
    }
    anyhow::bail!("could not allocate a unique temporary executable path")
}

fn verify_candidate_version(candidate: &Path, expected: &str) -> anyhow::Result<()> {
    let output = ProcessCommand::new(candidate)
        .arg("--version")
        .output()
        .context("downloaded release binary could not run --version")?;
    ensure!(
        output.status.success(),
        "downloaded release binary could not run --version (CPU feature or ABI mismatch)"
    );
    let reported = String::from_utf8_lossy(&output.stdout);
    ensure!(
        reported.trim() == format!("aroute {expected}"),
        "downloaded binary reports {:?}, but release tag says {expected}; installed binary was left unchanged",
        reported.trim()
    );
    Ok(())
}

fn replace_executable(candidate: &Path, executable: &Path) -> anyhow::Result<()> {
    ensure!(
        candidate.parent() == executable.parent(),
        "temporary release binary must be beside the installed executable"
    );
    let permissions = fs::metadata(executable)
        .context("could not read the installed binary permissions")?
        .permissions();
    fs::set_permissions(candidate, permissions)
        .context("could not preserve the installed binary permissions")?;
    fs::rename(candidate, executable).with_context(|| {
        format!(
            "could not atomically replace {}; run `aroute upgrade` with permission to write its directory",
            executable.display()
        )
    })?;
    if let Some(parent) = executable.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

struct RemoveCandidate(PathBuf);

impl Drop for RemoveCandidate {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Release, ReleaseAsset, find_asset, parse_sha256_digest, parse_version, replace_executable,
        validate_api_url, validate_asset_url, validate_download_url,
    };
    use reqwest::Url;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn release_tag_should_parse_a_stable_three_part_version() {
        assert_eq!(parse_version("0.1.24").expect("valid semver"), (0, 1, 24));
    }

    #[test]
    fn release_tag_should_reject_prerelease_and_non_numeric_versions() {
        assert!(parse_version("0.1.24-rc1").is_err() && parse_version("v0.1.x").is_err());
    }

    #[test]
    fn release_metadata_should_deserialize_the_required_asset_fields() {
        let json = r#"{
            "tag_name":"v0.1.24",
            "draft":false,
            "prerelease":false,
            "assets":[{
                "name":"aroute-x86_64-unknown-linux-musl",
                "browser_download_url":"https://github.com/ishan-parihar/artificial-route/releases/download/v0.1.24/aroute-x86_64-unknown-linux-musl",
                "size":18685680,
                "digest":"sha256:f91cb428fed90a4b047dd4beb93dbc7f82bc7ba2c894ea9286d79ec6882baeed"
            }]
        }"#;
        let release: Release = serde_json::from_str(json).expect("release fixture parses");
        assert_eq!(
            (
                release.tag_name.as_str(),
                release.draft,
                release.prerelease,
                release.assets[0].size
            ),
            ("v0.1.24", false, false, 18_685_680)
        );
    }

    #[test]
    fn release_asset_selection_should_match_the_published_binary_name() {
        let release = Release {
            tag_name: "v0.1.24".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![ReleaseAsset {
                name: "SHA256SUMS".to_owned(),
                browser_download_url: "https://github.com/ishan-parihar/artificial-route/releases/download/v0.1.24/aroute-x86_64-unknown-linux-musl".to_owned(),
                size: 1,
                digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            }, ReleaseAsset {
                name: "aroute-x86_64-unknown-linux-musl".to_owned(),
                browser_download_url: "https://github.com/ishan-parihar/artificial-route/releases/download/v0.1.24/aroute-x86_64-unknown-linux-musl".to_owned(),
                size: 1,
                digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            }],
        };
        assert_eq!(
            find_asset(&release, "aroute-x86_64-unknown-linux-musl")
                .expect("release has the expected binary")
                .name,
            "aroute-x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn release_asset_url_should_reject_untrusted_hosts_and_paths() {
        let trusted = validate_asset_url(
            "https://github.com/ishan-parihar/artificial-route/releases/download/v0.1.24/aroute-x86_64-unknown-linux-musl",
            "v0.1.24",
            "aroute-x86_64-unknown-linux-musl",
        );
        let untrusted = validate_asset_url(
            "https://attacker.example/ishan-parihar/artificial-route/releases/download/v0.1.24/aroute-x86_64-unknown-linux-musl",
            "v0.1.24",
            "aroute-x86_64-unknown-linux-musl",
        );
        assert!(trusted.is_ok() && untrusted.is_err());
    }

    #[test]
    fn release_redirect_should_allow_only_https_github_download_hosts() {
        let trusted =
            Url::parse("https://release-assets.githubusercontent.com/asset").expect("URL");
        let untrusted = Url::parse("https://downloads.example/asset").expect("URL");
        let untrusted_port =
            Url::parse("https://release-assets.githubusercontent.com:444/asset").expect("URL");
        assert!(
            validate_download_url(&trusted).is_ok()
                && validate_download_url(&untrusted).is_err()
                && validate_download_url(&untrusted_port).is_err()
        );
    }

    #[test]
    fn release_metadata_should_stay_on_the_github_api_origin() {
        let trusted = Url::parse(
            "https://api.github.com/repos/ishan-parihar/artificial-route/releases/latest",
        )
        .expect("URL");
        let untrusted = Url::parse("https://attacker.example/releases/latest").expect("URL");
        assert!(validate_api_url(&trusted).is_ok() && validate_api_url(&untrusted).is_err());
    }

    #[test]
    fn release_digest_should_match_the_sha256_bytes() {
        let expected = parse_sha256_digest(
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .expect("valid digest");
        let actual = Sha256::digest(b"abc");
        assert_eq!(actual.as_slice(), expected);
    }

    #[test]
    fn release_digest_should_reject_missing_or_malformed_hashes() {
        assert!(
            parse_sha256_digest(
                "sha1:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            )
            .is_err()
                && parse_sha256_digest("sha256:bad").is_err()
        );
    }

    #[test]
    fn atomic_replacement_should_preserve_the_installed_mode() {
        let directory = test_directory();
        let executable = directory.join("aroute");
        let candidate = directory.join(".candidate");
        fs::write(&executable, b"old").expect("old executable writes");
        fs::write(&candidate, b"new").expect("candidate writes");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o751)).expect("mode sets");
        replace_executable(&candidate, &executable).expect("atomic replacement succeeds");
        let actual = fs::metadata(&executable).expect("executable remains");
        assert_eq!(
            (
                fs::read(&executable).expect("new bytes read"),
                actual.permissions().mode() & 0o777
            ),
            (b"new".to_vec(), 0o751)
        );
        fs::remove_dir_all(directory).expect("test directory removes");
    }

    fn test_directory() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aroute-upgrade-test-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("test directory creates");
        path
    }
}
