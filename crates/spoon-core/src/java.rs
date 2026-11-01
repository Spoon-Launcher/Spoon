//! Private Java runtime management: fetch downloadable JDKs from the Adoptium
//! (Eclipse Temurin) API, install them into Spoon's own directory, list and
//! remove them. Runtimes here are only visible to Spoon.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const ADOPTIUM_API: &str = "https://api.adoptium.net/v3";
/// Metadata file written into each installed runtime's folder.
const RUNTIME_META: &str = "spoon-java.json";

/// A JDK offered by the Adoptium API for this host's OS and architecture.
#[derive(Debug, Clone)]
pub struct JavaCandidate {
    /// Feature release, e.g. 21 for "Java 21".
    pub feature: u32,
    /// Full OpenJDK version, e.g. "21.0.5+11".
    pub release: String,
    pub download_url: String,
    pub size: u64,
}

/// A JDK installed into Spoon's private java directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaRuntime {
    /// Stable id (the install folder name), e.g. "jdk-21.0.5-11".
    pub id: String,
    pub feature: u32,
    /// Full OpenJDK version, e.g. "21.0.5+11".
    pub release: String,
}

impl JavaRuntime {
    /// Absolute path to the `java` executable inside this runtime.
    pub fn java_bin(&self, store: &JavaStore) -> PathBuf {
        let home = store.root.join(&self.id);
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        home.join("bin").join(exe)
    }
}

/// Manages Spoon's private JDK directory (e.g. `~/.config/Spoon/java`).
#[derive(Debug, Clone)]
pub struct JavaStore {
    root: PathBuf,
}

impl JavaStore {
    /// Spoon's java directory under the platform config dir, if it can be determined.
    pub fn new() -> Option<Self> {
        let root = dirs::config_dir()?.join("Spoon").join("java");
        Some(Self { root })
    }

    /// Create a store rooted at an arbitrary directory (tests).
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// All runtimes whose folder still contains a `bin/java` executable.
    pub fn list(&self) -> Vec<JavaRuntime> {
        let mut runtimes = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return runtimes;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(text) = fs::read_to_string(dir.join(RUNTIME_META)) else {
                continue;
            };
            let Ok(runtime) = serde_json::from_str::<JavaRuntime>(&text) else {
                continue;
            };
            if runtime.java_bin(self).exists() {
                runtimes.push(runtime);
            }
        }
        runtimes.sort_by_key(|runtime| std::cmp::Reverse(runtime.feature));
        runtimes
    }

    pub fn find(&self, id: &str) -> Option<JavaRuntime> {
        self.list().into_iter().find(|runtime| runtime.id == id)
    }

    /// Download and install a JDK candidate. The archive is extracted into a
    /// temp folder, the folder containing `bin/java` becomes the runtime home,
    /// and it is moved to `<root>/<id>` together with its metadata file.
    pub fn install(
        &self,
        candidate: &JavaCandidate,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<JavaRuntime, String> {
        fs::create_dir_all(&self.root).map_err(|err| format!("creating java dir: {err}"))?;
        let id = sanitize_id(&format!("jdk-{}", candidate.release));
        let home = self.root.join(&id);
        if home.exists() {
            return Err(format!("{} is already installed.", candidate.release));
        }

        let archive_ext = if candidate.download_url.ends_with(".zip") {
            ".zip"
        } else {
            ".tar.gz"
        };
        let archive = self.root.join(format!(".download-{id}{archive_ext}"));
        let extract_dir = self.root.join(format!(".extract-{id}"));
        let result = (|| {
            download_file(&candidate.download_url, &archive, progress)?;
            extract_archive(&archive, &extract_dir)?;
            let java_home = find_java_home(&extract_dir)
                .ok_or_else(|| "Downloaded JDK did not contain a java binary.".to_string())?;
            // Move the home (the dir holding bin/) to its final place, then add
            // the metadata file beside it.
            fs::rename(&java_home, &home).map_err(|err| format!("moving JDK into place: {err}"))?;
            let runtime = JavaRuntime {
                id: id.clone(),
                feature: candidate.feature,
                release: candidate.release.clone(),
            };
            let meta = serde_json::to_string_pretty(&runtime).map_err(|err| err.to_string())?;
            fs::write(home.join(RUNTIME_META), meta).map_err(|err| err.to_string())?;
            Ok(runtime)
        })();
        let _ = fs::remove_file(&archive);
        let _ = fs::remove_dir_all(&extract_dir);
        result
    }

    /// Remove an installed runtime from disk.
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let home = self.root.join(id);
        if !home.is_dir() {
            return Err("Runtime is not installed.".to_string());
        }
        fs::remove_dir_all(&home).map_err(|err| format!("removing runtime: {err}"))
    }
}

/// Turn a release string into a safe, unique folder name.
fn sanitize_id(name: &str) -> String {
    let mut id = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' {
            id.push(ch.to_ascii_lowercase());
        } else {
            id.push('-');
        }
    }
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    id.trim_matches('-').to_string()
}

/// Depth-first search for a directory layout containing `bin/java[.exe]`;
/// returns the directory that holds `bin` (the JAVA_HOME).
fn find_java_home(root: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let bin = root.join("bin").join(exe);
    if bin.exists() {
        return Some(root.to_path_buf());
    }
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && !path
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        {
            if let Some(home) = find_java_home(&path) {
                return Some(home);
            }
        }
    }
    None
}

fn download_file(url: &str, dest: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|err| err.to_string())?;
    let mut response = client
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| err.to_string())?;
    let total = response.content_length().unwrap_or(0);
    let mut file =
        fs::File::create(dest).map_err(|err| format!("creating download file: {err}"))?;
    let mut downloaded = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|err| err.to_string())?;
        downloaded += n as u64;
        progress(downloaded, total);
    }
    Ok(())
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|err| err.to_string())?;
    if archive.extension().is_some_and(|ext| ext == "zip") {
        extract_zip(archive, dest)
    } else {
        extract_tar_gz(archive, dest)
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|err| err.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| err.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|err| err.to_string())?;
        let Some(path) = entry.enclosed_name().map(|p| dest.join(p)) else {
            continue; // zip-slip attempt or weird name: skip
        };
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(|err| err.to_string())?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            let mut out = fs::File::create(&path).map_err(|err| err.to_string())?;
            io::copy(&mut entry, &mut out).map_err(|err| err.to_string())?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = out.set_permissions(fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|err| err.to_string())?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(|err| err.to_string())? {
        let mut entry = entry.map_err(|err| err.to_string())?;
        // Skip anything that would escape the destination (tar-slip).
        let Ok(path) = entry.path().map(|p| p.to_path_buf()) else {
            continue;
        };
        if path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            continue;
        }
        let target = dest.join(path);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target).map_err(|err| err.to_string())?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            let mut out = fs::File::create(&target).map_err(|err| err.to_string())?;
            io::copy(&mut entry, &mut out).map_err(|err| err.to_string())?;
            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = out.set_permissions(fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn adoptium_os_arch() -> Result<(&'static str, &'static str), String> {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "mac",
        "linux" => "linux",
        other => return Err(format!("Unsupported operating system: {other}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        "x86" => "x86",
        "arm" => "arm",
        other => return Err(format!("Unsupported CPU architecture: {other}")),
    };
    Ok((os, arch))
}

/// Feature releases (LTS first, e.g. [8, 11, 17, 21, 25]) currently offered by
/// Adoptium. Falls back to all feature releases if no LTS list is present.
fn fetch_feature_releases(client: &reqwest::blocking::Client) -> Result<Vec<u32>, String> {
    #[derive(Deserialize)]
    struct AvailableReleases {
        available_lts_releases: Option<Vec<u32>>,
        available_releases: Vec<u32>,
    }
    let response: AvailableReleases = client
        .get(format!("{ADOPTIUM_API}/info/available_releases"))
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json())
        .map_err(|err| format!("Fetching Java releases: {err}"))?;
    Ok(match response.available_lts_releases {
        Some(lts) if !lts.is_empty() => lts,
        _ => response.available_releases,
    })
}

fn fetch_candidate(
    client: &reqwest::blocking::Client,
    feature: u32,
    os: &str,
    arch: &str,
) -> Result<Option<JavaCandidate>, String> {
    #[derive(Deserialize)]
    struct ReleaseAsset {
        binaries: Vec<Binary>,
        release_name: String,
    }
    #[derive(Deserialize)]
    struct Binary {
        package: Package,
    }
    #[derive(Deserialize)]
    struct Package {
        link: String,
        size: u64,
    }
    let url = format!(
        "{ADOPTIUM_API}/assets/feature_releases/{feature}/ga?architecture={arch}&heap_size=normal&image_type=jdk&os={os}&vendor=eclipse"
    );
    let response = client
        .get(url)
        .send()
        .map_err(|err| format!("Fetching Java {feature}: {err}"))?;
    // Some feature releases are not published for every OS/architecture
    // (e.g. no Java 8 for macOS aarch64); skip those.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let assets: Vec<ReleaseAsset> = response
        .error_for_status()
        .and_then(|response| response.json())
        .map_err(|err| format!("Fetching Java {feature}: {err}"))?;
    Ok(assets.first().and_then(|asset| {
        asset.binaries.first().map(|binary| JavaCandidate {
            feature,
            release: asset
                .release_name
                .strip_prefix("jdk-")
                .unwrap_or(&asset.release_name)
                .to_string(),
            download_url: binary.package.link.clone(),
            size: binary.package.size,
        })
    }))
}

/// Downloadable JDKs for this host, newest feature release first. One network
/// round-trip per feature release.
pub fn fetch_candidates() -> Result<Vec<JavaCandidate>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Spoon-Launcher")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|err| err.to_string())?;
    let (os, arch) = adoptium_os_arch()?;
    let mut candidates = Vec::new();
    for feature in fetch_feature_releases(&client)? {
        match fetch_candidate(&client, feature, os, arch) {
            Ok(Some(candidate)) => candidates.push(candidate),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.feature));
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_makes_safe_folder_names() {
        assert_eq!(sanitize_id("jdk-21.0.5+11"), "jdk-21.0.5-11");
        assert_eq!(sanitize_id("JDK 17.0.2 (hotspot)"), "jdk-17.0.2-hotspot");
        assert_eq!(sanitize_id("+++"), "");
    }

    #[test]
    fn store_lists_only_runtimes_with_java_binary() {
        let root = std::env::temp_dir().join(format!("spoon-java-test-{}", new_id()));
        let store = JavaStore::at(root.clone());

        // Runtime with a fake bin/java + metadata.
        let home = root.join("jdk-21.0.5-11");
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(
            home.join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" }),
            "",
        )
        .unwrap();
        let meta = JavaRuntime {
            id: "jdk-21.0.5-11".into(),
            feature: 21,
            release: "21.0.5+11".into(),
        };
        fs::write(
            home.join(RUNTIME_META),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();

        // A folder missing the binary must be ignored.
        let broken = root.join("jdk-17-broken");
        fs::create_dir_all(broken.join("bin")).unwrap();
        fs::write(
            broken.join(RUNTIME_META),
            serde_json::to_string_pretty(&JavaRuntime {
                id: "jdk-17-broken".into(),
                feature: 17,
                release: "17".into(),
            })
            .unwrap(),
        )
        .unwrap();

        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].feature, 21);
        assert!(list[0].java_bin(&store).exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn find_java_home_handles_nested_jdk_layouts() {
        let root = std::env::temp_dir().join(format!("spoon-java-home-test-{}", new_id()));
        let nested = root.join("jdk-21.jdk/Contents/Home");
        fs::create_dir_all(nested.join("bin")).unwrap();
        let exe = nested
            .join("bin")
            .join(if cfg!(windows) { "java.exe" } else { "java" });
        fs::write(&exe, "").unwrap();
        assert_eq!(find_java_home(&root).unwrap(), nested);
        fs::remove_dir_all(&root).unwrap();
    }

    fn new_id() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// Real download + install + uninstall against the live Adoptium API.
    /// Excluded from normal runs (`--ignored`); uses the smallest offered JDK.
    #[test]
    #[ignore = "downloads a JDK from the internet"]
    fn installs_a_jdk_end_to_end() {
        let candidates = fetch_candidates().expect("fetch candidates");
        assert!(!candidates.is_empty());
        let candidate = candidates.iter().min_by_key(|c| c.size).unwrap().clone();

        let root = std::env::temp_dir().join(format!("spoon-java-e2e-{}", new_id()));
        let store = JavaStore::at(root.clone());
        let mut progress_calls = 0u32;
        let runtime = store
            .install(&candidate, &mut |done, total| {
                assert!(total > 0);
                assert!(done <= total);
                progress_calls += 1;
            })
            .expect("install");
        assert!(progress_calls > 0);

        let java_bin = runtime.java_bin(&store);
        assert!(java_bin.exists());
        let output = std::process::Command::new(&java_bin)
            .arg("-version")
            .output()
            .expect("run java");
        let banner = String::from_utf8_lossy(&output.stderr);
        assert!(banner.starts_with("openjdk"), "unexpected banner: {banner}");

        assert_eq!(store.list().len(), 1);
        store.uninstall(&runtime.id).unwrap();
        assert!(store.list().is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
