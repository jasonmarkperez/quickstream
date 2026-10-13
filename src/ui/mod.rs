//! Interactive Terminal User Interface (Ratatui + Crossterm + Skim integration).

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::*;

pub mod search;

use self::search::SearchModalState;
use crate::discovery::DeviceManager;
use crate::metadata::is_supported_audio_file;
use crate::paths;
use crate::player::local::LocalPlayer;
use crate::player::AudioPlayer;
use crate::queue::PlaybackQueue;
use crate::settings::{SettingsManager, SETTING_DEFINITIONS};

pub struct AppUI {
    pub active_tab: usize,
    pub queue_focus: bool, // false = browser, true = queue
    pub browser_dir: PathBuf,
    pub browser_entries: Vec<(String, PathBuf, bool)>, // (display_name, path, is_dir)
    pub browser_cursor: usize,
    pub queue_cursor: usize,
    pub devices_cursor: usize,
    pub settings_cursor: usize,
    pub bookmarks: Vec<String>,
    pub in_bookmarks_view: bool,
    pub show_help_modal: bool,
    pub status_message: String,
    pub queue: PlaybackQueue,
    pub player: LocalPlayer,
    pub device_manager: DeviceManager,
    pub settings_mgr: SettingsManager,
    pub metadata_cache: std::collections::HashMap<PathBuf, crate::metadata::AudioMetadata>,
    pub browser_state: ListState,
    pub queue_state: ListState,
    pub devices_state: ListState,
    pub settings_state: ListState,
    pub search_modal: Option<SearchModalState>,
    pub browser_page_height: usize,
    pub queue_page_height: usize,
    pub web_server: Option<crate::web::WebServerHandle>,
    pub web_state: std::sync::Arc<std::sync::RwLock<crate::web::NowPlayingState>>,
    pub web_cmd_rx: Option<std::sync::mpsc::Receiver<crate::web::WebCommand>>,
}

impl AppUI {
    pub fn new(start_dir: Option<PathBuf>, settings_mgr: SettingsManager) -> Self {
        let cwd = start_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let backend = settings_mgr.settings.player_backend.clone();
        let bookmarks = Self::load_bookmarks();
        let device_manager = DeviceManager::new();
        device_manager.start_background_scan();

        let web_state = std::sync::Arc::new(std::sync::RwLock::new(crate::web::NowPlayingState::default()));
        let (web_server, web_cmd_rx) = if settings_mgr.settings.web_server_enabled {
            let (tx, rx) = std::sync::mpsc::channel();
            match crate::web::WebServer::start(
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

        let mut app = Self {
            active_tab: 0,
            queue_focus: false,
            browser_dir: cwd,
            browser_entries: Vec::new(),
            browser_cursor: 0,
            queue_cursor: 0,
            devices_cursor: 0,
            settings_cursor: 0,
            bookmarks,
            in_bookmarks_view: false,
            show_help_modal: false,
            status_message: "Welcome to Quickstream (Rust)".to_string(),
            queue: PlaybackQueue::new(),
            player: LocalPlayer::new(&backend),
            device_manager,
            settings_mgr,
            metadata_cache: std::collections::HashMap::new(),
            browser_state: ListState::default(),
            queue_state: ListState::default(),
            devices_state: ListState::default(),
            settings_state: ListState::default(),
            search_modal: None,
            browser_page_height: 15,
            queue_page_height: 15,
            web_server,
            web_state,
            web_cmd_rx,
        };
        app.refresh_browser();
        app
    }

    fn load_bookmarks() -> Vec<String> {
        let path = paths::get_bookmarks_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(list) = serde_json::from_str::<Vec<String>>(&content) {
                    return list;
                }
            }
        }
        Vec::new()
    }

    fn save_bookmarks(&self) {
        let path = paths::get_bookmarks_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.bookmarks) {
            let _ = fs::write(path, json);
        }
    }

    pub fn refresh_browser(&mut self) {
        let old_cursor = self.browser_cursor;
        self.browser_entries.clear();

        if self.in_bookmarks_view {
            self.browser_entries
                .push((".. [Back to File System]".to_string(), self.browser_dir.clone(), true));
            for b in &self.bookmarks {
                let p = PathBuf::from(b);
                let label = if p.is_dir() {
                    format!("📁 {}/", p.file_name().unwrap_or_default().to_string_lossy())
                } else {
                    format!("📁 {} (Not found)/", p.file_name().unwrap_or_default().to_string_lossy())
                };
                self.browser_entries.push((label, p, true));
            }
            self.browser_cursor = old_cursor.min(self.browser_entries.len().saturating_sub(1));
            if self.browser_cursor == 0 {
                self.browser_state = ListState::default();
                self.browser_state.select(Some(0));
            }
            return;
        }

        self.browser_entries
            .push(("★ Music Homes".to_string(), PathBuf::from("___MUSIC_HOMES___"), true));

        if let Some(parent) = self.browser_dir.parent() {
            self.browser_entries
                .push((".. [Parent Directory]".to_string(), parent.to_path_buf(), true));
        }

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.browser_dir) {
            let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();

            for p in paths {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') {
                    continue;
                }
                if p.is_dir() {
                    dirs.push((format!("📁 {}/", name), p, true));
                } else if p.is_file() && is_supported_audio_file(&p) {
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_uppercase();
                    let label = if self.settings_mgr.settings.metadata_view {
                        let meta = match self.metadata_cache.get(&p) {
                            Some(m) => Some(m.clone()),
                            None => match crate::metadata::extract_metadata(&p) {
                                Ok(m) => {
                                    self.metadata_cache.insert(p.clone(), m.clone());
                                    Some(m)
                                }
                                Err(_) => None,
                            },
                        };
                        match meta {
                            Some(m) => m.format_display_label(true),
                            None => format!("🎵 [{}] {}", ext, name),
                        }
                    } else {
                        format!("🎵 [{}] {}", ext, name)
                    };
                    files.push((label, p, false));
                }
            }
        }

        self.browser_entries.extend(dirs);
        self.browser_entries.extend(files);
        self.browser_cursor = old_cursor.min(self.browser_entries.len().saturating_sub(1));
        if self.browser_cursor == 0 {
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
        }
    }

    pub fn navigate_browser(&mut self, path: PathBuf) {
        if path.to_string_lossy() == "___MUSIC_HOMES___" {
            self.in_bookmarks_view = true;
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
            self.refresh_browser();
            return;
        }

        if self.in_bookmarks_view {
            if path.is_dir() {
                self.in_bookmarks_view = false;
                self.browser_dir = path;
                self.browser_cursor = 0;
                self.browser_state = ListState::default();
                self.browser_state.select(Some(0));
                self.refresh_browser();
            } else {
                self.status_message = "Bookmarked directory not found".to_string();
            }
            return;
        }

        if path.is_dir() {
            let prev_dir = self.browser_dir.clone();
            let is_parent = prev_dir.parent().map(|p| p == path).unwrap_or(false);

            self.browser_dir = path;
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
            self.refresh_browser();

            if is_parent {
                if let Some(pos) = self.browser_entries.iter().position(|(name, p, is_dir)| {
                    *is_dir && p == &prev_dir && name.starts_with("📁 ")
                }) {
                    self.browser_cursor = pos;
                    self.browser_state = ListState::default();
                    self.browser_state.select(Some(pos));
                }
            }
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

    pub fn run(&mut self) -> anyhow::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let res = self.event_loop(&mut terminal);

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        res
    }

    fn event_loop<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> anyhow::Result<()> {
        loop {
            // Check if active track finished and auto-advance queue
            if self.player.is_track_finished() {
                self.player.reset_finished();
                self.play_next_track();
            }

            // Process remote web commands
            self.handle_web_commands();

            // Synchronize web server live state
            self.update_web_state();

            if let Some(ref mut modal) = self.search_modal {
                modal.poll_background_scan();
            }

            terminal.draw(|f| self.render(f))?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if self.show_help_modal {
                        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                            self.show_help_modal = false;
                        }
                        continue;
                    }

                    if let Some(ref mut modal) = self.search_modal {
                        match (key.code, key.modifiers) {
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
                                    self.queue.add_track(track.clone());
                                    self.queue.current_index = self.queue.len().saturating_sub(1);
                                    self.play_current_track();
                                    self.status_message = format!("Playing: {}", track.display_title());
                                }
                            }
                            (KeyCode::Tab, KeyModifiers::NONE) => {
                                if let Some(track) = modal.selected_track().cloned() {
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

                    // Global hotkeys
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('q'), KeyModifiers::NONE) | (KeyCode::Esc, KeyModifiers::NONE) => {
                            self.player.stop()?;
                            return Ok(());
                        }
                        (KeyCode::Char('?'), KeyModifiers::NONE) => {
                            self.show_help_modal = true;
                        }
                        (KeyCode::Tab, KeyModifiers::NONE) => {
                            self.active_tab = (self.active_tab + 1) % 3;
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
                        (KeyCode::Char(' '), KeyModifiers::NONE) => {
                            if !self.player.is_playing() && !self.player.is_paused() {
                                if !self.queue.is_empty() {
                                    self.play_current_track();
                                } else {
                                    self.status_message = "No tracks in queue".to_string();
                                }
                            } else {
                                let paused = self.player.toggle_pause()?;
                                self.status_message = if paused { "Paused" } else { "Playing" }.to_string();
                            }
                        }
                        (KeyCode::Char('s'), KeyModifiers::NONE) => {
                            self.player.stop()?;
                            self.status_message = "Stopped".to_string();
                        }
                        (KeyCode::Right, KeyModifiers::NONE) => {
                            self.play_next_track();
                        }
                        (KeyCode::Left, KeyModifiers::NONE) => {
                            self.play_prev_track();
                        }
                        (KeyCode::Right, KeyModifiers::SHIFT) => {
                            let step = self.settings_mgr.settings.seek_step_seconds;
                            let _ = self.player.seek(step);
                            self.status_message = format!("Seek +{:.0}s", step);
                        }
                        (KeyCode::Left, KeyModifiers::SHIFT) => {
                            let step = self.settings_mgr.settings.seek_step_seconds;
                            let _ = self.player.seek(-step);
                            self.status_message = format!("Seek -{:.0}s", step);
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
                            self.settings_mgr.cycle_setting(0, true);
                            self.refresh_browser();
                            self.status_message = format!(
                                "Display mode: {}",
                                if self.settings_mgr.settings.metadata_view {
                                    "Metadata Tags (Artist - Title) [TAGS]"
                                } else {
                                    "Filenames [FILES]"
                                }
                            );
                        }
                        (KeyCode::Char('/') | KeyCode::Char('f'), KeyModifiers::NONE) => {
                            self.open_fuzzy_search();
                        }
                        _ => {
                            // Tab specific key handling
                            self.handle_tab_key_with_modifiers(key.code, key.modifiers);
                        }
                    }
                }
            }
        }
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

        // 1. Immediately pull any cached tracks for target_dir
        let cached_map = crate::library::LibraryManager::load_cache_map();
        let initial_tracks: Vec<crate::metadata::AudioMetadata> = cached_map
            .values()
            .filter(|t| t.filepath.starts_with(&target_dir))
            .cloned()
            .collect();

        for t in &initial_tracks {
            self.metadata_cache.insert(t.filepath.clone(), t.clone());
        }

        // 2. Set up background streaming scanner
        let (tx, rx) = std::sync::mpsc::channel();
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let scan_dir = target_dir.clone();
        let scan_cache = cached_map;

        std::thread::spawn(move || {
            crate::library::LibraryManager::scan_directory_streaming(
                scan_dir,
                scan_cache,
                tx,
                stop_clone,
            );
        });

        let target_name = target_dir.file_name().unwrap_or_default().to_string_lossy();
        self.status_message = format!("Fuzzy search in {}", target_name);
        let modal = search::SearchModalState::new_with_scanner(
            target_dir,
            initial_tracks,
            Some(rx),
            Some(stop_flag),
        );
        self.search_modal = Some(modal);
    }

    pub fn handle_tab_key(&mut self, code: KeyCode) {
        self.handle_tab_key_with_modifiers(code, KeyModifiers::NONE);
    }

    pub fn handle_tab_key_with_modifiers(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match self.active_tab {
            0 => {
                // Queue Tab (Split pane)
                match (code, modifiers) {
                    (KeyCode::Char('h'), KeyModifiers::NONE) => self.queue_focus = false,
                    (KeyCode::Char('l'), KeyModifiers::NONE) => self.queue_focus = true,
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
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
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
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
                    | (KeyCode::Char('K'), _) => {
                        if self.queue_focus {
                            self.page_up_queue();
                        } else {
                            self.page_up_browser();
                        }
                    }
                    (KeyCode::PageDown, _)
                    | (KeyCode::Char('d'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('f'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('J'), _) => {
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
                            self.queue.current_index = self.queue_cursor;
                            self.play_current_track();
                        } else if self.browser_cursor < self.browser_entries.len() {
                            let (_, path, is_dir) = self.browser_entries[self.browser_cursor].clone();
                            if is_dir {
                                self.navigate_browser(path);
                            } else {
                                let meta_opt = self.metadata_cache.get(&path).cloned()
                                    .or_else(|| crate::metadata::extract_metadata(&path).ok());
                                if let Some(meta) = meta_opt {
                                    self.queue.add_track(meta);
                                    self.queue.current_index = self.queue.len().saturating_sub(1);
                                    self.play_current_track();
                                }
                            }
                        }
                    }
                    (KeyCode::Char('a'), KeyModifiers::NONE) if !self.in_bookmarks_view => {
                        let mut added = 0;
                        for (_, path, is_dir) in &self.browser_entries {
                            if !is_dir {
                                let meta_opt = self.metadata_cache.get(path).cloned()
                                    .or_else(|| crate::metadata::extract_metadata(path).ok());
                                if let Some(meta) = meta_opt {
                                    self.queue.add_track(meta);
                                    added += 1;
                                }
                            }
                        }
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
                            if let Some(removed) = self.queue.remove_track(self.queue_cursor) {
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
                        self.queue.clear();
                        self.status_message = "Cleared queue".to_string();
                    }
                    _ => {}
                }
            }
            1 => {
                // Devices Tab
                let devices = self.device_manager.get_devices();
                let total_devices = 1 + devices.len(); // 0 is Local Audio, 1.. are DLNA renderers
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        if self.devices_cursor > 0 {
                            self.devices_cursor -= 1;
                        } else {
                            self.devices_cursor = total_devices.saturating_sub(1);
                        }
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        if self.devices_cursor + 1 < total_devices {
                            self.devices_cursor += 1;
                        } else {
                            self.devices_cursor = 0;
                        }
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Char('u'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('K'), _) => {
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
                    | (KeyCode::Char('J'), _) => {
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
                            self.device_manager.set_active_device(None);
                            self.status_message = "Active output: Local Audio".to_string();
                        } else if let Some(dev) = devices.get(self.devices_cursor - 1) {
                            self.device_manager.set_active_device(Some(dev.clone()));
                            self.status_message = format!("Active output: {}", dev.friendly_name);
                        }
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => {
                        self.device_manager.start_background_scan();
                        self.status_message = "Scanning for DLNA MediaRenderers...".to_string();
                    }
                    _ => {}
                }
            }
            2 => {
                // Settings Tab
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        if self.settings_cursor > 0 {
                            self.settings_cursor -= 1;
                        } else {
                            self.settings_cursor = SETTING_DEFINITIONS.len() - 1;
                        }
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        if self.settings_cursor + 1 < SETTING_DEFINITIONS.len() {
                            self.settings_cursor += 1;
                        } else {
                            self.settings_cursor = 0;
                        }
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Home, _)
                    | (KeyCode::Char('g'), KeyModifiers::NONE)
                    | (KeyCode::Char('K'), _) => {
                        self.settings_cursor = 0;
                    }
                    (KeyCode::PageDown, _)
                    | (KeyCode::End, _)
                    | (KeyCode::Char('G'), _)
                    | (KeyCode::Char('J'), _) => {
                        self.settings_cursor = SETTING_DEFINITIONS.len() - 1;
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                        self.settings_mgr.cycle_setting(self.settings_cursor, true);
                        if self.settings_cursor == 0 {
                            self.refresh_browser();
                        } else if self.settings_cursor == 1 {
                            let backend = self.settings_mgr.settings.player_backend.clone();
                            self.player = LocalPlayer::new(&backend);
                        } else if self.settings_cursor == 6 {
                            self.sync_web_server();
                        }
                        self.status_message = format!(
                            "{}: {}",
                            SETTING_DEFINITIONS[self.settings_cursor].label,
                            self.settings_mgr.get_display_value(self.settings_cursor)
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn play_current_track(&mut self) {
        if let Some(track) = self.queue.current_track() {
            let path = track.filepath.clone();
            let title = track.display_title().to_string();
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message = format!("Playing: {}", title);
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
        }
    }

    fn play_next_track(&mut self) {
        let mode = self.settings_mgr.settings.queue_mode.clone();
        if let Some(track) = self.queue.next_track(&mode) {
            let path = track.filepath.clone();
            let title = track.display_title().to_string();
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message = format!("Playing: {}", title);
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
        }
    }

    fn play_prev_track(&mut self) {
        if let Some(track) = self.queue.prev_track() {
            let path = track.filepath.clone();
            let title = track.display_title().to_string();
            match self.player.play(&path) {
                Ok(_) => {
                    self.status_message = format!("Playing: {}", title);
                }
                Err(e) => {
                    self.status_message = format!("Playback error: {}", e);
                }
            }
        }
    }

    pub fn sync_web_server(&mut self) {
        if self.settings_mgr.settings.web_server_enabled {
            if self.web_server.is_none() {
                let (tx, rx) = std::sync::mpsc::channel();
                match crate::web::WebServer::start(
                    self.settings_mgr.settings.web_server_port,
                    self.web_state.clone(),
                    tx,
                ) {
                    Ok(handle) => {
                        self.status_message = format!("Started web server on port {}", handle.port);
                        self.web_server = Some(handle);
                        self.web_cmd_rx = Some(rx);
                    }
                    Err(e) => {
                        self.status_message = format!("Failed to start web server: {}", e);
                        self.settings_mgr.settings.web_server_enabled = false;
                        self.settings_mgr.save();
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
                    self.play_next_track();
                }
                crate::web::WebCommand::Prev => {
                    self.play_prev_track();
                }
                crate::web::WebCommand::SetVolume(vol) => {
                    let _ = self.player.set_volume(vol);
                    self.status_message = format!("Volume {}% (web)", vol);
                }
                crate::web::WebCommand::SeekTo(target) => {
                    let _ = self.player.seek_to(target);
                }
                crate::web::WebCommand::SeekRelative(delta) => {
                    let _ = self.player.seek(delta);
                }
            }
        }
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
            }

            if self.queue.len() > 1 && self.queue.current_index + 1 < self.queue.len() {
                state.next_track_title = self.queue.tracks.get(self.queue.current_index + 1).map(|t| t.display_title().to_string());
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
                Constraint::Length(4), // Bottom playback bar
            ])
            .split(f.area());

        self.render_tabs(f, chunks[0]);

        match self.active_tab {
            0 => self.render_queue_tab(f, chunks[1]),
            1 => self.render_devices_tab(f, chunks[1]),
            2 => self.render_settings_tab(f, chunks[1]),
            _ => {}
        }

        self.render_bottom_bar(f, chunks[2]);

        if let Some(ref mut modal) = self.search_modal {
            Self::render_search_modal(modal, f);
        } else if self.show_help_modal {
            self.render_help_modal(f);
        }
    }

    fn render_tabs(&self, f: &mut Frame, area: Rect) {
        let titles = vec![" [1] Queue & Library ", " [2] Devices ", " [3] Settings "];
        let tabs = Tabs::new(titles)
            .select(self.active_tab)
            .block(Block::default().borders(Borders::BOTTOM))
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_widget(tabs, area);
    }

    fn render_queue_tab(&mut self, f: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let inner_browser_h = split[0].height.saturating_sub(2) as usize;
        self.browser_page_height = inner_browser_h.max(1);

        let inner_queue_h = split[1].height.saturating_sub(2) as usize;
        self.queue_page_height = inner_queue_h.max(1);

        // Left Pane: File Browser
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
        let browser_title = if self.in_bookmarks_view {
            format!(" BROWSER [★ Music Homes]{} {} ", count_tag, mode_tag)
        } else {
            format!(" BROWSER [File System]{} {} ", count_tag, mode_tag)
        };
        let browser_block = Block::default()
            .borders(Borders::ALL)
            .title(browser_title.as_str())
            .border_style(if !self.queue_focus {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            });

        let browser_items: Vec<ListItem> = self
            .browser_entries
            .iter()
            .enumerate()
            .map(|(idx, (name, _, is_dir))| {
                let style = if idx == self.browser_cursor && !self.queue_focus {
                    Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
                } else if *is_dir {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Green)
                };
                ListItem::new(name.clone()).style(style)
            })
            .collect();

        self.browser_state.select(Some(self.browser_cursor));
        let browser_list = List::new(browser_items).block(browser_block);
        f.render_stateful_widget(browser_list, split[0], &mut self.browser_state);

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

        let queue_items: Vec<ListItem> = self
            .queue
            .tracks
            .iter()
            .enumerate()
            .map(|(idx, track)| {
                let prefix = if idx == self.queue.current_index {
                    "▶ "
                } else {
                    "  "
                };
                let text = format!("{}{}. {}", prefix, idx + 1, track.format_display_label(true));
                let style = if idx == self.queue_cursor && self.queue_focus {
                    Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
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
            Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
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
            let text = format!("{} {} [{}:{}]{}", bullet, dev.display_label(), dev.host, dev.port, status);
            let style = if self.devices_cursor == cursor_idx {
                Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
            };
            items.push(ListItem::new(text).style(style));
        }

        if devices.is_empty() && is_scanning {
            items.push(ListItem::new("  (Searching for DLNA / UPnP MediaRenderers...)").style(Style::default().fg(Color::DarkGray)));
        }

        self.devices_state.select(Some(self.devices_cursor));
        let list = List::new(items).block(block);
        f.render_stateful_widget(list, area, &mut self.devices_state);
    }

    fn render_settings_tab(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" APPLICATION SETTINGS ");

        let items: Vec<ListItem> = SETTING_DEFINITIONS
            .iter()
            .enumerate()
            .map(|(idx, defn)| {
                let val_str = self.settings_mgr.get_display_value(idx);
                let text = format!("{:<28} : {}", defn.label, val_str);
                let style = if idx == self.settings_cursor {
                    Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(text).style(style)
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
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
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
                    Span::styled(
                        track.formatted_duration(),
                        Style::default().fg(Color::Cyan),
                    ),
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
                    Span::styled(
                        format!("{}", track.channels),
                        Style::default().fg(Color::White),
                    ),
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
                Line::from(vec![Span::styled(
                    rel_path,
                    Style::default().fg(Color::Blue),
                )]),
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
            Span::styled(" [Enter] ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw("Play Now  │  "),
            Span::styled(" [Tab] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("Add to Queue  │  "),
            Span::styled(" [Shift+Tab] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!("Add All ({})  │  ", match_count)),
            Span::styled(" [Esc] ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw("Cancel"),
        ];
        let action_block = Block::default().borders(Borders::ALL);
        let action_p = Paragraph::new(Line::from(action_spans)).block(action_block);
        f.render_widget(action_p, vertical[2]);
    }

    fn render_bottom_bar(&self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL);
        f.render_widget(block, area);

        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .margin(1)
            .split(area);

        // Status line
        let state = if self.player.is_playing() {
            "▶ PLAYING"
        } else if self.player.is_paused() {
            "⏸ PAUSED"
        } else {
            "⏹ STOPPED"
        };

        let title = self
            .queue
            .current_track()
            .map(|t| t.display_title())
            .unwrap_or("No Track Loaded");

        let active_dev_name = self
            .device_manager
            .get_active_device()
            .map(|d| d.friendly_name)
            .unwrap_or_else(|| "Local Audio".to_string());

        let vol = self.player.volume();
        let web_badge = if let Some(ref s) = self.web_server {
            format!(" │ 🌐 :{}", s.port)
        } else {
            String::new()
        };
        let status_text = format!(
            "{} │ {} │ Vol: {}% │ Out: {}{} │ {}",
            state, title, vol, active_dev_name, web_badge, self.status_message
        );
        f.render_widget(
            Paragraph::new(status_text).style(Style::default().fg(Color::Yellow)),
            inner[0],
        );

        // Progress bar with formatted MM:SS / MM:SS
        let pos = self.player.position_seconds();
        let dur = self.player.duration_seconds();
        let pct = if dur > 0.0 { (pos / dur).min(1.0) } else { 0.0 };

        let pos_str = format_time(pos);
        let dur_str = format_time(dur);

        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(pct)
            .label(format!("{} / {}", pos_str, dur_str));
        f.render_widget(gauge, inner[1]);
    }

    fn render_help_modal(&self, f: &mut Frame) {
        let area = centered_rect(60, 60, f.area());
        f.render_widget(Clear, area);

        let text = vec![
            Line::from(vec![Span::styled(" QUICKSTREAM KEYBOARD CHEAT-SHEET ", Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow))]),
            Line::from(""),
            Line::from("[Tab] / [1,2,3]  : Switch Active Tab"),
            Line::from("[h] / [l]        : Focus Browser vs Queue"),
            Line::from("[j] / [k]        : Move Selection Up / Down (wraps around)"),
            Line::from("[PgDn] / [PgUp]  : Page Down / Up through list (also Ctrl+d/u, J/K)"),
            Line::from("[Home] / [End]   : Jump to Start / End of list (also g / G)"),
            Line::from("[Enter]          : Open folder / Add file to queue / Play track"),
            Line::from("[/] or [f]       : Fuzzy Search current folder & subfolders"),
            Line::from("[a]              : Add all audio files in folder to queue"),
            Line::from("[b]              : Bookmark / Unbookmark folder in Music Homes"),
            Line::from("[d] / [x]        : Remove item from queue or bookmarks"),
            Line::from("[c]              : Clear Queue"),
            Line::from("[v]              : Toggle Metadata Tags vs Filenames"),
            Line::from("[Space]          : Play / Pause"),
            Line::from("[s]              : Stop Playback"),
            Line::from("[Shift+Left/Right: Seek Backward / Forward 10s"),
            Line::from("[+] / [-]        : Volume Up / Down"),
            Line::from("[?]              : Toggle this Help Modal"),
            Line::from("[q] / [Esc]      : Exit Quickstream"),
        ];

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" [?] Help ")
            .style(Style::default().bg(Color::Black));
        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
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

fn format_time(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    let mins = total / 60;
    let secs = total % 60;
    format!("{:02}:{:02}", mins, secs)
}
