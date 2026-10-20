//! The file browser behind tab 2's LIBRARY panel: entry labels, the directory scan and its
//! cache, the browser's metadata fetch, navigation, the spotlight and the browser's paging keys.

use std::path::{Path, PathBuf};

use ratatui::widgets::*;

use super::tag_reads::TagRead;
use super::{is_subfolder_entry, AppUI, DirScanEvent, SPOTLIGHT_HINT};
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
            // Drop what the stopped scan already sent, also when no scan follows (Music Homes,
            // a cache hit): the poll ignores another scan's events.
            self.dir_scan_id += 1;
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
        // Nothing waits for the scan here (issue #101): its batches come in through
        // `poll_background_dir_scan`, which the event loop calls at frame rate while the folder
        // has just been opened (`key_poll_timeout`). The loop keeps the previous frame until the
        // first batch or `FOLDER_FIRST_FRAME_HOLD` (`holds_frame`), then the LIBRARY panel shows
        // its READING DIRECTORY row until the batches come. A Backspace's `pending_parent_restore` waits with
        // them and puts the cursor back on the folder it left once that folder is listed.
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
}
