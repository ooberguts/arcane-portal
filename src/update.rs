use std::fs::File;
use std::io::{Read, Seek, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use ureq::http::{StatusCode, Uri, header};
use ureq::{Agent, Body};

use crate::install::{self, StagedUpdate};

pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const RELEASES_URL: &str = "https://github.com/ooberguts/arcane-portal/releases/latest";
const API_URL: &str = "https://api.github.com/repos/ooberguts/arcane-portal/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/ooberguts/arcane-portal/releases/download";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

#[derive(Default)]
pub(crate) enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available {
        version: String,
    },
    Downloading,
    Ready {
        version: String,
    },
    Launching,
    Restarting,
    Failed(String),
}

#[derive(Default)]
pub(crate) struct Updater {
    status: UpdateStatus,
    worker: Option<Receiver<WorkerResult>>,
    release: Option<Release>,
    staged: Option<StagedUpdate>,
    progress: Option<DownloadProgress>,
}

struct DownloadProgress {
    received: Arc<AtomicU64>,
    total: u64,
}

type WorkerResult = Result<WorkerOutput, String>;

enum WorkerOutput {
    Check(Option<Release>),
    Stage {
        version: String,
        update: StagedUpdate,
    },
    Launched,
}

impl Updater {
    pub(crate) fn status(&self) -> &UpdateStatus {
        &self.status
    }

    pub(crate) fn progress(&self) -> Option<(u64, u64)> {
        self.progress
            .as_ref()
            .map(|progress| (progress.received.load(Ordering::Relaxed), progress.total))
    }

    pub(crate) fn report_failure(&mut self, error: String) {
        if matches!(self.status, UpdateStatus::Idle) {
            self.status = UpdateStatus::Failed(error);
        }
    }

    pub(crate) fn check(&mut self) {
        if self.worker.is_some()
            || self.staged.is_some()
            || matches!(self.status, UpdateStatus::Restarting)
        {
            return;
        }
        self.release = None;
        self.status = UpdateStatus::Checking;
        self.spawn(|| check_release().map(WorkerOutput::Check));
    }

    pub(crate) fn download(&mut self) {
        let Some(release) = self.release.take() else {
            return;
        };
        let received = Arc::new(AtomicU64::new(0));
        self.progress = Some(DownloadProgress {
            received: Arc::clone(&received),
            total: release.size,
        });
        self.status = UpdateStatus::Downloading;
        self.spawn(move || {
            let archive = download_package(&release, &received)?;
            let update = install::stage(archive)?;
            Ok(WorkerOutput::Stage {
                version: release.version.to_string(),
                update,
            })
        });
    }

    pub(crate) fn poll(&mut self) {
        let result = match self.worker.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => {
                Err("The update stopped early. Try checking again.".into())
            }
            _ => return,
        };
        self.worker = None;
        self.progress = None;
        self.status = match result {
            Ok(WorkerOutput::Check(Some(release))) => {
                let version = release.version.to_string();
                self.release = Some(release);
                UpdateStatus::Available { version }
            }
            Ok(WorkerOutput::Check(None)) => UpdateStatus::UpToDate,
            Ok(WorkerOutput::Stage { version, update }) => {
                self.staged = Some(update);
                UpdateStatus::Ready { version }
            }
            Ok(WorkerOutput::Launched) => UpdateStatus::Restarting,
            Err(error) => UpdateStatus::Failed(error),
        };
    }

    pub(crate) fn launch(&mut self) {
        if self.worker.is_some() || matches!(self.status, UpdateStatus::Restarting) {
            return;
        }
        let Some(update) = self.staged.take() else {
            self.status = UpdateStatus::Failed("Download an update before installing it.".into());
            return;
        };
        self.status = UpdateStatus::Launching;
        self.spawn(move || update.launch().map(|()| WorkerOutput::Launched));
    }

    fn spawn(&mut self, job: impl FnOnce() -> WorkerResult + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        match thread::Builder::new()
            .name("arcane-update".into())
            .spawn(move || {
                let _ = tx.send(job());
            }) {
            Ok(_) => self.worker = Some(rx),
            Err(error) => {
                self.progress = None;
                self.status = UpdateStatus::Failed(format!("Could not start the update. {error}"));
            }
        }
    }
}

#[derive(Debug)]
struct Release {
    version: Version,
    url: String,
    size: u64,
    digest: [u8; 32],
}

#[derive(Deserialize)]
struct ReleaseManifest {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    state: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
}

fn package_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("Arcane-Portal-Windows-x64.zip"),
        ("macos", "aarch64") => Some("Arcane-Portal-macOS-Apple-silicon.zip"),
        ("macos", "x86_64") => Some("Arcane-Portal-macOS-Intel.zip"),
        _ => None,
    }
}

fn check_release() -> Result<Option<Release>, String> {
    let name = package_name().ok_or("In-app updates are unavailable for this build.")?;
    let current = Version::parse(CURRENT_VERSION).map_err(|_| "The app has an invalid version.")?;
    let agent = http_client(Duration::from_secs(30));
    let mut response = agent
        .get(API_URL)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .call()
        .map_err(|error| format!("Could not check GitHub for updates. {error}"))?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => {
            return Err("No public stable Arcane Portal release is available on GitHub.".into());
        }
        status => {
            return Err(format!(
                "GitHub could not check for updates (HTTP {}).",
                status.as_u16()
            ));
        }
    }
    let manifest = response
        .body_mut()
        .with_config()
        .limit(MAX_MANIFEST_BYTES)
        .read_to_vec()
        .map_err(|error| format!("Could not read the GitHub release. {error}"))?;
    let manifest = serde_json::from_slice(&manifest)
        .map_err(|_| "GitHub returned invalid release information.")?;
    select_release(manifest, &current, name)
}

fn http_client(timeout: Duration) -> Agent {
    Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent(concat!("Arcane-Portal/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn select_release(
    manifest: ReleaseManifest,
    current: &Version,
    name: &str,
) -> Result<Option<Release>, String> {
    let version = Version::parse(
        manifest
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&manifest.tag_name),
    )
    .map_err(|_| "GitHub returned an invalid version tag.")?;
    if manifest.draft || manifest.prerelease || !version.pre.is_empty() || !version.build.is_empty()
    {
        return Err("GitHub did not return a stable Arcane Portal release.".into());
    }
    if !version.cmp_precedence(current).is_gt() {
        return Ok(None);
    }
    let mut assets = manifest
        .assets
        .into_iter()
        .filter(|asset| asset.name == name);
    let asset = assets
        .next()
        .ok_or("The newest release has no update package for this computer.")?;
    if assets.next().is_some()
        || asset.state != "uploaded"
        || !(1..=MAX_PACKAGE_BYTES).contains(&asset.size)
    {
        return Err("GitHub returned an invalid update package.".into());
    }
    let url = format!("{DOWNLOAD_BASE}/{}/{name}", manifest.tag_name);
    if asset.browser_download_url != url {
        return Err("The update package URL does not match the official release.".into());
    }
    let digest = asset.digest.as_deref().and_then(parse_digest).ok_or(
        "The release is missing a valid SHA-256 checksum. Use GitHub Releases to install manually.",
    )?;
    Ok(Some(Release {
        version,
        url,
        size: asset.size,
        digest,
    }))
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    let value = value.strip_prefix("sha256:")?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

fn download_package(release: &Release, received: &AtomicU64) -> Result<File, String> {
    let agent = http_client(Duration::from_secs(300));
    let mut body = download_body(&agent, &release.url)?;
    if body
        .content_length()
        .is_some_and(|length| length != release.size)
    {
        return Err("The download size does not match the release package.".into());
    }
    let mut archive = tempfile::tempfile()
        .map_err(|error| format!("Could not create a temporary update file. {error}"))?;
    copy_verified(
        body.as_reader(),
        &mut archive,
        release.size,
        release.digest,
        received,
    )?;
    archive
        .rewind()
        .map_err(|error| format!("Could not reopen the verified package. {error}"))?;
    Ok(archive)
}

fn download_body(agent: &Agent, initial_url: &str) -> Result<Body, String> {
    let mut url = Uri::try_from(initial_url).map_err(|_| "The update download URL is invalid.")?;
    for redirect_count in 0..=MAX_REDIRECTS {
        let response = agent.get(&url).call()
            .map_err(|_| "Could not download the GitHub release. Check your internet connection and try again.")?;
        let status = response.status();
        if status == StatusCode::OK {
            return Ok(response.into_body());
        }
        if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Err(format!(
                "GitHub could not provide the update package (HTTP {}).",
                status.as_u16()
            ));
        }
        if redirect_count == MAX_REDIRECTS {
            return Err("GitHub redirected the download too many times.".into());
        }
        url = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .and_then(official_redirect)
            .ok_or("GitHub directed the download to an unexpected address.")?;
    }
    unreachable!()
}

fn official_redirect(location: &str) -> Option<Uri> {
    let url = Uri::try_from(location).ok()?;
    let authority = url.authority()?.as_str();
    if url.scheme_str() != Some("https")
        || !matches!(
            authority,
            "release-assets.githubusercontent.com" | "objects.githubusercontent.com"
        )
        || !url.path().starts_with("/github-production-release-asset")
    {
        return None;
    }
    Some(url)
}

fn copy_verified(
    mut input: impl Read,
    mut output: impl Write,
    size: u64,
    digest: [u8; 32],
    received: &AtomicU64,
) -> Result<(), String> {
    let mut hasher = Sha256::new();
    let mut downloaded = 0;
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| "The download was interrupted. Check your connection and try again.")?;
        if count == 0 {
            break;
        }
        downloaded += count as u64;
        if downloaded > size {
            return Err("The download exceeded the expected package size.".into());
        }
        let chunk = &buffer[..count];
        output
            .write_all(chunk)
            .map_err(|error| format!("Could not save the update package. {error}"))?;
        hasher.update(chunk);
        received.store(downloaded, Ordering::Relaxed);
    }
    if downloaded != size {
        return Err("The download ended before the update package was complete.".into());
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if actual != digest {
        return Err(
            "The downloaded update failed SHA-256 verification. Check again before installing."
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PACKAGE: &str = "Arcane-Portal-Windows-x64.zip";

    fn manifest() -> ReleaseManifest {
        ReleaseManifest {
            tag_name: "v1.2.3".into(),
            draft: false,
            prerelease: false,
            assets: vec![ReleaseAsset {
                name: TEST_PACKAGE.into(),
                state: "uploaded".into(),
                size: 1024,
                browser_download_url: format!("{DOWNLOAD_BASE}/v1.2.3/{TEST_PACKAGE}"),
                digest: Some(format!("sha256:{}", "ff".repeat(32))),
            }],
        }
    }

    #[test]
    fn selects_newer_stable_release_for_exact_package() {
        let package = select_release(manifest(), &Version::new(1, 2, 2), TEST_PACKAGE)
            .unwrap()
            .unwrap();
        assert_eq!(package.version, Version::new(1, 2, 3));
        assert_eq!(package.digest, [255; 32]);
        assert!(
            select_release(manifest(), &Version::new(1, 2, 3), TEST_PACKAGE)
                .unwrap()
                .is_none()
        );
        assert!(
            select_release(manifest(), &Version::new(2, 0, 0), TEST_PACKAGE)
                .unwrap()
                .is_none()
        );
        assert!(
            select_release(
                manifest(),
                &Version::new(1, 2, 2),
                "Arcane-Portal-macOS-Intel.zip"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unpublished_or_unverified_packages() {
        let current = Version::new(1, 2, 2);
        let select = |manifest| select_release(manifest, &current, TEST_PACKAGE);
        let mut release = manifest();
        release.prerelease = true;
        assert!(select(release).is_err());
        let mut release = manifest();
        release.assets[0].digest = None;
        assert!(select(release).is_err());
        let mut release = manifest();
        release.assets[0].browser_download_url = format!("{DOWNLOAD_BASE}/v0.9.0/{TEST_PACKAGE}");
        assert!(select(release).is_err());
        let mut release = manifest();
        release.assets[0].size = MAX_PACKAGE_BYTES + 1;
        assert!(select(release).is_err());
    }

    #[test]
    fn download_redirects_require_the_github_release_cdn_over_https() {
        assert!(official_redirect("https://release-assets.githubusercontent.com/github-production-release-asset/123/456?signed=xyz").is_some());
        assert!(official_redirect("http://release-assets.githubusercontent.com/github-production-release-asset/123/456").is_none());
        assert!(official_redirect("https://release-assets.githubusercontent.com.evil.org/github-production-release-asset/123/456").is_none());
        assert!(official_redirect("https://example@release-assets.githubusercontent.com/github-production-release-asset/123/456").is_none());
        assert!(
            official_redirect("https://objects.githubusercontent.com/unrelated/path").is_none()
        );
    }

    #[test]
    fn verifies_download_length_checksum_and_progress() {
        let bytes = b"verified release sample";
        let size = bytes.len() as u64;
        let digest = Sha256::digest(bytes).into();
        let received = AtomicU64::new(0);
        let mut output = Vec::new();
        copy_verified(&bytes[..], &mut output, size, digest, &received).unwrap();
        assert_eq!(&output, bytes);
        assert_eq!(received.load(Ordering::Relaxed), size);
        assert!(copy_verified(&bytes[..], Vec::new(), size - 1, digest, &received).is_err());
        assert!(copy_verified(&bytes[..], Vec::new(), size + 1, digest, &received).is_err());
        assert!(copy_verified(&bytes[..], Vec::new(), size, [0; 32], &received).is_err());
    }
}
