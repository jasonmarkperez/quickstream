//! Tab 2 Queue & Library in the Telemetry style (mockups 2c, 3a, 3g): the LIBRARY panel (the
//! file browser) and the QUEUE panel side by side, or only the focused one under 80 columns.
//!
//! Rows are mapped from the browser entries and queue tracks at draw time (the stored labels are
//! left as they are) and only the rows on screen are drawn. What needs a pass over a whole list —
//! the set of queued paths behind `IN QUEUE`, the queue's and the folder's total length — is
//! cached and rebuilt only when the queue's or the metadata cache's revision changes, so a frame
//! never walks a 5,000-entry folder or reads a file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::prelude::*;

use super::kit::{self, text_width, Col, PanelSpec, RowMark};
use super::screen::{Screen, WidthTier};
use super::{theme, AppUI};

/// Path of the `★ Music Homes` row.
const MUSIC_HOMES: &str = "___MUSIC_HOMES___";

/// Separator between the fields of a footnote.
const SEP: &str = "  ·  ";

/// Columns between the two panels: 4 from 90 columns, 2 below (3g).
const GAP: u16 = 4;
const COMPACT_GAP: u16 = 2;

/// Width of the library's index column (`DIR`, `01`, `★`, `▪`): 6 from 90 columns, 5 below.
const INDEX_COLS: u16 = 6;
const COMPACT_INDEX_COLS: u16 = 5;

/// Width of the queue's marker column (`▶`, `▸`).
const MARKER_COLS: u16 = 2;

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
    /// Total length of the queue, per queue revision; `None` when a length is unknown.
    queue_total: Option<(u64, Option<f64>)>,
    /// Total length of the audio files in the browsed folder; `None` when a length is unknown.
    folder_total: Option<(FolderKey, Option<f64>)>,
    /// The home folder, shown as `~` in the library footnote (read once).
    home: Option<PathBuf>,
}

/// What the folder total depends on: the folder, its listing and the tags in memory.
#[derive(Debug)]
struct FolderKey {
    dir: PathBuf,
    scan_id: u64,
    entries: usize,
    metadata_revision: u64,
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

/// `m:ss`-style length for list rows and totals: `MM:SS`, or `HH:MM:SS` from an hour.
pub(super) fn clock(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `text` right-aligned in `width` columns.
fn right_aligned(text: &str, width: usize) -> String {
    format!("{}{}", " ".repeat(width.saturating_sub(text_width(text))), text)
}

/// The library and queue areas: side by side with a gap, or (under 80 columns) only the focused
/// pane across the whole width.
fn split_panes(area: Rect, tier: WidthTier, queue_focus: bool) -> (Option<Rect>, Option<Rect>) {
    if tier == WidthTier::Narrow {
        return if queue_focus {
            (None, Some(area))
        } else {
            (Some(area), None)
        };
    }
    let gap = if tier >= WidthTier::Medium { GAP } else { COMPACT_GAP };
    let left = area.width.saturating_sub(gap) / 2;
    let right = area.width.saturating_sub(gap + left);
    (
        Some(Rect { width: left, ..area }),
        Some(Rect::new(area.x + left + gap, area.y, right, area.height)),
    )
}

/// `path` with the home folder shown as `~`, in its original case (resolution 2).
fn display_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(rest) = home.and_then(|home| path.strip_prefix(home).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

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

    /// Total length of the queue, or `None` when it is empty or any length is unknown.
    fn queue_total(&mut self) -> Option<f64> {
        let revision = self.queue.revision();
        if let Some((cached, total)) = self.queue_tab.queue_total {
            if cached == revision {
                return total;
            }
        }
        let tracks = self.queue.tracks();
        let total = (!tracks.is_empty() && tracks.iter().all(|t| t.duration_seconds > 0.0))
            .then(|| tracks.iter().map(|t| t.duration_seconds).sum());
        self.queue_tab.queue_total = Some((revision, total));
        total
    }

    /// Total length of the audio files in the browsed folder, from the lengths already in the
    /// metadata cache: `None` while the folder is being read, in Music Homes, when it has no
    /// audio files or when any of their lengths is unknown. Recomputed only when the folder, its
    /// listing or the metadata cache changes.
    fn folder_total(&mut self) -> Option<f64> {
        if self.browser_loading || self.in_bookmarks_view {
            return None;
        }
        let (scan_id, entries, metadata_revision) = (
            self.dir_scan_id,
            self.browser_entries.len(),
            self.metadata_cache.revision(),
        );
        if let Some((cached, total)) = &self.queue_tab.folder_total {
            if cached.dir == self.browser_dir
                && (cached.scan_id, cached.entries, cached.metadata_revision) == (scan_id, entries, metadata_revision)
            {
                return *total;
            }
        }
        let key = FolderKey {
            dir: self.browser_dir.clone(),
            scan_id,
            entries,
            metadata_revision,
        };
        let first_file = self.browser_entries.partition_point(|(_, _, is_dir)| *is_dir);
        let files = &self.browser_entries[first_file..];
        let total = self.metadata_cache.read_with(|map| {
            let mut sum = 0.0;
            for (_, path, _) in files {
                let secs = map.get(path).map(|m| m.duration_seconds).unwrap_or(0.0);
                if secs <= 0.0 {
                    return None;
                }
                sum += secs;
            }
            (!files.is_empty()).then_some(sum)
        });
        self.queue_tab.folder_total = Some((key, total));
        total
    }

    /// Tab 2: the LIBRARY and QUEUE panels (2c). From 90 columns they are 4 columns apart; at
    /// 80–89 the artist and format columns drop, the panels are 2 apart and their footnotes move
    /// to the label row (3g); under 80 only the focused pane shows, and `h` / `l` switch it.
    /// The page sizes of PageUp / PageDown are the rows each list shows.
    pub(super) fn render_queue_tab(&mut self, f: &mut Frame, area: Rect) {
        let screen = Screen::new(f.area());
        let compact = screen.tier < WidthTier::Medium || screen.short;
        let (library_area, queue_area) = split_panes(area, screen.tier, self.queue_focus);
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
        self.refresh_queued_paths();

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
                            .map(clock)
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

        // Column widths shared by the visible file rows, so formats and durations line up.
        let files = || rows.iter().filter_map(|(_, r)| r.file.as_ref());
        let wide = tier >= WidthTier::Medium;
        let show_artist = wide && tags && files().any(|c| !c.artist.is_empty());
        let format_cols = files().map(|c| text_width(&c.format)).max().unwrap_or(0);
        let duration_cols = files().map(|c| text_width(&c.duration)).max().unwrap_or(0);
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
            let index_style = if mark == RowMark::Marker {
                theme::mid()
            } else {
                row.index_style
            };
            let mut name = Col::flex(row.name.as_str(), row.name_style);
            if let Some(range) = &row.matched {
                name = name.underline(range.clone());
            }
            let mut cols = vec![Col::fixed(row.index.as_str(), index_cols, index_style), name];
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
            suffix.push_str(&clock(total));
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
                .filter(|(_, p, is_dir)| *is_dir && p.as_os_str() != MUSIC_HOMES)
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
        let footnote = if compact {
            len.to_string()
        } else {
            let tracks = if len == 1 {
                "1 TRACK".to_string()
            } else {
                format!("{len} TRACKS")
            };
            match self.queue_total() {
                Some(total) => format!("{tracks}{SEP}{}", clock(total)),
                None => tracks,
            }
        };
        let note = match self.settings_mgr.settings.queue_mode.as_str() {
            "loop" => "LOOP".to_string(),
            "stop" => "STOP AT END".to_string(),
            "repeat_one" => "REPEAT ONE".to_string(),
            other => kit::upper(other),
        };
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
                    clock(track.duration_seconds)
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
        let duration_cols = rows.iter().map(|(_, r)| text_width(&r.duration)).max().unwrap_or(0);
        let show_artist = tags && tier >= WidthTier::Medium && rows.iter().any(|(_, r)| !r.artist.is_empty());
        // Title and artist share what is left about 3:2 (26 and 17 columns in 2c).
        let free = usize::from(list.width).saturating_sub(usize::from(MARKER_COLS + index_cols) + duration_cols);
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
            let (marker_style, index_style, title_style, side) = if row.playing {
                (bold, bold, bold, bold)
            } else if mark == RowMark::Marker {
                (theme::mid(), theme::mid(), theme::ink(), theme::mid())
            } else {
                (theme::ink(), theme::dim(), theme::ink(), theme::mid())
            };
            let marker = if row.playing { "▶" } else { "" };
            let mut cols = vec![
                Col::fixed(marker, MARKER_COLS, marker_style),
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
