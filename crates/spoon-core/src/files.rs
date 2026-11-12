use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
    /// Last modification time when the filesystem provides it.
    pub modified: Option<SystemTime>,
}

/// Metadata discovered for a file available at an HTTP(S) URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileInfo {
    pub filename: String,
    pub size: Option<u64>,
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
            modified: metadata.modified().ok(),
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

/// Queries a direct HTTP(S) file link for its filename and size without downloading its body.
pub fn query_remote_file(url: &str) -> Result<RemoteFileInfo, String> {
    let url = parse_remote_url(url)?;
    let client = remote_client(Duration::from_secs(30))?;
    let response = client
        .head(url.clone())
        .send()
        .map_err(|err| format!("Could not query file link: {err}"))?;
    let response = if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
        || response.status() == reqwest::StatusCode::NOT_IMPLEMENTED
    {
        client
            .get(url)
            .send()
            .map_err(|err| format!("Could not query file link: {err}"))?
    } else {
        response
    };
    if !response.status().is_success() {
        return Err(format!(
            "Could not query file link: server returned HTTP {}.",
            response.status()
        ));
    }

    let filename = remote_filename(&response)
        .ok_or_else(|| "Could not determine a filename from this link.".to_string())?;
    Ok(RemoteFileInfo {
        filename,
        size: response.content_length(),
    })
}

/// Downloads an HTTP(S) file into `directory` without replacing an existing file.
pub fn download_remote_file(url: &str, directory: &Path) -> Result<PathBuf, String> {
    let url = parse_remote_url(url)?;
    let client = remote_client(Duration::from_secs(600))?;
    let mut response = client
        .get(url)
        .send()
        .map_err(|err| format!("Could not download file: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Could not download file: server returned HTTP {}.",
            response.status()
        ));
    }

    let filename = remote_filename(&response)
        .ok_or_else(|| "Could not determine a filename from this link.".to_string())?;
    let (path, mut output) = create_unique_file(directory, &filename)
        .map_err(|err| format!("Could not create download file: {err}"))?;
    let result = io::copy(&mut response, &mut output).and_then(|_| output.flush());
    if let Err(err) = result {
        drop(output);
        return Err(remove_partial_file(
            &path,
            format!("Could not finish download: {err}"),
        ));
    }
    Ok(path)
}

/// Copies a local file into `directory`, choosing a new name rather than overwriting.
pub fn copy_file_into_dir(source: &Path, directory: &Path) -> io::Result<PathBuf> {
    let filename = source
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(safe_filename)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid source filename"))?;
    let mut input = fs::File::open(source)?;
    let (path, mut output) = create_unique_file(directory, &filename)?;
    if let Err(err) = io::copy(&mut input, &mut output).and_then(|_| output.flush()) {
        drop(output);
        return Err(io::Error::new(
            err.kind(),
            remove_partial_file(&path, format!("Could not copy file: {err}")),
        ));
    }
    Ok(path)
}

fn remote_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .user_agent(concat!("Spoon/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| format!("Could not create file download client: {err}"))
}

fn parse_remote_url(url: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(url.trim())
        .map_err(|err| format!("Enter a valid HTTP or HTTPS file link: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only HTTP and HTTPS file links are supported.".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("File links with embedded credentials are not supported.".to_string());
    }
    Ok(url)
}

fn remote_filename(response: &reqwest::blocking::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(filename_from_content_disposition)
        .or_else(|| {
            response
                .url()
                .path_segments()?
                .rev()
                .find(|segment| !segment.is_empty())
                .and_then(percent_decode)
                .and_then(|name| safe_filename(&name))
        })
}

fn filename_from_content_disposition(value: &str) -> Option<String> {
    let mut filename = None;
    for parameter in value.split(';').skip(1) {
        let Some((key, value)) = parameter.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if key.trim().eq_ignore_ascii_case("filename*") {
            if let Some((encoding, value)) = value.split_once("''") {
                if encoding.eq_ignore_ascii_case("utf-8") {
                    if let Some(decoded) =
                        percent_decode(value).and_then(|name| safe_filename(&name))
                    {
                        return Some(decoded);
                    }
                }
            }
        } else if key.trim().eq_ignore_ascii_case("filename") {
            filename = safe_filename(value);
        }
    }
    filename
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn safe_filename(filename: &str) -> Option<String> {
    let filename = filename.rsplit(['/', '\\']).next()?.trim();
    if filename.is_empty()
        || matches!(filename, "." | "..")
        || filename.ends_with('.')
        || filename.ends_with(' ')
        || filename
            .chars()
            .any(|character| character.is_control() || "<>:\"|?*".contains(character))
    {
        return None;
    }
    Some(filename.to_string())
}

fn create_unique_file(directory: &Path, filename: &str) -> io::Result<(PathBuf, fs::File)> {
    let filename = safe_filename(filename)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
    let file_path = Path::new(&filename);
    let stem = file_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("download");
    let extension = file_path.extension().and_then(|ext| ext.to_str());

    for suffix in 1..=u32::MAX {
        let candidate_name = match (suffix, extension) {
            (1, _) => filename.clone(),
            (_, Some(extension)) => format!("{stem}-{suffix}.{extension}"),
            (_, None) => format!("{stem}-{suffix}"),
        };
        let path = directory.join(candidate_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not find an unused filename",
    ))
}

fn remove_partial_file(path: &Path, message: String) -> String {
    match fs::remove_file(path) {
        Ok(()) => message,
        Err(err) => format!("{message}; could not remove partial file: {err}"),
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

/// Moves a file or directory into another directory inside `root`.
pub fn move_entry(root: &Path, source: &Path, destination_dir: &Path) -> io::Result<PathBuf> {
    if fs::symlink_metadata(source)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "moving symbolic links is not supported",
        ));
    }
    let root = root.canonicalize()?;
    let source = source.canonicalize()?;
    ensure_within(&root, &source)?;
    let destination_dir = destination_dir.canonicalize()?;
    ensure_within(&root, &destination_dir)?;
    if !destination_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "move destination is not a folder",
        ));
    }
    if source == root {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the server folder itself cannot be moved",
        ));
    }
    if destination_dir.starts_with(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder cannot be moved into itself",
        ));
    }
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid source path"))?;
    let destination = destination_dir.join(name);
    if destination == source {
        return Ok(source);
    }
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", destination.display()),
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    fs::rename(&source, &destination)?;
    Ok(destination)
}

/// Copies a directory and its contents, rejecting symbolic links.
pub fn copy_directory(source: &Path, target: &Path) -> io::Result<()> {
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("server folder does not exist: {}", source.display()),
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(target)?;
    match copy_directory_contents(source, target, false) {
        Ok(()) => Ok(()),
        Err(copy_error) => match fs::remove_dir_all(target) {
            Ok(()) => Err(copy_error),
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                Err(copy_error)
            }
            Err(cleanup_error) => Err(io::Error::new(
                copy_error.kind(),
                format!(
                    "{copy_error}; partial copy folder could not be removed ({}): {cleanup_error}",
                    target.display()
                ),
            )),
        },
    }
}

/// Copies a directory's contents into an existing target directory.
/// When `skip_root_server_jar` is true, only a root-level `server.jar` is skipped.
pub fn copy_directory_contents(
    source: &Path,
    target: &Path,
    skip_root_server_jar: bool,
) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "server folder contains a symbolic link: {}",
                    source_path.display()
                ),
            ));
        }
        if skip_root_server_jar && entry.file_name() == "server.jar" {
            continue;
        }
        if file_type.is_dir() {
            fs::create_dir_all(&target_path)?;
            copy_directory_contents(&source_path, &target_path, false)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported file in server folder: {}",
                    source_path.display()
                ),
            ));
        }
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

/// Returns whether `path` uses one of the supported archive formats.
pub fn is_archive(path: &Path) -> bool {
    archive_format(path).is_some()
}

/// Extracts a ZIP, TAR, TAR.GZ, TGZ, or GZ file into `destination` without overwriting files.
/// Unsafe archive paths and links are skipped.
pub fn uncompress_archive(archive: &Path, destination: &Path) -> Result<(), String> {
    let format = archive_format(archive)
        .ok_or_else(|| "This file format cannot be uncompressed.".to_string())?;
    let destination = destination
        .canonicalize()
        .map_err(|err| format!("Could not access destination folder: {err}"))?;
    if !destination.is_dir() {
        return Err("The destination is not a folder.".to_string());
    }
    if fs::symlink_metadata(archive)
        .map_err(|err| format!("Could not access archive: {err}"))?
        .file_type()
        .is_symlink()
    {
        return Err("Symbolic link archives cannot be uncompressed.".to_string());
    }
    let archive = archive
        .canonicalize()
        .map_err(|err| format!("Could not access archive: {err}"))?;
    ensure_within(&destination, &archive)
        .map_err(|err| format!("Archive must be inside the destination folder: {err}"))?;

    let mut created_files = Vec::new();
    let mut created_dirs = Vec::new();
    let result = match format {
        ArchiveFormat::Zip => extract_zip_into(
            &archive,
            &destination,
            &mut created_files,
            &mut created_dirs,
        ),
        ArchiveFormat::Tar => fs::File::open(&archive).and_then(|file| {
            extract_tar_into(file, &destination, &mut created_files, &mut created_dirs)
        }),
        ArchiveFormat::TarGz => extract_tar_gz_into(
            &archive,
            &destination,
            &mut created_files,
            &mut created_dirs,
        ),
        ArchiveFormat::Gz => extract_gz_into(
            &archive,
            &destination,
            &mut created_files,
            &mut created_dirs,
        ),
    };
    if let Err(err) = result {
        let cleanup = rollback_extraction(&created_files, &created_dirs);
        return Err(match cleanup {
            Some(cleanup) => format!("Could not uncompress archive: {err}; {cleanup}"),
            None => format!("Could not uncompress archive: {err}"),
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    Gz,
}

fn archive_format(path: &Path) -> Option<ArchiveFormat> {
    let filename = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "zip" => Some(ArchiveFormat::Zip),
            "tar" => Some(ArchiveFormat::Tar),
            "gz" => Some(ArchiveFormat::Gz),
            _ => None,
        }
    }
}

fn extract_zip_into(
    archive_path: &Path,
    destination: &Path,
    created_files: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let file = fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(io::Error::from)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(io::Error::from)?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            continue;
        }
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let Some(relative) = safe_archive_path(&relative) else {
            continue;
        };
        if entry.is_dir() {
            create_archive_directory(destination, &relative, created_dirs)?;
        } else {
            let mut output =
                create_archive_file(destination, &relative, created_files, created_dirs)?;
            io::copy(&mut entry, &mut output)?;
        }
    }
    Ok(())
}

fn extract_tar_into<R: io::Read>(
    reader: R,
    destination: &Path,
    created_files: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        let path = entry.path()?.into_owned();
        let Some(relative) = safe_archive_path(&path) else {
            continue;
        };
        if entry_type.is_dir() {
            create_archive_directory(destination, &relative, created_dirs)?;
        } else if entry_type.is_file() {
            let mut output =
                create_archive_file(destination, &relative, created_files, created_dirs)?;
            io::copy(&mut entry, &mut output)?;
        }
    }
    Ok(())
}

fn extract_tar_gz_into(
    archive_path: &Path,
    destination: &Path,
    created_files: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let file = fs::File::open(archive_path)?;
    extract_tar_into(
        flate2::read::GzDecoder::new(file),
        destination,
        created_files,
        created_dirs,
    )
}

fn extract_gz_into(
    archive_path: &Path,
    destination: &Path,
    created_files: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let filename = archive_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.to_ascii_lowercase().ends_with(".gz"))
        .map(|name| &name[..name.len() - 3])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid GZ filename"))?;
    let relative = safe_archive_path(Path::new(filename))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid GZ filename"))?;
    let mut input = flate2::read::GzDecoder::new(fs::File::open(archive_path)?);
    let mut output = create_archive_file(destination, &relative, created_files, created_dirs)?;
    io::copy(&mut input, &mut output)?;
    Ok(())
}

fn safe_archive_path(path: &Path) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => relative.push(name),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

fn create_archive_directory(
    destination: &Path,
    relative: &Path,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<PathBuf> {
    let relative = safe_archive_path(relative)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid archive path"))?;
    let mut current = destination.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("archive path uses a symbolic link: {}", current.display()),
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists and is not a folder", current.display()),
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                created_dirs.push(current.clone());
            }
            Err(err) => return Err(err),
        }
    }
    Ok(current)
}

fn create_archive_file(
    destination: &Path,
    relative: &Path,
    created_files: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> io::Result<fs::File> {
    let relative = safe_archive_path(relative)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid archive path"))?;
    if let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_archive_directory(destination, parent, created_dirs)?;
    }
    let path = destination.join(relative);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    created_files.push(path);
    Ok(file)
}

fn rollback_extraction(files: &[PathBuf], directories: &[PathBuf]) -> Option<String> {
    let mut failures = Vec::new();
    for path in files.iter().rev() {
        if let Err(err) = fs::remove_file(path) {
            if err.kind() != io::ErrorKind::NotFound {
                failures.push(format!("could not remove {}: {err}", path.display()));
            }
        }
    }
    for path in directories.iter().rev() {
        if let Err(err) = fs::remove_dir(path) {
            if err.kind() != io::ErrorKind::NotFound {
                failures.push(format!("could not remove {}: {err}", path.display()));
            }
        }
    }
    (!failures.is_empty()).then(|| failures.join("; "))
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
    fn copies_directory_contents_recursively() {
        let root = std::env::temp_dir().join(format!(
            "spoon-copy-test-{}-{}",
            std::process::id(),
            crate::new_id()
        ));
        let source = root.join("source");
        let target = root.join("copy");
        fs::create_dir_all(source.join("world")).unwrap();
        fs::write(source.join("server.jar"), b"jar").unwrap();
        fs::write(source.join("world").join("level.dat"), b"world").unwrap();

        copy_directory(&source, &target).unwrap();

        assert_eq!(fs::read(target.join("server.jar")).unwrap(), b"jar");
        assert_eq!(
            fs::read(target.join("world").join("level.dat")).unwrap(),
            b"world"
        );
        fs::remove_dir_all(root).unwrap();
    }

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

    #[test]
    fn remote_file_names_are_decoded_and_confined_to_a_leaf() {
        assert_eq!(
            filename_from_content_disposition(
                "attachment; filename=\"fallback.zip\"; filename*=UTF-8''world%20backup.zip"
            ),
            Some("world backup.zip".to_string())
        );
        assert_eq!(
            filename_from_content_disposition("attachment; filename=\"../server.jar\""),
            Some("server.jar".to_string())
        );
        assert_eq!(safe_filename(".."), None);
        assert_eq!(safe_filename("unsafe?.jar"), None);
        assert_eq!(safe_filename("server.jar"), Some("server.jar".to_string()));
    }

    #[test]
    fn remote_urls_require_http_or_https_without_embedded_credentials() {
        assert!(parse_remote_url("https://example.com/file.jar").is_ok());
        assert!(parse_remote_url("file:///server.jar").is_err());
        assert!(parse_remote_url("https://user:pass@example.com/file.jar").is_err());
    }

    #[test]
    fn copying_file_uses_a_unique_name_without_overwriting() {
        let root = temp_scope();
        let source = root.join("source").join("server.jar");
        let destination = root.join("destination");
        fs::create_dir_all(&destination).unwrap();
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"source contents").unwrap();
        fs::write(destination.join("server.jar"), b"keep existing").unwrap();

        let copied = copy_file_into_dir(&source, &destination).unwrap();

        assert_eq!(copied.file_name().unwrap(), "server-2.jar");
        assert_eq!(fs::read(&copied).unwrap(), b"source contents");
        assert_eq!(
            fs::read(destination.join("server.jar")).unwrap(),
            b"keep existing"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_detection_matches_supported_formats_but_not_jars() {
        for filename in [
            "world.zip",
            "world.TAR",
            "world.tar.gz",
            "world.TGZ",
            "world.Gz",
        ] {
            assert!(is_archive(Path::new(filename)), "{filename}");
        }
        assert!(!is_archive(Path::new("plugin.jar")));
        assert!(!is_archive(Path::new("world.txt")));
    }

    #[test]
    fn uncompressing_zip_extracts_files_without_overwriting() {
        let root = temp_scope();
        let archive_path = root.join("bundle.zip");
        fs::create_dir_all(&root).unwrap();
        let mut archive = zip::ZipWriter::new(fs::File::create(&archive_path).unwrap());
        archive
            .start_file("new/note.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"new file").unwrap();
        archive
            .start_file("existing.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"replacement").unwrap();
        archive.finish().unwrap();
        fs::write(root.join("existing.txt"), b"keep existing").unwrap();

        assert!(uncompress_archive(&archive_path, &root).is_err());
        assert!(!root.join("new").exists());
        assert_eq!(
            fs::read(root.join("existing.txt")).unwrap(),
            b"keep existing"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uncompressing_gz_creates_the_uncompressed_filename() {
        let root = temp_scope();
        fs::create_dir_all(&root).unwrap();
        let archive_path = root.join("server.properties.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            fs::File::create(&archive_path).unwrap(),
            flate2::Compression::default(),
        );
        encoder.write_all(b"motd=hello").unwrap();
        encoder.finish().unwrap();

        uncompress_archive(&archive_path, &root).unwrap();

        assert_eq!(
            fs::read(root.join("server.properties")).unwrap(),
            b"motd=hello"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uncompressing_tar_tar_gz_and_tgz_extracts_contents() {
        let root = temp_scope();
        let mut tar_archive = tar::Builder::new(Vec::new());
        let contents = b"packed file";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar_archive
            .append_data(&mut header, "nested/file.txt", &contents[..])
            .unwrap();
        let tar_bytes = tar_archive.into_inner().unwrap();

        for filename in ["bundle.tar", "bundle.tar.gz", "bundle.tgz"] {
            let destination = root.join(filename.replace('.', "_"));
            fs::create_dir_all(&destination).unwrap();
            let archive_path = destination.join(filename);
            if filename.ends_with(".tar") {
                fs::write(&archive_path, &tar_bytes).unwrap();
            } else {
                let mut encoder = flate2::write::GzEncoder::new(
                    fs::File::create(&archive_path).unwrap(),
                    flate2::Compression::default(),
                );
                encoder.write_all(&tar_bytes).unwrap();
                encoder.finish().unwrap();
            }

            uncompress_archive(&archive_path, &destination).unwrap();
            assert_eq!(
                fs::read(destination.join("nested/file.txt")).unwrap(),
                contents
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moving_entries_is_confined_and_does_not_overwrite() {
        let root = temp_scope();
        let source_dir = root.join("source");
        let target_dir = root.join("target");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(source_dir.join("config.txt"), b"source").unwrap();
        fs::write(target_dir.join("config.txt"), b"keep").unwrap();
        let root = root.canonicalize().unwrap();
        let source = root.join("source/config.txt");
        let target = root.join("target");

        assert!(move_entry(&root, &source, &target).is_err());
        assert!(source.exists());
        assert_eq!(fs::read(target.join("config.txt")).unwrap(), b"keep");
        assert!(move_entry(&root, &source, &std::env::temp_dir()).is_err());

        fs::remove_dir_all(root).unwrap();
    }
}
