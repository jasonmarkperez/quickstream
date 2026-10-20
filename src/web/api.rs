//! Route dispatch, the REST API (`/api/...`) and the dashboard page.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::metadata::{cover_image_mime, default_cover_svg, extract_cover_art};

use super::server::{read_request_body, read_request_head, reject_request, ConnectionPermit, Rejection, ServerLimits};
use super::state::{
    NowPlayingState, PlaylistEdit, PlaylistReply, WebBrowserEntry, WebBrowserResponse, WebCommand, WebPlaylistsState,
    WebReply,
};
use super::stream::{
    is_continuous_route, serve_audio_stream, serve_continuous_stream, ContinuousAdmission, StreamServerState,
};

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
/// `autopilot` / `autopilot_reshuffle` without a path before the terminal has published its
/// folder.
pub const NO_TERMINAL_FOLDER: &str = "The terminal hasn't opened a folder yet";

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
    allow: &mut dyn FnMut(PathBuf) -> Option<PathBuf>,
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
            let Some(canonical) = allow(PathBuf::from(&requested)) else {
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

pub(super) fn handle_client(
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
        let (roots, terminal_dir) = {
            let current = state.read().unwrap();
            (current.allowed_roots.clone(), current.terminal_dir.clone())
        };
        let mut rejected: Option<PathBuf> = None;
        // Canonicalized on first use, at most once per request.
        let mut allowed: Option<crate::web_access::AllowedRoots> = None;
        let mut allow = |p: PathBuf| -> Option<PathBuf> {
            let resolved = allowed
                .get_or_insert_with(|| crate::web_access::AllowedRoots::new(&roots))
                .resolve(&p);
            if resolved.is_none() {
                rejected = Some(p);
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
                    if let Some(p) = payload.path.map(PathBuf::from).and_then(&mut allow) {
                        let _ = cmd_tx.send(WebCommand::AddFile(p));
                    }
                }
                "add_folder" => {
                    if let Some(p) = payload.path.map(PathBuf::from).and_then(&mut allow) {
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
                        if let Some(dir) = allow(p.into()) {
                            let _ = cmd_tx.send(WebCommand::SetHome(Some(dir.to_string_lossy().to_string())));
                        }
                    }
                },
                // Without a path, the folder the terminal is browsing (as a path, so a non-UTF-8
                // name stays intact): resolved here, never on the UI thread (issue #102).
                "autopilot" | "autopilot_reshuffle" | "reshuffle" => {
                    match payload.path.map(PathBuf::from).or(terminal_dir) {
                        None => response = ActionResponse::refused("409 Conflict", NO_TERMINAL_FOLDER),
                        Some(dir) => {
                            if let Some(dir) = allow(dir) {
                                let _ = cmd_tx.send(if payload.action == "autopilot" {
                                    WebCommand::Autopilot(dir)
                                } else {
                                    WebCommand::AutopilotReshuffle(dir)
                                });
                            }
                        }
                    }
                }
                "shuffle" => {
                    let _ = cmd_tx.send(WebCommand::ShuffleQueue);
                }
                "set_mix_profile" | "mix_profile" => {
                    if let Some(profile_str) = payload.mode.or(payload.path) {
                        let _ = cmd_tx.send(WebCommand::SetMixProfile(profile_str));
                    }
                }
                "seed_radio" => {
                    if let Some(p) = payload.path.map(PathBuf::from).and_then(&mut allow) {
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

/// Embedded responsive HTML/CSS/JavaScript web dashboard.
const NOW_PLAYING_HTML: &str = include_str!("assets/index.html");
