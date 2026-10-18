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
    // Canonical (macOS: /private/var/...), since the queue holds checked, canonical paths.
    std::fs::canonicalize(&dir).unwrap()
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
        mount_hint(Path::new("/Volumes/Share/Radiohead/OK Computer/01 Airbag.flac")).as_deref(),
        Some("/Volumes/Share")
    );
    assert_eq!(
        mount_hint(Path::new("/Volumes/Music/a/b.mp3")).as_deref(),
        Some("/Volumes/Music")
    );
    assert_eq!(
        mount_hint(Path::new("/Volumes/Share/song.flac")).as_deref(),
        Some("/Volumes/Share")
    );
}

#[test]
fn test_mount_hint_never_names_a_file_or_a_top_level_folder() {
    common::isolate();
    // Issue #59: the hint comes from the folder holding the file, never the file itself. A file
    // at `/` or directly in a top-level folder (`/Volumes`, `/mnt`) has no share to name:
    // "Is /Volumes mounted?" reads wrong, so the hint is left out.
    assert_eq!(mount_hint(Path::new("/ghost.mp3")), None);
    assert_eq!(mount_hint(Path::new("/Volumes/ghost.mp3")), None);
    assert_eq!(mount_hint(Path::new("/Volumes/Share")), None);
}

#[test]
fn test_all_missing_without_a_mount_hint_leaves_out_the_question() {
    common::isolate();
    let dir = temp_dir("all_missing_root");
    write_playlist(vec![entry(Path::new("/nonexistent-qs-ghost.mp3"), "A", "One")]);
    let mut app = app_with_playlist(&dir);
    install_recording_player(&mut app);
    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("None of")));
    assert_eq!(app.status_message, "None of Road Trip's 1 track found.");
    let _ = std::fs::remove_dir_all(&dir);
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
        None
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
fn test_all_missing_single_entry_says_track() {
    common::isolate();
    let dir = temp_dir("all_missing_one");
    write_playlist(vec![entry(
        Path::new("/nonexistent-qs-volume/Music/Album/1.flac"),
        "A",
        "One",
    )]);
    let mut app = app_with_playlist(&dir);
    install_recording_player(&mut app);
    app.active_tab = 4;
    app.playlists.tracks_focus = true;
    app.playlists.track_cursor = 0;
    press(&mut app, crossterm::event::KeyCode::Enter);
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("None of")));
    assert_eq!(
        app.status_message,
        "None of Road Trip's 1 track found. Is /nonexistent-qs-volume/Music mounted?"
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

#[test]
fn test_symlink_to_a_non_audio_file_is_not_queued() {
    common::isolate();
    let dir = temp_dir("symlink");
    let good = dir.join("good.mp3");
    let secret = dir.join("secret.txt");
    let link = dir.join("x.mp3");
    touch(&good);
    touch(&secret);
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    write_playlist(vec![entry(&good, "Alpha", "One"), entry(&link, "Eve", "Link")]);
    let mut app = app_with_playlist(&dir);

    app.active_tab = 4;
    app.playlists.tracks_focus = false;
    press(&mut app, crossterm::event::KeyCode::Char('a'));
    assert!(
        poll_until(&mut app, |app| app.queue.len() == 1),
        "status: {}",
        app.status_message
    );
    assert_eq!(queue_paths(&app), vec![good.clone()]);
    assert_eq!(app.status_message, "Queued 1 track from Road Trip, 1 missing");
    assert!(app.playlists.is_missing(&link));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A checker that counts its calls and blocks like a stat on a hung share until the returned
/// sender is dropped, then reports every path present.
fn gated_checker() -> (
    ExistsChecker,
    std::sync::mpsc::Sender<()>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Arc::new(Mutex::new(gate_rx));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = calls.clone();
    let checker: ExistsChecker = Arc::new(move |path: &Path| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = gate_rx.lock().unwrap().recv();
        Some(path.to_path_buf())
    });
    (checker, gate_tx, calls)
}

/// Poll for `window`, so a second (wrong) result has time to arrive.
fn poll_for(app: &mut AppUI, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        app.poll_playlists();
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn test_second_queue_request_while_one_is_checking_starts_nothing() {
    common::isolate();
    let dir = temp_dir("one_request");
    let track = dir.join("a.mp3");
    touch(&track);
    write_playlist(vec![entry(&track, "Alpha", "One")]);
    let mut app = app_with_playlist(&dir);
    let (checker, gate_tx, calls) = gated_checker();
    app.playlists.set_exists_checker(checker);

    // The active tab stays off tab 5, so no row existence checks call the checker.
    app.queue_selected_playlist(None);
    assert_eq!(app.status_message, "Checking 1 tracks in Road Trip…");
    assert!(app.playlists.queue_check_pending());
    app.queue_selected_playlist(None);
    assert_eq!(app.status_message, "Still checking Road Trip…");

    drop(gate_tx);
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("Queued")),
        "status: {}",
        app.status_message
    );
    poll_for(&mut app, Duration::from_millis(200));
    assert_eq!(queue_paths(&app).len(), 1, "the playlist was queued twice");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a second worker ran"
    );
    assert!(!app.playlists.queue_check_pending());

    // Once the result is in, a new request starts.
    app.queue_selected_playlist(None);
    assert_eq!(app.status_message, "Checking 1 tracks in Road Trip…");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_playlist_result_after_autopilot_start_does_not_touch_the_queue() {
    common::isolate();
    let dir = temp_dir("stale_autopilot");
    let track = dir.join("a.mp3");
    touch(&track);
    write_playlist(vec![entry(&track, "Alpha", "One")]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    let (checker, gate_tx, _) = gated_checker();
    app.playlists.set_exists_checker(checker);
    app.queue.add_track(old_track(&dir));

    app.queue_selected_playlist(Some(0));
    let empty = temp_dir("stale_autopilot_lib");
    app.start_autopilot(empty.clone());

    drop(gate_tx);
    assert!(
        poll_until(&mut app, |app| !app.playlists.queue_check_pending()),
        "the playlist check never finished"
    );
    poll_for(&mut app, Duration::from_millis(100));
    assert_eq!(queue_paths(&app), vec![dir.join("old.mp3")]);
    assert!(
        !calls.lock().unwrap().iter().any(|c| c.starts_with("play ")),
        "a stale playlist result started playback"
    );
    if let Some(stop) = app.autopilot_scan_stop.take() {
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn test_playlist_result_after_clearing_the_queue_does_not_touch_it() {
    common::isolate();
    let dir = temp_dir("stale_clear");
    let track = dir.join("a.mp3");
    touch(&track);
    write_playlist(vec![entry(&track, "Alpha", "One")]);
    let mut app = app_with_playlist(&dir);
    install_recording_player(&mut app);
    let (checker, gate_tx, _) = gated_checker();
    app.playlists.set_exists_checker(checker);
    app.queue.add_track(old_track(&dir));

    app.queue_selected_playlist(None);
    app.active_tab = 1;
    app.queue_focus = true;
    press(&mut app, crossterm::event::KeyCode::Char('c'));
    assert!(app.queue.is_empty());

    drop(gate_tx);
    assert!(
        poll_until(&mut app, |app| !app.playlists.queue_check_pending()),
        "the playlist check never finished"
    );
    poll_for(&mut app, Duration::from_millis(100));
    assert!(app.queue.is_empty(), "queue: {:?}", queue_paths(&app));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The queue check resolves symlinks, and the queue (and so the stream registry) gets the path
/// it checked: a link swapped after the check can't point playback at another file.
#[test]
fn test_symlinked_entry_is_queued_by_its_checked_target() {
    common::isolate();
    let dir = temp_dir("symlink_target");
    let target = dir.join("real.mp3");
    let link = dir.join("link.mp3");
    touch(&target);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let file = write_playlist(vec![entry(&link, "Alpha", "One")]);
    let before = std::fs::read(&file).unwrap();
    let mut app = app_with_playlist(&dir);

    app.queue_selected_playlist(None);
    assert!(
        poll_until(&mut app, |app| app.queue.len() == 1),
        "status: {}",
        app.status_message
    );
    let canonical = std::fs::canonicalize(&target).unwrap();
    assert_eq!(queue_paths(&app), vec![canonical]);
    // The playlist keeps the link.
    app.persistence.flush();
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(app.playlists.store.get(PLAYLIST).unwrap().entries[0].path, link);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A checker that answers `instant` paths at once (present when `true`) and blocks on every
/// other path, like a stat on a hung share, until the returned sender is dropped; then it
/// reports them present. It records the order of the paths it was asked about.
fn checker_answering_first(
    instant: Vec<(PathBuf, bool)>,
) -> (ExistsChecker, std::sync::mpsc::Sender<()>, Arc<Mutex<Vec<PathBuf>>>) {
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Arc::new(Mutex::new(gate_rx));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let checker: ExistsChecker = Arc::new(move |path: &Path| {
        log.lock().unwrap().push(path.to_path_buf());
        if let Some((_, present)) = instant.iter().find(|(p, _)| p == path) {
            return present.then(|| path.to_path_buf());
        }
        let _ = gate_rx.lock().unwrap().recv();
        Some(path.to_path_buf())
    });
    (checker, gate_tx, asked)
}

fn play_calls(calls: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("play "))
        .cloned()
        .collect()
}

#[test]
fn test_enter_plays_the_selected_track_before_the_rest_are_checked() {
    common::isolate();
    // Issue #52: Enter waited for every entry to be checked before playing anything. The
    // selected entry is checked first and plays at once; the rest of the queue fills in when
    // their checks finish.
    let dir = temp_dir("play_first");
    let paths: Vec<PathBuf> = ["a.mp3", "b.mp3", "c.mp3", "d.mp3"]
        .iter()
        .map(|name| dir.join(name))
        .collect();
    for path in &paths {
        touch(path);
    }
    write_playlist(vec![
        entry(&paths[0], "Alpha", "One"),
        entry(&paths[1], "Beta", "Two"),
        entry(&paths[2], "Gamma", "Three"),
        entry(&paths[3], "Delta", "Four"),
    ]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    let (checker, gate_tx, asked) = checker_answering_first(vec![(paths[1].clone(), true)]);
    app.playlists.set_exists_checker(checker);
    app.queue.add_track(old_track(&dir));

    // The active tab stays off tab 5, so only the play request calls the checker.
    app.queue_selected_playlist(Some(1));
    assert!(
        poll_until(&mut app, |_| !play_calls(&calls).is_empty()),
        "the selected track did not start while the others were still being checked; status: {}",
        app.status_message
    );
    assert_eq!(
        asked.lock().unwrap()[0],
        paths[1],
        "the selected entry is checked first"
    );
    assert_eq!(play_calls(&calls), vec![format!("play {}", paths[1].display())]);
    assert_eq!(queue_paths(&app), vec![paths[1].clone()], "the old queue was replaced");
    assert_eq!(app.queue.current_index, 0);
    assert!(app.playlists.queue_check_pending(), "the rest are still being checked");
    assert_eq!(app.status_message, "Playing from Road Trip; checking the rest…");

    drop(gate_tx);
    assert!(
        poll_until(&mut app, |app| app.status_message.starts_with("Queued")),
        "status: {}",
        app.status_message
    );
    assert_eq!(app.status_message, "Queued 4 tracks from Road Trip");
    assert_eq!(queue_paths(&app), paths);
    assert_eq!(app.queue.current_index, 1, "the playing track keeps its place");
    assert_eq!(app.queue_cursor, 1);
    assert_eq!(
        play_calls(&calls).len(),
        1,
        "filling the queue must not restart playback"
    );
    assert!(!app.playlists.queue_check_pending());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_enter_on_a_missing_track_starts_the_next_existing_one_first() {
    common::isolate();
    let dir = temp_dir("play_first_missing");
    let first = dir.join("a.mp3");
    let ghost = dir.join("ghost.mp3");
    let third = dir.join("c.mp3");
    let fourth = dir.join("d.mp3");
    for path in [&first, &third, &fourth] {
        touch(path);
    }
    write_playlist(vec![
        entry(&first, "Alpha", "One"),
        entry(&ghost, "Ghost", "Gone"),
        entry(&third, "Gamma", "Three"),
        entry(&fourth, "Delta", "Four"),
    ]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    let (checker, gate_tx, asked) = checker_answering_first(vec![(ghost.clone(), false), (third.clone(), true)]);
    app.playlists.set_exists_checker(checker);

    app.queue_selected_playlist(Some(1));
    assert!(
        poll_until(&mut app, |_| !play_calls(&calls).is_empty()),
        "status: {}",
        app.status_message
    );
    assert_eq!(asked.lock().unwrap()[..2], [ghost.clone(), third.clone()]);
    assert_eq!(play_calls(&calls), vec![format!("play {}", third.display())]);
    assert_eq!(queue_paths(&app), vec![third.clone()]);

    drop(gate_tx);
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(app.status_message, "Queued 3 tracks from Road Trip, 1 missing");
    assert_eq!(queue_paths(&app), vec![first.clone(), third.clone(), fourth.clone()]);
    assert_eq!(app.queue.current_index, 1);
    assert_eq!(play_calls(&calls).len(), 1);
    assert!(app.playlists.is_missing(&ghost));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_enter_after_the_last_existing_track_wraps_to_the_first() {
    common::isolate();
    // Nothing exists at or after the selected entry: playback starts at the first existing
    // entry, so the checks go on from the top of the playlist.
    let dir = temp_dir("play_first_wrap");
    let first = dir.join("a.mp3");
    let second = dir.join("b.mp3");
    let ghost = dir.join("ghost.mp3");
    touch(&first);
    touch(&second);
    write_playlist(vec![
        entry(&first, "Alpha", "One"),
        entry(&second, "Beta", "Two"),
        entry(&ghost, "Ghost", "Gone"),
    ]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    let (checker, gate_tx, asked) = checker_answering_first(vec![(ghost.clone(), false), (first.clone(), true)]);
    app.playlists.set_exists_checker(checker);

    app.queue_selected_playlist(Some(2));
    assert!(poll_until(&mut app, |_| !play_calls(&calls).is_empty()));
    assert_eq!(asked.lock().unwrap()[..2], [ghost.clone(), first.clone()]);
    assert_eq!(play_calls(&calls), vec![format!("play {}", first.display())]);

    drop(gate_tx);
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(queue_paths(&app), vec![first.clone(), second.clone()]);
    assert_eq!(app.queue.current_index, 0);
    assert_eq!(play_calls(&calls).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_tracks_queued_while_the_rest_are_checked_stay_after_the_playlist() {
    common::isolate();
    let dir = temp_dir("play_first_append");
    let first = dir.join("a.mp3");
    let second = dir.join("b.mp3");
    let third = dir.join("c.mp3");
    for path in [&first, &second, &third] {
        touch(path);
    }
    write_playlist(vec![
        entry(&first, "Alpha", "One"),
        entry(&second, "Beta", "Two"),
        entry(&third, "Gamma", "Three"),
    ]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    let (checker, gate_tx, _) = checker_answering_first(vec![(second.clone(), true)]);
    app.playlists.set_exists_checker(checker);

    app.queue_selected_playlist(Some(1));
    assert!(poll_until(&mut app, |_| !play_calls(&calls).is_empty()));
    // Something else is queued (tab 2 `a`, the web dashboard) before the checks finish.
    app.queue.add_track(old_track(&dir));
    app.queue_cursor = 1;

    drop(gate_tx);
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(
        queue_paths(&app),
        vec![first.clone(), second.clone(), third.clone(), dir.join("old.mp3")]
    );
    assert_eq!(app.queue.current_index, 1);
    assert_eq!(app.queue_cursor, 3, "the cursor stays on the row it was on");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_rest_of_the_playlist_is_dropped_when_the_queue_is_cleared_meanwhile() {
    common::isolate();
    let dir = temp_dir("play_first_cleared");
    let first = dir.join("a.mp3");
    let second = dir.join("b.mp3");
    touch(&first);
    touch(&second);
    write_playlist(vec![entry(&first, "Alpha", "One"), entry(&second, "Beta", "Two")]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);
    let (checker, gate_tx, _) = checker_answering_first(vec![(first.clone(), true)]);
    app.playlists.set_exists_checker(checker);

    app.queue_selected_playlist(Some(0));
    assert!(poll_until(&mut app, |_| !play_calls(&calls).is_empty()));
    app.active_tab = 1;
    app.queue_focus = true;
    press(&mut app, crossterm::event::KeyCode::Char('c'));
    assert!(app.queue.is_empty());

    drop(gate_tx);
    assert!(poll_until(&mut app, |app| !app.playlists.queue_check_pending()));
    poll_for(&mut app, Duration::from_millis(100));
    assert!(app.queue.is_empty(), "queue: {:?}", queue_paths(&app));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_enter_on_a_link_to_an_audio_file_starts_on_its_target() {
    common::isolate();
    // A hand-edited playlist may list a path without an audio extension that links to an
    // audio file. The queue holds the checked target, so playback starts on it, and the rest of
    // the playlist goes around it rather than duplicating it.
    let dir = temp_dir("play_first_link");
    let first = dir.join("a.mp3");
    let target = dir.join("b.mp3");
    let link = dir.join("note.txt");
    let third = dir.join("c.mp3");
    for path in [&first, &target, &third] {
        touch(path);
    }
    std::os::unix::fs::symlink(&target, &link).unwrap();
    write_playlist(vec![
        entry(&first, "Alpha", "One"),
        entry(&link, "Beta", "Two"),
        entry(&third, "Gamma", "Three"),
    ]);
    let mut app = app_with_playlist(&dir);
    let calls = install_recording_player(&mut app);

    app.queue_selected_playlist(Some(1));
    assert!(poll_until(&mut app, |app| app.status_message.starts_with("Queued")));
    assert_eq!(queue_paths(&app), vec![first.clone(), target.clone(), third.clone()]);
    assert_eq!(app.queue.current_index, 1);
    assert_eq!(play_calls(&calls), vec![format!("play {}", target.display())]);
    let _ = std::fs::remove_dir_all(&dir);
}
