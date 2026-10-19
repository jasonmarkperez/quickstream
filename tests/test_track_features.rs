//! `ui::features::TrackFeaturesWorker`: the current track's analysis features (BPM, key, energy)
//! are read on a worker thread, never on the UI thread, and the web state reports them from the
//! worker's latest result.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quickstream::analysis::TrackFeatures;
use quickstream::metadata::AudioMetadata;
use quickstream::settings::SettingsManager;
use quickstream::ui::features::{FeatureSource, TrackFeaturesWorker, RETRY_INTERVAL};
use quickstream::ui::workers::WorkerRegistry;
use quickstream::ui::AppUI;

fn features(path: &Path, bpm: f64) -> TrackFeatures {
    TrackFeatures {
        filepath: path.to_path_buf(),
        bpm: Some(bpm),
        camelot_key: Some("8A".to_string()),
        initial_key: Some("Am".to_string()),
        energy: Some(0.4),
        ..TrackFeatures::default()
    }
}

/// Poll `worker` until `cond` holds or 3 seconds pass.
fn poll_until(worker: &mut TrackFeaturesWorker, cond: impl Fn(&TrackFeaturesWorker) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        worker.poll();
        if cond(worker) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A source that knows `bpm` for every path and counts its calls.
fn counting_source(bpm: f64) -> (FeatureSource, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let source: FeatureSource = Arc::new(move |path: &Path| {
        counter.fetch_add(1, Ordering::SeqCst);
        Some(features(path, bpm))
    });
    (source, calls)
}

#[test]
fn test_worker_returns_features_for_the_current_track() {
    common::isolate();
    let mut workers = WorkerRegistry::new();
    let (source, calls) = counting_source(124.0);
    let mut worker = TrackFeaturesWorker::spawn(&mut workers, source);
    assert!(worker.features().is_none());

    let track = PathBuf::from("/music/a.flac");
    worker.follow(Some(&track));
    assert!(poll_until(&mut worker, |w| w.features().is_some()));
    let feat = worker.features().unwrap();
    assert_eq!(feat.filepath, track);
    assert_eq!(feat.bpm, Some(124.0));

    // Following the same track again reads nothing more.
    for _ in 0..5 {
        worker.follow(Some(&track));
        worker.poll();
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // No track: nothing to show.
    worker.follow(None);
    assert!(worker.features().is_none());
    worker.stop();
    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
}

#[test]
fn test_result_for_a_track_that_is_no_longer_current_is_dropped() {
    common::isolate();
    let mut workers = WorkerRegistry::new();
    let a = PathBuf::from("/music/a.flac");
    let b = PathBuf::from("/music/b.flac");
    // Reading A waits until the test opens the gate, by which time B is current.
    let gate = Arc::new(Mutex::new(()));
    let held = gate.lock().unwrap();
    let a_started = Arc::new(AtomicUsize::new(0));
    let (gate2, a_started2, a2) = (gate.clone(), a_started.clone(), a.clone());
    let source: FeatureSource = Arc::new(move |path: &Path| {
        if path == a2 {
            a_started2.fetch_add(1, Ordering::SeqCst);
            let _open = gate2.lock().unwrap();
            Some(features(path, 90.0))
        } else {
            Some(features(path, 128.0))
        }
    });
    let mut worker = TrackFeaturesWorker::spawn(&mut workers, source);

    worker.follow(Some(&a));
    let deadline = Instant::now() + Duration::from_secs(3);
    while a_started.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(a_started.load(Ordering::SeqCst), 1, "A was never read");
    worker.follow(Some(&b));
    drop(held);

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        worker.poll();
        if let Some(feat) = worker.features() {
            assert_eq!(feat.filepath, b, "A's late result must not show for B");
            assert_eq!(feat.bpm, Some(128.0));
            break;
        }
        assert!(Instant::now() < deadline, "B's features never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A's result is not held back to show later either.
    std::thread::sleep(Duration::from_millis(50));
    worker.poll();
    assert_eq!(worker.features().unwrap().filepath, b);
    worker.stop();
    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
}

#[test]
fn test_worker_retries_while_there_are_no_features_yet() {
    common::isolate();
    let mut workers = WorkerRegistry::new();
    // The analysis worker finishes the track after the first two reads.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let source: FeatureSource =
        Arc::new(move |path: &Path| (counter.fetch_add(1, Ordering::SeqCst) >= 2).then(|| features(path, 100.0)));
    let mut worker = TrackFeaturesWorker::spawn_with_retry(&mut workers, source, Duration::from_millis(20));

    worker.follow(Some(Path::new("/music/late.flac")));
    assert!(poll_until(&mut worker, |w| w.features().is_some()));
    assert_eq!(worker.features().unwrap().bpm, Some(100.0));
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    // Once found, the track is not read again.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    worker.stop();
    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
}

#[test]
fn test_default_retry_waits_five_seconds_between_reads() {
    common::isolate();
    assert_eq!(RETRY_INTERVAL, Duration::from_secs(5));
    let mut workers = WorkerRegistry::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let source: FeatureSource = Arc::new(move |_: &Path| {
        counter.fetch_add(1, Ordering::SeqCst);
        None
    });
    let mut worker = TrackFeaturesWorker::spawn(&mut workers, source);
    worker.follow(Some(Path::new("/music/unanalysed.flac")));
    std::thread::sleep(Duration::from_millis(300));
    worker.poll();
    assert!(worker.features().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no busy retry loop");

    // Stopping does not wait out the retry interval.
    let start = Instant::now();
    worker.stop();
    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
    assert!(start.elapsed() < Duration::from_secs(1), "took {:?}", start.elapsed());
}

#[test]
fn test_slow_source_never_blocks_the_caller() {
    common::isolate();
    let mut workers = WorkerRegistry::new();
    let source: FeatureSource = Arc::new(|path: &Path| {
        std::thread::sleep(Duration::from_millis(400));
        Some(features(path, 140.0))
    });
    let mut worker = TrackFeaturesWorker::spawn(&mut workers, source);

    let start = Instant::now();
    for n in 0..20 {
        worker.follow(Some(&PathBuf::from(format!("/music/{}.flac", n % 2))));
        worker.poll();
        let _ = worker.features();
    }
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "took {:?}",
        start.elapsed()
    );

    worker.follow(Some(Path::new("/music/1.flac")));
    assert!(poll_until(&mut worker, |w| w.features().is_some()));
    assert_eq!(worker.features().unwrap().filepath, PathBuf::from("/music/1.flac"));
    worker.stop();
    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
}

#[test]
fn test_worker_shuts_down_when_stopped_or_dropped() {
    common::isolate();
    let mut workers = WorkerRegistry::new();
    let (source, _) = counting_source(120.0);
    let mut stopped = TrackFeaturesWorker::spawn(&mut workers, source.clone());
    stopped.follow(Some(Path::new("/music/a.flac")));
    stopped.stop();
    // A stopped worker ignores further tracks instead of failing.
    stopped.follow(Some(Path::new("/music/b.flac")));
    stopped.poll();

    let idle = TrackFeaturesWorker::spawn(&mut workers, source);
    drop(idle);

    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
    assert_eq!(workers.live_count(), 0);
}

// ---------------------------------------------------------------------------------------------
// AppUI
// ---------------------------------------------------------------------------------------------

fn track(path: &Path) -> AudioMetadata {
    AudioMetadata {
        filepath: path.to_path_buf(),
        title: "Halcyon Drift".to_string(),
        artist: "Mira Lenz".to_string(),
        ..AudioMetadata::default()
    }
}

fn web_app() -> AppUI {
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = true;
    settings_mgr.settings.web_server_lan = false;
    settings_mgr.settings.web_server_port = 0;
    let app = AppUI::new(Some(std::env::temp_dir()), settings_mgr);
    assert!(app.web_server.is_some(), "web server did not start");
    app
}

/// Run `update_web_state` until `cond` holds for the web state or 3 seconds pass.
fn web_state_until(app: &mut AppUI, cond: impl Fn(&quickstream::web::NowPlayingState) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        app.update_web_state();
        if cond(&app.web_state.read().unwrap()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn test_web_state_reports_bpm_and_key_from_the_worker() {
    common::isolate();
    let mut app = web_app();
    let (source, _) = counting_source(124.0);
    app.set_track_feature_source(source);
    let path = PathBuf::from("/music/halcyon.flac");
    app.queue.add_track(track(&path));

    assert!(
        web_state_until(&mut app, |s| s.bpm.is_some()),
        "BPM never reached the web state"
    );
    let state = app.web_state.read().unwrap().clone();
    assert_eq!(state.bpm, Some(124.0));
    assert_eq!(state.key.as_deref(), Some("Am (8A)"));
    assert_eq!(app.track_features().map(|f| f.filepath.clone()), Some(path));
}

#[test]
fn test_web_state_update_never_reads_the_database() {
    common::isolate();
    let mut app = web_app();
    let source: FeatureSource = Arc::new(|path: &Path| {
        std::thread::sleep(Duration::from_millis(400));
        Some(features(path, 124.0))
    });
    app.set_track_feature_source(source);
    app.queue.add_track(track(Path::new("/music/slow.flac")));

    let start = Instant::now();
    app.update_web_state();
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(app.web_state.read().unwrap().bpm, None, "not known yet");
    assert!(web_state_until(&mut app, |s| s.bpm == Some(124.0)));
}

#[test]
fn test_web_state_clears_features_when_the_track_changes() {
    common::isolate();
    let mut app = web_app();
    let known = PathBuf::from("/music/known.flac");
    let unknown = PathBuf::from("/music/unknown.flac");
    let known2 = known.clone();
    let source: FeatureSource = Arc::new(move |path: &Path| (path == known2).then(|| features(path, 124.0)));
    app.set_track_feature_source(source);
    app.queue.add_track(track(&known));
    app.queue.add_track(track(&unknown));
    assert!(web_state_until(&mut app, |s| s.bpm == Some(124.0)));

    app.queue.set_current_index(1);
    app.update_web_state();
    let state = app.web_state.read().unwrap().clone();
    assert_eq!(state.bpm, None, "the previous track's BPM must not stay");
    assert_eq!(state.key, None);
}

#[test]
fn test_app_shutdown_joins_the_features_worker() {
    common::isolate();
    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(std::env::temp_dir()), settings_mgr);
    let (source, _) = counting_source(124.0);
    app.set_track_feature_source(source);
    app.queue.add_track(track(Path::new("/music/a.flac")));
    app.poll_track_features();
    assert_eq!(app.shutdown_workers(Duration::from_secs(2)), 0);
    assert_eq!(app.live_worker_count(), 0);
}

#[test]
fn test_features_for_answers_only_for_the_followed_track() {
    common::isolate();
    let mut workers = WorkerRegistry::new();
    let (source, _calls) = counting_source(112.0);
    let mut worker = TrackFeaturesWorker::spawn(&mut workers, source);
    let a = PathBuf::from("/music/a.flac");
    let b = PathBuf::from("/music/b.flac");
    worker.follow(Some(&a));
    assert!(poll_until(&mut worker, |w| w.features_for(&a).is_some()));
    assert_eq!(worker.features_for(&a).and_then(|f| f.bpm), Some(112.0));
    // Asked about another track (the current one changed before the next follow): nothing.
    assert!(worker.features_for(&b).is_none());
    worker.stop();
    assert_eq!(workers.join_all(Duration::from_secs(2)), 0);
}
