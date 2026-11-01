//! Reading and accepting the Minecraft EULA (`eula.txt`) in a server folder.

use std::fs;
use std::io;
use std::path::Path;

pub const EULA_URL: &str = "https://aka.ms/MinecraftEULA";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EulaState {
    /// No `eula.txt` exists yet (server has not been started).
    Missing,
    /// `eula.txt` exists but does not say `eula=true`.
    Declined,
    Accepted,
}

/// EULA acceptance state of the server in `working_dir`.
pub fn eula_state(working_dir: &Path) -> io::Result<EulaState> {
    match fs::read_to_string(working_dir.join("eula.txt")) {
        Ok(contents) => Ok(if parse_eula(&contents) == Some(true) {
            EulaState::Accepted
        } else {
            EulaState::Declined
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(EulaState::Missing),
        Err(err) => Err(err),
    }
}

/// Set `eula=true` in `working_dir/eula.txt`, preserving any existing
/// content (and its header comments). Creates the file if missing.
pub fn accept_eula(working_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(working_dir)?;

    let path = working_dir.join("eula.txt");
    let contents = fs::read_to_string(&path).unwrap_or_default();

    let mut out = String::new();
    let mut replaced = false;
    for line in contents.lines() {
        if line.trim_start().starts_with("eula=") {
            if !replaced {
                out.push_str("eula=true\n");
                replaced = true;
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str("# By changing this setting to TRUE you are indicating your agreement to the Minecraft EULA.\n");
        out.push_str("# Accepted via Spoon. See ");
        out.push_str(EULA_URL);
        out.push('\n');
        out.push_str("eula=true\n");
    }

    fs::write(path, out)
}

/// Value of the last `eula=` line.
fn parse_eula(contents: &str) -> Option<bool> {
    let mut value = None;
    for line in contents.lines() {
        if let Some(rest) = line.trim().strip_prefix("eula=") {
            value = Some(rest.trim().eq_ignore_ascii_case("true"));
        }
    }
    value
}
