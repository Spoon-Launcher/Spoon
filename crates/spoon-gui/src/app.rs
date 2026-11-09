use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spoon_core::appearance::{
    clear_server_icon, read_server_icon, server_icon_path, write_server_icon,
};
use spoon_core::backup::{BackupInfo, BackupStore};
use spoon_core::bootstrap;
use spoon_core::eula::{accept_eula, eula_state, EulaState};
use spoon_core::files::{self, FileContent, FileEntry};
use spoon_core::highlight::{self, Language, TokenKind};
use spoon_core::java::{fetch_candidates, JavaCandidate, JavaRuntime, JavaStore};
use spoon_core::mcjars::{McJarsClient, ServerType, SoftwareSnapshot, VersionEntry};
use spoon_core::players::{
    add_ip_ban, add_player_ban, add_whitelist_player, fetch_player_skin, is_minecraft_username,
    parse_position_command_output, read_player_catalog, read_player_details, remove_ip_ban,
    remove_player_ban, remove_whitelist_player, PlayerCatalog, PlayerDetails, PlayerProfile,
};
use spoon_core::updater::{updates_dir, UpdateClient, UpdateInfo};
use spoon_core::{
    new_id, next_available_port, read_property, set_properties, unique_server_dir,
    write_server_properties, AppSettings, LogLine, ServerConfig, ServerHandle, ServerState,
    ServerStore, DEFAULT_SERVER_PORT,
};

use crate::theme;
use sysinfo::{Pid, ProcessesToUpdate, System};

const MAX_CONSOLE_LINES: usize = 10_000;
/// Space the console leaves below its output for the command input.
const COMMAND_BAR_HEIGHT: f32 = 78.0;
/// Space the console leaves below itself for the software box and its gap.
const SOFTWARE_CARD_HEIGHT: f32 = 70.0;
/// How often the MCJars catalog (software and versions) is re-checked.
const SOFTWARE_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How soon to retry when no catalog has loaded yet (e.g. launched offline).
const SOFTWARE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// Icon textures built per frame, so a burst of finished downloads can't stall one frame.
const MAX_ICONS_PER_FRAME: usize = 4;
const MAX_PLAYER_SKINS_IN_FLIGHT: usize = 4;
const PLAYER_MANAGEMENT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const BACKUP_STATUS_DURATION: Duration = Duration::from_secs(4);
/// Width of the dashboard icon rail: two 8px paddings, a 36px button and the border.
const RAIL_STRIP_WIDTH: f32 = 8.0 + 36.0 + 8.0 + 2.0;
/// The dashboard pages in rail order: page, icon glyph, hover label.
const RAIL_PAGES: [(ServerPage, &str, &str); 5] = [
    (ServerPage::Overview, theme::icons::GAUGE_HIGH, "Overview"),
    (ServerPage::Files, theme::icons::FOLDER, "Files"),
    (ServerPage::Players, theme::icons::USERS, "Players"),
    (ServerPage::Backups, theme::icons::ARCHIVE, "Backups"),
    (ServerPage::Settings, theme::icons::GEAR, "Settings"),
];

/// Result of one software icon download: the lowercase software name plus the raw PNG bytes.
type IconFetch = (String, Result<Vec<u8>, String>);
type PlayerSkinFetch = (String, Result<Vec<u8>, String>);

#[derive(Clone, Copy)]
enum SocialButtonIcon {
    Discord,
    KoFi,
}

fn social_link_button(
    ui: &mut egui::Ui,
    width: f32,
    label: &str,
    hover_text: &str,
    fill: egui::Color32,
    icon: SocialButtonIcon,
) -> bool {
    let response = ui
        .add_sized(
            [width, 32.0],
            theme::animated_button(
                egui::Button::new(""),
                fill,
                fill.linear_multiply(1.1),
                egui::Stroke::NONE,
                egui::Stroke::NONE,
            ),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(hover_text);
    let rect = response.rect;
    let painter = ui.painter();
    let text = painter.layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(12.5),
        egui::Color32::WHITE,
    );
    let icon_size = egui::vec2(18.0, 16.0);
    let gap = 4.0;
    let content_width = icon_size.x + gap + text.size().x;
    let content_left = rect.center().x - content_width * 0.5;
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(content_left, rect.center().y - icon_size.y * 0.5),
        icon_size,
    );

    let glyph = match icon {
        SocialButtonIcon::Discord => theme::brand_icons::DISCORD,
        SocialButtonIcon::KoFi => theme::brand_icons::KO_FI,
    };
    painter.text(
        icon_rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::new(16.0, theme::brand_icon_family()),
        egui::Color32::WHITE,
    );

    painter.galley(
        egui::pos2(
            content_left + icon_size.x + gap,
            rect.center().y - text.size().y * 0.5,
        ),
        text,
        egui::Color32::WHITE,
    );
    response.clicked()
}

/// The MCJars software and version catalog, loaded at launch and refreshed in
/// the background so the pickers never wait on the network.
struct SoftwareCache {
    snapshot: Option<Arc<SoftwareSnapshot>>,
    error: Option<String>,
    refresh_rx: Option<Receiver<Result<SoftwareSnapshot, String>>>,
    /// When the last refresh finished.
    refreshed: Instant,
}

impl SoftwareCache {
    fn new() -> Self {
        let mut cache = Self {
            snapshot: None,
            error: None,
            refresh_rx: None,
            refreshed: Instant::now(),
        };
        cache.start_refresh();
        cache
    }

    fn start_refresh(&mut self) {
        let previous = self.snapshot.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = McJarsClient::new()
                .snapshot(previous.as_deref())
                .map_err(|err| err.to_string());
            let _ = tx.send(result);
        });
        self.refresh_rx = Some(rx);
    }

    /// True while the first catalog is still loading.
    fn is_loading(&self) -> bool {
        self.snapshot.is_none() && self.refresh_rx.is_some()
    }

    /// Versions of one software, oldest to newest; empty until the catalog loads.
    fn versions(&self, server_type: &ServerType) -> &[VersionEntry] {
        self.snapshot
            .as_deref()
            .map(|snapshot| snapshot.versions(server_type))
            .unwrap_or_default()
    }

    /// Applies a finished refresh and starts the next one when due. Returns
    /// true when a new catalog arrived.
    fn poll(&mut self, ctx: &egui::Context) -> bool {
        let Some(rx) = &self.refresh_rx else {
            let interval = if self.snapshot.is_some() {
                SOFTWARE_REFRESH_INTERVAL
            } else {
                SOFTWARE_RETRY_INTERVAL
            };
            match interval.checked_sub(self.refreshed.elapsed()) {
                Some(remaining) => ctx.request_repaint_after(remaining),
                None => {
                    self.start_refresh();
                    ctx.request_repaint();
                }
            }
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Software refresh stopped unexpectedly.".to_string())
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(250));
                return false;
            }
        };
        self.refresh_rx = None;
        self.refreshed = Instant::now();
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(Arc::new(snapshot));
                self.error = None;
                true
            }
            Err(err) => {
                self.error = Some(err);
                false
            }
        }
    }
}

/// The dashboard pages each server shows in the main panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ServerPage {
    Overview,
    Files,
    Players,
    Backups,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlayerView {
    Online,
    Offline,
    Whitelist,
    IpBans,
}

enum PlayerManagementResult {
    Added(PlayerProfile),
    Removed(String),
    Banned(PlayerProfile),
    Unbanned(String),
    IpBanned(String),
    IpUnbanned(String),
}

struct PlayerManagementState {
    view: PlayerView,
    catalog: Option<PlayerCatalog>,
    username: String,
    ip_address: String,
    pending: Option<Receiver<Result<PlayerManagementResult, String>>>,
    refresh_pending: Option<Receiver<Result<PlayerCatalog, String>>>,
    last_refresh: Option<Instant>,
    error: Option<String>,
    notice: Option<String>,
}

impl PlayerManagementState {
    fn new() -> Self {
        Self {
            view: PlayerView::Online,
            catalog: None,
            username: String::new(),
            ip_address: String::new(),
            pending: None,
            refresh_pending: None,
            last_refresh: None,
            error: None,
            notice: None,
        }
    }

    fn refresh(&mut self, working_dir: PathBuf, clear_feedback: bool) {
        if self.pending.is_some() || self.refresh_pending.is_some() {
            return;
        }
        if clear_feedback {
            self.error = None;
            self.notice = None;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = read_player_catalog(&working_dir);
            let _ = tx.send(result);
        });
        self.refresh_pending = Some(rx);
    }

    fn poll_refresh(&mut self) -> bool {
        let result = self.refresh_pending.as_ref().map(Receiver::try_recv);
        match result {
            Some(Ok(Ok(catalog))) => {
                self.refresh_pending = None;
                self.catalog = Some(catalog);
                self.error = None;
                self.notice = None;
            }
            Some(Ok(Err(err))) => {
                self.refresh_pending = None;
                self.error = Some(err);
                self.notice = None;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.refresh_pending = None;
                self.error = Some("Player catalog refresh stopped unexpectedly.".to_string());
                self.notice = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) => return true,
            None => return false,
        }
        false
    }

    fn add(&mut self, working_dir: PathBuf) {
        if self.pending.is_some() {
            return;
        }
        let username = self.username.trim().to_string();
        if !is_minecraft_username(&username) {
            self.error = Some(
                "Enter a valid Minecraft username (1-16 letters, numbers, or underscores)."
                    .to_string(),
            );
            self.notice = None;
            return;
        }
        self.refresh_pending = None;
        self.error = None;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result =
                add_whitelist_player(&working_dir, &username).map(PlayerManagementResult::Added);
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn remove(&mut self, working_dir: PathBuf, uuid: String) {
        if self.pending.is_some() {
            return;
        }
        self.refresh_pending = None;
        self.error = None;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = remove_whitelist_player(&working_dir, &uuid)
                .map(|()| PlayerManagementResult::Removed(uuid));
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn ban_player(&mut self, working_dir: PathBuf, username: String) {
        if self.pending.is_some() {
            return;
        }
        self.refresh_pending = None;
        self.error = None;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result =
                add_player_ban(&working_dir, &username).map(PlayerManagementResult::Banned);
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn unban_player(&mut self, working_dir: PathBuf, uuid: String) {
        if self.pending.is_some() {
            return;
        }
        self.refresh_pending = None;
        self.error = None;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = remove_player_ban(&working_dir, &uuid)
                .map(|()| PlayerManagementResult::Unbanned(uuid));
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn ban_ip(&mut self, working_dir: PathBuf, ip: String) {
        if self.pending.is_some() {
            return;
        }
        self.refresh_pending = None;
        self.error = None;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = add_ip_ban(&working_dir, &ip).map(PlayerManagementResult::IpBanned);
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn unban_ip(&mut self, working_dir: PathBuf, ip: String) {
        if self.pending.is_some() {
            return;
        }
        self.refresh_pending = None;
        self.error = None;
        self.notice = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result =
                remove_ip_ban(&working_dir, &ip).map(|()| PlayerManagementResult::IpUnbanned(ip));
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn poll(&mut self, handle: Option<&mut ServerHandle>) -> bool {
        let result = self.pending.as_ref().map(Receiver::try_recv);
        match result {
            Some(Ok(Ok(PlayerManagementResult::Added(profile)))) => {
                self.pending = None;
                let whitelist = &mut self
                    .catalog
                    .get_or_insert_with(PlayerCatalog::default)
                    .whitelist;
                whitelist.push(profile.clone());
                whitelist.sort_by(|left, right| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                });
                self.username.clear();
                self.notice = Some(format!("Added {} to the whitelist.", profile.name));
                if let Some(handle) = handle {
                    if handle.state() != ServerState::Stopped {
                        handle.send_command("whitelist reload");
                    }
                }
            }
            Some(Ok(Ok(PlayerManagementResult::Removed(uuid)))) => {
                self.pending = None;
                if let Some(catalog) = &mut self.catalog {
                    catalog
                        .whitelist
                        .retain(|profile| !profile.uuid.eq_ignore_ascii_case(&uuid));
                }
                self.notice = Some("Removed player from the whitelist.".to_string());
                if let Some(handle) = handle {
                    if handle.state() != ServerState::Stopped {
                        handle.send_command("whitelist reload");
                    }
                }
            }
            Some(Ok(Ok(PlayerManagementResult::Banned(profile)))) => {
                self.pending = None;
                let banned_players = &mut self
                    .catalog
                    .get_or_insert_with(PlayerCatalog::default)
                    .banned_players;
                banned_players.push(profile.clone());
                banned_players.sort_by(|left, right| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                });
                self.notice = Some(format!(
                    "Banned {} for the next server start.",
                    profile.name
                ));
            }
            Some(Ok(Ok(PlayerManagementResult::Unbanned(uuid)))) => {
                self.pending = None;
                if let Some(catalog) = &mut self.catalog {
                    catalog
                        .banned_players
                        .retain(|profile| !profile.uuid.eq_ignore_ascii_case(&uuid));
                }
                self.notice = Some("Removed the player ban for the next server start.".to_string());
            }
            Some(Ok(Ok(PlayerManagementResult::IpBanned(ip)))) => {
                self.pending = None;
                self.ip_address.clear();
                let banned_ips = &mut self
                    .catalog
                    .get_or_insert_with(PlayerCatalog::default)
                    .banned_ips;
                banned_ips.push(ip.clone());
                banned_ips.sort_by_key(|address| address.to_ascii_lowercase());
                self.notice = Some(format!("Banned IP {ip} for the next server start."));
            }
            Some(Ok(Ok(PlayerManagementResult::IpUnbanned(ip)))) => {
                self.pending = None;
                if let Some(catalog) = &mut self.catalog {
                    catalog.banned_ips.retain(|address| {
                        address
                            .parse::<IpAddr>()
                            .map(|existing| existing.to_string() != ip)
                            .unwrap_or(true)
                    });
                }
                self.notice = Some(format!(
                    "Removed IP ban for {ip} for the next server start."
                ));
            }
            Some(Ok(Err(err))) => {
                self.pending = None;
                self.error = Some(err);
                self.notice = None;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.pending = None;
                self.error = Some("Player management task stopped unexpectedly.".to_string());
                self.notice = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) => return true,
            None => return false,
        }
        false
    }
}

struct PlayerDetailsDialog {
    server_index: usize,
    name: String,
    is_online: bool,
    details_rx: Option<Receiver<Result<PlayerDetails, String>>>,
    details: Option<PlayerDetails>,
    error: Option<String>,
    action_error: Option<String>,
    action_notice: Option<String>,
    message: String,
    live_position: Option<[f64; 3]>,
    position_query_started: Option<Instant>,
}

enum PlayerDetailsAction {
    SendCommand {
        command: String,
        notice: String,
        requires_online: bool,
    },
    AddPlayerBan,
    RemovePlayerBan(String),
    AddIpBan(String),
    RemoveIpBan(String),
}

struct ImportDialog {
    name: String,
    source_dir: Option<PathBuf>,
    error: Option<String>,
    pending: Option<Receiver<Result<ServerConfig, String>>>,
}

impl ImportDialog {
    fn new() -> Self {
        Self {
            name: String::new(),
            source_dir: None,
            error: None,
            pending: None,
        }
    }

    fn is_importing(&self) -> bool {
        self.pending.is_some()
    }

    fn choose_folder(&mut self) {
        let Some(folder) = rfd::FileDialog::new()
            .set_title("Select an existing server folder")
            .pick_folder()
        else {
            return;
        };
        match fs::canonicalize(&folder) {
            Ok(folder) => {
                self.source_dir = Some(folder);
                self.error = None;
            }
            Err(err) => self.error = Some(format!("Could not open server folder: {err}")),
        }
    }

    fn start_import(&mut self, servers_root: &Path) {
        self.error = None;
        let name = self.name.trim().to_string();
        if name.is_empty() {
            self.error = Some("Name is required.".to_string());
            return;
        }
        let Some(source_dir) = self.source_dir.clone() else {
            self.error = Some("Choose a server folder to import.".to_string());
            return;
        };
        let jar_path = match select_server_jar(&source_dir) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(err) => {
                self.error = Some(err);
                return;
            }
        };
        let relative_jar = match jar_path.strip_prefix(&source_dir) {
            Ok(path) => path.to_path_buf(),
            Err(_) => {
                self.error = Some(
                    "The selected server JAR must be directly inside its server folder.".into(),
                );
                return;
            }
        };

        let servers_root = servers_root.to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                fs::create_dir_all(&servers_root)
                    .map_err(|err| format!("Could not create Spoon server folder: {err}"))?;
                let servers_root = fs::canonicalize(&servers_root)
                    .map_err(|err| format!("Could not open Spoon server folder: {err}"))?;
                let target_dir = unique_server_dir(&servers_root, &name);
                if target_dir.starts_with(&source_dir) {
                    return Err(
                        "Spoon's server folder is inside the selected folder; choose a different folder."
                            .to_string(),
                    );
                }
                files::copy_directory(&source_dir, &target_dir)
                    .map_err(|err| format!("Could not copy server folder: {err}"))?;
                Ok(ServerConfig::new(
                    new_id(),
                    name,
                    target_dir.join(relative_jar),
                    target_dir,
                    4096,
                ))
            })();
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
    }

    fn take_finished(&mut self) -> Option<Result<ServerConfig, String>> {
        let rx = self.pending.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                Some(Err("Server folder copy stopped unexpectedly.".to_string()))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

pub struct ServerEntry {
    config: ServerConfig,
    handle: Option<ServerHandle>,
    console: VecDeque<LogLine>,
    command_input: String,
    command_history: Vec<String>,
    history_pos: Option<usize>,
    filter: String,
    case_sensitive: bool,
    last_error: Option<String>,
    restart_when_stopped: bool,
    started_at: Option<Instant>,
    cpu_usage: f32,
    memory_bytes: u64,
    online_players: BTreeSet<String>,
    max_players: u32,
    player_management: PlayerManagementState,
}

impl ServerEntry {
    fn new(config: ServerConfig) -> Self {
        let max_players = read_max_players(&config.working_dir);
        Self {
            config,
            handle: None,
            console: VecDeque::new(),
            command_input: String::new(),
            command_history: Vec::new(),
            history_pos: None,
            filter: String::new(),
            case_sensitive: false,
            last_error: None,
            restart_when_stopped: false,
            started_at: None,
            cpu_usage: 0.0,
            memory_bytes: 0,
            online_players: BTreeSet::new(),
            max_players,
            player_management: PlayerManagementState::new(),
        }
    }

    fn state(&self) -> ServerState {
        self.handle
            .as_ref()
            .map(ServerHandle::state)
            .unwrap_or(ServerState::Stopped)
    }

    fn push_logs(&mut self, mut lines: Vec<LogLine>) {
        self.console.extend(lines.drain(..));
        if self.console.len() > MAX_CONSOLE_LINES {
            let excess = self.console.len() - MAX_CONSOLE_LINES;
            self.console.drain(..excess);
        }
    }
}

/// App update check/download state machine, driven by `poll_update`.
enum UpdateState {
    Checking,
    Idle,
    Available(UpdateInfo),
    Downloading {
        rx: Receiver<UpdateDownloadEvent>,
        downloaded: u64,
        total: u64,
    },
    Ready {
        path: PathBuf,
        version: String,
    },
    Failed(String),
}

enum UpdateDownloadEvent {
    Progress(u64, u64),
    Done(Result<PathBuf, String>),
}

/// First-run managed-install flow (Windows release builds only): install the
/// newer of the launched build and latest GitHub release, add shortcuts, then
/// relaunch from the managed copy.
enum Bootstrap {
    None,
    /// A managed copy exists but this launch came from somewhere else
    /// (e.g. the original download); redirect to the managed executable.
    Relaunch,
    Setup(SetupState),
    /// The managed copy was launched; the window closes once it is up.
    Done,
}

enum SetupState {
    Preparing {
        rx: Receiver<UpdateDownloadEvent>,
        downloaded: u64,
        total: u64,
    },
    CreatingShortcuts {
        rx: Receiver<Result<(), String>>,
    },
    Failed(String),
}

/// Prepare the managed install from the newer of the launched build and the
/// latest GitHub release. The resulting executable path arrives on the channel.
fn start_managed_setup() -> SetupState {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let client = UpdateClient::new(env!("CARGO_PKG_VERSION"));
        let result: Result<PathBuf, String> = (|| {
            let info = client
                .latest()
                .map_err(|err| err.to_string())?
                .ok_or_else(|| "no published release found for this platform".to_string())?;
            if client.current_version_is_newer_than(&info.release.version) {
                return bootstrap::place_running_exe();
            }
            let dest = bootstrap::install_dir()
                .ok_or_else(|| "could not determine the Spoon folder".to_string())?;
            let downloaded = client
                .download(&info.asset, &dest, &mut |downloaded, total| {
                    let _ = tx.send(UpdateDownloadEvent::Progress(downloaded, total));
                })
                .map_err(|err| err.to_string())?;
            bootstrap::place_downloaded_exe(&downloaded)
        })();
        let _ = tx.send(UpdateDownloadEvent::Done(result));
    });
    SetupState::Preparing {
        rx,
        downloaded: 0,
        total: 0,
    }
}

/// Spawn the background creation of the desktop / start-menu shortcuts once
/// the managed executable has been prepared.
fn start_shortcuts(managed: PathBuf) -> SetupState {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(bootstrap::create_shortcuts(&managed));
    });
    SetupState::CreatingShortcuts { rx }
}

/// Launch the managed executable and move to `Bootstrap::Done` (or report the
/// failure as a setup error).
fn launch_managed_bootstrap() -> Bootstrap {
    let Some(exe) = bootstrap::managed_exe() else {
        return Bootstrap::Setup(SetupState::Failed(
            "could not determine the managed executable path".to_string(),
        ));
    };
    match bootstrap::launch(&exe) {
        Ok(_) => Bootstrap::Done,
        Err(err) => Bootstrap::Setup(SetupState::Failed(format!(
            "could not start {}: {err}",
            exe.display()
        ))),
    }
}

pub struct SpoonApp {
    splash: Option<Splash>,
    logo: Option<egui::TextureHandle>,
    logo_hover: Option<egui::TextureHandle>,
    store: ServerStore,
    servers: Vec<ServerEntry>,
    selected: Option<usize>,
    page: ServerPage,
    /// Sidebar search box filtering servers by name.
    sidebar_filter: String,
    create_page: Option<CreatePage>,
    import_dialog: Option<ImportDialog>,
    settings_dialog: Option<SettingsDialog>,
    files_dialog: Option<FilesDialog>,
    backups_dialog: Option<BackupsDialog>,
    confirm_remove: Option<usize>,
    /// Server index whose change-version modal is open.
    version_modal: Option<usize>,
    duplicate_task: Option<Receiver<Result<ServerConfig, String>>>,
    duplicate_source: Option<String>,
    update_state: UpdateState,
    update_check: Option<Receiver<Result<Option<UpdateInfo>, String>>>,
    update_dialog_open: bool,
    update_skipped: Option<String>,
    /// `UpdateInfo` kept while a download is running or failed, so the dialog
    /// can offer "Retry" without re-checking the API.
    update_info: Option<UpdateInfo>,
    bootstrap: Bootstrap,
    bootstrap_positioned: bool,
    /// Set once an update apply has been handed off (or attempted), so the
    /// window-close hook and the dialog button can't both fire.
    apply_started: bool,
    system: System,
    last_metrics_refresh: Instant,
    global_error: Option<String>,
    /// App-wide settings: which Java runtime launches servers (`None` = PATH).
    app_settings: AppSettings,
    java_store: Option<JavaStore>,
    backup_store: Option<BackupStore>,
    /// Installed private runtimes, cached so rendering never rescans the disk
    /// per frame; refreshed once a second and after every install/uninstall.
    installed_runtimes: Vec<JavaRuntime>,
    runtimes_refreshed: Instant,
    app_settings_open: bool,
    java_dialog: Option<JavaDialog>,
    /// Software icons by lowercase software name; `None` marks a fetch that failed
    /// (unknown software or network error) so we don't retry every frame.
    icon_textures: HashMap<String, Option<egui::TextureHandle>>,
    icon_pending: BTreeSet<String>,
    icon_tx: Sender<IconFetch>,
    icon_rx: Receiver<IconFetch>,
    player_skin_textures: HashMap<String, Option<egui::TextureHandle>>,
    player_skin_pending: BTreeSet<String>,
    player_skin_requests: BTreeSet<String>,
    player_skin_tx: Sender<PlayerSkinFetch>,
    player_skin_rx: Receiver<PlayerSkinFetch>,
    player_details: Option<PlayerDetailsDialog>,
    software: SoftwareCache,
}

impl SpoonApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let store = ServerStore::new().expect("could not determine config directory");
        let app_settings = AppSettings::load(store.config_dir());
        let servers: Vec<ServerEntry> = store.load().into_iter().map(ServerEntry::new).collect();
        // Open on the first server so the app never starts on the empty state by accident.
        let selected = (!servers.is_empty()).then_some(0);
        let logo = load_logo(&cc.egui_ctx, LOGO_PNG, "spoon_logo");
        let logo_hover = load_logo(&cc.egui_ctx, LOGO_HOVER_PNG, "spoon_logo_hover");
        // First-run managed install: either set up the managed copy (download
        // into the Spoon folder, shortcuts, relaunch) or redirect a stray
        // launch straight to the managed copy. Either way the bootstrap
        // screen replaces the splash and uses a small decorated window.
        let bootstrap = if bootstrap::needs_first_run_setup() {
            Bootstrap::Setup(start_managed_setup())
        } else if bootstrap::should_relaunch_from_managed() {
            Bootstrap::Relaunch
        } else {
            Bootstrap::None
        };
        let bootstrap_active = !matches!(bootstrap, Bootstrap::None);
        // Managed installs: sweep the pre-rename executable and the
        // moved-aside update copy; if one was removed the old shortcuts may
        // point at a name that no longer exists, so refresh them.
        if bootstrap::running_from_managed() {
            if let Ok(current) = std::env::current_exe() {
                std::thread::spawn(move || {
                    if bootstrap::cleanup_install_leftovers(&current) {
                        let _ = bootstrap::create_shortcuts(&current);
                    }
                });
            }
        }
        if bootstrap_active {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::Resizable(false));
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(520.0, 320.0)));
        }
        let (icon_tx, icon_rx) = mpsc::channel();
        let (player_skin_tx, player_skin_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = UpdateClient::new(env!("CARGO_PKG_VERSION"))
                .check()
                .map_err(|err| err.to_string());
            let _ = update_tx.send(result);
        });
        let java_store = JavaStore::new();
        let installed_runtimes = java_store
            .as_ref()
            .map(|store| store.list())
            .unwrap_or_default();
        Self {
            splash: if bootstrap_active {
                None
            } else {
                load_splash(&cc.egui_ctx)
            },
            logo,
            logo_hover,
            store,
            servers,
            selected,
            page: ServerPage::Overview,
            sidebar_filter: String::new(),
            create_page: None,
            import_dialog: None,
            settings_dialog: None,
            files_dialog: None,
            backups_dialog: None,
            confirm_remove: None,
            version_modal: None,
            duplicate_task: None,
            duplicate_source: None,
            update_state: UpdateState::Checking,
            update_check: Some(update_rx),
            update_dialog_open: false,
            update_skipped: None,
            update_info: None,
            bootstrap,
            bootstrap_positioned: false,
            apply_started: false,
            system: System::new(),
            last_metrics_refresh: Instant::now(),
            global_error: None,
            app_settings,
            java_store,
            backup_store: BackupStore::new(),
            installed_runtimes,
            runtimes_refreshed: Instant::now(),
            app_settings_open: false,
            java_dialog: None,
            icon_textures: HashMap::new(),
            icon_pending: BTreeSet::new(),
            icon_tx,
            icon_rx,
            player_skin_textures: HashMap::new(),
            player_skin_pending: BTreeSet::new(),
            player_skin_requests: BTreeSet::new(),
            player_skin_tx,
            player_skin_rx,
            player_details: None,
            software: SoftwareCache::new(),
        }
    }

    fn save(&self) {
        let configs: Vec<ServerConfig> = self.servers.iter().map(|s| s.config.clone()).collect();
        if let Err(err) = self.store.save(&configs) {
            eprintln!("failed to save server list: {err}");
        }
    }

    /// Selects a server and resets the per-server page state (dashboard
    /// pages, settings and files pages).
    fn select_server(&mut self, index: usize) {
        self.selected = Some(index);
        self.page = ServerPage::Overview;
        self.player_details = None;
        self.settings_dialog = None;
        self.files_dialog = None;
        self.backups_dialog = None;
        self.version_modal = None;
    }

    /// Opens the full-page server creation flow, suggesting the first free
    /// port (25565 and up) not taken by another Spoon server. A page with an
    /// in-flight download is left alone.
    fn open_create_page(&mut self) {
        if self
            .create_page
            .as_ref()
            .is_some_and(CreatePage::is_downloading)
        {
            return;
        }
        let assigned: Vec<u16> = self.servers.iter().map(|e| e.config.server_port).collect();
        self.create_page = Some(CreatePage::new(next_available_port(
            DEFAULT_SERVER_PORT,
            &assigned,
        )));
    }

    /// Switches the sub-navigation page, creating the page state on first
    /// visit and refusing to leave the files page with unsaved changes.
    fn select_page(&mut self, page: ServerPage, index: usize) {
        if self.page == ServerPage::Files
            && page != ServerPage::Files
            && self
                .files_dialog
                .as_ref()
                .is_some_and(FilesDialog::open_is_dirty)
        {
            if let Some(dialog) = &mut self.files_dialog {
                dialog.error = Some("Save or discard the open file's changes first.".to_string());
            }
            return;
        }
        self.page = page;
        self.player_details = None;
        match page {
            ServerPage::Files => {
                if let Some(dialog) = &mut self.files_dialog {
                    dialog.refresh();
                } else {
                    self.files_dialog = Some(FilesDialog::new(&self.servers[index].config));
                }
            }
            ServerPage::Settings if self.settings_dialog.is_none() => {
                self.settings_dialog = Some(SettingsDialog::new(&self.servers[index].config));
            }
            ServerPage::Backups if self.backups_dialog.is_none() => {
                if let Some(store) = &self.backup_store {
                    self.backups_dialog = Some(BackupsDialog::new(
                        &self.servers[index].config,
                        store.clone(),
                    ));
                }
            }
            ServerPage::Players => {
                let entry = &mut self.servers[index];
                entry
                    .player_management
                    .refresh(entry.config.working_dir.clone(), true);
                entry.player_management.last_refresh = Some(Instant::now());
                if let Some(handle) = entry.handle.as_mut() {
                    if handle.state() != ServerState::Stopped {
                        handle.send_command("list");
                    }
                }
            }
            _ => {}
        }
    }

    fn poll_servers(&mut self, ctx: &egui::Context) {
        let mut active = false;
        for (index, entry) in self.servers.iter_mut().enumerate() {
            let (lines, running) = match entry.handle.as_mut() {
                Some(handle) => {
                    let lines = handle.poll_logs();
                    handle.update();
                    (lines, handle.state() != ServerState::Stopped)
                }
                None => (Vec::new(), false),
            };
            if !lines.is_empty() {
                if let Some(dialog) = &mut self.player_details {
                    if dialog.server_index == index && dialog.live_position.is_none() {
                        dialog.live_position = lines.iter().rev().find_map(|line| {
                            parse_position_command_output(&line.text, &dialog.name)
                        });
                        if dialog.live_position.is_some() {
                            dialog.position_query_started = None;
                        }
                    }
                }
                for line in &lines {
                    update_online_players(&mut entry.online_players, &line.text);
                    if let Some(players) = parse_online_player_list(&line.text) {
                        entry.online_players = players.into_iter().collect();
                    }
                }
                entry.push_logs(lines);
                ctx.request_repaint();
            }
            if entry.handle.is_some() && !running {
                entry.handle = None;
                entry.started_at = None;
                entry.cpu_usage = 0.0;
                entry.memory_bytes = 0;
                entry.online_players.clear();
                if entry.restart_when_stopped {
                    entry.restart_when_stopped = false;
                    let java = resolve_java(
                        &self.java_store,
                        &self.installed_runtimes,
                        self.app_settings.java_runtime.as_deref(),
                        entry.config.java_runtime.as_deref(),
                    );
                    match ServerHandle::start(&entry.config, &java) {
                        Ok(handle) => {
                            entry.started_at = Some(Instant::now());
                            entry.handle = Some(handle);
                            entry.last_error = None;
                            active = true;
                        }
                        Err(err) => entry.last_error = Some(format!("Restart failed: {err}")),
                    }
                }
            }
            active |= running || entry.handle.is_some();
        }
        if active {
            // Keep repainting while a server is up so state changes are seen
            // even when the console is quiet.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn poll_player_management(&mut self, ctx: &egui::Context) {
        let mut pending = false;
        for entry in &mut self.servers {
            pending |= entry.player_management.poll_refresh();
            pending |= entry.player_management.poll(entry.handle.as_mut());
        }
        if pending {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn refresh_player_management_if_due(&mut self, ctx: &egui::Context) {
        if self.page != ServerPage::Players {
            return;
        }
        ctx.request_repaint_after(PLAYER_MANAGEMENT_REFRESH_INTERVAL);
        let Some(index) = self.selected else {
            return;
        };
        let Some(entry) = self.servers.get_mut(index) else {
            return;
        };
        let due = entry
            .player_management
            .last_refresh
            .is_none_or(|last| last.elapsed() >= PLAYER_MANAGEMENT_REFRESH_INTERVAL);
        if !due {
            return;
        }
        entry
            .player_management
            .refresh(entry.config.working_dir.clone(), false);
        entry.player_management.last_refresh = Some(Instant::now());
        if let Some(handle) = entry.handle.as_mut() {
            if handle.state() != ServerState::Stopped {
                handle.send_command("list");
            }
        }
    }

    fn poll_duplicate_task(&mut self) {
        let Some(rx) = &self.duplicate_task else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(config)) => {
                self.servers.push(ServerEntry::new(config));
                self.select_server(self.servers.len() - 1);
                self.global_error = None;
                self.duplicate_task = None;
                self.duplicate_source = None;
                self.save();
            }
            Ok(Err(err)) => {
                self.global_error = Some(format!("Could not duplicate server: {err}"));
                self.duplicate_task = None;
                self.duplicate_source = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.global_error =
                    Some("Could not duplicate server: background task stopped.".into());
                self.duplicate_task = None;
                self.duplicate_source = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Collect the background update check and any in-flight download events.
    fn poll_update(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.update_check {
            match rx.try_recv() {
                Ok(Ok(Some(info))) => {
                    self.update_state =
                        if self.update_skipped.as_deref() == Some(info.release.tag.as_str()) {
                            UpdateState::Idle
                        } else {
                            UpdateState::Available(info)
                        };
                    self.update_check = None;
                }
                Ok(Ok(None)) | Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    // No update, or the check failed silently in the background.
                    self.update_state = UpdateState::Idle;
                    self.update_check = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        let mut finished = None;
        if let UpdateState::Downloading {
            rx,
            downloaded,
            total,
        } = &mut self.update_state
        {
            for event in rx.try_iter() {
                match event {
                    UpdateDownloadEvent::Progress(done, size) => {
                        *downloaded = done;
                        *total = size;
                    }
                    UpdateDownloadEvent::Done(result) => finished = Some(result),
                }
            }
        }
        if let Some(result) = finished {
            match result {
                Ok(path) => {
                    let version = self
                        .update_info
                        .as_ref()
                        .map(|info| info.release.version.to_string())
                        .unwrap_or_default();
                    self.update_state = UpdateState::Ready { path, version };
                }
                Err(err) => self.update_state = UpdateState::Failed(err),
            }
        }

        if matches!(self.update_state, UpdateState::Checking)
            || matches!(self.update_state, UpdateState::Downloading { .. })
        {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// Spawn the download thread for the update shown in `UpdateState::Available`.
    fn start_update_download(&mut self) {
        let info = match &self.update_state {
            UpdateState::Available(info) => info.clone(),
            _ => return,
        };
        let (tx, rx) = mpsc::channel();
        let asset = info.asset.clone();
        std::thread::spawn(move || {
            let client = UpdateClient::new(env!("CARGO_PKG_VERSION"));
            let dest = updates_dir().unwrap_or_else(std::env::temp_dir);
            let result = client
                .download(&asset, &dest, &mut |downloaded, total| {
                    let _ = tx.send(UpdateDownloadEvent::Progress(downloaded, total));
                })
                .map_err(|err| err.to_string());
            let _ = tx.send(UpdateDownloadEvent::Done(result));
        });
        self.update_info = Some(info);
        self.update_state = UpdateState::Downloading {
            rx,
            downloaded: 0,
            total: 0,
        };
    }

    /// Drive the first-run setup state machine: prepare events, then the
    /// shortcut task, then hand off to the managed copy.
    fn poll_bootstrap(&mut self, ctx: &egui::Context) {
        enum Advance {
            Downloaded(PathBuf),
            DownloadFailed(String),
            ShortcutsDone,
            ShortcutsFailed(String),
        }
        let advance = match &mut self.bootstrap {
            Bootstrap::Setup(SetupState::Preparing { rx, .. }) => {
                let mut progress = None;
                let mut finished = None;
                for event in rx.try_iter() {
                    match event {
                        UpdateDownloadEvent::Progress(done, size) => progress = Some((done, size)),
                        UpdateDownloadEvent::Done(result) => finished = Some(result),
                    }
                }
                if let (
                    Some((done, size)),
                    Bootstrap::Setup(SetupState::Preparing {
                        downloaded, total, ..
                    }),
                ) = (progress, &mut self.bootstrap)
                {
                    *downloaded = done;
                    *total = size;
                }
                ctx.request_repaint_after(Duration::from_millis(200));
                finished.map(|result| match result {
                    Ok(path) => Advance::Downloaded(path),
                    Err(err) => Advance::DownloadFailed(err),
                })
            }
            Bootstrap::Setup(SetupState::CreatingShortcuts { rx }) => match rx.try_recv() {
                Ok(Ok(())) => Some(Advance::ShortcutsDone),
                Ok(Err(err)) => Some(Advance::ShortcutsFailed(err)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Advance::ShortcutsFailed(
                    "shortcut task stopped unexpectedly".into(),
                )),
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(200));
                    None
                }
            },
            _ => None,
        };
        match advance {
            Some(Advance::Downloaded(path)) => {
                self.bootstrap = Bootstrap::Setup(start_shortcuts(path));
            }
            Some(Advance::DownloadFailed(err)) | Some(Advance::ShortcutsFailed(err)) => {
                self.bootstrap = Bootstrap::Setup(SetupState::Failed(err));
            }
            Some(Advance::ShortcutsDone) => {
                self.bootstrap = launch_managed_bootstrap();
            }
            None => {}
        }
        // A finished relaunch (or a failure to start) is handled in render.
        if let Bootstrap::Relaunch = self.bootstrap {
            self.bootstrap = launch_managed_bootstrap();
        }
    }

    /// Render the first-run setup / relaunch screen shown instead of the app.
    fn render_bootstrap(&mut self, ctx: &egui::Context) {
        if !self.bootstrap_positioned {
            self.bootstrap_positioned = true;
            if let Some(cmd) = egui::ViewportCommand::center_on_screen(ctx) {
                ctx.send_viewport_cmd(cmd);
            }
        }
        let mut retry = false;
        let mut continue_portable = false;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(28)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    if let Some(logo) = &self.logo {
                        ui.add(egui::Image::new(logo).max_width(64.0));
                        ui.add_space(14.0);
                    }
                    match &mut self.bootstrap {
                        Bootstrap::Setup(SetupState::Preparing {
                            downloaded, total, ..
                        }) => {
                            theme::section_title_with_icon(
                                ui,
                                theme::icons::DOWNLOAD,
                                "Setting up Spoon",
                            );
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    egui::RichText::new("Preparing the managed Spoon install…")
                                        .color(theme::TEXT_MUTED),
                                );
                            });
                            if *total > 0 {
                                ui.add_space(12.0);
                                let fraction = (*downloaded as f32 / *total as f32).min(1.0);
                                ui.add(egui::ProgressBar::new(fraction).text(format!(
                                    "{} / {}",
                                    format_mb(*downloaded),
                                    format_mb(*total)
                                )));
                            }
                        }
                        Bootstrap::Setup(SetupState::CreatingShortcuts { .. }) => {
                            theme::section_title_with_icon(
                                ui,
                                theme::icons::CHECK,
                                "Setting up Spoon",
                            );
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(
                                    egui::RichText::new("Creating shortcuts…")
                                        .color(theme::TEXT_MUTED),
                                );
                            });
                        }
                        Bootstrap::Setup(SetupState::Failed(msg)) => {
                            theme::section_title_with_icon(
                                ui,
                                theme::icons::ROTATE,
                                "Couldn't finish setup",
                            );
                            ui.add_space(8.0);
                            egui::Frame::new()
                                .fill(theme::ERROR.gamma_multiply(0.10))
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    theme::ERROR.gamma_multiply(0.5),
                                ))
                                .corner_radius(egui::CornerRadius::same(6))
                                .inner_margin(egui::Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(msg.as_str())
                                            .color(theme::ERROR)
                                            .size(12.5),
                                    );
                                });
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(theme::primary_icon_button(theme::icons::ROTATE, "Retry"))
                                    .clicked()
                                {
                                    retry = true;
                                }
                                if ui
                                    .add(theme::ghost_button("Continue without installing"))
                                    .clicked()
                                {
                                    continue_portable = true;
                                }
                            });
                        }
                        Bootstrap::Relaunch | Bootstrap::Done => {
                            theme::section_title_with_icon(
                                ui,
                                theme::icons::ROTATE,
                                "Launching Spoon…",
                            );
                        }
                        Bootstrap::None => {}
                    }
                });
            });
        if retry {
            self.bootstrap = Bootstrap::Setup(start_managed_setup());
        }
        if continue_portable {
            self.bootstrap = Bootstrap::None;
            // Restore the normal app window that the bootstrap screen replaced.
            ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                820.0, 520.0,
            )));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ctx.request_repaint();
        }
        if matches!(self.bootstrap, Bootstrap::Done) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn refresh_metrics(&mut self, ctx: &egui::Context) {
        if self.last_metrics_refresh.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_metrics_refresh = Instant::now();
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        for entry in &mut self.servers {
            if let Some(pid) = entry.handle.as_ref().and_then(ServerHandle::pid) {
                if let Some(process) = self.system.process(Pid::from_u32(pid)) {
                    entry.cpu_usage = process.cpu_usage();
                    entry.memory_bytes = process.memory();
                }
            } else {
                entry.cpu_usage = 0.0;
                entry.memory_bytes = 0;
            }
        }
        ctx.request_repaint();
    }

    /// Collect icon downloads that finished since last frame, a few per frame.
    fn poll_icons(&mut self, ctx: &egui::Context) {
        let mut received = false;
        for (software, result) in self.icon_rx.try_iter().take(MAX_ICONS_PER_FRAME) {
            received = true;
            self.icon_pending.remove(&software);
            let texture = result
                .ok()
                .and_then(|bytes| load_icon_texture(ctx, &bytes, &software));
            self.icon_textures.insert(software, texture);
        }
        if received {
            ctx.request_repaint();
        }
    }

    /// Applies catalog refreshes and, when a new one arrives, retries failed
    /// icons and downloads every software icon ahead of the pickers.
    fn poll_software(&mut self, ctx: &egui::Context) {
        if !self.software.poll(ctx) {
            return;
        }
        self.icon_textures.retain(|_, texture| texture.is_some());
        let Some(snapshot) = &self.software.snapshot else {
            return;
        };
        for (server_type, _) in snapshot.types.all() {
            ensure_software_icon(
                &self.icon_textures,
                &mut self.icon_pending,
                &self.icon_tx,
                &server_type.api_name().to_lowercase(),
            );
        }
    }

    /// Kick off a background download for every server whose software icon we
    /// don't have yet. Cached and in-flight names are skipped.
    fn ensure_icons(&mut self) {
        for entry in &self.servers {
            let Some(software) = entry.config.server_type.as_deref() else {
                continue;
            };
            let key = software.to_lowercase();
            if self.icon_textures.contains_key(&key) || self.icon_pending.contains(&key) {
                continue;
            }
            self.icon_pending.insert(key.clone());
            let tx = self.icon_tx.clone();
            std::thread::spawn(move || {
                let result = fetch_software_icon(&key);
                let _ = tx.send((key, result));
            });
        }
    }
}

impl eframe::App for SpoonApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.splash.is_some() {
            egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
        } else {
            theme::BACKGROUND.to_normalized_gamma_f32()
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !matches!(self.bootstrap, Bootstrap::None) {
            self.poll_bootstrap(ctx);
            self.render_bootstrap(ctx);
            return;
        }
        if self.splash.is_some() {
            self.poll_icons(ctx);
            self.poll_software(ctx);
            self.ensure_icons();
            self.render_splash(ctx);
            return;
        }
        // Closing the window with a downloaded update applies it on the way
        // out; failure keeps the app open so the update isn't lost.
        if !self.apply_started && ctx.input(|i| i.viewport().close_requested()) {
            if let Some(path) = self.pending_apply_path() {
                match swap_in_update(&path) {
                    Ok(()) => self.apply_started = true,
                    Err(err) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                        let msg = format!("Could not apply the update: {err}");
                        self.global_error = Some(msg.clone());
                        self.update_state = UpdateState::Failed(msg);
                    }
                }
            }
        }
        self.poll_servers(ctx);
        self.refresh_player_management_if_due(ctx);
        self.poll_player_management(ctx);
        self.poll_player_details(ctx);
        self.poll_player_skins(ctx);
        self.ensure_player_skins();
        self.poll_duplicate_task();
        self.poll_update(ctx);
        if self.duplicate_task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        self.refresh_metrics(ctx);
        self.refresh_runtimes_if_stale();
        self.poll_icons(ctx);
        self.poll_software(ctx);
        self.ensure_icons();

        self.render_sidebar(ctx);
        self.render_main_panel(ctx);
        self.render_import_dialog(ctx);
        self.render_remove_confirm(ctx);
        self.render_update_dialog(ctx);
        self.render_version_dialog(ctx);
        self.render_app_settings_dialog(ctx);
        self.render_player_details(ctx);
    }
}

impl SpoonApp {
    fn render_splash(&mut self, ctx: &egui::Context) {
        let preload_complete = !self.software.is_loading() && self.icon_pending.is_empty();
        let Some(splash) = &mut self.splash else {
            return;
        };
        if !splash.positioned {
            splash.positioned = true;
            if let Some(cmd) = egui::ViewportCommand::center_on_screen(ctx) {
                ctx.send_viewport_cmd(cmd);
            }
        }
        let skip = ctx.input(|i| {
            i.pointer.any_click()
                || i.key_pressed(egui::Key::Enter)
                || i.key_pressed(egui::Key::Space)
        });
        if preload_complete && (splash.started.elapsed() >= SPLASH_DURATION || skip) {
            self.splash = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                820.0, 520.0,
            )));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ctx.request_repaint();
            return;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                ui.add(egui::Image::new(&splash.texture).fit_to_exact_size(ui.available_size()));
            });
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn render_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("server_list")
            .resizable(true)
            .default_width(230.0)
            .width_range(180.0..=360.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE.gamma_multiply(0.55))
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    if let Some(logo) = &self.logo {
                        let height = 80.0;
                        let size = logo.size_vec2() * (height / logo.size_vec2().y);
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        let hovered = ui.rect_contains_pointer(rect);
                        let texture = match (&self.logo_hover, hovered) {
                            (Some(hover), true) => hover,
                            _ => logo,
                        };
                        egui::Image::new(texture).paint_at(ui, rect);
                    }
                    ui.label(
                        egui::RichText::new("SPOON")
                            .font(egui::FontId::new(22.0, theme::title_family()))
                            .color(theme::TEXT),
                    );
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let button_width =
                        ((ui.available_width() - ui.spacing().item_spacing.x) * 0.5).max(0.0);
                    if social_link_button(
                        ui,
                        button_width,
                        "Discord",
                        "Join the Spoon community on Discord",
                        DISCORD_BUTTON_COLOR,
                        SocialButtonIcon::Discord,
                    ) {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab(DISCORD_INVITE_URL));
                    }
                    if social_link_button(
                        ui,
                        button_width,
                        "Ko-fi",
                        "Support Spoon on Ko-fi",
                        KOFI_BUTTON_COLOR,
                        SocialButtonIcon::KoFi,
                    ) {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(KOFI_URL));
                    }
                });
                ui.add_space(10.0);
                // Header: "Servers" + count, with start/stop-all shortcuts.
                let server_count = self.servers.len();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        egui::RichText::new("Servers")
                            .size(13.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    egui::Frame::new()
                        .fill(theme::SURFACE_HOVER)
                        .corner_radius(egui::CornerRadius::same(255))
                        .inner_margin(egui::Margin::symmetric(8, 2))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(server_count.to_string())
                                    .size(11.5)
                                    .color(theme::TEXT_MUTED),
                            );
                        });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let can_stop_any = self
                            .servers
                            .iter()
                            .any(|e| e.state() != ServerState::Stopped);
                        let can_start_any = self.servers.iter().enumerate().any(|(i, e)| {
                            e.state() == ServerState::Stopped
                                && self.duplicate_source.as_deref()
                                    != Some(self.servers[i].config.id.as_str())
                        });
                        let stop = egui::Button::new(
                            theme::icon(theme::icons::STOP, 13.0).color(theme::TEXT_MUTED),
                        )
                        .min_size(egui::vec2(28.0, 24.0));
                        if ui
                            .add_enabled(can_stop_any, stop)
                            .on_hover_text("Stop all servers")
                            .clicked()
                        {
                            self.stop_all_servers();
                        }
                        let start = egui::Button::new(
                            theme::icon(theme::icons::PLAY, 13.0).color(theme::PRIMARY),
                        )
                        .min_size(egui::vec2(28.0, 24.0));
                        if ui
                            .add_enabled(can_start_any, start)
                            .on_hover_text("Start all stopped servers")
                            .clicked()
                        {
                            self.start_all_servers();
                        }
                    });
                });
                ui.add_space(6.0);
                // Search filter.
                ui.horizontal(|ui| {
                    ui.label(theme::icon(theme::icons::SEARCH, 14.0).color(theme::TEXT_MUTED));
                    // The clear button is laid out first (right-aligned) so the
                    // input only fills what is left and never overflows the panel.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !self.sidebar_filter.is_empty()
                            && ui
                                .add(egui::Button::new(theme::icon(theme::icons::XMARK, 12.0)))
                                .on_hover_text("Clear search")
                                .clicked()
                        {
                            self.sidebar_filter.clear();
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.sidebar_filter)
                                .id(egui::Id::new("sidebar_server_filter"))
                                .desired_width(f32::INFINITY)
                                .hint_text("Search servers…"),
                        );
                    });
                });
                ui.add_space(4.0);
                if let Some(err) = &self.global_error {
                    egui::Frame::new()
                        .fill(theme::ERROR.gamma_multiply(0.10))
                        .stroke(egui::Stroke::new(1.0_f32, theme::ERROR.gamma_multiply(0.5)))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(err).color(theme::ERROR).size(12.5));
                        });
                    ui.add_space(4.0);
                }
                if self.duplicate_task.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::icon_text_job(
                            theme::icons::CLONE,
                            "Duplicating server…",
                            theme::TEXT_MUTED,
                        ));
                    });
                    ui.add_space(4.0);
                }

                let time = ui.ctx().input(|i| i.time);
                // Keep room for the pinned footer buttons below the list.
                let update_row = match &self.update_state {
                    UpdateState::Available(_) => true,
                    UpdateState::Ready { .. } => updates_apply_in_place(),
                    _ => false,
                };
                let list_height =
                    (ui.available_height() - sidebar_footer_height(update_row)).max(72.0);
                let mut select = None;
                let mut rename = None;
                let mut duplicate = None;
                let mut remove = None;
                let mut open_folder = None;
                let filter = self.sidebar_filter.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .id_salt("server_list_scroll")
                    .auto_shrink([false, false])
                    .max_height(list_height)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 10.0;
                        let mut matched = 0usize;
                        for i in 0..self.servers.len() {
                            if !filter.is_empty()
                                && !self.servers[i]
                                    .config
                                    .name
                                    .to_lowercase()
                                    .contains(filter.as_str())
                            {
                                continue;
                            }
                            matched += 1;
                            let (response, row_action) = self.server_row(ui, i, time);
                            let mut action = row_action;
                            response.context_menu(|ui| {
                                if let Some(a) = self.server_actions_menu(ui, i) {
                                    action = Some(a);
                                }
                            });
                            if response.clicked() {
                                select = Some(i);
                            }
                            match action {
                                Some(ServerAction::Settings) => rename = Some(i),
                                Some(ServerAction::Duplicate) => duplicate = Some(i),
                                Some(ServerAction::Remove) => remove = Some(i),
                                Some(ServerAction::OpenFolder) => open_folder = Some(i),
                                None => {}
                            }
                        }
                        if matched == 0 && !self.servers.is_empty() {
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new("No servers match your search.")
                                    .color(theme::TEXT_MUTED),
                            );
                        }
                    });

                if let Some(i) = select {
                    // Switching to a server dismisses the create page; an
                    // in-flight download must not be discarded.
                    if !self
                        .create_page
                        .as_ref()
                        .is_some_and(CreatePage::is_downloading)
                    {
                        self.create_page = None;
                        self.select_server(i);
                    }
                }
                if let Some(i) = rename {
                    self.select_server(i);
                    self.page = ServerPage::Settings;
                    self.settings_dialog = Some(SettingsDialog::new(&self.servers[i].config));
                }
                if let Some(i) = duplicate {
                    self.start_duplicate(i);
                }
                if let Some(i) = remove {
                    self.confirm_remove = Some(i);
                }
                if let Some(i) = open_folder {
                    open_directory(&self.servers[i].config.working_dir);
                }

                let mut restart_update = None;
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.add_sized(
                        [ui.available_width(), 18.0],
                        egui::Label::new(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(11.5)
                                .color(theme::TEXT_MUTED),
                        )
                        .halign(egui::Align::Center),
                    );
                    if ui
                        .add_sized(
                            [ui.available_width(), 36.0],
                            theme::ghost_icon_button(theme::icons::GEAR, "App Settings"),
                        )
                        .on_hover_text("App-wide settings, including the Java runtime")
                        .clicked()
                    {
                        self.app_settings_open = true;
                    }
                    if ui
                        .add_sized(
                            [ui.available_width(), 36.0],
                            theme::ghost_icon_button(
                                theme::icons::FOLDER_OPEN,
                                "Import Existing Folder",
                            ),
                        )
                        .clicked()
                    {
                        self.import_server_folder();
                    }
                    if ui
                        .add_sized(
                            [ui.available_width(), 36.0],
                            theme::primary_icon_button(theme::icons::PLUS, "New Server"),
                        )
                        .clicked()
                    {
                        self.open_create_page();
                    }
                    // The update row: an available update opens the dialog; a
                    // downloaded one restarts straight into it and stays here
                    // ("Restart to update") even after the dialog is closed.
                    match &self.update_state {
                        UpdateState::Available(info) => {
                            let version = info.release.version.to_string();
                            if ui
                                .add_sized(
                                    [ui.available_width(), 36.0],
                                    theme::warn_icon_button(
                                        theme::icons::ROTATE,
                                        "Update available",
                                    ),
                                )
                                .on_hover_text(format!("See what's new and update to v{version}"))
                                .clicked()
                            {
                                self.update_dialog_open = true;
                            }
                        }
                        UpdateState::Ready { path, version }
                            if updates_apply_in_place()
                                && ui
                                    .add_sized(
                                        [ui.available_width(), 36.0],
                                        theme::warn_icon_button(
                                            theme::icons::RESTART,
                                            "Restart to update",
                                        ),
                                    )
                                    .on_hover_text(format!(
                                        "Restart Spoon to apply update v{version}"
                                    ))
                                    .clicked() =>
                        {
                            restart_update = Some(path.clone());
                        }
                        _ => {}
                    }
                });
                if let Some(path) = restart_update {
                    self.apply_update_and_close(ctx, &path);
                }
            });
    }

    /// Centered landing view shown when no server is selected.
    fn render_empty_state(&mut self, ui: &mut egui::Ui) {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                if let Some(logo) = &self.logo {
                    ui.add(egui::Image::new(logo).max_width(96.0));
                    ui.add_space(14.0);
                }
                ui.label(
                    egui::RichText::new("No server selected")
                        .size(20.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Add a server to get started, or import an existing folder.",
                    )
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(theme::primary_icon_button(theme::icons::PLUS, "Add Server"))
                        .clicked()
                    {
                        self.open_create_page();
                    }
                    if ui
                        .add(theme::ghost_icon_button(
                            theme::icons::FOLDER_OPEN,
                            "Import Existing Folder",
                        ))
                        .clicked()
                    {
                        self.import_server_folder();
                    }
                });
            });
        });
    }

    fn render_main_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                if self.create_page.is_some() {
                    self.render_create_page(ui);
                    return;
                }
                let Some(index) = self.selected else {
                    self.render_empty_state(ui);
                    return;
                };
                if index >= self.servers.len() {
                    self.selected = None;
                    return;
                }

                let java_override = self.servers[index].config.java_runtime.clone();
                let java = self.java_for(java_override.as_deref());
                let entry = &mut self.servers[index];

                let can_edit_settings =
                    self.duplicate_source.as_deref() != Some(entry.config.id.as_str());
                let state = entry.state();
                let time = ui.ctx().input(|i| i.time);

                // Identity + actions band: status dot, name and state on the
                // left; start/stop/restart/kill actions plus Open Folder on the
                // right. Uptime and players live in the Overview stat cards.
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        let (dot_rect, _) =
                            ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                        let color = state_color(state);
                        let dot_center = dot_rect.center();
                        if state == ServerState::Running {
                            let pulse = (time as f32 * 1.6).sin() * 0.5 + 0.5;
                            ui.painter().circle_filled(
                                dot_center,
                                6.0 + pulse * 3.5,
                                color.gamma_multiply(0.08 + 0.14 * (1.0 - pulse)),
                            );
                        }
                        ui.painter().circle_filled(dot_center, 5.0, color);
                        let name_font = egui::FontId::proportional(24.0);
                        let name_width = (ui.available_width() * 0.45).clamp(120.0, 340.0);
                        let name = truncate_to_width(
                            ui,
                            &entry.config.name,
                            name_font.clone(),
                            name_width,
                        );
                        ui.label(
                            egui::RichText::new(name)
                                .strong()
                                .size(24.0)
                                .color(theme::TEXT),
                        )
                        .on_hover_text(&entry.config.name);
                        ui.label(egui::RichText::new(state.label()).size(15.0).color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            match state {
                                ServerState::Stopped => {
                                    if ui
                                        .add_enabled(
                                            can_edit_settings,
                                            theme::primary_icon_button(theme::icons::PLAY, "Start"),
                                        )
                                        .clicked()
                                    {
                                        match ServerHandle::start(&entry.config, &java) {
                                            Ok(handle) => {
                                                entry.last_error = None;
                                                entry.started_at = Some(Instant::now());
                                                entry.online_players.clear();
                                                entry.handle = Some(handle);
                                            }
                                            Err(err) => entry.last_error = Some(err.to_string()),
                                        }
                                    }
                                }
                                ServerState::Running | ServerState::Starting => {
                                    if ui
                                        .add(theme::secondary_icon_button(
                                            theme::icons::RESTART,
                                            "Restart",
                                        ))
                                        .clicked()
                                    {
                                        entry.restart_when_stopped = true;
                                        if let Some(handle) = &mut entry.handle {
                                            handle.request_stop();
                                        }
                                    }
                                    if ui
                                        .add(theme::secondary_icon_button(
                                            theme::icons::STOP,
                                            "Stop",
                                        ))
                                        .clicked()
                                    {
                                        if let Some(handle) = &mut entry.handle {
                                            handle.request_stop();
                                        }
                                    }
                                    if ui
                                        .add(theme::danger_icon_button(theme::icons::KILL, "Kill"))
                                        .clicked()
                                    {
                                        if let Some(handle) = &mut entry.handle {
                                            handle.kill();
                                        }
                                    }
                                }
                                ServerState::Stopping => {
                                    ui.add_enabled(false, theme::secondary_button("Stopping…"));
                                    if ui
                                        .add(theme::danger_icon_button(theme::icons::KILL, "Kill"))
                                        .clicked()
                                    {
                                        if let Some(handle) = &mut entry.handle {
                                            handle.kill();
                                        }
                                    }
                                }
                            }
                            if ui
                                .add(theme::ghost_icon_button(
                                    theme::icons::FOLDER_OPEN,
                                    "Open Folder",
                                ))
                                .on_hover_text("Reveal the server folder in the file manager")
                                .clicked()
                            {
                                open_directory(&entry.config.working_dir);
                            }
                        });
                    });
                });
                ui.add_space(8.0);

                // Dashboard pages: a slim icon rail on the left and the page
                // content filling the remaining width.
                let content_height = ui.available_height();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    self.render_page_rail(ui, index, can_edit_settings, content_height);
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        match self.page {
                            ServerPage::Overview => self.render_overview_page(ui, index),
                            ServerPage::Players => self.render_players_page(ui, index),
                            ServerPage::Files => self.render_files_page(ui, index),
                            ServerPage::Backups => self.render_backups_page(ui, index),
                            ServerPage::Settings => self.render_settings_page(ui, index),
                        }
                    });
                });
            });
    }

    /// The slim left-hand navigation rail for the dashboard pages: one
    /// icon-only button per page, with the page name in the hover tooltip.
    /// The buttons form a compact card, vertically centered next to the page
    /// content, that spreads out as pages are added.
    fn render_page_rail(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        can_edit_settings: bool,
        height: f32,
    ) {
        // Reserve a slim, full-height strip so the page content next to the
        // rail keeps its height, then paint a content-sized card centered
        // inside the strip and lay the buttons out within it. (Allocating
        // first and building a child ui mirrors how egui implements
        // SidePanel; a frame shown inside a horizontal row would otherwise
        // size to its content.)
        let (strip, _) =
            ui.allocate_exact_size(egui::vec2(RAIL_STRIP_WIDTH, height), egui::Sense::hover());
        let card_height = (RAIL_PAGES.len() as f32 * 36.0
            + (RAIL_PAGES.len() as f32 - 1.0) * 6.0
            + 2.0 * (10.0 + 1.0))
            .min(height);
        let card_rect =
            egui::Rect::from_center_size(strip.center(), egui::vec2(RAIL_STRIP_WIDTH, card_height));
        let frame = egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(8, 10));
        let content_rect = card_rect - frame.total_margin();
        ui.painter().add(frame.paint(content_rect));
        let mut rail_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(content_rect)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        rail_ui.spacing_mut().item_spacing.y = 6.0;
        for &(page, glyph, label) in &RAIL_PAGES {
            let active = self.page == page;
            let response = if page == ServerPage::Settings && !can_edit_settings {
                rail_ui
                    .add_enabled(false, theme::nav_icon_tab(active, glyph))
                    .on_hover_text("Finish the version change first")
            } else {
                rail_ui
                    .add(theme::nav_icon_tab(active, glyph))
                    .on_hover_text(label)
            };
            if response.clicked() {
                self.select_page(page, index);
            }
        }
    }

    /// Overview page: the server address, the EULA banner, live stat cards,
    /// the console and the software box.
    fn render_overview_page(&mut self, ui: &mut egui::Ui, index: usize) {
        if let Some(err) = &self.servers[index].last_error {
            ui.label(egui::RichText::new(err).color(theme::ERROR));
            ui.add_space(4.0);
        }
        self.render_address_card(ui, index);
        self.render_eula_banner(ui, index);
        self.render_stat_cards(ui, index);
        self.render_console(ui, index);
        ui.add_space(8.0);
        self.render_software_card(ui, index);
    }

    /// Live CPU, RAM, uptime and player meters.
    fn render_stat_cards(&self, ui: &mut egui::Ui, index: usize) {
        let entry = &self.servers[index];
        let uptime = entry
            .started_at
            .map(|started| format_duration(started.elapsed()))
            .unwrap_or_else(|| "Offline".to_string());
        let players = entry
            .online_players
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        // Consumption levels for each meter (0..1).
        let cpu_fill = (entry.cpu_usage / 100.0).clamp(0.0, 1.0);
        let ram_fill = if entry.config.max_memory_mb > 0 {
            ((entry.memory_bytes as f64)
                / (f64::from(entry.config.max_memory_mb) * 1024.0 * 1024.0))
                .clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let players_fill =
            (entry.online_players.len() as f32 / entry.max_players.max(1) as f32).clamp(0.0, 1.0);
        let cpu_value = format!("{:.1}%", entry.cpu_usage);
        let ram_value = format!(
            "{} / {}",
            format_mb(entry.memory_bytes),
            format_mb(u64::from(entry.config.max_memory_mb) * 1024 * 1024)
        );
        let players_value = format!("{} / {}", entry.online_players.len(), entry.max_players);
        let players_label = format!("Players ({})", entry.online_players.len());
        let players_hover = if players.is_empty() {
            "No online players detected from server output."
        } else {
            players.as_str()
        };
        // Same 8px gap between boxes as between the stacked sections.
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.columns(4, |columns| {
                theme::stat_card(
                    &mut columns[0],
                    theme::icons::CPU,
                    "CPU",
                    &cpu_value,
                    Some(cpu_fill),
                );
                theme::stat_card(
                    &mut columns[1],
                    theme::icons::MEMORY,
                    "RAM",
                    &ram_value,
                    Some(ram_fill),
                );
                // Uptime has no consumption measure, so the meter stays an empty track.
                theme::stat_card(
                    &mut columns[2],
                    theme::icons::CLOCK,
                    "Uptime",
                    &uptime,
                    None,
                );
                theme::stat_card(
                    &mut columns[3],
                    theme::icons::USERS,
                    &players_label,
                    &players_value,
                    Some(players_fill),
                )
                .response
                .on_hover_text(players_hover);
            });
        });
        ui.add_space(8.0);
    }

    /// Software box: icon, name, current version and a change-version shortcut.
    fn render_software_card(&mut self, ui: &mut egui::Ui, index: usize) {
        let software_icon =
            self.servers[index]
                .config
                .server_type
                .as_deref()
                .and_then(|software| {
                    self.icon_textures
                        .get(&software.to_lowercase())
                        .and_then(|texture| texture.as_ref())
                        .cloned()
                });
        let mut open_version_modal = false;
        let can_edit_version =
            self.duplicate_source.as_deref() != Some(self.servers[index].config.id.as_str());
        let entry = &self.servers[index];
        let software_title = software_name(&entry.config);
        let version = entry
            .config
            .server_version
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let icon_size = 40.0;
                let (icon_rect, _) =
                    ui.allocate_exact_size(egui::vec2(icon_size, icon_size), egui::Sense::hover());
                match &software_icon {
                    Some(texture) => egui::Image::new(texture)
                        .corner_radius(egui::CornerRadius::same(8))
                        .paint_at(ui, icon_rect),
                    None => {
                        ui.painter()
                            .rect_filled(icon_rect, 8.0, theme::SURFACE_HOVER);
                        ui.painter().text(
                            icon_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            theme::icons::SERVER,
                            egui::FontId::new(18.0, theme::icon_family()),
                            theme::TEXT_MUTED,
                        );
                    }
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(&software_title)
                            .size(16.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(format!("Version {version}"))
                            .size(12.5)
                            .color(theme::TEXT_MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(can_edit_version, icon_toolbar_button(theme::icons::PEN))
                        .on_hover_text(if can_edit_version {
                            "Change software or version"
                        } else {
                            "Finish the version change first"
                        })
                        .clicked()
                    {
                        open_version_modal = true;
                    }
                });
            });
        });
        if open_version_modal {
            self.settings_dialog = Some(SettingsDialog::new(&self.servers[index].config));
            self.version_modal = Some(index);
        }
    }

    /// Server address viewer, refreshed every few seconds rather than every frame.
    fn render_address_card(&self, ui: &mut egui::Ui, index: usize) {
        let entry = &self.servers[index];
        let cache_id = egui::Id::new(("server_addr", index));
        let now = std::time::Instant::now();
        let cached: Option<(std::time::Instant, String, u16)> = ui.data(|d| d.get_temp(cache_id));
        let (_, local_ip, port) = match cached {
            Some(c) if now.duration_since(c.0).as_secs() < 5 => c,
            _ => {
                let fresh = (
                    now,
                    local_ip_address(),
                    read_server_port(&entry.config.working_dir),
                );
                ui.data_mut(|d| d.insert_temp(cache_id, fresh.clone()));
                fresh
            }
        };
        let local_addr = format!("{local_ip}:{port}");
        let local_only = format!("localhost:{port}");
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_space(2.0);
                ui.label(theme::icon(theme::icons::SERVER, 14.0).color(theme::TEXT_MUTED));
                ui.label(
                    egui::RichText::new("ADDRESS")
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
                ui.label(
                    egui::RichText::new(&local_addr)
                        .size(16.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(format!("or {local_only} on this PC"))
                        .size(12.0)
                        .color(theme::TEXT_MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(icon_toolbar_button(theme::icons::COPY))
                        .on_hover_text("Copy address for players on your network")
                        .clicked()
                    {
                        ui.ctx().copy_text(local_addr.clone());
                    }
                });
            });
        });
        ui.add_space(8.0);
    }

    /// EULA acceptance banner, shown only while the EULA is unaccepted.
    fn render_eula_banner(&mut self, ui: &mut egui::Ui, index: usize) {
        let entry = &mut self.servers[index];
        match eula_state(&entry.config.working_dir) {
            Ok(EulaState::Accepted) | Err(_) => {}
            Ok(_) => {
                egui::Frame::new()
                        .fill(theme::SURFACE)
                        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (bar, _) = ui.allocate_exact_size(
                                    egui::vec2(3.0, 34.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    bar,
                                    egui::CornerRadius::same(2),
                                    theme::WARN,
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "This server needs the Minecraft EULA accepted before it can run.",
                                    )
                                    .color(theme::TEXT),
                                );
                                ui.hyperlink_to("Read the EULA", spoon_core::eula::EULA_URL);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(theme::primary_icon_button(
                                                theme::icons::CHECK,
                                                "Accept EULA",
                                            ))
                                            .clicked()
                                        {
                                            match accept_eula(&entry.config.working_dir) {
                                                Ok(()) => entry.last_error = None,
                                                Err(err) => entry.last_error =
                                                    Some(format!("Could not write eula.txt: {err}")),
                                            }
                                        }
                                    },
                                );
                            });
                        });
                ui.add_space(10.0);
            }
        }
    }

    /// Console section of the overview: filter toolbar, live output and the command input.
    fn render_console(&mut self, ui: &mut egui::Ui, index: usize) {
        let entry = &mut self.servers[index];

        // Console toolbar: filter, copy, clear.
        let mut copy = false;
        let mut clear = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::icon(theme::icons::SEARCH, 15.0).color(theme::TEXT_MUTED));
            let search_width = (ui.available_width() * 0.35).clamp(160.0, 320.0);
            ui.add(
                egui::TextEdit::singleline(&mut entry.filter)
                    .desired_width(search_width)
                    .hint_text("Search console…"),
            );
            if !entry.filter.is_empty() && ui.button("✕").on_hover_text("Clear filter").clicked()
            {
                entry.filter.clear();
            }
            let match_case = egui::Button::new(egui::RichText::new("Aa").monospace().size(13.0))
                .selected(entry.case_sensitive);
            if ui.add(match_case).on_hover_text("Match case").clicked() {
                entry.case_sensitive = !entry.case_sensitive;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(theme::ghost_icon_button(theme::icons::BROOM, "Clear"))
                    .clicked()
                {
                    clear = true;
                }
                if ui
                    .add(theme::ghost_icon_button(theme::icons::COPY, "Copy"))
                    .on_hover_text("Copy shown output")
                    .clicked()
                {
                    copy = true;
                }
            });
        });
        ui.add_space(6.0);

        let needle = if entry.case_sensitive {
            entry.filter.clone()
        } else {
            entry.filter.to_lowercase()
        };
        let case_sensitive = entry.case_sensitive;
        let visible: Vec<&LogLine> = entry
            .console
            .iter()
            .filter(|line| {
                needle.is_empty()
                    || if case_sensitive {
                        strip_ansi(&line.text).contains(&needle)
                    } else {
                        strip_ansi(&line.text).to_lowercase().contains(&needle)
                    }
            })
            .collect();

        if copy {
            let text = visible
                .iter()
                .map(|line| format_line(line))
                .collect::<Vec<_>>()
                .join("\n");
            ui.ctx().copy_text(text);
        }

        // Console output: fill everything left above the command bar and the software box.
        let console_height =
            (ui.available_height() - COMMAND_BAR_HEIGHT - SOFTWARE_CARD_HEIGHT).max(60.0);
        egui::Frame::new()
            .fill(theme::CONSOLE_BG)
            .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_height(console_height);
                ui.spacing_mut().item_spacing.y = 2.0;
                let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show_rows(ui, row_height, visible.len(), |ui, range| {
                        for line in &visible[range] {
                            ui.add(egui::Label::new(console_job(line)).extend());
                        }
                    });
            });
        if clear {
            entry.console.clear();
        }

        ui.add_space(8.0);

        // Command input with Up/Down history.
        let mut send = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            // Button is laid out first (right-aligned) so the input only fills the remainder.
            let mut send_clicked = false;
            let response = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    send_clicked = ui
                        .add(theme::primary_icon_button(
                            theme::icons::PAPER_PLANE,
                            "Send",
                        ))
                        .clicked();
                    ui.add(
                        egui::TextEdit::singleline(&mut entry.command_input)
                            .desired_width(f32::INFINITY)
                            .hint_text("Type a server command and press Enter…"),
                    )
                })
                .inner;
            if response.has_focus() && !entry.command_history.is_empty() {
                let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
                let len = entry.command_history.len();
                if up {
                    let pos = match entry.history_pos {
                        None => len - 1,
                        Some(p) => p.saturating_sub(1),
                    };
                    entry.history_pos = Some(pos);
                    entry.command_input = entry.command_history[pos].clone();
                } else if down {
                    match entry.history_pos {
                        Some(p) if p + 1 < len => {
                            entry.history_pos = Some(p + 1);
                            entry.command_input = entry.command_history[p + 1].clone();
                        }
                        Some(_) => {
                            entry.history_pos = None;
                            entry.command_input.clear();
                        }
                        None => {}
                    }
                }
            }
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                send = true;
                response.request_focus();
            }
            if send_clicked {
                send = true;
            }
        });
        if send && !entry.command_input.trim().is_empty() {
            let command = entry.command_input.trim().to_string();
            if let Some(handle) = &mut entry.handle {
                handle.send_command(&command);
            }
            if entry.command_history.last() != Some(&command) {
                entry.command_history.push(command);
            }
            entry.history_pos = None;
            entry.command_input.clear();
        }
    }

    /// Player lists and whitelist management for the selected server.
    fn render_players_page(&mut self, ui: &mut egui::Ui, index: usize) {
        let skin_textures = &self.player_skin_textures;
        let entry = &mut self.servers[index];
        let online_players = entry.online_players.iter().cloned().collect::<Vec<_>>();
        let online_names = online_players
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let catalog = entry.player_management.catalog.clone();
        let offline_players = catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .known_players
                    .iter()
                    .filter(|player| !online_names.contains(&player.name.to_ascii_lowercase()))
                    .map(|player| player.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let whitelist = catalog
            .as_ref()
            .map(|catalog| catalog.whitelist.clone())
            .unwrap_or_default();
        let ip_bans = catalog
            .as_ref()
            .map(|catalog| catalog.banned_ips.clone())
            .unwrap_or_default();
        let server_running = entry
            .handle
            .as_ref()
            .is_some_and(|handle| handle.state() != ServerState::Stopped);
        let mut selected_view = entry.player_management.view;
        let mut copy = false;
        let mut add_player = false;
        let mut remove_player = None;
        let mut add_ip_entry = false;
        let mut remove_ip_entry = None;
        let mut selected_player = None;
        let mut visible_skin_requests = Vec::new();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                ui.label(theme::title_with_icon(
                    theme::icons::USERS,
                    "Player Management",
                    theme::PRIMARY,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if selected_view == PlayerView::Online
                        && !online_players.is_empty()
                        && ui
                            .add(theme::ghost_icon_button(theme::icons::COPY, "Copy list"))
                            .clicked()
                    {
                        copy = true;
                    }
                });
            },
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            for (view, glyph, label) in [
                (
                    PlayerView::Online,
                    theme::icons::USERS,
                    format!("Online ({})", online_players.len()),
                ),
                (
                    PlayerView::Offline,
                    theme::icons::CLOCK,
                    format!("Offline ({})", offline_players.len()),
                ),
                (
                    PlayerView::Whitelist,
                    theme::icons::CHECK,
                    format!("Whitelist ({})", whitelist.len()),
                ),
                (
                    PlayerView::IpBans,
                    theme::icons::GLOBE,
                    format!("IP Bans ({})", ip_bans.len()),
                ),
            ] {
                if ui
                    .add(theme::nav_tab(selected_view == view, glyph, &label))
                    .clicked()
                {
                    selected_view = view;
                }
            }
        });
        entry.player_management.view = selected_view;
        ui.add_space(10.0);

        if let Some(error) = &entry.player_management.error {
            ui.label(egui::RichText::new(error).color(theme::ERROR));
            ui.add_space(6.0);
        }
        if let Some(notice) = &entry.player_management.notice {
            ui.label(egui::RichText::new(notice).color(theme::PRIMARY));
            ui.add_space(6.0);
        }

        match selected_view {
            PlayerView::Online => render_player_list(
                ui,
                &online_players,
                skin_textures,
                PlayerListPresentation {
                    status: "ONLINE",
                    status_color: theme::PRIMARY,
                    empty_title: "No players online",
                    empty_hint: "Players appear here when they join the server.",
                    is_online: true,
                },
                &mut selected_player,
                &mut visible_skin_requests,
            ),
            PlayerView::Offline => {
                if entry.player_management.catalog.is_none() {
                    render_player_list_loading(
                        ui,
                        entry.player_management.refresh_pending.is_some(),
                    );
                } else {
                    render_player_list(
                        ui,
                        &offline_players,
                        skin_textures,
                        PlayerListPresentation {
                            status: "OFFLINE",
                            status_color: theme::TEXT_MUTED,
                            empty_title: "No offline players found",
                            empty_hint: "Players appear here after they have joined the server.",
                            is_online: false,
                        },
                        &mut selected_player,
                        &mut visible_skin_requests,
                    );
                }
            }
            PlayerView::Whitelist => {
                let pending = entry.player_management.pending.is_some();
                let can_edit = !pending && entry.player_management.catalog.is_some();
                theme::section_title_with_icon(ui, theme::icons::CHECK, "WHITELIST");
                ui.add_space(4.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Add a player by Minecraft username")
                            .color(theme::TEXT),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(can_edit, |ui| {
                            let response = ui.add(
                                egui::TextEdit::singleline(
                                    &mut entry.player_management.username,
                                )
                                .desired_width(300.0)
                                .hint_text("Username"),
                            );
                            if response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                            {
                                add_player = true;
                            }
                            if ui
                                .add(theme::primary_icon_button(
                                    theme::icons::PLUS,
                                    "Add",
                                ))
                                .clicked()
                            {
                                add_player = true;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Online-mode servers look up UUIDs with Mojang; offline-mode servers use offline UUIDs.",
                        )
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                    );
                });
                ui.add_space(8.0);
                if pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("Updating whitelist...").color(theme::TEXT_MUTED),
                        );
                    });
                } else if entry.player_management.catalog.is_none() {
                    render_player_list_loading(
                        ui,
                        entry.player_management.refresh_pending.is_some(),
                    );
                } else if whitelist.is_empty() {
                    render_player_list_empty(
                        ui,
                        "Whitelist is empty",
                        "Add a username above to allow a player to join.",
                    );
                } else {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    for profile in &whitelist {
                        render_whitelist_row(
                            ui,
                            profile,
                            skin_textures,
                            can_edit,
                            &mut remove_player,
                            &mut visible_skin_requests,
                        );
                    }
                }
            }
            PlayerView::IpBans => {
                let pending = entry.player_management.pending.is_some();
                let can_edit = !pending && entry.player_management.catalog.is_some();
                theme::section_title_with_icon(ui, theme::icons::GLOBE, "IP BANS");
                ui.add_space(4.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("Add an IP address to the ban list").color(theme::TEXT),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let response = ui.add_enabled(
                            can_edit,
                            egui::TextEdit::singleline(&mut entry.player_management.ip_address)
                                .desired_width(300.0)
                                .hint_text("IPv4 or IPv6 address"),
                        );
                        if response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter))
                        {
                            add_ip_entry = true;
                        }
                        let candidate = entry
                            .player_management
                            .ip_address
                            .trim()
                            .parse::<IpAddr>()
                            .ok();
                        let already_banned = candidate.is_some_and(|candidate| {
                            ip_bans
                                .iter()
                                .any(|ip| ip.parse::<IpAddr>().ok() == Some(candidate))
                        });
                        if ui
                            .add_enabled(
                                can_edit && candidate.is_some() && !already_banned,
                                theme::primary_icon_button(theme::icons::PLUS, "Add"),
                            )
                            .clicked()
                        {
                            add_ip_entry = true;
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(if server_running {
                            "Changes are sent to the running server."
                        } else {
                            "Changes are saved to banned-ips.json for the next server start."
                        })
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                    );
                });
                ui.add_space(8.0);
                if pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("Updating IP bans...").color(theme::TEXT_MUTED),
                        );
                    });
                } else if entry.player_management.catalog.is_none() {
                    render_player_list_loading(
                        ui,
                        entry.player_management.refresh_pending.is_some(),
                    );
                } else if ip_bans.is_empty() {
                    render_player_list_empty(
                        ui,
                        "No IP bans",
                        "Add an IP address above to block it from joining.",
                    );
                } else {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    for ip in &ip_bans {
                        render_ip_ban_row(ui, ip, can_edit, &mut remove_ip_entry);
                    }
                }
            }
        }
        if copy {
            ui.ctx().copy_text(online_players.join(", "));
        }
        if add_player {
            entry
                .player_management
                .add(entry.config.working_dir.clone());
        }
        if let Some(uuid) = remove_player {
            entry
                .player_management
                .remove(entry.config.working_dir.clone(), uuid);
        }
        if add_ip_entry {
            if entry.player_management.pending.is_some()
                || entry.player_management.catalog.is_none()
            {
                entry.player_management.error =
                    Some("Player data is updating. Try the action again shortly.".to_string());
                entry.player_management.notice = None;
            } else if let Ok(address) = entry.player_management.ip_address.trim().parse::<IpAddr>()
            {
                let ip = address.to_string();
                let already_banned = ip_bans
                    .iter()
                    .any(|banned| banned.parse::<IpAddr>().ok() == Some(address));
                if already_banned {
                    entry.player_management.error = Some(format!("{ip} is already banned."));
                    entry.player_management.notice = None;
                } else if server_running {
                    if let Some(handle) = entry.handle.as_mut() {
                        handle.send_command(&format!("ban-ip {ip}"));
                    }
                    entry.player_management.last_refresh = None;
                    entry.player_management.error = None;
                    entry.player_management.ip_address.clear();
                    entry.player_management.notice = Some(format!("IP ban command sent for {ip}."));
                } else {
                    entry
                        .player_management
                        .ban_ip(entry.config.working_dir.clone(), ip);
                }
            } else {
                entry.player_management.error = Some("Enter a valid IP address.".to_string());
                entry.player_management.notice = None;
            }
        }
        if let Some(ip) = remove_ip_entry {
            if server_running {
                if let Some(handle) = entry.handle.as_mut() {
                    handle.send_command(&format!("pardon-ip {ip}"));
                }
                entry.player_management.last_refresh = None;
                entry.player_management.error = None;
                entry.player_management.notice = Some(format!("IP unban command sent for {ip}."));
            } else {
                entry
                    .player_management
                    .unban_ip(entry.config.working_dir.clone(), ip);
            }
        }
        self.player_skin_requests.extend(visible_skin_requests);
        if let Some((name, is_online)) = selected_player {
            self.open_player_details(index, name, is_online);
        }
    }

    fn open_player_details(&mut self, server_index: usize, name: String, is_online: bool) {
        let Some(entry) = self.servers.get_mut(server_index) else {
            return;
        };
        let working_dir = entry.config.working_dir.clone();
        let log_lines = entry
            .console
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel();
        let player_name = name.clone();
        std::thread::spawn(move || {
            let result = read_player_details(&working_dir, &player_name, &log_lines);
            let _ = tx.send(result);
        });

        let position_query_started = if is_online && is_minecraft_username(&name) {
            entry.handle.as_mut().map(|handle| {
                handle.send_command(&format!("data get entity {name} Pos"));
                Instant::now()
            })
        } else {
            None
        };
        self.player_details = Some(PlayerDetailsDialog {
            server_index,
            name,
            is_online,
            details_rx: Some(rx),
            details: None,
            error: None,
            action_error: None,
            action_notice: None,
            message: String::new(),
            live_position: None,
            position_query_started,
        });
    }

    fn poll_player_details(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.player_details else {
            return;
        };
        let result = dialog.details_rx.as_ref().map(Receiver::try_recv);
        match result {
            Some(Ok(Ok(details))) => {
                dialog.details = Some(details);
                dialog.details_rx = None;
            }
            Some(Ok(Err(err))) => {
                dialog.error = Some(err);
                dialog.details_rx = None;
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                dialog.error = Some("Player details task stopped unexpectedly.".to_string());
                dialog.details_rx = None;
            }
            Some(Err(mpsc::TryRecvError::Empty)) => {
                ctx.request_repaint_after(Duration::from_millis(150));
            }
            None => {}
        }
        if dialog
            .position_query_started
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(3))
        {
            dialog.position_query_started = None;
        } else if dialog.position_query_started.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn render_player_details(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.player_details.take() else {
            return;
        };
        let player_ip = dialog
            .details
            .as_ref()
            .and_then(|details| details.ip_address.as_deref())
            .and_then(|ip| ip.parse::<IpAddr>().ok());
        let (
            is_online,
            server_running,
            banned_uuid,
            is_ip_banned,
            catalog_loaded,
            management_pending,
            management_error,
            management_notice,
        ) = self
            .servers
            .get(dialog.server_index)
            .map(|entry| {
                let player_uuid = dialog
                    .details
                    .as_ref()
                    .and_then(|details| details.uuid.clone())
                    .or_else(|| {
                        entry
                            .player_management
                            .catalog
                            .as_ref()
                            .and_then(|catalog| {
                                catalog
                                    .known_players
                                    .iter()
                                    .find(|profile| profile.name.eq_ignore_ascii_case(&dialog.name))
                                    .map(|profile| profile.uuid.clone())
                            })
                    });
                let banned_profile = entry
                    .player_management
                    .catalog
                    .as_ref()
                    .and_then(|catalog| {
                        catalog.banned_players.iter().find(|profile| {
                            profile.name.eq_ignore_ascii_case(&dialog.name)
                                || player_uuid
                                    .as_deref()
                                    .is_some_and(|uuid| profile.uuid.eq_ignore_ascii_case(uuid))
                        })
                    });
                let is_ip_banned =
                    entry
                        .player_management
                        .catalog
                        .as_ref()
                        .is_some_and(|catalog| {
                            player_ip.is_some_and(|ip| {
                                catalog
                                    .banned_ips
                                    .iter()
                                    .any(|address| address.parse::<IpAddr>().ok() == Some(ip))
                            })
                        });
                (
                    entry
                        .online_players
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&dialog.name)),
                    entry
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.state() != ServerState::Stopped),
                    banned_profile.map(|profile| profile.uuid.clone()),
                    is_ip_banned,
                    entry.player_management.catalog.is_some(),
                    entry.player_management.pending.is_some(),
                    entry.player_management.error.clone(),
                    entry.player_management.notice.clone(),
                )
            })
            .unwrap_or((false, false, None, false, false, false, None, None));
        dialog.is_online = is_online;
        let player_name_valid = is_minecraft_username(&dialog.name);
        let mut close = false;
        let mut requested_action = None;
        let modal = egui::Modal::new(egui::Id::new((
            "player_details",
            dialog.server_index,
            &dialog.name,
        )))
        .frame(
            egui::Frame::new()
                .fill(theme::SURFACE)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(12))
                .inner_margin(egui::Margin::same(20)),
        )
        .backdrop_color(egui::Color32::from_black_alpha(140))
        .show(ctx, |ui| {
            ui.set_width(500.0);
            ui.horizontal(|ui| {
                ui.label(theme::title_with_icon(
                    theme::icons::USERS,
                    "Player Details",
                    theme::PRIMARY,
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(theme::ghost_icon_button(theme::icons::XMARK, "Close"))
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
            ui.add_space(12.0);
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.horizontal(|ui| {
                        if let Some(Some(texture)) = self
                            .player_skin_textures
                            .get(&dialog.name.to_ascii_lowercase())
                        {
                            ui.add(
                                egui::Image::new(texture).fit_to_exact_size(egui::vec2(56.0, 56.0)),
                            );
                        } else {
                            ui.label(
                                theme::icon(theme::icons::USERS, 40.0).color(theme::TEXT_MUTED),
                            );
                        }
                        ui.vertical_centered(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(&dialog.name)
                                        .size(18.0)
                                        .strong()
                                        .color(theme::TEXT),
                                );
                                theme::status_pill(
                                    ui,
                                    if dialog.is_online {
                                        "ONLINE"
                                    } else {
                                        "OFFLINE"
                                    },
                                    if dialog.is_online {
                                        theme::PRIMARY
                                    } else {
                                        theme::TEXT_MUTED
                                    },
                                );
                            });
                        });
                    });
                });
            });
            ui.add_space(10.0);

            if let Some(error) = &dialog.error {
                ui.label(egui::RichText::new(error).color(theme::ERROR));
                ui.add_space(8.0);
            }

            let playtime = dialog
                .details
                .as_ref()
                .and_then(|details| details.play_time_ticks)
                .map(|ticks| format_duration(Duration::from_secs(ticks / 20)))
                .unwrap_or_else(|| {
                    if dialog.details.is_none() && dialog.error.is_none() {
                        "Loading...".to_string()
                    } else {
                        "Unavailable".to_string()
                    }
                });
            let coordinates = if let Some([x, y, z]) = dialog.live_position {
                format!("{x:.1}, {y:.1}, {z:.1} · Current")
            } else if let Some([x, y, z]) = dialog
                .details
                .as_ref()
                .and_then(|details| details.last_position)
            {
                format!("{x:.1}, {y:.1}, {z:.1} · Last login")
            } else if dialog.position_query_started.is_some() {
                "Querying...".to_string()
            } else {
                "Unavailable".to_string()
            };
            let ip_address = dialog
                .details
                .as_ref()
                .and_then(|details| details.ip_address.as_deref())
                .unwrap_or("Unavailable");
            let uuid = dialog
                .details
                .as_ref()
                .and_then(|details| details.uuid.as_deref())
                .unwrap_or("Unavailable");
            egui::Grid::new(("player_details_grid", dialog.server_index))
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    player_detail_card(ui, theme::icons::CLOCK, "Playtime", &playtime);
                    player_detail_card(ui, theme::icons::LOCATION_DOT, "Coordinates", &coordinates);
                    ui.end_row();
                    player_detail_card(ui, theme::icons::GLOBE, "IP Address", ip_address);
                    player_detail_card(ui, theme::icons::FINGERPRINT, "UUID", uuid);
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Playtime reflects the latest saved server stats.")
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
            ui.add_space(12.0);
            theme::section_title_with_icon(ui, theme::icons::TERMINAL, "PLAYER CONTROLS");
            ui.add_space(6.0);
            if let Some(error) = &dialog.action_error {
                ui.label(egui::RichText::new(error).color(theme::ERROR));
            }
            if let Some(notice) = &dialog.action_notice {
                ui.label(egui::RichText::new(notice).color(theme::PRIMARY));
            }
            if let Some(error) = &management_error {
                ui.label(egui::RichText::new(error).color(theme::ERROR));
            }
            if let Some(notice) = &management_notice {
                ui.label(egui::RichText::new(notice).color(theme::PRIMARY));
            }

            let ip_ban_target = if dialog.is_online {
                Some(dialog.name.clone())
            } else {
                player_ip.map(|ip| ip.to_string())
            };
            let ban_enabled = player_name_valid
                && (server_running || (catalog_loaded && !management_pending));
            ui.horizontal_wrapped(|ui| {
                if dialog.is_online
                    && ui
                        .add_enabled(
                            server_running && player_name_valid,
                            theme::danger_button("Kick"),
                        )
                        .clicked()
                {
                    requested_action = Some(PlayerDetailsAction::SendCommand {
                        command: format!("kick {}", dialog.name),
                        notice: format!("Kick command sent for {}.", dialog.name),
                        requires_online: true,
                    });
                }
                let ban_button = if banned_uuid.is_some() {
                    theme::secondary_button("Unban")
                } else {
                    theme::danger_button("Ban")
                };
                if ui.add_enabled(ban_enabled, ban_button).clicked() {
                    requested_action = if server_running {
                        Some(PlayerDetailsAction::SendCommand {
                            command: if banned_uuid.is_some() {
                                format!("pardon {}", dialog.name)
                            } else {
                                format!("ban {}", dialog.name)
                            },
                            notice: if banned_uuid.is_some() {
                                format!("Unban command sent for {}.", dialog.name)
                            } else {
                                format!("Ban command sent for {}.", dialog.name)
                            },
                            requires_online: false,
                        })
                    } else if let Some(uuid) = &banned_uuid {
                        Some(PlayerDetailsAction::RemovePlayerBan(uuid.clone()))
                    } else {
                        Some(PlayerDetailsAction::AddPlayerBan)
                    };
                }
                let ip_button = if is_ip_banned {
                    theme::secondary_button("IP Unban")
                } else {
                    theme::danger_button("IP Ban")
                };
                let ip_ban_enabled = if server_running {
                    player_name_valid && (dialog.is_online || player_ip.is_some())
                } else {
                    catalog_loaded && !management_pending && player_ip.is_some()
                };
                if ui.add_enabled(ip_ban_enabled, ip_button).clicked() {
                    let ip = player_ip.map(|ip| ip.to_string());
                    requested_action = if is_ip_banned {
                        ip.map(|ip| {
                            if server_running {
                                PlayerDetailsAction::SendCommand {
                                    command: format!("pardon-ip {ip}"),
                                    notice: format!("IP unban command sent for {ip}."),
                                    requires_online: false,
                                }
                            } else {
                                PlayerDetailsAction::RemoveIpBan(ip)
                            }
                        })
                    } else if server_running {
                        ip_ban_target.map(|target| PlayerDetailsAction::SendCommand {
                            command: format!("ban-ip {target}"),
                            notice: format!("IP ban command sent for {}.", dialog.name),
                            requires_online: false,
                        })
                    } else {
                        ip.map(PlayerDetailsAction::AddIpBan)
                    };
                }
            });
            ui.label(
                egui::RichText::new(
                    "IP Ban uses the current IP for online players and the last known IP otherwise.",
                )
                .size(10.5)
                .color(theme::TEXT_MUTED),
            );
            if dialog.is_online {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let input_width = (ui.available_width() - 90.0).max(120.0);
                    ui.add_sized(
                        [input_width, 34.0],
                        egui::TextEdit::singleline(&mut dialog.message)
                            .hint_text("Message this player"),
                    );
                    if ui
                        .add_enabled(
                            server_running
                                && player_name_valid
                                && !dialog.message.trim().is_empty(),
                            theme::primary_button("Message"),
                        )
                        .clicked()
                    {
                        let message = dialog.message.trim().to_string();
                        if message.chars().any(|character| matches!(character, '\r' | '\n')) {
                            dialog.action_error =
                                Some("Messages must be a single line.".to_string());
                        } else {
                            requested_action = Some(PlayerDetailsAction::SendCommand {
                                command: format!("tell {} {message}", dialog.name),
                                notice: format!("Message sent to {}.", dialog.name),
                                requires_online: true,
                            });
                            dialog.message.clear();
                        }
                    }
                });
            }
        });
        if let Some(action) = requested_action {
            self.apply_player_details_action(&mut dialog, action);
        }
        if modal.should_close() || close {
            self.player_details = None;
        } else {
            self.player_details = Some(dialog);
        }
    }

    fn apply_player_details_action(
        &mut self,
        dialog: &mut PlayerDetailsDialog,
        action: PlayerDetailsAction,
    ) {
        dialog.action_error = None;
        dialog.action_notice = None;
        match action {
            PlayerDetailsAction::SendCommand {
                command,
                notice,
                requires_online,
            } => {
                let Some(entry) = self.servers.get_mut(dialog.server_index) else {
                    dialog.action_error = Some("The server is no longer available.".to_string());
                    return;
                };
                let is_online = entry
                    .online_players
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&dialog.name));
                if requires_online && !is_online {
                    dialog.is_online = false;
                    dialog.action_error =
                        Some("This action requires the player to be online.".to_string());
                    return;
                }
                if !is_minecraft_username(&dialog.name) {
                    dialog.action_error = Some("This player name is invalid.".to_string());
                    return;
                }
                let Some(handle) = entry
                    .handle
                    .as_mut()
                    .filter(|handle| handle.state() != ServerState::Stopped)
                else {
                    dialog.action_error =
                        Some("Start the server before using player controls.".to_string());
                    return;
                };
                handle.send_command(&command);
                if command.starts_with("ban ")
                    || command.starts_with("pardon ")
                    || command.starts_with("ban-ip ")
                    || command.starts_with("pardon-ip ")
                {
                    entry.player_management.last_refresh = None;
                }
                dialog.action_notice =
                    Some(format!("{notice} Check the server console for result."));
            }
            PlayerDetailsAction::AddPlayerBan => {
                let Some(entry) = self.servers.get_mut(dialog.server_index) else {
                    dialog.action_error = Some("The server is no longer available.".to_string());
                    return;
                };
                if entry
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.state() != ServerState::Stopped)
                {
                    dialog.action_error =
                        Some("The server started; use the Ban command while it is running.".into());
                    return;
                }
                if entry.player_management.pending.is_some() {
                    dialog.action_error =
                        Some("Player data is updating. Try the action again shortly.".to_string());
                    return;
                }
                entry
                    .player_management
                    .ban_player(entry.config.working_dir.clone(), dialog.name.clone());
            }
            PlayerDetailsAction::RemovePlayerBan(uuid) => {
                let Some(entry) = self.servers.get_mut(dialog.server_index) else {
                    dialog.action_error = Some("The server is no longer available.".to_string());
                    return;
                };
                if entry
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.state() != ServerState::Stopped)
                {
                    dialog.action_error =
                        Some("The server started; use Unban while it is running.".into());
                    return;
                }
                if entry.player_management.pending.is_some() {
                    dialog.action_error =
                        Some("Player data is updating. Try the action again shortly.".to_string());
                    return;
                }
                entry
                    .player_management
                    .unban_player(entry.config.working_dir.clone(), uuid);
            }
            PlayerDetailsAction::AddIpBan(ip) => {
                let Some(entry) = self.servers.get_mut(dialog.server_index) else {
                    dialog.action_error = Some("The server is no longer available.".to_string());
                    return;
                };
                if entry
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.state() != ServerState::Stopped)
                {
                    dialog.action_error =
                        Some("The server started; use IP Ban while it is running.".into());
                    return;
                }
                if entry.player_management.pending.is_some() {
                    dialog.action_error =
                        Some("Player data is updating. Try the action again shortly.".to_string());
                    return;
                }
                entry
                    .player_management
                    .ban_ip(entry.config.working_dir.clone(), ip);
            }
            PlayerDetailsAction::RemoveIpBan(ip) => {
                let Some(entry) = self.servers.get_mut(dialog.server_index) else {
                    dialog.action_error = Some("The server is no longer available.".to_string());
                    return;
                };
                if entry
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.state() != ServerState::Stopped)
                {
                    dialog.action_error =
                        Some("The server started; use IP Unban while it is running.".into());
                    return;
                }
                if entry.player_management.pending.is_some() {
                    dialog.action_error =
                        Some("Player data is updating. Try the action again shortly.".to_string());
                    return;
                }
                entry
                    .player_management
                    .unban_ip(entry.config.working_dir.clone(), ip);
            }
        }
    }

    fn ensure_player_skins(&mut self) {
        let names = self
            .player_skin_requests
            .iter()
            .take(MAX_PLAYER_SKINS_IN_FLIGHT)
            .cloned()
            .collect::<Vec<_>>();
        for name in names {
            self.player_skin_requests.remove(&name);
            let key = name.to_ascii_lowercase();
            if self.player_skin_textures.contains_key(&key)
                || self.player_skin_pending.contains(&key)
            {
                continue;
            }
            if !is_minecraft_username(&name) {
                self.player_skin_textures.insert(key, None);
                continue;
            }
            self.player_skin_pending.insert(key.clone());
            let tx = self.player_skin_tx.clone();
            std::thread::spawn(move || {
                let result = fetch_player_skin(&name);
                let _ = tx.send((key, result));
            });
        }
    }

    fn poll_player_skins(&mut self, ctx: &egui::Context) {
        for _ in 0..MAX_PLAYER_SKINS_IN_FLIGHT {
            let (key, result) = match self.player_skin_rx.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            };
            self.player_skin_pending.remove(&key);
            let texture = match result {
                Ok(bytes) => {
                    let texture = load_player_skin_texture(ctx, &bytes, &key);
                    if texture.is_none() {
                        eprintln!("Could not decode player skin image for {key}.");
                    }
                    texture
                }
                Err(err) => {
                    eprintln!("Could not download player skin for {key}: {err}");
                    None
                }
            };
            self.player_skin_textures.insert(key, texture);
            ctx.request_repaint();
        }
        if !self.player_skin_pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    /// The full-page server creation flow, rendered in the main panel while
    /// `self.create_page` is open.
    fn render_create_page(&mut self, ui: &mut egui::Ui) {
        if self.create_page.is_none() {
            return;
        }
        let mut cancel = false;
        let mut create_clicked = false;

        ui.spacing_mut().item_spacing.y = 8.0;
        ui.label(theme::title_with_icon(
            theme::icons::PLUS,
            "Create Server",
            theme::PRIMARY,
        ));
        ui.label(
            egui::RichText::new("Download and configure a new Minecraft server.")
                .size(13.0)
                .color(theme::TEXT_MUTED),
        );

        let page = self.create_page.as_ref().unwrap();
        let footer_height = 48.0
            + if page.pending.is_some() { 32.0 } else { 0.0 }
            + if page.error.is_some() { 28.0 } else { 0.0 };
        // The gap lives outside the scroll area: inside, the columns fill the
        // whole body height, so any extra pixel would make the page scroll.
        ui.add_space(6.0);
        let body_height = (ui.available_height() - footer_height).max(0.0);
        show_create_page_scroll_area(ui, footer_height, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                // The first child of a horizontal row only sees the row's
                // current (small) height; grow the cursor up front so the
                // left column's lists can fill the computed body height.
                let _ = ui.allocate_exact_size(egui::vec2(0.0, body_height), egui::Sense::hover());
                // Left: software and version pickers.
                ui.vertical(|ui| {
                    ui.set_width((ui.available_width() * 0.48).clamp(280.0, 430.0));
                    self.render_create_software_column(ui);
                });
                // Right: configuration form.
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    self.render_create_settings_form(ui);
                });
            });
        });

        let page = self.create_page.as_mut().unwrap();
        if let Some(pending) = &page.pending {
            ui.add_space(6.0);
            let fraction = if pending.total > 0 {
                (pending.downloaded as f32 / pending.total as f32).min(1.0)
            } else {
                0.0
            };
            ui.add(egui::ProgressBar::new(fraction).text(format!(
                "Downloading {} / {}",
                format_mb(pending.downloaded),
                format_mb(pending.total)
            )));
        }
        if let Some(err) = &page.error {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(err).color(theme::ERROR));
        }

        ui.add_space(6.0);
        let downloading = page.is_downloading();
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if downloading {
                    ui.add_enabled(false, theme::primary_button("  Creating…  "));
                } else if ui
                    .add(theme::primary_icon_button(
                        theme::icons::CHECK,
                        "Create Server",
                    ))
                    .clicked()
                {
                    create_clicked = true;
                }
                if ui
                    .add_enabled(
                        !downloading,
                        theme::ghost_icon_button(theme::icons::XMARK, "Cancel"),
                    )
                    .clicked()
                {
                    cancel = true;
                }
            });
        });

        // Esc discards the page like the cancel button; an in-flight download
        // must not be interrupted.
        if !downloading && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if downloading {
            // Keep refreshing while the download is in flight.
            ui.ctx().request_repaint();
        }
        page.poll();

        if create_clicked {
            let servers_root = self.store.servers_dir();
            let assigned: Vec<u16> = self.servers.iter().map(|e| e.config.server_port).collect();
            let page = self.create_page.as_mut().unwrap();
            page.start_create(&servers_root, &assigned);
        }

        let finished = self
            .create_page
            .as_mut()
            .and_then(CreatePage::take_finished);
        if let Some((config, result)) = finished {
            match result {
                Ok(()) => {
                    self.servers.push(ServerEntry::new(config));
                    self.select_server(self.servers.len() - 1);
                    self.save();
                    self.create_page = None;
                }
                Err(err) => {
                    let page = self.create_page.as_mut().unwrap();
                    page.pending = None;
                    page.error = Some(err);
                }
            }
        }

        if cancel
            && !self
                .create_page
                .as_ref()
                .is_some_and(CreatePage::is_downloading)
        {
            self.create_page = None;
        }
    }

    /// Left column of the create page: searchable software list and, below it,
    /// the searchable version list for the selected software. Both lists share
    /// the column height so the cards fill the page instead of a fixed slice.
    fn render_create_software_column(&mut self, ui: &mut egui::Ui) {
        let page = self.create_page.as_mut().unwrap();
        // Pick the newest build once the version list for the preselected
        // software has loaded.
        if page.selected_version.is_none() {
            if let Some(server_type) = &page.selected_type {
                page.selected_version = self
                    .software
                    .versions(server_type)
                    .last()
                    .map(|version| version.id.clone());
            }
        }

        // Each card spends roughly this much on its title, search field, gaps
        // and margins. The software list takes about half of the column; the
        // version list is measured inside its own card (below), so it always
        // fills whatever height the software card actually left behind.
        const CARD_CHROME: f32 = 101.0;
        const GAP: f32 = 10.0;
        let software_list_height = ((ui.available_height() - GAP) * 0.5 - CARD_CHROME).max(80.0);

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title_with_icon(ui, theme::icons::SERVER, "SERVER SOFTWARE");
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut page.software_filter)
                    .id(egui::Id::new("create_software_filter"))
                    .desired_width(f32::INFINITY)
                    .hint_text("Search software…"),
            );
            ui.add_space(6.0);
            let filter = page.software_filter.trim().to_lowercase();
            egui::ScrollArea::vertical()
                .id_salt("create_software_list")
                .auto_shrink([false, false])
                .hscroll(false)
                .max_height(software_list_height)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let Some(snapshot) = &self.software.snapshot else {
                        if self.software.is_loading() {
                            ui.label("Loading server software…");
                        } else {
                            let message = self
                                .software
                                .error
                                .as_deref()
                                .unwrap_or("Failed to load server software.");
                            ui.label(egui::RichText::new(message).color(theme::ERROR));
                        }
                        return;
                    };
                    let mut clicked = None;
                    let mut matched = 0usize;
                    for (server_type, info) in snapshot.types.all() {
                        if !filter.is_empty() && !info.name.to_lowercase().contains(&filter) {
                            continue;
                        }
                        matched += 1;
                        let key = server_type.api_name().to_lowercase();
                        if software_row(
                            &self.icon_textures,
                            ui,
                            &key,
                            &info.name,
                            page.selected_type.as_ref() == Some(server_type),
                        ) {
                            clicked = Some(server_type.clone());
                        }
                    }
                    if matched == 0 {
                        ui.label(
                            egui::RichText::new("No software found.").color(theme::TEXT_MUTED),
                        );
                    }
                    if let Some(server_type) = clicked {
                        page.selected_version = self
                            .software
                            .versions(&server_type)
                            .last()
                            .map(|version| version.id.clone());
                        page.selected_type = Some(server_type);
                    }
                });
        });
        ui.add_space(GAP);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title_with_icon(ui, theme::icons::TAG, "VERSION");
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut page.version_filter)
                    .id(egui::Id::new("create_version_filter"))
                    .desired_width(f32::INFINITY)
                    .hint_text("Search versions…"),
            );
            ui.add_space(6.0);
            // Measured here, after the card chrome: this is exactly the
            // height the scroll area can occupy below, so the card always
            // reaches the bottom of the column.
            let version_list_height = ui.available_height().max(80.0);
            let filter = page.version_filter.trim().to_lowercase();
            egui::ScrollArea::vertical()
                .id_salt("create_version_list")
                .auto_shrink([false, false])
                .hscroll(false)
                .max_height(version_list_height)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let Some(server_type) = page.selected_type.as_ref() else {
                        ui.label(
                            egui::RichText::new("Select a software first.")
                                .color(theme::TEXT_MUTED),
                        );
                        return;
                    };
                    let versions = self.software.versions(server_type);
                    if versions.is_empty() {
                        ui.label("No versions found.");
                        return;
                    }
                    let latest = versions.last().map(|version| version.id.clone());
                    let mut matched = 0usize;
                    for version in versions.iter().rev() {
                        if !filter.is_empty() && !version.id.to_lowercase().contains(&filter) {
                            continue;
                        }
                        matched += 1;
                        let selected = page.selected_version.as_ref() == Some(&version.id);
                        if version_row(
                            ui,
                            &version.id,
                            selected,
                            latest.as_ref() == Some(&version.id),
                        ) {
                            page.selected_version = Some(version.id.clone());
                        }
                    }
                    if matched == 0 {
                        ui.label(
                            egui::RichText::new("No versions match your search.")
                                .color(theme::TEXT_MUTED),
                        );
                    }
                });
        });
    }

    /// Right column of the create page: the server configuration form.
    fn render_create_settings_form(&mut self, ui: &mut egui::Ui) {
        let page = self.create_page.as_mut().unwrap();

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title_with_icon(ui, theme::icons::SLIDERS, "GENERAL");
            ui.add_space(6.0);
            form_label(ui, "Server name");
            ui.add(egui::TextEdit::singleline(&mut page.name).desired_width(f32::INFINITY));
            ui.add_space(10.0);
            memory_stepper(ui, "Memory (max)", &mut page.max_memory_mb, 512, 65536);
            ui.add_space(10.0);
            form_label(ui, "JVM arguments (optional)");
            ui.add(
                egui::TextEdit::singleline(&mut page.jvm_args)
                    .id(egui::Id::new("create_jvm_args"))
                    .desired_width(f32::INFINITY)
                    .hint_text("-XX:+UseG1GC"),
            );
        });
        ui.add_space(10.0);

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title_with_icon(ui, theme::icons::GLOBE, "WORLD");
            ui.add_space(6.0);
            settings_combo(
                ui,
                "create_gamemode",
                "Gamemode",
                &GAMEMODE_OPTIONS,
                &mut page.gamemode,
            );
            ui.add_space(8.0);
            settings_combo(
                ui,
                "create_difficulty",
                "Difficulty",
                &DIFFICULTY_OPTIONS,
                &mut page.difficulty,
            );
            ui.add_space(8.0);
            settings_combo(
                ui,
                "create_level_type",
                "Level type",
                &LEVEL_TYPE_OPTIONS,
                &mut page.level_type,
            );
            ui.add_space(8.0);
            form_label(ui, "Level seed (optional)");
            ui.add(
                egui::TextEdit::singleline(&mut page.level_seed)
                    .id(egui::Id::new("create_level_seed"))
                    .desired_width(f32::INFINITY)
                    .hint_text("Random when empty"),
            );
            ui.add_space(10.0);
            form_label(ui, "Max players");
            ui.add(
                egui::DragValue::new(&mut page.max_players)
                    .range(1..=1000)
                    .speed(1),
            );
        });
        ui.add_space(10.0);

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section_title_with_icon(ui, theme::icons::LOCATION_DOT, "NETWORK");
            ui.add_space(6.0);
            form_label(ui, "Server port");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut page.server_port)
                        .range(1..=65535)
                        .speed(1),
                );
                let assigned: Vec<u16> =
                    self.servers.iter().map(|e| e.config.server_port).collect();
                if assigned.contains(&page.server_port) {
                    ui.label(
                        egui::RichText::new("Already used by another Spoon server")
                            .size(12.0)
                            .color(theme::WARN),
                    );
                } else if !spoon_core::port::is_port_available(page.server_port) {
                    ui.label(
                        egui::RichText::new("Port is in use")
                            .size(12.0)
                            .color(theme::WARN),
                    );
                }
            });
            ui.add_space(10.0);
            settings_toggle(
                ui,
                &mut page.online_mode,
                "Online Mode",
                "Verify that players own the game",
            );
            ui.add_space(8.0);
            settings_toggle(
                ui,
                &mut page.use_whitelist,
                "Use Whitelist",
                "Only allow listed players to join",
            );
        });
    }

    fn render_import_dialog(&mut self, ctx: &egui::Context) {
        if self.import_dialog.is_none() {
            return;
        }
        let mut cancel = false;
        let mut choose_folder = false;
        let mut import_clicked = false;
        let modal = egui::Modal::new(egui::Id::new("import_server_dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(20)),
            )
            .backdrop_color(egui::Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(theme::title_with_icon(
                    theme::icons::FOLDER_OPEN,
                    "Import Existing Server",
                    theme::PRIMARY,
                ));
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("Choose a name and folder to copy into Spoon.")
                        .size(13.0)
                        .color(theme::TEXT_MUTED),
                );
                ui.add_space(14.0);
                let dialog = self.import_dialog.as_mut().unwrap();
                ui.label(
                    egui::RichText::new("Name")
                        .size(12.0)
                        .color(theme::TEXT_MUTED),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .hint_text("Server name")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(theme::ghost_icon_button(
                            theme::icons::FOLDER_OPEN,
                            "Choose Folder",
                        ))
                        .clicked()
                    {
                        choose_folder = true;
                    }
                    match &dialog.source_dir {
                        Some(folder) => {
                            let name = folder
                                .file_name()
                                .unwrap_or(folder.as_os_str())
                                .to_string_lossy();
                            ui.label(egui::RichText::new(name.as_ref()).color(theme::TEXT_MUTED))
                                .on_hover_text(folder.display().to_string());
                        }
                        None => {
                            ui.label(
                                egui::RichText::new("No folder selected").color(theme::TEXT_MUTED),
                            );
                        }
                    }
                });
                if let Some(err) = &dialog.error {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(err).color(theme::ERROR));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let importing = dialog.is_importing();
                        if importing {
                            ui.add_enabled(false, theme::primary_button("  Importing…  "));
                        } else if ui
                            .add(theme::primary_icon_button(theme::icons::CHECK, "Import"))
                            .clicked()
                        {
                            import_clicked = true;
                        }
                        if ui
                            .add_enabled(
                                !importing,
                                theme::ghost_icon_button(theme::icons::XMARK, "Cancel"),
                            )
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            });

        let importing = self
            .import_dialog
            .as_ref()
            .is_some_and(ImportDialog::is_importing);
        if modal.should_close() && !importing {
            self.import_dialog = None;
        }
        if importing {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        if choose_folder {
            self.import_dialog.as_mut().unwrap().choose_folder();
        }
        if import_clicked {
            let servers_root = self.store.servers_dir();
            self.import_dialog
                .as_mut()
                .unwrap()
                .start_import(&servers_root);
        }

        let finished = self
            .import_dialog
            .as_mut()
            .and_then(ImportDialog::take_finished);
        if let Some(result) = finished {
            match result {
                Ok(config) => {
                    self.servers.push(ServerEntry::new(config));
                    self.select_server(self.servers.len() - 1);
                    self.save();
                    self.import_dialog = None;
                    self.global_error = None;
                }
                Err(err) => {
                    if let Some(dialog) = &mut self.import_dialog {
                        dialog.error = Some(err);
                    }
                }
            }
        }

        if cancel {
            self.import_dialog = None;
        }
    }

    /// Confirmation dialog shown before a server is removed from the list.
    fn render_remove_confirm(&mut self, ctx: &egui::Context) {
        let Some(index) = self.confirm_remove else {
            return;
        };
        if index >= self.servers.len() {
            self.confirm_remove = None;
            return;
        }
        let name = self.servers[index].config.name.clone();
        // Only folders Spoon owns (created/duplicated under the servers dir)
        // are deleted; imported servers keep their files. Backups always live
        // in Spoon's own backups dir and are deleted with the server either way.
        let dir = self.servers[index].config.working_dir.clone();
        let server_id = self.servers[index].config.id.clone();
        let managed = dir.starts_with(self.store.servers_dir());
        let mut remove = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("remove_server_dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(20)),
            )
            .backdrop_color(egui::Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(theme::title_with_icon(
                    theme::icons::TRASH,
                    "Remove server?",
                    theme::ERROR,
                ));
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(if managed {
                        format!("\"{name}\", its files and its backups will be permanently deleted.")
                    } else {
                        format!(
                            "\"{name}\" will be removed from Spoon. Its files stay on disk, but its backups will be deleted."
                        )
                    })
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(theme::danger_icon_button(theme::icons::TRASH, "Remove"))
                            .clicked()
                        {
                            remove = true;
                        }
                        if ui
                            .add(theme::ghost_icon_button(theme::icons::XMARK, "Cancel"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });
        if modal.should_close() {
            close = true;
        }
        if remove && index < self.servers.len() {
            let mut deleted = true;
            if managed {
                match fs::remove_dir_all(&dir) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        self.global_error =
                            Some(format!("Could not delete {}: {err}", dir.display()));
                        deleted = false;
                    }
                }
            }
            if deleted {
                if let Some(store) = &self.backup_store {
                    if let Err(err) = store.remove_all(&server_id) {
                        self.global_error = Some(format!(
                            "Server removed, but its backups could not be deleted ({}): {err}",
                            store.server_dir(&server_id).display()
                        ));
                    }
                }
                self.servers.remove(index);
                if let Some(selected) = self.selected {
                    self.selected = if selected == index {
                        None
                    } else if selected > index {
                        Some(selected - 1)
                    } else {
                        Some(selected)
                    };
                }
                if self.selected.is_none() {
                    self.page = ServerPage::Overview;
                    self.settings_dialog = None;
                    self.files_dialog = None;
                    self.backups_dialog = None;
                }
                self.save();
                close = true;
            }
        }
        if close {
            self.confirm_remove = None;
        }
    }

    /// App update dialog: shows the new release, drives the download and then
    /// either restarts into the downloaded update (managed installs) or
    /// reveals it in the file manager (other platforms).
    fn render_update_dialog(&mut self, ctx: &egui::Context) {
        if !self.update_dialog_open {
            return;
        }
        let applies_in_place = updates_apply_in_place();
        let mut close = false;
        let mut download = false;
        let mut skip = false;
        let mut retry = false;
        let mut restart: Option<PathBuf> = None;
        let mut open_folder: Option<PathBuf> = None;
        let modal = egui::Modal::new(egui::Id::new("app_update_dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(20)),
            )
            .backdrop_color(egui::Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(440.0);
                theme::section_title_with_icon(ui, theme::icons::ROTATE, "App Update");
                ui.add_space(10.0);
                match &mut self.update_state {
                    UpdateState::Available(info) => {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Current: {}",
                                    env!("CARGO_PKG_VERSION")
                                ))
                                .color(theme::TEXT),
                            );
                            ui.label(
                                theme::icon(theme::icons::ARROW_RIGHT, 12.0)
                                    .color(theme::TEXT_MUTED),
                            );
                            ui.label(
                                egui::RichText::new(format!("New: {}", info.release.version))
                                    .color(theme::TEXT),
                            );
                        });
                        ui.add_space(4.0);
                        let mut details = format!("Size: {}", format_mb(info.asset.size));
                        if let Some(published) = &info.release.published_at {
                            details = format!("{details}  ·  Published: {published}");
                        }
                        ui.label(
                            egui::RichText::new(details)
                                .size(12.5)
                                .color(theme::TEXT_MUTED),
                        );
                        if !info.release.notes.is_empty() {
                            ui.add_space(8.0);
                            egui::ScrollArea::vertical()
                                .id_salt("update_notes_scroll")
                                .max_height(180.0)
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(&info.release.notes)
                                            .color(theme::TEXT_MUTED),
                                    );
                                });
                        }
                    }
                    UpdateState::Downloading {
                        downloaded, total, ..
                    } => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(egui::RichText::new("Downloading update…").color(theme::TEXT));
                        });
                        ui.add_space(8.0);
                        let fraction = if *total > 0 {
                            (*downloaded as f32 / *total as f32).min(1.0)
                        } else {
                            0.0
                        };
                        ui.add(egui::ProgressBar::new(fraction).text(format!(
                            "Downloading {} / {}",
                            format_mb(*downloaded),
                            format_mb(*total)
                        )));
                    }
                    UpdateState::Ready { path, version } => {
                        let text = if applies_in_place {
                            format!(
                                "Update v{version} downloaded. Restart Spoon to finish updating."
                            )
                        } else {
                            format!("Update v{version} saved to {}", path.display())
                        };
                        ui.label(egui::RichText::new(text).color(theme::TEXT_MUTED));
                    }
                    UpdateState::Failed(msg) => {
                        egui::Frame::new()
                            .fill(theme::ERROR.gamma_multiply(0.10))
                            .stroke(egui::Stroke::new(1.0_f32, theme::ERROR.gamma_multiply(0.5)))
                            .corner_radius(egui::CornerRadius::same(6))
                            .inner_margin(egui::Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(msg.as_str())
                                        .color(theme::ERROR)
                                        .size(12.5),
                                );
                            });
                    }
                    UpdateState::Checking | UpdateState::Idle => {
                        ui.label(
                            egui::RichText::new("No update information available.")
                                .color(theme::TEXT_MUTED),
                        );
                    }
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match &self.update_state {
                            UpdateState::Available(_) => {
                                if ui
                                    .add(theme::primary_icon_button(
                                        theme::icons::DOWNLOAD,
                                        "Download",
                                    ))
                                    .clicked()
                                {
                                    download = true;
                                }
                                if ui.add(theme::ghost_button("Skip this version")).clicked() {
                                    skip = true;
                                }
                                if ui.add(theme::ghost_button("Later")).clicked() {
                                    close = true;
                                }
                            }
                            UpdateState::Ready { path, .. } if applies_in_place => {
                                if ui
                                    .add(theme::primary_icon_button(
                                        theme::icons::RESTART,
                                        "Restart to Update",
                                    ))
                                    .clicked()
                                {
                                    restart = Some(path.clone());
                                }
                                if ui.add(theme::ghost_button("Later")).clicked() {
                                    close = true;
                                }
                            }
                            UpdateState::Ready { path, .. } => {
                                if ui
                                    .add(theme::primary_icon_button(
                                        theme::icons::FOLDER_OPEN,
                                        "Open Download Folder",
                                    ))
                                    .clicked()
                                {
                                    open_folder = Some(path.clone());
                                }
                                if ui.add(theme::ghost_button("Later")).clicked() {
                                    close = true;
                                }
                            }
                            UpdateState::Failed(_) => {
                                if self.update_info.is_some()
                                    && ui
                                        .add(theme::primary_icon_button(
                                            theme::icons::ROTATE,
                                            "Retry",
                                        ))
                                        .clicked()
                                {
                                    retry = true;
                                }
                                if ui.add(theme::ghost_button("Close")).clicked() {
                                    close = true;
                                }
                            }
                            _ => {}
                        }
                    });
                });
            });

        // Don't let a backdrop click or Esc cancel an in-flight download.
        if modal.should_close() && !matches!(self.update_state, UpdateState::Downloading { .. }) {
            close = true;
        }
        if download {
            self.start_update_download();
        }
        if skip {
            if let UpdateState::Available(info) = &self.update_state {
                self.update_skipped = Some(info.release.tag.clone());
            }
            self.update_state = UpdateState::Idle;
            close = true;
        }
        if retry {
            match self.update_info.take() {
                Some(info) => self.update_state = UpdateState::Available(info),
                None => close = true,
            }
        }
        if let Some(path) = restart {
            self.apply_update_and_close(ctx, &path);
        }
        if let Some(path) = open_folder {
            reveal_in_folder(&path);
        }
        if close {
            self.update_dialog_open = false;
        }
    }

    /// Spawn the downloaded replacement, which swaps itself in for the managed
    /// executable once this process exits, then close the window.
    fn apply_update_and_close(&mut self, ctx: &egui::Context, downloaded: &Path) {
        match swap_in_update(downloaded) {
            Ok(()) => {
                self.apply_started = true;
                self.update_dialog_open = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(err) => {
                self.update_state =
                    UpdateState::Failed(format!("Could not apply the update: {err}"));
            }
        }
    }

    /// Path of the downloaded update waiting for a restart, when this install
    /// applies updates in place.
    fn pending_apply_path(&self) -> Option<PathBuf> {
        if !updates_apply_in_place() {
            return None;
        }
        match &self.update_state {
            UpdateState::Ready { path, .. } => Some(path.clone()),
            _ => None,
        }
    }

    /// Refresh the installed-runtime cache unconditionally, right after an
    /// install or uninstall changed the state on disk.
    fn refresh_runtimes(&mut self) {
        self.runtimes_refreshed = Instant::now();
        let runtimes = self
            .java_store
            .as_ref()
            .map(|store| store.list())
            .unwrap_or_default();
        self.installed_runtimes = runtimes;
    }

    /// The installed-runtime cache is rescanned at most once per second so
    /// per-frame rendering never hits the disk.
    fn refresh_runtimes_if_stale(&mut self) {
        if self.runtimes_refreshed.elapsed() >= Duration::from_secs(1) {
            self.refresh_runtimes();
        }
    }

    /// The `java` command for a server: its own override if set and installed,
    /// then the app-wide runtime, otherwise the system PATH.
    fn java_for(&self, override_id: Option<&str>) -> String {
        resolve_java(
            &self.java_store,
            &self.installed_runtimes,
            self.app_settings.java_runtime.as_deref(),
            override_id,
        )
    }

    /// Short human label for the Java runtime a server will actually use.
    fn java_label_for(&self, override_id: Option<&str>) -> String {
        if let Some(id) = override_id {
            if let Some(runtime) = self
                .installed_runtimes
                .iter()
                .find(|runtime| runtime.id == id)
            {
                return format!("Java {} ({})", runtime.feature, runtime.release);
            }
            return "app default (selected runtime missing)".to_string();
        }
        self.java_label()
    }

    /// Short human label for the currently selected Java runtime.
    fn java_label(&self) -> String {
        if let Some(id) = &self.app_settings.java_runtime {
            if let Some(runtime) = self
                .installed_runtimes
                .iter()
                .find(|runtime| runtime.id.as_str() == id.as_str())
            {
                return format!("Java {} ({})", runtime.feature, runtime.release);
            }
        }
        "system Java (PATH)".to_string()
    }

    fn save_app_settings(&self) {
        if let Err(err) = self.app_settings.save(self.store.config_dir()) {
            eprintln!("failed to save app settings: {err}");
        }
    }

    /// App-wide settings modal. Currently: Java runtime management — download
    /// JDKs from the Adoptium API into Spoon's private directory, choose which
    /// one launches servers, or uninstall them again.
    fn render_app_settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.app_settings_open {
            return;
        }
        if self.java_dialog.is_none() {
            self.java_dialog = Some(JavaDialog::new());
        }
        let store_root = self
            .java_store
            .as_ref()
            .map(|store| store.root().display().to_string())
            .unwrap_or_default();
        let installed: Vec<JavaRuntime> = self.installed_runtimes.clone();
        let selected = self.app_settings.java_runtime.clone();

        let mut select: Option<Option<String>> = None;
        let mut remove: Option<String> = None;
        let mut install: Option<JavaCandidate> = None;
        let mut refetch = false;
        let mut close = false;

        let modal = egui::Modal::new(egui::Id::new("app_settings_dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(20)),
            )
            .backdrop_color(egui::Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                ui.label(theme::title_with_icon(
                    theme::icons::GEAR,
                    "App Settings",
                    theme::PRIMARY,
                ));
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Downloaded Java runtimes live in Spoon's own folder and are only used by Spoon.",
                    )
                    .size(13.0)
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(14.0);

                // --- Java runtime selector ---
                theme::section_title_with_icon(ui, theme::icons::JAVA, "JAVA RUNTIME");
                ui.add_space(4.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let system_selected = selected.is_none();
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if ui.add(egui::RadioButton::new(system_selected, "")).clicked() {
                            select = Some(None);
                        }
                        ui.label(
                            theme::icon(theme::icons::JAVA, 14.0).color(theme::TEXT_MUTED),
                        );
                        ui.label(egui::RichText::new("System Java (PATH)").color(theme::TEXT));
                    });
                    for runtime in &installed {
                        let is_selected = selected.as_deref() == Some(runtime.id.as_str());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if ui.add(egui::RadioButton::new(is_selected, "")).clicked() {
                                select = Some(Some(runtime.id.clone()));
                            }
                            ui.label(
                                theme::icon(theme::icons::JAVA, 14.0).color(theme::PRIMARY),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "Java {} · {}",
                                    runtime.feature, runtime.release
                                ))
                                .color(theme::TEXT),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                theme::icon(theme::icons::TRASH, 13.0)
                                                    .color(theme::ERROR),
                                            )
                                            .min_size(egui::vec2(28.0, 24.0)),
                                        )
                                        .on_hover_text("Uninstall this Java runtime")
                                        .clicked()
                                    {
                                        remove = Some(runtime.id.clone());
                                    }
                                },
                            );
                        });
                    }
                    if installed.is_empty() && !store_root.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(
                                "No private runtimes yet — download one below, or keep using the system Java.",
                            )
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                        );
                    }
                });
                ui.add_space(14.0);

                // --- Downloads ---
                theme::section_title_with_icon(ui, theme::icons::DOWNLOAD, "DOWNLOAD JAVA");
                ui.add_space(4.0);
                theme::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let dialog = self.java_dialog.as_mut().unwrap();
                    match &dialog.candidates {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Fetching available Java versions…");
                            });
                        }
                        Some(Err(err)) => {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(err).color(theme::ERROR));
                                if ui.add(theme::ghost_button("Retry")).clicked() {
                                    refetch = true;
                                }
                            });
                        }
                        Some(Ok(candidates)) => {
                            for candidate in candidates {
                                let is_installed = installed
                                    .iter()
                                    .any(|runtime| runtime.release == candidate.release);
                                let installing_label =
                                    format!("Java {}", candidate.feature);
                                let is_installing = dialog.installing.as_deref()
                                    == Some(installing_label.as_str());
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    ui.label(
                                        egui::RichText::new(format!("Java {}", candidate.feature))
                                            .strong()
                                            .color(theme::TEXT),
                                    );
                                    ui.label(
                                        egui::RichText::new(&candidate.release)
                                            .size(12.0)
                                            .color(theme::TEXT_MUTED),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if is_installing {
                                                let (done, total) = dialog.install_progress;
                                                ui.add(
                                                    egui::ProgressBar::new(if total > 0 {
                                                        done as f32 / total as f32
                                                    } else {
                                                        0.0
                                                    })
                                                    .desired_width(140.0)
                                                    .show_percentage(),
                                                );
                                            } else if is_installed {
                                                ui.add_enabled(
                                                    false,
                                                    theme::secondary_icon_button(
                                                        theme::icons::CHECK,
                                                        "Installed",
                                                    ),
                                                );
                                            } else {
                                                let download = egui::Button::new(
                                                    theme::icon(
                                                        theme::icons::DOWNLOAD,
                                                        13.0,
                                                    )
                                                    .color(theme::PRIMARY),
                                                )
                                                .min_size(egui::vec2(30.0, 26.0));
                                                if ui
                                                    .add_enabled(!dialog.busy(), download)
                                                    .on_hover_text(format!(
                                                        "Download Java {} (~{:.0} MB)",
                                                        candidate.feature,
                                                        candidate.size as f64 / 1_048_576.0
                                                    ))
                                                    .clicked()
                                                {
                                                    install = Some(candidate.clone());
                                                }
                                            }
                                        },
                                    );
                                });
                                ui.add_space(2.0);
                            }
                        }
                    }
                    if let Some(err) = &self.java_dialog.as_ref().unwrap().error {
                        ui.label(egui::RichText::new(err).color(theme::ERROR));
                    }
                    if !store_root.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(format!("Stored in {store_root}"))
                                .size(11.5)
                                .color(theme::TEXT_MUTED),
                        );
                    }
                });
                ui.add_space(14.0);

                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(theme::ghost_icon_button(theme::icons::XMARK, "Close"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });

        // Apply actions collected while the modal was open.
        if let Some(sel) = select {
            self.app_settings.java_runtime = sel;
            self.save_app_settings();
        }
        if let Some(id) = remove {
            if let Some(store) = &self.java_store {
                match store.uninstall(&id) {
                    Ok(()) => {
                        self.refresh_runtimes();
                        if self.app_settings.java_runtime.as_deref() == Some(id.as_str()) {
                            self.app_settings.java_runtime = None;
                            self.save_app_settings();
                        }
                    }
                    Err(err) => {
                        if let Some(dialog) = &mut self.java_dialog {
                            dialog.error = Some(err);
                        }
                    }
                }
            }
        }
        if let Some(candidate) = install {
            if let (Some(store), Some(dialog)) = (&self.java_store, &mut self.java_dialog) {
                dialog.start_install(store.clone(), candidate);
            }
        }
        if refetch {
            if let Some(dialog) = &mut self.java_dialog {
                dialog.refetch();
            }
        }

        let mut auto_select: Option<String> = None;
        let installed_now = if let Some(dialog) = &mut self.java_dialog {
            let finished = match dialog.poll() {
                Some(Ok(runtime)) => Some(runtime.id),
                _ => None,
            };
            if dialog.busy() || dialog.candidates_rx.is_some() {
                ctx.request_repaint();
            }
            finished
        } else {
            None
        };
        if installed_now.is_some() {
            self.refresh_runtimes();
            // First download selects itself as the app-wide runtime.
            if self.app_settings.java_runtime.is_none() {
                auto_select = installed_now;
            }
        }
        if let Some(id) = auto_select {
            if self
                .installed_runtimes
                .iter()
                .any(|runtime| runtime.id == id)
            {
                self.app_settings.java_runtime = Some(id);
                self.save_app_settings();
            }
        }

        let busy = self.java_dialog.as_ref().is_some_and(JavaDialog::busy);
        if (modal.should_close() || close) && !busy {
            self.app_settings_open = false;
            self.java_dialog = None;
        }
    }

    /// Change-version modal opened from the Overview software box: pick a new
    /// software or version and install it without leaving the page.
    fn render_version_dialog(&mut self, ctx: &egui::Context) {
        let Some(index) = self.version_modal else {
            return;
        };
        if index >= self.servers.len() {
            self.version_modal = None;
            self.settings_dialog = None;
            return;
        }
        let server_name = self.servers[index].config.name.clone();
        let mut close = false;
        let mut install_clicked = false;
        let installing = self
            .settings_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.version_task.is_some());
        let modal = egui::Modal::new(egui::Id::new("change_version_dialog"))
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(20)),
            )
            .backdrop_color(egui::Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(theme::title_with_icon(
                    theme::icons::ROTATE,
                    "Change Version",
                    theme::PRIMARY,
                ));
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(format!(
                        "Pick a new software or version for {server_name}."
                    ))
                    .size(13.0)
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(14.0);
                self.settings_dialog
                    .as_mut()
                    .unwrap()
                    .sync_version(&self.software);
                install_clicked = self.render_software_version_card(ui, index);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if installing {
                            ui.add_enabled(false, theme::secondary_button("Installing…"));
                        }
                        if ui
                            .add_enabled(
                                !installing,
                                theme::ghost_icon_button(theme::icons::XMARK, "Close"),
                            )
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });

        // Don't let a backdrop click or Esc discard an in-flight install.
        if modal.should_close() && !installing {
            close = true;
        }

        if install_clicked {
            let config = self.servers[index].config.clone();
            self.settings_dialog
                .as_mut()
                .unwrap()
                .start_version_change(config);
        }

        if let Some(result) = self
            .settings_dialog
            .as_mut()
            .and_then(SettingsDialog::take_version_result)
        {
            match result {
                Ok((server_type, version, warning)) => {
                    let entry = &mut self.servers[index];
                    entry.config.server_type = Some(server_type.api_name());
                    entry.config.server_version = Some(version);
                    entry.last_error = warning;
                    self.save();
                    close = true;
                }
                Err(err) => {
                    if let Some(dialog) = &mut self.settings_dialog {
                        dialog.version_error = Some(err);
                    }
                }
            }
        }

        if self
            .settings_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.version_task.is_some())
        {
            ctx.request_repaint();
        }

        if close {
            self.version_modal = None;
            self.settings_dialog = None;
        }
    }

    /// The "Software & Version" picker card, shared by the settings page and the
    /// change-version modal. Returns true when "Install selected version" was hit.
    fn render_software_version_card(&mut self, ui: &mut egui::Ui, server_index: usize) -> bool {
        let server_id = self.servers[server_index].config.id.clone();
        let running = self.servers[server_index].state() != ServerState::Stopped;
        let dialog = self.settings_dialog.as_mut().unwrap();
        let mut install_clicked = false;
        theme::card().show(ui, |ui| {
            ui.add_enabled_ui(!running, |ui| {
                // Software picker: scrollable icon list.
                theme::section_title(ui, "SOFTWARE");
                ui.add_space(4.0);
                if let Some(server_type) = software_picker(
                    ui,
                    &self.software,
                    &self.icon_textures,
                    ("software_picker", &server_id),
                    dialog.selected_type.as_ref(),
                ) {
                    dialog.select_type(server_type, &self.software);
                }

                ui.add_space(10.0);
                theme::section_title(ui, "VERSION");
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Current:")
                        .on_hover_text("Recorded software version");
                    ui.label(
                        self.servers[server_index]
                            .config
                            .server_version
                            .as_deref()
                            .unwrap_or("Unknown / imported"),
                    );
                });
                ui.add_space(4.0);
                version_picker(
                    ui,
                    &self.software,
                    ("version_picker", &server_id),
                    dialog.selected_type.as_ref(),
                    &mut dialog.selected_version,
                );
            });
            if let Some(err) = &dialog.version_error {
                ui.label(egui::RichText::new(err).color(theme::ERROR));
            }
            if dialog.version_task.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Installing selected server version…");
                });
            } else if ui
                .add_enabled(
                    !running
                        && dialog.selected_type.is_some()
                        && dialog.selected_version.is_some()
                        && dialog
                            .selected_type
                            .as_ref()
                            .is_some_and(|t| !self.software.versions(t).is_empty()),
                    theme::primary_icon_button(theme::icons::DOWNLOAD, "Install selected version"),
                )
                .clicked()
            {
                install_clicked = true;
            }
        });
        install_clicked
    }

    fn render_settings_page(&mut self, ui: &mut egui::Ui, index: usize) {
        let server_index = index;
        let server_id = self.servers[server_index].config.id.clone();
        // Recreate the page state if it belongs to a different server.
        let stale = self
            .settings_dialog
            .as_ref()
            .map(|dialog| dialog.server_id != server_id)
            .unwrap_or(true);
        if stale {
            self.settings_dialog = Some(SettingsDialog::new(&self.servers[server_index].config));
        }
        let running = self.servers[server_index].state() != ServerState::Stopped;
        let app_java_label = self.java_label();
        let java_override = self.servers[server_index].config.java_runtime.clone();
        let effective_java_label = self.java_label_for(java_override.as_deref());
        let installed = self.installed_runtimes.clone();
        let mut close = false;
        let mut save_settings = false;
        let mut should_save = false;
        self.settings_dialog
            .as_mut()
            .unwrap()
            .sync_version(&self.software);

        ui.label(theme::title_with_icon(
            theme::icons::GEAR,
            "Server Settings",
            theme::PRIMARY,
        ));
        ui.add_space(14.0);
        let working_dir = self.servers[server_index].config.working_dir.clone();
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 70.0).max(240.0))
            .show(ui, |ui| {
                // Two columns on wide windows: launch settings on the left,
                // public-facing appearance settings on the right.
                let column_count = if ui.available_width() >= 760.0 { 2 } else { 1 };
                ui.columns(column_count, |columns| {
                    {
                        let ui = &mut columns[0];
                        theme::section_title_with_icon(ui, theme::icons::SLIDERS, "General");
                        ui.add_space(4.0);
                        render_settings_name_card(ui, self.settings_dialog.as_mut().unwrap());
                        ui.add_space(14.0);

                        theme::section_title_with_icon(ui, theme::icons::JAVA, "Java");
                        ui.add_space(4.0);
                        render_settings_java_card(
                            ui,
                            self.settings_dialog.as_mut().unwrap(),
                            running,
                            &app_java_label,
                            &installed,
                        );
                        ui.add_space(14.0);

                        theme::section_title_with_icon(ui, theme::icons::MEMORY, "Memory");
                        ui.add_space(4.0);
                        let usage = format_mb(self.servers[server_index].memory_bytes);
                        render_settings_memory_card(
                            ui,
                            self.settings_dialog.as_mut().unwrap(),
                            running,
                            &usage,
                            &effective_java_label,
                        );
                    }
                    {
                        let ui = &mut columns[column_count - 1];
                        theme::section_title_with_icon(ui, theme::icons::COMMENTS, "MOTD");
                        ui.add_space(4.0);
                        theme::card().show(ui, |ui| {
                            ui.add_enabled_ui(!running, |ui| {
                                render_motd_editor(ui, self.settings_dialog.as_mut().unwrap());
                            });
                        });
                        ui.add_space(14.0);

                        theme::section_title_with_icon(ui, theme::icons::GLOBE, "Resource Pack");
                        ui.add_space(4.0);
                        theme::card().show(ui, |ui| {
                            ui.add_enabled_ui(!running, |ui| {
                                render_resource_pack_field(
                                    ui,
                                    self.settings_dialog.as_mut().unwrap(),
                                );
                            });
                        });
                        ui.add_space(14.0);

                        theme::section_title_with_icon(ui, theme::icons::IMAGE, "Server Icon");
                        ui.add_space(4.0);
                        theme::card().show(ui, |ui| {
                            ui.add_enabled_ui(!running, |ui| {
                                render_icon_selector(
                                    ui,
                                    &ctx,
                                    self.settings_dialog.as_mut().unwrap(),
                                    &working_dir,
                                );
                            });
                        });
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new("Applied on next start")
                                .size(12.0)
                                .color(theme::TEXT_MUTED),
                        );
                    }
                });
                ui.add_space(14.0);

                if running {
                    ui.label(
                        egui::RichText::new("Stop the server to change settings.")
                            .color(theme::TEXT_MUTED),
                    );
                }
            });
        ui.add_space(14.0);
        let saved_until = self
            .settings_dialog
            .as_ref()
            .and_then(|dialog| dialog.saved_until)
            .filter(|until| *until > Instant::now());
        if let Some(until) = saved_until {
            ui.ctx()
                .request_repaint_after(until.saturating_duration_since(Instant::now()));
        }
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        !running,
                        theme::primary_icon_button(
                            if saved_until.is_some() {
                                theme::icons::CHECK
                            } else {
                                theme::icons::SAVE
                            },
                            if saved_until.is_some() {
                                "Saved"
                            } else {
                                "Save settings"
                            },
                        ),
                    )
                    .clicked()
                {
                    save_settings = true;
                }
                if ui
                    .add(theme::ghost_icon_button(theme::icons::XMARK, "Close"))
                    .clicked()
                {
                    close = true;
                }
            });
        });
        let dialog = self.settings_dialog.as_mut().unwrap();

        if save_settings {
            if dialog.name.trim().is_empty() {
                dialog.name_error = Some("Server name is required.".into());
            } else {
                let mut appearance_ok = true;
                if !valid_resource_pack_url(&dialog.resource_pack) {
                    dialog.resource_pack_error =
                        Some("Enter a valid http(s) URL or leave it empty.".into());
                    appearance_ok = false;
                }
                let working_dir = self.servers[server_index].config.working_dir.clone();
                if appearance_ok {
                    if let Err(message) = apply_icon_choice(&working_dir, &dialog.icon) {
                        dialog.icon_error = Some(message);
                        appearance_ok = false;
                    }
                }
                if appearance_ok {
                    let mut entries = Vec::new();
                    if !dialog.motd.is_empty() || dialog.had_motd {
                        entries.push(("motd".to_string(), dialog.motd.clone()));
                    }
                    if !dialog.resource_pack.is_empty() || dialog.had_resource_pack {
                        entries.push(("resource-pack".to_string(), dialog.resource_pack.clone()));
                    }
                    if !entries.is_empty() {
                        if let Err(err) =
                            set_properties(&working_dir.join("server.properties"), &entries)
                        {
                            dialog.resource_pack_error =
                                Some(format!("Could not write server.properties: {err}"));
                            appearance_ok = false;
                        }
                    }
                }
                if !appearance_ok {
                    return;
                }
                let entry = &mut self.servers[server_index];
                entry.config.name = dialog.name.trim().to_string();
                entry.config.max_memory_mb = dialog.max_memory_mb;
                entry.config.min_memory_mb = dialog.min_memory_mb.min(dialog.max_memory_mb);
                let chosen_java = dialog.java_runtime.clone();
                entry.config.java_runtime = match (&chosen_java, &self.java_store) {
                    (Some(id), Some(_)) if !installed.iter().any(|r| r.id == *id) => None,
                    _ => chosen_java,
                };
                should_save = true;
                // Stay on the settings page; re-baseline the draft against what
                // was just written so follow-up saves behave like the first.
                let dialog = self.settings_dialog.as_mut().unwrap();
                dialog.had_motd = true;
                dialog.had_resource_pack = true;
                dialog.has_icon_file = match &dialog.icon {
                    IconChoice::None => false,
                    IconChoice::Keep => dialog.has_icon_file,
                    IconChoice::Preset(_) | IconChoice::Custom(_) => true,
                };
                dialog.icon_previews.remove("current");
                match matching_preset_icon(&working_dir) {
                    Ok(preset) => dialog.current_icon_preset = preset,
                    Err(err) => dialog.icon_error = Some(err),
                }
                dialog.icon = IconChoice::Keep;
                dialog.saved_until = Some(Instant::now() + Duration::from_secs(2));
                ui.ctx().request_repaint();
            }
        }

        if close {
            self.settings_dialog = None;
            self.page = ServerPage::Overview;
            ui.ctx().request_repaint();
        }
        if should_save {
            self.save();
        }
    }

    fn render_files_page(&mut self, ui: &mut egui::Ui, index: usize) {
        let server_index = index;
        let server_id = self.servers[server_index].config.id.clone();
        let server_dir = self.servers[server_index].config.working_dir.clone();
        let server_name = self.servers[server_index].config.name.clone();
        // Recreate the page state if it belongs to a different server.
        let stale = self
            .files_dialog
            .as_ref()
            .map(|dialog| dialog.server_id != server_id)
            .unwrap_or(true);
        if stale {
            self.files_dialog = Some(FilesDialog::new(&self.servers[server_index].config));
        }
        let mut close = false;
        let mut save_file = false;
        let mut should_save = false;

        ui.label(theme::title_with_icon(
            theme::icons::FOLDER,
            "Files",
            theme::PRIMARY,
        ));
        ui.add_space(2.0);
        let breadcrumbs = {
            let dialog = self.files_dialog.as_ref().unwrap();
            file_path_breadcrumbs(&dialog.root, &dialog.current, &server_name)
        };
        let mut breadcrumb_target = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding = egui::vec2(2.0, 0.0);
            ui.label(egui::RichText::new("/").size(13.0).color(theme::TEXT_MUTED));
            for (index, (path, name)) in breadcrumbs.iter().enumerate() {
                if index > 0 {
                    ui.label(egui::RichText::new("/").size(13.0).color(theme::TEXT_MUTED));
                }
                if index + 1 == breadcrumbs.len() {
                    ui.label(
                        egui::RichText::new(name)
                            .size(13.0)
                            .color(theme::TEXT_MUTED),
                    );
                } else {
                    let button = theme::animated_button(
                        egui::Button::new(
                            egui::RichText::new(name.as_str())
                                .size(13.0)
                                .color(theme::TEXT),
                        ),
                        egui::Color32::TRANSPARENT,
                        theme::SURFACE_HOVER,
                        egui::Stroke::NONE,
                        egui::Stroke::NONE,
                    );
                    if ui
                        .add(button)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        breadcrumb_target = Some(path.clone());
                    }
                }
            }
        });
        if let Some(target) = breadcrumb_target {
            self.files_dialog.as_mut().unwrap().navigate_to(&target);
        }
        ui.add_space(14.0);
        // Both panes take the height left above the footer.
        let body_height = (ui.available_height() - 54.0).max(240.0);
        ui.horizontal(|ui| {
            // Left pane: folder listing and file operations.
            ui.vertical(|ui| {
                ui.set_width(340.0);
                ui.set_height(body_height);
                let dialog = self.files_dialog.as_mut().unwrap();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if ui
                        .add(icon_toolbar_button(theme::icons::ROTATE))
                        .on_hover_text("Refresh")
                        .clicked()
                    {
                        dialog.refresh();
                    }
                    if ui
                        .add(icon_toolbar_button(theme::icons::FOLDER_OPEN))
                        .on_hover_text("Open server folder")
                        .clicked()
                    {
                        open_directory(&server_dir);
                    }
                    if ui
                        .add(icon_toolbar_button(theme::icons::FILE_CIRCLE_PLUS))
                        .on_hover_text("New file")
                        .clicked()
                    {
                        dialog.pending = Some(PendingFileOp::NewFile);
                        dialog.pending_name.clear();
                        dialog.confirm_delete = None;
                    }
                    if ui
                        .add(icon_toolbar_button(theme::icons::FOLDER_PLUS))
                        .on_hover_text("New folder")
                        .clicked()
                    {
                        dialog.pending = Some(PendingFileOp::NewFolder);
                        dialog.pending_name.clear();
                        dialog.confirm_delete = None;
                    }
                    let has_selection = dialog.selected.is_some();
                    if ui
                        .add_enabled(has_selection, icon_toolbar_button(theme::icons::PEN))
                        .on_hover_text("Rename")
                        .clicked()
                    {
                        if let Some(path) = dialog.selected.clone() {
                            dialog.pending_name = path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            dialog.pending = Some(PendingFileOp::Rename(path));
                            dialog.confirm_delete = None;
                        }
                    }
                    let can_delete = has_selection || !dialog.checked.is_empty();
                    if ui
                        .add_enabled(can_delete, icon_toolbar_button(theme::icons::TRASH))
                        .on_hover_text("Delete")
                        .clicked()
                    {
                        let targets: Vec<PathBuf> = if dialog.checked.is_empty() {
                            dialog.selected.iter().cloned().collect()
                        } else {
                            dialog.checked.iter().cloned().collect()
                        };
                        dialog.confirm_delete = Some(targets);
                        dialog.pending = None;
                    }
                });
                dialog.poll_compress();
                if dialog.compress_task.is_some() {
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                if !dialog.checked.is_empty() {
                    let busy = dialog.compress_task.is_some();
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(
                            egui::RichText::new(format!("{} selected", dialog.checked.len()))
                                .color(theme::TEXT_MUTED),
                        );
                        if busy {
                            ui.spinner();
                            ui.label(egui::RichText::new("Compressing…").color(theme::TEXT_MUTED));
                        }
                        ui.add_enabled_ui(!busy, |ui| {
                            if ui
                                .add(theme::primary_icon_button(
                                    theme::icons::FILE_ZIPPER,
                                    "Compress",
                                ))
                                .clicked()
                            {
                                dialog.compress_checked();
                            }
                            if ui
                                .add(theme::ghost_icon_button(theme::icons::XMARK, "Clear"))
                                .clicked()
                            {
                                dialog.checked.clear();
                            }
                        });
                    });
                }
                if let Some(status) = &dialog.status {
                    ui.label(
                        egui::RichText::new(status)
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                    );
                }
                if let Some(err) = &dialog.error {
                    ui.label(egui::RichText::new(err).color(theme::ERROR));
                }
                if dialog.pending.is_some() {
                    let confirm_label = match dialog.pending {
                        Some(PendingFileOp::Rename(_)) => "Rename",
                        _ => "Create",
                    };
                    theme::card().show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Name")
                                .size(12.0)
                                .color(theme::TEXT_MUTED),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.pending_name)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .add(theme::primary_icon_button(
                                    theme::icons::CHECK,
                                    confirm_label,
                                ))
                                .clicked()
                            {
                                match dialog.pending.clone() {
                                    Some(PendingFileOp::NewFile) => dialog.create_entry(false),
                                    Some(PendingFileOp::NewFolder) => dialog.create_entry(true),
                                    Some(PendingFileOp::Rename(from)) => dialog.rename_entry(&from),
                                    None => {}
                                }
                            }
                            if ui
                                .add(theme::ghost_icon_button(theme::icons::XMARK, "Cancel"))
                                .clicked()
                            {
                                dialog.pending = None;
                            }
                        });
                    });
                }
                if let Some(targets) = dialog.confirm_delete.clone() {
                    let prompt = match targets.as_slice() {
                        [only] => format!(
                            "Delete {}?",
                            only.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ),
                        _ => format!("Delete {} items?", targets.len()),
                    };
                    theme::card().show(ui, |ui| {
                        ui.label(egui::RichText::new(prompt).color(theme::ERROR));
                        ui.horizontal(|ui| {
                            if ui
                                .add(theme::danger_icon_button(theme::icons::TRASH, "Delete"))
                                .clicked()
                            {
                                dialog.delete_entries(&targets);
                            }
                            if ui
                                .add(theme::ghost_icon_button(theme::icons::XMARK, "Cancel"))
                                .clicked()
                            {
                                dialog.confirm_delete = None;
                            }
                        });
                    });
                }
                ui.add_space(4.0);
                if !dialog.entries.is_empty() {
                    let all_checked = dialog
                        .entries
                        .iter()
                        .all(|entry| dialog.checked.contains(&entry.path));
                    let mut select_all = all_checked;
                    if ui.checkbox(&mut select_all, "Select all").changed() {
                        if select_all {
                            let paths: Vec<PathBuf> =
                                dialog.entries.iter().map(|e| e.path.clone()).collect();
                            dialog.checked.extend(paths);
                        } else {
                            dialog.checked.clear();
                        }
                    }
                }
                egui::ScrollArea::vertical()
                    .id_salt(("files_list", &server_id))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Take the listing out so rows don't clone it every
                        // frame; it is restored below, or replaced when a
                        // folder click triggers navigation.
                        let entries = std::mem::take(&mut dialog.entries);
                        let mut navigate: Option<PathBuf> = None;
                        for entry in &entries {
                            let is_selected =
                                dialog.selected.as_deref() == Some(entry.path.as_path());
                            let glyph = if entry.is_dir {
                                theme::icons::FOLDER
                            } else {
                                theme::icons::FILE_LINES
                            };
                            let job = theme::icon_text_job(glyph, &entry.name, theme::TEXT);
                            let mut clicked_dir = false;
                            ui.horizontal(|ui| {
                                let mut ticked = dialog.checked.contains(&entry.path);
                                if ui.checkbox(&mut ticked, "").changed() {
                                    if ticked {
                                        dialog.checked.insert(entry.path.clone());
                                    } else {
                                        dialog.checked.remove(&entry.path);
                                    }
                                }
                                if ui.selectable_label(is_selected, job).clicked() {
                                    if entry.is_dir {
                                        clicked_dir = true;
                                    } else {
                                        dialog.selected = Some(entry.path.clone());
                                        dialog.open_file(&entry.path);
                                    }
                                }
                            });
                            if clicked_dir {
                                navigate = Some(entry.path.clone());
                                break;
                            }
                        }
                        match navigate {
                            Some(dir) => dialog.navigate_to(&dir),
                            None => dialog.entries = entries,
                        }
                    });
            });
            // Right pane: text editor for the open file.
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.set_height(body_height);
                let dialog = self.files_dialog.as_mut().unwrap();
                if let Some(open) = &mut dialog.open {
                    let file_name = open
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dirty = open.content != open.saved_content;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(theme::icon(theme::icons::FILE_LINES, 14.0).color(theme::PRIMARY));
                        ui.label(egui::RichText::new(file_name).strong());
                        if dirty {
                            ui.label(egui::RichText::new("●").color(theme::PRIMARY));
                        }
                        if open.read_only {
                            ui.label(
                                egui::RichText::new("read-only")
                                    .size(11.0)
                                    .color(theme::TEXT_MUTED),
                            );
                        }
                        if let Some(status) = &dialog.status {
                            ui.label(
                                egui::RichText::new(status)
                                    .size(11.0)
                                    .color(theme::TEXT_MUTED),
                            );
                        }
                    });
                    if let Some(note) = &open.note {
                        ui.label(egui::RichText::new(note).color(theme::TEXT_MUTED));
                    }
                    ui.add_space(4.0);
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
                        if let Some(search) = &mut open.search {
                            search.focus = true;
                        }
                    }
                    let mut matches: Vec<(usize, usize)> = Vec::new();
                    let mut jump = false;
                    if let Some(search) = &mut open.search {
                        matches = find_matches(&open.content, &search.query);
                        ui.horizontal(|ui| {
                            let edit = ui.add(
                                egui::TextEdit::singleline(&mut search.query)
                                    .hint_text("Find in file")
                                    .desired_width(220.0),
                            );
                            if search.focus {
                                edit.request_focus();
                                search.focus = false;
                            }
                            let (enter, shift, escape) = ui.input(|i| {
                                (
                                    i.key_pressed(egui::Key::Enter),
                                    i.modifiers.shift,
                                    i.key_pressed(egui::Key::Escape),
                                )
                            });
                            let len = matches.len();
                            let mut step_next = false;
                            let mut step_prev = false;
                            if edit.changed() {
                                search.current = 0;
                                matches = find_matches(&open.content, &search.query);
                                jump = true;
                            }
                            if edit.lost_focus() && enter {
                                step_next = !shift;
                                step_prev = shift;
                                edit.request_focus();
                            }
                            if edit.has_focus() && escape && !search.query.is_empty() {
                                search.query.clear();
                                search.current = 0;
                                matches.clear();
                            }
                            if ui.small_button("Prev").clicked() {
                                step_prev = true;
                            }
                            if ui.small_button("Next").clicked() {
                                step_next = true;
                            }
                            if len > 0 {
                                if step_next {
                                    search.current = (search.current + 1) % len;
                                    jump = true;
                                } else if step_prev {
                                    search.current = (search.current + len - 1) % len;
                                    jump = true;
                                }
                            }
                            if !search.query.is_empty() {
                                let label = if matches.is_empty() {
                                    "No results".to_string()
                                } else {
                                    format!(
                                        "{}/{}",
                                        search.current.min(matches.len() - 1) + 1,
                                        matches.len()
                                    )
                                };
                                ui.label(egui::RichText::new(label).color(theme::TEXT_MUTED));
                            }
                        });
                        if !matches.is_empty() {
                            search.current = search.current.min(matches.len() - 1);
                        }
                        ui.add_space(4.0);
                    }
                    let current = open.search.as_ref().map(|search| search.current);
                    ui.add_enabled_ui(!open.read_only, |ui| {
                        let height = ui.available_height().max(120.0);
                        // Highlight known formats; very large files stay plain
                        // to keep typing responsive.
                        let language = open.language.filter(|_| open.content.len() <= 512 * 1024);
                        let match_ranges = &matches;
                        let mut layouter = move |ui: &egui::Ui, text: &str, wrap_width: f32| {
                            let job = syntax_job(ui, text, language, wrap_width);
                            let job = highlight_matches(job, text, match_ranges, current);
                            ui.fonts(|fonts| fonts.layout_job(job))
                        };
                        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
                        let rows = ((height / row_height) as usize).max(4);
                        egui::ScrollArea::vertical()
                            .id_salt("file_editor_scroll")
                            .auto_shrink([false, false])
                            .max_height(height)
                            .show(ui, |ui| {
                                let output = egui::TextEdit::multiline(&mut open.content)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY)
                                    .desired_rows(rows)
                                    .layouter(&mut layouter)
                                    .show(ui);
                                if jump {
                                    if let Some(&(start, _)) = current.and_then(|c| matches.get(c))
                                    {
                                        let index = open.content[..start].chars().count();
                                        let rect = output
                                            .galley
                                            .pos_from_ccursor(egui::text::CCursor::new(index))
                                            .translate(output.galley_pos.to_vec2());
                                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                    }
                                }
                            });
                    });
                } else {
                    ui.label(
                        egui::RichText::new("Select a file to view or edit.")
                            .color(theme::TEXT_MUTED),
                    );
                }
            });
        });
        // Footer pinned to the bottom of the page.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dialog = self.files_dialog.as_mut().unwrap();
                    if dialog.confirm_close {
                        if ui
                            .add(theme::danger_icon_button(
                                theme::icons::TRASH,
                                "Discard & Close",
                            ))
                            .clicked()
                        {
                            close = true;
                        }
                        if ui
                            .add(theme::ghost_icon_button(theme::icons::XMARK, "Cancel"))
                            .clicked()
                        {
                            dialog.confirm_close = false;
                        }
                    } else {
                        let can_save = dialog.open.as_ref().is_some_and(|open| {
                            !open.read_only && open.content != open.saved_content
                        });
                        if ui
                            .add_enabled(
                                can_save,
                                theme::primary_icon_button(theme::icons::SAVE, "Save"),
                            )
                            .clicked()
                        {
                            save_file = true;
                        }
                        if ui
                            .add(theme::ghost_icon_button(theme::icons::XMARK, "Close"))
                            .clicked()
                        {
                            if dialog.open_is_dirty() {
                                dialog.confirm_close = true;
                            } else {
                                close = true;
                            }
                        }
                    }
                });
            });
        });

        if save_file {
            let mut saved_properties = false;
            let dialog = self.files_dialog.as_mut().unwrap();
            if let Some(open) = dialog.open.as_mut() {
                match fs::write(&open.path, &open.content) {
                    Ok(()) => {
                        open.saved_content = open.content.clone();
                        saved_properties = open.path == dialog.root.join("server.properties");
                        dialog.status = Some("Saved.".to_string());
                    }
                    Err(err) => dialog.error = Some(format!("Could not save file: {err}")),
                }
            }
            if saved_properties {
                let entry = &mut self.servers[server_index];
                entry.max_players = read_max_players(&entry.config.working_dir);
                should_save = true;
            }
        }

        if close {
            self.files_dialog = None;
            self.page = ServerPage::Overview;
            ui.ctx().request_repaint();
        }
        if should_save {
            self.save();
        }
    }

    /// Backups page: snapshot the server's folder into Spoon's backups
    /// directory, browse past snapshots and delete ones no longer needed.
    fn render_backups_page(&mut self, ui: &mut egui::Ui, index: usize) {
        let server_index = index;
        let server_id = self.servers[server_index].config.id.clone();
        // Recreate the page state if it belongs to a different server.
        let stale = self
            .backups_dialog
            .as_ref()
            .map(|dialog| dialog.server_id != server_id)
            .unwrap_or(true);
        if stale {
            self.backups_dialog = self
                .backup_store
                .as_ref()
                .map(|store| BackupsDialog::new(&self.servers[server_index].config, store.clone()));
        }

        ui.label(theme::title_with_icon(
            theme::icons::ARCHIVE,
            "Backups",
            theme::PRIMARY,
        ));
        ui.add_space(14.0);

        let Some(dialog) = self.backups_dialog.as_mut() else {
            ui.label(
                egui::RichText::new("Spoon could not determine its backups folder.")
                    .color(theme::ERROR),
            );
            return;
        };

        if dialog.poll() {
            // The server folder changed; re-read state Spoon caches from its files.
            self.servers[server_index].max_players = read_max_players(&dialog.working_dir);
        }
        dialog.poll_status(ui.ctx());
        let running = self.servers[server_index].state() != ServerState::Stopped;
        let busy = dialog.busy();
        let mut create = false;
        let mut open_folder = false;
        let mut delete: Option<String> = None;
        let mut cancel_delete = false;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ui
                .add_enabled(
                    !running && !busy,
                    theme::primary_icon_button(theme::icons::ARCHIVE, "Create backup"),
                )
                .on_hover_text("Zip the server folder into Spoon's backups folder")
                .clicked()
            {
                create = true;
            }
            if busy {
                ui.spinner();
                let label = if dialog.create_task.is_some() {
                    "Creating backup…"
                } else {
                    "Restoring backup…"
                };
                ui.label(egui::RichText::new(label).color(theme::TEXT_MUTED));
            } else if running {
                ui.label(
                    egui::RichText::new("Stop the server to create or restore backups.")
                        .color(theme::TEXT_MUTED),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(theme::ghost_icon_button(
                        theme::icons::FOLDER_OPEN,
                        "Open Folder",
                    ))
                    .on_hover_text("Reveal this server's backups in the file manager")
                    .clicked()
                {
                    open_folder = true;
                }
                if ui
                    .add(icon_toolbar_button(theme::icons::ROTATE))
                    .on_hover_text("Refresh the list")
                    .clicked()
                {
                    dialog.refresh();
                }
            });
        });
        if busy {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        if let Some(err) = &dialog.error {
            ui.label(egui::RichText::new(err).color(theme::ERROR));
        }
        if let Some(status) = &dialog.status {
            ui.label(egui::RichText::new(status).color(theme::TEXT_MUTED));
        }
        ui.add_space(4.0);

        if let Some(name) = dialog.confirm_delete.clone() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Delete backup \"{name}\"?"))
                            .color(theme::ERROR),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(theme::danger_icon_button(theme::icons::TRASH, "Delete"))
                            .clicked()
                        {
                            delete = Some(name.clone());
                        }
                        if ui
                            .add(theme::ghost_icon_button(theme::icons::XMARK, "Cancel"))
                            .clicked()
                        {
                            cancel_delete = true;
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }

        // Take the listing out so row clicks can mutate dialog state; it is
        // restored below (mirrors the files page).
        let backups = std::mem::take(&mut dialog.backups);
        let empty = backups.is_empty();
        egui::ScrollArea::vertical()
            .id_salt(("backups_list", &server_id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if empty {
                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                theme::icon(theme::icons::ARCHIVE, 28.0).color(theme::TEXT_MUTED),
                            );
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new("No backups yet")
                                    .size(16.0)
                                    .strong()
                                    .color(theme::TEXT),
                            );
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(
                                    "Create a backup to snapshot this server's files",
                                )
                                .size(12.5)
                                .color(theme::TEXT_MUTED),
                            );
                        });
                        ui.add_space(12.0);
                    });
                } else {
                    theme::card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (position, backup) in backups.iter().enumerate() {
                            if position > 0 {
                                ui.separator();
                            }
                            let created = chrono::DateTime::<chrono::Local>::from(backup.created)
                                .format("%Y-%m-%d %H:%M");
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                ui.label(
                                    theme::icon(theme::icons::ARCHIVE, 15.0).color(theme::PRIMARY),
                                );
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(egui::RichText::new(&backup.name).color(theme::TEXT));
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{created}  ·  {}",
                                            format_mb(backup.size_bytes)
                                        ))
                                        .size(12.0)
                                        .color(theme::TEXT_MUTED),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(egui::Button::new(
                                                theme::icon(theme::icons::TRASH, 13.0)
                                                    .color(theme::ERROR),
                                            ))
                                            .on_hover_text("Delete this backup")
                                            .clicked()
                                        {
                                            dialog.confirm_delete = Some(backup.name.clone());
                                        }
                                        if ui
                                            .add_enabled(
                                                !running && !busy,
                                                egui::Button::new(
                                                    theme::icon(theme::icons::RESTORE, 13.0)
                                                        .color(theme::PRIMARY),
                                                ),
                                            )
                                            .on_hover_text(
                                                "Restore this backup into the server folder",
                                            )
                                            .clicked()
                                        {
                                            dialog.confirm_restore = Some(backup.name.clone());
                                        }
                                    },
                                );
                            });
                        }
                    });
                }
            });
        dialog.backups = backups;

        if create {
            dialog.start_create();
        }
        if let Some(name) = delete {
            dialog.delete_backup(&name);
        }
        if cancel_delete {
            dialog.confirm_delete = None;
        }
        if open_folder {
            reveal_in_folder(&dialog.store.server_dir(&dialog.server_id));
        }

        // Restore confirmation: the user picks whether the server folder is
        // wiped before the archive is extracted.
        if let Some(name) = dialog.confirm_restore.clone() {
            let mut wipe: Option<bool> = None;
            let mut close = false;
            let busy = dialog.busy();
            let modal = egui::Modal::new(egui::Id::new(("restore_backup_dialog", &server_id)))
                .frame(
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                        .corner_radius(egui::CornerRadius::same(12))
                        .inner_margin(egui::Margin::same(20)),
                )
                .backdrop_color(egui::Color32::from_black_alpha(140))
                .show(ui.ctx(), |ui| {
                    ui.set_width(460.0);
                    ui.label(theme::title_with_icon(
                        theme::icons::RESTORE,
                        "Restore Backup",
                        theme::PRIMARY,
                    ));
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Restore \"{name}\" into \"{}\"?",
                            dialog.server_name
                        ))
                        .size(13.0)
                        .color(theme::TEXT_MUTED),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Restore next to the existing files, or replace everything in the server folder first. Restoring overwrites files with the same name.",
                        )
                        .size(12.5)
                        .color(theme::TEXT_MUTED),
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(theme::ghost_icon_button(theme::icons::XMARK, "Cancel"))
                                .clicked()
                            {
                                close = true;
                            }
                            if ui
                                .add_enabled(
                                    !busy,
                                    theme::secondary_icon_button(
                                        theme::icons::RESTORE,
                                        "Keep files & restore",
                                    ),
                                )
                                .on_hover_text("Extract the backup next to the existing files")
                                .clicked()
                            {
                                wipe = Some(false);
                            }
                            if ui
                                .add_enabled(
                                    !busy,
                                    theme::danger_icon_button(
                                        theme::icons::TRASH,
                                        "Delete files & restore",
                                    ),
                                )
                                .on_hover_text(
                                    "Delete everything in the server folder first — cannot be undone",
                                )
                                .clicked()
                            {
                                wipe = Some(true);
                            }
                        });
                    });
                });
            if modal.should_close() && !busy {
                close = true;
            }
            match wipe {
                Some(wipe) => {
                    dialog.start_restore(&name, wipe);
                    dialog.confirm_restore = None;
                }
                None => {
                    if close {
                        dialog.confirm_restore = None;
                    }
                }
            }
        }
    }

    fn start_duplicate(&mut self, index: usize) {
        let Some(entry) = self.servers.get(index) else {
            return;
        };
        if entry.state() != ServerState::Stopped || self.duplicate_task.is_some() {
            return;
        }
        let original = entry.config.clone();
        let mut duplicate = original.clone();
        duplicate.id = new_id();
        let base_name = format!("{} Copy", original.name);
        duplicate.name = base_name.clone();
        let mut suffix = 2;
        while self
            .servers
            .iter()
            .any(|entry| entry.config.name.eq_ignore_ascii_case(&duplicate.name))
        {
            duplicate.name = format!("{base_name} {suffix}");
            suffix += 1;
        }
        let source_dir = original.working_dir.clone();
        let jar_source = original.jar_path.clone();
        let target_dir = unique_server_dir(&self.store.servers_dir(), &duplicate.name);
        duplicate.working_dir = target_dir.clone();
        duplicate.jar_path = jar_source
            .strip_prefix(&source_dir)
            .map(|relative| target_dir.join(relative))
            .unwrap_or_else(|_| target_dir.join("server.jar"));

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| {
                if !jar_source.is_file() {
                    return Err(format!("Server jar not found: {}", jar_source.display()));
                }
                files::copy_directory(&source_dir, &target_dir).map_err(|err| err.to_string())?;
                if !jar_source.starts_with(&source_dir) {
                    fs::copy(&jar_source, &duplicate.jar_path)
                        .map_err(|err| format!("Could not copy server jar: {err}"))?;
                }
                Ok(duplicate)
            })();
            let result = match result {
                Err(err) => match fs::remove_dir_all(&target_dir) {
                    Ok(()) => Err(err),
                    Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(err),
                    Err(cleanup) => Err(format!(
                        "{err}; partial duplicate folder could not be removed ({}): {cleanup}",
                        target_dir.display()
                    )),
                },
                Ok(config) => Ok(config),
            };
            let _ = tx.send(result);
        });
        self.select_server(index);
        self.duplicate_source = Some(original.id);
        self.duplicate_task = Some(rx);
    }

    fn import_server_folder(&mut self) {
        self.import_dialog = Some(ImportDialog::new());
    }
}

fn format_line(line: &LogLine) -> String {
    let stamp = chrono::DateTime::<chrono::Local>::from(line.timestamp).format("%H:%M:%S");
    format!("[{stamp}] {}", strip_ansi(&line.text))
}

/// Removes ANSI escape sequences so text can be searched and copied cleanly.
/// Lines without escape codes are returned without allocating.
fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\x1b') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(out)
}

fn ansi_basic(code: u8, bright: bool) -> egui::Color32 {
    let (n, b) = if bright { (code, true) } else { (code, false) };
    match (n, b) {
        (0, false) => egui::Color32::from_rgb(0x4A, 0x55, 0x50),
        (0, true) => egui::Color32::from_rgb(0x7C, 0x88, 0x82),
        (1, false) => egui::Color32::from_rgb(0xE3, 0x6B, 0x6B),
        (1, true) => egui::Color32::from_rgb(0xFF, 0x8F, 0x8F),
        (2, false) => egui::Color32::from_rgb(0x5F, 0xC9, 0x84),
        (2, true) => theme::PRIMARY,
        (3, false) => egui::Color32::from_rgb(0xD9, 0xB8, 0x4A),
        (3, true) => theme::WARN,
        (4, false) => egui::Color32::from_rgb(0x5B, 0x8F, 0xE0),
        (4, true) => egui::Color32::from_rgb(0x8F, 0xB6, 0xFF),
        (5, false) => egui::Color32::from_rgb(0xB0, 0x6B, 0xD9),
        (5, true) => egui::Color32::from_rgb(0xD9, 0x9B, 0xFF),
        (6, false) => egui::Color32::from_rgb(0x4F, 0xC1, 0xC9),
        (6, true) => egui::Color32::from_rgb(0x8F, 0xE8, 0xF0),
        (_, false) => egui::Color32::from_rgb(0xC8, 0xD0, 0xCB),
        (_, true) => theme::TEXT,
    }
}

fn ansi_256(n: u8) -> egui::Color32 {
    match n {
        0..=7 => ansi_basic(n, false),
        8..=15 => ansi_basic(n - 8, true),
        16..=231 => {
            let n = n - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            egui::Color32::from_rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + (n - 232) * 10;
            egui::Color32::from_rgb(v, v, v)
        }
    }
}

/// Applies the parameters of one SGR (`ESC[...m`) sequence.
fn apply_sgr(params: &str, color: &mut Option<egui::Color32>, bold: &mut bool) {
    let nums: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => {
                *color = None;
                *bold = false;
            }
            1 => *bold = true,
            22 => *bold = false,
            30..=37 => *color = Some(ansi_basic((nums[i] - 30) as u8, *bold)),
            39 => *color = None,
            90..=97 => *color = Some(ansi_basic((nums[i] - 90) as u8, true)),
            38 => match nums.get(i + 1) {
                Some(5) if i + 2 < nums.len() => {
                    *color = Some(ansi_256(nums[i + 2] as u8));
                    i += 2;
                }
                Some(2) if i + 4 < nums.len() => {
                    *color = Some(egui::Color32::from_rgb(
                        nums[i + 2] as u8,
                        nums[i + 3] as u8,
                        nums[i + 4] as u8,
                    ));
                    i += 4;
                }
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
}

/// Builds a monospace layout job for one console line, honoring ANSI colors.
fn console_job(line: &LogLine) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let font = egui::FontId::monospace(13.5);
    let mut job = LayoutJob::default();
    let stamp = chrono::DateTime::<chrono::Local>::from(line.timestamp).format("%H:%M:%S");
    job.append(
        &format!("[{stamp}] "),
        0.0,
        TextFormat::simple(font.clone(), theme::TEXT_MUTED),
    );

    let base = line_color(line);
    let mut color: Option<egui::Color32> = None;
    let mut bold = false;
    let mut segment = String::new();
    let flush = |job: &mut LayoutJob, segment: &mut String, color: Option<egui::Color32>| {
        if !segment.is_empty() {
            job.append(
                segment,
                0.0,
                TextFormat::simple(font.clone(), color.unwrap_or(base)),
            );
            segment.clear();
        }
    };
    let mut chars = line.text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            segment.push(ch);
            continue;
        }
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut params = String::new();
        let mut final_byte = None;
        for c in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&c) {
                final_byte = Some(c);
                break;
            }
            params.push(c);
        }
        if final_byte == Some('m') {
            flush(&mut job, &mut segment, color);
            if params.is_empty() {
                params.push('0');
            }
            apply_sgr(&params, &mut color, &mut bold);
        }
    }
    flush(&mut job, &mut segment, color);
    job
}

/// Colorizes by stream and by the log level found in the line.
/// Maps a highlighted token class onto the UI palette.
fn syntax_color(kind: TokenKind) -> egui::Color32 {
    match kind {
        TokenKind::Comment | TokenKind::Punct => theme::TEXT_MUTED,
        TokenKind::Key | TokenKind::Section | TokenKind::Info => theme::PRIMARY,
        TokenKind::Str => theme::WARN,
        TokenKind::Number => theme::SYNTAX_NUMBER,
        TokenKind::Bool => theme::SYNTAX_BOOL,
        TokenKind::Warn => theme::WARN,
        TokenKind::Error => theme::ERROR,
    }
}

/// Builds the editor layout job, coloring tokens for the file's language
/// (`None` renders plain monospace).
fn syntax_job(
    ui: &egui::Ui,
    text: &str,
    language: Option<Language>,
    wrap_width: f32,
) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let plain = egui::TextFormat::simple(font.clone(), theme::TEXT);
    let Some(language) = language else {
        job.append(text, 0.0, plain);
        return job;
    };
    let mut last = 0;
    for span in highlight::highlight(language, text) {
        if span.start > last {
            job.append(&text[last..span.start], 0.0, plain.clone());
        }
        let format = egui::TextFormat::simple(font.clone(), syntax_color(span.kind));
        job.append(&text[span.start..span.end], 0.0, format);
        last = span.end;
    }
    if last < text.len() {
        job.append(&text[last..], 0.0, plain);
    }
    job
}

/// Byte ranges of every case-insensitive (ASCII) occurrence of `query`.
fn find_matches(text: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    haystack
        .match_indices(&needle)
        .map(|(start, found)| (start, start + found.len()))
        .collect()
}

/// Splits the job's sections at match boundaries and paints a background on matches.
fn highlight_matches(
    job: egui::text::LayoutJob,
    text: &str,
    matches: &[(usize, usize)],
    current: Option<usize>,
) -> egui::text::LayoutJob {
    if matches.is_empty() {
        return job;
    }
    let mut out = egui::text::LayoutJob {
        text: job.text.clone(),
        wrap: job.wrap.clone(),
        ..Default::default()
    };
    for section in &job.sections {
        let mut pos = section.byte_range.start;
        let end = section.byte_range.end;
        let mut push = |range: std::ops::Range<usize>, background: Option<egui::Color32>| {
            if range.is_empty() {
                return;
            }
            let mut format = section.format.clone();
            if let Some(color) = background {
                format.background = color;
            }
            out.sections.push(egui::text::LayoutSection {
                leading_space: if range.start == section.byte_range.start {
                    section.leading_space
                } else {
                    0.0
                },
                byte_range: range,
                format,
            });
        };
        for (index, &(start, stop)) in matches.iter().enumerate() {
            if stop <= pos || start >= end || stop > text.len() {
                continue;
            }
            let from = start.max(pos);
            let to = stop.min(end);
            push(pos..from, None);
            let color = if Some(index) == current {
                theme::PRIMARY.gamma_multiply(0.75)
            } else {
                theme::WARN.gamma_multiply(0.45)
            };
            push(from..to, Some(color));
            pos = to;
        }
        push(pos..end, None);
    }
    out
}

fn line_color(line: &LogLine) -> egui::Color32 {
    if line.stream == spoon_core::LogStream::Stderr {
        return theme::ERROR;
    }
    let stripped = strip_ansi(&line.text);
    let text = &stripped;
    if text.contains("ERROR]") || text.contains("FATAL]") || text.contains(" ERROR ") {
        theme::ERROR
    } else if text.contains("WARN]") || text.contains(" WARN ") {
        theme::WARN
    } else if text.contains("DEBUG]") || text.contains("TRACE]") {
        theme::TEXT_MUTED
    } else if text.contains("INFO]")
        && (text.contains(" joined the game")
            || text.contains(" left the game")
            || text.contains("Done ("))
    {
        theme::PRIMARY
    } else {
        theme::TEXT
    }
}

const SPLASH_PNG: &[u8] = include_bytes!("../../../assets/images/Splash.png");
const SPLASH_DURATION: Duration = Duration::from_secs(5);
pub const SPLASH_SIZE: [f32; 2] = [720.0, 516.0];

struct Splash {
    texture: egui::TextureHandle,
    started: Instant,
    positioned: bool,
}

fn load_splash(ctx: &egui::Context) -> Option<Splash> {
    let img = image::load_from_memory_with_format(SPLASH_PNG, image::ImageFormat::Png)
        .ok()?
        .to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(Splash {
        texture: ctx.load_texture("spoon_splash", color, egui::TextureOptions::LINEAR),
        started: Instant::now(),
        positioned: false,
    })
}

const LOGO_PNG: &[u8] = include_bytes!("../../../assets/images/Spoon-No-Border.png");
const LOGO_HOVER_PNG: &[u8] = include_bytes!("../../../assets/images/Spoon-Hover.png");
/// Built-in server icons offered in the appearance settings (64×64 PNGs,
/// copied to the server folder as `server-icon.png` when picked).
const PRESET_SERVER_ICONS: [(&str, &[u8]); 3] = [
    (
        "Spoon Classic",
        include_bytes!("../../../assets/images/server-icons/Spoon-Classic.png"),
    ),
    (
        "Spoon Alone",
        include_bytes!("../../../assets/images/server-icons/Spoon-Alone.png"),
    ),
    (
        "Blob Alone",
        include_bytes!("../../../assets/images/server-icons/Blob-Alone.png"),
    ),
];
const DISCORD_INVITE_URL: &str = "https://discord.gg/7UEZnsMMkf";
const DISCORD_BUTTON_COLOR: egui::Color32 = egui::Color32::from_rgb(88, 101, 242);
const KOFI_URL: &str = "https://ko-fi.com/pengucc";
const KOFI_BUTTON_COLOR: egui::Color32 = egui::Color32::from_rgb(0xFF, 0x5E, 0x5B);

/// Window and taskbar icon.
pub fn window_icon() -> egui::IconData {
    let img = image::load_from_memory_with_format(LOGO_PNG, image::ImageFormat::Png)
        .map(|img| spoon_core::fit_square(&img.to_rgba8(), 256))
        .unwrap_or_else(|_| image::RgbaImage::new(1, 1));
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}

fn load_logo(ctx: &egui::Context, bytes: &[u8], name: &str) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let img = img.to_rgba8();
    let scale = 128.0 / img.width().max(img.height()) as f32;
    let w = ((img.width() as f32 * scale).round() as u32).max(1);
    let h = ((img.height() as f32 * scale).round() as u32).max(1);
    let img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture(name, color, egui::TextureOptions::LINEAR))
}

/// Base URL of the MCJars S3 bucket that hosts per-software icons (lowercase names).
const MCJARS_ICON_BASE: &str = "https://s3.mcjars.app/icons";

fn load_player_skin_texture(
    ctx: &egui::Context,
    bytes: &[u8],
    username: &str,
) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let image = image::imageops::resize(&image, 64, 64, image::imageops::FilterType::Nearest);
    let color = egui::ColorImage::from_rgba_unmultiplied([64, 64], image.as_raw());
    Some(ctx.load_texture(
        format!("player_skin_{username}"),
        color,
        egui::TextureOptions::NEAREST,
    ))
}

fn player_detail_card(ui: &mut egui::Ui, glyph: &str, label: &str, value: &str) {
    theme::card().show(ui, |ui| {
        ui.set_width(220.0);
        theme::section_title_with_icon(ui, glyph, label);
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(value)
                    .size(14.0)
                    .strong()
                    .color(theme::TEXT),
            )
            .wrap(),
        );
    });
}

struct PlayerListPresentation<'a> {
    status: &'a str,
    status_color: egui::Color32,
    empty_title: &'a str,
    empty_hint: &'a str,
    is_online: bool,
}

fn render_player_list(
    ui: &mut egui::Ui,
    players: &[String],
    skin_textures: &HashMap<String, Option<egui::TextureHandle>>,
    presentation: PlayerListPresentation<'_>,
    selected_player: &mut Option<(String, bool)>,
    visible_skin_requests: &mut Vec<String>,
) {
    if players.is_empty() {
        render_player_list_empty(ui, presentation.empty_title, presentation.empty_hint);
        return;
    }

    ui.spacing_mut().item_spacing.y = 8.0;
    for name in players {
        let skin_key = name.to_ascii_lowercase();
        let response = theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if let Some(Some(texture)) = skin_textures.get(&skin_key) {
                    ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(40.0, 40.0)));
                } else {
                    ui.label(theme::icon(theme::icons::USERS, 22.0).color(theme::TEXT_MUTED));
                }
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(name)
                            .size(15.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(presentation.status)
                            .size(10.0)
                            .color(presentation.status_color),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(theme::ghost_icon_button(
                            theme::icons::ARROW_RIGHT,
                            "Details",
                        ))
                        .clicked()
                    {
                        *selected_player = Some((name.clone(), presentation.is_online));
                    }
                });
            });
        });
        if ui.is_rect_visible(response.response.rect) && !skin_textures.contains_key(&skin_key) {
            visible_skin_requests.push(name.clone());
        }
    }
}

fn render_player_list_empty(ui: &mut egui::Ui, title: &str, hint: &str) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(theme::icon(theme::icons::USERS, 28.0).color(theme::TEXT_MUTED));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(hint)
                    .size(12.5)
                    .color(theme::TEXT_MUTED),
            );
        });
        ui.add_space(12.0);
    });
}

fn render_player_list_loading(ui: &mut egui::Ui, loading: bool) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            if loading {
                ui.spinner();
                ui.label(
                    egui::RichText::new("Loading server player data...").color(theme::TEXT_MUTED),
                );
            } else {
                ui.label(
                    egui::RichText::new("Player data is unavailable.").color(theme::TEXT_MUTED),
                );
            }
        });
    });
}

fn render_whitelist_row(
    ui: &mut egui::Ui,
    profile: &PlayerProfile,
    skin_textures: &HashMap<String, Option<egui::TextureHandle>>,
    can_remove: bool,
    remove_player: &mut Option<String>,
    visible_skin_requests: &mut Vec<String>,
) {
    let response = theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let skin_key = profile.name.to_ascii_lowercase();
            if let Some(Some(texture)) = skin_textures.get(&skin_key) {
                ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(36.0, 36.0)));
            } else {
                ui.label(theme::icon(theme::icons::USERS, 20.0).color(theme::TEXT_MUTED));
            }
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(&profile.name)
                        .size(14.5)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.label(
                    egui::RichText::new(&profile.uuid)
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        can_remove,
                        theme::danger_icon_button(theme::icons::TRASH, "Remove"),
                    )
                    .clicked()
                {
                    *remove_player = Some(profile.uuid.clone());
                }
            });
        });
    });
    let skin_key = profile.name.to_ascii_lowercase();
    if ui.is_rect_visible(response.response.rect) && !skin_textures.contains_key(&skin_key) {
        visible_skin_requests.push(profile.name.clone());
    }
}

fn render_ip_ban_row(
    ui: &mut egui::Ui,
    ip: &str,
    can_remove: bool,
    remove_ip: &mut Option<String>,
) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(ip)
                    .size(14.5)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(
                        can_remove,
                        theme::danger_icon_button(theme::icons::TRASH, "Unban"),
                    )
                    .clicked()
                {
                    *remove_ip = Some(ip.to_string());
                }
            });
        });
    });
}

/// Downloads one software icon from the MCJars S3 bucket.
fn fetch_software_icon(software: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(format!("{MCJARS_ICON_BASE}/{software}.png"))
        .send()
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|err| err.to_string())
}

/// Turns downloaded icon bytes into a 64x64 egui texture.
fn load_icon_texture(
    ctx: &egui::Context,
    bytes: &[u8],
    software: &str,
) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = image::imageops::resize(
        &img.to_rgba8(),
        64,
        64,
        image::imageops::FilterType::Lanczos3,
    );
    let color = egui::ColorImage::from_rgba_unmultiplied([64, 64], img.as_raw());
    Some(ctx.load_texture(
        format!("server_icon_{software}"),
        color,
        egui::TextureOptions::LINEAR,
    ))
}

/// Actions offered by a server card's "⋯" menu (also used for right-click).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ServerAction {
    Settings,
    OpenFolder,
    Duplicate,
    Remove,
}

impl SpoonApp {
    /// A full-width server card: software icon (or a generic placeholder) with a
    /// status dot badge, the name, a "status · software version" meta line and a
    /// "⋯" action menu. Returns the row response plus the chosen action, if any.
    fn server_row(
        &self,
        ui: &mut egui::Ui,
        index: usize,
        time: f64,
    ) -> (egui::Response, Option<ServerAction>) {
        let entry = &self.servers[index];
        let state = entry.state();
        let color = state_color(state);
        let selected = self.selected == Some(index);
        let running = state == ServerState::Running;

        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 52.0), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let fill = if selected {
                theme::PRIMARY.gamma_multiply(0.16)
            } else {
                theme::animated_hover_fill(
                    ui,
                    response.id.with("hover_fill"),
                    response.hovered(),
                    egui::Color32::TRANSPARENT,
                    theme::SURFACE_HOVER,
                )
            };
            let painter = ui.painter();
            painter.rect_filled(rect, 8.0, fill);
            if selected {
                painter.rect_stroke(
                    rect,
                    8.0,
                    egui::Stroke::new(1.0_f32, theme::PRIMARY.gamma_multiply(0.5)),
                    egui::StrokeKind::Inside,
                );
            }

            // Icon (or placeholder glyph) with the status dot as a corner badge.
            let icon_size = 32.0;
            let icon_rect = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 8.0 + icon_size / 2.0, rect.center().y),
                egui::vec2(icon_size, icon_size),
            );
            let icon = entry.config.server_type.as_deref().and_then(|software| {
                self.icon_textures
                    .get(&software.to_lowercase())
                    .and_then(|texture| texture.as_ref())
            });
            match icon {
                Some(texture) => egui::Image::new(texture)
                    .corner_radius(egui::CornerRadius::same(7))
                    .paint_at(ui, icon_rect),
                None => {
                    painter.rect_filled(icon_rect, 7.0, theme::SURFACE_HOVER);
                    painter.text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        theme::icons::SERVER,
                        egui::FontId::new(15.0, theme::icon_family()),
                        theme::TEXT_MUTED,
                    );
                }
            }
            let dot_center = egui::pos2(icon_rect.right() - 4.0, icon_rect.bottom() - 4.0);
            if running {
                // Soft expanding ring so running servers stand out at a glance.
                let pulse = (time as f32 * 1.6).sin() * 0.5 + 0.5;
                painter.circle_filled(
                    dot_center,
                    5.5 + pulse * 3.0,
                    color.gamma_multiply(0.08 + 0.14 * (1.0 - pulse)),
                );
            }
            painter.circle_filled(dot_center, 4.5, color);

            // Text column: name over a "status · software · version" meta line.
            let text_left = icon_rect.right() + 10.0;
            let menu_slot = 30.0;
            let max_text_width = (rect.right() - menu_slot - text_left - 6.0).max(24.0);
            let name_font = egui::FontId::proportional(15.0);
            let meta_font = egui::FontId::proportional(12.0);
            let name = truncate_to_width(ui, &entry.config.name, name_font.clone(), max_text_width);
            let name_rect = painter.text(
                egui::pos2(text_left, rect.top() + 6.0),
                egui::Align2::LEFT_TOP,
                name,
                name_font,
                theme::TEXT,
            );
            let mut meta = format!("{} · {}", state.label(), software_label(&entry.config));
            if running && !entry.online_players.is_empty() {
                meta = format!("{meta} · {} online", entry.online_players.len());
            }
            let meta = truncate_to_width(ui, &meta, meta_font.clone(), max_text_width);
            painter.text(
                name_rect.left_bottom(),
                egui::Align2::LEFT_TOP,
                meta,
                meta_font,
                theme::TEXT_MUTED,
            );
        }

        // "⋯" menu button pinned to the row's right edge.
        let mut action = None;
        let menu_rect = egui::Rect::from_min_size(
            egui::pos2(
                rect.right() - 30.0,
                rect.top() + (rect.height() - 28.0) / 2.0,
            ),
            egui::vec2(26.0, 28.0),
        );
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(menu_rect), |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            // Slim the default button padding so the "⋯" stays a compact square.
            ui.scope(|ui| {
                ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
                ui.menu_button(
                    theme::icon(theme::icons::ELLIPSIS, 14.0).color(theme::TEXT_MUTED),
                    |ui| {
                        action = self.server_actions_menu(ui, index);
                    },
                );
            });
        });

        (
            response.on_hover_cursor(egui::CursorIcon::PointingHand),
            action,
        )
    }

    /// The per-server action menu shared by the "⋯" button and right-click.
    fn server_actions_menu(&self, ui: &mut egui::Ui, index: usize) -> Option<ServerAction> {
        let entry = &self.servers[index];
        let editable = self.duplicate_source.as_deref() != Some(entry.config.id.as_str());
        let mut action = None;
        if ui
            .button(theme::icon_text_job(
                theme::icons::FOLDER_OPEN,
                "Open Folder",
                theme::TEXT,
            ))
            .clicked()
        {
            action = Some(ServerAction::OpenFolder);
            ui.close_menu();
        }
        if ui
            .add_enabled(
                editable,
                egui::Button::new(theme::icon_text_job(
                    theme::icons::GEAR,
                    "Settings / Rename",
                    theme::TEXT,
                )),
            )
            .clicked()
        {
            action = Some(ServerAction::Settings);
            ui.close_menu();
        }
        if ui
            .add_enabled(
                entry.state() == ServerState::Stopped && self.duplicate_task.is_none(),
                egui::Button::new(theme::icon_text_job(
                    theme::icons::CLONE,
                    "Duplicate",
                    theme::TEXT,
                )),
            )
            .clicked()
        {
            action = Some(ServerAction::Duplicate);
            ui.close_menu();
        }
        if ui
            .add_enabled(
                entry.state() == ServerState::Stopped && editable && self.confirm_remove.is_none(),
                egui::Button::new(theme::icon_text_job(
                    theme::icons::TRASH,
                    "Remove",
                    theme::TEXT,
                )),
            )
            .clicked()
        {
            action = Some(ServerAction::Remove);
            ui.close_menu();
        }
        action
    }

    /// Starts every stopped server, skipping one that is mid-duplication.
    fn start_all_servers(&mut self) {
        for i in 0..self.servers.len() {
            let entry = &self.servers[i];
            if entry.state() != ServerState::Stopped
                || self.duplicate_source.as_deref() == Some(entry.config.id.as_str())
            {
                continue;
            }
            let config = entry.config.clone();
            let java = self.java_for(config.java_runtime.as_deref());
            match ServerHandle::start(&config, &java) {
                Ok(handle) => {
                    let entry = &mut self.servers[i];
                    entry.last_error = None;
                    entry.started_at = Some(Instant::now());
                    entry.online_players.clear();
                    entry.handle = Some(handle);
                }
                Err(err) => self.servers[i].last_error = Some(err.to_string()),
            }
        }
    }

    /// Requests a graceful stop on every server that is running.
    fn stop_all_servers(&mut self) {
        for entry in &mut self.servers {
            if let Some(handle) = &mut entry.handle {
                handle.request_stop();
            }
        }
    }
}

/// Scrollable software list with icons, shared by the create page and
/// the change-version card. Returns the software that was clicked, if any.
fn software_picker(
    ui: &mut egui::Ui,
    software: &SoftwareCache,
    icon_textures: &HashMap<String, Option<egui::TextureHandle>>,
    id_salt: impl std::hash::Hash,
    selected: Option<&ServerType>,
) -> Option<ServerType> {
    let Some(snapshot) = &software.snapshot else {
        if software.is_loading() {
            ui.label("Loading server software…");
        } else {
            let message = software
                .error
                .as_deref()
                .unwrap_or("Failed to load server software.");
            ui.label(egui::RichText::new(message).color(theme::ERROR));
        }
        return None;
    };
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .max_height(170.0)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (server_type, info) in snapshot.types.all() {
                let key = server_type.api_name().to_lowercase();
                if software_row(
                    icon_textures,
                    ui,
                    &key,
                    &info.name,
                    selected == Some(server_type),
                ) {
                    clicked = Some(server_type.clone());
                }
            }
        });
    clicked
}

fn show_create_page_scroll_area(
    ui: &mut egui::Ui,
    footer_height: f32,
    contents: impl FnOnce(&mut egui::Ui),
) {
    egui::ScrollArea::vertical()
        .id_salt("create_server_page")
        .auto_shrink([false, false])
        .max_height((ui.available_height() - footer_height).max(0.0))
        .show(ui, contents);
}

/// Scrollable version list (newest first) for the selected software, used by
/// the change-version card. The create page renders its own searchable list.
fn version_picker(
    ui: &mut egui::Ui,
    software: &SoftwareCache,
    id_salt: impl std::hash::Hash,
    selected_type: Option<&ServerType>,
    selected_version: &mut Option<String>,
) {
    let Some(server_type) = selected_type else {
        ui.label(egui::RichText::new("Select a software first.").color(theme::TEXT_MUTED));
        return;
    };
    let versions = software.versions(server_type);
    if versions.is_empty() {
        ui.label("No versions found.");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .max_height(240.0)
        .show(ui, |ui| {
            ui.set_min_height(200.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, version) in versions.iter().rev().enumerate() {
                let selected = selected_version.as_ref() == Some(&version.id);
                if version_row(ui, &version.id, selected, i == 0) {
                    *selected_version = Some(version.id.clone());
                }
            }
        });
}

/// A selectable row in the version picker: version id, a "LATEST" tag on the
/// newest entry, accent highlight and a check mark when selected.
/// Returns true when clicked.
fn version_row(ui: &mut egui::Ui, version: &str, selected: bool, is_latest: bool) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if selected {
            theme::PRIMARY.gamma_multiply(0.14)
        } else {
            theme::animated_hover_fill(
                ui,
                response.id.with("hover_fill"),
                response.hovered(),
                egui::Color32::TRANSPARENT,
                theme::SURFACE_HOVER,
            )
        };
        painter.rect_filled(rect, 6.0, fill);
        if selected {
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.0_f32, theme::PRIMARY.gamma_multiply(0.5)),
                egui::StrokeKind::Inside,
            );
        }
        painter.text(
            egui::pos2(rect.left() + 10.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            version,
            egui::FontId::proportional(13.5),
            if selected {
                theme::PRIMARY
            } else {
                theme::TEXT
            },
        );
        let mut right = rect.right() - 10.0;
        if selected {
            painter.text(
                egui::pos2(right - 6.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                theme::icons::CHECK,
                egui::FontId::new(13.0, theme::icon_family()),
                theme::PRIMARY,
            );
            right -= 24.0;
        }
        if is_latest {
            let text = "LATEST";
            let font = egui::FontId::proportional(10.5);
            let width = ui.fonts(|fonts| {
                fonts
                    .layout_no_wrap(text.to_string(), font.clone(), egui::Color32::PLACEHOLDER)
                    .size()
                    .x
            }) + 14.0;
            let pill_rect = egui::Rect::from_min_size(
                egui::pos2(right - width, rect.center().y - 8.0),
                egui::vec2(width, 16.0),
            );
            painter.rect_filled(pill_rect, 8.0, theme::PRIMARY.gamma_multiply(0.16));
            painter.text(
                pill_rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                font,
                theme::PRIMARY,
            );
        }
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Kick off a software icon download unless it is cached or already in flight.
fn ensure_software_icon(
    icon_textures: &HashMap<String, Option<egui::TextureHandle>>,
    icon_pending: &mut BTreeSet<String>,
    icon_tx: &Sender<IconFetch>,
    key: &str,
) {
    if icon_textures.contains_key(key) || icon_pending.contains(key) {
        return;
    }
    icon_pending.insert(key.to_string());
    let tx = icon_tx.clone();
    let key = key.to_string();
    std::thread::spawn(move || {
        let result = fetch_software_icon(&key);
        let _ = tx.send((key, result));
    });
}

/// A selectable software row: icon, display name, accent highlight and a
/// check mark when selected. Returns true when clicked.
fn software_row(
    icon_textures: &HashMap<String, Option<egui::TextureHandle>>,
    ui: &mut egui::Ui,
    key: &str,
    name: &str,
    selected: bool,
) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if selected {
            theme::PRIMARY.gamma_multiply(0.14)
        } else {
            theme::animated_hover_fill(
                ui,
                response.id.with("hover_fill"),
                response.hovered(),
                egui::Color32::TRANSPARENT,
                theme::SURFACE_HOVER,
            )
        };
        painter.rect_filled(rect, 6.0, fill);
        if selected {
            painter.rect_stroke(
                rect,
                6.0,
                egui::Stroke::new(1.0_f32, theme::PRIMARY.gamma_multiply(0.5)),
                egui::StrokeKind::Inside,
            );
        }
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 6.0, rect.top() + (rect.height() - 20.0) / 2.0),
            egui::vec2(20.0, 20.0),
        );
        match icon_textures.get(key).and_then(|t| t.as_ref()) {
            Some(texture) => egui::Image::new(texture)
                .corner_radius(egui::CornerRadius::same(4))
                .paint_at(ui, icon_rect),
            None => {
                painter.rect_filled(icon_rect, 4.0, theme::SURFACE_HOVER);
                painter.text(
                    icon_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    theme::icons::SERVER,
                    egui::FontId::new(11.0, theme::icon_family()),
                    theme::TEXT_MUTED,
                );
            }
        }
        painter.text(
            egui::pos2(icon_rect.right() + 8.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(14.0),
            if selected {
                theme::PRIMARY
            } else {
                theme::TEXT
            },
        );
        if selected {
            painter.text(
                egui::pos2(rect.right() - 16.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                theme::icons::CHECK,
                egui::FontId::new(13.0, theme::icon_family()),
                theme::PRIMARY,
            );
        }
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Resolve the `java` command for one server: per-server override first, then
/// the app-wide setting, falling back to the system PATH when a selected
/// runtime is missing or nothing is selected.
fn resolve_java(
    store: &Option<JavaStore>,
    runtimes: &[JavaRuntime],
    app_default: Option<&str>,
    override_id: Option<&str>,
) -> String {
    for id in [override_id, app_default].into_iter().flatten() {
        if let Some(store) = store {
            if let Some(runtime) = runtimes.iter().find(|runtime| runtime.id == id) {
                let bin = runtime.java_bin(store);
                if bin.exists() {
                    return bin.display().to_string();
                }
            }
        }
    }
    "java".to_string()
}

/// Human-readable software name for a server (e.g. "Paper", or "Custom jar").
fn software_name(config: &ServerConfig) -> String {
    fn prettify(server_type: &str) -> String {
        let mut chars = server_type.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => "Custom".to_string(),
        }
    }
    match &config.server_type {
        Some(server_type) => prettify(server_type),
        None => "Custom jar".to_string(),
    }
}

/// Human-readable "Paper 1.21.1"-style label for a server's software and version.
fn software_label(config: &ServerConfig) -> String {
    match &config.server_version {
        Some(version) => format!("{} {version}", software_name(config)),
        None => software_name(config),
    }
}

/// Shortens `text` so it fits `max_width` pixels, appending an ellipsis when cut.
fn truncate_to_width(ui: &egui::Ui, text: &str, font_id: egui::FontId, max_width: f32) -> String {
    let width_of = |value: &str| {
        ui.fonts(|fonts| {
            fonts
                .layout_no_wrap(
                    value.to_string(),
                    font_id.clone(),
                    egui::Color32::PLACEHOLDER,
                )
                .size()
                .x
        })
    };
    if width_of(text) <= max_width {
        return text.to_string();
    }
    let ellipsis = "…";
    let mut cut = text.to_string();
    while !cut.is_empty() {
        cut.pop();
        let candidate = format!("{cut}{ellipsis}");
        if width_of(&candidate) <= max_width {
            return candidate;
        }
    }
    ellipsis.to_string()
}

/// A compact square icon-only button for file-browser toolbars.
fn icon_toolbar_button(glyph: &str) -> theme::AnimatedButton {
    theme::animated_button(
        egui::Button::new(theme::icon(glyph, 14.0)).min_size(egui::vec2(32.0, 30.0)),
        egui::Color32::TRANSPARENT,
        theme::SURFACE_HOVER,
        egui::Stroke::new(1.0_f32, theme::BORDER),
        egui::Stroke::new(1.0_f32, theme::PRIMARY.gamma_multiply(0.6)),
    )
}

fn state_color(state: ServerState) -> egui::Color32 {
    match state {
        ServerState::Running => theme::PRIMARY,
        ServerState::Starting | ServerState::Stopping => theme::WARN,
        ServerState::Stopped => theme::TEXT_MUTED,
    }
}

fn format_mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1024.0 / 1024.0)
}

/// Formats a memory size in MB as "N GB" when it divides evenly, else "N MB".
fn memory_label(mb: u32) -> String {
    if mb >= 1024 && mb.is_multiple_of(1024) {
        format!("{} GB", mb / 1024)
    } else {
        format!("{mb} MB")
    }
}

/// A labelled RAM stepper (`− value +`), moving in 512 MB jumps clamped to
/// `min..=max`.
fn memory_stepper(ui: &mut egui::Ui, label: &str, value_mb: &mut u32, min: u32, max: u32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let step_button = |glyph: &str| {
                egui::Button::new(egui::RichText::new(glyph).size(14.0))
                    .min_size(egui::vec2(28.0, 24.0))
            };
            if ui.add(step_button("+")).clicked() {
                *value_mb = (*value_mb as i64 + 512).clamp(i64::from(min), i64::from(max)) as u32;
            }
            ui.label(
                egui::RichText::new(memory_label(*value_mb))
                    .size(13.5)
                    .strong()
                    .color(theme::TEXT),
            );
            if ui.add(step_button("−")).clicked() {
                *value_mb = (*value_mb as i64 - 512).clamp(i64::from(min), i64::from(max)) as u32;
            }
        });
    });
}

/// (Label, server.properties value) pairs for the create-page dropdowns.
const GAMEMODE_OPTIONS: [(&str, &str); 4] = [
    ("Survival", "survival"),
    ("Creative", "creative"),
    ("Adventure", "adventure"),
    ("Spectator", "spectator"),
];
const DIFFICULTY_OPTIONS: [(&str, &str); 4] = [
    ("Peaceful", "peaceful"),
    ("Easy", "easy"),
    ("Normal", "normal"),
    ("Hard", "hard"),
];
const LEVEL_TYPE_OPTIONS: [(&str, &str); 5] = [
    ("Default", "minecraft:normal"),
    ("Superflat", "minecraft:flat"),
    ("Large Biomes", "minecraft:large_biomes"),
    ("Amplified", "minecraft:amplified"),
    ("Single Biome", "minecraft:single_biome_surface"),
];

/// Small muted caption used above form fields.
fn form_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(theme::TEXT_MUTED),
    );
}

/// A full-width dropdown bound to one of `options`, showing the label of the
/// selected value.
fn settings_combo(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    label: &str,
    options: &[(&str, &str)],
    value: &mut String,
) {
    form_label(ui, label);
    let selected_text = options
        .iter()
        .find(|(_, candidate)| candidate == value)
        .map(|(name, _)| *name)
        .unwrap_or(value.as_str());
    egui::ComboBox::from_id_salt(id_salt)
        .width(ui.available_width())
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            for (name, candidate) in options {
                ui.selectable_value(value, candidate.to_string(), *name);
            }
        });
}

/// A switch with a title and a one-line explanation beneath it.
fn settings_toggle(ui: &mut egui::Ui, on: &mut bool, title: &str, description: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        theme::toggle(ui, on);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(egui::RichText::new(title).size(13.5).color(theme::TEXT));
            ui.label(
                egui::RichText::new(description)
                    .size(11.5)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
}

/// Swap a downloaded update in for the managed executable and start it. Runs
/// entirely in this process (Windows allows renaming a running image), so it
/// works no matter which version the downloaded binary was built from, and a
/// failure leaves the current install untouched.
fn swap_in_update(downloaded: &Path) -> Result<(), String> {
    let target = bootstrap::managed_exe()
        .ok_or_else(|| "could not determine the managed executable path".to_string())?;
    bootstrap::apply_update_download(downloaded, &target)?;
    bootstrap::launch(&target)
        .map(|_| ())
        .map_err(|err| format!("could not relaunch {}: {err}", target.display()))
}

/// Height the sidebar footer needs: full-width 36px button rows, a version
/// label, 8px gaps, and a small margin. Must match `render_sidebar` or the
/// server list and the pinned footer start to overlap.
pub fn sidebar_footer_height(update_row_visible: bool) -> f32 {
    const BUTTON_HEIGHT: f32 = 36.0;
    const GAP: f32 = 8.0;
    const VERSION_HEIGHT: f32 = 18.0;
    const BOTTOM_MARGIN: f32 = 4.0;
    let buttons = if update_row_visible { 4.0 } else { 3.0 };
    BUTTON_HEIGHT * buttons + GAP * buttons + VERSION_HEIGHT + BOTTOM_MARGIN
}

/// Whether downloaded updates replace the running app in one restart. Only
/// managed Windows installs do this; elsewhere the dialog reveals the
/// downloaded file instead.
fn updates_apply_in_place() -> bool {
    bootstrap::is_managed_flow_enabled() && bootstrap::running_from_managed()
}

/// Opens the file's location in the system file manager.
fn reveal_in_folder(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
}

/// Opens a directory directly in the system file manager.
fn open_directory(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn update_online_players(players: &mut BTreeSet<String>, text: &str) {
    let message = text.rsplit("]: ").next().unwrap_or(text);
    if let Some(name) = message.strip_suffix(" joined the game") {
        players.insert(name.trim().to_string());
    } else if let Some(name) = message.strip_suffix(" left the game") {
        players.remove(name.trim());
    }
}

fn parse_online_player_list(text: &str) -> Option<Vec<String>> {
    let message = text.rsplit("]: ").next().unwrap_or(text).trim();
    let (_, names) = message.split_once(" players online:")?;
    let names = names.trim();
    if names.is_empty() {
        return Some(Vec::new());
    }
    Some(
        names
            .split(',')
            .map(str::trim)
            .filter(|name| is_minecraft_username(name))
            .map(str::to_string)
            .collect(),
    )
}

/// Reads `max-players` from a server's server.properties (default 20).
fn read_max_players(working_dir: &Path) -> u32 {
    let Ok(text) = fs::read_to_string(working_dir.join("server.properties")) else {
        return 20;
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("max-players=") {
            if let Ok(max) = value.trim().parse::<u32>() {
                return max.max(1);
            }
        }
    }
    20
}

/// Reads `server-port` from a server's server.properties (default 25565).
fn read_server_port(working_dir: &Path) -> u16 {
    fs::read_to_string(working_dir.join("server.properties"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("server-port="))
                .and_then(|value| value.trim().parse::<u16>().ok())
        })
        .unwrap_or(25565)
}

/// Best-effort LAN IP: a UDP "connect" picks the outbound interface without sending anything.
fn local_ip_address() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// File browser/editor modal state for one server instance.
struct FilesDialog {
    server_id: String,
    /// Canonicalized server folder; navigation is confined to it.
    root: PathBuf,
    /// Directory currently being listed; always inside `root`.
    current: PathBuf,
    entries: Vec<FileEntry>,
    error: Option<String>,
    selected: Option<PathBuf>,
    /// Entries ticked for bulk compress/delete (within the current folder).
    checked: std::collections::BTreeSet<PathBuf>,
    compress_task: Option<Receiver<Result<PathBuf, String>>>,
    open: Option<OpenFile>,
    pending: Option<PendingFileOp>,
    pending_name: String,
    confirm_delete: Option<Vec<PathBuf>>,
    /// Set when the user tries to close with unsaved changes.
    confirm_close: bool,
    status: Option<String>,
}

struct OpenFile {
    path: PathBuf,
    content: String,
    saved_content: String,
    read_only: bool,
    note: Option<String>,
    language: Option<Language>,
    search: Option<FileSearch>,
}

#[derive(Default)]
struct FileSearch {
    query: String,
    current: usize,
    focus: bool,
}

#[derive(Clone)]
enum PendingFileOp {
    NewFile,
    NewFolder,
    Rename(PathBuf),
}

impl FilesDialog {
    fn new(config: &ServerConfig) -> Self {
        let root =
            fs::canonicalize(&config.working_dir).unwrap_or_else(|_| config.working_dir.clone());
        let current = root.clone();
        let mut dialog = Self {
            server_id: config.id.clone(),
            root,
            current,
            entries: Vec::new(),
            error: None,
            selected: None,
            checked: Default::default(),
            compress_task: None,
            open: None,
            pending: None,
            pending_name: String::new(),
            confirm_delete: None,
            confirm_close: false,
            status: None,
        };
        dialog.refresh();
        dialog
    }

    fn refresh(&mut self) {
        self.error = None;
        match files::list_dir(&self.current) {
            Ok(entries) => self.entries = entries,
            Err(err) => {
                self.entries.clear();
                self.error = Some(format!("Could not list folder: {err}"));
            }
        }
    }

    fn navigate_to(&mut self, dir: &Path) {
        if self.open_is_dirty() {
            self.error = Some("Save or discard the open file's changes first.".to_string());
            return;
        }
        if let Err(err) = files::ensure_within(&self.root, dir) {
            self.error = Some(err.to_string());
            return;
        }
        self.current = dir.to_path_buf();
        self.selected = None;
        self.checked.clear();
        self.open = None;
        self.refresh();
    }

    fn open_file(&mut self, path: &Path) {
        if self.open_is_dirty() {
            self.error = Some("Save or discard the open file's changes first.".to_string());
            return;
        }
        self.open = None;
        self.status = None;
        match files::read_file(path) {
            Ok(FileContent::Text(content)) => {
                self.open = Some(OpenFile {
                    path: path.to_path_buf(),
                    saved_content: content.clone(),
                    content,
                    read_only: false,
                    note: None,
                    language: highlight::detect(path),
                    search: Some(FileSearch::default()),
                })
            }
            Ok(FileContent::Binary) => {
                self.open = Some(OpenFile {
                    path: path.to_path_buf(),
                    content: String::new(),
                    saved_content: String::new(),
                    read_only: true,
                    note: Some("Binary file — cannot be edited.".to_string()),
                    language: None,
                    search: Some(FileSearch::default()),
                })
            }
            Ok(FileContent::TooLarge) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.error = Some(format!(
                    "{name} is larger than {} MB and was not opened.",
                    files::MAX_EDITABLE_BYTES / 1024 / 1024
                ));
            }
            Err(err) => self.error = Some(format!("Could not read file: {err}")),
        }
    }

    fn create_entry(&mut self, is_dir: bool) {
        let name = self.pending_name.trim();
        if name.is_empty() {
            return;
        }
        let target = self.current.join(name);
        let result = if is_dir {
            fs::create_dir(&target)
        } else {
            fs::File::create(&target).map(|_| ())
        };
        match result {
            Ok(()) => {
                self.pending = None;
                self.selected = Some(target);
                self.refresh();
            }
            Err(err) => {
                let kind = if is_dir { "folder" } else { "file" };
                self.error = Some(format!("Could not create {kind}: {err}"));
            }
        }
    }

    fn rename_entry(&mut self, from: &Path) {
        let name = self.pending_name.trim();
        if name.is_empty() {
            return;
        }
        let Some(to) = from.parent().map(|parent| parent.join(name)) else {
            return;
        };
        match fs::rename(from, &to) {
            Ok(()) => {
                self.pending = None;
                if self.selected.as_deref() == Some(from) {
                    self.selected = Some(to.clone());
                }
                if let Some(open) = &mut self.open {
                    if open.path == from {
                        open.path = to.clone();
                    }
                }
                self.refresh();
            }
            Err(err) => self.error = Some(format!("Could not rename: {err}")),
        }
    }

    fn delete_entries(&mut self, paths: &[PathBuf]) {
        let mut failure = None;
        for path in paths {
            let result = if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            if let Err(err) = result {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                failure = Some(format!("Could not delete {name}: {err}"));
                continue;
            }
            self.checked.remove(path);
            if self.selected.as_deref() == Some(path.as_path()) {
                self.selected = None;
            }
            if self.open.as_ref().map(|open| open.path.as_path()) == Some(path.as_path()) {
                self.open = None;
            }
            if self.current.starts_with(path) {
                self.current = self.root.clone();
            }
        }
        self.confirm_delete = None;
        self.refresh();
        if failure.is_some() {
            self.error = failure;
        }
    }

    /// Zips the ticked entries into the current folder on a background thread.
    fn compress_checked(&mut self) {
        if self.compress_task.is_some() {
            return;
        }
        let items: Vec<PathBuf> = self.checked.iter().cloned().collect();
        let base = if items.len() == 1 {
            items[0]
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archive".to_string())
        } else {
            "archive".to_string()
        };
        let dir = self.current.clone();
        let dest = files::unique_zip_path(&dir, &base);
        self.status = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = files::zip_paths(&dir, &items, &dest)
                .map(|()| dest)
                .map_err(|err| err.to_string());
            let _ = tx.send(result);
        });
        self.compress_task = Some(rx);
    }

    fn poll_compress(&mut self) {
        let Some(rx) = &self.compress_task else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(dest)) => {
                self.compress_task = None;
                self.checked.clear();
                self.refresh();
                self.status = dest
                    .file_name()
                    .map(|name| format!("Created {}", name.to_string_lossy()));
            }
            Ok(Err(err)) => {
                self.compress_task = None;
                self.error = Some(format!("Could not compress: {err}"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.compress_task = None;
                self.error = Some("Compression stopped unexpectedly.".to_string());
            }
        }
    }

    fn open_is_dirty(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|open| open.content != open.saved_content)
    }
}

fn file_path_breadcrumbs(root: &Path, current: &Path, root_label: &str) -> Vec<(PathBuf, String)> {
    let mut breadcrumbs = vec![(root.to_path_buf(), root_label.to_string())];
    let Ok(relative) = current.strip_prefix(root) else {
        return breadcrumbs;
    };
    let mut path = root.to_path_buf();
    for name in relative.iter() {
        path.push(name);
        breadcrumbs.push((path.clone(), name.to_string_lossy().into_owned()));
    }
    breadcrumbs
}

/// Backups page state for one server instance. The heavy zipping runs on a
/// background thread so the UI never blocks.
struct BackupsDialog {
    server_id: String,
    /// Cached from the server config so the create thread needs no app state.
    server_name: String,
    working_dir: PathBuf,
    store: BackupStore,
    backups: Vec<BackupInfo>,
    create_task: Option<Receiver<Result<BackupInfo, String>>>,
    restore_task: Option<Receiver<Result<String, String>>>,
    /// Backup waiting for delete confirmation (file stem).
    confirm_delete: Option<String>,
    /// Backup waiting for the restore choice (file stem).
    confirm_restore: Option<String>,
    error: Option<String>,
    status: Option<String>,
    status_expires: Option<Instant>,
}

impl BackupsDialog {
    fn new(config: &ServerConfig, store: BackupStore) -> Self {
        let mut dialog = Self {
            server_id: config.id.clone(),
            server_name: config.name.clone(),
            working_dir: config.working_dir.clone(),
            store,
            backups: Vec::new(),
            create_task: None,
            restore_task: None,
            confirm_delete: None,
            confirm_restore: None,
            error: None,
            status: None,
            status_expires: None,
        };
        dialog.refresh();
        dialog
    }

    fn busy(&self) -> bool {
        self.create_task.is_some() || self.restore_task.is_some()
    }

    fn refresh(&mut self) {
        self.backups = self.store.list(&self.server_id);
    }

    /// Zip the server folder into the backups directory on a background thread.
    fn start_create(&mut self) {
        if self.create_task.is_some() {
            return;
        }
        self.error = None;
        self.status = None;
        self.status_expires = None;
        let store = self.store.clone();
        let server_id = self.server_id.clone();
        let server_name = self.server_name.clone();
        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = store.create(&server_id, &server_name, &working_dir);
            let _ = tx.send(result);
        });
        self.create_task = Some(rx);
    }

    /// Extract a backup into the server folder on a background thread.
    /// `wipe` deletes everything in the server folder first.
    fn start_restore(&mut self, name: &str, wipe: bool) {
        if self.busy() {
            return;
        }
        self.error = None;
        self.status = None;
        self.status_expires = None;
        let store = self.store.clone();
        let server_id = self.server_id.clone();
        let name = name.to_string();
        let working_dir = self.working_dir.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = store
                .restore(&server_id, &name, &working_dir, wipe)
                .map(|()| name.clone());
            let _ = tx.send(result);
        });
        self.restore_task = Some(rx);
    }

    /// Drain finished create/restore tasks; refreshes the listing when a
    /// backup was created. Returns true when a restore finished successfully,
    /// so the caller can refresh state derived from the server folder.
    fn poll(&mut self) -> bool {
        if let Some(rx) = &self.create_task {
            match rx.try_recv() {
                Ok(Ok(backup)) => {
                    self.create_task = None;
                    self.set_status(format!("Created {}", backup.name));
                    self.refresh();
                }
                Ok(Err(err)) => {
                    self.create_task = None;
                    self.error = Some(err);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.create_task = None;
                    self.error = Some("Backup stopped unexpectedly.".to_string());
                }
            }
        }
        let mut restored = false;
        if let Some(rx) = &self.restore_task {
            match rx.try_recv() {
                Ok(Ok(name)) => {
                    self.restore_task = None;
                    self.set_status(format!("Restored {name}"));
                    restored = true;
                }
                Ok(Err(err)) => {
                    self.restore_task = None;
                    self.error = Some(err);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.restore_task = None;
                    self.error = Some("Restore stopped unexpectedly.".to_string());
                }
            }
        }
        restored
    }

    fn set_status(&mut self, status: String) {
        self.status = Some(status);
        self.status_expires = Some(Instant::now() + BACKUP_STATUS_DURATION);
    }

    fn poll_status(&mut self, ctx: &egui::Context) {
        let Some(expires) = self.status_expires else {
            return;
        };
        match expires.checked_duration_since(Instant::now()) {
            Some(remaining) => ctx.request_repaint_after(remaining),
            None => {
                self.status = None;
                self.status_expires = None;
            }
        }
    }

    fn delete_backup(&mut self, name: &str) {
        match self.store.remove(&self.server_id, name) {
            Ok(()) => {
                self.confirm_delete = None;
                self.refresh();
            }
            Err(err) => self.error = Some(err),
        }
    }
}

enum JavaInstallEvent {
    Progress(u64, u64),
    Done(Result<JavaRuntime, String>),
}

/// State for the app settings dialog's Java runtime manager.
struct JavaDialog {
    candidates_rx: Option<Receiver<Result<Vec<JavaCandidate>, String>>>,
    candidates: Option<Result<Vec<JavaCandidate>, String>>,
    install_rx: Option<Receiver<JavaInstallEvent>>,
    /// Label of the runtime currently downloading, e.g. "Java 21".
    installing: Option<String>,
    install_progress: (u64, u64),
    error: Option<String>,
}

impl JavaDialog {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_candidates());
        });
        Self {
            candidates_rx: Some(rx),
            candidates: None,
            install_rx: None,
            installing: None,
            install_progress: (0, 0),
            error: None,
        }
    }

    fn refetch(&mut self) {
        *self = Self::new();
    }

    fn busy(&self) -> bool {
        self.installing.is_some()
    }

    /// Drain pending events; returns the finished install result, if one
    /// completed since the last call.
    fn poll(&mut self) -> Option<Result<JavaRuntime, String>> {
        if let Some(rx) = &self.candidates_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.candidates = Some(result);
                    self.candidates_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.candidates =
                        Some(Err("Connection lost while fetching Java versions.".into()));
                    self.candidates_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.install_rx {
            let mut finished = None;
            while let Ok(event) = rx.try_recv() {
                match event {
                    JavaInstallEvent::Progress(done, total) => {
                        self.install_progress = (done, total)
                    }
                    JavaInstallEvent::Done(result) => finished = Some(result),
                }
            }
            if let Some(result) = finished {
                self.install_rx = None;
                self.installing = None;
                match &result {
                    Ok(_runtime) => self.error = None,
                    Err(err) => self.error = Some(err.clone()),
                }
                return Some(result);
            }
        }
        None
    }

    fn start_install(&mut self, store: JavaStore, candidate: JavaCandidate) {
        self.error = None;
        self.installing = Some(format!("Java {}", candidate.feature));
        self.install_progress = (0, 0);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut progress = |done: u64, total: u64| {
                let _ = tx.send(JavaInstallEvent::Progress(done, total));
            };
            let result = store.install(&candidate, &mut progress);
            let _ = tx.send(JavaInstallEvent::Done(result));
        });
        self.install_rx = Some(rx);
    }
}

/// Minecraft §-code colors (code → color, name) in palette order.
const MOTD_COLORS: [(char, egui::Color32, &str); 16] = [
    ('0', egui::Color32::from_rgb(0x00, 0x00, 0x00), "Black"),
    ('1', egui::Color32::from_rgb(0x00, 0x00, 0xAA), "Dark Blue"),
    ('2', egui::Color32::from_rgb(0x00, 0xAA, 0x00), "Dark Green"),
    ('3', egui::Color32::from_rgb(0x00, 0xAA, 0xAA), "Dark Aqua"),
    ('4', egui::Color32::from_rgb(0xAA, 0x00, 0x00), "Dark Red"),
    (
        '5',
        egui::Color32::from_rgb(0xAA, 0x00, 0xAA),
        "Dark Purple",
    ),
    ('6', egui::Color32::from_rgb(0xFF, 0xAA, 0x00), "Gold"),
    ('7', egui::Color32::from_rgb(0xAA, 0xAA, 0xAA), "Gray"),
    ('8', egui::Color32::from_rgb(0x55, 0x55, 0x55), "Dark Gray"),
    ('9', egui::Color32::from_rgb(0x55, 0x55, 0xFF), "Blue"),
    ('a', egui::Color32::from_rgb(0x55, 0xFF, 0x55), "Green"),
    ('b', egui::Color32::from_rgb(0x55, 0xFF, 0xFF), "Aqua"),
    ('c', egui::Color32::from_rgb(0xFF, 0x55, 0x55), "Red"),
    (
        'd',
        egui::Color32::from_rgb(0xFF, 0x55, 0xFF),
        "Light Purple",
    ),
    ('e', egui::Color32::from_rgb(0xFF, 0xFF, 0x55), "Yellow"),
    ('f', egui::Color32::from_rgb(0xFF, 0xFF, 0xFF), "White"),
];

/// Formatting codes the palette offers besides colors: §l bold, §o italic,
/// §n underline, §m strikethrough, §r reset (§k obfuscation has no static
/// preview and is intentionally not insertable).
const MOTD_FORMATS: [(char, &str, &str); 5] = [
    ('l', "B", "Bold"),
    ('o', "I", "Italic"),
    ('n', "U", "Underline"),
    ('m', "S", "Strikethrough"),
    ('r', "R", "Reset"),
];

/// One run of MOTD text sharing the same formatting.
#[derive(Debug, PartialEq)]
struct MotdRun {
    text: String,
    color: Option<char>,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

/// Splits a MOTD into runs at `§` formatting codes. Unknown or trailing `§`
/// characters stay literal.
fn parse_motd(text: &str) -> Vec<MotdRun> {
    let mut runs = Vec::new();
    let mut run = MotdRun {
        text: String::new(),
        color: None,
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{00a7}' {
            if let Some(&code) = chars.peek() {
                let known = MOTD_COLORS.iter().any(|(c, _, _)| *c == code)
                    || MOTD_FORMATS.iter().any(|(c, _, _)| *c == code)
                    || code == 'k';
                if known {
                    chars.next();
                    if !run.text.is_empty() {
                        let carry_color = run.color;
                        runs.push(run);
                        run = MotdRun {
                            text: String::new(),
                            color: carry_color,
                            bold: false,
                            italic: false,
                            underline: false,
                            strikethrough: false,
                        };
                    }
                    match code {
                        'k' => {}
                        'l' => run.bold = true,
                        'o' => run.italic = true,
                        'n' => run.underline = true,
                        'm' => run.strikethrough = true,
                        // Color codes reset other formatting, like in game.
                        'r' => {
                            run.color = None;
                            run.bold = false;
                            run.italic = false;
                            run.underline = false;
                            run.strikethrough = false;
                        }
                        color => {
                            run.color = Some(color);
                            run.bold = false;
                            run.italic = false;
                            run.underline = false;
                            run.strikethrough = false;
                        }
                    }
                    continue;
                }
            }
        }
        run.text.push(c);
    }
    if !run.text.is_empty() {
        runs.push(run);
    }
    runs
}

/// Layout job rendering a MOTD the way the in-game server list would (within
/// egui's font limits: bold uses the heavier display font, obfuscation is
/// shown unformatted).
fn motd_layout_job(text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    for run in parse_motd(text) {
        let color = run
            .color
            .and_then(|code| MOTD_COLORS.iter().find(|(c, _, _)| *c == code))
            .map(|(_, color, _)| *color)
            .unwrap_or(egui::Color32::WHITE);
        let family = if run.bold {
            theme::title_family()
        } else {
            egui::FontFamily::Proportional
        };
        job.append(
            &run.text,
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::new(15.0, family),
                color,
                italics: run.italic,
                underline: if run.underline {
                    egui::Stroke::new(1.0_f32, color)
                } else {
                    egui::Stroke::NONE
                },
                strikethrough: if run.strikethrough {
                    egui::Stroke::new(1.0_f32, color)
                } else {
                    egui::Stroke::NONE
                },
                ..Default::default()
            },
        );
    }
    job
}

/// Inserts `§code` into `text` at the char-index cursor (appends when the
/// cursor is unknown or out of bounds).
fn insert_motd_code(text: &mut String, cursor: Option<usize>, code: char) {
    let len = text.chars().count();
    let pos = cursor.unwrap_or(len).min(len);
    let byte = text
        .char_indices()
        .nth(pos)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text.insert(byte, '\u{00a7}');
    text.insert(byte + '\u{00a7}'.len_utf8(), code);
}

/// Resource pack URLs must be empty or an absolute http(s) URL.
fn valid_resource_pack_url(url: &str) -> bool {
    let url = url.trim();
    url.is_empty() || url.starts_with("https://") || url.starts_with("http://")
}

/// Server name card with inline validation feedback.
fn render_settings_name_card(ui: &mut egui::Ui, dialog: &mut SettingsDialog) {
    theme::card().show(ui, |ui| {
        ui.label(
            egui::RichText::new("Name")
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        let name_response =
            ui.add(egui::TextEdit::singleline(&mut dialog.name).desired_width(f32::INFINITY));
        if name_response.changed() {
            dialog.name_error = None;
            dialog.saved_until = None;
        }
        if let Some(err) = &dialog.name_error {
            ui.label(egui::RichText::new(err).color(theme::ERROR));
        }
    });
}

/// Java runtime card: per-server override picked from the installed runtimes.
fn render_settings_java_card(
    ui: &mut egui::Ui,
    dialog: &mut SettingsDialog,
    running: bool,
    app_java_label: &str,
    installed: &[JavaRuntime],
) {
    theme::card().show(ui, |ui| {
        ui.add_enabled_ui(!running, |ui| {
            let mut draft = dialog.java_runtime.clone();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if ui
                    .add(egui::RadioButton::new(draft.is_none(), ""))
                    .clicked()
                {
                    draft = None;
                }
                ui.label(theme::icon(theme::icons::JAVA, 14.0).color(theme::TEXT_MUTED));
                ui.label(
                    egui::RichText::new(format!("App default ({app_java_label})"))
                        .color(theme::TEXT),
                );
            });
            for runtime in installed {
                let id = Some(runtime.id.clone());
                let is_selected = draft == id;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if ui.add(egui::RadioButton::new(is_selected, "")).clicked() {
                        draft = id.clone();
                    }
                    ui.label(theme::icon(theme::icons::JAVA, 14.0).color(theme::PRIMARY));
                    ui.label(
                        egui::RichText::new(format!(
                            "Java {} · {}",
                            runtime.feature, runtime.release
                        ))
                        .color(theme::TEXT),
                    );
                });
            }
            if draft != dialog.java_runtime {
                dialog.java_runtime = draft;
            }
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Applied on next start")
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
    });
}

/// Memory card: min/max RAM steppers plus the live usage readout.
fn render_settings_memory_card(
    ui: &mut egui::Ui,
    dialog: &mut SettingsDialog,
    running: bool,
    usage: &str,
    effective_java_label: &str,
) {
    theme::card().show(ui, |ui| {
        ui.add_enabled_ui(!running, |ui| {
            memory_stepper(
                ui,
                "Minimum RAM",
                &mut dialog.min_memory_mb,
                128,
                dialog.max_memory_mb.max(128),
            );
            ui.add_space(6.0);
            memory_stepper(ui, "Maximum RAM", &mut dialog.max_memory_mb, 512, 65_536);
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!(
                    "Applied on next start · launched with {effective_java_label}"
                ))
                .size(12.0)
                .color(theme::TEXT_MUTED),
            );
        });
        if running {
            ui.label(
                egui::RichText::new(format!("Using {usage} right now"))
                    .size(12.5)
                    .color(theme::PRIMARY),
            );
        }
    });
}

/// MOTD field (multi-line, like the in-game server list) with a §-code
/// palette and a live preview.
fn render_motd_editor(ui: &mut egui::Ui, dialog: &mut SettingsDialog) {
    let edit_id = egui::Id::new("settings_motd_edit");
    let output = egui::TextEdit::multiline(&mut dialog.motd)
        .id(edit_id)
        .desired_rows(2)
        .desired_width(f32::INFINITY)
        .hint_text("A Minecraft Server")
        .show(ui);
    if let Some(range) = output.cursor_range {
        let primary = range.primary.ccursor.index;
        let secondary = range.secondary.ccursor.index;
        dialog.motd_cursor = Some((primary.min(secondary), primary.max(secondary)));
    }

    ui.add_space(6.0);
    render_motd_palette(ui, dialog);
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::CONSOLE_BG)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if dialog.motd.is_empty() {
                ui.label(
                    egui::RichText::new("Minecraft server list preview")
                        .size(15.0)
                        .color(theme::TEXT_MUTED),
                );
            } else {
                ui.label(motd_layout_job(&dialog.motd));
            }
        });
}

/// The MOTD formatting palette: color swatches plus style buttons. Picking
/// one inserts its `§` code at the text cursor.
fn render_motd_palette(ui: &mut egui::Ui, dialog: &mut SettingsDialog) {
    let edit_id = egui::Id::new("settings_motd_edit");
    let mut picked: Option<char> = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
        for (code, color, name) in MOTD_COLORS {
            if motd_swatch(ui, code, color, name) {
                picked = Some(code);
            }
        }
        ui.add_space(4.0);
        for (code, label, tooltip) in MOTD_FORMATS {
            if motd_format_button(ui, label, tooltip) {
                picked = Some(code);
            }
        }
    });
    if let Some(code) = picked {
        // Insert in front of the highlighted text so the selection is colored.
        let pos = dialog
            .motd_cursor
            .map(|(start, _)| start)
            .unwrap_or_else(|| dialog.motd.chars().count());
        let pos = pos.min(dialog.motd.chars().count());
        insert_motd_code(&mut dialog.motd, Some(pos), code);
        dialog.saved_until = None;
        let cursor = pos + 2;
        dialog.motd_cursor = Some((cursor, cursor));
        // Keep the text cursor behind the inserted code so colors can be chained.
        if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), edit_id) {
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
            state.store(ui.ctx(), edit_id);
        }
    }
}

/// A MOTD palette color swatch; returns true when picked.
fn motd_swatch(ui: &mut egui::Ui, code: char, color: egui::Color32, name: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    let fill = theme::animated_hover_fill(
        ui,
        response.id,
        response.hovered(),
        color,
        color.linear_multiply(1.2),
    );
    ui.painter().rect_filled(rect, 4, fill);
    ui.painter().rect_stroke(
        rect.shrink(0.5),
        4,
        egui::Stroke::new(1.0_f32, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!("{name} (§{code})"))
        .clicked()
}

/// A MOTD palette formatting button (bold/italic/…); returns true when picked.
fn motd_format_button(ui: &mut egui::Ui, label: &str, tooltip: &str) -> bool {
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(12.5))
            .min_size(egui::vec2(22.0, 18.0))
            .corner_radius(4),
    )
    .on_hover_text(tooltip)
    .clicked()
}

/// Resource pack URL field with inline validation feedback.
fn render_resource_pack_field(ui: &mut egui::Ui, dialog: &mut SettingsDialog) {
    ui.label(
        egui::RichText::new("Resource Pack URL")
            .size(12.0)
            .color(theme::TEXT_MUTED),
    );
    let response = ui.add(
        egui::TextEdit::singleline(&mut dialog.resource_pack)
            .desired_width(f32::INFINITY)
            .hint_text("https://example.com/resource-pack.zip"),
    );
    if response.changed() {
        dialog.resource_pack_error = None;
        dialog.saved_until = None;
    }
    if let Some(error) = &dialog.resource_pack_error {
        ui.label(egui::RichText::new(error).color(theme::ERROR));
    }
}

/// Server icon picker: a preview of the staged choice plus tiles for none, the
/// current icon, the built-in presets and a custom image from the user's files.
fn render_icon_selector(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    dialog: &mut SettingsDialog,
    working_dir: &Path,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let (preview_rect, _) =
            ui.allocate_exact_size(egui::vec2(64.0, 64.0), egui::Sense::hover());
        ui.painter().rect_filled(preview_rect, 6, theme::CONSOLE_BG);
        if let Some(texture) = staged_icon_texture(ctx, dialog, working_dir) {
            paint_icon(ui.painter(), preview_rect.shrink(2.0), &texture);
        }

        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                if icon_option_tile(
                    ui,
                    "icon_none",
                    "None",
                    dialog.icon == IconChoice::None,
                    |painter, rect| {
                        painter.rect_filled(rect, 4, theme::CONSOLE_BG);
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            theme::icons::XMARK,
                            egui::FontId::new(14.0, theme::icon_family()),
                            theme::TEXT_MUTED,
                        );
                    },
                ) {
                    dialog.icon = IconChoice::None;
                    dialog.icon_error = None;
                    dialog.saved_until = None;
                }
                if dialog.has_icon_file {
                    let texture =
                        icon_texture(ctx, dialog, "current", IconSource::Current(working_dir));
                    let tooltip = dialog
                        .current_icon_preset
                        .map(|name| format!("Current ({name})"))
                        .unwrap_or_else(|| "Current".to_string());
                    let clicked = icon_option_tile(
                        ui,
                        "icon_current",
                        &tooltip,
                        dialog.icon == IconChoice::Keep,
                        |painter, rect| {
                            if let Some(texture) = &texture {
                                paint_icon(painter, rect, texture);
                            }
                        },
                    );
                    if clicked {
                        dialog.icon = IconChoice::Keep;
                        dialog.icon_error = None;
                        dialog.saved_until = None;
                    }
                }
                for (name, bytes) in PRESET_SERVER_ICONS {
                    if dialog.current_icon_preset == Some(name) {
                        continue;
                    }
                    let texture = icon_texture(
                        ctx,
                        dialog,
                        &format!("preset:{name}"),
                        IconSource::Bytes(name, bytes),
                    );
                    let clicked = icon_option_tile(
                        ui,
                        &format!("icon_preset_{name}"),
                        name,
                        dialog.icon == IconChoice::Preset(name),
                        |painter, rect| {
                            if let Some(texture) = &texture {
                                paint_icon(painter, rect, texture);
                            }
                        },
                    );
                    if clicked {
                        dialog.icon = IconChoice::Preset(name);
                        dialog.icon_error = None;
                        dialog.saved_until = None;
                    }
                }
                if icon_option_tile(
                    ui,
                    "icon_custom",
                    "Custom",
                    matches!(dialog.icon, IconChoice::Custom(_)),
                    |painter, rect| {
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            theme::icons::FOLDER_OPEN,
                            egui::FontId::new(16.0, theme::icon_family()),
                            theme::TEXT_MUTED,
                        );
                    },
                ) {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg"])
                        .pick_file()
                    {
                        dialog
                            .icon_previews
                            .retain(|key, _| !key.starts_with("custom:"));
                        dialog.icon = IconChoice::Custom(path);
                        dialog.icon_error = None;
                        dialog.saved_until = None;
                    }
                }
            });
            if let Some(error) = &dialog.icon_error {
                ui.label(egui::RichText::new(error).color(theme::ERROR));
            }
        });
    });
}

/// Texture for the icon choice currently staged in the dialog.
fn staged_icon_texture(
    ctx: &egui::Context,
    dialog: &mut SettingsDialog,
    working_dir: &Path,
) -> Option<egui::TextureHandle> {
    match dialog.icon.clone() {
        IconChoice::Keep if dialog.has_icon_file => {
            icon_texture(ctx, dialog, "current", IconSource::Current(working_dir))
        }
        IconChoice::Preset(name) => {
            let bytes = preset_icon_bytes(name)?;
            icon_texture(
                ctx,
                dialog,
                &format!("preset:{name}"),
                IconSource::Bytes(name, bytes),
            )
        }
        IconChoice::Custom(path) => icon_texture(
            ctx,
            dialog,
            &format!("custom:{}", path.display()),
            IconSource::Custom(path),
        ),
        _ => None,
    }
}

/// Where an icon texture's pixels come from.
enum IconSource<'a> {
    /// Embedded preset PNG bytes (name, bytes).
    Bytes(&'a str, &'a [u8]),
    /// The server's existing server-icon.png.
    Current(&'a Path),
    /// A user-picked image file (staged until save).
    Custom(PathBuf),
}

impl IconSource<'_> {
    fn load(self, ctx: &egui::Context, dialog: &mut SettingsDialog) -> Option<egui::TextureHandle> {
        let invalid = |dialog: &mut SettingsDialog, what: String| {
            dialog.icon_error = Some(format!("{what} is not a usable image."));
            None
        };
        match self {
            IconSource::Bytes(name, bytes) => icon_texture_from_bytes(ctx, name, bytes)
                .or_else(|| invalid(dialog, "The preset icon".to_string())),
            IconSource::Current(dir) => match read_server_icon(dir) {
                Ok(Some(bytes)) => icon_texture_from_bytes(ctx, "server_icon_current", &bytes)
                    .or_else(|| invalid(dialog, "The existing icon file".to_string())),
                Ok(None) => None,
                Err(err) => {
                    dialog.icon_error = Some(format!("Could not read the icon: {err}"));
                    None
                }
            },
            IconSource::Custom(path) => match fs::read(&path) {
                Ok(bytes) => icon_texture_from_bytes(ctx, "server_icon_custom", &bytes)
                    .or_else(|| invalid(dialog, format!("{}", path.display()))),
                Err(err) => {
                    dialog.icon_error = Some(format!("Could not read {}: {err}", path.display()));
                    None
                }
            },
        }
    }
}

/// Lazily decodes and caches a 64×64 texture for `source` under `key`.
fn icon_texture(
    ctx: &egui::Context,
    dialog: &mut SettingsDialog,
    key: &str,
    source: IconSource<'_>,
) -> Option<egui::TextureHandle> {
    if let Some(texture) = dialog.icon_previews.get(key) {
        return texture.clone();
    }
    let texture = source.load(ctx, dialog);
    dialog
        .icon_previews
        .insert(key.to_string(), texture.clone());
    texture
}

fn preset_icon_bytes(name: &str) -> Option<&'static [u8]> {
    PRESET_SERVER_ICONS
        .iter()
        .find(|(preset, _)| *preset == name)
        .map(|(_, bytes)| *bytes)
}

fn matching_preset_icon(working_dir: &Path) -> Result<Option<&'static str>, String> {
    let Some(bytes) =
        read_server_icon(working_dir).map_err(|err| format!("Could not read the icon: {err}"))?
    else {
        return Ok(None);
    };
    let current = image::load_from_memory(&bytes)
        .map_err(|err| format!("Could not decode the current icon: {err}"))?
        .to_rgba8();
    let current = if current.dimensions() == (64, 64) {
        current
    } else {
        spoon_core::fit_square(&current, 64)
    };
    for (name, bytes) in PRESET_SERVER_ICONS {
        let preset = image::load_from_memory(bytes)
            .map_err(|err| format!("Could not decode preset icon {name}: {err}"))?
            .to_rgba8();
        if current == spoon_core::fit_square(&preset, 64) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Decodes image bytes into a 64×64 texture (aspect preserved, like the file
/// that ends up next to the server jar).
fn icon_texture_from_bytes(
    ctx: &egui::Context,
    name: &str,
    bytes: &[u8],
) -> Option<egui::TextureHandle> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let image = spoon_core::fit_square(&image, 64);
    let size = [image.width() as usize, image.height() as usize];
    Some(ctx.load_texture(
        name.to_string(),
        egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

fn paint_icon(painter: &egui::Painter, rect: egui::Rect, texture: &egui::TextureHandle) {
    painter.image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

/// A clickable icon option tile; `content` paints the tile interior and
/// `tooltip` names the option on hover.
fn icon_option_tile(
    ui: &mut egui::Ui,
    id_salt: &str,
    tooltip: &str,
    selected: bool,
    content: impl FnOnce(&egui::Painter, egui::Rect),
) -> bool {
    let tile = egui::vec2(44.0, 44.0);
    let (rect, response) = ui.allocate_exact_size(tile, egui::Sense::click());
    let fill = theme::animated_hover_fill(
        ui,
        ui.id().with(id_salt),
        response.hovered(),
        theme::SURFACE_HOVER,
        theme::SURFACE_ACTIVE,
    );
    ui.painter().rect_filled(rect, theme::RADIUS, fill);
    content(ui.painter(), rect.shrink(5.0));
    ui.painter().rect_stroke(
        rect,
        theme::RADIUS,
        egui::Stroke::new(
            if selected { 1.6_f32 } else { 1.0_f32 },
            if selected {
                theme::PRIMARY
            } else {
                theme::BORDER
            },
        ),
        egui::StrokeKind::Inside,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tooltip)
        .clicked()
}

/// Applies the staged icon choice to the server folder when settings save.
fn apply_icon_choice(working_dir: &Path, choice: &IconChoice) -> Result<(), String> {
    match choice {
        IconChoice::Keep => Ok(()),
        IconChoice::None => clear_server_icon(working_dir)
            .map_err(|err| format!("Could not remove the icon: {err}")),
        IconChoice::Preset(name) => preset_icon_bytes(name)
            .ok_or_else(|| "Unknown preset icon.".to_string())
            .and_then(|bytes| {
                write_server_icon(bytes, working_dir)
                    .map_err(|err| format!("Could not save the icon: {err}"))
            }),
        IconChoice::Custom(path) => {
            let bytes = fs::read(path)
                .map_err(|err| format!("Could not read {}: {err}", path.display()))?;
            write_server_icon(&bytes, working_dir)
                .map_err(|err| format!("{} is not a usable image: {err}", path.display()))
        }
    }
}

struct SettingsDialog {
    server_id: String,
    name: String,
    name_error: Option<String>,
    min_memory_mb: u32,
    max_memory_mb: u32,
    /// Draft per-server Java override (`None` = app default).
    java_runtime: Option<String>,
    selected_type: Option<ServerType>,
    selected_version: Option<String>,
    /// Whether the draft version was checked against the loaded catalog yet.
    version_synced: bool,
    version_error: Option<String>,
    version_task: Option<Receiver<Result<(ServerType, String, Option<String>), String>>>,
    /// MOTD draft (`motd` in server.properties, may hold § formatting codes).
    motd: String,
    /// Whether server.properties already had a `motd` key, so clearing the
    /// draft can skip writing one and leave the server default untouched.
    had_motd: bool,
    /// Resource pack URL draft (`resource-pack` in server.properties).
    resource_pack: String,
    had_resource_pack: bool,
    resource_pack_error: Option<String>,
    /// Server icon draft; applied to `server-icon.png` on save.
    icon: IconChoice,
    /// Whether the server folder currently has an icon file.
    has_icon_file: bool,
    /// Preset whose pixels match the saved icon, so the picker can avoid a
    /// duplicate option tile.
    current_icon_preset: Option<&'static str>,
    icon_error: Option<String>,
    /// Lazily loaded 64×64 icon previews, keyed by source ("current",
    /// "preset:<name>", "custom:<path>"); `None` marks a failed decode.
    icon_previews: HashMap<String, Option<egui::TextureHandle>>,
    /// Last MOTD selection as `(start, end)` char indices, retained while the
    /// palette takes focus; palette codes insert at `start`.
    motd_cursor: Option<(usize, usize)>,
    /// Expiration time for the temporary save-button confirmation.
    saved_until: Option<Instant>,
}

/// What to do with the server's `server-icon.png` when settings are saved.
#[derive(Clone, PartialEq)]
enum IconChoice {
    /// Leave the existing icon file untouched.
    Keep,
    /// Remove the icon.
    None,
    /// Write one of the built-in preset icons.
    Preset(&'static str),
    /// Import a user-picked image file (resized to 64×64).
    Custom(PathBuf),
}

impl SettingsDialog {
    fn new(config: &ServerConfig) -> Self {
        let properties_path = config.working_dir.join("server.properties");
        let motd = read_property(&properties_path, "motd");
        let resource_pack = read_property(&properties_path, "resource-pack");
        let has_icon_file = server_icon_path(&config.working_dir).is_file();
        let (current_icon_preset, icon_error) = if has_icon_file {
            match matching_preset_icon(&config.working_dir) {
                Ok(preset) => (preset, None),
                Err(err) => (None, Some(err)),
            }
        } else {
            (None, None)
        };
        Self {
            server_id: config.id.clone(),
            name: config.name.clone(),
            name_error: None,
            min_memory_mb: config.min_memory_mb,
            max_memory_mb: config.max_memory_mb,
            java_runtime: config.java_runtime.clone(),
            selected_type: config.server_type.as_deref().map(ServerType::from_name),
            selected_version: config.server_version.clone(),
            version_synced: false,
            version_error: None,
            version_task: None,
            motd: motd.clone().unwrap_or_default(),
            had_motd: motd.is_some(),
            resource_pack: resource_pack.clone().unwrap_or_default(),
            had_resource_pack: resource_pack.is_some(),
            resource_pack_error: None,
            icon: IconChoice::Keep,
            has_icon_file,
            current_icon_preset,
            icon_error,
            icon_previews: HashMap::new(),
            motd_cursor: None,
            saved_until: None,
        }
    }

    /// Once the catalog is loaded, makes sure the draft version exists for the
    /// draft software (falling back to the newest).
    fn sync_version(&mut self, software: &SoftwareCache) {
        if self.version_synced || software.snapshot.is_none() {
            return;
        }
        self.version_synced = true;
        if let Some(server_type) = &self.selected_type {
            let versions = software.versions(server_type);
            self.reconcile_version(versions);
        }
    }

    /// Switches the draft software, keeping the draft version when the new
    /// software offers it and otherwise picking its newest.
    fn select_type(&mut self, server_type: ServerType, software: &SoftwareCache) {
        self.version_error = None;
        self.reconcile_version(software.versions(&server_type));
        self.selected_type = Some(server_type);
    }

    fn reconcile_version(&mut self, versions: &[VersionEntry]) {
        if !self
            .selected_version
            .as_ref()
            .is_some_and(|selected| versions.iter().any(|v| &v.id == selected))
        {
            self.selected_version = versions.last().map(|v| v.id.clone());
        }
    }

    fn start_version_change(&mut self, config: ServerConfig) {
        let (Some(server_type), Some(version)) =
            (self.selected_type.clone(), self.selected_version.clone())
        else {
            return;
        };
        self.version_error = None;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = install_server_version(&config, &server_type, &version)
                .map(|warning| (server_type, version, warning));
            let _ = tx.send(result);
        });
        self.version_task = Some(rx);
    }

    fn take_version_result(
        &mut self,
    ) -> Option<Result<(ServerType, String, Option<String>), String>> {
        let rx = self.version_task.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.version_task = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.version_task = None;
                Some(Err("Version installation task stopped unexpectedly.".into()))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }
}

enum DownloadEvent {
    Progress(u64, u64),
    Done(Result<(), String>),
}

struct PendingCreate {
    config: ServerConfig,
    rx: Receiver<DownloadEvent>,
    downloaded: u64,
    total: u64,
    finished: Option<Result<(), String>>,
}

/// The full-page "Create Server" form: software/version pickers plus the
/// world and network settings written to server.properties on creation.
pub struct CreatePage {
    name: String,
    max_memory_mb: u32,
    jvm_args: String,
    gamemode: String,
    difficulty: String,
    level_type: String,
    max_players: u32,
    server_port: u16,
    level_seed: String,
    online_mode: bool,
    use_whitelist: bool,
    software_filter: String,
    version_filter: String,
    error: Option<String>,

    selected_type: Option<ServerType>,
    selected_version: Option<String>,

    pending: Option<PendingCreate>,
}

impl CreatePage {
    fn new(suggested_port: u16) -> Self {
        Self {
            name: "Server".to_string(),
            max_memory_mb: 4096,
            jvm_args: String::new(),
            gamemode: "survival".to_string(),
            difficulty: "easy".to_string(),
            level_type: "minecraft:normal".to_string(),
            max_players: 20,
            server_port: suggested_port,
            level_seed: String::new(),
            online_mode: true,
            use_whitelist: true,
            software_filter: String::new(),
            version_filter: String::new(),
            error: None,
            // Paper is the most common choice; the version defaults to the
            // newest build once the catalog has loaded.
            selected_type: Some(ServerType::from_name("PAPER")),
            selected_version: None,
            pending: None,
        }
    }

    fn is_downloading(&self) -> bool {
        self.pending.is_some()
    }

    /// Drain any in-flight download events. Called once per frame.
    fn poll(&mut self) {
        if let Some(pending) = &mut self.pending {
            for event in pending.rx.try_iter() {
                match event {
                    DownloadEvent::Progress(downloaded, total) => {
                        pending.downloaded = downloaded;
                        pending.total = total;
                    }
                    DownloadEvent::Done(result) => {
                        pending.finished = Some(result);
                    }
                }
            }
        }
    }

    fn take_finished(&mut self) -> Option<(ServerConfig, Result<(), String>)> {
        self.pending.as_mut()?;
        let finished = self.pending.as_mut()?.finished.take()?;
        let pending = self.pending.take().unwrap();
        Some((pending.config, finished))
    }

    fn start_create(&mut self, servers_root: &Path, assigned_ports: &[u16]) {
        self.error = None;

        let name = self.name.trim().to_string();
        if name.is_empty() {
            self.error = Some("Name is required.".to_string());
            return;
        }
        let (Some(server_type), Some(version)) =
            (self.selected_type.clone(), self.selected_version.clone())
        else {
            self.error = Some("Select a server software and version.".to_string());
            return;
        };
        if assigned_ports.contains(&self.server_port)
            || !spoon_core::port::is_port_available(self.server_port)
        {
            let suggestion = next_available_port(DEFAULT_SERVER_PORT, assigned_ports);
            self.error = Some(format!(
                "Port {} is already in use; try {}.",
                self.server_port, suggestion
            ));
            return;
        }

        let dir = unique_server_dir(servers_root, &name);
        if let Err(err) = fs::create_dir_all(&dir) {
            self.error = Some(format!("Could not create server folder: {err}"));
            return;
        }

        let mut config = ServerConfig::new(
            new_id(),
            name,
            dir.join("server.jar"),
            dir,
            self.max_memory_mb,
        );
        config.jvm_args = self
            .jvm_args
            .split_whitespace()
            .map(str::to_string)
            .collect();
        config.server_type = Some(server_type.api_name());
        config.server_version = Some(version.clone());
        config.gamemode = self.gamemode.clone();
        config.difficulty = self.difficulty.clone();
        config.level_type = self.level_type.clone();
        config.max_players = self.max_players;
        config.server_port = self.server_port;
        config.level_seed = self.level_seed.trim().to_string();
        config.online_mode = self.online_mode;
        config.use_whitelist = self.use_whitelist;

        let (tx, rx) = mpsc::channel();
        let thread_config = config.clone();
        let thread_type = server_type.clone();
        std::thread::spawn(move || {
            let result =
                download_and_install(&thread_type, &version, &thread_config.working_dir, &tx)
                    .and_then(|_| {
                        write_server_properties(&thread_config).map_err(|err| err.to_string())
                    });
            let _ = tx.send(DownloadEvent::Done(result));
        });

        self.pending = Some(PendingCreate {
            config,
            rx,
            downloaded: 0,
            total: 0,
            finished: None,
        });
    }
}

fn download_and_install(
    server_type: &ServerType,
    version: &str,
    dir: &Path,
    tx: &mpsc::Sender<DownloadEvent>,
) -> Result<(), String> {
    let client = McJarsClient::new();
    let builds = client
        .builds(server_type, version)
        .map_err(|err| err.to_string())?;
    let build = builds
        .into_iter()
        .next()
        .ok_or_else(|| format!("No builds found for {version}"))?;
    client
        .install_build(&build, dir, &mut |downloaded, total| {
            let _ = tx.send(DownloadEvent::Progress(downloaded, total));
        })
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn select_server_jar(folder: &Path) -> Result<Option<PathBuf>, String> {
    let mut jars = Vec::new();
    for entry in
        fs::read_dir(folder).map_err(|err| format!("Could not read server folder: {err}"))?
    {
        let entry = entry.map_err(|err| format!("Could not inspect server folder: {err}"))?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        {
            jars.push(path);
        }
    }
    jars.sort();
    let jar_path = if let Some(server_jar) = jars
        .iter()
        .find(|path| path.file_name().is_some_and(|name| name == "server.jar"))
    {
        server_jar.clone()
    } else if jars.len() == 1 {
        jars.remove(0)
    } else {
        let Some(jar) = rfd::FileDialog::new()
            .set_title("Select the server JAR")
            .set_directory(folder)
            .add_filter("Java archive", &["jar"])
            .pick_file()
        else {
            return Ok(None);
        };
        jar
    };
    let jar_path =
        fs::canonicalize(&jar_path).map_err(|err| format!("Could not open server JAR: {err}"))?;
    if jar_path.parent() != Some(folder) {
        return Err("The selected server JAR must be directly inside its server folder.".into());
    }
    Ok(Some(jar_path))
}

fn install_server_version(
    config: &ServerConfig,
    server_type: &ServerType,
    version: &str,
) -> Result<Option<String>, String> {
    let staging_dir = std::env::temp_dir()
        .join("Spoon")
        .join(format!("version-{}", new_id()));
    fs::create_dir_all(&staging_dir)
        .map_err(|err| format!("Could not create version staging folder: {err}"))?;

    let result = (|| {
        let client = McJarsClient::new();
        let build = client
            .builds(server_type, version)
            .map_err(|err| err.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| format!("No builds found for {version}"))?;
        let new_jar = client
            .install_build(&build, &staging_dir, &mut |_, _| {})
            .map_err(|err| err.to_string())?;

        fs::create_dir_all(&config.working_dir)
            .map_err(|err| format!("Could not access server folder: {err}"))?;
        files::copy_directory_contents(&staging_dir, &config.working_dir, true)
            .map_err(|err| format!("Could not install server files: {err}"))?;

        let target = &config.jar_path;
        let parent = target
            .parent()
            .ok_or_else(|| "Server JAR path has no parent folder.".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|err| format!("Could not access server JAR folder: {err}"))?;
        let suffix = new_id();
        let replacement = parent.join(format!(".spoon-{suffix}.jar"));
        let backup = parent.join(format!(".spoon-{suffix}.backup"));
        if let Err(err) = fs::copy(&new_jar, &replacement) {
            let cleanup = fs::remove_file(&replacement)
                .err()
                .filter(|cleanup| cleanup.kind() != std::io::ErrorKind::NotFound);
            return Err(match cleanup {
                Some(cleanup) => format!(
                    "Could not prepare the new server JAR: {err}; temporary file cleanup failed: {cleanup}"
                ),
                None => format!("Could not prepare the new server JAR: {err}"),
            });
        }

        let had_old_jar = target.exists();
        if had_old_jar {
            if let Err(err) = fs::rename(target, &backup) {
                let _ = fs::remove_file(&replacement);
                return Err(format!("Could not back up the current server JAR: {err}"));
            }
        }

        if let Err(err) = fs::rename(&replacement, target) {
            let rollback_error = if had_old_jar {
                fs::rename(&backup, target).err()
            } else {
                None
            };
            let cleanup_error = fs::remove_file(&replacement)
                .err()
                .filter(|cleanup| cleanup.kind() != std::io::ErrorKind::NotFound);
            let mut message = format!("Could not activate the new server JAR: {err}");
            if let Some(rollback_error) = rollback_error {
                message.push_str(&format!(
                    "; restoring the previous JAR also failed ({}): {rollback_error}",
                    backup.display()
                ));
            }
            if let Some(cleanup_error) = cleanup_error {
                message.push_str(&format!("; temporary JAR cleanup failed: {cleanup_error}"));
            }
            return Err(message);
        }
        if had_old_jar {
            if let Err(err) = fs::remove_file(&backup) {
                return Ok(Some(format!(
                    "Version installed, but old JAR backup remains: {err}"
                )));
            }
        }
        Ok(None)
    })();

    match fs::remove_dir_all(&staging_dir) {
        Ok(()) => result,
        Err(err) if result.is_ok() => {
            let warning = result.ok().flatten();
            Ok(Some(match warning {
                Some(warning) => format!("{warning}; staging cleanup failed: {err}"),
                None => format!("Version installed, but staging cleanup failed: {err}"),
            }))
        }
        Err(err) => result.map_err(|error| format!("{error}; staging cleanup failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        file_path_breadcrumbs, format_duration, insert_motd_code, parse_motd,
        parse_online_player_list, show_create_page_scroll_area, sidebar_footer_height,
        update_online_players, valid_resource_pack_url, MotdRun,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn file_breadcrumbs_include_only_server_folder_ancestors() {
        let root = Path::new("server");
        let current = root.join("world").join("players");
        let breadcrumbs = file_path_breadcrumbs(root, &current, "Survival");

        assert_eq!(
            breadcrumbs
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>(),
            ["Survival", "world", "players"]
        );
        assert_eq!(
            breadcrumbs
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![root.to_path_buf(), root.join("world"), current]
        );
    }

    #[test]
    fn file_breadcrumbs_do_not_expose_paths_outside_server_root() {
        let root = Path::new("server");
        let breadcrumbs = file_path_breadcrumbs(root, Path::new("outside/world"), "Survival");

        assert_eq!(
            breadcrumbs,
            vec![(root.to_path_buf(), "Survival".to_string())]
        );
    }

    #[test]
    fn sidebar_footer_height_covers_every_button() {
        // Three button rows normally, four when the update notice is shown;
        // the pinned version label also needs room beneath the buttons.
        assert_eq!(
            sidebar_footer_height(false),
            36.0 * 3.0 + 8.0 * 3.0 + 18.0 + 4.0
        );
        assert_eq!(
            sidebar_footer_height(true),
            36.0 * 4.0 + 8.0 * 4.0 + 18.0 + 4.0
        );
        assert!(sidebar_footer_height(true) > sidebar_footer_height(false));
    }

    #[test]
    fn create_page_actions_stay_inside_visible_area() {
        let panel_bottom = Cell::new(None);
        let button_rects = RefCell::new(Vec::new());
        let fonts_ready = Cell::new(false);
        let mut harness = egui_kittest::Harness::new(|ctx| {
            super::theme::apply(ctx);
            if !fonts_ready.get() {
                fonts_ready.set(true);
                ctx.request_repaint();
                return;
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                panel_bottom.set(Some(ui.max_rect().bottom()));
                show_create_page_scroll_area(ui, 48.0, |ui| ui.set_min_height(900.0));
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut rects = button_rects.borrow_mut();
                    rects.push(
                        ui.add(super::theme::primary_icon_button(
                            super::theme::icons::CHECK,
                            "Create Server",
                        ))
                        .rect,
                    );
                    rects.push(
                        ui.add(super::theme::ghost_icon_button(
                            super::theme::icons::XMARK,
                            "Cancel",
                        ))
                        .rect,
                    );
                });
            });
        });
        harness.run_steps(2);

        let bottom = panel_bottom.get().expect("central panel rendered");
        for rect in button_rects.borrow().iter() {
            assert!(
                rect.bottom() <= bottom,
                "footer button extends below the visible panel: {:.1} > {:.1}",
                rect.bottom(),
                bottom
            );
        }
    }

    #[test]
    fn player_list_tracks_join_and_leave_console_events() {
        let mut players = BTreeSet::new();
        update_online_players(&mut players, "[Server thread/INFO]: Alex joined the game");
        update_online_players(&mut players, "[Server thread/INFO]: Sam joined the game");
        assert_eq!(players.iter().cloned().collect::<Vec<_>>(), ["Alex", "Sam"]);

        update_online_players(&mut players, "[Server thread/INFO]: Alex left the game");
        assert_eq!(players.iter().cloned().collect::<Vec<_>>(), ["Sam"]);
    }

    #[test]
    fn player_list_command_reconciles_current_players() {
        assert_eq!(
            parse_online_player_list(
                "[Server thread/INFO]: There are 2 of a max of 20 players online: Alex, Sam"
            ),
            Some(vec!["Alex".to_string(), "Sam".to_string()])
        );
        assert_eq!(
            parse_online_player_list(
                "[Server thread/INFO]: There are 0 of a max of 20 players online:"
            ),
            Some(Vec::new())
        );
        assert_eq!(
            parse_online_player_list("[Server thread/INFO]: Alex joined the game"),
            None
        );
    }

    #[test]
    fn player_catalog_refresh_does_not_block_mutation_state() {
        let dir = std::env::temp_dir().join(format!("spoon-player-refresh-{}", super::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = super::PlayerManagementState::new();
        state.refresh(dir.clone(), false);

        assert!(state.refresh_pending.is_some());
        assert!(state.pending.is_none());

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while state.poll_refresh() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(state.catalog.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn formats_uptime_as_readable_elapsed_time() {
        assert_eq!(format_duration(Duration::from_secs(7)), "7s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(format_duration(Duration::from_secs(3_723)), "1h 02m 03s");
    }

    fn run(text: &str, color: Option<char>) -> MotdRun {
        MotdRun {
            text: text.to_string(),
            color,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
        }
    }

    #[test]
    fn motd_parser_splits_at_formatting_codes() {
        assert_eq!(parse_motd("Plain"), vec![run("Plain", None)]);
        assert_eq!(parse_motd("§aGreen"), vec![run("Green", Some('a'))]);
        assert_eq!(
            parse_motd("§aGreen §9Blue"),
            vec![run("Green ", Some('a')), run("Blue", Some('9'))]
        );
        // A color code resets earlier formatting.
        assert_eq!(parse_motd("§l§cB"), vec![run("B", Some('c'))]);
        // Reset code and stray section signs stay literal.
        assert_eq!(parse_motd("§rHi"), vec![run("Hi", None)]);
        assert_eq!(parse_motd("50§ off §"), vec![run("50§ off §", None)]);
        // Formatting flags carry per run.
        let mut fancy = run("x", None);
        fancy.bold = true;
        fancy.italic = true;
        fancy.underline = true;
        fancy.strikethrough = true;
        assert_eq!(parse_motd("§l§o§n§mx"), vec![fancy]);
    }

    #[test]
    fn motd_code_inserts_at_char_cursor() {
        let mut text = "AB".to_string();
        insert_motd_code(&mut text, Some(1), 'a');
        assert_eq!(text, "A§aB");
        insert_motd_code(&mut text, None, 'l');
        assert_eq!(text, "A§aB§l");
        let mut beyond = "é".to_string();
        insert_motd_code(&mut beyond, Some(9), 'c');
        assert_eq!(beyond, "é§c");
    }

    /// Clicking palette swatches must put `§` codes into the MOTD draft — this
    /// is the exact path that writes colors into server.properties on save.
    #[test]
    fn motd_palette_click_inserts_format_code() {
        let dir = std::env::temp_dir().join(format!("spoon-motd-test-{}", super::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = super::ServerConfig::new(
            super::new_id(),
            "Test".into(),
            dir.join("server.jar"),
            dir.clone(),
            4096,
        );
        let dialog = RefCell::new(super::SettingsDialog::new(&config));
        dialog.borrow_mut().motd = "AB".into();
        dialog.borrow_mut().motd_cursor = Some((1, 2));
        let mut harness = egui_kittest::Harness::new(|ctx| {
            super::theme::apply(ctx);
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |ui| {
                    super::render_motd_palette(ui, &mut dialog.borrow_mut());
                });
        });
        harness.run_steps(1);

        // Swatches are 18×18 with 4pt spacing; centers at (9 + 22·i, 9).
        let click_swatch = |harness: &mut egui_kittest::Harness, index: usize| {
            let pos = egui::pos2(9.0 + 22.0 * index as f32, 9.0);
            harness
                .input_mut()
                .events
                .push(egui::Event::PointerMoved(pos));
            for pressed in [true, false] {
                harness.input_mut().events.push(egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            harness.step();
        };
        click_swatch(&mut harness, 1);
        assert_eq!(dialog.borrow().motd, "A\u{00a7}1B");
        click_swatch(&mut harness, 2);
        assert_eq!(dialog.borrow().motd, "A\u{00a7}1\u{00a7}2B");
        assert_eq!(dialog.borrow().motd_cursor, Some((5, 5)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_saved_preset_icon_is_detected() {
        let dir = std::env::temp_dir().join(format!("spoon-preset-test-{}", super::new_id()));
        super::write_server_icon(super::preset_icon_bytes("Spoon Classic").unwrap(), &dir).unwrap();
        assert_eq!(
            super::matching_preset_icon(&dir).unwrap(),
            Some("Spoon Classic")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn social_brand_font_contains_discord_and_kofi_glyphs() {
        let ctx = egui::Context::default();
        super::theme::apply(&ctx);
        let font_id = egui::FontId::new(16.0, super::theme::brand_icon_family());
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(ctx.fonts(|fonts| fonts.has_glyph(&font_id, '\u{f392}')));
            assert!(ctx.fonts(|fonts| fonts.has_glyph(&font_id, '\u{e856}')));
        });
    }

    #[test]
    fn resource_pack_url_accepts_empty_or_http_urls_only() {
        assert!(valid_resource_pack_url(""));
        assert!(valid_resource_pack_url(" https://example.com/p.zip "));
        assert!(valid_resource_pack_url("http://example.com/p.zip"));
        assert!(!valid_resource_pack_url("example.com/p.zip"));
        assert!(!valid_resource_pack_url("ftp://example.com/p.zip"));
    }
}
