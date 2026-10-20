//! Embedded local Now Playing web server with live metadata, cover art, and remote control.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::metadata::{cover_image_mime, default_cover_svg, extract_cover_art};

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
    Autopilot(Option<PathBuf>),
    AutopilotReshuffle(Option<PathBuf>),
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

/// How long a web request waits for the UI thread's answer to a playlist edit or play. The UI handles
/// web commands every tick, so this only runs out when it is stuck.
const EDIT_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct ActionPayload {
    action: String,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<usize>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    /// Playlist name (playlist actions).
    #[serde(default)]
    name: Option<String>,
    /// [`WebPlaylists::revision`] the request was made against (playlist actions with an index).
    #[serde(default)]
    revision: Option<u64>,
    /// New name (`playlist_rename`).
    #[serde(default)]
    new_name: Option<String>,
    /// `up` or `down` (`playlist_move`).
    #[serde(default)]
    direction: Option<String>,
}

/// A JSON response for `/api/action`: status line and body.
struct ActionResponse {
    status: &'static str,
    body: serde_json::Value,
}

impl ActionResponse {
    fn ok() -> Self {
        Self {
            status: "200 OK",
            body: serde_json::json!({ "ok": true }),
        }
    }

    fn done(message: String) -> Self {
        Self {
            status: "200 OK",
            body: serde_json::json!({ "ok": true, "message": message }),
        }
    }

    fn refused(status: &'static str, error: impl Into<String>) -> Self {
        Self {
            status,
            body: serde_json::json!({ "ok": false, "error": error.into() }),
        }
    }
}

/// `playlist_create` / `_rename` / `_delete` / `_add` / `_remove` / `_move`: build the edit,
/// hand it to the UI thread and return its answer. A file to add is checked like `add_file`
/// (`allow` records a path outside the allowed folders and the request is refused with 403).
/// Name rules, duplicates and the loaded check are the store's, applied on the UI thread.
fn edit_playlist_action(
    payload: &ActionPayload,
    cmd_tx: &mpsc::Sender<WebCommand>,
    allow: &mut dyn FnMut(String) -> Option<PathBuf>,
) -> ActionResponse {
    let bad = |error: &str| ActionResponse::refused("400 Bad Request", error);
    let Some(name) = payload.name.clone() else {
        return bad(MISSING_NAME);
    };
    let index = || payload.index.ok_or_else(|| bad("Missing track index"));
    // An index names a track only in the list it was read from (PR #65 review).
    let revision = || payload.revision.ok_or_else(|| bad(MISSING_REVISION));
    let edit = match payload.action.as_str() {
        "playlist_create" => PlaylistEdit::Create { name },
        "playlist_rename" => match payload.new_name.clone() {
            Some(new_name) => PlaylistEdit::Rename { name, new_name },
            None => return bad("Missing new name"),
        },
        "playlist_delete" => PlaylistEdit::Delete { name },
        "playlist_add" => {
            let Some(requested) = payload.path.clone() else {
                return bad("Missing path");
            };
            let Some(canonical) = allow(requested.clone()) else {
                // Refused with 403 by the caller.
                return ActionResponse::ok();
            };
            if !canonical.is_file() || !crate::metadata::is_supported_audio_file(&canonical) {
                return bad("Not an audio file");
            }
            // `allow` only accepts absolute paths. Store the form a terminal add would, but only
            // when it is the file that was checked: after a symlinked folder, text-only `..`
            // removal can name a different file, possibly outside the allowed folders.
            let normalized = crate::playlist::normalize_path(Path::new(&requested));
            let path = match std::fs::canonicalize(&normalized) {
                Ok(resolved) if resolved == canonical => normalized,
                _ => canonical.clone(),
            };
            PlaylistEdit::Add { name, path, canonical }
        }
        "playlist_remove" => match index().and_then(|index| Ok((index, revision()?))) {
            Ok((index, revision)) => PlaylistEdit::Remove { name, index, revision },
            Err(response) => return response,
        },
        "playlist_move" => {
            let (index, revision) = match index().and_then(|index| Ok((index, revision()?))) {
                Ok(found) => found,
                Err(response) => return response,
            };
            let dir = match payload.direction.as_deref() {
                Some("up") => crate::playlist_store::MoveDir::Up,
                Some("down") => crate::playlist_store::MoveDir::Down,
                _ => return bad("Direction must be up or down"),
            };
            PlaylistEdit::Move {
                name,
                index,
                dir,
                revision,
            }
        }
        _ => return bad("Unknown playlist action"),
    };
    let (reply, answer) = WebReply::channel();
    if cmd_tx.send(WebCommand::EditPlaylist(edit, reply)).is_err() {
        return ActionResponse::refused("503 Service Unavailable", NO_ANSWER);
    }
    ui_answer(&answer)
}

/// The UI thread's answer to a playlist request: 200 with its message, 409 with its refusal, or
/// 503 when it didn't answer in time.
fn ui_answer(answer: &mpsc::Receiver<PlaylistReply>) -> ActionResponse {
    match answer.recv_timeout(EDIT_REPLY_TIMEOUT) {
        Ok(Ok(message)) => ActionResponse::done(message),
        Ok(Err(error)) => ActionResponse::refused("409 Conflict", error),
        Err(_) => ActionResponse::refused("503 Service Unavailable", NO_ANSWER),
    }
}

/// Error when the UI thread did not answer an edit in time (or has gone away).
const NO_ANSWER: &str = "Quickstream didn't answer; try again";

/// Status for a playlist request whose index refers to a list that changed meanwhile.
pub const PLAYLISTS_CHANGED: &str = "Playlists changed; try again";

/// Error for a playlist request that names a track by index without the snapshot revision the
/// index came from.
pub const MISSING_REVISION: &str = "Missing revision: reload the playlists and try again";

/// Error for a playlist action that names no playlist.
const MISSING_NAME: &str = "Missing playlist name";

/// Whether a playlist entry may be queued from the web: its file resolves inside the allowed
/// folders, or it doesn't exist (nothing to follow) and its path lies inside them; the queue
/// skips missing files. Anything else, including a symlink leading out, is outside. Touches the
/// disk: web thread only.
fn playlist_entry_allowed(
    path: &Path,
    allowed: &crate::web_access::AllowedRoots,
    visible: &crate::web_access::VisibleRoots,
) -> bool {
    if allowed.resolve(path).is_some() {
        return true;
    }
    std::fs::symlink_metadata(path).is_err() && visible.contains(path)
}

/// `playlist_play` / `playlist_append`: check the snapshot's playlist and every entry's path on
/// this (web) thread, then hand the entries to the UI as [`WebCommand::QueuePlaylist`]. An entry
/// outside the allowed folders refuses the whole request, like `add_file`.
fn queue_playlist_action(
    payload: &ActionPayload,
    play: bool,
    state: &Arc<RwLock<NowPlayingState>>,
    cmd_tx: &mpsc::Sender<WebCommand>,
    rejected: &mut Option<PathBuf>,
) -> ActionResponse {
    let (snapshot, roots) = {
        let s = state.read().unwrap();
        (s.playlists.clone(), s.allowed_roots.clone())
    };
    if snapshot.state != WebPlaylistsState::Ready {
        let message = snapshot.message.clone().unwrap_or_default();
        return ActionResponse::refused("409 Conflict", message);
    }
    let Some(name) = payload.name.as_deref() else {
        return ActionResponse::refused("400 Bad Request", MISSING_NAME);
    };
    let Some(playlist) = snapshot
        .playlists
        .iter()
        .find(|p| crate::playlist::names_equal(&p.name, name))
    else {
        let missing = crate::playlist_store::StoreError::NotFound(name.to_string());
        return ActionResponse::refused("404 Not Found", missing.to_string());
    };
    // Playing from an index needs the revision it came from (PR #65 review).
    if payload.index.is_some() && payload.revision.is_none() {
        return ActionResponse::refused("400 Bad Request", MISSING_REVISION);
    }
    if payload.revision.is_some_and(|r| r != snapshot.revision) {
        return ActionResponse::refused("409 Conflict", PLAYLISTS_CHANGED);
    }
    if playlist.entries.is_empty() {
        return ActionResponse::refused("409 Conflict", format!("{} is empty", playlist.name));
    }
    let play_from = play.then(|| payload.index.unwrap_or(0));
    if play_from.is_some_and(|i| i >= playlist.entries.len()) {
        let error = crate::playlist_store::StoreError::IndexOutOfRange;
        return ActionResponse::refused("400 Bad Request", error.to_string());
    }
    // The roots are canonicalized once for the whole playlist, not once per entry.
    let allowed = crate::web_access::AllowedRoots::new(&roots);
    let visible = crate::web_access::VisibleRoots::new(&roots);
    if let Some(outside) = playlist
        .entries
        .iter()
        .map(|e| PathBuf::from(&e.path))
        .find(|p| !playlist_entry_allowed(p, &allowed, &visible))
    {
        *rejected = Some(outside);
        return ActionResponse::ok();
    }
    let entries = playlist
        .entries
        .iter()
        .map(|e| crate::playlist::PlaylistEntry {
            path: PathBuf::from(&e.path),
            duration_secs: e.duration_seconds,
            artist: e.artist.clone(),
            title: e.title.clone(),
        })
        .collect();
    // Wait for the UI's answer: it may refuse (`Still checking …`), and the dashboard must not
    // report a play that never started.
    let (reply, answer) = WebReply::channel();
    let command = WebCommand::QueuePlaylist {
        name: playlist.name.clone(),
        entries,
        play_from,
        reply,
    };
    if cmd_tx.send(command).is_err() {
        return ActionResponse::refused("503 Service Unavailable", NO_ANSWER);
    }
    ui_answer(&answer)
}

/// `GET /api/playlists`: the published snapshot, with paths outside the allowed folders
/// cleared as `/api/status` does.
fn playlists_response(state: &Arc<RwLock<NowPlayingState>>) -> String {
    let (snapshot, roots) = {
        let s = state.read().unwrap();
        (s.playlists.clone(), s.allowed_roots.clone())
    };
    let visible = crate::web_access::VisibleRoots::new(&roots);
    let mut out = (*snapshot).clone();
    for entry in out.playlists.iter_mut().flat_map(|p| p.entries.iter_mut()) {
        if !visible.contains(Path::new(&entry.path)) {
            entry.path.clear();
            entry.outside = true;
        }
    }
    serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
}

/// An armed continuous stream: the renderer must request `/stream/continuous-<nonce>.wav`.
#[derive(Debug)]
struct ContinuousSession {
    nonce: String,
    /// The first peer that connected; other hosts are refused for the rest of the session.
    peer: Option<IpAddr>,
}

/// Whether a request for a continuous stream path may be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuousAdmission {
    /// Serve the stream for the session with this nonce.
    Admitted(String),
    /// No session is armed, or the path does not name it (404).
    NotFound,
    /// The session belongs to another host (409).
    Conflict,
}

const CONTINUOUS_PREFIX: &str = "/stream/continuous-";
const CONTINUOUS_SUFFIX: &str = ".wav";

/// The nonce in a `/stream/continuous-<32 hex>.wav` route, if `route` has that shape.
fn continuous_route_nonce(route: &str) -> Option<&str> {
    let nonce = route.strip_prefix(CONTINUOUS_PREFIX)?.strip_suffix(CONTINUOUS_SUFFIX)?;
    (nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit())).then_some(nonce)
}

/// Whether `route` (path without query) addresses the continuous stream, armed or not.
fn is_continuous_route(route: &str) -> bool {
    route == "/stream/continuous.wav" || route == "/stream/autopilot.wav" || continuous_route_nonce(route).is_some()
}

/// Controls continuous PCM gapless streaming for DLNA renderers.
#[derive(Debug, Default)]
pub struct ContinuousStreamController {
    pub is_active: Arc<AtomicBool>,
    pub active_conn_id: Arc<AtomicU64>,
    pub current_track_index: Arc<Mutex<usize>>,
    pub current_position: Arc<Mutex<f64>>,
    pub current_duration: Arc<Mutex<f64>>,
    pub skip_requested: Arc<AtomicBool>,
    pub target_index: Arc<Mutex<Option<usize>>>,
    pub seek_requested: Arc<AtomicBool>,
    pub target_seek: Arc<Mutex<Option<f64>>>,
    pub active_child: Arc<Mutex<Option<std::process::Child>>>,
    session: Mutex<Option<ContinuousSession>>,
    /// The track being decoded: its queue index and path. Set with `current_duration` held, so
    /// the two always name the same track (lock order: the web state, then `current_duration`,
    /// then this).
    decoding: Mutex<Option<(usize, PathBuf)>>,
}

impl ContinuousStreamController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new continuous session (replacing any previous one) and return the URL path the
    /// renderer must request.
    pub fn arm_session(&self) -> String {
        let nonce = crate::web_access::generate_token();
        let path = format!("{}{}{}", CONTINUOUS_PREFIX, nonce, CONTINUOUS_SUFFIX);
        *self.session.lock().unwrap() = Some(ContinuousSession { nonce, peer: None });
        path
    }

    /// End the continuous session: its path stops being served.
    pub fn disarm_session(&self) {
        *self.session.lock().unwrap() = None;
    }

    /// The URL path of the armed session, if any.
    pub fn session_path(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| format!("{}{}{}", CONTINUOUS_PREFIX, s.nonce, CONTINUOUS_SUFFIX))
    }

    /// Decide whether `peer` may stream `route` (a path without query). The first admitted peer
    /// owns the session.
    pub fn admit(&self, route: &str, peer: IpAddr) -> ContinuousAdmission {
        let Some(requested) = continuous_route_nonce(route) else {
            return ContinuousAdmission::NotFound;
        };
        let mut guard = self.session.lock().unwrap();
        let Some(session) = guard.as_mut() else {
            return ContinuousAdmission::NotFound;
        };
        if !crate::web_access::constant_time_eq(requested, &session.nonce) {
            return ContinuousAdmission::NotFound;
        }
        match session.peer {
            Some(owner) if owner != peer => ContinuousAdmission::Conflict,
            _ => {
                session.peer = Some(peer);
                ContinuousAdmission::Admitted(session.nonce.clone())
            }
        }
    }

    /// Whether the session with `nonce` is still the armed one.
    fn is_session(&self, nonce: &str) -> bool {
        self.session.lock().unwrap().as_ref().is_some_and(|s| s.nonce == nonce)
    }

    /// If the length of the track being decoded (`path`, at `index` in the queue) was unknown
    /// when it started (placeholder metadata, tags not read yet), take it from `state`'s queue
    /// once the UI has published it there. Called while streaming, so the length (and the seek
    /// clamp that needs it) shows without waiting for the next track. Returns whether it was
    /// learned now.
    pub fn learn_duration(&self, state: &mut NowPlayingState, index: usize, path: &Path) -> bool {
        let current = self.current_duration.lock().unwrap();
        Self::learn_duration_locked(current, state, index, path)
    }

    /// The stream has started `index` (`path`) with `duration` (0 when unknown): set both
    /// together, so a length learned from the published queue is always the one of the track
    /// being decoded.
    fn start_track(&self, index: usize, path: &Path, duration: f64) {
        let mut current = self.current_duration.lock().unwrap();
        *current = duration;
        *self.decoding.lock().unwrap() = Some((index, path.to_path_buf()));
    }

    /// Whoever publishes the queue into `state` (the UI's `update_web_state`) offers the stream
    /// the lengths it now knows: if the track being decoded started with an unknown length and
    /// `state`'s queue has it at the same index and path, it is learned now, whatever the stream's
    /// socket is doing (a renderer reading at real time can keep the stream's writer blocked
    /// for seconds). Never blocks on the socket. Returns whether it was learned now.
    pub fn learn_published_duration(&self, state: &mut NowPlayingState) -> bool {
        if !self.is_active.load(Ordering::SeqCst) {
            return false;
        }
        let current = self.current_duration.lock().unwrap();
        let Some((index, path)) = self.decoding.lock().unwrap().clone() else {
            return false;
        };
        Self::learn_duration_locked(current, state, index, &path)
    }

    fn learn_duration_locked(
        mut current: std::sync::MutexGuard<'_, f64>,
        state: &mut NowPlayingState,
        index: usize,
        path: &Path,
    ) -> bool {
        if *current > 0.0 {
            return false;
        }
        let known = state
            .queue
            .get(index)
            .filter(|t| Path::new(&t.filepath) == path)
            .map(|t| t.duration_seconds)
            .filter(|d| *d > 0.0);
        let Some(duration) = known else {
            return false;
        };
        *current = duration;
        state.duration_seconds = duration;
        true
    }

    pub fn skip_to_index(&self, index: usize) {
        *self.target_index.lock().unwrap() = Some(index);
        self.skip_requested.store(true, Ordering::SeqCst);
        self.kill_active_decoder();
    }

    pub fn seek_to(&self, position: f64) {
        *self.target_seek.lock().unwrap() = Some(position);
        self.seek_requested.store(true, Ordering::SeqCst);
        self.kill_active_decoder();
    }

    pub fn kill_active_decoder(&self) {
        if let Some(mut child) = self.active_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Manages registered media files and active streaming TCP connections.
pub struct StreamServerState {
    pub active_sockets: Mutex<HashMap<u64, TcpStream>>,
    pub file_registry: RwLock<HashMap<String, PathBuf>>,
    pub continuous: Arc<ContinuousStreamController>,
    next_conn_id: AtomicU64,
}

impl Default for StreamServerState {
    fn default() -> Self {
        Self {
            active_sockets: Mutex::new(HashMap::new()),
            file_registry: RwLock::new(HashMap::new()),
            continuous: Arc::new(ContinuousStreamController::new()),
            next_conn_id: AtomicU64::new(1),
        }
    }
}

impl StreamServerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a file on disk so that /stream/<filename> resolves to its exact path.
    pub fn register_file(&self, path: &Path) {
        // Lossy so names that are not valid UTF-8 match the lossily encoded URLs built by upnp.rs.
        if let Some(filename) = path.file_name().map(|f| f.to_string_lossy()) {
            let mut reg = self.file_registry.write().unwrap();
            reg.insert(filename.to_string(), path.to_path_buf());
            reg.insert(filename.to_lowercase(), path.to_path_buf());
        }
    }

    /// Look up a file by exact or case-insensitive filename.
    pub fn lookup_file(&self, name: &str) -> Option<PathBuf> {
        let reg = self.file_registry.read().unwrap();
        if let Some(p) = reg.get(name) {
            return Some(p.clone());
        }
        let lower = name.to_lowercase();
        if let Some(p) = reg.get(&lower) {
            return Some(p.clone());
        }
        None
    }

    /// Register an active streaming socket clone for clean abort/termination.
    pub fn register_socket(&self, stream: &TcpStream) -> Option<u64> {
        if let Ok(cloned) = stream.try_clone() {
            let id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
            self.active_sockets.lock().unwrap().insert(id, cloned);
            Some(id)
        } else {
            None
        }
    }

    /// Untrack an active streaming socket when the request finishes.
    pub fn unregister_socket(&self, id: u64) {
        self.active_sockets.lock().unwrap().remove(&id);
    }

    /// Start a continuous session; see [`ContinuousStreamController::arm_session`].
    pub fn arm_continuous_session(&self) -> String {
        self.continuous.arm_session()
    }

    /// The URL path of the armed continuous session, if any.
    pub fn continuous_session_path(&self) -> Option<String> {
        self.continuous.session_path()
    }

    /// Immediately abort and close all active streaming TCP connections to stereos, and end any
    /// continuous session.
    pub fn close_active_streams(&self) {
        self.continuous.disarm_session();
        self.continuous.is_active.store(false, Ordering::SeqCst);
        self.continuous.kill_active_decoder();
        let mut sockets = self.active_sockets.lock().unwrap();
        let count = sockets.len();
        if count > 0 {
            crate::log_info!("web", "Closing {} active streaming TCP socket(s)", count);
            for (_, sock) in sockets.drain() {
                let _ = sock.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

struct SocketGuard {
    stream_state: Arc<StreamServerState>,
    conn_id: Option<u64>,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(id) = self.conn_id {
            self.stream_state.unregister_socket(id);
        }
    }
}

/// Network interfaces the web server listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebBind {
    /// 127.0.0.1 only: reachable from this computer.
    Loopback,
    /// All interfaces: reachable from other devices on the network. Needed while a DLNA
    /// renderer streams from this server.
    Lan,
}

/// Handle to the background web server process.
pub struct WebServerHandle {
    pub port: u16,
    /// Interfaces the server actually bound to.
    pub bind: WebBind,
    stop_flag: Arc<AtomicBool>,
    stream_state: Arc<StreamServerState>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl WebServerHandle {
    pub fn stream_state(&self) -> Arc<StreamServerState> {
        self.stream_state.clone()
    }

    pub fn close_active_streams(&self) {
        self.stream_state.close_active_streams();
    }

    pub fn register_stream_file(&self, path: &Path) {
        self.stream_state.register_file(path);
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stream_state.close_active_streams();
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WebServerHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.stream_state.close_active_streams();
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

/// Resource limits for the embedded HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLimits {
    /// Connections allowed at once while reading a request or serving a non-stream route.
    pub max_connections: usize,
    /// `/stream/...` responses allowed at once. Streams are long-lived, so they move out of the
    /// `max_connections` pool once the route is known.
    pub max_streams: usize,
    /// Total time allowed to receive the request line, headers and body (not per read).
    pub request_timeout: Duration,
    /// Longest request line or header line, in bytes.
    pub max_line_bytes: usize,
    /// Most header fields in one request.
    pub max_headers: usize,
    /// Most bytes in the request line plus headers.
    pub max_head_bytes: usize,
    /// Largest request body (`Content-Length`), in bytes.
    pub max_body_bytes: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_streams: 16,
            request_timeout: Duration::from_secs(10),
            max_line_bytes: 8 * 1024,
            max_headers: 100,
            max_head_bytes: 64 * 1024,
            max_body_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermitKind {
    Request,
    Stream,
}

/// Counts live connection handlers so a flood of connections cannot spawn unbounded threads.
struct ConnectionLimiter {
    limits: ServerLimits,
    requests: AtomicUsize,
    streams: AtomicUsize,
}

impl ConnectionLimiter {
    fn new(limits: ServerLimits) -> Self {
        Self {
            limits,
            requests: AtomicUsize::new(0),
            streams: AtomicUsize::new(0),
        }
    }

    fn counter(&self, kind: PermitKind) -> &AtomicUsize {
        match kind {
            PermitKind::Request => &self.requests,
            PermitKind::Stream => &self.streams,
        }
    }

    fn try_acquire(self: &Arc<Self>, kind: PermitKind) -> Option<ConnectionPermit> {
        let max = match kind {
            PermitKind::Request => self.limits.max_connections,
            PermitKind::Stream => self.limits.max_streams,
        };
        self.counter(kind)
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1))
            .ok()?;
        Some(ConnectionPermit {
            limiter: self.clone(),
            kind,
        })
    }
}

/// A slot in one of the limiter's pools, released on drop.
struct ConnectionPermit {
    limiter: Arc<ConnectionLimiter>,
    kind: PermitKind,
}

impl ConnectionPermit {
    /// Move a request-pool slot to the stream pool; returns `self` unchanged if the stream pool is full.
    fn into_stream(self) -> Result<ConnectionPermit, ConnectionPermit> {
        match self.limiter.try_acquire(PermitKind::Stream) {
            Some(stream_permit) => Ok(stream_permit),
            None => Err(self),
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.limiter.counter(self.kind).fetch_sub(1, Ordering::AcqRel);
    }
}

/// Error statuses the server sends before routing a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    BadRequest,
    RequestTimeout,
    PayloadTooLarge,
    UriTooLong,
    HeadersTooLarge,
    Busy,
}

impl Rejection {
    fn status_line(self) -> &'static str {
        match self {
            Rejection::BadRequest => "400 Bad Request",
            Rejection::RequestTimeout => "408 Request Timeout",
            Rejection::PayloadTooLarge => "413 Content Too Large",
            Rejection::UriTooLong => "414 URI Too Long",
            Rejection::HeadersTooLarge => "431 Request Header Fields Too Large",
            Rejection::Busy => "503 Service Unavailable",
        }
    }

    fn response(self) -> String {
        format!(
            "HTTP/1.1 {}\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
            self.status_line(),
            if self == Rejection::Busy {
                "Retry-After: 1\r\n"
            } else {
                ""
            }
        )
    }
}

/// Answer a connection the accept loop has no slot for. Runs on the accept thread, so it never
/// waits on the client: the response fits in a fresh socket's send buffer.
fn refuse_busy_connection(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.write_all(Rejection::Busy.response().as_bytes());
    let _ = stream.flush();
}

/// Send an error response, then drain unread request bytes for a moment before closing: closing
/// with unread data makes the kernel send a reset, which can discard the response at the client.
fn reject_request(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, rejection: Rejection) {
    let _ = stream.write_all(rejection.response().as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);

    const DRAIN_TIME: Duration = Duration::from_millis(500);
    const DRAIN_BYTES: usize = 256 * 1024;
    let deadline = Instant::now() + DRAIN_TIME;
    let mut drained = 0usize;
    let mut buf = [0u8; 8192];
    while drained < DRAIN_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || reader.get_ref().set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

/// Request line and headers of an incoming request.
struct RequestHead {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    content_length: usize,
}

impl RequestHead {
    /// Value of the first header named `name` (case-insensitive).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

enum LineError {
    /// The peer closed the connection or the socket failed.
    Closed,
    TooLong,
    Timeout,
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// Set the socket read timeout to the time left before `deadline`.
fn arm_read_deadline(stream: &TcpStream, deadline: Instant) -> Result<(), LineError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(LineError::Timeout);
    }
    stream.set_read_timeout(Some(remaining)).map_err(|_| LineError::Closed)
}

/// Read one line (including its `\n`) of at most `max` bytes, finishing before `deadline`.
fn read_line_bounded(reader: &mut BufReader<TcpStream>, deadline: Instant, max: usize) -> Result<Vec<u8>, LineError> {
    let mut line = Vec::new();
    loop {
        arm_read_deadline(reader.get_ref(), deadline)?;
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout(&e) => return Err(LineError::Timeout),
            Err(_) => return Err(LineError::Closed),
        };
        if available.is_empty() {
            return Err(LineError::Closed);
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if line.len() + take > max {
            return Err(LineError::TooLong);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if done {
            return Ok(line);
        }
    }
}

/// Read the request line and headers within the configured limits. `Ok(None)` means the client
/// closed the connection before sending a request line.
fn read_request_head(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
    limits: &ServerLimits,
) -> Result<Option<RequestHead>, Rejection> {
    let request_line = match read_line_bounded(reader, deadline, limits.max_line_bytes) {
        Ok(line) => line,
        Err(LineError::Closed) => return Ok(None),
        Err(LineError::TooLong) => return Err(Rejection::UriTooLong),
        Err(LineError::Timeout) => return Err(Rejection::RequestTimeout),
    };
    let mut head_bytes = request_line.len();
    let request_line = std::str::from_utf8(&request_line).map_err(|_| Rejection::BadRequest)?;
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Err(Rejection::BadRequest);
    };
    let mut head = RequestHead {
        method: method.to_string(),
        path: path.to_string(),
        headers: Vec::new(),
        content_length: 0,
    };

    loop {
        let line = match read_line_bounded(reader, deadline, limits.max_line_bytes) {
            Ok(line) => line,
            // A client that half-closes after its last header still gets an answer.
            Err(LineError::Closed) => break,
            Err(LineError::TooLong) => return Err(Rejection::HeadersTooLarge),
            Err(LineError::Timeout) => return Err(Rejection::RequestTimeout),
        };
        head_bytes += line.len();
        if head_bytes > limits.max_head_bytes {
            return Err(Rejection::HeadersTooLarge);
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            break;
        }
        if head.headers.len() >= limits.max_headers {
            return Err(Rejection::HeadersTooLarge);
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(Rejection::BadRequest);
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Rejection::BadRequest);
            }
            // Digits only, so a parse failure means the value overflowed usize.
            let declared = value.parse::<usize>().map_err(|_| Rejection::PayloadTooLarge)?;
            if declared > limits.max_body_bytes {
                return Err(Rejection::PayloadTooLarge);
            }
            head.content_length = declared;
        }
        head.headers.push((name.to_string(), value.to_string()));
    }
    Ok(Some(head))
}

/// Read a request body of `len` bytes (already checked against the body limit) before `deadline`.
fn read_request_body(reader: &mut BufReader<TcpStream>, len: usize, deadline: Instant) -> Option<Vec<u8>> {
    let mut body = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        arm_read_deadline(reader.get_ref(), deadline).ok()?;
        match reader.read(&mut body[filled..]) {
            Ok(0) => return None,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(body)
}

pub struct WebServer;

impl WebServer {
    /// Start the local web server on loopback on the requested port (or fallback to an available port).
    pub fn start(
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_bind(WebBind::Loopback, port, state, cmd_tx)
    }

    /// Start the local web server on the given interfaces.
    pub fn start_with_bind(
        bind: WebBind,
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_stream_state(bind, port, state, cmd_tx, Arc::new(StreamServerState::new()))
    }

    /// Start the local web server sharing an existing StreamServerState.
    pub fn start_with_stream_state(
        bind: WebBind,
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
        stream_state: Arc<StreamServerState>,
    ) -> anyhow::Result<WebServerHandle> {
        Self::start_with_options(bind, port, state, cmd_tx, stream_state, ServerLimits::default())
    }

    /// Start the local web server with a shared StreamServerState and explicit resource limits.
    pub fn start_with_options(
        bind: WebBind,
        port: u16,
        state: Arc<RwLock<NowPlayingState>>,
        cmd_tx: mpsc::Sender<WebCommand>,
        stream_state: Arc<StreamServerState>,
        limits: ServerLimits,
    ) -> anyhow::Result<WebServerHandle> {
        let candidate_ports = [port, 8080, 8090, 8091, 0];
        let mut listener_opt = None;
        // Never fall back to the other interface: if another app holds *:port, binding
        // 127.0.0.1:port still succeeds on macOS, and renderers' LAN requests would reach
        // that other app. Move to the next candidate port instead.
        let host = match bind {
            WebBind::Lan => "0.0.0.0",
            WebBind::Loopback => "127.0.0.1",
        };
        for &p in &candidate_ports {
            if let Ok(l) = TcpListener::bind(format!("{}:{}", host, p)) {
                listener_opt = Some(l);
                break;
            }
        }
        let listener = listener_opt.ok_or_else(|| anyhow::anyhow!("Failed to bind web server to any port"))?;
        let actual_bind = bind;
        let actual_port = listener.local_addr()?.port();
        crate::log_info!("web", "Web server listening on {}", listener.local_addr()?);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_thread = stop_flag.clone();
        let stream_state_thread = stream_state.clone();
        let limiter = Arc::new(ConnectionLimiter::new(limits));

        // Non-blocking accept so the loop sees the stop flag within one poll interval. A wake-up
        // connection to 127.0.0.1:port is not reliable: another listener bound to 127.0.0.1 on
        // the same port (allowed next to a 0.0.0.0 bind) would receive it instead.
        listener.set_nonblocking(true)?;
        let join_handle = std::thread::Builder::new()
            .name("QuickstreamWebServer".to_string())
            .spawn(move || {
                while !stop_flag_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if stop_flag_thread.load(Ordering::SeqCst) {
                                break;
                            }
                            // Accepted sockets inherit O_NONBLOCK on macOS/BSD.
                            let _ = stream.set_nonblocking(false);
                            let Some(permit) = limiter.try_acquire(PermitKind::Request) else {
                                crate::log_warn!(
                                    "web",
                                    "Connection limit ({}) reached; refusing {}",
                                    limits.max_connections,
                                    peer
                                );
                                refuse_busy_connection(stream);
                                continue;
                            };
                            let state_clone = state.clone();
                            let cmd_tx_clone = cmd_tx.clone();
                            let stream_state_clone = stream_state_thread.clone();
                            let spawned = std::thread::Builder::new()
                                .name("QuickstreamWebConn".to_string())
                                .spawn(move || {
                                    let _ = handle_client(
                                        stream,
                                        permit,
                                        &limits,
                                        &state_clone,
                                        &cmd_tx_clone,
                                        &stream_state_clone,
                                    );
                                });
                            if let Err(e) = spawned {
                                // The closure (socket and permit) is dropped with the error.
                                crate::log_warn!("web", "Failed to spawn connection handler for {}: {}", peer, e);
                            }
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })?;

        Ok(WebServerHandle {
            port: actual_port,
            bind: actual_bind,
            stop_flag,
            stream_state,
            join_handle: Some(join_handle),
        })
    }
}

fn handle_client(
    mut stream: TcpStream,
    permit: ConnectionPermit,
    limits: &ServerLimits,
    state: &Arc<RwLock<NowPlayingState>>,
    cmd_tx: &mpsc::Sender<WebCommand>,
    stream_state: &Arc<StreamServerState>,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(4)))?;

    // One deadline for the whole request (line, headers and body), so a client cannot hold the
    // connection open by sending a byte at a time.
    let deadline = Instant::now() + limits.request_timeout;
    let mut reader = BufReader::new(stream.try_clone()?);
    let head = match read_request_head(&mut reader, deadline, limits) {
        Ok(Some(head)) => head,
        Ok(None) => return Ok(()),
        Err(rejection) => {
            reject_request(&mut stream, &mut reader, rejection);
            return Ok(());
        }
    };

    let method = head.method.as_str();
    let path = head.path.as_str();
    // The path without its query string or fragment.
    let route = path.split(['?', '#']).next().unwrap_or(path);
    let content_length = head.content_length;
    let range_header = head.header("range").map(str::to_string);
    let mut auth_headers = crate::web_access::AuthHeaders::default();
    for (name, value) in &head.headers {
        auth_headers.observe(&format!("{}: {}", name, value));
    }

    // Streams can run for hours: move them to their own pool so they do not use up the slots
    // for short requests.
    let _permit = if (method == "GET" || method == "HEAD") && path.starts_with("/stream") {
        match permit.into_stream() {
            Ok(stream_permit) => stream_permit,
            Err(_request_permit) => {
                crate::log_warn!(
                    "web",
                    "Stream limit ({}) reached; refusing {}",
                    limits.max_streams,
                    path
                );
                reject_request(&mut stream, &mut reader, Rejection::Busy);
                return Ok(());
            }
        }
    } else {
        permit
    };

    // Read from memory per request, so toggling the setting applies to the next request.
    let remote_access = state.read().is_ok_and(|s| s.remote_access);
    if !crate::web_access::check_request(&mut stream, method, path, &auth_headers, remote_access)? {
        return Ok(());
    }

    // Route dispatch
    if method == "GET" || method == "HEAD" {
        let is_head = method == "HEAD";
        if route == "/" || route == "/index.html" {
            // Serve single-page dashboard
            let body = NOW_PLAYING_HTML.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
        } else if path.starts_with("/api/status") {
            // Serve JSON playback status
            let mut current = {
                let s = state.read().unwrap();
                s.clone()
            };
            // Only reveal where the terminal is browsing when the dashboard may go there too.
            if !current.current_browser_dir.is_empty()
                && crate::web_access::resolve_allowed_path(
                    Path::new(&current.current_browser_dir),
                    &current.allowed_roots,
                )
                .is_none()
            {
                current.current_browser_dir.clear();
            }
            // Likewise for track and folder paths outside the allowed folders: queue entries lose
            // their path, the current track keeps only its file name.
            let visible = crate::web_access::VisibleRoots::new(&current.allowed_roots);
            for item in &mut current.queue {
                if !visible.contains(Path::new(&item.filepath)) {
                    item.filepath.clear();
                }
            }
            if let Some(ref fp) = current.filepath {
                if !visible.contains(fp) {
                    current.filepath = fp.file_name().map(PathBuf::from);
                }
            }
            if current
                .autopilot_dir
                .as_deref()
                .is_some_and(|d| !visible.contains(Path::new(d)))
            {
                current.autopilot_dir = None;
            }
            let json = serde_json::to_string(&current).unwrap_or_else(|_| "{}".to_string());
            let body = json.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
        } else if route == "/api/playlists" {
            // The snapshot the UI thread published; the playlists folder is never read here.
            let json = playlists_response(state);
            let body = json.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
            // Someone is looking at the playlists: have their files checked on workers.
            let _ = cmd_tx.send(WebCommand::CheckPlaylists);
        } else if path.starts_with("/api/cover") {
            // Serve cover art (JPEG/PNG/WebP/GIF) or SVG placeholder. `extract_cover_art` only
            // returns allowlisted MIME types; re-check here so a tag string never reaches the
            // response head.
            let filepath = {
                let s = state.read().unwrap();
                s.filepath.clone()
            };

            let art_opt = filepath
                .as_deref()
                .and_then(extract_cover_art)
                .and_then(|(bytes, mime)| cover_image_mime(Some(&mime), &bytes).map(|m| (bytes, m)));
            if let Some((bytes, mime)) = art_opt {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: public, max-age=10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    mime,
                    bytes.len()
                );
                stream.write_all(header.as_bytes())?;
                if !is_head {
                    stream.write_all(&bytes)?;
                }
                stream.flush()?;
            } else {
                let svg = default_cover_svg().as_bytes();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nX-Content-Type-Options: nosniff\r\nCache-Control: public, max-age=10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    svg.len()
                );
                stream.write_all(header.as_bytes())?;
                if !is_head {
                    stream.write_all(svg)?;
                }
                stream.flush()?;
            }
        } else if path.starts_with("/api/browser") {
            let requested_path = if let Some(query_idx) = path.find('?') {
                let query_str = &path[query_idx + 1..];
                let mut p_val = None;
                for pair in query_str.split('&') {
                    let mut kv = pair.splitn(2, '=');
                    if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
                        if k == "path" {
                            p_val = Some(crate::player::upnp::url_decode(v));
                            break;
                        }
                    }
                }
                p_val
            } else {
                None
            };

            let (current_home_dir, current_terminal_dir, roots, bookmarks) = {
                let s = state.read().unwrap();
                (
                    s.home_dir.clone(),
                    s.current_browser_dir.clone(),
                    s.allowed_roots.clone(),
                    s.bookmarks.clone(),
                )
            };
            // Only folders inside the allowed roots (startup home, bookmarks, launch folder).
            let allowed_dir = |p: &Path| crate::web_access::resolve_allowed_path(p, &roots).filter(|d| d.is_dir());
            let terminal_dir = if current_terminal_dir.is_empty() {
                None
            } else {
                allowed_dir(Path::new(&current_terminal_dir)).map(|_| current_terminal_dir.clone())
            };
            let target_opt = match requested_path {
                Some(ref p) => allowed_dir(Path::new(p)),
                None => terminal_dir
                    .as_deref()
                    .and_then(|t| allowed_dir(Path::new(t)))
                    .or_else(|| current_home_dir.as_deref().and_then(|h| allowed_dir(Path::new(h))))
                    .or_else(|| roots.iter().find_map(|r| allowed_dir(r))),
            };
            let Some(target_dir) = target_opt else {
                crate::web_access::write_path_forbidden(&mut stream)?;
                return Ok(());
            };

            let parent_path = target_dir
                .parent()
                .and_then(allowed_dir)
                .map(|p| p.to_string_lossy().to_string());

            let mut dirs = Vec::new();
            let mut files = Vec::new();

            if let Ok(read_entries) = std::fs::read_dir(&target_dir) {
                for entry in read_entries.filter_map(|e| e.ok()) {
                    let entry_path = entry.path();
                    let filename = entry.file_name().to_string_lossy().to_string();
                    if filename.starts_with('.') {
                        continue;
                    }
                    if entry_path.is_dir() {
                        dirs.push(WebBrowserEntry {
                            name: filename,
                            path: entry_path.to_string_lossy().to_string(),
                            is_dir: true,
                        });
                    } else if crate::metadata::is_supported_audio_file(&entry_path) {
                        files.push(WebBrowserEntry {
                            name: filename,
                            path: entry_path.to_string_lossy().to_string(),
                            is_dir: false,
                        });
                    }
                }
            }

            dirs.sort_by_key(|a| a.name.to_lowercase());
            files.sort_by_key(|a| a.name.to_lowercase());
            // Some SMB shares return each entry several times (issue #35).
            dirs.dedup_by(|a, b| a.path == b.path);
            files.dedup_by(|a, b| a.path == b.path);

            let mut all_entries = dirs;
            all_entries.extend(files);

            let resp_data = WebBrowserResponse {
                current_path: target_dir.to_string_lossy().to_string(),
                parent_path,
                bookmarks,
                entries: all_entries,
                home_dir: current_home_dir,
                terminal_dir,
            };

            let json = serde_json::to_string(&resp_data).unwrap_or_else(|_| "{}".to_string());
            let body = json.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes())?;
            if !is_head {
                stream.write_all(body)?;
            }
            stream.flush()?;
        } else if is_continuous_route(route) {
            // Only the armed UPnP continuous session's path is served, to the first host that
            // connects to it.
            let peer = stream.peer_addr()?.ip();
            match stream_state.continuous.admit(route, peer) {
                ContinuousAdmission::Admitted(nonce) => {
                    serve_continuous_stream(stream, is_head, state, cmd_tx, stream_state, &nonce)?;
                }
                ContinuousAdmission::NotFound => {
                    stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found",
                    )?;
                    stream.flush()?;
                }
                ContinuousAdmission::Conflict => {
                    crate::log_warn!(
                        "web",
                        "Refusing continuous stream request from {}: the session belongs to another host",
                        peer
                    );
                    stream.write_all(
                        b"HTTP/1.1 409 Conflict\r\nContent-Length: 8\r\nConnection: close\r\n\r\nConflict",
                    )?;
                    stream.flush()?;
                }
            }
            return Ok(());
        } else if path.starts_with("/stream") {
            // Serve DLNA audio streaming with HTTP byte ranges
            serve_audio_stream(stream, path, range_header.as_deref(), is_head, state, stream_state)?;
            return Ok(());
        } else {
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
        }
    } else if method == "POST" && path.starts_with("/api/action") {
        // content_length was checked against limits.max_body_bytes with the headers.
        let body_bytes = read_request_body(&mut reader, content_length, deadline).unwrap_or_default();

        // Paths must resolve inside the allowed folders; commands carry the resolved path.
        let roots = state.read().unwrap().allowed_roots.clone();
        let mut rejected: Option<PathBuf> = None;
        // Canonicalized on first use, at most once per request.
        let mut allowed: Option<crate::web_access::AllowedRoots> = None;
        let mut allow = |p: String| -> Option<PathBuf> {
            let resolved = allowed
                .get_or_insert_with(|| crate::web_access::AllowedRoots::new(&roots))
                .resolve(Path::new(&p));
            if resolved.is_none() {
                rejected = Some(PathBuf::from(p));
            }
            resolved
        };
        let mut response = ActionResponse::ok();
        if let Ok(payload) = serde_json::from_slice::<ActionPayload>(&body_bytes) {
            match payload.action.as_str() {
                "toggle_pause" => {
                    let _ = cmd_tx.send(WebCommand::TogglePause);
                }
                "play" => {
                    let _ = cmd_tx.send(WebCommand::Play);
                }
                "pause" => {
                    let _ = cmd_tx.send(WebCommand::Pause);
                }
                "next" => {
                    let _ = cmd_tx.send(WebCommand::Next);
                }
                "prev" => {
                    let _ = cmd_tx.send(WebCommand::Prev);
                }
                "volume" => {
                    if let Some(vol) = payload.value {
                        let _ = cmd_tx.send(WebCommand::SetVolume(vol.clamp(0.0, 100.0) as u8));
                    }
                }
                "seek" => {
                    if let Some(target) = payload.value {
                        let _ = cmd_tx.send(WebCommand::SeekTo(target));
                    }
                }
                "seek_relative" => {
                    if let Some(delta) = payload.value {
                        let _ = cmd_tx.send(WebCommand::SeekRelative(delta));
                    }
                }
                "play_index" => {
                    if let Some(idx) = payload.index {
                        let _ = cmd_tx.send(WebCommand::PlayIndex(idx));
                    }
                }
                "remove_index" => {
                    if let Some(idx) = payload.index {
                        let _ = cmd_tx.send(WebCommand::RemoveIndex(idx));
                    }
                }
                "clear_queue" => {
                    let _ = cmd_tx.send(WebCommand::ClearQueue);
                }
                "add_file" | "add_track" => {
                    if let Some(p) = payload.path.and_then(&mut allow) {
                        let _ = cmd_tx.send(WebCommand::AddFile(p));
                    }
                }
                "add_folder" => {
                    if let Some(p) = payload.path.and_then(&mut allow) {
                        let _ = cmd_tx.send(WebCommand::AddFolder(p));
                    }
                }
                "select_device" => {
                    if let Some(id) = payload.id.or(payload.index) {
                        let _ = cmd_tx.send(WebCommand::SelectDevice(id));
                    }
                }
                "rescan_devices" => {
                    let _ = cmd_tx.send(WebCommand::RescanDevices);
                }
                "set_queue_mode" => {
                    if let Some(mode) = payload.mode {
                        let _ = cmd_tx.send(WebCommand::SetQueueMode(mode));
                    }
                }
                "set_home" => match payload.path {
                    None => {
                        let _ = cmd_tx.send(WebCommand::SetHome(None));
                    }
                    Some(p) => {
                        if let Some(dir) = allow(p) {
                            let _ = cmd_tx.send(WebCommand::SetHome(Some(dir.to_string_lossy().to_string())));
                        }
                    }
                },
                "autopilot" => match payload.path {
                    None => {
                        let _ = cmd_tx.send(WebCommand::Autopilot(None));
                    }
                    Some(p) => {
                        if let Some(dir) = allow(p) {
                            let _ = cmd_tx.send(WebCommand::Autopilot(Some(dir)));
                        }
                    }
                },
                "autopilot_reshuffle" | "reshuffle" => match payload.path {
                    None => {
                        let _ = cmd_tx.send(WebCommand::AutopilotReshuffle(None));
                    }
                    Some(p) => {
                        if let Some(dir) = allow(p) {
                            let _ = cmd_tx.send(WebCommand::AutopilotReshuffle(Some(dir)));
                        }
                    }
                },
                "shuffle" => {
                    let _ = cmd_tx.send(WebCommand::ShuffleQueue);
                }
                "set_mix_profile" | "mix_profile" => {
                    if let Some(profile_str) = payload.mode.or(payload.path) {
                        let _ = cmd_tx.send(WebCommand::SetMixProfile(profile_str));
                    }
                }
                "seed_radio" => {
                    if let Some(p) = payload.path.and_then(&mut allow) {
                        let _ = cmd_tx.send(WebCommand::SeedRadio(p));
                    }
                }
                "playlist_play" | "playlist_append" => {
                    let play = payload.action == "playlist_play";
                    response = queue_playlist_action(&payload, play, state, cmd_tx, &mut rejected);
                }
                "playlist_create" | "playlist_rename" | "playlist_delete" | "playlist_add" | "playlist_remove"
                | "playlist_move" => {
                    response = edit_playlist_action(&payload, cmd_tx, &mut allow);
                }
                _ => {}
            }
        }

        if let Some(path) = rejected {
            let _ = cmd_tx.send(WebCommand::PathRejected(path));
            crate::web_access::write_path_forbidden(&mut stream)?;
            return Ok(());
        }

        let body = response.body.to_string();
        let header = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.status,
            body.len()
        );
        stream.write_all(header.as_bytes())?;
        stream.write_all(body.as_bytes())?;
        stream.flush()?;
    } else {
        let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
        stream.write_all(not_found)?;
        stream.flush()?;
    }

    Ok(())
}
fn find_mp3_frame_sync(file: &mut std::fs::File, start_offset: u64, max_scan: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return start_offset;
    }
    let mut buf = vec![0u8; max_scan as usize];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return start_offset,
    };
    if n < 4 {
        return start_offset;
    }
    for i in 0..(n - 3) {
        // Frame sync: 11 set bits (0xFF, 0xEx)
        if buf[i] == 0xFF && (buf[i + 1] & 0xE0) == 0xE0 {
            // Check MPEG version (01 reserved)
            if (buf[i + 1] & 0x18) == 0x08 {
                continue;
            }
            // Check bitrate index (1111 invalid)
            if (buf[i + 2] & 0xF0) == 0xF0 {
                continue;
            }
            // Check sampling frequency (11 reserved)
            if (buf[i + 2] & 0x0C) == 0x0C {
                continue;
            }
            return start_offset + i as u64;
        }
    }
    start_offset
}

fn get_id3v2_size(file: &mut std::fs::File) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let _ = file.seek(SeekFrom::Start(0));
    let mut hdr = [0u8; 10];
    if file.read_exact(&mut hdr).is_ok() && &hdr[0..3] == b"ID3" {
        let size = ((hdr[6] as u64 & 0x7F) << 21)
            | ((hdr[7] as u64 & 0x7F) << 14)
            | ((hdr[8] as u64 & 0x7F) << 7)
            | (hdr[9] as u64 & 0x7F);
        return 10 + size;
    }
    0
}

fn find_mp3_stream_offset(file: &mut std::fs::File, offset_seconds: f64, duration: f64, total_size: u64) -> u64 {
    let id3_size = get_id3v2_size(file);
    let audio_size = total_size.saturating_sub(id3_size);
    let dur = if duration > 0.0 { duration } else { 1.0 };
    let ratio = (offset_seconds / dur).clamp(0.0, 0.995);
    let est_offset = id3_size + (audio_size as f64 * ratio) as u64;
    find_mp3_frame_sync(file, est_offset, 4096)
}

fn find_wav_pcm_offset(file: &mut std::fs::File, offset_seconds: f64) -> (u64, Option<Vec<u8>>) {
    use std::io::{Read, Seek, SeekFrom};
    let _ = file.seek(SeekFrom::Start(0));
    let mut riff_hdr = [0u8; 12];
    if file.read_exact(&mut riff_hdr).is_err() || &riff_hdr[0..4] != b"RIFF" || &riff_hdr[8..12] != b"WAVE" {
        return (0, None);
    }
    let mut fmt_info = None;
    let mut data_offset = 0u64;
    let mut data_size = 0u32;

    while let Ok(chunk_hdr) = (|| -> std::io::Result<[u8; 8]> {
        let mut h = [0u8; 8];
        file.read_exact(&mut h)?;
        Ok(h)
    })() {
        let cid = &chunk_hdr[0..4];
        let csize = u32::from_le_bytes(chunk_hdr[4..8].try_into().unwrap());
        if cid == b"fmt " {
            let mut fmt_bytes = vec![0u8; csize as usize];
            if file.read_exact(&mut fmt_bytes).is_ok() && fmt_bytes.len() >= 16 {
                let audio_fmt = u16::from_le_bytes(fmt_bytes[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(fmt_bytes[2..4].try_into().unwrap());
                let srate = u32::from_le_bytes(fmt_bytes[4..8].try_into().unwrap());
                let brate = u32::from_le_bytes(fmt_bytes[8..12].try_into().unwrap());
                let align = u16::from_le_bytes(fmt_bytes[12..14].try_into().unwrap());
                let bps = u16::from_le_bytes(fmt_bytes[14..16].try_into().unwrap());
                fmt_info = Some((audio_fmt, channels, srate, brate, align, bps));
            }
        } else if cid == b"data" {
            data_offset = file.stream_position().unwrap_or(0);
            data_size = csize;
            break;
        } else {
            let pad = (csize % 2) as i64;
            let _ = file.seek(SeekFrom::Current(csize as i64 + pad));
        }
    }

    if let (Some((audio_fmt, channels, srate, brate, align, bps)), true) = (fmt_info, data_offset > 0 && data_size > 0)
    {
        let pcm_offset = (offset_seconds * brate as f64) as u64;
        let aligned_pcm = if align > 0 {
            (pcm_offset / align as u64) * align as u64
        } else {
            pcm_offset
        };
        let clamped_pcm = aligned_pcm.min(data_size as u64);
        let file_seek_pos = data_offset + clamped_pcm;
        let remaining_pcm = (data_size as u64).saturating_sub(clamped_pcm) as u32;

        let mut hdr = Vec::with_capacity(44);
        hdr.extend_from_slice(b"RIFF");
        hdr.extend_from_slice(&(36u32 + remaining_pcm).to_le_bytes());
        hdr.extend_from_slice(b"WAVEfmt ");
        hdr.extend_from_slice(&16u32.to_le_bytes());
        hdr.extend_from_slice(&audio_fmt.to_le_bytes());
        hdr.extend_from_slice(&channels.to_le_bytes());
        hdr.extend_from_slice(&srate.to_le_bytes());
        hdr.extend_from_slice(&brate.to_le_bytes());
        hdr.extend_from_slice(&align.to_le_bytes());
        hdr.extend_from_slice(&bps.to_le_bytes());
        hdr.extend_from_slice(b"data");
        hdr.extend_from_slice(&remaining_pcm.to_le_bytes());
        return (file_seek_pos, Some(hdr));
    }
    (0, None)
}

pub fn calculate_stream_offset(
    filepath: &std::path::Path,
    file: &mut std::fs::File,
    offset_seconds: f64,
    total_size: u64,
    duration_hint: f64,
) -> (u64, Option<Vec<u8>>) {
    if offset_seconds <= 0.0 || total_size == 0 {
        return (0, None);
    }
    let ext = filepath
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let duration = if duration_hint > 0.0 {
        duration_hint
    } else {
        crate::metadata::extract_metadata(filepath)
            .map(|m| m.duration_seconds)
            .unwrap_or(0.0)
    };

    if ext == "wav" || ext == "wave" {
        find_wav_pcm_offset(file, offset_seconds)
    } else if ext == "mp3" {
        let off = find_mp3_stream_offset(file, offset_seconds, duration, total_size);
        (off, None)
    } else {
        let dur = if duration > 0.0 { duration } else { 1.0 };
        let ratio = (offset_seconds / dur).clamp(0.0, 0.995);
        let seek_pos = (total_size as f64 * ratio) as u64;
        (seek_pos, None)
    }
}

/// Resolve a `/stream/<name>` request to a file the player has explicitly exposed.
///
/// Only files in the stream registry, or the current now-playing file matched by file
/// name, are served. Raw filesystem paths are never accepted, so a LAN client cannot
/// read arbitrary files from disk.
fn resolve_stream_file(name: &str, stream_state: &StreamServerState, current: Option<&Path>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    // The now-playing file wins over the registry: the registry is keyed by bare file name, so a
    // later registration of a same-named track from another album would otherwise replace it.
    let found = current
        .filter(|p| {
            p.file_name()
                .is_some_and(|f| f.to_string_lossy().to_lowercase() == name.to_lowercase())
        })
        .map(Path::to_path_buf)
        .or_else(|| stream_state.lookup_file(name))?;
    found.is_file().then_some(found)
}

/// How a `Range` request header applies to a representation of a given length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// No usable range: send the whole representation with `200 OK`.
    Full,
    /// Send bytes `start..=end` with `206 Partial Content`.
    Partial { start: u64, end: u64 },
    /// No requested range overlaps the representation: send `416`.
    Unsatisfiable,
}

/// Evaluate a `Range` header value against a representation of `len` bytes (RFC 9110 §14).
///
/// - Invalid syntax or a unit other than `bytes` is ignored (`Full`), as RFC 9110 allows.
/// - Suffix ranges (`bytes=-N`) select the last N bytes.
/// - With several ranges, only the first satisfiable one is served, as a single-part 206 whose
///   `Content-Range` says which bytes it holds. Renderers do not send multi-range requests, and
///   this avoids multipart/byteranges responses; a 416 would wrongly claim nothing overlaps.
/// - `Unsatisfiable` only when no range overlaps the representation (including `first > last`).
pub fn parse_byte_range(header: &str, len: u64) -> ByteRange {
    fn decimal(s: &str) -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Digits only, so a parse failure means overflow; any such value is past the end.
        Some(s.parse().unwrap_or(u64::MAX))
    }

    let Some((unit, set)) = header.trim().split_once('=') else {
        return ByteRange::Full;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return ByteRange::Full;
    }

    let mut any_spec = false;
    let mut first_satisfiable = None;
    for spec in set.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        any_spec = true;
        let Some((first, last)) = spec.split_once('-') else {
            return ByteRange::Full;
        };
        let range = if first.is_empty() {
            let Some(suffix) = decimal(last) else {
                return ByteRange::Full;
            };
            (suffix > 0 && len > 0).then(|| (len.saturating_sub(suffix), len - 1))
        } else {
            let Some(start) = decimal(first) else {
                return ByteRange::Full;
            };
            let end = if last.is_empty() {
                u64::MAX
            } else {
                match decimal(last) {
                    Some(end) => end,
                    None => return ByteRange::Full,
                }
            };
            (start <= end && start < len).then(|| (start, end.min(len - 1)))
        };
        // Keep parsing so a later malformed spec still makes the whole header invalid.
        if first_satisfiable.is_none() {
            first_satisfiable = range;
        }
    }

    match first_satisfiable {
        Some((start, end)) => ByteRange::Partial { start, end },
        None if any_spec => ByteRange::Unsatisfiable,
        None => ByteRange::Full,
    }
}

fn serve_audio_stream(
    mut stream: TcpStream,
    path: &str,
    range_header: Option<&str>,
    head_only: bool,
    state: &Arc<RwLock<NowPlayingState>>,
    stream_state: &Arc<StreamServerState>,
) -> std::io::Result<()> {
    let client_ip = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let conn_id = stream_state.register_socket(&stream);
    let _socket_guard = SocketGuard {
        stream_state: stream_state.clone(),
        conn_id,
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);

    // 1. Resolve target file and optional seek offset query parameter
    let raw_encoded = path.strip_prefix("/stream/").unwrap_or("");
    let encoded_path = raw_encoded.split('?').next().unwrap_or(raw_encoded);
    let decoded_path = crate::player::upnp::url_decode(encoded_path);

    let query_str = path.split_once('?').map(|(_, q)| q);
    let mut offset_seconds: f64 = 0.0;
    if let Some(query) = query_str {
        for param in query.split('&') {
            if let Some((k, v)) = param.split_once('=') {
                if k == "pos" || k == "offset" {
                    if let Ok(sec) = v.parse::<f64>() {
                        offset_seconds = sec.max(0.0);
                    }
                }
            }
        }
    }

    let (current_filepath, current_duration) = {
        let s = state.read().unwrap();
        (s.filepath.clone(), s.duration_seconds)
    };

    let filepath = match resolve_stream_file(&decoded_path, stream_state, current_filepath.as_deref()) {
        Some(p) => p,
        None => {
            crate::log_warn!(
                "web",
                "Stream 404: '{}' is not a registered or now-playing file",
                decoded_path
            );
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
            return Ok(());
        }
    };

    let mut file = match std::fs::File::open(&filepath) {
        Ok(f) => f,
        Err(e) => {
            crate::log_warn!("web", "Stream 404: Failed to open '{}': {}", filepath.display(), e);
            let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
            stream.write_all(not_found)?;
            stream.flush()?;
            return Ok(());
        }
    };

    let total_size = file.metadata()?.len();
    let mime = crate::metadata::get_audio_mime_type(&filepath);

    // 2. Calculate base byte offset for seek queries (e.g. ?pos=12.5)
    let (base_offset, synthesized_hdr) =
        calculate_stream_offset(&filepath, &mut file, offset_seconds, total_size, current_duration);

    let effective_total = if let Some(ref hdr) = synthesized_hdr {
        hdr.len() as u64 + total_size.saturating_sub(base_offset)
    } else {
        total_size.saturating_sub(base_offset)
    };

    // 3. Parse Range header if present
    let range = range_header.map_or(ByteRange::Full, |h| parse_byte_range(h, effective_total));
    let (req_start, req_end, is_range) = match range {
        ByteRange::Full => (0, effective_total.saturating_sub(1), false),
        ByteRange::Partial { start, end } => (start, end, true),
        ByteRange::Unsatisfiable => {
            crate::log_warn!(
                "web",
                "Stream 416 Range Not Satisfiable: {:?} of {} bytes for client {}",
                range_header,
                effective_total,
                client_ip
            );
            let err_resp = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                effective_total
            );
            stream.write_all(err_resp.as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    };

    let content_length = if is_range {
        req_end - req_start + 1
    } else {
        effective_total
    };

    // 4. Write DLNA HTTP headers
    let status_line = if is_range {
        "HTTP/1.1 206 Partial Content"
    } else {
        "HTTP/1.1 200 OK"
    };

    let display_name = filepath.file_name().and_then(|f| f.to_str()).unwrap_or("track");

    crate::log_info!(
        "web",
        "Serving '{}' to {} (method: {}, range: {:?}, offset: {:.2}s, bytes: {}-{}/{}, status: {})",
        display_name,
        client_ip,
        if head_only { "HEAD" } else { "GET" },
        range_header,
        offset_seconds,
        req_start,
        req_end,
        effective_total,
        status_line
    );

    let mut headers = format!(
        "{}\r\n\
        Content-Type: {}\r\n\
        Content-Length: {}\r\n\
        Accept-Ranges: bytes\r\n\
        transferMode.dlna.org: Streaming\r\n\
        contentFeatures.dlna.org: DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n\
        Connection: close\r\n",
        status_line, mime, content_length
    );

    if is_range {
        headers.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            req_start, req_end, effective_total
        ));
    }
    headers.push_str("\r\n");

    stream.write_all(headers.as_bytes())?;
    stream.flush()?;

    if head_only {
        return Ok(());
    }

    // 5. Stream audio file
    use std::io::Seek;
    let mut streamed_count = 0u64;

    if let Some(hdr) = synthesized_hdr {
        let hdr_len = hdr.len() as u64;

        // If request starts inside the synthesized header
        if req_start < hdr_len {
            let hdr_slice_start = req_start as usize;
            let hdr_slice_end = (req_end + 1).min(hdr_len) as usize;
            let slice = &hdr[hdr_slice_start..hdr_slice_end];
            if stream.write_all(slice).is_err() {
                crate::log_info!(
                    "web",
                    "Client {} disconnected during synthesized header write after {} bytes",
                    client_ip,
                    streamed_count
                );
                return Ok(());
            }
            streamed_count += slice.len() as u64;
        }

        // Remaining bytes from file
        if req_end >= hdr_len {
            let file_start_offset = if req_start > hdr_len {
                base_offset + (req_start - hdr_len)
            } else {
                base_offset
            };
            file.seek(std::io::SeekFrom::Start(file_start_offset))?;
            let mut remaining = content_length.saturating_sub(streamed_count);
            let mut buf = [0u8; 65536];
            while remaining > 0 {
                let to_read = remaining.min(buf.len() as u64) as usize;
                let n = match file.read(&mut buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                if stream.write_all(&buf[..n]).is_err() {
                    crate::log_info!(
                        "web",
                        "Client {} disconnected or stream aborted after {} bytes for '{}'",
                        client_ip,
                        streamed_count,
                        display_name
                    );
                    return Ok(());
                }
                remaining -= n as u64;
                streamed_count += n as u64;
            }
        }
    } else {
        let file_start = base_offset + req_start;
        file.seek(std::io::SeekFrom::Start(file_start))?;

        let mut buf = [0u8; 65536];
        let mut remaining = content_length;
        while remaining > 0 {
            let to_read = remaining.min(buf.len() as u64) as usize;
            let n = match file.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if stream.write_all(&buf[..n]).is_err() {
                crate::log_info!(
                    "web",
                    "Client {} disconnected or stream aborted after {} bytes for '{}'",
                    client_ip,
                    streamed_count,
                    display_name
                );
                return Ok(());
            }
            remaining -= n as u64;
            streamed_count += n as u64;
        }
    }
    let _ = stream.flush();

    if streamed_count < content_length {
        // The file shrank or failed to read after Content-Length was sent. Close at once so the
        // client sees an incomplete response instead of a silently short body.
        crate::log_warn!(
            "web",
            "Source for '{}' ended after {} of {} advertised bytes; closing connection to {}",
            display_name,
            streamed_count,
            content_length,
            client_ip
        );
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return Ok(());
    }

    crate::log_info!(
        "web",
        "Streamed {} bytes to {} for '{}' (requested: {})",
        streamed_count,
        client_ip,
        display_name,
        content_length
    );

    Ok(())
}

/// Construct a standard 44-byte RIFF/WAV header for an infinite (live) 16-bit 44.1kHz stereo PCM stream.
pub fn make_continuous_wav_header() -> [u8; 44] {
    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Streaming/infinite length
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes()); // Subchunk1 size: 16
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes()); // Audio format: PCM (1)
    hdr[22..24].copy_from_slice(&2u16.to_le_bytes()); // Channels: 2 (Stereo)
    hdr[24..28].copy_from_slice(&44100u32.to_le_bytes()); // Sample rate: 44,100 Hz
    hdr[28..32].copy_from_slice(&(44100u32 * 2 * 2).to_le_bytes()); // Byte rate: 176,400 B/s
    hdr[32..34].copy_from_slice(&4u16.to_le_bytes()); // Block align: 4
    hdr[34..36].copy_from_slice(&16u16.to_le_bytes()); // Bits per sample: 16
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // Data size: infinite
    hdr
}

/// Serve a continuous 16-bit 44.1kHz PCM WAV stream to DLNA renderers.
/// Slices tracks sequentially directly into the exact same TCP socket without drops or DAC resets.
/// `session` is the nonce of the armed session the request was admitted to; the stream ends when
/// that session ends.
pub fn serve_continuous_stream(
    mut stream: TcpStream,
    is_head: bool,
    state: &Arc<RwLock<NowPlayingState>>,
    cmd_tx: &mpsc::Sender<WebCommand>,
    stream_state: &Arc<StreamServerState>,
    session: &str,
) -> std::io::Result<()> {
    let conn_id = stream_state.register_socket(&stream);
    let _socket_guard = SocketGuard {
        stream_state: stream_state.clone(),
        conn_id,
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);

    crate::log_info!(
        "web",
        "Continuous gapless stream client connected (conn_id: {:?})",
        conn_id
    );

    let headers = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: audio/x-wav\r\n",
        "Accept-Ranges: none\r\n",
        "transferMode.dlna.org: Streaming\r\n",
        "contentFeatures.dlna.org: DLNA.ORG_OP=00;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01500000000000000000000000000000\r\n",
        "Connection: close\r\n",
        "\r\n"
    );
    stream.write_all(headers.as_bytes())?;
    if is_head {
        stream.flush()?;
        return Ok(());
    }

    let cid = conn_id.unwrap_or(0);
    let controller = stream_state.continuous.clone();
    controller.active_conn_id.store(cid, Ordering::SeqCst);
    controller.is_active.store(true, Ordering::SeqCst);

    struct ContinuousActiveGuard {
        controller: Arc<ContinuousStreamController>,
        conn_id: u64,
    }
    impl Drop for ContinuousActiveGuard {
        fn drop(&mut self) {
            if self.conn_id == 0 || self.controller.active_conn_id.load(Ordering::SeqCst) == self.conn_id {
                self.controller.is_active.store(false, Ordering::SeqCst);
                self.controller.kill_active_decoder();
                crate::log_info!(
                    "web",
                    "Continuous gapless stream finished or disconnected (conn_id: {})",
                    self.conn_id
                );
            } else {
                crate::log_info!(
                    "web",
                    "Continuous stream connection {} closed (superseded by conn_id {})",
                    self.conn_id,
                    self.controller.active_conn_id.load(Ordering::SeqCst)
                );
            }
        }
    }
    let _active_guard = ContinuousActiveGuard {
        controller: controller.clone(),
        conn_id: cid,
    };

    // Mark the session active before the client can read any body bytes, so a client that has
    // the WAV header always sees `is_active`; the guard clears it if this write fails.
    let wav_hdr = make_continuous_wav_header();
    stream.write_all(&wav_hdr)?;
    stream.flush()?;

    while controller.is_active.load(Ordering::SeqCst) {
        if !controller.is_session(session) {
            crate::log_info!(
                "web",
                "Continuous stream connection {} outlived its session, closing",
                cid
            );
            return Ok(());
        }
        if cid != 0 && controller.active_conn_id.load(Ordering::SeqCst) != cid {
            crate::log_info!(
                "web",
                "Continuous stream connection {} superseded by newer connection {}, closing older stream",
                cid,
                controller.active_conn_id.load(Ordering::SeqCst)
            );
            return Ok(());
        }

        // 1. Get current track information
        let (track_path, track_duration, track_index, queue_len) = {
            let s = state.read().unwrap();
            if s.queue.is_empty() {
                drop(s);
                // Send a small burst of silence PCM frames so renderer buffer doesn't starve
                let silence = [0u8; 4096];
                if let Err(e) = stream.write_all(&silence) {
                    crate::log_info!("web", "Continuous stream socket closed while idle: {}", e);
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let idx = *controller.current_track_index.lock().unwrap();
            let safe_idx = if idx < s.queue.len() { idx } else { 0 };
            let track = &s.queue[safe_idx];
            (
                PathBuf::from(&track.filepath),
                track.duration_seconds,
                safe_idx,
                s.queue.len(),
            )
        };

        *controller.current_track_index.lock().unwrap() = track_index;
        controller.start_track(track_index, &track_path, track_duration);
        *controller.current_position.lock().unwrap() = 0.0;

        let start_pos = controller.target_seek.lock().unwrap().take().unwrap_or(0.0);
        controller.seek_requested.store(false, Ordering::SeqCst);
        controller.skip_requested.store(false, Ordering::SeqCst);

        // Synchronize track info into NowPlayingState
        if let Ok(mut s) = state.write() {
            s.queue_index = track_index;
            if let Some(t) = s.queue.get(track_index).cloned() {
                s.title = t.title;
                s.artist = t.artist;
                s.album = t.album;
                s.duration_seconds = t.duration_seconds;
                s.filepath = Some(PathBuf::from(&t.filepath));
                s.position_seconds = start_pos;
                s.is_playing = true;
                s.is_paused = false;
            }
            if queue_len > 1 {
                let next_idx = (track_index + 1) % queue_len;
                s.next_track_title = s.queue.get(next_idx).map(|t| t.title.clone());
            }
        }

        crate::log_info!(
            "web",
            "Continuous stream decoding track [{}/{}]: '{}' (seek: {:.2}s)",
            track_index + 1,
            queue_len,
            track_path.display(),
            start_pos
        );

        if !track_path.exists() {
            crate::log_warn!("web", "Continuous stream track missing: {}", track_path.display());
            let next_idx = (track_index + 1) % queue_len;
            *controller.current_track_index.lock().unwrap() = next_idx;
            continue;
        }

        // 2. Spawn ffmpeg to decode to 44.1kHz 16-bit LE stereo raw PCM
        let mut cmd = std::process::Command::new("ffmpeg");
        if start_pos > 0.0 {
            cmd.arg("-ss").arg(format!("{:.3}", start_pos));
        }
        cmd.arg("-v")
            .arg("quiet")
            .arg("-i")
            .arg(&track_path)
            .arg("-f")
            .arg("s16le")
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg("44100")
            .arg("-")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                crate::log_warn!("web", "Failed to spawn ffmpeg for continuous stream: {}", e);
                std::thread::sleep(Duration::from_millis(500));
                let next_idx = (track_index + 1) % queue_len;
                *controller.current_track_index.lock().unwrap() = next_idx;
                continue;
            }
        };

        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                continue;
            }
        };

        *controller.active_child.lock().unwrap() = Some(child);

        // 3. Stream PCM chunks
        let mut buf = [0u8; 32768];
        let mut bytes_streamed: u64 = (start_pos * 176400.0) as u64;
        let mut last_pos_update = std::time::Instant::now();
        let mut track_finished_normally = false;

        loop {
            if !controller.is_active.load(Ordering::SeqCst) {
                controller.kill_active_decoder();
                return Ok(());
            }

            if cid != 0 && controller.active_conn_id.load(Ordering::SeqCst) != cid {
                controller.kill_active_decoder();
                return Ok(());
            }

            if controller.skip_requested.load(Ordering::SeqCst) || controller.seek_requested.load(Ordering::SeqCst) {
                controller.kill_active_decoder();
                if let Some(target) = controller.target_index.lock().unwrap().take() {
                    *controller.current_track_index.lock().unwrap() = target;
                }
                break;
            }

            match stdout.read(&mut buf) {
                Ok(0) => {
                    track_finished_normally = true;
                    break;
                }
                Ok(n) => {
                    if let Err(e) = stream.write_all(&buf[..n]) {
                        crate::log_info!("web", "Continuous stream socket closed: {}", e);
                        controller.kill_active_decoder();
                        return Ok(());
                    }
                    bytes_streamed += n as u64;

                    if last_pos_update.elapsed() >= Duration::from_millis(250) {
                        last_pos_update = std::time::Instant::now();
                        let pos = bytes_streamed as f64 / 176400.0;
                        *controller.current_position.lock().unwrap() = pos;
                        if let Ok(mut s) = state.write() {
                            s.position_seconds = pos;
                            controller.learn_duration(&mut s, track_index, &track_path);
                        }
                    }
                }
                Err(e) => {
                    crate::log_warn!("web", "Continuous stream read error: {}", e);
                    break;
                }
            }
        }

        controller.kill_active_decoder();

        if track_finished_normally {
            // Reshuffle once per wrap, and only from the current connection of the armed session.
            let owns_session =
                controller.is_session(session) && (cid == 0 || controller.active_conn_id.load(Ordering::SeqCst) == cid);
            if track_index + 1 >= queue_len && queue_len > 1 && owns_session {
                let _ = cmd_tx.send(WebCommand::ShuffleQueue);
            }
            let next_idx = (track_index + 1) % queue_len;
            *controller.current_track_index.lock().unwrap() = next_idx;
            *controller.current_position.lock().unwrap() = 0.0;
        }
    }

    Ok(())
}

/// Embedded responsive HTML/CSS/JavaScript web dashboard.
const NOW_PLAYING_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
  <title>Quickstream · Control & Now Playing</title>
  <style>
    :root {
      --bg: #0d0f14;
      --card-bg: rgba(22, 26, 36, 0.85);
      --card-border: rgba(255, 255, 255, 0.08);
      --accent: #00d2ff;
      --accent-grad: linear-gradient(135deg, #00d2ff 0%, #0072ff 100%);
      --text: #f0f4f8;
      --text-muted: #8c9ba8;
      --text-dim: #5c6878;
      --playing: #00e676;
      --paused: #ff9100;
      --danger: #ff5252;
      --font: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    }

    * { box-sizing: border-box; margin: 0; padding: 0; }

    body {
      background-color: var(--bg);
      color: var(--text);
      font-family: var(--font);
      min-height: 100vh;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      padding: 1rem;
      overflow-x: hidden;
      position: relative;
    }

    /* Ambient blurred backdrop */
    .ambient-glow {
      position: fixed;
      top: 50%;
      left: 50%;
      transform: translate(-50%, -50%);
      width: 650px;
      height: 650px;
      background: radial-gradient(circle, rgba(0, 210, 255, 0.14) 0%, rgba(0, 114, 255, 0.04) 50%, transparent 70%);
      filter: blur(90px);
      z-index: 0;
      pointer-events: none;
      transition: background 1s ease;
    }

    .container {
      position: relative;
      z-index: 1;
      width: 480px;
      max-width: 100%;
      height: min(680px, calc(100dvh - 1.5rem));
      max-height: calc(100dvh - 1.5rem);
      min-height: 0;
      background: var(--card-bg);
      backdrop-filter: blur(24px);
      -webkit-backdrop-filter: blur(24px);
      border: 1px solid var(--card-border);
      border-radius: 28px;
      padding: 1.25rem;
      box-shadow: 0 24px 64px rgba(0, 0, 0, 0.6), 0 4px 16px rgba(0, 0, 0, 0.4);
      display: flex;
      flex-direction: column;
      align-items: center;
      overflow: hidden;
    }

    /* Tab Bar Navigation */
    .tabs {
      display: flex;
      gap: 4px;
      padding: 4px;
      background: rgba(0, 0, 0, 0.35);
      border-radius: 14px;
      margin-bottom: 0.85rem;
      width: 100%;
      flex-shrink: 0;
      border: 1px solid rgba(255, 255, 255, 0.05);
    }

    .tab-btn {
      flex: 1;
      padding: 0.55rem 0.2rem;
      font-size: 0.78rem;
      font-weight: 600;
      border: none;
      background: transparent;
      color: var(--text-muted);
      border-radius: 10px;
      cursor: pointer;
      transition: all 0.2s ease;
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 4px;
      -webkit-tap-highlight-color: transparent;
    }
    .tab-btn:hover {
      color: var(--text);
    }
    .tab-btn.active {
      background: rgba(255, 255, 255, 0.12);
      color: #ffffff;
      box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
    }

    .tab-badge {
      background: rgba(0, 210, 255, 0.2);
      color: var(--accent);
      border-radius: 10px;
      padding: 1px 6px;
      font-size: 0.68rem;
      font-weight: 700;
    }

    /* Tab Panels */
    .tab-panel {
      width: 100%;
      flex: 1;
      min-height: 0;
      display: none;
      flex-direction: column;
    }
    .tab-panel.active {
      display: flex;
    }
    .tab-panel.centered {
      align-items: center;
      text-align: center;
      overflow-y: auto;
      overflow-x: hidden;
      scrollbar-width: thin;
      scrollbar-color: rgba(255, 255, 255, 0.12) transparent;
    }

    /* Header Bar */
    .header {
      width: 100%;
      display: flex;
      justify-content: space-between;
      align-items: center;
      margin-bottom: 0.75rem;
      font-size: 0.8rem;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      color: var(--text-dim);
      flex-shrink: 0;
    }

    .logo {
      font-weight: 800;
      color: var(--text);
      display: flex;
      align-items: center;
      gap: 0.4rem;
    }
    .logo span {
      background: var(--accent-grad);
      -webkit-background-clip: text;
      -webkit-text-fill-color: transparent;
    }

    .status-badge {
      display: flex;
      align-items: center;
      gap: 0.35rem;
      padding: 0.25rem 0.65rem;
      border-radius: 20px;
      background: rgba(255, 255, 255, 0.05);
      border: 1px solid rgba(255, 255, 255, 0.08);
      font-weight: 600;
    }
    .status-dot {
      width: 7px;
      height: 7px;
      border-radius: 50%;
      background: var(--text-dim);
    }
    .status-dot.playing {
      background: var(--playing);
      box-shadow: 0 0 8px var(--playing);
      animation: pulse 2s infinite;
    }
    .status-dot.paused {
      background: var(--paused);
    }

    @keyframes pulse {
      0%, 100% { opacity: 1; transform: scale(1); }
      50% { opacity: 0.6; transform: scale(0.85); }
    }

    /* Cover Artwork */
    .artwork-container {
      position: relative;
      width: min(230px, 30vh, 52vw);
      height: min(230px, 30vh, 52vw);
      aspect-ratio: 1 / 1;
      border-radius: 20px;
      overflow: hidden;
      box-shadow: 0 16px 40px rgba(0, 0, 0, 0.6);
      margin-bottom: 0.75rem;
      background: #141720;
      flex-shrink: 1;
      min-width: 110px;
      min-height: 110px;
    }

    .artwork {
      width: 100%;
      height: 100%;
      object-fit: cover;
      display: block;
      transition: transform 0.4s ease, opacity 0.3s ease;
    }
    .artwork-container:hover .artwork {
      transform: scale(1.02);
    }

    /* Track Metadata */
    .track-info {
      width: 100%;
      margin-bottom: 0.55rem;
      flex-shrink: 0;
    }

    .title {
      font-size: 1.25rem;
      font-weight: 700;
      color: var(--text);
      line-height: 1.25;
      margin-bottom: 0.25rem;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .artist {
      font-size: 0.98rem;
      font-weight: 500;
      color: var(--accent);
      margin-bottom: 0.2rem;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .album {
      font-size: 0.82rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    /* Badges */
    .badges {
      display: flex;
      flex-wrap: wrap;
      justify-content: center;
      gap: 0.35rem;
      margin-bottom: 0.6rem;
      flex-shrink: 0;
    }

    .badge {
      font-size: 0.72rem;
      font-weight: 600;
      padding: 0.2rem 0.55rem;
      border-radius: 6px;
      background: rgba(255, 255, 255, 0.05);
      border: 1px solid rgba(255, 255, 255, 0.08);
      color: var(--text-muted);
      letter-spacing: 0.03em;
    }
    .badge.format {
      background: rgba(0, 210, 255, 0.12);
      border-color: rgba(0, 210, 255, 0.25);
      color: #7de7ff;
    }

    /* Progress Bar */
    .progress-container {
      width: 100%;
      margin-bottom: 0.6rem;
      cursor: pointer;
      padding: 6px 0;
      user-select: none;
      -webkit-user-select: none;
      touch-action: none;
      flex-shrink: 0;
    }

    .progress-bar-bg {
      width: 100%;
      height: 6px;
      background: rgba(255, 255, 255, 0.08);
      border-radius: 4px;
      position: relative;
      transition: height 0.15s ease;
    }
    .progress-container:hover .progress-bar-bg,
    .progress-container.scrubbing .progress-bar-bg {
      height: 8px;
    }

    .progress-bar-fill {
      height: 100%;
      width: 0%;
      background: var(--accent-grad);
      border-radius: 4px;
      position: relative;
    }
    .progress-bar-fill::after {
      content: '';
      position: absolute;
      right: -6px;
      top: 50%;
      transform: translateY(-50%) scale(0);
      width: 14px;
      height: 14px;
      background: #ffffff;
      border-radius: 50%;
      box-shadow: 0 0 10px rgba(0, 210, 255, 0.9);
      transition: transform 0.15s ease;
    }
    .progress-container:hover .progress-bar-fill::after,
    .progress-container.scrubbing .progress-bar-fill::after {
      transform: translateY(-50%) scale(1);
    }

    .time-labels {
      display: flex;
      justify-content: space-between;
      margin-top: 0.4rem;
      font-size: 0.78rem;
      color: var(--text-dim);
      font-variant-numeric: tabular-nums;
      font-weight: 500;
    }

    /* Controls */
    .controls {
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 1.15rem;
      margin-bottom: 0.6rem;
      width: 100%;
      flex-shrink: 0;
    }

    .btn {
      background: none;
      border: none;
      color: var(--text);
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: center;
      border-radius: 50%;
      transition: transform 0.15s ease, background 0.2s ease, color 0.2s ease;
      -webkit-tap-highlight-color: transparent;
    }
    .btn:active {
      transform: scale(0.92);
    }

    .btn-secondary {
      width: 44px;
      height: 44px;
      color: var(--text-muted);
      background: rgba(255, 255, 255, 0.04);
    }
    .btn-secondary:hover {
      background: rgba(255, 255, 255, 0.09);
      color: var(--text);
    }

    .btn-primary {
      width: 58px;
      height: 58px;
      background: var(--accent-grad);
      color: #0d0f14;
      box-shadow: 0 8px 24px rgba(0, 210, 255, 0.35);
    }
    .btn-primary:hover {
      box-shadow: 0 10px 28px rgba(0, 210, 255, 0.5);
      transform: scale(1.04);
    }

    /* Volume Row */
    .volume-row {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      width: 80%;
      color: var(--text-dim);
      margin-bottom: 0.35rem;
      flex-shrink: 0;
    }
    .volume-row svg {
      width: 16px;
      height: 16px;
      flex-shrink: 0;
    }
    .volume-slider {
      flex: 1;
      height: 4px;
      accent-color: var(--accent);
      cursor: pointer;
    }

    /* Up Next Drawer */
    .up-next {
      margin-top: 0.5rem;
      padding-top: 0.5rem;
      border-top: 1px solid rgba(255, 255, 255, 0.06);
      width: 100%;
      font-size: 0.75rem;
      color: var(--text-dim);
      display: flex;
      justify-content: space-between;
      align-items: center;
      flex-shrink: 0;
    }
    .up-next-title {
      color: var(--text-muted);
      font-weight: 500;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
      max-width: 260px;
    }

    /* Panel Header Common */
    .panel-header {
      width: 100%;
      display: flex;
      justify-content: space-between;
      align-items: center;
      margin-bottom: 1rem;
      flex-shrink: 0;
    }
    .panel-title {
      font-size: 1.05rem;
      font-weight: 700;
      color: var(--text);
    }
    .panel-subtitle {
      font-size: 0.78rem;
      color: var(--text-dim);
      font-weight: 500;
      margin-left: 0.4rem;
    }

    .btn-sm {
      padding: 0.35rem 0.75rem;
      border-radius: 8px;
      font-size: 0.75rem;
      font-weight: 600;
      border: 1px solid rgba(255, 255, 255, 0.1);
      background: rgba(255, 255, 255, 0.04);
      color: var(--text);
      cursor: pointer;
      display: inline-flex;
      align-items: center;
      gap: 0.35rem;
      transition: all 0.15s;
    }
    .btn-sm:hover {
      background: rgba(255, 255, 255, 0.09);
    }
    .btn-sm.accent {
      background: rgba(0, 210, 255, 0.15);
      border-color: rgba(0, 210, 255, 0.35);
      color: var(--accent);
    }
    .btn-sm.accent:hover {
      background: var(--accent);
      color: #0d0f14;
    }
    .btn-sm:disabled {
      opacity: 0.4;
      cursor: not-allowed;
    }

    .btn-text {
      background: none;
      border: none;
      font-size: 0.75rem;
      font-weight: 600;
      cursor: pointer;
      color: var(--text-muted);
    }
    .btn-text.danger {
      color: var(--danger);
    }
    .btn-text.danger:hover {
      text-decoration: underline;
    }

    /* Queue Tab Elements */
    .mode-bar {
      display: flex;
      align-items: center;
      gap: 0.4rem;
      margin-bottom: 0.85rem;
      width: 100%;
      flex-shrink: 0;
    }
    .mode-label {
      font-size: 0.75rem;
      color: var(--text-dim);
      font-weight: 600;
      margin-right: 0.2rem;
    }
    .mode-btn {
      padding: 0.28rem 0.65rem;
      border-radius: 8px;
      font-size: 0.72rem;
      font-weight: 600;
      border: 1px solid rgba(255, 255, 255, 0.08);
      background: rgba(255, 255, 255, 0.03);
      color: var(--text-muted);
      cursor: pointer;
      transition: all 0.15s;
    }
    .mode-btn:hover {
      background: rgba(255, 255, 255, 0.07);
      color: var(--text);
    }
    .mode-btn.active {
      background: rgba(0, 210, 255, 0.15);
      border-color: rgba(0, 210, 255, 0.4);
      color: var(--accent);
    }

    .queue-list {
      display: flex;
      flex-direction: column;
      gap: 0.4rem;
      flex: 1;
      min-height: 0;
      overflow-y: auto;
      width: 100%;
      padding-right: 4px;
    }
    .queue-list::-webkit-scrollbar,
    .browser-list::-webkit-scrollbar {
      width: 5px;
    }
    .queue-list::-webkit-scrollbar-thumb,
    .browser-list::-webkit-scrollbar-thumb {
      background: rgba(255, 255, 255, 0.12);
      border-radius: 4px;
    }

    .queue-item {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      padding: 0.55rem 0.75rem;
      border-radius: 12px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.05);
      transition: all 0.15s ease;
      cursor: pointer;
    }
    .queue-item:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .queue-item.current {
      background: rgba(0, 210, 255, 0.08);
      border-color: rgba(0, 210, 255, 0.3);
    }
    .queue-item.current .queue-num {
      color: #ffd600;
      font-weight: 700;
    }
    .queue-item.current .queue-item-title {
      color: #ffd600;
      font-weight: 600;
    }

    .queue-item-main {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      flex: 1;
      min-width: 0;
    }
    .queue-num {
      font-size: 0.8rem;
      color: var(--text-dim);
      min-width: 1.5rem;
      font-variant-numeric: tabular-nums;
    }
    .queue-item-meta {
      flex: 1;
      min-width: 0;
      text-align: left;
    }
    .queue-item-title {
      font-size: 0.88rem;
      font-weight: 500;
      color: var(--text);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .queue-item-artist {
      font-size: 0.75rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .queue-item-dur {
      font-size: 0.75rem;
      color: var(--text-dim);
      font-variant-numeric: tabular-nums;
      margin-right: 0.2rem;
    }

    .btn-icon-sm {
      width: 26px;
      height: 26px;
      border-radius: 6px;
      border: none;
      background: transparent;
      color: var(--text-dim);
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: center;
      transition: all 0.15s;
    }
    .btn-icon-sm:hover {
      background: rgba(255, 255, 255, 0.08);
      color: var(--text);
    }
    .btn-icon-sm.danger-hover:hover {
      background: rgba(255, 82, 82, 0.2);
      color: var(--danger);
    }

    .bookmarks-row {
      display: flex;
      gap: 0.4rem;
      overflow-x: auto;
      padding-bottom: 0.5rem;
      margin-bottom: 0.75rem;
      width: 100%;
      flex-shrink: 0;
    }
    .bookmarks-row::-webkit-scrollbar {
      height: 3px;
    }
    .chip-btn {
      flex-shrink: 0;
      padding: 0.28rem 0.65rem;
      border-radius: 12px;
      font-size: 0.75rem;
      font-weight: 600;
      border: 1px solid rgba(255, 214, 0, 0.25);
      background: rgba(255, 214, 0, 0.08);
      color: #ffd600;
      cursor: pointer;
      transition: all 0.15s;
    }
    .chip-btn:hover {
      background: rgba(255, 214, 0, 0.18);
    }

    .browser-path-bar {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      margin-bottom: 0.45rem;
      width: 100%;
      flex-shrink: 0;
      background: rgba(0, 0, 0, 0.25);
      padding: 0.45rem 0.65rem;
      border-radius: 8px;
      border: 1px solid rgba(255, 255, 255, 0.06);
      box-sizing: border-box;
    }
    .browser-current-path {
      flex: 1;
      font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
      font-size: 0.78rem;
      color: var(--text);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
      text-align: left;
    }

    .browser-actions-bar {
      display: flex;
      align-items: center;
      gap: 0.4rem;
      margin-bottom: 0.65rem;
      width: 100%;
      flex-shrink: 0;
      overflow-x: auto;
      scrollbar-width: none;
    }
    .browser-actions-bar::-webkit-scrollbar {
      display: none;
    }
    .browser-actions-bar .btn-sm {
      white-space: nowrap;
      flex-shrink: 0;
    }

    .search-input {
      width: 100%;
      padding: 0.5rem 0.75rem;
      border-radius: 10px;
      border: 1px solid rgba(255, 255, 255, 0.08);
      background: rgba(0, 0, 0, 0.25);
      color: var(--text);
      font-size: 0.82rem;
      margin-bottom: 0.75rem;
      outline: none;
      flex-shrink: 0;
      transition: border-color 0.15s;
    }
    .search-input:focus {
      border-color: var(--accent);
    }

    .browser-list {
      display: flex;
      flex-direction: column;
      gap: 0.35rem;
      flex: 1;
      min-height: 0;
      overflow-y: auto;
      width: 100%;
      padding-right: 4px;
    }
    .browser-item {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      padding: 0.5rem 0.75rem;
      border-radius: 10px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.04);
      cursor: pointer;
      transition: all 0.15s;
    }
    .browser-item:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .browser-icon {
      font-size: 1rem;
      flex-shrink: 0;
    }
    .browser-name {
      flex: 1;
      font-size: 0.85rem;
      color: var(--text);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
      text-align: left;
    }
    .browser-arrow {
      color: var(--text-dim);
      font-size: 1rem;
    }
    .add-btn {
      background: rgba(0, 210, 255, 0.12);
      color: var(--accent);
    }
    .add-btn:hover {
      background: var(--accent);
      color: #0d0f14;
    }

    /* Devices Tab */
    .devices-list {
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      flex: 1;
      min-height: 0;
      overflow-y: auto;
      width: 100%;
      padding-right: 4px;
    }
    .device-item {
      display: flex;
      align-items: center;
      gap: 0.75rem;
      padding: 0.75rem 0.9rem;
      border-radius: 14px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.05);
      cursor: pointer;
      transition: all 0.2s;
    }
    .device-item:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .device-item.active {
      background: rgba(0, 230, 118, 0.08);
      border-color: rgba(0, 230, 118, 0.3);
    }
    .device-radio {
      font-size: 1.1rem;
      color: var(--text-dim);
      flex-shrink: 0;
    }
    .device-item.active .device-radio {
      color: var(--playing);
    }
    .device-info {
      flex: 1;
      text-align: left;
    }
    .device-name {
      font-size: 0.92rem;
      font-weight: 600;
      color: var(--text);
      display: flex;
      align-items: center;
      gap: 0.5rem;
    }
    .device-host {
      font-size: 0.75rem;
      color: var(--text-dim);
    }
    .active-tag {
      font-size: 0.65rem;
      font-weight: 700;
      color: var(--playing);
      background: rgba(0, 230, 118, 0.15);
      border-radius: 4px;
      padding: 1px 5px;
    }

    /* Playlists */
    .playlist-card {
      display: flex;
      flex-direction: column;
      flex-shrink: 0;
      border-radius: 12px;
      background: rgba(255, 255, 255, 0.02);
      border: 1px solid rgba(255, 255, 255, 0.05);
    }
    .playlist-card.open {
      border-color: rgba(0, 210, 255, 0.3);
    }
    .playlist-head {
      display: flex;
      align-items: center;
      gap: 0.35rem;
      padding: 0.5rem 0.6rem 0.5rem 0.75rem;
    }
    .playlist-head-main {
      display: flex;
      align-items: center;
      gap: 0.6rem;
      flex: 1;
      min-width: 0;
      cursor: pointer;
    }
    .playlist-chevron {
      color: var(--text-dim);
      font-size: 1rem;
      width: 0.8rem;
      transition: transform 0.15s ease;
    }
    .playlist-card.open .playlist-chevron {
      transform: rotate(90deg);
    }
    .playlist-name {
      font-size: 0.9rem;
      font-weight: 600;
      color: var(--text);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .playlist-meta {
      font-size: 0.72rem;
      color: var(--text-dim);
    }
    .playlist-tracks {
      display: flex;
      flex-direction: column;
      gap: 0.2rem;
      padding: 0 0.4rem 0.5rem;
    }
    .playlist-track {
      display: flex;
      align-items: center;
      gap: 0.35rem;
      padding: 0.3rem 0.35rem 0.3rem 0.5rem;
      border-radius: 8px;
    }
    .playlist-track:hover {
      background: rgba(255, 255, 255, 0.06);
    }
    .playlist-track .queue-item-main {
      cursor: pointer;
    }
    .playlist-track.missing .queue-item-title,
    .playlist-track.outside .queue-item-title {
      color: var(--text-dim);
    }
    .playlist-track.missing .queue-item-title {
      color: var(--danger);
    }
    .playlists-note {
      text-align: center;
      padding: 2rem 0;
    }
    .picker-backdrop {
      position: fixed;
      inset: 0;
      z-index: 90;
      display: none;
      align-items: center;
      justify-content: center;
      padding: 1rem;
      background: rgba(0, 0, 0, 0.55);
    }
    .picker-backdrop.show {
      display: flex;
    }
    .picker {
      width: 340px;
      max-width: 100%;
      max-height: 70vh;
      display: flex;
      flex-direction: column;
      gap: 0.5rem;
      padding: 1rem;
      border-radius: 18px;
      background: rgba(15, 20, 28, 0.97);
      border: 1px solid var(--card-border);
      box-shadow: 0 16px 48px rgba(0, 0, 0, 0.6);
    }
    .picker-title {
      font-weight: 700;
      color: var(--text);
    }
    .picker-label {
      font-size: 0.8rem;
      color: var(--text-muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .picker-list {
      display: flex;
      flex-direction: column;
      gap: 0.3rem;
      min-height: 0;
      overflow-y: auto;
    }
    .picker-row {
      text-align: left;
      padding: 0.5rem 0.7rem;
      border-radius: 10px;
      border: 1px solid rgba(255, 255, 255, 0.06);
      background: rgba(255, 255, 255, 0.03);
      color: var(--text);
      font-size: 0.85rem;
      cursor: pointer;
    }
    .picker-row:hover {
      background: rgba(255, 255, 255, 0.08);
    }
    .picker-row.last {
      border-color: rgba(0, 210, 255, 0.4);
      background: rgba(0, 210, 255, 0.08);
    }
    .picker-row.new {
      color: #00e676;
    }

    /* Empty States */
    .empty-state {
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      flex: 1;
      min-height: 180px;
      padding: 2rem 1rem;
      color: var(--text-muted);
      text-align: center;
    }
    .empty-sub {
      font-size: 0.78rem;
      color: var(--text-dim);
      margin-top: 0.3rem;
    }

    /* Floating Toast */
    .toast {
      position: fixed;
      bottom: 24px;
      left: 50%;
      transform: translateX(-50%) translateY(30px);
      background: rgba(15, 20, 28, 0.95);
      border: 1px solid var(--accent);
      color: #ffffff;
      padding: 0.55rem 1.25rem;
      border-radius: 20px;
      font-size: 0.82rem;
      font-weight: 500;
      box-shadow: 0 8px 24px rgba(0, 0, 0, 0.6);
      pointer-events: none;
      opacity: 0;
      transition: all 0.25s ease;
      z-index: 100;
      white-space: nowrap;
    }
    .toast.show {
      transform: translateX(-50%) translateY(0);
      opacity: 1;
    }

    @media (max-width: 520px) {
      body {
        padding: 0.4rem;
      }
      .container {
        padding: 0.85rem;
        border-radius: 20px;
        height: calc(100dvh - 0.8rem);
        max-height: calc(100dvh - 0.8rem);
      }
      .artwork-container {
        width: min(180px, 24vh, 48vw);
        height: min(180px, 24vh, 48vw);
        margin-bottom: 0.5rem;
      }
      .title {
        font-size: 1.15rem;
      }
      .artist {
        font-size: 0.92rem;
      }
      .album {
        font-size: 0.78rem;
      }
      .controls {
        gap: 0.85rem;
        margin-bottom: 0.5rem;
      }
      .btn-primary {
        width: 52px;
        height: 52px;
      }
      .btn-secondary {
        width: 38px;
        height: 38px;
      }
      .volume-row {
        width: 90%;
      }
    }

    @media (max-height: 720px) {
      .artwork-container {
        width: min(160px, 22vh, 44vw);
        height: min(160px, 22vh, 44vw);
        margin-bottom: 0.4rem;
      }
      .header {
        margin-bottom: 0.4rem;
      }
      .tabs {
        margin-bottom: 0.5rem;
      }
      .track-info {
        margin-bottom: 0.35rem;
      }
      .badges {
        margin-bottom: 0.45rem;
      }
      .progress-container {
        margin-bottom: 0.45rem;
        padding: 4px 0;
      }
      .controls {
        margin-bottom: 0.45rem;
      }
      .up-next {
        margin-top: 0.35rem;
        padding-top: 0.35rem;
      }
    }
  </style>
</head>
<body>
  <div class="ambient-glow" id="glow"></div>

  <div class="container">
    <!-- Top Segmented Tabs -->
    <div class="tabs">
      <button class="tab-btn active" data-tab="tab-now-playing">Now Playing</button>
      <button class="tab-btn" data-tab="tab-queue" id="tabBtnQueue">Queue <span id="queueBadgeTab" class="tab-badge">0</span></button>
      <button class="tab-btn" data-tab="tab-browser" id="tabBtnBrowser">Library</button>
      <button class="tab-btn" data-tab="tab-devices" id="tabBtnDevices">Devices</button>
      <button class="tab-btn" data-tab="tab-playlists" id="tabBtnPlaylists">Playlists</button>
    </div>

    <!-- TAB 1: Now Playing -->
    <div class="tab-panel centered active" id="tab-now-playing">
      <div class="header">
        <div class="logo">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
            <polygon points="5 3 19 12 5 21 5 3"></polygon>
          </svg>
          <span>QUICKSTREAM</span>
        </div>
        <div class="status-badge">
          <div class="status-dot" id="statusDot"></div>
          <span id="statusLabel">OFFLINE</span>
        </div>
      </div>

      <div class="artwork-container">
        <img id="coverImg" class="artwork" src="/api/cover" alt="Album Cover">
      </div>

      <div class="track-info">
        <div class="title" id="title">Loading...</div>
        <div class="artist" id="artist"></div>
        <div class="album" id="album"></div>
      </div>

      <div class="badges" id="badges">
        <span class="badge" id="autopilotBadge" style="display:none;background:rgba(234, 179, 8, 0.2);color:#fde047;border:1px solid rgba(234, 179, 8, 0.4);">⚡ AUTOPILOT</span>
        <span class="badge format" id="formatBadge">AUDIO</span>
        <span class="badge" id="outputBadge">Local Audio</span>
        <span class="badge" id="queueBadge">Track 0 of 0</span>
      </div>

      <div class="progress-container" id="progressContainer">
        <div class="progress-bar-bg" id="progressBar">
          <div class="progress-bar-fill" id="progressFill"></div>
        </div>
        <div class="time-labels">
          <span id="elapsedTime">00:00</span>
          <span id="durationTime">00:00</span>
        </div>
      </div>

      <div class="controls">
        <button class="btn btn-secondary" id="btnPrev" title="Previous Track">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor">
            <polygon points="19 20 9 12 19 4 19 20"></polygon>
            <line x1="5" y1="19" x2="5" y2="5" stroke="currentColor" stroke-width="3"></line>
          </svg>
        </button>

        <button class="btn btn-secondary" id="btnSeekBack" title="Seek Backward 10s">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"></path>
            <path d="M3 3v5h5"></path>
            <text x="12" y="15.5" font-size="8" font-family="sans-serif" font-weight="bold" fill="currentColor" stroke="none" text-anchor="middle">10</text>
          </svg>
        </button>

        <button class="btn btn-primary" id="btnPlayPause" title="Play / Pause">
          <svg id="playIcon" width="24" height="24" viewBox="0 0 24 24" fill="currentColor">
            <polygon points="6 4 20 12 6 20 6 4"></polygon>
          </svg>
          <svg id="pauseIcon" width="24" height="24" viewBox="0 0 24 24" fill="currentColor" style="display:none;">
            <rect x="6" y="4" width="4" height="16"></rect>
            <rect x="14" y="4" width="4" height="16"></rect>
          </svg>
        </button>

        <button class="btn btn-secondary" id="btnSeekFwd" title="Seek Forward 10s">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12a9 9 0 1 1-9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"></path>
            <path d="M21 3v5h-5"></path>
            <text x="12" y="15.5" font-size="8" font-family="sans-serif" font-weight="bold" fill="currentColor" stroke="none" text-anchor="middle">10</text>
          </svg>
        </button>

        <button class="btn btn-secondary" id="btnNext" title="Next Track">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor">
            <polygon points="5 4 15 12 5 20 5 4"></polygon>
            <line x1="19" y1="5" x2="19" y2="19" stroke="currentColor" stroke-width="3"></line>
          </svg>
        </button>
      </div>

      <div class="volume-row">
        <svg viewBox="0 0 24 24" fill="currentColor">
          <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
        </svg>
        <input type="range" class="volume-slider" id="volumeSlider" min="0" max="100" value="100">
        <svg viewBox="0 0 24 24" fill="currentColor">
          <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
          <path d="M15.54 8.46a5 5 0 0 1 0 7.07" stroke="currentColor" stroke-width="2" fill="none"></path>
          <path d="M19.07 4.93a10 10 0 0 1 0 14.14" stroke="currentColor" stroke-width="2" fill="none"></path>
        </svg>
      </div>

      <div class="up-next" id="upNextRow" style="display:none;">
        <span>Up Next</span>
        <span class="up-next-title" id="upNextTitle"></span>
      </div>
    </div>

    <!-- TAB 2: Queue Management -->
    <div class="tab-panel" id="tab-queue">
      <div class="panel-header">
        <div>
          <span class="panel-title">Queue</span>
          <span class="panel-subtitle" id="queueSummaryText">0 tracks</span>
        </div>
        <div style="display:flex;gap:0.5rem;align-items:center;">
          <button class="btn-sm" id="btnShuffleQueue" title="Shuffle queue (randomize track order)">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="16 3 21 3 21 8"></polyline><line x1="4" y1="20" x2="21" y2="3"></line><polyline points="21 16 21 21 16 21"></polyline><line x1="15" y1="15" x2="21" y2="21"></line><line x1="4" y1="4" x2="9" y2="9"></line></svg>
            Shuffle
          </button>
          <button class="btn-sm" id="btnReshuffleSeed" title="Reshuffle Autopilot Seed (replace upcoming tracks from next track forward without interrupting playback)">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"></polygon></svg>
            Reshuffle Seed
          </button>
          <button class="btn-text danger" id="btnClearQueue">Clear Queue</button>
        </div>
      </div>

      <div class="mode-bar">
        <span class="mode-label">Mode:</span>
        <button class="mode-btn" data-mode="loop">Continuous</button>
        <button class="mode-btn" data-mode="stop">Stop at End</button>
        <button class="mode-btn" data-mode="repeat_one">Repeat 1</button>
      </div>

      <div class="queue-list" id="queueList"></div>
    </div>

    <!-- TAB 3: Library / File Browser -->
    <div class="tab-panel" id="tab-browser">
      <div class="bookmarks-row" id="bookmarksRow" style="display:none;"></div>

      <div class="browser-path-bar">
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" style="flex-shrink:0;opacity:0.6;"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path></svg>
        <div class="browser-current-path" id="browserCurrentPath" title="Current Directory">/</div>
      </div>

      <div class="browser-actions-bar">
        <button class="btn-sm" id="btnBrowserUp" title="Parent Directory">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="15 18 9 12 15 6"></polyline></svg>
          Up
        </button>
        <button class="btn-sm" id="btnBrowserTerminal" title="Jump to Terminal Browsed Directory">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="4 17 10 11 4 5"></polyline><line x1="12" y1="19" x2="20" y2="19"></line></svg>
          Terminal
        </button>
        <button class="btn-sm" id="btnBrowserHome" title="Jump to Startup Home Directory">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"></path><polyline points="9 22 9 12 15 12 15 22"></polyline></svg>
          Home
        </button>
        <button class="btn-sm" id="btnBrowserSetHome" title="Set current directory as startup home">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M19 21l-7-5-7 5V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2z"></path></svg>
          Set Home
        </button>
        <button class="btn-sm accent" id="btnBrowserAddAll" title="Add all audio files in this folder">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="12" y1="5" x2="12" y2="19"></line><line x1="5" y1="12" x2="19" y2="12"></line></svg>
          Add All
        </button>
        <button class="btn-sm accent" id="btnBrowserAutopilot" style="background: linear-gradient(135deg, rgba(234, 179, 8, 0.25), rgba(245, 158, 11, 0.15)); border-color: rgba(234, 179, 8, 0.4); color: #fde047;" title="Autopilot (start if empty, append 60 tracks if playing)">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polygon points="13 2 3 14 12 14 11 22 21 10 12 10 13 2"></polygon></svg>
          Autopilot
        </button>
        <button class="btn-sm accent" id="btnBrowserReshuffle" style="background: linear-gradient(135deg, rgba(168, 85, 247, 0.25), rgba(139, 92, 246, 0.15)); border-color: rgba(168, 85, 247, 0.4); color: #d8b4fe;" title="Reshuffle Autopilot Seed (replace upcoming tracks from next track forward without interrupting playback)">
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="16 3 21 3 21 8"></polyline><line x1="4" y1="20" x2="21" y2="3"></line><polyline points="21 16 21 21 16 21"></polyline><line x1="15" y1="15" x2="21" y2="21"></line><line x1="4" y1="4" x2="9" y2="9"></line></svg>
          Reshuffle Seed
        </button>
      </div>

      <div class="mode-bar" style="margin-top:0.35rem;margin-bottom:0.75rem;">
        <span class="mode-label">Mix Vibe:</span>
        <button class="mix-profile-btn mode-btn active" data-profile="diverse" title="Diverse rotation across all artists">Diverse</button>
        <button class="mix-profile-btn mode-btn" data-profile="bpm_flow" title="Smooth BPM tempo gradient & harmonic keys">BPM Flow</button>
        <button class="mix-profile-btn mode-btn" data-profile="chill" title="Ambient, acoustic & downtempo vibe">Chill</button>
        <button class="mix-profile-btn mode-btn" data-profile="workout" title="High energy, dance & electronic vibe">Workout</button>
        <button class="mix-profile-btn mode-btn" data-profile="groove" title="Soul, funk & disco groove vibe">Groove</button>
      </div>

      <input type="text" id="browserSearchInput" class="search-input" placeholder="Filter files and folders...">

      <div class="browser-list" id="browserList"></div>
    </div>

    <!-- TAB 4: Output Devices -->
    <div class="tab-panel" id="tab-devices">
      <div class="panel-header">
        <div>
          <span class="panel-title">Output Devices</span>
          <span class="panel-subtitle" id="deviceScanTag"></span>
        </div>
        <button class="btn-sm" id="btnRescanDevices">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M21.5 2v6h-6M21.34 15.57a10 10 0 1 1-.57-8.38l5.67-5.67"/></svg>
          Rescan
        </button>
      </div>

      <div class="devices-list" id="devicesList"></div>
    </div>

    <!-- TAB 5: Playlists (issue #50) -->
    <div class="tab-panel" id="tab-playlists">
      <div class="panel-header">
        <div>
          <span class="panel-title">Playlists</span>
          <span class="panel-subtitle" id="playlistsSummaryText"></span>
        </div>
        <div style="display:flex;gap:0.5rem;align-items:center;">
          <button class="btn-sm" id="btnPlaylistAddCurrent" title="Add the now-playing track to a playlist">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M9 18V5l12-2v13"></path><circle cx="6" cy="18" r="3"></circle><circle cx="18" cy="16" r="3"></circle></svg>
            Add Playing
          </button>
          <button class="btn-sm accent" id="btnPlaylistCreate" title="Create a playlist">
            <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="12" y1="5" x2="12" y2="19"></line><line x1="5" y1="12" x2="19" y2="12"></line></svg>
            New
          </button>
        </div>
      </div>

      <div class="queue-list" id="playlistsList"></div>
    </div>
  </div>

  <div class="toast" id="toast"></div>

  <!-- Add-to-playlist picker (issue #50) -->
  <div class="picker-backdrop" id="playlistPicker">
    <div class="picker" role="dialog" aria-label="Add to playlist">
      <div class="picker-title">Add to playlist</div>
      <div class="picker-label" id="playlistPickerLabel"></div>
      <div class="picker-list" id="playlistPickerList"></div>
      <button class="btn-text" id="btnPlaylistPickerCancel">Cancel</button>
    </div>
  </div>

  <script>
    let localPosition = 0;
    let localDuration = 0;
    let isPlaying = false;
    let lastPollTime = Date.now();
    let lastStatus = null;
    let currentTrackPath = "";
    let isScrubbing = false;
    let scrubPosition = 0;

    let currentBrowserPath = "";
    let currentBrowserData = null;
    let latestTerminalDir = "";
    let toastTimer = null;

    function formatTime(seconds) {
      if (!seconds || isNaN(seconds) || seconds < 0) return "00:00";
      const total = Math.floor(seconds);
      const mins = Math.floor(total / 60);
      const secs = total % 60;
      return `${String(mins).padStart(2, '0')}:${String(secs).padStart(2, '0')}`;
    }

    // --- SAFE DOM BUILDING ---
    // Track titles, file names, paths and device names are untrusted (tags,
    // shared volumes, SSDP). They are only ever written with textContent or
    // dataset, never parsed as HTML or JS. Markup strings are constants only.
    const ICON_SEED_RADIO = '<span style="font-size:12px;">📻</span>';
    const ICON_REMOVE = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="18" y1="6" x2="6" y2="18"></line><line x1="6" y1="6" x2="18" y2="18"></line></svg>';
    const ICON_ADD = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="12" y1="5" x2="12" y2="19"></line><line x1="5" y1="12" x2="19" y2="12"></line></svg>';
    const ICON_PLAYLIST_ADD = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><line x1="3" y1="6" x2="15" y2="6"></line><line x1="3" y1="12" x2="15" y2="12"></line><line x1="3" y1="18" x2="11" y2="18"></line><line x1="18" y1="13" x2="18" y2="21"></line><line x1="14" y1="17" x2="22" y2="17"></line></svg>';

    function el(tag, className, text) {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (text !== undefined && text !== null) node.textContent = String(text);
      return node;
    }

    function iconButton(className, title, action, iconMarkup) {
      const btn = el('button', className);
      btn.title = title;
      btn.dataset.action = action;
      btn.innerHTML = iconMarkup;
      return btn;
    }

    function clearChildren(node) {
      node.textContent = '';
    }

    // Find the innermost element carrying a data-action between the click
    // target and the delegating container.
    function findActionTarget(target, container) {
      for (let n = target; n && n !== container; n = n.parentNode) {
        if (n.dataset && n.dataset.action) return n;
      }
      return null;
    }

    function delegateClicks(containerId, handlers) {
      const container = document.getElementById(containerId);
      if (!container) return;
      container.addEventListener('click', (e) => {
        const node = findActionTarget(e.target, container);
        if (!node) return;
        const handler = handlers[node.dataset.action];
        if (handler) handler(node.dataset);
      });
    }

    function showToast(msg) {
      const t = document.getElementById('toast');
      if (!t) return;
      t.textContent = msg;
      t.classList.add('show');
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => {
        t.classList.remove('show');
      }, 2200);
    }

    async function sendAction(action, value = null, extra = {}) {
      try {
        const payload = Object.assign({ action, value }, extra);
        await fetch('/api/action', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload)
        });
        setTimeout(pollStatus, 80);
      } catch (e) {
        console.error("Action error:", e);
      }
    }

    async function pollStatus() {
      try {
        const res = await fetch('/api/status?t=' + Date.now());
        if (res.status === 403) {
          const body = await res.json().catch(() => ({}));
          if (body.error === "remote access is off") {
            document.getElementById('statusDot').className = "status-dot";
            document.getElementById('statusLabel').textContent = "REMOTE ACCESS OFF";
            return;
          }
        }
        if (!res.ok) throw new Error("Status failed");
        const data = await res.json();
        renderStatus(data);
      } catch (e) {
        document.getElementById('statusDot').className = "status-dot";
        document.getElementById('statusLabel').textContent = "OFFLINE";
      }
    }

    function renderStatus(data) {
      lastStatus = data;
      // 1. Now Playing interpolation
      if (!isScrubbing) {
        isPlaying = data.is_playing && !data.is_paused;
        localPosition = data.position_seconds || 0;
        localDuration = data.duration_seconds || 0;
        lastPollTime = Date.now();
      } else {
        if (data.duration_seconds) localDuration = data.duration_seconds;
      }

      // Track titles
      document.getElementById('title').textContent = data.title || "No Track Loaded";
      document.getElementById('artist').textContent = data.artist || "";
      document.getElementById('album').textContent = data.album || "";

      // Cover Art cache bust on track change
      const newPath = data.filepath || "";
      if (newPath !== currentTrackPath) {
        currentTrackPath = newPath;
        document.getElementById('coverImg').src = '/api/cover?t=' + Date.now();
      }

      // Play/Pause icon & status badge
      const dot = document.getElementById('statusDot');
      const label = document.getElementById('statusLabel');
      const playIcon = document.getElementById('playIcon');
      const pauseIcon = document.getElementById('pauseIcon');

      if (isPlaying) {
        dot.className = "status-dot playing";
        label.textContent = "PLAYING";
        playIcon.style.display = "none";
        pauseIcon.style.display = "block";
      } else if (data.is_paused) {
        dot.className = "status-dot paused";
        label.textContent = "PAUSED";
        playIcon.style.display = "block";
        pauseIcon.style.display = "none";
      } else {
        dot.className = "status-dot";
        label.textContent = "STOPPED";
        playIcon.style.display = "block";
        pauseIcon.style.display = "none";
      }

      // Badges
      const apBadge = document.getElementById('autopilotBadge');
      if (apBadge) {
        apBadge.style.display = data.is_autopilot ? 'inline-flex' : 'none';
        if (data.active_mix_profile) {
          apBadge.textContent = '⚡ ' + data.active_mix_profile;
        }
      }

      let fmtText = data.format || "AUDIO";
      if (data.bpm) {
        fmtText += ` · ${Math.round(data.bpm)} BPM`;
      }
      if (data.key) {
        fmtText += ` · ${data.key}`;
      }
      if (data.sample_rate_hz > 0) {
        fmtText += ` · ${(data.sample_rate_hz / 1000).toFixed(1)}kHz`;
      }
      if (data.bitrate_kbps > 0) {
        fmtText += ` · ${data.bitrate_kbps}kbps`;
      }
      document.getElementById('formatBadge').textContent = fmtText;
      document.getElementById('outputBadge').textContent = data.output_device || "Local Audio";

      if (data.active_mix_profile) {
        const profLower = data.active_mix_profile.toLowerCase();
        document.querySelectorAll('.mix-profile-btn').forEach(b => {
          const key = b.getAttribute('data-profile');
          if (key && profLower.includes(key.replace('_', ' '))) {
            b.classList.add('active');
          } else if (key && profLower.includes(key)) {
            b.classList.add('active');
          } else {
            b.classList.remove('active');
          }
        });
      }

      if (data.queue_total > 0) {
        document.getElementById('queueBadge').textContent = `Track ${data.queue_index + 1} of ${data.queue_total}`;
      } else {
        document.getElementById('queueBadge').textContent = "Queue Empty";
      }

      // Volume slider
      if (document.activeElement !== document.getElementById('volumeSlider')) {
        document.getElementById('volumeSlider').value = data.volume;
      }

      // Up Next
      const upNextRow = document.getElementById('upNextRow');
      const upNextTitle = document.getElementById('upNextTitle');
      if (data.next_track_title) {
        upNextRow.style.display = "flex";
        upNextTitle.textContent = data.next_track_title;
      } else {
        upNextRow.style.display = "none";
      }

      updateProgressUI();

      // 2. Queue view sync
      renderQueue(data);

      // 3. Devices view sync
      renderDevices(data);

      if (data.current_browser_dir) {
        latestTerminalDir = data.current_browser_dir;
      }

      // 4. Playlists: fetch again when the app published a new snapshot.
      if (data.playlists_version !== playlistsVersion) {
        playlistsVersion = data.playlists_version;
        if (document.getElementById('tab-playlists').classList.contains('active')) {
          loadPlaylists();
        }
      }
    }

    function updateProgressUI() {
      let currentPos = localPosition;
      if (isScrubbing) {
        currentPos = scrubPosition;
      } else if (isPlaying) {
        const elapsed = (Date.now() - lastPollTime) / 1000;
        currentPos = Math.min(localDuration, localPosition + elapsed);
      }

      const pct = localDuration > 0 ? (currentPos / localDuration) * 100 : 0;
      document.getElementById('progressFill').style.width = Math.min(100, Math.max(0, pct)) + "%";
      document.getElementById('elapsedTime').textContent = formatTime(currentPos);
      document.getElementById('durationTime').textContent = formatTime(localDuration);
    }

    // Scrubbing and seeking
    function getSeekPositionFromEvent(e) {
      const bar = document.getElementById('progressBar');
      const rect = bar.getBoundingClientRect();
      let clientX = e.clientX;
      if (clientX === undefined) {
        if (e.touches && e.touches.length > 0) clientX = e.touches[0].clientX;
        else if (e.changedTouches && e.changedTouches.length > 0) clientX = e.changedTouches[0].clientX;
        else clientX = 0;
      }
      const offsetX = Math.max(0, Math.min(clientX - rect.left, rect.width));
      const ratio = rect.width > 0 ? offsetX / rect.width : 0;
      return ratio * localDuration;
    }

    const progressContainer = document.getElementById('progressContainer');

    function onScrubStart(e) {
      if (localDuration <= 0) return;
      isScrubbing = true;
      progressContainer.classList.add('scrubbing');
      scrubPosition = getSeekPositionFromEvent(e);
      updateProgressUI();
    }

    function onScrubMove(e) {
      if (!isScrubbing) return;
      scrubPosition = getSeekPositionFromEvent(e);
      updateProgressUI();
    }

    function onScrubEnd(e) {
      if (!isScrubbing) return;
      isScrubbing = false;
      progressContainer.classList.remove('scrubbing');
      const target = getSeekPositionFromEvent(e);
      localPosition = target;
      lastPollTime = Date.now();
      updateProgressUI();
      sendAction('seek', target);
    }

    progressContainer.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      onScrubStart(e);

      function onPointerMove(ev) {
        onScrubMove(ev);
      }
      function onPointerUp(ev) {
        window.removeEventListener('pointermove', onPointerMove);
        window.removeEventListener('pointerup', onPointerUp);
        window.removeEventListener('pointercancel', onPointerUp);
        onScrubEnd(ev);
      }

      window.addEventListener('pointermove', onPointerMove);
      window.addEventListener('pointerup', onPointerUp);
      window.addEventListener('pointercancel', onPointerUp);
    });

    // Seek buttons
    document.getElementById('btnSeekBack').addEventListener('click', () => sendAction('seek_relative', -10));
    document.getElementById('btnSeekFwd').addEventListener('click', () => sendAction('seek_relative', 10));

    // Interactive playback controls
    document.getElementById('btnPlayPause').addEventListener('click', () => sendAction('toggle_pause'));
    document.getElementById('btnNext').addEventListener('click', () => sendAction('next'));
    document.getElementById('btnPrev').addEventListener('click', () => sendAction('prev'));

    document.getElementById('volumeSlider').addEventListener('change', (e) => {
      sendAction('volume', parseFloat(e.target.value));
    });

    // --- TAB SWITCHING ---
    document.querySelectorAll('.tab-btn').forEach(btn => {
      btn.addEventListener('click', () => {
        document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
        document.querySelectorAll('.tab-panel').forEach(p => p.classList.remove('active'));

        btn.classList.add('active');
        const targetId = btn.getAttribute('data-tab');
        const targetPanel = document.getElementById(targetId);
        if (targetPanel) {
          targetPanel.classList.add('active');
        }

        if (targetId === 'tab-browser' && !currentBrowserData) {
          loadBrowser();
        }
        if (targetId === 'tab-playlists') {
          loadPlaylists();
        }
      });
    });

    // --- QUEUE MANAGEMENT ---
    function renderQueue(data) {
      const queue = data.queue || [];
      document.getElementById('queueBadgeTab').textContent = queue.length;

      let totalSecs = 0;
      for (const item of queue) {
        totalSecs += item.duration_seconds || 0;
      }
      document.getElementById('queueSummaryText').textContent = `${queue.length} track${queue.length === 1 ? '' : 's'} · ${formatTime(totalSecs)}`;

      // Mode buttons
      const mode = data.queue_mode || 'loop';
      document.querySelectorAll('.mode-btn').forEach(btn => {
        if (btn.getAttribute('data-mode') === mode) {
          btn.classList.add('active');
        } else {
          btn.classList.remove('active');
        }
      });

      const list = document.getElementById('queueList');
      if (queue.length === 0) {
        list.innerHTML = `
          <div class="empty-state">
            <svg width="40" height="40" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
              <path d="M9 18V5l12-2v13"></path>
              <circle cx="6" cy="18" r="3"></circle>
              <circle cx="18" cy="16" r="3"></circle>
            </svg>
            <div style="font-weight:600; margin-top:0.5rem;">Queue is empty</div>
            <div class="empty-sub">Add tracks or folders from the Library tab.</div>
          </div>`;
        return;
      }

      clearChildren(list);
      for (const item of queue) {
        const index = String(item.index);
        const row = el('div', item.is_current ? 'queue-item current' : 'queue-item');

        const main = el('div', 'queue-item-main');
        main.dataset.action = 'play';
        main.dataset.index = index;
        main.appendChild(el('span', 'queue-num', item.is_current ? '▶' : (item.index + 1) + '.'));
        const meta = el('div', 'queue-item-meta');
        meta.appendChild(el('div', 'queue-item-title', item.title || ''));
        meta.appendChild(el('div', 'queue-item-artist', item.artist || 'Unknown Artist'));
        main.appendChild(meta);
        row.appendChild(main);

        row.appendChild(el('span', 'queue-item-dur', formatTime(item.duration_seconds)));

        // Tracks outside the allowed folders are listed without a path and cannot seed a station.
        if (item.filepath) {
          const seedBtn = iconButton('btn-icon-sm', 'Seed Radio station from this track', 'seed', ICON_SEED_RADIO);
          seedBtn.dataset.path = item.filepath;
          row.appendChild(seedBtn);
        }

        const removeBtn = iconButton('btn-icon-sm danger-hover', 'Remove track', 'remove', ICON_REMOVE);
        removeBtn.dataset.index = index;
        row.appendChild(removeBtn);

        list.appendChild(row);
      }
    }

    delegateClicks('queueList', {
      play: (d) => playIndex(Number(d.index)),
      seed: (d) => seedRadio(d.path),
      remove: (d) => removeIndex(Number(d.index)),
    });

    function playIndex(idx) {
      sendAction('play_index', null, { index: idx });
      showToast(`Playing track ${idx + 1}`);
    }

    function removeIndex(idx) {
      sendAction('remove_index', null, { index: idx });
      showToast('Track removed from queue');
    }

    document.getElementById('btnClearQueue').addEventListener('click', () => {
      if (confirm('Clear the entire playback queue?')) {
        sendAction('clear_queue');
        showToast('Queue cleared');
      }
    });

    document.getElementById('btnShuffleQueue')?.addEventListener('click', () => {
      sendAction('shuffle');
      showToast('Queue shuffled');
    });

    document.getElementById('btnReshuffleSeed')?.addEventListener('click', () => {
      sendAction('autopilot_reshuffle');
      showToast('Reshuffling Autopilot seed from next track forward...');
    });

    document.querySelectorAll('.mode-btn').forEach(btn => {
      btn.addEventListener('click', () => {
        const mode = btn.getAttribute('data-mode');
        sendAction('set_queue_mode', null, { mode });
        showToast(`Queue mode: ${mode}`);
      });
    });

    // --- LIBRARY / FILE BROWSER ---
    async function loadBrowser(path = "") {
      try {
        const url = path ? '/api/browser?path=' + encodeURIComponent(path) : '/api/browser';
        const res = await fetch(url);
        if (!res.ok) throw new Error("Failed to load browser");
        const data = await res.json();
        currentBrowserData = data;
        currentBrowserPath = data.current_path;
        renderBrowser(data, document.getElementById('browserSearchInput').value.trim().toLowerCase());
      } catch (e) {
        console.error("Browser error:", e);
      }
    }

    function renderBrowser(data, filter = "") {
      document.getElementById('browserCurrentPath').textContent = data.current_path || "/";
      document.getElementById('btnBrowserUp').disabled = !data.parent_path;

      if (data.terminal_dir) {
        latestTerminalDir = data.terminal_dir;
      }

      const setHomeBtn = document.getElementById('btnBrowserSetHome');
      if (data.home_dir && data.home_dir === data.current_path) {
        setHomeBtn.classList.add('accent');
        setHomeBtn.title = "Current directory is startup home (click to clear)";
      } else {
        setHomeBtn.classList.remove('accent');
        setHomeBtn.title = "Set current directory as startup home";
      }

      // Bookmarks chips
      const bmRow = document.getElementById('bookmarksRow');
      if (data.bookmarks && data.bookmarks.length > 0) {
        bmRow.style.display = "flex";
        clearChildren(bmRow);
        for (const b of data.bookmarks) {
          const parts = String(b).split(/[/\\]/);
          const name = parts[parts.length - 1] || parts[parts.length - 2] || b;
          const chip = el('button', 'chip-btn', '★ ' + name);
          chip.dataset.action = 'browse';
          chip.dataset.path = b;
          bmRow.appendChild(chip);
        }
      } else {
        bmRow.style.display = "none";
      }

      // Filter and render entries
      let entries = data.entries || [];
      if (filter) {
        entries = entries.filter(e => e.name.toLowerCase().includes(filter));
      }

      const list = document.getElementById('browserList');
      if (entries.length === 0) {
        list.innerHTML = `<div class="empty-sub" style="text-align:center; padding: 2rem 0;">No matching files or folders</div>`;
        return;
      }

      clearChildren(list);
      for (const item of entries) {
        const path = item.path || '';
        if (item.is_dir) {
          const row = el('div', 'browser-item dir');
          row.dataset.action = 'browse';
          row.dataset.path = path;
          row.appendChild(el('span', 'browser-icon', '📁'));
          row.appendChild(el('span', 'browser-name', (item.name || '') + '/'));
          row.appendChild(el('span', 'browser-arrow', '›'));
          list.appendChild(row);
        } else {
          const row = el('div', 'browser-item file');
          row.dataset.action = 'add';
          row.dataset.path = path;
          row.appendChild(el('span', 'browser-icon', '🎵'));
          row.appendChild(el('span', 'browser-name', item.name || ''));
          const actions = el('div');
          actions.style.display = 'flex';
          actions.style.gap = '0.35rem';
          actions.style.alignItems = 'center';
          const seedBtn = iconButton('btn-icon-sm', 'Seed Radio station from this track', 'seed', ICON_SEED_RADIO);
          seedBtn.dataset.path = path;
          actions.appendChild(seedBtn);
          const playlistBtn = iconButton('btn-icon-sm', 'Add to playlist', 'to-playlist', ICON_PLAYLIST_ADD);
          playlistBtn.dataset.path = path;
          playlistBtn.dataset.name = item.name || '';
          actions.appendChild(playlistBtn);
          const addBtn = iconButton('btn-icon-sm add-btn', 'Add to queue', 'add', ICON_ADD);
          addBtn.dataset.path = path;
          actions.appendChild(addBtn);
          row.appendChild(actions);
          list.appendChild(row);
        }
      }
    }

    const browserClickHandlers = {
      browse: (d) => browseTo(d.path),
      add: (d) => addFile(d.path),
      seed: (d) => seedRadio(d.path),
      'to-playlist': (d) => openPlaylistPicker(d.name, d.path),
    };
    delegateClicks('browserList', browserClickHandlers);
    delegateClicks('bookmarksRow', browserClickHandlers);

    function browseTo(path) {
      document.getElementById('browserSearchInput').value = "";
      loadBrowser(path);
    }

    document.getElementById('btnBrowserTerminal').addEventListener('click', () => {
      if (latestTerminalDir) {
        browseTo(latestTerminalDir);
      } else {
        browseTo("");
      }
    });

    document.getElementById('btnBrowserHome').addEventListener('click', () => {
      if (currentBrowserData && currentBrowserData.home_dir) {
        browseTo(currentBrowserData.home_dir);
      } else {
        browseTo("");
      }
    });

    document.getElementById('btnBrowserUp').addEventListener('click', () => {
      if (currentBrowserData && currentBrowserData.parent_path) {
        browseTo(currentBrowserData.parent_path);
      }
    });

    document.getElementById('btnBrowserSetHome').addEventListener('click', async () => {
      if (currentBrowserPath) {
        const isCurrentHome = currentBrowserData && currentBrowserData.home_dir === currentBrowserPath;
        if (isCurrentHome) {
          await sendAction('set_home', null, { path: null });
          showToast('Startup home directory cleared');
        } else {
          await sendAction('set_home', null, { path: currentBrowserPath });
          showToast(`Startup home set to ${currentBrowserPath}`);
        }
        await loadBrowser(currentBrowserPath);
      }
    });

    document.getElementById('btnBrowserAddAll').addEventListener('click', () => {
      if (currentBrowserPath) {
        sendAction('add_folder', null, { path: currentBrowserPath });
        showToast('Adding all audio files in folder...');
      }
    });

    document.getElementById('btnBrowserAutopilot')?.addEventListener('click', () => {
      if (currentBrowserPath) {
        sendAction('autopilot', null, { path: currentBrowserPath });
        showToast('Starting Autopilot on folder...');
      }
    });

    document.getElementById('btnBrowserReshuffle')?.addEventListener('click', () => {
      if (currentBrowserPath) {
        sendAction('autopilot_reshuffle', null, { path: currentBrowserPath });
        showToast('Reshuffling Autopilot seed from next track forward...');
      }
    });

    document.getElementById('browserSearchInput').addEventListener('input', (e) => {
      if (currentBrowserData) {
        renderBrowser(currentBrowserData, e.target.value.trim().toLowerCase());
      }
    });

    async function addFile(path) {
      await sendAction('add_file', null, { path });
      showToast('Added track to queue');
    }

    async function seedRadio(path) {
      if (!path) return;
      await sendAction('seed_radio', null, { path });
      showToast('📻 Starting Seed Radio station...');
    }

    document.querySelectorAll('.mix-profile-btn').forEach(btn => {
      btn.addEventListener('click', () => {
        const profile = btn.getAttribute('data-profile');
        sendAction('set_mix_profile', null, { mode: profile });
        document.querySelectorAll('.mix-profile-btn').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');
        showToast(`Mix profile: ${btn.innerText}`);
      });
    });

    // --- DEVICES TAB ---
    function renderDevices(data) {
      const devices = data.devices || [];
      const scanTag = document.getElementById('deviceScanTag');
      if (data.is_scanning) {
        scanTag.textContent = "(Scanning...)";
      } else {
        scanTag.textContent = "";
      }

      const list = document.getElementById('devicesList');
      if (devices.length === 0) {
        list.innerHTML = `<div class="empty-sub" style="text-align:center; padding: 2rem 0;">No devices detected. Click Rescan to search.</div>`;
        return;
      }

      clearChildren(list);
      for (const dev of devices) {
        const row = el('div', dev.is_active ? 'device-item active' : 'device-item');
        row.dataset.action = 'select';
        row.dataset.id = String(dev.id);
        row.appendChild(el('div', 'device-radio', dev.is_active ? '●' : '○'));
        const info = el('div', 'device-info');
        const name = el('div', 'device-name');
        name.appendChild(document.createTextNode(dev.name || ''));
        if (dev.is_active) {
          name.appendChild(el('span', 'active-tag', 'ACTIVE'));
        }
        info.appendChild(name);
        info.appendChild(el('div', 'device-host', dev.host_port || ''));
        row.appendChild(info);
        list.appendChild(row);
      }
    }

    delegateClicks('devicesList', {
      select: (d) => selectDevice(Number(d.id)),
    });

    function selectDevice(id) {
      sendAction('select_device', null, { id });
      showToast('Switching audio output...');
    }

    document.getElementById('btnRescanDevices').addEventListener('click', () => {
      sendAction('rescan_devices');
      showToast('Scanning network for DLNA renderers...');
    });

    // --- PLAYLISTS TAB (issue #50) ---
    // The snapshot comes from GET /api/playlists. Actions that name a track by index send the
    // snapshot's revision, so Quickstream refuses them if the playlists changed meanwhile.
    let playlistsData = null;
    let playlistsVersion = null;
    const openPlaylists = new Set();
    const ICON_PLAY = '<svg width="13" height="13" viewBox="0 0 24 24" fill="currentColor"><polygon points="6 4 20 12 6 20 6 4"></polygon></svg>';
    const ICON_RENAME = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><path d="M12 20h9"></path><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"></path></svg>';
    const ICON_UP = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="18 15 12 9 6 15"></polyline></svg>';
    const ICON_DOWN = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5"><polyline points="6 9 12 15 18 9"></polyline></svg>';

    // m:ss, or h:mm:ss from one hour; -:-- when unknown (as in the terminal).
    function playlistDuration(seconds) {
      if (seconds === null || seconds === undefined || seconds < 0) return '-:--';
      const total = Math.floor(seconds);
      const hours = Math.floor(total / 3600);
      const mins = Math.floor((total % 3600) / 60);
      const secs = String(total % 60).padStart(2, '0');
      return hours > 0 ? `${hours}:${String(mins).padStart(2, '0')}:${secs}` : `${mins}:${secs}`;
    }

    function trackCountLabel(count) {
      return count === 1 ? '1 track' : `${count} tracks`;
    }

    function playlistEntryLabel(entry) {
      return entry.artist ? `${entry.artist} - ${entry.title}` : (entry.title || '');
    }

    async function loadPlaylists() {
      try {
        const res = await fetch('/api/playlists?t=' + Date.now());
        if (!res.ok) throw new Error("Failed to load playlists");
        renderPlaylists(await res.json());
      } catch (e) {
        console.error("Playlists error:", e);
      }
    }

    function renderPlaylists(data) {
      playlistsData = data;
      if (pickerOpen()) renderPlaylistPicker(data);
      const list = document.getElementById('playlistsList');
      const summary = document.getElementById('playlistsSummaryText');
      clearChildren(list);
      if (!data || data.state !== 'ready') {
        summary.textContent = '';
        list.appendChild(el('div', 'empty-sub playlists-note', (data && data.message) || 'Loading playlists…'));
        return;
      }
      const playlists = data.playlists || [];
      summary.textContent = playlists.length === 1 ? '1 playlist' : `${playlists.length} playlists`;
      if (playlists.length === 0) {
        list.appendChild(el('div', 'empty-sub playlists-note', 'No playlists yet.'));
        return;
      }
      for (const playlist of playlists) {
        const name = playlist.name || '';
        const open = openPlaylists.has(name);
        const card = el('div', open ? 'playlist-card open' : 'playlist-card');

        const head = el('div', 'playlist-head');
        const main = el('div', 'playlist-head-main');
        main.dataset.action = 'toggle';
        main.dataset.name = name;
        main.appendChild(el('span', 'playlist-chevron', '›'));
        const meta = el('div', 'queue-item-meta');
        meta.appendChild(el('div', 'playlist-name', name));
        meta.appendChild(el('div', 'playlist-meta',
          `${trackCountLabel(playlist.track_count)} · ${playlistDuration(playlist.total_duration_seconds)}`));
        main.appendChild(meta);
        head.appendChild(main);
        const playBtn = iconButton('btn-icon-sm', 'Play playlist (replaces the queue)', 'play', ICON_PLAY);
        playBtn.dataset.name = name;
        head.appendChild(playBtn);
        const appendBtn = iconButton('btn-icon-sm', 'Append playlist to the queue', 'append', ICON_ADD);
        appendBtn.dataset.name = name;
        head.appendChild(appendBtn);
        const renameBtn = iconButton('btn-icon-sm', 'Rename playlist', 'rename', ICON_RENAME);
        renameBtn.dataset.name = name;
        head.appendChild(renameBtn);
        const deleteBtn = iconButton('btn-icon-sm danger-hover', 'Delete playlist', 'delete', ICON_REMOVE);
        deleteBtn.dataset.name = name;
        deleteBtn.dataset.count = String(playlist.track_count);
        head.appendChild(deleteBtn);
        card.appendChild(head);

        if (open) {
          card.appendChild(renderPlaylistTracks(playlist));
        }
        list.appendChild(card);
      }
    }

    function renderPlaylistTracks(playlist) {
      const tracks = el('div', 'playlist-tracks');
      const entries = playlist.entries || [];
      if (entries.length === 0) {
        tracks.appendChild(el('div', 'empty-sub', 'Empty. Add tracks from the Library.'));
        return tracks;
      }
      for (const entry of entries) {
        const missing = entry.missing === true;
        let className = 'playlist-track';
        if (missing) className += ' missing';
        if (entry.outside) className += ' outside';
        const row = el('div', className);

        const main = el('div', 'queue-item-main');
        main.dataset.action = 'play-from';
        main.dataset.name = playlist.name || '';
        main.dataset.index = String(entry.index);
        main.appendChild(el('span', 'queue-num', (entry.index + 1) + '.'));
        const meta = el('div', 'queue-item-meta');
        const label = playlistEntryLabel(entry);
        meta.appendChild(el('div', 'queue-item-title', missing ? '✗ missing: ' + label : label));
        if (entry.outside) {
          meta.appendChild(el('div', 'queue-item-artist', 'Outside your home, bookmarks and launch folder'));
        }
        main.appendChild(meta);
        row.appendChild(main);
        row.appendChild(el('span', 'queue-item-dur', missing ? '----' : playlistDuration(entry.duration_seconds)));
        for (const [title, action, icon] of [
          ['Move up', 'move-up', ICON_UP],
          ['Move down', 'move-down', ICON_DOWN],
          ['Remove from playlist', 'remove', ICON_REMOVE],
        ]) {
          const btn = iconButton(action === 'remove' ? 'btn-icon-sm danger-hover' : 'btn-icon-sm', title, action, icon);
          btn.dataset.name = playlist.name || '';
          btn.dataset.index = String(entry.index);
          row.appendChild(btn);
        }
        tracks.appendChild(row);
      }
      return tracks;
    }

    // Send a playlist action and report Quickstream's answer.
    async function playlistAction(action, extra, doneMessage) {
      try {
        const res = await fetch('/api/action', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(Object.assign({ action, value: null }, extra))
        });
        let data = {};
        try {
          data = await res.json();
        } catch (e) {
          data = {};
        }
        if (!res.ok || data.ok === false) {
          showToast(data.error || 'Request failed');
          loadPlaylists();
          return false;
        }
        showToast(data.message || doneMessage || 'Done');
        loadPlaylists();
        setTimeout(pollStatus, 80);
        return true;
      } catch (e) {
        console.error("Playlist action error:", e);
        return false;
      }
    }

    function playlistRevision() {
      return playlistsData ? playlistsData.revision : null;
    }

    function togglePlaylist(name) {
      if (openPlaylists.has(name)) {
        openPlaylists.delete(name);
      } else {
        openPlaylists.add(name);
      }
      if (playlistsData) renderPlaylists(playlistsData);
    }

    function playPlaylist(name, index) {
      playlistAction('playlist_play', { name, index, revision: playlistRevision() }, `Playing ${name}`);
    }

    function appendPlaylist(name) {
      playlistAction('playlist_append', { name }, `Appending ${name}`);
    }

    function renamePlaylist(name) {
      const newName = prompt(`Rename "${name}" to:`, name);
      if (newName === null) return;
      playlistAction('playlist_rename', { name, new_name: newName });
    }

    function deletePlaylist(name, count) {
      if (!confirm(`Delete "${name}" (${trackCountLabel(count)})?`)) return;
      playlistAction('playlist_delete', { name });
    }

    function moveTrack(name, index, direction) {
      playlistAction('playlist_move', { name, index, direction, revision: playlistRevision() });
    }

    function removeTrack(name, index) {
      playlistAction('playlist_remove', { name, index, revision: playlistRevision() });
    }

    delegateClicks('playlistsList', {
      toggle: (d) => togglePlaylist(d.name),
      play: (d) => playPlaylist(d.name, 0),
      'play-from': (d) => playPlaylist(d.name, Number(d.index)),
      append: (d) => appendPlaylist(d.name),
      rename: (d) => renamePlaylist(d.name),
      delete: (d) => deletePlaylist(d.name, Number(d.count)),
      'move-up': (d) => moveTrack(d.name, Number(d.index), 'up'),
      'move-down': (d) => moveTrack(d.name, Number(d.index), 'down'),
      remove: (d) => removeTrack(d.name, Number(d.index)),
    });

    document.getElementById('btnPlaylistCreate').addEventListener('click', () => {
      const name = prompt('New playlist name:');
      if (name === null) return;
      playlistAction('playlist_create', { name });
    });

    // The track the terminal plays, if any. Quickstream refuses a path outside the allowed
    // folders (the status then holds only its file name).
    document.getElementById('btnPlaylistAddCurrent').addEventListener('click', () => {
      const playing = lastStatus && (lastStatus.is_playing || lastStatus.is_paused) && lastStatus.filepath;
      if (!playing) {
        showToast('Nothing is playing');
        return;
      }
      openPlaylistPicker(lastStatus.title || lastStatus.filepath, lastStatus.filepath);
    });

    // --- ADD-TO-PLAYLIST PICKER ---
    // Lists the playlists with the last one added to highlighted, then "+ New playlist…".
    let pickerPath = null;

    function pickerOpen() {
      return document.getElementById('playlistPicker').classList.contains('show');
    }

    function openPlaylistPicker(label, path) {
      if (!path) return;
      pickerPath = path;
      document.getElementById('playlistPickerLabel').textContent = label || path;
      document.getElementById('playlistPicker').classList.add('show');
      renderPlaylistPicker(playlistsData);
      loadPlaylists();
    }

    function closePlaylistPicker() {
      document.getElementById('playlistPicker').classList.remove('show');
    }

    function renderPlaylistPicker(data) {
      const list = document.getElementById('playlistPickerList');
      clearChildren(list);
      if (!data || data.state !== 'ready') {
        list.appendChild(el('div', 'empty-sub', (data && data.message) || 'Loading playlists…'));
        return;
      }
      for (const playlist of data.playlists || []) {
        const row = el('button', playlist.name === data.last_playlist ? 'picker-row last' : 'picker-row',
          `${playlist.name} (${trackCountLabel(playlist.track_count)})`);
        row.dataset.action = 'pick';
        row.dataset.name = playlist.name || '';
        list.appendChild(row);
      }
      const newRow = el('button', 'picker-row new', '+ New playlist…');
      newRow.dataset.action = 'new';
      list.appendChild(newRow);
    }

    function addToPlaylist(name) {
      const path = pickerPath;
      closePlaylistPicker();
      playlistAction('playlist_add', { name, path });
    }

    async function newPlaylistAndAdd() {
      const path = pickerPath;
      closePlaylistPicker();
      const input = prompt('New playlist name:');
      if (input === null) return;
      const name = input.trim();
      if (await playlistAction('playlist_create', { name })) {
        playlistAction('playlist_add', { name, path });
      }
    }

    delegateClicks('playlistPickerList', {
      pick: (d) => addToPlaylist(d.name),
      new: () => newPlaylistAndAdd(),
    });
    document.getElementById('btnPlaylistPickerCancel').addEventListener('click', closePlaylistPicker);
    window.addEventListener('keydown', (e) => {
      if (e.key === 'Escape' && pickerOpen()) closePlaylistPicker();
    });

    // Keyboard shortcuts
    window.addEventListener('keydown', (e) => {
      if (e.target && (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA')) return;
      if (e.code === 'Space') {
        e.preventDefault();
        sendAction('toggle_pause');
      } else if (e.code === 'ArrowLeft') {
        e.preventDefault();
        sendAction('seek_relative', -10);
      } else if (e.code === 'ArrowRight') {
        e.preventDefault();
        sendAction('seek_relative', 10);
      }
    });

    // Smooth UI interpolation loop (100ms)
    setInterval(updateProgressUI, 100);

    // Polling loop (600ms)
    setInterval(pollStatus, 600);
    pollStatus();
  </script>
</body>
</html>
"##;
