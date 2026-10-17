//! Playlists (issue #37): the extended M3U8 file format, playlist names, and turning a playlist
//! into queue tracks. Pure: nothing here reads or writes the disk, so it is safe on the UI thread.

use std::path::{Path, PathBuf};

use crate::metadata::AudioMetadata;

/// Extension of a playlist file in the playlists folder.
pub const PLAYLIST_EXT: &str = "m3u8";

/// Longest playlist name [`validate_name`] accepts, in characters (not bytes).
pub const MAX_NAME_CHARS: usize = 100;

/// One track of a playlist. The path is the source of truth for the file; artist, title and
/// duration are only what the `#EXTINF` line shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    /// Whole seconds, or -1 when unknown.
    pub duration_secs: i64,
    pub artist: String,
    pub title: String,
}

impl PlaylistEntry {
    /// Entry from metadata the app already holds (a queue row, the now-playing track or a
    /// metadata cache hit).
    pub fn from_metadata(meta: &AudioMetadata) -> Self {
        let duration_secs = if meta.duration_seconds > 0.0 {
            whole_seconds(meta.duration_seconds)
        } else {
            -1
        };
        Self {
            path: meta.filepath.clone(),
            duration_secs,
            artist: meta.artist.clone(),
            title: meta.display_title().to_string(),
        }
    }

    /// Entry for a file the app holds no metadata for. Reads nothing from disk: the title is the
    /// file stem, the artist empty and the duration unknown.
    pub fn from_path(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            duration_secs: -1,
            artist: String::new(),
            title: file_stem(path),
        }
    }

    /// `Artist - Title`, or the title alone when the artist is empty.
    pub fn display_label(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.artist, self.title)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Playlist {
    pub name: String,
    pub entries: Vec<PlaylistEntry>,
}

impl Playlist {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            entries: Vec::new(),
        }
    }

    /// Extended M3U8 text: UTF-8, LF line endings, no BOM, an `#EXTINF` line before each path.
    pub fn to_m3u8(&self) -> String {
        let mut out = String::from("#EXTM3U\n");
        out.push_str(&format!("#PLAYLIST:{}\n", single_line(&self.name)));
        for entry in &self.entries {
            let secs = entry.duration_secs.max(-1);
            out.push_str(&format!("#EXTINF:{},{}\n", secs, single_line(&entry.display_label())));
            // M3U8 is UTF-8 text, so a path that is not valid UTF-8 is written lossily.
            out.push_str(&entry.path.to_string_lossy());
            out.push('\n');
        }
        out
    }

    /// Sum of the known durations; entries with an unknown (-1) duration are left out.
    pub fn total_duration_secs(&self) -> i64 {
        self.entries
            .iter()
            .filter(|e| e.duration_secs >= 0)
            .fold(0i64, |total, e| total.saturating_add(e.duration_secs))
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        self.entries.iter().any(|e| e.path == path)
    }
}

/// Parse extended M3U / M3U8 text, including files edited by hand or copied in from other
/// players: a leading BOM, CRLF line endings, blank lines and unknown `#` lines are accepted.
/// `name` (the file stem) is the playlist's name; a `#PLAYLIST` line is ignored. Relative paths
/// are resolved against `base_dir`, the folder holding the playlist file. Any other line is kept
/// as a path entry as written (a Windows path or a `file://` URL simply shows up as missing).
pub fn parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut playlist = Playlist::new(name);
    // The `#EXTINF` duration and text waiting for the path line they describe.
    let mut pending: Option<(i64, String)> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(info) = strip_extinf(line) {
            pending = Some(parse_extinf(info));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        // `join` keeps an absolute path unchanged and resolves a relative one against the folder.
        let path = base_dir.join(line);
        let (duration_secs, label) = pending.take().unwrap_or((-1, String::new()));
        let (artist, title) = split_label(&label);
        let title = if title.is_empty() { file_stem(&path) } else { title };
        playlist.entries.push(PlaylistEntry {
            path,
            duration_secs,
            artist,
            title,
        });
    }
    playlist
}

/// The text after `#EXTINF:` (tag matched case-insensitively), or `None` for any other line.
fn strip_extinf(line: &str) -> Option<&str> {
    const TAG: &str = "#EXTINF:";
    match line.get(..TAG.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(TAG) => Some(&line[TAG.len()..]),
        _ => None,
    }
}

/// `<duration>[ attributes],<text>` into (whole seconds or -1, text).
fn parse_extinf(info: &str) -> (i64, String) {
    let (head, text) = info.split_once(',').unwrap_or((info, ""));
    let secs = head
        .split_whitespace()
        .next()
        .and_then(|d| d.parse::<f64>().ok())
        .map(whole_seconds)
        .unwrap_or(-1);
    (secs, text.trim().to_string())
}

/// `<artist> - <title>` split at the first ` - `; text without one is the title alone.
fn split_label(label: &str) -> (String, String) {
    match label.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_string(), title.trim().to_string()),
        None => (String::new(), label.to_string()),
    }
}

/// Rounded seconds, or -1 for a negative, NaN or infinite value.
fn whole_seconds(secs: f64) -> i64 {
    if secs.is_finite() && secs >= 0.0 {
        secs.round() as i64
    } else {
        -1
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Control characters (a newline in a tag, say) replaced by spaces, so one value stays on one
/// line of the file.
fn single_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Why a playlist name was refused. The `Display` text is shown in the name prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    Slash,
    Backslash,
    ControlChar,
    LeadingDot,
    /// Another playlist already has this name, ignoring case. Raised by the store, which knows
    /// the other names; [`validate_name`] never returns it.
    Duplicate(String),
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "Name can't be empty"),
            NameError::TooLong => write!(f, "Name is too long (max {} characters)", MAX_NAME_CHARS),
            NameError::Slash => write!(f, "Name can't contain /"),
            NameError::Backslash => write!(f, "Name can't contain \\"),
            NameError::ControlChar => write!(f, "Name can't contain control characters"),
            NameError::LeadingDot => write!(f, "Name can't start with ."),
            NameError::Duplicate(name) => write!(f, "\"{}\" already exists", name),
        }
    }
}

impl std::error::Error for NameError {}

/// Trim `name` and check it can be a playlist file name. Returns the trimmed name.
pub fn validate_name(name: &str) -> Result<String, NameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(NameError::TooLong);
    }
    if name.contains('/') {
        return Err(NameError::Slash);
    }
    if name.contains('\\') {
        return Err(NameError::Backslash);
    }
    if name.chars().any(char::is_control) {
        return Err(NameError::ControlChar);
    }
    if name.starts_with('.') {
        return Err(NameError::LeadingDot);
    }
    Ok(name.to_string())
}

/// Playlist names compare case-insensitively (Unicode lower case), because the default macOS
/// file system does: `road trip.m3u8` and `Road Trip.m3u8` are the same file there.
pub fn names_equal(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// `<dir>/<name>.m3u8`, or `None` when that file would not sit directly in `dir`. Every playlist
/// write and delete goes through this, so no playlist operation can reach the music library or
/// the other config files.
pub fn playlist_file_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(format!("{}.{}", name, PLAYLIST_EXT));
    (path.parent() == Some(dir)).then_some(path)
}

/// Queue metadata built from a playlist entry alone. Unlike `metadata::fallback_metadata` it does
/// not `stat` the file, so it never waits on a slow or unmounted share. Fields the entry does not
/// know stay empty or zero; the queue's background tag fetch resolves tracks that have no artist
/// or duration.
pub fn placeholder_metadata(entry: &PlaylistEntry) -> AudioMetadata {
    let filename = entry
        .path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_string());
    let format_name = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("UNKNOWN")
        .to_uppercase();
    AudioMetadata {
        filepath: entry.path.clone(),
        filename,
        title: entry.title.clone(),
        artist: entry.artist.clone(),
        format_name,
        duration_seconds: entry.duration_secs.max(0) as f64,
        ..Default::default()
    }
}

/// What to put in the queue for a playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuePlan {
    /// The entries whose file exists, in playlist order.
    pub tracks: Vec<AudioMetadata>,
    /// Position in `tracks` to start playing from; `None` when no track was selected or no file
    /// exists.
    pub start_index: Option<usize>,
    /// Entries left out because their file is missing.
    pub missing: usize,
}

/// Keep the entries whose `exists[i]` is true (an index past the end of `exists` counts as
/// missing), using `cached` metadata when the app has it and [`placeholder_metadata`] otherwise.
/// With a `selected` entry, playback starts at that entry if it exists, else at the next existing
/// entry after it, else at the first existing entry.
pub fn plan_queue(
    entries: &[PlaylistEntry],
    exists: &[bool],
    selected: Option<usize>,
    cached: &dyn Fn(&Path) -> Option<AudioMetadata>,
) -> QueuePlan {
    let mut tracks = Vec::new();
    // Existing entries before the selected one: the queue position of the selected entry if it
    // exists, otherwise of the next existing entry after it.
    let mut kept_before_selected = 0;
    for (i, entry) in entries.iter().enumerate() {
        if !exists.get(i).copied().unwrap_or(false) {
            continue;
        }
        if selected.is_some_and(|s| i < s) {
            kept_before_selected += 1;
        }
        tracks.push(cached(&entry.path).unwrap_or_else(|| placeholder_metadata(entry)));
    }
    let start_index = match selected {
        Some(_) if kept_before_selected < tracks.len() => Some(kept_before_selected),
        Some(_) if !tracks.is_empty() => Some(0),
        _ => None,
    };
    let missing = entries.len() - tracks.len();
    QueuePlan {
        tracks,
        start_index,
        missing,
    }
}
