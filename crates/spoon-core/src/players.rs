use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const PLAYER_SKIN_BASE: &str = "https://mc-heads.net/avatar";

/// Player information available from a Minecraft server's files and logs.
#[derive(Debug, Default, PartialEq)]
pub struct PlayerDetails {
    pub uuid: Option<String>,
    pub play_time_ticks: Option<u64>,
    pub ip_address: Option<String>,
    pub last_position: Option<[f64; 3]>,
}

/// A player's cached Minecraft name and UUID.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PlayerProfile {
    /// Player's canonical username.
    pub name: String,
    /// UUID formatted with hyphens.
    pub uuid: String,
}

/// Known server players and access lists loaded from server files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlayerCatalog {
    /// Players retained in the server's `usercache.json`.
    pub known_players: Vec<PlayerProfile>,
    /// Players present in the server's `whitelist.json`.
    pub whitelist: Vec<PlayerProfile>,
    /// Players present in the server's `banned-players.json`.
    pub banned_players: Vec<PlayerProfile>,
    /// IP addresses present in the server's `banned-ips.json`.
    pub banned_ips: Vec<String>,
}

/// Reads the available profile details for an online player.
///
/// Playtime is read from the world's stats file, while UUID, address, and the
/// last login position are recovered from `usercache.json` and server output.
pub fn read_player_details(
    working_dir: &Path,
    player_name: &str,
    log_lines: &[String],
) -> Result<PlayerDetails, String> {
    let cache_path = working_dir.join("usercache.json");
    let usercache = read_optional_file(&cache_path)?
        .map(|text| {
            serde_json::from_str::<Value>(&text)
                .map_err(|err| format!("Could not parse {}: {err}", cache_path.display()))
        })
        .transpose()?;

    let uuid = log_lines
        .iter()
        .rev()
        .find_map(|line| player_uuid_from_log(line, player_name))
        .or_else(|| {
            usercache
                .as_ref()
                .and_then(|cache| player_uuid_from_cache(cache, player_name))
        });

    let play_time_ticks = match (uuid.as_deref(), world_directory(working_dir)?) {
        (Some(uuid), Some(world_dir)) => {
            let stats_path = world_dir.join("stats").join(format!("{uuid}.json"));
            read_optional_file(&stats_path)?
                .map(|text| {
                    let stats = serde_json::from_str::<Value>(&text).map_err(|err| {
                        format!("Could not parse {}: {err}", stats_path.display())
                    })?;
                    Ok::<_, String>(
                        stats
                            .get("stats")
                            .and_then(|stats| stats.get("minecraft:custom"))
                            .and_then(|stats| stats.get("minecraft:play_time"))
                            .and_then(Value::as_u64),
                    )
                })
                .transpose()?
                .flatten()
        }
        _ => None,
    };

    Ok(PlayerDetails {
        uuid,
        play_time_ticks,
        ip_address: log_lines
            .iter()
            .rev()
            .find_map(|line| player_ip_from_log(line, player_name)),
        last_position: log_lines
            .iter()
            .rev()
            .find_map(|line| player_position_from_login(line, player_name)),
    })
}

/// Parses the live position returned by `data get entity <name> Pos`.
pub fn parse_position_command_output(line: &str, player_name: &str) -> Option<[f64; 3]> {
    let message = line.rsplit("]: ").next().unwrap_or(line);
    let prefix = format!("{player_name} has the following entity data: [");
    let values = message.strip_prefix(&prefix)?.strip_suffix(']')?;
    parse_coordinates(values)
}

/// Downloads a player's face image from MC-Heads.
pub fn fetch_player_skin(player_name: &str) -> Result<Vec<u8>, String> {
    if !is_minecraft_username(player_name) {
        return Err("Invalid Minecraft username.".to_string());
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(format!("{PLAYER_SKIN_BASE}/{player_name}/64"))
        .send()
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > 1024 * 1024)
    {
        return Err("Skin image is larger than expected.".to_string());
    }
    let bytes = response.bytes().map_err(|err| err.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("Skin image is larger than expected.".to_string());
    }
    Ok(bytes.to_vec())
}

/// Checks whether a player name is safe to use in Minecraft commands and URLs.
pub fn is_minecraft_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Reads cached player identities and whitelist entries from a server folder.
pub fn read_player_catalog(working_dir: &Path) -> Result<PlayerCatalog, String> {
    let cache_path = working_dir.join("usercache.json");
    let known_players = read_optional_file(&cache_path)?
        .map(|text| {
            serde_json::from_str::<Vec<PlayerProfile>>(&text)
                .map_err(|err| format!("Could not parse {}: {err}", cache_path.display()))
        })
        .transpose()?
        .unwrap_or_default();

    let whitelist = read_whitelist_file(working_dir)?;
    let banned_players = read_banned_player_records(working_dir)?
        .into_iter()
        .map(|entry| PlayerProfile {
            name: entry.name,
            uuid: entry.uuid,
        })
        .collect();
    let banned_ips = read_banned_ip_records(working_dir)?
        .into_iter()
        .map(|entry| entry.ip)
        .collect();
    Ok(PlayerCatalog {
        known_players: sort_profiles(known_players),
        whitelist: sort_profiles(whitelist),
        banned_players: sort_profiles(banned_players),
        banned_ips,
    })
}

/// Resolves a username and adds its UUID to `whitelist.json`.
///
/// Cached UUIDs are reused when available. Otherwise, online-mode servers use
/// Mojang's profile API and offline-mode servers use Minecraft's deterministic
/// `OfflinePlayer:<name>` UUID.
pub fn add_whitelist_player(working_dir: &Path, username: &str) -> Result<PlayerProfile, String> {
    let username = username.trim();
    if !is_minecraft_username(username) {
        return Err(
            "Enter a valid Minecraft username (1-16 letters, numbers, or underscores).".to_string(),
        );
    }

    let profile = resolve_player_profile(working_dir, username)?;

    let mut whitelist = read_whitelist_file(working_dir)?;
    if whitelist.iter().any(|entry| {
        entry.uuid.eq_ignore_ascii_case(&profile.uuid)
            || entry.name.eq_ignore_ascii_case(&profile.name)
    }) {
        return Err(format!("{} is already on the whitelist.", profile.name));
    }
    whitelist.push(profile.clone());
    write_whitelist_file(working_dir, sort_profiles(whitelist))?;
    Ok(profile)
}

/// Removes a UUID from the server's `whitelist.json`.
pub fn remove_whitelist_player(working_dir: &Path, uuid: &str) -> Result<(), String> {
    let mut whitelist = read_whitelist_file(working_dir)?;
    let original_len = whitelist.len();
    whitelist.retain(|entry| !entry.uuid.eq_ignore_ascii_case(uuid));
    if whitelist.len() == original_len {
        return Err("That player is no longer on the whitelist.".to_string());
    }
    write_whitelist_file(working_dir, whitelist)
}

/// Adds a username to `banned-players.json` while the server is stopped.
///
/// Cached UUIDs are reused before resolving an online-mode username through Mojang.
pub fn add_player_ban(working_dir: &Path, username: &str) -> Result<PlayerProfile, String> {
    let username = username.trim();
    if !is_minecraft_username(username) {
        return Err("Invalid Minecraft username.".to_string());
    }

    let profile = resolve_player_profile(working_dir, username)?;
    let mut entries = read_banned_player_records(working_dir)?;
    if entries.iter().any(|entry| {
        entry.uuid.eq_ignore_ascii_case(&profile.uuid)
            || entry.name.eq_ignore_ascii_case(&profile.name)
    }) {
        return Err(format!("{} is already banned.", profile.name));
    }
    entries.push(PlayerBanEntry::new(&profile));
    entries.sort_by_cached_key(|entry| entry.name.to_ascii_lowercase());
    write_banned_player_records(working_dir, entries)?;
    Ok(profile)
}

/// Removes a UUID from `banned-players.json` while the server is stopped.
pub fn remove_player_ban(working_dir: &Path, uuid: &str) -> Result<(), String> {
    let mut entries = read_banned_player_records(working_dir)?;
    let original_len = entries.len();
    entries.retain(|entry| !entry.uuid.eq_ignore_ascii_case(uuid));
    if entries.len() == original_len {
        return Err("That player is no longer banned.".to_string());
    }
    write_banned_player_records(working_dir, entries)
}

/// Adds an IP address to `banned-ips.json` while the server is stopped.
pub fn add_ip_ban(working_dir: &Path, ip: &str) -> Result<String, String> {
    let ip = ip
        .parse::<IpAddr>()
        .map_err(|_| "Enter a valid IP address.".to_string())?
        .to_string();
    let mut entries = read_banned_ip_records(working_dir)?;
    if entries.iter().any(|entry| {
        entry
            .ip
            .parse::<IpAddr>()
            .is_ok_and(|existing| existing.to_string() == ip)
    }) {
        return Err(format!("{ip} is already banned."));
    }
    entries.push(IpBanEntry::new(&ip));
    entries.sort_by_cached_key(|entry| entry.ip.to_ascii_lowercase());
    write_banned_ip_records(working_dir, entries)?;
    Ok(ip)
}

/// Removes an IP address from `banned-ips.json` while the server is stopped.
pub fn remove_ip_ban(working_dir: &Path, ip: &str) -> Result<(), String> {
    let ip = ip
        .parse::<IpAddr>()
        .map_err(|_| "Enter a valid IP address.".to_string())?
        .to_string();
    let mut entries = read_banned_ip_records(working_dir)?;
    let original_len = entries.len();
    entries.retain(|entry| {
        !entry
            .ip
            .parse::<IpAddr>()
            .is_ok_and(|existing| existing.to_string() == ip)
    });
    if entries.len() == original_len {
        return Err(format!("{ip} is no longer banned."));
    }
    write_banned_ip_records(working_dir, entries)
}

fn read_optional_file(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("Could not read {}: {err}", path.display())),
    }
}

fn resolve_player_profile(working_dir: &Path, username: &str) -> Result<PlayerProfile, String> {
    let cache_path = working_dir.join("usercache.json");
    let cached_profiles = read_optional_file(&cache_path)?
        .map(|text| {
            serde_json::from_str::<Vec<PlayerProfile>>(&text)
                .map_err(|err| format!("Could not parse {}: {err}", cache_path.display()))
        })
        .transpose()?
        .unwrap_or_default();
    if let Some(profile) = cached_profiles
        .into_iter()
        .find(|profile| profile.name.eq_ignore_ascii_case(username))
    {
        return Ok(profile);
    }

    if read_online_mode(working_dir)? {
        resolve_online_profile(username)
    } else {
        Ok(PlayerProfile {
            name: username.to_string(),
            uuid: offline_player_uuid(username),
        })
    }
}

fn read_whitelist_file(working_dir: &Path) -> Result<Vec<PlayerProfile>, String> {
    let path = working_dir.join("whitelist.json");
    read_optional_file(&path)?
        .map(|text| {
            serde_json::from_str::<Vec<PlayerProfile>>(&text)
                .map_err(|err| format!("Could not parse {}: {err}", path.display()))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn read_banned_player_records(working_dir: &Path) -> Result<Vec<PlayerBanEntry>, String> {
    let path = working_dir.join("banned-players.json");
    read_optional_file(&path)?
        .map(|text| {
            serde_json::from_str::<Vec<PlayerBanEntry>>(&text)
                .map_err(|err| format!("Could not parse {}: {err}", path.display()))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn write_banned_player_records(
    working_dir: &Path,
    entries: Vec<PlayerBanEntry>,
) -> Result<(), String> {
    let path = working_dir.join("banned-players.json");
    let text = serde_json::to_string_pretty(&entries)
        .map_err(|err| format!("Could not serialize player bans: {err}"))?;
    std::fs::write(&path, text).map_err(|err| format!("Could not write {}: {err}", path.display()))
}

fn read_banned_ip_records(working_dir: &Path) -> Result<Vec<IpBanEntry>, String> {
    let path = working_dir.join("banned-ips.json");
    read_optional_file(&path)?
        .map(|text| {
            serde_json::from_str::<Vec<IpBanEntry>>(&text)
                .map_err(|err| format!("Could not parse {}: {err}", path.display()))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn write_banned_ip_records(working_dir: &Path, entries: Vec<IpBanEntry>) -> Result<(), String> {
    let path = working_dir.join("banned-ips.json");
    let text = serde_json::to_string_pretty(&entries)
        .map_err(|err| format!("Could not serialize IP bans: {err}"))?;
    std::fs::write(&path, text).map_err(|err| format!("Could not write {}: {err}", path.display()))
}

#[derive(Debug, Deserialize, Serialize)]
struct PlayerBanEntry {
    uuid: String,
    name: String,
    #[serde(default = "current_ban_time")]
    created: String,
    #[serde(default = "default_ban_source")]
    source: String,
    #[serde(default = "default_ban_expiry")]
    expires: String,
    #[serde(default = "default_player_ban_reason")]
    reason: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl PlayerBanEntry {
    fn new(profile: &PlayerProfile) -> Self {
        Self {
            uuid: profile.uuid.clone(),
            name: profile.name.clone(),
            created: current_ban_time(),
            source: default_ban_source(),
            expires: default_ban_expiry(),
            reason: default_player_ban_reason(),
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct IpBanEntry {
    ip: String,
    #[serde(default = "current_ban_time")]
    created: String,
    #[serde(default = "default_ban_source")]
    source: String,
    #[serde(default = "default_ban_expiry")]
    expires: String,
    #[serde(default = "default_ip_ban_reason")]
    reason: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl IpBanEntry {
    fn new(ip: &str) -> Self {
        Self {
            ip: ip.to_string(),
            created: current_ban_time(),
            source: default_ban_source(),
            expires: default_ban_expiry(),
            reason: default_ip_ban_reason(),
            extra: Map::new(),
        }
    }
}

fn current_ban_time() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S %z").to_string()
}

fn default_ban_source() -> String {
    "Server".to_string()
}

fn default_ban_expiry() -> String {
    "forever".to_string()
}

fn default_player_ban_reason() -> String {
    "Banned by an operator.".to_string()
}

fn default_ip_ban_reason() -> String {
    "IP banned by an operator.".to_string()
}

fn write_whitelist_file(working_dir: &Path, whitelist: Vec<PlayerProfile>) -> Result<(), String> {
    let path = working_dir.join("whitelist.json");
    let text = serde_json::to_string_pretty(&whitelist)
        .map_err(|err| format!("Could not serialize whitelist: {err}"))?;
    std::fs::write(&path, text).map_err(|err| format!("Could not write {}: {err}", path.display()))
}

fn sort_profiles(mut profiles: Vec<PlayerProfile>) -> Vec<PlayerProfile> {
    profiles.sort_by_cached_key(|profile| profile.name.to_ascii_lowercase());
    let mut seen_uuids = HashSet::new();
    profiles.retain(|profile| seen_uuids.insert(profile.uuid.to_ascii_lowercase()));
    profiles
}

fn read_online_mode(working_dir: &Path) -> Result<bool, String> {
    let path = working_dir.join("server.properties");
    let properties = read_optional_file(&path)?;
    let value = properties
        .as_deref()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("online-mode="))
        })
        .map(str::trim);
    match value {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(value) => Err(format!(
            "Invalid online-mode value '{value}' in {}.",
            path.display()
        )),
        None => Ok(true),
    }
}

#[derive(Deserialize)]
struct MojangProfile {
    id: String,
    name: String,
}

fn resolve_online_profile(username: &str) -> Result<PlayerProfile, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|err| format!("Could not create Minecraft profile client: {err}"))?;
    let response = client
        .get(format!(
            "https://api.mojang.com/users/profiles/minecraft/{username}"
        ))
        .send()
        .map_err(|err| format!("Could not look up {username} with Mojang: {err}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND
        || response.status() == reqwest::StatusCode::NO_CONTENT
    {
        return Err(format!("No Mojang profile was found for {username}."));
    }
    if !response.status().is_success() {
        return Err(format!(
            "Mojang profile lookup failed with HTTP {}.",
            response.status()
        ));
    }
    let profile = response
        .json::<MojangProfile>()
        .map_err(|err| format!("Could not read Mojang profile for {username}: {err}"))?;
    Ok(PlayerProfile {
        name: profile.name,
        uuid: format_profile_uuid(&profile.id)?,
    })
}

fn format_profile_uuid(id: &str) -> Result<String, String> {
    let id = id.to_ascii_lowercase();
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Mojang returned an invalid player UUID.".to_string());
    }
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &id[0..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..32]
    ))
}

fn offline_player_uuid(username: &str) -> String {
    let mut bytes: [u8; 16] = Md5::digest(format!("OfflinePlayer:{username}").as_bytes()).into();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

fn world_directory(working_dir: &Path) -> Result<Option<PathBuf>, String> {
    let properties_path = working_dir.join("server.properties");
    let properties = read_optional_file(&properties_path)?;
    let level_name = properties
        .as_deref()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("level-name=")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
        })
        .unwrap_or("world");
    let relative_path = Path::new(level_name);
    if relative_path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(Some(working_dir.join(relative_path)))
    } else {
        Ok(None)
    }
}

fn player_uuid_from_cache(cache: &Value, player_name: &str) -> Option<String> {
    cache.as_array()?.iter().find_map(|entry| {
        let name = entry.get("name")?.as_str()?;
        if name.eq_ignore_ascii_case(player_name) {
            entry.get("uuid")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

fn player_uuid_from_log(line: &str, player_name: &str) -> Option<String> {
    let message = line.rsplit("]: ").next().unwrap_or(line);
    let prefix = format!("UUID of player {player_name} is ");
    let uuid = message.strip_prefix(&prefix)?.trim();
    (!uuid.is_empty()).then(|| uuid.to_string())
}

fn player_ip_from_log(line: &str, player_name: &str) -> Option<String> {
    let message = line.rsplit("]: ").next().unwrap_or(line);
    let connection = message
        .strip_prefix(player_name)?
        .strip_prefix("[/")?
        .split_once(']')?
        .0;
    connection
        .parse::<SocketAddr>()
        .ok()
        .map(|address| address.ip().to_string())
}

fn player_position_from_login(line: &str, player_name: &str) -> Option<[f64; 3]> {
    let message = line.rsplit("]: ").next().unwrap_or(line);
    let login = message.strip_prefix(player_name)?;
    if !login.starts_with("[/") {
        return None;
    }
    let position = login.split_once(" at (")?.1.strip_suffix(')')?;
    let coordinates = position
        .rsplit_once(']')
        .map(|(_, coordinates)| coordinates)
        .unwrap_or(position);
    parse_coordinates(coordinates)
}

fn parse_coordinates(values: &str) -> Option<[f64; 3]> {
    let mut coordinates = values.split(',').map(|value| {
        value
            .trim()
            .trim_end_matches(['d', 'D'])
            .parse::<f64>()
            .ok()
    });
    let result = [
        coordinates.next()??,
        coordinates.next()??,
        coordinates.next()??,
    ];
    if coordinates.next().is_some() || result.iter().any(|value| !value.is_finite()) {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_server_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("spoon-player-test-{nanos}"))
    }

    #[test]
    fn reads_profile_data_from_server_files_and_logs() {
        let root = temp_server_dir();
        let world = root.join("custom_world");
        fs::create_dir_all(world.join("stats")).unwrap();
        fs::write(root.join("server.properties"), "level-name=custom_world\n").unwrap();
        fs::write(
            root.join("usercache.json"),
            r#"[{"name":"Alex","uuid":"12345678-1234-1234-1234-123456789abc"}]"#,
        )
        .unwrap();
        fs::write(
            world.join("stats/12345678-1234-1234-1234-123456789abc.json"),
            r#"{"stats":{"minecraft:custom":{"minecraft:play_time":72000}}}"#,
        )
        .unwrap();
        let logs = vec![
            "[Server thread/INFO]: Alex[/192.0.2.5:54321] logged in with entity id 1 at ([world]12.8, 64.0, -4.1)".to_string(),
        ];

        let details = read_player_details(&root, "Alex", &logs).unwrap();
        assert_eq!(
            details.uuid.as_deref(),
            Some("12345678-1234-1234-1234-123456789abc")
        );
        assert_eq!(details.play_time_ticks, Some(72_000));
        assert_eq!(details.ip_address.as_deref(), Some("192.0.2.5"));
        assert_eq!(details.last_position, Some([12.8, 64.0, -4.1]));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_live_position_output_and_ignores_other_players() {
        let line =
            "[Server thread/INFO]: Alex has the following entity data: [12.8d, 64.0d, -4.1d]";
        assert_eq!(
            parse_position_command_output(line, "Alex"),
            Some([12.8, 64.0, -4.1])
        );
        assert_eq!(parse_position_command_output(line, "Sam"), None);
    }

    #[test]
    fn validates_player_names_for_commands_and_skin_urls() {
        assert!(is_minecraft_username("Alex_123"));
        assert!(!is_minecraft_username(""));
        assert!(!is_minecraft_username("name with spaces"));
        assert!(!is_minecraft_username("name/other"));
        assert!(!is_minecraft_username("this_name_is_too_long"));
    }

    #[test]
    fn rejects_world_paths_outside_the_server_directory() {
        let root = temp_server_dir();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("server.properties"), "level-name=../outside\n").unwrap();
        let logs = Vec::new();

        let details = read_player_details(&root, "Alex", &logs).unwrap();
        assert_eq!(details.play_time_ticks, None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adds_and_removes_offline_players_from_the_whitelist() {
        let root = temp_server_dir();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("server.properties"), "online-mode=false\n").unwrap();

        let profile = add_whitelist_player(&root, "Notch").unwrap();
        assert_eq!(profile.name, "Notch");
        assert_eq!(profile.uuid, "b50ad385-829d-3141-a216-7e7d7539ba7f");

        let catalog = read_player_catalog(&root).unwrap();
        assert_eq!(catalog.whitelist.as_slice(), std::slice::from_ref(&profile));
        remove_whitelist_player(&root, &profile.uuid).unwrap();
        assert!(read_player_catalog(&root).unwrap().whitelist.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_banned_players_from_the_server_catalog() {
        let root = temp_server_dir();
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("banned-players.json"),
            r#"[{"uuid":"12345678-1234-1234-1234-123456789abc","name":"Alex","created":"2026-01-01 00:00:00 +0000","source":"Server","expires":"forever","reason":"test"}]"#,
        )
        .unwrap();

        let catalog = read_player_catalog(&root).unwrap();
        assert_eq!(
            catalog.banned_players,
            [PlayerProfile {
                name: "Alex".to_string(),
                uuid: "12345678-1234-1234-1234-123456789abc".to_string(),
            }]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adds_and_removes_offline_player_bans() {
        let root = temp_server_dir();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("server.properties"), "online-mode=false\n").unwrap();

        let profile = add_player_ban(&root, "Notch").unwrap();
        assert_eq!(profile.uuid, "b50ad385-829d-3141-a216-7e7d7539ba7f");
        let catalog = read_player_catalog(&root).unwrap();
        assert_eq!(
            catalog.banned_players.as_slice(),
            std::slice::from_ref(&profile)
        );
        let record: Value =
            serde_json::from_str(&fs::read_to_string(root.join("banned-players.json")).unwrap())
                .unwrap();
        assert_eq!(record[0]["expires"], "forever");

        remove_player_ban(&root, &profile.uuid).unwrap();
        assert!(read_player_catalog(&root)
            .unwrap()
            .banned_players
            .is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adds_and_removes_ip_bans_using_canonical_addresses() {
        let root = temp_server_dir();
        fs::create_dir_all(&root).unwrap();

        assert_eq!(add_ip_ban(&root, "2001:0db8::1").unwrap(), "2001:db8::1");
        assert!(add_ip_ban(&root, "2001:db8:0:0:0:0:0:1").is_err());
        assert_eq!(
            read_player_catalog(&root).unwrap().banned_ips,
            ["2001:db8::1"]
        );
        remove_ip_ban(&root, "2001:db8:0:0:0:0:0:1").unwrap();
        assert!(read_player_catalog(&root).unwrap().banned_ips.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_invalid_whitelist_names_without_network_access() {
        let root = temp_server_dir();
        fs::create_dir_all(&root).unwrap();

        assert!(add_whitelist_player(&root, "invalid name").is_err());
        assert!(!root.join("whitelist.json").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn formats_mojang_profile_ids_as_hyphenated_uuids() {
        assert_eq!(
            format_profile_uuid("0123456789ABCDEF0123456789ABCDEF").unwrap(),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
        assert!(format_profile_uuid("not-a-uuid").is_err());
    }
}
