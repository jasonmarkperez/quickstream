//! Tab 5 (`[5] Playlists`): hand-curated playlists on the left, the tracks of the selected one on
//! the right (issue #37). The store is read by a worker at startup and every edit is written
//! through to disk by the persistence worker, so nothing here touches the disk on the UI thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::AppUI;
use crate::paths;
use crate::playlist::{Playlist, PlaylistEntry};
use crate::playlist_store::PlaylistStore;

/// Last row of the add picker: create a playlist and add to it.
pub const NEW_PLAYLIST_ROW: &str = "+ New playlist…";

/// What the name prompt's name is for.
#[derive(Debug, Clone)]
pub enum NamePurpose {
    Create,
    Rename {
        old: String,
    },
    /// "+ New playlist…" in the add picker: create the playlist, then add `PendingAdd` to it.
    CreateAndAdd(PendingAdd),
}

/// Tracks waiting for the user to choose a playlist.
#[derive(Debug, Clone)]
pub enum PendingAdd {
    /// Entries built from metadata the UI already holds; `label` is shown in the picker header.
    Entries { label: String, entries: Vec<PlaylistEntry> },
    /// A browser folder; a worker lists its audio files once a playlist is chosen.
    Folder { label: String, dir: PathBuf },
}

/// Modal owned by the playlists feature. While one is open it takes every key.
#[derive(Debug, Clone)]
pub enum PlaylistModal {
    NamePrompt {
        purpose: NamePurpose,
        input: String,
        error: Option<String>,
    },
    ConfirmDelete {
        name: String,
        count: usize,
    },
    AddPicker {
        pending: PendingAdd,
        cursor: usize,
    },
}

/// Tab 5 state, held by `AppUI::playlists`.
pub struct PlaylistsUi {
    pub store: PlaylistStore,
    /// Selected row in the playlists (left) pane.
    pub list_cursor: usize,
    /// Selected row in the tracks (right) pane.
    pub track_cursor: usize,
    /// false = playlists pane focused, true = tracks pane.
    pub tracks_focus: bool,
    pub modal: Option<PlaylistModal>,
    /// `Path::exists()` results for playlist entries, cached for the session.
    pub exists: HashMap<PathBuf, bool>,
    /// Result of the startup load (playlists, stems of skipped files); `None` once applied.
    load_rx: Option<mpsc::Receiver<(Vec<Playlist>, Vec<String>)>>,
    /// The load worker died without sending; the store stays unloaded so no edit can overwrite
    /// a file that was never read.
    load_failed: bool,
    list_state: ListState,
    tracks_state: ListState,
}

impl PlaylistsUi {
    pub fn new(persist: crate::persist::PersistHandle) -> Self {
        Self {
            store: PlaylistStore::new(paths::get_playlists_dir(), persist),
            list_cursor: 0,
            track_cursor: 0,
            tracks_focus: false,
            modal: None,
            exists: HashMap::new(),
            load_rx: None,
            load_failed: false,
            list_state: ListState::default(),
            tracks_state: ListState::default(),
        }
    }

    /// Playlist under the left-pane cursor.
    pub fn selected_playlist(&self) -> Option<&Playlist> {
        self.store.playlists().get(self.list_cursor)
    }

    /// Name of the playlist under the left-pane cursor.
    pub fn selected_name(&self) -> Option<String> {
        self.selected_playlist().map(|p| p.name.clone())
    }

    /// Keep both cursors inside their lists after playlists or tracks went away.
    pub fn clamp_cursors(&mut self) {
        let playlists = self.store.playlists().len();
        if self.list_cursor >= playlists {
            self.list_cursor = playlists.saturating_sub(1);
        }
        let tracks = self.selected_playlist().map_or(0, |p| p.entries.len());
        if self.track_cursor >= tracks {
            self.track_cursor = tracks.saturating_sub(1);
        }
        if playlists == 0 {
            self.tracks_focus = false;
        }
    }

    /// Move the cursor of the focused pane one row, wrapping at both ends.
    fn move_cursor(&mut self, down: bool) {
        if self.tracks_focus {
            let tracks = self.selected_playlist().map_or(0, |p| p.entries.len());
            self.track_cursor = wrap_step(self.track_cursor, tracks, down);
        } else {
            let before = self.list_cursor;
            self.list_cursor = wrap_step(self.list_cursor, self.store.playlists().len(), down);
            if self.list_cursor != before {
                self.track_cursor = 0;
            }
        }
    }
}

/// Next row in a list of `len` rows, wrapping from the last row to the first and back.
fn wrap_step(cursor: usize, len: usize, down: bool) -> usize {
    if len == 0 {
        0
    } else if down {
        (cursor + 1) % len
    } else if cursor == 0 {
        len - 1
    } else {
        cursor - 1
    }
}

/// `m:ss`, or `h:mm:ss` from one hour; `-:--` when the length is unknown (negative).
pub fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return "-:--".to_string();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// "1 track" / "24 tracks".
pub fn track_count_label(count: usize) -> String {
    if count == 1 {
        "1 track".to_string()
    } else {
        format!("{} tracks", count)
    }
}

/// Tracks-pane row for the entry at `index`: `<n>. <artist> - <title>  <m:ss>`.
pub fn track_row_label(index: usize, entry: &PlaylistEntry) -> String {
    format!(
        "{}. {}  {}",
        index + 1,
        entry.display_label(),
        format_duration(entry.duration_secs)
    )
}

impl AppUI {
    /// Read the playlists folder on a worker; `poll_playlists` hands the result to the store.
    /// The worker also returns the names of the files it skipped, so the store can keep them
    /// reserved and never write over them.
    pub fn start_playlist_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.playlists.load_rx = Some(rx);
        self.playlists.load_failed = false;
        let dir = self.playlists.store.dir().to_path_buf();
        self.workers.spawn("playlist-load", move || {
            let _ = tx.send(PlaylistStore::load_dir_reserving(&dir));
        });
    }

    /// Apply finished playlist work without blocking. Called every UI tick.
    pub fn poll_playlists(&mut self) {
        if let Some(rx) = self.playlists.load_rx.as_ref() {
            match rx.try_recv() {
                Ok((playlists, skipped)) => {
                    crate::log_info!(
                        "playlists",
                        "Loaded {} playlists ({} skipped files keep their names reserved)",
                        playlists.len(),
                        skipped.len()
                    );
                    self.playlists.store.set_loaded(playlists);
                    self.playlists.store.reserve_names(skipped);
                    self.playlists.load_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    crate::log_error!("playlists", "Playlist load worker exited without a result");
                    self.playlists.load_rx = None;
                    self.playlists.load_failed = true;
                    self.status_message = "Couldn't load playlists".to_string();
                }
            }
        }
        self.playlists.clamp_cursors();
    }

    /// Keys while a playlist modal is open. Returns true when the app should quit.
    pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.player.stop()?;
            return Ok(true);
        }
        if key.code == KeyCode::Esc {
            self.playlists.modal = None;
        }
        Ok(false)
    }

    /// Tab 5 keys that the global hotkeys did not take.
    pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.playlists.clamp_cursors();
        match (code, modifiers) {
            (KeyCode::Char('h'), KeyModifiers::NONE) => self.playlists.tracks_focus = false,
            (KeyCode::Char('l'), KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE)
                if !self.playlists.tracks_focus && self.playlists.selected_playlist().is_some() =>
            {
                self.playlists.tracks_focus = true;
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.playlists.move_cursor(false);
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.playlists.move_cursor(true);
            }
            _ => {}
        }
    }

    pub(super) fn render_playlists_tab(&mut self, f: &mut Frame, area: Rect) {
        self.playlists.clamp_cursors();
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        let focused = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let unfocused = Style::default().fg(Color::DarkGray);
        let selected = Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let hint = Style::default().fg(Color::DarkGray);
        let empty_tracks = Block::default()
            .borders(Borders::ALL)
            .title(" TRACKS ")
            .border_style(unfocused);

        let ui = &mut self.playlists;
        if !ui.store.is_loaded() {
            let text = if ui.load_failed {
                "Couldn't load playlists. Restart Quickstream to try again."
            } else {
                "Loading playlists…"
            };
            let loading = Paragraph::new(text)
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" PLAYLISTS ")
                        .border_style(focused),
                );
            f.render_widget(loading, split[0]);
            f.render_widget(empty_tracks, split[1]);
            return;
        }

        // Left pane: playlists with their track counts.
        let list_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" PLAYLISTS ({}) ", ui.store.playlists().len()))
            .border_style(if ui.tracks_focus { unfocused } else { focused });
        if ui.store.playlists().is_empty() {
            let empty = Paragraph::new("No playlists yet. Press c to create one, or L on any track.")
                .style(hint)
                .wrap(Wrap { trim: true })
                .block(list_block);
            f.render_widget(empty, split[0]);
            f.render_widget(empty_tracks, split[1]);
            return;
        }
        let rows: Vec<ListItem> = ui
            .store
            .playlists()
            .iter()
            .enumerate()
            .map(|(idx, playlist)| {
                let text = format!("{}  ({})", playlist.name, track_count_label(playlist.entries.len()));
                let style = if idx != ui.list_cursor {
                    Style::default().fg(Color::White)
                } else if ui.tracks_focus {
                    // The open playlist stays marked while its tracks have focus.
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    selected
                };
                ListItem::new(text).style(style)
            })
            .collect();
        ui.list_state.select(Some(ui.list_cursor));
        f.render_stateful_widget(List::new(rows).block(list_block), split[0], &mut ui.list_state);

        // Right pane: the selected playlist's tracks.
        let playlist = &ui.store.playlists()[ui.list_cursor];
        let title = format!(
            " {} ({}, {}) ",
            playlist.name.to_uppercase(),
            track_count_label(playlist.entries.len()),
            format_duration(playlist.total_duration_secs())
        );
        let tracks_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if ui.tracks_focus { focused } else { unfocused });
        if playlist.entries.is_empty() {
            let empty = Paragraph::new("Empty. Press L on a track in tab 2 to add it.")
                .style(hint)
                .wrap(Wrap { trim: true })
                .block(tracks_block);
            f.render_widget(empty, split[1]);
            return;
        }
        let rows: Vec<ListItem> = playlist
            .entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let style = if idx == ui.track_cursor && ui.tracks_focus {
                    selected
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(track_row_label(idx, entry)).style(style)
            })
            .collect();
        ui.tracks_state.select(Some(ui.track_cursor));
        f.render_stateful_widget(List::new(rows).block(tracks_block), split[1], &mut ui.tracks_state);
    }

    pub(super) fn render_playlist_modal(&self, f: &mut Frame) {
        let Some(modal) = self.playlists.modal.as_ref() else {
            return;
        };
        let area = super::centered_rect(60, 50, f.area());
        f.render_widget(Clear, area);

        let text_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        let hint = Style::default().fg(Color::DarkGray);
        let mut scroll = 0u16;
        let (title, lines) = match modal {
            PlaylistModal::NamePrompt { purpose, input, error } => {
                let title = match purpose {
                    NamePurpose::Create | NamePurpose::CreateAndAdd(_) => " NEW PLAYLIST ".to_string(),
                    NamePurpose::Rename { old } => format!(" RENAME \"{}\" ", old),
                };
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled("Name: ", hint),
                        Span::styled(input.clone(), text_style),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ]),
                    Line::from(""),
                ];
                if let Some(error) = error {
                    lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled("[Enter] Save   [Esc] Cancel", hint)));
                (title, lines)
            }
            PlaylistModal::ConfirmDelete { name, count } => (
                " DELETE PLAYLIST ".to_string(),
                vec![
                    Line::from(Span::styled(
                        format!("Delete \"{}\" ({})? y/n", name, track_count_label(*count)),
                        text_style,
                    )),
                    Line::from(""),
                    Line::from(Span::styled("[y] Delete   any other key: Cancel", hint)),
                ],
            ),
            PlaylistModal::AddPicker { pending, cursor } => {
                let label = match pending {
                    PendingAdd::Entries { label, .. } | PendingAdd::Folder { label, .. } => label,
                };
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!("Add {} to:", label),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                ];
                let names = self
                    .playlists
                    .store
                    .playlists()
                    .iter()
                    .map(|p| p.name.as_str())
                    .chain(std::iter::once(NEW_PLAYLIST_ROW));
                for (idx, name) in names.enumerate() {
                    let (prefix, style) = if idx == *cursor {
                        (
                            " ▶ ",
                            Style::default()
                                .bg(Color::Blue)
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("   ", Style::default().fg(Color::White))
                    };
                    lines.push(Line::from(Span::styled(format!("{}{}", prefix, name), style)));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("[Enter] Add   [Esc] Cancel", hint)));
                // Keep the cursor row (after the 2 header lines) inside the visible rows.
                let visible = area.height.saturating_sub(2) as usize;
                scroll = (2 + *cursor + 1).saturating_sub(visible) as u16;
                (" ADD TO PLAYLIST ".to_string(), lines)
            }
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                title,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));
        let body = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        f.render_widget(body, area);
    }
}
