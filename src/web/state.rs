//! The snapshot the UI thread publishes for the web server and the commands the web server sends back.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Individual track representation in the web queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebQueueItem {
    pub index: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_seconds: f64,
    pub filepath: String,
    pub is_current: bool,
}

/// Output device representation for web rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebDeviceItem {
    pub id: usize,
    pub name: String,
    pub host_port: String,
    pub is_active: bool,
}

/// Directory entry for filesystem browsing via web.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebBrowserEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// Full response payload for GET /api/browser.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebBrowserResponse {
    pub current_path: String,
    pub parent_path: Option<String>,
    pub bookmarks: Vec<String>,
    pub entries: Vec<WebBrowserEntry>,
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default)]
    pub terminal_dir: Option<String>,
}

/// One track of a playlist in the `GET /api/playlists` snapshot (issue #50).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebPlaylistEntry {
    pub index: usize,
    pub title: String,
    pub artist: String,
    /// Whole seconds, or -1 when unknown.
    pub duration_seconds: i64,
    /// The entry's path. Responses clear it (and set `outside`) when it lies outside the allowed
    /// folders, as `/api/status` does for queue entries.
    pub path: String,
    /// Set in responses for an entry whose path lies outside the allowed folders: the dashboard
    /// can't play a playlist holding one.
    #[serde(default)]
    pub outside: bool,
    /// What this session's existence check found: `Some(true)` missing, `Some(false)` found,
    /// `None` not checked yet. Checks run on workers.
    #[serde(default)]
    pub missing: Option<bool>,
}

/// One playlist in the `GET /api/playlists` snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebPlaylist {
    pub name: String,
    pub track_count: usize,
    /// Sum of the known durations, in seconds.
    pub total_duration_seconds: i64,
    pub entries: Vec<WebPlaylistEntry>,
}

/// Whether the playlists can be used yet (the store is read by a worker at startup).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebPlaylistsState {
    #[default]
    Loading,
    /// The playlists folder couldn't be listed; edits stay refused for the session.
    Failed,
    Ready,
}

/// The playlists as the UI thread last published them; `GET /api/playlists` serves this, never
/// the playlists folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebPlaylists {
    pub state: WebPlaylistsState,
    /// Why playlists can't be used yet (the TUI's message), `None` when ready.
    pub message: Option<String>,
    /// `PlaylistStore::revision` of this snapshot. Requests that name a track by index send it
    /// back, so an index into a list that changed meanwhile is refused.
    pub revision: u64,
    /// The playlist last added to, which the add picker highlights.
    pub last_playlist: Option<String>,
    pub playlists: Vec<WebPlaylist>,
}

impl Default for WebPlaylists {
    fn default() -> Self {
        Self {
            state: WebPlaylistsState::Loading,
            message: Some(crate::playlist_store::StoreError::NotLoaded.to_string()),
            revision: 0,
            last_playlist: None,
            playlists: Vec::new(),
        }
    }
}

/// Live playback state shared between Quickstream and the web server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlayingState {
    pub is_playing: bool,
    pub is_paused: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub format: String,
    pub duration_seconds: f64,
    pub position_seconds: f64,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    /// 0 when unknown (lofty couldn't read it, or tags not read yet).
    pub channels: u8,
    pub volume: u8,
    pub output_device: String,
    pub filepath: Option<PathBuf>,
    pub queue_index: usize,
    pub queue_total: usize,
    pub has_cover: bool,
    pub next_track_title: Option<String>,
    #[serde(default)]
    pub queue: Vec<WebQueueItem>,
    #[serde(default)]
    pub devices: Vec<WebDeviceItem>,
    #[serde(default)]
    pub queue_mode: String,
    #[serde(default)]
    pub is_scanning: bool,
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default)]
    pub current_browser_dir: String,
    #[serde(default)]
    pub is_autopilot: bool,
    #[serde(default)]
    pub autopilot_dir: Option<String>,
    #[serde(default)]
    pub active_mix_profile: String,
    #[serde(default)]
    pub bpm: Option<f64>,
    #[serde(default)]
    pub key: Option<String>,
    /// Folders the web dashboard may browse and act on (startup home, bookmarks, launch folder).
    /// Never sent to clients.
    #[serde(skip)]
    pub allowed_roots: Vec<PathBuf>,
    /// The bookmarks the UI holds, for `GET /api/browser`: the web thread never reads
    /// bookmarks.json. Never sent with `/api/status`.
    #[serde(skip)]
    pub bookmarks: Vec<String>,
    /// `Settings::web_remote_access` (Web Remote Access and the web server both on). While it is
    /// off, network peers get only `/stream/*` (`web_access::authorize`), even when a renderer has
    /// the server on the LAN.
    /// Off until the UI publishes it. Never sent to clients.
    #[serde(skip)]
    pub remote_access: bool,
    /// The playlists snapshot the UI thread publishes for `GET /api/playlists` (issue #50). Not
    /// part of `/api/status`, which is polled several times a second.
    #[serde(skip)]
    pub playlists: Arc<WebPlaylists>,
    /// Changes whenever `playlists` is republished, so the dashboard knows when to fetch it.
    #[serde(default)]
    pub playlists_version: u64,
}

impl Default for NowPlayingState {
    fn default() -> Self {
        Self {
            is_playing: false,
            is_paused: false,
            title: "No Track Loaded".to_string(),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            format: String::new(),
            duration_seconds: 0.0,
            position_seconds: 0.0,
            bitrate_kbps: 0,
            sample_rate_hz: 0,
            channels: 0,
            volume: 100,
            output_device: "Local Audio".to_string(),
            filepath: None,
            queue_index: 0,
            queue_total: 0,
            has_cover: false,
            next_track_title: None,
            queue: Vec::new(),
            devices: Vec::new(),
            queue_mode: "loop".to_string(),
            is_scanning: false,
            home_dir: None,
            current_browser_dir: String::new(),
            is_autopilot: false,
            autopilot_dir: None,
            active_mix_profile: "Diverse Artists Shuffle".to_string(),
            bpm: None,
            key: None,
            allowed_roots: Vec::new(),
            bookmarks: Vec::new(),
            remote_access: false,
            playlists: Arc::new(WebPlaylists::default()),
            playlists_version: 0,
        }
    }
}

/// Commands sent from the web interface to Quickstream's playback engine.
#[derive(Debug, Clone, PartialEq)]
pub enum WebCommand {
    TogglePause,
    Play,
    Pause,
    Next,
    Prev,
    SetVolume(u8),
    SeekRelative(f64),
    SeekTo(f64),
    PlayIndex(usize),
    RemoveIndex(usize),
    ClearQueue,
    AddFile(PathBuf),
    AddFolder(PathBuf),
    SelectDevice(usize),
    RescanDevices,
    SetQueueMode(String),
    SetHome(Option<String>),
    /// Start (or reshuffle) Autopilot in this folder: the one the request named, else the
    /// terminal's (`current_browser_dir`), resolved inside the allowed folders by the web thread
    /// (issue #102).
    Autopilot(PathBuf),
    AutopilotReshuffle(PathBuf),
    ShuffleQueue,
    SetMixProfile(String),
    SeedRadio(PathBuf),
    /// A web request named a path outside the allowed folders; it was ignored.
    PathRejected(PathBuf),
    /// Queue a playlist (issue #50): replace the queue and play from entry `play_from`, or
    /// append when `None`. `entries` are the snapshot's entries, every path already checked
    /// against the allowed folders by the web thread; the UI queues these, not the store's
    /// current list, so it never queues an entry the web thread did not check. The UI answers
    /// on `reply` whether it started checking them or refused (`Still checking …`), and the web
    /// request returns that answer.
    QueuePlaylist {
        name: String,
        entries: Vec<crate::playlist::PlaylistEntry>,
        play_from: Option<usize>,
        reply: WebReply,
    },
    /// The dashboard fetched the playlists: check their files on workers so missing ones are
    /// marked in the snapshot.
    CheckPlaylists,
    /// Change a playlist (issue #50). The UI applies it through `PlaylistStore` and answers on
    /// the reply channel; the web request returns that answer.
    EditPlaylist(PlaylistEdit, WebReply),
}

/// A playlist edit from the web dashboard. Names are matched ignoring case, like the TUI's.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaylistEdit {
    Create {
        name: String,
    },
    Rename {
        name: String,
        new_name: String,
    },
    Delete {
        name: String,
    },
    /// Add an audio file. `path` is the file as the request named it (absolute, normalized
    /// with [`crate::playlist::normalize_path`]), the form a terminal add stores, so the same file
    /// added from either side is a duplicate and its tags are found; when that form resolves to a
    /// different file than the one checked, `path` is `canonical` instead. `canonical` is the same file
    /// with symlinks resolved, which the web thread checked lies inside the allowed folders; a
    /// playlist already holding it counts as already holding the file.
    Add {
        name: String,
        path: PathBuf,
        canonical: PathBuf,
    },
    /// Remove track `index`. `revision` is the snapshot revision the index came from; the edit
    /// is refused when the playlists changed since. Requests without one are refused.
    Remove {
        name: String,
        index: usize,
        revision: u64,
    },
    /// Move track `index` one row (no wraparound). `revision` as for `Remove`.
    Move {
        name: String,
        index: usize,
        dir: crate::playlist_store::MoveDir,
        revision: u64,
    },
}

/// What the UI answers to a [`PlaylistEdit`]: the status message, or why it was refused.
pub type PlaylistReply = Result<String, String>;

/// Where the UI thread sends its answer to a web request that waits for one.
#[derive(Clone, Default)]
pub struct WebReply(Option<mpsc::Sender<PlaylistReply>>);

impl WebReply {
    /// A reply slot and the receiver its answer arrives on.
    pub fn channel() -> (Self, mpsc::Receiver<PlaylistReply>) {
        let (tx, rx) = mpsc::channel();
        (Self(Some(tx)), rx)
    }

    /// Answer the request; a no-op when nobody waits any more.
    pub fn send(&self, reply: PlaylistReply) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(reply);
        }
    }
}

impl std::fmt::Debug for WebReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebReply")
    }
}

/// The reply channel is not part of what a command asks for: commands that differ only in it
/// are equal.
impl PartialEq for WebReply {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
