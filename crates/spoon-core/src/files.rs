use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Upper size bound for files opened in the built-in editor.
pub const MAX_EDITABLE_BYTES: u64 = 10 * 1024 * 1024;

const SNIFF_LEN: usize = 8192;

/// A single entry in a directory listing.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
}

/// Contents of a file opened in the editor.
pub enum FileContent {
    /// Decoded UTF-8 text, editable.
    Text(String),
    /// Binary or non-UTF-8 file; not editable.
    Binary,
    /// Larger than [`MAX_EDITABLE_BYTES`]; not opened.
    TooLarge,
}

/// Lists `path` sorted directories-first, then case-insensitively by name.
pub fn list_dir(path: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(path)? {
        let item = item?;
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        let metadata = item.metadata()?;
        entries.push(FileEntry {
            name,
            path: item.path(),
            is_dir: metadata.is_dir(),
            size: metadata.len(),
        });
    }
    // Cached keys: one lowercase allocation per entry instead of two per comparison.
    entries.sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
    Ok(entries)
}

/// Reads a file for the editor, guarding against huge and binary files.
pub fn read_file(path: &Path) -> io::Result<FileContent> {
    if fs::metadata(path)?.len() > MAX_EDITABLE_BYTES {
        return Ok(FileContent::TooLarge);
    }
    let bytes = fs::read(path)?;
    if is_binary(&bytes) {
        return Ok(FileContent::Binary);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(FileContent::Text(text)),
        Err(_) => Ok(FileContent::Binary),
    }
}

/// Heuristic binary sniff: a NUL byte in the first bytes of the file.
pub fn is_binary(bytes: &[u8]) -> bool {
    let sniff = &bytes[..bytes.len().min(SNIFF_LEN)];
    sniff.contains(&0)
}

/// Errors with `PermissionDenied` unless `candidate` resolves to a path
/// inside `root` (canonicalization resolves `.`/`..` and symlinks).
pub fn ensure_within(root: &Path, candidate: &Path) -> io::Result<()> {
    let root = root.canonicalize()?;
    let candidate = candidate.canonicalize()?;
    if candidate.strip_prefix(&root).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is outside the server folder", candidate.display()),
        ));
    }
    Ok(())
}

/// Picks a non-existing `base.zip` / `base-2.zip` / ... name inside `dir`.
pub fn unique_zip_path(dir: &Path, base: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{base}.zip"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base}-{n}.zip"));
        n += 1;
    }
    candidate
}

/// Compresses `items` (files and folders, recursively) into a zip at `dest`.
/// Entry names are relative to `base`; symlinks are skipped.
pub fn zip_paths(base: &Path, items: &[PathBuf], dest: &Path) -> io::Result<()> {
    let result = (|| {
        let mut writer = zip::ZipWriter::new(io::BufWriter::with_capacity(
            256 * 1024,
            fs::File::create(dest)?,
        ));
        for item in items {
            add_to_zip(&mut writer, base, item, dest)?;
        }
        writer.finish().map_err(io::Error::from)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

/// Formats that are already compressed (Minecraft region chunks are zlib'd);
/// recompressing them wastes CPU for no size benefit.
fn is_already_compressed(path: &Path) -> bool {
    const EXTENSIONS: [&str; 14] = [
        "mca", "mcr", "jar", "zip", "gz", "png", "jpg", "jpeg", "ogg", "mp3", "mp4", "7z", "xz",
        "zst",
    ];
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

fn add_to_zip<W: io::Write + io::Seek>(
    writer: &mut zip::ZipWriter<W>,
    base: &Path,
    path: &Path,
    dest: &Path,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || path == dest {
        return Ok(());
    }
    let relative = path.strip_prefix(base).unwrap_or(path);
    let name = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let options = if is_already_compressed(path) {
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
    } else {
        // Fastest deflate level: far quicker than the default with a similar ratio on text.
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(1))
    }
    .large_file(metadata.len() >= 4 * 1024 * 1024 * 1024);
    if metadata.is_dir() {
        writer
            .add_directory(format!("{name}/"), options)
            .map_err(io::Error::from)?;
        for child in fs::read_dir(path)? {
            add_to_zip(writer, base, &child?.path(), dest)?;
        }
    } else {
        writer.start_file(name, options).map_err(io::Error::from)?;
        let mut file = fs::File::open(path)?;
        io::copy(&mut file, writer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn zips_files_and_folders_with_relative_names() {
        let root = std::env::temp_dir().join(format!(
            "spoon-zip-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(root.join("world/region")).unwrap();
        fs::write(root.join("world/region/r.mca"), "data").unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        let dest = unique_zip_path(&root, "archive");
        zip_paths(&root, &[root.join("world"), root.join("a.txt")], &dest).unwrap();

        let mut archive = zip::ZipArchive::new(fs::File::open(&dest).unwrap()).unwrap();
        let mut names: Vec<String> = archive.file_names().map(String::from).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["a.txt", "world/", "world/region/", "world/region/r.mca"]
        );
        let mut text = String::new();
        io::Read::read_to_string(&mut archive.by_name("a.txt").unwrap(), &mut text).unwrap();
        assert_eq!(text, "a");
        assert_eq!(
            unique_zip_path(&root, "archive").file_name().unwrap(),
            "archive-2.zip"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    fn temp_scope() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("spoon-files-test-{nanos}"))
    }

    #[test]
    fn lists_directories_first_case_insensitively() {
        let root = temp_scope();
        fs::create_dir_all(root.join("beta")).unwrap();
        fs::create_dir_all(root.join("Alpha")).unwrap();
        fs::write(root.join("zeta.txt"), "z").unwrap();
        fs::write(root.join("Delta.txt"), "d").unwrap();

        let names: Vec<String> = list_dir(&root)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["Alpha", "beta", "Delta.txt", "zeta.txt"]);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn detects_binary_files_by_nul_bytes() {
        assert!(!is_binary(b"level-name=world\n"));
        assert!(!is_binary(&[]));
        assert!(is_binary(b"PK\x03\x04\0\0"));
        let large = vec![b'x'; SNIFF_LEN + 16];
        assert!(!is_binary(&large));
    }

    #[test]
    fn reads_text_and_rejects_binary_and_huge_files() {
        let root = temp_scope();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("note.txt"), "hello").unwrap();
        fs::write(root.join("data.bin"), [0x01_u8, 0x00, 0x02]).unwrap();
        let mut huge = String::with_capacity(MAX_EDITABLE_BYTES as usize + 16);
        for _ in 0..(MAX_EDITABLE_BYTES / 16 + 1) {
            huge.push_str("0123456789abcdef");
        }
        fs::write(root.join("huge.txt"), &huge).unwrap();

        match read_file(&root.join("note.txt")).unwrap() {
            FileContent::Text(text) => assert_eq!(text, "hello"),
            other => panic!(
                "expected text, got {:?}",
                matches!(other, FileContent::Binary)
            ),
        }
        assert!(matches!(
            read_file(&root.join("data.bin")).unwrap(),
            FileContent::Binary
        ));
        assert!(matches!(
            read_file(&root.join("huge.txt")).unwrap(),
            FileContent::TooLarge
        ));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ensure_within_confines_navigation_to_root() {
        let root = temp_scope();
        fs::create_dir_all(root.join("world/region")).unwrap();
        let root = root.canonicalize().unwrap();

        assert!(ensure_within(&root, &root.join("world")).is_ok());
        assert!(ensure_within(&root, &root.join("world/region")).is_ok());
        assert!(ensure_within(&root, &root.join("..")).is_err());

        fs::remove_dir_all(&root).unwrap();
    }
}
