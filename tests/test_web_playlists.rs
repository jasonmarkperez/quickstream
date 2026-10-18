//! Issue #50: playlists on the web dashboard.
//!
//! The UI thread publishes a snapshot of the playlists in the shared web state; `GET
//! /api/playlists` serves it without reading the playlists folder. Play and append go through
//! `/api/action` as `WebCommand::QueuePlaylist`, after the web thread checked every entry's path
//! against the allowed folders, and the UI thread queues them with the code tab 5 uses. Edits
//! go through `/api/action` as `WebCommand::EditPlaylist`; the UI thread applies them through
//! `PlaylistStore` and answers on the command's reply channel, which the web thread returns.
//!
//! Run with `--test-threads=1`: the tests share one playlists folder.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use quickstream::playlist::{Playlist, PlaylistEntry};
use quickstream::playlist_store::MoveDir;
use quickstream::ui::AppUI;
use quickstream::web::{
    NowPlayingState, PlaylistEdit, WebCommand, WebPlaylist, WebPlaylistEntry, WebPlaylists, WebPlaylistsState,
    WebReply, WebServer,
};

const PLAYLIST: &str = "Road Trip";

/// A fresh, canonical temp folder (macOS: /private/var/...), since queued paths are canonical.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_web_pl_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(&dir).unwrap()
}

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, b"not really audio").unwrap();
}

fn entry(path: &Path, artist: &str, title: &str, secs: i64) -> PlaylistEntry {
    PlaylistEntry {
        path: path.to_path_buf(),
        duration_secs: secs,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

/// Replace the playlists folder with these playlists.
fn write_playlists(playlists: &[(&str, Vec<PlaylistEntry>)]) {
    let dir = quickstream::paths::get_playlists_dir();
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for (name, entries) in playlists {
        let playlist = Playlist {
            name: name.to_string(),
            entries: entries.clone(),
        };
        fs::write(dir.join(format!("{}.m3u8", name)), playlist.to_m3u8()).unwrap();
    }
}

/// An app with the web server on (loopback, any port) whose allowed folder is `music`, and its
/// startup playlist load finished.
fn web_app(music: &Path) -> AppUI {
    let mut settings_mgr = quickstream::settings::SettingsManager::new();
    settings_mgr.settings.web_server_enabled = true;
    settings_mgr.settings.web_server_lan = false;
    settings_mgr.settings.web_server_port = 0;
    settings_mgr.settings.home_dir = Some(music.to_string_lossy().to_string());
    let mut app = AppUI::new(Some(music.to_path_buf()), settings_mgr);
    assert!(app.web_server.is_some(), "web server did not start");
    assert!(
        poll_until(&mut app, |app| app.playlists.store.is_loaded()),
        "playlists did not load"
    );
    app
}

/// Run the UI tick's playlist and web work until `cond` holds or 3 seconds pass.
fn poll_until(app: &mut AppUI, cond: impl Fn(&AppUI) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        app.handle_web_commands();
        app.poll_playlists();
        app.update_web_state();
        if cond(app) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn snapshot(app: &AppUI) -> Arc<WebPlaylists> {
    app.web_state.read().unwrap().playlists.clone()
}

/// Feed `cmd` to the app as if the web server had sent it.
fn send(app: &mut AppUI, cmd: WebCommand) {
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);
    tx.send(cmd).unwrap();
    app.handle_web_commands();
}

fn queue_paths(app: &AppUI) -> Vec<PathBuf> {
    app.queue.tracks().iter().map(|t| t.filepath.clone()).collect()
}

/// A player that records calls and never starts a real backend.
struct SilentPlayer {
    playing: bool,
}

impl quickstream::player::AudioPlayer for SilentPlayer {
    fn play(&mut self, _path: &Path) -> anyhow::Result<()> {
        self.playing = true;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.playing = false;
        Ok(())
    }
    fn seek(&mut self, _delta: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn seek_to(&mut self, _position: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_volume(&mut self, _volume: u8) -> anyhow::Result<()> {
        Ok(())
    }
    fn volume(&self) -> u8 {
        50
    }
    fn is_playing(&self) -> bool {
        self.playing
    }
    fn is_paused(&self) -> bool {
        false
    }
    fn position_seconds(&self) -> f64 {
        0.0
    }
    fn duration_seconds(&self) -> f64 {
        0.0
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP helpers (loopback, like the other web tests)
// ---------------------------------------------------------------------------------------------

fn raw_request(port: u16, req: &str) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let body = raw.get(header_end + 4..).map(|b| b.to_vec()).unwrap_or_default();
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, headers, body)
}

fn get(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    raw_request(
        port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, port
        ),
    )
}

fn post(port: u16, body: &serde_json::Value) -> (u16, serde_json::Value) {
    let body = body.to_string();
    let (status, _, bytes) = raw_request(
        port,
        &format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            port,
            body.len(),
            body
        ),
    );
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn start_server(
    state: Arc<RwLock<NowPlayingState>>,
) -> (quickstream::web::WebServerHandle, mpsc::Receiver<WebCommand>) {
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("start web server");
    (handle, rx)
}

fn web_entry(index: usize, path: &Path, title: &str) -> WebPlaylistEntry {
    WebPlaylistEntry {
        index,
        title: title.to_string(),
        artist: "Artist".to_string(),
        duration_seconds: 100,
        path: path.to_string_lossy().to_string(),
        outside: false,
        missing: None,
    }
}

/// Web state whose allowed folder is `music` and whose snapshot holds one ready playlist.
fn state_with(music: &Path, revision: u64, playlists: Vec<WebPlaylist>) -> Arc<RwLock<NowPlayingState>> {
    let state = NowPlayingState {
        allowed_roots: vec![music.to_path_buf()],
        playlists: Arc::new(WebPlaylists {
            state: WebPlaylistsState::Ready,
            message: None,
            revision,
            last_playlist: None,
            playlists,
        }),
        ..Default::default()
    };
    Arc::new(RwLock::new(state))
}

fn web_playlist(name: &str, entries: Vec<WebPlaylistEntry>) -> WebPlaylist {
    WebPlaylist {
        name: name.to_string(),
        track_count: entries.len(),
        total_duration_seconds: entries.iter().map(|e| e.duration_seconds.max(0)).sum(),
        entries,
    }
}

fn drain(rx: &mpsc::Receiver<WebCommand>) -> Vec<WebCommand> {
    std::thread::sleep(Duration::from_millis(50));
    rx.try_iter().collect()
}

// ---------------------------------------------------------------------------------------------
// Snapshot published by the UI thread
// ---------------------------------------------------------------------------------------------

#[test]
fn test_ui_publishes_the_playlists_snapshot() {
    common::isolate();
    let music = temp_dir("snapshot");
    let a = music.join("a.mp3");
    let b = music.join("b.flac");
    write_playlists(&[
        (PLAYLIST, vec![entry(&a, "Alpha", "One", 200), entry(&b, "", "Two", -1)]),
        ("ambient", vec![]),
    ]);
    let mut app = web_app(&music);
    app.settings_mgr.settings.last_playlist = Some(PLAYLIST.to_string());
    let version_before = app.web_state.read().unwrap().playlists_version;
    app.update_web_state();

    let snap = snapshot(&app);
    assert_eq!(snap.state, WebPlaylistsState::Ready);
    assert_eq!(snap.message, None);
    assert_eq!(snap.revision, app.playlists.store.revision());
    assert_eq!(snap.last_playlist.as_deref(), Some(PLAYLIST));
    // Sorted like tab 5: ignoring case.
    let names: Vec<&str> = snap.playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["ambient", PLAYLIST]);
    let road = &snap.playlists[1];
    assert_eq!(road.track_count, 2);
    assert_eq!(road.total_duration_seconds, 200, "unknown durations are left out");
    assert_eq!(
        road.entries[0],
        WebPlaylistEntry {
            index: 0,
            title: "One".to_string(),
            artist: "Alpha".to_string(),
            duration_seconds: 200,
            path: a.to_string_lossy().to_string(),
            outside: false,
            missing: None,
        }
    );
    assert_eq!(road.entries[1].duration_seconds, -1);
    assert_ne!(app.web_state.read().unwrap().playlists_version, version_before);

    // An edit in the TUI shows up on the next tick.
    let version = app.web_state.read().unwrap().playlists_version;
    app.playlists.store.create("Zed").unwrap();
    app.update_web_state();
    let snap = snapshot(&app);
    assert_eq!(snap.playlists.len(), 3);
    assert_eq!(snap.revision, app.playlists.store.revision());
    assert_ne!(app.web_state.read().unwrap().playlists_version, version);

    // Nothing changed: the snapshot is not rebuilt.
    let version = app.web_state.read().unwrap().playlists_version;
    app.update_web_state();
    assert_eq!(app.web_state.read().unwrap().playlists_version, version);
    assert!(app.persistence.flush());
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_snapshot_says_loading_until_the_store_loads() {
    common::isolate();
    let music = temp_dir("loading");
    write_playlists(&[]);
    let mut app = web_app(&music);
    app.playlists.store = quickstream::playlist_store::PlaylistStore::new(
        quickstream::paths::get_playlists_dir(),
        app.persistence.handle(),
    );
    app.update_web_state();
    let snap = snapshot(&app);
    assert_eq!(snap.state, WebPlaylistsState::Loading);
    assert_eq!(snap.message.as_deref(), Some("Playlists are still loading"));
    assert!(snap.playlists.is_empty());
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_snapshot_says_when_the_load_failed() {
    common::isolate();
    use std::os::unix::fs::PermissionsExt;
    let music = temp_dir("load_failed");
    write_playlists(&[(PLAYLIST, vec![])]);
    let dir = quickstream::paths::get_playlists_dir();
    let mut app = web_app(&music);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
    // Root lists any folder, so the test proves nothing there.
    let listable = fs::read_dir(&dir).is_ok();
    app.playlists.store = quickstream::playlist_store::PlaylistStore::new(dir.clone(), app.persistence.handle());
    app.start_playlist_load();
    let failed = poll_until(&mut app, |app| app.playlists.load_failed());
    let snap = snapshot(&app);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    if listable {
        return;
    }
    assert!(failed);
    assert_eq!(snap.state, WebPlaylistsState::Failed);
    assert_eq!(snap.message.as_deref(), Some("Couldn't load playlists"));
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_check_playlists_marks_missing_entries_from_a_worker() {
    common::isolate();
    let music = temp_dir("missing");
    let here = music.join("here.mp3");
    let ghost = music.join("ghost.mp3");
    touch(&here);
    write_playlists(&[(
        PLAYLIST,
        vec![entry(&here, "A", "Here", 100), entry(&ghost, "A", "Ghost", 100)],
    )]);
    let mut app = web_app(&music);
    app.update_web_state();
    // Nothing is checked until the dashboard asks for the playlists.
    assert_eq!(snapshot(&app).playlists[0].entries[1].missing, None);
    assert_eq!(app.playlists.checks_in_flight(), 0);

    send(&mut app, WebCommand::CheckPlaylists);
    assert!(
        poll_until(&mut app, |app| {
            let snap = snapshot(app);
            snap.playlists[0].entries.iter().all(|e| e.missing.is_some())
        }),
        "existence checks did not finish"
    );
    let snap = snapshot(&app);
    assert_eq!(snap.playlists[0].entries[0].missing, Some(false));
    assert_eq!(snap.playlists[0].entries[1].missing, Some(true));
    // The playlist itself is never changed by a check.
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 2);
    let _ = fs::remove_dir_all(&music);
}

// ---------------------------------------------------------------------------------------------
// GET /api/playlists
// ---------------------------------------------------------------------------------------------

#[test]
fn test_playlists_api_serves_the_snapshot_and_hides_outside_paths() {
    common::isolate();
    let dir = temp_dir("api");
    let music = dir.join("Music");
    let private = dir.join("Private");
    fs::create_dir_all(&music).unwrap();
    fs::create_dir_all(&private).unwrap();
    let inside = music.join("in.mp3");
    let outside = private.join("out.mp3");
    let state = state_with(
        &music,
        7,
        vec![web_playlist(
            PLAYLIST,
            vec![web_entry(0, &inside, "In"), web_entry(1, &outside, "Out")],
        )],
    );
    let (handle, rx) = start_server(state.clone());

    let (status, headers, body) = get(handle.port, "/api/playlists");
    assert_eq!(status, 200);
    assert!(headers.contains("application/json"), "{}", headers);
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(!text.contains("Private"), "{}", text);
    let got: WebPlaylists = serde_json::from_slice(&body).unwrap();
    assert_eq!(got.state, WebPlaylistsState::Ready);
    assert_eq!(got.revision, 7);
    let entries = &got.playlists[0].entries;
    assert_eq!(entries[0].path, inside.to_string_lossy());
    assert!(!entries[0].outside);
    assert_eq!(entries[1].path, "");
    assert!(entries[1].outside);
    assert_eq!(entries[1].title, "Out", "the label is still shown");
    // The shared snapshot keeps the real path (play needs it).
    assert_eq!(
        state.read().unwrap().playlists.playlists[0].entries[1].path,
        outside.to_string_lossy()
    );
    // Serving the playlists asks the UI to check their files.
    assert!(drain(&rx).contains(&WebCommand::CheckPlaylists));

    // HEAD answers without a body.
    let (status, _, body) = raw_request(
        handle.port,
        &format!(
            "HEAD /api/playlists HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            handle.port
        ),
    );
    assert_eq!(status, 200);
    assert!(body.is_empty());
    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_playlists_api_keeps_the_host_check() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (handle, rx) = start_server(state);
    let (status, _, body) = raw_request(
        handle.port,
        &format!(
            "GET /api/playlists HTTP/1.1\r\nHost: rebind.evil.example:{}\r\nConnection: close\r\n\r\n",
            handle.port
        ),
    );
    assert_eq!(status, 403);
    assert!(!String::from_utf8_lossy(&body).contains("playlists"));
    assert!(drain(&rx).is_empty(), "a refused request must not reach the UI");
    handle.stop();
}

#[test]
fn test_playlists_api_reports_loading_before_the_first_snapshot() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (handle, _rx) = start_server(state);
    let (status, _, body) = get(handle.port, "/api/playlists");
    assert_eq!(status, 200);
    let got: WebPlaylists = serde_json::from_slice(&body).unwrap();
    assert_eq!(got.state, WebPlaylistsState::Loading);
    assert_eq!(got.message.as_deref(), Some("Playlists are still loading"));
    handle.stop();
}

// ---------------------------------------------------------------------------------------------
// Play / append through /api/action
// ---------------------------------------------------------------------------------------------

#[test]
fn test_play_and_append_send_the_checked_entries_to_the_ui() {
    common::isolate();
    let music = temp_dir("play_cmd");
    let a = music.join("a.mp3");
    let b = music.join("b.mp3");
    let ghost = music.join("ghost.mp3");
    touch(&a);
    touch(&b);
    let entries = vec![
        web_entry(0, &a, "A"),
        web_entry(1, &ghost, "Ghost"),
        web_entry(2, &b, "B"),
    ];
    let state = state_with(&music, 3, vec![web_playlist(PLAYLIST, entries)]);
    let (handle, rx) = start_server(state);
    let rx = answer_edits(rx, Ok("Checking 3 tracks in Road Trip…".to_string()));
    let expected: Vec<PlaylistEntry> = [(&a, "A"), (&ghost, "Ghost"), (&b, "B")]
        .iter()
        .map(|(p, t)| entry(p, "Artist", t, 100))
        .collect();

    // Names match ignoring case, like everywhere else.
    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_play", "name": "road trip", "index": 2, "revision": 3}),
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(json["ok"], true);
    // The answer is the UI's: what it started.
    assert_eq!(json["message"], "Checking 3 tracks in Road Trip…");
    assert_eq!(
        drain(&rx),
        vec![WebCommand::QueuePlaylist {
            name: PLAYLIST.to_string(),
            entries: expected.clone(),
            play_from: Some(2),
            reply: WebReply::default(),
        }]
    );

    // Play without an index starts at the top; append queues without playing.
    post(
        handle.port,
        &serde_json::json!({"action": "playlist_play", "name": PLAYLIST}),
    );
    let (status, _) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_append", "name": PLAYLIST}),
    );
    assert_eq!(status, 200);
    assert_eq!(
        drain(&rx),
        vec![
            WebCommand::QueuePlaylist {
                name: PLAYLIST.to_string(),
                entries: expected.clone(),
                play_from: Some(0),
                reply: WebReply::default(),
            },
            WebCommand::QueuePlaylist {
                name: PLAYLIST.to_string(),
                entries: expected,
                play_from: None,
                reply: WebReply::default(),
            },
        ]
    );
    handle.stop();
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_play_with_an_entry_outside_the_allowed_folders_is_rejected() {
    common::isolate();
    let dir = temp_dir("play_outside");
    let music = dir.join("Music");
    let private = dir.join("Private");
    let inside = music.join("in.mp3");
    let outside = private.join("out.mp3");
    touch(&inside);
    touch(&outside);
    // A symlink inside the music folder that leads out of it counts as outside.
    let link = music.join("link.mp3");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    // A missing file can't be followed: outside by its path alone.
    let ghost_outside = private.join("ghost.mp3");

    for (bad, playlist) in [
        (
            &outside,
            vec![web_entry(0, &inside, "In"), web_entry(1, &outside, "Out")],
        ),
        (&link, vec![web_entry(0, &link, "Link"), web_entry(1, &inside, "In")]),
        (&ghost_outside, vec![web_entry(0, &ghost_outside, "Ghost")]),
    ] {
        let state = state_with(&music, 1, vec![web_playlist(PLAYLIST, playlist)]);
        let (handle, rx) = start_server(state);
        for action in ["playlist_play", "playlist_append"] {
            let (status, json) = post(handle.port, &serde_json::json!({"action": action, "name": PLAYLIST}));
            assert_eq!(status, 403, "{} {}: {}", action, bad.display(), json);
            assert_eq!(json["error"], "path is outside the allowed folders");
            assert_eq!(
                drain(&rx),
                vec![WebCommand::PathRejected(bad.to_path_buf())],
                "{}",
                action
            );
        }
        handle.stop();
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_play_refusals_name_the_reason() {
    common::isolate();
    let music = temp_dir("play_refusals");
    let a = music.join("a.mp3");
    touch(&a);
    let ready = state_with(
        &music,
        4,
        vec![
            web_playlist(PLAYLIST, vec![web_entry(0, &a, "A")]),
            web_playlist("Empty", vec![]),
        ],
    );
    let (handle, rx) = start_server(ready);
    let cases = [
        (
            serde_json::json!({"action": "playlist_play", "name": "Nope"}),
            404,
            "No playlist named \"Nope\"",
        ),
        (
            serde_json::json!({"action": "playlist_play", "name": "Empty"}),
            409,
            "Empty is empty",
        ),
        (
            serde_json::json!({"action": "playlist_append", "name": "Empty"}),
            409,
            "Empty is empty",
        ),
        (
            serde_json::json!({"action": "playlist_play", "name": PLAYLIST, "index": 1, "revision": 4}),
            400,
            "No such track",
        ),
        // PR #65 review: an index names a track only together with the revision it came from.
        (
            serde_json::json!({"action": "playlist_play", "name": PLAYLIST, "index": 0}),
            400,
            "Missing revision",
        ),
        (
            serde_json::json!({"action": "playlist_play", "name": PLAYLIST, "revision": 3}),
            409,
            "Playlists changed; try again",
        ),
        (
            serde_json::json!({"action": "playlist_play"}),
            400,
            "Missing playlist name",
        ),
    ];
    for (body, want_status, want_error) in cases {
        let (status, json) = post(handle.port, &body);
        assert_eq!(status, want_status, "{}: {}", body, json);
        assert_eq!(json["ok"], false, "{}", body);
        assert!(
            json["error"].as_str().unwrap_or("").starts_with(want_error),
            "{}: {}",
            body,
            json
        );
    }
    assert!(drain(&rx).is_empty(), "a refused play must not reach the UI");
    handle.stop();

    for (state, message) in [
        (WebPlaylistsState::Loading, "Playlists are still loading"),
        (WebPlaylistsState::Failed, "Couldn't load playlists"),
    ] {
        let not_ready = Arc::new(RwLock::new(NowPlayingState {
            allowed_roots: vec![music.clone()],
            playlists: Arc::new(WebPlaylists {
                state,
                message: Some(message.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }));
        let (handle, rx) = start_server(not_ready);
        let (status, json) = post(
            handle.port,
            &serde_json::json!({"action": "playlist_play", "name": PLAYLIST}),
        );
        assert_eq!(status, 409);
        assert_eq!(json["error"], message);
        assert!(drain(&rx).is_empty());
        handle.stop();
    }
    let _ = fs::remove_dir_all(&music);
}

// ---------------------------------------------------------------------------------------------
// The UI applies QueuePlaylist with tab 5's code
// ---------------------------------------------------------------------------------------------

#[test]
fn test_web_play_replaces_the_queue_and_plays_from_the_track() {
    common::isolate();
    let music = temp_dir("ui_play");
    let a = music.join("a.mp3");
    let ghost = music.join("ghost.mp3");
    let c = music.join("c.mp3");
    touch(&a);
    touch(&c);
    let entries = vec![
        entry(&a, "A", "One", 100),
        entry(&ghost, "A", "Ghost", 100),
        entry(&c, "A", "Three", 100),
    ];
    write_playlists(&[(PLAYLIST, entries.clone())]);
    let mut app = web_app(&music);
    app.player = Box::new(SilentPlayer { playing: false });
    let old = music.join("old.mp3");
    app.queue.add_track(quickstream::metadata::AudioMetadata {
        filepath: old.clone(),
        title: "Old".to_string(),
        ..Default::default()
    });
    app.autopilot_dir = Some(music.clone());

    // Index 1 is missing: playback starts on the next existing track, as in tab 5.
    send(
        &mut app,
        WebCommand::QueuePlaylist {
            name: PLAYLIST.to_string(),
            entries,
            play_from: Some(1),
            reply: WebReply::default(),
        },
    );
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("Queued")),
        "{}",
        app.status_message
    );
    assert_eq!(queue_paths(&app), vec![a.clone(), c.clone()]);
    assert_eq!(app.queue.current_index(), 1);
    assert!(app.player.is_playing());
    assert_eq!(app.autopilot_dir, None, "replace ends the autopilot session");
    // The status names the playlist, as tab 5 does.
    assert_eq!(app.status_message, "Queued 2 tracks from Road Trip, 1 missing");
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_append_keeps_the_queue() {
    common::isolate();
    let music = temp_dir("ui_append");
    let a = music.join("a.mp3");
    touch(&a);
    let entries = vec![entry(&a, "A", "One", 100)];
    write_playlists(&[(PLAYLIST, entries.clone())]);
    let mut app = web_app(&music);
    app.player = Box::new(SilentPlayer { playing: false });
    let old = music.join("old.mp3");
    app.queue.add_track(quickstream::metadata::AudioMetadata {
        filepath: old.clone(),
        ..Default::default()
    });

    send(
        &mut app,
        WebCommand::QueuePlaylist {
            name: PLAYLIST.to_string(),
            entries,
            play_from: None,
            reply: WebReply::default(),
        },
    );
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(queue_paths(&app), vec![old, a]);
    assert!(!app.player.is_playing());
    assert_eq!(app.status_message, "Queued 1 track from Road Trip");
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_play_never_queues_a_file_that_resolves_outside_the_allowed_folders() {
    common::isolate();
    // The web thread checked the paths, but a symlink may change before the worker checks the
    // file. The worker only queues files that still resolve inside the allowed folders.
    let dir = temp_dir("ui_symlink");
    let music = dir.join("Music");
    let private = dir.join("Private");
    let inside = music.join("in.mp3");
    let secret = private.join("secret.mp3");
    touch(&inside);
    touch(&secret);
    let link = music.join("link.mp3");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let entries = vec![entry(&link, "A", "Link", 100), entry(&inside, "A", "In", 100)];
    write_playlists(&[(PLAYLIST, entries.clone())]);
    let mut app = web_app(&music);
    app.player = Box::new(SilentPlayer { playing: false });

    send(
        &mut app,
        WebCommand::QueuePlaylist {
            name: PLAYLIST.to_string(),
            entries,
            play_from: None,
            reply: WebReply::default(),
        },
    );
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(queue_paths(&app), vec![inside]);
    assert_eq!(app.status_message, "Queued 1 track from Road Trip, 1 missing");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_web_play_is_refused_until_the_playlists_load() {
    common::isolate();
    let music = temp_dir("ui_not_loaded");
    let a = music.join("a.mp3");
    touch(&a);
    write_playlists(&[]);
    let mut app = web_app(&music);
    app.playlists.store = quickstream::playlist_store::PlaylistStore::new(
        quickstream::paths::get_playlists_dir(),
        app.persistence.handle(),
    );
    send(
        &mut app,
        WebCommand::QueuePlaylist {
            name: PLAYLIST.to_string(),
            entries: vec![entry(&a, "A", "One", 100)],
            play_from: Some(0),
            reply: WebReply::default(),
        },
    );
    assert_eq!(app.status_message, "Playlists are still loading");
    assert!(!app.playlists.queue_check_pending());
    assert!(app.queue.is_empty());
    let _ = fs::remove_dir_all(&music);
}

/// Send `body` to the app's own server from another thread while the UI handles web commands,
/// as the event loop does, and return the response.
fn request_while_ticking(app: &mut AppUI, port: u16, body: serde_json::Value) -> (u16, serde_json::Value) {
    let worker = std::thread::spawn(move || post(port, &body));
    while !worker.is_finished() {
        app.handle_web_commands();
        std::thread::sleep(Duration::from_millis(5));
    }
    worker.join().unwrap()
}

/// End to end: the dashboard's request reaches the app's own server, and the next UI ticks
/// play the playlist.
#[test]
fn test_dashboard_play_request_plays_the_playlist() {
    common::isolate();
    let music = temp_dir("e2e_play");
    let a = music.join("a.mp3");
    let b = music.join("b.mp3");
    touch(&a);
    touch(&b);
    write_playlists(&[(PLAYLIST, vec![entry(&a, "A", "One", 100), entry(&b, "B", "Two", 100)])]);
    let mut app = web_app(&music);
    app.player = Box::new(SilentPlayer { playing: false });
    app.update_web_state();
    let port = app.web_server.as_ref().unwrap().port;
    let revision = snapshot(&app).revision;

    // The request waits for the UI's answer, so the UI must keep ticking meanwhile.
    let (status, json) = request_while_ticking(
        &mut app,
        port,
        serde_json::json!({"action": "playlist_play", "name": PLAYLIST, "index": 1, "revision": revision}),
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(json["message"], "Checking 2 tracks in Road Trip…");
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(queue_paths(&app), vec![a, b]);
    assert_eq!(app.queue.current_index(), 1);
    let _ = fs::remove_dir_all(&music);
}

// ---------------------------------------------------------------------------------------------
// Edits: the UI applies EditPlaylist through PlaylistStore and answers on the reply channel
// ---------------------------------------------------------------------------------------------

/// Apply `edit` as the web server would send it and return the UI's answer.
fn edit(app: &mut AppUI, edit: PlaylistEdit) -> Result<String, String> {
    let (reply, rx) = WebReply::channel();
    send(app, WebCommand::EditPlaylist(edit, reply));
    rx.recv_timeout(Duration::from_secs(1))
        .expect("the UI answers every edit")
}

fn playlist_file(name: &str) -> PathBuf {
    quickstream::paths::get_playlists_dir().join(format!("{}.m3u8", name))
}

fn names(app: &AppUI) -> Vec<String> {
    app.playlists.store.playlists().iter().map(|p| p.name.clone()).collect()
}

fn titles(app: &AppUI, playlist: &str) -> Vec<String> {
    app.playlists
        .store
        .get(playlist)
        .unwrap()
        .entries
        .iter()
        .map(|e| e.title.clone())
        .collect()
}

#[test]
fn test_web_create_rename_and_delete_go_through_the_store() {
    common::isolate();
    let music = temp_dir("edit_crd");
    write_playlists(&[(PLAYLIST, vec![])]);
    let mut app = web_app(&music);
    app.settings_mgr.settings.last_playlist = Some(PLAYLIST.to_string());

    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Create {
                name: "  Mix ".to_string()
            }
        ),
        Ok("Created \"Mix\"".to_string())
    );
    assert_eq!(
        app.status_message, "Created \"Mix\"",
        "the terminal shows the same status"
    );
    assert!(app.persistence.flush());
    assert!(playlist_file("Mix").is_file());

    // Rename names the playlist ignoring case, and last_playlist follows it.
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Rename {
                name: "road trip".to_string(),
                new_name: "Summer".to_string(),
            }
        ),
        Ok("Renamed \"Road Trip\" to \"Summer\"".to_string())
    );
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Summer"));
    assert!(app.persistence.flush());
    assert!(playlist_file("Summer").is_file());
    assert!(!playlist_file(PLAYLIST).exists());

    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Delete {
                name: "summer".to_string()
            }
        ),
        Ok("Deleted \"Summer\"".to_string())
    );
    assert_eq!(app.settings_mgr.settings.last_playlist, None);
    assert!(app.persistence.flush());
    assert!(!playlist_file("Summer").exists());
    assert_eq!(names(&app), vec!["Mix".to_string()]);
    let _ = fs::remove_dir_all(&music);
}

/// PR #65 review: two quick dashboard calls can't race the writer. While a rename that will
/// fail is still being saved, a rename or create onto the name it started from is refused with
/// the terminal's message, so no two playlists ever claim one file.
#[test]
fn test_web_create_and_rename_onto_a_name_still_saving_are_refused() {
    common::isolate();
    let music = temp_dir("edit_pending");
    write_playlists(&[(PLAYLIST, vec![]), ("Mix", vec![])]);
    let mut app = web_app(&music);
    assert!(app.persistence.flush());
    // A file the store never read is in the way of the first rename.
    fs::write(playlist_file("Summer"), "theirs").unwrap();

    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Rename {
                name: PLAYLIST.to_string(),
                new_name: "Summer".to_string(),
            }
        ),
        Ok("Renamed \"Road Trip\" to \"Summer\"".to_string())
    );
    let busy = "Still renaming \"Road Trip\" to \"Summer\", try again".to_string();
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Rename {
                name: "Mix".to_string(),
                new_name: PLAYLIST.to_string(),
            }
        ),
        Err(busy.clone())
    );
    assert_eq!(app.status_message, busy);
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Create {
                name: PLAYLIST.to_string()
            }
        ),
        Err(busy)
    );

    assert!(app.persistence.flush());
    assert!(poll_until(&mut app, |app| app
        .status_message
        .starts_with("Couldn't rename")));
    assert_eq!(names(&app), vec!["Mix".to_string(), PLAYLIST.to_string()]);
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Delete {
                name: PLAYLIST.to_string()
            }
        ),
        Ok("Deleted \"Road Trip\"".to_string())
    );
    assert!(app.persistence.flush());
    assert!(!playlist_file(PLAYLIST).exists());
    assert!(playlist_file("Mix").is_file());
    assert_eq!(fs::read_to_string(playlist_file("Summer")).unwrap(), "theirs");
    let _ = fs::remove_dir_all(&music);
}

/// PR #65 review (after the race fix): three quick dashboard renames of one playlist, the first
/// of which fails. Renaming back and then on used to be accepted, and the undo then called the
/// playlist by its old name while its file had moved on, so a delete left that file behind. The
/// second and third renames are refused until the first is settled.
#[test]
fn test_web_rename_of_a_playlist_whose_rename_is_still_saving_is_refused() {
    common::isolate();
    let music = temp_dir("edit_pending_again");
    write_playlists(&[(PLAYLIST, vec![]), ("Mix", vec![])]);
    let mut app = web_app(&music);
    assert!(app.persistence.flush());
    fs::write(playlist_file("Summer"), "theirs").unwrap();
    let rename = |name: &str, new_name: &str| PlaylistEdit::Rename {
        name: name.to_string(),
        new_name: new_name.to_string(),
    };

    assert_eq!(
        edit(&mut app, rename(PLAYLIST, "Summer")),
        Ok("Renamed \"Road Trip\" to \"Summer\"".to_string())
    );
    let busy = "Still renaming \"Road Trip\" to \"Summer\", try again".to_string();
    assert_eq!(edit(&mut app, rename("Summer", PLAYLIST)), Err(busy.clone()));
    assert_eq!(app.status_message, busy);
    assert_eq!(edit(&mut app, rename("Summer", "Winter")), Err(busy));

    assert!(app.persistence.flush());
    assert!(poll_until(&mut app, |app| app
        .status_message
        .starts_with("Couldn't rename")));
    assert_eq!(names(&app), vec!["Mix".to_string(), PLAYLIST.to_string()]);
    assert_eq!(
        edit(&mut app, rename(PLAYLIST, "Winter")),
        Ok("Renamed \"Road Trip\" to \"Winter\"".to_string())
    );
    assert!(app.persistence.flush());
    app.poll_playlists();
    assert_eq!(names(&app), vec!["Mix".to_string(), "Winter".to_string()]);
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Delete {
                name: "Winter".to_string()
            }
        ),
        Ok("Deleted \"Winter\"".to_string())
    );
    assert!(app.persistence.flush());
    let mut files: Vec<String> = fs::read_dir(quickstream::paths::get_playlists_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    files.sort();
    assert_eq!(files, vec!["Mix.m3u8".to_string(), "Summer.m3u8".to_string()]);
    assert_eq!(fs::read_to_string(playlist_file("Summer")).unwrap(), "theirs");
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_names_get_the_same_checks_and_messages_as_the_terminal() {
    common::isolate();
    let music = temp_dir("edit_names");
    write_playlists(&[(PLAYLIST, vec![]), ("Mix", vec![])]);
    let mut app = web_app(&music);
    app.playlists.store.reserve_names(vec!["Latin".to_string()]);
    let create = |name: &str| PlaylistEdit::Create { name: name.to_string() };
    let rename = |new_name: &str| PlaylistEdit::Rename {
        name: "Mix".to_string(),
        new_name: new_name.to_string(),
    };
    let cases = [
        (create("a/b"), "Name can't contain /"),
        (create("a\\b"), "Name can't contain \\"),
        (create("   "), "Name can't be empty"),
        (create(".hidden"), "Name can't start with ."),
        (create("ROAD TRIP"), "\"Road Trip\" already exists"),
        (
            create("latin"),
            "A playlist file named \"Latin\" exists but couldn't be loaded",
        ),
        (rename("road trip"), "\"Road Trip\" already exists"),
        (rename("x/y"), "Name can't contain /"),
        (
            rename("Latin"),
            "A playlist file named \"Latin\" exists but couldn't be loaded",
        ),
        (
            PlaylistEdit::Rename {
                name: "Nope".to_string(),
                new_name: "Other".to_string(),
            },
            "No playlist named \"Nope\"",
        ),
        (
            PlaylistEdit::Delete {
                name: "Nope".to_string(),
            },
            "No playlist named \"Nope\"",
        ),
    ];
    let revision = app.playlists.store.revision();
    for (request, message) in cases {
        let shown = format!("{:?}", request);
        assert_eq!(edit(&mut app, request), Err(message.to_string()), "{}", shown);
        assert_eq!(app.status_message, message);
    }
    assert_eq!(app.playlists.store.revision(), revision, "nothing changed");
    assert_eq!(names(&app), vec!["Mix".to_string(), PLAYLIST.to_string()]);
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_add_remove_and_move_tracks() {
    common::isolate();
    let music = temp_dir("edit_tracks");
    let a = music.join("a.mp3");
    let b = music.join("b.mp3");
    let c = music.join("c.flac");
    touch(&c);
    write_playlists(&[(PLAYLIST, vec![entry(&a, "A", "One", 100), entry(&b, "B", "Two", 100)])]);
    let mut app = web_app(&music);
    // The now-playing track's tags come from the queue.
    app.queue.add_track(quickstream::metadata::AudioMetadata {
        filepath: c.clone(),
        title: "Three".to_string(),
        artist: "C".to_string(),
        duration_seconds: 61.0,
        ..Default::default()
    });

    let add = PlaylistEdit::Add {
        name: PLAYLIST.to_string(),
        path: c.clone(),
        canonical: c.clone(),
    };
    assert_eq!(
        edit(&mut app, add.clone()),
        Ok("Added \"Three\" to Road Trip (3 tracks)".to_string())
    );
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some(PLAYLIST));
    let added = &app.playlists.store.get(PLAYLIST).unwrap().entries[2];
    assert_eq!((added.artist.as_str(), added.duration_secs), ("C", 61));
    assert_eq!(edit(&mut app, add), Ok("Already in Road Trip".to_string()));

    let revision = app.playlists.store.revision();
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Move {
                name: PLAYLIST.to_string(),
                index: 2,
                dir: MoveDir::Up,
                revision,
            }
        ),
        Ok("Moved \"C - Three\" to 2".to_string())
    );
    assert_eq!(titles(&app, PLAYLIST), vec!["One", "Three", "Two"]);

    // An index from before that move is refused: the list changed.
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Remove {
                name: PLAYLIST.to_string(),
                index: 2,
                revision,
            }
        ),
        Err("Playlists changed; try again".to_string())
    );
    assert_eq!(titles(&app, PLAYLIST), vec!["One", "Three", "Two"]);

    let revision = app.playlists.store.revision();
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Remove {
                name: PLAYLIST.to_string(),
                index: 0,
                revision,
            }
        ),
        Ok("Removed \"A - One\" from Road Trip".to_string())
    );
    assert_eq!(titles(&app, PLAYLIST), vec!["Three", "Two"]);
    let revision = app.playlists.store.revision();
    assert_eq!(
        edit(
            &mut app,
            PlaylistEdit::Remove {
                name: PLAYLIST.to_string(),
                index: 5,
                revision,
            }
        ),
        Err("No such track".to_string())
    );

    // Every edit reached the file through the writer.
    assert!(app.persistence.flush());
    let text = fs::read_to_string(playlist_file(PLAYLIST)).unwrap();
    let on_disk = quickstream::playlist::parse_m3u8(PLAYLIST, &text, Path::new("/"));
    assert_eq!(
        on_disk.entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
        vec!["Three", "Two"]
    );
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_edits_are_refused_until_the_playlists_load() {
    common::isolate();
    let music = temp_dir("edit_not_loaded");
    write_playlists(&[(PLAYLIST, vec![])]);
    let mut app = web_app(&music);
    app.playlists.store = quickstream::playlist_store::PlaylistStore::new(
        quickstream::paths::get_playlists_dir(),
        app.persistence.handle(),
    );
    for request in [
        PlaylistEdit::Create {
            name: "Mix".to_string(),
        },
        PlaylistEdit::Delete {
            name: PLAYLIST.to_string(),
        },
        PlaylistEdit::Add {
            name: PLAYLIST.to_string(),
            path: music.join("a.mp3"),
            canonical: music.join("a.mp3"),
        },
    ] {
        assert_eq!(edit(&mut app, request), Err("Playlists are still loading".to_string()));
    }
    assert!(app.persistence.flush());
    assert!(!playlist_file("Mix").exists());
    assert!(playlist_file(PLAYLIST).exists());
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_edits_are_refused_after_a_failed_load() {
    common::isolate();
    use std::os::unix::fs::PermissionsExt;
    let music = temp_dir("edit_load_failed");
    write_playlists(&[(PLAYLIST, vec![])]);
    let dir = quickstream::paths::get_playlists_dir();
    let mut app = web_app(&music);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
    let listable = fs::read_dir(&dir).is_ok();
    app.playlists.store = quickstream::playlist_store::PlaylistStore::new(dir.clone(), app.persistence.handle());
    app.start_playlist_load();
    let failed = poll_until(&mut app, |app| app.playlists.load_failed());
    let answer = edit(
        &mut app,
        PlaylistEdit::Create {
            name: "Mix".to_string(),
        },
    );
    let flushed = app.persistence.flush();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    if listable {
        return;
    }
    assert!(failed && flushed);
    assert_eq!(answer, Err("Couldn't load playlists".to_string()));
    assert!(!playlist_file("Mix").exists());
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_web_edits_keep_the_terminal_on_its_playlist() {
    common::isolate();
    let music = temp_dir("edit_cursor");
    write_playlists(&[("Middle", vec![]), ("Zed", vec![])]);
    let mut app = web_app(&music);
    app.playlists.list_cursor = app.playlists.store.index_of("Zed").unwrap();

    edit(
        &mut app,
        PlaylistEdit::Create {
            name: "Alpha".to_string(),
        },
    )
    .unwrap();
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Zed"));
    edit(
        &mut app,
        PlaylistEdit::Rename {
            name: "Zed".to_string(),
            new_name: "Beta".to_string(),
        },
    )
    .unwrap();
    assert_eq!(
        app.playlists.selected_name().as_deref(),
        Some("Beta"),
        "follows its rename"
    );
    edit(
        &mut app,
        PlaylistEdit::Delete {
            name: "Alpha".to_string(),
        },
    )
    .unwrap();
    assert_eq!(app.playlists.selected_name().as_deref(), Some("Beta"));
    assert!(app.persistence.flush());
    let _ = fs::remove_dir_all(&music);
}

// ---------------------------------------------------------------------------------------------
// Edits over HTTP: the web thread waits for the UI's answer
// ---------------------------------------------------------------------------------------------

/// Answer every edit and every play / append the server sends with `answer`, and hand the
/// commands back on the returned channel. Other commands are passed on too.
fn answer_edits(rx: mpsc::Receiver<WebCommand>, answer: Result<String, String>) -> mpsc::Receiver<WebCommand> {
    let (out_tx, out_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for cmd in rx {
            match cmd {
                WebCommand::EditPlaylist(_, ref reply) | WebCommand::QueuePlaylist { ref reply, .. } => {
                    reply.send(answer.clone())
                }
                _ => {}
            }
            if out_tx.send(cmd).is_err() {
                break;
            }
        }
    });
    out_rx
}

fn edits(rx: &mpsc::Receiver<WebCommand>) -> Vec<PlaylistEdit> {
    drain(rx)
        .into_iter()
        .filter_map(|cmd| match cmd {
            WebCommand::EditPlaylist(edit, _) => Some(edit),
            _ => None,
        })
        .collect()
}

#[test]
fn test_edit_requests_reach_the_ui_and_return_its_answer() {
    common::isolate();
    let music = temp_dir("http_edits");
    let state = state_with(&music, 5, vec![web_playlist(PLAYLIST, vec![])]);
    let (handle, rx) = start_server(state);
    let rx = answer_edits(rx, Ok("Created \"Mix\"".to_string()));
    let requests = [
        (
            serde_json::json!({"action": "playlist_create", "name": "Mix"}),
            PlaylistEdit::Create {
                name: "Mix".to_string(),
            },
        ),
        (
            serde_json::json!({"action": "playlist_rename", "name": PLAYLIST, "new_name": "Summer"}),
            PlaylistEdit::Rename {
                name: PLAYLIST.to_string(),
                new_name: "Summer".to_string(),
            },
        ),
        (
            serde_json::json!({"action": "playlist_delete", "name": PLAYLIST}),
            PlaylistEdit::Delete {
                name: PLAYLIST.to_string(),
            },
        ),
        (
            serde_json::json!({"action": "playlist_remove", "name": PLAYLIST, "index": 3, "revision": 5}),
            PlaylistEdit::Remove {
                name: PLAYLIST.to_string(),
                index: 3,
                revision: 5,
            },
        ),
        (
            serde_json::json!({"action": "playlist_move", "name": PLAYLIST, "index": 1, "direction": "down", "revision": 5}),
            PlaylistEdit::Move {
                name: PLAYLIST.to_string(),
                index: 1,
                dir: MoveDir::Down,
                revision: 5,
            },
        ),
    ];
    for (body, expected) in requests {
        let (status, json) = post(handle.port, &body);
        assert_eq!(status, 200, "{}: {}", body, json);
        assert_eq!(json, serde_json::json!({"ok": true, "message": "Created \"Mix\""}));
        assert_eq!(edits(&rx), vec![expected], "{}", body);
    }
    handle.stop();

    // A refusal comes back as 409 with the UI's message.
    let state = state_with(&music, 5, vec![]);
    let (handle, rx) = start_server(state);
    let _rx = answer_edits(rx, Err("Name can't contain /".to_string()));
    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_create", "name": "a/b"}),
    );
    assert_eq!(status, 409);
    assert_eq!(json, serde_json::json!({"ok": false, "error": "Name can't contain /"}));
    handle.stop();
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_malformed_edit_requests_are_refused_before_the_ui() {
    common::isolate();
    let music = temp_dir("http_malformed");
    let state = state_with(&music, 5, vec![]);
    let (handle, rx) = start_server(state);
    let rx = answer_edits(rx, Ok("unexpected".to_string()));
    for body in [
        serde_json::json!({"action": "playlist_create"}),
        serde_json::json!({"action": "playlist_rename", "name": PLAYLIST}),
        serde_json::json!({"action": "playlist_delete"}),
        serde_json::json!({"action": "playlist_add", "name": PLAYLIST}),
        serde_json::json!({"action": "playlist_remove", "name": PLAYLIST}),
        serde_json::json!({"action": "playlist_move", "name": PLAYLIST, "index": 0}),
        serde_json::json!({"action": "playlist_move", "name": PLAYLIST, "index": 0, "direction": "sideways", "revision": 5}),
        // PR #65 review: index-based edits need the revision their index came from.
        serde_json::json!({"action": "playlist_remove", "name": PLAYLIST, "index": 0}),
        serde_json::json!({"action": "playlist_move", "name": PLAYLIST, "index": 0, "direction": "up"}),
    ] {
        let (status, json) = post(handle.port, &body);
        assert_eq!(status, 400, "{}: {}", body, json);
        assert_eq!(json["ok"], false, "{}", body);
    }
    assert!(edits(&rx).is_empty());
    handle.stop();
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_adding_a_file_checks_its_path_like_add_file() {
    common::isolate();
    let dir = temp_dir("http_add");
    let music = dir.join("Music");
    let private = dir.join("Private");
    let inside = music.join("in.mp3");
    let secret = private.join("secret.mp3");
    let notes = music.join("notes.txt");
    touch(&inside);
    touch(&secret);
    touch(&notes);
    let link = music.join("link.mp3");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let state = state_with(&music, 1, vec![web_playlist(PLAYLIST, vec![])]);
    let (handle, rx) = start_server(state);
    let rx = answer_edits(rx, Ok("Added".to_string()));

    for bad in [&secret, &link, &private.join("ghost.mp3")] {
        let (status, json) = post(
            handle.port,
            &serde_json::json!({"action": "playlist_add", "name": PLAYLIST, "path": bad}),
        );
        assert_eq!(status, 403, "{}: {}", bad.display(), json);
        assert_eq!(drain(&rx), vec![WebCommand::PathRejected(bad.to_path_buf())]);
    }
    // A relative path (the dashboard shows only a file name for tracks outside the folders).
    let (status, _) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_add", "name": PLAYLIST, "path": "secret.mp3"}),
    );
    assert_eq!(status, 403);
    assert_eq!(drain(&rx), vec![WebCommand::PathRejected(PathBuf::from("secret.mp3"))]);

    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_add", "name": PLAYLIST, "path": notes}),
    );
    assert_eq!(status, 400);
    assert_eq!(json["error"], "Not an audio file");
    assert!(drain(&rx).is_empty());

    // Inside: the UI gets the path as the request named it, normalized as a terminal add
    // would store it, and the checked (canonical) path.
    fs::create_dir_all(music.join("sub")).unwrap();
    let spelled = music.join("sub/../in.mp3");
    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_add", "name": PLAYLIST, "path": spelled}),
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(
        edits(&rx),
        vec![PlaylistEdit::Add {
            name: PLAYLIST.to_string(),
            path: inside.clone(),
            canonical: inside.clone(),
        }]
    );
    // PR #65 review: through a symlinked folder the stored path keeps the folder the request
    // named, like a terminal add; the canonical path is only for the check.
    let real = music.join("Real");
    touch(&real.join("song.mp3"));
    std::os::unix::fs::symlink(&real, music.join("Linked")).unwrap();
    let linked = music.join("Linked").join("song.mp3");
    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_add", "name": PLAYLIST, "path": linked}),
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(
        edits(&rx),
        vec![PlaylistEdit::Add {
            name: PLAYLIST.to_string(),
            path: linked.clone(),
            canonical: real.join("song.mp3"),
        }]
    );
    // `..` after a symlinked folder: the OS resolves it from the link's target, text-only
    // normalization from the link's parent. Here the text form names Private/secret.mp3
    // (outside), the checked file is Music/Private/secret.mp3 (inside). The stored path must
    // be the file that was checked, never the text form.
    fs::create_dir_all(music.join("a/b")).unwrap();
    std::os::unix::fs::symlink(music.join("a/b"), music.join("Deep")).unwrap();
    let checked = music.join("Private").join("secret.mp3");
    touch(&checked);
    let tricky = music.join("Deep/../../Private/secret.mp3");
    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_add", "name": PLAYLIST, "path": tricky}),
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(
        edits(&rx),
        vec![PlaylistEdit::Add {
            name: PLAYLIST.to_string(),
            path: checked.clone(),
            canonical: checked.clone(),
        }]
    );
    handle.stop();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_edit_without_an_answer_reports_it() {
    common::isolate();
    let music = temp_dir("http_no_answer");
    let state = state_with(&music, 1, vec![]);
    let (handle, rx) = start_server(state);
    // The UI went away: the command (and its reply channel) is dropped unanswered.
    drop(rx);
    let (status, json) = post(
        handle.port,
        &serde_json::json!({"action": "playlist_create", "name": "Mix"}),
    );
    assert_eq!(status, 503, "{}", json);
    assert_eq!(json["ok"], false);
    handle.stop();
    let _ = fs::remove_dir_all(&music);
}

/// End to end: a dashboard edit waits for the app's next tick and returns its answer.
#[test]
fn test_dashboard_edit_requests_change_the_store() {
    common::isolate();
    let music = temp_dir("e2e_edit");
    let a = music.join("a.mp3");
    touch(&a);
    write_playlists(&[(PLAYLIST, vec![])]);
    let mut app = web_app(&music);
    let port = app.web_server.as_ref().unwrap().port;

    let request = |body: serde_json::Value, app: &mut AppUI| {
        let worker = std::thread::spawn(move || post(port, &body));
        while !worker.is_finished() {
            app.handle_web_commands();
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.join().unwrap()
    };
    let (status, json) = request(
        serde_json::json!({"action": "playlist_create", "name": "Mix"}),
        &mut app,
    );
    assert_eq!((status, json["message"].as_str()), (200, Some("Created \"Mix\"")));
    let (status, json) = request(
        serde_json::json!({"action": "playlist_create", "name": "road trip"}),
        &mut app,
    );
    assert_eq!(
        (status, json["error"].as_str()),
        (409, Some("\"Road Trip\" already exists"))
    );
    let (status, json) = request(
        serde_json::json!({"action": "playlist_add", "name": "Mix", "path": a}),
        &mut app,
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(app.playlists.store.get("Mix").unwrap().entries[0].path, a);
    assert!(app.persistence.flush());
    assert!(playlist_file("Mix").is_file());
    let _ = fs::remove_dir_all(&music);
}

// ---------------------------------------------------------------------------------------------
// PR #65 review
// ---------------------------------------------------------------------------------------------

/// SP Minor 6: a file reached through a symlinked folder is stored as the request named it, as
/// a terminal add stores it, so adding it from both sides is a duplicate and its tags are found.
#[test]
fn test_web_add_through_a_symlinked_folder_matches_the_terminal_add() {
    common::isolate();
    let music = temp_dir("add_symlinked");
    let real = music.join("Real");
    let song = real.join("song.mp3");
    touch(&song);
    let linked_dir = music.join("Linked");
    std::os::unix::fs::symlink(&real, &linked_dir).unwrap();
    let linked = linked_dir.join("song.mp3");
    write_playlists(&[(PLAYLIST, vec![]), ("Old", vec![entry(&song, "", "song", -1)])]);
    let mut app = web_app(&music);
    // The browser read the tags through the symlinked folder.
    app.metadata_cache.insert(quickstream::metadata::AudioMetadata {
        filepath: linked.clone(),
        title: "Song".to_string(),
        artist: "Band".to_string(),
        duration_seconds: 90.0,
        ..Default::default()
    });
    let add = |name: &str| PlaylistEdit::Add {
        name: name.to_string(),
        path: linked.clone(),
        canonical: song.clone(),
    };

    assert_eq!(
        edit(&mut app, add(PLAYLIST)),
        Ok("Added \"Song\" to Road Trip (1 track)".to_string())
    );
    let stored = &app.playlists.store.get(PLAYLIST).unwrap().entries[0];
    assert_eq!(stored.path, linked, "stored as a terminal add would store it");
    assert_eq!((stored.artist.as_str(), stored.duration_secs), ("Band", 90));
    // The terminal adding the same file finds it already there.
    let result = app
        .playlists
        .store
        .add(PLAYLIST, vec![PlaylistEntry::from_path(&linked)])
        .unwrap();
    assert_eq!((result.added, result.skipped), (0, 1));
    // A playlist that already holds the file by its canonical path (an older web add) gets no
    // second copy either.
    assert_eq!(edit(&mut app, add("Old")), Ok("Already in Old".to_string()));
    assert_eq!(app.playlists.store.get("Old").unwrap().entries.len(), 1);
    let _ = fs::remove_dir_all(&music);
}

/// SP Minor 5: a play the UI refuses (another playlist is still being checked) is reported as
/// refused, not as started.
#[test]
fn test_web_play_refused_by_the_ui_is_not_reported_as_started() {
    common::isolate();
    let music = temp_dir("play_refused_by_ui");
    let a = music.join("a.mp3");
    touch(&a);
    write_playlists(&[
        (PLAYLIST, vec![entry(&a, "A", "One", 100)]),
        ("Mix", vec![entry(&a, "A", "One", 100)]),
    ]);
    let mut app = web_app(&music);
    app.player = Box::new(SilentPlayer { playing: false });
    // A check that never finishes during the test, like a hung share.
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let gate_rx = std::sync::Mutex::new(gate_rx);
    let checker: quickstream::ui::playlists::ExistsChecker = Arc::new(move |path: &Path| {
        let _ = gate_rx.lock().unwrap().recv();
        Some(path.to_path_buf())
    });
    app.playlists.set_exists_checker(checker);

    app.update_web_state();
    let port = app.web_server.as_ref().unwrap().port;
    let (status, json) = request_while_ticking(
        &mut app,
        port,
        serde_json::json!({"action": "playlist_play", "name": "Mix"}),
    );
    assert_eq!(status, 200, "{}", json);
    assert_eq!(
        json,
        serde_json::json!({"ok": true, "message": "Checking 1 track in Mix…"})
    );

    // Mix is still being checked: the UI refuses, and the request says so with a 409, so the
    // dashboard shows the refusal instead of "Playing Road Trip".
    for action in ["playlist_play", "playlist_append"] {
        let (status, json) =
            request_while_ticking(&mut app, port, serde_json::json!({"action": action, "name": PLAYLIST}));
        assert_eq!(status, 409, "{}: {}", action, json);
        assert_eq!(json, serde_json::json!({"ok": false, "error": "Still checking Mix…"}));
    }
    drop(gate_tx);
    let _ = fs::remove_dir_all(&music);
}

/// CC7: while existence checks stream in, the snapshot is rebuilt (and `playlists_version`
/// bumped, which makes the dashboard fetch again) at most every 500 ms. Edits still publish at
/// once.
#[test]
fn test_snapshot_rebuilds_for_existence_checks_are_throttled() {
    common::isolate();
    let music = temp_dir("throttle");
    let here = music.join("here.mp3");
    let ghost = music.join("ghost.mp3");
    touch(&here);
    write_playlists(&[(
        PLAYLIST,
        vec![entry(&here, "A", "Here", 100), entry(&ghost, "A", "Ghost", 100)],
    )]);
    let mut app = web_app(&music);
    app.update_web_state();
    let version = |app: &AppUI| app.web_state.read().unwrap().playlists_version;
    // Let any earlier republish age past the interval, then publish an edit: edits are never
    // held back, and it starts a new interval.
    std::thread::sleep(Duration::from_millis(600));
    app.playlists.store.create("Mix").unwrap();
    let before = version(&app);
    app.update_web_state();
    assert_eq!(version(&app), before + 1, "an edit publishes at once");
    let published = version(&app);

    send(&mut app, WebCommand::CheckPlaylists);
    let deadline = Instant::now() + Duration::from_secs(2);
    while app.playlists.checks_in_flight() > 0 && Instant::now() < deadline {
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(app.playlists.checks_in_flight(), 0, "checks did not finish");
    for _ in 0..5 {
        app.update_web_state();
    }
    assert_eq!(
        version(&app),
        published,
        "existence results within 500 ms are held back"
    );
    assert_eq!(snapshot(&app).playlists[1].entries[1].missing, None);

    std::thread::sleep(Duration::from_millis(550));
    app.update_web_state();
    assert_eq!(version(&app), published + 1, "the held-back results are published");
    assert_eq!(snapshot(&app).playlists[1].entries[1].missing, Some(true));
    app.update_web_state();
    assert_eq!(version(&app), published + 1, "nothing changed since");
    let _ = fs::remove_dir_all(&music);
}

/// CC7: the dashboard sends CheckPlaylists on every fetch; paths already checked (or being
/// checked) start no new check, so fetching again can't loop.
#[test]
fn test_repeated_check_playlists_does_not_check_paths_again() {
    common::isolate();
    let music = temp_dir("recheck");
    let here = music.join("here.mp3");
    touch(&here);
    write_playlists(&[(
        PLAYLIST,
        vec![
            entry(&here, "A", "Here", 100),
            entry(&music.join("ghost.mp3"), "A", "Ghost", 100),
        ],
    )]);
    let mut app = web_app(&music);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = calls.clone();
    let checker: quickstream::ui::playlists::ExistsChecker = Arc::new(move |path: &Path| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        quickstream::ui::playlists::playable_audio_file(path)
    });
    app.playlists.set_exists_checker(checker);

    send(&mut app, WebCommand::CheckPlaylists);
    // Sent again while the first checks may still run: nothing is checked twice.
    send(&mut app, WebCommand::CheckPlaylists);
    assert!(poll_until(&mut app, |app| app.playlists.checks_in_flight() == 0));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    for _ in 0..3 {
        send(&mut app, WebCommand::CheckPlaylists);
        app.poll_playlists();
    }
    std::thread::sleep(Duration::from_millis(50));
    app.poll_playlists();
    assert_eq!(app.playlists.checks_in_flight(), 0);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "already-checked paths were checked again"
    );
    let _ = fs::remove_dir_all(&music);
}
