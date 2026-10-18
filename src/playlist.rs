//! Playlists (issue #37): the extended M3U8 file format, playlist names, and turning a playlist
//! into queue tracks. Pure: nothing here reads or writes the disk, so it is safe on the UI thread.

use std::path::{Component, Path, PathBuf};

use crate::metadata::AudioMetadata;

/// Extension of a playlist file in the playlists folder.
pub const PLAYLIST_EXT: &str = "m3u8";

/// Longest playlist name [`validate_name`] accepts, in characters (not bytes).
pub const MAX_NAME_CHARS: usize = 100;

/// Longest playlist name [`validate_name`] accepts, in UTF-8 bytes. File names are limited to 255
/// bytes and the save writes `<name>.m3u8.<pid>.tmp`, so wide characters need their own limit.
pub const MAX_NAME_BYTES: usize = 200;

/// One track of a playlist. The path is the source of truth for the file; artist, title and
/// duration are only what the `#EXTINF` and `#EXTART` lines show.
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

    /// Extended M3U8 text: UTF-8, LF line endings, no BOM. Each path follows an `#EXTINF` line
    /// (`Artist - Title`, as other players show it) and an `#EXTART` line holding the artist,
    /// empty when there is none, so [`parse_m3u8`] can tell where the artist ends even when the
    /// artist or title contains ` - ` (issue #52). Artist and title are written trimmed, since
    /// the parser trims tag text.
    pub fn to_m3u8(&self) -> String {
        let mut out = String::from("#EXTM3U\n");
        out.push_str(&format!("#PLAYLIST:{}\n", single_line(&self.name)));
        for entry in &self.entries {
            let secs = entry.duration_secs.max(-1);
            let artist = single_line(&entry.artist);
            let artist = artist.trim();
            let title = single_line(&entry.title);
            let title = title.trim();
            let label = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{} - {}", artist, title)
            };
            out.push_str(&format!("#EXTINF:{},{}\n", secs, label));
            out.push_str(&format!("#EXTART:{}\n", artist));
            // M3U8 is UTF-8 text, so a path that is not valid UTF-8 is written lossily. The path is
            // written as is, spaces included.
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
/// are resolved against `base_dir`, the folder holding the playlist file, and every path is
/// normalized with [`normalize_path`] so a `..` is never written back. Any other line is kept
/// as a path entry (a Windows path or a `file://` URL simply shows up as missing). A path line
/// loses its line ending and leading whitespace only: a file name may end in spaces (issue #52).
/// Artist and title come from the entry's `#EXTINF` and `#EXTART` lines, as `entry_label` describes.
pub fn parse_m3u8(name: &str, text: &str, base_dir: &Path) -> Playlist {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut playlist = Playlist::new(name);
    // The `#EXTINF` duration and text, and the `#EXTART` artist, waiting for the path line they
    // describe.
    let mut pending: Option<(i64, String)> = None;
    let mut pending_artist: Option<String> = None;
    for raw in text.lines() {
        // `lines` drops `\n` and `\r\n`; a stray `\r` left before them is a line ending too.
        let line = raw.trim_end_matches('\r').trim_start();
        let tag = line.trim_end();
        if tag.is_empty() {
            continue;
        }
        if let Some(info) = strip_tag(tag, "#EXTINF:") {
            pending = Some(parse_extinf(info));
            continue;
        }
        if let Some(artist) = strip_tag(tag, "#EXTART:") {
            pending_artist = Some(artist.trim().to_string());
            continue;
        }
        if tag.starts_with('#') {
            continue;
        }
        // `join` keeps an absolute path unchanged and resolves a relative one against the folder.
        let path = normalize_path(&base_dir.join(line));
        let (duration_secs, label) = pending.take().unwrap_or((-1, String::new()));
        let (artist, title) = entry_label(&label, pending_artist.take().as_deref());
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

/// `path` with `.` components dropped and each `..` cancelling the component before it, worked
/// out from the text alone (issue #58): the disk is never read and symlinks are not followed, so
/// `link/..` becomes the folder holding `link` even when `link` points elsewhere. A `..` right
/// after the root is dropped, as the OS does; on a relative path a leading `..` with nothing to
/// cancel is kept. A relative path that cancels out entirely becomes `.`.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    // Normal components in `out` that a later `..` may cancel.
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !out.has_root() {
                    out.push(component);
                }
            }
            Component::Normal(name) => {
                out.push(name);
                depth += 1;
            }
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Component::CurDir);
    }
    out
}

/// The text after `tag` (such as `#EXTINF:`, matched case-insensitively), or `None` for any
/// other line.
fn strip_tag<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    match line.get(..tag.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(tag) => Some(&line[tag.len()..]),
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

/// (artist, title) of an entry from its `#EXTINF` text and `#EXTART` artist (issue #52). Our own
/// files always have both: an empty `#EXTART` means no artist and the whole text is the title;
/// otherwise the title is the text after `<artist> - `. Other players use `#EXTART` for the
/// album artist, so when the text does not start with `<artist> -` the `#EXTART` line is
/// ignored, and without one the text is split by [`split_label`] as it always was.
fn entry_label(label: &str, extart: Option<&str>) -> (String, String) {
    match extart {
        Some("") => (String::new(), label.to_string()),
        Some(artist) => {
            // The text is trimmed, so `<artist> - ` with an empty title reads `<artist> -`.
            let rest = label.strip_prefix(artist).and_then(|r| r.strip_prefix(" -"));
            match rest {
                Some(title) if title.is_empty() || title.starts_with(' ') => {
                    (artist.to_string(), title.trim().to_string())
                }
                _ => split_label(label),
            }
        }
        None => split_label(label),
    }
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
    /// Short enough in characters but over [`MAX_NAME_BYTES`] in UTF-8.
    TooManyBytes,
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
            NameError::TooManyBytes => write!(f, "Name is too long (max {} bytes)", MAX_NAME_BYTES),
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
    if name.len() > MAX_NAME_BYTES {
        return Err(NameError::TooManyBytes);
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

/// Playlist names compare the way the default macOS file system (APFS) compares file names:
/// ignoring case (full Unicode case folding, so `Straße` matches `Strasse`) and Unicode
/// normalization (`Zoë` typed as NFC matches `Zoë` pasted from a Finder file name as NFD). Two
/// names that open the same file there must compare equal, or a create would overwrite it.
pub fn names_equal(a: &str, b: &str) -> bool {
    name_key(a) == name_key(b)
}

/// The form [`names_equal`] compares: Unicode's canonical caseless key, NFD(casefold(NFD)).
pub fn name_key(name: &str) -> String {
    use caseless::Caseless;
    use unicode_normalization::UnicodeNormalization;
    name.nfd().default_case_fold().nfd().collect()
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
        // A playlist file can list any path. Only audio files may reach the queue, because queued
        // tracks are served over HTTP. The extension check does not touch the disk.
        if !exists.get(i).copied().unwrap_or(false) || !crate::metadata::is_supported_audio_file(&entry.path) {
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
