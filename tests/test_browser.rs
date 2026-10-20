mod common;

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use quickstream::metadata::AudioMetadata;
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
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_browser_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let song1 = temp_dir.join("test_track.wav");
    create_dummy_wav(&song1);

    let config_path = temp_dir.join("config.json");
    let mut settings_mgr = SettingsManager::with_path(config_path);
    settings_mgr.settings.metadata_view = true;

    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);

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
        let entry = app
            .browser_entries
            .iter()
            .find(|(_, p, is_dir)| !*is_dir && p == &song1);
        assert!(entry.is_some(), "WAV track should be in browser entries");
        entry.unwrap().0.clone()
    };

    // With metadata_view = true, label contains duration "(01:00)"
    assert!(
        label_meta.contains("01:00") || label_meta.contains("00:01"),
        "Metadata label should contain duration: {}",
        label_meta
    );
    assert!(
        label_meta.contains("[WAV]"),
        "Metadata label should contain format badge: {}",
        label_meta
    );

    // Now toggle metadata_view to false
    app.settings_mgr.settings.metadata_view = false;
    app.refresh_browser();

    let entry_file = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p == &song1);
    assert!(entry_file.is_some());
    let (label_filename, _, _) = entry_file.unwrap();

    // With metadata_view = false, label displays clean filename
    assert_eq!(label_filename, "test_track.wav");
    assert_ne!(label_meta.as_str(), label_filename.as_str());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_non_blocking_navigation_large_directory() {
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);

    let start = std::time::Instant::now();
    app.navigate_browser(music_dir.clone());
    let nav_elapsed = start.elapsed();

    // Navigation must be instantaneous with zero UI freeze: the folder is read on a worker
    assert!(
        nav_elapsed < std::time::Duration::from_millis(20),
        "Navigation took too long: {:?}",
        nav_elapsed
    );

    // All 30 folders and 30 tracks + Homes + Parent are present once the poll has the scan
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_entries.len(), 62);

    // Initial labels for uncached tracks should be fast filenames
    let first_track = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p.ends_with("track_00.wav"));
    assert!(first_track.is_some());

    // Allow background worker to populate metadata
    for _ in 0..50 {
        app.poll_background_metadata();
        if app.browser_entries.iter().any(|(l, p, is_dir)| {
            !*is_dir && p.ends_with("track_00.wav") && (l.contains("00:01") || l.contains("01:00"))
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    app.poll_background_metadata();

    let updated_track = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p.ends_with("track_00.wav"))
        .unwrap();
    assert!(
        updated_track.0.contains("00:01") || updated_track.0.contains("01:00"),
        "Track label should be updated by background worker: {}",
        updated_track.0
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_scrolling_beyond_viewport() {
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);

    let total_entries = app.browser_entries.len();
    assert!(
        total_entries >= 40,
        "Should have at least 40 entries, found {}",
        total_entries
    );

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
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);
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
    app.handle_tab_key_with_modifiers(crossterm::event::KeyCode::Down, crossterm::event::KeyModifiers::SHIFT);
    assert_eq!(app.browser_cursor, 10);

    // 12. MacBook friendly: Ctrl+D jumps down by page height (10 -> 20)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('d'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(app.browser_cursor, 20);

    // 13. MacBook friendly: Ctrl+U jumps up by page height (20 -> 10)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(app.browser_cursor, 10);

    // 14. MacBook friendly: Shift + Up jumps up by page height (10 -> 0)
    app.handle_tab_key_with_modifiers(crossterm::event::KeyCode::Up, crossterm::event::KeyModifiers::SHIFT);
    assert_eq!(app.browser_cursor, 0);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_navigate_into_folder_resets_cursor_and_offset() {
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);

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
    common::wait_for_dir_scan(&mut app);

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
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);

    let ambient_idx = app
        .browser_entries
        .iter()
        .position(|(name, _, _)| name.contains("Ambient Works"))
        .unwrap();
    let audio_bullys_idx = app
        .browser_entries
        .iter()
        .position(|(name, _, _)| name.contains("Audio Bullys"))
        .unwrap();
    let audioslave_idx = app
        .browser_entries
        .iter()
        .position(|(name, _, _)| name.contains("Audioslave"))
        .unwrap();
    let austin_wintory_idx = app
        .browser_entries
        .iter()
        .position(|(name, _, _)| name.contains("Austin Wintory"))
        .unwrap();
    let beatles_idx = app
        .browser_entries
        .iter()
        .position(|(name, _, _)| name.contains("Beatles"))
        .unwrap();

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
    common::isolate();
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
    common::isolate();
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
    common::isolate();
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
    let stop_a = app
        .dir_scan_stop
        .clone()
        .expect("Stop flag should be set for active scan");

    // Immediately navigate to DirB
    app.navigate_browser(dir_b.clone());

    // Old scan's stop flag must be signaled
    assert!(
        stop_a.load(std::sync::atomic::Ordering::Relaxed),
        "DirA scan should be cancelled"
    );
    assert_eq!(app.browser_dir, dir_b);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_cache_instant_back_navigation() {
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);

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
    let artist_pos = app
        .browser_entries
        .iter()
        .position(|(_name, p, is_dir)| *is_dir && p == &sub_dir);
    assert!(artist_pos.is_some());
    assert_eq!(app.browser_cursor, artist_pos.unwrap());

    // Test 'r' key refreshes and invalidates cache
    app.handle_tab_key(crossterm::event::KeyCode::Char('r'));
    assert!(app.status_message.contains("Refreshing directory"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_small_folder_streaming_and_incremental_sorting() {
    common::isolate();
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
    common::wait_for_dir_scan(&mut app);

    // Opening the folder only starts its scan: the listing comes in through the poll (#101).
    app.navigate_browser(album_dir.clone());
    assert_eq!(app.browser_dir, album_dir);
    assert!(app.browser_loading);
    assert_eq!(app.browser_entries.len(), 2, "only the header rows until the poll");
    common::wait_for_dir_scan(&mut app);

    // Verify all 3 items + 2 headers are present: Homes, Parent, Bonus_Discs, Track_A, Track_B
    assert_eq!(app.browser_entries.len(), 5);
    assert_eq!(app.browser_entries[0].0, "★ Music Homes");
    assert_eq!(app.browser_entries[1].0, ".. [Parent Directory]");
    assert!(app.browser_entries[2].0.contains("Bonus_Discs"));
    assert!(app.browser_entries[3].0.contains("01_Track_A"));
    assert!(app.browser_entries[4].0.contains("02_Track_B"));

    // Verify cache has is_complete == true
    let cached = app
        .browser_dir_cache
        .get(&album_dir)
        .expect("Cache should contain album_dir");
    assert!(cached.is_complete, "Cache should be marked complete");

    // Incomplete cache test: if is_complete is false, refresh_browser removes partial cache and rescans
    if let Some(c) = app.browser_dir_cache.get_mut(&album_dir) {
        c.is_complete = false;
        c.dirs.clear();
        c.files.clear();
    }
    app.refresh_browser();
    assert!(app.browser_loading, "an incomplete cache entry is scanned again");
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_entries.len(), 5);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_ui_seek_hotkeys() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_seek_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Initial status
    assert!(app.status_message.contains("Welcome") || app.status_message.contains("Found"));

    // Press ']'
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(']'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(
        app.status_message.contains("Seek +10s"),
        "Status should report Seek +10s, got: {}",
        app.status_message
    );

    // Press '['
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('['),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(
        app.status_message.contains("Seek -10s"),
        "Status should report Seek -10s, got: {}",
        app.status_message
    );

    // Press Shift + Right
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::SHIFT,
    ));
    assert!(app.status_message.contains("Seek +10s"));

    // Press Shift + Left
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyModifiers::SHIFT,
    ));
    assert!(app.status_message.contains("Seek -10s"));

    // Press Ctrl + Right
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(app.status_message.contains("Seek +10s"));

    // Press Ctrl + Left
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(app.status_message.contains("Seek -10s"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_add_track_to_queue_does_not_start_playback() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_add_track_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let song1 = temp_dir.join("01_ambient_intro.mp3");
    let song2 = temp_dir.join("02_beat_drop.flac");
    fs::write(&song1, b"ID3\x03\0\0\0\0\0#TIT2\0\0\0\x0f\0\0\0Ambient Intro").unwrap();
    fs::write(&song2, b"fLaC\0\0\0").unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);

    // Initial state: queue is empty, player is not playing
    assert_eq!(app.queue.len(), 0);
    assert!(!app.player.is_playing());

    // Locate song 1 in browser entries and position cursor
    let song1_idx = app
        .browser_entries
        .iter()
        .position(|(_, p, is_dir)| !*is_dir && p == &song1)
        .unwrap();
    app.browser_cursor = song1_idx;
    assert!(!app.queue_focus);

    // Press Enter to add song1 to queue
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));

    // VERIFY:
    // 1. Song was added to queue
    assert_eq!(app.queue.len(), 1);
    assert!(app.status_message.contains("Added to queue: "));
    // 2. Playback did NOT start immediately!
    assert!(
        !app.player.is_playing(),
        "Adding track to queue must NOT start playback immediately!"
    );

    // Locate song 2 in browser entries and position cursor
    let song2_idx = app
        .browser_entries
        .iter()
        .position(|(_, p, is_dir)| !*is_dir && p == &song2)
        .unwrap();
    app.browser_cursor = song2_idx;

    // Press Enter to add song2 to queue
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));

    // VERIFY:
    // 1. Both songs in queue
    assert_eq!(app.queue.len(), 2);
    // 2. Current index remained 0
    assert_eq!(app.queue.current_index(), 0);
    // 3. Playback still NOT started
    assert!(!app.player.is_playing());
    assert!(app.status_message.contains("Added to queue: "));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_backspace_parent_navigation() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_backspace_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let sub_dir = temp_dir.join("subfolder");
    fs::create_dir_all(&sub_dir).unwrap();

    let song = sub_dir.join("song.wav");
    create_dummy_wav(&song);

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);

    let mut app = AppUI::new(Some(sub_dir.clone()), settings_mgr);
    assert_eq!(app.browser_dir, sub_dir);

    // Press Backspace while browser pane has focus
    assert!(!app.queue_focus);
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::NONE,
    ));

    // VERIFY: Navigated to parent directory
    assert_eq!(app.browser_dir, temp_dir);

    // Switch focus to queue
    app.queue_focus = true;
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::NONE,
    ));

    // VERIFY: Did NOT navigate when queue has focus
    assert_eq!(app.browser_dir, temp_dir);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_help_modal_lifecycle_and_render() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_help_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    assert!(!app.show_help_modal);

    // Press '?' -> opens modal
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('?'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.show_help_modal);

    // Render with TestBackend (verify no panic)
    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.render(f)).unwrap();

    // Also test small terminal rendering (e.g. 70x20)
    let backend_small = ratatui::backend::TestBackend::new(70, 20);
    let mut terminal_small = ratatui::Terminal::new(backend_small).unwrap();
    terminal_small.draw(|f| app.render(f)).unwrap();

    // Press 'Esc' -> dismisses modal
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.show_help_modal);

    // Press '?' then 'q' -> dismisses modal
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('?'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.show_help_modal);
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('q'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.show_help_modal);

    // Press '?' then '?' -> toggles closed
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('?'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.show_help_modal);
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('?'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.show_help_modal);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_queue_track_skip_debouncing_and_flushing() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_debounce_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let mut track_paths = Vec::new();
    for i in 1..=5 {
        let p = temp_dir.join(format!("song_{:02}.wav", i));
        create_dummy_wav(&p);
        track_paths.push(p);
    }

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    for p in &track_paths {
        let meta = quickstream::metadata::fallback_metadata(p);
        app.queue.add_track(meta);
    }

    assert_eq!(app.queue.len(), 5);
    assert_eq!(app.queue.current_index(), 0);

    // Initial state: no pending skip
    assert!(!app.pending_track_skip);
    assert!(app.last_track_skip_time.is_none());

    // Rapidly press Right arrow 3 times
    for _ in 0..3 {
        let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        ));
    }

    // Cursor should have advanced 3 times: 0 -> 1 -> 2 -> 3
    assert_eq!(app.queue.current_index(), 3);
    assert_eq!(app.queue_cursor, 3);
    assert!(app.pending_track_skip);
    assert!(app.last_track_skip_time.is_some());
    assert!(app.status_message.contains("Skipped to:"));

    // Intermediate tracks were not played yet because debounce timer has not expired
    // Before 300ms, poll should NOT flush
    app.poll_pending_track_skip();
    assert!(app.pending_track_skip);

    // Wait until 350ms elapsed
    std::thread::sleep(std::time::Duration::from_millis(350));
    app.poll_pending_track_skip();

    // Now it should have flushed!
    assert!(!app.pending_track_skip);
    assert!(app.last_track_skip_time.is_none());
    assert_eq!(app.queue.current_index(), 3);

    // Test Prev skip debounce
    for _ in 0..2 {
        let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::NONE,
        ));
    }
    assert_eq!(app.queue.current_index(), 1);
    assert!(app.pending_track_skip);

    // Test that pressing Space immediately flushes pending skip without waiting
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char(' '),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.pending_track_skip);
    assert_eq!(app.queue.current_index(), 1);

    // Test that pressing 's' cancels pending skip
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.pending_track_skip);
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('s'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.pending_track_skip);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_metadata_view_toggle_in_place_and_settings_tab() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_toggle_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let song = temp_dir.join("01_test_song.wav");
    create_dummy_wav(&song);

    let config_path = temp_dir.join("config.json");
    let mut settings_mgr = SettingsManager::with_path(config_path);
    settings_mgr.settings.metadata_view = false;

    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);

    // Filenames mode initially
    let initial_entry = app.browser_entries.iter().find(|(_, p, is_dir)| !*is_dir && p == &song);
    assert!(initial_entry.is_some());
    assert_eq!(initial_entry.unwrap().0, "01_test_song.wav");

    // Press 'v' to toggle to metadata mode
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('v'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.settings_mgr.settings.metadata_view);

    // Wait a brief moment for metadata worker to parse tag and poll
    for _ in 0..50 {
        app.poll_background_metadata();
        let e = app
            .browser_entries
            .iter()
            .find(|(_, p, is_dir)| !*is_dir && p == &song)
            .unwrap();
        if e.0.contains("01:00") || e.0.contains("00:01") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    app.poll_background_metadata();

    let tag_entry = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p == &song)
        .unwrap();
    assert!(tag_entry.0.contains("[WAV]"));
    assert!(tag_entry.0.contains("01:00") || tag_entry.0.contains("00:01"));

    // Press 'v' again to toggle back to filenames mode
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('v'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.settings_mgr.settings.metadata_view);

    let filename_entry = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p == &song)
        .unwrap();
    assert_eq!(filename_entry.0, "01_test_song.wav");

    // Also verify cached directory has the filename updated
    if let Some(cached) = app.browser_dir_cache.get(&temp_dir) {
        let cached_file = cached
            .files
            .iter()
            .find(|(_, p, is_dir)| !*is_dir && p == &song)
            .unwrap();
        assert_eq!(cached_file.0, "01_test_song.wav");
    }

    // Now test Tab 3 (Settings) cycling:
    app.active_tab = 3;
    app.settings_cursor = 0; // File List Display

    // Press 'Enter' to cycle forward -> metadata mode
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.settings_mgr.settings.metadata_view);
    let entry_after_tab3 = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p == &song)
        .unwrap();
    assert!(entry_after_tab3.0.contains("01:00") || entry_after_tab3.0.contains("00:01"));

    // Press 'Left' on Tab 3 to cycle backward -> filenames mode
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.settings_mgr.settings.metadata_view);
    let entry_after_left = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p == &song)
        .unwrap();
    assert_eq!(entry_after_left.0, "01_test_song.wav");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_perf_and_scan_lifecycle_tracking() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_perf_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let folder = temp_dir.join("MusicFolder");
    fs::create_dir_all(&folder).unwrap();
    for i in 0..5 {
        create_dummy_wav(&folder.join(format!("song_{}.wav", i)));
    }

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Initial load: AppUI::new starts the scan of temp_dir
    assert!(app.browser_loading);
    assert!(app.browser_load_start.is_some());
    common::wait_for_dir_scan(&mut app);
    assert!(app.browser_load_start.is_none());

    // Navigate to folder: the scan runs until the poll sees DirScanEvent::Complete
    app.navigate_browser(folder.clone());
    assert!(app.browser_loading);
    assert!(app.browser_load_start.is_some());
    common::wait_for_dir_scan(&mut app);
    assert!(app.browser_load_start.is_none());
    assert!(app
        .browser_entries
        .iter()
        .any(|(_, p, is_dir)| !*is_dir && p.ends_with("song_0.wav")));

    // When cached, refresh_browser is instant and browser_load_start remains None
    app.refresh_browser();
    assert!(!app.browser_loading);
    assert!(app.browser_load_start.is_none());

    // When cache is cleared, refresh_browser triggers a fresh scan
    app.browser_dir_cache.remove(&folder);
    app.refresh_browser();
    assert!(app.browser_loading);
    assert!(app.browser_load_start.is_some());
    common::wait_for_dir_scan(&mut app);
    assert!(app.browser_load_start.is_none());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_queue_rendering_without_file_type_and_file_list_display_swap() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_q_render_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let song_path = temp_dir.join("01_test_song.wav");
    create_dummy_wav(&song_path);

    let config_path = temp_dir.join("config.json");
    let mut settings_mgr = SettingsManager::with_path(config_path);
    settings_mgr.settings.metadata_view = true;

    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);

    let meta = AudioMetadata {
        filepath: song_path.clone(),
        filename: "01_test_song.wav".to_string(),
        title: "Test Track".to_string(),
        artist: "Test Artist".to_string(),
        duration_seconds: 60.0,
        format_name: "WAV".to_string(),
        ..Default::default()
    };
    app.queue.add_track(meta);

    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    // 1. In metadata_view = true, the queue row shows the title and artist in their own
    //    columns (UPPERCASE) and the length, without a [WAV] badge.
    app.active_tab = 1;
    terminal.draw(|f| app.render(f)).unwrap();
    let view1 = terminal.backend().to_string();
    let queue_row = view1
        .lines()
        .find(|line| line.contains("TEST TRACK") && line.contains("01:00"))
        .unwrap_or_else(|| panic!("no queue row with title and length:\n{view1}"));
    assert!(queue_row.contains("TEST ARTIST"), "{queue_row:?}");
    assert!(!view1.contains("[WAV]"));

    // 2. Toggle via 'v' hotkey -> metadata_view = false (Filename mode)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('v'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(!app.settings_mgr.settings.metadata_view);

    // In Filename mode, the queue row shows the file name as it is on disk and the length,
    // without [WAV].
    terminal.draw(|f| app.render(f)).unwrap();
    let view2 = terminal.backend().to_string();
    assert!(
        view2
            .lines()
            .any(|line| line.contains("01_test_song.wav") && line.contains("01:00")),
        "{view2}"
    );
    assert!(!view2.contains("[WAV]"));

    // Browser entries also show clean filename
    let browser_entry = app
        .browser_entries
        .iter()
        .find(|(_, p, is_dir)| !*is_dir && p == &song_path)
        .unwrap();
    assert_eq!(browser_entry.0, "01_test_song.wav");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_settings_view_file_actions_disabled_and_home_dir_editing() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_set_edit_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Switch to Tab 3 (Settings)
    app.active_tab = 3;
    app.settings_cursor = 0; // File List Display

    // 1. Fuzzy search keys '/' and 'f' must NOT open fuzzy search on Settings view
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('/'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(
        app.search_modal.is_none(),
        "Fuzzy search must not open on settings view with '/'"
    );

    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('f'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(
        app.search_modal.is_none(),
        "Fuzzy search must not open on settings view with 'f'"
    );

    // 'v' must NOT toggle metadata view from settings view
    let initial_meta = app.settings_mgr.settings.metadata_view;
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('v'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(app.settings_mgr.settings.metadata_view, initial_meta);

    // 2. Navigate to Startup Home Directory (row 7)
    app.settings_cursor = 7;
    assert_eq!(app.settings_mgr.settings.home_dir, None);

    // Press 'Enter' to enter edit mode
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.editing_home_dir.is_some());

    // Type "/music/flac"
    for ch in "/music/flac".chars() {
        let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(ch),
            crossterm::event::KeyModifiers::NONE,
        ));
    }
    assert_eq!(app.editing_home_dir.as_deref(), Some("/music/flac"));

    // Press 'Backspace'
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Backspace,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(app.editing_home_dir.as_deref(), Some("/music/fla"));

    // Press 'Enter' to save
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.editing_home_dir.is_none());
    assert_eq!(app.settings_mgr.settings.home_dir.as_deref(), Some("/music/fla"));

    // 3. Clear setting with 'c'
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('c'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(app.settings_mgr.settings.home_dir, None);

    // 4. Pressing '/' directly on row 7 starts editing with '/'
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('/'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.editing_home_dir.is_some());
    assert_eq!(app.editing_home_dir.as_deref(), Some("/"));

    // Press 'Esc' to cancel
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.editing_home_dir.is_none());
    assert_eq!(app.settings_mgr.settings.home_dir, None);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_drops_duplicate_directory_entries_from_os() {
    common::isolate();
    // Some SMB shares return each directory entry several times (issue #35).
    use quickstream::ui::DirScanEvent;
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_dupes_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let sub = temp_dir.join("Artist");
    fs::create_dir_all(&sub).unwrap();

    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.browser_loading && std::time::Instant::now() < deadline {
        app.poll_background_dir_scan();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    app.navigate_browser(temp_dir.clone());

    // The same entry arrives again in later batches of the same scan.
    for _ in 0..2 {
        app.dir_scan_tx
            .send(DirScanEvent::Batch {
                scan_id: app.dir_scan_id,
                dir: app.browser_dir.clone(),
                dirs: vec![("📁 Artist/".to_string(), sub.clone(), true)],
                files: vec![],
                unfetched_tracks: vec![],
            })
            .unwrap();
    }
    app.poll_background_dir_scan();

    let shown = app.browser_entries.iter().filter(|(_, p, _)| p == &sub).count();
    assert_eq!(shown, 1, "Artist/ listed {} times", shown);
    let _ = fs::remove_dir_all(&temp_dir);
}

/// Holds back the browser's reads of one folder until released, like a share that is slow to
/// answer; other folders are read at once. Dropping it releases the read.
struct HeldRead {
    open: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl HeldRead {
    fn install(app: &mut AppUI, held: &std::path::Path) -> Self {
        let open = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let gate = open.clone();
        let held = held.to_path_buf();
        app.set_dir_reader(std::sync::Arc::new(move |dir: &std::path::Path| {
            if dir == held {
                let (lock, cvar) = &*gate;
                let _open = cvar.wait_while(lock.lock().unwrap(), |open| !*open).unwrap();
            }
            fs::read_dir(dir)
        }));
        HeldRead { open }
    }

    fn release(&self) {
        let (lock, cvar) = &*self.open;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

impl Drop for HeldRead {
    fn drop(&mut self) {
        self.release();
    }
}

/// How long a key that opens a folder may take: the folder is listed on a worker, never waited
/// for (the wait this replaces took at least 50 ms whenever the read was slower than that).
const KEY_BUDGET: std::time::Duration = std::time::Duration::from_millis(20);

fn press(app: &mut AppUI, code: crossterm::event::KeyCode) -> std::time::Duration {
    let start = std::time::Instant::now();
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ));
    start.elapsed()
}

fn listed_files(app: &AppUI) -> usize {
    app.browser_entries.iter().filter(|(_, _, is_dir)| !is_dir).count()
}

#[test]
fn test_browser_open_folder_does_not_wait_for_a_slow_read() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_slow_open_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(&slow).unwrap();
    for name in ["a.wav", "b.wav", "c.wav"] {
        create_dummy_wav(&slow.join(name));
    }
    let mut settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);

    let held = HeldRead::install(&mut app, &slow);
    app.browser_cursor = app
        .browser_entries
        .iter()
        .position(|(_, p, is_dir)| *is_dir && p == &slow)
        .expect("Slow/ listed");
    let elapsed = press(&mut app, crossterm::event::KeyCode::Enter);
    assert_eq!(app.browser_dir, slow);
    assert!(elapsed < KEY_BUDGET, "Enter on a folder took {:?}", elapsed);

    // Until the read answers the panel shows READING DIRECTORY, and polling never waits.
    assert!(app.browser_loading);
    let start = std::time::Instant::now();
    app.poll_background_dir_scan();
    assert!(start.elapsed() < KEY_BUDGET);
    assert!(app.browser_loading);
    assert_eq!(listed_files(&app), 0);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(screen.contains("READING DIRECTORY"), "{}", screen);

    held.release();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(listed_files(&app), 3);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(!screen.contains("READING DIRECTORY"), "{}", screen);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_backspace_restores_cursor_once_slow_parent_is_listed() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_slow_parent_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let parent = temp_dir.join("Music");
    // More folders than the scan's first batch (5), so the one to return to may come later.
    for name in ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel"] {
        fs::create_dir_all(parent.join(name)).unwrap();
    }
    let golf = parent.join("Golf");
    let mut settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    let mut app = AppUI::new(Some(golf.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);
    assert!(!app.browser_dir_cache.contains_key(&parent));

    let held = HeldRead::install(&mut app, &parent);
    let elapsed = press(&mut app, crossterm::event::KeyCode::Backspace);
    assert_eq!(app.browser_dir, parent);
    assert!(elapsed < KEY_BUDGET, "Backspace took {:?}", elapsed);
    assert!(app.browser_loading);
    assert_eq!(app.pending_parent_restore.as_ref(), Some(&golf));

    held.release();
    common::wait_for_dir_scan(&mut app);
    let golf_row = app
        .browser_entries
        .iter()
        .position(|(_, p, is_dir)| *is_dir && p == &golf)
        .expect("Golf/ listed");
    assert_eq!(app.browser_cursor, golf_row, "cursor back on the folder Backspace left");
    assert_eq!(app.pending_parent_restore, None);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_rescan_does_not_wait_for_a_slow_read() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_slow_rescan_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    create_dummy_wav(&temp_dir.join("a.wav"));
    let mut settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);
    assert_eq!(listed_files(&app), 1);

    let held = HeldRead::install(&mut app, &temp_dir);
    app.browser_cursor = 0; // the Music Homes row: `r` rescans the folder
    let elapsed = press(&mut app, crossterm::event::KeyCode::Char('r'));
    assert!(elapsed < KEY_BUDGET, "r took {:?}", elapsed);
    assert!(app.browser_loading);
    assert!(app.status_message.contains("Refreshing directory"));

    held.release();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(listed_files(&app), 1);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_event_loop_polls_keys_at_frame_rate_while_a_folder_is_read() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_key_poll_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let mut app = AppUI::new(
        Some(temp_dir.clone()),
        SettingsManager::with_path(temp_dir.join("config.json")),
    );
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.key_poll_timeout(), std::time::Duration::from_millis(100));
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(&slow).unwrap();
    let _held = HeldRead::install(&mut app, &slow);
    app.navigate_browser(slow);
    // While a folder has just been opened its batches are picked up within a frame (60 FPS)...
    assert!(app.key_poll_timeout() <= std::time::Duration::from_millis(16));
    // ...but a read that keeps going (a hung share) goes back to the usual tick.
    std::thread::sleep(AppUI::FAST_TICK_WHILE_READING + std::time::Duration::from_millis(20));
    app.poll_background_dir_scan();
    assert!(app.browser_loading);
    assert_eq!(app.key_poll_timeout(), std::time::Duration::from_millis(100));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_stopped_scan_never_lists_into_music_homes() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_stopped_scan_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folder = temp_dir.join("X");
    for name in ["A1", "A2", "A3", "A4", "A5", "A6", "A7"] {
        fs::create_dir_all(folder.join(name)).unwrap();
    }
    let homes: Vec<PathBuf> = ["HomeA", "HomeB", "HomeC"].iter().map(|n| temp_dir.join(n)).collect();
    for home in &homes {
        fs::create_dir_all(home).unwrap();
    }
    let mut app = AppUI::new(
        Some(temp_dir.clone()),
        SettingsManager::with_path(temp_dir.join("config.json")),
    );
    common::wait_for_dir_scan(&mut app);
    app.bookmarks = homes.iter().map(|h| h.to_string_lossy().to_string()).collect();

    // X's batches are queued but not yet polled when Music Homes opens.
    let held = HeldRead::install(&mut app, &folder);
    app.navigate_browser(folder.clone());
    held.release();
    std::thread::sleep(std::time::Duration::from_millis(200));
    app.navigate_browser(PathBuf::from("___MUSIC_HOMES___"));
    app.poll_background_dir_scan();

    let listed: Vec<&PathBuf> = app.browser_entries.iter().skip(1).map(|(_, p, _)| p).collect();
    assert_eq!(
        listed,
        homes.iter().collect::<Vec<_>>(),
        "Music Homes lists only the homes"
    );

    // `d` on a home removes that home, not one a stray row pushed down.
    app.browser_cursor = 2;
    app.handle_tab_key(crossterm::event::KeyCode::Char('d'));
    assert!(!app.bookmarks.contains(&homes[1].to_string_lossy().to_string()));
    assert_eq!(app.bookmarks.len(), 2);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_cancelled_scan_never_lists_later() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_cancel_scan_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folder = temp_dir.join("Slow");
    fs::create_dir_all(folder.join("Sub")).unwrap();
    let mut app = AppUI::new(
        Some(temp_dir.clone()),
        SettingsManager::with_path(temp_dir.join("config.json")),
    );
    common::wait_for_dir_scan(&mut app);

    let held = HeldRead::install(&mut app, &folder);
    app.navigate_browser(folder.clone());
    // Once the READING DIRECTORY row is on screen, Esc cancels the scan, even when the read
    // answered just before (its rows are queued, not yet polled).
    std::thread::sleep(AppUI::FOLDER_FIRST_FRAME_HOLD + std::time::Duration::from_millis(20));
    app.poll_background_dir_scan();
    held.release();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let quit = app
        .handle_input_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
        .unwrap();
    assert!(!quit);
    assert_eq!(app.status_message, "Cancelled directory scan");
    app.poll_background_dir_scan();
    assert_eq!(app.browser_entries.len(), 2, "the cancelled scan's rows are dropped");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_esc_right_after_opening_a_folder_quits() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_esc_quit_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folder = temp_dir.join("X");
    fs::create_dir_all(folder.join("S1")).unwrap();
    fs::create_dir_all(folder.join("S2")).unwrap();
    let mut app = AppUI::new(
        Some(temp_dir.clone()),
        SettingsManager::with_path(temp_dir.join("config.json")),
    );
    common::wait_for_dir_scan(&mut app);

    // Nothing of X has been drawn yet (the previous frame is still on screen), so Esc quits
    // as it would have once a fast folder was listed.
    app.navigate_browser(folder.clone());
    let quit = app
        .handle_input_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
        .unwrap();
    assert!(quit, "Esc quits; status {:?}", app.status_message);

    let _ = fs::remove_dir_all(&temp_dir);
}

/// One tick of `AppUI::event_loop` without a terminal: poll the scan, then draw unless the loop
/// keeps the previous frame. Returns the frame drawn, if any.
fn tick(app: &mut AppUI) -> Option<String> {
    app.poll_background_dir_scan();
    if app.holds_frame() {
        return None;
    }
    Some(common::tui::render_text(app, 120, 40))
}

#[test]
fn test_event_loop_never_draws_reading_directory_for_a_fast_folder() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_first_frame_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folders: Vec<PathBuf> = (0..10).map(|i| temp_dir.join(format!("F{i:02}"))).collect();
    for folder in &folders {
        fs::create_dir_all(folder.join("Sub")).unwrap();
        create_dummy_wav(&folder.join("a.wav"));
    }
    let mut settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);
    common::wait_for_dir_scan(&mut app);
    app.active_tab = 1;

    for folder in &folders {
        app.navigate_browser(folder.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let first = loop {
            if let Some(frame) = tick(&mut app) {
                break frame;
            }
            assert!(std::time::Instant::now() < deadline, "no frame drawn");
            std::thread::sleep(app.key_poll_timeout());
        };
        assert!(!first.contains("READING DIRECTORY"), "{}", first);
        assert!(first.contains("SUB/"), "{}", first);
        app.navigate_browser(temp_dir.clone());
        common::wait_for_dir_scan(&mut app);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_event_loop_draws_reading_directory_once_a_slow_read_passes_the_hold() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_hold_slow_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(slow.join("Sub")).unwrap();
    let mut app = AppUI::new(
        Some(temp_dir.clone()),
        SettingsManager::with_path(temp_dir.join("config.json")),
    );
    common::wait_for_dir_scan(&mut app);
    app.active_tab = 1;

    let held = HeldRead::install(&mut app, &slow);
    app.navigate_browser(slow.clone());
    assert_eq!(
        tick(&mut app),
        None,
        "the previous frame stays while the read may still answer"
    );
    std::thread::sleep(AppUI::FOLDER_FIRST_FRAME_HOLD + std::time::Duration::from_millis(20));
    let frame = tick(&mut app).expect("drawn once the hold is over");
    assert!(frame.contains("READING DIRECTORY"), "{}", frame);

    held.release();
    common::wait_for_dir_scan(&mut app);
    let frame = tick(&mut app).expect("drawn");
    assert!(frame.contains("SUB/"), "{}", frame);

    let _ = fs::remove_dir_all(&temp_dir);
}
