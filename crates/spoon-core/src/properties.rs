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
/// Values are escaped the way `java.util.Properties` writes them, so non-ASCII
/// text (e.g. MOTD formatting codes) stays readable on any Java version.
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
                out.push_str(&escape_property_value(value));
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
        out.push_str(&escape_property_value(value));
        out.push('\n');
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, out)
}

/// Reads the last value set for `key` in a `.properties`-style file, decoding
/// `java.util.Properties` escape sequences (`\uXXXX`, `\\`, `\t`, `\n`, `\r`).
/// Returns `None` when the file or key is missing.
pub fn read_property(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            (!k.trim_start().starts_with(['#', '!'])).then_some((k.trim(), v))
        })
        .rev()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| unescape_property_value(v.trim()))
}

/// Escapes a value the way `java.util.Properties.store` does: backslash and
/// control characters get escaped and everything outside printable ASCII
/// becomes `\uXXXX` (astral characters as surrogate pairs).
pub fn escape_property_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let code = c as u32;
                if code > 0xffff {
                    let v = code - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (v >> 10),
                        0xdc00 + (v & 0x3ff)
                    ));
                } else {
                    out.push_str(&format!("\\u{code:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape_property_value`]; unknown escapes keep their character
/// (matching `java.util.Properties.load`, which drops the backslash).
pub fn unescape_property_value(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut out = String::with_capacity(value.len());
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let Some(unit) = read_hex4(&mut chars) else {
                    out.push('\\');
                    continue;
                };
                if (0xd800..=0xdbff).contains(&unit) {
                    // High surrogate: combine with a directly following low surrogate.
                    let mut lookahead = chars.clone();
                    if lookahead.next() == Some('\\') && lookahead.next() == Some('u') {
                        if let Some(low) = read_hex4(&mut lookahead) {
                            if (0xdc00..=0xdfff).contains(&low) {
                                chars.next();
                                chars.next();
                                read_hex4(&mut chars);
                                let combined = 0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00);
                                if let Some(c) = char::from_u32(combined) {
                                    out.push(c);
                                }
                                continue;
                            }
                        }
                    }
                } else if let Some(c) = char::from_u32(unit) {
                    out.push(c);
                }
            }
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn read_hex4(chars: &mut impl Iterator<Item = char>) -> Option<u32> {
    let hex: String = chars.take(4).collect();
    if hex.len() < 4 {
        return None;
    }
    u32::from_str_radix(&hex, 16).ok()
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

    #[test]
    fn escapes_and_reads_back_formatting_codes() {
        let dir = temp_dir();
        let path = dir.join("server.properties");
        let motd = "§aGreen §9Blue \\o/ 🚀".to_string();
        set_properties(
            &path,
            &[
                ("motd".to_string(), motd.clone()),
                (
                    "resource-pack".to_string(),
                    "https://example.com/pack.zip".into(),
                ),
            ],
        )
        .unwrap();

        // Non-ASCII is stored \uXXXX-escaped like java.util.Properties writes it
        // (astral characters as surrogate pairs).
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("motd=\\u00a7aGreen \\u00a79Blue \\\\o/ \\ud83d\\ude80\n"));
        assert!(text.contains("resource-pack=https://example.com/pack.zip\n"));
        assert_eq!(read_property(&path, "motd"), Some(motd));
        assert_eq!(
            read_property(&path, "resource-pack"),
            Some("https://example.com/pack.zip".into())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_property_returns_last_value_and_skips_comments() {
        let dir = temp_dir();
        let path = dir.join("server.properties");
        fs::write(
            &path,
            "#motd=commented\nmotd=first\n! also a comment\n  motd  = second \nother=x\n",
        )
        .unwrap();
        assert_eq!(read_property(&path, "motd"), Some("second".into()));
        assert_eq!(read_property(&path, "other"), Some("x".into()));
        assert_eq!(read_property(&path, "missing"), None);
        assert_eq!(read_property(&dir.join("absent.properties"), "motd"), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn escapes_match_java_properties() {
        assert_eq!(escape_property_value("a=b:c"), "a=b:c");
        assert_eq!(escape_property_value("tab\there"), "tab\\there");
        assert_eq!(escape_property_value("§"), "\\u00a7");
        assert_eq!(unescape_property_value("\\u00a7x"), "§x");
        assert_eq!(unescape_property_value("a\\tb\\\\c\\q"), "a\tb\\cq");
        assert_eq!(unescape_property_value("trailing\\"), "trailing\\");
        // Round-trip anything the GUI can type.
        let value = "motd §lBold§r \\ \u{1f600}\n".to_string();
        assert_eq!(
            unescape_property_value(&escape_property_value(&value)),
            value
        );
    }
}
