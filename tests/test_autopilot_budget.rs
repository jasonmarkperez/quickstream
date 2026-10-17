//! Profile mixes stay bounded on large libraries with few matches (issue #38).
//!
//! Own test binary: points XDG_DATA_HOME (log location) at a temp dir for the whole process.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use quickstream::analysis::{AnalysisDatabase, MixProfile, MoodType};
use quickstream::library::{AutopilotEvent, LibraryManager, ProfileScanLimits};

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

/// A library of `folders` artist folders with `per_folder` non-matching tracks each, plus one
/// Chill-matching track at the top level. Built once per process.
fn sparse_library() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let root = std::env::temp_dir().join(format!("qs_autopilot_budget_{}", std::process::id()));
    ONCE.call_once(|| {
        let _ = fs::remove_dir_all(&root);
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        quickstream::logger::init();
        let library = root.join("library");
        for folder in 0..520 {
            for track in 0..3 {
                create_silent_wav(&library.join(format!("Band {:03}/Record/{:02} Song.wav", folder, track)));
            }
        }
        create_silent_wav(&library.join("Chill Intro.wav"));
    });
    root
}

struct MixRun {
    first_track_at: Option<Duration>,
    finished_at: Duration,
    log: String,
}

fn run_chill_mix(limits: ProfileScanLimits) -> MixRun {
    let root = sparse_library();
    let log_path = root.join("data").join("quickstream").join("quickstream.log");
    let log_before = fs::read_to_string(&log_path).unwrap_or_default().len();

    let (tx, rx) = mpsc::channel();
    let library = root.join("library");
    let start = Instant::now();
    let handle = std::thread::spawn(move || {
        LibraryManager::scan_directory_autopilot_with_profile_limits(
            library,
            Vec::new(),
            Default::default(),
            MixProfile::MoodVibe(MoodType::Chill),
            AnalysisDatabase::open_in_memory().unwrap(),
            tx,
            Arc::new(AtomicBool::new(false)),
            limits,
        )
    });

    let mut first_track_at = None;
    let finished_at = loop {
        match rx.recv_timeout(Duration::from_secs(120)).expect("scan finished") {
            AutopilotEvent::Track(_) => {
                first_track_at.get_or_insert(start.elapsed());
            }
            AutopilotEvent::Finished(_) => break start.elapsed(),
        }
    };
    handle.join().unwrap();
    let log = fs::read_to_string(&log_path).unwrap()[log_before..].to_string();
    MixRun { first_track_at, finished_at, log }
}

fn gathered_line(log: &str) -> String {
    log.lines()
        .find(|l| l.contains("[autopilot] candidates gathered"))
        .unwrap_or_else(|| panic!("no gathered line in:\n{}", log))
        .to_string()
}

#[test]
fn test_sparse_profile_scan_stops_at_folder_limit() {
    let run = run_chill_mix(ProfileScanLimits::default());
    let gathered = gathered_line(&run.log);
    assert!(gathered.contains("folders=500 "), "{}", gathered);
}

#[test]
fn test_profile_scan_stops_at_time_budget() {
    let run = run_chill_mix(ProfileScanLimits {
        time_budget: Duration::ZERO,
        ..ProfileScanLimits::default()
    });
    // At least one folder is always scanned so the mix can make progress.
    let gathered = gathered_line(&run.log);
    assert!(gathered.contains("folders=1 "), "{}", gathered);
}

#[test]
fn test_first_match_is_queued_before_gathering_finishes() {
    let run = run_chill_mix(ProfileScanLimits::default());
    let first = run.first_track_at.expect("at least one track");
    assert!(
        first < run.finished_at / 4,
        "first track after {:?}, scan finished after {:?}",
        first,
        run.finished_at
    );
}

#[test]
fn test_sequencing_after_queued_track_keeps_artist_spacing() {
    use quickstream::analysis::mixer::DjMixer;
    use quickstream::metadata::AudioMetadata;
    let track = |artist: &str, n: usize| AudioMetadata {
        filepath: PathBuf::from(format!("/music/{}/{}.flac", artist, n)),
        title: format!("{} {}", artist, n),
        artist: artist.to_string(),
        album: format!("{} album", artist),
        ..Default::default()
    };
    let queued = track("Alpha", 0);
    let candidates = vec![track("Alpha", 1), track("Beta", 1), track("Gamma", 1)];
    let db = AnalysisDatabase::open_in_memory().unwrap();

    let rest = DjMixer::sequence_mix_after(Some(&queued), candidates, &db, 3);
    assert_eq!(rest.len(), 3);
    assert!(rest.iter().all(|t| t.filepath != queued.filepath), "queued track is not repeated");
    assert_ne!(rest[0].artist, "Alpha", "next track must not repeat the queued track's artist");
}
