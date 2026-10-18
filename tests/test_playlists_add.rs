//! Adding to playlists with `L`: selection rules, the add picker, folder adds and
//! `last_playlist` (issue #37).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quickstream::metadata::AudioMetadata;
use quickstream::player::AudioPlayer;
use quickstream::settings::{Settings, SettingsManager};
use quickstream::ui::playlists::{list_folder_audio_files, NamePurpose, PendingAdd, PlaylistModal};
use quickstream::ui::AppUI;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Fresh scratch folder for one test, removed first if a previous run left it behind.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quickstream_playlists_add_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// App browsing `dir`, with no playlists on disk. Tests in this binary share one isolated
/// config dir, so each one starts by removing the playlists folder.
fn new_app(dir: &Path) -> AppUI {
    let _ = fs::remove_dir_all(quickstream::paths::get_playlists_dir());
    let mut settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    // No background tag reads: the tests seed the metadata cache themselves.
    settings_mgr.settings.metadata_view = false;
    AppUI::new(Some(dir.to_path_buf()), settings_mgr)
}

/// Poll the playlist channels until `done` holds (about 2 s at most).
fn wait_for(app: &mut AppUI, what: &str, done: impl Fn(&AppUI) -> bool) {
    for _ in 0..200 {
        app.poll_playlists();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", what);
}

fn wait_loaded(app: &mut AppUI) {
    wait_for(app, "playlists to load", |a| a.playlists.store.is_loaded());
}

/// Wait for the browser's directory scan to finish.
fn wait_browser_listed(app: &mut AppUI) {
    for _ in 0..200 {
        app.poll_background_dir_scan();
        if !app.browser_loading {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("browser listing never finished");
}

/// Put the browser cursor on the row for `path` once the listing is complete.
fn select_browser_row(app: &mut AppUI, path: &Path) {
    wait_browser_listed(app);
    app.browser_cursor = app
        .browser_entries
        .iter()
        .position(|(_, p, _)| p == path)
        .unwrap_or_else(|| panic!("{} is not in the browser", path.display()));
}

/// Send a key the way a terminal does: uppercase letters carry SHIFT.
fn press(app: &mut AppUI, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(c) if c.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    app.handle_input_key(KeyEvent::new(code, modifiers)).unwrap();
}

fn type_text(app: &mut AppUI, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// Stands in for mpv so `L` outside tab 2 has a now-playing track.
struct StubPlayer {
    playing: bool,
}

impl AudioPlayer for StubPlayer {
    fn play(&mut self, _filepath: &Path) -> anyhow::Result<()> {
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
    fn seek(&mut self, _delta_seconds: f64) -> anyhow::Result<()> {
        Ok(())
    }
    fn seek_to(&mut self, _position_seconds: f64) -> anyhow::Result<()> {
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

#[test]
fn test_last_playlist_defaults_to_none_and_round_trips() {
    common::isolate();
    assert_eq!(Settings::default().last_playlist, None);
    let legacy: Settings = serde_json::from_str(
        r#"{"metadata_view":true,"default_device":"local-audio","seek_step_seconds":10.0,"volume_step":5,"queue_mode":"loop","force_transcode":false,"player_backend":"mpv"}"#,
    )
    .unwrap();
    assert_eq!(
        legacy.last_playlist, None,
        "configs saved before playlists existed load with no last playlist"
    );

    let dir = scratch_dir("settings");
    let path = dir.join("config.json");
    let mut mgr = SettingsManager::with_path(path.clone());
    mgr.settings.last_playlist = Some("Road Trip".to_string());
    assert!(mgr.save());
    let loaded = SettingsManager::load_from(&path).expect("settings load");
    assert_eq!(loaded.last_playlist.as_deref(), Some("Road Trip"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_list_folder_audio_files_is_flat_sorted_and_audio_only() {
    common::isolate();
    let dir = scratch_dir("listing");
    for name in [
        "02 Second.flac",
        "01 First.mp3",
        "10 Tenth.WAV",
        "cover.jpg",
        "notes.txt",
        "._01 First.mp3",
    ] {
        fs::write(dir.join(name), b"").unwrap();
    }
    fs::create_dir_all(dir.join("Disc 2")).unwrap();
    fs::write(dir.join("Disc 2").join("01 Nested.flac"), b"").unwrap();

    let files = list_folder_audio_files(&dir).expect("folder is readable");
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // Name order, audio only, no AppleDouble "._" files, nothing from subfolders.
    assert_eq!(names, vec!["01 First.mp3", "02 Second.flac", "10 Tenth.WAV"]);
    assert!(files.iter().all(|p| p.parent() == Some(dir.as_path())));
    assert!(list_folder_audio_files(&dir.join("missing")).is_err());
    let _ = fs::remove_dir_all(&dir);
}

/// A folder named like a track (`Live.flac`) is not a track; a symlink to a real file is.
#[test]
fn test_list_folder_audio_files_skips_directories_with_audio_names() {
    common::isolate();
    let dir = scratch_dir("listing_dirs");
    fs::write(dir.join("01 Real.flac"), b"").unwrap();
    fs::create_dir_all(dir.join("Live.flac")).unwrap();
    fs::write(dir.join("Live.flac").join("inner.flac"), b"").unwrap();
    std::os::unix::fs::symlink(dir.join("01 Real.flac"), dir.join("02 Link.flac")).unwrap();
    std::os::unix::fs::symlink(dir.join("Live.flac"), dir.join("03 DirLink.flac")).unwrap();

    let files = list_folder_audio_files(&dir).expect("folder is readable");
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["01 Real.flac", "02 Link.flac"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_refused_while_loading_and_on_header_rows() {
    common::isolate();
    let dir = scratch_dir("headers");
    let mut app = new_app(&dir);

    // The startup load result is applied by poll_playlists, which has not run yet.
    press(&mut app, KeyCode::Char('L'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Playlists are still loading");

    wait_loaded(&mut app);
    wait_browser_listed(&mut app);
    assert_eq!(app.browser_entries[0].0, "★ Music Homes");
    assert!(app.browser_entries[1].0.starts_with(".."));
    for row in [0, 1] {
        app.status_message.clear();
        app.browser_cursor = row;
        press(&mut app, KeyCode::Char('L'));
        assert!(app.playlists.modal.is_none(), "row {} is not addable", row);
        assert_eq!(app.status_message, "Select a track or folder");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_on_browser_track_adds_via_picker_and_remembers_playlist() {
    common::isolate();
    let dir = scratch_dir("track");
    let track = dir.join("Kerala.flac");
    fs::write(&track, b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Alpha").unwrap();
    app.playlists.store.create("Road Trip").unwrap();
    app.metadata_cache.insert(AudioMetadata {
        filepath: track.clone(),
        filename: "Kerala.flac".to_string(),
        title: "Kerala".to_string(),
        artist: "Bonobo".to_string(),
        duration_seconds: 245.4,
        ..Default::default()
    });
    select_browser_row(&mut app, &track);

    press(&mut app, KeyCode::Char('L'));
    match &app.playlists.modal {
        Some(PlaylistModal::AddPicker {
            pending: PendingAdd::Entries { label, entries },
            cursor,
        }) => {
            assert_eq!(label, "Kerala");
            assert_eq!(*cursor, 0, "no last playlist yet: first row");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, track);
            assert_eq!(entries[0].artist, "Bonobo", "entry comes from the metadata cache");
            assert_eq!(entries[0].duration_secs, 245);
        }
        _ => panic!("L on a track should open the add picker"),
    }
    press(&mut app, KeyCode::Down); // Road Trip
    press(&mut app, KeyCode::Enter);
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Added \"Kerala\" to Road Trip (1 track)");
    let road_trip = app.playlists.store.get("Road Trip").unwrap();
    assert_eq!(road_trip.entries.len(), 1);
    assert_eq!(road_trip.entries[0].path, track);
    assert!(app.playlists.store.get("Alpha").unwrap().entries.is_empty());
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Road Trip"));
    assert!(app.persistence.flush());
    let saved = SettingsManager::load_from(&dir.join("config.json")).expect("settings saved");
    assert_eq!(saved.last_playlist.as_deref(), Some("Road Trip"));

    // Next time the picker starts on Road Trip; adding the same track again changes nothing.
    press(&mut app, KeyCode::Char('L'));
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::AddPicker { cursor: 1, .. })
    ));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.status_message, "Already in Road Trip");
    assert_eq!(app.playlists.store.get("Road Trip").unwrap().entries.len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_picker_wraps_cancels_and_falls_back_when_last_playlist_is_gone() {
    common::isolate();
    let dir = scratch_dir("picker");
    let track = dir.join("Kerala.flac");
    fs::write(&track, b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Alpha").unwrap();
    app.playlists.store.create("Road Trip").unwrap();
    select_browser_row(&mut app, &track);

    // last_playlist matches case-insensitively, like every playlist name.
    app.settings_mgr.settings.last_playlist = Some("road trip".to_string());
    press(&mut app, KeyCode::Char('L'));
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::AddPicker { cursor: 1, .. })
    ));
    press(&mut app, KeyCode::Esc);
    assert!(app.playlists.modal.is_none());

    // A last playlist that no longer exists highlights the first row.
    app.settings_mgr.settings.last_playlist = Some("Deleted Mix".to_string());
    press(&mut app, KeyCode::Char('L'));
    let cursor = |app: &AppUI| match &app.playlists.modal {
        Some(PlaylistModal::AddPicker { cursor, .. }) => *cursor,
        _ => panic!("add picker should be open"),
    };
    assert_eq!(cursor(&app), 0);
    // Rows: Alpha, Road Trip, "+ New playlist…"; movement wraps both ways.
    press(&mut app, KeyCode::Up);
    assert_eq!(cursor(&app), 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(cursor(&app), 0);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(cursor(&app), 2);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(cursor(&app), 0);
    press(&mut app, KeyCode::Char('q'));
    assert!(app.playlists.modal.is_none());

    assert!(app.playlists.store.playlists().iter().all(|p| p.entries.is_empty()));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_new_playlist_row_creates_the_playlist_and_adds() {
    common::isolate();
    let dir = scratch_dir("create_add");
    let track = dir.join("Kerala.flac");
    fs::write(&track, b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Alpha").unwrap();
    app.playlists.store.create("Zulu").unwrap();
    select_browser_row(&mut app, &track);

    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Up); // wraps to "+ New playlist…"
    press(&mut app, KeyCode::Enter);
    assert!(matches!(
        app.playlists.modal,
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::CreateAndAdd(_),
            ..
        })
    ));
    type_text(&mut app, "Road Trip");
    press(&mut app, KeyCode::Enter);

    assert!(app.playlists.modal.is_none());
    let road_trip = app.playlists.store.get("Road Trip").expect("playlist created");
    assert_eq!(road_trip.entries.len(), 1);
    // No cached tags: the title falls back to the file stem.
    assert_eq!(road_trip.entries[0].title, "Kerala");
    assert_eq!(road_trip.entries[0].duration_secs, -1);
    assert_eq!(app.status_message, "Added \"Kerala\" to Road Trip (1 track)");
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Road Trip"));
    // The tab 5 list cursor follows the new playlist to its sorted position.
    assert_eq!(app.playlists.list_cursor, 1);

    // A duplicate name keeps the prompt open with the error and the pending track.
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down); // Road Trip (1) -> Zulu (2) -> "+ New playlist…" (3)
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "road trip");
    press(&mut app, KeyCode::Enter);
    match &app.playlists.modal {
        Some(PlaylistModal::NamePrompt {
            purpose: NamePurpose::CreateAndAdd(PendingAdd::Entries { entries, .. }),
            error: Some(error),
            ..
        }) => {
            assert!(error.contains("already exists"), "{}", error);
            assert_eq!(entries.len(), 1);
        }
        _ => panic!("duplicate name should keep the prompt open with an error"),
    }
    press(&mut app, KeyCode::Esc);
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.playlists.store.playlists().len(), 3);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_on_folder_adds_its_audio_files_in_name_order() {
    common::isolate();
    let dir = scratch_dir("folder");
    let album = dir.join("Album");
    fs::create_dir_all(album.join("Bonus")).unwrap();
    for name in ["02 Paranoid Android.flac", "01 Airbag.flac", "folder.jpg"] {
        fs::write(album.join(name), b"").unwrap();
    }
    fs::write(album.join("Bonus").join("03 Nested.flac"), b"").unwrap();
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();
    select_browser_row(&mut app, &album);

    press(&mut app, KeyCode::Char('L'));
    match &app.playlists.modal {
        Some(PlaylistModal::AddPicker {
            pending: PendingAdd::Folder { label, dir: folder },
            ..
        }) => {
            assert_eq!(label, "Album (folder)");
            assert_eq!(folder, &album);
        }
        _ => panic!("L on a folder should open the add picker for the folder"),
    }
    press(&mut app, KeyCode::Enter);
    assert!(app.playlists.modal.is_none());
    // The listing runs on a worker; nothing is added until poll_playlists sees the result.
    assert_eq!(app.status_message, "Reading Album…");
    wait_for(&mut app, "folder add", |a| {
        a.playlists.store.get("Road Trip").is_some_and(|p| p.entries.len() == 2)
    });
    let paths: Vec<PathBuf> = app
        .playlists
        .store
        .get("Road Trip")
        .unwrap()
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![album.join("01 Airbag.flac"), album.join("02 Paranoid Android.flac")]
    );
    assert_eq!(app.status_message, "Added 2 to Road Trip (2 tracks)");
    assert_eq!(app.settings_mgr.settings.last_playlist.as_deref(), Some("Road Trip"));

    // Adding the folder again after a new file appears adds only the new file.
    fs::write(album.join("00 Intro.flac"), b"").unwrap();
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "second folder add", |a| a.status_message.contains("skipped"));
    assert_eq!(app.status_message, "Added 1 to Road Trip, skipped 2 already in it");
    assert_eq!(app.playlists.store.get("Road Trip").unwrap().entries.len(), 3);

    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "third folder add", |a| {
        a.status_message == "Already in Road Trip"
    });
    let _ = fs::remove_dir_all(&dir);
}

/// A 2-second silent PCM WAV, enough for the tag reader to report a duration.
fn write_two_second_wav(path: &Path) {
    let sample_rate: u32 = 44100;
    let channels: u16 = 2;
    let bits: u16 = 16;
    let block_align = channels * (bits / 8);
    let data_size = sample_rate * 2 * block_align as u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    bytes.resize(bytes.len() + data_size as usize, 0);
    fs::write(path, bytes).unwrap();
}

/// Files in a folder that was never shown in the browser have no cached tags. The folder add
/// worker reads them, so the playlist stores real durations and the cache is filled.
#[test]
fn test_folder_add_reads_tags_for_files_not_in_the_cache() {
    common::isolate();
    let dir = scratch_dir("folder_tags");
    let album = dir.join("Album");
    fs::create_dir_all(&album).unwrap();
    let track = album.join("01 Intro.wav");
    write_two_second_wav(&track);
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();
    select_browser_row(&mut app, &album);
    assert!(app.metadata_cache.get(&track).is_none());

    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "folder add", |a| {
        a.playlists.store.get("Road Trip").is_some_and(|p| p.entries.len() == 1)
    });

    let entry = app.playlists.store.get("Road Trip").unwrap().entries[0].clone();
    assert_eq!(entry.path, track);
    assert_eq!(entry.duration_secs, 2);
    assert!(app.metadata_cache.get(&track).is_some());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_folder_that_cannot_be_read_reports_it() {
    common::isolate();
    let dir = scratch_dir("unreadable");
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();

    // The folder was listed in the browser, then the share went away.
    let gone = dir.join("Unplugged");
    app.playlists.modal = Some(PlaylistModal::AddPicker {
        pending: PendingAdd::Folder {
            label: "Unplugged (folder)".to_string(),
            dir: gone,
        },
        cursor: 0,
    });
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, "folder read error", |a| {
        a.status_message == "Couldn't read Unplugged"
    });
    assert!(app.playlists.store.get("Road Trip").unwrap().entries.is_empty());
    assert_eq!(app.settings_mgr.settings.last_playlist, None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_key_uses_queue_row_and_now_playing_track() {
    common::isolate();
    let dir = scratch_dir("queue_now_playing");
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();

    app.queue_focus = true;
    press(&mut app, KeyCode::Char('L'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Queue is empty");

    let airbag = AudioMetadata {
        filepath: dir.join("01 Airbag.flac"),
        filename: "01 Airbag.flac".to_string(),
        title: "Airbag".to_string(),
        artist: "Radiohead".to_string(),
        duration_seconds: 284.4,
        ..Default::default()
    };
    let lucky = AudioMetadata {
        filepath: dir.join("11 Lucky.flac"),
        filename: "11 Lucky.flac".to_string(),
        title: "Lucky".to_string(),
        artist: "Radiohead".to_string(),
        duration_seconds: 259.0,
        ..Default::default()
    };
    app.queue.add_track(airbag.clone());
    app.queue.add_track(lucky.clone());

    // Queue pane: the selected queue row, with its tags.
    app.queue_cursor = 0;
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.status_message, "Added \"Airbag\" to Road Trip (1 track)");
    let entry = app.playlists.store.get("Road Trip").unwrap().entries[0].clone();
    assert_eq!(entry.path, airbag.filepath);
    assert_eq!(entry.artist, "Radiohead");
    assert_eq!(entry.title, "Airbag");
    assert_eq!(entry.duration_secs, 284);

    // Any other tab: the now-playing track, and only while something is playing.
    app.active_tab = 0;
    app.queue.current_index = 1;
    press(&mut app, KeyCode::Char('L'));
    assert!(app.playlists.modal.is_none());
    assert_eq!(app.status_message, "Nothing is playing");

    app.player = Box::new(StubPlayer { playing: true });
    press(&mut app, KeyCode::Char('L'));
    match &app.playlists.modal {
        Some(PlaylistModal::AddPicker {
            pending: PendingAdd::Entries { label, .. },
            ..
        }) => assert_eq!(label, "Lucky"),
        _ => panic!("L while playing should open the add picker for the current track"),
    }
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.status_message, "Added \"Lucky\" to Road Trip (2 tracks)");
    assert_eq!(
        app.playlists.store.get("Road Trip").unwrap().entries[1].path,
        lucky.filepath
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_add_picker_renders_header_rows_and_new_playlist_row() {
    common::isolate();
    let dir = scratch_dir("render");
    let mut app = new_app(&dir);
    wait_loaded(&mut app);
    app.playlists.store.create("Road Trip").unwrap();
    app.queue.add_track(AudioMetadata {
        filepath: dir.join("01 Airbag.flac"),
        title: "Airbag".to_string(),
        artist: "Radiohead".to_string(),
        ..Default::default()
    });
    app.queue_focus = true;
    press(&mut app, KeyCode::Char('L'));

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let view = terminal.backend().to_string();
    assert!(view.contains("ADD TO PLAYLIST"), "{}", view);
    assert!(view.contains("Adding: Airbag"), "{}", view);
    assert!(view.contains("Road Trip  (0 tracks)"), "{}", view);
    assert!(view.contains("+ New playlist…"), "{}", view);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_help_modal_lists_add_to_playlist_key() {
    common::isolate();
    let dir = scratch_dir("help");
    let mut app = new_app(&dir);
    app.show_help_modal = true;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();
    let view = terminal.backend().to_string();
    assert!(view.contains("Add to playlist"), "help modal should list L:\n{}", view);
    let _ = fs::remove_dir_all(&dir);
}
