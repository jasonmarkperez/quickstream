use std::fs::{self, File};
use std::io::Write;

use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

fn create_dummy_wav(path: &std::path::Path) {
    let mut file = File::create(path).unwrap();
    let sample_rate: u32 = 44100;
    let channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * ((bits_per_sample as u32) / 8);
    let block_align = channels * (bits_per_sample / 8);
    let num_samples: u32 = 44100; // 1 second
    let data_size = num_samples * (block_align as u32);

    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();
    file.write_all(&vec![0u8; data_size as usize]).unwrap();
}

#[test]
fn test_browser_metadata_view_toggle() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_browser_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let song1 = temp_dir.join("test_track.wav");
    create_dummy_wav(&song1);

    let config_path = temp_dir.join("config.json");
    let mut settings_mgr = SettingsManager::with_path(config_path);
    settings_mgr.settings.metadata_view = true;

    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Verify song1 is found in browser_entries
    let label_meta = {
        let entry = app.browser_entries.iter().find(|(_, p, is_dir)| !*is_dir && p == &song1);
        assert!(entry.is_some(), "WAV track should be in browser entries");
        entry.unwrap().0.clone()
    };

    // With metadata_view = true, label contains duration "(01:00)"
    assert!(
        label_meta.contains("01:00") || label_meta.contains("00:01"),
        "Metadata label should contain duration: {}",
        label_meta
    );
    assert!(label_meta.contains("[WAV]"), "Metadata label should contain format badge: {}", label_meta);

    // Now toggle metadata_view to false
    app.settings_mgr.settings.metadata_view = false;
    app.refresh_browser();

    let entry_file = app.browser_entries.iter().find(|(_, p, is_dir)| !*is_dir && p == &song1);
    assert!(entry_file.is_some());
    let (label_filename, _, _) = entry_file.unwrap();

    // With metadata_view = false, label displays clean filename
    assert_eq!(label_filename, "🎵 [WAV] test_track.wav");
    assert_ne!(label_meta.as_str(), label_filename.as_str());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_scrolling_beyond_viewport() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_scroll_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // Create 40 dummy tracks so list is much taller than terminal viewport
    for i in 0..40 {
        let f = temp_dir.join(format!("track_{:02}.wav", i));
        create_dummy_wav(&f);
    }

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let total_entries = app.browser_entries.len();
    assert!(total_entries >= 40, "Should have at least 40 entries, found {}", total_entries);

    // Set up a mock terminal with height 20 (viewport fits ~12 list items)
    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal.draw(|f| app.render(f)).unwrap();
    assert_eq!(app.browser_cursor, 0);
    assert_eq!(app.browser_state.selected(), Some(0));
    assert_eq!(app.browser_state.offset(), 0);

    // Move cursor down 25 times
    for _ in 0..25 {
        app.handle_tab_key(crossterm::event::KeyCode::Down);
    }
    assert_eq!(app.browser_cursor, 25);

    // Render frame: ListState offset must advance beyond 0 so item 25 is visible in viewport
    terminal.draw(|f| app.render(f)).unwrap();
    assert_eq!(app.browser_state.selected(), Some(25));
    assert!(
        app.browser_state.offset() > 0,
        "ListState offset ({}) must advance beyond 0 to scroll item 25 into view",
        app.browser_state.offset()
    );

    // Test wraparound: Up at index 0 wraps to last item
    app.browser_cursor = 0;
    app.handle_tab_key(crossterm::event::KeyCode::Up);
    assert_eq!(app.browser_cursor, total_entries - 1);

    // Down at last item wraps to 0
    app.handle_tab_key(crossterm::event::KeyCode::Down);
    assert_eq!(app.browser_cursor, 0);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_paging_and_jump_shortcuts() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_page_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // Create 50 dummy tracks
    for i in 0..50 {
        let f = temp_dir.join(format!("track_{:02}.wav", i));
        create_dummy_wav(&f);
    }

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    app.browser_page_height = 10;

    let total_entries = app.browser_entries.len();
    assert!(total_entries >= 50, "Total entries should be at least 50");
    let last_idx = total_entries - 1;

    assert_eq!(app.browser_cursor, 0);

    // 1. PageDown key jumps by page height (10)
    app.handle_tab_key(crossterm::event::KeyCode::PageDown);
    assert_eq!(app.browser_cursor, 10);

    // 2. Ctrl+D jumps by page height (10 -> 20)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('d'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(app.browser_cursor, 20);

    // 3. Shift+J (uppercase J) jumps by page height (20 -> 30)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('J'),
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.browser_cursor, 30);

    // 4. Ctrl+F jumps by page height (30 -> 40)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('f'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(app.browser_cursor, 40);

    // 5. End / G jumps directly to last item
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('G'),
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.browser_cursor, last_idx);

    // 6. PageDown at last item wraps to 0
    app.handle_tab_key(crossterm::event::KeyCode::PageDown);
    assert_eq!(app.browser_cursor, 0);

    // 7. PageUp at 0 wraps to last item
    app.handle_tab_key(crossterm::event::KeyCode::PageUp);
    assert_eq!(app.browser_cursor, last_idx);

    // 8. Ctrl+U jumps up by page height
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(app.browser_cursor, last_idx - 10);

    // 9. Shift+K jumps up by page height
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('K'),
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.browser_cursor, last_idx - 20);

    // 10. Home / g jumps directly to index 0
    app.handle_tab_key(crossterm::event::KeyCode::Home);
    assert_eq!(app.browser_cursor, 0);

    let _ = fs::remove_dir_all(&temp_dir);
}

