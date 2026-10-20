mod common;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use quickstream::queue::FastRng;
use quickstream::settings::SettingsManager;
use quickstream::ui::AppUI;
use quickstream::web::{make_continuous_wav_header, NowPlayingState, WebCommand, WebServer};

fn create_dummy_audio(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let data_len = 1000u32;
    let riff_len = 36 + data_len;
    let mut file = File::create(path).unwrap();
    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&riff_len.to_le_bytes());
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes());
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes());
    hdr[22..24].copy_from_slice(&2u16.to_le_bytes());
    hdr[24..28].copy_from_slice(&44100u32.to_le_bytes());
    hdr[28..32].copy_from_slice(&(44100u32 * 4).to_le_bytes());
    hdr[32..34].copy_from_slice(&4u16.to_le_bytes());
    hdr[34..36].copy_from_slice(&16u16.to_le_bytes());
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&data_len.to_le_bytes());
    file.write_all(&hdr).unwrap();
    file.write_all(&vec![0u8; data_len as usize]).unwrap();
}

#[test]
fn test_splitmix64_fast_rng_shuffle() {
    common::isolate();
    let mut rng = FastRng::with_seed(42);
    let mut numbers: Vec<usize> = (0..50).collect();
    let original = numbers.clone();
    rng.shuffle(&mut numbers);

    // Verify all original elements are preserved
    let mut sorted = numbers.clone();
    sorted.sort();
    assert_eq!(sorted, original);

    // Verify order was altered
    assert_ne!(numbers, original);
}

#[test]
fn test_continuous_wav_header_format() {
    common::isolate();
    let hdr = make_continuous_wav_header();
    assert_eq!(hdr.len(), 44);
    assert_eq!(&hdr[0..4], b"RIFF");
    assert_eq!(&hdr[4..8], &[0xFF, 0xFF, 0xFF, 0xFF]); // Streaming infinite size
    assert_eq!(&hdr[8..12], b"WAVE");
    assert_eq!(&hdr[12..16], b"fmt ");
    assert_eq!(&hdr[16..20], &16u32.to_le_bytes()); // Subchunk size 16
    assert_eq!(&hdr[20..22], &1u16.to_le_bytes()); // PCM format 1
    assert_eq!(&hdr[22..24], &2u16.to_le_bytes()); // 2 channels stereo
    assert_eq!(&hdr[24..28], &44100u32.to_le_bytes()); // 44100 Hz
    assert_eq!(&hdr[28..32], &(44100u32 * 4).to_le_bytes()); // 176400 B/s
    assert_eq!(&hdr[32..34], &4u16.to_le_bytes()); // Block align 4
    assert_eq!(&hdr[34..36], &16u16.to_le_bytes()); // 16-bit
    assert_eq!(&hdr[36..40], b"data");
    assert_eq!(&hdr[40..44], &[0xFF, 0xFF, 0xFF, 0xFF]); // Streaming infinite data chunk
}

#[test]
fn test_autopilot_recursive_scan_and_queue() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_autopilot_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let track1 = temp_dir.join("root_song.wav");
    let track2 = temp_dir.join("sub1").join("sub_song.wav");
    let track3 = temp_dir.join("sub1").join("sub2").join("deep_song.wav");

    create_dummy_audio(&track1);
    create_dummy_audio(&track2);
    create_dummy_audio(&track3);

    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(2));

    // Verify 3 tracks discovered across recursive subdirectories
    assert_eq!(app.queue.len(), 3);
    assert_eq!(app.autopilot_dir, Some(temp_dir.clone()));
    assert!(app.status_message.contains("Autopilot"));

    // Verify all 3 files are present in the queue
    let queue_paths: Vec<PathBuf> = app.queue.tracks().iter().map(|t| t.filepath.clone()).collect();
    assert!(queue_paths.contains(&track1));
    assert!(queue_paths.contains(&track2));
    assert!(queue_paths.contains(&track3));

    // Verify NowPlayingState reflected is_autopilot
    if let Ok(s) = app.web_state.read() {
        assert!(s.is_autopilot);
        assert_eq!(s.autopilot_dir, Some(temp_dir.to_string_lossy().to_string()));
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_autopilot_and_shuffle_actions() {
    common::isolate();
    // Web commands only reach folders inside the allowed roots.
    let music = std::env::temp_dir().join(format!("qs_web_autopilot_{}", std::process::id()));
    fs::create_dir_all(music.join("jazz")).unwrap();
    let jazz = fs::canonicalize(music.join("jazz")).unwrap();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    state.write().unwrap().allowed_roots = vec![music.clone()];
    let (tx, rx) = mpsc::channel();
    let handle = WebServer::start(0, state, tx).expect("Failed to start test web server");
    let port = handle.port;

    // 1. Send POST /api/action with action: "autopilot"
    {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let body = format!(
            r#"{{"action":"autopilot","path":{}}}"#,
            serde_json::to_string(&jazz.to_string_lossy()).unwrap()
        );
        let req = format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            port,
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).unwrap();

        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"));

        let cmd = rx.recv_timeout(Duration::from_secs(2)).expect("Command not received");
        assert_eq!(cmd, WebCommand::Autopilot(Some(jazz.clone())));
    }

    // 2. Send POST /api/action with action: "shuffle"
    {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let body = r#"{"action":"shuffle"}"#;
        let req = format!(
            "POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            port,
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).unwrap();

        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"));

        let cmd = rx.recv_timeout(Duration::from_secs(2)).expect("Command not received");
        assert_eq!(cmd, WebCommand::ShuffleQueue);
    }

    handle.stop();
    let _ = fs::remove_dir_all(&music);
}

#[test]
fn test_continuous_wav_stream_endpoint() {
    common::isolate();
    let state = Arc::new(RwLock::new(NowPlayingState::default()));
    let (tx, _rx) = mpsc::channel();
    let handle = WebServer::start(0, state.clone(), tx).expect("Failed to start test web server");
    let port = handle.port;

    // The continuous stream is served at the armed session's path.
    let session = handle.stream_state().arm_continuous_session();
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        session, port
    );
    stream.write_all(req.as_bytes()).unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert_eq!(status_line.trim(), "HTTP/1.1 200 OK");

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        headers.push(line);
    }

    assert!(headers
        .iter()
        .any(|h| h.to_lowercase().contains("content-type: audio/x-wav")));
    assert!(headers
        .iter()
        .any(|h| h.to_lowercase().contains("transfermode.dlna.org: streaming")));

    // Read 44-byte WAV header
    let mut wav_hdr = [0u8; 44];
    reader.read_exact(&mut wav_hdr).unwrap();
    assert_eq!(&wav_hdr[0..4], b"RIFF");
    assert_eq!(&wav_hdr[8..12], b"WAVE");
    assert_eq!(&wav_hdr[36..40], b"data");

    // Verify stream controller was marked active
    assert!(handle.stream_state().continuous.is_active.load(Ordering::SeqCst));

    // Closing client socket triggers clean termination
    drop(reader);
    std::thread::sleep(Duration::from_millis(100));

    handle.stop();
}

#[test]
fn test_parse_path_heuristics_title_artist_album() {
    common::isolate();
    use quickstream::library::LibraryManager;

    // 1. Path with track-disc prefix, artist in filename and folder structure
    let p1 = Path::new("/Volumes/Music/Ludovico Einaudi/2006 - Divenire/01-08 Einaudi- Come Un Fiore.flac");
    let (title1, artist1, album1) = LibraryManager::parse_path_heuristics(p1);
    assert_eq!(title1, "Come Un Fiore");
    assert_eq!(artist1, "Ludovico Einaudi");
    assert_eq!(album1, "2006 - Divenire");

    // 2. Path with 3-part "Album - Artist - Title" filename
    let p2 = Path::new("/Volumes/Music/Podcasts/Mysteries Of The Deep - Eric Cloutier - COLD IS THE VOID.mp3");
    let (title2, artist2, album2) = LibraryManager::parse_path_heuristics(p2);
    assert_eq!(title2, "COLD IS THE VOID");
    assert_eq!(artist2, "Eric Cloutier");
    assert_eq!(album2, "Mysteries Of The Deep");

    // 3. Simple track number prefix with artist & album folders
    let p3 = Path::new("/Volumes/Music/Bonobo/Black Sands/03 Paper.flac");
    let (title3, artist3, album3) = LibraryManager::parse_path_heuristics(p3);
    assert_eq!(title3, "Paper");
    assert_eq!(artist3, "Bonobo");
    assert_eq!(album3, "Black Sands");
}

#[test]
fn test_autopilot_diverse_sampling_across_artists() {
    common::isolate();
    use quickstream::library::LibraryManager;

    let temp_dir = std::env::temp_dir().join(format!("quickstream_diverse_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // Create 4 artist directories, each with 5 tracks (20 tracks total)
    let artists = ["ArtistA", "ArtistB", "ArtistC", "ArtistD"];
    for artist in &artists {
        let artist_dir = temp_dir.join(artist).join("Album");
        fs::create_dir_all(&artist_dir).unwrap();
        for i in 1..=5 {
            let track_path = artist_dir.join(format!("0{} Song {}.wav", i, i));
            create_dummy_audio(&track_path);
        }
    }

    let cache = std::collections::HashMap::new();
    let tracks = LibraryManager::scan_directory_scoped_with_cache(&temp_dir, &cache);

    // Verify tracks are drawn from MULTIPLE artists (not just 1 artist taking all slots)
    let mut sampled_artists = std::collections::HashSet::new();
    for t in &tracks {
        sampled_artists.insert(t.artist.clone());
    }

    // Must have sampled from all 4 artists
    assert_eq!(sampled_artists.len(), 4);
    for artist in &artists {
        assert!(sampled_artists.contains(*artist));
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_queue_background_metadata_update() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_bg_meta_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let track_path = temp_dir.join("01-08 Einaudi- Come Un Fiore.wav");
    create_dummy_audio(&track_path);

    let mut settings_mgr = SettingsManager::new();
    settings_mgr.settings.web_server_enabled = false;
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(2));
    assert_eq!(app.queue.len(), 1);

    // Initial heuristic state: artist derived from filename or folder
    assert_eq!(app.queue.tracks()[0].title, "Come Un Fiore");

    // Simulate background metadata reception
    let mut updated_meta = app.queue.tracks()[0].clone();
    updated_meta.artist = "Ludovico Einaudi (Official)".to_string();
    updated_meta.duration_seconds = 234.0;
    app.metadata_tx.send(updated_meta).unwrap();

    // Poll background metadata - must update queue tracks in-place
    app.poll_background_metadata();

    assert_eq!(app.queue.tracks()[0].artist, "Ludovico Einaudi (Official)");
    assert_eq!(app.queue.tracks()[0].duration_seconds, 234.0);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_autopilot_progressive_streaming_events() {
    common::isolate();
    use quickstream::library::{AutopilotEvent, LibraryManager};
    use std::sync::atomic::AtomicBool;

    let temp_dir = std::env::temp_dir().join(format!("quickstream_prog_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // Create 3 artist folders with songs
    for i in 1..=3 {
        let artist_dir = temp_dir.join(format!("Artist_{}", i)).join("Album");
        fs::create_dir_all(&artist_dir).unwrap();
        let track_path = artist_dir.join(format!("0{} Song.wav", i));
        create_dummy_audio(&track_path);
    }

    let cache = std::collections::HashMap::new();
    let (tx, rx) = mpsc::channel();
    let stop_flag = Arc::new(AtomicBool::new(false));

    // Run progressive scanner in background thread
    let dir_clone = temp_dir.clone();
    let stop_clone = stop_flag.clone();
    let handle = std::thread::spawn(move || {
        LibraryManager::scan_directory_autopilot_progressive(dir_clone, Vec::new(), cache, tx, stop_clone);
    });

    // Receive events progressively
    let mut tracks_received = 0;
    let mut finished = false;

    while let Ok(event) = rx.recv_timeout(Duration::from_secs(2)) {
        match event {
            AutopilotEvent::Track(t) => {
                tracks_received += 1;
                assert!(!t.title.is_empty());
            }
            AutopilotEvent::Candidates(paths) => {
                panic!("Diverse Shuffle has no pool beyond what it queues: {paths:?}");
            }
            AutopilotEvent::Finished(cnt) => {
                finished = true;
                assert_eq!(cnt, 3);
                break;
            }
        }
    }

    handle.join().unwrap();
    assert!(finished);
    assert_eq!(tracks_received, 3);

    let _ = fs::remove_dir_all(&temp_dir);
}

/// An analysis row for `path` as the file is now (mtime and size), by `artist`, at `bpm`.
fn analysis_row(path: &Path, artist: &str, bpm: f64) -> quickstream::analysis::TrackFeatures {
    let md = fs::metadata(path).unwrap();
    let mtime = md
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    quickstream::analysis::TrackFeatures {
        filepath: path.to_path_buf(),
        mtime,
        file_size: md.len(),
        title: path.file_stem().unwrap().to_string_lossy().to_string(),
        artist: artist.to_string(),
        bpm: Some(bpm),
        ..Default::default()
    }
}

/// Run a BPM Flow mix at 120 BPM over `dir` and collect what it sends: the queued tracks, and
/// the candidates for analysis in the order sent. Asserts the candidates come after the mix's
/// tracks and before `Finished`.
fn run_bpm_flow_mix(dir: &Path, db: &quickstream::analysis::AnalysisDatabase) -> (Vec<PathBuf>, Vec<PathBuf>) {
    use quickstream::analysis::MixProfile;
    use quickstream::library::{AutopilotEvent, LibraryManager, MetadataCache};
    use std::sync::atomic::AtomicBool;

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let (dir, scan_db) = (dir.to_path_buf(), db.clone());
    let handle = std::thread::spawn(move || {
        LibraryManager::scan_directory_autopilot_with_profile(
            dir,
            Vec::new(),
            MetadataCache::default(),
            MixProfile::BpmFlow {
                target_bpm: 120.0,
                tolerance: 0.05,
            },
            scan_db,
            tx,
            stop,
        );
    });

    let mut queued = Vec::new();
    let mut candidates = Vec::new();
    let mut finished = false;
    while let Ok(event) = rx.recv_timeout(Duration::from_secs(30)) {
        match event {
            AutopilotEvent::Track(t) => {
                assert!(candidates.is_empty(), "candidates come after the mix");
                queued.push(t.filepath);
            }
            AutopilotEvent::Candidates(paths) => candidates.extend(paths),
            AutopilotEvent::Finished(_) => {
                finished = true;
                break;
            }
        }
    }
    handle.join().unwrap();
    assert!(finished);
    (queued, candidates)
}

/// A BPM Flow mix sends its candidate pool for analysis (issue #30): the candidates it gathered
/// but did not queue, without those already analysed, once, before `Finished`.
#[test]
fn test_profile_mix_sends_its_unqueued_candidates_for_analysis() {
    common::isolate();
    use quickstream::analysis::AnalysisDatabase;
    use std::collections::HashSet;

    let temp_dir = std::env::temp_dir().join(format!("quickstream_pool_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let db = AnalysisDatabase::open_in_memory().unwrap();

    // A small flat folder: three tracks in the BPM range (the mix), one analysed out of range
    // (up to date: not sent again) and four never analysed (the pool to analyse).
    let mut unanalysed = HashSet::new();
    for i in 1..=8 {
        let path = temp_dir.join(format!("{i:02} Song.wav"));
        create_dummy_audio(&path);
        let artist = format!("Artist {i}");
        match i {
            1..=3 => db.upsert_track(&analysis_row(&path, &artist, 120.0)).unwrap(),
            4 => db.upsert_track(&analysis_row(&path, &artist, 90.0)).unwrap(),
            _ => {
                unanalysed.insert(path);
            }
        }
    }

    let (queued, candidates) = run_bpm_flow_mix(&temp_dir, &db);
    assert_eq!(queued.len(), 3, "the three tracks in range make the mix");
    let sent: HashSet<PathBuf> = candidates.iter().cloned().collect();
    assert_eq!(sent.len(), candidates.len(), "each candidate is sent once");
    assert_eq!(sent, unanalysed, "the pool minus queued and up-to-date tracks");

    let _ = fs::remove_dir_all(&temp_dir);
}

/// A row for the file's size but an older mtime (a retag that kept the size) is stale: the
/// worker would analyse the file again, so the mix sends it, as the worker's own check would.
#[test]
fn test_profile_mix_sends_a_candidate_whose_row_has_a_stale_mtime() {
    common::isolate();
    use quickstream::analysis::AnalysisDatabase;

    let temp_dir = std::env::temp_dir().join(format!("quickstream_pool_stale_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let db = AnalysisDatabase::open_in_memory().unwrap();

    for i in 1..=3 {
        let path = temp_dir.join(format!("{i:02} Song.wav"));
        create_dummy_audio(&path);
        db.upsert_track(&analysis_row(&path, &format!("Artist {i}"), 120.0))
            .unwrap();
    }
    let current = temp_dir.join("04 Current.wav");
    let stale = temp_dir.join("05 Retagged.wav");
    create_dummy_audio(&current);
    create_dummy_audio(&stale);
    db.upsert_track(&analysis_row(&current, "Current", 90.0)).unwrap();
    let mut row = analysis_row(&stale, "Retagged", 90.0);
    row.mtime -= 3600;
    db.upsert_track(&row).unwrap();

    let (queued, candidates) = run_bpm_flow_mix(&temp_dir, &db);
    assert_eq!(queued.len(), 3, "the three tracks in range make the mix");
    assert_eq!(
        candidates,
        vec![stale],
        "the stale row is sent again, the current one is not"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

/// A large library's pool can hold more than the analysis worker keeps waiting (up to 120
/// matched candidates and 200 others): at most `MAX_PENDING_CANDIDATES` are sent, the
/// profile-matched ones first, so none of those is pushed out by the run's own others.
#[test]
fn test_analysis_candidates_are_bounded_matched_first() {
    common::isolate();
    use quickstream::analysis::worker::MAX_PENDING_CANDIDATES;
    use quickstream::library::analysis_candidates;

    let track = |name: String| PathBuf::from(format!("/music/{name}.flac"));
    let matched: Vec<PathBuf> = (0..120).map(|i| track(format!("match-{i}"))).collect();
    // The others list holds the matched candidates too (gathering adds them to both).
    let others: Vec<PathBuf> = matched
        .iter()
        .cloned()
        .chain((0..200).map(|i| track(format!("other-{i}"))))
        .collect();
    let queued = matched[0].clone();
    let analysed = track("other-0".to_string());

    let sent = analysis_candidates(&matched, &others, |p| *p == queued || *p == analysed);
    assert_eq!(sent.len(), MAX_PENDING_CANDIDATES, "never more than the worker keeps");
    assert_eq!(sent[..119], matched[1..], "the matched candidates first, each once");
    assert_eq!(
        sent[119],
        track("other-1".to_string()),
        "then the others, less the skipped"
    );
    assert_eq!(
        sent[199],
        track("other-81".to_string()),
        "the rest of the others give way"
    );
}

#[test]
fn test_autopilot_append_mode_does_not_interrupt_playback() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_ap_append_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    for i in 1..=4 {
        let artist_dir = temp_dir.join(format!("Band_{}", i)).join("Album");
        fs::create_dir_all(&artist_dir).unwrap();
        let track_path = artist_dir.join(format!("Track_{}.wav", i));
        create_dummy_audio(&track_path);
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);

    // Seed existing queue with 2 tracks
    for i in 1..=2 {
        let meta = quickstream::metadata::AudioMetadata {
            title: format!("Existing {}", i),
            filepath: temp_dir.join(format!("Existing_{}.wav", i)),
            ..Default::default()
        };
        app.queue.add_track(meta);
    }
    app.queue.set_current_index(1); // "Existing 2" is playing
    assert_eq!(app.queue.len(), 2);

    // Trigger autopilot via start_autopilot (must detect non-empty queue and select Append mode)
    app.start_autopilot(temp_dir.clone());
    assert_eq!(app.autopilot_run_mode, quickstream::ui::AutopilotRunMode::Append);

    // Initial tracks must NOT be wiped
    assert_eq!(app.queue.tracks()[0].title, "Existing 1");
    assert_eq!(app.queue.tracks()[1].title, "Existing 2");
    assert_eq!(app.queue.current_index(), 1);

    // Wait for progressive scan to complete
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    // Queue must now have original 2 tracks + newly appended tracks
    assert_eq!(app.queue.len(), 6);
    assert_eq!(app.queue.tracks()[0].title, "Existing 1");
    assert_eq!(app.queue.tracks()[1].title, "Existing 2");
    assert_eq!(app.queue.current_index(), 1);
    assert!(app.status_message.contains("Appended"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_autopilot_reshuffle_from_next_track_forward() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_ap_reshuffle_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    for i in 1..=5 {
        let artist_dir = temp_dir.join(format!("Artist_{}", i)).join("Album");
        fs::create_dir_all(&artist_dir).unwrap();
        let track_path = artist_dir.join(format!("Song_{}.wav", i));
        create_dummy_audio(&track_path);
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);

    // Create 4 initial tracks in queue
    for i in 1..=4 {
        let meta = quickstream::metadata::AudioMetadata {
            title: format!("Original Track {}", i),
            filepath: temp_dir.join(format!("orig_{}.wav", i)),
            ..Default::default()
        };
        app.queue.add_track(meta);
    }
    app.queue.set_current_index(1); // "Original Track 2" is playing

    // Trigger reshuffle from next track forward
    app.reshuffle_autopilot(temp_dir.clone());
    assert_eq!(app.autopilot_run_mode, quickstream::ui::AutopilotRunMode::Reshuffle);

    // Tracks 0 and 1 must be preserved; tracks 2 and 3 must be truncated immediately
    assert_eq!(app.queue.len(), 2);
    assert_eq!(app.queue.tracks()[0].title, "Original Track 1");
    assert_eq!(app.queue.tracks()[1].title, "Original Track 2");
    assert_eq!(app.queue.current_index(), 1);

    // Wait for scan to stream new seed into queue
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    // Queue must now have 2 original tracks + 5 newly seeded tracks
    assert_eq!(app.queue.len(), 7);
    assert_eq!(app.queue.tracks()[0].title, "Original Track 1");
    assert_eq!(app.queue.tracks()[1].title, "Original Track 2");
    assert_eq!(app.queue.current_index(), 1);
    assert!(app.status_message.contains("Reshuffled"));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_autopilot_reshuffle_action() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_web_ap_reshuffle_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    for i in 1..=3 {
        let artist_dir = temp_dir.join(format!("Singer_{}", i)).join("Album");
        fs::create_dir_all(&artist_dir).unwrap();
        let track_path = artist_dir.join(format!("Track_{}.wav", i));
        create_dummy_audio(&track_path);
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);

    // Setup web cmd channel
    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);

    // Queue 3 songs
    for i in 1..=3 {
        let meta = quickstream::metadata::AudioMetadata {
            title: format!("Song {}", i),
            filepath: temp_dir.join(format!("song_{}.wav", i)),
            ..Default::default()
        };
        app.queue.add_track(meta);
    }
    app.queue.set_current_index(0);

    // Send WebCommand::AutopilotReshuffle
    tx.send(WebCommand::AutopilotReshuffle(Some(temp_dir.clone()))).unwrap();

    app.handle_web_commands();
    assert_eq!(app.autopilot_run_mode, quickstream::ui::AutopilotRunMode::Reshuffle);
    assert_eq!(app.queue.len(), 1); // Truncated after index 0
    assert_eq!(app.queue.tracks()[0].title, "Song 1");

    app.wait_for_autopilot_scan(Duration::from_secs(3));
    assert_eq!(app.queue.len(), 4); // 1 + 3 new
    assert_eq!(app.queue.tracks()[0].title, "Song 1");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_autopilot_bpm_flow_and_mood_vibe() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_mix_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let db = quickstream::analysis::AnalysisDatabase::open_in_memory().unwrap();

    // Create 6 tracks: 3 high tempo (124-128 BPM), 3 low tempo (70-75 BPM)
    for i in 1..=6 {
        let track_path = temp_dir.join(format!("track_{}.wav", i));
        create_dummy_audio(&track_path);

        let (bpm, genre, mood) = if i <= 3 {
            (
                Some(124.0 + (i as f64) * 2.0),
                "Electronic".to_string(),
                Some("workout".to_string()),
            )
        } else {
            (
                Some(70.0 + (i as f64)),
                "Ambient".to_string(),
                Some("chill".to_string()),
            )
        };

        let feat = quickstream::analysis::TrackFeatures {
            filepath: track_path.clone(),
            title: format!("Track {}", i),
            artist: format!("Artist {}", i),
            album: format!("Album {}", i),
            genre,
            bpm,
            energy: if i <= 3 { Some(0.85) } else { Some(0.25) },
            mood,
            ..Default::default()
        };
        db.upsert_track(&feat).unwrap();
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);
    app.analysis_db = db;

    // Test 1: BpmFlow ~125 BPM
    app.active_mix_profile = quickstream::analysis::MixProfile::BpmFlow {
        target_bpm: 125.0,
        tolerance: 0.10,
    };
    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    // The 3 high tempo tracks (126, 128, 130) should be selected
    assert!(!app.queue.is_empty());
    for track in app.queue.tracks() {
        let feat = app.analysis_db.get_track(&track.filepath).unwrap();
        assert!(feat.bpm.unwrap() >= 100.0);
    }

    // Test 2: MoodVibe Chill
    app.queue.clear();
    app.active_mix_profile = quickstream::analysis::MixProfile::MoodVibe(quickstream::analysis::MoodType::Chill);
    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    assert!(!app.queue.is_empty());
    for track in app.queue.tracks() {
        let feat = app.analysis_db.get_track(&track.filepath).unwrap();
        assert!(feat.bpm.unwrap() <= 98.0);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_web_mix_profile_and_seed_radio_commands() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_web_mix_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let seed_track = temp_dir.join("seed.wav");
    create_dummy_audio(&seed_track);

    for i in 1..=3 {
        let t = temp_dir.join(format!("t_{}.wav", i));
        create_dummy_audio(&t);
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);

    let (tx, rx) = mpsc::channel();
    app.web_cmd_rx = Some(rx);

    // 1. SetMixProfile
    tx.send(WebCommand::SetMixProfile("workout".to_string())).unwrap();
    app.handle_web_commands();
    assert_eq!(
        app.active_mix_profile,
        quickstream::analysis::MixProfile::MoodVibe(quickstream::analysis::MoodType::Workout)
    );

    // 2. SeedRadio
    tx.send(WebCommand::SeedRadio(seed_track.clone())).unwrap();
    app.handle_web_commands();
    assert_eq!(
        app.active_mix_profile,
        quickstream::analysis::MixProfile::SeedRadio(seed_track.clone())
    );

    app.wait_for_autopilot_scan(Duration::from_secs(3));
    assert!(!app.queue.is_empty());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_autopilot_mood_vibe_diverse_artist_sampling_no_single_artist_loop() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_diverse_mood_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // 1. Dominant artist directory with 25 tracks across multiple albums
    for album in ["Accelerator", "Lifeforms", "ISDN", "Dead Cities"] {
        let fsol_dir = temp_dir.join("The Future Sound of London").join(album);
        fs::create_dir_all(&fsol_dir).unwrap();
        for i in 1..=6 {
            let track_path = fsol_dir.join(format!("0{} Ambient Cascade {}.wav", i, i));
            create_dummy_audio(&track_path);
        }
    }

    // 2. Several other artist directories with tracks
    let other_artists = [
        ("Boards of Canada", "Music Has the Right to Children", "Chill Track"),
        ("Brian Eno", "Music for Airports", "Ambient 1"),
        ("Aphex Twin", "Selected Ambient Works", "Xtal"),
        ("Tycho", "Dive", "Coastal Brake"),
        ("Global Communication", "76 14", "4 02"),
    ];

    for (artist, album, song) in &other_artists {
        let artist_dir = temp_dir.join(artist).join(album);
        fs::create_dir_all(&artist_dir).unwrap();
        for i in 1..=4 {
            let track_path = artist_dir.join(format!("0{} {} {}.wav", i, song, i));
            create_dummy_audio(&track_path);
        }
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);

    // Run 1: Chill & Ambient
    app.active_mix_profile = quickstream::analysis::MixProfile::MoodVibe(quickstream::analysis::MoodType::Chill);
    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    assert!(!app.queue.is_empty());
    let mut artists_in_mix = std::collections::HashSet::new();
    for track in app.queue.tracks() {
        artists_in_mix.insert(track.artist.clone());
    }

    // Must sample across multiple artists, NOT 100% Future Sound of London!
    assert!(
        artists_in_mix.len() >= 4,
        "Autopilot mix must contain diverse artists, found only: {:?}",
        artists_in_mix
    );

    // Verify consecutive tracks are never by the same artist
    for i in 1..app.queue.len() {
        assert_ne!(
            app.queue.tracks()[i].artist,
            app.queue.tracks()[i - 1].artist,
            "Consecutive tracks must not be by the same artist at index {} and {}",
            i - 1,
            i
        );
    }

    // Run 2: Fresh run with Late Night Groove
    app.queue.clear();
    app.active_mix_profile = quickstream::analysis::MixProfile::MoodVibe(quickstream::analysis::MoodType::Groove);
    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    assert!(!app.queue.is_empty());
    let mut groove_artists = std::collections::HashSet::new();
    for track in app.queue.tracks() {
        groove_artists.insert(track.artist.clone());
    }

    assert!(
        groove_artists.len() >= 4,
        "Late Night Groove mix must sample diverse artists, found only: {:?}",
        groove_artists
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_autopilot_mix_generates_full_queue_when_library_has_sparse_matches() {
    common::isolate();
    let temp_dir = std::env::temp_dir().join(format!("quickstream_sparse_mix_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // Create 15 artists, each with 2 tracks (30 tracks total > 10 tracks)
    // Only Artist 1 has a track matching "Chill"
    for i in 1..=15 {
        let artist_dir = temp_dir.join(format!("Artist_{:02}", i)).join("Album");
        fs::create_dir_all(&artist_dir).unwrap();
        for j in 1..=2 {
            let title = if i == 1 && j == 1 {
                "01 Chill Vibes.wav".to_string()
            } else {
                format!("0{} Regular Track {}.wav", j, j)
            };
            create_dummy_audio(&artist_dir.join(title));
        }
    }

    let mgr = SettingsManager::default();
    let mut app = AppUI::new(Some(temp_dir.clone()), mgr);

    app.active_mix_profile = quickstream::analysis::MixProfile::MoodVibe(quickstream::analysis::MoodType::Chill);
    app.start_autopilot(temp_dir.clone());
    app.wait_for_autopilot_scan(Duration::from_secs(3));

    // Even though only 1 track matched the "Chill" mood keyword,
    // the mix must NOT be truncated to 1 song! It should supplement up to all available candidates.
    assert!(
        app.queue.len() >= 20,
        "Expected at least 20 tracks queued in a 30-track library, but found only {}",
        app.queue.len()
    );

    let _ = fs::remove_dir_all(&temp_dir);
}
