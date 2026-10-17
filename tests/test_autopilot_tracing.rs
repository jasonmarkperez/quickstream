//! Autopilot mix generation with a profile logs where its time goes.
//!
//! Own test binary: it initializes the process-wide logger in the isolated XDG data dir.

mod common;

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use quickstream::analysis::{AnalysisDatabase, MixProfile, MoodType};
use quickstream::library::{AutopilotEvent, LibraryManager};

fn create_silent_wav(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let data_len = 1000u32;
    let mut hdr = Vec::new();
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&(36 + data_len).to_le_bytes());
    hdr.extend_from_slice(b"WAVEfmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes());
    hdr.extend_from_slice(&2u16.to_le_bytes());
    hdr.extend_from_slice(&44100u32.to_le_bytes());
    hdr.extend_from_slice(&(44100u32 * 4).to_le_bytes());
    hdr.extend_from_slice(&4u16.to_le_bytes());
    hdr.extend_from_slice(&16u16.to_le_bytes());
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&data_len.to_le_bytes());
    let mut file = File::create(path).unwrap();
    file.write_all(&hdr).unwrap();
    file.write_all(&vec![0u8; data_len as usize]).unwrap();
}

#[test]
fn test_profile_mix_logs_phase_timings() {
    let root = common::isolate();
    let log_path = quickstream::logger::init();

    let library = root.join("library");
    for artist in 0..6 {
        for track in 0..3 {
            create_silent_wav(&library.join(format!("Artist {}/Album/{:02} Song.wav", artist, track)));
        }
    }

    let (tx, rx) = mpsc::channel();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    LibraryManager::scan_directory_autopilot_with_profile(
        library.clone(),
        Vec::new(),
        Default::default(),
        MixProfile::MoodVibe(MoodType::Chill),
        db,
        tx,
        Arc::new(AtomicBool::new(false)),
    );
    let mut finished = false;
    while let Ok(event) = rx.recv_timeout(Duration::from_secs(5)) {
        if let AutopilotEvent::Finished(_) = event {
            finished = true;
            break;
        }
    }
    assert!(finished);

    assert!(quickstream::logger::flush(), "log writer did not flush");
    let log = fs::read_to_string(&log_path).unwrap();
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("[autopilot]")).collect();
    let find = |needle: &str| {
        lines
            .iter()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no '{}' line in autopilot log:\n{}", needle, lines.join("\n")))
            .to_string()
    };

    let start = find("mix start");
    assert!(start.contains("profile=\"Mood: Chill"), "{}", start);
    assert!(start.contains("subdirs=6"), "{}", start);

    let gathered = find("candidates gathered");
    assert!(gathered.contains("folders=6"), "{}", gathered);
    assert!(gathered.contains("files=18"), "{}", gathered);
    for field in ["walk=", "metadata=", "tag_reads=", "cache_hits=", "db_hits=", "matching=", "matched="] {
        assert!(gathered.contains(field), "missing {} in: {}", field, gathered);
    }

    let sequenced = find("sequenced");
    assert!(sequenced.contains("candidates="), "{}", sequenced);

    let total = find("mix complete");
    assert!(total.contains("total="), "{}", total);
}
