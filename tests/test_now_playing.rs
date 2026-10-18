mod common;

use std::fs;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

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
