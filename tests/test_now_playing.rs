mod common;

use std::fs;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use quickstream::analysis::{MixProfile, MoodType};
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

#[test]
fn test_now_playing_tab_key_navigation() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_keys_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Initial tab is 1 (Queue & Library)
    assert_eq!(app.active_tab, 1);

    // Press '1': switch to Now Playing (tab 0)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);

    // Press 'n': toggle to Queue & Library (tab 1)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 1);

    // Press 'n' again: toggle back to Now Playing (tab 0)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);

    // Numeric keys 2, 3, 4
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 2); // Devices
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 3); // Settings
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 1); // Queue

    // Tab cycling
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.active_tab, 2);

    // Esc is the global quit key, also on Now Playing (#57)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);
    app.browser_loading = false;
    let quit = app
        .handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(quit, "Esc on Now Playing should quit");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_render_now_playing_tab_empty_and_populated() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_render_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Render empty Now Playing tab (active_tab = 0)
    app.active_tab = 0;
    terminal.draw(|f| app.render(f)).unwrap();

    let view = terminal.backend().to_string();
    assert!(view.contains("NOW PLAYING"), "Should render NOW PLAYING title in view");
    assert!(
        view.contains("No Track Loaded"),
        "Should render No Track Loaded when queue is empty"
    );

    // 2. Add tracks to queue and render populated hero layout
    let meta1 = AudioMetadata {
        filepath: temp_dir.join("song1.flac"),
        title: "Starlight".to_string(),
        artist: "Muse".to_string(),
        album: "Black Holes and Revelations".to_string(),
        duration_seconds: 240.0,
        format_name: "flac".to_string(),
        sample_rate_hz: 44100,
        bitrate_kbps: 1024,
        channels: 2,
        ..Default::default()
    };

    let meta2 = AudioMetadata {
        filepath: temp_dir.join("song2.flac"),
        filename: "song2.flac".to_string(),
        title: "Supermassive Black Hole".to_string(),
        artist: "Muse".to_string(),
        duration_seconds: 210.0,
        ..Default::default()
    };

    app.queue.add_track(meta1);
    app.queue.add_track(meta2);

    terminal.draw(|f| app.render(f)).unwrap();
    let view2 = terminal.backend().to_string();
    assert!(view2.contains("Starlight"), "Hero title should appear in render");
    assert!(view2.contains("Muse"), "Hero artist should appear in render");
    assert!(
        view2.contains("UP NEXT IN QUEUE"),
        "Up Next section should appear in render"
    );
    assert!(view2.contains("TITLE"), "Table header TITLE should appear in render");
    assert!(view2.contains("ARTIST"), "Table header ARTIST should appear in render");
    assert!(view2.contains("TIME"), "Table header TIME should appear in render");
    assert!(view2.contains("#02"), "Track index #02 should appear in table");
    assert!(
        view2.contains("Supermassive Black Hole"),
        "Next track title should appear in render"
    );

    // 3. Toggle to Filenames mode -> Up Next table shows #, FILE, TIME
    app.settings_mgr.settings.metadata_view = false;
    terminal.draw(|f| app.render(f)).unwrap();
    let view3 = terminal.backend().to_string();
    assert!(
        view3.contains("FILE"),
        "Table header FILE should appear in filename mode"
    );
    assert!(
        view3.contains("TIME"),
        "Table header TIME should appear in filename mode"
    );
    assert!(view3.contains("song2.flac"), "Filename should appear in table");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// The Now Playing key row (the row holding the `N` keycap) at `width` x 30.
fn now_playing_footer(app: &mut AppUI, width: u16) -> String {
    app.active_tab = 0;
    let buffer = common::tui::render(app, width, 30);
    let y = common::tui::find_row(&buffer, " N  QUEUE").unwrap_or_else(|| {
        panic!(
            "Now Playing key row at {width} columns:\n{}",
            common::tui::rows(&buffer).join("\n")
        )
    });
    common::tui::row(&buffer, y)
}

#[test]
fn test_now_playing_footer_names_n_not_esc_at_80_columns() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_footer_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Esc is the global quit key, so the key row must not offer it for switching tabs (#57),
    // and `N QUEUE` stays at every width.
    for width in [120, 100, 85, 80, 70, 40] {
        let footer = now_playing_footer(&mut app, width);
        assert!(footer.contains(" N  QUEUE"), "key row at {width} columns: {footer}");
        assert!(footer.contains(" ?  KEYS"), "key row at {width} columns: {footer}");
        assert!(
            !footer.to_uppercase().contains("ESC"),
            "key row at {width} columns: {footer}"
        );
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_now_playing_footer_shows_the_seek_step_setting() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_np_seek_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    app.settings_mgr.settings.seek_step_seconds = 30.0;
    let footer = now_playing_footer(&mut app, 120);
    assert!(footer.contains(" [ ]  SEEK 30S"), "footer: {footer}");
    let footer = now_playing_footer(&mut app, 100);
    assert!(footer.contains("SEEK 30S"), "footer: {footer}");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// A fresh app browsing a temp dir of its own, with `titles` queued (5:00, 5:52, ...).
fn hero_app(tag: &str, titles: &[&str]) -> (AppUI, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("quickstream_np_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let settings_mgr = SettingsManager::with_path(dir.join("config.json"));
    let mut app = AppUI::new(Some(dir.clone()), settings_mgr);
    for (i, title) in titles.iter().enumerate() {
        app.queue.add_track(AudioMetadata {
            filepath: dir.join(format!("{title}.flac")),
            title: title.to_string(),
            artist: "Mira Lenz".to_string(),
            duration_seconds: 300.0 + 52.0 * i as f64,
            ..Default::default()
        });
    }
    app.active_tab = 0;
    (app, dir)
}

/// The hero timeline: the row with the bar (`█` or `░`).
fn timeline_row(rows: &[String]) -> String {
    rows.iter()
        .find(|r| r.contains('░') || r.contains('█'))
        .cloned()
        .unwrap_or_else(|| panic!("no timeline:\n{}", rows.join("\n")))
}

#[test]
fn test_now_playing_shows_a_pending_skip_as_queued_from_its_start() {
    common::isolate();
    let (mut app, dir) = hero_app("skip", &["One", "Two"]);
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(125.0, 300.0));

    let rows = common::tui::rows(&common::tui::render(&mut app, 120, 40));
    let screen = rows.join("\n");
    assert!(screen.contains("▶ PLAYING"), "{screen}");
    assert!(timeline_row(&rows).contains("02:05"), "{screen}");

    // Right was pressed: the queue moved to Two, the player still plays One until the debounce
    // fires. The hero shows Two from its start, marked queued, as the strip does.
    app.queue.current_index = 1;
    app.pending_track_skip = true;
    let rows = common::tui::rows(&common::tui::render(&mut app, 120, 40));
    let screen = rows.join("\n");
    assert!(screen.contains("» QUEUED"), "{screen}");
    assert!(
        !screen.contains("▶ PLAYING"),
        "the pill no longer says PLAYING: {screen}"
    );
    assert!(screen.contains("Two"), "{screen}");
    let timeline = timeline_row(&rows);
    assert!(
        timeline.contains("00:00") && timeline.contains("05:52"),
        "Two from 00:00 of 05:52: {timeline:?}"
    );
    assert!(!screen.contains("02:05"), "One's position is gone: {screen}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_pending_skip_never_shows_the_old_tracks_duration() {
    common::isolate();
    // Two's duration isn't known yet (e.g. added from the browser before its tags were read).
    let (mut app, dir) = hero_app("skip_nodur", &["One", "Two"]);
    app.queue.tracks[1].duration_seconds = 0.0;
    app.player = Box::new(common::fake_player::FakePlayer::playing_at(125.0, 300.0));

    // Right was pressed: the player still holds One (5:00) until the debounce fires.
    app.queue.current_index = 1;
    app.pending_track_skip = true;
    let rows = common::tui::rows(&common::tui::render(&mut app, 120, 40));
    let screen = rows.join("\n");
    assert!(screen.contains("» QUEUED"), "{screen}");
    let timeline = timeline_row(&rows);
    assert!(timeline.contains("00:00"), "Two from 00:00: {timeline:?}");
    assert!(
        !screen.contains("05:00"),
        "One's duration is not shown for Two: {screen}"
    );

    // The strip on tab 2 shows the same: Two from 00:00, no 05:00.
    app.active_tab = 1;
    let buffer = common::tui::render(&mut app, 120, 40);
    let y = common::tui::find_row(&buffer, "QUEUED").expect("strip");
    let strip = common::tui::row(&buffer, y);
    assert!(strip.contains("00:00") && !strip.contains("05:00"), "{strip:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_now_playing_autopilot_badge_names_the_mix_profile() {
    common::isolate();
    let (mut app, dir) = hero_app("profile", &["One", "Two"]);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(!screen.contains("AUTOPILOT"), "no badge without autopilot: {screen}");

    app.autopilot_dir = Some(dir.clone());
    for (profile, name) in [
        (MixProfile::DiverseShuffle, "AUTOPILOT · DIVERSE SHUFFLE"),
        (
            MixProfile::BpmFlow {
                target_bpm: 120.0,
                tolerance: 0.15,
            },
            "AUTOPILOT · BPM FLOW",
        ),
        (MixProfile::MoodVibe(MoodType::Chill), "AUTOPILOT · CHILL & AMBIENT"),
        (MixProfile::MoodVibe(MoodType::Workout), "AUTOPILOT · WORKOUT"),
        (MixProfile::MoodVibe(MoodType::Groove), "AUTOPILOT · LATE NIGHT GROOVE"),
        (MixProfile::SeedRadio(dir.join("One.flac")), "AUTOPILOT · SEED RADIO"),
    ] {
        app.active_mix_profile = profile;
        let screen = common::tui::render_text(&mut app, 120, 40);
        assert!(screen.contains(name), "{name:?} missing:\n{screen}");
    }
    let _ = fs::remove_dir_all(&dir);
}
