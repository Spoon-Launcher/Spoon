//! Server backups: snapshot a server's folder as a zip archive under
//! `Spoon/backups/<server-id>`, so world data survives accidents, failed
//! updates and experiments. Archives are named `<server>-<timestamp>.zip`
//! and can be listed and removed again.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::sanitize_folder_name;
use crate::files;

/// One backup archive on disk.
#[derive(Debug, Clone)]
pub struct BackupInfo {
    /// File stem, e.g. `Survival-2025-01-05-143022`.
    pub name: String,
    /// Absolute path of the zip archive.
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Last modification time of the archive file.
    pub created: SystemTime,
}

/// Manages Spoon's backups directory (`backups/` under the platform config
/// dir) with one subfolder per server id, so renames and duplicate names
/// can't mix different servers' archives.
#[derive(Debug, Clone)]
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    /// Spoon's backups directory under the platform config dir, if it can be determined.
    pub fn new() -> Option<Self> {
        let root = dirs::config_dir()?.join("Spoon").join("backups");
        Some(Self { root })
    }

    /// Create a store rooted at an arbitrary directory (tests).
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Folder holding one server's backups: `<root>/<server-id>`.
    pub fn server_dir(&self, server_id: &str) -> PathBuf {
        self.root.join(server_id)
    }

    /// Zip everything in `working_dir` into this server's backups folder.
    /// The archive is named `<server-name>-<timestamp>.zip` (`-2`, `-3`, …
    /// when several land in the same second). Callers should back up stopped
    /// servers so the world files are not being written mid-copy.
    pub fn create(
        &self,
        server_id: &str,
        server_name: &str,
        working_dir: &Path,
    ) -> Result<BackupInfo, String> {
        let dir = self.server_dir(server_id);
        fs::create_dir_all(&dir).map_err(|err| format!("creating backups folder: {err}"))?;
        let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
        let base = format!("{}-{stamp}", sanitize_folder_name(server_name));
        let dest = files::unique_zip_path(&dir, &base);
        let mut items: Vec<PathBuf> = fs::read_dir(working_dir)
            .map_err(|err| format!("reading server folder: {err}"))?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        items.sort();
        files::zip_paths(working_dir, &items, &dest)
            .map_err(|err| format!("writing backup: {err}"))?;
        Self::info(&dest).ok_or_else(|| "Backup vanished after creation.".to_string())
    }

    /// All backups of one server, newest first; empty when none exist.
    pub fn list(&self, server_id: &str) -> Vec<BackupInfo> {
        let mut backups: Vec<BackupInfo> = fs::read_dir(self.server_dir(server_id))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| Self::info(&entry.path()))
                    .collect()
            })
            .unwrap_or_default();
        backups.sort_by_key(|backup| std::cmp::Reverse(backup.created));
        backups
    }

    /// Delete one backup by its file stem (as given by [`BackupStore::list`]).
    pub fn remove(&self, server_id: &str, name: &str) -> Result<(), String> {
        if name.contains(['/', '\\']) {
            return Err("Invalid backup name.".to_string());
        }
        let path = self.server_dir(server_id).join(format!("{name}.zip"));
        if !path.is_file() {
            return Err("Backup not found.".to_string());
        }
        fs::remove_file(&path).map_err(|err| format!("deleting backup: {err}"))
    }

    /// Delete the whole backups folder of one server. A missing folder is not
    /// an error; other servers' backups are untouched.
    pub fn remove_all(&self, server_id: &str) -> Result<(), String> {
        match fs::remove_dir_all(self.server_dir(server_id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("deleting backups: {err}")),
        }
    }

    /// Extract one backup into `working_dir`. With `wipe` set, everything in
    /// the server folder is deleted first (cannot be undone); otherwise the
    /// archive is extracted next to the existing files, overwriting
    /// same-named ones. Callers should restore stopped servers.
    pub fn restore(
        &self,
        server_id: &str,
        name: &str,
        working_dir: &Path,
        wipe: bool,
    ) -> Result<(), String> {
        if name.contains(['/', '\\']) {
            return Err("Invalid backup name.".to_string());
        }
        let archive = self.server_dir(server_id).join(format!("{name}.zip"));
        if !archive.is_file() {
            return Err("Backup not found.".to_string());
        }
        if wipe {
            wipe_dir(working_dir)?;
        }
        fs::create_dir_all(working_dir).map_err(|err| format!("creating server folder: {err}"))?;
        extract_zip(&archive, working_dir)
    }

    /// Builds a [`BackupInfo`] for one archive file, skipping anything that
    /// doesn't look like a zip backup.
    fn info(path: &Path) -> Option<BackupInfo> {
        if path.extension()?.to_str()? != "zip" {
            return None;
        }
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(BackupInfo {
            name: path.file_stem()?.to_string_lossy().into_owned(),
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            created: metadata.modified().ok()?,
        })
    }
}

/// Delete everything inside `dir`, keeping the folder itself.
fn wipe_dir(dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|err| format!("reading server folder: {err}"))? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let removed = if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|err| format!("deleting {}: {err}", path.display()))?;
    }
    Ok(())
}

/// Extract a zip archive into `dest`, skipping entries that would escape it.
fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|err| format!("opening backup: {err}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| format!("reading backup: {err}"))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|err| err.to_string())?;
        let Some(path) = entry.enclosed_name().map(|name| dest.join(name)) else {
            continue; // zip-slip attempt or weird name: skip
        };
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(|err| format!("creating folder: {err}"))?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|err| format!("creating folder: {err}"))?;
            }
            let mut out = fs::File::create(&path)
                .map_err(|err| format!("writing {}: {err}", path.display()))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|err| format!("extracting {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;

    fn temp_scope() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("spoon-backup-test-{nanos}"))
    }

    fn server_config(root: &Path) -> ServerConfig {
        let working_dir = root.join("server");
        fs::create_dir_all(working_dir.join("world/region")).unwrap();
        fs::write(working_dir.join("server.properties"), "max-players=10").unwrap();
        fs::write(working_dir.join("world/region/r.0.0.mca"), b"chunk").unwrap();
        ServerConfig::new(
            "server-id".into(),
            "My Server".into(),
            working_dir.join("server.jar"),
            working_dir,
            4096,
        )
    }

    #[test]
    fn create_lists_and_removes_a_backup() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));
        let config = server_config(&root);

        let backup = store
            .create(&config.id, &config.name, &config.working_dir)
            .expect("create backup");
        assert!(backup.path.is_file());
        assert_eq!(backup.size_bytes, fs::metadata(&backup.path).unwrap().len());

        // The archive mirrors the server folder at its top level (no wrapper dir).
        let archive = zip::ZipArchive::new(fs::File::open(&backup.path).unwrap()).unwrap();
        let names: Vec<String> = archive.file_names().map(String::from).collect();
        assert!(names.contains(&"server.properties".to_string()));
        assert!(names.iter().any(|name| name.starts_with("world/region/")));

        let listed = store.list(&config.id);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, backup.name);
        assert!(store.list("other-id").is_empty());

        store.remove(&config.id, &backup.name).unwrap();
        assert!(store.list(&config.id).is_empty());
        assert!(!backup.path.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn archives_created_in_the_same_second_get_a_suffix() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));
        let config = server_config(&root);

        let first = store
            .create(&config.id, &config.name, &config.working_dir)
            .unwrap();
        let second = store
            .create(&config.id, &config.name, &config.working_dir)
            .unwrap();

        assert_ne!(first.name, second.name);
        assert_eq!(store.list(&config.id).len(), 2);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn create_reports_a_missing_server_folder() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));

        let err = store
            .create("id", "Name", &root.join("missing"))
            .unwrap_err();
        assert!(err.contains("reading server folder"), "{err}");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn remove_rejects_names_outside_the_backup_folder() {
        let root = temp_scope();
        fs::create_dir_all(&root).unwrap();
        let store = BackupStore::at(root.join("backups"));

        assert!(store.remove("id", "../evil").is_err());
        assert!(store.remove("id", "missing").is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn remove_all_deletes_only_that_servers_backups() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));
        let config = server_config(&root);
        let other = ServerConfig::new(
            "other-id".into(),
            "Other".into(),
            config.working_dir.join("server.jar"),
            config.working_dir.clone(),
            4096,
        );

        store
            .create(&config.id, &config.name, &config.working_dir)
            .unwrap();
        store
            .create(&other.id, &other.name, &other.working_dir)
            .unwrap();

        store.remove_all(&config.id).unwrap();
        assert!(store.list(&config.id).is_empty());
        assert!(!store.server_dir(&config.id).exists());
        // A server without backups removes cleanly too.
        store.remove_all(&config.id).unwrap();
        // The other server's backups are untouched.
        assert_eq!(store.list(&other.id).len(), 1);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_merges_into_the_server_folder() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));
        let config = server_config(&root);
        let backup = store
            .create(&config.id, &config.name, &config.working_dir)
            .unwrap();

        // Change the server after backing it up.
        fs::write(
            config.working_dir.join("server.properties"),
            "max-players=99",
        )
        .unwrap();
        fs::write(config.working_dir.join("extra.txt"), "new").unwrap();

        store
            .restore(&config.id, &backup.name, &config.working_dir, false)
            .unwrap();

        // Backed-up content is back; files added later survive (merge).
        assert_eq!(
            fs::read_to_string(config.working_dir.join("server.properties")).unwrap(),
            "max-players=10"
        );
        assert_eq!(
            fs::read(config.working_dir.join("world/region/r.0.0.mca")).unwrap(),
            b"chunk"
        );
        assert!(config.working_dir.join("extra.txt").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_with_wipe_replaces_the_server_folder() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));
        let config = server_config(&root);
        let backup = store
            .create(&config.id, &config.name, &config.working_dir)
            .unwrap();

        fs::write(config.working_dir.join("extra.txt"), "new").unwrap();
        store
            .restore(&config.id, &backup.name, &config.working_dir, true)
            .unwrap();

        assert_eq!(
            fs::read(config.working_dir.join("world/region/r.0.0.mca")).unwrap(),
            b"chunk"
        );
        assert!(!config.working_dir.join("extra.txt").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn restore_rejects_unknown_or_dangerous_names() {
        let root = temp_scope();
        let store = BackupStore::at(root.join("backups"));
        let config = server_config(&root);

        assert!(store
            .restore(&config.id, "missing", &config.working_dir, false)
            .is_err());
        assert!(store
            .restore(&config.id, "../evil", &config.working_dir, false)
            .is_err());

        fs::remove_dir_all(&root).unwrap();
    }
}
