use std::fs;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

#[test]
fn test_now_playing_tab_key_navigation() {
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

    // Press 'Esc': return to Queue & Library (tab 1)
    app.handle_tab_key(KeyCode::Esc);
    assert_eq!(app.active_tab, 1);

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

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_render_now_playing_tab_empty_and_populated() {
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
    assert!(view.contains("No Track Loaded"), "Should render No Track Loaded when queue is empty");

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
    assert!(view2.contains("UP NEXT IN QUEUE"), "Up Next section should appear in render");
    assert!(view2.contains("Supermassive Black Hole"), "Next track title should appear in render");

    let _ = fs::remove_dir_all(&temp_dir);
}
