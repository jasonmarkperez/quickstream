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
    /// The ranked list's query words (UPPERCASE, as the rows draw their text).
    ranked_tokens: Vec<String>,
    /// Per `filtered` row, the query words the ranking matched in the title and in the artist
    /// (bit `i` for word `i`; words past the 64th are never marked).
    row_fields: Vec<(u64, u64)>,
    /// Whether a `filtered` track is an hour or longer: the length column is then 7 wide.
    long_lengths: bool,
    /// Per `filtered` row, its underlined characters once a frame has drawn it.
    row_marks: Vec<Option<RowMarks>>,
    /// Scans and match work done for drawing the list ([`Self::view_work`]).
    view_work: u64,
}

/// The characters a ranked row underlines: char indices into its UPPERCASE title and artist.
pub type RowMarks = (Vec<usize>, Vec<usize>);

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
            ranked_tokens: Vec::new(),
            row_fields: Vec::new(),
            long_lengths: false,
            row_marks: Vec::new(),
            view_work: 0,
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
        self.ranked_tokens = trimmed.split_whitespace().map(str::to_uppercase).collect();
        if trimmed.is_empty() {
            // Return all tracks sorted by artist / title / filename
            let mut indices: Vec<usize> = (0..self.all_tracks.len()).collect();
            indices.sort_by(|&a, &b| self.search_items[a].sort_key.cmp(&self.search_items[b].sort_key));

            self.filtered = indices
                .into_iter()
                .take(2000)
                .map(|idx| (0i64, self.all_tracks[idx].clone()))
                .collect();
            self.row_fields = vec![(0, 0); self.filtered.len()];
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
                // The words this item ranked on in its title and in its artist.
                let mut in_title = 0u64;
                let mut in_artist = 0u64;

                for (word, (token, lower_token, has_slash)) in prepared_tokens.iter().enumerate() {
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

                    let title_score = score_title.map(|s| s + 30); // boost title match
                    let artist_score = score_artist.map(|s| s + 20); // boost artist match
                    let best_token_score = [
                        title_score,
                        artist_score,
                        score_album.map(|s| s + 10), // boost album match
                        score_filename,
                        score_comp,
                        score_rel,
                    ]
                    .into_iter()
                    .flatten()
                    .max();

                    if let Some(s) = best_token_score {
                        total_score += s;
                        // Marked where it ranked (the title on a tie with the artist); a word
                        // that ranked on the album, file name or a folder marks nothing.
                        let bit = 1u64.checked_shl(word as u32).unwrap_or(0);
                        if title_score == Some(s) {
                            in_title |= bit;
                        } else if artist_score == Some(s) {
                            in_artist |= bit;
                        }
                    } else {
                        matches_all = false;
                        break;
                    }
                }

                if matches_all {
                    scored.push((total_score, item.track_idx, in_title, in_artist));
                }
            }

            // Sort by match score descending, then by precomputed sort_key
            scored.sort_by(|a, b| {
                b.0.cmp(&a.0)
                    .then_with(|| self.search_items[a.1].sort_key.cmp(&self.search_items[b.1].sort_key))
            });

            scored.truncate(1000);
            self.row_fields = scored.iter().map(|&(_, _, t, a)| (t, a)).collect();
            self.filtered = scored
                .into_iter()
                .map(|(score, idx, _, _)| (score, self.all_tracks[idx].clone()))
                .collect();
        }

        // What the rows need that depends on the whole list, worked out once per list.
        self.long_lengths = self.filtered.iter().any(|(_, t)| t.duration_seconds.round() >= 3600.0);
        self.row_marks = vec![None; self.filtered.len()];
        self.view_work += 1;

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

    /// Whether the ranked list has a track an hour or longer (its length column is 7 wide, not
    /// 5). Worked out once per list, not per frame.
    pub fn long_lengths(&self) -> bool {
        self.long_lengths
    }

    /// The characters to underline in ranked row `index`: char indices into its title and artist
    /// as the row draws them (UPPERCASE). Each query word is marked only in the field the
    /// ranking matched it in (the title or the artist, with the ranking's boosts); a word ranked
    /// on the album, file name or a folder marks nothing. Worked out the first time the row is
    /// drawn and kept until the list changes.
    pub fn row_marks(&mut self, index: usize) -> RowMarks {
        if self.row_marks.len() != self.filtered.len() {
            self.row_marks = vec![None; self.filtered.len()];
        }
        if let Some(Some(marks)) = self.row_marks.get(index) {
            return marks.clone();
        }
        let Some((_, track)) = self.filtered.get(index) else {
            return RowMarks::default();
        };
        let (in_title, in_artist) = self.row_fields.get(index).copied().unwrap_or((0, 0));
        let marks = if in_title == 0 && in_artist == 0 {
            RowMarks::default()
        } else {
            self.view_work += 1;
            let title = super::kit::upper(track.display_title());
            let artist = super::kit::upper(&track.artist);
            let matcher = SkimMatcherV2::default();
            let mut marks = RowMarks::default();
            for (word, token) in self.ranked_tokens.iter().enumerate().take(64) {
                let bit = 1u64 << word;
                let (text, out) = if in_title & bit != 0 {
                    (&title, &mut marks.0)
                } else if in_artist & bit != 0 {
                    (&artist, &mut marks.1)
                } else {
                    continue;
                };
                if let Some((_, indices)) = matcher.fuzzy_indices(text, token) {
                    out.extend(indices);
                }
            }
            marks
        };
        self.row_marks[index] = Some(marks.clone());
        marks
    }

    /// How many list scans and row match computations drawing the list has needed (test seam:
    /// a redraw of an unchanged list needs none).
    #[doc(hidden)]
    pub fn view_work(&self) -> u64 {
        self.view_work
    }

    pub fn all_filtered_tracks(&self) -> Vec<AudioMetadata> {
        self.filtered.iter().map(|(_, t)| t.clone()).collect()
    }
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
