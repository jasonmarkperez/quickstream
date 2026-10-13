//! In-App Recursive Fuzzy Search Modal State and Scoring Engine.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::widgets::ListState;

use crate::metadata::AudioMetadata;

/// Precomputed search indexing data for a single track.
#[derive(Clone)]
pub struct SearchItem {
    pub track_idx: usize,
    pub haystack: String,
    pub lower_haystack: String,
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
    pub spinner_frame: usize,
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
            spinner_frame: 0,
        };

        if !tracks.is_empty() {
            state.add_tracks(tracks);
        } else {
            state.apply_filter();
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
            let rel_path = track
                .filepath
                .strip_prefix(&self.root_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let haystack = format!(
                "{} {} {} {} {}",
                track.artist, track.album, track.title, track.filename, rel_path
            );
            let lower_haystack = haystack.to_lowercase();
            let sort_key = format!(
                "{} - {} - {}",
                track.artist.to_lowercase(),
                track.title.to_lowercase(),
                track.filename.to_lowercase()
            );

            self.search_items.push(SearchItem {
                track_idx: idx,
                haystack,
                lower_haystack,
                sort_key,
            });
            self.all_tracks.push(track);
        }

        self.apply_filter();
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

            if disconnected {
                self.is_scanning = false;
                self.scan_receiver = None;
            }
        }
        self.spinner_frame = (self.spinner_frame + 1) % 10;
    }

    pub fn spinner_char(&self) -> &'static str {
        const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        SPINNERS[self.spinner_frame % SPINNERS.len()]
    }

    pub fn close(&mut self) {
        if let Some(ref flag) = self.stop_flag {
            flag.store(true, Ordering::Relaxed);
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
            let tokens: Vec<String> = trimmed
                .split_whitespace()
                .map(|s| s.to_lowercase())
                .collect();

            let mut scored = Vec::new();

            for item in &self.search_items {
                // Nanosecond subsequence pre-filter: all tokens must be present in sequence
                let mut matches_all = true;
                for token in &tokens {
                    if !contains_subsequence(&item.lower_haystack, token) {
                        matches_all = false;
                        break;
                    }
                }
                if !matches_all {
                    continue;
                }

                // High-precision fuzzy scoring on surviving candidates
                let mut total_score = 0i64;
                for token in &tokens {
                    if let Some(score) = matcher.fuzzy_match(&item.haystack, token) {
                        total_score += score;
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
                b.0.cmp(&a.0).then_with(|| {
                    self.search_items[a.1].sort_key.cmp(&self.search_items[b.1].sort_key)
                })
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

    pub fn selected_track(&self) -> Option<&AudioMetadata> {
        self.filtered.get(self.selected_index).map(|(_, t)| t)
    }

    pub fn all_filtered_tracks(&self) -> Vec<AudioMetadata> {
        self.filtered.iter().map(|(_, t)| t.clone()).collect()
    }
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
