//! Writing `server.properties` for a server instance.

use std::fs;
use std::io;
use std::path::Path;

use crate::config::ServerConfig;

/// Writes the settings Spoon manages into `server.properties` inside the
/// server's working directory, replacing existing keys and appending missing
/// ones while leaving every other line (comments, unknown keys) untouched.
/// Creates the file when absent.
pub fn write_server_properties(config: &ServerConfig) -> io::Result<()> {
    let mut entries: Vec<(String, String)> = vec![
        ("gamemode".to_string(), config.gamemode.clone()),
        ("difficulty".to_string(), config.difficulty.clone()),
        ("level-type".to_string(), config.level_type.clone()),
        ("max-players".to_string(), config.max_players.to_string()),
        ("server-port".to_string(), config.server_port.to_string()),
        ("online-mode".to_string(), config.online_mode.to_string()),
        ("white-list".to_string(), config.use_whitelist.to_string()),
    ];
    if !config.level_seed.trim().is_empty() {
        entries.push((
            "level-seed".to_string(),
            config.level_seed.trim().to_string(),
        ));
    }
    set_properties(&config.working_dir.join("server.properties"), &entries)
}

/// Sets `entries` (key/value) in a Java `.properties`-style file, preserving
/// existing content. The value of the last matching `key=` line wins, so
/// entries replace all earlier occurrences and are appended when missing.
pub fn set_properties(path: &Path, entries: &[(String, String)]) -> io::Result<()> {
    let contents = fs::read_to_string(path).unwrap_or_default();

    let mut out = String::new();
    let mut remaining: Vec<&(String, String)> = entries.iter().collect();
    for line in contents.lines() {
        let key = line
            .split_once('=')
            .map(|(key, _)| key.trim())
            .filter(|key| !key.starts_with('#') && !key.starts_with('!'));
        match key.and_then(|key| remaining.iter().position(|(k, _)| k == key)) {
            Some(position) => {
                let (key, value) = remaining.remove(position);
                out.push_str(key);
                out.push('=');
                out.push_str(value);
                out.push('\n');
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (key, value) in remaining {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::new_id;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spoon-properties-test-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_config(dir: &Path) -> ServerConfig {
        let mut config = ServerConfig::new(
            new_id(),
            "Test".into(),
            dir.join("server.jar"),
            dir.to_path_buf(),
            4096,
        );
        config.gamemode = "creative".into();
        config.difficulty = "hard".into();
        config.level_type = "minecraft:flat".into();
        config.max_players = 10;
        config.server_port = 25570;
        config.online_mode = false;
        config.use_whitelist = true;
        config
    }

    #[test]
    fn creates_properties_with_managed_keys() {
        let dir = temp_dir();
        let config = sample_config(&dir);
        write_server_properties(&config).unwrap();

        let text = fs::read_to_string(dir.join("server.properties")).unwrap();
        assert!(text.contains("gamemode=creative\n"));
        assert!(text.contains("difficulty=hard\n"));
        assert!(text.contains("level-type=minecraft:flat\n"));
        assert!(text.contains("max-players=10\n"));
        assert!(text.contains("server-port=25570\n"));
        assert!(text.contains("online-mode=false\n"));
        assert!(text.contains("white-list=true\n"));
        // Empty seed stays unset so the server generates a random world.
        assert!(!text.contains("level-seed"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_seed_only_when_set() {
        let dir = temp_dir();
        let mut config = sample_config(&dir);
        config.level_seed = "  -123  ".into();
        write_server_properties(&config).unwrap();

        let text = fs::read_to_string(dir.join("server.properties")).unwrap();
        assert!(text.contains("level-seed=-123\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_existing_keys_and_keeps_other_lines() {
        let dir = temp_dir();
        fs::write(
            dir.join("server.properties"),
            "# header comment\nmax-players=20\nserver-port=25565\nmotd=A server\n",
        )
        .unwrap();
        let config = sample_config(&dir);
        write_server_properties(&config).unwrap();

        let text = fs::read_to_string(dir.join("server.properties")).unwrap();
        assert!(text.contains("# header comment\n"));
        assert!(text.contains("motd=A server\n"));
        // Old values replaced, not duplicated.
        assert_eq!(text.matches("max-players=").count(), 1);
        assert_eq!(text.matches("server-port=").count(), 1);
        assert!(text.contains("max-players=10\n"));
        assert!(text.contains("server-port=25570\n"));
        // New keys appended.
        assert!(text.contains("gamemode=creative\n"));
        let _ = fs::remove_dir_all(&dir);
    }
}
