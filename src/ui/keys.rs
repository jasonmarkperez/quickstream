//! Key routing: `normalize_key`, `handle_input_key` (modals and typed text, then the global
//! hotkeys) and the tab and pane keys (`handle_tab_key_with_modifiers`), and opening the fuzzy
//! search and the startup home editor.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::*;

use super::{search, AppUI};
use crate::settings::{setting, SettingsContext, SETTING_DISPLAY_ORDER};

/// A character key as every handler matches it. Terminals report an uppercase letter or a
/// shifted symbol with SHIFT (Shift held, Caps Lock on, a CSI-u or modifyOtherKeys terminal), and
/// some report Shift+N as `n` with SHIFT. The character carries the Shift, so it is dropped: a
/// plain key is its character with no modifiers (`N`, `?`, `+`). A Ctrl / Alt chord of a letter
/// is the lowercase letter with only its chord modifiers, as terminals that can't tell
/// Ctrl+Shift+C from Ctrl+C send it. Other keys keep their modifiers.
pub fn normalize_key(mut key: KeyEvent) -> KeyEvent {
    let KeyCode::Char(c) = key.code else {
        return key;
    };
    let chord = !key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
    if chord {
        key.code = KeyCode::Char(one_char(c.to_lowercase(), c));
    } else if key.modifiers.contains(KeyModifiers::SHIFT) {
        key.code = KeyCode::Char(one_char(c.to_uppercase(), c));
    }
    key.modifiers.remove(KeyModifiers::SHIFT);
    key
}

/// `c`'s case mapping when it is one character, else `c` itself (`ß` stays `ß`).
fn one_char(mut mapped: impl Iterator<Item = char>, c: char) -> char {
    match (mapped.next(), mapped.next()) {
        (Some(m), None) => m,
        _ => c,
    }
}

impl AppUI {
    /// Every key the terminal sends comes here (`run`), normalised first (`normalize_key`), so
    /// each handler below matches a character with `NONE` (or its chord, e.g. `CONTROL`) only.
    /// Returns true when the app should quit.
    pub fn handle_input_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        let key = normalize_key(key);

        // Help, search and spotlight take every key before a pending seek or skip is applied.
        if self.show_help_modal {
            if (key.code, key.modifiers) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
                self.player.stop()?;
                return Ok(true);
            }
            if matches!(
                (key.code, key.modifiers),
                (KeyCode::Esc, _) | (KeyCode::Char('q' | '?'), KeyModifiers::NONE)
            ) {
                self.show_help_modal = false;
            }
            return Ok(false);
        }
        if self.search_modal.is_some() {
            return self.handle_search_key(key);
        }
        if self.browser_spotlight.is_some() {
            return self.handle_spotlight_key(key);
        }

        // If user presses any non-seek key, immediately flush any pending debounced seek
        let is_seek_key = matches!(
            (key.code, key.modifiers),
            (KeyCode::Right, KeyModifiers::SHIFT)
                | (KeyCode::Right, KeyModifiers::CONTROL)
                | (KeyCode::Right, KeyModifiers::ALT)
                | (KeyCode::Char(']'), KeyModifiers::NONE)
                | (KeyCode::Char('>'), KeyModifiers::NONE)
                | (KeyCode::Left, KeyModifiers::SHIFT)
                | (KeyCode::Left, KeyModifiers::CONTROL)
                | (KeyCode::Left, KeyModifiers::ALT)
                | (KeyCode::Char('['), KeyModifiers::NONE)
                | (KeyCode::Char('<'), KeyModifiers::NONE)
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

        // If mix picker modal is open, intercept input
        if self.show_mix_picker_modal {
            if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                self.player.stop()?;
                return Ok(true);
            }
            // One row per profile the picker lists; digits 1 to that count pick a profile.
            let count = crate::analysis::MixProfile::picker_profiles().len();
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('q' | 'M'), KeyModifiers::NONE) => {
                    self.show_mix_picker_modal = false;
                    return Ok(false);
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    self.mix_picker_cursor = (self.mix_picker_cursor + count - 1) % count;
                    return Ok(false);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    self.mix_picker_cursor = (self.mix_picker_cursor + 1) % count;
                    return Ok(false);
                }
                (KeyCode::Char(c), KeyModifiers::NONE) => {
                    match c.to_digit(10).map(|d| d as usize) {
                        Some(d) if (1..=count).contains(&d) => self.apply_mix_picker_selection(d - 1),
                        _ => {}
                    }
                    return Ok(false);
                }
                (KeyCode::Enter, _) => {
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
                            if let Some(home) = self.queue_tab.home.as_deref() {
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
                KeyCode::Char(c) if key.modifiers.is_empty() => {
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
                if key.code == KeyCode::Esc {
                    // A folder still being opened (the previous one is on screen): stay there.
                    if let Some(dir) = self.cancel_folder_open() {
                        self.status_message = format!(
                            "Stopped opening '{}'",
                            dir.file_name().unwrap_or(dir.as_os_str()).to_string_lossy()
                        );
                        return Ok(false);
                    }
                    // An Autopilot start still checking its folder (a slow share): stop it.
                    if let Some(dir) = self.cancel_autopilot_start() {
                        self.status_message = format!(
                            "Stopped starting Autopilot in '{}'",
                            dir.file_name().unwrap_or(dir.as_os_str()).to_string_lossy()
                        );
                        return Ok(false);
                    }
                }
                if key.code == KeyCode::Esc && self.browser_loading {
                    if let Some(stop) = self.dir_scan_stop.take() {
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // Drop what the scan already sent: the poll ignores another scan's events.
                    self.dir_scan_id += 1;
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
            (KeyCode::Char('n' | 'N'), KeyModifiers::NONE) => {
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
            | (KeyCode::Char('>'), KeyModifiers::NONE) => {
                let step = self.settings_mgr.settings.seek_step_seconds;
                self.accumulate_seek(step);
                self.update_web_state();
            }
            (KeyCode::Left, KeyModifiers::SHIFT)
            | (KeyCode::Left, KeyModifiers::CONTROL)
            | (KeyCode::Left, KeyModifiers::ALT)
            | (KeyCode::Char('['), KeyModifiers::NONE)
            | (KeyCode::Char('<'), KeyModifiers::NONE) => {
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
                    self.settings_mgr
                        .cycle_setting(setting::METADATA_VIEW, true, &settings_context!(self));
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
                } else if self.active_tab == 3 && self.settings_cursor == setting::HOME_DIR {
                    self.open_home_dir_editor();
                }
            }
            (KeyCode::Char('f'), KeyModifiers::NONE) => {
                if self.active_tab == 0 || self.active_tab == 1 {
                    self.open_fuzzy_search();
                }
            }
            (KeyCode::Char('z' | 'Z'), KeyModifiers::NONE) => {
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
            (KeyCode::Char('P'), KeyModifiers::NONE) => {
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
            (KeyCode::Char('M'), KeyModifiers::NONE) => {
                self.show_mix_picker_modal = !self.show_mix_picker_modal;
                self.mix_picker_cursor = self.active_mix_profile.picker_index();
                self.mix_picker_list = ListState::default();
            }
            (KeyCode::Char('L'), KeyModifiers::NONE) => {
                self.open_add_picker();
            }
            _ => {
                // Tab specific key handling
                self.handle_tab_key_with_modifiers(key.code, key.modifiers);
            }
        }
        Ok(false)
    }

    /// Keys while the fuzzy search is open: letters (`q` included) are query text. Returns true
    /// when the app should quit.
    fn handle_search_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        let Some(ref mut modal) = self.search_modal else {
            return Ok(false);
        };
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                if let Some(mut m) = self.search_modal.take() {
                    m.close();
                }
                self.player.stop()?;
                return Ok(true);
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
            (KeyCode::Char(c), KeyModifiers::NONE) => {
                modal.append_char(c);
            }
            _ => {}
        }
        Ok(false)
    }

    /// Keys while the library spotlight is open: characters are its query. Returns true when the
    /// app should quit.
    fn handle_spotlight_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.close_spotlight();
                self.player.stop()?;
                return Ok(true);
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
            (KeyCode::Char(c), KeyModifiers::NONE) => {
                if let Some(ref mut q) = self.browser_spotlight {
                    q.push(c);
                }
                self.jump_spotlight_to_first_match();
            }
            _ => {}
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

    /// Open the startup home editor on tab 4 (`Enter`, `e` and `/` all come here), holding the
    /// current startup home, or nothing when none is set. The key that opened it is not text.
    fn open_home_dir_editor(&mut self) {
        self.editing_home_dir = Some(self.settings_mgr.settings.home_dir.clone().unwrap_or_default());
        self.status_message = "Editing Startup Home Directory (Enter: Save, Esc: Cancel)".to_string();
    }

    pub fn handle_tab_key(&mut self, code: KeyCode) {
        self.handle_tab_key_with_modifiers(code, KeyModifiers::NONE);
    }

    /// The active tab's and pane's keys, for a key `handle_input_key` has normalised
    /// (`normalize_key`): an uppercase letter comes with `NONE`, not `SHIFT`.
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
                            self.browser_cursor_moved();
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
                            self.browser_cursor_moved();
                        }
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Char('u'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('b'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('K'), KeyModifiers::NONE)
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
                    | (KeyCode::Char('J'), KeyModifiers::NONE)
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
                    (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::NONE) => {
                        if self.queue_focus {
                            self.end_queue();
                        } else {
                            self.end_browser();
                        }
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if self.queue_focus {
                            self.supersede_playlist_queue();
                            self.queue.set_current_index(self.queue_cursor);
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
                            self.navigate_browser(self.browser_dir.clone());
                        } else if let Some(parent) = self.browser_dir.parent() {
                            let parent_path = parent.to_path_buf();
                            self.navigate_browser(parent_path);
                        }
                    }
                    (KeyCode::Char('a'), KeyModifiers::NONE) if !self.in_bookmarks_view && self.browser_loading => {
                        // Part of the folder is listed: adding that would leave the rest out.
                        self.status_message = format!(
                            "Still reading {}, try again when it's listed",
                            self.browser_dir
                                .file_name()
                                .unwrap_or(self.browser_dir.as_os_str())
                                .to_string_lossy()
                        );
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
                            let seed = self.queue.tracks()[self.queue_cursor].filepath.clone();
                            self.start_seed_radio(seed);
                        } else if let Some(current) = self.queue.current_track() {
                            self.start_seed_radio(current.filepath.clone());
                        } else {
                            self.status_message = "No track in queue to start Seed Radio".to_string();
                        }
                    }
                    (KeyCode::Char('H'), KeyModifiers::NONE) => {
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
                    (KeyCode::Char('~'), KeyModifiers::NONE) => {
                        let home = if let Some(ref h) = self.settings_mgr.settings.home_dir {
                            PathBuf::from(h)
                        } else if let Some(base) = directories::BaseDirs::new() {
                            base.home_dir().to_path_buf()
                        } else {
                            std::env::current_dir().unwrap_or_default()
                        };
                        self.jump_to_home(home);
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
                    | (KeyCode::Char('K'), KeyModifiers::NONE)
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
                    | (KeyCode::Char('J'), KeyModifiers::NONE)
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
                    (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::NONE) => {
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
                    // The cursor holds a setting index and moves in the order the tab lists
                    // the settings (grouped), wrapping at either end.
                    (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        self.settings_cursor = crate::settings::step_in_display_order(self.settings_cursor, false);
                    }
                    (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        self.settings_cursor = crate::settings::step_in_display_order(self.settings_cursor, true);
                    }
                    (KeyCode::PageUp, _)
                    | (KeyCode::Home, _)
                    | (KeyCode::Char('g'), KeyModifiers::NONE)
                    | (KeyCode::Char('K'), KeyModifiers::NONE)
                    | (KeyCode::Up, KeyModifiers::SHIFT) => {
                        self.settings_cursor = SETTING_DISPLAY_ORDER[0];
                    }
                    (KeyCode::PageDown, _)
                    | (KeyCode::End, _)
                    | (KeyCode::Char('G'), KeyModifiers::NONE)
                    | (KeyCode::Char('J'), KeyModifiers::NONE)
                    | (KeyCode::Down, KeyModifiers::SHIFT) => {
                        self.settings_cursor = SETTING_DISPLAY_ORDER[SETTING_DISPLAY_ORDER.len() - 1];
                    }
                    (KeyCode::Char('e'), KeyModifiers::NONE) if self.settings_cursor == setting::HOME_DIR => {
                        self.open_home_dir_editor();
                    }
                    (KeyCode::Char('c'), KeyModifiers::NONE) | (KeyCode::Delete, _)
                        if self.settings_cursor == setting::HOME_DIR =>
                    {
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
                        if self.settings_cursor == setting::HOME_DIR {
                            self.open_home_dir_editor();
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
}
