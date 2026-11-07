use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Static configuration for a single Minecraft server instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub server_type: Option<String>,
    #[serde(default)]
    pub server_version: Option<String>,
    /// Per-server Java runtime override: the id of a managed runtime, or `None`
    /// to use the app-wide default.
    #[serde(default)]
    pub java_runtime: Option<String>,
    /// Path to the server jar (e.g. paper-1.21.jar).
    pub jar_path: PathBuf,
    /// Directory the server process runs in (world/, server.properties live here).
    pub working_dir: PathBuf,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub jvm_args: Vec<String>,
    pub server_args: Vec<String>,
    /// `gamemode` written to server.properties when the server is created.
    #[serde(default = "default_gamemode")]
    pub gamemode: String,
    /// `difficulty` written to server.properties when the server is created.
    #[serde(default = "default_difficulty")]
    pub difficulty: String,
    /// `level-type` written to server.properties when the server is created.
    #[serde(default = "default_level_type")]
    pub level_type: String,
    /// `max-players` written to server.properties when the server is created.
    #[serde(default = "default_max_players")]
    pub max_players: u32,
    /// `server-port`; defaults to 25565 like a stock Minecraft server.
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    /// `level-seed`; empty means the server picks a random seed.
    #[serde(default)]
    pub level_seed: String,
    /// `online-mode` written to server.properties when the server is created.
    #[serde(default = "default_true")]
    pub online_mode: bool,
    /// `white-list` written to server.properties when the server is created.
    #[serde(default = "default_true")]
    pub use_whitelist: bool,
}

fn default_gamemode() -> String {
    "survival".to_string()
}

fn default_difficulty() -> String {
    "easy".to_string()
}

fn default_level_type() -> String {
    "minecraft:normal".to_string()
}

fn default_max_players() -> u32 {
    20
}

fn default_server_port() -> u16 {
    crate::port::DEFAULT_SERVER_PORT
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    pub fn new(
        id: String,
        name: String,
        jar_path: PathBuf,
        working_dir: PathBuf,
        max_memory_mb: u32,
    ) -> Self {
        Self {
            id,
            name,
            server_type: None,
            server_version: None,
            java_runtime: None,
            jar_path,
            working_dir,
            min_memory_mb: 512,
            max_memory_mb,
            jvm_args: Vec::new(),
            server_args: Vec::new(),
            gamemode: default_gamemode(),
            difficulty: default_difficulty(),
            level_type: default_level_type(),
            max_players: default_max_players(),
            server_port: default_server_port(),
            level_seed: String::new(),
            online_mode: default_true(),
            use_whitelist: default_true(),
        }
    }

    /// Full command line used to launch the server, for display purposes.
    pub fn command_line(&self) -> String {
        let mut parts = vec![
            "java".to_string(),
            format!("-Xms{}M", self.min_memory_mb),
            format!("-Xmx{}M", self.max_memory_mb),
        ];
        parts.extend(self.jvm_args.iter().cloned());
        parts.push("-jar".to_string());
        parts.push(self.jar_path.display().to_string());
        parts.extend(self.server_args.iter().cloned());
        parts.push("--nogui".to_string());
        parts.join(" ")
    }
}

/// Settings for the app as a whole, persisted next to the server list as
/// `settings.json` in the platform config directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppSettings {
    /// Id of the Java runtime Spoon should launch servers with; `None` means
    /// the system `java` from PATH.
    #[serde(default)]
    pub java_runtime: Option<String>,
}

impl AppSettings {
    pub fn load(dir: &Path) -> Self {
        fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        fs::write(dir.join("settings.json"), text)
    }
}

/// Persists the list of server instances to a JSON file in the platform
/// config directory (e.g. `%APPDATA%/Spoon/servers.json` on Windows).
#[derive(Debug)]
pub struct ServerStore {
    dir: PathBuf,
    path: PathBuf,
}

impl ServerStore {
    pub fn new() -> Option<Self> {
        let dir = dirs::config_dir()?.join("Spoon");
        Some(Self {
            path: dir.join("servers.json"),
            dir,
        })
    }

    /// Root folder that contains one subfolder per server instance.
    pub fn servers_dir(&self) -> PathBuf {
        self.dir.join("servers")
    }

    /// The Spoon config directory (`AppSettings` lives here).
    pub fn config_dir(&self) -> &Path {
        &self.dir
    }

    pub fn load(&self) -> Vec<ServerConfig> {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self, servers: &[ServerConfig]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(servers).map_err(std::io::Error::other)?;
        fs::write(&self.path, text)
    }
}

/// Generate a unique id for a new server instance.
pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:032x}")
}

/// Turn a server name into a safe folder name. Invalid characters become
/// underscores, trailing dots/spaces are stripped, and Windows reserved
/// device names (CON, COM1, …) get a trailing underscore.
pub fn sanitize_folder_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let out = out.trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() {
        return "server".to_string();
    }
    let upper = out.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or(&upper);
    let reserved = matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("{out}_")
    } else {
        out
    }
}

/// Returns a server folder path for `name` that does not collide with any
/// existing folder, e.g. `Survival`, `Survival-2`, `Survival-3`, …
pub fn unique_server_dir(servers_root: &Path, name: &str) -> PathBuf {
    let base = sanitize_folder_name(name);
    let mut candidate = servers_root.join(&base);
    let mut n = 2;
    while candidate.exists() {
        candidate = servers_root.join(format!("{base}-{n}"));
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::ServerConfig;

    #[test]
    fn loads_older_server_configs_without_version_metadata() {
        let json = r#"{
            "id": "legacy",
            "name": "Legacy",
            "jar_path": "server.jar",
            "working_dir": "server",
            "min_memory_mb": 512,
            "max_memory_mb": 4096,
            "jvm_args": [],
            "server_args": []
        }"#;
        let config: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.server_type, None);
        assert_eq!(config.server_version, None);
        assert_eq!(config.java_runtime, None);
        assert_eq!(config.gamemode, "survival");
        assert_eq!(config.difficulty, "easy");
        assert_eq!(config.level_type, "minecraft:normal");
        assert_eq!(config.max_players, 20);
        assert_eq!(config.server_port, 25565);
        assert_eq!(config.level_seed, "");
        assert!(config.online_mode);
        assert!(config.use_whitelist);
    }

    #[test]
    fn roundtrips_world_and_network_settings() {
        let mut config = ServerConfig::new(
            "id".into(),
            "Name".into(),
            "server.jar".into(),
            "server".into(),
            4096,
        );
        config.gamemode = "creative".into();
        config.difficulty = "hard".into();
        config.level_type = "minecraft:flat".into();
        config.max_players = 100;
        config.server_port = 25599;
        config.level_seed = "42".into();
        config.online_mode = false;
        config.use_whitelist = false;
        let text = serde_json::to_string(&config).unwrap();
        let parsed: ServerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.gamemode, "creative");
        assert_eq!(parsed.difficulty, "hard");
        assert_eq!(parsed.level_type, "minecraft:flat");
        assert_eq!(parsed.max_players, 100);
        assert_eq!(parsed.server_port, 25599);
        assert_eq!(parsed.level_seed, "42");
        assert!(!parsed.online_mode);
        assert!(!parsed.use_whitelist);
    }

    #[test]
    fn roundtrips_java_runtime_override() {
        let mut config = ServerConfig::new(
            "id".into(),
            "Name".into(),
            "server.jar".into(),
            "server".into(),
            4096,
        );
        config.java_runtime = Some("jdk-21.0.5-11".into());
        let text = serde_json::to_string(&config).unwrap();
        let parsed: ServerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.java_runtime.as_deref(), Some("jdk-21.0.5-11"));
    }
}
