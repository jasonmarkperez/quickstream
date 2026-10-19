//! In-memory playlists, written through to `<playlists dir>/<name>.m3u8` (issue #37).
//!
//! The UI thread holds the store, and the store never touches the disk on that thread:
//! [`PlaylistStore::load_dir_reserving`] runs once on a worker at startup. Every edit changes
//! the in-memory copy, then hands the whole file to the background writer ([`PersistHandle`]),
//! which applies saves, deletes and renames in the order they were sent. Every path is built by
//! [`playlist_file_path`], so no playlist operation can write outside the playlists folder.
//! Files skipped at load keep their names reserved, so no edit ever writes over one.
//!
//! A new playlist's file is created without replacing anything, and renames never replace
//! another file, so a file the name checks can't see (an HFS+, SMB or exFAT name match) is left
//! alone. The writer reports such failures back on a channel; [`PlaylistStore::poll_write_failures`]
//! (called by the UI every tick) then removes the playlist that couldn't be created, or puts a
//! playlist that couldn't be renamed back under its file's name (issues #62, #52).
//!
//! The writer also acknowledges every create and rename that worked, so the store knows which
//! are still pending. Until one is settled, creating another playlist under a name it starts
//! from or goes to, or renaming another playlist onto one, is refused
//! ([`StoreError::StillCreating`], [`StoreError::StillRenaming`]): the file that name stands for
//! may still belong to the pending playlist, and if the pending operation fails, the undo puts
//! that playlist back under that very name (PR #65 review). A playlist whose own rename is still
//! pending can't be renamed again either: if that rename fails, the undo restores the name it
//! started from, which is only the file's real name while no later rename has moved the file.
//! The playlist may still be renamed while its create is pending, and once it is deleted its
//! names are free.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::persist::{PersistFailure, PersistHandle, PersistReport, Reporter};
use crate::playlist::{
    name_key, names_equal, normalize_path, parse_m3u8, playlist_file_path, validate_name, NameError, Playlist,
    PlaylistEntry, PLAYLIST_EXT,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The startup load hasn't finished; an edit now could overwrite a file not yet read.
    NotLoaded,
    NotFound(String),
    Name(NameError),
    IndexOutOfRange,
    /// The playlist's file would land outside the playlists folder.
    UnsafePath,
    /// A create of this name has not been settled by the writer yet.
    StillCreating(String),
    /// A rename from `from` to `to` has not been settled by the writer yet.
    StillRenaming {
        from: String,
        to: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotLoaded => write!(f, "Playlists are still loading"),
            StoreError::NotFound(name) => write!(f, "No playlist named \"{}\"", name),
            StoreError::Name(e) => write!(f, "{}", e),
            StoreError::IndexOutOfRange => write!(f, "No such track"),
            StoreError::UnsafePath => write!(f, "Refusing to write outside the playlists folder"),
            StoreError::StillCreating(name) => write!(f, "Still creating \"{}\", try again", name),
            StoreError::StillRenaming { from, to } => {
                write!(f, "Still renaming \"{}\" to \"{}\", try again", from, to)
            }
        }
    }
}

/// Outcome of [`PlaylistStore::add`]: tracks added, tracks skipped (path already in the
/// playlist, or containing a line break), and the playlist's length afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddResult {
    pub added: usize,
    pub skipped: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDir {
    Up,
    Down,
}

/// A create or rename the background writer couldn't do, after the store undid it in memory.
/// The UI shows `message` as the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteFailure {
    /// The playlist's file couldn't be created (a file was already there, or the disk refused),
    /// so the playlist, now called `name`, was removed.
    CreateUndone { name: String, message: String },
    /// The rename didn't happen, so the playlist called `attempted` is called `restored` again:
    /// the name its file still has.
    RenameUndone {
        attempted: String,
        restored: String,
        message: String,
    },
}

impl WriteFailure {
    pub fn message(&self) -> &str {
        match self {
            WriteFailure::CreateUndone { message, .. } | WriteFailure::RenameUndone { message, .. } => message,
        }
    }
}

/// What [`PlaylistStore::load_dir_reserving`] returns: the playlists and the stems of the
/// files it skipped, or the error that kept it from listing the folder.
pub type LoadResult = io::Result<(Vec<Playlist>, Vec<String>)>;

/// A create or rename handed to the writer that it hasn't settled yet, by the operation's tag.
struct PendingWrite {
    /// Id of the playlist it is for.
    playlist: u64,
    /// The name renamed from; `None` for a create.
    from: Option<String>,
    /// The name created or renamed to.
    to: String,
}

impl PendingWrite {
    fn involves(&self, name: &str) -> bool {
        names_equal(&self.to, name) || self.from.as_deref().is_some_and(|from| names_equal(from, name))
    }

    fn refusal(&self) -> StoreError {
        match &self.from {
            None => StoreError::StillCreating(self.to.clone()),
            Some(from) => StoreError::StillRenaming {
                from: from.clone(),
                to: self.to.clone(),
            },
        }
    }
}

pub struct PlaylistStore {
    dir: PathBuf,
    /// Sorted by [`name_key`]; no two names are equal under [`names_equal`].
    playlists: Vec<Playlist>,
    /// Stems of playlist files skipped at load. `create` and `rename` refuse these names.
    reserved: Vec<String>,
    loaded: bool,
    persist: PersistHandle,
    /// Id of each playlist that has been created or renamed this session, or asked for with
    /// [`id_of`](Self::id_of), by its current name. The writer's failure reports and pending
    /// folder adds carry the id, so they find their playlist even after later renames.
    ids: HashMap<String, u64>,
    next_id: u64,
    /// Creates and renames the writer hasn't settled, by operation tag; see
    /// [`ensure_not_pending`](Self::ensure_not_pending). An entry stays until its failure has been
    /// undone, not just received.
    pending: HashMap<u64, PendingWrite>,
    next_op: u64,
    reports_tx: mpsc::Sender<PersistReport>,
    reports_rx: mpsc::Receiver<PersistReport>,
    /// Failures taken off `reports_rx` by [`has_write_failures`](Self::has_write_failures) and
    /// not yet applied.
    unapplied: Vec<PersistFailure>,
    /// Bumped by every change to the playlists; see [`revision`](Self::revision).
    revision: u64,
}

impl PlaylistStore {
    /// An empty store over `dir` that refuses edits until [`set_loaded`](Self::set_loaded).
    pub fn new(dir: PathBuf, persist: PersistHandle) -> Self {
        let (reports_tx, reports_rx) = mpsc::channel();
        Self {
            dir,
            playlists: Vec::new(),
            reserved: Vec::new(),
            loaded: false,
            persist,
            ids: HashMap::new(),
            next_id: 0,
            pending: HashMap::new(),
            next_op: 0,
            reports_tx,
            reports_rx,
            unapplied: Vec::new(),
            revision: 0,
        }
    }

    /// The playlists in `dir`, without the names of skipped files. Blocking: call it on a
    /// worker. See [`load_dir_reserving`](Self::load_dir_reserving).
    pub fn load_dir(dir: &Path) -> io::Result<Vec<Playlist>> {
        Self::load_dir_reserving(dir).map(|(playlists, _)| playlists)
    }

    /// Read every `*.m3u8` file directly inside `dir`. Blocking: call it on a worker. Returns
    /// the playlists and the file stems that were skipped. A playlist's name is its file stem.
    /// Only the exact lower-case extension is read: every write goes to `<name>.m3u8`, so on a
    /// case-sensitive disk the store could never rewrite, rename or delete a `Mix.M3U8`. Such
    /// files are logged and skipped, and so are entries that aren't regular files, files that
    /// can't be read and files that aren't UTF-8. Their stems come back as the second value;
    /// hand them to [`reserve_names`](Self::reserve_names) so no edit writes over them. Hidden
    /// files (macOS `._Road Trip.m3u8`) are skipped silently and not returned, because no valid
    /// name starts with a dot. Never writes; a missing folder gives two empty lists. Any other
    /// error listing the folder (permissions, say) is returned: the folder may hold playlists,
    /// so the store must not be loaded as empty and then write over them.
    pub fn load_dir_reserving(dir: &Path) -> LoadResult {
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            // A symlink to a folder that isn't there (an unmounted volume) is not "no playlists
            // yet": once the volume mounts, a playlist created now would overwrite its file.
            Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(dir).is_err() => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(e) => {
                log_warn(&format!("could not list playlists folder '{}': {}", dir.display(), e));
                return Err(e);
            }
        };
        // An entry that can't be read could be a playlist; leaving it out would let a create
        // overwrite it, so the whole load fails instead.
        let mut files = Vec::new();
        for entry in listing {
            let path = entry
                .inspect_err(|e| log_warn(&format!("could not list playlists folder '{}': {}", dir.display(), e)))?
                .path();
            if has_playlist_extension(&path) {
                files.push(path);
            }
        }
        files.sort();

        let mut playlists = Vec::new();
        let mut skipped = Vec::new();
        for path in files {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                // A typed name is always UTF-8 and can never reach this file: nothing to reserve.
                log_warn(&format!(
                    "skipping playlist with a non-UTF-8 file name: '{}'",
                    path.display()
                ));
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            match read_playlist_text(&path) {
                Ok(text) => playlists.push(parse_m3u8(name, &text, dir)),
                Err(reason) => {
                    log_warn(&format!("skipping playlist '{}': {}", path.display(), reason));
                    skipped.push(name.to_string());
                }
            }
        }
        Ok((playlists, skipped))
    }

    /// Install the playlists read by [`load_dir_reserving`](Self::load_dir_reserving) and allow
    /// edits. When names differ only in case or Unicode normalization (possible on a disk that
    /// tells them apart), the first in sort order is kept and the rest are logged and ignored.
    /// The ignored names are reserved, so no create or rename ever writes over their files.
    pub fn set_loaded(&mut self, playlists: Vec<Playlist>) {
        let mut playlists = playlists;
        sort_by_name(&mut playlists);
        self.playlists = Vec::with_capacity(playlists.len());
        for playlist in playlists {
            if self
                .playlists
                .last()
                .is_some_and(|kept| names_equal(&kept.name, &playlist.name))
            {
                log_warn(&format!(
                    "ignoring playlist \"{}\": another playlist has the same name in a different case or form",
                    playlist.name
                ));
                // Its file is still on disk and is never rewritten (spec Section 1).
                self.reserved.push(playlist.name);
                continue;
            }
            self.playlists.push(playlist);
        }
        self.loaded = true;
        self.changed();
    }

    /// Treat `names` (the stems [`load_dir_reserving`](Self::load_dir_reserving) skipped) as
    /// taken: [`create`](Self::create) and [`rename`](Self::rename) refuse them, compared with
    /// [`names_equal`].
    /// On a case-insensitive disk `latin.m3u8` is the skipped `Latin.m3u8`, so writing it would
    /// overwrite a file the store never read.
    pub fn reserve_names(&mut self, names: Vec<String>) {
        self.reserved.extend(names);
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// A number that changes whenever the playlists change (load, create, add, remove, move,
    /// rename, delete, or an undone write) and only then. The web dashboard rebuilds its
    /// snapshot when it changes, and its edits name the revision they were made against so an
    /// edit of a list that changed meanwhile is refused (issue #50).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn playlists(&self) -> &[Playlist] {
        &self.playlists
    }

    /// The playlist named `name`, ignoring case.
    pub fn get(&self, name: &str) -> Option<&Playlist> {
        self.index_of(name).map(|i| &self.playlists[i])
    }

    /// Position of the playlist named `name` (ignoring case) in [`playlists`](Self::playlists).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.playlists.iter().position(|p| names_equal(&p.name, name))
    }

    /// Stable id of the playlist named `name` (ignoring case), giving it one if it has none.
    /// The id follows the playlist through renames, including a rename the writer undid, and
    /// ends when it is deleted (or its create is undone); a new playlist under the same name
    /// gets a new id. Work that outlives a UI tick, such as a folder add, names its target by
    /// id (issue #52).
    pub fn id_of(&mut self, name: &str) -> Option<u64> {
        let name = self.get(name)?.name.clone();
        Some(self.id_for(&name))
    }

    /// Current name of the playlist with id `id`, or `None` once it was deleted.
    pub fn name_of(&self, id: u64) -> Option<&str> {
        self.ids.iter().find(|(_, v)| **v == id).map(|(name, _)| name.as_str())
    }

    /// Create an empty playlist and write its file. Returns the stored (trimmed) name. The file
    /// is created without replacing anything: if one is already there, the writer leaves it
    /// alone and [`poll_write_failures`](Self::poll_write_failures) removes the playlist again.
    /// A name that a create or rename the writer hasn't settled starts from or goes to is refused
    /// with [`StoreError::StillCreating`] or [`StoreError::StillRenaming`] while that playlist
    /// still exists.
    pub fn create(&mut self, name: &str) -> Result<String, StoreError> {
        self.ensure_loaded()?;
        let name = validate_name(name).map_err(StoreError::Name)?;
        if let Some(existing) = self.get(&name) {
            return Err(StoreError::Name(NameError::Duplicate(existing.name.clone())));
        }
        self.ensure_not_reserved(&name)?;
        self.ensure_not_pending(&name, None)?;
        let path = self.file_for(&name)?;
        let playlist = Playlist::new(&name);
        let bytes = playlist.to_m3u8().into_bytes();
        self.playlists.push(playlist);
        sort_by_name(&mut self.playlists);
        let reporter = self.reporter_for(&name, None);
        self.persist.create(path, bytes, reporter);
        self.changed();
        Ok(name)
    }

    /// Append `entries` to the playlist named `name`, skipping any whose path is already in it
    /// (including repeats within `entries`) or contains a line break, which would split its
    /// M3U8 line. Paths are made absolute and normalized before the check, so `/m/a/../a/x.mp3`
    /// matches `/m/a/x.mp3`. Writes the file only when something was added.
    pub fn add(&mut self, name: &str, entries: Vec<PlaylistEntry>) -> Result<AddResult, StoreError> {
        let (slot, path) = self.locate(name)?;
        let playlist = &mut self.playlists[slot];
        let mut present: HashSet<PathBuf> = playlist.entries.iter().map(|e| e.path.clone()).collect();
        let (mut added, mut skipped) = (0, 0);
        for mut entry in entries {
            // A relative path (`quickstream .`) would be read back against the playlists
            // folder, so store it absolute. `std::path::absolute` never reads the disk but keeps
            // `..` on Unix, so normalize too: the same file reached through `..` is then a
            // duplicate, and no `..` is written to the file (issue #58).
            if let Ok(abs) = std::path::absolute(&entry.path) {
                entry.path = abs;
            }
            entry.path = normalize_path(&entry.path);
            let bytes = entry.path.as_os_str().as_encoded_bytes();
            if bytes.contains(&b'\n') || bytes.contains(&b'\r') {
                skipped += 1;
                continue;
            }
            if present.insert(entry.path.clone()) {
                playlist.entries.push(entry);
                added += 1;
            } else {
                skipped += 1;
            }
        }
        let total = playlist.entries.len();
        if added > 0 {
            self.write_through(slot, path);
            self.changed();
        }
        Ok(AddResult { added, skipped, total })
    }

    /// Remove and return track `index` of playlist `name`.
    pub fn remove(&mut self, name: &str, index: usize) -> Result<PlaylistEntry, StoreError> {
        let (slot, path) = self.locate(name)?;
        let entries = &mut self.playlists[slot].entries;
        if index >= entries.len() {
            return Err(StoreError::IndexOutOfRange);
        }
        let removed = entries.remove(index);
        self.write_through(slot, path);
        self.changed();
        Ok(removed)
    }

    /// Move track `index` one step up or down and return its new index. Moves don't wrap
    /// around: at an edge the index is returned unchanged and nothing is written.
    pub fn move_entry(&mut self, name: &str, index: usize, dir: MoveDir) -> Result<usize, StoreError> {
        let (slot, path) = self.locate(name)?;
        let entries = &mut self.playlists[slot].entries;
        if index >= entries.len() {
            return Err(StoreError::IndexOutOfRange);
        }
        let target = match dir {
            MoveDir::Up if index > 0 => index - 1,
            MoveDir::Down if index + 1 < entries.len() => index + 1,
            _ => return Ok(index),
        };
        entries.swap(index, target);
        self.write_through(slot, path);
        self.changed();
        Ok(target)
    }

    /// Rename playlist `old` to `new` and return the stored (trimmed) new name. The duplicate
    /// check ignores the playlist being renamed, so a case-only rename is allowed. A name
    /// reserved by a file skipped at load is refused, and so is a case-only rename onto a
    /// reserved name with exactly the same bytes. If the writer can't move the file (another file
    /// is at the new name, or the disk refuses), [`poll_write_failures`](Self::poll_write_failures)
    /// puts the old name back. A new name that another playlist's unsettled create or rename
    /// starts from or goes to is refused like in [`create`](Self::create). So is any rename of a
    /// playlist whose own previous rename is still pending ([`StoreError::StillRenaming`]), even
    /// straight back: the undo of a failed rename would otherwise restore a name the file no
    /// longer has. The playlist's own pending create doesn't hold it up.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<String, StoreError> {
        let (slot, old_path) = self.locate(old)?;
        let new_name = validate_name(new).map_err(StoreError::Name)?;
        if let Some(other) = self
            .playlists
            .iter()
            .enumerate()
            .find(|(i, p)| *i != slot && names_equal(&p.name, &new_name))
            .map(|(_, p)| p)
        {
            return Err(StoreError::Name(NameError::Duplicate(other.name.clone())));
        }
        // Naming a playlist what it is already called changes nothing, so no reserved name or
        // file system quirk can make it fail.
        if self.playlists[slot].name == new_name {
            return Ok(new_name);
        }
        // A case-only rename keeps the playlist's own file, so only a different name can land
        // on a skipped file.
        if !names_equal(&self.playlists[slot].name, &new_name) {
            self.ensure_not_reserved(&new_name)?;
        }
        // Except on a case-sensitive disk, where `set_loaded` may have dropped `mix.m3u8` next
        // to the kept `Mix.m3u8`: renaming `Mix` to exactly `mix` would replace that file.
        // Case-sensitive APFS still ignores normalization, so compare canonical forms.
        if let Some(r) = self.reserved.iter().find(|r| same_canonical(r, &new_name)) {
            return Err(StoreError::Name(NameError::Reserved(r.clone())));
        }
        let renaming = self.ids.get(&self.playlists[slot].name).copied();
        self.ensure_not_pending(&new_name, renaming)?;
        let new_path = self.file_for(&new_name)?;
        let old_name = std::mem::replace(&mut self.playlists[slot].name, new_name.clone());
        let reporter = self.reporter_for(&old_name, Some(&new_name));
        if let Some(id) = self.ids.remove(&old_name) {
            self.ids.insert(new_name.clone(), id);
        }
        let bytes = self.playlists[slot].to_m3u8().into_bytes();
        sort_by_name(&mut self.playlists);
        // Move with the OS rename, never "write the new file, delete the old one": on a
        // case-insensitive disk `road trip.m3u8` and `Road Trip.m3u8` are one file, so the
        // delete would remove the playlist just written.
        self.persist.rename_reporting(old_path, new_path.clone(), reporter);
        // Save again so the `#PLAYLIST` line carries the new name.
        self.persist.save(new_path, bytes);
        self.changed();
        Ok(new_name)
    }

    /// Delete playlist `name` and its file.
    pub fn delete(&mut self, name: &str) -> Result<(), StoreError> {
        let (slot, path) = self.locate(name)?;
        let removed = self.playlists.remove(slot);
        self.ids.remove(&removed.name);
        self.persist.delete(path);
        self.changed();
        Ok(())
    }

    /// Undo, in memory, every create or rename the writer has reported failing since the last
    /// call, and say what was undone. Never blocks: call it from the UI tick.
    ///
    /// - A failed create removes the playlist (whatever it is called now). When a file was in
    ///   the way, its name is reserved like a file skipped at load, so a second create is refused
    ///   with `A playlist file named "Mix" exists but couldn't be loaded` instead of failing the
    ///   same way. A report for a playlist already deleted changes nothing.
    /// - A failed rename gives the playlist back the name its file still has and saves it there,
    ///   so edits made in the meantime (which the writer dropped) reach the real file. No other
    ///   playlist can hold that name: creating or renaming onto it was refused while the rename
    ///   was pending.
    ///
    /// Each undone operation stops being pending, so its names can be used again.
    pub fn poll_write_failures(&mut self) -> Vec<WriteFailure> {
        self.has_write_failures();
        let reports = std::mem::take(&mut self.unapplied);
        let undone: Vec<WriteFailure> = reports.into_iter().filter_map(|report| self.undo(report)).collect();
        if !undone.is_empty() {
            self.changed();
        }
        undone
    }

    /// Whether the writer has reported failures that [`poll_write_failures`](Self::poll_write_failures)
    /// would apply. Cheap enough for every UI tick; no playlist changes until that call. Creates
    /// and renames the writer reports done stop being pending here.
    pub fn has_write_failures(&mut self) -> bool {
        for report in self.reports_rx.try_iter() {
            match report {
                PersistReport::Done { tag } => {
                    self.pending.remove(&tag);
                }
                PersistReport::Failed(failure) => self.unapplied.push(failure),
            }
        }
        !self.unapplied.is_empty()
    }

    fn undo(&mut self, report: PersistFailure) -> Option<WriteFailure> {
        match report {
            PersistFailure::Create { tag, path, kind, error } => {
                let playlist = self.pending.remove(&tag)?.playlist;
                let created = file_stem(&path)?;
                // The file in the way stays, whether or not the playlist is still here.
                if kind == io::ErrorKind::AlreadyExists && !self.reserved.contains(&created) {
                    self.reserved.push(created.clone());
                }
                // Deleted already: nothing left to undo.
                let name = self.name_with_id(playlist)?;
                self.ids.remove(&name);
                self.playlists.retain(|p| p.name != name);
                log_warn(&format!("removed playlist \"{}\": its file couldn't be created", name));
                Some(WriteFailure::CreateUndone {
                    name,
                    message: format!("Couldn't create \"{}\": {}", created, failure_reason(kind, &error)),
                })
            }
            PersistFailure::Rename {
                tag,
                from,
                to,
                kind,
                error,
            } => {
                let playlist = self.pending.remove(&tag)?.playlist;
                let restored = file_stem(&from)?;
                // Deleted already: the delete removed the file still at the old name.
                let attempted = self.name_with_id(playlist)?;
                // Creating or renaming onto the old name was refused while the rename was
                // pending, so only this playlist can hold it.
                if let Some(other) = self
                    .playlists
                    .iter()
                    .find(|p| p.name != attempted && names_equal(&p.name, &restored))
                {
                    debug_assert!(false, "\"{}\" took the name of a pending rename", other.name);
                    log_warn(&format!(
                        "not restoring \"{}\" to \"{}\": another playlist has that name",
                        attempted, restored
                    ));
                    return None;
                }
                let slot = self.playlists.iter().position(|p| p.name == attempted)?;
                self.playlists[slot].name = restored.clone();
                self.ids.remove(&attempted);
                self.ids.insert(restored.clone(), playlist);
                sort_by_name(&mut self.playlists);
                let slot = self.playlists.iter().position(|p| p.name == restored)?;
                if let Ok(path) = self.file_for(&restored) {
                    self.write_through(slot, path);
                }
                log_warn(&format!(
                    "playlist \"{}\" is \"{}\" again: its file couldn't be renamed",
                    attempted, restored
                ));
                let target = file_stem(&to).unwrap_or_else(|| attempted.clone());
                Some(WriteFailure::RenameUndone {
                    attempted,
                    restored: restored.clone(),
                    message: format!(
                        "Couldn't rename \"{}\" to \"{}\": {}",
                        restored,
                        target,
                        failure_reason(kind, &error)
                    ),
                })
            }
        }
    }

    /// The reporter for a create of the playlist called exactly `name` (`to` is `None`), or a
    /// rename of it to `to`, giving the playlist an id first if it has none. The operation is
    /// pending until the writer settles it.
    fn reporter_for(&mut self, name: &str, to: Option<&str>) -> Reporter {
        let playlist = self.id_for(name);
        self.next_op += 1;
        let pending = match to {
            None => PendingWrite {
                playlist,
                from: None,
                to: name.to_string(),
            },
            Some(to) => PendingWrite {
                playlist,
                from: Some(name.to_string()),
                to: to.to_string(),
            },
        };
        self.pending.insert(self.next_op, pending);
        Reporter::new(self.next_op, self.reports_tx.clone())
    }

    /// Refuse to create a playlist under `name`, or rename the playlist with id `renaming` onto
    /// it, while another playlist's create or rename that the writer hasn't settled starts from or
    /// goes to it (ignoring case, like the disk). That playlist's file may still be at the name,
    /// or its operation may fail and the undo put it back under the name; a second playlist
    /// claiming the name would then share its file, so deleting one would delete the other's
    /// file and the undo would drop saves (PR #65 review).
    ///
    /// Also refused: any rename of the playlist `renaming` while a rename of its own is pending.
    /// If that rename fails, the undo gives the playlist back the name it was renamed from; once a
    /// second rename has gone through (straight back, then on to a third name, or a case-only
    /// rename back), the file is no longer there and the restored name would be wrong.
    ///
    /// Not refused: the playlist's own pending create (its undo removes the playlist by id,
    /// whatever it is called by then), and the operations of a playlist deleted since (the
    /// writer's delete removed the file the operation left behind, or left alone a file that was
    /// never ours, so a later create or rename of the name gets its own report). Reports already
    /// received are settled first, so a finished operation never holds anything up.
    fn ensure_not_pending(&mut self, name: &str, renaming: Option<u64>) -> Result<(), StoreError> {
        self.has_write_failures();
        let blocking = self
            .pending
            .iter()
            .filter(|(_, p)| {
                // The playlist being renamed has a rename of its own still unsettled. If that one
                // fails, the undo puts back the name it started from, which is only right while
                // the file hasn't moved since; a rename back and on would move it (PR #65 review).
                let own_rename = Some(p.playlist) == renaming && p.from.is_some();
                let others = p.involves(name) && Some(p.playlist) != renaming && self.name_of(p.playlist).is_some();
                own_rename || others
            })
            // The earliest such operation, so the message doesn't depend on hash order.
            .min_by_key(|(tag, _)| **tag);
        match blocking {
            Some((_, pending)) => Err(pending.refusal()),
            None => Ok(()),
        }
    }

    /// The file that holds playlist `name`, as far as the store knows: while a rename of it is
    /// unsettled (the writer hasn't reported it done; see
    /// [`has_write_failures`](Self::has_write_failures)), the name it was renamed from, which the
    /// file keeps until the writer moves it, and which the undo restores if the move fails.
    /// Otherwise its own name. `None` for an unknown name or one whose file would land outside the
    /// playlists folder. Cheap: a playlist has at most one rename of its own pending.
    pub fn file_path_of(&self, name: &str) -> Option<PathBuf> {
        let playlist = self.get(name)?;
        let file_name = self
            .ids
            .get(&playlist.name)
            .and_then(|id| {
                self.pending
                    .values()
                    .find(|p| p.playlist == *id && p.from.is_some())
                    .and_then(|p| p.from.as_deref())
            })
            .unwrap_or(&playlist.name);
        playlist_file_path(&self.dir, file_name)
    }

    /// Id of the playlist called exactly `name`, giving it one if it has none.
    fn id_for(&mut self, name: &str) -> u64 {
        match self.ids.get(name) {
            Some(id) => *id,
            None => {
                self.next_id += 1;
                self.ids.insert(name.to_string(), self.next_id);
                self.next_id
            }
        }
    }

    /// Current name of the playlist with id `id`.
    fn name_with_id(&self, id: u64) -> Option<String> {
        self.name_of(id).map(str::to_string)
    }

    /// Record a change to the playlists for [`revision`](Self::revision).
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn ensure_loaded(&self) -> Result<(), StoreError> {
        if self.loaded {
            Ok(())
        } else {
            Err(StoreError::NotLoaded)
        }
    }

    /// Refuse `name` when it is the name of a file skipped at load, ignoring case. The error
    /// carries the file's own stem, so the message reads
    /// `A playlist file named "Latin" exists but couldn't be loaded`: no such playlist is listed,
    /// so `"Latin" already exists` would not make sense.
    fn ensure_not_reserved(&self, name: &str) -> Result<(), StoreError> {
        match self.reserved.iter().find(|r| names_equal(r, name)) {
            Some(reserved) => Err(StoreError::Name(NameError::Reserved(reserved.clone()))),
            None => Ok(()),
        }
    }

    /// The file for playlist `name`, refused when it would land outside the playlists folder.
    fn file_for(&self, name: &str) -> Result<PathBuf, StoreError> {
        playlist_file_path(&self.dir, name).ok_or(StoreError::UnsafePath)
    }

    /// For an edit of an existing playlist: its position and its file, checked in contract
    /// order (loaded, exists, safe path).
    fn locate(&self, name: &str) -> Result<(usize, PathBuf), StoreError> {
        self.ensure_loaded()?;
        let slot = self
            .index_of(name)
            .ok_or_else(|| StoreError::NotFound(name.to_string()))?;
        let path = self.file_for(&self.playlists[slot].name)?;
        Ok((slot, path))
    }

    /// Hand the playlist at `slot` to the background writer as the whole new file.
    fn write_through(&self, slot: usize, path: PathBuf) {
        self.persist.save(path, self.playlists[slot].to_m3u8().into_bytes());
    }
}

/// True when the extension is `m3u8` in any case. [`PlaylistStore::load_dir_reserving`] then
/// reads only the exact lower-case extension and logs and reserves the others, so the user can
/// see why a file is missing and no edit writes over it.
fn has_playlist_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PLAYLIST_EXT))
}

/// The text of the playlist file at `path`, or why it can't be loaded.
fn read_playlist_text(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err("not a regular file".to_string());
    }
    if path.extension() != Some(OsStr::new(PLAYLIST_EXT)) {
        return Err(format!("rename it to end in .{} to use it", PLAYLIST_EXT));
    }
    let bytes = fs::read(path).map_err(|e| format!("could not read it: {}", e))?;
    String::from_utf8(bytes).map_err(|_| "not valid UTF-8".to_string())
}

/// The playlist name a playlist file path stands for (its stem).
fn file_stem(path: &Path) -> Option<String> {
    path.file_stem().and_then(|s| s.to_str()).map(str::to_string)
}

/// Why a create or rename failed, for the status line.
fn failure_reason(kind: io::ErrorKind, error: &str) -> String {
    if kind == io::ErrorKind::AlreadyExists {
        "a file with that name already exists".to_string()
    } else {
        error.to_string()
    }
}

/// Name order ignoring case; the exact name breaks ties so the order is deterministic.
/// Sorting by [`name_key`] puts names that [`names_equal`] matches next to each other, which
/// `set_loaded` relies on to drop duplicates.
fn sort_by_name(playlists: &mut [Playlist]) {
    playlists.sort_by_cached_key(|p| (name_key(&p.name), p.name.clone()));
}

/// Equal up to Unicode normalization, but case-sensitive.
fn same_canonical(a: &str, b: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    a.nfd().eq(b.nfd())
}

fn log_warn(message: &str) {
    crate::logger::log_raw("WARN", "playlists", message);
}
