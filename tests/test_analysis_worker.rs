//! Integration tests for the background track analysis worker (issue #15) and the
//! ffmpeg robustness fixes in `BpmAnalyzer` (issue #19).

mod common;

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
    common::isolate();
    let music = scratch_dir("tbpm");
    let track = music.join("tagged.wav");
    write_tagged_wav(&track, "128", "Am");

    assert_eq!(BpmAnalyzer::read_embedded_bpm(&track), Some(128.0));
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_acoustic_tempo_on_track_shorter_than_seek_offset() {
    common::isolate();
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
    common::isolate();
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
    common::isolate();
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
    common::isolate();
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
    let bpm = feat
        .bpm
        .expect("acoustic BPM must be detected for a short untagged track");
    assert!((115.0..=125.0).contains(&bpm), "expected ~120 BPM, got {}", bpm);
    assert!(feat.energy.is_some(), "energy should be filled from the same decode");

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_skips_tracks_already_in_db() {
    common::isolate();
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
    common::isolate();
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
    common::isolate();
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
    assert!(
        db.count_analyzed() < 200,
        "worker should have stopped before finishing everything"
    );
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

// ---------------------------------------------------------------------------
// App wiring: autopilot-discovered tracks are analysed in the background
// ---------------------------------------------------------------------------

#[test]
fn test_autopilot_tracks_are_analysed_in_background() {
    common::isolate();
    use quickstream::settings::SettingsManager;
    use quickstream::ui::AppUI;

    // `common::isolate()` points XDG_CACHE_HOME (analysis DB, library cache) at a temp dir.
    let cache = common::isolate().join("cache");

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
    assert_eq!(
        app.analysis_db.get_track(&b).and_then(|f| f.camelot_key),
        Some("9A".to_string())
    );
    assert!(cache.join("quickstream").join("track_analysis.db").exists());

    let _ = app.player.stop();
    drop(app);
    let _ = fs::remove_dir_all(&music);
}

// ---------------------------------------------------------------------------
// Failed acoustic analyses must not be persisted as up to date (PR #32 review)
// ---------------------------------------------------------------------------

use quickstream::analysis::bpm::FfmpegConfig;

/// A file with an audio extension whose contents ffmpeg cannot decode (and lofty cannot tag).
fn write_undecodable_mp3(path: &Path) {
    fs::write(path, "this is not audio at all\n".repeat(4000)).unwrap();
}

/// Fake `ffmpeg` executable: appends its arguments to `log` (one line per invocation), then
/// runs `body`. Written with [`common::fake_exec::write_script`], so it starts even while other
/// test threads spawn processes (issue #71, #80).
#[cfg(unix)]
fn fake_ffmpeg(dir: &Path, log: &Path, body: &str) -> PathBuf {
    let script = dir.join("fake-ffmpeg");
    let text = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{}\n", log.display(), body);
    common::fake_exec::write_script(&script, &text);
    script
}

/// Bytes in 10 s of mono s16le PCM at [`SAMPLE_RATE`]: enough audio for a decode to count.
const TEN_SECONDS_PCM_BYTES: usize = SAMPLE_RATE as usize * 2 * 10;

/// A [`fake_ffmpeg`] that runs `at_offset` when called with `-ss <offset>` and otherwise
/// answers at once with [`TEN_SECONDS_PCM_BYTES`] of silence. A call without `-ss` counts as
/// offset 0. Values are compared as numbers (`30`, `30.0` and `030` are all 30 s); a value that
/// isn't a plain number (`00:00:30`) answers too, so a call at any offset but `offset` succeeds
/// rather than stalling or failing.
#[cfg(unix)]
fn fake_ffmpeg_answering_except_at(dir: &Path, log: &Path, offset: u32, at_offset: &str) -> PathBuf {
    let body = format!(
        "ss=0\nprev=\nfor arg in \"$@\"; do\n  [ \"$prev\" = -ss ] && ss=$arg\n  prev=$arg\ndone\n\
         if awk -v v=\"$ss\" 'BEGIN {{ exit !(v ~ /^[0-9]+(\\.[0-9]*)?$/ && v + 0 == {}) }}'; then\n  \
         {}\nfi\nhead -c {} /dev/zero\nexit 0",
        offset, at_offset, TEN_SECONDS_PCM_BYTES
    );
    fake_ffmpeg(dir, log, &body)
}

/// The arguments of each logged invocation of a [`fake_ffmpeg`].
#[cfg(unix)]
fn calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The `-ss` offset of each logged invocation, in seconds: 0 when it has none, `None` when its
/// value isn't a number.
#[cfg(unix)]
fn seek_offsets(log: &Path) -> Vec<Option<f64>> {
    calls(log)
        .iter()
        .map(|call| {
            let mut args = call.split_whitespace();
            match args.position(|a| a == "-ss") {
                Some(_) => args.next().and_then(|v| v.parse().ok()),
                None => Some(0.0),
            }
        })
        .collect()
}

#[cfg(unix)]
fn invocations(log: &Path) -> usize {
    calls(log).len()
}

#[test]
fn test_worker_does_not_persist_when_ffmpeg_fails() {
    common::isolate();
    if !ffmpeg_available() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let music = scratch_dir("worker_ffmpeg_fail");
    let cache = scratch_dir("worker_ffmpeg_fail_db");
    let track = music.join("broken.mp3");
    write_undecodable_mp3(&track);

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 1);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(60)), "worker did not finish");

    assert!(
        db.get_track(&track).is_none(),
        "a failed acoustic analysis must not be stored"
    );
    let p = worker.progress();
    assert_eq!(p.failed, 1);
    assert_eq!(p.analyzed, 0);
    assert_eq!(p.skipped, 0);

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_retries_in_later_session_when_ffmpeg_was_missing() {
    common::isolate();
    let music = scratch_dir("worker_ffmpeg_missing");
    let cache = scratch_dir("worker_ffmpeg_missing_db");
    let track = music.join("untagged_120.wav");
    write_wav(&track, &click_track(120.0, 12.0));
    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();

    // Session 1: ffmpeg is not installed.
    let missing = FfmpegConfig {
        binary: music.join("no-such-ffmpeg"),
        ..FfmpegConfig::default()
    };
    let mut worker = AnalysisWorker::spawn_with_ffmpeg(db.clone(), 1, missing);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(20)), "worker did not finish");
    assert!(
        db.get_track(&track).is_none(),
        "missing ffmpeg must not mark the track as analysed"
    );
    let p = worker.progress();
    assert_eq!((p.failed, p.analyzed, p.skipped), (1, 0, 0));
    worker.shutdown();

    // Session 2: the user has installed ffmpeg; the track is retried, not skipped.
    if ffmpeg_available() {
        let mut worker = AnalysisWorker::spawn(db.clone(), 1);
        worker.enqueue(vec![track.clone()]);
        assert!(worker.wait_until_idle(Duration::from_secs(60)), "worker did not finish");
        let p = worker.progress();
        assert_eq!((p.failed, p.analyzed, p.skipped), (0, 1, 0));
        let bpm = db
            .get_track(&track)
            .and_then(|f| f.bpm)
            .expect("BPM must be filled on retry");
        assert!((115.0..=125.0).contains(&bpm), "expected ~120 BPM, got {}", bpm);
        worker.shutdown();
    }

    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[cfg(unix)]
#[test]
fn test_worker_does_not_persist_when_ffmpeg_produces_no_audio() {
    common::isolate();
    let music = scratch_dir("worker_ffmpeg_empty");
    let cache = scratch_dir("worker_ffmpeg_empty_db");
    let track = music.join("untagged.wav");
    write_wav(&track, &click_track(120.0, 12.0));
    let log = cache.join("calls.log");
    let ffmpeg = fake_ffmpeg(&cache, &log, "exit 0");

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let cfg = FfmpegConfig {
        binary: ffmpeg,
        ..FfmpegConfig::default()
    };
    let mut worker = AnalysisWorker::spawn_with_ffmpeg(db.clone(), 1, cfg);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(20)), "worker did not finish");

    assert!(invocations(&log) >= 1, "fake ffmpeg should have been invoked");
    assert!(db.get_track(&track).is_none(), "an empty decode must not be stored");
    let p = worker.progress();
    assert_eq!((p.failed, p.analyzed), (1, 0));

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_persists_silent_track_without_bpm() {
    common::isolate();
    if !ffmpeg_available() {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let music = scratch_dir("worker_silence");
    let cache = scratch_dir("worker_silence_db");
    let track = music.join("silence.wav");
    write_wav(&track, &vec![0i16; SAMPLE_RATE as usize * 12]);

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 1);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(60)), "worker did not finish");

    let feat = db
        .get_track(&track)
        .expect("a successful decode with no detectable tempo is a real result and must be stored");
    assert_eq!(feat.bpm, None);
    assert_eq!(feat.energy, Some(0.0));
    let p = worker.progress();
    assert_eq!((p.failed, p.analyzed), (0, 1));

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn test_worker_persists_tagged_track_even_when_ffmpeg_missing() {
    common::isolate();
    let music = scratch_dir("worker_tagged_no_ffmpeg");
    let cache = scratch_dir("worker_tagged_no_ffmpeg_db");
    let track = music.join("tagged.wav");
    write_tagged_wav(&track, "128", "Am");

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let cfg = FfmpegConfig {
        binary: music.join("no-such-ffmpeg"),
        ..FfmpegConfig::default()
    };
    let mut worker = AnalysisWorker::spawn_with_ffmpeg(db.clone(), 1, cfg);
    worker.enqueue(vec![track.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(20)), "worker did not finish");

    assert_eq!(db.get_track(&track).and_then(|f| f.bpm), Some(128.0));
    let p = worker.progress();
    assert_eq!((p.failed, p.analyzed), (0, 1));

    worker.shutdown();
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[cfg(unix)]
#[test]
fn test_decode_does_not_retry_from_zero_after_ffmpeg_failure() {
    common::isolate();
    let dir = scratch_dir("decode_no_retry_fail");
    let track = dir.join("track.wav");
    write_wav(&track, &click_track(120.0, 12.0));
    let log = dir.join("calls.log");
    let cfg = FfmpegConfig {
        binary: fake_ffmpeg(&dir, &log, "exit 1"),
        ..FfmpegConfig::default()
    };

    let out = BpmAnalyzer::decode_pcm_slice_with(&cfg, &track, 30, 20, SAMPLE_RATE, None);
    assert!(out.is_none());
    assert_eq!(
        invocations(&log),
        1,
        "a failed decode must not be retried from offset 0"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn test_decode_does_not_retry_from_zero_after_timeout() {
    common::isolate();
    let dir = scratch_dir("decode_no_retry_timeout");
    let track = dir.join("track.wav");
    write_wav(&track, &click_track(120.0, 12.0));
    let log = dir.join("calls.log");
    // Stalls at the 30 s seek and answers at once at any other offset, so a retry from 0 (or
    // from anywhere else) would make the decode succeed.
    let cfg = FfmpegConfig {
        binary: fake_ffmpeg_answering_except_at(&dir, &log, 30, "exec sleep 30"),
        // Nothing waits on the fake making progress (its log line may be missing: under load it
        // can be killed before writing it). The timeout only has to exceed what the kill, reap
        // and stdout join add on top of it, so that `elapsed < 2 * timeout` holds under load.
        timeout: Duration::from_millis(1500),
    };

    let start = Instant::now();
    let out = BpmAnalyzer::decode_pcm_slice_with(&cfg, &track, 30, 20, SAMPLE_RATE, None);
    let elapsed = start.elapsed();
    let logged = calls(&log);
    // Proof the timeout path ran: a fake that failed to start, or exited, returns at once.
    assert!(
        elapsed >= cfg.timeout,
        "the stalled decode must run until its timeout, returned after {:?} (calls: {:?})",
        elapsed,
        logged
    );
    assert!(
        out.is_none(),
        "a timed-out decode must give no audio; a retry would have (calls: {:?})",
        logged
    );
    assert!(
        seek_offsets(&log).iter().all(|o| *o == Some(30.0)),
        "every decode must seek to 30 s, none retried from another offset (calls: {:?})",
        logged
    );
    assert!(logged.len() <= 1, "expected one decode at most, got {:?}", logged);
    // A second decode that also stalled would cost a second timeout.
    assert!(
        elapsed < 2 * cfg.timeout,
        "a stalled file must cost at most one timeout, took {:?}",
        elapsed
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn test_decode_retries_from_zero_when_first_decode_is_too_short() {
    common::isolate();
    let dir = scratch_dir("decode_retry_short");
    let track = dir.join("track.wav");
    write_wav(&track, &click_track(120.0, 12.0));
    let log = dir.join("calls.log");
    // Succeeds with no audio at the seek offset (track shorter than offset); 10s of audio from 0.
    let cfg = FfmpegConfig {
        binary: fake_ffmpeg_answering_except_at(&dir, &log, 30, "exit 0"),
        ..FfmpegConfig::default()
    };

    let out = BpmAnalyzer::decode_pcm_slice_with(&cfg, &track, 30, 20, SAMPLE_RATE, None)
        .expect("a short first decode must be retried from offset 0");
    assert_eq!(out.len(), TEN_SECONDS_PCM_BYTES / 2);
    assert_eq!(seek_offsets(&log), vec![Some(30.0), Some(0.0)]);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The autopilot candidate pool (#30): queued tracks first, candidates bounded
// ---------------------------------------------------------------------------

use quickstream::analysis::worker::{AnalysisQueue, MAX_PENDING_CANDIDATES};
use quickstream::library::AutopilotEvent;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|n| PathBuf::from(format!("/music/{n}.flac")))
        .collect()
}

fn path(name: &str) -> PathBuf {
    paths(&[name]).remove(0)
}

fn drain(queue: &mut AnalysisQueue) -> Vec<PathBuf> {
    std::iter::from_fn(|| queue.pop()).collect()
}

#[test]
fn queued_tracks_are_analysed_before_candidates() {
    common::isolate();
    let mut queue = AnalysisQueue::new(10);
    for p in paths(&["c1", "c2", "c3"]) {
        assert!(queue.push_candidate(p));
    }
    for p in paths(&["t1", "t2"]) {
        assert!(queue.push_queued(p));
    }
    assert_eq!(queue.len(), 5);
    assert_eq!(drain(&mut queue), paths(&["t1", "t2", "c1", "c2", "c3"]));
    assert!(queue.is_empty());
}

#[test]
fn a_candidate_that_reaches_the_queue_moves_ahead_once() {
    common::isolate();
    let mut queue = AnalysisQueue::new(10);
    for p in paths(&["c1", "c2", "c3"]) {
        queue.push_candidate(p);
    }
    // Already pending as a candidate: promoted, not added twice.
    assert!(!queue.push_queued(path("c3")));
    assert_eq!(queue.len(), 3);
    assert_eq!(drain(&mut queue), paths(&["c3", "c1", "c2"]));
}

#[test]
fn a_path_is_never_queued_twice_in_a_session() {
    common::isolate();
    let mut queue = AnalysisQueue::new(10);
    let (t, c) = (path("t"), path("c"));
    assert!(queue.push_queued(t.clone()));
    assert!(!queue.push_queued(t.clone()));
    assert!(
        !queue.push_candidate(t.clone()),
        "a queued track is not a candidate too"
    );
    assert!(queue.push_candidate(c.clone()));
    assert!(!queue.push_candidate(c.clone()));
    assert_eq!(drain(&mut queue), vec![t.clone(), c.clone()]);
    // Analysed (or being analysed) this session: offered again, ignored.
    assert!(!queue.push_queued(t));
    assert!(!queue.push_candidate(c));
    assert!(queue.is_empty());
}

#[test]
fn candidates_are_bounded_and_the_oldest_give_way() {
    common::isolate();
    let mut queue = AnalysisQueue::new(3);
    for p in paths(&["c1", "c2", "c3", "c4", "c5"]) {
        queue.push_candidate(p);
    }
    assert_eq!(queue.len(), 3, "candidates never exceed the bound");
    // A dropped candidate may be offered again by a later run.
    assert!(queue.push_candidate(path("c1")));
    // Queued tracks are never dropped, however many candidates arrive.
    for p in paths(&["t1", "t2", "t3", "t4"]) {
        queue.push_queued(p);
    }
    for p in paths(&["c6", "c7"]) {
        queue.push_candidate(p);
    }
    assert_eq!(drain(&mut queue), paths(&["t1", "t2", "t3", "t4", "c1", "c6", "c7"]));
}

#[test]
fn a_burst_of_candidates_keeps_the_worker_queue_bounded() {
    common::isolate();
    let cache = scratch_dir("worker_burst_db");
    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db, 1);
    let burst: Vec<PathBuf> = (0..MAX_PENDING_CANDIDATES * 3)
        .map(|i| cache.join(format!("missing-{i}.flac")))
        .collect();
    worker.enqueue_candidates(burst);
    // At most the bound waiting, plus the one being analysed.
    assert!(worker.progress().pending <= MAX_PENDING_CANDIDATES + 1);
    assert!(worker.wait_until_idle(Duration::from_secs(20)));
    worker.shutdown();
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn the_worker_analyses_candidates_into_the_database() {
    common::isolate();
    let music = scratch_dir("worker_candidates");
    let cache = scratch_dir("worker_candidates_db");
    let a = music.join("a.wav");
    let b = music.join("b.wav");
    write_tagged_wav(&a, "122", "8A");
    write_tagged_wav(&b, "90", "3B");
    let before = snapshot(&music);

    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn(db.clone(), 1);
    worker.enqueue_candidates(vec![a.clone(), b.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(20)));
    assert_eq!(worker.progress().analyzed, 2);
    assert_eq!(db.get_track(&a).and_then(|f| f.bpm), Some(122.0));
    assert_eq!(db.get_track(&b).and_then(|f| f.bpm), Some(90.0));
    worker.shutdown();

    assert_eq!(snapshot(&music), before, "music library must be untouched");
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

/// The order a fake ffmpeg was first called on each of `tracks`, from its argument log.
#[cfg(unix)]
fn first_calls(log: &Path, tracks: &[PathBuf]) -> Vec<PathBuf> {
    let text = fs::read_to_string(log).unwrap_or_default();
    let mut order: Vec<(usize, PathBuf)> = tracks
        .iter()
        .filter_map(|t| {
            let name = t.to_string_lossy().to_string();
            text.lines().position(|l| l.contains(&name)).map(|i| (i, t.clone()))
        })
        .collect();
    order.sort();
    order.into_iter().map(|(_, t)| t).collect()
}

#[cfg(unix)]
#[test]
fn a_track_queued_during_a_candidate_burst_is_analysed_next() {
    common::isolate();
    let music = scratch_dir("worker_priority");
    let cache = scratch_dir("worker_priority_db");
    let candidates: Vec<PathBuf> = (1..=4).map(|i| music.join(format!("cand{i}.wav"))).collect();
    let queued = music.join("queued.wav");
    for t in candidates.iter().chain([&queued]) {
        write_wav(t, &click_track(120.0, 2.0));
    }
    let log = cache.join("calls.log");
    let args = cache.join("args.log");
    // Each call takes a moment, so the queued track arrives while a candidate is analysed.
    let body = format!("printf '%s\\n' \"$*\" >> '{}'\nsleep 0.2\nexit 0", args.display());
    let cfg = FfmpegConfig {
        binary: fake_ffmpeg(&cache, &log, &body),
        ..FfmpegConfig::default()
    };
    let db = AnalysisDatabase::open_at(&cache.join("track_analysis.db")).unwrap();
    let mut worker = AnalysisWorker::spawn_with_ffmpeg(db, 1, cfg);
    worker.enqueue_candidates(candidates.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    while invocations(&log) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    worker.enqueue(vec![queued.clone()]);
    assert!(worker.wait_until_idle(Duration::from_secs(30)));
    worker.shutdown();

    let all: Vec<PathBuf> = candidates.iter().cloned().chain([queued.clone()]).collect();
    let order = first_calls(&args, &all);
    assert_eq!(order.len(), 5, "every track reached ffmpeg: {order:?}");
    assert_eq!(order[0], candidates[0], "the candidate already started comes first");
    assert_eq!(
        order[1], queued,
        "the queued track goes before the other candidates: {order:?}"
    );
    let _ = fs::remove_dir_all(&music);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn autopilot_candidates_reach_the_analysis_worker_but_not_the_queue() {
    common::isolate();

    let music = scratch_dir("ui_candidates");
    let a = music.join("a.wav");
    let b = music.join("b.wav");
    write_tagged_wav(&a, "118", "8A");
    write_tagged_wav(&b, "119", "9A");

    let mut mgr = SettingsManager::new();
    mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(music.clone()), mgr);
    let (tx, rx) = std::sync::mpsc::channel();
    app.autopilot_rx = Some(rx);
    app.is_autopilot_scanning = true;
    tx.send(AutopilotEvent::Candidates(vec![a.clone(), b.clone()])).unwrap();
    tx.send(AutopilotEvent::Finished(0)).unwrap();
    app.poll_background_autopilot();

    assert!(app.queue.is_empty(), "candidates are analysed, not queued");
    assert!(app.analysis_worker.wait_until_idle(Duration::from_secs(20)));
    assert_eq!(app.analysis_worker.progress().analyzed, 2);
    assert_eq!(app.analysis_db.get_track(&a).and_then(|f| f.bpm), Some(118.0));
    let _ = fs::remove_dir_all(&music);
}
