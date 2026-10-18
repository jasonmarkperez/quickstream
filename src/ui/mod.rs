//! Interactive Terminal User Interface (Ratatui + Crossterm).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use ratatui::widgets::*;

pub mod playlists;
pub mod search;
pub mod workers;

use self::search::SearchModalState;
use self::workers::WorkerRegistry;
use crate::discovery::DeviceManager;
use crate::metadata::is_supported_audio_file;
use crate::paths;
use crate::player::local::LocalPlayer;
use crate::player::worker::AsyncPlayer;
use crate::player::AudioPlayer;
use crate::queue::PlaybackQueue;
use crate::settings::{SettingsManager, SETTING_DEFINITIONS};

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

/// Create a local (mpv/ffplay) player that runs on its own worker thread.
fn spawn_local_player(backend: &str, initial_volume: u8) -> Box<dyn AudioPlayer> {
    let backend = backend.to_string();
    Box::new(AsyncPlayer::spawn(
        move || Box::new(LocalPlayer::new(&backend)),
        initial_volume,
    ))
}

/// Start 4 threads that read tags for paths sent to the returned inbox and report results on
/// `metadata_tx`. The threads exit once the inbox sender is dropped or `stop` is set (paths still
/// queued in the inbox are then skipped).
fn spawn_queue_metadata_workers(
    workers: &mut WorkerRegistry,
    metadata_tx: std::sync::mpsc::Sender<crate::metadata::AudioMetadata>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::sync::mpsc::Sender<PathBuf> {
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for _ in 0..4 {
        let rx = rx.clone();
        let metadata_tx = metadata_tx.clone();
        let stop = stop.clone();
        workers.spawn("queue-metadata", move || loop {
            let path = match rx.lock() {
                Ok(guard) => match guard.recv() {
                    Ok(p) => p,
                    Err(_) => break,
                },
                Err(_) => break,
            };
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            if let Ok(meta) = crate::metadata::extract_metadata(&path) {
                crate::library::LibraryManager::append_to_cache(&meta);
                if metadata_tx.send(meta).is_err() {
                    break;
                }
            }
        });
    }
    tx
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
    pub pending_parent_restore: Option<PathBuf>,
    pub queue_cursor: usize,
    pub devices_cursor: usize,
    pub settings_cursor: usize,
    pub bookmarks: Vec<String>,
    pub in_bookmarks_view: bool,
    pub show_help_modal: bool,
    pub status_message: String,
    pub queue: PlaybackQueue,
    pub player: Box<dyn AudioPlayer>,
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
    pub active_mix_profile: crate::analysis::MixProfile,
    pub show_mix_picker_modal: bool,
    pub mix_picker_cursor: usize,
    /// Writes bookmarks and settings off the UI thread; flushed and joined on drop.
    pub persistence: crate::persist::PersistenceWorker,
    /// Tab 5: playlist store, cursors and the playlist modals.
    pub playlists: playlists::PlaylistsUi,
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
        let bookmarks = Self::load_bookmarks();
        // Discovery starts in run(): SSDP opens a socket on all interfaces, which tests that
        // only construct the app should not do (macOS firewall prompts).
        let device_manager = DeviceManager::new();

        let web_state = std::sync::Arc::new(std::sync::RwLock::new(crate::web::NowPlayingState::default()));
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

        let (metadata_tx, metadata_rx) = std::sync::mpsc::channel();
        let (queue_add_tx, queue_add_rx) = std::sync::mpsc::channel();
        let metadata_cache = crate::library::MetadataCache::from(crate::library::LibraryManager::load_cache_map());
        let (dir_scan_tx, dir_scan_rx) = std::sync::mpsc::channel();
        let analysis_db = crate::analysis::AnalysisDatabase::open_default()
            .unwrap_or_else(|_| crate::analysis::AnalysisDatabase::open_in_memory().unwrap());
        let active_mix_profile = settings_mgr.settings.mix_profile();

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
            queue_cursor: 0,
            devices_cursor: 0,
            settings_cursor: 0,
            bookmarks,
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
            workers: WorkerRegistry::new(),
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
            analysis_db,
            active_mix_profile,
            show_mix_picker_modal: false,
            mix_picker_cursor: 0,
            persistence,
            playlists,
        };
        app.refresh_browser();
        app.start_playlist_load();
        app.update_web_state();
        app
    }

    fn load_bookmarks() -> Vec<String> {
        // A corrupt file is backed up to bookmarks.json.bak before we start from an empty list.
        crate::persist::load_json_or_backup(&paths::get_bookmarks_path(), "bookmarks").unwrap_or_default()
    }

    /// Queue the bookmarks file write on the persistence worker (never blocks the UI).
    fn save_bookmarks(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.bookmarks) {
            self.persistence.save(paths::get_bookmarks_path(), json.into_bytes());
        }
    }

    /// Replace the persistence worker (tests use one with an instrumented writer). The old
    /// worker is flushed and joined first so no queued write is lost.
    pub fn set_persistence_worker(&mut self, worker: crate::persist::PersistenceWorker) {
        self.settings_mgr.set_persistence(worker.handle());
        drop(std::mem::replace(&mut self.persistence, worker));
    }

    pub fn format_file_label(path: &Path, metadata_view: bool, cache: &crate::library::MetadataCache) -> String {
        if metadata_view {
            if let Some(m) = cache.get(path) {
                return m.format_display_label(true);
            }
        }
        path.file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    pub fn update_browser_entry_labels(&mut self) {
        let metadata_view = self.settings_mgr.settings.metadata_view;
        let mut unfetched = Vec::new();

        // 1. Update current visible entries
        if !self.in_bookmarks_view {
            for (label, path, is_dir) in &mut self.browser_entries {
                if !*is_dir {
                    *label = Self::format_file_label(path, metadata_view, &self.metadata_cache);
                    if metadata_view && !self.metadata_cache.contains(path) {
                        unfetched.push(path.clone());
                    }
                }
            }

            let header_count = if self.browser_dir.parent().is_some() { 2 } else { 1 };
            if self.browser_entries.len() > header_count {
                let mut content_entries = self.browser_entries.split_off(header_count);
                content_entries.sort_by(|a, b| match (a.2, b.2) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
                });
                self.browser_entries.extend(content_entries);
            }
        }

        // 2. Update cached entries across all cached directories
        for cached in self.browser_dir_cache.values_mut() {
            for (label, path, is_dir) in &mut cached.files {
                if !*is_dir {
                    *label = Self::format_file_label(path, metadata_view, &self.metadata_cache);
                }
            }
            cached.files.sort_by_key(|a| a.0.to_lowercase());
        }

        // 3. If toggled to metadata view and there are unfetched tracks in the current dir, start background fetch
        if metadata_view && !unfetched.is_empty() {
            self.start_background_metadata_fetch(unfetched);
        }
    }

    pub fn refresh_browser(&mut self) {
        let refresh_start = std::time::Instant::now();
        let old_cursor = self.browser_cursor;

        if let Some(stop) = self.dir_scan_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if self.browser_loading {
            if let Some(prev_start) = self.browser_load_start.take() {
                crate::logger::log_raw(
                    "PERF",
                    "browser",
                    &format!(
                        "previous directory scan cancelled for '{}' after {:.2?}",
                        self.browser_dir.display(),
                        prev_start.elapsed()
                    ),
                );
            }
        }

        self.browser_entries.clear();

        if self.in_bookmarks_view {
            self.browser_loading = false;
            self.browser_load_start = None;
            self.browser_entries
                .push((".. [Back to File System]".to_string(), self.browser_dir.clone(), true));
            for b in &self.bookmarks {
                let p = PathBuf::from(b);
                let name = p
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| b.clone());
                let label = format!("📁 {}/", name);
                self.browser_entries.push((label, p, true));
            }
            self.browser_cursor = old_cursor.min(self.browser_entries.len().saturating_sub(1));
            if self.browser_cursor == 0 {
                self.browser_state = ListState::default();
                self.browser_state.select(Some(0));
            }
            crate::logger::log_raw(
                "PERF",
                "browser",
                &format!(
                    "bookmarks view loaded ({} bookmarks) -> elapsed: {:.2?}",
                    self.bookmarks.len(),
                    refresh_start.elapsed()
                ),
            );
            return;
        }

        self.browser_entries
            .push(("★ Music Homes".to_string(), PathBuf::from("___MUSIC_HOMES___"), true));

        if let Some(parent) = self.browser_dir.parent() {
            self.browser_entries
                .push((".. [Parent Directory]".to_string(), parent.to_path_buf(), true));
        }

        let header_count = if self.browser_dir.parent().is_some() { 2 } else { 1 };

        // Cache hit: instantaneous display with zero filesystem overhead!
        if let Some(cached) = self.browser_dir_cache.get(&self.browser_dir).cloned() {
            if cached.is_complete {
                self.browser_loading = false;
                self.browser_load_start = None;
                self.browser_entries.extend(cached.dirs);
                let metadata_view = self.settings_mgr.settings.metadata_view;
                for (mut label, path, is_dir) in cached.files {
                    if !is_dir {
                        label = Self::format_file_label(&path, metadata_view, &self.metadata_cache);
                    }
                    self.browser_entries.push((label, path, is_dir));
                }

                let header_count = if self.browser_dir.parent().is_some() { 2 } else { 1 };
                if self.browser_entries.len() > header_count {
                    let mut content_entries = self.browser_entries.split_off(header_count);
                    content_entries.sort_by(|a, b| match (a.2, b.2) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
                    });
                    self.browser_entries.extend(content_entries);
                }

                if let Some(target_sub) = self.pending_parent_restore.take() {
                    if let Some(pos) = self
                        .browser_entries
                        .iter()
                        .position(|(name, p, is_dir)| *is_dir && p == &target_sub && name.starts_with("📁 "))
                    {
                        self.browser_cursor = pos;
                        self.browser_state = ListState::default();
                        self.browser_state.select(Some(pos));
                        let item_count = self.browser_entries.len().saturating_sub(header_count);
                        crate::logger::log_raw(
                            "PERF",
                            "browser",
                            &format!(
                                "directory loaded from cache ({} items) for '{}' -> elapsed: {:.2?}",
                                item_count,
                                self.browser_dir.display(),
                                refresh_start.elapsed()
                            ),
                        );
                        return;
                    }
                }

                self.browser_cursor = old_cursor.min(self.browser_entries.len().saturating_sub(1));
                if self.browser_cursor == 0 {
                    self.browser_state = ListState::default();
                    self.browser_state.select(Some(0));
                }

                if !cached.unfetched_tracks.is_empty() && self.settings_mgr.settings.metadata_view {
                    self.start_background_metadata_fetch(cached.unfetched_tracks);
                }
                let item_count = self.browser_entries.len().saturating_sub(header_count);
                crate::logger::log_raw(
                    "PERF",
                    "browser",
                    &format!(
                        "directory loaded from cache ({} items) for '{}' -> elapsed: {:.2?}",
                        item_count,
                        self.browser_dir.display(),
                        refresh_start.elapsed()
                    ),
                );
                return;
            }
        }

        // Incomplete or uncached directory: clear any partial cache entry so fresh scan is clean
        self.browser_dir_cache.remove(&self.browser_dir);

        self.browser_cursor = old_cursor.min(self.browser_entries.len().saturating_sub(1));
        if self.browser_cursor == 0 {
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
        }

        self.browser_loading = true;
        self.browser_load_start = Some(refresh_start);

        self.dir_scan_id += 1;
        let scan_id = self.dir_scan_id;
        let scan_dir = self.browser_dir.clone();
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.dir_scan_stop = Some(stop_flag.clone());
        let tx = self.dir_scan_tx.clone();
        let metadata_view = self.settings_mgr.settings.metadata_view;
        let cached_map = self.metadata_cache.clone(); // shared handle, not a copy

        self.workers.spawn("dir-scan", move || {
            let thread_start = std::time::Instant::now();
            match std::fs::read_dir(&scan_dir) {
                Ok(entries) => {
                    let mut batch_dirs = Vec::new();
                    let mut batch_files = Vec::new();
                    let mut batch_unfetched = Vec::new();
                    let mut last_flush = std::time::Instant::now();
                    let mut total_sent = 0;

                    for entry_res in entries {
                        if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        let entry = match entry_res {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        let file_name = entry.file_name();
                        let name = file_name.to_string_lossy();
                        if name.starts_with('.') {
                            continue;
                        }
                        let path = entry.path();

                        // Optimization: Check supported audio file by extension first to avoid expensive stat calls
                        if is_supported_audio_file(&path) {
                            let label = Self::format_file_label(&path, metadata_view, &cached_map);
                            if metadata_view && !cached_map.contains(&path) {
                                batch_unfetched.push(path.clone());
                            }
                            batch_files.push((label, path, false));
                        } else {
                            let ft_res = entry.file_type();
                            let is_dir = match ft_res {
                                Ok(ft) => {
                                    if ft.is_dir() {
                                        true
                                    } else if ft.is_symlink() {
                                        path.is_dir()
                                    } else {
                                        false
                                    }
                                }
                                Err(_) => path.is_dir(),
                            };

                            if is_dir {
                                batch_dirs.push((format!("📁 {}/", name), path, true));
                            }
                        }

                        let pending = batch_dirs.len() + batch_files.len();
                        let should_flush = (total_sent == 0 && pending >= 5)
                            || pending >= 40
                            || (pending > 0 && last_flush.elapsed() >= std::time::Duration::from_millis(80));

                        if should_flush {
                            if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            total_sent += pending;
                            last_flush = std::time::Instant::now();
                            let _ = tx.send(DirScanEvent::Batch {
                                scan_id,
                                dir: scan_dir.clone(),
                                dirs: std::mem::take(&mut batch_dirs),
                                files: std::mem::take(&mut batch_files),
                                unfetched_tracks: std::mem::take(&mut batch_unfetched),
                            });
                        }
                    }

                    if (!batch_dirs.is_empty() || !batch_files.is_empty())
                        && !stop_flag.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        let _ = tx.send(DirScanEvent::Batch {
                            scan_id,
                            dir: scan_dir.clone(),
                            dirs: batch_dirs,
                            files: batch_files,
                            unfetched_tracks: batch_unfetched,
                        });
                    }

                    if !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                        let thread_elapsed = thread_start.elapsed();
                        crate::logger::log_raw(
                            "PERF",
                            "browser",
                            &format!(
                                "background read_dir complete for '{}' -> elapsed: {:.2?}",
                                scan_dir.display(),
                                thread_elapsed
                            ),
                        );
                        let _ = tx.send(DirScanEvent::Complete { scan_id, dir: scan_dir });
                    }
                }
                Err(err) => {
                    if !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = tx.send(DirScanEvent::Error {
                            scan_id,
                            dir: scan_dir,
                            message: err.to_string(),
                        });
                    }
                }
            }
        });

        // Fast path for immediate local reads: process any events available within up to 150ms
        let fast_path_start = std::time::Instant::now();
        while let Ok(event) = self.dir_scan_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            let is_complete = matches!(event, DirScanEvent::Complete { .. } | DirScanEvent::Error { .. });
            self.handle_dir_scan_event(event);
            if is_complete || fast_path_start.elapsed() >= std::time::Duration::from_millis(150) {
                break;
            }
        }

        if self.browser_loading {
            let partial_count = self.browser_entries.len().saturating_sub(header_count);
            crate::logger::log_raw(
                "DEBUG",
                "browser",
                &format!(
                    "refresh_browser fast-path yielded after {:.2?} ({} items streamed so far), scan continuing in background for '{}'",
                    refresh_start.elapsed(),
                    partial_count,
                    self.browser_dir.display()
                ),
            );
        }
    }

    fn handle_dir_scan_event(&mut self, event: DirScanEvent) {
        match event {
            DirScanEvent::Batch {
                scan_id,
                dir,
                mut dirs,
                mut files,
                mut unfetched_tracks,
            } => {
                if scan_id != self.dir_scan_id || dir != self.browser_dir {
                    return;
                }

                {
                    let seen = &mut self.browser_dir_cache.entry(dir.clone()).or_default().seen;
                    dirs.retain(|(_, path, _)| seen.insert(path.clone()));
                    let before = files.len();
                    files.retain(|(_, path, _)| seen.insert(path.clone()));
                    if files.len() != before {
                        let kept: std::collections::HashSet<&PathBuf> = files.iter().map(|(_, p, _)| p).collect();
                        unfetched_tracks.retain(|p| kept.contains(p));
                    }
                }

                let metadata_view = self.settings_mgr.settings.metadata_view;
                for (label, path, is_dir) in &mut files {
                    if !*is_dir {
                        *label = Self::format_file_label(path, metadata_view, &self.metadata_cache);
                    }
                }

                let cached = self.browser_dir_cache.entry(dir).or_default();
                cached.dirs.extend(dirs.clone());
                cached.files.extend(files.clone());
                cached.unfetched_tracks.extend(unfetched_tracks.clone());
                cached.is_complete = false;

                self.browser_entries.extend(dirs);
                self.browser_entries.extend(files);

                // Incremental sort so streamed entries are always alphabetical
                let header_count = if self.browser_dir.parent().is_some() { 2 } else { 1 };
                if self.browser_entries.len() > header_count {
                    let mut content_entries = self.browser_entries.split_off(header_count);
                    content_entries.sort_by(|a, b| match (a.2, b.2) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
                    });
                    self.browser_entries.extend(content_entries);
                }

                if let Some(ref target_sub) = self.pending_parent_restore {
                    if let Some(pos) = self
                        .browser_entries
                        .iter()
                        .position(|(name, p, is_dir)| *is_dir && p == target_sub && name.starts_with("📁 "))
                    {
                        self.browser_cursor = pos;
                        self.browser_state = ListState::default();
                        self.browser_state.select(Some(pos));
                    }
                }
            }
            DirScanEvent::Complete { scan_id, dir } => {
                if scan_id != self.dir_scan_id || dir != self.browser_dir {
                    return;
                }
                self.browser_loading = false;
                let load_duration = self.browser_load_start.take().map(|s| s.elapsed());

                let header_count = if self.browser_dir.parent().is_some() { 2 } else { 1 };
                if self.browser_entries.len() > header_count {
                    let mut content_entries = self.browser_entries.split_off(header_count);
                    content_entries.sort_by(|a, b| match (a.2, b.2) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
                    });
                    self.browser_entries.extend(content_entries);
                }

                if let Some(cached) = self.browser_dir_cache.get_mut(&dir) {
                    cached.dirs.sort_by_key(|a| a.0.to_lowercase());
                    cached.files.sort_by_key(|a| a.0.to_lowercase());
                    cached.is_complete = true;
                }

                let item_count = self.browser_entries.len().saturating_sub(header_count);
                if let Some(dur) = load_duration {
                    crate::logger::log_raw(
                        "PERF",
                        "browser",
                        &format!(
                            "directory scan complete ({} items) for '{}' -> total elapsed: {:.2?}",
                            item_count,
                            dir.display(),
                            dur
                        ),
                    );
                }

                if let Some(target_sub) = self.pending_parent_restore.take() {
                    if let Some(pos) = self
                        .browser_entries
                        .iter()
                        .position(|(name, p, is_dir)| *is_dir && p == &target_sub && name.starts_with("📁 "))
                    {
                        self.browser_cursor = pos;
                        self.browser_state = ListState::default();
                        self.browser_state.select(Some(pos));
                    }
                }

                if self.settings_mgr.settings.metadata_view {
                    if let Some(cached) = self.browser_dir_cache.get(&dir) {
                        if !cached.unfetched_tracks.is_empty() {
                            self.start_background_metadata_fetch(cached.unfetched_tracks.clone());
                        }
                    }
                }
            }
            DirScanEvent::Error { scan_id, dir, message } => {
                if scan_id != self.dir_scan_id || dir != self.browser_dir {
                    return;
                }
                self.browser_loading = false;
                let dur = self.browser_load_start.take().map(|s| s.elapsed());
                if let Some(d) = dur {
                    crate::logger::log_raw(
                        "PERF",
                        "browser",
                        &format!(
                            "directory scan failed for '{}' after {:.2?}: {}",
                            dir.display(),
                            d,
                            message
                        ),
                    );
                }
                self.status_message = format!("Error reading directory '{}': {}", dir.display(), message);
            }
        }
    }

    pub fn poll_background_dir_scan(&mut self) {
        while let Ok(event) = self.dir_scan_rx.try_recv() {
            self.handle_dir_scan_event(event);
        }

        if self.browser_loading {
            if let Some(start) = self.browser_load_start {
                if start.elapsed() > std::time::Duration::from_secs(3) {
                    let count = self.browser_entries.len().saturating_sub(2);
                    self.status_message = format!(
                        "Loaded {} items from '{}'... Esc or '..' to go back",
                        count,
                        self.browser_dir.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }
    }

    pub fn start_background_metadata_fetch(&mut self, paths: Vec<PathBuf>) {
        let total_tracks = paths.len();
        let scan_dir_display = self.browser_dir.display().to_string();
        crate::log_info!(
            "browser",
            "Queuing {} tracks for background metadata extraction in '{}'",
            total_tracks,
            scan_dir_display
        );
        if let Some(stop) = self.metadata_worker_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.metadata_worker_stop = Some(stop.clone());
        let tx = self.metadata_tx.clone();

        self.workers.spawn("browser-metadata", move || {
            let fetch_start = std::time::Instant::now();
            let mut extracted = 0;
            for path in paths {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                if let Ok(meta) = crate::metadata::extract_metadata(&path) {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    crate::library::LibraryManager::append_to_cache(&meta);
                    let _ = tx.send(meta);
                    extracted += 1;
                }
            }
            if !stop.load(std::sync::atomic::Ordering::Relaxed) {
                crate::logger::log_raw(
                    "PERF",
                    "browser",
                    &format!(
                        "background metadata extraction complete ({}/{} tracks extracted) for '{}' -> elapsed: {:.2?}",
                        extracted,
                        total_tracks,
                        scan_dir_display,
                        fetch_start.elapsed()
                    ),
                );
            }
        });
    }

    /// Resolve tags for queue tracks that still have placeholder metadata. Each path is read at
    /// most once per session by a persistent pool of 4 workers; results arrive via `metadata_rx`.
    /// A zero sample rate marks a placeholder that has an artist and duration (a playlist's
    /// `#EXTINF`) but no album or audio properties yet.
    pub fn start_queue_metadata_fetch(&mut self) {
        let candidates: Vec<PathBuf> = self
            .queue
            .tracks
            .iter()
            .filter(|t| t.duration_seconds <= 0.0 || t.artist.is_empty() || t.sample_rate_hz == 0)
            .map(|t| t.filepath.clone())
            .collect();
        let new_paths: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|p| self.queue_metadata_attempted.insert(p.clone()))
            .collect();
        if new_paths.is_empty() {
            return;
        }

        let metadata_tx = self.metadata_tx.clone();
        let (workers, stop) = (&mut self.workers, self.queue_metadata_stop.clone());
        let tx = self
            .queue_metadata_tx
            .get_or_insert_with(|| spawn_queue_metadata_workers(workers, metadata_tx, stop));
        for path in new_paths {
            let _ = tx.send(path);
        }
    }

    pub fn poll_pending_queue_adds(&mut self) {
        while let Ok(tracks) = self.queue_add_rx.try_recv() {
            let tracks: Vec<_> = tracks
                .into_iter()
                .map(|t| self.metadata_cache.get(&t.filepath).unwrap_or(t))
                .collect();
            let added = self.enqueue_tracks(tracks);
            self.status_message = format!("Added {} tracks (web)", added);
            self.update_web_state();
        }
    }

    /// Cached metadata for `path`, or a path-derived placeholder. Never reads the audio file.
    fn quick_metadata(&self, path: &std::path::Path) -> crate::metadata::AudioMetadata {
        self.metadata_cache
            .get(path)
            .unwrap_or_else(|| crate::metadata::fallback_metadata(path))
    }

    /// Append tracks to the queue and resolve missing tags on background workers.
    fn enqueue_tracks(&mut self, tracks: Vec<crate::metadata::AudioMetadata>) -> usize {
        let added = tracks.len();
        for meta in tracks {
            self.register_stream_file(&meta.filepath);
            self.queue.add_track(meta);
        }
        if added > 0 {
            self.start_queue_metadata_fetch();
        }
        added
    }

    /// Keep the queue cursor on a real row after the queue shrinks.
    pub fn clamp_queue_cursor(&mut self) {
        self.queue_cursor = self.queue_cursor.min(self.queue.len().saturating_sub(1));
    }

    /// Remove a queue row (TUI `d` or the web dashboard). Removing the track that is playing or
    /// paused moves playback on to the track that took its place; emptying the queue stops it.
    pub fn remove_queue_track(&mut self, index: usize) -> Option<crate::metadata::AudioMetadata> {
        let removed = self.queue.remove(index)?;
        self.clamp_queue_cursor();
        if removed.was_current {
            if self.queue.is_empty() {
                let _ = self.player.stop();
            } else if self.player.is_playing() || self.player.is_paused() {
                self.play_current_track();
            }
        }
        Some(removed.track)
    }

    pub fn poll_background_metadata(&mut self) {
        let mut queue_updated = false;
        while let Ok(meta) = self.metadata_rx.try_recv() {
            let path = meta.filepath.clone();
            self.metadata_cache.insert(meta.clone());

            // 1. Update matching tracks in queue
            for track in &mut self.queue.tracks {
                if track.filepath == path {
                    *track = meta.clone();
                    queue_updated = true;
                }
            }

            // 2. Update browser entries if active
            if self.settings_mgr.settings.metadata_view {
                if !self.in_bookmarks_view {
                    for (label, p, is_dir) in &mut self.browser_entries {
                        if !*is_dir && p == &path {
                            *label = meta.format_display_label(true);
                            break;
                        }
                    }
                }

                if let Some(parent) = path.parent() {
                    if let Some(cached) = self.browser_dir_cache.get_mut(parent) {
                        for (label, p, is_dir) in &mut cached.files {
                            if !*is_dir && p == &path {
                                *label = meta.format_display_label(true);
                                break;
                            }
                        }
                    }
                }
            }
        }

        if queue_updated {
            self.update_web_state();
        }
    }

    pub fn poll_background_autopilot(&mut self) {
        let mut new_tracks = Vec::new();
        let mut finished = false;

        if let Some(ref rx) = self.autopilot_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    crate::library::AutopilotEvent::Track(track) => {
                        new_tracks.push(track);
                    }
                    crate::library::AutopilotEvent::Finished(_) => {
                        finished = true;
                    }
                }
            }
        }

        if finished {
            self.is_autopilot_scanning = false;
            self.autopilot_rx = None;
        }

        if new_tracks.is_empty() {
            if finished {
                if self.queue.is_empty() {
                    let dir_name = self
                        .autopilot_dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default();
                    self.status_message = format!("No audio files found in '{}'", dir_name);
                } else {
                    let dir_name = self
                        .autopilot_dir
                        .as_ref()
                        .and_then(|d| d.file_name())
                        .unwrap_or_default()
                        .to_string_lossy();
                    match self.autopilot_run_mode {
                        AutopilotRunMode::Fresh => {
                            self.status_message =
                                format!("⚡ Autopilot: {} tracks from '{}'", self.queue.len(), dir_name);
                        }
                        AutopilotRunMode::Append => {
                            self.status_message = format!(
                                "⚡ Autopilot: Appended {} tracks (queue: {})",
                                self.autopilot_added_count,
                                self.queue.len()
                            );
                        }
                        AutopilotRunMode::Reshuffle => {
                            self.status_message = format!(
                                "🎲 Autopilot: Reshuffled {} tracks from next track forward",
                                self.autopilot_added_count
                            );
                        }
                    }
                }
            }
            return;
        }

        let was_empty = self.queue.is_empty();

        self.analysis_worker
            .enqueue(new_tracks.iter().map(|t| t.filepath.clone()));
        for track in new_tracks {
            if let Some(ref ws) = self.web_server {
                ws.register_stream_file(&track.filepath);
            }
            self.autopilot_added_count += 1;
            self.queue.add_track(track);
        }

        if was_empty && !self.queue.is_empty() {
            self.queue.current_index = 0;
            self.queue_cursor = 0;

            let _ = self.ensure_web_server_started();
            self.update_web_state();

            let is_dlna = self.device_manager.get_active_device().is_some();
            let gapless = self.settings_mgr.settings.gapless_autopilot;

            if is_dlna && gapless {
                if let Some(ref ws) = self.web_server {
                    ws.close_active_streams();
                    *ws.stream_state().continuous.current_track_index.lock().unwrap() = 0;
                    *ws.stream_state().continuous.current_position.lock().unwrap() = 0.0;
                }
                if let Ok(mut s) = self.web_state.write() {
                    s.is_autopilot = true;
                    s.autopilot_dir = self.autopilot_dir.as_ref().map(|d| d.to_string_lossy().to_string());
                    s.is_playing = true;
                    s.is_paused = false;
                }
                let _ = self.player.play(Path::new("__continuous__.wav"));
            } else {
                if let Ok(mut s) = self.web_state.write() {
                    s.is_autopilot = true;
                    s.autopilot_dir = self.autopilot_dir.as_ref().map(|d| d.to_string_lossy().to_string());
                }
                self.play_current_track();
            }
        }

        self.start_queue_metadata_fetch();
        self.update_web_state();

        let dir_name = self
            .autopilot_dir
            .as_ref()
            .and_then(|d| d.file_name())
            .unwrap_or_default()
            .to_string_lossy();
        if self.is_autopilot_scanning {
            match self.autopilot_run_mode {
                AutopilotRunMode::Fresh => {
                    self.status_message = format!("⚡ Autopilot: {} tracks queued (discovering...)", self.queue.len());
                }
                AutopilotRunMode::Append => {
                    self.status_message = format!(
                        "⚡ Autopilot: Appending from '{}' (+{} queued, total: {})...",
                        dir_name,
                        self.autopilot_added_count,
                        self.queue.len()
                    );
                }
                AutopilotRunMode::Reshuffle => {
                    self.status_message = format!(
                        "🎲 Autopilot: Reshuffling from '{}' (+{} queued)...",
                        dir_name, self.autopilot_added_count
                    );
                }
            }
        } else if !self.queue.is_empty() {
            match self.autopilot_run_mode {
                AutopilotRunMode::Fresh => {
                    self.status_message = format!("⚡ Autopilot: {} tracks from '{}'", self.queue.len(), dir_name);
                }
                AutopilotRunMode::Append => {
                    self.status_message = format!(
                        "⚡ Autopilot: Appended {} tracks (queue: {})",
                        self.autopilot_added_count,
                        self.queue.len()
                    );
                }
                AutopilotRunMode::Reshuffle => {
                    self.status_message = format!(
                        "🎲 Autopilot: Reshuffled {} tracks from next track forward",
                        self.autopilot_added_count
                    );
                }
            }
        }
    }

    /// Test helper: block, polling autopilot results, until the scan finishes or `timeout`
    /// passes. It sleeps on the calling thread, so never call it from the TUI event loop. Public
    /// only because integration tests in `tests/` drive `AppUI` directly.
    #[doc(hidden)]
    pub fn wait_for_autopilot_scan(&mut self, timeout: std::time::Duration) {
        let start = std::time::Instant::now();
        while self.is_autopilot_scanning && start.elapsed() < timeout {
            self.poll_background_autopilot();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.poll_background_autopilot();
    }

    pub fn navigate_browser(&mut self, path: PathBuf) {
        self.browser_spotlight = None;

        if path.to_string_lossy() == "___MUSIC_HOMES___" {
            self.in_bookmarks_view = true;
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
            self.refresh_browser();
            self.update_web_state();
            return;
        }

        if self.in_bookmarks_view {
            self.in_bookmarks_view = false;
            self.browser_dir = path;
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
            self.refresh_browser();
            self.update_web_state();
            return;
        }

        let prev_dir = self.browser_dir.clone();
        let is_parent = prev_dir.parent().map(|p| p == path).unwrap_or(false);

        if is_parent {
            self.pending_parent_restore = Some(prev_dir);
        } else {
            self.pending_parent_restore = None;
        }

        self.browser_dir = path;
        self.browser_cursor = 0;
        self.browser_state = ListState::default();
        self.browser_state.select(Some(0));
        self.refresh_browser();
        self.update_web_state();
    }

    pub fn open_spotlight(&mut self) {
        self.browser_spotlight = Some(String::new());
        self.status_message = "Spotlight: type to jump to subfolder (Esc to exit, Enter to open)".to_string();
    }

    pub fn close_spotlight(&mut self) {
        self.browser_spotlight = None;
    }

    pub fn spotlight_matches(&self) -> Vec<usize> {
        let query = match &self.browser_spotlight {
            Some(q) if !q.is_empty() => q.to_lowercase(),
            _ => return Vec::new(),
        };

        let mut prefix_matches = Vec::new();
        let mut substring_matches = Vec::new();

        for (idx, (name, path, is_dir)) in self.browser_entries.iter().enumerate() {
            if !*is_dir || path.to_string_lossy() == "___MUSIC_HOMES___" {
                continue;
            }
            let clean_name = name
                .strip_prefix("📁 ")
                .and_then(|s| s.strip_suffix('/'))
                .unwrap_or(name.as_str());
            let lower = clean_name.to_lowercase();

            if lower.starts_with(&query) {
                prefix_matches.push(idx);
            } else if lower.contains(&query) {
                substring_matches.push(idx);
            }
        }

        prefix_matches.extend(substring_matches);
        prefix_matches
    }

    pub fn jump_spotlight_to_first_match(&mut self) {
        let matches = self.spotlight_matches();
        if let Some(&first_idx) = matches.first() {
            self.browser_cursor = first_idx;
        }
    }

    pub fn spotlight_next_match(&mut self) {
        let matches = self.spotlight_matches();
        if matches.is_empty() {
            return;
        }
        if let Some(&next_idx) = matches.iter().find(|&&i| i > self.browser_cursor) {
            self.browser_cursor = next_idx;
        } else if let Some(&first_idx) = matches.first() {
            self.browser_cursor = first_idx;
        }
    }

    pub fn spotlight_prev_match(&mut self) {
        let matches = self.spotlight_matches();
        if matches.is_empty() {
            return;
        }
        if let Some(&prev_idx) = matches.iter().rev().find(|&&i| i < self.browser_cursor) {
            self.browser_cursor = prev_idx;
        } else if let Some(&last_idx) = matches.last() {
            self.browser_cursor = last_idx;
        }
    }

    pub fn page_down_browser(&mut self) {
        if self.browser_entries.is_empty() {
            return;
        }
        let len = self.browser_entries.len();
        let step = self.browser_page_height.max(1);
        if self.browser_cursor + step < len {
            self.browser_cursor += step;
        } else if self.browser_cursor < len - 1 {
            self.browser_cursor = len - 1;
        } else {
            self.browser_cursor = 0;
        }
    }

    pub fn page_up_browser(&mut self) {
        if self.browser_entries.is_empty() {
            return;
        }
        let len = self.browser_entries.len();
        let step = self.browser_page_height.max(1);
        if self.browser_cursor >= step {
            self.browser_cursor -= step;
        } else if self.browser_cursor > 0 {
            self.browser_cursor = 0;
        } else {
            self.browser_cursor = len - 1;
        }
    }

    pub fn home_browser(&mut self) {
        if !self.browser_entries.is_empty() {
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
        }
    }

    pub fn end_browser(&mut self) {
        if !self.browser_entries.is_empty() {
            self.browser_cursor = self.browser_entries.len() - 1;
        }
    }

    pub fn page_down_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let step = self.queue_page_height.max(1);
        if self.queue_cursor + step < len {
            self.queue_cursor += step;
        } else if self.queue_cursor < len - 1 {
            self.queue_cursor = len - 1;
        } else {
            self.queue_cursor = 0;
        }
    }

    pub fn page_up_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let step = self.queue_page_height.max(1);
        if self.queue_cursor >= step {
            self.queue_cursor -= step;
        } else if self.queue_cursor > 0 {
            self.queue_cursor = 0;
        } else {
            self.queue_cursor = len - 1;
        }
    }

    pub fn home_queue(&mut self) {
        if !self.queue.is_empty() {
            self.queue_cursor = 0;
        }
    }

    pub fn end_queue(&mut self) {
        if !self.queue.is_empty() {
            self.queue_cursor = self.queue.len().saturating_sub(1);
        }
    }

    /// Drop a replaced player on a worker thread so its shutdown (killing mpv, sending Stop to a
    /// renderer) never blocks the UI. Shutdown waits for it, so mpv is gone before the process exits.
    fn retire_player(&mut self, player: Box<dyn AudioPlayer>) {
        self.workers.spawn("retire-player", move || drop(player));
    }

    /// Number of background worker threads still running.
    pub fn live_worker_count(&mut self) -> usize {
        self.workers.live_count()
    }

    /// Signal every background worker to stop (directory scan, tag readers, queue metadata pool,
    /// search scanner, autopilot scanner), then join them, waiting at most `timeout` in total.
    /// Workers still running at the deadline are detached. Returns how many were left running.
    /// Called when the app is dropped; safe to call more than once.
    pub fn shutdown_workers(&mut self, timeout: Duration) -> usize {
        let stops = [
            self.dir_scan_stop.take(),
            self.metadata_worker_stop.take(),
            self.autopilot_scan_stop.take(),
            Some(self.queue_metadata_stop.clone()),
        ];
        for stop in stops.into_iter().flatten() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.playlists.stop_checks();
        // Dropping the inbox wakes queue metadata workers blocked waiting for work.
        self.queue_metadata_tx = None;
        if let Some(mut modal) = self.search_modal.take() {
            modal.close();
        }
        self.autopilot_rx = None;
        self.is_autopilot_scanning = false;

        let start = Instant::now();
        let left = self.workers.join_all(timeout);
        crate::log_info!(
            "ui",
            "Background workers stopped in {:.2?} ({} detached)",
            start.elapsed(),
            left
        );
        left
    }

    /// Stop playback and return true if a shutdown was requested (e.g. by SIGINT/SIGTERM/SIGHUP).
    pub fn poll_shutdown_request(&mut self) -> bool {
        if !SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        crate::log_info!("ui", "Shutdown requested by signal, stopping playback");
        let _ = self.player.stop();
        true
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        self.device_manager.start_background_scan();
        enable_raw_mode()?;
        // From here on the terminal is restored on every exit path, including `?` and panics.
        let guard = TerminalGuard::claim();
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        let res = self.event_loop(&mut terminal);
        SHUTTING_DOWN.store(true, std::sync::atomic::Ordering::SeqCst);
        // A bookmark or setting changed right before quitting must reach disk.
        self.persistence.flush();
        drop(guard);

        res
    }

    fn event_loop<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> anyhow::Result<()> {
        loop {
            if self.poll_shutdown_request() {
                return Ok(());
            }

            if let Some(err) = self.player.take_error() {
                self.status_message = format!("Playback error: {}", err);
            }

            // Check if active track finished and auto-advance queue
            if self.player.is_track_finished() {
                self.player.reset_finished();
                self.play_next_track();
            }

            // Synchronize continuous gapless stream track index
            if let Some(ref ws) = self.web_server {
                if ws
                    .stream_state()
                    .continuous
                    .is_active
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    let cur_idx = *ws.stream_state().continuous.current_track_index.lock().unwrap();
                    if cur_idx != self.queue.current_index && cur_idx < self.queue.len() {
                        self.queue.current_index = cur_idx;
                        self.queue_cursor = cur_idx;
                    }
                }
            }

            // Process remote web commands
            self.handle_web_commands();

            // Synchronize web server live state
            self.update_web_state();

            // Poll background directory scan
            self.poll_background_dir_scan();

            // Poll background metadata extraction
            self.poll_background_metadata();
            self.poll_pending_queue_adds();
            self.poll_playlists();

            // Poll progressive background autopilot discovery
            self.poll_background_autopilot();

            if let Some(ref mut modal) = self.search_modal {
                modal.poll_background_scan();
            }

            // Flush debounced seek if debounce timer expired
            self.poll_pending_seek();

            // Flush debounced track skip if debounce timer expired
            self.poll_pending_track_skip();

            terminal.draw(|f| self.render(f))?;

            let poll_dur = Duration::from_millis(100);

            if event::poll(poll_dur)? {
                if let Event::Key(key) = event::read()? {
                    if self.show_help_modal {
                        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                            self.player.stop()?;
                            return Ok(());
                        }
                        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                            self.show_help_modal = false;
                        }
                        continue;
                    }

                    if let Some(ref mut modal) = self.search_modal {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                                self.player.stop()?;
                                return Ok(());
                            }
                            (KeyCode::Esc, _) => {
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                            }
                            (KeyCode::Enter, KeyModifiers::NONE) => {
                                let track_opt = modal.selected_track().cloned();
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                                if let Some(track) = track_opt {
                                    let title = track.display_title().to_string();
                                    self.register_stream_file(&track.filepath);
                                    self.queue.add_track(track);
                                    self.status_message = format!("Added to queue: {}", title);
                                }
                            }
                            (KeyCode::Tab, KeyModifiers::NONE) => {
                                if let Some(track) = modal.selected_track().cloned() {
                                    self.register_stream_file(&track.filepath);
                                    self.queue.add_track(track.clone());
                                    self.status_message = format!("Added to queue: {}", track.display_title());
                                }
                            }
                            (KeyCode::BackTab, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                                let tracks = modal.all_filtered_tracks();
                                let count = tracks.len();
                                if let Some(mut m) = self.search_modal.take() {
                                    m.close();
                                }
                                for t in tracks {
                                    self.register_stream_file(&t.filepath);
                                    self.queue.add_track(t);
                                }
                                self.status_message = format!("Added {} tracks to queue", count);
                            }
                            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                                modal.move_selection_up();
                            }
                            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                                modal.move_selection_down();
                            }
                            (KeyCode::PageUp, _)
                            | (KeyCode::Char('u'), KeyModifiers::CONTROL)
                            | (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                                modal.page_up(15);
                            }
                            (KeyCode::PageDown, _)
                            | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                            | (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                                modal.page_down(15);
                            }
                            (KeyCode::Home, _) => {
                                modal.home();
                            }
                            (KeyCode::End, _) => {
                                modal.end();
                            }
                            (KeyCode::Backspace, _) => {
                                modal.backspace();
                            }
                            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                                modal.append_char(c);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if self.browser_spotlight.is_some() {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                self.close_spotlight();
                                self.player.stop()?;
                                return Ok(());
                            }
                            (KeyCode::Esc, _) => {
                                self.close_spotlight();
                            }
                            (KeyCode::Enter, KeyModifiers::NONE) => {
                                self.close_spotlight();
                                if self.browser_cursor < self.browser_entries.len() {
                                    let (_, path, is_dir) = self.browser_entries[self.browser_cursor].clone();
                                    if is_dir {
                                        self.navigate_browser(path);
                                    } else {
                                        let meta = self.quick_metadata(&path);
                                        let title = meta.display_title().to_string();
                                        self.enqueue_tracks(vec![meta]);
                                        self.status_message = format!("Added to queue: {}", title);
                                    }
                                }
                            }
                            (KeyCode::Down, _) | (KeyCode::Tab, KeyModifiers::NONE) => {
                                self.spotlight_next_match();
                            }
                            (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
                                self.spotlight_prev_match();
                            }
                            (KeyCode::Backspace, _) => {
                                if let Some(ref mut q) = self.browser_spotlight {
                                    q.pop();
                                }
                                self.jump_spotlight_to_first_match();
                            }
                            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                                if let Some(ref mut q) = self.browser_spotlight {
                                    q.push(c);
                                }
                                self.jump_spotlight_to_first_match();
                            }
                            _ => {}
                        }
                        continue;
                    }

                    if self.handle_input_key(key)? {
                        return Ok(());
                    }
                }
            }
        }
    }

    pub fn accumulate_seek(&mut self, delta: f64) {
        if !self.player.is_playing() && !self.player.is_paused() {
            let sign = if delta > 0.0 { "+" } else { "" };
            self.status_message = format!("Seek {}{:.0}s (00:00:00)", sign, delta);
            return;
        }

        // If seek direction changed (+ to - or - to +), flush previous seek first
        if (self.pending_seek_delta > 0.0 && delta < 0.0) || (self.pending_seek_delta < 0.0 && delta > 0.0) {
            self.flush_pending_seek();
        }

        let dur = self.player.duration_seconds();
        let current_base = self
            .pending_seek_target
            .unwrap_or_else(|| self.player.position_seconds());
        let new_target = if dur > 0.0 {
            (current_base + delta).clamp(0.0, dur)
        } else {
            (current_base + delta).max(0.0)
        };
        self.pending_seek_delta += delta;
        self.pending_seek_target = Some(new_target);
        self.last_seek_key_time = Some(std::time::Instant::now());

        let sign = if self.pending_seek_delta > 0.0 { "+" } else { "" };
        self.status_message = format!(
            "Seek {}{:.0}s ({})",
            sign,
            self.pending_seek_delta,
            crate::player::upnp::format_seconds_to_upnp_time(new_target)
        );
        crate::log_info!(
            "ui",
            "Accumulated seek: target={:.2}s (delta={:.1}s)",
            new_target,
            self.pending_seek_delta
        );
    }

    pub fn flush_pending_seek(&mut self) {
        if let Some(target) = self.pending_seek_target.take() {
            let delta = self.pending_seek_delta;
            self.pending_seek_delta = 0.0;
            self.last_seek_key_time = None;

            let sign = if delta > 0.0 { "+" } else { "" };
            match self.player.seek_to(target) {
                Ok(_) => {
                    self.status_message = format!(
                        "Seek {}{:.0}s ({})",
                        sign,
                        delta,
                        crate::player::upnp::format_seconds_to_upnp_time(target)
                    );
                    crate::log_info!("ui", "Flushed seek: target={:.2}s (delta={:.1}s)", target, delta);
                }
                Err(e) => {
                    self.status_message = format!("Seek error: {}", e);
                    crate::log_warn!("ui", "Flushed seek to {:.2}s failed: {:?}", target, e);
                }
            }
            self.update_web_state();
        }
    }

    pub fn poll_pending_seek(&mut self) {
        if self.pending_seek_target.is_some() {
            if let Some(t) = self.last_seek_key_time {
                if t.elapsed() >= Duration::from_millis(350) {
                    self.flush_pending_seek();
                }
            }
        }
    }

    pub fn skip_next_track_debounced(&mut self) {
        if self.queue.is_empty() {
            return;
        }

        let raw_mode = self.settings_mgr.settings.queue_mode.clone();
        let mode = if raw_mode == "repeat_one" { "loop" } else { &raw_mode };
        if let Some(track) = self.queue.next_track(mode) {
            let track_title = track.display_title().to_string();
            self.queue_cursor = self.queue.current_index;

            if !self.pending_track_skip && self.player.is_playing() {
                let _ = self.player.stop();
            }

            self.pending_track_skip = true;
            self.last_track_skip_time = Some(Instant::now());
            self.status_message = format!("Skipped to: {}", track_title);
            self.update_web_state();
        } else {
            self.status_message = "End of queue".to_string();
        }
    }

    pub fn skip_prev_track_debounced(&mut self) {
        if self.queue.is_empty() {
            return;
        }

        if let Some(track) = self.queue.prev_track() {
            let track_title = track.display_title().to_string();
            self.queue_cursor = self.queue.current_index;

            if !self.pending_track_skip && self.player.is_playing() {
                let _ = self.player.stop();
            }

            self.pending_track_skip = true;
            self.last_track_skip_time = Some(Instant::now());
            self.status_message = format!("Skipped to: {}", track_title);
            self.update_web_state();
        }
    }

    pub fn flush_pending_track_skip(&mut self) {
        if self.pending_track_skip {
            self.pending_track_skip = false;
            self.last_track_skip_time = None;
            self.play_current_track();
        }
    }

    pub fn poll_pending_track_skip(&mut self) {
        if self.pending_track_skip {
            if let Some(t) = self.last_track_skip_time {
                if t.elapsed() >= Duration::from_millis(300) {
                    self.flush_pending_track_skip();
                }
            }
        }
    }

    pub fn handle_input_key(&mut self, key: crossterm::event::KeyEvent) -> anyhow::Result<bool> {
        // If user presses any non-seek key, immediately flush any pending debounced seek
        let is_seek_key = matches!(
            (key.code, key.modifiers),
            (KeyCode::Right, KeyModifiers::SHIFT)
                | (KeyCode::Right, KeyModifiers::CONTROL)
                | (KeyCode::Right, KeyModifiers::ALT)
                | (KeyCode::Char(']'), KeyModifiers::NONE)
                | (KeyCode::Char('>'), _)
                | (KeyCode::Left, KeyModifiers::SHIFT)
                | (KeyCode::Left, KeyModifiers::CONTROL)
                | (KeyCode::Left, KeyModifiers::ALT)
                | (KeyCode::Char('['), KeyModifiers::NONE)
                | (KeyCode::Char('<'), _)
        );
        if !is_seek_key && self.pending_seek_target.is_some() {
            self.flush_pending_seek();
        }

        // If user presses any non-skip key, flush any pending debounced track skip
        let is_skip_key = matches!(
            (key.code, key.modifiers),
            (KeyCode::Right, KeyModifiers::NONE) | (KeyCode::Left, KeyModifiers::NONE)
        );
        if !is_skip_key && self.pending_track_skip && !matches!(key.code, KeyCode::Char('s') | KeyCode::Char('c')) {
            self.flush_pending_track_skip();
        }

        // If help modal is open, let Esc, q, or ? dismiss it
        if self.show_help_modal {
            if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                self.player.stop()?;
                return Ok(true);
            }
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                self.show_help_modal = false;
                return Ok(false);
            }
            return Ok(false);
        }

        // If mix picker modal is open, intercept input
        if self.show_mix_picker_modal {
            if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                self.player.stop()?;
                return Ok(true);
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('M') => {
                    self.show_mix_picker_modal = false;
                    return Ok(false);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.mix_picker_cursor > 0 {
                        self.mix_picker_cursor -= 1;
                    } else {
                        self.mix_picker_cursor = 4;
                    }
                    return Ok(false);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.mix_picker_cursor = (self.mix_picker_cursor + 1) % 5;
                    return Ok(false);
                }
                KeyCode::Char('1') => {
                    self.apply_mix_picker_selection(0);
                    return Ok(false);
                }
                KeyCode::Char('2') => {
                    self.apply_mix_picker_selection(1);
                    return Ok(false);
                }
                KeyCode::Char('3') => {
                    self.apply_mix_picker_selection(2);
                    return Ok(false);
                }
                KeyCode::Char('4') => {
                    self.apply_mix_picker_selection(3);
                    return Ok(false);
                }
                KeyCode::Char('5') => {
                    self.apply_mix_picker_selection(4);
                    return Ok(false);
                }
                KeyCode::Enter => {
                    self.apply_mix_picker_selection(self.mix_picker_cursor);
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }

        // If editing startup home directory on settings view, intercept text input
        if let Some(ref mut input) = self.editing_home_dir {
            if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                self.player.stop()?;
                return Ok(true);
            }
            match key.code {
                KeyCode::Esc => {
                    self.editing_home_dir = None;
                    self.status_message = "Cancelled editing startup home directory".to_string();
                }
                KeyCode::Enter => {
                    let raw = input.trim();
                    if raw.is_empty() || raw.eq_ignore_ascii_case("default") || raw.eq_ignore_ascii_case("none") {
                        self.settings_mgr.settings.home_dir = None;
                        self.settings_mgr.save();
                        self.status_message = "Cleared startup home (defaulting to launch directory)".to_string();
                    } else {
                        let resolved = if raw.starts_with('~') {
                            if let Some(base) = directories::BaseDirs::new() {
                                let home = base.home_dir();
                                if raw == "~" {
                                    home.to_string_lossy().to_string()
                                } else if let Some(stripped) = raw.strip_prefix("~/") {
                                    home.join(stripped).to_string_lossy().to_string()
                                } else {
                                    raw.to_string()
                                }
                            } else {
                                raw.to_string()
                            }
                        } else {
                            raw.to_string()
                        };
                        self.settings_mgr.settings.home_dir = Some(resolved.clone());
                        self.settings_mgr.save();
                        self.status_message = format!("Set startup home directory: {}", resolved);
                    }
                    self.editing_home_dir = None;
                    self.update_web_state();
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
                    input.push(c);
                }
                _ => {}
            }
            return Ok(false);
        }

        // A playlist modal (name prompt, delete confirmation, add picker) takes every key.
        if self.playlists.modal.is_some() {
            return self.handle_playlist_modal_key(key);
        }

        // Global hotkeys
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), KeyModifiers::NONE)
            | (KeyCode::Esc, KeyModifiers::NONE)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if key.code == KeyCode::Esc && self.browser_loading {
                    if let Some(stop) = self.dir_scan_stop.take() {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.browser_loading = false;
                    if let Some(start) = self.browser_load_start.take() {
                        crate::logger::log_raw(
                            "PERF",
                            "browser",
                            &format!(
                                "directory scan cancelled by user for '{}' after {:.2?}",
                                self.browser_dir.display(),
                                start.elapsed()
                            ),
                        );
                    }
                    self.status_message = "Cancelled directory scan".to_string();
                    return Ok(false);
                }
                crate::log_info!("ui", "Quit requested by user");
                self.player.stop()?;
                return Ok(true);
            }
            (KeyCode::Char('?'), KeyModifiers::NONE) => {
                self.show_help_modal = !self.show_help_modal;
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.active_tab = (self.active_tab + 1) % 5;
            }
            (KeyCode::BackTab, _) => {
                self.active_tab = (self.active_tab + 4) % 5;
            }
            (KeyCode::Char('1'), KeyModifiers::NONE) => {
                self.active_tab = 0;
            }
            (KeyCode::Char('2'), KeyModifiers::NONE) => {
                self.active_tab = 1;
            }
            (KeyCode::Char('3'), KeyModifiers::NONE) => {
                self.active_tab = 2;
            }
            (KeyCode::Char('4'), KeyModifiers::NONE) => {
                self.active_tab = 3;
            }
            (KeyCode::Char('5'), KeyModifiers::NONE) => {
                self.active_tab = 4;
            }
            (KeyCode::Char('n') | KeyCode::Char('N'), KeyModifiers::NONE) => {
                if self.active_tab == 0 {
                    self.active_tab = 1;
                } else {
                    self.active_tab = 0;
                }
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                if self.active_tab == 3 {
                    self.cycle_and_apply_setting(true);
                } else if self.pending_track_skip {
                    self.flush_pending_track_skip();
                } else if !self.player.is_playing() && !self.player.is_paused() {
                    if !self.queue.is_empty() {
                        self.play_current_track();
                    } else {
                        self.status_message = "No tracks in queue".to_string();
                    }
                } else {
                    match self.player.toggle_pause() {
                        Ok(_) => {
                            self.status_message.clear();
                        }
                        Err(e) => {
                            self.status_message = format!("Pause error: {}", e);
                            crate::log_warn!("ui", "Toggle pause error: {:?}", e);
                        }
                    }
                }
            }
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                self.pending_track_skip = false;
                self.last_track_skip_time = None;
                self.player.stop()?;
                self.status_message.clear();
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                if self.active_tab == 3 {
                    self.cycle_and_apply_setting(true);
                } else {
                    self.skip_next_track_debounced();
                }
            }
            (KeyCode::Left, KeyModifiers::NONE) => {
                if self.active_tab == 3 {
                    self.cycle_and_apply_setting(false);
                } else {
                    self.skip_prev_track_debounced();
                }
            }
            (KeyCode::Right, KeyModifiers::SHIFT)
            | (KeyCode::Right, KeyModifiers::CONTROL)
            | (KeyCode::Right, KeyModifiers::ALT)
            | (KeyCode::Char(']'), KeyModifiers::NONE)
            | (KeyCode::Char('>'), _) => {
                let step = self.settings_mgr.settings.seek_step_seconds;
                self.accumulate_seek(step);
                self.update_web_state();
            }
            (KeyCode::Left, KeyModifiers::SHIFT)
            | (KeyCode::Left, KeyModifiers::CONTROL)
            | (KeyCode::Left, KeyModifiers::ALT)
            | (KeyCode::Char('['), KeyModifiers::NONE)
            | (KeyCode::Char('<'), _) => {
                let step = self.settings_mgr.settings.seek_step_seconds;
                self.accumulate_seek(-step);
                self.update_web_state();
            }
            (KeyCode::Char('+') | KeyCode::Char('='), KeyModifiers::NONE) => {
                let step = self.settings_mgr.settings.volume_step;
                let new_vol = self.player.volume().saturating_add(step).min(100);
                let _ = self.player.set_volume(new_vol);
                self.status_message = format!("Volume {}%", new_vol);
            }
            (KeyCode::Char('-'), KeyModifiers::NONE) => {
                let step = self.settings_mgr.settings.volume_step;
                let new_vol = self.player.volume().saturating_sub(step);
                let _ = self.player.set_volume(new_vol);
                self.status_message = format!("Volume {}%", new_vol);
            }
            (KeyCode::Char('v'), KeyModifiers::NONE) => {
                if self.active_tab == 0 || self.active_tab == 1 {
                    self.settings_mgr.cycle_setting(0, true);
                    self.update_browser_entry_labels();
                    self.status_message = format!(
                        "Display mode: {}",
                        if self.settings_mgr.settings.metadata_view {
                            "Metadata Tags (Artist - Title) [TAGS]"
                        } else {
                            "Filenames [FILES]"
                        }
                    );
                }
            }
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                if self.active_tab == 1 && !self.queue_focus {
                    self.open_spotlight();
                } else if self.active_tab == 0 || (self.active_tab == 1 && self.queue_focus) {
                    self.open_fuzzy_search();
                } else if self.active_tab == 3 && self.settings_cursor == 7 {
                    let curr = self
                        .settings_mgr
                        .settings
                        .home_dir
                        .clone()
                        .unwrap_or_else(|| "/".to_string());
                    self.editing_home_dir = Some(curr);
                    self.status_message = "Editing Startup Home Directory (Enter: Save, Esc: Cancel)".to_string();
                }
            }
            (KeyCode::Char('f'), KeyModifiers::NONE) => {
                if self.active_tab == 0 || self.active_tab == 1 {
                    self.open_fuzzy_search();
                }
            }
            (KeyCode::Char('z') | KeyCode::Char('Z'), KeyModifiers::NONE) => {
                if !self.queue.is_empty() {
                    self.queue.shuffle();
                    self.status_message = "🔀 Queue shuffled".to_string();
                    self.update_web_state();
                }
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => {
                let target_dir = if self.in_bookmarks_view {
                    if self.browser_cursor > 0 && self.browser_cursor < self.browser_entries.len() {
                        self.browser_entries[self.browser_cursor].1.clone()
                    } else {
                        self.browser_dir.clone()
                    }
                } else {
                    self.browser_dir.clone()
                };
                self.start_autopilot(target_dir);
            }
            (KeyCode::Char('P'), _) | (KeyCode::Char('p'), KeyModifiers::SHIFT) => {
                let target_dir = if self.in_bookmarks_view {
                    if self.browser_cursor > 0 && self.browser_cursor < self.browser_entries.len() {
                        self.browser_entries[self.browser_cursor].1.clone()
                    } else {
                        self.browser_dir.clone()
                    }
                } else {
                    self.browser_dir.clone()
                };
                self.reshuffle_autopilot(target_dir);
            }
            (KeyCode::Char('M'), _) | (KeyCode::Char('m'), KeyModifiers::SHIFT) => {
                self.show_mix_picker_modal = !self.show_mix_picker_modal;
                self.mix_picker_cursor = match &self.active_mix_profile {
                    crate::analysis::MixProfile::DiverseShuffle => 0,
                    crate::analysis::MixProfile::BpmFlow { .. } => 1,
                    crate::analysis::MixProfile::MoodVibe(m) => match m {
                        crate::analysis::MoodType::Chill => 2,
                        crate::analysis::MoodType::Workout => 3,
                        crate::analysis::MoodType::Groove | crate::analysis::MoodType::Focus => 4,
                    },
                    crate::analysis::MixProfile::SeedRadio(_) => 0,
                };
            }
            (KeyCode::Char('L'), _) | (KeyCode::Char('l'), KeyModifiers::SHIFT) => {
                self.open_add_picker();
            }
            _ => {
                // Tab specific key handling
                self.handle_tab_key_with_modifiers(key.code, key.modifiers);
            }
        }
        Ok(false)
    }

    pub fn open_fuzzy_search(&mut self) {
        let target_dir = if self.in_bookmarks_view {
            if self.browser_cursor > 0 && self.browser_cursor <= self.bookmarks.len() {
                PathBuf::from(&self.bookmarks[self.browser_cursor - 1])
            } else {
                self.browser_dir.clone()
            }
        } else {
            self.browser_dir.clone()
        };

        // Background thread: send cached tracks for target_dir first (the cache covers the whole
        // library and can be large), then stream newly discovered tracks.
        let (tx, rx) = std::sync::mpsc::channel();
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let scan_dir = target_dir.clone();

        if let Some(mut previous) = self.search_modal.take() {
            previous.close();
        }
        self.workers.spawn("search-scan", move || {
            let cached_map = crate::library::LibraryManager::load_cache_map();
            let initial_tracks: Vec<crate::metadata::AudioMetadata> = cached_map
                .values()
                .filter(|t| t.filepath.starts_with(&scan_dir))
                .cloned()
                .collect();
            if !initial_tracks.is_empty() && tx.send(initial_tracks).is_err() {
                return;
            }
            crate::library::LibraryManager::scan_directory_streaming(scan_dir, cached_map, tx, stop_clone);
        });

        let target_name = target_dir.file_name().unwrap_or_default().to_string_lossy();
        self.status_message = format!("Fuzzy search in {}", target_name);
        let modal = search::SearchModalState::new_with_scanner(target_dir, Vec::new(), Some(rx), Some(stop_flag));
        self.search_modal = Some(modal);
    }

    pub fn handle_tab_key(&mut self, code: KeyCode) {
        self.handle_tab_key_with_modifiers(code, KeyModifiers::NONE);
    }

    pub fn handle_tab_key_with_modifiers(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match self.active_tab {
            // Now Playing Tab: `r` is its only tab key
            0 if (code, modifiers) == (KeyCode::Char('r'), KeyModifiers::NONE) => {
                if let Some(current) = self.queue.current_track() {
                    self.start_seed_radio(current.filepath.clone());
                } else {
                    self.status_message = "No track playing to start Seed Radio".to_string();
                }
            }
            1 => {
                // Queue Tab (Split pane)
                match (code, modifiers) {
                    (KeyCode::Char('h'), KeyModifiers::NONE) => self.queue_focus = false,
                    (KeyCode::Char('l'), KeyModifiers::NONE) => self.queue_focus = true,
                    (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        if self.queue_focus {
                            if !self.queue.is_empty() {
                                if self.queue_cursor > 0 {
                                    self.queue_cursor -= 1;
                                } else {
                                    self.queue_cursor = self.queue.len() - 1;
                                }
                            }
                        } else if !self.browser_entries.is_empty() {
                            if self.browser_cursor > 0 {
                                self.browser_cursor -= 1;
                            } else {
                                self.browser_cursor = self.browser_entries.len() - 1;
                            }
                        }
                    }
                    (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        if self.queue_focus {
                            if !self.queue.is_empty() {
                                if self.queue_cursor + 1 < self.queue.len() {
                                    self.queue_cursor += 1;
                                } else {
                                    self.queue_cursor = 0;
                                }
                            }
                        } else if !self.browser_entries.is_empty() {
                            if self.browser_cursor + 1 < self.browser_entries.len() {
                                self.browser_cursor += 1;
                            } else {
                                self.browser_cursor = 0;
                            }
                        }
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Char('u'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('b'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('K'), _)
                    | (KeyCode::Up, KeyModifiers::SHIFT) => {
                        if self.queue_focus {
                            self.page_up_queue();
                        } else {
                            self.page_up_browser();
                        }
                    }
                    (KeyCode::PageDown, _)
                    | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('f'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('J'), _)
                    | (KeyCode::Down, KeyModifiers::SHIFT) => {
                        if self.queue_focus {
                            self.page_down_queue();
                        } else {
                            self.page_down_browser();
                        }
                    }
                    (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                        if self.queue_focus {
                            self.home_queue();
                        } else {
                            self.home_browser();
                        }
                    }
                    (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                        if self.queue_focus {
                            self.end_queue();
                        } else {
                            self.end_browser();
                        }
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if self.queue_focus {
                            self.supersede_playlist_queue();
                            self.queue.current_index = self.queue_cursor;
                            self.play_current_track();
                        } else if self.browser_cursor < self.browser_entries.len() {
                            let (_, path, is_dir) = self.browser_entries[self.browser_cursor].clone();
                            if is_dir {
                                self.navigate_browser(path);
                            } else {
                                let meta = self.quick_metadata(&path);
                                let title = meta.display_title().to_string();
                                self.enqueue_tracks(vec![meta]);
                                self.status_message = format!("Added to queue: {}", title);
                            }
                        }
                    }
                    (KeyCode::Backspace, _) if !self.queue_focus => {
                        if self.in_bookmarks_view {
                            self.in_bookmarks_view = false;
                            self.refresh_browser();
                        } else if let Some(parent) = self.browser_dir.parent() {
                            let parent_path = parent.to_path_buf();
                            self.navigate_browser(parent_path);
                        }
                    }
                    (KeyCode::Char('a'), KeyModifiers::NONE) if !self.in_bookmarks_view => {
                        let tracks: Vec<_> = self
                            .browser_entries
                            .iter()
                            .filter(|(_, _, is_dir)| !is_dir)
                            .map(|(_, path, _)| self.quick_metadata(path))
                            .collect();
                        let added = self.enqueue_tracks(tracks);
                        self.status_message = format!("Added {} tracks", added);
                    }
                    (KeyCode::Char('b'), KeyModifiers::NONE) => {
                        // Toggle bookmark
                        let path_str = self.browser_dir.to_string_lossy().to_string();
                        if self.bookmarks.contains(&path_str) {
                            self.bookmarks.retain(|b| b != &path_str);
                            self.save_bookmarks();
                            self.status_message = "Removed bookmark".to_string();
                        } else {
                            self.bookmarks.push(path_str);
                            self.save_bookmarks();
                            self.status_message = "Saved bookmark".to_string();
                        }
                    }
                    (KeyCode::Char('d'), KeyModifiers::NONE)
                    | (KeyCode::Char('x'), KeyModifiers::NONE)
                    | (KeyCode::Delete, _) => {
                        if self.queue_focus {
                            if let Some(removed) = self.remove_queue_track(self.queue_cursor) {
                                self.status_message = format!("Removed {}", removed.display_title());
                            }
                        } else if self.in_bookmarks_view && self.browser_cursor > 0 {
                            let idx = self.browser_cursor - 1;
                            if idx < self.bookmarks.len() {
                                self.bookmarks.remove(idx);
                                self.save_bookmarks();
                                self.refresh_browser();
                                self.status_message = "Removed bookmark".to_string();
                            }
                        }
                    }
                    (KeyCode::Char('c'), KeyModifiers::NONE) if self.queue_focus => {
                        self.supersede_playlist_queue();
                        self.queue.clear();
                        self.clamp_queue_cursor();
                        self.status_message = "Cleared queue".to_string();
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) if !self.queue_focus => {
                        if !self.in_bookmarks_view && self.browser_cursor < self.browser_entries.len() {
                            let (_, p, is_dir) = &self.browser_entries[self.browser_cursor];
                            if !*is_dir && p.to_string_lossy() != "___MUSIC_HOMES___" {
                                self.start_seed_radio(p.clone());
                                return;
                            }
                        }
                        self.browser_dir_cache.remove(&self.browser_dir);
                        self.refresh_browser();
                        self.status_message = format!("Refreshing directory '{}'", self.browser_dir.display());
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) if self.queue_focus => {
                        if self.queue_cursor < self.queue.len() {
                            let seed = self.queue.tracks[self.queue_cursor].filepath.clone();
                            self.start_seed_radio(seed);
                        } else if let Some(current) = self.queue.current_track() {
                            self.start_seed_radio(current.filepath.clone());
                        } else {
                            self.status_message = "No track in queue to start Seed Radio".to_string();
                        }
                    }
                    (KeyCode::Char('z') | KeyCode::Char('Z'), _) if self.queue_focus && !self.queue.is_empty() => {
                        self.queue.shuffle();
                        self.status_message = "🔀 Queue shuffled".to_string();
                        self.update_web_state();
                    }
                    (KeyCode::Char('H'), _) => {
                        let target = if self.in_bookmarks_view
                            && self.browser_cursor > 0
                            && self.browser_cursor <= self.bookmarks.len()
                        {
                            self.bookmarks[self.browser_cursor - 1].clone()
                        } else {
                            self.browser_dir.to_string_lossy().to_string()
                        };
                        if self.settings_mgr.settings.home_dir.as_deref() == Some(&target) {
                            self.settings_mgr.settings.home_dir = None;
                            self.settings_mgr.save();
                            self.status_message = "Cleared startup home (defaulting to launch folder)".to_string();
                        } else {
                            self.settings_mgr.settings.home_dir = Some(target.clone());
                            self.settings_mgr.save();
                            self.status_message = format!("Set startup home directory: {}", target);
                        }
                        self.update_web_state();
                    }
                    (KeyCode::Char('~'), _) => {
                        let home = if let Some(ref h) = self.settings_mgr.settings.home_dir {
                            PathBuf::from(h)
                        } else if let Some(base) = directories::BaseDirs::new() {
                            base.home_dir().to_path_buf()
                        } else {
                            std::env::current_dir().unwrap_or_default()
                        };
                        if home.is_dir() {
                            self.status_message = format!("Jumped to home: {}", home.display());
                            self.navigate_browser(home);
                        } else {
                            self.status_message = format!("Home directory not found: {}", home.display());
                        }
                    }
                    _ => {}
                }
            }
            2 => {
                // Devices Tab
                let devices = self.device_manager.get_devices();
                let total_devices = 1 + devices.len(); // 0 is Local Audio, 1.. are DLNA renderers
                match (code, modifiers) {
                    (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        if self.devices_cursor > 0 {
                            self.devices_cursor -= 1;
                        } else {
                            self.devices_cursor = total_devices.saturating_sub(1);
                        }
                    }
                    (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        if self.devices_cursor + 1 < total_devices {
                            self.devices_cursor += 1;
                        } else {
                            self.devices_cursor = 0;
                        }
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Char('u'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('K'), _)
                    | (KeyCode::Up, KeyModifiers::SHIFT) => {
                        let step = 10;
                        if self.devices_cursor >= step {
                            self.devices_cursor -= step;
                        } else if self.devices_cursor > 0 {
                            self.devices_cursor = 0;
                        } else {
                            self.devices_cursor = total_devices.saturating_sub(1);
                        }
                    }
                    (KeyCode::PageDown, _)
                    | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('J'), _)
                    | (KeyCode::Down, KeyModifiers::SHIFT) => {
                        let step = 10;
                        if self.devices_cursor + step < total_devices {
                            self.devices_cursor += step;
                        } else if self.devices_cursor < total_devices.saturating_sub(1) {
                            self.devices_cursor = total_devices.saturating_sub(1);
                        } else {
                            self.devices_cursor = 0;
                        }
                    }
                    (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                        self.devices_cursor = 0;
                    }
                    (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                        self.devices_cursor = total_devices.saturating_sub(1);
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if self.devices_cursor == 0 {
                            self.switch_to_local_player();
                        } else if let Some(dev) = devices.get(self.devices_cursor - 1) {
                            self.switch_to_upnp_player(dev.clone());
                        }
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => {
                        self.device_manager.start_background_scan();
                        self.status_message = "Scanning for DLNA MediaRenderers...".to_string();
                    }
                    _ => {}
                }
            }
            3 => {
                // Settings Tab
                match (code, modifiers) {
                    (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        if self.settings_cursor > 0 {
                            self.settings_cursor -= 1;
                        } else {
                            self.settings_cursor = SETTING_DEFINITIONS.len() - 1;
                        }
                    }
                    (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        if self.settings_cursor + 1 < SETTING_DEFINITIONS.len() {
                            self.settings_cursor += 1;
                        } else {
                            self.settings_cursor = 0;
                        }
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Home, _)
                    | (KeyCode::Char('g'), KeyModifiers::NONE)
                    | (KeyCode::Char('K'), _)
                    | (KeyCode::Up, KeyModifiers::SHIFT) => {
                        self.settings_cursor = 0;
                    }
                    (KeyCode::PageDown, _)
                    | (KeyCode::End, _)
                    | (KeyCode::Char('G'), _)
                    | (KeyCode::Char('J'), _)
                    | (KeyCode::Down, KeyModifiers::SHIFT) => {
                        self.settings_cursor = SETTING_DEFINITIONS.len() - 1;
                    }
                    (KeyCode::Char('e'), KeyModifiers::NONE) if self.settings_cursor == 7 => {
                        let curr = self.settings_mgr.settings.home_dir.clone().unwrap_or_default();
                        self.editing_home_dir = Some(curr);
                        self.status_message = "Editing Startup Home Directory (Enter: Save, Esc: Cancel)".to_string();
                    }
                    (KeyCode::Char('c'), KeyModifiers::NONE) | (KeyCode::Delete, _) if self.settings_cursor == 7 => {
                        self.settings_mgr.settings.home_dir = None;
                        self.settings_mgr.save();
                        self.status_message = "Cleared startup home (defaulting to launch directory)".to_string();
                        self.update_web_state();
                    }
                    (KeyCode::Left, KeyModifiers::NONE) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                        self.cycle_and_apply_setting(false);
                    }
                    (KeyCode::Right, KeyModifiers::NONE) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
                        self.cycle_and_apply_setting(true);
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if self.settings_cursor == 7 {
                            let curr = self.settings_mgr.settings.home_dir.clone().unwrap_or_default();
                            self.editing_home_dir = Some(curr);
                            self.status_message =
                                "Editing Startup Home Directory (Enter: Save, Esc: Cancel)".to_string();
                        } else {
                            self.cycle_and_apply_setting(true);
                        }
                    }
                    (KeyCode::Char(' '), KeyModifiers::NONE) => {
                        self.cycle_and_apply_setting(true);
                    }
                    _ => {}
                }
            }
            4 => self.handle_playlists_tab_key(code, modifiers),
            _ => {}
        }
    }

    pub fn cycle_and_apply_setting(&mut self, forward: bool) {
        self.settings_mgr.cycle_setting(self.settings_cursor, forward);
        if self.settings_cursor == 0 {
            self.update_browser_entry_labels();
        } else if self.settings_cursor == 1 {
            let backend = self.settings_mgr.settings.player_backend.clone();
            if self.device_manager.get_active_device().is_none() {
                let volume = self.player.volume();
                let mut new_player = spawn_local_player(&backend, volume);
                let _ = new_player.set_volume(volume);
                let old = std::mem::replace(&mut self.player, new_player);
                self.retire_player(old);
            }
        } else if self.settings_cursor == 6 || SETTING_DEFINITIONS[self.settings_cursor].key == "web_server_lan" {
            self.sync_web_server();
        } else if self.settings_cursor == 7 {
            self.update_web_state();
        }
        self.status_message = format!(
            "{}: {}",
            SETTING_DEFINITIONS[self.settings_cursor].label,
            self.settings_mgr.get_display_value(self.settings_cursor)
        );
    }

    /// Play the queue's current track. Returns the playback error, if `player.play` failed (it
    /// is also put in the status as "Playback error: ...").
    pub fn play_current_track(&mut self) -> Option<String> {
        self.pending_track_skip = false;
        self.last_track_skip_time = None;
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                ws.stream_state().continuous.skip_to_index(self.queue.current_index);
                self.update_web_state();
                return None;
            }
        }
        let mut playback_error = None;
        if let Some(track) = self.queue.current_track() {
            let path = track.filepath.clone();
            if track.duration_seconds <= 0.0 || track.artist.is_empty() {
                self.start_queue_metadata_fetch();
            }
            if let Some(ref ws) = self.web_server {
                ws.close_active_streams();
                ws.register_stream_file(&path);
            }
            if let Ok(mut s) = self.web_state.write() {
                s.filepath = Some(path.clone());
            }
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message.clear();
                }
                Err(e) => {
                    let message = format!("Playback error: {}", e);
                    self.status_message = message.clone();
                    playback_error = Some(message);
                }
            }
            self.update_web_state();
        }
        playback_error
    }

    pub fn play_next_track(&mut self) {
        self.pending_track_skip = false;
        self.last_track_skip_time = None;
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
                && !self.queue.is_empty()
            {
                let next_idx = (self.queue.current_index + 1) % self.queue.len();
                self.queue.current_index = next_idx;
                self.queue_cursor = next_idx;
                ws.stream_state().continuous.skip_to_index(next_idx);
                self.update_web_state();
                return;
            }
        }
        let mode = self.settings_mgr.settings.queue_mode.clone();
        if let Some(track) = self.queue.next_track(&mode) {
            let path = track.filepath.clone();
            if track.duration_seconds <= 0.0 || track.artist.is_empty() {
                self.start_queue_metadata_fetch();
            }
            if let Some(ref ws) = self.web_server {
                ws.close_active_streams();
                ws.register_stream_file(&path);
            }
            if let Ok(mut s) = self.web_state.write() {
                s.filepath = Some(path.clone());
            }
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message.clear();
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
            self.update_web_state();
        }
    }

    pub fn play_prev_track(&mut self) {
        self.pending_track_skip = false;
        self.last_track_skip_time = None;
        if let Some(ref ws) = self.web_server {
            if ws
                .stream_state()
                .continuous
                .is_active
                .load(std::sync::atomic::Ordering::SeqCst)
                && !self.queue.is_empty()
            {
                let prev_idx = if self.queue.current_index > 0 {
                    self.queue.current_index - 1
                } else {
                    self.queue.len().saturating_sub(1)
                };
                self.queue.current_index = prev_idx;
                self.queue_cursor = prev_idx;
                ws.stream_state().continuous.skip_to_index(prev_idx);
                self.update_web_state();
                return;
            }
        }
        if let Some(track) = self.queue.prev_track() {
            let path = track.filepath.clone();
            if track.duration_seconds <= 0.0 || track.artist.is_empty() {
                self.start_queue_metadata_fetch();
            }
            if let Some(ref ws) = self.web_server {
                ws.close_active_streams();
                ws.register_stream_file(&path);
            }
            if let Ok(mut s) = self.web_state.write() {
                s.filepath = Some(path.clone());
            }
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message.clear();
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
            self.update_web_state();
        }
    }

    pub fn start_autopilot(&mut self, dir: PathBuf) {
        let mode = if self.queue.is_empty() {
            AutopilotRunMode::Fresh
        } else {
            AutopilotRunMode::Append
        };
        self.start_autopilot_with_mode(dir, mode);
    }

    pub fn reshuffle_autopilot(&mut self, dir: PathBuf) {
        let mode = if self.queue.is_empty() {
            AutopilotRunMode::Fresh
        } else {
            AutopilotRunMode::Reshuffle
        };
        self.start_autopilot_with_mode(dir, mode);
    }

    pub fn start_autopilot_with_mode(&mut self, dir: PathBuf, mode: AutopilotRunMode) {
        self.supersede_playlist_queue();
        // 1. Cancel any active autopilot scanner
        if let Some(stop) = self.autopilot_scan_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.autopilot_scan_stop = Some(stop_flag.clone());

        self.autopilot_dir = Some(dir.clone());
        self.is_autopilot_scanning = true;
        self.autopilot_run_mode = mode;
        self.autopilot_added_count = 0;

        let dir_display = dir.file_name().unwrap_or_default().to_string_lossy().to_string();

        match mode {
            AutopilotRunMode::Fresh => {
                // Clear current queue and stop previous playback
                self.queue.clear();
                self.queue.current_index = 0;
                self.queue_cursor = 0;
                self.status_message = format!("⚡ Autopilot: Discovering tracks from '{}'...", dir_display);
                let _ = self.player.stop();
            }
            AutopilotRunMode::Append => {
                // Keep current queue and playback uninterrupted
                self.status_message = format!("⚡ Autopilot: Appending tracks from '{}'...", dir_display);
            }
            AutopilotRunMode::Reshuffle => {
                // Keep tracks 0..=current_index uninterrupted; truncate everything after current_index
                if self.queue.current_index < self.queue.len() {
                    self.queue.tracks.truncate(self.queue.current_index + 1);
                }
                self.clamp_queue_cursor();
                self.status_message = format!(
                    "🎲 Autopilot: Reshuffling mix from next track forward (from '{}')...",
                    dir_display
                );
            }
        }

        // 3. Reuse any subdirectories already discovered by the browser if viewing the same folder
        let known_subdirs: Vec<PathBuf> = if self.browser_dir == dir {
            self.browser_entries
                .iter()
                .filter(|(_, p, is_dir)| *is_dir && p.to_string_lossy() != "___MUSIC_HOMES___")
                .map(|(_, p, _)| p.clone())
                .collect()
        } else {
            Vec::new()
        };

        let cache = self.metadata_cache.clone(); // shared handle, not a copy
        let (tx, rx) = std::sync::mpsc::channel();
        self.autopilot_rx = Some(rx);

        // 4. Spawn background worker to discover tracks and stream them progressively
        let scan_dir = dir.clone();
        let profile = self.active_mix_profile.clone();
        let db = self.analysis_db.clone();
        self.workers.spawn("autopilot", move || {
            crate::library::LibraryManager::scan_directory_autopilot_with_profile(
                scan_dir,
                known_subdirs,
                cache,
                profile,
                db,
                tx,
                stop_flag,
            );
        });

        self.update_web_state();
    }

    pub fn apply_mix_picker_selection(&mut self, index: usize) {
        self.show_mix_picker_modal = false;
        self.active_mix_profile = match index {
            1 => crate::analysis::MixProfile::BpmFlow {
                target_bpm: 120.0,
                tolerance: 0.15,
            },
            2 => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Chill),
            3 => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Workout),
            4 => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Groove),
            _ => crate::analysis::MixProfile::DiverseShuffle,
        };
        let profile_name = self.active_mix_profile.name();
        let scan_dir = if self.browser_dir.exists() && !self.in_bookmarks_view {
            self.browser_dir.clone()
        } else if let Some(ref d) = self.autopilot_dir {
            d.clone()
        } else {
            std::env::current_dir().unwrap_or_default()
        };
        self.status_message = format!("⚡ Mix Profile: '{}' - Starting Autopilot...", profile_name);
        self.start_autopilot(scan_dir);
    }

    pub fn start_seed_radio(&mut self, seed_path: PathBuf) {
        let scan_dir = if self.browser_dir.exists() && !self.in_bookmarks_view {
            self.browser_dir.clone()
        } else {
            seed_path.parent().unwrap_or(&self.browser_dir).to_path_buf()
        };
        self.active_mix_profile = crate::analysis::MixProfile::SeedRadio(seed_path.clone());
        let track_name = seed_path.file_stem().unwrap_or_default().to_string_lossy();
        self.status_message = format!("📻 Seed Radio: Initiating station from '{}'...", track_name);
        self.start_autopilot(scan_dir);
    }

    pub fn get_selected_track_path(&self) -> Option<PathBuf> {
        if self.active_tab == 1 {
            if self.queue_focus {
                if self.queue_cursor < self.queue.len() {
                    return Some(self.queue.tracks[self.queue_cursor].filepath.clone());
                }
            } else if self.browser_cursor < self.browser_entries.len() {
                let (_, p, is_dir) = &self.browser_entries[self.browser_cursor];
                if !is_dir && p.to_string_lossy() != "___MUSIC_HOMES___" {
                    return Some(p.clone());
                }
            }
        }
        self.queue.current_track().map(|t| t.filepath.clone())
    }

    /// Interfaces the web server must listen on right now: the LAN while a DLNA renderer is
    /// active or the user enabled LAN access, otherwise loopback only.
    pub fn required_web_bind(&self) -> crate::web::WebBind {
        let device = self.device_manager.get_active_device();
        self.web_bind_for_renderer(device.as_ref())
    }

    /// Interfaces needed while `renderer` (if any) streams from this server. A renderer on this
    /// computer (loopback address) reaches the server over loopback.
    fn web_bind_for_renderer(&self, renderer: Option<&crate::discovery::DLNADevice>) -> crate::web::WebBind {
        let remote_renderer = renderer.is_some_and(|d| {
            d.host != "localhost" && !d.host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        });
        if self.settings_mgr.settings.web_server_lan || remote_renderer {
            crate::web::WebBind::Lan
        } else {
            crate::web::WebBind::Loopback
        }
    }

    pub fn ensure_web_server_started(&mut self) -> u16 {
        let bind = self.required_web_bind();
        self.ensure_web_server_bound(bind)
            .unwrap_or(self.settings_mgr.settings.web_server_port)
    }

    /// Make sure the web server is running on `bind`, restarting it if it is bound differently.
    fn ensure_web_server_bound(&mut self, bind: crate::web::WebBind) -> anyhow::Result<u16> {
        if let Some(ref handle) = self.web_server {
            if handle.bind == bind {
                return Ok(handle.port);
            }
        }
        // Reuse the running server's port so open web remotes reconnect to the same address.
        let port = match self.web_server.take() {
            Some(old) => {
                let port = old.port;
                old.stop();
                port
            }
            None => self.settings_mgr.settings.web_server_port,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        match crate::web::WebServer::start_with_bind(bind, port, self.web_state.clone(), tx) {
            Ok(handle) => {
                let actual_port = handle.port;
                for track in &self.queue.tracks {
                    handle.register_stream_file(&track.filepath);
                }
                self.web_server = Some(handle);
                self.web_cmd_rx = Some(rx);
                Ok(actual_port)
            }
            Err(e) => {
                crate::log_info!("ui", "Failed to start streaming server: {}", e);
                self.web_cmd_rx = None;
                Err(e)
            }
        }
    }

    pub fn register_stream_file(&self, path: &std::path::Path) {
        if let Some(ref ws) = self.web_server {
            ws.register_stream_file(path);
        }
    }

    pub fn switch_to_local_player(&mut self) {
        let was_playing = self.player.is_playing();
        let was_paused = self.player.is_paused();
        let pos = self.player.position_seconds();
        let vol = self.player.volume();
        let current_track = self.queue.current_track().cloned();

        self.player.detach_stream_state();
        let _ = self.player.stop();
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
        }
        self.device_manager.set_active_device(None);

        let backend = self.settings_mgr.settings.player_backend.clone();
        let mut new_player = spawn_local_player(&backend, vol);
        let _ = new_player.set_volume(vol);

        if let Some(track) = current_track {
            if was_playing || was_paused {
                if let Ok(mut s) = self.web_state.write() {
                    s.filepath = Some(track.filepath.clone());
                }
                if new_player.play_at(&track.filepath, pos).is_ok() && was_paused {
                    let _ = new_player.pause();
                }
            }
        }

        let old = std::mem::replace(&mut self.player, new_player);
        self.retire_player(old);
        self.status_message = "Active output: Local Audio".to_string();
        self.sync_web_server();
        self.update_web_state();
    }

    pub fn switch_to_upnp_player(&mut self, dev: crate::discovery::DLNADevice) {
        // Re-selecting the active renderer is a no-op: a new player would queue Play while the
        // old player's delayed Stop could still reach the same device.
        if self
            .device_manager
            .get_active_device()
            .is_some_and(|d| d.udn == dev.udn)
        {
            self.status_message = format!("Active output: {}", dev.friendly_name);
            return;
        }
        let bind = self.web_bind_for_renderer(Some(&dev));
        let port = self
            .ensure_web_server_bound(bind)
            .unwrap_or(self.settings_mgr.settings.web_server_port);
        let was_playing = self.player.is_playing();
        let was_paused = self.player.is_paused();
        let pos = self.player.position_seconds();
        let current_track = self.queue.current_track().cloned();

        self.player.detach_stream_state();
        let _ = self.player.stop();
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
            for track in &self.queue.tracks {
                ws.register_stream_file(&track.filepath);
            }
        }
        self.device_manager.set_active_device(Some(dev.clone()));

        let stream_state = self.web_server.as_ref().map(|ws| ws.stream_state());
        let worker_dev = dev.clone();
        // Built on the worker thread: UPnPPlayer::new queries the renderer's volume over the network.
        // Do NOT overwrite the remote hardware volume with local software volume!
        // The UPnP device preserves its own hardware volume setting.
        let mut upnp_player: Box<dyn AudioPlayer> = Box::new(AsyncPlayer::spawn(
            move || Box::new(crate::player::upnp::UPnPPlayer::new(worker_dev, port).with_stream_state(stream_state)),
            // The renderer's volume is unknown until UPnPPlayer::new has queried it. Report 0 rather
            // than the local volume so a volume key in that window cannot jump the speaker.
            0,
        ));

        let status = format!("Active output: {}", dev.friendly_name);
        if let Some(track) = current_track {
            if was_playing || was_paused {
                if let Some(ref ws) = self.web_server {
                    ws.register_stream_file(&track.filepath);
                }
                if let Ok(mut s) = self.web_state.write() {
                    s.filepath = Some(track.filepath.clone());
                }
                if upnp_player.play_at(&track.filepath, pos).is_ok() && was_paused {
                    let _ = upnp_player.pause();
                }
            }
        }

        let old = std::mem::replace(&mut self.player, upnp_player);
        self.retire_player(old);
        self.status_message = status;
        self.update_web_state();
    }

    pub fn sync_web_server(&mut self) {
        let needs_server =
            self.settings_mgr.settings.web_server_enabled || self.device_manager.get_active_device().is_some();

        if needs_server {
            let bind = self.required_web_bind();
            let was_running = self.web_server.is_some();
            let already_bound = self.web_server.as_ref().is_some_and(|h| h.bind == bind);
            if !already_bound {
                match self.ensure_web_server_bound(bind) {
                    // A rebind (e.g. after switching output) keeps the caller's status message.
                    Ok(port) if !was_running => {
                        self.status_message = format!("Started server on port {}", port);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.status_message = format!("Failed to start server: {}", e);
                        // Only a failed cold start turns the setting off; a failed rebind may be transient.
                        if !was_running && self.settings_mgr.settings.web_server_enabled {
                            self.settings_mgr.settings.web_server_enabled = false;
                            self.settings_mgr.save();
                        }
                    }
                }
            }
        } else if let Some(handle) = self.web_server.take() {
            handle.stop();
            self.web_cmd_rx = None;
            self.status_message = "Stopped web server".to_string();
        }
    }

    pub fn handle_web_commands(&mut self) {
        let commands: Vec<crate::web::WebCommand> = if let Some(ref rx) = self.web_cmd_rx {
            let mut list = Vec::new();
            while let Ok(cmd) = rx.try_recv() {
                list.push(cmd);
            }
            list
        } else {
            Vec::new()
        };

        for cmd in commands {
            match cmd {
                crate::web::WebCommand::TogglePause => {
                    if self.player.is_playing() {
                        let _ = self.player.pause();
                        self.status_message = "Paused playback (web)".to_string();
                    } else if self.player.is_paused() {
                        let _ = self.player.resume();
                        self.status_message = "Resumed playback (web)".to_string();
                    } else {
                        self.play_current_track();
                    }
                }
                crate::web::WebCommand::Play => {
                    if self.player.is_paused() {
                        let _ = self.player.resume();
                    } else if !self.player.is_playing() {
                        self.play_current_track();
                    }
                }
                crate::web::WebCommand::Pause => {
                    if self.player.is_playing() {
                        let _ = self.player.pause();
                    }
                }
                crate::web::WebCommand::Next => {
                    self.skip_next_track_debounced();
                }
                crate::web::WebCommand::Prev => {
                    self.skip_prev_track_debounced();
                }
                crate::web::WebCommand::SetVolume(vol) => {
                    let _ = self.player.set_volume(vol);
                    self.status_message = format!("Volume {}% (web)", vol);
                }
                crate::web::WebCommand::SeekTo(target) => {
                    let _ = self.player.seek_to(target);
                    self.update_web_state();
                }
                crate::web::WebCommand::SeekRelative(delta) => {
                    let _ = self.player.seek(delta);
                    self.update_web_state();
                }
                crate::web::WebCommand::PlayIndex(idx) => {
                    if idx < self.queue.len() {
                        self.supersede_playlist_queue();
                        self.queue.current_index = idx;
                        self.queue_cursor = idx;
                        self.play_current_track();
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::RemoveIndex(idx) => {
                    if self.remove_queue_track(idx).is_some() {
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::ClearQueue => {
                    self.supersede_playlist_queue();
                    let _ = self.player.stop();
                    self.queue.clear();
                    self.clamp_queue_cursor();
                    self.update_web_state();
                }
                crate::web::WebCommand::AddFile(path) => {
                    if path.exists() && crate::metadata::is_supported_audio_file(&path) {
                        let meta = self.quick_metadata(&path);
                        let title = meta.display_title().to_string();
                        self.enqueue_tracks(vec![meta]);
                        self.status_message = format!("Added to queue (web): {}", title);
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::AddFolder(path) => {
                    // Listing a folder on a network share can be slow: do it off the UI thread.
                    let tx = self.queue_add_tx.clone();
                    self.workers.spawn("add-folder", move || {
                        if let Ok(entries) = std::fs::read_dir(&path) {
                            let mut files: Vec<std::path::PathBuf> = entries
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|p| crate::metadata::is_supported_audio_file(p))
                                .collect();
                            files.sort();
                            files.dedup(); // Some SMB shares return each entry several times (issue #35).
                            let tracks = files.iter().map(|p| crate::metadata::fallback_metadata(p)).collect();
                            let _ = tx.send(tracks);
                        }
                    });
                }
                crate::web::WebCommand::SelectDevice(id) => {
                    let devices = self.device_manager.get_devices();
                    if id == 0 {
                        self.switch_to_local_player();
                        self.status_message = "Switched to Local Audio (web)".to_string();
                    } else if let Some(dev) = devices.get(id.saturating_sub(1)) {
                        let name = dev.friendly_name.clone();
                        self.switch_to_upnp_player(dev.clone());
                        self.status_message = format!("Switched to {} (web)", name);
                    }
                    self.update_web_state();
                }
                crate::web::WebCommand::RescanDevices => {
                    self.device_manager.start_background_scan();
                    self.status_message = "Scanning for DLNA MediaRenderers (web)...".to_string();
                    self.update_web_state();
                }
                crate::web::WebCommand::SetQueueMode(mode) => {
                    self.settings_mgr.settings.queue_mode = mode;
                    let _ = self.settings_mgr.save();
                    self.update_web_state();
                }
                crate::web::WebCommand::SetHome(path_opt) => {
                    self.settings_mgr.settings.home_dir = path_opt.clone();
                    let _ = self.settings_mgr.save();
                    if let Some(ref p) = path_opt {
                        self.status_message = format!("Set startup home directory (web): {}", p);
                    } else {
                        self.status_message = "Cleared startup home directory (web)".to_string();
                    }
                    self.update_web_state();
                }
                crate::web::WebCommand::Autopilot(path_opt) => {
                    if let Some(target) = path_opt.or_else(|| self.web_terminal_folder()) {
                        self.start_autopilot(target);
                    }
                }
                crate::web::WebCommand::AutopilotReshuffle(path_opt) => {
                    if let Some(target) = path_opt.or_else(|| self.web_terminal_folder()) {
                        self.reshuffle_autopilot(target);
                    }
                }
                crate::web::WebCommand::ShuffleQueue => {
                    if !self.queue.is_empty() {
                        self.queue.shuffle();
                        self.status_message = "🔀 Queue shuffled (web)".to_string();
                        self.update_web_state();
                    }
                }
                crate::web::WebCommand::SetMixProfile(profile_str) => {
                    let profile = match profile_str.to_lowercase().as_str() {
                        "bpm_flow" | "bpm" => crate::analysis::MixProfile::BpmFlow {
                            target_bpm: 120.0,
                            tolerance: 0.15,
                        },
                        "chill" | "ambient" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Chill),
                        "workout" | "energy" => {
                            crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Workout)
                        }
                        "focus" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Focus),
                        "groove" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Groove),
                        _ => crate::analysis::MixProfile::DiverseShuffle,
                    };
                    self.active_mix_profile = profile;
                    self.status_message = format!("⚡ Mix Profile set to: {}", self.active_mix_profile.name());
                    self.update_web_state();
                }
                crate::web::WebCommand::SeedRadio(path) => {
                    self.start_seed_radio(path);
                }
                crate::web::WebCommand::PathRejected(path) => {
                    self.status_message = format!(
                        "Ignored web request: {} is outside your home, bookmarks and launch folder",
                        path.display()
                    );
                }
            }
        }
    }

    /// The folder the terminal is browsing, for web commands that name no folder. `None` (with a
    /// status message) when it lies outside the folders the web dashboard may act on.
    fn web_terminal_folder(&mut self) -> Option<PathBuf> {
        let resolved = crate::web_access::resolve_allowed_path(&self.browser_dir, &self.web_allowed_roots());
        if resolved.is_none() {
            self.status_message = format!(
                "Ignored web request: {} is outside your home, bookmarks and launch folder",
                self.browser_dir.display()
            );
        }
        resolved
    }

    /// Folders the web dashboard may browse and act on: the startup home, the bookmarks and the
    /// folder the browser opened at launch. Folders browsed later in the TUI are not included,
    /// and neither are the filesystem root or the home folder itself (too broad).
    pub fn web_allowed_roots(&self) -> Vec<PathBuf> {
        static LOGGED_SKIP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let home = crate::web_access::home_dir();
        let mut roots: Vec<PathBuf> = Vec::new();
        let candidates = self
            .settings_mgr
            .settings
            .home_dir
            .iter()
            .chain(self.bookmarks.iter())
            .map(PathBuf::from)
            .chain(std::iter::once(self.launch_dir.clone()));
        for root in candidates {
            if roots.contains(&root) {
                continue;
            }
            if !crate::web_access::is_acceptable_root(&root, home.as_deref()) {
                if root.is_absolute() && !LOGGED_SKIP.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    crate::log_info!(
                        "web",
                        "Not letting the web dashboard reach {}: too broad (use a music folder, bookmark or startup home)",
                        root.display()
                    );
                }
                continue;
            }
            roots.push(root);
        }
        roots
    }

    pub fn update_web_state(&mut self) {
        if self.web_server.is_none() {
            return;
        }
        if let Ok(mut state) = self.web_state.write() {
            state.is_playing = self.player.is_playing();
            state.is_paused = self.player.is_paused();
            state.position_seconds = self.player.position_seconds();
            state.duration_seconds = self.player.duration_seconds();
            state.volume = self.player.volume();
            state.output_device = self
                .device_manager
                .get_active_device()
                .map(|d| d.friendly_name)
                .unwrap_or_else(|| "Local Audio".to_string());
            state.queue_index = self.queue.current_index;
            state.queue_total = self.queue.len();
            state.queue_mode = self.settings_mgr.settings.queue_mode.clone();
            state.is_scanning = self.device_manager.is_scanning();
            state.home_dir = self.settings_mgr.settings.home_dir.clone();
            state.current_browser_dir = self.browser_dir.to_string_lossy().to_string();
            state.allowed_roots = self.web_allowed_roots();
            state.is_autopilot = self.autopilot_dir.is_some();
            state.autopilot_dir = self.autopilot_dir.as_ref().map(|d| d.to_string_lossy().to_string());
            state.active_mix_profile = self.active_mix_profile.name();

            state.queue = self
                .queue
                .tracks
                .iter()
                .enumerate()
                .map(|(idx, track)| crate::web::WebQueueItem {
                    index: idx,
                    title: track.display_title().to_string(),
                    artist: track.artist.clone(),
                    album: track.album.clone(),
                    duration_seconds: track.duration_seconds,
                    filepath: track.filepath.to_string_lossy().to_string(),
                    is_current: idx == self.queue.current_index,
                })
                .collect();

            let active_dev = self.device_manager.get_active_device();
            let mut devices = Vec::new();
            devices.push(crate::web::WebDeviceItem {
                id: 0,
                name: "Local Audio (mpv/ffplay)".to_string(),
                host_port: "Local Device".to_string(),
                is_active: active_dev.is_none(),
            });
            for (idx, dev) in self.device_manager.get_devices().iter().enumerate() {
                let is_active = active_dev.as_ref().map(|d| d.udn == dev.udn).unwrap_or(false);
                devices.push(crate::web::WebDeviceItem {
                    id: idx + 1,
                    name: dev.display_label(),
                    host_port: format!("{}:{}", dev.host, dev.port),
                    is_active,
                });
            }
            state.devices = devices;

            if let Some(track) = self.queue.current_track() {
                state.title = track.title.clone();
                state.artist = track.artist.clone();
                state.album = track.album.clone();
                state.genre = track.genre.clone();
                state.format = track.format_name.clone();
                state.bitrate_kbps = track.bitrate_kbps;
                state.sample_rate_hz = track.sample_rate_hz;
                state.channels = track.channels;
                state.filepath = Some(track.filepath.clone());
                state.has_cover = true;
                if let Some(feat) = self.analysis_db.get_features(&track.filepath) {
                    state.bpm = feat.bpm;
                    state.key = feat.display_key();
                } else {
                    state.bpm = None;
                    state.key = None;
                }
            } else {
                state.title = "No Track Loaded".to_string();
                state.artist = String::new();
                state.album = String::new();
                state.genre = String::new();
                state.format = String::new();
                state.bitrate_kbps = 0;
                state.sample_rate_hz = 0;
                state.channels = 2;
                state.filepath = None;
                state.has_cover = false;
                state.bpm = None;
                state.key = None;
            }

            if self.queue.len() > 1 && self.queue.current_index + 1 < self.queue.len() {
                state.next_track_title = self
                    .queue
                    .tracks
                    .get(self.queue.current_index + 1)
                    .map(|t| t.display_title().to_string());
            } else if !self.queue.is_empty() && self.settings_mgr.settings.queue_mode == "loop" {
                state.next_track_title = self.queue.tracks.first().map(|t| t.display_title().to_string());
            } else {
                state.next_track_title = None;
            }
        }
    }

    pub fn render(&mut self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Tab header
                Constraint::Min(5),    // Main workspace
                Constraint::Length(5), // Bottom playback bar
            ])
            .split(f.area());

        self.render_tabs(f, chunks[0]);

        match self.active_tab {
            0 => self.render_now_playing_tab(f, chunks[1]),
            1 => self.render_queue_tab(f, chunks[1]),
            2 => self.render_devices_tab(f, chunks[1]),
            3 => self.render_settings_tab(f, chunks[1]),
            4 => self.render_playlists_tab(f, chunks[1]),
            _ => {}
        }

        self.render_bottom_bar(f, chunks[2]);

        if let Some(ref mut modal) = self.search_modal {
            Self::render_search_modal(modal, f);
        } else if self.playlists.modal.is_some() {
            self.render_playlist_modal(f);
        } else if self.show_mix_picker_modal {
            self.render_mix_picker_modal(f);
        } else if self.show_help_modal {
            self.render_help_modal(f);
        }
    }

    fn render_tabs(&self, f: &mut Frame, area: Rect) {
        let titles = vec![
            " [1] Now Playing",
            " [2] Queue & Library",
            " [3] Devices",
            " [4] Settings",
            " [5] Playlists",
        ];
        // Zero padding keeps all five labels inside a standard 80-column terminal.
        let tabs = Tabs::new(titles)
            .padding("", "")
            .select(self.active_tab)
            .block(Block::default().borders(Borders::BOTTOM))
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        f.render_widget(tabs, area);
    }

    fn render_now_playing_tab(&mut self, f: &mut Frame, area: Rect) {
        if area.width < 10 || area.height < 4 {
            return;
        }

        let outer_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " [1] NOW PLAYING ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " [Space] Play/Pause   [←/→] Prev/Next Track   [[/]] Seek 10s   [+/-] Vol   [n] Queue & Library ",
                Style::default().fg(Color::DarkGray),
            ));

        let inner = outer_block.inner(area);
        f.render_widget(outer_block, area);

        // If terminal has plenty of vertical space, split into Hero Track and Up Next Queue
        let show_up_next = inner.height >= 14;
        let (hero_area, up_next_area) = if show_up_next {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(8), Constraint::Min(4)])
                .split(inner);
            (chunks[0], Some(chunks[1]))
        } else {
            (inner, None)
        };

        // --- 1. Hero Track Presentation ---
        let mut hero_lines = Vec::new();

        // Row 1: State Pill, Active Output, Volume, Mode Pill
        let mut row1 = Vec::new();
        if self.player.is_playing() {
            row1.push(Span::styled(
                " ▶ PLAYING ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if self.player.is_paused() {
            row1.push(Span::styled(
                " ⏸ PAUSED ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            row1.push(Span::styled(
                " ⏹ STOPPED ",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let active_dev_name = self
            .device_manager
            .get_active_device()
            .map(|d| d.friendly_name)
            .unwrap_or_else(|| format!("Local Audio ({})", self.settings_mgr.settings.player_backend));
        row1.push(Span::raw("  "));
        row1.push(Span::styled(
            format!("[Output: {}]", active_dev_name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        row1.push(Span::raw("  "));
        row1.push(Span::styled(
            format!("Vol: {}%", self.player.volume()),
            Style::default().fg(Color::White),
        ));
        row1.push(Span::raw("  "));
        let mode_label = match self.settings_mgr.settings.queue_mode.as_str() {
            "loop" => "Continuous (Loop)",
            "stop" => "Stop at End",
            "repeat_one" => "Repeat Track",
            m => m,
        };
        row1.push(Span::styled(
            format!("Mode: {}", mode_label),
            Style::default().fg(Color::DarkGray),
        ));

        if !self.queue.is_empty() {
            row1.push(Span::raw("  "));
            row1.push(Span::styled(
                format!("Track {} of {}", self.queue.current_index + 1, self.queue.len()),
                Style::default().fg(Color::Yellow),
            ));
        }

        hero_lines.push(Line::from(row1));

        if let Some(track) = self.queue.current_track() {
            // Row 2: Prominent Hero Title
            let title_line = Line::from(vec![
                Span::styled("♫  ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(
                    track.display_title(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
            ]);
            hero_lines.push(title_line);

            // Row 3: Artist & Album
            let mut artist_album_spans = Vec::new();
            if !track.artist.is_empty() {
                artist_album_spans.push(Span::styled(
                    &track.artist,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ));
            } else {
                artist_album_spans.push(Span::styled("Unknown Artist", Style::default().fg(Color::DarkGray)));
            }
            if !track.album.is_empty() {
                artist_album_spans.push(Span::styled("  —  ", Style::default().fg(Color::DarkGray)));
                artist_album_spans.push(Span::styled(&track.album, Style::default().fg(Color::White)));
            }
            hero_lines.push(Line::from(artist_album_spans));

            // Row 4: Format Quality Badges
            let mut badge_spans = Vec::new();
            if self.autopilot_dir.is_some() {
                badge_spans.push(Span::styled(
                    "[⚡ AUTOPILOT]",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                badge_spans.push(Span::raw(" "));
            }
            if !track.format_name.is_empty() {
                badge_spans.push(Span::styled(
                    format!("[{}]", track.format_name.to_uppercase()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                badge_spans.push(Span::raw(" "));
            }
            if track.sample_rate_hz > 0 {
                let khz = track.sample_rate_hz as f64 / 1000.0;
                badge_spans.push(Span::styled(
                    format!("[{:.1} kHz]", khz),
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ));
                badge_spans.push(Span::raw(" "));
            }
            if track.bitrate_kbps > 0 {
                badge_spans.push(Span::styled(
                    format!("[{} kbps]", track.bitrate_kbps),
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ));
                badge_spans.push(Span::raw(" "));
            }
            if track.channels > 0 {
                let chan_label = if track.channels == 1 {
                    "Mono"
                } else if track.channels == 2 {
                    "Stereo"
                } else {
                    "Multi"
                };
                badge_spans.push(Span::styled(
                    format!("[{}]", chan_label),
                    Style::default().fg(Color::White).bg(Color::DarkGray),
                ));
                badge_spans.push(Span::raw(" "));
            }
            if !track.genre.is_empty() {
                badge_spans.push(Span::styled(
                    format!("[{}]", track.genre),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ));
                badge_spans.push(Span::raw(" "));
            }
            if track.file_size > 0 {
                let mb = track.file_size as f64 / 1_048_576.0;
                badge_spans.push(Span::styled(
                    format!("[{:.1} MB]", mb),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            hero_lines.push(Line::from(badge_spans));

            // Row 5: Full-Width Scrubbable Timeline Progress Bar
            let pos = self
                .pending_seek_target
                .unwrap_or_else(|| self.player.position_seconds());
            let dur = if track.duration_seconds > 0.0 {
                track.duration_seconds
            } else {
                self.player.duration_seconds().max(1.0)
            };
            let pos_str = format_time(pos);
            let dur_str = format_time(dur);
            let ratio = (pos / dur).clamp(0.0, 1.0);

            let available_w = (hero_area.width as usize).saturating_sub(pos_str.len() + dur_str.len() + 6);
            let filled = (ratio * available_w as f64).round() as usize;
            let empty = available_w.saturating_sub(filled);

            let bar_str = format!("{}{}", "█".repeat(filled), "░".repeat(empty));

            let prog_line = Line::from(vec![
                Span::styled(pos_str, Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(bar_str, Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(dur_str, Style::default().fg(Color::DarkGray)),
            ]);
            hero_lines.push(prog_line);

            // Row 6: Percentage & Time Remaining
            let percent = (ratio * 100.0).round() as usize;
            let remaining = (dur - pos).max(0.0);
            let rem_str = format_time(remaining);
            let info_line = Line::from(vec![
                Span::styled(format!("{}% complete", percent), Style::default().fg(Color::White)),
                Span::styled("   •   ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("-{} remaining", rem_str), Style::default().fg(Color::DarkGray)),
                Span::styled("   •   ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    track.filepath.display().to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            hero_lines.push(info_line);
        } else {
            let empty_title = Line::from(vec![
                Span::styled("♫  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "No Track Loaded",
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
                ),
            ]);
            hero_lines.push(empty_title);
            hero_lines.push(Line::from(vec![Span::styled(
                "Press Space to play active queue, or navigate to [2] Queue & Library to browse your collection.",
                Style::default().fg(Color::DarkGray),
            )]));
        }

        f.render_widget(Paragraph::new(hero_lines), hero_area);

        // --- 2. Up Next in Queue Section ---
        if let Some(up_area) = up_next_area {
            let up_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(Span::styled(
                    " UP NEXT IN QUEUE ",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ));

            let up_inner = up_block.inner(up_area);
            f.render_widget(up_block, up_area);

            if self.queue.is_empty() {
                let up_lines = vec![Line::from(vec![Span::styled(
                    "Queue is empty. Select files or press 'a' in the library to enqueue tracks.",
                    Style::default().fg(Color::DarkGray),
                )])];
                f.render_widget(Paragraph::new(up_lines), up_inner);
            } else {
                let current_idx = self.queue.current_index;
                let next_idx = current_idx + 1;
                let max_items = (up_inner.height as usize).saturating_sub(1).max(1);
                let upcoming: Vec<_> = self
                    .queue
                    .tracks
                    .iter()
                    .enumerate()
                    .skip(next_idx)
                    .take(max_items)
                    .collect();

                if upcoming.is_empty() {
                    let loop_mode = self.settings_mgr.settings.queue_mode == "loop";
                    if loop_mode && self.queue.len() > 1 {
                        let first = &self.queue.tracks[0];
                        let up_lines = vec![Line::from(vec![
                            Span::styled(
                                "↺ End of queue reached. Loops to #1: ",
                                Style::default().fg(Color::Cyan),
                            ),
                            Span::styled(first.display_title(), Style::default().fg(Color::Yellow)),
                            Span::styled(format!("  —  {}", first.artist), Style::default().fg(Color::White)),
                        ])];
                        f.render_widget(Paragraph::new(up_lines), up_inner);
                    } else {
                        let up_lines = vec![Line::from(vec![Span::styled(
                            "End of playback queue. (Playback will stop after this track)",
                            Style::default().fg(Color::DarkGray),
                        )])];
                        f.render_widget(Paragraph::new(up_lines), up_inner);
                    }
                } else {
                    let metadata_view = self.settings_mgr.settings.metadata_view;
                    let (header, widths, rows) = if metadata_view {
                        let header = Row::new(vec![
                            Cell::from("#"),
                            Cell::from("TITLE"),
                            Cell::from("ARTIST"),
                            Cell::from("TIME"),
                        ])
                        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

                        let widths = vec![
                            Constraint::Length(5),
                            Constraint::Fill(3),
                            Constraint::Fill(2),
                            Constraint::Length(7),
                        ];

                        let rows: Vec<Row> = upcoming
                            .into_iter()
                            .map(|(i, t)| {
                                let dur_label = if t.duration_seconds > 0.0 {
                                    format_time(t.duration_seconds)
                                } else {
                                    "--:--".to_string()
                                };
                                let artist_text = if !t.artist.is_empty() {
                                    t.artist.as_str()
                                } else {
                                    "Unknown Artist"
                                };
                                Row::new(vec![
                                    Cell::from(Span::styled(
                                        format!("#{:02}", i + 1),
                                        Style::default().fg(Color::Cyan),
                                    )),
                                    Cell::from(Span::styled(t.display_title(), Style::default().fg(Color::White))),
                                    Cell::from(Span::styled(artist_text, Style::default().fg(Color::DarkGray))),
                                    Cell::from(Span::styled(dur_label, Style::default().fg(Color::DarkGray))),
                                ])
                            })
                            .collect();

                        (header, widths, rows)
                    } else {
                        let header = Row::new(vec![Cell::from("#"), Cell::from("FILE"), Cell::from("TIME")])
                            .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

                        let widths = vec![Constraint::Length(5), Constraint::Fill(1), Constraint::Length(7)];

                        let rows: Vec<Row> = upcoming
                            .into_iter()
                            .map(|(i, t)| {
                                let dur_label = if t.duration_seconds > 0.0 {
                                    format_time(t.duration_seconds)
                                } else {
                                    "--:--".to_string()
                                };
                                let fname = if !t.filename.is_empty() {
                                    t.filename.as_str()
                                } else {
                                    t.filepath.file_name().and_then(|f| f.to_str()).unwrap_or("")
                                };
                                Row::new(vec![
                                    Cell::from(Span::styled(
                                        format!("#{:02}", i + 1),
                                        Style::default().fg(Color::Cyan),
                                    )),
                                    Cell::from(Span::styled(fname, Style::default().fg(Color::White))),
                                    Cell::from(Span::styled(dur_label, Style::default().fg(Color::DarkGray))),
                                ])
                            })
                            .collect();

                        (header, widths, rows)
                    };

                    let table = Table::new(rows, widths).header(header).column_spacing(2);

                    f.render_widget(table, up_inner);
                }
            }
        }
    }

    fn render_queue_tab(&mut self, f: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Left Pane: File Browser (with optional inline Spotlight Search bar)
        let spotlight_matches = self.spotlight_matches();
        let (browser_area, spotlight_area) = if self.browser_spotlight.is_some() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(1)])
                .split(split[0]);
            (chunks[1], Some(chunks[0]))
        } else {
            (split[0], None)
        };

        let inner_browser_h = browser_area.height.saturating_sub(2) as usize;
        self.browser_page_height = inner_browser_h.max(1);

        let inner_queue_h = split[1].height.saturating_sub(2) as usize;
        self.queue_page_height = inner_queue_h.max(1);

        if let Some(s_area) = spotlight_area {
            let query = self.browser_spotlight.as_deref().unwrap_or("");
            let match_text = if query.is_empty() {
                let subfolder_count = self
                    .browser_entries
                    .iter()
                    .filter(|(_, p, is_dir)| *is_dir && p.to_string_lossy() != "___MUSIC_HOMES___")
                    .count();
                format!("Type to jump ({} subfolders)", subfolder_count)
            } else if spotlight_matches.is_empty() {
                "No matching subfolders".to_string()
            } else {
                let current_match_pos = spotlight_matches
                    .iter()
                    .position(|&i| i == self.browser_cursor)
                    .map(|p| p + 1)
                    .unwrap_or(1);
                format!(
                    "{} of {} matching subfolders",
                    current_match_pos,
                    spotlight_matches.len()
                )
            };

            let spotlight_block = Block::default()
                .borders(Borders::ALL)
                .title(" 🔍 SPOTLIGHT SUBFOLDER JUMP (Esc exit, Enter open) ")
                .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

            let status_style = if spotlight_matches.is_empty() && !query.is_empty() {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let spotlight_text = Line::from(vec![
                Span::styled(" > ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(query, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled("█", Style::default().fg(Color::Yellow)),
                Span::raw("   "),
                Span::styled(format!("[{}]", match_text), status_style),
            ]);
            let spotlight_widget = Paragraph::new(spotlight_text).block(spotlight_block);
            f.render_widget(spotlight_widget, s_area);
        }

        let mode_tag = if self.settings_mgr.settings.metadata_view {
            "[TAGS]"
        } else {
            "[FILES]"
        };
        let count_tag = if self.browser_entries.is_empty() {
            String::new()
        } else {
            format!(" ({}/{})", self.browser_cursor + 1, self.browser_entries.len())
        };
        let loading_tag = if self.browser_loading { " [LOADING...]" } else { "" };
        let browser_title = if self.in_bookmarks_view {
            format!(" BROWSER [★ Music Homes]{}{} {} ", count_tag, loading_tag, mode_tag)
        } else {
            format!(" BROWSER [File System]{}{} {} ", count_tag, loading_tag, mode_tag)
        };
        let browser_block = Block::default()
            .borders(Borders::ALL)
            .title(browser_title.as_str())
            .border_style(if !self.queue_focus {
                if self.browser_spotlight.is_some() {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let match_set: std::collections::HashSet<usize> = if self.browser_spotlight.is_some() {
            spotlight_matches.iter().copied().collect()
        } else {
            std::collections::HashSet::new()
        };

        let mut browser_items: Vec<ListItem> = self
            .browser_entries
            .iter()
            .enumerate()
            .map(|(idx, (name, _, is_dir))| {
                let is_selected = idx == self.browser_cursor && !self.queue_focus;
                let is_spotlight_match = match_set.contains(&idx);

                let style = if is_selected {
                    if self.browser_spotlight.is_some() {
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    }
                } else if is_spotlight_match {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else if *is_dir {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Green)
                };
                ListItem::new(name.clone()).style(style)
            })
            .collect();

        if self.browser_loading {
            let header_count = if self.browser_dir.parent().is_some() { 2 } else { 1 };
            let content_count = self.browser_entries.len().saturating_sub(header_count);
            let loading_text = if content_count > 0 {
                format!("  ⏳ Reading directory contents... ({} items loaded)", content_count)
            } else {
                "  ⏳ Reading directory contents...".to_string()
            };
            browser_items.push(
                ListItem::new(loading_text).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC)),
            );
        }

        self.browser_state.select(Some(self.browser_cursor));
        let browser_list = List::new(browser_items).block(browser_block);
        f.render_stateful_widget(browser_list, browser_area, &mut self.browser_state);

        // Right Pane: Queue
        let queue_title = if self.queue.is_empty() {
            " QUEUE (0 tracks) ".to_string()
        } else {
            format!(" QUEUE ({}/{} tracks) ", self.queue_cursor + 1, self.queue.len())
        };
        let queue_block = Block::default()
            .borders(Borders::ALL)
            .title(queue_title.as_str())
            .border_style(if self.queue_focus {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let metadata_view = self.settings_mgr.settings.metadata_view;
        let queue_items: Vec<ListItem> = self
            .queue
            .tracks
            .iter()
            .enumerate()
            .map(|(idx, track)| {
                let prefix = if idx == self.queue.current_index { "▶ " } else { "  " };
                let text = format!("{}{}. {}", prefix, idx + 1, track.format_queue_label(metadata_view));
                let style = if idx == self.queue_cursor && self.queue_focus {
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(text).style(style)
            })
            .collect();

        self.queue_state.select(Some(self.queue_cursor));
        let queue_list = List::new(queue_items).block(queue_block);
        f.render_stateful_widget(queue_list, split[1], &mut self.queue_state);
    }

    fn render_devices_tab(&mut self, f: &mut Frame, area: Rect) {
        let is_scanning = self.device_manager.is_scanning();
        let title = if is_scanning {
            " AUDIO OUTPUT DEVICES (Scanning...) "
        } else {
            " AUDIO OUTPUT DEVICES "
        };
        let block = Block::default().borders(Borders::ALL).title(title);

        let active_dev = self.device_manager.get_active_device();
        let devices = self.device_manager.get_devices();

        let mut items = Vec::new();

        // [0] Local Audio
        let is_local_active = active_dev.is_none();
        let local_bullet = if is_local_active { "●" } else { "○" };
        let local_status = if is_local_active { " (Active)" } else { "" };
        let local_text = format!("{} Local Audio (mpv/ffplay){}", local_bullet, local_status);
        let local_style = if self.devices_cursor == 0 {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else if is_local_active {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::White)
        };
        items.push(ListItem::new(local_text).style(local_style));

        // [1..] Discovered Network Renderers
        for (idx, dev) in devices.iter().enumerate() {
            let cursor_idx = idx + 1;
            let is_active = active_dev.as_ref().map(|d| d.udn == dev.udn).unwrap_or(false);
            let bullet = if is_active { "●" } else { "○" };
            let status = if is_active { " (Active)" } else { "" };
            let text = format!(
                "{} {} [{}:{}]{}",
                bullet,
                dev.display_label(),
                dev.host,
                dev.port,
                status
            );
            let style = if self.devices_cursor == cursor_idx {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
            };
            items.push(ListItem::new(text).style(style));
        }

        if devices.is_empty() && is_scanning {
            items.push(
                ListItem::new("  (Searching for DLNA / UPnP MediaRenderers...)")
                    .style(Style::default().fg(Color::DarkGray)),
            );
        }

        self.devices_state.select(Some(self.devices_cursor));
        let list = List::new(items).block(block);
        f.render_stateful_widget(list, area, &mut self.devices_state);
    }

    fn render_settings_tab(&mut self, f: &mut Frame, area: Rect) {
        let mut block = Block::default().borders(Borders::ALL).title(" APPLICATION SETTINGS ");

        if self.editing_home_dir.is_some() {
            block = block.title_bottom(Span::styled(
                " [Enter] Save Path   [Esc] Cancel Editing   [Backspace] Delete ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        } else if self.settings_cursor == 7 {
            block = block.title_bottom(Span::styled(
                " [Enter / e] Edit Path   [c] Clear   [←/→] Cycle Presets   [↑/↓] Navigate ",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            block = block.title_bottom(Span::styled(
                " [Enter / Space / ←/→] Change   [↑/↓] Navigate ",
                Style::default().fg(Color::DarkGray),
            ));
        }

        let items: Vec<ListItem> = SETTING_DEFINITIONS
            .iter()
            .enumerate()
            .map(|(idx, defn)| {
                if idx == 7 && self.editing_home_dir.is_some() {
                    let edit_val = self.editing_home_dir.as_deref().unwrap_or("");
                    let text = format!("{:<28} : [ {}█ ]", defn.label, edit_val);
                    ListItem::new(text).style(
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    let val_str = self.settings_mgr.get_display_value(idx);
                    let text = format!("{:<28} : {}", defn.label, val_str);
                    let style = if idx == self.settings_cursor {
                        Style::default()
                            .bg(Color::Blue)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    ListItem::new(text).style(style)
                }
            })
            .collect();

        self.settings_state.select(Some(self.settings_cursor));
        let list = List::new(items).block(block);
        f.render_stateful_widget(list, area, &mut self.settings_state);
    }

    fn render_search_modal(modal: &mut SearchModalState, f: &mut Frame) {
        let area = centered_rect(84, 80, f.area());
        f.render_widget(Clear, area);

        let dir_name = modal
            .root_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| modal.root_dir.to_string_lossy().to_string());

        let outer_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" [🔍 Fuzzy Search: {}] ", dir_name))
            .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        let inner_area = outer_block.inner(area);
        f.render_widget(outer_block, area);

        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Search input bar
                Constraint::Min(5),    // Results and detail split
                Constraint::Length(3), // Action legend bar
            ])
            .split(inner_area);

        // 1. Search Query Input Bar
        let match_count = modal.filtered.len();
        let total_count = modal.all_tracks.len();
        let scan_badge = if modal.is_scanning {
            format!(" [Scanning... {}]", modal.spinner_char())
        } else {
            String::new()
        };
        let query_title = format!(
            " Search Query ({} of {} tracks matched){} ",
            match_count, total_count, scan_badge
        );
        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(query_title)
            .border_style(Style::default().fg(Color::Cyan));
        let query_text = format!("🔍 {}█", modal.query);
        let input_widget = Paragraph::new(query_text)
            .block(input_block)
            .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
        f.render_widget(input_widget, vertical[0]);

        // 2. Split Workspace: Results list on Left, Track detail card on Right
        let content_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(vertical[1]);

        // Results list
        let results_items: Vec<ListItem> = modal
            .filtered
            .iter()
            .enumerate()
            .map(|(idx, (_, track))| {
                let label = track.format_display_label(true);
                let style = if idx == modal.selected_index {
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(label).style(style)
            })
            .collect();

        let results_block = Block::default()
            .borders(Borders::ALL)
            .title(" Ranked Matches ")
            .border_style(Style::default().fg(Color::DarkGray));
        let results_list = List::new(results_items).block(results_block);
        f.render_stateful_widget(results_list, content_split[0], &mut modal.list_state);

        // Track detail card
        let detail_block = Block::default()
            .borders(Borders::ALL)
            .title(" Track Details ")
            .border_style(Style::default().fg(Color::DarkGray));

        if let Some(track) = modal.selected_track() {
            let rel_path = track
                .filepath
                .strip_prefix(&modal.root_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| track.filepath.to_string_lossy().to_string());

            let detail_lines = vec![
                Line::from(vec![
                    Span::styled("Title:    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        track.display_title(),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Artist:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if track.artist.is_empty() {
                            "<Unknown>"
                        } else {
                            &track.artist
                        },
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Album:    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        if track.album.is_empty() {
                            "<Unknown>"
                        } else {
                            &track.album
                        },
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Duration: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(track.formatted_duration(), Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled("Format:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!(
                            "{} ({} kbps, {} Hz)",
                            track.format_name, track.bitrate_kbps, track.sample_rate_hz
                        ),
                        Style::default().fg(Color::Green),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Channels: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}", track.channels), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("Size:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:.2} MB", track.file_size as f64 / 1_048_576.0),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "Relative Path:",
                    Style::default().fg(Color::DarkGray),
                )]),
                Line::from(vec![Span::styled(rel_path, Style::default().fg(Color::Blue))]),
            ];
            let detail_p = Paragraph::new(detail_lines).block(detail_block);
            f.render_widget(detail_p, content_split[1]);
        } else {
            let empty_p = Paragraph::new("No tracks match your query.")
                .block(detail_block)
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(empty_p, content_split[1]);
        }

        // 3. Action legend bar
        let action_spans = vec![
            Span::styled(
                " [Enter] ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("Add & Close  │  "),
            Span::styled(
                " [Tab] ",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw("Add to Queue  │  "),
            Span::styled(
                " [Shift+Tab] ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("Add All ({})  │  ", match_count)),
            Span::styled(" [Esc] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw("Cancel"),
        ];
        let action_block = Block::default().borders(Borders::ALL);
        let action_p = Paragraph::new(Line::from(action_spans)).block(action_block);
        f.render_widget(action_p, vertical[2]);
    }

    fn render_bottom_bar(&self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            " NOW PLAYING ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(block, area);

        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Row 1: Music metadata
                Constraint::Length(1), // Row 2: Status & Controls
                Constraint::Length(1), // Row 3: Progress gauge
            ])
            .margin(1)
            .split(area);

        if inner.len() < 3 {
            return;
        }

        // --- Row 1: Dedicated solely to music metadata (Title, Artist, Album, Format/Bitrate) ---
        let row1_line = if let Some(track) = self.queue.current_track() {
            let mut spans = Vec::new();

            // Title (Bold Yellow)
            spans.push(Span::styled(
                track.display_title(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));

            // Artist (Cyan)
            if !track.artist.is_empty() {
                spans.push(Span::styled("  —  ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    &track.artist,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ));
            }

            // Album (White)
            if !track.album.is_empty() {
                spans.push(Span::styled("  —  ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(&track.album, Style::default().fg(Color::White)));
            }

            // Format / Bitrate (Gray)
            let mut fmt_parts = Vec::new();
            if !track.format_name.is_empty() {
                fmt_parts.push(track.format_name.to_uppercase());
            }
            if track.bitrate_kbps > 0 {
                fmt_parts.push(format!("{}kbps", track.bitrate_kbps));
            }
            if !fmt_parts.is_empty() {
                spans.push(Span::styled(
                    format!("  ({})", fmt_parts.join(" ")),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            if let Some(feat) = self.analysis_db.get_features(&track.filepath) {
                let mut feat_parts = Vec::new();
                if let Some(bpm) = feat.bpm {
                    feat_parts.push(format!("{:.0} BPM", bpm));
                }
                if let Some(ref key) = feat.display_key() {
                    feat_parts.push(key.clone());
                }
                if !feat_parts.is_empty() {
                    spans.push(Span::styled("  •  ", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled(
                        feat_parts.join(" • "),
                        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                    ));
                }
            }

            Line::from(spans)
        } else {
            Line::from(vec![Span::styled(
                "No Track Loaded",
                Style::default().fg(Color::DarkGray),
            )])
        };
        f.render_widget(Paragraph::new(row1_line), inner[0]);

        // --- Row 2: State badge, Active Device, Volume, Web Server URL/Port, Notices ---
        let mut row2_spans = Vec::new();

        // State badge
        if self.pending_track_skip {
            row2_spans.push(Span::styled(
                "⏳ QUEUED",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        } else if self.player.is_playing() {
            row2_spans.push(Span::styled(
                "▶ PLAYING",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        } else if self.player.is_paused() {
            row2_spans.push(Span::styled(
                "⏸ PAUSED",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        } else {
            row2_spans.push(Span::styled(
                "⏹ STOPPED",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            ));
        }

        if self.autopilot_dir.is_some() {
            row2_spans.push(Span::raw(" "));
            row2_spans.push(Span::styled(
                format!("[⚡ {}]", self.active_mix_profile.name()),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }

        // Output Device (without "Out:" prefix)
        let active_dev_name = self
            .device_manager
            .get_active_device()
            .map(|d| d.friendly_name)
            .unwrap_or_else(|| "Local Audio".to_string());
        row2_spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
        row2_spans.push(Span::styled(active_dev_name, Style::default().fg(Color::White)));

        // Volume
        let vol = self.player.volume();
        row2_spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
        row2_spans.push(Span::styled(
            format!("Vol: {}%", vol),
            Style::default().fg(Color::White),
        ));

        // Web server badge
        if let Some(ref s) = self.web_server {
            row2_spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
            row2_spans.push(Span::styled(
                format!("🌐 :{}", s.port),
                Style::default().fg(Color::Cyan),
            ));
        }

        // Transient non-redundant notices
        let is_redundant = self.status_message.is_empty()
            || self.status_message.starts_with("Playing:")
            || self.status_message == "Playing"
            || self.status_message == "Paused"
            || self.status_message == "Stopped"
            || self.status_message.starts_with("Welcome to Quickstream");

        if !is_redundant {
            row2_spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
            let msg_lower = self.status_message.to_lowercase();
            let msg_style = if msg_lower.contains("error") || msg_lower.contains("failed") {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            };
            row2_spans.push(Span::styled(&self.status_message, msg_style));
        }

        f.render_widget(Paragraph::new(Line::from(row2_spans)), inner[1]);

        // --- Row 3: Progress bar gauge with MM:SS / MM:SS ---
        let (pos, dur) = if self.pending_track_skip {
            let dur = self.queue.current_track().map(|t| t.duration_seconds).unwrap_or(0.0);
            (0.0, dur)
        } else {
            let p = self
                .pending_seek_target
                .unwrap_or_else(|| self.player.position_seconds());
            (p, self.player.duration_seconds())
        };
        let pct = if dur > 0.0 { (pos / dur).min(1.0) } else { 0.0 };

        let pos_str = format_time(pos);
        let dur_str = format_time(dur);

        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(pct)
            .label(format!("{} / {}", pos_str, dur_str));
        f.render_widget(gauge, inner[2]);
    }

    fn render_help_modal(&self, f: &mut Frame) {
        let area = modal_rect_with_height(f.area(), HELP_MODAL_HEIGHT);
        f.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " QUICKSTREAM KEYBOARD CHEAT-SHEET ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));
        f.render_widget(block, area);

        let inner = if area.height >= 17 {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(15),   // 2-column key list
                    Constraint::Length(1), // Footer instruction
                ])
                .margin(1)
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1)])
                .margin(1)
                .split(area)
        };

        if inner.is_empty() {
            return;
        }

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner[0]);

        if cols.len() < 2 {
            return;
        }

        // Left Column: Playback & Navigation. Both columns must fit the 19 rows an 80x24
        // terminal leaves for the key list, and each description fits in 29 columns there.
        let left_lines = vec![
            Self::help_section_header("PLAYBACK & AUDIO"),
            Self::help_key_line("Space", "Play / Pause"),
            Self::help_key_line("s", "Stop playback"),
            Self::help_key_line("← / →", "Previous / Next track"),
            Self::help_key_line("[ / ]", "Seek -10s / +10s"),
            Self::help_key_line("+ / -", "Volume Up / Down"),
            Line::from(""),
            Self::help_section_header("NAVIGATION & TABS"),
            Self::help_key_line("n", "Toggle Now Playing ↔ Queue"),
            Self::help_key_line("1 - 5", "Tab: Now/Queue/Dev/Set/PL"),
            Self::help_key_line("h / l", "Focus panes (tabs 2 & 5)"),
            Self::help_key_line("j / k", "Move Up / Down (wraps)"),
            Self::help_key_line("J / K", "Page (2) / Move track (5)"),
            Self::help_key_line("g / G", "Jump to Start / End"),
            Self::help_key_line("Bksp", "Go up to parent dir"),
            Self::help_key_line("~", "Jump to startup home dir"),
            Self::help_section_header("PLAYLISTS (TAB 5)"),
            Self::help_key_line("c / R / D", "New / Rename / Delete"),
            Self::help_key_line("Enter/a", "Open/Play / Queue playlist"),
        ];
        f.render_widget(Paragraph::new(left_lines), cols[0]);

        // Right Column: Library & Search (concise to guarantee <= 34 chars)
        let right_lines = vec![
            Self::help_section_header("LIBRARY & QUEUE"),
            Self::help_key_line("Enter", "Open / Queue / Play"),
            Self::help_key_line("p", "Autopilot (append if playing)"),
            Self::help_key_line("P", "Autopilot Reshuffle (next+)"),
            Self::help_key_line("M", "Mix Picker modal (vibe/BPM)"),
            Self::help_key_line("r", "Seed Radio / Rescan dev"),
            Self::help_key_line("Z / z", "Shuffle current queue"),
            Self::help_key_line("a", "Add all songs in folder"),
            Self::help_key_line("L", "Add to playlist"),
            Self::help_key_line("d / x", "Remove track / bookmark"),
            Self::help_key_line("c", "Clear queue (queue pane)"),
            Self::help_key_line("b", "Bookmark folder in Homes"),
            Self::help_key_line("H", "Set folder as startup home"),
            Self::help_key_line("v", "Toggle tags vs filenames"),
            Line::from(""),
            Self::help_section_header("SEARCH & DISCOVERY"),
            Self::help_key_line("/", "Spotlight jump in folder"),
            Self::help_key_line("f", "Full library fuzzy search"),
        ];
        f.render_widget(Paragraph::new(right_lines), cols[1]);
    }

    fn render_mix_picker_modal(&self, f: &mut Frame) {
        let area = help_modal_rect(f.area());
        f.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " ⚡ AUTOPILOT MIX PROFILE SELECTOR ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));
        f.render_widget(block, area);

        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Subtitle
                Constraint::Min(10),   // Options list
                Constraint::Length(2), // Footer instruction
            ])
            .margin(1)
            .split(area);

        if inner.len() < 3 {
            return;
        }

        let header_p = Paragraph::new(Line::from(vec![Span::styled(
            "Select a mix profile to sequence cohesive playlists with DJ spacing & transitions:",
            Style::default().fg(Color::DarkGray),
        )]));
        f.render_widget(header_p, inner[0]);

        let options = [
            (
                "Diverse Artists Shuffle",
                "Fair random sampling across all artists (prevents cluster bias)",
            ),
            (
                "BPM Flow (~120 BPM)",
                "Cohesive tempo gradient with harmonic Camelot wheel transitions",
            ),
            (
                "Chill & Ambient",
                "Downtempo, acoustic, lofi & low energy vibes (60-95 BPM)",
            ),
            (
                "Workout & High Energy",
                "Dance, electronic, rock & upbeat vibes (124-145 BPM)",
            ),
            (
                "Late Night Groove",
                "Soul, funk, disco, house & groove vibes (110-128 BPM)",
            ),
        ];

        let mut lines = Vec::new();
        for (idx, (title, desc)) in options.iter().enumerate() {
            let is_cursor = idx == self.mix_picker_cursor;
            let is_active = matches!(
                (&self.active_mix_profile, idx),
                (crate::analysis::MixProfile::DiverseShuffle, 0)
                    | (crate::analysis::MixProfile::BpmFlow { .. }, 1)
                    | (
                        crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Chill),
                        2
                    )
                    | (
                        crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Workout),
                        3
                    )
                    | (
                        crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Groove),
                        4
                    )
            );

            let prefix = if is_cursor { " ▶ " } else { "   " };
            let active_marker = if is_active { " [ACTIVE] " } else { "          " };

            let num_key = format!("[{}] ", idx + 1);

            let line_style = if is_cursor {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(num_key, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<26}", title), line_style),
                Span::styled(
                    active_marker,
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("— {}", desc), Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(""));
        }

        f.render_widget(Paragraph::new(lines), inner[1]);

        let footer_line = Line::from(vec![
            Span::styled(
                " [1-5 / Enter] ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Select & Launch   ", Style::default().fg(Color::White)),
            Span::styled(" [r] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("Seed Radio on Track   ", Style::default().fg(Color::White)),
            Span::styled(
                " [Esc / q] ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Close", Style::default().fg(Color::White)),
        ]);
        f.render_widget(Paragraph::new(footer_line).alignment(Alignment::Center), inner[2]);

        // Footer: dismiss instruction (if vertical room allows)
        if inner.len() >= 2 {
            let footer_line = Line::from(vec![
                Span::styled("Press ", Style::default().fg(Color::DarkGray)),
                Span::styled("[?]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(", ", Style::default().fg(Color::DarkGray)),
                Span::styled("[Esc]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(", or ", Style::default().fg(Color::DarkGray)),
                Span::styled("[q]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(" to close", Style::default().fg(Color::DarkGray)),
            ]);
            f.render_widget(Paragraph::new(footer_line).alignment(Alignment::Center), inner[1]);
        }
    }

    fn help_section_header(title: &str) -> Line<'static> {
        Line::from(vec![Span::styled(
            format!("── {} ──", title),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )])
    }

    fn help_key_line(key: &str, desc: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!(" {:<7} ", key),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(desc.to_string(), Style::default().fg(Color::White)),
        ])
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn help_modal_rect(r: Rect) -> Rect {
    modal_rect_with_height(r, 20)
}

/// Rows of the help modal: the key list, its footer and the border must fit.
const HELP_MODAL_HEIGHT: u16 = 30;

/// Centered modal up to 90 columns wide and `height` rows tall, shrunk to fit small terminals.
fn modal_rect_with_height(r: Rect, height: u16) -> Rect {
    let max_w = if r.width >= 96 { 90 } else { r.width.saturating_sub(2) };
    let target_width = max_w.min(r.width);
    let target_height = height.min(r.height.saturating_sub(2)).min(r.height);

    let x = r.x + (r.width.saturating_sub(target_width)) / 2;
    let y = r.y + (r.height.saturating_sub(target_height)) / 2;
    Rect::new(x, y, target_width, target_height)
}

fn format_time(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    let mins = total / 60;
    let secs = total % 60;
    format!("{:02}:{:02}", mins, secs)
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
