//! Tab 2 Queue & Library in the Telemetry style (mockups 2c, 3a, 3g): the LIBRARY panel (the
//! file browser) and the QUEUE panel side by side, or only the focused one under 80 columns.
//!
//! Rows are mapped from the browser entries and queue tracks at draw time (the stored labels are
//! left as they are) and only the rows on screen are drawn. What needs a pass over a whole list —
//! the set of queued paths behind `IN QUEUE`, the queue's and the folder's total length, and the
//! column layout (decided from every row, so it doesn't change while scrolling) — is cached and
//! rebuilt only when the queue's revision or the browsed folder's tags
//! (`MetadataCache::folder_revision`) change, so a frame never walks a 5,000-entry folder or
//! reads a file, and tags read for other folders don't make it.

use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::prelude::*;

use super::kit::{self, text_width, Col, PanelSpec, RowMark};
use super::screen::{Screen, WidthTier};
use super::{theme, AppUI};

/// Path of the `★ Music Homes` row.
const MUSIC_HOMES: &str = "___MUSIC_HOMES___";

/// Separator between the fields of a footnote or a label note.
pub(super) const SEP: &str = "  ·  ";

/// Columns between the two panels: 4 from 90 columns, 2 below (3g).
const GAP: u16 = 4;
const COMPACT_GAP: u16 = 2;

/// Width of the library's index column (`DIR`, `01`, `★`, `▪`): 6 from 90 columns, 5 below.
const INDEX_COLS: u16 = 6;
const COMPACT_INDEX_COLS: u16 = 5;

/// Width of the marker column both panels start every row with: the unfocused pane's `▸`, in a
/// column of its own so it never hides the playing track's `▶` / `▪`.
const MARKER_COLS: u16 = 2;

/// Width of the queue's playing column (`▶`).
const PLAYING_COLS: u16 = 2;

/// Shown in the format column of a library file that is in the queue (resolution 6).
const IN_QUEUE: &str = "IN QUEUE";

/// Values for tab 2 that would otherwise need a pass over a whole list every frame.
#[derive(Debug, Default)]
pub(super) struct QueueTabCache {
    /// Paths of the queued tracks, for `IN QUEUE`.
    queued: HashSet<PathBuf>,
    /// `PlaybackQueue::revision` the set was built for.
    queued_revision: Option<u64>,
    /// How many times the set has been built (tests check it is not rebuilt every frame).
    queued_builds: u64,
    /// The queue's total length and column layout, per queue revision.
    queue_summary: Option<(u64, QueueSummary)>,
    /// The browsed folder's total length and column layout.
    folder_summary: Option<(FolderKey, FolderSummary)>,
    /// How many times `folder_summary` has been computed.
    folder_builds: u64,
    /// Whether a file of the browsed folder is queued, per (`folder_builds`, queue revision).
    folder_queued: Option<((u64, u64), bool)>,
    /// The home folder, shown as `~` in the library and playlist footnotes (read once).
    pub(super) home: Option<PathBuf>,
}

/// What the folder summary depends on: the folder, its listing and the tags of its files.
#[derive(Debug)]
struct FolderKey {
    dir: PathBuf,
    scan_id: u64,
    entries: usize,
    bookmarks: bool,
    folder_revision: u64,
}

/// The browsed folder's total length and what its columns need, from all its files (not only
/// the rows on screen), so the columns don't change while the list scrolls.
#[derive(Debug, Clone, Copy)]
struct FolderSummary {
    /// `None` when the folder has no audio files or any length is unknown.
    total: Option<f64>,
    any_artist: bool,
    /// Widest format (file extension), before `IN QUEUE` is taken into account.
    format_cols: usize,
    duration_cols: usize,
}

/// The queue's total length and what its columns need, from all its tracks.
#[derive(Debug, Clone, Copy)]
struct QueueSummary {
    /// `None` when the queue is empty or any length is unknown.
    total: Option<f64>,
    duration_cols: usize,
    any_artist: bool,
}

/// Width of a length column whose longest value is `longest` seconds (0 when none is known):
/// [`kit::duration`] is never narrower for a longer length.
pub(super) fn duration_cols(longest: f64) -> usize {
    if longest > 0.0 {
        text_width(&kit::duration(longest))
    } else {
        0
    }
}

impl QueueTabCache {
    pub(super) fn new() -> Self {
        Self {
            home: crate::web_access::home_dir(),
            ..Self::default()
        }
    }
}

/// One library row, mapped from a browser entry (or the loading row).
struct LibraryRow {
    index: String,
    index_style: Style,
    name: String,
    name_style: Style,
    /// Char range of `name` that the spotlight query matched.
    matched: Option<std::ops::Range<usize>>,
    /// Dropped to DIM by the spotlight (not a match).
    dimmed: bool,
    /// Files only: artist (Tags mode), format (or `IN QUEUE`) and duration.
    file: Option<FileCols>,
}

struct FileCols {
    artist: String,
    format: String,
    duration: String,
}

/// One queue row.
struct QueueRow {
    playing: bool,
    title: String,
    artist: String,
    duration: String,
}

/// The queue mode setting as the panels name it (`LOOP`, `STOP AT END`, `REPEAT ONE`).
pub(super) fn queue_mode_note(mode: &str) -> String {
    match mode {
        "loop" => "LOOP".to_string(),
        "stop" => "STOP AT END".to_string(),
        "repeat_one" => "REPEAT ONE".to_string(),
        other => kit::upper(other),
    }
}

/// `text` right-aligned in `width` columns.
pub(super) fn right_aligned(text: &str, width: usize) -> String {
    format!("{}{}", " ".repeat(width.saturating_sub(text_width(text))), text)
}

/// The left and right panes of a two-pane tab (2 and 5): side by side with a gap, the left one
/// `share.0 / share.1` of the width the gap leaves, or (under 80 columns) only the focused pane
/// across the whole width.
pub(super) fn split_panes(
    area: Rect,
    tier: WidthTier,
    right_focus: bool,
    share: (u16, u16),
) -> (Option<Rect>, Option<Rect>) {
    if tier == WidthTier::Narrow {
        return if right_focus {
            (None, Some(area))
        } else {
            (Some(area), None)
        };
    }
    let gap = if tier >= WidthTier::Medium { GAP } else { COMPACT_GAP };
    let free = area.width.saturating_sub(gap);
    let left = (u32::from(free) * u32::from(share.0) / u32::from(share.1.max(1))) as u16;
    let right = area.width.saturating_sub(gap + left);
    (
        Some(Rect { width: left, ..area }),
        Some(Rect::new(area.x + left + gap, area.y, right, area.height)),
    )
}

pub(super) use crate::paths::display_path;

/// Char range of the first occurrence of `query` in `text`, both compared in UPPERCASE (as the
/// rows are drawn).
fn match_range(text: &str, query: &str) -> Option<std::ops::Range<usize>> {
    let query = kit::upper(query);
    if query.is_empty() {
        return None;
    }
    let byte = text.find(&query)?;
    let start = text[..byte].chars().count();
    Some(start..start + query.chars().count())
}

impl AppUI {
    /// How many times the set of queued paths behind `IN QUEUE` has been built.
    #[doc(hidden)]
    pub fn in_queue_set_builds(&self) -> u64 {
        self.queue_tab.queued_builds
    }

    /// Rebuild the set of queued paths if the queue changed since it was built.
    fn refresh_queued_paths(&mut self) {
        let revision = self.queue.revision();
        if self.queue_tab.queued_revision == Some(revision) {
            return;
        }
        self.queue_tab.queued = self.queue.tracks().iter().map(|t| t.filepath.clone()).collect();
        self.queue_tab.queued_revision = Some(revision);
        self.queue_tab.queued_builds += 1;
    }

    /// The queue's total length and column layout, rebuilt only when the queue changes.
    fn queue_summary(&mut self) -> QueueSummary {
        let revision = self.queue.revision();
        if let Some((cached, summary)) = self.queue_tab.queue_summary {
            if cached == revision {
                return summary;
            }
        }
        let tracks = self.queue.tracks();
        let all_known = !tracks.is_empty() && tracks.iter().all(|t| t.duration_seconds > 0.0);
        let longest = tracks.iter().map(|t| t.duration_seconds).fold(0.0, f64::max);
        let summary = QueueSummary {
            total: all_known.then(|| self.queue.total_duration_seconds()),
            duration_cols: duration_cols(longest),
            any_artist: tracks.iter().any(|t| !t.artist.is_empty()),
        };
        self.queue_tab.queue_summary = Some((revision, summary));
        summary
    }

    /// How many times the browsed folder's summary (total length, column layout) has been
    /// computed (tests check tags for other folders don't recompute it).
    #[doc(hidden)]
    pub fn folder_summary_builds(&self) -> u64 {
        self.queue_tab.folder_builds
    }

    /// The browsed folder's total length and column layout, from the tags already in the
    /// metadata cache. Recomputed only when the folder, its listing or the tags of a file
    /// directly inside it change ([`MetadataCache::folder_revision`]), never for tags read for
    /// other folders.
    ///
    /// [`MetadataCache::folder_revision`]: crate::library::MetadataCache::folder_revision
    fn folder_summary(&mut self) -> FolderSummary {
        let (scan_id, entries, bookmarks, folder_revision) = (
            self.dir_scan_id,
            self.browser_entries.len(),
            self.in_bookmarks_view,
            self.metadata_cache.folder_revision(&self.browser_dir),
        );
        if let Some((cached, summary)) = &self.queue_tab.folder_summary {
            if cached.dir == self.browser_dir
                && (cached.scan_id, cached.entries, cached.bookmarks, cached.folder_revision)
                    == (scan_id, entries, bookmarks, folder_revision)
            {
                return *summary;
            }
        }
        let key = FolderKey {
            dir: self.browser_dir.clone(),
            scan_id,
            entries,
            bookmarks,
            folder_revision,
        };
        let files = self.browser_files();
        let summary = self.metadata_cache.read_with(|map| {
            let mut total = Some(0.0);
            let (mut longest, mut format_cols, mut any_artist) = (0.0_f64, 0, false);
            for (_, path, _) in files {
                let format = path.extension().map(|e| kit::upper(&e.to_string_lossy()));
                format_cols = format_cols.max(format.as_deref().map(text_width).unwrap_or(0));
                let meta = map.get(path);
                any_artist |= meta.is_some_and(|m| !m.artist.is_empty());
                let secs = meta.map(|m| m.duration_seconds).unwrap_or(0.0);
                if secs > 0.0 {
                    longest = longest.max(secs);
                    total = total.map(|t| t + secs);
                } else {
                    total = None;
                }
            }
            FolderSummary {
                total: total.filter(|_| !files.is_empty()),
                any_artist,
                format_cols,
                duration_cols: duration_cols(longest),
            }
        });
        self.queue_tab.folder_summary = Some((key, summary));
        self.queue_tab.folder_builds += 1;
        summary
    }

    /// The file rows of the browsed folder (they follow the folder rows).
    fn browser_files(&self) -> &[(String, PathBuf, bool)] {
        let first_file = self.browser_entries.partition_point(|(_, _, is_dir)| *is_dir);
        &self.browser_entries[first_file..]
    }

    /// Whether any file of the browsed folder is queued (its format column then needs room for
    /// `IN QUEUE`), rebuilt only when the folder summary or the queue changes. Call after
    /// [`Self::folder_summary`] and [`Self::refresh_queued_paths`].
    fn folder_has_queued(&mut self) -> bool {
        let key = (self.queue_tab.folder_builds, self.queue.revision());
        if let Some((cached, any)) = self.queue_tab.folder_queued {
            if cached == key {
                return any;
            }
        }
        let queued = &self.queue_tab.queued;
        let any = self.browser_files().iter().any(|(_, path, _)| queued.contains(path));
        self.queue_tab.folder_queued = Some((key, any));
        any
    }

    /// Total length of the audio files in the browsed folder: `None` while the folder is being
    /// read, in Music Homes, when it has no audio files or when any of their lengths is unknown.
    fn folder_total(&mut self) -> Option<f64> {
        if self.browser_loading || self.in_bookmarks_view {
            return None;
        }
        self.folder_summary().total
    }

    /// Tab 2: the LIBRARY and QUEUE panels (2c). From 90 columns they are 4 columns apart; at
    /// 80–89 the artist and format columns drop, the panels are 2 apart and their footnotes move
    /// to the label row (3g); under 80 only the focused pane shows, and `h` / `l` switch it.
    /// The page sizes of PageUp / PageDown are the rows each list shows.
    pub(super) fn render_queue_tab(&mut self, f: &mut Frame, area: Rect) {
        let screen = Screen::new(f.area());
        let compact = screen.tier < WidthTier::Medium || screen.short;
        let (library_area, queue_area) = split_panes(area, screen.tier, self.queue_focus, (1, 2));
        let library_rows = library_area.map(|a| self.render_library_panel(f, a, screen.tier, compact));
        let queue_rows = queue_area.map(|a| self.render_queue_panel(f, a, screen.tier, compact));
        // A hidden pane pages by the rows the shown one has: the same panel, a pane-switch away.
        self.browser_page_height = library_rows.or(queue_rows).unwrap_or(1).max(1) as usize;
        self.queue_page_height = queue_rows.or(library_rows).unwrap_or(1).max(1) as usize;
    }

    /// The LIBRARY panel; returns the rows its list shows.
    fn render_library_panel(&mut self, f: &mut Frame, area: Rect, tier: WidthTier, compact: bool) -> u16 {
        let text_cols = usize::from(area.width.saturating_sub(2));
        let tags = self.settings_mgr.settings.metadata_view;
        let spotlight = self.browser_spotlight.clone();
        let matches = self.spotlight_matches();
        let position = format!("{} / {}", self.browser_cursor + 1, self.browser_entries.len());

        let (footnote, compact) = if let Some(query) = &spotlight {
            // The query needs its own row, even where footnotes go to the label row.
            (self.spotlight_footnote(query, &matches, text_cols), false)
        } else if compact {
            (Line::from(position.replace(' ', "")), true)
        } else {
            (self.library_footnote(&position, text_cols), false)
        };
        let note = if spotlight.is_some() {
            "SPOTLIGHT"
        } else if tags {
            "TAGS"
        } else {
            "FILES"
        };
        let list = kit::panel(
            f,
            area,
            PanelSpec {
                label: "Library",
                right: Some(note),
                focused: !self.queue_focus,
                footnote: Some(footnote),
                compact,
            },
        );

        let loading_row = usize::from(self.browser_loading);
        let len = self.browser_entries.len() + loading_row;
        self.browser_state.select(Some(self.browser_cursor));
        let window = kit::visible_window(&mut self.browser_state, len, usize::from(list.height));
        let summary = self.folder_summary();
        self.refresh_queued_paths();
        let any_queued = self.folder_has_queued();

        let query = spotlight.as_deref().filter(|q| !q.is_empty());
        // With a query that matches, everything but the matches drops to DIM (3a).
        let dim_others = query.is_some() && !matches.is_empty();
        let match_set: HashSet<usize> = matches.iter().copied().collect();
        let playing = self.queue.current_track().map(|t| t.filepath.clone());
        let first_file = self.browser_entries.partition_point(|(_, _, is_dir)| *is_dir);
        let header_rows = if self.browser_dir.parent().is_some() { 2 } else { 1 };

        let rows: Vec<(usize, LibraryRow)> = self.metadata_cache.read_with(|map| {
            window
                .clone()
                .map(|idx| {
                    let Some((label, path, is_dir)) = self.browser_entries.get(idx) else {
                        let items = self.browser_entries.len().saturating_sub(header_rows);
                        let text = if items > 0 {
                            format!("READING DIRECTORY{SEP}{items} ITEMS")
                        } else {
                            "READING DIRECTORY".to_string()
                        };
                        return (
                            idx,
                            LibraryRow {
                                index: "···".to_string(),
                                index_style: theme::dim(),
                                name: text,
                                name_style: theme::dim(),
                                matched: None,
                                dimmed: false,
                                file: None,
                            },
                        );
                    };
                    let mut row = if path.as_os_str() == MUSIC_HOMES {
                        LibraryRow {
                            index: "★".to_string(),
                            index_style: theme::mid(),
                            name: "MUSIC HOMES".to_string(),
                            name_style: theme::mid(),
                            matched: None,
                            dimmed: false,
                            file: None,
                        }
                    } else if *is_dir && label.starts_with("..") {
                        let name = if self.in_bookmarks_view {
                            "FILE SYSTEM"
                        } else {
                            "PARENT"
                        };
                        LibraryRow {
                            index: "..".to_string(),
                            index_style: theme::mid(),
                            name: name.to_string(),
                            name_style: theme::mid(),
                            matched: None,
                            dimmed: false,
                            file: None,
                        }
                    } else if *is_dir {
                        let clean = label
                            .strip_prefix("📁 ")
                            .and_then(|s| s.strip_suffix('/'))
                            .unwrap_or(label);
                        let name = format!("{}/", kit::upper(clean));
                        let matched = query
                            .filter(|_| match_set.contains(&idx))
                            .and_then(|q| match_range(&name, q));
                        LibraryRow {
                            index: "DIR".to_string(),
                            index_style: theme::dim(),
                            name,
                            name_style: theme::ink().add_modifier(Modifier::BOLD),
                            matched,
                            dimmed: false,
                            file: None,
                        }
                    } else {
                        let meta = map.get(path);
                        let (name, artist) = match (tags, meta) {
                            (true, Some(m)) => (kit::upper(m.display_title()), kit::upper(&m.artist)),
                            (true, None) => (kit::upper(label), String::new()),
                            // Filenames mode: the name as it is on disk (resolution 2).
                            (false, _) => (label.clone(), String::new()),
                        };
                        let format = if self.queue_tab.queued.contains(path) {
                            IN_QUEUE.to_string()
                        } else {
                            path.extension()
                                .map(|e| kit::upper(&e.to_string_lossy()))
                                .unwrap_or_default()
                        };
                        let duration = meta
                            .map(|m| m.duration_seconds)
                            .filter(|&s| s > 0.0)
                            .map(kit::duration)
                            .unwrap_or_default();
                        let is_playing = playing.as_deref() == Some(path.as_path());
                        LibraryRow {
                            index: if is_playing {
                                "▪".to_string()
                            } else {
                                format!("{:02}", (idx + 1).saturating_sub(first_file))
                            },
                            index_style: if is_playing { theme::ink() } else { theme::dim() },
                            name,
                            name_style: theme::ink(),
                            matched: None,
                            dimmed: false,
                            file: Some(FileCols {
                                artist,
                                format,
                                duration,
                            }),
                        }
                    };
                    if dim_others && !match_set.contains(&idx) {
                        row.index_style = theme::dim();
                        row.name_style = theme::dim();
                        row.dimmed = true;
                    }
                    (idx, row)
                })
                .collect()
        });

        // Columns decided from every file of the folder, so they don't change while scrolling.
        let wide = tier >= WidthTier::Medium;
        let show_artist = wide && tags && summary.any_artist;
        let format_cols = if any_queued {
            summary.format_cols.max(text_width(IN_QUEUE))
        } else {
            summary.format_cols
        };
        let duration_cols = summary.duration_cols;
        let index_cols = if wide { INDEX_COLS } else { COMPACT_INDEX_COLS };

        for (row_y, (idx, row)) in rows.iter().enumerate() {
            let y = list.y + row_y as u16;
            let mark = if *idx != self.browser_cursor {
                RowMark::Plain
            } else if self.queue_focus {
                RowMark::Marker
            } else {
                RowMark::Cursor
            };
            let mut name = Col::flex(row.name.as_str(), row.name_style);
            if let Some(range) = &row.matched {
                name = name.underline(range.clone());
            }
            let mut cols = vec![
                Col::fixed("", MARKER_COLS, theme::mid()),
                Col::fixed(row.index.as_str(), index_cols, row.index_style),
                name,
            ];
            if let Some(file) = &row.file {
                // Everything on the right of a file row is MID, unless the spotlight dims it.
                let side = if row.dimmed { theme::dim() } else { theme::mid() };
                if show_artist {
                    cols.push(Col::flex(file.artist.as_str(), side));
                }
                if wide && format_cols > 0 {
                    let format = right_aligned(&file.format, format_cols);
                    cols.push(Col::fixed(format, format_cols as u16 + 2, side));
                }
                if duration_cols > 0 {
                    let duration = right_aligned(&file.duration, duration_cols);
                    cols.push(Col::fixed(duration, duration_cols as u16, side));
                }
            }
            kit::list_row(f, Rect::new(list.x, y, list.width, 1), &cols, mark);
        }
        list.height
    }

    /// The library footnote: the folder (`~` for home, original case; its start cut with `…`
    /// when long), the cursor position and, when every length is known, the folder's total length.
    fn library_footnote(&mut self, position: &str, text_cols: usize) -> Line<'static> {
        let mut suffix = format!("{SEP}{position}");
        if let Some(total) = self.folder_total() {
            suffix.push_str(SEP);
            suffix.push_str(&kit::duration(total));
        }
        let place = if self.in_bookmarks_view {
            "MUSIC HOMES".to_string()
        } else {
            display_path(&self.browser_dir, self.queue_tab.home.as_deref())
        };
        let room = text_cols.saturating_sub(text_width(&suffix));
        Line::from(format!("{}{suffix}", kit::visible_tail(&place, room)))
    }

    /// The spotlight footnote (3a): the `/` keycap, the query as typed with a block cursor, and
    /// on the right the match count (ERROR when nothing matches).
    fn spotlight_footnote(&self, query: &str, matches: &[usize], text_cols: usize) -> Line<'static> {
        let (count, count_style) = if query.is_empty() {
            let folders = self
                .browser_entries
                .iter()
                .filter(|entry| super::is_subfolder_entry(entry))
                .count();
            (format!("TYPE TO JUMP ({folders} SUBFOLDERS)"), theme::mid())
        } else if matches.is_empty() {
            ("NO MATCHING SUBFOLDERS".to_string(), theme::error())
        } else {
            let at = matches
                .iter()
                .position(|&i| i == self.browser_cursor)
                .map(|p| p + 1)
                .unwrap_or(1);
            (format!("{at} OF {} SUBFOLDERS", matches.len()), theme::mid())
        };
        // ` / `, 2 spaces, the query, the cursor block, at least 2 spaces before the count.
        let fixed = 3 + 2 + 1 + 2;
        let count_cols = text_width(&count);
        let (count, count_cols) = if fixed + count_cols < text_cols {
            (count, count_cols)
        } else {
            (String::new(), 0)
        };
        let query = kit::visible_tail(query, text_cols.saturating_sub(fixed + count_cols));
        let pad = text_cols.saturating_sub(fixed - 2 + text_width(&query) + count_cols);
        Line::from(vec![
            Span::styled(" / ", theme::keycap()),
            Span::raw("  "),
            Span::styled(query, theme::ink().add_modifier(Modifier::BOLD)),
            Span::styled(" ", theme::cursor()),
            Span::raw(" ".repeat(pad)),
            Span::styled(count, count_style),
        ])
    }

    /// The QUEUE panel; returns the rows its list shows.
    fn render_queue_panel(&mut self, f: &mut Frame, area: Rect, tier: WidthTier, compact: bool) -> u16 {
        let len = self.queue.len();
        let summary = self.queue_summary();
        let footnote = if compact {
            len.to_string()
        } else {
            let tracks = kit::upper(&super::playlists::track_count_label(len));
            match summary.total {
                Some(total) => format!("{tracks}{SEP}{}", kit::duration(total)),
                None => tracks,
            }
        };
        let note = queue_mode_note(&self.settings_mgr.settings.queue_mode);
        let list = kit::panel(
            f,
            area,
            PanelSpec {
                label: "Queue",
                right: Some(&note),
                focused: self.queue_focus,
                footnote: Some(Line::from(footnote)),
                compact,
            },
        );

        self.queue_state.select(Some(self.queue_cursor));
        let window = kit::visible_window(&mut self.queue_state, len, usize::from(list.height));
        let tags = self.settings_mgr.settings.metadata_view;
        let current = self.queue.current_index();
        let rows: Vec<(usize, QueueRow)> = window
            .map(|idx| {
                let track = &self.queue.tracks()[idx];
                let (title, artist) = if tags {
                    (kit::upper(track.display_title()), kit::upper(&track.artist))
                } else {
                    let name = if track.filename.is_empty() {
                        track
                            .filepath
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    } else {
                        track.filename.clone()
                    };
                    (name, String::new())
                };
                let duration = if track.duration_seconds > 0.0 {
                    kit::duration(track.duration_seconds)
                } else {
                    String::new()
                };
                (
                    idx,
                    QueueRow {
                        playing: idx == current,
                        title,
                        artist,
                        duration,
                    },
                )
            })
            .collect();

        let digits = len.max(1).to_string().len().max(2);
        let index_cols = digits as u16 + 2;
        // Columns decided from the whole queue, so they don't change while scrolling.
        let duration_cols = summary.duration_cols;
        let show_artist = tags && tier >= WidthTier::Medium && summary.any_artist;
        // Title and artist share what is left about 3:2 (26 and 17 columns in 2c).
        let free = usize::from(list.width)
            .saturating_sub(usize::from(MARKER_COLS + PLAYING_COLS + index_cols) + duration_cols);
        let title_cols = ((free * 3 + 2) / 5) as u16;

        for (row_y, (idx, row)) in rows.iter().enumerate() {
            let y = list.y + row_y as u16;
            let mark = if *idx != self.queue_cursor {
                RowMark::Plain
            } else if self.queue_focus {
                RowMark::Cursor
            } else {
                RowMark::Marker
            };
            let bold = theme::ink().add_modifier(Modifier::BOLD);
            let (index_style, title_style, side) = if row.playing {
                (bold, bold, bold)
            } else if mark == RowMark::Marker {
                (theme::mid(), theme::ink(), theme::mid())
            } else {
                (theme::dim(), theme::ink(), theme::mid())
            };
            let playing = if row.playing { "▶" } else { "" };
            let mut cols = vec![
                Col::fixed("", MARKER_COLS, theme::mid()),
                Col::fixed(playing, PLAYING_COLS, bold),
                Col::fixed(format!("{:02}", idx + 1), index_cols, index_style),
            ];
            if show_artist {
                cols.push(Col::fixed(row.title.as_str(), title_cols, title_style));
                cols.push(Col::flex(row.artist.as_str(), side));
            } else {
                cols.push(Col::flex(row.title.as_str(), title_style));
            }
            if duration_cols > 0 {
                cols.push(Col::fixed(
                    right_aligned(&row.duration, duration_cols),
                    duration_cols as u16,
                    side,
                ));
            }
            kit::list_row(f, Rect::new(list.x, y, list.width, 1), &cols, mark);
        }
        list.height
    }
}
