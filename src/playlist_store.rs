//! In-memory playlists, written through to `<playlists dir>/<name>.m3u8` (issue #37).
//!
//! The UI thread holds the store, and the store never touches the disk on that thread:
//! [`PlaylistStore::load_dir_reserving`] runs once on a worker at startup. Every edit changes
//! the in-memory copy, then hands the whole file to the background writer ([`PersistHandle`]),
//! which applies saves, deletes and renames in the order they were sent. Every path is built by
//! [`playlist_file_path`], so no playlist operation can write outside the playlists folder.
//! Files skipped at load keep their names reserved, so no edit ever writes over one.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::persist::PersistHandle;
use crate::playlist::{
    names_equal, parse_m3u8, playlist_file_path, validate_name, NameError, Playlist, PlaylistEntry, PLAYLIST_EXT,
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

pub struct PlaylistStore {
    dir: PathBuf,
    /// Sorted by name ignoring case; no two names are equal ignoring case.
    playlists: Vec<Playlist>,
    /// Stems of playlist files skipped at load. `create` and `rename` refuse these names.
    reserved: Vec<String>,
    loaded: bool,
    persist: PersistHandle,
}

impl PlaylistStore {
    /// An empty store over `dir` that refuses edits until [`set_loaded`](Self::set_loaded).
    pub fn new(dir: PathBuf, persist: PersistHandle) -> Self {
        Self {
            dir,
            playlists: Vec::new(),
            reserved: Vec::new(),
            loaded: false,
            persist,
        }
    }

    /// The playlists in `dir`, without the names of skipped files. Blocking: call it on a
    /// worker. See [`load_dir_reserving`](Self::load_dir_reserving).
    pub fn load_dir(dir: &Path) -> Vec<Playlist> {
        Self::load_dir_reserving(dir).0
    }

    /// Read every `*.m3u8` file directly inside `dir`. Blocking: call it on a worker. Returns
    /// the playlists and the file stems that were skipped. A playlist's name is its file stem.
    /// Only the exact lower-case extension is read: every write goes to `<name>.m3u8`, so on a
    /// case-sensitive disk the store could never rewrite, rename or delete a `Mix.M3U8`. Such
    /// files are logged and skipped, and so are entries that aren't regular files, files that
    /// can't be read and files that aren't UTF-8. Their stems come back as the second value;
    /// hand them to [`reserve_names`](Self::reserve_names) so no edit writes over them. Hidden
    /// files (macOS `._Road Trip.m3u8`) are skipped silently and not returned, because no valid
    /// name starts with a dot. Never writes; a missing folder gives two empty lists.
    pub fn load_dir_reserving(dir: &Path) -> (Vec<Playlist>, Vec<String>) {
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (Vec::new(), Vec::new()),
            Err(e) => {
                log_warn(&format!("could not list playlists folder '{}': {}", dir.display(), e));
                return (Vec::new(), Vec::new());
            }
        };
        let mut files: Vec<PathBuf> = listing
            .flatten()
            .map(|e| e.path())
            .filter(|p| has_playlist_extension(p))
            .collect();
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
        (playlists, skipped)
    }

    /// Install the playlists read by [`load_dir_reserving`](Self::load_dir_reserving) and allow
    /// edits. When names differ only in case (possible on a case-sensitive disk), the first in
    /// sort order is kept and the rest are logged and ignored. The ignored names are reserved,
    /// so no create or rename ever writes over their files.
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
                    "ignoring playlist \"{}\": another playlist has the same name in a different case",
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
    /// taken: [`create`](Self::create) and [`rename`](Self::rename) refuse them, ignoring case.
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

    /// Create an empty playlist and write its file. Returns the stored (trimmed) name.
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
        self.persist.save(path, bytes);
        Ok(name)
    }

    /// Append `entries` to the playlist named `name`, skipping any whose path is already in it
    /// (including repeats within `entries`) or contains a line break, which would split its
    /// M3U8 line. Writes the file only when something was added.
    pub fn add(&mut self, name: &str, entries: Vec<PlaylistEntry>) -> Result<AddResult, StoreError> {
        let (slot, path) = self.locate(name)?;
        let playlist = &mut self.playlists[slot];
        let mut present: HashSet<PathBuf> = playlist.entries.iter().map(|e| e.path.clone()).collect();
        let (mut added, mut skipped) = (0, 0);
        for mut entry in entries {
            // A relative path (`quickstream .`) would be read back against the playlists
            // folder, so store it absolute. `std::path::absolute` never reads the disk.
            if let Ok(abs) = std::path::absolute(&entry.path) {
                entry.path = abs;
            }
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
    /// reserved name with exactly the same bytes.
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
        if let Some(r) = self.reserved.iter().find(|r| **r == new_name) {
            return Err(StoreError::Name(NameError::Duplicate(r.clone())));
        }
        let new_path = self.file_for(&new_name)?;
        self.playlists[slot].name = new_name.clone();
        let bytes = self.playlists[slot].to_m3u8().into_bytes();
        sort_by_name(&mut self.playlists);
        // Move with the OS rename, never "write the new file, delete the old one": on a
        // case-insensitive disk `road trip.m3u8` and `Road Trip.m3u8` are one file, so the
        // delete would remove the playlist just written.
        self.persist.rename(old_path, new_path.clone());
        // Save again so the `#PLAYLIST` line carries the new name.
        self.persist.save(new_path, bytes);
        Ok(new_name)
    }

    /// Delete playlist `name` and its file.
    pub fn delete(&mut self, name: &str) -> Result<(), StoreError> {
        let (slot, path) = self.locate(name)?;
        self.playlists.remove(slot);
        self.persist.delete(path);
        Ok(())
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

/// Case-insensitive name order; the exact name breaks ties so the order is deterministic.
fn sort_by_name(playlists: &mut [Playlist]) {
    playlists.sort_by_cached_key(|p| (p.name.to_lowercase(), p.name.clone()));
}

fn log_warn(message: &str) {
    crate::logger::log_raw("WARN", "playlists", message);
}
