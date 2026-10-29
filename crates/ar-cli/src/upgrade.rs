//! Fetches and atomically installs the latest published aroute release.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::Url;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "ishan-parihar/artificial-route";
const LATEST_RELEASE_URL: &str =
    "https://github.com/ishan-parihar/artificial-route/releases/latest";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const MAX_CHECKSUMS_BYTES: usize = 16 * 1024;
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

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
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if is_github_download_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .context("could not initialize the HTTPS client")?;
    let latest_response = client
        .get(LATEST_RELEASE_URL)
        .send()
        .await
        .context("could not fetch the latest GitHub release")?;
    let latest_url = latest_response.url().clone();
    let latest_tag = release_tag_from_url(&latest_url)?.to_owned();
    latest_response
        .error_for_status()
        .context("GitHub rejected the latest-release request")?;
    let latest_version = latest_tag
        .strip_prefix('v')
        .context("latest release tag must start with `v`")?;
    let latest_key = parse_version(latest_version)
        .with_context(|| format!("latest release tag {latest_tag:?} is not a stable version"))?;

    println!("aroute upgrade: current {current_version}; latest {latest_version}");
    if latest_key <= current_key {
        println!("aroute is already current; no binary was changed");
        return Ok(());
    }

    let checksums_url = release_asset_url(&latest_tag, CHECKSUMS_ASSET)?;
    let mut checksums_response = client
        .get(checksums_url)
        .send()
        .await
        .context("could not download the release checksum manifest")?
        .error_for_status()
        .context("GitHub rejected the release checksum-manifest download")?;
    validate_download_url(checksums_response.url())?;
    if let Some(content_length) = checksums_response.content_length() {
        ensure!(
            content_length <= MAX_CHECKSUMS_BYTES as u64,
            "release checksum manifest is too large"
        );
    }
    let mut checksum_bytes = Vec::with_capacity(MAX_CHECKSUMS_BYTES);
    while let Some(chunk) = checksums_response
        .chunk()
        .await
        .context("failed while reading the release checksum manifest")?
    {
        ensure!(
            checksum_bytes.len() + chunk.len() <= MAX_CHECKSUMS_BYTES,
            "release checksum manifest exceeded its permitted size"
        );
        checksum_bytes.extend_from_slice(&chunk);
    }
    let checksums =
        std::str::from_utf8(&checksum_bytes).context("release checksum manifest was not UTF-8")?;
    let expected_digest = parse_sha256sum(checksums, asset_name)?;
    let asset_url = release_asset_url(&latest_tag, asset_name)?;

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
            content_length <= MAX_ASSET_BYTES,
            "release asset exceeds the permitted size"
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
            downloaded <= MAX_ASSET_BYTES,
            "release asset exceeded its permitted size"
        );
        hasher.update(&chunk);
        candidate_file
            .write_all(&chunk)
            .context("could not write the temporary release binary")?;
    }
    ensure!(downloaded > 0, "release asset was empty");
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

fn parse_sha256_digest(hex: &str) -> anyhow::Result<[u8; 32]> {
    ensure!(
        hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "release checksum manifest contains a malformed SHA-256 digest"
    );
    let mut parsed = [0_u8; 32];
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    ensure!(
        remainder.is_empty(),
        "release checksum digest is incomplete"
    );
    for (index, pair) in pairs.iter().enumerate() {
        let digits = std::str::from_utf8(pair).context("checksum digest was not ASCII")?;
        parsed[index] = u8::from_str_radix(digits, 16).context("digest was not hexadecimal")?;
    }
    Ok(parsed)
}

fn parse_sha256sum(manifest: &str, expected_name: &str) -> anyhow::Result<[u8; 32]> {
    let mut digest = None;
    for line in manifest.lines() {
        let (hex, name) = line
            .split_once("  ")
            .context("release checksum manifest contains a malformed row")?;
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == expected_name {
            ensure!(
                digest.is_none(),
                "release checksum manifest repeats {expected_name}"
            );
            digest = Some(parse_sha256_digest(hex)?);
        }
    }
    digest.with_context(|| format!("release checksum manifest has no {expected_name} entry"))
}

fn release_tag_from_url(url: &Url) -> anyhow::Result<&str> {
    ensure!(
        is_github_origin(url),
        "latest-release redirect left github.com"
    );
    let prefix = format!("/{REPOSITORY}/releases/tag/");
    let tag = url
        .path()
        .strip_prefix(&prefix)
        .context("latest-release redirect did not point to a release tag")?;
    ensure!(
        !tag.is_empty() && !tag.contains('/') && url.query().is_none() && url.fragment().is_none(),
        "latest-release redirect returned an invalid tag path"
    );
    Ok(tag)
}

fn release_asset_url(tag: &str, asset_name: &str) -> anyhow::Result<Url> {
    ensure!(
        tag.starts_with('v')
            && tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-'),
        "release tag contains invalid path characters"
    );
    ensure!(
        asset_name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-' || byte == b'_'
        }),
        "release asset name contains invalid path characters"
    );
    Url::parse(&format!(
        "https://github.com/{REPOSITORY}/releases/download/{tag}/{asset_name}"
    ))
    .context("could not construct a GitHub release-asset URL")
}

fn is_github_origin(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
}

fn is_github_download_url(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    url.scheme() == "https"
        && (host == "github.com" || host.ends_with(".githubusercontent.com"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
}

fn validate_download_url(url: &Url) -> anyhow::Result<()> {
    ensure!(
        is_github_download_url(url),
        "release asset redirected outside GitHub's HTTPS download hosts"
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
        is_github_download_url, parse_sha256_digest, parse_sha256sum, parse_version,
        release_asset_url, release_tag_from_url, replace_executable, validate_download_url,
    };
    use reqwest::Url;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn release_version_should_parse_a_stable_three_part_version() {
        assert_eq!(parse_version("0.1.26").expect("valid semver"), (0, 1, 26));
    }

    #[test]
    fn release_version_should_reject_prerelease_and_non_numeric_versions() {
        assert!(parse_version("0.1.26-rc1").is_err() && parse_version("v0.1.x").is_err());
    }

    #[test]
    fn latest_release_redirect_should_return_only_a_github_release_tag() {
        let trusted =
            Url::parse("https://github.com/ishan-parihar/artificial-route/releases/tag/v0.1.26")
                .expect("URL");
        let untrusted = Url::parse("https://attacker.example/releases/tag/v0.1.26").expect("URL");
        let unrelated =
            Url::parse("https://github.com/ishan-parihar/artificial-route/releases/latest")
                .expect("URL");
        assert!(
            release_tag_from_url(&trusted).expect("release tag") == "v0.1.26"
                && release_tag_from_url(&untrusted).is_err()
                && release_tag_from_url(&unrelated).is_err()
        );
    }

    #[test]
    fn release_asset_url_should_construct_only_safe_paths() {
        let expected = Url::parse(
            "https://github.com/ishan-parihar/artificial-route/releases/download/v0.1.26/aroute-x86_64-unknown-linux-musl",
        )
        .expect("URL");
        assert_eq!(
            (
                release_asset_url("v0.1.26", "aroute-x86_64-unknown-linux-musl")
                    .expect("valid release URL"),
                release_asset_url("../../attacker", "aroute").is_err()
            ),
            (expected, true)
        );
    }

    #[test]
    fn release_checksum_manifest_should_select_the_exact_asset_digest() {
        let manifest = concat!(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  aroute-x86_64-unknown-linux-musl\n",
            "0000000000000000000000000000000000000000000000000000000000000000  SHA256SUMS\n"
        );
        let expected = Sha256::digest(b"abc");
        let actual = parse_sha256sum(manifest, "aroute-x86_64-unknown-linux-musl")
            .expect("matching checksum exists");
        assert_eq!(actual.as_slice(), expected.as_slice());
    }

    #[test]
    fn release_checksum_manifest_should_reject_missing_duplicate_or_malformed_rows() {
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let valid_row = format!("{digest}  aroute-x86_64-unknown-linux-musl\n");
        let duplicate_rows = format!("{valid_row}{valid_row}");
        assert!(
            parse_sha256sum(&valid_row, "missing").is_err()
                && parse_sha256sum(&duplicate_rows, "aroute-x86_64-unknown-linux-musl").is_err()
                && parse_sha256sum("bad row", "aroute").is_err()
                && parse_sha256_digest("bad").is_err()
        );
    }

    #[test]
    fn release_redirect_should_allow_only_https_github_download_hosts() {
        let trusted =
            Url::parse("https://release-assets.githubusercontent.com/asset").expect("URL");
        let untrusted = Url::parse("https://downloads.example/asset").expect("URL");
        let untrusted_port =
            Url::parse("https://release-assets.githubusercontent.com:444/asset").expect("URL");
        let http = Url::parse("http://github.com/asset").expect("URL");
        assert!(
            is_github_download_url(&trusted)
                && validate_download_url(&trusted).is_ok()
                && validate_download_url(&untrusted).is_err()
                && validate_download_url(&untrusted_port).is_err()
                && !is_github_download_url(&http)
        );
    }

    #[test]
    fn release_digest_should_match_the_sha256_bytes() {
        let expected =
            parse_sha256_digest("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .expect("valid digest");
        let actual = Sha256::digest(b"abc");
        assert_eq!(actual.as_slice(), expected);
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
