//! Integration tests for the background track analysis worker (issue #15) and the
//! ffmpeg robustness fixes in `BpmAnalyzer` (issue #19).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quickstream::analysis::bpm::BpmAnalyzer;
use quickstream::analysis::worker::AnalysisWorker;
use quickstream::analysis::{AnalysisDatabase, TrackFeatures};

const SAMPLE_RATE: u32 = 11025;

/// Unique scratch directory under the OS temp dir (never inside a real music library).
fn scratch_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "quickstream_analysis_{}_{}_{}",
        name,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write a mono 16-bit PCM WAV file.
fn write_wav(path: &Path, samples: &[i16]) {
    let data_len = (samples.len() * 2) as u32;
    let mut b: Vec<u8> = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    b.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, b).unwrap();
}

/// Click track: short 880 Hz bursts at `bpm` beats per minute.
fn click_track(bpm: f64, seconds: f64) -> Vec<i16> {
    let total = (SAMPLE_RATE as f64 * seconds) as usize;
    let mut samples = vec![0i16; total];
    let beat = (SAMPLE_RATE as f64 * 60.0 / bpm) as usize;
    let burst = (SAMPLE_RATE as f64 * 0.05) as usize;
    let mut start = 0;
    while start < total {
        for j in 0..burst {
            if start + j >= total {
                break;
            }
            let t = j as f64 / SAMPLE_RATE as f64;
            let env = 1.0 - j as f64 / burst as f64;
            samples[start + j] = (26_000.0 * env * (2.0 * std::f64::consts::PI * 880.0 * t).sin()) as i16;
        }
        start += beat;
    }
    samples
}

/// Write a short WAV carrying an ID3v2 TBPM / TKEY tag.
fn write_tagged_wav(path: &Path, bpm: &str, key: &str) {
    use lofty::config::WriteOptions;
    use lofty::prelude::*;
    use lofty::tag::{ItemKey, Tag, TagType};

    write_wav(path, &vec![0i16; SAMPLE_RATE as usize * 3]);
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title("Tagged Song".to_string());
    tag.set_artist("Tag Artist".to_string());
    tag.insert_text(ItemKey::IntegerBpm, bpm.to_string());
    tag.insert_text(ItemKey::InitialKey, key.to_string());
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

/// Snapshot of (path -> (size, mtime)) for every file under `dir`.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    let mut map = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let md = entry.metadata().unwrap();
        map.insert(entry.path(), (md.len(), md.modified().unwrap()));
    }
    map
}

// ---------------------------------------------------------------------------
// BpmAnalyzer robustness (#19)
// ---------------------------------------------------------------------------

#[test]
fn test_read_embedded_bpm_reads_id3_tbpm_frame() {
    let music = scratch_dir("tbpm");
    let track = music.join("tagged.wav");
    write_tagged_wav(&track, "128", "Am");

    assert_eq!(BpmAnalyzer::read_embedded_bpm(&track), Some(128.0));
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_acoustic_tempo_on_track_shorter_than_seek_offset() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let music = scratch_dir("short");
    let track = music.join("short_120.wav");
    write_wav(&track, &click_track(120.0, 12.0));

    let bpm = BpmAnalyzer::analyze_acoustic_tempo(&track)
        .expect("a 12s track must still be analysed (seek offset should fall back to 0)");
    assert!((115.0..=125.0).contains(&bpm), "expected ~120 BPM, got {}", bpm);

    let energy = BpmAnalyzer::calculate_energy(&track);
    assert!(energy.is_some(), "energy must be computed for a 12s track");
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_subprocess_timeout_kills_hung_child() {
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("30");
    let start = Instant::now();
    let out = BpmAnalyzer::run_with_timeout(cmd, Duration::from_millis(300), None);
    assert!(out.is_none(), "timed-out subprocess must yield no output");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "hung child must be killed promptly, took {:?}",
        start.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Background analysis worker (#15)
// ---------------------------------------------------------------------------

#[test]
fn test_worker_populates_db_from_embedded_tags() {
    let music = scratch_dir("worker_tags");
    let cache = scratch_dir("worker_tags_db");
    let track = music.join("tagged.wav");
    write_tagged_wav(&track, "128", "Am");

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 1);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(20)), "worker did not finish");

    let feat = db.get_track(&track).expect("track_features row must exist");
    assert_eq!(feat.bpm, Some(128.0));
    assert_eq!(feat.initial_key.as_deref(), Some("Am"));
    assert_eq!(feat.camelot_key.as_deref(), Some("8A"));
    assert_eq!(feat.title, "Tagged Song");
    assert_eq!(feat.artist, "Tag Artist");
    assert!(feat.analyzed_at > 0);
    assert_eq!(worker.progress().analyzed, 1);

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_analyses_untagged_short_track_acoustically() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let music = scratch_dir("worker_acoustic");
    let cache = scratch_dir("worker_acoustic_db");
    let track = music.join("untagged_120.wav");
    write_wav(&track, &click_track(120.0, 12.0));

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 1);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(60)), "worker did not finish");

    let feat = db.get_track(&track).expect("track_features row must exist");
    let bpm = feat.bpm.expect("acoustic BPM must be detected for a short untagged track");
    assert!((115.0..=125.0).contains(&bpm), "expected ~120 BPM, got {}", bpm);
    assert!(feat.energy.is_some(), "energy should be filled from the same decode");

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_skips_tracks_already_in_db() {
    let music = scratch_dir("worker_resume");
    let cache = scratch_dir("worker_resume_db");
    let track = music.join("tagged.wav");
    write_tagged_wav(&track, "128", "Am");

    let md = fs::metadata(&track).unwrap();
    let mtime = md.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let existing = TrackFeatures {
        filepath: track.clone(),
        mtime,
        file_size: md.len(),
        title: "Previously Analysed".to_string(),
        bpm: Some(99.0),
        analyzed_at: 1,
        ..Default::default()
    };
    db.upsert_track(&existing).unwrap();

    let mut worker = AnalysisWorker::spawn(db.clone(), 1);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(20)));

    let feat = db.get_track(&track).unwrap();
    assert_eq!(feat.bpm, Some(99.0), "up-to-date rows must not be re-analysed");
    assert_eq!(feat.title, "Previously Analysed");
    let p = worker.progress();
    assert_eq!(p.analyzed, 0);
    assert_eq!(p.skipped, 1);

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_never_writes_into_music_library() {
    let music = scratch_dir("worker_zero_touch");
    let cache = scratch_dir("worker_zero_touch_db");
    write_tagged_wav(&music.join("a.wav"), "100", "C");
    write_wav(&music.join("b.wav"), &click_track(90.0, 4.0));
    let before = snapshot(&music);

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 2);
    worker.enqueue(before.keys().cloned().collect::<Vec<_>>());
    assert!(worker.wait_until_idle(Duration::from_secs(60)));
    worker.shutdown();

    assert_eq!(snapshot(&music), before, "music library must be untouched");
    assert_eq!(db.count_analyzed(), 2);
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_shutdown_is_prompt_with_pending_work() {
    let music = scratch_dir("worker_shutdown");
    let cache = scratch_dir("worker_shutdown_db");
    let mut tracks = Vec::new();
    for i in 0..200 {
        let p = music.join(format!("t{}.wav", i));
        write_wav(&p, &click_track(120.0, 3.0));
        tracks.push(p);
    }

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 2);
    worker.enqueue(tracks);
    std::thread::sleep(Duration::from_millis(100));

    let start = Instant::now();
    worker.shutdown();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "shutdown took {:?}",
        start.elapsed()
    );
    assert!(db.count_analyzed() < 200, "worker should have stopped before finishing everything");
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

// ---------------------------------------------------------------------------
// App wiring: autopilot-discovered tracks are analysed in the background
// ---------------------------------------------------------------------------

#[test]
fn test_autopilot_tracks_are_analysed_in_background() {
    use quickstream::settings::SettingsManager;
    use quickstream::ui::AppUI;

    // Redirect the analysis DB / caches away from the real user cache.
    let cache = scratch_dir("ui_cache");
    std::env::set_var("XDG_CACHE_HOME", &cache);

    let music = scratch_dir("ui_music");
    let a = music.join("a.wav");
    let b = music.join("b.wav");
    write_tagged_wav(&a, "124", "8A");
    write_tagged_wav(&b, "126", "9A");

    let mut mgr = SettingsManager::new();
    mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(music.clone()), mgr);
    app.start_autopilot(music.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(2));
    assert_eq!(app.queue.len(), 2);

    assert!(app.analysis_worker.wait_until_idle(Duration::from_secs(20)));
    assert_eq!(app.analysis_worker.progress().analyzed, 2);
    assert_eq!(app.analysis_db.get_track(&a).and_then(|f| f.bpm), Some(124.0));
    assert_eq!(app.analysis_db.get_track(&b).and_then(|f| f.camelot_key), Some("9A".to_string()));
    assert!(cache.join("quickstream").join("track_analysis.db").exists());

    let _ = app.player.stop();
    drop(app);
    std::env::remove_var("XDG_CACHE_HOME");
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}
