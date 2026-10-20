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

    // Navigation never waits for the listing: the folder is read on a worker, and until the
    // poll takes its scan in, the previous folder stays on screen.
    app.navigate_browser(music_dir.clone());
    assert_eq!(app.opening_folder(), Some(music_dir.as_path()));
    assert_eq!(app.browser_dir, temp_dir);

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

    // 3. Shift+J (uppercase J, as the terminal sends it) jumps by page height (20 -> 30)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('J'),
        crossterm::event::KeyModifiers::SHIFT,
    ));
    assert_eq!(app.browser_cursor, 30);

    // 4. Ctrl+F jumps by page height (30 -> 40)
    app.handle_tab_key_with_modifiers(
        crossterm::event::KeyCode::Char('f'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(app.browser_cursor, 40);

    // 5. End / G jumps directly to last item
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('G'),
        crossterm::event::KeyModifiers::SHIFT,
    ));
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
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('K'),
        crossterm::event::KeyModifiers::SHIFT,
    ));
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
    common::wait_for_dir_scan(&mut app);
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

    // Opening the folder only starts its scan: the listing comes in through the poll (#101),
    // and the previous folder stays on screen until then.
    let shown = app.browser_entries.clone();
    app.navigate_browser(album_dir.clone());
    assert_eq!(app.opening_folder(), Some(album_dir.as_path()));
    assert_eq!(app.browser_dir, temp_dir);
    assert_eq!(app.browser_entries, shown);
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, album_dir);

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
    assert_eq!(
        app.opening_folder(),
        Some(album_dir.as_path()),
        "an incomplete cache entry is scanned again"
    );
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

    // Navigate to folder: it is being opened until the poll sees DirScanEvent::Complete
    app.navigate_browser(folder.clone());
    assert_eq!(app.opening_folder(), Some(folder.as_path()));
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
    assert_eq!(app.opening_folder(), Some(folder.as_path()));
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

    // 4. Pressing '/' directly on row 7 starts editing, empty like Enter and 'e' (#100)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('/'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.editing_home_dir.is_some());
    assert_eq!(app.editing_home_dir.as_deref(), Some(""));

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

/// Stands in for a slow share when the browser lists one folder: its entries come in reverse
/// name order (so later batches sort above earlier ones), and the read stops before the entry
/// at each of `pauses` (0: before the first) until the test resumes it. `wait_paused` returns
/// once it has stopped there, when the scan has sent everything it flushed before. Other
/// folders are read at once; `reads` counts the gated folder's reads. Dropping it lets the
/// read run to its end.
struct GatedRead {
    resumes: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
    paused: std::sync::mpsc::Receiver<usize>,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// The gated folder's entries, handed out by `GatedRead`'s rules.
struct GatedEntries {
    entries: std::vec::IntoIter<std::io::Result<fs::DirEntry>>,
    index: usize,
    pauses: Vec<usize>,
    passed: usize,
    resumes: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
    paused: std::sync::mpsc::Sender<usize>,
}

impl Iterator for GatedEntries {
    type Item = std::io::Result<fs::DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pauses.get(self.passed) == Some(&self.index) {
            let _ = self.paused.send(self.index);
            let (lock, cvar) = &*self.resumes;
            let passed = self.passed;
            drop(
                cvar.wait_while(lock.lock().unwrap(), |resumes| *resumes <= passed)
                    .unwrap(),
            );
            self.passed += 1;
        }
        self.index += 1;
        self.entries.next()
    }
}

impl GatedRead {
    fn install(app: &mut AppUI, gated: &std::path::Path, pauses: &[usize]) -> Self {
        let resumes = std::sync::Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new()));
        let (paused_tx, paused) = std::sync::mpsc::channel();
        let paused_tx = std::sync::Mutex::new(paused_tx);
        let (gated, pauses, gate) = (gated.to_path_buf(), pauses.to_vec(), resumes.clone());
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reads.clone();
        app.set_dir_reader(std::sync::Arc::new(move |dir: &std::path::Path| {
            let entries = fs::read_dir(dir)?;
            if dir != gated {
                return Ok(Box::new(entries) as quickstream::ui::DirEntries);
            }
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut entries: Vec<_> = entries.collect();
            entries.sort_by_key(|e| std::cmp::Reverse(e.as_ref().map(|e| e.file_name()).ok()));
            Ok(Box::new(GatedEntries {
                entries: entries.into_iter(),
                index: 0,
                pauses: pauses.clone(),
                passed: 0,
                resumes: gate.clone(),
                paused: paused_tx.lock().unwrap().clone(),
            }) as quickstream::ui::DirEntries)
        }));
        GatedRead { resumes, paused, reads }
    }

    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait (at most 5 s, a deadlock guard) until the read stops at its next pause.
    fn wait_paused(&self) -> usize {
        self.paused
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the gated read never paused")
    }

    fn resume(&self) {
        let (lock, cvar) = &*self.resumes;
        *lock.lock().unwrap() += 1;
        cvar.notify_all();
    }
}

impl Drop for GatedRead {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.resumes;
        *lock.lock().unwrap() = usize::MAX;
        cvar.notify_all();
    }
}

/// Long enough that only the end of its scan shows a folder being opened.
const NO_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

fn key(app: &mut AppUI, code: crossterm::event::KeyCode) -> bool {
    app.handle_input_key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ))
    .unwrap()
}

fn listed_files(app: &AppUI) -> usize {
    app.browser_entries.iter().filter(|(_, _, is_dir)| !is_dir).count()
}

fn row_of(app: &AppUI, path: &std::path::Path) -> usize {
    app.browser_entries
        .iter()
        .position(|(_, p, _)| p == path)
        .unwrap_or_else(|| panic!("{} not listed", path.display()))
}

fn selected(app: &AppUI) -> PathBuf {
    app.browser_entries[app.browser_cursor].1.clone()
}

fn browser_app(temp_dir: &std::path::Path, start: &std::path::Path) -> AppUI {
    let mut settings_mgr = SettingsManager::with_path(temp_dir.join("config.json"));
    settings_mgr.settings.metadata_view = false;
    let mut app = AppUI::new(Some(start.to_path_buf()), settings_mgr);
    common::wait_for_dir_scan(&mut app);
    app.active_tab = 1;
    app
}

#[test]
fn test_browser_open_folder_does_not_wait_for_a_slow_read() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_slow_open_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(&slow).unwrap();
    for name in ["a", "b", "c", "d", "e", "f", "g"] {
        create_dummy_wav(&slow.join(format!("{name}.wav")));
    }
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(NO_DEADLINE);

    // The read sends its first batch (5 files), then stops.
    let gate = GatedRead::install(&mut app, &slow, &[5]);
    app.browser_cursor = row_of(&app, &slow);
    assert!(!key(&mut app, crossterm::event::KeyCode::Enter));
    // Enter only started the scan: nothing of it was taken in (waiting for it would have).
    assert_eq!(app.opening_folder(), Some(slow.as_path()));
    assert_eq!(app.browser_dir, temp_dir, "the previous folder stays on screen");
    assert!(!app.browser_dir_cache.contains_key(&slow));
    gate.wait_paused();
    assert!(
        !app.browser_dir_cache.contains_key(&slow),
        "nothing is taken in before a poll"
    );

    // The poll takes the batch in without showing it: the previous folder keeps the screen.
    app.poll_background_dir_scan();
    assert_eq!(app.browser_dir_cache[&slow].files.len(), 5);
    assert_eq!(app.browser_dir, temp_dir);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(!screen.contains("READING DIRECTORY"), "{}", screen);
    assert!(screen.contains("SLOW/"), "{}", screen);

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, slow);
    assert_eq!(listed_files(&app), 7);
    let screen = common::tui::render_text(&mut app, 120, 40);
    assert!(!screen.contains("READING DIRECTORY"), "{}", screen);

    let _ = fs::remove_dir_all(&temp_dir);
}

/// A parent with eight folders: in the gated read's reverse order, the first batch is
/// Hotel … Delta and Charlie, Bravo, Alpha come after the pause.
fn parent_with_folders(tag: &str) -> (PathBuf, PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let parent = temp_dir.join("Music");
    for name in ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel"] {
        fs::create_dir_all(parent.join(name)).unwrap();
    }
    (temp_dir, parent)
}

#[test]
fn test_browser_backspace_restores_cursor_once_slow_parent_is_listed() {
    common::isolate();
    let (temp_dir, parent) = parent_with_folders("slow_parent");
    let bravo = parent.join("Bravo");
    let mut app = browser_app(&temp_dir, &bravo);
    assert!(!app.browser_dir_cache.contains_key(&parent));

    // Shown at the deadline with its first batch, which doesn't hold Bravo.
    app.set_folder_open_deadline(std::time::Duration::ZERO);
    let gate = GatedRead::install(&mut app, &parent, &[5]);
    assert!(!key(&mut app, crossterm::event::KeyCode::Backspace));
    assert_eq!(app.opening_folder(), Some(parent.as_path()));
    assert_eq!(app.browser_dir, bravo);
    gate.wait_paused();
    app.poll_background_dir_scan();
    assert_eq!(app.browser_dir, parent);
    assert!(app.browser_loading);
    assert_eq!(app.pending_parent_restore.as_ref(), Some(&bravo));

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(selected(&app), bravo, "cursor back on the folder Backspace left");
    assert_eq!(app.pending_parent_restore, None);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_backspace_restore_never_undoes_the_users_moves() {
    common::isolate();
    let (temp_dir, parent) = parent_with_folders("restore_moves");
    let bravo = parent.join("Bravo");
    let mut app = browser_app(&temp_dir, &bravo);

    app.set_folder_open_deadline(std::time::Duration::ZERO);
    let gate = GatedRead::install(&mut app, &parent, &[5]);
    key(&mut app, crossterm::event::KeyCode::Backspace);
    gate.wait_paused();
    app.poll_background_dir_scan();
    assert_eq!(app.pending_parent_restore.as_ref(), Some(&bravo));

    // The user moves to Delta while the rest (Bravo among it) is still being read.
    key(&mut app, crossterm::event::KeyCode::Char('j'));
    key(&mut app, crossterm::event::KeyCode::Char('j'));
    assert_eq!(selected(&app), parent.join("Delta"));
    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(
        selected(&app),
        parent.join("Delta"),
        "the cursor stays where the user put it"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_cursor_stays_on_its_entry_while_batches_sort_in() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_resort_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folder = temp_dir.join("X");
    // Reverse order: M5 … M1 in the first batch, then A5 … A1, which sort above them.
    for name in ["A1", "A2", "A3", "A4", "A5", "M1", "M2", "M3", "M4", "M5"] {
        fs::create_dir_all(folder.join(name)).unwrap();
    }
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(std::time::Duration::ZERO);
    let gate = GatedRead::install(&mut app, &folder, &[5]);
    app.navigate_browser(folder.clone());
    gate.wait_paused();
    app.poll_background_dir_scan();
    assert_eq!(app.browser_dir, folder);
    assert!(app.browser_loading);

    // Header rows, then M1 M2 M3: the cursor goes to M3.
    for _ in 0..4 {
        key(&mut app, crossterm::event::KeyCode::Char('j'));
    }
    let m3 = folder.join("M3");
    assert_eq!(selected(&app), m3);
    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(
        selected(&app),
        m3,
        "the rows sorted in above don't move the cursor off M3"
    );

    // So Enter opens the folder that was highlighted.
    key(&mut app, crossterm::event::KeyCode::Enter);
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, m3);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_rescan_keeps_the_listing_until_the_slow_read_answers() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_slow_rescan_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    create_dummy_wav(&temp_dir.join("a.wav"));
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(NO_DEADLINE);
    assert_eq!(listed_files(&app), 1);

    let gate = GatedRead::install(&mut app, &temp_dir, &[0]);
    app.browser_cursor = 0; // the Music Homes row: `r` rescans the folder
    key(&mut app, crossterm::event::KeyCode::Char('r'));
    assert!(app.status_message.contains("Refreshing directory"));
    assert_eq!(app.opening_folder(), Some(temp_dir.as_path()));
    assert!(!app.browser_loading);
    assert_eq!(listed_files(&app), 1, "the listing stays while the read is held");

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.opening_folder(), None);
    assert_eq!(listed_files(&app), 1);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_backspace_again_keeps_opening_the_same_parent() {
    common::isolate();
    let (temp_dir, parent) = parent_with_folders("backspace_twice");
    let bravo = parent.join("Bravo");
    let mut app = browser_app(&temp_dir, &bravo);
    let deadline = std::time::Duration::from_millis(300);
    app.set_folder_open_deadline(deadline);

    let gate = GatedRead::install(&mut app, &parent, &[0]);
    let first = std::time::Instant::now();
    key(&mut app, crossterm::event::KeyCode::Backspace);
    gate.wait_paused();
    std::thread::sleep(std::time::Duration::from_millis(200));
    // A second Backspace (a held key) acts on Bravo, still shown: the parent keeps opening.
    key(&mut app, crossterm::event::KeyCode::Backspace);
    assert_eq!(app.opening_folder(), Some(parent.as_path()));
    assert_eq!(app.browser_dir, bravo);

    // The deadline still counts from the first Backspace, so the parent shows on time.
    while first.elapsed() < deadline + std::time::Duration::from_millis(20) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    app.poll_background_dir_scan();
    assert_eq!(app.browser_dir, parent, "shown at the first Backspace's deadline");
    assert!(app.browser_loading);

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(gate.reads(), 1, "the parent is read once");
    assert_eq!(selected(&app), bravo);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_removing_another_home_keeps_opening_the_home() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_remove_home_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let homes: Vec<PathBuf> = ["H1", "H2", "H3"].iter().map(|n| temp_dir.join(n)).collect();
    for home in &homes {
        fs::create_dir_all(home.join("Sub")).unwrap();
    }
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(NO_DEADLINE);
    app.bookmarks = homes.iter().map(|h| h.to_string_lossy().to_string()).collect();
    app.navigate_browser(PathBuf::from("___MUSIC_HOMES___"));

    let gate = GatedRead::install(&mut app, &homes[0], &[0]);
    app.browser_cursor = 1;
    key(&mut app, crossterm::event::KeyCode::Enter);
    assert_eq!(app.opening_folder(), Some(homes[0].as_path()));
    gate.wait_paused();

    // `d` on H2 removes it; H1 keeps opening.
    app.browser_cursor = 2;
    key(&mut app, crossterm::event::KeyCode::Char('d'));
    assert_eq!(app.bookmarks.len(), 2);
    assert_eq!(app.opening_folder(), Some(homes[0].as_path()));
    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, homes[0], "the home being opened is shown");
    assert!(!app.in_bookmarks_view);

    // Removing the home being opened stops opening it.
    app.navigate_browser(PathBuf::from("___MUSIC_HOMES___"));
    let gate = GatedRead::install(&mut app, &homes[2], &[0]);
    app.browser_cursor = 2;
    key(&mut app, crossterm::event::KeyCode::Enter);
    assert_eq!(app.opening_folder(), Some(homes[2].as_path()));
    gate.wait_paused();
    key(&mut app, crossterm::event::KeyCode::Char('d'));
    assert_eq!(app.opening_folder(), None);
    gate.resume();
    app.poll_background_dir_scan();
    assert!(app.in_bookmarks_view);
    assert_eq!(app.browser_entries.len(), 2, "`..` and H1");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_spotlight_closes_when_the_folder_being_opened_shows() {
    common::isolate();
    let (temp_dir, folder) = folder_with_subfolders("spotlight_opening");
    fs::create_dir_all(temp_dir.join("Sierra")).unwrap();
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(NO_DEADLINE);

    let gate = GatedRead::install(&mut app, &folder, &[0]);
    app.navigate_browser(folder.clone());
    gate.wait_paused();
    // A spotlight opened on the folder still shown.
    key(&mut app, crossterm::event::KeyCode::Char('/'));
    app.browser_spotlight = Some("s".to_string());
    app.jump_spotlight_to_first_match();
    assert_eq!(selected(&app), temp_dir.join("Sierra"));

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    assert_eq!(app.browser_dir, folder);
    assert_eq!(app.browser_spotlight, None, "its query was for the previous folder");
    assert!(!app.status_message.starts_with("Spotlight:"), "{}", app.status_message);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_event_loop_ticks_at_frame_rate_only_while_a_folder_is_being_read() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_key_poll_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(&slow).unwrap();
    let mut app = browser_app(&temp_dir, &temp_dir);
    let frame = std::time::Duration::from_millis(16);
    assert_eq!(app.key_poll_timeout(), quickstream::ui::TickClock::FULL_TICK);

    app.set_folder_open_deadline(NO_DEADLINE);
    let _gate = GatedRead::install(&mut app, &slow, &[0]);
    app.navigate_browser(slow.clone());
    assert!(app.opening_folder().is_some());
    assert_eq!(app.key_poll_timeout(), frame, "while the folder is being opened");

    app.set_folder_open_deadline(std::time::Duration::ZERO);
    app.poll_background_dir_scan();
    assert!(app.opening_folder().is_none() && app.browser_loading);
    assert_eq!(app.key_poll_timeout(), frame, "in the read's first moments");

    // A read that keeps going (a hung share) goes back to the usual tick.
    let opened = std::time::Instant::now()
        .checked_sub(AppUI::FAST_TICK_WHILE_READING + std::time::Duration::from_millis(1))
        .unwrap();
    app.browser_load_start = Some(opened);
    assert!(app.browser_loading);
    assert_eq!(app.key_poll_timeout(), quickstream::ui::TickClock::FULL_TICK);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_event_loop_runs_its_other_pollers_at_their_usual_rate() {
    common::isolate();
    use quickstream::ui::TickClock;
    let ms = std::time::Duration::from_millis;
    let t0 = std::time::Instant::now();
    let mut clock = TickClock::new(t0);
    assert!(clock.full_tick(t0), "the first tick polls everything");

    // Frame-rate ticks in between poll only the directory scan.
    for n in 1..=6 {
        assert!(!clock.full_tick(t0 + ms(16 * n)), "tick at {} ms", 16 * n);
    }
    assert_eq!(
        clock.wait(t0 + ms(96), ms(16)),
        ms(4),
        "never waits past the next full tick"
    );
    assert!(clock.full_tick(t0 + ms(100)));
    assert!(!clock.full_tick(t0 + ms(116)));
    assert_eq!(clock.wait(t0 + ms(120), ms(100)), ms(80));

    // A key makes the next tick a full one.
    clock.input(t0 + ms(130));
    assert_eq!(clock.wait(t0 + ms(130), ms(16)), ms(0));
    assert!(clock.full_tick(t0 + ms(130)));
    assert!(!clock.full_tick(t0 + ms(146)));
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
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.bookmarks = homes.iter().map(|h| h.to_string_lossy().to_string()).collect();

    // X's first batch is sent but not yet polled when Music Homes opens.
    let gate = GatedRead::install(&mut app, &folder, &[5]);
    app.navigate_browser(folder.clone());
    gate.wait_paused();
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

/// A folder with six subfolders S1 … S6 next to `start`'s other entries.
fn folder_with_subfolders(tag: &str) -> (PathBuf, PathBuf) {
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folder = temp_dir.join("Slow");
    for name in ["S1", "S2", "S3", "S4", "S5", "S6"] {
        fs::create_dir_all(folder.join(name)).unwrap();
    }
    (temp_dir, folder)
}

#[test]
fn test_browser_esc_while_a_folder_is_being_opened_stays_and_never_quits() {
    common::isolate();
    let (temp_dir, folder) = folder_with_subfolders("esc_opening");
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(NO_DEADLINE);
    let shown = app.browser_entries.clone();

    let gate = GatedRead::install(&mut app, &folder, &[5]);
    app.navigate_browser(folder.clone());
    gate.wait_paused(); // its first batch is sent, not yet polled
    assert!(!key(&mut app, crossterm::event::KeyCode::Esc), "Esc never quits here");
    assert_eq!(app.status_message, "Stopped opening 'Slow'");
    assert_eq!(app.opening_folder(), None);
    assert_eq!(app.browser_dir, temp_dir);

    gate.resume();
    app.poll_background_dir_scan();
    assert_eq!(app.browser_entries, shown, "the folder on screen stays as it was");
    assert!(!app.browser_dir_cache.contains_key(&folder));

    // With nothing loading, Esc quits.
    assert!(key(&mut app, crossterm::event::KeyCode::Esc));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_cancelled_scan_never_lists_later() {
    common::isolate();
    let (temp_dir, folder) = folder_with_subfolders("cancel_scan");
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(std::time::Duration::ZERO);

    // Shown at the deadline before anything is read, so READING DIRECTORY is on screen.
    let gate = GatedRead::install(&mut app, &folder, &[0, 5]);
    app.navigate_browser(folder.clone());
    gate.wait_paused();
    app.poll_background_dir_scan();
    assert_eq!(app.browser_dir, folder);
    assert!(app.browser_loading);
    // The first batch is sent, not yet polled, when Esc cancels the scan.
    gate.resume();
    gate.wait_paused();
    assert!(!key(&mut app, crossterm::event::KeyCode::Esc));
    assert_eq!(app.status_message, "Cancelled directory scan");
    app.poll_background_dir_scan();
    assert_eq!(app.browser_entries.len(), 2, "the cancelled scan's rows are dropped");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// One tick of `AppUI::event_loop` without a terminal: poll the scan, then draw.
fn tick(app: &mut AppUI) -> String {
    app.poll_background_dir_scan();
    common::tui::render_text(app, 120, 40)
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
    let mut app = browser_app(&temp_dir, &temp_dir);
    // Only the scan's end shows the folder, however long the machine takes to list it.
    app.set_folder_open_deadline(NO_DEADLINE);

    for folder in &folders {
        app.navigate_browser(folder.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let frame = tick(&mut app);
            assert!(!frame.contains("READING DIRECTORY"), "{}", frame);
            if app.browser_dir == *folder {
                assert!(frame.contains("SUB/"), "{}", frame);
                break;
            }
            assert!(frame.contains("F00/"), "the previous folder until then: {}", frame);
            assert!(std::time::Instant::now() < deadline, "{} never shown", folder.display());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        app.navigate_browser(temp_dir.clone());
        common::wait_for_dir_scan(&mut app);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_event_loop_draws_reading_directory_once_a_slow_read_passes_the_deadline() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_deadline_slow_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(slow.join("Sub")).unwrap();
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.set_folder_open_deadline(std::time::Duration::ZERO);

    let gate = GatedRead::install(&mut app, &slow, &[0]);
    app.navigate_browser(slow.clone());
    let frame = common::tui::render_text(&mut app, 120, 40);
    assert!(
        !frame.contains("READING DIRECTORY"),
        "the previous folder until the poll: {}",
        frame
    );
    assert!(frame.contains("SLOW/"), "{}", frame);
    gate.wait_paused();
    let frame = tick(&mut app);
    assert!(frame.contains("READING DIRECTORY"), "{}", frame);

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    let frame = tick(&mut app);
    assert!(frame.contains("SUB/"), "{}", frame);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_browser_add_all_waits_for_the_whole_folder() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_add_partial_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let slow = temp_dir.join("Slow");
    fs::create_dir_all(&slow).unwrap();
    for name in ["a", "b", "c", "d", "e", "f", "g"] {
        create_dummy_wav(&slow.join(format!("{name}.wav")));
    }
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.player = Box::new(common::fake_player::FakePlayer::default());
    app.set_folder_open_deadline(std::time::Duration::ZERO);
    let gate = GatedRead::install(&mut app, &slow, &[5]);
    app.navigate_browser(slow.clone());
    gate.wait_paused();
    app.poll_background_dir_scan();
    assert!(app.browser_loading);
    assert_eq!(listed_files(&app), 5);

    // Five of seven files are listed: `a` adds none of them yet.
    key(&mut app, crossterm::event::KeyCode::Char('a'));
    assert_eq!(app.queue.len(), 0);
    assert_eq!(app.status_message, "Still reading Slow, try again when it's listed");

    gate.resume();
    common::wait_for_dir_scan(&mut app);
    key(&mut app, crossterm::event::KeyCode::Char('a'));
    assert_eq!(app.queue.len(), 7);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_autopilot_reads_a_folder_still_being_listed_itself() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("qs_browser_autopilot_partial_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let folder = temp_dir.join("Library");
    // 90 folders; only D000 … D004 hold tracks, and the gated read lists them last.
    for i in 0..90 {
        let sub = folder.join(format!("D{i:03}"));
        fs::create_dir_all(&sub).unwrap();
        if i < 5 {
            create_dummy_wav(&sub.join("track.wav"));
        }
    }
    let mut app = browser_app(&temp_dir, &temp_dir);
    app.player = Box::new(common::fake_player::FakePlayer::default());
    app.set_folder_open_deadline(std::time::Duration::ZERO);
    // Batches of 5, 40 and 40 folders, then a pause: 85 listed, enough for autopilot to take
    // the browser's subfolders instead of reading the folder.
    let gate = GatedRead::install(&mut app, &folder, &[85]);
    app.navigate_browser(folder.clone());
    gate.wait_paused();
    app.poll_background_dir_scan();
    assert!(app.browser_loading);
    assert_eq!(app.browser_entries.len() - app.browser_header_rows(), 85);

    key(&mut app, crossterm::event::KeyCode::Char('p'));
    app.wait_for_autopilot_scan(std::time::Duration::from_secs(20));
    assert!(!app.is_autopilot_scanning, "autopilot scan did not finish");
    assert_eq!(
        app.queue.len(),
        5,
        "the tracks in the folders not listed yet are found too"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}
