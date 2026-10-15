use std::fs::{self, File};
use std::io::Write;

use quickstream::metadata::{extract_metadata, is_supported_audio_file};

fn create_dummy_wav(path: &std::path::Path) {
    let mut file = File::create(path).unwrap();
    let sample_rate: u32 = 44100;
    let channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * ((bits_per_sample as u32) / 8);
    let block_align = channels * (bits_per_sample / 8);
    let num_samples: u32 = 44100 * 2; // 2 seconds
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
fn test_extract_metadata_real_audio() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_meta_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let wav_file = temp_dir.join("test_audio.wav");
    create_dummy_wav(&wav_file);

    assert!(is_supported_audio_file(&wav_file));

    let meta = extract_metadata(&wav_file).expect("Should extract metadata from valid WAV file");
    assert_eq!(meta.format_name, "WAV");
    assert_eq!(meta.channels, 2);
    assert_eq!(meta.sample_rate_hz, 44100);
    assert!((meta.duration_seconds - 2.0).abs() < 0.1);
    assert_eq!(meta.formatted_duration(), "00:02");

    // Display labels
    let label_tags = meta.format_display_label(true);
    let label_files = meta.format_display_label(false);

    assert!(label_tags.contains("[WAV]"));
    assert!(label_tags.contains("(00:02)"));
    assert_eq!(label_files, "test_audio.wav");
    assert_ne!(label_tags, label_files);

    // Queue labels should NOT contain file type / format badge
    let q_tags = meta.format_queue_label(true);
    let q_files = meta.format_queue_label(false);
    assert!(!q_tags.contains("[WAV]"));
    assert!(!q_files.contains("[WAV]"));
    assert!(q_tags.contains("(00:02)"));
    assert_eq!(q_files, "test_audio.wav (00:02)");

    let _ = fs::remove_dir_all(&temp_dir);
}
