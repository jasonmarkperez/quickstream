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
use crate::playlist_store::{LoadResult, PlaylistStore, WriteFailure};

/// Last row of the add picker: create a playlist and add to it.
pub const NEW_PLAYLIST_ROW: &str = "+ New playlist…";

/// Status after the startup load failed. Edits stay refused for the session.
const LOAD_FAILED: &str = "Couldn't load playlists";

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
    /// `resolved` says which files exist (tab 5), rather than which the web may queue.
    records_exists: bool,
}

/// `Enter`: the entry to start playing passed its check before the rest of the playlist was
/// checked, so playback can start now and the rest of the queue fill in later.
struct QueueStart {
    name: String,
    entry: crate::playlist::PlaylistEntry,
    /// The checked (canonical) path to queue.
    resolved: std::path::PathBuf,
    generation: u64,
}

/// What a `playlist-queue` worker sends: for `Enter`, the track to start on as soon as it is
/// found, then (for `Enter` and `a`) every entry's check.
enum QueueCheckUpdate {
    Start(QueueStart),
    Done(QueueCheckResult),
}

/// The track an `Enter` started before the rest of its playlist was checked.
struct StartedPlay {
    /// Its queued (checked) path, to find it in the queue when the rest arrives.
    path: std::path::PathBuf,
    /// `play_current_track`'s error, kept for the final status.
    playback_error: Option<String>,
}

/// Background existence checks for tab 5. Files are only checked on workers: a `stat` on an
/// unmounted share can hang, and the UI must keep drawing.
struct PlaylistChecks {
    checker: ExistsChecker,
    /// Paths sent to an existence-check worker whose result has not arrived yet.
    in_flight: std::collections::HashSet<std::path::PathBuf>,
    exists_tx: std::sync::mpsc::Sender<(std::path::PathBuf, bool)>,
    exists_rx: std::sync::mpsc::Receiver<(std::path::PathBuf, bool)>,
    queue_tx: std::sync::mpsc::Sender<QueueCheckUpdate>,
    queue_rx: std::sync::mpsc::Receiver<QueueCheckUpdate>,
    /// Name of the playlist whose queue check is running; at most one runs at a time.
    queue_pending: Option<String>,
    /// Set while the track an `Enter` started plays and the rest of its playlist is checked.
    started: Option<StartedPlay>,
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
            started: None,
            queue_generation: 0,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// The first two folders above the file at `path` (`/Volumes/Music/...` -> `/Volumes/Music`):
/// the share a playlist whose files are all missing most likely lives on. `None` when the file
/// sits fewer than two folders deep (`/ghost.mp3`, `/Volumes/ghost.mp3`): a top-level folder
/// such as `/Volumes` or `/mnt` holds mounts but is not one, so there is no share to name.
/// Pure: no file system access.
pub fn mount_hint(path: &std::path::Path) -> Option<String> {
    let parent = path.parent()?;
    let mut hint = std::path::PathBuf::new();
    let mut folders = 0;
    for component in parent.components() {
        if matches!(component, std::path::Component::Normal(_)) {
            if folders == 2 {
                break;
            }
            folders += 1;
        }
        hint.push(component);
    }
    (folders == 2).then(|| hint.display().to_string())
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
    /// Bumped when a check changes `exists`, so the web snapshot shows the new marks.
    exists_version: u64,
    /// What the last web snapshot was built from; `None` before the first.
    web_published: Option<WebSnapshotKey>,
    checks: PlaylistChecks,
    /// Folder listings for folder adds (`L` on a folder), sent by `playlist-folder` workers.
    folder_tx: mpsc::Sender<FolderUpdate>,
    folder_rx: mpsc::Receiver<FolderUpdate>,
    /// Result of the startup load (playlists, stems of skipped files); `None` once applied.
    load_rx: Option<mpsc::Receiver<LoadResult>>,
    /// The playlists folder couldn't be listed, or the load worker died without sending; the
    /// store stays unloaded so no edit can overwrite a file that was never read.
    load_failed: bool,
    list_state: ListState,
    tracks_state: ListState,
    /// Rows each pane showed at the last render: how far PageUp / PageDown move.
    pub list_page_height: usize,
    pub tracks_page_height: usize,
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
            exists_version: 0,
            web_published: None,
            checks: PlaylistChecks::new(),
            folder_tx,
            folder_rx,
            load_rx: None,
            load_failed: false,
            list_state: ListState::default(),
            tracks_state: ListState::default(),
            list_page_height: 10,
            tracks_page_height: 10,
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

    /// True when the startup load failed; edits stay refused for the session.
    pub fn load_failed(&self) -> bool {
        self.load_failed
    }

    /// Why an edit is refused before the store is loaded: still loading, or the load failed.
    pub fn not_loaded_message(&self) -> String {
        if self.load_failed {
            LOAD_FAILED.to_string()
        } else {
            crate::playlist_store::StoreError::NotLoaded.to_string()
        }
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

    /// Put the focused pane's cursor on `row(len)`, clamped to the list (`g`/`G`, PageUp/PageDown:
    /// no wraparound).
    fn set_focused_cursor(&mut self, row: impl Fn(usize, usize) -> usize) {
        if self.tracks_focus {
            let tracks = self.selected_playlist().map_or(0, |p| p.entries.len());
            self.track_cursor = row(self.track_cursor, tracks).min(tracks.saturating_sub(1));
        } else {
            let playlists = self.store.playlists().len();
            let before = self.list_cursor;
            self.list_cursor = row(self.list_cursor, playlists).min(playlists.saturating_sub(1));
            if self.list_cursor != before {
                self.track_cursor = 0;
            }
        }
    }

    /// `g` / `G` (and Home / End): first or last row of the focused pane.
    fn jump_cursor(&mut self, to_end: bool) {
        self.set_focused_cursor(|_, len| if to_end { len.saturating_sub(1) } else { 0 });
    }

    /// PageUp / PageDown: move the focused pane's cursor by the rows it showed at the last
    /// render, stopping at the first or last row.
    fn page_cursor(&mut self, down: bool) {
        let page = if self.tracks_focus {
            self.tracks_page_height
        } else {
            self.list_page_height
        }
        .max(1);
        self.set_focused_cursor(|cursor, _| {
            if down {
                cursor.saturating_add(page)
            } else {
                cursor.saturating_sub(page)
            }
        });
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
                Ok(Ok((playlists, skipped))) => {
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
                // The store stays not loaded, so no edit can write over files it never read.
                Ok(Err(e)) => {
                    crate::log_error!("playlists", "Couldn't list the playlists folder: {}", e);
                    self.playlists.load_rx = None;
                    self.playlists.load_failed = true;
                    self.status_message = LOAD_FAILED.to_string();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    crate::log_error!("playlists", "Playlist load worker exited without a result");
                    self.playlists.load_rx = None;
                    self.playlists.load_failed = true;
                    self.status_message = LOAD_FAILED.to_string();
                }
            }
        }
        self.apply_playlist_write_failures();
        self.playlists.clamp_cursors();
        self.poll_playlist_folder_adds();
        self.poll_playlist_checks();
    }

    /// Show what the background writer couldn't do (a create that found a file in its way, a
    /// rename that failed) once the store has undone it in memory. The cursor stays on the
    /// playlist it was on, under its restored name after a failed rename, and `last_playlist`
    /// follows the same way.
    fn apply_playlist_write_failures(&mut self) {
        if !self.playlists.store.has_write_failures() {
            return;
        }
        // Before the store re-sorts: afterwards the cursor's row may hold another playlist.
        let mut selected = self.playlists.selected_name();
        let failures = self.playlists.store.poll_write_failures();
        for failure in failures {
            match &failure {
                WriteFailure::CreateUndone { name, .. } => self.follow_last_playlist(name, None),
                WriteFailure::RenameUndone {
                    attempted, restored, ..
                } => {
                    self.follow_last_playlist(attempted, Some(restored));
                    if selected.as_deref() == Some(attempted.as_str()) {
                        selected = Some(restored.clone());
                    }
                }
            }
            self.status_message = failure.message().to_string();
        }
        if let Some(index) = selected.and_then(|name| self.playlists.store.index_of(&name)) {
            self.playlists.list_cursor = index;
        }
    }

    /// Keys while a playlist modal is open. Returns true when the app should quit.
    pub fn handle_playlist_modal_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.player.stop()?;
            return Ok(true);
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
                self.playlists.modal = Some(modal);
                self.handle_add_picker_key(key);
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
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => self.playlists.jump_cursor(false),
            (KeyCode::End, _) | (KeyCode::Char('G'), _) => self.playlists.jump_cursor(true),
            (KeyCode::PageUp, _) => self.playlists.page_cursor(false),
            (KeyCode::PageDown, _) => self.playlists.page_cursor(true),
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
        // Rows inside each pane's borders: one page for PageUp / PageDown.
        ui.list_page_height = usize::from(split[0].height.saturating_sub(2)).max(1);
        ui.tracks_page_height = usize::from(split[1].height.saturating_sub(2)).max(1);
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
            self.status_message = self.playlists.not_loaded_message();
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

    /// Apply a confirmed name prompt: rename, create, or (from the add picker's
    /// "+ New playlist…") create and then add. An invalid or duplicate name puts the prompt back
    /// with the error so the user can fix it.
    fn submit_name_prompt(&mut self, purpose: NamePurpose, input: String) {
        let result = match &purpose {
            NamePurpose::Rename { old } => self.rename_playlist(old, &input),
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
            NamePurpose::Rename { old } => self.status_message = renamed_message(&old, &name),
            NamePurpose::Create => {
                self.playlists.track_cursor = 0;
                self.status_message = created_message(&name);
            }
            NamePurpose::CreateAndAdd(pending) => {
                self.playlists.track_cursor = 0;
                self.apply_pending_add(&name, pending, true);
            }
        }
    }

    /// Rename playlist `old` (its stored name) to `new` and keep `last_playlist` on it. Returns
    /// the stored new name.
    fn rename_playlist(&mut self, old: &str, new: &str) -> Result<String, crate::playlist_store::StoreError> {
        let name = self.playlists.store.rename(old, new)?;
        self.follow_last_playlist(old, Some(&name));
        Ok(name)
    }

    /// Delete playlist `name` (matched ignoring case) and its file, and clear `last_playlist`
    /// if it named it. Returns the status message.
    fn delete_playlist_named(&mut self, name: &str) -> Result<String, crate::playlist_store::StoreError> {
        let stored = self.stored_playlist_name(name)?;
        self.playlists.store.delete(&stored)?;
        self.follow_last_playlist(&stored, None);
        Ok(format!("Deleted \"{}\"", stored))
    }

    /// Remove track `index` of playlist `name`. Returns the status message.
    fn remove_playlist_track(&mut self, name: &str, index: usize) -> Result<String, crate::playlist_store::StoreError> {
        let stored = self.stored_playlist_name(name)?;
        let entry = self.playlists.store.remove(&stored, index)?;
        Ok(format!("Removed \"{}\" from {}", entry.display_label(), stored))
    }

    /// The name playlist `name` (matched ignoring case) is stored under.
    fn stored_playlist_name(&self, name: &str) -> Result<String, crate::playlist_store::StoreError> {
        use crate::playlist_store::StoreError;
        if !self.playlists.store.is_loaded() {
            return Err(StoreError::NotLoaded);
        }
        self.playlists
            .store
            .get(name)
            .map(|playlist| playlist.name.clone())
            .ok_or_else(|| StoreError::NotFound(name.to_string()))
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
        match self.delete_playlist_named(name) {
            Ok(message) => {
                // Stay on the same row, or move up to the new last row when the last one went.
                let len = self.playlists.store.playlists().len();
                self.playlists.list_cursor = self.playlists.list_cursor.min(len.saturating_sub(1));
                self.playlists.track_cursor = 0;
                self.status_message = message;
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
        match self.remove_playlist_track(&name, self.playlists.track_cursor) {
            Ok(message) => {
                let remaining = len - 1;
                self.playlists.track_cursor = self.playlists.track_cursor.min(remaining.saturating_sub(1));
                self.status_message = message;
            }
            Err(e) => self.status_message = e.to_string(),
        }
    }
}

/// Status after a create.
fn created_message(name: &str) -> String {
    format!("Created \"{}\"", name)
}

/// Status after a rename from `old` to `new`.
fn renamed_message(old: &str, new: &str) -> String {
    if old == new {
        "Name unchanged".to_string()
    } else {
        format!("Renamed \"{}\" to \"{}\"", old, new)
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
    // Scroll long names horizontally: the field shows the end of the name, where typing
    // happens, and the cursor block (issue #52).
    let label = "Name: ";
    let field_width = (area.width as usize)
        .saturating_sub(2) // borders
        .saturating_sub(label.len())
        .saturating_sub(1); // cursor
    let lines = vec![
        Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{}█", visible_tail(input, field_width)),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(""),
        message,
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Columns `c` takes in the terminal (2 for wide characters such as CJK).
fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// `text` if it fits in `width` columns; otherwise `…` followed by as much of its end as fits.
fn visible_tail(text: &str, width: usize) -> String {
    if Span::raw(text).width() <= width {
        return text.to_string();
    }
    let mut used = 1; // the `…`
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        used += char_width(c);
        if used > width {
            break;
        }
        start = index;
    }
    format!("…{}", &text[start..])
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

/// `text` with its first character uppercased, to start a status line.
fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Status for `L` on a browser row that is neither a track nor a folder (`★ Music Homes`, `..`).
const SELECT_TRACK_OR_FOLDER: &str = "Select a track or folder";

/// A folder listing for a folder add, sent from the worker to the UI thread.
#[derive(Debug)]
struct FolderListing {
    /// Id of the target playlist ([`crate::playlist_store::PlaylistStore::id_of`]), so the add
    /// follows a rename made while the tags were read (issue #52).
    target: u64,
    /// The target's name when the add started, for the message if it was deleted meanwhile.
    playlist: String,
    /// The playlist was created for this add ("+ New playlist…").
    created: bool,
    dir: PathBuf,
    result: std::io::Result<Vec<PathBuf>>,
}

/// What a `playlist-folder` worker sends: tag-reading progress, then the listing.
#[derive(Debug)]
enum FolderUpdate {
    Progress { dir: PathBuf, done: usize, total: usize },
    Listing(FolderListing),
}

/// A cached entry with no duration is a placeholder (a failed or skipped tag read), so it is
/// read again, the same rule the queue's metadata fetch uses.
fn needs_tag_read(cache: &crate::library::MetadataCache, path: &Path) -> bool {
    cache.get(path).is_none_or(|meta| meta.duration_seconds <= 0.0)
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
            self.status_message = self.playlists.not_loaded_message();
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
            Some(name) => self.apply_pending_add(&name, pending, false),
            None => {
                self.playlists.modal = Some(PlaylistModal::NamePrompt {
                    purpose: NamePurpose::CreateAndAdd(pending),
                    input: String::new(),
                    error: None,
                });
            }
        }
    }

    /// Add `pending` to `playlist`. `created` is true when the name prompt just created the
    /// playlist for this add, so a folder that adds nothing still reports the new playlist.
    fn apply_pending_add(&mut self, playlist: &str, pending: PendingAdd, created: bool) {
        match pending {
            // The status shows the outcome.
            PendingAdd::Entries { entries, .. } => {
                let _ = self.add_entries_to_playlist(playlist, entries);
            }
            PendingAdd::Folder { dir, .. } => self.start_folder_add(playlist, dir, created),
        }
    }

    /// Add `entries` to `playlist`, report the result and remember the playlist for the picker.
    /// Returns the status message it set, as `Err` when nothing could be added.
    fn add_entries_to_playlist(&mut self, playlist: &str, entries: Vec<PlaylistEntry>) -> Result<String, String> {
        let single_title = match entries.as_slice() {
            [only] => Some(only.title.clone()),
            _ => None,
        };
        let result = match self.playlists.store.add(playlist, entries) {
            Ok(crate::playlist_store::AddResult { added, skipped, .. }) => {
                let (stored, total) = self
                    .playlists
                    .store
                    .get(playlist)
                    .map(|p| (p.name.clone(), p.entries.len()))
                    .unwrap_or_else(|| (playlist.to_string(), 0));
                let message = add_result_message(&stored, single_title.as_deref(), added, skipped, total);
                self.settings_mgr.settings.last_playlist = Some(stored);
                self.settings_mgr.save();
                Ok(message)
            }
            Err(e) => Err(e.to_string()),
        };
        self.status_message = match &result {
            Ok(message) | Err(message) => message.clone(),
        };
        result
    }

    /// List `dir` and read the tags of files the metadata cache has no real entry for, on a
    /// worker (it may be on a slow or unmounted share). Tags go through the shared tag reads
    /// (`super::tag_reads`): saved to the on-disk index, put in the cache and sent on
    /// `metadata_tx`, so browser labels and queue rows refresh and `poll_playlist_folder_adds`
    /// builds the entries from the cache when the listing arrives. A file another worker is
    /// reading is not read again: the folder add waits for that read and uses its tags. The
    /// worker stops, adding nothing, once shutdown is signalled.
    fn start_folder_add(&mut self, playlist: &str, dir: PathBuf, created: bool) {
        let Some(target) = self.playlists.store.id_of(playlist) else {
            self.status_message = crate::playlist_store::StoreError::NotFound(playlist.to_string()).to_string();
            return;
        };
        self.status_message = format!("Reading {}…", folder_display_name(&dir));
        let tx = self.playlists.folder_tx.clone();
        let tag_reads = self.tag_reads.clone();
        let cache = self.metadata_cache.clone();
        let stop = self.playlists.checks.stop.clone();
        let playlist = playlist.to_string();
        self.workers.spawn("playlist-folder", move || {
            let stopped = || stop.load(std::sync::atomic::Ordering::Relaxed);
            let result = list_folder_audio_files(&dir);
            if let Ok(files) = &result {
                let to_read: Vec<&PathBuf> = files.iter().filter(|path| needs_tag_read(&cache, path)).collect();
                let total = to_read.len();
                for (done, path) in to_read.into_iter().enumerate() {
                    if stopped() {
                        return;
                    }
                    let _ = tx.send(FolderUpdate::Progress {
                        dir: dir.clone(),
                        done,
                        total,
                    });
                    if tag_reads.read_or_wait(path, &stop) == super::tag_reads::TagRead::Stopped {
                        return;
                    }
                }
            }
            if !stopped() {
                let _ = tx.send(FolderUpdate::Listing(FolderListing {
                    target,
                    playlist,
                    created,
                    dir,
                    result,
                }));
            }
        });
    }

    /// Apply finished folder listings from `start_folder_add`, to the target playlist under
    /// its current name. A target deleted while the tags were read gets nothing.
    fn poll_playlist_folder_adds(&mut self) {
        while let Ok(update) = self.playlists.folder_rx.try_recv() {
            let listing = match update {
                FolderUpdate::Progress { dir, done, total } => {
                    self.status_message = format!("Reading tags in {}… {}/{}", folder_display_name(&dir), done, total);
                    continue;
                }
                FolderUpdate::Listing(listing) => listing,
            };
            let Some(playlist) = self.playlists.store.name_of(listing.target).map(str::to_string) else {
                self.status_message = format!("{} was deleted; nothing added", listing.playlist);
                continue;
            };
            let folder = folder_display_name(&listing.dir);
            let problem = match listing.result {
                Ok(files) if files.is_empty() => format!("no audio files in {}", folder),
                Ok(files) => {
                    let entries = files.iter().map(|path| self.playlist_entry_for_path(path)).collect();
                    let _ = self.add_entries_to_playlist(&playlist, entries);
                    continue;
                }
                Err(e) => {
                    crate::log_warn!("playlists", "Couldn't read {}: {}", listing.dir.display(), e);
                    format!("couldn't read {}", folder)
                }
            };
            // "+ New playlist…" creates the playlist before the folder is listed, so the name
            // prompt can still show a name error. When the folder then adds nothing, the status
            // says the playlist was created anyway.
            self.status_message = if listing.created {
                format!("Created \"{}\"; {}", playlist, problem)
            } else {
                capitalize_first(&problem)
            };
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

    /// Cache a check's result for the session.
    fn record_exists(&mut self, path: PathBuf, exists: bool) {
        if self.exists.insert(path, exists) != Some(exists) {
            self.exists_version = self.exists_version.wrapping_add(1);
        }
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
        let cursor = self.playlists.list_cursor;
        self.check_playlist_entries(cursor..cursor + 1);
    }

    /// Start existence checks, on one worker, for the entries of the playlists at `range` (store
    /// positions) that were not checked this session and are not being checked now.
    fn check_playlist_entries(&mut self, range: std::ops::Range<usize>) {
        let ui = &mut self.playlists;
        let Some(playlists) = ui.store.playlists().get(range) else {
            return;
        };
        let mut paths = Vec::new();
        for entry in playlists.iter().flat_map(|p| &p.entries) {
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
            self.playlists.record_exists(path, exists);
        }
        while let Ok(update) = self.playlists.checks.queue_rx.try_recv() {
            match update {
                QueueCheckUpdate::Start(start) => self.start_playlist_play(start),
                QueueCheckUpdate::Done(result) => self.apply_playlist_queue(result),
            }
        }
        self.request_playlist_exists_checks();
    }

    /// `Enter` in the tracks pane (`play_from` = selected track) or `a` (`None`): check the
    /// selected playlist's files on a worker, then replace the queue and play, or append.
    ///
    /// For `Enter` the worker checks the selected entry first, then the ones after it, then the
    /// ones before it. The first entry in that order that can be queued is the one playback
    /// starts on (`plan_queue`'s rule), so it is sent as soon as it is found and plays while the
    /// rest are checked (issue #52).
    pub fn queue_selected_playlist(&mut self, play_from: Option<usize>) {
        if !self.playlists.store.is_loaded() {
            self.status_message = self.playlists.not_loaded_message();
            return;
        }
        let Some(playlist) = self.playlists.store.playlists().get(self.playlists.list_cursor) else {
            self.status_message = "No playlist selected".to_string();
            return;
        };
        let name = playlist.name.clone();
        let entries = playlist.entries.clone();
        self.queue_playlist_entries(name, entries, play_from, None);
    }

    /// Check `entries` (playlist `name`) on a worker, then replace the queue and play from
    /// `play_from`, or append when `None`: tab 5's `Enter` / `a` and the web dashboard's Play /
    /// Append. With `allowed_roots` (web requests), an entry is only queued when its checked file
    /// lies inside those folders, so a symlink changed after the web thread's check can't reach
    /// anything else.
    fn queue_playlist_entries(
        &mut self,
        name: String,
        entries: Vec<PlaylistEntry>,
        play_from: Option<usize>,
        allowed_roots: Option<Vec<PathBuf>>,
    ) {
        if entries.is_empty() {
            self.status_message = format!("{} is empty", name);
            return;
        }
        // One request at a time: a hung share would otherwise pile up workers, and each result
        // would replace the queue again whenever it arrived.
        if let Some(pending) = &self.playlists.checks.queue_pending {
            self.status_message = format!("Still checking {}…", pending);
            return;
        }
        self.status_message = format!("Checking {} tracks in {}…", entries.len(), name);
        self.playlists.checks.queue_pending = Some(name.clone());
        let generation = self.playlists.checks.queue_generation;
        // A restricted check also refuses files that exist, so its results don't go in the
        // session's missing-file cache.
        let records_exists = allowed_roots.is_none();
        let checker = match allowed_roots {
            None => self.playlists.checks.checker.clone(),
            Some(roots) => {
                let check = self.playlists.checks.checker.clone();
                let restricted: ExistsChecker = std::sync::Arc::new(move |path: &Path| {
                    check(path).filter(|resolved| crate::web_access::resolve_allowed_path(resolved, &roots).is_some())
                });
                restricted
            }
        };
        let tx = self.playlists.checks.queue_tx.clone();
        let stop = self.playlists.checks.stop.clone();
        self.workers.spawn("playlist-queue", move || {
            let len = entries.len();
            let order: Vec<usize> = match play_from {
                Some(selected) => {
                    let selected = selected.min(len);
                    (selected..len).chain(0..selected).collect()
                }
                None => (0..len).collect(),
            };
            let mut resolved = vec![None; len];
            let mut start_sent = play_from.is_none();
            for index in order {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let entry = &entries[index];
                resolved[index] = checker(entry.path.as_path());
                if start_sent {
                    continue;
                }
                // The test `plan_checked_queue` applies (an audio file at the checked path), so
                // the start is the track `apply_playlist_queue` will find at its start index.
                if let Some(path) = resolved[index]
                    .as_ref()
                    .filter(|path| crate::metadata::is_supported_audio_file(path))
                {
                    start_sent = true;
                    let _ = tx.send(QueueCheckUpdate::Start(QueueStart {
                        name: name.clone(),
                        entry: entry.clone(),
                        resolved: path.clone(),
                        generation,
                    }));
                }
            }
            let _ = tx.send(QueueCheckUpdate::Done(QueueCheckResult {
                name,
                entries,
                resolved,
                play_from,
                generation,
                records_exists,
            }));
        });
    }

    /// The queue plan for checked entries: `resolved[i]` is entry `i`'s checked path, or `None`
    /// when it can't be queued. Tracks carry the checked paths only: the queue and the /stream
    /// registry never see an entry's own path, which may be a symlink swapped since the check.
    fn plan_checked_queue(
        &self,
        entries: &[PlaylistEntry],
        resolved: &[Option<PathBuf>],
        play_from: Option<usize>,
    ) -> crate::playlist::QueuePlan {
        let mut originals = HashMap::new();
        let mut checked = Vec::with_capacity(entries.len());
        let mut exists = Vec::with_capacity(entries.len());
        for (entry, resolved) in entries.iter().zip(resolved) {
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
        crate::playlist::plan_queue(&checked, &exists, play_from, &cached)
    }

    /// `Enter`: the track to start on passed its check. Replace the queue with it and play it
    /// now; `apply_playlist_queue` puts the rest of the playlist around it once checked.
    fn start_playlist_play(&mut self, start: QueueStart) {
        if start.generation != self.playlists.checks.queue_generation {
            // The queue was replaced or playback started while this check ran.
            return;
        }
        self.playlists.record_exists(start.entry.path.clone(), true);
        let plan = self.plan_checked_queue(
            std::slice::from_ref(&start.entry),
            &[Some(start.resolved.clone())],
            Some(0),
        );
        if plan.tracks.is_empty() {
            return;
        }
        // Stop any autopilot scan and continuous stream, then the queue is this one track.
        self.reset_playback_session();
        self.enqueue_tracks(plan.tracks);
        self.queue.current_index = 0;
        self.queue_cursor = 0;
        let playback_error = self.play_current_track();
        self.status_message = match &playback_error {
            Some(err) => format!("{}. Checking the rest of {}…", err, start.name),
            None => format!("Playing from {}; checking the rest…", start.name),
        };
        self.playlists.checks.started = Some(StartedPlay {
            path: start.resolved,
            playback_error,
        });
        self.update_web_state();
    }

    /// Put the rest of a playlist around the track `Enter` already started: the tracks before
    /// it at the top of the queue, the tracks after it right behind it, so anything queued
    /// meanwhile follows the playlist. If the started track has left the queue, the rest is
    /// appended in playlist order.
    fn fill_playlist_around(&mut self, started: &Path, mut tracks: Vec<crate::metadata::AudioMetadata>, start: usize) {
        // The worker picks the start by `plan_queue`'s rule, so `tracks[start]` is the started
        // track itself.
        debug_assert_eq!(tracks.get(start).map(|t| t.filepath.as_path()), Some(started));
        let after = tracks.split_off(start + 1);
        tracks.truncate(start);
        let before = tracks;
        match self.queue.tracks.iter().position(|t| t.filepath == started) {
            Some(position) => {
                self.insert_queue_tracks(position + 1, after);
                self.insert_queue_tracks(0, before);
            }
            None => {
                let mut rest = before;
                rest.extend(after);
                self.enqueue_tracks(rest);
            }
        }
    }

    /// Queue the files of a checked playlist that exist. Replace (`play_from` set) only clears
    /// the queue once something can be queued, so an all-missing playlist leaves it as it was.
    /// When `start_playlist_play` already started the first track, the rest is put around it.
    fn apply_playlist_queue(&mut self, result: QueueCheckResult) {
        self.playlists.checks.queue_pending = None;
        let started = self.playlists.checks.started.take();
        // Fresh results update the session cache; they never change the playlist or its file.
        if result.records_exists {
            for (entry, resolved) in result.entries.iter().zip(&result.resolved) {
                self.playlists.record_exists(entry.path.clone(), resolved.is_some());
            }
        }
        if result.generation != self.playlists.checks.queue_generation {
            // The queue was replaced or playback started while this check ran.
            return;
        }
        let crate::playlist::QueuePlan {
            tracks,
            start_index,
            missing,
        } = self.plan_checked_queue(&result.entries, &result.resolved, result.play_from);
        if tracks.is_empty() {
            let mut status = format!(
                "None of {}'s {} found.",
                result.name,
                track_count_label(result.entries.len())
            );
            if let Some(hint) = result.entries.first().and_then(|e| mount_hint(&e.path)) {
                status.push_str(&format!(" Is {} mounted?", hint));
            }
            self.status_message = status;
            return;
        }
        let queued = tracks.len();
        // enqueue_tracks starts the background tag fetch that fills in placeholder entries.
        // play_current_track returns its "Playback error: ..." when the player fails to start.
        // The status keeps that error and puts the queue summary after it, so a failed start is
        // never reported as plain success.
        let playback_error = match (result.play_from, start_index, started) {
            (Some(_), Some(start), Some(started)) => {
                self.fill_playlist_around(&started.path, tracks, start);
                started.playback_error
            }
            // Not reached in practice: the worker sends the start before any track that
            // `plan_queue` keeps. Kept so a result without one still replaces and plays.
            (Some(_), Some(start), None) => {
                self.reset_playback_session();
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
            format!("Queued {} from {}", track_count_label(queued), result.name)
        } else {
            format!(
                "Queued {} from {}, {} missing",
                track_count_label(queued),
                result.name,
                missing
            )
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
}

/// What a web snapshot was built from; while none of it changes the snapshot is current.
#[derive(Debug, Clone, PartialEq)]
struct WebSnapshotKey {
    revision: u64,
    exists_version: u64,
    loaded: bool,
    load_failed: bool,
    last_playlist: Option<String>,
}

// Web dashboard (issue #50): the snapshot behind `GET /api/playlists`, and the web's Play and
// Append, which go through the same checks and queue code as tab 5.
impl AppUI {
    /// Publish the playlists to the web state when anything they show changed since the last
    /// snapshot. Called with the rest of the web state on every tick; cheap when nothing changed.
    pub(super) fn publish_web_playlists(&mut self) {
        let key = WebSnapshotKey {
            revision: self.playlists.store.revision(),
            exists_version: self.playlists.exists_version,
            loaded: self.playlists.store.is_loaded(),
            load_failed: self.playlists.load_failed,
            last_playlist: self.settings_mgr.settings.last_playlist.clone(),
        };
        if self.playlists.web_published.as_ref() == Some(&key) {
            return;
        }
        let snapshot = std::sync::Arc::new(self.web_playlists_snapshot());
        if let Ok(mut state) = self.web_state.write() {
            state.playlists = snapshot;
            state.playlists_version = state.playlists_version.wrapping_add(1);
        }
        self.playlists.web_published = Some(key);
    }

    fn web_playlists_snapshot(&self) -> crate::web::WebPlaylists {
        use crate::web::{WebPlaylist, WebPlaylistEntry, WebPlaylists, WebPlaylistsState};
        let ui = &self.playlists;
        let state = if ui.load_failed {
            WebPlaylistsState::Failed
        } else if !ui.store.is_loaded() {
            WebPlaylistsState::Loading
        } else {
            WebPlaylistsState::Ready
        };
        let playlists = ui
            .store
            .playlists()
            .iter()
            .map(|playlist| WebPlaylist {
                name: playlist.name.clone(),
                track_count: playlist.entries.len(),
                total_duration_seconds: playlist.total_duration_secs(),
                entries: playlist
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| WebPlaylistEntry {
                        index,
                        title: entry.title.clone(),
                        artist: entry.artist.clone(),
                        duration_seconds: entry.duration_secs,
                        path: entry.path.to_string_lossy().into_owned(),
                        outside: false,
                        missing: ui.exists.get(&entry.path).map(|exists| !exists),
                    })
                    .collect(),
            })
            .collect();
        WebPlaylists {
            state,
            message: (state != WebPlaylistsState::Ready).then(|| ui.not_loaded_message()),
            revision: ui.store.revision(),
            last_playlist: self.settings_mgr.settings.last_playlist.clone(),
            playlists,
        }
    }

    /// The dashboard fetched the playlists: check every playlist's files that this session has
    /// not checked, on a worker, so the snapshot can mark the missing ones.
    pub(super) fn check_playlists_for_web(&mut self) {
        let count = self.playlists.store.playlists().len();
        self.check_playlist_entries(0..count);
    }

    /// The dashboard's Play (`play_from` set) or Append, with entries the web thread checked
    /// against the allowed folders. Same checks, queueing and status messages as tab 5; the
    /// status names the playlist rather than saying `(web)`.
    pub(super) fn queue_web_playlist(&mut self, name: String, entries: Vec<PlaylistEntry>, play_from: Option<usize>) {
        if !self.playlists.store.is_loaded() {
            self.status_message = self.playlists.not_loaded_message();
            return;
        }
        let roots = self.web_allowed_roots();
        self.queue_playlist_entries(name, entries, play_from, Some(roots));
    }

    /// Apply a playlist edit from the dashboard with the same store calls, rules and messages
    /// as tab 5, show the outcome in the status line and return it for the web request. Edits
    /// are refused while the store is loading or after its load failed, and an index-based
    /// edit made against an older revision is refused. Tab 5 stays on the playlist it showed.
    pub(super) fn apply_web_playlist_edit(&mut self, edit: crate::web::PlaylistEdit) -> Result<String, String> {
        use crate::web::PlaylistEdit;
        let result = if self.playlists.store.is_loaded() {
            let mut selected = self.playlists.selected_name();
            let result = match edit {
                PlaylistEdit::Create { name } => self
                    .playlists
                    .store
                    .create(&name)
                    .map(|name| created_message(&name))
                    .map_err(|e| e.to_string()),
                PlaylistEdit::Rename { name, new_name } => self.web_rename(&name, &new_name, &mut selected),
                PlaylistEdit::Delete { name } => self.delete_playlist_named(&name).map_err(|e| e.to_string()),
                PlaylistEdit::Add { name, path } => {
                    let entry = self.playlist_entry_for_web_add(&path);
                    self.add_entries_to_playlist(&name, vec![entry])
                }
                PlaylistEdit::Remove { name, index, revision } => self
                    .check_web_revision(revision)
                    .and_then(|()| self.remove_playlist_track(&name, index).map_err(|e| e.to_string())),
                PlaylistEdit::Move {
                    name,
                    index,
                    dir,
                    revision,
                } => self
                    .check_web_revision(revision)
                    .and_then(|()| self.web_move(&name, index, dir)),
            };
            if let Some(index) = selected.and_then(|name| self.playlists.store.index_of(&name)) {
                self.playlists.list_cursor = index;
            }
            self.playlists.clamp_cursors();
            result
        } else {
            Err(self.playlists.not_loaded_message())
        };
        self.status_message = match &result {
            Ok(message) | Err(message) => message.clone(),
        };
        result
    }

    /// Refuse an index-based edit made against a list that has changed since.
    fn check_web_revision(&self, revision: Option<u64>) -> Result<(), String> {
        match revision {
            Some(r) if r != self.playlists.store.revision() => Err(crate::web::PLAYLISTS_CHANGED.to_string()),
            _ => Ok(()),
        }
    }

    /// Rename for the dashboard; `selected` (tab 5's playlist) follows the rename.
    fn web_rename(&mut self, name: &str, new_name: &str, selected: &mut Option<String>) -> Result<String, String> {
        let old = self.stored_playlist_name(name).map_err(|e| e.to_string())?;
        let new = self.rename_playlist(&old, new_name).map_err(|e| e.to_string())?;
        if selected.as_deref() == Some(old.as_str()) {
            *selected = Some(new.clone());
        }
        Ok(renamed_message(&old, &new))
    }

    /// Move for the dashboard. Tab 5 shows no message for a move; the dashboard says where the
    /// track went.
    fn web_move(&mut self, name: &str, index: usize, dir: crate::playlist_store::MoveDir) -> Result<String, String> {
        let stored = self.stored_playlist_name(name).map_err(|e| e.to_string())?;
        let moved = self
            .playlists
            .store
            .move_entry(&stored, index, dir)
            .map_err(|e| e.to_string())?;
        let label = self
            .playlists
            .store
            .get(&stored)
            .and_then(|p| p.entries.get(moved))
            .map(PlaylistEntry::display_label)
            .unwrap_or_default();
        Ok(if moved == index {
            let edge = match dir {
                crate::playlist_store::MoveDir::Up => "top",
                crate::playlist_store::MoveDir::Down => "bottom",
            };
            format!("\"{}\" is already at the {}", label, edge)
        } else {
            format!("Moved \"{}\" to {}", label, moved + 1)
        })
    }

    /// Playlist entry for a file the dashboard adds, from metadata the app already holds: the
    /// queue row for that file (the now-playing track), else the metadata cache, else the file
    /// name. Never reads the file.
    fn playlist_entry_for_web_add(&self, path: &Path) -> PlaylistEntry {
        match self.queue.tracks.iter().find(|track| track.filepath == path) {
            Some(track) => PlaylistEntry::from_metadata(track),
            None => self.playlist_entry_for_path(path),
        }
    }
}
