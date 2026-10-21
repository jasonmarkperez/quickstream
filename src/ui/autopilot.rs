//! Autopilot and Seed Radio: the background scan's results, starting, reshuffling and stopping a
//! mix, the mix picker's choice, and resetting the playback session.

use std::path::{Path, PathBuf};

use super::{AppUI, AutopilotRunMode};

impl AppUI {
    pub fn poll_background_autopilot(&mut self) {
        self.poll_autopilot_folder();

        let mut new_tracks = Vec::new();
        let mut candidates = Vec::new();
        let mut finished = false;

        if let Some(ref rx) = self.autopilot_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    crate::library::AutopilotEvent::Track(track) => {
                        new_tracks.push(track);
                    }
                    crate::library::AutopilotEvent::Candidates(paths) => {
                        candidates.extend(paths);
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

        // The mix's candidate pool waits behind the queued tracks for analysis (issue #30), so
        // the next BPM Flow or mood mix has more analysed tracks to choose from.
        if !candidates.is_empty() {
            self.analysis_worker.enqueue_candidates(candidates);
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
            self.queue.set_current_index(0);
            self.queue_cursor = 0;

            let _ = self.ensure_web_server_started();
            self.update_web_state();

            let is_dlna = self.device_manager.get_active_device().is_some();
            let gapless = self.settings_mgr.settings.gapless_autopilot;

            if is_dlna && gapless {
                self.end_continuous_stream();
                if let Ok(mut s) = self.web_state.write() {
                    s.is_autopilot = true;
                    s.autopilot_dir = self.autopilot_dir.as_ref().map(|d| d.to_string_lossy().to_string());
                    s.is_playing = true;
                    s.is_paused = false;
                }
                // The stream follows the queue itself: no single file is the player's.
                self.player_track = None;
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

    /// Take in the folder a start's worker chose: the fallback, when the preferred one is gone.
    fn poll_autopilot_folder(&mut self) {
        match self.autopilot_folder_rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(dir)) => {
                self.autopilot_folder_rx = None;
                self.autopilot_settled_dir = None;
                self.take_autopilot_folder(dir);
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => self.autopilot_folder_rx = None,
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    /// The folder the last mix scans: `autopilot_dir`, or while a start still checks the folder
    /// it named there, the one named before it.
    fn last_autopilot_dir(&self) -> Option<PathBuf> {
        if self.autopilot_folder_rx.is_some() {
            self.autopilot_settled_dir.clone()
        } else {
            self.autopilot_dir.clone()
        }
    }

    /// The scan's worker chose `dir` over the folder the start named: name it from now on.
    fn take_autopilot_folder(&mut self, dir: PathBuf) {
        if self.autopilot_dir.as_ref() == Some(&dir) {
            return;
        }
        let named = self
            .autopilot_dir
            .as_deref()
            .map(|old| Self::autopilot_start_status(self.autopilot_run_mode, old));
        if named.as_ref() == Some(&self.status_message) {
            self.status_message = Self::autopilot_start_status(self.autopilot_run_mode, &dir);
        }
        self.autopilot_dir = Some(dir);
        self.update_web_state();
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

    /// `mode`, or `Fresh` when the queue is empty.
    fn run_mode_or(&self, mode: AutopilotRunMode) -> AutopilotRunMode {
        if self.queue.is_empty() {
            AutopilotRunMode::Fresh
        } else {
            mode
        }
    }

    pub fn start_autopilot(&mut self, dir: PathBuf) {
        let mode = self.run_mode_or(AutopilotRunMode::Append);
        self.start_autopilot_scan(None, dir, mode);
    }

    pub fn reshuffle_autopilot(&mut self, dir: PathBuf) {
        let mode = self.run_mode_or(AutopilotRunMode::Reshuffle);
        self.start_autopilot_scan(None, dir, mode);
    }

    /// Stop any autopilot scan: signal its worker and drop its channels.
    fn stop_autopilot_scan(&mut self) {
        if let Some(stop) = self.autopilot_scan_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.autopilot_rx = None;
        self.autopilot_folder_rx = None;
        self.autopilot_settled_dir = None;
        self.is_autopilot_scanning = false;
    }

    /// Esc while a start's worker still checks its folder (a slow share): stop the start and
    /// name the folder named before it again. Returns the folder the stopped start named.
    pub(super) fn cancel_autopilot_start(&mut self) -> Option<PathBuf> {
        self.autopilot_folder_rx.as_ref()?;
        let named = std::mem::replace(&mut self.autopilot_dir, self.autopilot_settled_dir.take());
        self.stop_autopilot_scan();
        self.update_web_state();
        named
    }

    /// The status line a start shows (`mode`, in `dir`) until its first tracks come in.
    fn autopilot_start_status(mode: AutopilotRunMode, dir: &Path) -> String {
        let dir_display = dir.file_name().unwrap_or_default().to_string_lossy();
        match mode {
            AutopilotRunMode::Fresh => format!("⚡ Autopilot: Discovering tracks from '{}'...", dir_display),
            AutopilotRunMode::Append => format!("⚡ Autopilot: Appending tracks from '{}'...", dir_display),
            AutopilotRunMode::Reshuffle => format!(
                "🎲 Autopilot: Reshuffling mix from next track forward (from '{}')...",
                dir_display
            ),
        }
    }

    /// End any continuous DLNA stream and rewind its track index and position, so the next
    /// continuous session starts from the top of the queue.
    fn end_continuous_stream(&self) {
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
            *ws.stream_state().continuous.current_track_index.lock().unwrap() = 0;
            *ws.stream_state().continuous.current_position.lock().unwrap() = 0.0;
        }
    }

    /// Stop playback and empty the queue.
    fn stop_and_clear_queue(&mut self) {
        let _ = self.player.stop();
        self.queue.clear();
        self.queue_cursor = 0;
    }

    /// End the whole playback session before something else replaces the queue: stop any
    /// autopilot scan, end any continuous DLNA stream, stop playback and empty the queue.
    pub fn reset_playback_session(&mut self) {
        self.stop_autopilot_scan();
        // The queue is no longer an autopilot mix (web dashboard badge).
        self.autopilot_dir = None;
        self.end_continuous_stream();
        self.stop_and_clear_queue();
    }

    /// Start a mix (`mode`, with the active mix profile) in `preferred` if it is there, else in
    /// `fallback`; with no `preferred`, in `fallback`. It starts at once: a running scan stops,
    /// a fresh mix clears the queue, and the status names `preferred`. Whether `preferred` is
    /// there is asked on the scan's worker (issue #103), which reports the folder it scans;
    /// `poll_background_autopilot` names the fallback if it took it.
    fn start_autopilot_scan(&mut self, preferred: Option<PathBuf>, fallback: PathBuf, mode: AutopilotRunMode) {
        self.supersede_playlist_queue();
        // What `autopilot_dir` names until the worker answers, not a folder still unchecked.
        let settled = self.last_autopilot_dir();
        // 1. Cancel any active autopilot scanner (its channel is replaced below)
        self.stop_autopilot_scan();
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.autopilot_scan_stop = Some(stop_flag.clone());

        let dir = preferred.clone().unwrap_or_else(|| fallback.clone());
        self.autopilot_dir = Some(dir.clone());
        self.is_autopilot_scanning = true;
        self.autopilot_run_mode = mode;
        self.autopilot_added_count = 0;

        match mode {
            // Clear current queue and stop previous playback
            AutopilotRunMode::Fresh => self.stop_and_clear_queue(),
            // Keep current queue and playback uninterrupted
            AutopilotRunMode::Append => {}
            AutopilotRunMode::Reshuffle => {
                // Keep tracks 0..=current_index uninterrupted; truncate everything after current_index
                self.queue.truncate_after(self.queue.current_index());
                self.clamp_queue_cursor();
            }
        }
        self.status_message = Self::autopilot_start_status(mode, &dir);

        // 3. Reuse any subdirectories already discovered by the browser if viewing the same
        // folder, once it is listed whole: a partial list would leave the rest of it out.
        let known_subdirs: Vec<PathBuf> = if self.browser_dir == dir && !self.browser_loading {
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

        let folder_tx = preferred.is_some().then(|| {
            let (folder_tx, folder_rx) = std::sync::mpsc::channel();
            self.autopilot_folder_rx = Some(folder_rx);
            self.autopilot_settled_dir = settled;
            folder_tx
        });

        // 4. Spawn background worker to discover tracks and stream them progressively
        let profile = self.active_mix_profile.clone();
        let db = self.analysis_db.clone();
        let checks = self.folder_checks.clone();
        // The library index walks the folder a mix starts in next, so the next mix there finds
        // it indexed (#41).
        let index_hint = self.library_index_hint();
        self.workers.spawn("autopilot", move || {
            let scan_dir = match (preferred, folder_tx) {
                (Some(preferred), Some(folder_tx)) => {
                    let Some(exists) = checks.exists(&preferred, &stop_flag) else {
                        return;
                    };
                    let chosen = if exists { preferred } else { fallback };
                    // A stopped start's channel is gone.
                    if folder_tx.send(chosen.clone()).is_err() {
                        return;
                    }
                    chosen
                }
                _ => fallback,
            };
            if let Some((control, roots)) = &index_hint {
                if roots.iter().any(|root| scan_dir.starts_with(root)) {
                    control.prioritise(scan_dir.clone());
                }
            }
            // The browser's listing is of the folder the start named.
            let known_subdirs = if scan_dir == dir { known_subdirs } else { Vec::new() };
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
        let profiles = crate::analysis::MixProfile::picker_profiles();
        self.active_mix_profile = profiles.get(index).unwrap_or(&profiles[0]).clone();
        let profile_name = self.active_mix_profile.name();
        // The browsed folder if it is still there (checked on the scan's worker), else the last
        // mix's folder (a start still checking its folder isn't one yet), else the current
        // directory.
        self.poll_autopilot_folder();
        let preferred = (!self.in_bookmarks_view).then(|| self.browser_dir.clone());
        let fallback = self
            .last_autopilot_dir()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        self.status_message = format!("⚡ Mix Profile: '{}' - Starting Autopilot...", profile_name);
        let mode = self.run_mode_or(AutopilotRunMode::Append);
        self.start_autopilot_scan(preferred, fallback, mode);
    }

    pub fn start_seed_radio(&mut self, seed_path: PathBuf) {
        // The browsed folder if it is still there (checked on the scan's worker), else the
        // seed's.
        let preferred = (!self.in_bookmarks_view).then(|| self.browser_dir.clone());
        let fallback = seed_path.parent().unwrap_or(&self.browser_dir).to_path_buf();
        self.seed_radio_scan(seed_path, preferred, fallback);
    }

    /// Seed Radio from the web dashboard: `preferred` (the terminal's folder) and `fallback` (the
    /// seed's) are the web thread's, both inside the allowed folders (issue #126). The scan's
    /// worker checks `preferred` is still there, as for `r`.
    pub fn start_seed_radio_in(&mut self, seed_path: PathBuf, preferred: Option<PathBuf>, fallback: PathBuf) {
        self.seed_radio_scan(seed_path, preferred, fallback);
    }

    /// The folder `p` / `P` start Autopilot in: the browsed one, or in Music Homes the
    /// highlighted home (the browsed one on the `..` row).
    pub(super) fn autopilot_key_dir(&self) -> PathBuf {
        if self.in_bookmarks_view && self.browser_cursor > 0 {
            if let Some((_, home, _)) = self.browser_entries.get(self.browser_cursor) {
                return home.clone();
            }
        }
        self.browser_dir.clone()
    }

    fn seed_radio_scan(&mut self, seed_path: PathBuf, preferred: Option<PathBuf>, fallback: PathBuf) {
        self.active_mix_profile = crate::analysis::MixProfile::SeedRadio(seed_path.clone());
        let track_name = seed_path.file_stem().unwrap_or_default().to_string_lossy();
        self.status_message = format!("📻 Seed Radio: Initiating station from '{}'...", track_name);
        let mode = self.run_mode_or(AutopilotRunMode::Append);
        self.start_autopilot_scan(preferred, fallback, mode);
    }

    pub fn get_selected_track_path(&self) -> Option<PathBuf> {
        if self.active_tab == 1 {
            if self.queue_focus {
                if self.queue_cursor < self.queue.len() {
                    return Some(self.queue.tracks()[self.queue_cursor].filepath.clone());
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
}
