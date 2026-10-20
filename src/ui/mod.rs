//! Interactive Terminal User Interface (Ratatui + Crossterm).

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::widgets::*;

mod chrome;
mod devices_tab;
pub mod features;
mod help;
pub mod kit;
mod mix_picker;
mod now_playing;
pub mod playlists;
mod queue_tab;
pub mod screen;
pub mod search;
mod search_view;
mod settings_tab;
pub mod tag_reads;
pub mod theme;
pub mod workers;

use self::search::SearchModalState;
use self::tag_reads::SharedTagReads;
use self::workers::WorkerRegistry;
use crate::discovery::DeviceManager;
use crate::paths;
use crate::player::local::LocalPlayer;
use crate::player::worker::AsyncPlayer;
use crate::player::AudioPlayer;
use crate::queue::PlaybackQueue;
use crate::settings::{setting, SettingsContext, SettingsManager, SETTING_DEFINITIONS, SETTING_DISPLAY_ORDER};

/// The [`SettingsContext`] built from an `AppUI`'s fields one by one (not through `&self`), so
/// `settings_mgr` can be borrowed mutably beside it: the Music Homes in memory, the music folder
/// once its worker has found it, the home folder the library shows as `~`, and the port the
/// running web server bound.
macro_rules! settings_context {
    ($app:expr) => {
        SettingsContext {
            bookmarks: &$app.bookmarks,
            music_dir: $app.music_dir.get().and_then(|dir| dir.as_deref()),
            home: $app.queue_tab.home.as_deref(),
            web_port: $app.web_server.as_ref().map(|server| server.port),
        }
    };
}

// `AppUI`'s methods, by area. Declared after `settings_context!` so they can use it.
mod autopilot;
mod browser;
mod event_loop;
pub use event_loop::TickClock;
mod folder_checks;
pub use folder_checks::{PathCheck, PathKind};
mod keys;
mod output;
mod playback;
mod queue_ops;
mod render;
mod web_cmds;

#[derive(Clone, Debug, Default)]
pub struct CachedDir {
    pub dirs: Vec<(String, PathBuf, bool)>,
    pub files: Vec<(String, PathBuf, bool)>,
    pub unfetched_tracks: Vec<PathBuf>,
    pub is_complete: bool,
    /// Paths already listed. Some SMB shares return each entry several times (issue #35).
    pub seen: std::collections::HashSet<PathBuf>,
}

#[derive(Debug)]
pub enum DirScanEvent {
    Batch {
        scan_id: u64,
        dir: PathBuf,
        dirs: Vec<(String, PathBuf, bool)>,
        files: Vec<(String, PathBuf, bool)>,
        unfetched_tracks: Vec<PathBuf>,
    },
    Complete {
        scan_id: u64,
        dir: PathBuf,
    },
    Error {
        scan_id: u64,
        dir: PathBuf,
        message: String,
    },
}

/// A folder's entries as the browser's `dir-scan` worker reads them.
pub type DirEntries = Box<dyn Iterator<Item = io::Result<std::fs::DirEntry>> + Send>;

/// Lists a folder for the browser's `dir-scan` worker: [`std::fs::read_dir`] except in tests,
/// which hold the read back (all of it, or after some entries) to stand in for a slow share
/// (`AppUI::set_dir_reader`).
pub type DirReader = std::sync::Arc<dyn Fn(&std::path::Path) -> io::Result<DirEntries> + Send + Sync>;

/// Where the browser's cursor goes when a folder is shown.
#[derive(Debug)]
enum CursorTarget {
    /// The first row: another folder was opened.
    Top,
    /// The row of this entry once it is listed: the folder `Backspace` left.
    Entry(PathBuf),
    /// The entry the cursor is on when the listing is shown (else the same row): the shown
    /// folder was listed again (`r`).
    Selection,
}

/// A folder being opened while the previous one stays on screen and keeps the keys (issue
/// #101). Its scan's batches collect in `browser_dir_cache`; it is shown when the scan
/// completes or fails, or once `folder_open_deadline` has passed.
struct PendingFolder {
    dir: PathBuf,
    cursor: CursorTarget,
    opened: Instant,
}

/// Builds the player an output switch moves to, in place of mpv or a renderer client (tests).
pub type OutputPlayerFactory = Box<dyn FnMut() -> Box<dyn AudioPlayer> + Send>;

/// Create a local (mpv/ffplay) player that runs on its own worker thread.
fn spawn_local_player(backend: &str, initial_volume: u8) -> Box<dyn AudioPlayer> {
    let backend = backend.to_string();
    Box::new(AsyncPlayer::spawn(
        move || Box::new(LocalPlayer::new(&backend)),
        initial_volume,
    ))
}

/// How long quitting waits in total for background workers to finish before detaching them.
pub const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TUI_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ask the running TUI to exit cleanly on its next tick. Safe to call from a signal handler thread.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Reset a pending shutdown request.
pub fn clear_shutdown_request() {
    SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Whether the TUI event loop has exited and the app is releasing resources (players, mpv).
pub fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether the interactive TUI event loop is currently running.
pub fn is_tui_active() -> bool {
    TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Thread that put the terminal in raw mode / the alternate screen, while it is still set up.
static TERMINAL_OWNER: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);

fn terminal_owner() -> std::sync::MutexGuard<'static, Option<std::thread::ThreadId>> {
    TERMINAL_OWNER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Leave the alternate screen, disable raw mode and show the cursor, if the TUI set the terminal
/// up and has not restored it yet. Returns whether anything was restored. Safe to call from any
/// thread, repeatedly, and from a panic hook.
pub fn restore_terminal() -> bool {
    if terminal_owner().take().is_none() {
        return false;
    }
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    true
}

/// Restore the terminal only if the current thread is the one running the TUI. A panic on a
/// worker thread must not tear the screen out from under a TUI that keeps running.
fn restore_terminal_if_owner() -> bool {
    let owns_terminal = *terminal_owner() == Some(std::thread::current().id());
    owns_terminal && restore_terminal()
}

/// Panic hook body: log the panic, call `restore`, then run `prev` (normally the default hook),
/// so the panic message prints on the normal screen instead of the discarded alternate screen.
pub fn terminal_panic_hook<R, P>(restore: R, prev: P) -> impl Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static
where
    R: Fn() + Send + Sync + 'static,
    P: Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static,
{
    move |info| {
        crate::log_error!("panic", "{}", info);
        restore();
        prev(info);
    }
}

/// Install a process-wide panic hook that restores the terminal before the previous hook prints
/// the panic message (issue #9). Call once, early in `main`.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(terminal_panic_hook(
        || {
            restore_terminal_if_owner();
        },
        prev,
    )));
}

/// Restores the terminal when `AppUI::run` returns, errors out or unwinds.
struct TerminalGuard;

impl TerminalGuard {
    fn claim() -> Self {
        *terminal_owner() = Some(std::thread::current().id());
        TerminalGuard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        restore_terminal();
    }
}

pub struct AppUI {
    pub active_tab: usize,
    pub queue_focus: bool, // false = browser, true = queue
    pub browser_dir: PathBuf,
    pub browser_entries: Vec<(String, PathBuf, bool)>, // (display_name, path, is_dir)
    pub browser_cursor: usize,
    pub browser_loading: bool,
    pub browser_load_start: Option<std::time::Instant>,
    pub browser_dir_cache: std::collections::HashMap<PathBuf, CachedDir>,
    pub dir_scan_tx: std::sync::mpsc::Sender<DirScanEvent>,
    pub dir_scan_rx: std::sync::mpsc::Receiver<DirScanEvent>,
    pub dir_scan_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub dir_scan_id: u64,
    /// The entry the cursor goes to once the shown folder's scan lists it (the folder
    /// `Backspace` left); dropped when the user moves the cursor.
    pub pending_parent_restore: Option<PathBuf>,
    /// The folder being opened while the previous one is still shown (`opening_folder`).
    pending_folder: Option<PendingFolder>,
    /// How long `pending_folder` may wait for its scan before it is shown with what has been
    /// listed so far (`set_folder_open_deadline`).
    folder_open_deadline: Duration,
    /// Lists folders on the `dir-scan` worker (`set_dir_reader`).
    dir_reader: DirReader,
    /// Looks paths up on `folder-check` workers (`set_path_check`).
    path_check: PathCheck,
    folder_check_tx: std::sync::mpsc::Sender<folder_checks::FolderCheckAnswer>,
    folder_check_rx: std::sync::mpsc::Receiver<folder_checks::FolderCheckAnswer>,
    /// The id of the last folder check started; answers to older ones are dropped.
    folder_check_id: u64,
    /// How many folder check answers `poll_folder_checks` has taken in (`folder_check_answers`).
    folder_check_answers: u64,
    /// The `~` key's check of the startup home, while it runs.
    home_check: Option<folder_checks::HomeCheck>,
    /// How many times `navigate_browser` has sent the browser somewhere; a `~` answer is
    /// dropped if it changed after the key.
    browser_navigations: u64,
    /// The id of the mix picker's or Seed Radio's check of the folder Autopilot is to scan, and
    /// the profile the scan is to use, while it runs; any other Autopilot start or the end of
    /// the session drops it.
    autopilot_check: Option<(u64, crate::analysis::MixProfile)>,
    pub queue_cursor: usize,
    pub devices_cursor: usize,
    pub settings_cursor: usize,
    pub bookmarks: Vec<String>,
    /// Where the bookmarks were read from and are saved to, resolved once at startup (#60).
    bookmarks_file: paths::ConfigFile,
    pub in_bookmarks_view: bool,
    pub show_help_modal: bool,
    pub status_message: String,
    pub queue: PlaybackQueue,
    pub player: Box<dyn AudioPlayer>,
    /// The file the player was last told to play, so an output switch can tell whether it still
    /// holds the queue's current track (issue #48). `None` before anything played and while the
    /// continuous stream plays (it follows the queue itself).
    player_track: Option<PathBuf>,
    /// Builds the player an output switch moves to instead of mpv / a renderer client
    /// (`set_output_player_factory`, for tests).
    output_player_factory: Option<OutputPlayerFactory>,
    pub device_manager: DeviceManager,
    pub settings_mgr: SettingsManager,
    /// Shared with background scanners; clone the handle, never the map (issue #4).
    pub metadata_cache: crate::library::MetadataCache,
    pub metadata_tx: std::sync::mpsc::Sender<crate::metadata::AudioMetadata>,
    pub metadata_rx: std::sync::mpsc::Receiver<crate::metadata::AudioMetadata>,
    pub queue_add_tx: std::sync::mpsc::Sender<Vec<crate::metadata::AudioMetadata>>,
    pub queue_add_rx: std::sync::mpsc::Receiver<Vec<crate::metadata::AudioMetadata>>,
    pub metadata_worker_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Inbox of the queue metadata worker pool (spawned on first use; workers exit when dropped).
    pub queue_metadata_tx: Option<std::sync::mpsc::Sender<PathBuf>>,
    /// Tells the queue metadata pool to stop reading tags (set on shutdown).
    pub queue_metadata_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Paths already sent to the queue metadata workers this session.
    pub queue_metadata_attempted: std::collections::HashSet<PathBuf>,
    /// Tag reads shared by the browser, queue and folder-add workers, so no two of them read
    /// the same file at once (issue #52).
    tag_reads: SharedTagReads,
    /// Handles of background worker threads, joined (with a bounded wait) on shutdown.
    pub workers: WorkerRegistry,
    pub browser_state: ListState,
    pub queue_state: ListState,
    pub devices_state: ListState,
    pub settings_state: ListState,
    pub search_modal: Option<SearchModalState>,
    pub browser_spotlight: Option<String>,
    pub browser_page_height: usize,
    pub queue_page_height: usize,
    pub web_server: Option<crate::web::WebServerHandle>,
    pub web_state: std::sync::Arc<std::sync::RwLock<crate::web::NowPlayingState>>,
    pub web_cmd_rx: Option<std::sync::mpsc::Receiver<crate::web::WebCommand>>,
    /// Folder the browser opened at launch; one of the folders the web dashboard may reach.
    pub launch_dir: PathBuf,
    pub pending_seek_delta: f64,
    pub pending_seek_target: Option<f64>,
    pub last_seek_key_time: Option<std::time::Instant>,
    pub pending_track_skip: bool,
    pub last_track_skip_time: Option<std::time::Instant>,
    pub editing_home_dir: Option<String>,
    pub autopilot_dir: Option<PathBuf>,
    pub autopilot_rx: Option<std::sync::mpsc::Receiver<crate::library::AutopilotEvent>>,
    pub autopilot_scan_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub is_autopilot_scanning: bool,
    pub autopilot_run_mode: AutopilotRunMode,
    pub autopilot_added_count: usize,
    pub analysis_db: crate::analysis::AnalysisDatabase,
    /// Background BPM/key/energy analysis of discovered tracks (stops and joins on drop).
    pub analysis_worker: crate::analysis::worker::AnalysisWorker,
    /// Reads the current track's analysis features off the UI thread.
    track_features: features::TrackFeaturesWorker,
    pub active_mix_profile: crate::analysis::MixProfile,
    /// The profile the latest Autopilot scan was started with (`autopilot_scan_profile`).
    autopilot_scan_profile: Option<crate::analysis::MixProfile>,
    pub show_mix_picker_modal: bool,
    pub mix_picker_cursor: usize,
    /// The mix picker's scroll position on screens too short for every profile (reset on open).
    mix_picker_list: ListState,
    /// Writes bookmarks and settings off the UI thread; flushed and joined on drop.
    pub persistence: crate::persist::PersistenceWorker,
    /// Tab 5: playlist store, cursors and the playlist modals.
    pub playlists: playlists::PlaylistsUi,
    /// The header clock, read at most once a second.
    clock: kit::ClockCache,
    /// Tab 2 values cached per queue / metadata revision (queued paths, total lengths).
    queue_tab: queue_tab::QueueTabCache,
    /// Tab 1 values cached per queue revision (Up Next count, total length, columns).
    now_playing: now_playing::NowPlayingCache,
    /// When the UI was created: the clock the Devices tab's scanning spinner turns by.
    ui_epoch: Instant,
    /// The music folder (a startup home choice), set by a worker at startup
    /// (`set_music_dir_lookup`); empty until it has answered.
    music_dir: std::sync::Arc<std::sync::OnceLock<Option<PathBuf>>>,
    /// The status message the web server row set while its pairing link was not known yet
    /// (`show_setting_status`); `poll_settings_status` adds the link once the LAN address is in.
    web_link_status: Option<String>,
}

impl Drop for AppUI {
    /// Runs after the TUI has exited (and the terminal is restored): stop and join background
    /// workers with a bounded wait so quitting stays fast. The player, web server and analysis
    /// worker fields then shut down in their own `Drop` impls.
    fn drop(&mut self) {
        self.shutdown_workers(WORKER_SHUTDOWN_TIMEOUT);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotRunMode {
    Fresh,
    Append,
    Reshuffle,
}

impl AppUI {
    pub fn new(start_dir: Option<PathBuf>, mut settings_mgr: SettingsManager) -> Self {
        let persistence = crate::persist::PersistenceWorker::spawn();
        settings_mgr.set_persistence(persistence.handle());
        let playlists = playlists::PlaylistsUi::new(persistence.handle());
        let cwd = start_dir
            .or_else(|| {
                settings_mgr
                    .settings
                    .home_dir
                    .as_ref()
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        // Browser entries inherit this; relative ones would be saved into playlists as-is.
        let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
        let backend = settings_mgr.settings.player_backend.clone();
        // A corrupt file is backed up to bookmarks.json.bak before we start from an empty list.
        let bookmarks_file = paths::bookmarks_file();
        let bookmarks = bookmarks_file.load_json("bookmarks").unwrap_or_default();
        // Discovery starts in run(): SSDP opens a socket on all interfaces, which tests that
        // only construct the app should not do (macOS firewall prompts).
        let device_manager = DeviceManager::new();

        let web_state = std::sync::Arc::new(std::sync::RwLock::new(crate::web::NowPlayingState {
            remote_access: settings_mgr.settings.web_remote_access(),
            ..Default::default()
        }));
        let (web_server, web_cmd_rx) = if settings_mgr.settings.web_server_enabled {
            let (tx, rx) = std::sync::mpsc::channel();
            let bind = if settings_mgr.settings.web_server_lan {
                crate::web::WebBind::Lan
            } else {
                crate::web::WebBind::Loopback
            };
            match crate::web::WebServer::start_with_bind(
                bind,
                settings_mgr.settings.web_server_port,
                web_state.clone(),
                tx,
            ) {
                Ok(handle) => (Some(handle), Some(rx)),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        };
        if settings_mgr.settings.web_server_lan {
            // So the pairing link is ready by the time the Settings tab or a status shows it.
            settings_mgr.prefetch_lan_address();
        }

        let (metadata_tx, metadata_rx) = std::sync::mpsc::channel();
        let (queue_add_tx, queue_add_rx) = std::sync::mpsc::channel();
        let metadata_cache = crate::library::MetadataCache::from(crate::library::LibraryManager::load_cache_map());
        let tag_reads = SharedTagReads::new(metadata_cache.clone(), metadata_tx.clone());
        let (dir_scan_tx, dir_scan_rx) = std::sync::mpsc::channel();
        let (folder_check_tx, folder_check_rx) = std::sync::mpsc::channel();
        let analysis_db = crate::analysis::AnalysisDatabase::open_default()
            .unwrap_or_else(|_| crate::analysis::AnalysisDatabase::open_in_memory().unwrap());
        let active_mix_profile = settings_mgr.settings.mix_profile();
        let mut workers = WorkerRegistry::new();
        let track_features =
            features::TrackFeaturesWorker::spawn(&mut workers, features::database_source(analysis_db.clone()));

        let mut app = Self {
            active_tab: 1,
            queue_focus: false,
            launch_dir: cwd.clone(),
            browser_dir: cwd,
            browser_entries: Vec::new(),
            browser_cursor: 0,
            browser_loading: false,
            browser_load_start: None,
            browser_dir_cache: std::collections::HashMap::new(),
            dir_scan_tx,
            dir_scan_rx,
            dir_scan_stop: None,
            dir_scan_id: 0,
            pending_parent_restore: None,
            pending_folder: None,
            folder_open_deadline: Self::FOLDER_OPEN_DEADLINE,
            dir_reader: std::sync::Arc::new(|dir: &std::path::Path| {
                std::fs::read_dir(dir).map(|entries| Box::new(entries) as DirEntries)
            }),
            path_check: std::sync::Arc::new(PathKind::of),
            folder_check_tx,
            folder_check_rx,
            folder_check_id: 0,
            folder_check_answers: 0,
            home_check: None,
            browser_navigations: 0,
            autopilot_check: None,
            queue_cursor: 0,
            devices_cursor: 0,
            settings_cursor: SETTING_DISPLAY_ORDER[0],
            bookmarks,
            bookmarks_file,
            in_bookmarks_view: false,
            show_help_modal: false,
            status_message: "Welcome to Quickstream (Rust)".to_string(),
            queue: PlaybackQueue::new(),
            player: spawn_local_player(&backend, 50),
            device_manager,
            settings_mgr,
            metadata_cache,
            metadata_tx,
            metadata_rx,
            queue_add_tx,
            queue_add_rx,
            metadata_worker_stop: None,
            queue_metadata_tx: None,
            queue_metadata_stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            queue_metadata_attempted: std::collections::HashSet::new(),
            tag_reads,
            workers,
            browser_state: ListState::default(),
            queue_state: ListState::default(),
            devices_state: ListState::default(),
            settings_state: ListState::default(),
            search_modal: None,
            browser_spotlight: None,
            browser_page_height: 15,
            queue_page_height: 15,
            web_server,
            web_state,
            web_cmd_rx,
            player_track: None,
            output_player_factory: None,
            pending_seek_delta: 0.0,
            pending_seek_target: None,
            last_seek_key_time: None,
            pending_track_skip: false,
            last_track_skip_time: None,
            editing_home_dir: None,
            autopilot_dir: None,
            autopilot_rx: None,
            autopilot_scan_stop: None,
            is_autopilot_scanning: false,
            autopilot_run_mode: AutopilotRunMode::Fresh,
            autopilot_added_count: 0,
            analysis_worker: crate::analysis::worker::AnalysisWorker::spawn(analysis_db.clone(), 1),
            track_features,
            analysis_db,
            active_mix_profile,
            autopilot_scan_profile: None,
            show_mix_picker_modal: false,
            mix_picker_cursor: 0,
            mix_picker_list: ListState::default(),
            persistence,
            playlists,
            clock: kit::ClockCache::new(),
            queue_tab: queue_tab::QueueTabCache::new(),
            now_playing: now_playing::NowPlayingCache::default(),
            ui_epoch: Instant::now(),
            music_dir: Default::default(),
            web_link_status: None,
        };
        // On Linux the music folder lookup reads user-dirs.dirs: never on the UI thread.
        app.set_music_dir_lookup(crate::settings::lookup_music_dir);
        app.refresh_browser();
        app.start_playlist_load();
        app.update_web_state();
        app
    }

    /// Queue the bookmarks file write on the persistence worker (never blocks the UI).
    fn save_bookmarks(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.bookmarks) {
            self.persistence
                .save(self.bookmarks_file.write_path.clone(), json.into_bytes());
        }
    }

    /// Replace the persistence worker (tests use one with an instrumented writer). The old
    /// worker is flushed and joined first so no queued write is lost.
    pub fn set_persistence_worker(&mut self, worker: crate::persist::PersistenceWorker) {
        self.settings_mgr.set_persistence(worker.handle());
        drop(std::mem::replace(&mut self.persistence, worker));
    }

    /// Replace how the browser lists a folder for scans started from now on (tests: a read held
    /// back like a slow share). Public only because integration tests in `tests/` drive `AppUI`
    /// directly.
    #[doc(hidden)]
    pub fn set_dir_reader(&mut self, reader: DirReader) {
        self.dir_reader = reader;
    }

    /// Replace how `folder-check` workers look a path up (tests: a check held back like a slow
    /// share). Public only because integration tests in `tests/` drive `AppUI` directly.
    #[doc(hidden)]
    pub fn set_path_check(&mut self, check: PathCheck) {
        self.path_check = check;
    }

    /// Replace how long a folder being opened keeps the previous one on screen before it is
    /// shown with what its scan has listed so far (`FOLDER_OPEN_DEADLINE`; tests: zero, or long
    /// enough that only the scan's end shows it). Public only because integration tests in
    /// `tests/` drive `AppUI` directly.
    #[doc(hidden)]
    pub fn set_folder_open_deadline(&mut self, deadline: Duration) {
        self.folder_open_deadline = deadline;
    }

    /// Replace the tag reader used by workers started from now on (tests use a counting or
    /// blocking stand-in for the share). Public only because integration tests in `tests/`
    /// drive `AppUI` directly.
    #[doc(hidden)]
    pub fn set_tag_reader(&mut self, reader: tag_reads::TagReader) {
        self.tag_reads.set_reader(reader);
    }

    /// Number of files a background worker is reading tags from right now.
    pub fn tag_reads_in_flight(&self) -> usize {
        self.tag_reads.in_flight()
    }

    /// Number of background worker threads still running.
    pub fn live_worker_count(&mut self) -> usize {
        self.workers.live_count()
    }

    pub fn cycle_and_apply_setting(&mut self, forward: bool) {
        self.settings_mgr
            .cycle_setting(self.settings_cursor, forward, &settings_context!(self));
        if self.settings_cursor == setting::METADATA_VIEW {
            self.update_browser_entry_labels();
        } else if self.settings_cursor == setting::PLAYER_BACKEND {
            let backend = self.settings_mgr.settings.player_backend.clone();
            if self.device_manager.get_active_device().is_none() {
                let volume = self.player.volume();
                let mut new_player = spawn_local_player(&backend, volume);
                let _ = new_player.set_volume(volume);
                let old = std::mem::replace(&mut self.player, new_player);
                self.retire_player(old);
            }
        } else if setting::WEB.contains(&self.settings_cursor) {
            self.sync_web_server();
            // Remote access off takes effect now, also on a server a renderer keeps on the LAN.
            self.update_web_state();
        } else if self.settings_cursor == setting::HOME_DIR {
            self.update_web_state();
        }
        self.show_setting_status(self.settings_cursor);
    }

    /// What setting `index`'s values and choices depend on besides config.json (see
    /// [`SettingsContext`]), all from memory.
    pub fn settings_context(&self) -> SettingsContext<'_> {
        settings_context!(self)
    }

    /// The port the running web server bound, `None` while none runs.
    pub fn web_port(&self) -> Option<u16> {
        self.web_server.as_ref().map(|server| server.port)
    }

    /// Look the music folder up on a worker with `lookup` (`AppUI::new` uses
    /// `settings::lookup_music_dir`; tests pass their own). Until it has answered, the startup
    /// home's choices and cycling leave the music folder out, as if there were none.
    pub fn set_music_dir_lookup(&mut self, lookup: impl FnOnce() -> Option<PathBuf> + Send + 'static) {
        let cell = std::sync::Arc::new(std::sync::OnceLock::new());
        self.music_dir = cell.clone();
        self.workers.spawn("music-dir", move || {
            let _ = cell.set(lookup());
        });
    }

    /// Put setting `index` and its long value in the status line (`Now Playing Web Server:
    /// Enabled (<address>)`). On the web server row with network access, while the LAN address
    /// is still being looked up, the address is left out and `poll_settings_status` adds it once
    /// it is known.
    pub fn show_setting_status(&mut self, index: usize) {
        let Some(def) = SETTING_DEFINITIONS.get(index) else {
            return;
        };
        if setting::WEB.contains(&index) && self.settings_mgr.settings.web_server_lan {
            self.settings_mgr.prefetch_lan_address();
        }
        self.status_message = format!(
            "{}: {}",
            def.label,
            self.settings_mgr.get_display_value(index, &self.settings_context())
        );
        let link_pending = index == setting::WEB_SERVER_ENABLED
            && self.settings_mgr.settings.web_server_enabled
            && self.web_server.is_some()
            && self.settings_mgr.web_url(self.web_port()).is_none();
        self.web_link_status = link_pending.then(|| self.status_message.clone());
    }

    /// Once per tick: when the status still reads what `show_setting_status` put there without
    /// the web address and the address is now known, show it with the address. Any other status
    /// shown since, or the server stopping, drops the update.
    pub fn poll_settings_status(&mut self) {
        let Some(sent) = self.web_link_status.as_ref() else {
            return;
        };
        if *sent != self.status_message || !self.settings_mgr.settings.web_server_enabled || self.web_server.is_none() {
            self.web_link_status = None;
            return;
        }
        if self.settings_mgr.web_url(self.web_port()).is_some() {
            self.show_setting_status(setting::WEB_SERVER_ENABLED);
        }
    }

    /// Whether the UPnP continuous stream is playing the queue: it then plays on to the end of
    /// the queue whatever the queue mode, and `play_next_track` wraps.
    pub fn continuous_stream_active(&self) -> bool {
        self.web_server.as_ref().is_some_and(|ws| {
            ws.stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
        })
    }

    /// Point the features worker at the current track and take what it has found. Never blocks.
    /// The event loop calls it once per tick, before `update_web_state`.
    pub fn poll_track_features(&mut self) {
        let track = self.queue.current_track().map(|t| t.filepath.as_path());
        self.track_features.follow(track);
        self.track_features.poll();
    }

    /// How many times the features worker has been polled (tests check once per tick).
    #[doc(hidden)]
    pub fn track_feature_polls(&self) -> u64 {
        self.track_features.polls()
    }

    /// The current track's analysis features, once the worker has found them.
    pub fn track_features(&self) -> Option<&crate::analysis::TrackFeatures> {
        self.track_features.features()
    }

    /// Read track features from `source` instead of the analysis database (test seam). The old
    /// worker is stopped and a new one started.
    pub fn set_track_feature_source(&mut self, source: features::FeatureSource) {
        self.track_features = features::TrackFeaturesWorker::spawn(&mut self.workers, source);
    }
}

/// Status line while the browser spotlight is open.
const SPOTLIGHT_HINT: &str = "Spotlight: type to jump to subfolder (Esc to exit, Enter to open)";

/// Whether a browser entry is a folder the spotlight can jump to: a subfolder (or a bookmarked
/// Music Home), labelled `📁 name/`. The `..` row and the `★ Music Homes` row are not.
pub(crate) fn is_subfolder_entry((label, _, is_dir): &(String, PathBuf, bool)) -> bool {
    *is_dir && label.starts_with("📁 ")
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    #[test]
    fn test_terminal_restore_only_from_owner_thread() {
        let guard = TerminalGuard::claim();

        // A panic on a worker thread must leave the TUI's terminal alone.
        let restored_elsewhere = std::thread::spawn(restore_terminal_if_owner).join().unwrap();
        assert!(!restored_elsewhere);
        assert!(terminal_owner().is_some());

        // The UI thread itself restores once; later calls (e.g. the guard's drop) are no-ops.
        assert!(restore_terminal_if_owner());
        assert!(terminal_owner().is_none());
        assert!(!restore_terminal());
        drop(guard);
        assert!(!restore_terminal());
    }
}
