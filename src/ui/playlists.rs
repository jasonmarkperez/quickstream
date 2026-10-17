//! Tab 5 (`[5] Playlists`): hand-curated playlists on the left, the tracks of the selected one on
//! the right (issue #37). The store is read by a worker at startup and every edit is written
//! through to disk by the persistence worker, so nothing here touches the disk on the UI thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// Existence test used by the playlist workers: the path that may be played (symlinks
/// resolved), or `None` when the entry can't be queued. Tests swap in a slow one to stand in
/// for a hung network share.
pub type ExistsChecker = std::sync::Arc<dyn Fn(&std::path::Path) -> Option<std::path::PathBuf> + Send + Sync>;

/// A playlist snapshot whose files were checked on a worker, ready to be queued.
struct QueueCheckResult {
    name: String,
    entries: Vec<crate::playlist::PlaylistEntry>,
    /// Per entry, the checked (canonical) path to queue, or `None` when it can't be queued.
    resolved: Vec<Option<std::path::PathBuf>>,
    /// Track to play from (`Enter`: replace the queue); `None` appends (`a`).
    play_from: Option<usize>,
    /// [`PlaylistChecks::queue_generation`] when the request was made.
    generation: u64,
}

/// Background existence checks for tab 5. Files are only checked on workers: a `stat` on an
/// unmounted share can hang, and the UI must keep drawing.
struct PlaylistChecks {
    checker: ExistsChecker,
    /// Paths sent to an existence-check worker whose result has not arrived yet.
    in_flight: std::collections::HashSet<std::path::PathBuf>,
    exists_tx: std::sync::mpsc::Sender<(std::path::PathBuf, bool)>,
    exists_rx: std::sync::mpsc::Receiver<(std::path::PathBuf, bool)>,
    queue_tx: std::sync::mpsc::Sender<QueueCheckResult>,
    queue_rx: std::sync::mpsc::Receiver<QueueCheckResult>,
    /// Name of the playlist whose queue check is running; at most one runs at a time.
    queue_pending: Option<String>,
    /// Bumped by anything else that replaces the queue or starts playback, so a check that
    /// finishes afterwards is dropped instead of undoing it.
    queue_generation: u64,
    /// Set on shutdown: workers stop checking and send nothing more.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PlaylistChecks {
    fn new() -> Self {
        let (exists_tx, exists_rx) = std::sync::mpsc::channel();
        let (queue_tx, queue_rx) = std::sync::mpsc::channel();
        let checker: ExistsChecker = std::sync::Arc::new(playable_audio_file);
        Self {
            checker,
            in_flight: std::collections::HashSet::new(),
            exists_tx,
            exists_rx,
            queue_tx,
            queue_rx,
            queue_pending: None,
            queue_generation: 0,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// The first two folders of `path` (`/Volumes/Music/...` -> `/Volumes/Music`): the share a
/// playlist whose files are all missing most likely lives on. Pure: no file system access.
pub fn mount_hint(path: &std::path::Path) -> String {
    let mut hint = std::path::PathBuf::new();
    let mut folders = 0;
    for component in path.components() {
        if matches!(component, std::path::Component::Normal(_)) {
            if folders == 2 {
                break;
            }
            folders += 1;
        }
        hint.push(component);
    }
    hint.display().to_string()
}

/// Row text for a playlist entry whose file was not found: `<n>. ✗ missing: <label>  ----`.
pub fn missing_row_text(index: usize, entry: &crate::playlist::PlaylistEntry) -> String {
    format!("{}. ✗ missing: {}  ----", index + 1, entry.display_label())
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
    checks: PlaylistChecks,
    /// Folder listings for folder adds (`L` on a folder), sent by `playlist-folder` workers.
    folder_tx: mpsc::Sender<FolderListing>,
    folder_rx: mpsc::Receiver<FolderListing>,
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
        let (folder_tx, folder_rx) = mpsc::channel();
        Self {
            store: PlaylistStore::new(paths::get_playlists_dir(), persist),
            list_cursor: 0,
            track_cursor: 0,
            tracks_focus: false,
            modal: None,
            exists: HashMap::new(),
            checks: PlaylistChecks::new(),
            folder_tx,
            folder_rx,
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
        self.poll_playlist_folder_adds();
        self.poll_playlist_checks();
    }

    /// Keys while a playlist modal is open. Returns true when the app should quit.
    pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.player.stop()?;
            return Ok(true);
        }
        if matches!(self.playlists.modal, Some(PlaylistModal::AddPicker { .. })) {
            self.handle_add_picker_key(key);
            return Ok(false);
        }
        // Enter in a prompt opened from "+ New playlist…" creates the playlist and adds to it;
        // typing, Backspace and Esc stay with the shared name prompt handling below.
        if key.code == KeyCode::Enter
            && matches!(
                self.playlists.modal,
                Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(_),
                    ..
                })
            )
        {
            self.confirm_create_and_add();
            return Ok(false);
        }
        let Some(modal) = self.playlists.modal.take() else {
            return Ok(false);
        };
        match modal {
            // Like the startup-home editor: every character is text, q and digits included.
            PlaylistModal::NamePrompt {
                purpose,
                mut input,
                error,
            } => match key.code {
                KeyCode::Esc => self.status_message = "Cancelled".to_string(),
                KeyCode::Enter => self.submit_name_prompt(purpose, input),
                KeyCode::Backspace => {
                    input.pop();
                    self.playlists.modal = Some(PlaylistModal::NamePrompt {
                        purpose,
                        input,
                        error: None,
                    });
                }
                KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
                    input.push(c);
                    self.playlists.modal = Some(PlaylistModal::NamePrompt {
                        purpose,
                        input,
                        error: None,
                    });
                }
                _ => self.playlists.modal = Some(PlaylistModal::NamePrompt { purpose, input, error }),
            },
            // Only `y` deletes; any other key cancels.
            PlaylistModal::ConfirmDelete { name, .. } => {
                if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
                    self.delete_playlist(&name);
                } else {
                    self.status_message = format!("Kept \"{}\"", name);
                }
            }
            modal @ PlaylistModal::AddPicker { .. } => {
                if key.code != KeyCode::Esc {
                    self.playlists.modal = Some(modal);
                }
            }
        }
        Ok(false)
    }

    /// Tab 5 keys that the global hotkeys did not take.
    pub(super) fn handle_playlists_tab_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if self.handle_playlist_edit_key(code, modifiers) {
            return;
        }
        self.playlists.clamp_cursors();
        match (code, modifiers) {
            (KeyCode::Enter, _) if self.playlists.tracks_focus => {
                self.queue_selected_playlist(Some(self.playlists.track_cursor));
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                self.queue_selected_playlist(None);
            }
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
                let text = if ui.is_missing(&entry.path) {
                    missing_row_text(idx, entry)
                } else {
                    track_row_label(idx, entry)
                };
                ListItem::new(text).style(style)
            })
            .collect();
        ui.tracks_state.select(Some(ui.track_cursor));
        f.render_stateful_widget(List::new(rows).block(tracks_block), split[1], &mut ui.tracks_state);
    }

    pub(super) fn render_playlist_modal(&self, f: &mut Frame) {
        if let Some(PlaylistModal::AddPicker { pending, cursor }) = &self.playlists.modal {
            self.render_add_picker(f, pending, *cursor);
            return;
        }
        match &self.playlists.modal {
            Some(PlaylistModal::NamePrompt { purpose, input, error }) => {
                render_name_prompt(f, purpose, input, error.as_deref())
            }
            Some(PlaylistModal::ConfirmDelete { name, count }) => render_confirm_delete(f, name, *count),
            Some(PlaylistModal::AddPicker { pending, cursor }) => self.render_add_picker(f, pending, *cursor),
            None => {}
        }
    }
}

// Editing: name prompt (create / rename), delete confirmation, moving and removing tracks.
impl AppUI {
    /// Tab 5 editing keys. Returns true when the key was handled; anything else falls through
    /// to the navigation keys in `handle_playlists_tab_key`.
    fn handle_playlist_edit_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        let tracks_focus = self.playlists.tracks_focus;
        match (code, modifiers) {
            (KeyCode::Char('c'), KeyModifiers::NONE) if !tracks_focus => self.open_create_prompt(),
            (KeyCode::Char('R'), _) if !tracks_focus => self.open_rename_prompt(),
            (KeyCode::Char('D'), _) if !tracks_focus => self.open_delete_confirm(),
            (KeyCode::Char('K'), _) if tracks_focus => self.move_selected_track(crate::playlist_store::MoveDir::Up),
            (KeyCode::Char('J'), _) if tracks_focus => self.move_selected_track(crate::playlist_store::MoveDir::Down),
            (KeyCode::Char('d') | KeyCode::Char('x'), KeyModifiers::NONE) | (KeyCode::Delete, _) if tracks_focus => {
                self.remove_selected_track()
            }
            _ => return false,
        }
        true
    }

    fn open_create_prompt(&mut self) {
        // Creating before the startup load arrives would let it overwrite the files on disk.
        if !self.playlists.store.is_loaded() {
            self.status_message = crate::playlist_store::StoreError::NotLoaded.to_string();
            return;
        }
        self.playlists.modal = Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Create,
            input: String::new(),
            error: None,
        });
    }

    fn open_rename_prompt(&mut self) {
        let Some(old) = self.playlists.selected_name() else {
            return;
        };
        self.playlists.modal = Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::Rename { old: old.clone() },
            input: old,
            error: None,
        });
    }

    fn open_delete_confirm(&mut self) {
        let Some(name) = self.playlists.selected_name() else {
            return;
        };
        let count = self.playlists.store.get(&name).map_or(0, |p| p.entries.len());
        self.playlists.modal = Some(PlaylistModal::ConfirmDelete { name, count });
    }

    /// Apply a confirmed name prompt. An invalid or duplicate name puts the prompt back with
    /// the error so the user can fix it.
    fn submit_name_prompt(&mut self, purpose: NamePurpose, input: String) {
        let result = match &purpose {
            NamePurpose::Rename { old } => self.playlists.store.rename(old, &input),
            NamePurpose::Create | NamePurpose::CreateAndAdd(_) => self.playlists.store.create(&input),
        };
        let name = match result {
            Ok(name) => name,
            Err(e) => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose,
                    input,
                    error: Some(e.to_string()),
                });
                return;
            }
        };
        // The store re-sorts on create and rename: keep the cursor on the playlist just named.
        if let Some(index) = self.playlists.store.index_of(&name) {
            self.playlists.list_cursor = index;
        }
        match purpose {
            NamePurpose::Rename { old } => {
                self.follow_last_playlist(&old, Some(&name));
                self.status_message = if old == name {
                    "Name unchanged".to_string()
                } else {
                    format!("Renamed \"{}\" to \"{}\"", old, name)
                };
            }
            NamePurpose::Create | NamePurpose::CreateAndAdd(_) => {
                self.playlists.track_cursor = 0;
                self.status_message = format!("Created \"{}\"", name);
            }
        }
    }

    /// Keep `last_playlist` (the add picker's start row) pointing at a playlist that exists:
    /// `old` was renamed to `new`, or deleted when `new` is `None`.
    fn follow_last_playlist(&mut self, old: &str, new: Option<&str>) {
        let settings = &mut self.settings_mgr.settings;
        if !settings
            .last_playlist
            .as_deref()
            .is_some_and(|last| crate::playlist::names_equal(last, old))
        {
            return;
        }
        settings.last_playlist = new.map(str::to_string);
        self.settings_mgr.save();
    }

    fn delete_playlist(&mut self, name: &str) {
        match self.playlists.store.delete(name) {
            Ok(()) => {
                self.follow_last_playlist(name, None);
                // Stay on the same row, or move up to the new last row when the last one went.
                let len = self.playlists.store.playlists().len();
                self.playlists.list_cursor = self.playlists.list_cursor.min(len.saturating_sub(1));
                self.playlists.track_cursor = 0;
                self.status_message = format!("Deleted \"{}\"", name);
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }

    /// Move the selected track one row; the cursor stays on it. Moves do not wrap around.
    fn move_selected_track(&mut self, dir: crate::playlist_store::MoveDir) {
        let Some(name) = self.playlists.selected_name() else {
            return;
        };
        let len = self.playlists.store.get(&name).map_or(0, |p| p.entries.len());
        if self.playlists.track_cursor >= len {
            return;
        }
        match self.playlists.store.move_entry(&name, self.playlists.track_cursor, dir) {
            Ok(index) => self.playlists.track_cursor = index,
            Err(e) => self.status_message = e.to_string(),
        }
    }

    fn remove_selected_track(&mut self) {
        let Some(name) = self.playlists.selected_name() else {
            return;
        };
        let len = self.playlists.store.get(&name).map_or(0, |p| p.entries.len());
        if self.playlists.track_cursor >= len {
            return;
        }
        match self.playlists.store.remove(&name, self.playlists.track_cursor) {
            Ok(entry) => {
                let remaining = len - 1;
                self.playlists.track_cursor = self.playlists.track_cursor.min(remaining.saturating_sub(1));
                self.status_message = format!("Removed \"{}\" from {}", entry.display_label(), name);
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }
}

/// A centred box `height` rows tall and at most 64 columns wide for the playlist prompts.
fn playlist_prompt_rect(r: Rect, height: u16) -> Rect {
    let width = 64.min(r.width.saturating_sub(4));
    let height = height.min(r.height);
    let x = r.x + r.width.saturating_sub(width) / 2;
    let y = r.y + r.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn render_name_prompt(f: &mut Frame, purpose: &NamePurpose, input: &str, error: Option<&str>) {
    let area = playlist_prompt_rect(f.area(), 5);
    f.render_widget(Clear, area);

    let title = match purpose {
        NamePurpose::Create | NamePurpose::CreateAndAdd(_) => " NEW PLAYLIST ".to_string(),
        NamePurpose::Rename { old } => format!(" RENAME \"{}\" ", old),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            title,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    // The error replaces the key hint so the prompt keeps its height.
    let message = match error {
        Some(err) => Line::from(Span::styled(err.to_string(), Style::default().fg(Color::Red))),
        None => Line::from(Span::styled(
            "[Enter] Save   [Esc] Cancel",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("Name: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{}█", input), Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        message,
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_confirm_delete(f: &mut Frame, name: &str, count: usize) {
    let area = playlist_prompt_rect(f.area(), 5);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " DELETE PLAYLIST ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(Color::Black));
    let lines = vec![
        Line::from(Span::styled(
            format!("Delete \"{}\" ({})? y/n", name, track_count_label(count)),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[y] Delete   any other key: Cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The audio files directly inside `dir` (not recursive), sorted by name case-insensitively.
/// Blocking: call it on a worker thread, never on the UI thread.
pub fn list_folder_audio_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        // Skip hidden files, including the "._name" AppleDouble files macOS leaves on network shares.
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter(|entry| crate::metadata::is_supported_audio_file(&entry.path()))
        // Only files: a folder named `Live.flac` is not a track. A symlink counts when it
        // resolves to a file.
        .filter(|entry| match entry.file_type() {
            Ok(t) if t.is_file() => true,
            Ok(t) if t.is_symlink() => std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file()),
            _ => false,
        })
        .map(|entry| entry.path())
        .collect();
    files.sort_by_cached_key(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()));
    Ok(files)
}

/// Status for `L` on a browser row that is neither a track nor a folder (`★ Music Homes`, `..`).
const SELECT_TRACK_OR_FOLDER: &str = "Select a track or folder";

/// A folder listing for a folder add, sent from the worker to the UI thread.
#[derive(Debug)]
struct FolderListing {
    playlist: String,
    dir: PathBuf,
    result: std::io::Result<Vec<PathBuf>>,
}

impl PendingAdd {
    /// What the add picker's header shows: the track title, or `<folder> (folder)`.
    pub fn label(&self) -> &str {
        match self {
            PendingAdd::Entries { label, .. } | PendingAdd::Folder { label, .. } => label,
        }
    }
}

/// Pending add of one track; the picker header shows its title.
fn pending_track(entry: PlaylistEntry) -> PendingAdd {
    PendingAdd::Entries {
        label: entry.title.clone(),
        entries: vec![entry],
    }
}

/// Last path component of `dir` for messages, or the whole path for a root.
fn folder_display_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// Status line after an add: `Added "Kerala" to Road Trip (25 tracks)`, `Already in Road Trip`
/// or `Added 9 to Road Trip, skipped 3 already in it`.
fn add_result_message(
    playlist: &str,
    single_title: Option<&str>,
    added: usize,
    skipped: usize,
    total: usize,
) -> String {
    if added == 0 {
        format!("Already in {}", playlist)
    } else if let Some(title) = single_title {
        format!("Added \"{}\" to {} ({})", title, playlist, track_count_label(total))
    } else if skipped == 0 {
        format!("Added {} to {} ({})", added, playlist, track_count_label(total))
    } else {
        format!("Added {} to {}, skipped {} already in it", added, playlist, skipped)
    }
}

impl super::AppUI {
    /// `L`: open the add picker for the current selection. On tab 2 that is the focused pane's
    /// row (a browser track or folder, or a queue track); on every other tab it is the
    /// now-playing track.
    pub fn open_add_picker(&mut self) {
        if !self.playlists.store.is_loaded() {
            self.status_message = crate::playlist_store::StoreError::NotLoaded.to_string();
            return;
        }
        let pending = match self.pending_add_for_selection() {
            Ok(pending) => pending,
            Err(message) => {
                self.status_message = message;
                return;
            }
        };
        // Start on the playlist last added to; the first row if it is unset or gone.
        let cursor = self
            .settings_mgr
            .settings
            .last_playlist
            .as_deref()
            .and_then(|name| self.playlists.store.index_of(name))
            .unwrap_or(0);
        self.playlists.modal = Some(PlaylistModal::AddPicker { pending, cursor });
    }

    /// What `L` would add, or the status message explaining why nothing can be added.
    fn pending_add_for_selection(&self) -> Result<PendingAdd, String> {
        if self.active_tab == 1 && self.queue_focus {
            return match self.queue.tracks.get(self.queue_cursor) {
                Some(track) => Ok(pending_track(PlaylistEntry::from_metadata(track))),
                None => Err("Queue is empty".to_string()),
            };
        }
        if self.active_tab == 1 {
            let Some((label, path, is_dir)) = self.browser_entries.get(self.browser_cursor) else {
                return Err(SELECT_TRACK_OR_FOLDER.to_string());
            };
            if !*is_dir {
                return Ok(pending_track(self.playlist_entry_for_path(path)));
            }
            // Folder rows (also bookmarked Music Homes) are labelled "📁 name/"; the
            // "★ Music Homes" and ".." rows are navigation, not folders to add.
            if label.starts_with("📁 ") {
                return Ok(PendingAdd::Folder {
                    label: format!("{} (folder)", folder_display_name(path)),
                    dir: path.clone(),
                });
            }
            return Err(SELECT_TRACK_OR_FOLDER.to_string());
        }
        if self.player.is_playing() || self.player.is_paused() {
            if let Some(track) = self.queue.current_track() {
                return Ok(pending_track(PlaylistEntry::from_metadata(track)));
            }
        }
        Err("Nothing is playing".to_string())
    }

    /// Playlist entry for a file from metadata the app already holds. Never reads the file.
    fn playlist_entry_for_path(&self, path: &Path) -> PlaylistEntry {
        match self.metadata_cache.get(path) {
            Some(meta) => PlaylistEntry::from_metadata(&meta),
            None => PlaylistEntry::from_path(path),
        }
    }

    /// Keys while the add picker is open: j/k/arrows move (wrapping), Enter adds, Esc/q cancel.
    fn handle_add_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.playlists.modal = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_add_picker_cursor(false),
            KeyCode::Down | KeyCode::Char('j') => self.move_add_picker_cursor(true),
            KeyCode::Enter => self.confirm_add_picker(),
            _ => {}
        }
    }

    fn move_add_picker_cursor(&mut self, down: bool) {
        // One row per playlist, then "+ New playlist…".
        let rows = self.playlists.store.playlists().len() + 1;
        if let Some(PlaylistModal::AddPicker { cursor, .. }) = self.playlists.modal.as_mut() {
            *cursor = if down {
                (*cursor + 1) % rows
            } else {
                (*cursor + rows - 1) % rows
            };
        }
    }

    /// Enter in the add picker: add to the highlighted playlist, or open the name prompt on
    /// "+ New playlist…".
    fn confirm_add_picker(&mut self) {
        let (pending, cursor) = match self.playlists.modal.take() {
            Some(PlaylistModal::AddPicker { pending, cursor }) => (pending, cursor),
            other => {
                self.playlists.modal = other;
                return;
            }
        };
        match self.playlists.store.playlists().get(cursor).map(|p| p.name.clone()) {
            Some(name) => self.apply_pending_add(&name, pending),
            None => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(pending),
                    input: String::new(),
                    error: None,
                });
            }
        }
    }

    /// Enter in the name prompt opened from "+ New playlist…": create the playlist, then add.
    fn confirm_create_and_add(&mut self) {
        let (pending, input) = match self.playlists.modal.take() {
            Some(PlaylistModal::NamePrompt {
                purpose: NamePurpose::CreateAndAdd(pending),
                input,
                ..
            }) => (pending, input),
            other => {
                self.playlists.modal = other;
                return;
            }
        };
        match self.playlists.store.create(&input) {
            Ok(name) => {
                // Tab 5's list cursor follows the new playlist to its sorted position.
                if let Some(index) = self.playlists.store.index_of(&name) {
                    self.playlists.list_cursor = index;
                    self.playlists.track_cursor = 0;
                }
                self.apply_pending_add(&name, pending);
            }
            // Invalid or duplicate name: keep the prompt open with the error.
            Err(e) => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(pending),
                    input,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    fn apply_pending_add(&mut self, playlist: &str, pending: PendingAdd) {
        match pending {
            PendingAdd::Entries { entries, .. } => self.add_entries_to_playlist(playlist, entries),
            PendingAdd::Folder { dir, .. } => self.start_folder_add(playlist, dir),
        }
    }

    /// Add `entries` to `playlist`, report the result and remember the playlist for the picker.
    fn add_entries_to_playlist(&mut self, playlist: &str, entries: Vec<PlaylistEntry>) {
        let single_title = match entries.as_slice() {
            [only] => Some(only.title.clone()),
            _ => None,
        };
        match self.playlists.store.add(playlist, entries) {
            Ok(crate::playlist_store::AddResult { added, skipped, .. }) => {
                let (stored, total) = self
                    .playlists
                    .store
                    .get(playlist)
                    .map(|p| (p.name.clone(), p.entries.len()))
                    .unwrap_or_else(|| (playlist.to_string(), 0));
                self.status_message = add_result_message(&stored, single_title.as_deref(), added, skipped, total);
                self.settings_mgr.settings.last_playlist = Some(stored);
                self.settings_mgr.save();
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }

    /// List `dir` on a worker (it may be on a slow or unmounted share);
    /// `poll_playlist_folder_adds` adds the files when the listing arrives.
    fn start_folder_add(&mut self, playlist: &str, dir: PathBuf) {
        self.status_message = format!("Reading {}…", folder_display_name(&dir));
        let tx = self.playlists.folder_tx.clone();
        let playlist = playlist.to_string();
        self.workers.spawn("playlist-folder", move || {
            let result = list_folder_audio_files(&dir);
            let _ = tx.send(FolderListing { playlist, dir, result });
        });
    }

    /// Apply finished folder listings from `start_folder_add`.
    fn poll_playlist_folder_adds(&mut self) {
        while let Ok(listing) = self.playlists.folder_rx.try_recv() {
            let folder = folder_display_name(&listing.dir);
            match listing.result {
                Ok(files) if files.is_empty() => {
                    self.status_message = format!("No audio files in {}", folder);
                }
                Ok(files) => {
                    let entries = files.iter().map(|path| self.playlist_entry_for_path(path)).collect();
                    self.add_entries_to_playlist(&listing.playlist, entries);
                }
                Err(e) => {
                    crate::log_warn!("playlists", "Couldn't read {}: {}", listing.dir.display(), e);
                    self.status_message = format!("Couldn't read {}", folder);
                }
            }
        }
    }

    /// The add picker: what is being added, one row per playlist, then "+ New playlist…".
    fn render_add_picker(&self, f: &mut Frame, pending: &PendingAdd, cursor: usize) {
        let area = super::centered_rect(60, 60, f.area());
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " ADD TO PLAYLIST ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " [Enter] Add   [j/k] Move   [Esc] Cancel ",
                Style::default().fg(Color::DarkGray),
            ))
            .border_style(Style::default().fg(Color::Cyan))
            .style(Style::default().bg(Color::Black));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(inner);
        let header = Paragraph::new(Line::from(vec![
            Span::styled("Adding: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                pending.label().to_string(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]));
        f.render_widget(header, chunks[0]);

        let mut items: Vec<ListItem> = self
            .playlists
            .store
            .playlists()
            .iter()
            .map(|p| ListItem::new(format!("{}  ({})", p.name, track_count_label(p.entries.len()))))
            .collect();
        items.push(ListItem::new(Span::styled(
            NEW_PLAYLIST_ROW,
            Style::default().fg(Color::Green),
        )));
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = ListState::default();
        state.select(Some(cursor));
        f.render_stateful_widget(list, chunks[1], &mut state);
    }
}

impl PlaylistsUi {
    /// Replace the existence test (tests use a blocking one to stand in for a hung share).
    #[doc(hidden)]
    pub fn set_exists_checker(&mut self, checker: ExistsChecker) {
        self.checks.checker = checker;
    }

    /// Whether a check this session found `path` missing. Paths not checked yet are not missing.
    pub fn is_missing(&self, path: &std::path::Path) -> bool {
        self.exists.get(path) == Some(&false)
    }

    /// Number of paths whose existence check is still running.
    pub fn checks_in_flight(&self) -> usize {
        self.checks.in_flight.len()
    }

    /// Whether a play/queue request is still checking its files.
    pub fn queue_check_pending(&self) -> bool {
        self.checks.queue_pending.is_some()
    }

    /// Tell playlist workers to stop checking (shutdown). A check already stuck in a hung `stat`
    /// is detached by the bounded worker join.
    pub fn stop_checks(&self) {
        self.checks.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The path to queue for a playlist entry: the entry resolved (through any symlinks) to a
/// regular file with an audio extension, or `None`. Callers queue the returned path, never the
/// entry's own, so a `song.mp3` symlink swapped after the check can't point the stream server
/// at some other file. Runs on a worker; it touches the disk.
pub fn playable_audio_file(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let resolved = std::fs::canonicalize(path).ok()?;
    (resolved.is_file() && crate::metadata::is_supported_audio_file(&resolved)).then_some(resolved)
}

impl AppUI {
    /// Check, on a worker, the entries of the playlist shown in the tracks pane that were not
    /// checked this session and are not being checked now. Called on every poll; never stats on
    /// the UI thread.
    pub fn request_playlist_exists_checks(&mut self) {
        if self.active_tab != 4 {
            return;
        }
        let ui = &mut self.playlists;
        let Some(playlist) = ui.store.playlists().get(ui.list_cursor) else {
            return;
        };
        let mut paths = Vec::new();
        for entry in &playlist.entries {
            // Called every tick: look up before cloning, so a fully checked playlist allocates
            // nothing.
            if ui.exists.contains_key(&entry.path) || ui.checks.in_flight.contains(&entry.path) {
                continue;
            }
            ui.checks.in_flight.insert(entry.path.clone());
            paths.push(entry.path.clone());
        }
        if paths.is_empty() {
            return;
        }
        let checker = ui.checks.checker.clone();
        let tx = ui.checks.exists_tx.clone();
        let stop = ui.checks.stop.clone();
        // Results are sent one by one, so rows turn into `✗ missing` as each check finishes.
        self.workers.spawn("playlist-exists", move || {
            for path in paths {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let exists = checker(path.as_path()).is_some();
                if tx.send((path, exists)).is_err() {
                    break;
                }
            }
        });
    }

    /// Apply finished existence checks, then start checks for anything newly shown.
    fn poll_playlist_checks(&mut self) {
        while let Ok((path, exists)) = self.playlists.checks.exists_rx.try_recv() {
            self.playlists.checks.in_flight.remove(&path);
            self.playlists.exists.insert(path, exists);
        }
        while let Ok(result) = self.playlists.checks.queue_rx.try_recv() {
            self.apply_playlist_queue(result);
        }
        self.request_playlist_exists_checks();
    }

    /// `Enter` in the tracks pane (`play_from` = selected track) or `a` (`None`): check the
    /// selected playlist's files on a worker, then replace the queue and play, or append.
    pub fn queue_selected_playlist(&mut self, play_from: Option<usize>) {
        if !self.playlists.store.is_loaded() {
            self.status_message = crate::playlist_store::StoreError::NotLoaded.to_string();
            return;
        }
        let Some(playlist) = self.playlists.store.playlists().get(self.playlists.list_cursor) else {
            self.status_message = "No playlist selected".to_string();
            return;
        };
        if playlist.entries.is_empty() {
            self.status_message = format!("{} is empty", playlist.name);
            return;
        }
        // One request at a time: a hung share would otherwise pile up workers, and each result
        // would replace the queue again whenever it arrived.
        if let Some(pending) = &self.playlists.checks.queue_pending {
            self.status_message = format!("Still checking {}…", pending);
            return;
        }
        let name = playlist.name.clone();
        let entries = playlist.entries.clone();
        self.status_message = format!("Checking {} tracks in {}…", entries.len(), name);
        self.playlists.checks.queue_pending = Some(name.clone());
        let generation = self.playlists.checks.queue_generation;
        let checker = self.playlists.checks.checker.clone();
        let tx = self.playlists.checks.queue_tx.clone();
        let stop = self.playlists.checks.stop.clone();
        self.workers.spawn("playlist-queue", move || {
            let mut resolved = Vec::with_capacity(entries.len());
            for entry in &entries {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                resolved.push(checker(entry.path.as_path()));
            }
            let _ = tx.send(QueueCheckResult {
                name,
                entries,
                resolved,
                play_from,
                generation,
            });
        });
    }

    /// Queue the files of a checked playlist that exist. Replace (`play_from` set) only clears
    /// the queue once something can be queued, so an all-missing playlist leaves it as it was.
    fn apply_playlist_queue(&mut self, result: QueueCheckResult) {
        self.playlists.checks.queue_pending = None;
        // Fresh results update the session cache; they never change the playlist or its file.
        for (entry, resolved) in result.entries.iter().zip(&result.resolved) {
            self.playlists.exists.insert(entry.path.clone(), resolved.is_some());
        }
        if result.generation != self.playlists.checks.queue_generation {
            // The queue was replaced or playback started while this check ran.
            return;
        }
        // Queue the checked paths only: the queue and the /stream registry never see an entry's
        // own path, which may be a symlink swapped since the check.
        let mut originals = HashMap::new();
        let mut checked = Vec::with_capacity(result.entries.len());
        let mut exists = Vec::with_capacity(result.entries.len());
        for (entry, resolved) in result.entries.iter().zip(&result.resolved) {
            let mut entry = entry.clone();
            if let Some(resolved) = resolved {
                originals.insert(resolved.clone(), entry.path.clone());
                entry.path = resolved.clone();
            }
            exists.push(resolved.is_some());
            checked.push(entry);
        }
        let cached = |path: &std::path::Path| {
            let original = originals.get(path).map(|p| p.as_path());
            self.metadata_cache
                .get(path)
                .or_else(|| original.and_then(|o| self.metadata_cache.get(o)))
                .map(|mut meta| {
                    meta.filepath = path.to_path_buf();
                    meta
                })
        };
        let crate::playlist::QueuePlan {
            tracks,
            start_index,
            missing,
        } = crate::playlist::plan_queue(&checked, &exists, result.play_from, &cached);
        if tracks.is_empty() {
            let hint = result.entries.first().map(|e| mount_hint(&e.path)).unwrap_or_default();
            self.status_message = format!(
                "None of {}'s {} tracks found. Is {} mounted?",
                result.name,
                result.entries.len(),
                hint
            );
            return;
        }
        let queued = tracks.len();
        // enqueue_tracks starts the background tag fetch that fills in placeholder entries.
        // play_current_track returns its "Playback error: ..." when the player fails to start.
        // The status keeps that error and puts the queue summary after it, so a failed start is
        // never reported as plain success.
        let playback_error = match (result.play_from, start_index) {
            (Some(_), Some(start)) => {
                self.clear_queue_for_playlist();
                self.enqueue_tracks(tracks);
                self.queue.current_index = start;
                self.queue_cursor = start;
                self.play_current_track()
            }
            _ => {
                self.enqueue_tracks(tracks);
                None
            }
        };
        let summary = if missing == 0 {
            format!("Queued {} tracks from {}", queued, result.name)
        } else {
            format!("Queued {} tracks from {}, {} missing", queued, result.name, missing)
        };
        self.status_message = match playback_error {
            Some(err) => format!("{}. {}", err, summary),
            None => summary,
        };
        self.update_web_state();
    }

    /// Drop the result of any playlist play/queue check still running: called by every other
    /// action that replaces the queue or starts playback.
    pub fn supersede_playlist_queue(&mut self) {
        self.playlists.checks.queue_generation = self.playlists.checks.queue_generation.wrapping_add(1);
    }

    /// Before a playlist replaces the queue: stop any autopilot scan, end any continuous DLNA
    /// stream the way autopilot does before its own fresh start, stop playback, empty the queue.
    fn clear_queue_for_playlist(&mut self) {
        if let Some(stop) = self.autopilot_scan_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.autopilot_rx = None;
        self.is_autopilot_scanning = false;
        // The queue is now the playlist, not an autopilot mix (web dashboard badge).
        self.autopilot_dir = None;
        if let Some(ref ws) = self.web_server {
            ws.close_active_streams();
            *ws.stream_state().continuous.current_track_index.lock().unwrap() = 0;
            *ws.stream_state().continuous.current_position.lock().unwrap() = 0.0;
        }
        let _ = self.player.stop();
        self.queue.clear();
        self.queue_cursor = 0;
    }
}
