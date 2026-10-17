//! Tab 5 playlists: missing-file checks, play (`Enter`) and queue (`a`) (issue #37).
//!
//! Existence checks, including the ones behind play/queue, run on workers because a `stat` on an
//! unmounted share can hang. Run with `--test-threads=1`: the tests share one playlists folder.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quickstream::metadata::AudioMetadata;
use quickstream::player::AudioPlayer;
use quickstream::playlist::{Playlist, PlaylistEntry};
use quickstream::settings::SettingsManager;
use quickstream::ui::playlists::{missing_row_text, mount_hint, ExistsChecker};
use quickstream::ui::AppUI;

const PLAYLIST: &str = "Road Trip";
const UI_BUDGET: Duration = Duration::from_millis(200);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qs_pl_play_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn touch(path: &Path) {
    std::fs::write(path, b"not really audio").unwrap();
}

fn entry(path: &Path, artist: &str, title: &str) -> PlaylistEntry {
    PlaylistEntry {
        path: path.to_path_buf(),
        duration_secs: 200,
        artist: artist.to_string(),
        title: title.to_string(),
    }
}

/// Replace the playlists folder with a single `PLAYLIST` file and return its path.
fn write_playlist(entries: Vec<PlaylistEntry>) -> PathBuf {
    let dir = quickstream::paths::get_playlists_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let playlist = Playlist {
        name: PLAYLIST.to_string(),
        entries,
    };
    let file = dir.join(format!("{}.m3u8", PLAYLIST));
    std::fs::write(&file, playlist.to_m3u8()).unwrap();
    file
}

/// Call `poll_playlists` until `cond` holds or 2 seconds pass.
fn poll_until(app: &mut AppUI, cond: impl Fn(&AppUI) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        app.poll_playlists();
        if cond(app) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An app whose store has loaded `PLAYLIST`, selected in tab 5's left pane. The active tab is
/// left at its default (not tab 5), so no existence check has started yet.
fn app_with_playlist(dir: &Path) -> AppUI {
    let mut app = AppUI::new(
        Some(dir.to_path_buf()),
        SettingsManager::with_path(dir.join("config.json")),
    );
    app.start_playlist_load();
    assert!(
        poll_until(&mut app, |app| app.playlists.store.get(PLAYLIST).is_some()),
        "playlist did not load"
    );
    app.playlists.list_cursor = app.playlists.store.index_of(PLAYLIST).unwrap();
    app
}

fn render(app: &mut AppUI, width: u16, height: u16) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    terminal.backend().to_string()
}

#[test]
fn test_mount_hint_is_the_first_two_folders() {
    common::isolate();
    assert_eq!(
        mount_hint(Path::new("/Volumes/Share/Radiohead/OK Computer/01 Airbag.flac")),
        "/Volumes/Share"
    );
    assert_eq!(mount_hint(Path::new("/Volumes/Share")), "/Volumes/Share");
    assert_eq!(mount_hint(Path::new("/song.flac")), "/song.flac");
}

#[test]
fn test_missing_row_text_marks_the_entry() {
    common::isolate();
    let ghost = entry(Path::new("/music/gone.flac"), "Ghost", "Gone");
    assert_eq!(missing_row_text(1, &ghost), "2. ✗ missing: Ghost - Gone  ----");
}

#[test]
fn test_missing_entries_render_as_missing_and_playlist_is_untouched() {
    common::isolate();
    let dir = temp_dir("missing_rows");
    let present = dir.join("present.mp3");
    let ghost = dir.join("ghost.mp3");
    touch(&present);
    let file = write_playlist(vec![entry(&present, "Alpha", "One"), entry(&ghost, "Ghost", "Gone")]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);

    app.active_tab = 4;
    assert!(
        poll_until(&mut app, |app| app.playlists.exists.len() == 2),
        "existence checks did not finish"
    );
    assert!(app.playlists.is_missing(&ghost));
    assert!(!app.playlists.is_missing(&present));

    let view = render(&mut app, 120, 30);
    assert!(view.contains("✗ missing: Ghost - Gone  ----"), "{}", view);
    assert!(view.contains("Alpha - One"), "{}", view);
    assert!(!view.contains("✗ missing: Alpha"), "{}", view);

    // A missing file never changes the playlist or its file.
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_hung_existence_check_does_not_block_ui() {
    common::isolate();
    let dir = temp_dir("hung_check");
    let first = dir.join("first.mp3");
    let ghost = dir.join("ghost.mp3");
    write_playlist(vec![entry(&first, "Alpha", "One"), entry(&ghost, "Ghost", "Gone")]);
    let mut app = app_with_playlist(&dir);

    // Each check blocks like a stat on a hung share until the gate's sender is dropped, then
    // reports the path missing. The watchdog drops it after 2s so a blocking UI fails the
    // timing asserts below instead of hanging the test.
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Arc::new(Mutex::new(gate_rx));
    let checker: ExistsChecker = Arc::new(move |_path: &Path| {
        let _ = gate_rx.lock().unwrap().recv();
        false
    });
    app.playlists.set_exists_checker(checker);
    let (open_tx, open_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let _ = open_rx.recv_timeout(Duration::from_secs(2));
        drop(gate_tx);
    });

    app.active_tab = 4;
    let start = Instant::now();
    app.poll_playlists();
    let elapsed = start.elapsed();
    assert!(elapsed < UI_BUDGET, "poll_playlists blocked for {:?}", elapsed);
    assert_eq!(app.playlists.checks_in_flight(), 2);

    // Rows not checked yet render normally.
    let view = render(&mut app, 120, 30);
    assert!(view.contains("Ghost - Gone"), "{}", view);
    assert!(!view.contains("✗ missing"), "{}", view);

    // Polling again neither blocks nor checks the same paths a second time.
    let start = Instant::now();
    app.poll_playlists();
    let elapsed = start.elapsed();
    assert!(elapsed < UI_BUDGET, "poll_playlists blocked for {:?}", elapsed);
    assert_eq!(app.playlists.checks_in_flight(), 2);

    open_tx.send(()).unwrap();
    watchdog.join().unwrap();
    assert!(
        poll_until(&mut app, |app| app.playlists.checks_in_flight() == 0),
        "checks did not finish after the gate opened"
    );
    assert!(app.playlists.is_missing(&first));
    assert!(app.playlists.is_missing(&ghost));

    // Results are cached for the session: showing the playlist again starts no new check.
    app.poll_playlists();
    assert_eq!(app.playlists.checks_in_flight(), 0);
    let view = render(&mut app, 120, 30);
    assert!(view.contains("✗ missing: Ghost - Gone"), "{}", view);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Player stand-in that records commands instead of starting mpv.
struct RecordingPlayer {
    calls: Arc<Mutex<Vec<String>>>,
    playing: bool,
    paused: bool,
    volume: u8,
    fail_play: bool,
}

impl AudioPlayer for RecordingPlayer {
    fn play(&mut self, filepath: &Path) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("play {}", filepath.display()));
        if self.fail_play {
            anyhow::bail!("no audio backend");
        }
        self.playing = true;
        self.paused = false;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.paused = true;
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.paused = false;
        Ok(())
    }
    fn toggle_pause(&mut self) -> anyhow::Result<bool> {
        self.paused = !self.paused;
        Ok(self.paused)
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("stop".to_string());
        self.playing = false;
        self.paused = false;
        Ok(())
    }
    fn seek(&mut self, _delta_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn seek_to(&mut self, _position_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_volume(&mut self, volume: u8) -> anyhow::Result<()> {
        self.volume = volume;
        Ok(())
    }
    fn volume(&self) -> u8 {
        self.volume
    }
    fn is_playing(&self) -> bool {
        self.playing
    }
    fn is_paused(&self) -> bool {
        self.paused
    }
    fn position_seconds(&self) -> f64 {
        0.0
    }
    fn duration_seconds(&self) -> f64 {
        0.0
    }
}

fn install_recording_player(app: &mut AppUI) -> Arc<Mutex<Vec<String>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    app.player = Box::new(RecordingPlayer {
        calls: calls.clone(),
        playing: false,
        paused: false,
        volume: 50,
        fail_play: false,
    });
    calls
}

fn press(app: &mut AppUI, code: crossterm::event::KeyCode) {
    app.handle_input_key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ))
    .unwrap();
}

fn queue_paths(app: &AppUI) -> Vec<PathBuf> {
    app.queue.tracks.iter().map(|t| t.filepath.clone()).collect()
}

fn old_track(dir: &Path) -> AudioMetadata {
    AudioMetadata {
        filepath: dir.join("old.mp3"),
        title: "Old".to_string(),
        ..Default::default()
    }
}

#[test]
fn test_enter_replaces_queue_stops_autopilot_and_plays_next_existing_track() {
    common::isolate();
    let dir = temp_dir("replace");
    let first = dir.join("a.mp3");
    let ghost = dir.join("ghost.mp3");
    let third = dir.join("c.mp3");
    touch(&first);
    touch(&third);
    let file = write_playlist(vec![
        entry(&first, "Alpha", "One"),
        entry(&ghost, "Ghost", "Gone"),
        entry(&third, "Gamma", "Three"),
    ]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    // Fully resolved tags: nothing left for the background fetch to fill in.
    app.metadata_cache.insert(AudioMetadata {
        filepath: first.clone(),
        title: "Cached One".to_string(),
        artist: "Alpha".to_string(),
        duration_seconds: 200.0,
        sample_rate_hz: 44_100,
        ..Default::default()
    });

    // An old queue and a running autopilot scan, both of which the replace must end.
    app.queue.add_track(old_track(&dir));
    let scan_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    app.autopilot_scan_stop = Some(scan_stop.clone());
    let (_autopilot_tx, autopilot_rx) = std::sync::mpsc::channel::<quickstream::library::AutopilotEvent>();
    app.autopilot_rx = Some(autopilot_rx);
    app.is_autopilot_scanning = true;
    app.autopilot_dir = Some(dir.clone());

    // Enter on the missing track plays the next existing one.
    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 1;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("Queued")),
        "Enter did not queue the playlist; status: {}",
        app.status_message
    );

    assert_eq!(app.status_message, "Queued 2 tracks from Road Trip, 1 missing");
    assert_eq!(queue_paths(&app), vec![first.clone(), third.clone()]);
    assert_eq!(app.queue.tracks[0].title, "Cached One", "cached metadata is used");
    assert_eq!(app.queue.current_index, 1);
    assert_eq!(app.queue_cursor, 1);

    // The uncached entry is a placeholder with an #EXTINF artist and duration but no audio
    // properties: its real tags (album, bitrate, sample rate) are still fetched in the background.
    assert_eq!(app.queue.tracks[1].artist, "Gamma");
    assert!(app.queue.tracks[1].duration_seconds > 0.0);
    assert!(
        app.queue_metadata_attempted.contains(&third),
        "placeholder with artist and duration was not sent for a tag fetch"
    );
    assert!(
        !app.queue_metadata_attempted.contains(&first),
        "fully cached track was fetched again"
    );

    let calls = calls.lock().unwrap().clone();
    let stop_at = calls
        .iter()
        .position(|c| c == "stop")
        .expect("playback was not stopped");
    let play_at = calls
        .iter()
        .rposition(|c| c.starts_with("play "))
        .expect("playback did not start");
    assert!(stop_at < play_at, "calls: {:?}", calls);
    assert_eq!(calls[play_at], format!("play {}", third.display()));

    assert!(
        scan_stop.load(std::sync::atomic::Ordering::SeqCst),
        "autopilot scan not stopped"
    );
    assert!(app.autopilot_rx.is_none());
    assert!(!app.is_autopilot_scanning);
    assert!(app.autopilot_dir.is_none());

    // The missing result is cached for the rows but never changes the playlist or its file.
    assert!(app.playlists.is_missing(&ghost));
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_a_appends_playlist_from_either_pane_without_touching_playback() {
    common::isolate();
    let dir = temp_dir("append");
    let first = dir.join("first.mp3");
    let second = dir.join("second.mp3");
    touch(&first);
    touch(&second);
    write_playlist(vec![entry(&first, "Alpha", "One"), entry(&second, "Beta", "Two")]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    app.queue.add_track(old_track(&dir));

    // Left pane.
    app.active_tab = 4;
    app.playlists.tracks_focus = false;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert!(
        poll_until(&mut app, |app| app.queue.len() == 3),
        "a did not append the playlist; status: {}",
        app.status_message
    );
    assert_eq!(app.status_message, "Queued 2 tracks from Road Trip");
    assert_eq!(
        queue_paths(&app),
        vec![dir.join("old.mp3"), first.clone(), second.clone()]
    );
    assert_eq!(app.queue.current_index, 0);

    // Tracks pane.
    app.playlists.tracks_focus = true;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert!(
        poll_until(&mut app, |app| app.queue.len() == 5),
        "a in the tracks pane did not append; status: {}",
        app.status_message
    );
    assert_eq!(app.queue.current_index, 0);
    assert!(
        calls.lock().unwrap().is_empty(),
        "append must not stop or start playback"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_all_missing_leaves_queue_unchanged() {
    common::isolate();
    let dir = temp_dir("all_missing");
    let file = write_playlist(vec![
        entry(Path::new("/nonexistent-qs-volume/Music/Album/1.flac"), "A", "One"),
        entry(Path::new("/nonexistent-qs-volume/Music/Album/2.flac"), "B", "Two"),
    ]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    app.queue.add_track(old_track(&dir));

    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("None of")),
        "no all-missing status; status: {}",
        app.status_message
    );

    assert_eq!(
        app.status_message,
        "None of Road Trip's 2 tracks found. Is /nonexistent-qs-volume/Music mounted?"
    );
    assert_eq!(queue_paths(&app), vec![dir.join("old.mp3")]);
    assert!(
        calls.lock().unwrap().is_empty(),
        "queue and playback must be left alone"
    );
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_queueing_an_empty_playlist_reports_it() {
    common::isolate();
    let dir = temp_dir("empty");
    write_playlist(Vec::new());
    let mut app = app_with_playlist(&dir);
    app.queue.add_track(old_track(&dir));

    app.active_tab = 4;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert_eq!(app.status_message, "Road Trip is empty");
    assert_eq!(app.queue.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_help_modal_lists_playlist_play_and_queue_keys() {
    common::isolate();
    let dir = temp_dir("help");
    let mut app = AppUI::new(Some(dir.clone()), SettingsManager::with_path(dir.join("config.json")));
    app.show_help_modal = true;
    let view = render(&mut app, 120, 40);
    assert!(view.contains("Open/Play / Queue playlist"), "{}", view);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_playback_error_is_kept_when_enter_queues_the_playlist() {
    common::isolate();
    let dir = temp_dir("play_error");
    let first = dir.join("a.mp3");
    let second = dir.join("b.mp3");
    touch(&first);
    touch(&second);
    write_playlist(vec![entry(&first, "Alpha", "One"), entry(&second, "Beta", "Two")]);
    let mut app = app_with_playlist(&dir);
    app.player = Box::new(RecordingPlayer {
        calls: Arc::new(Mutex::new(Vec::new())),
        playing: false,
        paused: false,
        volume: 50,
        fail_play: true,
    });
    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(
        poll_until(&mut app, |app| app.status_message.contains("Queued")),
        "Enter did not queue the playlist; status: {}",
        app.status_message
    );
    assert_eq!(
        app.status_message, "Playback error: no audio backend. Queued 2 tracks from Road Trip",
        "error swallowed or summary missing"
    );
    assert!(!app.player.is_playing(), "a failed start must not report playing");
    let _ = std::fs::remove_dir_all(&dir);
}
