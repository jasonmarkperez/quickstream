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

    // Wait for background metadata worker to extract tags and poll
    for _ in 0..50 {
        app.poll_background_metadata();
        if app
            .browser_entries
            .iter()
            .any(|(l, p, is_dir)| !*is_dir && p == &song1 && (l.contains("01:00") || l.contains("00:01")))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    app.poll_background_metadata();

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
fn test_browser_non_blocking_navigation_large_directory() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_fast_nav_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let music_dir = temp_dir.join("Music");
    fs::create_dir_all(&music_dir).unwrap();

    // Create 30 subfolders
    for i in 0..30 {
        fs::create_dir_all(music_dir.join(format!("Artist_{:02}", i))).unwrap();
    }
    // Create 30 audio tracks
    for i in 0..30 {
        create_dummy_wav(&music_dir.join(format!("track_{:02}.wav", i)));
    }

    let config_path = temp_dir.join("config.json");
    let mut settings_mgr = SettingsManager::with_path(config_path);
    settings_mgr.settings.metadata_view = true;

    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let start = std::time::Instant::now();
    app.navigate_browser(music_dir.clone());
    let nav_elapsed = start.elapsed();

    // Navigation must be instantaneous (under 200ms) with zero UI freeze
    assert!(
        nav_elapsed < std::time::Duration::from_millis(200),
        "Navigation took too long: {:?}",
        nav_elapsed
    );

    // All 30 folders and 30 tracks + Homes + Parent must be immediately present
    assert_eq!(app.browser_entries.len(), 62);

    // Initial labels for uncached tracks should be fast filenames
    let first_track = app.browser_entries.iter().find(|(_, p, is_dir)| !*is_dir && p.ends_with("track_00.wav"));
    assert!(first_track.is_some());

    // Allow background worker to populate metadata
    for _ in 0..50 {
        app.poll_background_metadata();
        if app.browser_entries.iter().any(|(l, p, is_dir)| !*is_dir && p.ends_with("track_00.wav") && (l.contains("00:01") || l.contains("01:00"))) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    app.poll_background_metadata();

    let updated_track = app.browser_entries.iter().find(|(_, p, is_dir)| !*is_dir && p.ends_with("track_00.wav")).unwrap();
    assert!(
        updated_track.0.contains("00:01") || updated_track.0.contains("01:00"),
        "Track label should be updated by background worker: {}",
        updated_track.0
    );

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

    // 11. MacBook friendly: Shift + Down jumps down by page height (10)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.browser_cursor, 10);

    // 12. MacBook friendly: ']' bracket key jumps down by page height (10 -> 20)
    app.handle_tab_key(crossterm::event::KeyCode::Char(']'));
    assert_eq!(app.browser_cursor, 20);

    // 13. MacBook friendly: '[' bracket key jumps up by page height (20 -> 10)
    app.handle_tab_key(crossterm::event::KeyCode::Char('['));
    assert_eq!(app.browser_cursor, 10);

    // 14. MacBook friendly: Shift + Up jumps up by page height (10 -> 0)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.browser_cursor, 0);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_navigate_into_folder_resets_cursor_and_offset() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_nav_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // 1. Create 40 subfolders in parent dir
    for i in 0..40 {
        let sub = temp_dir.join(format!("folder_{:02}", i));
        fs::create_dir_all(&sub).unwrap();
    }

    // 2. In folder_35, create 15 child subfolders
    let target_sub = temp_dir.join("folder_35");
    for j in 0..15 {
        let child = target_sub.join(format!("child_{:02}", j));
        fs::create_dir_all(&child).unwrap();
    }

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    // Initial render
    terminal.draw(|f| app.render(f)).unwrap();
    assert_eq!(app.browser_cursor, 0);
    assert_eq!(app.browser_state.offset(), 0);

    // Find index of folder_35 in browser entries
    let folder_35_idx = app
        .browser_entries
        .iter()
        .position(|(_name, p, is_dir)| *is_dir && p == &target_sub)
        .expect("folder_35 should exist");
    assert!(folder_35_idx >= 30, "folder_35 index should be >= 30");

    // Move cursor down to folder_35
    for _ in 0..folder_35_idx {
        app.handle_tab_key(crossterm::event::KeyCode::Down);
    }
    assert_eq!(app.browser_cursor, folder_35_idx);

    // Draw frame so ListState scrolls down and offset advances
    terminal.draw(|f| app.render(f)).unwrap();
    assert!(
        app.browser_state.offset() > 0,
        "Offset should have advanced when scrolling down to index {}",
        folder_35_idx
    );

    // Press Enter to navigate into folder_35
    app.handle_tab_key(crossterm::event::KeyCode::Enter);

    // VERIFY:
    // 1. Current directory changed
    assert_eq!(app.browser_dir, target_sub);

    // 2. Cursor MUST reset to 0 (top of the directory), NOT be clamped to the last element (16)
    assert_eq!(
        app.browser_cursor, 0,
        "Browser cursor must reset to 0 when navigating into a subfolder"
    );

    // 3. Render frame: offset must be 0 so the first entries are visible at the top
    terminal.draw(|f| app.render(f)).unwrap();
    assert_eq!(
        app.browser_state.offset(),
        0,
        "ListState offset must reset to 0 so the folder starts at the top, not at the last element"
    );

    // 4. Test navigating back up to parent directory:
    // Cursor is currently at 0 (Music Homes). Move down 1 to index 1: ".. [Parent Directory]"
    app.handle_tab_key(crossterm::event::KeyCode::Down);
    assert_eq!(app.browser_cursor, 1);
    assert_eq!(app.browser_entries[1].0, ".. [Parent Directory]");

    // Press Enter to go up to parent
    app.handle_tab_key(crossterm::event::KeyCode::Enter);
    assert_eq!(app.browser_dir, temp_dir);

    // When returning to parent, cursor should cleanly restore to folder_35
    assert_eq!(
        app.browser_cursor, folder_35_idx,
        "Going up to parent directory should restore cursor to the folder we just exited"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_spotlight_subfolder_jump() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_spotlight_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let folders = vec![
        "Ambient Works",
        "Audio Bullys",
        "Audioslave",
        "Austin Wintory",
        "Beatles",
        "Bonobo",
    ];
    for f in &folders {
        fs::create_dir_all(temp_dir.join(f)).unwrap();
    }

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let ambient_idx = app.browser_entries.iter().position(|(name, _, _)| name.contains("Ambient Works")).unwrap();
    let audio_bullys_idx = app.browser_entries.iter().position(|(name, _, _)| name.contains("Audio Bullys")).unwrap();
    let audioslave_idx = app.browser_entries.iter().position(|(name, _, _)| name.contains("Audioslave")).unwrap();
    let austin_wintory_idx = app.browser_entries.iter().position(|(name, _, _)| name.contains("Austin Wintory")).unwrap();
    let beatles_idx = app.browser_entries.iter().position(|(name, _, _)| name.contains("Beatles")).unwrap();

    // 1. Initially spotlight is not active
    assert!(app.browser_spotlight.is_none());

    // 2. Open spotlight
    app.open_spotlight();
    assert_eq!(app.browser_spotlight.as_deref(), Some(""));

    // 3. Type 'A': jumps to first 'A' folder (Ambient Works)
    app.browser_spotlight = Some("A".to_string());
    app.jump_spotlight_to_first_match();
    assert_eq!(app.browser_cursor, ambient_idx);

    // 4. Type 'u' (query is now "Au"): jumps to Audio Bullys
    app.browser_spotlight = Some("Au".to_string());
    app.jump_spotlight_to_first_match();
    assert_eq!(app.browser_cursor, audio_bullys_idx);

    // 5. Next match: jumps to Audioslave
    app.spotlight_next_match();
    assert_eq!(app.browser_cursor, audioslave_idx);

    // 6. Next match: jumps to Austin Wintory
    app.spotlight_next_match();
    assert_eq!(app.browser_cursor, austin_wintory_idx);

    // 7. Next match wraps around to Audio Bullys
    app.spotlight_next_match();
    assert_eq!(app.browser_cursor, audio_bullys_idx);

    // 8. Prev match wraps backwards to Austin Wintory
    app.spotlight_prev_match();
    assert_eq!(app.browser_cursor, austin_wintory_idx);

    // 9. Type "b": jumps to Beatles
    app.browser_spotlight = Some("b".to_string());
    app.jump_spotlight_to_first_match();
    assert_eq!(app.browser_cursor, beatles_idx);

    // 10. Verify render with active Spotlight does not panic and renders input bar
    let backend = ratatui::backend::TestBackend::new(80, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();

    // 11. Close spotlight
    app.close_spotlight();
    assert!(app.browser_spotlight.is_none());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_bookmarks_instant_load_no_stat() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_bookmarks_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Set bookmarks that simulate remote or non-existent network mount paths
    app.bookmarks = vec![
        "/Volumes/Music".to_string(),
        "/Volumes/home/Drive/Mixes".to_string(),
        "/NonExistent/DeadShare".to_string(),
    ];

    // Navigate to Music Homes
    app.navigate_browser(std::path::PathBuf::from("___MUSIC_HOMES___"));

    assert!(app.in_bookmarks_view);
    assert_eq!(app.browser_entries.len(), 4); // Back + 3 bookmarks
    assert_eq!(app.browser_entries[0].0, ".. [Back to File System]");
    assert_eq!(app.browser_entries[1].0, "📁 Music/");
    assert_eq!(app.browser_entries[2].0, "📁 Mixes/");
    assert_eq!(app.browser_entries[3].0, "📁 DeadShare/");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_async_scanner_nonexistent_directory() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_nonexistent_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let nonexistent = temp_dir.join("nonexistent_subdir");
    app.navigate_browser(nonexistent.clone());

    // Wait for the background worker to finish and report the error
    for _ in 0..50 {
        app.poll_background_dir_scan();
        if !app.browser_loading {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(!app.browser_loading);
    assert!(app.status_message.contains("Error reading directory"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_cancellation_on_navigation() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_cancel_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let dir_a = temp_dir.join("DirA");
    let dir_b = temp_dir.join("DirB");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Navigate to DirA, starting scan
    app.navigate_browser(dir_a.clone());
    let stop_a = app.dir_scan_stop.clone().expect("Stop flag should be set for active scan");

    // Immediately navigate to DirB
    app.navigate_browser(dir_b.clone());

    // Old scan's stop flag must be signaled
    assert!(stop_a.load(std::sync::atomic::Ordering::Relaxed), "DirA scan should be cancelled");
    assert_eq!(app.browser_dir, dir_b);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_cache_instant_back_navigation() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_cache_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let parent_dir = temp_dir.join("Music");
    let sub_dir = parent_dir.join("ArtistA");
    fs::create_dir_all(&sub_dir).unwrap();

    create_dummy_wav(&sub_dir.join("track1.wav"));

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(parent_dir.clone()), settings_mgr);

    // Initial load of parent_dir
    assert_eq!(app.browser_dir, parent_dir);
    assert!(!app.browser_loading);
    assert!(app.browser_dir_cache.contains_key(&parent_dir));

    // Navigate into sub_dir
    app.navigate_browser(sub_dir.clone());
    assert_eq!(app.browser_dir, sub_dir);

    // Navigate back to parent_dir
    let start = std::time::Instant::now();
    app.navigate_browser(parent_dir.clone());
    let elapsed = start.elapsed();

    // Must be instantaneous (<50ms) from cache!
    assert!(elapsed < std::time::Duration::from_millis(50));
    assert_eq!(app.browser_dir, parent_dir);
    assert!(!app.browser_loading);
    // Cursor must be restored to ArtistA
    let artist_pos = app.browser_entries.iter().position(|(_name, p, is_dir)| *is_dir && p == &sub_dir);
    assert!(artist_pos.is_some());
    assert_eq!(app.browser_cursor, artist_pos.unwrap());

    // Test 'r' key refreshes and invalidates cache
    app.handle_tab_key(crossterm::event::KeyCode::Char('r'));
    assert!(app.status_message.contains("Refreshing directory"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_small_folder_streaming_and_incremental_sorting() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_small_folder_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let album_dir = temp_dir.join("Album");
    fs::create_dir_all(&album_dir).unwrap();

    // Create 1 subfolder and 2 tracks (fewer than 5 items, like The Darkest Knight)
    fs::create_dir_all(album_dir.join("Bonus_Discs")).unwrap();
    create_dummy_wav(&album_dir.join("02_Track_B.wav"));
    create_dummy_wav(&album_dir.join("01_Track_A.wav"));

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    let start = std::time::Instant::now();
    app.navigate_browser(album_dir.clone());
    let elapsed = start.elapsed();

    // Fast-path should load all items quickly
    assert!(elapsed < std::time::Duration::from_millis(200));
    assert_eq!(app.browser_dir, album_dir);
    assert!(!app.browser_loading);

    // Verify all 3 items + 2 headers are present: Homes, Parent, Bonus_Discs, Track_A, Track_B
    assert_eq!(app.browser_entries.len(), 5);
    assert_eq!(app.browser_entries[0].0, "★ Music Homes");
    assert_eq!(app.browser_entries[1].0, ".. [Parent Directory]");
    assert!(app.browser_entries[2].0.contains("Bonus_Discs"));
    assert!(app.browser_entries[3].0.contains("01_Track_A"));
    assert!(app.browser_entries[4].0.contains("02_Track_B"));

    // Verify cache has is_complete == true
    let cached = app.browser_dir_cache.get(&album_dir).expect("Cache should contain album_dir");
    assert!(cached.is_complete, "Cache should be marked complete");

    // Incomplete cache test: if is_complete is false, refresh_browser removes partial cache and rescans
    if let Some(c) = app.browser_dir_cache.get_mut(&album_dir) {
        c.is_complete = false;
        c.dirs.clear();
        c.files.clear();
    }
    app.refresh_browser();
    // After refresh_browser, it should rescan and complete
    assert_eq!(app.browser_entries.len(), 5);

    let _ = fs::remove_dir_all(&temp_dir);
}


