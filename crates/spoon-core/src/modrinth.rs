//! Client for the Modrinth API (https://docs.modrinth.com), used to browse and
//! download Minecraft plugins and mods for a server.
//!
//! Typical flow for installing content on a server:
//! 1. [`ContentSupport::for_software`] — decide what the server's software accepts
//! 2. [`ModrinthClient::search`] — browse compatible projects
//! 3. [`ModrinthClient::versions`] — list a project's versions filtered to the
//!    server's loaders (and game version)
//! 4. [`ModrinthClient::install_version`] — download into `plugins/` or `mods/`

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_BASE_URL: &str = "https://api.modrinth.com/v2";
/// Project icons live on Modrinth's CDN, not the API host.
pub const DEFAULT_CDN_URL: &str = "https://cdn.modrinth.com";

/// What Modrinth content a server software can install: which project types to
/// search, which loaders/categories they must support, and the folder installs
/// land in. `None` from [`ContentSupport::for_software`] means the software has
/// no Modrinth-compatible content (e.g. vanilla).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentSupport {
    /// Modrinth project types to search, e.g. `["plugin"]` or `["mod"]`.
    pub project_types: &'static [&'static str],
    /// Modrinth loaders/categories the software is compatible with. Search
    /// matches any of them (Paper also runs Spigot/Bukkit plugins, etc.).
    pub loaders: &'static [&'static str],
    /// Folder inside the server directory that installs land in.
    pub install_dir: &'static str,
    /// UI label for this content: "Plugins" or "Mods".
    pub label: &'static str,
}

fn plugins(loaders: &'static [&'static str]) -> ContentSupport {
    ContentSupport {
        project_types: &["plugin"],
        loaders,
        install_dir: "plugins",
        label: "Plugins",
    }
}

fn mods(loaders: &'static [&'static str]) -> ContentSupport {
    ContentSupport {
        project_types: &["mod"],
        loaders,
        install_dir: "mods",
        label: "Mods",
    }
}

impl ContentSupport {
    /// The content a server software accepts, keyed by the MCJars API name
    /// stored in `ServerConfig.server_type` (case-insensitive). Hybrid
    /// software (Mohist, Arclight, …) accepts both plugins and mods.
    pub fn for_software(server_type: Option<&str>) -> Option<ContentSupport> {
        let name = server_type?.to_ascii_uppercase();
        let support = match name.as_str() {
            // Paper family: run Bukkit/Spigot plugins too.
            "PAPER" | "ASPAPER" | "PUFFERFISH" | "LEAVES" => {
                plugins(&["paper", "spigot", "bukkit"])
            }
            "PURPUR" | "DIVINEMC" => plugins(&["purpur", "paper", "spigot", "bukkit"]),
            "FOLIA" | "CANVAS" => plugins(&["folia", "paper"]),
            "SPIGOT" => plugins(&["spigot", "bukkit"]),
            "VELOCITY" => plugins(&["velocity"]),
            "WATERFALL" => plugins(&["waterfall", "bungeecord"]),
            "BUNGEECORD" => plugins(&["bungeecord", "waterfall"]),
            "SPONGE" => plugins(&["sponge"]),
            "FABRIC" | "LEGACY_FABRIC" => mods(&["fabric"]),
            "QUILT" => mods(&["quilt", "fabric"]),
            "FORGE" => mods(&["forge"]),
            "NEOFORGE" => mods(&["neoforge"]),
            "MOHIST" => ContentSupport {
                project_types: &["plugin", "mod"],
                loaders: &["paper", "spigot", "bukkit", "forge"],
                install_dir: "mods",
                label: "Plugins & Mods",
            },
            "ARCLIGHT" => ContentSupport {
                project_types: &["plugin", "mod"],
                loaders: &["paper", "spigot", "bukkit", "forge", "neoforge"],
                install_dir: "mods",
                label: "Plugins & Mods",
            },
            // Vanilla, limbo servers and unknown/custom jars accept nothing.
            _ => return None,
        };
        Some(support)
    }

    /// The folder a project type installs into, relative to the server
    /// directory. Plugins always land in `plugins/`; everything else uses the
    /// support's install folder (`mods/`).
    pub fn install_dir_for(&self, project_type: &str) -> &'static str {
        if project_type == "plugin" {
            "plugins"
        } else {
            self.install_dir
        }
    }
}

// ---------- API models ----------

/// How search results are sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIndex {
    Relevance,
    Downloads,
    Follows,
    Newest,
    Updated,
}

impl SearchIndex {
    pub const ALL: [SearchIndex; 5] = [
        SearchIndex::Relevance,
        SearchIndex::Downloads,
        SearchIndex::Follows,
        SearchIndex::Newest,
        SearchIndex::Updated,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SearchIndex::Relevance => "Most relevant",
            SearchIndex::Downloads => "Most downloaded",
            SearchIndex::Follows => "Most followed",
            SearchIndex::Newest => "Newest",
            SearchIndex::Updated => "Recently updated",
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            SearchIndex::Relevance => "relevance",
            SearchIndex::Downloads => "downloads",
            SearchIndex::Follows => "follows",
            SearchIndex::Newest => "newest",
            SearchIndex::Updated => "updated",
        }
    }
}

/// One project in search results. `slug` is the vanity name used in API paths,
/// falling back to `id` when absent.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectHit {
    #[serde(rename = "project_id")]
    pub id: String,
    pub slug: Option<String>,
    pub title: String,
    pub description: String,
    pub author: String,
    #[serde(rename = "project_type")]
    pub project_type: String,
    #[serde(default)]
    pub categories: Vec<String>,
    pub downloads: u64,
    pub follows: u64,
    pub icon_url: Option<String>,
}

impl ProjectHit {
    /// Identifier accepted by the versions endpoint.
    pub fn project_ref(&self) -> &str {
        self.slug.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub hits: Vec<ProjectHit>,
    pub total_hits: usize,
}

/// The release channel of a project version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionType {
    Release,
    Beta,
    Alpha,
}

impl VersionType {
    pub fn label(&self) -> &'static str {
        match self {
            VersionType::Release => "Release",
            VersionType::Beta => "Beta",
            VersionType::Alpha => "Alpha",
        }
    }
}

/// One downloadable version of a project, as returned by
/// `GET /project/{id|slug}/version`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectVersion {
    pub id: String,
    pub name: String,
    pub version_number: String,
    #[serde(rename = "version_type")]
    pub channel: VersionType,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    pub downloads: u64,
    #[serde(default)]
    pub files: Vec<VersionFile>,
}

impl ProjectVersion {
    /// The file to install: the primary one, or the first listed.
    pub fn primary_file(&self) -> Option<&VersionFile> {
        self.files
            .iter()
            .find(|file| file.primary)
            .or_else(|| self.files.first())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    pub url: String,
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    pub primary: bool,
}

/// A plugin/mod jar already present in a server folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFile {
    pub name: String,
    pub path: PathBuf,
    /// Which folder it lives in ("plugins" or "mods").
    pub folder: &'static str,
}

/// Scan the folders a software installs into for jar files, sorted by folder
/// then file name.
pub fn installed_content(server_dir: &Path, support: &ContentSupport) -> Vec<InstalledFile> {
    let mut folders: Vec<&'static str> = support
        .project_types
        .iter()
        .map(|project_type| support.install_dir_for(project_type))
        .collect();
    folders.sort_unstable();
    folders.dedup();

    let mut files = Vec::new();
    for folder in folders {
        let dir = server_dir.join(folder);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jar") {
                files.push(InstalledFile {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    path,
                    folder,
                });
            }
        }
    }
    files.sort_by(|a, b| a.folder.cmp(b.folder).then_with(|| a.name.cmp(&b.name)));
    files
}

// ---------- errors ----------

#[derive(Debug)]
pub enum ModrinthError {
    Request(reqwest::Error),
    Api { status: u16, message: String },
    Json(serde_json::Error),
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for ModrinthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModrinthError::Request(err) => write!(f, "request failed: {err}"),
            ModrinthError::Api { status, message } => {
                write!(f, "api error ({status}): {message}")
            }
            ModrinthError::Json(err) => write!(f, "invalid api response: {err}"),
            ModrinthError::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ModrinthError {}

impl From<reqwest::Error> for ModrinthError {
    fn from(err: reqwest::Error) -> Self {
        ModrinthError::Request(err)
    }
}

impl From<serde_json::Error> for ModrinthError {
    fn from(err: serde_json::Error) -> Self {
        ModrinthError::Json(err)
    }
}

fn io_err(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> ModrinthError {
    let path = path.into();
    move |source| ModrinthError::Io { path, source }
}

// ---------- client ----------

/// Blocking HTTP client for the Modrinth API. All methods perform network I/O
/// and are meant to run on background threads.
pub struct ModrinthClient {
    base: String,
    client: reqwest::blocking::Client,
}

impl Default for ModrinthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ModrinthClient {
    pub fn new() -> Self {
        Self::with_base(DEFAULT_BASE_URL)
    }

    pub fn with_base(base: &str) -> Self {
        let client = reqwest::blocking::Client::builder()
            // Modrinth requires a uniquely identifying User-Agent.
            .user_agent(format!("Spoon/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build http client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            client,
        }
    }

    /// Search projects compatible with `support`, sorted by `index`.
    pub fn search(
        &self,
        query: &str,
        support: &ContentSupport,
        index: SearchIndex,
        offset: usize,
        limit: usize,
    ) -> Result<SearchResults, ModrinthError> {
        #[derive(Deserialize)]
        struct Response {
            hits: Vec<ProjectHit>,
            #[serde(default)]
            total_hits: usize,
        }
        let offset = offset.to_string();
        let limit = limit.to_string();
        let response: Response =
            self.send_json(self.client.get(format!("{}/search", self.base)).query(&[
                ("query", query),
                ("facets", search_facets(support).as_str()),
                ("index", index.as_str()),
                ("offset", &offset),
                ("limit", &limit),
            ]))?;
        Ok(SearchResults {
            hits: response.hits,
            total_hits: response.total_hits,
        })
    }

    /// Versions of a project filtered to the server's loaders, and to
    /// `game_version` when given. Newest first as returned by the API.
    pub fn versions(
        &self,
        project: &str,
        support: &ContentSupport,
        game_version: Option<&str>,
    ) -> Result<Vec<ProjectVersion>, ModrinthError> {
        let loaders = serde_json::to_string(&support.loaders)?;
        let request = self
            .client
            .get(format!("{}/project/{}/version", self.base, project))
            .query(&[
                ("loaders", loaders.as_str()),
                ("include_changelog", "false"),
            ]);
        let request = match game_version {
            Some(version) => {
                let game_versions = serde_json::to_string(&[version])?;
                request.query(&[("game_versions", game_versions.as_str())])
            }
            None => request,
        };
        self.send_json(request)
    }

    /// Download a version file into `dir`, returning the installed file's
    /// path. `progress` reports `(downloaded, total)` bytes.
    pub fn install_file(
        &self,
        file: &VersionFile,
        dir: &Path,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, ModrinthError> {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let dest = dir.join(&file.filename);
        let mut response = self.client.get(&file.url).send()?.error_for_status()?;
        let mut out = File::create(&dest).map_err(io_err(&dest))?;
        let mut buf = [0u8; 64 * 1024];
        let mut downloaded = 0u64;
        loop {
            let n = response.read(&mut buf).map_err(io_err(&dest))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(io_err(&dest))?;
            downloaded += n as u64;
            progress(downloaded, file.size);
        }
        Ok(dest)
    }

    fn send_json<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<T, ModrinthError> {
        let response = request.send()?;
        let status = response.status();
        if !status.is_success() {
            #[derive(Deserialize)]
            struct ErrorBody {
                #[serde(default)]
                description: String,
                #[serde(default)]
                error: String,
            }
            let message = response
                .json::<ErrorBody>()
                .map(|body| {
                    if body.description.is_empty() {
                        body.error
                    } else {
                        body.description
                    }
                })
                .unwrap_or_else(|_| "unknown error".to_string());
            return Err(ModrinthError::Api {
                status: status.as_u16(),
                message,
            });
        }
        Ok(response.json::<T>()?)
    }
}

/// Fetch a project icon's image bytes (used by the GUI's project list).
pub fn fetch_project_icon(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("Spoon/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client.get(url).send().map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|err| err.to_string())
}

/// Build the `facets` query value for a content search: the project's type must
/// match (OR within one array) and one of the software's loaders must match.
fn search_facets(support: &ContentSupport) -> String {
    let groups = [
        support
            .project_types
            .iter()
            .map(|project_type| format!("\"project_type:{project_type}\""))
            .collect::<Vec<_>>(),
        support
            .loaders
            .iter()
            .map(|loader| format!("\"categories:{loader}\""))
            .collect::<Vec<_>>(),
    ];
    let groups = groups
        .iter()
        .map(|group| format!("[{}]", group.join(",")))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{groups}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    const SEARCH_JSON: &str = r#"{"hits":[{"project_id":"AABBCCDD","slug":"luckperms",
        "title":"LuckPerms","description":"Permissions","author":"Luck","project_type":"plugin",
        "categories":["paper","velocity"],"downloads":1234567,"follows":42,
        "icon_url":"https://cdn.modrinth.com/data/AABBCCDD/icon.png"},
        {"project_id":"EEFFGGHH","slug":null,"title":"NoSlug","description":"d","author":"a",
        "project_type":"plugin","categories":[],"downloads":5,"follows":0,"icon_url":null}],
        "offset":0,"limit":2,"total_hits":9}"#;
    const VERSIONS_JSON: &str = r#"[{"id":"IIJJKKLL","name":"LuckPerms 5.4",
        "version_number":"5.4.0","version_type":"release","game_versions":["1.20.1","1.20.4"],
        "loaders":["paper","velocity"],"downloads":100,"files":[
        {"url":"https://cdn.modrinth.com/f.jar","filename":"luckperms.jar","size":10,
         "primary":false},
        {"url":"https://cdn.modrinth.com/f2.jar","filename":"luckperms-2.jar","size":20,
         "primary":true}]},
        {"id":"MMNNOOPP","name":"Beta","version_number":"5.5.0-beta","version_type":"beta",
        "game_versions":["1.21"],"loaders":["paper"],"downloads":5,"files":[
        {"url":"https://cdn.modrinth.com/f3.jar","filename":"luckperms-beta.jar","size":30,
         "primary":true}]}]"#;

    /// Records the last search request line and serves canned responses.
    fn stub_api() -> (String, Arc<std::sync::Mutex<Option<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(None));
        let seen_thread = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = seen_thread.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    let mut header = String::new();
                    while reader.read_line(&mut header).unwrap() > 2 {
                        header.clear();
                    }
                    *seen.lock().unwrap() = Some(request_line.clone());
                    let (status, body) = if request_line.contains("/search") {
                        ("200 OK", SEARCH_JSON)
                    } else if request_line.contains("/project/luckperms/version") {
                        ("200 OK", VERSIONS_JSON)
                    } else {
                        (
                            "500 Internal Server Error",
                            r#"{"error":"oops","description":"boom"}"#,
                        )
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
        (base, seen)
    }

    fn temp_root() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("spoon-modrinth-test-{nanos}"))
    }

    #[test]
    fn support_tracks_server_software() {
        let paper = ContentSupport::for_software(Some("PAPER")).unwrap();
        assert_eq!(paper.label, "Plugins");
        assert_eq!(paper.project_types, &["plugin"]);
        assert!(paper.loaders.contains(&"paper") && paper.loaders.contains(&"spigot"));
        assert_eq!(paper.install_dir_for("plugin"), "plugins");

        let purpur = ContentSupport::for_software(Some("purpur")).unwrap();
        assert!(purpur.loaders.contains(&"purpur"));

        let fabric = ContentSupport::for_software(Some("FABRIC")).unwrap();
        assert_eq!(fabric.label, "Mods");
        assert_eq!(fabric.install_dir_for("mod"), "mods");

        let mohist = ContentSupport::for_software(Some("MOHIST")).unwrap();
        assert_eq!(mohist.project_types, &["plugin", "mod"]);
        assert_eq!(mohist.install_dir_for("plugin"), "plugins");
        assert_eq!(mohist.install_dir_for("mod"), "mods");

        assert!(ContentSupport::for_software(Some("VANILLA")).is_none());
        assert!(ContentSupport::for_software(Some("NANO_LIMBO")).is_none());
        assert!(ContentSupport::for_software(None).is_none());
        assert!(ContentSupport::for_software(Some("mystery")).is_none());
    }

    #[test]
    fn facets_and_or_project_type_and_loaders() {
        let facets = search_facets(&ContentSupport::for_software(Some("paper")).unwrap());
        assert_eq!(
            facets,
            r#"[["project_type:plugin"],["categories:paper","categories:spigot","categories:bukkit"]]"#
        );
        let facets = search_facets(&ContentSupport::for_software(Some("mohist")).unwrap());
        assert!(facets.contains(r#""project_type:plugin""#));
        assert!(facets.contains(r#""project_type:mod""#));
        assert!(facets.contains(r#""categories:forge""#));
    }

    #[test]
    fn search_sends_facets_and_parses_hits() {
        let (base, seen) = stub_api();
        let client = ModrinthClient::with_base(&base);
        let support = ContentSupport::for_software(Some("paper")).unwrap();
        let results = client
            .search("perms", &support, SearchIndex::Downloads, 0, 2)
            .unwrap();
        assert_eq!(results.total_hits, 9);
        assert_eq!(results.hits.len(), 2);
        assert_eq!(results.hits[0].title, "LuckPerms");
        assert_eq!(results.hits[0].project_ref(), "luckperms");
        assert_eq!(results.hits[1].project_ref(), "EEFFGGHH");

        let request = seen.lock().unwrap().clone().unwrap();
        assert!(
            request.starts_with("GET /search?"),
            "unexpected request: {request}"
        );
        assert!(
            request.contains("query=perms"),
            "unexpected request: {request}"
        );
        assert!(
            request.contains("index=downloads"),
            "unexpected request: {request}"
        );
        assert!(request.contains("facets="), "unexpected request: {request}");
        assert!(request.contains("limit=2"), "unexpected request: {request}");
    }

    #[test]
    fn versions_parse_and_pick_primary_file() {
        let (base, _seen) = stub_api();
        let client = ModrinthClient::with_base(&base);
        let support = ContentSupport::for_software(Some("velocity")).unwrap();
        let versions = client
            .versions("luckperms", &support, Some("1.20.1"))
            .unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].channel, VersionType::Release);
        assert_eq!(versions[1].channel.label(), "Beta");
        let file = versions[0].primary_file().unwrap();
        assert_eq!(file.filename, "luckperms-2.jar");
        assert_eq!(file.size, 20);
    }

    #[test]
    fn api_errors_surface_message() {
        let (base, _seen) = stub_api();
        let client = ModrinthClient::with_base(&base);
        let support = ContentSupport::for_software(Some("paper")).unwrap();
        // The stub 500s any path it doesn't know, with a Modrinth-style body.
        let err = client.versions("unknown", &support, None).unwrap_err();
        match err {
            ModrinthError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("expected api error, got {other}"),
        }
    }

    #[test]
    fn installed_content_lists_jars_sorted() {
        let root = temp_root();
        let support = ContentSupport::for_software(Some("MOHIST")).unwrap();
        fs::create_dir_all(root.join("plugins")).unwrap();
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::write(root.join("plugins/b.jar"), b"x").unwrap();
        fs::write(root.join("plugins/a.jar"), b"x").unwrap();
        fs::write(root.join("plugins/readme.txt"), b"x").unwrap();
        fs::write(root.join("mods/c.jar"), b"x").unwrap();

        let files = installed_content(&root, &support);
        let names: Vec<(&str, &str)> = files
            .iter()
            .map(|file| (file.folder, file.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("mods", "c.jar"),
                ("plugins", "a.jar"),
                ("plugins", "b.jar"),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn pure_plugin_software_scans_plugins_only() {
        let root = temp_root();
        let support = ContentSupport::for_software(Some("PAPER")).unwrap();
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::write(root.join("mods/ignored.jar"), b"x").unwrap();
        assert!(installed_content(&root, &support).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
