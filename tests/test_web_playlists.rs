//! Issue #50: playlists on the web dashboard.
//!
//! The UI thread publishes a snapshot of the playlists in the shared web state; `GET
//! /api/playlists` serves it without reading the playlists folder. Play and append go through
//! `/api/action` as `WebCommand::QueuePlaylist`, after the web thread checked every entry's path
//! against the allowed folders, and the UI thread queues them with the code tab 5 uses.
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
use quickstream::ui::AppUI;
use quickstream::web::{
    NowPlayingState, WebCommand, WebPlaylist, WebPlaylistEntry, WebPlaylists, WebPlaylistsState, WebServer,
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
    app.queue.tracks.iter().map(|t| t.filepath.clone()).collect()
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
    assert_eq!(
        drain(&rx),
        vec![WebCommand::QueuePlaylist {
            name: PLAYLIST.to_string(),
            entries: expected.clone(),
            play_from: Some(2),
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
            },
            WebCommand::QueuePlaylist {
                name: PLAYLIST.to_string(),
                entries: expected,
                play_from: None,
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
            serde_json::json!({"action": "playlist_play", "name": PLAYLIST, "index": 1}),
            400,
            "No such track",
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
        },
    );
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("Queued")),
        "{}",
        app.status_message
    );
    assert_eq!(queue_paths(&app), vec![a.clone(), c.clone()]);
    assert_eq!(app.queue.current_index, 1);
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
        },
    );
    assert_eq!(app.status_message, "Playlists are still loading");
    assert!(!app.playlists.queue_check_pending());
    assert!(app.queue.is_empty());
    let _ = fs::remove_dir_all(&music);
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

    let (status, json) = post(
        port,
        &serde_json::json!({"action": "playlist_play", "name": PLAYLIST, "index": 1, "revision": revision}),
    );
    assert_eq!(status, 200, "{}", json);
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(queue_paths(&app), vec![a, b]);
    assert_eq!(app.queue.current_index, 1);
    let _ = fs::remove_dir_all(&music);
}
