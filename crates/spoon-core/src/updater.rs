use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_REPO: &str = "Spoon-Launcher/Spoon";
const API_BASE: &str = "https://api.github.com";

/// Name of the release asset for the platform this build runs on,
/// or `None` on platforms Spoon doesn't publish artifacts for.
pub fn asset_name() -> Option<&'static str> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("Spoon-Launcher-Windows-x86_64.exe"),
        ("linux", "x86_64") => Some("Spoon-Launcher-Linux-x86_64.AppImage"),
        ("macos", "x86_64") => Some("Spoon-Launcher-macOS-Intel.zip"),
        ("macos", "aarch64") => Some("Spoon-Launcher-macOS-AppleSilicon.zip"),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub size: u64,
    pub download_url: String,
}

/// A published GitHub release of Spoon with a parsed semver version.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub version: semver::Version,
    pub notes: String,
    pub published_at: Option<String>,
    pub assets: Vec<ReleaseAsset>,
}

/// A newer release plus the platform's asset within it.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub release: Release,
    pub asset: ReleaseAsset,
}

/// Failure modes of the update check and download.
#[derive(Debug)]
pub enum UpdateError {
    Request(reqwest::Error),
    Api { status: u16, message: String },
    Json(serde_json::Error),
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::Request(err) => write!(f, "request failed: {err}"),
            UpdateError::Api { status, message } => write!(f, "api error ({status}): {message}"),
            UpdateError::Json(err) => write!(f, "invalid api response: {err}"),
            UpdateError::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<reqwest::Error> for UpdateError {
    fn from(err: reqwest::Error) -> Self {
        UpdateError::Request(err)
    }
}

impl From<serde_json::Error> for UpdateError {
    fn from(err: serde_json::Error) -> Self {
        UpdateError::Json(err)
    }
}

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> UpdateError {
    let path = path.into();
    move |source| UpdateError::Io { path, source }
}

// ---------- client ----------

/// Blocking client for the GitHub releases of the Spoon repo.
pub struct UpdateClient {
    repo: String,
    current: semver::Version,
    client: reqwest::blocking::Client,
}

impl UpdateClient {
    pub fn new(current_version: &str) -> Self {
        let repo =
            std::env::var("SPOON_RELEASES_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string());
        Self::with_repo(&repo, current_version)
    }

    pub fn with_repo(repo: &str, current_version: &str) -> Self {
        let current = semver::Version::parse(current_version)
            .unwrap_or_else(|_| panic!("invalid current app version: {current_version}"));
        let client = reqwest::blocking::Client::builder()
            .user_agent(format!("Spoon/{current}"))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build http client");
        Self {
            repo: repo.trim_matches('/').to_string(),
            current,
            client,
        }
    }

    /// Check for a release newer than the current version that ships the
    /// asset for this platform (see [`asset_name`]). Returns `Ok(None)` when
    /// up to date, when there is no (non-draft, non-prerelease) release, when
    /// its tag isn't semver, when the platform has no published asset, or
    /// when the asset is missing.
    pub fn check(&self) -> Result<Option<UpdateInfo>, UpdateError> {
        let Some(asset_name) = asset_name() else {
            return Ok(None);
        };
        let Some(release) = self.latest_release()? else {
            return Ok(None);
        };
        if release.version <= self.current {
            return Ok(None);
        }
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .or_else(|| {
                release
                    .assets
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(asset_name))
            })
            .cloned();
        let Some(asset) = asset else {
            return Ok(None);
        };
        Ok(Some(UpdateInfo { release, asset }))
    }

    /// Download `asset` into `dest_dir` (created if needed), removing the
    /// partial file on failure. On success returns the path to the artifact.
    pub fn download(
        &self,
        asset: &ReleaseAsset,
        dest_dir: &Path,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, UpdateError> {
        fs::create_dir_all(dest_dir).map_err(io_err(dest_dir))?;
        let dest = dest_dir.join(&asset.name);
        if let Err(err) = self.download_file(&asset.download_url, &dest, asset.size, progress) {
            let _ = fs::remove_file(&dest);
            return Err(err);
        }
        Ok(dest)
    }

    fn latest_release(&self) -> Result<Option<Release>, UpdateError> {
        let response = self
            .client
            .get(format!("{API_BASE}/repos/{}/releases/latest", self.repo))
            .send()?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(api_error(response));
        }
        Ok(release_from_github(response.json::<GitHubRelease>()?))
    }

    fn download_file(
        &self,
        url: &str,
        dest: &Path,
        total: u64,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), UpdateError> {
        let mut response = self.client.get(url).send()?.error_for_status()?;
        let mut file = File::create(dest).map_err(io_err(dest))?;
        let mut downloaded = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = response.read(&mut buf).map_err(io_err(dest))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(io_err(dest))?;
            downloaded += n as u64;
            progress(downloaded, total);
        }
        Ok(())
    }
}

fn parse_tag(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.trim_start_matches(['v', 'V'])).ok()
}

fn release_from_github(release: GitHubRelease) -> Option<Release> {
    let version = parse_tag(&release.tag_name)?;
    Some(Release {
        tag: release.tag_name,
        version,
        notes: release.body.unwrap_or_default(),
        published_at: release.published_at,
        assets: release
            .assets
            .into_iter()
            .map(|a| ReleaseAsset {
                name: a.name,
                size: a.size,
                download_url: a.browser_download_url,
            })
            .collect(),
    })
}

fn api_error(response: reqwest::blocking::Response) -> UpdateError {
    let status = response.status().as_u16();
    let message = response
        .json::<serde_json::Value>()
        .ok()
        .and_then(|body| {
            body["message"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown error".to_string());
    UpdateError::Api { status, message }
}

/// Folder where downloaded app updates are stored
/// (e.g. `%APPDATA%/Spoon/updates` on Windows).
pub fn updates_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("Spoon").join("updates"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_match_supported_platforms() {
        assert_eq!(
            asset_name_for("windows", "x86_64"),
            Some("Spoon-Launcher-Windows-x86_64.exe")
        );
        assert_eq!(
            asset_name_for("linux", "x86_64"),
            Some("Spoon-Launcher-Linux-x86_64.AppImage")
        );
        assert_eq!(
            asset_name_for("macos", "x86_64"),
            Some("Spoon-Launcher-macOS-Intel.zip")
        );
        assert_eq!(
            asset_name_for("macos", "aarch64"),
            Some("Spoon-Launcher-macOS-AppleSilicon.zip")
        );
        assert_eq!(asset_name_for("freebsd", "x86_64"), None);
        assert_eq!(asset_name_for("windows", "aarch64"), None);
    }

    #[test]
    fn parses_release_fixture() {
        let json = r#"{
            "tag_name": "v0.2.0",
            "body": "What's new",
            "published_at": "2025-06-01T12:00:00Z",
            "assets": [
                {
                    "name": "Spoon-Launcher.exe",
                    "size": 12345678,
                    "browser_download_url": "https://github.com/galactiq-nodes/Spoon/releases/download/v0.2.0/Spoon-Launcher.exe"
                },
                {
                    "name": "other.txt",
                    "size": 10,
                    "browser_download_url": "https://example.com/other.txt"
                }
            ]
        }"#;
        let release = release_from_github(serde_json::from_str(json).unwrap()).unwrap();
        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.version, semver::Version::new(0, 2, 0));
        assert_eq!(release.notes, "What's new");
        assert_eq!(release.assets.len(), 2);
    }

    #[test]
    fn skips_non_semver_tags() {
        let json = r#"{ "tag_name": "release-candidate", "assets": [] }"#;
        assert!(release_from_github(serde_json::from_str(json).unwrap()).is_none());
    }

    #[test]
    fn tag_versions_parse_with_or_without_v_prefix() {
        assert_eq!(parse_tag("v1.2.3"), Some(semver::Version::new(1, 2, 3)));
        assert_eq!(parse_tag("1.2.3"), Some(semver::Version::new(1, 2, 3)));
        assert_eq!(
            parse_tag("v2.0.0-beta.1"),
            Some(semver::Version::parse("2.0.0-beta.1").unwrap())
        );
        assert!(parse_tag("latest").is_none());
    }
}
