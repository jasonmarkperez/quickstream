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

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::persist::{PersistFailure, PersistHandle, Reporter};
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
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NotLoaded => write!(f, "Playlists are still loading"),
            StoreError::NotFound(name) => write!(f, "No playlist named \"{}\"", name),
            StoreError::Name(e) => write!(f, "{}", e),
            StoreError::IndexOutOfRange => write!(f, "No such track"),
            StoreError::UnsafePath => write!(f, "Refusing to write outside the playlists folder"),
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

pub struct PlaylistStore {
    dir: PathBuf,
    /// Sorted by [`name_key`]; no two names are equal under [`names_equal`].
    playlists: Vec<Playlist>,
    /// Stems of playlist files skipped at load. `create` and `rename` refuse these names.
    reserved: Vec<String>,
    loaded: bool,
    persist: PersistHandle,
    /// Id of each playlist that has been created or renamed this session, by its current name.
    /// The writer's failure reports carry the id, so a report finds its playlist even after
    /// later renames.
    ids: HashMap<String, u64>,
    next_id: u64,
    failures_tx: mpsc::Sender<PersistFailure>,
    failures_rx: mpsc::Receiver<PersistFailure>,
    /// Reports taken off `failures_rx` by [`has_write_failures`](Self::has_write_failures) and
    /// not yet applied.
    unapplied: Vec<PersistFailure>,
}

impl PlaylistStore {
    /// An empty store over `dir` that refuses edits until [`set_loaded`](Self::set_loaded).
    pub fn new(dir: PathBuf, persist: PersistHandle) -> Self {
        let (failures_tx, failures_rx) = mpsc::channel();
        Self {
            dir,
            playlists: Vec::new(),
            reserved: Vec::new(),
            loaded: false,
            persist,
            ids: HashMap::new(),
            next_id: 0,
            failures_tx,
            failures_rx,
            unapplied: Vec::new(),
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

    /// Create an empty playlist and write its file. Returns the stored (trimmed) name. The file
    /// is created without replacing anything: if one is already there, the writer leaves it
    /// alone and [`poll_write_failures`](Self::poll_write_failures) removes the playlist again.
    pub fn create(&mut self, name: &str) -> Result<String, StoreError> {
        self.ensure_loaded()?;
        let name = validate_name(name).map_err(StoreError::Name)?;
        if let Some(existing) = self.get(&name) {
            return Err(StoreError::Name(NameError::Duplicate(existing.name.clone())));
        }
        self.ensure_not_reserved(&name)?;
        let path = self.file_for(&name)?;
        let playlist = Playlist::new(&name);
        let bytes = playlist.to_m3u8().into_bytes();
        self.playlists.push(playlist);
        sort_by_name(&mut self.playlists);
        let reporter = self.reporter_for(&name);
        self.persist.create(path, bytes, reporter);
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
        Ok(target)
    }

    /// Rename playlist `old` to `new` and return the stored (trimmed) new name. The duplicate
    /// check ignores the playlist being renamed, so a case-only rename is allowed. A name
    /// reserved by a file skipped at load is refused, and so is a case-only rename onto a
    /// reserved name with exactly the same bytes. If the writer can't move the file (another file
    /// is at the new name, or the disk refuses), [`poll_write_failures`](Self::poll_write_failures)
    /// puts the old name back.
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
            return Err(StoreError::Name(NameError::Duplicate(r.clone())));
        }
        let new_path = self.file_for(&new_name)?;
        let old_name = std::mem::replace(&mut self.playlists[slot].name, new_name.clone());
        let reporter = self.reporter_for(&old_name);
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
        Ok(new_name)
    }

    /// Delete playlist `name` and its file.
    pub fn delete(&mut self, name: &str) -> Result<(), StoreError> {
        let (slot, path) = self.locate(name)?;
        let removed = self.playlists.remove(slot);
        self.ids.remove(&removed.name);
        self.persist.delete(path);
        Ok(())
    }

    /// Undo, in memory, every create or rename the writer has reported failing since the last
    /// call, and say what was undone. Never blocks: call it from the UI tick.
    ///
    /// - A failed create removes the playlist (whatever it is called now). When a file was in
    ///   the way, its name is reserved like a file skipped at load, so a second create is refused
    ///   with `"Mix" already exists` instead of failing the same way. A report for a playlist
    ///   already deleted changes nothing.
    /// - A failed rename gives the playlist back the name its file still has and saves it there,
    ///   so edits made in the meantime (which the writer dropped) reach the real file.
    pub fn poll_write_failures(&mut self) -> Vec<WriteFailure> {
        self.has_write_failures();
        let reports = std::mem::take(&mut self.unapplied);
        reports.into_iter().filter_map(|report| self.undo(report)).collect()
    }

    /// Whether the writer has reported failures that [`poll_write_failures`](Self::poll_write_failures)
    /// would apply. Cheap enough for every UI tick; no playlist changes until that call.
    pub fn has_write_failures(&mut self) -> bool {
        self.unapplied.extend(self.failures_rx.try_iter());
        !self.unapplied.is_empty()
    }

    fn undo(&mut self, report: PersistFailure) -> Option<WriteFailure> {
        match report {
            PersistFailure::Create { tag, path, kind, error } => {
                let created = file_stem(&path)?;
                // Deleted already: nothing left to undo.
                let name = self.name_with_id(tag)?;
                if kind == io::ErrorKind::AlreadyExists {
                    self.reserved.push(created.clone());
                }
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
                let restored = file_stem(&from)?;
                let attempted = self.name_with_id(tag)?;
                // A playlist created or renamed onto the old name in the meantime can't have
                // its own file there (the writer never replaces one), so it gives way.
                if let Some(other) = self
                    .playlists
                    .iter()
                    .find(|p| p.name != attempted && names_equal(&p.name, &restored))
                    .map(|p| p.name.clone())
                {
                    log_warn(&format!(
                        "dropping playlist \"{}\": \"{}\" keeps its file after a failed rename",
                        other, restored
                    ));
                    self.ids.remove(&other);
                    self.playlists.retain(|p| p.name != other);
                }
                let slot = self.playlists.iter().position(|p| p.name == attempted)?;
                self.playlists[slot].name = restored.clone();
                self.ids.remove(&attempted);
                self.ids.insert(restored.clone(), tag);
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

    /// The reporter for writes of the playlist called exactly `name`, giving it an id first if
    /// it has none.
    fn reporter_for(&mut self, name: &str) -> Reporter {
        let id = match self.ids.get(name) {
            Some(id) => *id,
            None => {
                self.next_id += 1;
                self.ids.insert(name.to_string(), self.next_id);
                self.next_id
            }
        };
        Reporter::new(id, self.failures_tx.clone())
    }

    /// Current name of the playlist with id `id`.
    fn name_with_id(&self, id: u64) -> Option<String> {
        self.ids.iter().find(|(_, v)| **v == id).map(|(name, _)| name.clone())
    }

    fn ensure_loaded(&self) -> Result<(), StoreError> {
        if self.loaded {
            Ok(())
        } else {
            Err(StoreError::NotLoaded)
        }
    }

    /// Refuse `name` when it is the name of a file skipped at load, ignoring case. The error
    /// carries the file's own stem, so the message reads `"Latin" already exists`.
    fn ensure_not_reserved(&self, name: &str) -> Result<(), StoreError> {
        match self.reserved.iter().find(|r| names_equal(r, name)) {
            Some(reserved) => Err(StoreError::Name(NameError::Duplicate(reserved.clone()))),
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
