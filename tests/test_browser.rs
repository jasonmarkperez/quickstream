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
