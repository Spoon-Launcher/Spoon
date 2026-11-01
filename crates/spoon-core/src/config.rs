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
