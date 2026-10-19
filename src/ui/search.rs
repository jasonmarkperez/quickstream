//! In-App Recursive Fuzzy Search Modal State and Field-Isolated Scoring Engine.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::widgets::ListState;

use crate::metadata::AudioMetadata;

/// Minimum time between filter passes triggered by streamed scan batches. Each pass scores and
/// sorts every indexed track, so re-running it for every batch makes typing lag on large libraries.
const SCAN_FILTER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Precomputed search indexing data for a single track with field boundary isolation.
#[derive(Clone)]
pub struct SearchItem {
    pub track_idx: usize,
    pub title: String,
    pub lower_title: String,
    pub artist: String,
    pub lower_artist: String,
    pub album: String,
    pub lower_album: String,
    pub filename: String,
    pub lower_filename: String,
    pub rel_path: String,
    pub lower_rel_path: String,
    pub path_components: Vec<String>,
    pub lower_path_components: Vec<String>,
    pub sort_key: String,
}

pub struct SearchModalState {
    pub root_dir: PathBuf,
    pub query: String,
    pub all_tracks: Vec<AudioMetadata>,
    pub search_items: Vec<SearchItem>,
    pub filtered: Vec<(i64, AudioMetadata)>,
    pub selected_index: usize,
    pub list_state: ListState,
    pub is_scanning: bool,
    pub scan_receiver: Option<Receiver<Vec<AudioMetadata>>>,
    pub stop_flag: Option<Arc<AtomicBool>>,
    pub scan_start: Option<std::time::Instant>,
    last_filter_at: Option<std::time::Instant>,
    filter_pending: bool,
}

impl SearchModalState {
    pub fn new(root_dir: PathBuf, tracks: Vec<AudioMetadata>) -> Self {
        Self::new_with_scanner(root_dir, tracks, None, None)
    }

    pub fn new_with_scanner(
        root_dir: PathBuf,
        tracks: Vec<AudioMetadata>,
        scan_receiver: Option<Receiver<Vec<AudioMetadata>>>,
        stop_flag: Option<Arc<AtomicBool>>,
    ) -> Self {
        let is_scanning = scan_receiver.is_some();
        let scan_start = if is_scanning {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let mut state = Self {
            root_dir,
            query: String::new(),
            all_tracks: Vec::new(),
            search_items: Vec::new(),
            filtered: Vec::new(),
            selected_index: 0,
            list_state: ListState::default(),
            is_scanning,
            scan_receiver,
            stop_flag,
            scan_start,
            last_filter_at: None,
            filter_pending: false,
        };

        if !tracks.is_empty() {
            state.add_tracks(tracks);
        } else {
            state.apply_filter();
            // Nothing is shown yet, so the first streamed batch should display immediately.
            state.last_filter_at = None;
        }

        state
    }

    /// Add newly discovered tracks incrementally and re-apply current query filter.
    pub fn add_tracks(&mut self, new_tracks: Vec<AudioMetadata>) {
        if new_tracks.is_empty() {
            return;
        }

        for track in new_tracks {
            let idx = self.all_tracks.len();
            let rel_path_buf = track
                .filepath
                .strip_prefix(&self.root_dir)
                .unwrap_or(&track.filepath)
                .to_path_buf();
            let rel_path = rel_path_buf.to_string_lossy().to_string();
            let lower_rel_path = rel_path.to_lowercase();

            let mut path_components = Vec::new();
            let mut lower_path_components = Vec::new();
            if let Some(parent) = rel_path_buf.parent() {
                for comp in parent.components() {
                    let s = comp.as_os_str().to_string_lossy().to_string();
                    if !s.is_empty() {
                        lower_path_components.push(s.to_lowercase());
                        path_components.push(s);
                    }
                }
            }

            let lower_title = track.title.to_lowercase();
            let lower_artist = track.artist.to_lowercase();
            let lower_album = track.album.to_lowercase();
            let lower_filename = track.filename.to_lowercase();

            let sort_key = format!("{} - {} - {}", lower_artist, lower_title, lower_filename);

            self.search_items.push(SearchItem {
                track_idx: idx,
                title: track.title.clone(),
                lower_title,
                artist: track.artist.clone(),
                lower_artist,
                album: track.album.clone(),
                lower_album,
                filename: track.filename.clone(),
                lower_filename,
                rel_path,
                lower_rel_path,
                path_components,
                lower_path_components,
                sort_key,
            });
            self.all_tracks.push(track);
        }

        let recently_filtered = self.last_filter_at.is_some_and(|t| t.elapsed() < SCAN_FILTER_INTERVAL);
        if self.is_scanning && recently_filtered {
            self.filter_pending = true;
        } else {
            self.apply_filter();
        }
    }

    /// Poll any background scanner batches and update progress.
    pub fn poll_background_scan(&mut self) {
        if let Some(ref rx) = self.scan_receiver {
            let mut batches = Vec::new();
            let mut disconnected = false;

            loop {
                match rx.try_recv() {
                    Ok(batch) => batches.extend(batch),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if !batches.is_empty() {
                self.add_tracks(batches);
            }

            let interval_passed = self.last_filter_at.is_none_or(|t| t.elapsed() >= SCAN_FILTER_INTERVAL);
            if self.filter_pending && (disconnected || interval_passed) {
                self.apply_filter();
            }

            if disconnected {
                self.is_scanning = false;
                self.scan_receiver = None;
                if let Some(start) = self.scan_start.take() {
                    crate::logger::log_raw(
                        "PERF",
                        "search",
                        &format!(
                            "library search scan complete ({} total tracks indexed) for '{}' -> elapsed: {:.2?}",
                            self.all_tracks.len(),
                            self.root_dir.display(),
                            start.elapsed()
                        ),
                    );
                }
            }
        }
    }

    pub fn close(&mut self) {
        if let Some(ref flag) = self.stop_flag {
            flag.store(true, Ordering::Relaxed);
        }
        if self.is_scanning {
            if let Some(start) = self.scan_start.take() {
                crate::logger::log_raw(
                    "PERF",
                    "search",
                    &format!(
                        "library search scan cancelled after {:.2?} (indexed {} tracks so far) for '{}'",
                        start.elapsed(),
                        self.all_tracks.len(),
                        self.root_dir.display()
                    ),
                );
            }
        }
        self.is_scanning = false;
        self.scan_receiver = None;
    }

    pub fn update_query(&mut self, new_query: String) {
        self.query = new_query;
        self.apply_filter();
    }

    pub fn append_char(&mut self, c: char) {
        self.query.push(c);
        self.apply_filter();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.apply_filter();
    }

    pub fn apply_filter(&mut self) {
        self.last_filter_at = Some(std::time::Instant::now());
        self.filter_pending = false;
        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            // Return all tracks sorted by artist / title / filename
            let mut indices: Vec<usize> = (0..self.all_tracks.len()).collect();
            indices.sort_by(|&a, &b| self.search_items[a].sort_key.cmp(&self.search_items[b].sort_key));

            self.filtered = indices
                .into_iter()
                .take(2000)
                .map(|idx| (0i64, self.all_tracks[idx].clone()))
                .collect();
        } else {
            let matcher = SkimMatcherV2::default();
            let prepared_tokens: Vec<(String, String, bool)> = trimmed
                .split_whitespace()
                .map(|s| (s.to_string(), s.to_lowercase(), s.contains('/')))
                .collect();

            let mut scored = Vec::new();

            for item in &self.search_items {
                let mut total_score = 0i64;
                let mut matches_all = true;

                for (token, lower_token, has_slash) in &prepared_tokens {
                    let score_title = score_field(&matcher, &item.title, &item.lower_title, token, lower_token);
                    let score_artist = score_field(&matcher, &item.artist, &item.lower_artist, token, lower_token);
                    let score_album = score_field(&matcher, &item.album, &item.lower_album, token, lower_token);
                    let score_filename =
                        score_field(&matcher, &item.filename, &item.lower_filename, token, lower_token);

                    let score_comp = item
                        .path_components
                        .iter()
                        .zip(&item.lower_path_components)
                        .filter_map(|(raw, lower)| score_field(&matcher, raw, lower, token, lower_token))
                        .max();

                    let score_rel = if *has_slash {
                        score_field(&matcher, &item.rel_path, &item.lower_rel_path, token, lower_token)
                    } else {
                        None
                    };

                    let best_token_score = [
                        score_title.map(|s| s + 30),  // boost title match
                        score_artist.map(|s| s + 20), // boost artist match
                        score_album.map(|s| s + 10),  // boost album match
                        score_filename,
                        score_comp,
                        score_rel,
                    ]
                    .into_iter()
                    .flatten()
                    .max();

                    if let Some(s) = best_token_score {
                        total_score += s;
                    } else {
                        matches_all = false;
                        break;
                    }
                }

                if matches_all {
                    scored.push((total_score, item.track_idx));
                }
            }

            // Sort by match score descending, then by precomputed sort_key
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| self.search_items[a.1].sort_key.cmp(&self.search_items[b.1].sort_key))
            });

            self.filtered = scored
                .into_iter()
                .take(1000)
                .map(|(score, idx)| (score, self.all_tracks[idx].clone()))
                .collect();
        }

        if self.filtered.is_empty() {
            self.selected_index = 0;
            self.list_state.select(None);
        } else {
            if self.selected_index >= self.filtered.len() {
                self.selected_index = 0;
            }
            self.list_state.select(Some(self.selected_index));
        }
    }

    pub fn move_selection_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected_index > 0 {
            self.selected_index -= 1;
        } else {
            self.selected_index = self.filtered.len() - 1;
        }
        self.list_state.select(Some(self.selected_index));
    }

    pub fn move_selection_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected_index + 1 < self.filtered.len() {
            self.selected_index += 1;
        } else {
            self.selected_index = 0;
        }
        self.list_state.select(Some(self.selected_index));
    }

    pub fn page_up(&mut self, step: usize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len();
        if self.selected_index >= step {
            self.selected_index -= step;
        } else if self.selected_index > 0 {
            self.selected_index = 0;
        } else {
            self.selected_index = len.saturating_sub(1);
        }
        self.list_state.select(Some(self.selected_index));
    }

    pub fn page_down(&mut self, step: usize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len();
        if self.selected_index + step < len {
            self.selected_index += step;
        } else if self.selected_index < len.saturating_sub(1) {
            self.selected_index = len.saturating_sub(1);
        } else {
            self.selected_index = 0;
        }
        self.list_state.select(Some(self.selected_index));
    }

    pub fn home(&mut self) {
        if !self.filtered.is_empty() {
            self.selected_index = 0;
            self.list_state.select(Some(0));
        }
    }

    pub fn end(&mut self) {
        if !self.filtered.is_empty() {
            self.selected_index = self.filtered.len().saturating_sub(1);
            self.list_state.select(Some(self.selected_index));
        }
    }

    pub fn selected_track(&self) -> Option<&AudioMetadata> {
        self.filtered.get(self.selected_index).map(|(_, t)| t)
    }

    pub fn all_filtered_tracks(&self) -> Vec<AudioMetadata> {
        self.filtered.iter().map(|(_, t)| t.clone()).collect()
    }
}

/// The characters to underline in a ranked match row: char indices into `title` and `artist` as
/// the row draws them (UPPERCASE). Each token of `query` is marked in the field it matches best,
/// with the boosts the ranking gives (a title match over an artist match); a token matched only
/// in the album, file name or path marks nothing. Cheap enough for the visible rows each frame.
pub fn row_match_indices(query: &str, title: &str, artist: &str) -> (Vec<usize>, Vec<usize>) {
    let matcher = SkimMatcherV2::default();
    let mut in_title = Vec::new();
    let mut in_artist = Vec::new();
    for token in query.split_whitespace() {
        let token = token.to_uppercase();
        let best = |text: &str, boost: i64| {
            if text.is_empty() {
                return None;
            }
            matcher
                .fuzzy_indices(text, &token)
                .map(|(score, indices)| (score + boost, indices))
        };
        match (best(title, 30), best(artist, 20)) {
            (Some((t, indices)), Some((a, _))) if t >= a => in_title.extend(indices),
            (Some((_, indices)), None) => in_title.extend(indices),
            (_, Some((_, indices))) => in_artist.extend(indices),
            (None, None) => {}
        }
    }
    (in_title, in_artist)
}

/// Score a single isolated metadata field against a token.
#[inline(always)]
fn score_field(
    matcher: &SkimMatcherV2,
    raw_field: &str,
    lower_field: &str,
    token: &str,
    lower_token: &str,
) -> Option<i64> {
    if raw_field.is_empty() {
        return None;
    }
    // Fast screening: characters of lower_token must appear in order in lower_field
    if !contains_subsequence(lower_field, lower_token) {
        return None;
    }
    matcher.fuzzy_match(raw_field, token)
}

/// Fast subsequence check: verifies that all characters of query appear in order in haystack.
#[inline(always)]
fn contains_subsequence(haystack: &str, query: &str) -> bool {
    let mut query_chars = query.chars();
    let mut target = match query_chars.next() {
        Some(c) => c,
        None => return true,
    };
    for hc in haystack.chars() {
        if hc == target {
            match query_chars.next() {
                Some(c) => target = c,
                None => return true,
            }
        }
    }
    false
}
