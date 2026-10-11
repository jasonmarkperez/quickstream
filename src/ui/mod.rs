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

use crate::discovery::DeviceManager;
use crate::library::LibraryManager;
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
}

impl AppUI {
    pub fn new(start_dir: Option<PathBuf>, settings_mgr: SettingsManager) -> Self {
        let cwd = start_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let backend = settings_mgr.settings.player_backend.clone();
        let bookmarks = Self::load_bookmarks();
        let device_manager = DeviceManager::new();
        device_manager.start_background_scan();

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
            self.browser_cursor = 0;
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
                    let label = if self.settings_mgr.settings.metadata_view {
                        if let Ok(meta) = crate::metadata::extract_metadata(&p) {
                            meta.format_display_label(true)
                        } else {
                            format!("🎵 {}", name)
                        }
                    } else {
                        format!("🎵 {}", name)
                    };
                    files.push((label, p, false));
                }
            }
        }

        self.browser_entries.extend(dirs);
        self.browser_entries.extend(files);
        self.browser_cursor = 0;
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

            terminal.draw(|f| self.render(f))?;

            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if self.show_help_modal {
                        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                            self.show_help_modal = false;
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
                                self.settings_mgr.get_display_value(0)
                            );
                        }
                        (KeyCode::Char('/') | KeyCode::Char('f'), KeyModifiers::NONE) => {
                            // Skim interactive fuzzy search
                            self.trigger_skim_search(terminal)?;
                        }
                        _ => {
                            // Tab specific key handling
                            self.handle_tab_key(key.code);
                        }
                    }
                }
            }
        }
    }

    fn trigger_skim_search<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> anyhow::Result<()> {
        disable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, LeaveAlternateScreen)?;

        let mut tracks = LibraryManager::load_cache();
        if tracks.is_empty() {
            tracks = LibraryManager::scan_directory(&self.browser_dir);
            LibraryManager::save_cache(&tracks);
        }

        let selected = LibraryManager::search_with_skim(&tracks);

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        terminal.clear()?;

        if !selected.is_empty() {
            let count = selected.len();
            for track in selected {
                self.queue.add_track(track);
            }
            self.status_message = format!("Added {} tracks via Skim search", count);
        }

        Ok(())
    }

    fn handle_tab_key(&mut self, code: KeyCode) {
        match self.active_tab {
            0 => {
                // Queue Tab (Split pane)
                match code {
                    KeyCode::Char('h') => self.queue_focus = false,
                    KeyCode::Char('l') => self.queue_focus = true,
                    KeyCode::Up | KeyCode::Char('k') => {
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
                    KeyCode::Down | KeyCode::Char('j') => {
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
                    KeyCode::Enter => {
                        if self.queue_focus {
                            self.queue.current_index = self.queue_cursor;
                            self.play_current_track();
                        } else if self.browser_cursor < self.browser_entries.len() {
                            let (_, path, is_dir) = &self.browser_entries[self.browser_cursor];
                            if *is_dir {
                                if path.to_string_lossy() == "___MUSIC_HOMES___" {
                                    self.in_bookmarks_view = true;
                                    self.refresh_browser();
                                } else {
                                    if self.in_bookmarks_view {
                                        self.in_bookmarks_view = false;
                                    }
                                    if path.is_dir() {
                                        self.browser_dir = path.clone();
                                        self.refresh_browser();
                                    }
                                }
                            } else if let Ok(meta) = crate::metadata::extract_metadata(path) {
                                self.queue.add_track(meta.clone());
                                if !self.player.is_playing() && !self.player.is_paused() {
                                    self.queue.current_index = self.queue.len().saturating_sub(1);
                                    self.play_current_track();
                                } else {
                                    self.status_message = format!("Added {}", meta.display_title());
                                }
                            }
                        }
                    }
                    KeyCode::Char('a') if !self.in_bookmarks_view => {
                        let mut added = 0;
                        for (_, path, is_dir) in &self.browser_entries {
                            if !is_dir {
                                if let Ok(meta) = crate::metadata::extract_metadata(path) {
                                    self.queue.add_track(meta);
                                    added += 1;
                                }
                            }
                        }
                        self.status_message = format!("Added {} tracks", added);
                    }
                    KeyCode::Char('b') => {
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
                    KeyCode::Char('d') | KeyCode::Char('x') | KeyCode::Delete => {
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
                    KeyCode::Char('c') if self.queue_focus => {
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
                match code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.devices_cursor > 0 {
                            self.devices_cursor -= 1;
                        } else {
                            self.devices_cursor = total_devices.saturating_sub(1);
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.devices_cursor + 1 < total_devices {
                            self.devices_cursor += 1;
                        } else {
                            self.devices_cursor = 0;
                        }
                    }
                    KeyCode::Enter => {
                        if self.devices_cursor == 0 {
                            self.device_manager.set_active_device(None);
                            self.status_message = "Active output: Local Audio".to_string();
                        } else if let Some(dev) = devices.get(self.devices_cursor - 1) {
                            self.device_manager.set_active_device(Some(dev.clone()));
                            self.status_message = format!("Active output: {}", dev.friendly_name);
                        }
                    }
                    KeyCode::Char('r') => {
                        self.device_manager.start_background_scan();
                        self.status_message = "Scanning for DLNA MediaRenderers...".to_string();
                    }
                    _ => {}
                }
            }
            2 => {
                // Settings Tab
                match code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.settings_cursor > 0 {
                            self.settings_cursor -= 1;
                        } else {
                            self.settings_cursor = SETTING_DEFINITIONS.len() - 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.settings_cursor + 1 < SETTING_DEFINITIONS.len() {
                            self.settings_cursor += 1;
                        } else {
                            self.settings_cursor = 0;
                        }
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        self.settings_mgr.cycle_setting(self.settings_cursor, true);
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
            if self.player.play(&path).is_ok() {
                self.status_message = format!("Playing: {}", title);
            }
        }
    }

    fn play_next_track(&mut self) {
        let mode = self.settings_mgr.settings.queue_mode.clone();
        if let Some(track) = self.queue.next_track(&mode) {
            let path = track.filepath.clone();
            let title = track.display_title().to_string();
            if self.player.play(&path).is_ok() {
                self.status_message = format!("Playing: {}", title);
            }
        }
    }

    fn play_prev_track(&mut self) {
        if let Some(track) = self.queue.prev_track() {
            let path = track.filepath.clone();
            let title = track.display_title().to_string();
            if self.player.play(&path).is_ok() {
                self.status_message = format!("Playing: {}", title);
            }
        }
    }

    fn render(&self, f: &mut Frame) {
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

        if self.show_help_modal {
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

    fn render_queue_tab(&self, f: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Left Pane: File Browser
        let browser_title = if self.in_bookmarks_view {
            " BROWSER [★ Music Homes] "
        } else {
            " BROWSER [File System] "
        };
        let browser_block = Block::default()
            .borders(Borders::ALL)
            .title(browser_title)
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

        let browser_list = List::new(browser_items).block(browser_block);
        f.render_widget(browser_list, split[0]);

        // Right Pane: Queue
        let queue_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" QUEUE ({} tracks) ", self.queue.len()))
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

        let queue_list = List::new(queue_items).block(queue_block);
        f.render_widget(queue_list, split[1]);
    }

    fn render_devices_tab(&self, f: &mut Frame, area: Rect) {
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

        let list = List::new(items).block(block);
        f.render_widget(list, area);
    }

    fn render_settings_tab(&self, f: &mut Frame, area: Rect) {
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

        let list = List::new(items).block(block);
        f.render_widget(list, area);
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
        let status_text = format!(
            "{} │ {} │ Vol: {}% │ Out: {} │ {}",
            state, title, vol, active_dev_name, self.status_message
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
            Line::from("[Enter]          : Open folder / Add file to queue / Play track"),
            Line::from("[/] or [f]       : Skim Fuzzy Search across Library"),
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
