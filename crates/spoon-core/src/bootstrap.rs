//! Managed self-install: a release build copies itself into the Spoon folder
//! on first run, adds desktop / start-menu shortcuts, and always relaunches
//! from the managed copy. Updates then replace the managed executable in one
//! restart instead of leaving a download for the user to swap by hand.
//!
//! Only Windows release builds use this flow for now; other platforms keep
//! downloading updates into the updates folder.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::updater::asset_name;

/// File name of the managed executable in the Spoon folder. Shorter than the
/// release asset name, which only downloads use.
pub const MANAGED_EXE_NAME: &str = "Spoon-Launcher.exe";

/// How long applying an update retries the file swap and how often, for
/// transient locks (antimalware scanning the fresh download, and the like).
const APPLY_COPY_TIMEOUT: Duration = Duration::from_secs(10);
const APPLY_COPY_INTERVAL: Duration = Duration::from_millis(200);
const CLEANUP_RETRY_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// Whether this build manages its own install: first-run setup into the Spoon
/// folder, shortcuts, relaunching from the managed copy, and restart-to-apply
/// updates. Debug builds are excluded so `cargo run` never tries to replace
/// itself with a release download.
pub fn is_managed_flow_enabled() -> bool {
    cfg!(all(target_os = "windows", not(debug_assertions))) && asset_name().is_some()
}

/// The Spoon folder that holds the app data and the managed executable
/// (`%APPDATA%\Spoon` on Windows).
pub fn install_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("Spoon"))
}

/// The executable Spoon always relaunches from,
/// `%APPDATA%\Spoon\Spoon-Launcher.exe`.
pub fn managed_exe() -> Option<PathBuf> {
    install_dir().map(|dir| managed_exe_in(&dir))
}

fn managed_exe_in(dir: &Path) -> PathBuf {
    dir.join(MANAGED_EXE_NAME)
}

/// True when the running executable is the managed copy in the Spoon folder.
pub fn running_from_managed() -> bool {
    let (Ok(current), Some(managed)) = (std::env::current_exe(), managed_exe()) else {
        return false;
    };
    same_path(&current, &managed)
}

/// First run of a release build: no managed copy exists yet, so the app must
/// install the newer of the launched build and the latest published release.
pub fn needs_first_run_setup() -> bool {
    is_managed_flow_enabled()
        && !running_from_managed()
        && !managed_exe().is_some_and(|exe| exe.exists())
}

/// A managed install exists but the user launched a different copy (e.g. the
/// original download in their Downloads folder); redirect to the managed
/// executable so updates always apply to the app actually in use.
pub fn should_relaunch_from_managed() -> bool {
    is_managed_flow_enabled() && !running_from_managed() && !needs_first_run_setup()
}

/// Start the executable at `path` (working directory = its folder).
pub fn launch(path: &Path) -> io::Result<std::process::Child> {
    let work_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Command::new(path).current_dir(work_dir).spawn()
}

/// Rename a setup download (still carrying the release asset name) to the
/// managed executable name. Returns the managed path.
pub fn place_downloaded_exe(downloaded: &Path) -> Result<PathBuf, String> {
    let Some(managed) = managed_exe() else {
        return Err("could not determine the managed executable path".to_string());
    };
    if same_path(downloaded, &managed) {
        return Ok(managed);
    }
    fs::rename(downloaded, &managed).map_err(|err| {
        format!(
            "could not rename {} to {}: {err}",
            downloaded.display(),
            managed.display()
        )
    })?;
    Ok(managed)
}

/// Copy the currently running executable into the managed install folder.
pub fn place_running_exe() -> Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|err| format!("could not determine the running executable path: {err}"))?;
    let Some(managed) = managed_exe() else {
        return Err("could not determine the managed executable path".to_string());
    };
    copy_executable(&current, &managed)?;
    Ok(managed)
}

fn copy_executable(source: &Path, target: &Path) -> Result<(), String> {
    if same_path(source, target) {
        return Ok(());
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("could not determine the folder for {}", target.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("could not create {}: {err}", parent.display()))?;

    let Some(file_name) = target.file_name() else {
        return Err(format!(
            "could not determine the file name for {}",
            target.display()
        ));
    };
    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(".setup-{}.tmp", std::process::id()));
    let temp = target.with_file_name(temp_name);
    fs::copy(source, &temp).map_err(|err| {
        let _ = fs::remove_file(&temp);
        format!(
            "could not copy {} to {}: {err}",
            source.display(),
            target.display()
        )
    })?;
    if let Err(err) = fs::rename(&temp, target) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "could not install {} as {}: {err}",
            source.display(),
            target.display()
        ));
    }
    Ok(())
}

/// Swap a downloaded update in for the managed executable.
///
/// Windows refuses to overwrite a running image but allows renaming it, so
/// the running exe moves aside first and the downloaded copy takes its name.
/// The moved-aside copy is deleted on the next launch (see
/// [`cleanup_install_leftovers`]). If the copy fails, the rename is rolled
/// back so the app still has an executable to run.
pub fn apply_update_download(downloaded: &Path, target: &Path) -> Result<(), String> {
    let backup = apply_backup_path(target);
    let _ = fs::remove_file(&backup);
    fs::rename(target, &backup)
        .map_err(|err| format!("could not move {} aside: {err}", target.display()))?;
    match copy_replacing(
        downloaded,
        target,
        APPLY_COPY_TIMEOUT,
        APPLY_COPY_INTERVAL,
        &std::thread::sleep,
    ) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::rename(&backup, target);
            Err(err)
        }
    }
}

fn apply_backup_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".old");
    target.with_file_name(name)
}

/// Best-effort removal of files left by earlier installs or applied updates.
/// Never removes `current` itself. Retries a locked update backup briefly
/// because the previous app process may still be shutting down.
/// Returns true when something was removed (the old shortcuts may then point
/// at a name that no longer exists).
pub fn cleanup_install_leftovers(current: &Path) -> bool {
    let Some(dir) = install_dir() else {
        return false;
    };
    let backup = apply_backup_path(&managed_exe_in(&dir));
    let retry_backup = backup != current && backup.exists();
    let deadline = std::time::Instant::now() + CLEANUP_RETRY_TIMEOUT;
    let mut removed = false;

    loop {
        removed |= install_leftovers_in(&dir, current);
        if !retry_backup || !backup.exists() || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(CLEANUP_RETRY_INTERVAL);
    }

    removed
}

fn install_leftovers_in(dir: &Path, current: &Path) -> bool {
    let mut removed = false;
    let backup = apply_backup_path(&managed_exe_in(dir));
    let applied_update = backup != current && backup.exists();
    if let Some(asset) = asset_name() {
        removed |= remove_if_not_current(&dir.join(asset), current);
        if applied_update {
            let downloaded = dir.join("updates").join(asset);
            let downloaded_removed = remove_if_not_current(&downloaded, current);
            removed |= downloaded_removed;
            if downloaded_removed || !downloaded.exists() {
                removed |= remove_if_not_current(&backup, current);
            }
        } else {
            removed |= remove_if_not_current(&backup, current);
        }
    } else {
        removed |= remove_if_not_current(&backup, current);
    }
    removed
}

fn remove_if_not_current(path: &Path, current: &Path) -> bool {
    if path != current && path.exists() {
        return fs::remove_file(path).is_ok();
    }
    false
}

fn copy_replacing(
    source: &Path,
    target: &Path,
    timeout: Duration,
    interval: Duration,
    sleep: &dyn Fn(Duration),
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match fs::copy(source, target) {
            Ok(_) => return Ok(()),
            Err(err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "could not replace {} with {}: {err}",
                        target.display(),
                        source.display()
                    ));
                }
                sleep(interval);
            }
        }
    }
}

/// Create a "Spoon Launcher" shortcut to `exe` on the desktop and in the
/// start menu. No-op on platforms without the managed flow.
pub fn create_shortcuts(exe: &Path) -> Result<(), String> {
    create_shortcuts_platform(exe)
}

#[cfg(target_os = "windows")]
fn create_shortcuts_platform(exe: &Path) -> Result<(), String> {
    use windows::Win32::System::Com::{
        CoGetApartmentType, CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
    };

    let desktop =
        dirs::desktop_dir().ok_or_else(|| "could not find the desktop folder".to_string())?;
    let start_menu = dirs::config_dir()
        .map(|dir| {
            dir.join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
        })
        .ok_or_else(|| "could not find the start menu folder".to_string())?;

    // Balance the COM init only when this thread didn't already have an
    // apartment (the GUI thread gets one from the windowing runtime).
    let mut apt_type = windows::Win32::System::Com::APTTYPE(0);
    let mut apt_qualifier = windows::Win32::System::Com::APTTYPEQUALIFIER(0);
    let already_initialized =
        unsafe { CoGetApartmentType(&mut apt_type, &mut apt_qualifier) }.is_ok();
    if !already_initialized {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() {
            return Err(format!("could not initialize COM: {hr:?}"));
        }
    }
    let result = (|| {
        for lnk in [
            desktop.join("Spoon Launcher.lnk"),
            start_menu.join("Spoon Launcher.lnk"),
        ] {
            create_shortcut(&lnk, exe)?;
        }
        Ok(())
    })();
    if !already_initialized {
        unsafe { CoUninitialize() };
    }
    result
}

/// Create one .lnk pointing at `exe` via IShellLink (in-process COM).
#[cfg(target_os = "windows")]
fn create_shortcut(lnk: &Path, exe: &Path) -> Result<(), String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::IPersistFile;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    let link: IShellLinkW = unsafe {
        CoCreateInstance(&ShellLink, None, CLSCTX_ALL)
            .map_err(|err| format!("could not create ShellLink object: {err}"))?
    };
    let target = to_wide(exe);
    let work_dir = to_wide(exe.parent().unwrap_or(exe));
    unsafe {
        link.SetPath(PCWSTR(target.as_ptr()))
            .and_then(|_| link.SetWorkingDirectory(PCWSTR(work_dir.as_ptr())))
            .and_then(|_| link.SetIconLocation(PCWSTR(target.as_ptr()), 0))
            .map_err(|err| format!("could not configure shortcut: {err}"))?;
        let persist: IPersistFile = link
            .cast()
            .map_err(|err| format!("could not get IPersistFile: {err}"))?;
        persist
            .Save(PCWSTR(to_wide(lnk).as_ptr()), true)
            .map_err(|err| format!("could not save {}: {err}", lnk.display()))?;
    }
    Ok(())
}

/// NUL-terminated wide string for the Windows APIs.
#[cfg(target_os = "windows")]
fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn create_shortcuts_platform(_exe: &Path) -> Result<(), String> {
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spoon-bootstrap-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn managed_exe_uses_short_name() {
        let exe = managed_exe_in(Path::new("C:\\Spoon"));
        assert_eq!(exe.file_name().unwrap(), MANAGED_EXE_NAME);
    }

    #[test]
    fn copying_running_exe_preserves_the_source_and_installs_a_copy() {
        let dir = temp_test_dir("copy-running-exe");
        let source = dir.join("Downloads").join(MANAGED_EXE_NAME);
        let target = dir.join("Spoon").join(MANAGED_EXE_NAME);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"newer local version").unwrap();

        copy_executable(&source, &target).unwrap();

        assert_eq!(fs::read(&source).unwrap(), b"newer local version");
        assert_eq!(fs::read(&target).unwrap(), b"newer local version");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_update_swaps_download_in_and_keeps_backup() {
        let dir = temp_test_dir("apply-swap");
        let downloaded = dir.join("downloaded.exe");
        let target = dir.join(MANAGED_EXE_NAME);
        fs::write(&downloaded, b"new version").unwrap();
        fs::write(&target, b"old version").unwrap();

        apply_update_download(&downloaded, &target).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new version");
        assert_eq!(
            fs::read(apply_backup_path(&target)).unwrap(),
            b"old version"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_update_rolls_back_when_the_copy_fails() {
        let dir = temp_test_dir("apply-rollback");
        let downloaded = dir.join("downloaded.exe");
        fs::write(&downloaded, b"new version").unwrap();
        // A directory as the target makes the copy fail deterministically.
        apply_update_download(&downloaded, &dir).unwrap_err();
        assert!(dir.exists(), "the rolled-back folder must still be there");
        assert!(!apply_backup_path(&dir).exists(), "no backup left behind");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_legacy_and_backup_but_never_current() {
        let dir = temp_test_dir("cleanup");
        let current = dir.join(MANAGED_EXE_NAME);
        fs::write(&current, b"current").unwrap();
        let asset = asset_name().map(|name| dir.join(name));
        if let Some(legacy) = &asset {
            fs::write(legacy, b"legacy").unwrap();
        }
        let downloaded = asset_name().map(|name| {
            let path = dir.join("updates").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"downloaded").unwrap();
            path
        });
        let backup = apply_backup_path(&current);
        fs::write(&backup, b"backup").unwrap();
        let unrelated = dir.join("servers.json");
        fs::write(&unrelated, b"{}").unwrap();

        let removed = install_leftovers_in(&dir, &current);

        assert!(removed);
        assert!(current.exists());
        assert!(!backup.exists());
        if let Some(legacy) = &asset {
            assert!(!legacy.exists());
        }
        if let Some(downloaded) = &downloaded {
            assert!(!downloaded.exists());
        }
        assert!(unrelated.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_preserves_a_download_that_has_not_been_applied() {
        let dir = temp_test_dir("cleanup-pending-update");
        let current = managed_exe_in(&dir);
        fs::write(&current, b"current").unwrap();
        let downloaded = asset_name().map(|name| {
            let path = dir.join("updates").join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"downloaded").unwrap();
            path
        });

        install_leftovers_in(&dir, &current);

        if let Some(downloaded) = &downloaded {
            assert!(downloaded.exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_reports_nothing_removed_for_a_clean_install() {
        let dir = temp_test_dir("cleanup-clean");
        let current = managed_exe_in(&dir);
        fs::write(&current, b"current").unwrap();
        assert!(!install_leftovers_in(&dir, &current));
        assert!(current.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_path_matches_same_file_through_dot_segments() {
        let dir = temp_test_dir("same-path");
        let file = dir.join("app.exe");
        fs::write(&file, b"spoon").unwrap();
        let dot_path = dir.join(".").join("app.exe");
        assert!(same_path(&file, &dot_path));
        let other = dir.join("other.exe");
        fs::write(&other, b"spoon").unwrap();
        assert!(!same_path(&file, &other));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_replacing_overwrites_target() {
        let dir = temp_test_dir("copy-replacing");
        let source = dir.join("new.exe");
        let target = dir.join("managed.exe");
        fs::write(&source, b"new version").unwrap();
        fs::write(&target, b"old version").unwrap();
        copy_replacing(
            &source,
            &target,
            Duration::from_secs(1),
            Duration::from_millis(1),
            &|_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new version");
        let _ = fs::remove_dir_all(&dir);
    }

    /// COM shortcut creation must produce a .lnk that resolves back to the
    /// target, or desktop / start-menu shortcuts would be dead.
    #[cfg(target_os = "windows")]
    #[test]
    fn creates_shortcut_that_round_trips_to_target() {
        use windows::core::{Interface, PCWSTR};
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_ALL,
            COINIT_APARTMENTTHREADED, STGM_READ,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

        let dir = temp_test_dir("shortcut");
        let exe = dir.join("app.exe");
        fs::write(&exe, b"spoon").unwrap();
        let lnk = dir.join("app.lnk");

        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .expect("init COM");
        create_shortcut(&lnk, &exe).unwrap();
        assert!(lnk.exists());

        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_ALL) }.expect("create link");
        let persist: IPersistFile = link.cast().expect("IPersistFile");
        unsafe { persist.Load(PCWSTR(to_wide(&lnk).as_ptr()), STGM_READ) }.expect("load lnk");
        let mut buf = vec![0u16; 260];
        unsafe { link.GetPath(&mut buf, std::ptr::null_mut(), 0) }.expect("read target");
        let len = buf
            .iter()
            .position(|&c| c == 0)
            .expect("nul-terminated path");
        let loaded = PathBuf::from(String::from_utf16(&buf[..len]).unwrap());
        assert!(
            same_path(&loaded, &exe),
            "shortcut points at {loaded:?}, expected {exe:?}"
        );
        unsafe { CoUninitialize() };
        let _ = fs::remove_dir_all(&dir);
    }
}
