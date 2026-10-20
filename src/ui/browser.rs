//! The file browser behind tab 2's LIBRARY panel: entry labels, the directory scan and its
//! cache, the browser's metadata fetch, navigation, the spotlight and the browser's paging keys.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::widgets::*;

use super::tag_reads::TagRead;
use super::{is_subfolder_entry, AppUI, CursorTarget, DirScanEvent, PendingFolder, SPOTLIGHT_HINT};
use crate::metadata::is_supported_audio_file;

impl AppUI {
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

    /// How long a folder being opened keeps the previous one on screen while its scan runs:
    /// a folder listed within it is shown whole, a slower one with its first rows and READING
    /// DIRECTORY (issue #101).
    pub const FOLDER_OPEN_DEADLINE: Duration = Duration::from_millis(150);

    /// The navigation rows at the top of the browser list, before the folder's entries:
    /// `★ Music Homes` and `..` (no `..` at the file system's root); in Music Homes, `..` alone.
    pub fn browser_header_rows(&self) -> usize {
        if self.in_bookmarks_view {
            1
        } else {
            1 + usize::from(self.browser_dir.parent().is_some())
        }
    }

    /// The folder being opened while the previous one is still shown, if any: keys act on the
    /// shown one until it is swapped in (`poll_background_dir_scan`).
    pub fn opening_folder(&self) -> Option<&Path> {
        self.pending_folder.as_ref().map(|pending| pending.dir.as_path())
    }

    /// Sort the folder's entries (subfolders first, then by name, ignoring case) under the
    /// header rows, keeping the cursor on the entry it was on.
    fn sort_browser_entries(&mut self) {
        let header = self.browser_header_rows();
        if self.browser_entries.len() <= header {
            return;
        }
        let selected = (self.browser_cursor >= header)
            .then(|| self.browser_entries.get(self.browser_cursor).map(|(_, p, _)| p.clone()))
            .flatten();
        self.browser_entries[header..].sort_by(|a, b| match (a.2, b.2) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        });
        if let Some(path) = selected {
            if let Some(row) = self.browser_entry_row(&path) {
                self.browser_cursor = row;
            }
        }
    }

    /// The row of the folder's entry for `path` (never a header row).
    fn browser_entry_row(&self, path: &Path) -> Option<usize> {
        let header = self.browser_header_rows();
        self.browser_entries
            .iter()
            .skip(header)
            .position(|(_, p, _)| p == path)
            .map(|pos| header + pos)
    }

    /// The user moved the browser's cursor: a Backspace's restore no longer applies.
    pub(super) fn browser_cursor_moved(&mut self) {
        self.pending_parent_restore = None;
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
            self.sort_browser_entries();
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

    /// List the shown folder again (`r`, startup), or Music Homes. The cursor stays on the
    /// entry it is on.
    pub fn refresh_browser(&mut self) {
        if self.in_bookmarks_view {
            let refresh_start = Instant::now();
            self.stop_dir_scan();
            self.browser_loading = false;
            self.browser_load_start = None;
            let old_cursor = self.browser_cursor;
            self.browser_entries.clear();
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
        let dir = self.browser_dir.clone();
        self.open_folder(dir, CursorTarget::Selection);
    }

    /// Stop the running directory scan, if any, and drop a folder still being opened. What the
    /// scan already sent is dropped too, also when no scan follows (Music Homes, a cache hit):
    /// the poll ignores another scan's events.
    fn stop_dir_scan(&mut self) {
        if let Some(stop) = self.dir_scan_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.dir_scan_id += 1;
        }
        let (dir, start) = match self.pending_folder.take() {
            Some(pending) => (pending.dir, Some(pending.opened)),
            None if self.browser_loading => (self.browser_dir.clone(), self.browser_load_start),
            None => return,
        };
        if let Some(start) = start {
            crate::logger::log_raw(
                "PERF",
                "browser",
                &format!(
                    "previous directory scan cancelled for '{}' after {:.2?}",
                    dir.display(),
                    start.elapsed()
                ),
            );
        }
    }

    /// Open `dir` (another folder, or the shown one again). A complete cached listing is shown
    /// at once. Otherwise its scan starts on a worker and nothing here waits for it (issue
    /// #101): while a complete listing is on screen it stays there, and keeps the keys, until
    /// the scan completes or fails or `folder_open_deadline` passes (`poll_background_dir_scan`),
    /// so a fast folder never shows an empty READING DIRECTORY frame. With nothing to keep (at
    /// startup, or while the shown folder is itself still being read) `dir` is shown at once.
    fn open_folder(&mut self, dir: PathBuf, cursor: CursorTarget) {
        let refresh_start = Instant::now();
        self.stop_dir_scan();

        // Cache hit: instantaneous display with zero filesystem overhead!
        if self
            .browser_dir_cache
            .get(&dir)
            .is_some_and(|cached| cached.is_complete)
        {
            self.show_folder(dir, cursor, None);
            let unfetched = self
                .browser_dir_cache
                .get(&self.browser_dir)
                .map(|cached| cached.unfetched_tracks.clone())
                .unwrap_or_default();
            if !unfetched.is_empty() && self.settings_mgr.settings.metadata_view {
                self.start_background_metadata_fetch(unfetched);
            }
            crate::logger::log_raw(
                "PERF",
                "browser",
                &format!(
                    "directory loaded from cache ({} items) for '{}' -> elapsed: {:.2?}",
                    self.browser_entries.len().saturating_sub(self.browser_header_rows()),
                    self.browser_dir.display(),
                    refresh_start.elapsed()
                ),
            );
            return;
        }

        // Incomplete or uncached directory: clear any partial cache entry so fresh scan is clean
        self.browser_dir_cache.remove(&dir);
        self.spawn_dir_scan(dir.clone());
        if !self.browser_entries.is_empty() && !self.browser_loading {
            self.pending_folder = Some(PendingFolder {
                dir,
                cursor,
                opened: refresh_start,
            });
        } else {
            self.show_folder(dir, cursor, Some(refresh_start));
        }
    }

    /// Show `dir` with what `browser_dir_cache` holds of it (all of it, part, or nothing yet),
    /// as still being read when `load_start` is given. The cursor goes to `cursor`'s row; an
    /// entry not listed yet becomes `pending_parent_restore` while the scan runs.
    fn show_folder(&mut self, dir: PathBuf, cursor: CursorTarget, load_start: Option<Instant>) {
        let same = !self.in_bookmarks_view && self.browser_dir == dir;
        let (target, fallback) = match cursor {
            CursorTarget::Top => (None, 0),
            CursorTarget::Entry(path) => (Some(path), 0),
            CursorTarget::Selection => {
                let selected = (self.browser_cursor >= self.browser_header_rows())
                    .then(|| self.browser_entries.get(self.browser_cursor).map(|(_, p, _)| p.clone()))
                    .flatten();
                (selected, self.browser_cursor)
            }
        };

        self.in_bookmarks_view = false;
        self.browser_dir = dir;
        self.browser_loading = load_start.is_some();
        self.browser_load_start = load_start;
        self.browser_entries.clear();
        self.browser_entries
            .push(("★ Music Homes".to_string(), PathBuf::from("___MUSIC_HOMES___"), true));
        if let Some(parent) = self.browser_dir.parent() {
            self.browser_entries
                .push((".. [Parent Directory]".to_string(), parent.to_path_buf(), true));
        }
        if let Some(cached) = self.browser_dir_cache.get(&self.browser_dir) {
            self.browser_entries.extend(cached.dirs.iter().cloned());
            let metadata_view = self.settings_mgr.settings.metadata_view;
            for (_, path, is_dir) in &cached.files {
                let label = Self::format_file_label(path, metadata_view, &self.metadata_cache);
                self.browser_entries.push((label, path.clone(), *is_dir));
            }
        }
        self.browser_cursor = 0;
        self.sort_browser_entries();

        let row = target.as_deref().and_then(|path| self.browser_entry_row(path));
        self.browser_cursor = row.unwrap_or(fallback).min(self.browser_entries.len() - 1);
        self.pending_parent_restore = if row.is_none() && self.browser_loading {
            target
        } else {
            None
        };
        if !same {
            self.browser_state = ListState::default();
        }
        self.browser_state.select(Some(self.browser_cursor));
    }

    /// Start listing `dir` on a `dir-scan` worker; its batches come in through
    /// `poll_background_dir_scan`.
    fn spawn_dir_scan(&mut self, scan_dir: PathBuf) {
        self.dir_scan_id += 1;
        let scan_id = self.dir_scan_id;
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.dir_scan_stop = Some(stop_flag.clone());
        let tx = self.dir_scan_tx.clone();
        let metadata_view = self.settings_mgr.settings.metadata_view;
        let cached_map = self.metadata_cache.clone(); // shared handle, not a copy
        let read_dir = self.dir_reader.clone();

        self.workers.spawn("dir-scan", move || {
            let thread_start = std::time::Instant::now();
            match read_dir(&scan_dir) {
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
    }

    /// Show the folder being opened, with what its scan has listed so far.
    fn show_pending_folder(&mut self, still_reading: bool) {
        if let Some(pending) = self.pending_folder.take() {
            self.show_folder(pending.dir, pending.cursor, still_reading.then_some(pending.opened));
            // Its scan counts from when the folder was opened (logs, the slow-read status).
            if !still_reading {
                self.browser_load_start = Some(pending.opened);
            }
        }
    }

    fn handle_dir_scan_event(&mut self, event: DirScanEvent) {
        let (scan_id, dir) = match &event {
            DirScanEvent::Batch { scan_id, dir, .. }
            | DirScanEvent::Complete { scan_id, dir }
            | DirScanEvent::Error { scan_id, dir, .. } => (*scan_id, dir.clone()),
        };
        // The scan of the folder being opened, else of the shown folder.
        let opening = match &self.pending_folder {
            Some(pending) => pending.dir == dir,
            None if dir == self.browser_dir && !self.in_bookmarks_view => false,
            None => return,
        };
        if scan_id != self.dir_scan_id || (self.pending_folder.is_some() && !opening) {
            return;
        }
        match event {
            DirScanEvent::Batch {
                mut dirs,
                mut files,
                mut unfetched_tracks,
                ..
            } => {
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
                cached.unfetched_tracks.extend(unfetched_tracks);
                cached.is_complete = false;

                // A folder being opened is shown from the cache once its scan ends or the
                // deadline passes, never at its first batch.
                if opening {
                    return;
                }
                self.browser_entries.extend(dirs);
                self.browser_entries.extend(files);
                // Incremental sort so streamed entries are always alphabetical; the cursor
                // stays on its entry.
                self.sort_browser_entries();
                self.restore_browser_cursor();
            }
            DirScanEvent::Complete { .. } => {
                if let Some(cached) = self.browser_dir_cache.get_mut(&dir) {
                    cached.dirs.sort_by_key(|a| a.0.to_lowercase());
                    cached.files.sort_by_key(|a| a.0.to_lowercase());
                    cached.is_complete = true;
                }
                if opening {
                    self.show_pending_folder(false);
                } else {
                    self.sort_browser_entries();
                    self.restore_browser_cursor();
                }
                self.browser_loading = false;
                self.pending_parent_restore = None;
                self.dir_scan_stop = None;
                let load_duration = self.browser_load_start.take().map(|s| s.elapsed());

                let item_count = self.browser_entries.len().saturating_sub(self.browser_header_rows());
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

                if self.settings_mgr.settings.metadata_view {
                    if let Some(cached) = self.browser_dir_cache.get(&dir) {
                        if !cached.unfetched_tracks.is_empty() {
                            self.start_background_metadata_fetch(cached.unfetched_tracks.clone());
                        }
                    }
                }
            }
            DirScanEvent::Error { message, .. } => {
                if opening {
                    self.show_pending_folder(false);
                }
                self.browser_loading = false;
                self.pending_parent_restore = None;
                self.dir_scan_stop = None;
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

    /// Put the cursor on the folder `Backspace` left once the shown folder's scan lists it.
    fn restore_browser_cursor(&mut self) {
        let Some(target) = self.pending_parent_restore.as_deref() else {
            return;
        };
        if let Some(row) = self.browser_entry_row(target) {
            self.browser_cursor = row;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(row));
            self.pending_parent_restore = None;
        }
    }

    /// Take in what the directory scan has sent, without waiting, and show a folder being
    /// opened once its deadline has passed (with its rows so far and READING DIRECTORY).
    /// Returns whether anything came in or changed, so the event loop knows to draw.
    pub fn poll_background_dir_scan(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.dir_scan_rx.try_recv() {
            self.handle_dir_scan_event(event);
            changed = true;
        }
        if self
            .pending_folder
            .as_ref()
            .is_some_and(|pending| pending.opened.elapsed() >= self.folder_open_deadline)
        {
            self.show_pending_folder(true);
            changed = true;
        }

        if self.browser_loading {
            if let Some(start) = self.browser_load_start {
                if start.elapsed() > std::time::Duration::from_secs(3) {
                    let count = self.browser_entries.len().saturating_sub(self.browser_header_rows());
                    self.status_message = format!(
                        "Loaded {} items from '{}'... Esc or '..' to go back",
                        count,
                        self.browser_dir.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }
        changed
    }

    /// Esc while a folder is being opened: stop opening it and stay in the shown folder.
    /// Returns the folder that was being opened.
    pub(super) fn cancel_folder_open(&mut self) -> Option<PathBuf> {
        let dir = self.pending_folder.as_ref()?.dir.clone();
        self.stop_dir_scan();
        self.browser_dir_cache.remove(&dir);
        Some(dir)
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
        let tag_reads = self.tag_reads.clone();

        self.workers.spawn("browser-metadata", move || {
            let fetch_start = std::time::Instant::now();
            let mut extracted = 0;
            for path in paths {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // A file another worker is reading is skipped; that read refreshes its label.
                // A finished read is always published, even if the stop flag was set meanwhile:
                // a worker that skipped the file, or is waiting for it, relies on these tags.
                if tag_reads.read_unless_busy(&path) == TagRead::Read {
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

    pub fn navigate_browser(&mut self, path: PathBuf) {
        self.close_spotlight();

        if path.to_string_lossy() == "___MUSIC_HOMES___" {
            self.in_bookmarks_view = true;
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
            self.refresh_browser();
            self.update_web_state();
            return;
        }

        // To the parent (Backspace, `..`): the cursor goes back to the folder it left.
        let cursor = if !self.in_bookmarks_view && self.browser_dir.parent() == Some(path.as_path()) {
            CursorTarget::Entry(self.browser_dir.clone())
        } else {
            CursorTarget::Top
        };
        self.open_folder(path, cursor);
        self.update_web_state();
    }

    pub fn open_spotlight(&mut self) {
        self.browser_spotlight = Some(String::new());
        self.status_message = SPOTLIGHT_HINT.to_string();
    }

    /// Close the spotlight, and its hint in the status line if nothing has replaced it, so the
    /// hint does not hide keys on other tabs.
    pub fn close_spotlight(&mut self) {
        self.browser_spotlight = None;
        if self.status_message == SPOTLIGHT_HINT {
            self.status_message.clear();
        }
    }

    /// Indices of the subfolders whose name matches the spotlight query (prefix matches
    /// first). Only real subfolders (and, in Music Homes, the homes) take part: never the `..`
    /// row, which the LIBRARY panel draws as `PARENT` / `FILE SYSTEM`, nor the Music Homes row.
    pub fn spotlight_matches(&self) -> Vec<usize> {
        let query = match &self.browser_spotlight {
            Some(q) if !q.is_empty() => q.to_lowercase(),
            _ => return Vec::new(),
        };

        let mut prefix_matches = Vec::new();
        let mut substring_matches = Vec::new();

        for (idx, entry) in self.browser_entries.iter().enumerate() {
            if !is_subfolder_entry(entry) {
                continue;
            }
            let name = &entry.0;
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
            self.browser_cursor_moved();
        }
    }

    pub fn spotlight_next_match(&mut self) {
        let matches = self.spotlight_matches();
        if matches.is_empty() {
            return;
        }
        self.browser_cursor_moved();
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
        self.browser_cursor_moved();
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
        self.browser_cursor_moved();
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
        self.browser_cursor_moved();
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
            self.browser_cursor_moved();
            self.browser_cursor = 0;
            self.browser_state = ListState::default();
            self.browser_state.select(Some(0));
        }
    }

    pub fn end_browser(&mut self) {
        if !self.browser_entries.is_empty() {
            self.browser_cursor_moved();
            self.browser_cursor = self.browser_entries.len() - 1;
        }
    }
}
