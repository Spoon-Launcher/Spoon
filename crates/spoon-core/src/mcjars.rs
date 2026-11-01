//! Client for the MCJars API (https://mcjars.app), used to browse and
//! download Minecraft server jars.
//!
//! Typical flow for creating a server:
//! 1. [`McJarsClient::types`] — list available server software (Paper, Fabric, …)
//! 2. [`McJarsClient::versions`] — list Minecraft versions for one software
//! 3. [`McJarsClient::builds`] — list builds for a version (first = latest)
//! 4. [`McJarsClient::install_build`] — download/install into a server folder

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use indexmap::IndexMap;
use serde::Deserialize;

pub const DEFAULT_BASE_URL: &str = "https://mcjars.app";

// ---------- API models ----------

/// Server software projects known to MCJars. Unknown types from the API are
/// preserved as [`ServerType::Unknown`] so the client keeps working when
/// MCJars adds new software.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServerType {
    Vanilla,
    Paper,
    Pufferfish,
    Spigot,
    Folia,
    Purpur,
    Waterfall,
    Velocity,
    Fabric,
    Bungeecord,
    Quilt,
    Forge,
    Neoforge,
    Mohist,
    Arclight,
    Sponge,
    Leaves,
    Canvas,
    Aspaper,
    LegacyFabric,
    LoohpLimbo,
    Nanolimbo,
    Divinemc,
    Unknown(String),
}

impl ServerType {
    /// Parse an API type name (e.g. `PAPER`, `VELOCITY_CTD`). Case-insensitive;
    /// unrecognized names become [`ServerType::Unknown`].
    pub fn from_name(name: &str) -> Self {
        match name.to_ascii_uppercase().as_str() {
            "VANILLA" => Self::Vanilla,
            "PAPER" => Self::Paper,
            "PUFFERFISH" => Self::Pufferfish,
            "SPIGOT" => Self::Spigot,
            "FOLIA" => Self::Folia,
            "PURPUR" => Self::Purpur,
            "WATERFALL" => Self::Waterfall,
            "VELOCITY" => Self::Velocity,
            "FABRIC" => Self::Fabric,
            "BUNGEECORD" => Self::Bungeecord,
            "QUILT" => Self::Quilt,
            "FORGE" => Self::Forge,
            "NEOFORGE" => Self::Neoforge,
            "MOHIST" => Self::Mohist,
            "ARCLIGHT" => Self::Arclight,
            "SPONGE" => Self::Sponge,
            "LEAVES" => Self::Leaves,
            "CANVAS" => Self::Canvas,
            "ASPAPER" => Self::Aspaper,
            "LEGACY_FABRIC" | "LEGACYFABRIC" => Self::LegacyFabric,
            "LOOHP_LIMBO" | "LOOHPLIMBO" => Self::LoohpLimbo,
            "NANO_LIMBO" | "NANOLIMBO" => Self::Nanolimbo,
            "DIVINEMC" | "DIVINE_MC" => Self::Divinemc,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Name used in API paths, e.g. `PAPER`.
    pub fn api_name(&self) -> String {
        match self {
            Self::Vanilla => "VANILLA",
            Self::Paper => "PAPER",
            Self::Pufferfish => "PUFFERFISH",
            Self::Spigot => "SPIGOT",
            Self::Folia => "FOLIA",
            Self::Purpur => "PURPUR",
            Self::Waterfall => "WATERFALL",
            Self::Velocity => "VELOCITY",
            Self::Fabric => "FABRIC",
            Self::Bungeecord => "BUNGEECORD",
            Self::Quilt => "QUILT",
            Self::Forge => "FORGE",
            Self::Neoforge => "NEOFORGE",
            Self::Mohist => "MOHIST",
            Self::Arclight => "ARCLIGHT",
            Self::Sponge => "SPONGE",
            Self::Leaves => "LEAVES",
            Self::Canvas => "CANVAS",
            Self::Aspaper => "ASPAPER",
            Self::LegacyFabric => "LEGACY_FABRIC",
            Self::LoohpLimbo => "LOOHP_LIMBO",
            Self::Nanolimbo => "NANO_LIMBO",
            Self::Divinemc => "DIVINEMC",
            Self::Unknown(raw) => raw,
        }
        .to_string()
    }
}

impl<'de> serde::Deserialize<'de> for ServerType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(ServerType::from_name(&name))
    }
}

/// Display metadata for one server software project, as returned by
/// `GET /api/v2/types`.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerTypeInfo {
    pub name: String,
    pub homepage: String,
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub experimental: bool,
    pub description: String,
}

/// Server software grouped by how MCJars categorizes them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypeCatalog {
    #[serde(default)]
    pub recommended: IndexMap<ServerType, ServerTypeInfo>,
    #[serde(default)]
    pub established: IndexMap<ServerType, ServerTypeInfo>,
    #[serde(default)]
    pub experimental: IndexMap<ServerType, ServerTypeInfo>,
    #[serde(default)]
    pub miscellaneous: IndexMap<ServerType, ServerTypeInfo>,
    #[serde(default)]
    pub limbos: IndexMap<ServerType, ServerTypeInfo>,
}

impl TypeCatalog {
    pub fn all(&self) -> impl Iterator<Item = (&ServerType, &ServerTypeInfo)> + '_ {
        self.recommended
            .iter()
            .chain(&self.established)
            .chain(&self.experimental)
            .chain(&self.miscellaneous)
            .chain(&self.limbos)
    }
}

/// Whether a project version is a stable release or a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VersionType {
    Release,
    Snapshot,
}

/// One Minecraft (or project) version of a server software, as returned by
/// `GET /api/v2/builds/{type}`. `id` is the version identifier used in
/// subsequent calls (e.g. `1.21.4` for Paper, `3.4.0-SNAPSHOT` for Velocity).
#[derive(Debug, Clone)]
pub struct VersionEntry {
    pub id: String,
    pub info: VersionInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "type")]
    pub version_type: VersionType,
    pub supported: bool,
    pub java: i16,
    pub builds: i64,
    pub latest: Build,
}

/// A downloadable build of a server software version.
#[derive(Debug, Clone, Deserialize)]
pub struct Build {
    pub id: i32,
    #[serde(rename = "versionId", alias = "version_id", default)]
    pub version_id: Option<String>,
    #[serde(rename = "projectVersionId", alias = "project_version_id", default)]
    pub project_version_id: Option<String>,
    pub name: String,
    #[serde(rename = "buildNumber", alias = "build_number")]
    pub build_number: i32,
    #[serde(default)]
    pub experimental: bool,
    #[serde(rename = "jarUrl", alias = "jar_url", default)]
    pub jar_url: Option<String>,
    #[serde(rename = "jarSize", alias = "jar_size", default)]
    pub jar_size: Option<i32>,
    #[serde(default)]
    pub installation: Vec<Vec<InstallationStep>>,
    #[serde(default)]
    pub changes: Vec<String>,
}

impl Build {
    /// Total bytes that installing this build will download.
    pub fn total_download_size(&self) -> u64 {
        let steps: u64 = self
            .installation
            .iter()
            .flatten()
            .filter_map(|step| match step {
                InstallationStep::Download { size, .. } => Some(*size),
                _ => None,
            })
            .sum();
        if steps > 0 {
            steps
        } else {
            self.jar_size.map(|s| s as u64).unwrap_or(0)
        }
    }
}

/// One step of a build's installation procedure. Steps are grouped; groups
/// run in order and steps within a group run in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InstallationStep {
    Download {
        url: String,
        file: String,
        size: u64,
    },
    Unzip {
        file: String,
        location: String,
    },
    Remove {
        location: String,
    },
}

// ---------- errors ----------

#[derive(Debug)]
pub enum McJarsError {
    Request(reqwest::Error),
    Api {
        status: u16,
        errors: Vec<String>,
    },
    Json(serde_json::Error),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Zip {
        path: PathBuf,
        source: zip::result::ZipError,
    },
    /// Installation finished but no `server.jar` exists in the target folder.
    MissingServerJar(PathBuf),
}

impl fmt::Display for McJarsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McJarsError::Request(err) => write!(f, "request failed: {err}"),
            McJarsError::Api { status, errors } => {
                write!(f, "api error ({status}): {}", errors.join(", "))
            }
            McJarsError::Json(err) => write!(f, "invalid api response: {err}"),
            McJarsError::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
            McJarsError::Zip { path, source } => {
                write!(f, "failed to unzip {}: {source}", path.display())
            }
            McJarsError::MissingServerJar(path) => {
                write!(f, "no server.jar found in {}", path.display())
            }
        }
    }
}

impl std::error::Error for McJarsError {}

impl From<reqwest::Error> for McJarsError {
    fn from(err: reqwest::Error) -> Self {
        McJarsError::Request(err)
    }
}

impl From<serde_json::Error> for McJarsError {
    fn from(err: serde_json::Error) -> Self {
        McJarsError::Json(err)
    }
}

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> McJarsError {
    let path = path.into();
    move |source| McJarsError::Io { path, source }
}

/// Every software type with all of its versions, fetched ahead of time so the
/// pickers never wait on the network. Use [`McJarsClient::snapshot`] to build one.
#[derive(Debug, Clone, Default)]
pub struct SoftwareSnapshot {
    pub types: TypeCatalog,
    versions: HashMap<ServerType, Vec<VersionEntry>>,
}

impl SoftwareSnapshot {
    /// Versions of one software, ordered oldest to newest; empty when none are known.
    pub fn versions(&self, server_type: &ServerType) -> &[VersionEntry] {
        self.versions
            .get(server_type)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// Parallel version requests made while building a [`SoftwareSnapshot`].
const SNAPSHOT_WORKERS: usize = 6;

// ---------- client ----------

/// Progress callback: `(downloaded_bytes, total_bytes)`. `total` is 0 when
/// the total size is unknown.
pub type ProgressCallback<'a> = &'a mut dyn FnMut(u64, u64);

/// Blocking HTTP client for the MCJars API. All methods perform network I/O
/// and are meant to run on background threads.
pub struct McJarsClient {
    base: String,
    client: reqwest::blocking::Client,
}

impl McJarsClient {
    pub fn new() -> Self {
        Self::with_base(DEFAULT_BASE_URL)
    }

    pub fn with_base(base: &str) -> Self {
        let client = reqwest::blocking::Client::builder()
            .user_agent(format!("Spoon/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build http client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            client,
        }
    }

    /// All server software, grouped by category.
    pub fn types(&self) -> Result<TypeCatalog, McJarsError> {
        #[derive(Deserialize)]
        struct Response {
            types: TypeCatalog,
        }
        let response: Response = self.get_json("/api/v2/types")?;
        Ok(response.types)
    }

    /// Versions for a server software, ordered oldest to newest by the API.
    pub fn versions(&self, server_type: &ServerType) -> Result<Vec<VersionEntry>, McJarsError> {
        #[derive(Deserialize)]
        struct Response {
            builds: IndexMap<String, VersionInfo>,
        }
        let response: Response =
            self.get_json(&format!("/api/v2/builds/{}", server_type.api_name()))?;
        Ok(response
            .builds
            .into_iter()
            .map(|(id, info)| VersionEntry { id, info })
            .collect())
    }

    /// Fetches all software types and, concurrently, the versions of each.
    ///
    /// A type whose versions fail to load keeps the entry from `previous`
    /// (when given), so a flaky refresh never empties a picker. Only a
    /// failure to load the type list itself is an error.
    pub fn snapshot(
        &self,
        previous: Option<&SoftwareSnapshot>,
    ) -> Result<SoftwareSnapshot, McJarsError> {
        let types = self.types()?;
        let wanted: Vec<&ServerType> = types.all().map(|(server_type, _)| server_type).collect();
        let (next, wanted_ref) = (AtomicUsize::new(0), &wanted);
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..SNAPSHOT_WORKERS.min(wanted_ref.len()) {
                let (tx, next) = (tx.clone(), &next);
                scope.spawn(move || {
                    while let Some(&server_type) =
                        wanted_ref.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        if let Ok(versions) = self.versions(server_type) {
                            let _ = tx.send((server_type.clone(), versions));
                        }
                    }
                });
            }
        });
        drop(tx);
        let mut versions: HashMap<ServerType, Vec<VersionEntry>> = rx.into_iter().collect();
        if let Some(previous) = previous {
            for &server_type in &wanted {
                if !versions.contains_key(server_type) {
                    if let Some(old) = previous.versions.get(server_type) {
                        versions.insert(server_type.clone(), old.clone());
                    }
                }
            }
        }
        Ok(SoftwareSnapshot { types, versions })
    }

    /// Builds for a version, ordered newest first (index 0 = latest).
    pub fn builds(
        &self,
        server_type: &ServerType,
        version: &str,
    ) -> Result<Vec<Build>, McJarsError> {
        #[derive(Deserialize)]
        struct Response {
            builds: Vec<Build>,
        }
        let response: Response = self.get_json(&format!(
            "/api/v2/builds/{}/{}",
            server_type.api_name(),
            version
        ))?;
        Ok(response.builds)
    }

    /// Download and install a build into `dir`, executing the build's
    /// installation steps (downloads, unzips, removals) as needed.
    ///
    /// On success returns the path to the runnable `server.jar`.
    pub fn install_build(
        &self,
        build: &Build,
        dir: &Path,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, McJarsError> {
        fs::create_dir_all(dir).map_err(io_err(dir))?;

        let total = build.total_download_size();
        let mut downloaded = 0u64;

        if build.installation.is_empty() {
            let url = build.jar_url.as_deref().ok_or_else(|| McJarsError::Api {
                status: 0,
                errors: vec![format!(
                    "build {} has neither installation steps nor a jar url",
                    build.name
                )],
            })?;
            let dest = dir.join("server.jar");
            self.download_file(url, &dest, &mut downloaded, total, progress)?;
        } else {
            for group in &build.installation {
                for step in group {
                    match step {
                        InstallationStep::Download { url, file, .. } => {
                            let dest = dir.join(file);
                            self.download_file(url, &dest, &mut downloaded, total, progress)?;
                        }
                        InstallationStep::Unzip { file, location } => {
                            let archive = dir.join(file);
                            let dest = dir.join(location);
                            fs::create_dir_all(&dest).map_err(io_err(&dest))?;
                            let handle = File::open(&archive).map_err(io_err(&archive))?;
                            let mut zip = zip::ZipArchive::new(io::BufReader::new(handle))
                                .map_err(|source| McJarsError::Zip {
                                    path: archive.clone(),
                                    source,
                                })?;
                            zip.extract(&dest).map_err(|source| McJarsError::Zip {
                                path: archive.clone(),
                                source,
                            })?;
                        }
                        InstallationStep::Remove { location } => {
                            let target = dir.join(location);
                            match fs::remove_file(&target) {
                                Ok(()) => {}
                                Err(_) => {
                                    let _ = fs::remove_dir_all(&target);
                                }
                            }
                        }
                    }
                }
            }
        }

        let server_jar = dir.join("server.jar");
        if server_jar.is_file() {
            Ok(server_jar)
        } else {
            Err(McJarsError::MissingServerJar(dir.to_path_buf()))
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, McJarsError> {
        let response = self.client.get(format!("{}{}", self.base, path)).send()?;
        let status = response.status();
        if !status.is_success() {
            #[derive(Deserialize)]
            struct ErrorBody {
                #[serde(default)]
                errors: Vec<String>,
            }
            let errors = response
                .json::<ErrorBody>()
                .map(|body| body.errors)
                .unwrap_or_default();
            return Err(McJarsError::Api {
                status: status.as_u16(),
                errors,
            });
        }
        Ok(response.json::<T>()?)
    }

    fn download_file(
        &self,
        url: &str,
        dest: &Path,
        downloaded: &mut u64,
        total: u64,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), McJarsError> {
        let mut response = self.client.get(url).send()?.error_for_status()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        let mut file = File::create(dest).map_err(io_err(dest))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = response.read(&mut buf).map_err(io_err(dest))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(io_err(dest))?;
            *downloaded += n as u64;
            progress(*downloaded, total);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    const TYPES_JSON: &str = r#"{"types":{"recommended":{
        "PAPER":{"name":"Paper","homepage":"x","description":"d"},
        "FABRIC":{"name":"Fabric","homepage":"x","description":"d"}}}}"#;
    const PAPER_BUILDS_JSON: &str = r#"{"builds":{"1.21":{"type":"RELEASE","supported":true,
        "java":21,"builds":1,"latest":{"id":1,"name":"b","buildNumber":1}}}}"#;

    /// Serves the types list and Paper versions; every Fabric request fails.
    fn stub_api() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap() > 2 {
                        header.clear();
                    }
                    let (status, body) = if request_line.contains("/api/v2/types") {
                        ("200 OK", TYPES_JSON)
                    } else if request_line.contains("/builds/PAPER") {
                        ("200 OK", PAPER_BUILDS_JSON)
                    } else {
                        ("500 Internal Server Error", r#"{"errors":[]}"#)
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = (&stream).write_all(response.as_bytes());
                });
            }
        });
        base
    }

    #[test]
    fn snapshot_collects_versions_and_keeps_previous_on_failure() {
        let client = McJarsClient::with_base(&stub_api());
        let first = client.snapshot(None).unwrap();
        assert_eq!(first.versions(&ServerType::Paper).len(), 1);
        assert!(first.versions(&ServerType::Fabric).is_empty());

        let mut previous = SoftwareSnapshot::default();
        previous.versions.insert(
            ServerType::Fabric,
            first.versions(&ServerType::Paper).to_vec(),
        );
        let refreshed = client.snapshot(Some(&previous)).unwrap();
        assert_eq!(refreshed.versions(&ServerType::Fabric).len(), 1);
        assert_eq!(refreshed.versions(&ServerType::Paper).len(), 1);
    }
}
