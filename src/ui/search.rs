//! In-App Recursive Fuzzy Search Modal State and Scoring Engine.

use std::path::PathBuf;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::widgets::ListState;

use crate::metadata::AudioMetadata;

pub struct SearchModalState {
    pub root_dir: PathBuf,
    pub query: String,
    pub all_tracks: Vec<AudioMetadata>,
    pub filtered: Vec<(i64, AudioMetadata)>,
    pub selected_index: usize,
    pub list_state: ListState,
}

impl SearchModalState {
    pub fn new(root_dir: PathBuf, tracks: Vec<AudioMetadata>) -> Self {
        let mut state = Self {
            root_dir,
            query: String::new(),
            all_tracks: tracks,
            filtered: Vec::new(),
            selected_index: 0,
            list_state: ListState::default(),
        };
        state.apply_filter();
        state
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
            // Return all tracks sorted by artist / title
            let mut all = self.all_tracks.clone();
            all.sort_by(|a, b| {
                a.artist
                    .to_lowercase()
                    .cmp(&b.artist.to_lowercase())
                    .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                    .then_with(|| a.filename.to_lowercase().cmp(&b.filename.to_lowercase()))
            });
            self.filtered = all.into_iter().map(|t| (0i64, t)).collect();
        } else {
            let matcher = SkimMatcherV2::default();
            let mut scored = Vec::new();

            for track in &self.all_tracks {
                // Construct multi-field haystack: artist, album, title, filename, relative path
                let rel_path = track
                    .filepath
                    .strip_prefix(&self.root_dir)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                let haystack = format!(
                    "{} {} {} {} {}",
                    track.artist, track.album, track.title, track.filename, rel_path
                );

                if let Some(score) = matcher.fuzzy_match(&haystack, trimmed) {
                    scored.push((score, track.clone()));
                }
            }

            // Sort by match score descending, then artist / title
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0).then_with(|| {
                    a.1.artist
                        .to_lowercase()
                        .cmp(&b.1.artist.to_lowercase())
                        .then_with(|| a.1.title.to_lowercase().cmp(&b.1.title.to_lowercase()))
                })
            });

            self.filtered = scored;
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
