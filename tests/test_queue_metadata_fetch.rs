//! Queue metadata is resolved at most once per track per session (PR #32 review).
//!
//! The XDG dirs point at a temp dir for the whole process (`common::isolate()`).

mod common;

use std::time::{Duration, Instant};

use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

fn write_silent_wav(path: &std::path::Path) {
    let samples: u32 = 44_100;
    let data_len = samples * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&44_100u32.to_le_bytes());
    bytes.extend_from_slice(&88_200u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(bytes.len() + data_len as usize, 0);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn test_unresolvable_queue_track_is_read_once_per_session() {
    let root = common::isolate();
    // Untagged file directly under a "Music" folder: tags and path heuristics give no artist,
    // so it never leaves the "needs metadata" set.
    let music = root.join("Music");
    std::fs::create_dir_all(&music).unwrap();
    let song = music.join("song.wav");
    write_silent_wav(&song);

    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(None, settings_mgr);
    app.queue.add_track(quickstream::metadata::fallback_metadata(&song));

    for _ in 0..5 {
        app.start_queue_metadata_fetch();
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            app.poll_background_metadata();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let index = root.join("cache").join("quickstream").join("music_index.jsonl");
    let lines = std::fs::read_to_string(&index).unwrap_or_default();
    let appended = lines.lines().filter(|l| l.contains("song.wav")).count();
    assert_eq!(
        appended, 1,
        "track was read and appended to the cache {} times",
        appended
    );
}
