//! Profile mixes read the library index first (#41): folders whose whole subtree has current
//! rows in the analysis database are drawn from those rows without walking them or reading
//! tags, and only the other folders are walked. Section 4's rules hold for both.
//!
//! Own test binary: it initializes the process-wide logger in the isolated XDG data dir. The
//! index is filled with `write_index_batch`, as the background indexer will.

mod common;

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use quickstream::analysis::db::IndexedDir;
use quickstream::analysis::{AnalysisDatabase, MixProfile, MoodType, TrackFeatures};
use quickstream::library::{AutopilotEvent, LibraryManager, MetadataCache, ProfileScanLimits};
use quickstream::metadata::AudioMetadata;

/// Mixes that read the log run one at a time, so their lines don't interleave.
static LOG_LOCK: Mutex<()> = Mutex::new(());

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

/// A fresh library folder in the isolated root.
fn library(name: &str) -> PathBuf {
    let dir = common::isolate().join(format!("lib-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn stamp(path: &Path) -> (u64, u64) {
    let md = fs::metadata(path).unwrap();
    let mtime = md.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
    (mtime, md.len())
}

/// A tag-only index row for `path` as the file is now. With a `bpm` it is complete (a BPM tag);
/// without one it waits for its acoustic pass.
fn row(path: &Path, artist: &str, album: &str, genre: &str, bpm: Option<f64>) -> TrackFeatures {
    let (mtime, file_size) = stamp(path);
    TrackFeatures {
        filepath: path.to_path_buf(),
        mtime,
        file_size,
        title: path.file_stem().unwrap().to_string_lossy().to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        genre: genre.to_string(),
        bpm,
        needs_acoustic: bpm.is_none(),
        analyzed_at: 1,
        ..Default::default()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Record `folder` as indexed now: its rows, its folder record, and its subtree marked complete.
fn index_folder(db: &AnalysisDatabase, folder: &Path, rows: &[TrackFeatures]) {
    index_folder_at(db, folder, rows, now_secs());
}

/// Record `folder` as indexed at `walked_at` (seconds).
fn index_folder_at(db: &AnalysisDatabase, folder: &Path, rows: &[TrackFeatures], walked_at: u64) {
    let record = IndexedDir {
        dirpath: folder.to_path_buf(),
        audio_files: rows.len(),
        complete: true,
        walked_at,
    };
    db.write_index_batch(rows, Some(&record)).unwrap();
    assert!(db.set_subtree_complete(folder, true).unwrap());
}

/// `n` silent tracks in `folder`.
fn tracks(folder: &Path, n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let path = folder.join(format!("{:02} Song.wav", i));
            create_silent_wav(&path);
            path
        })
        .collect()
}

fn bpm_flow() -> MixProfile {
    MixProfile::BpmFlow {
        target_bpm: 120.0,
        tolerance: 0.05,
    }
}

struct MixRun {
    tracks: Vec<PathBuf>,
    metas: Vec<AudioMetadata>,
    candidates: Vec<PathBuf>,
    gathered: String,
    complete: String,
}

fn run_mix(dir: &Path, db: &AnalysisDatabase, profile: MixProfile, limits: ProfileScanLimits) -> MixRun {
    run_mix_with_cache(dir, db, profile, limits, MetadataCache::default())
}

fn run_mix_with_cache(
    dir: &Path,
    db: &AnalysisDatabase,
    profile: MixProfile,
    limits: ProfileScanLimits,
    cache: MetadataCache,
) -> MixRun {
    let _serial = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let log_path = quickstream::logger::init();
    quickstream::logger::flush();
    let log_before = fs::read_to_string(&log_path).unwrap_or_default().len();

    let (tx, rx) = mpsc::channel();
    let (scan_dir, scan_db) = (dir.to_path_buf(), db.clone());
    let handle = std::thread::spawn(move || {
        LibraryManager::scan_directory_autopilot_with_profile_limits(
            scan_dir,
            Vec::new(),
            cache,
            profile,
            scan_db,
            tx,
            Arc::new(AtomicBool::new(false)),
            limits,
        )
    });
    let (mut sent, mut metas, mut candidates, mut finished) = (Vec::new(), Vec::new(), Vec::new(), false);
    while let Ok(event) = rx.recv_timeout(Duration::from_secs(60)) {
        match event {
            AutopilotEvent::Track(meta) => {
                assert!(candidates.is_empty(), "tracks come before the candidates");
                sent.push(meta.filepath.clone());
                metas.push(meta);
            }
            AutopilotEvent::Candidates(paths) => candidates = paths,
            AutopilotEvent::Finished(count) => {
                assert_eq!(count, sent.len());
                finished = true;
                break;
            }
        }
    }
    handle.join().unwrap();
    assert!(finished, "the mix finished");
    assert!(quickstream::logger::flush(), "log writer did not flush");
    let log = fs::read_to_string(&log_path).unwrap()[log_before..].to_string();
    let line = |needle: &str| {
        log.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no '{needle}' line in:\n{log}"))
            .to_string()
    };
    MixRun {
        tracks: sent,
        metas,
        candidates,
        gathered: line("[autopilot] candidates gathered"),
        complete: line("[autopilot] mix complete"),
    }
}

fn counter(line: &str, name: &str) -> usize {
    let key = format!(" {name}=");
    let start = line.find(&key).unwrap_or_else(|| panic!("no {name} in {line}")) + key.len();
    line[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

/// A Chill mix over a library whose every folder is indexed reads no tags and walks no folder:
/// its tracks come from the index rows, the first one before the mix finishes.
#[test]
fn a_fully_indexed_chill_mix_reads_no_tags_and_walks_no_folders() {
    common::isolate();
    let lib = library("full");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    for a in 0..12 {
        let folder = lib.join(format!("Artist {a:02}"));
        let rows: Vec<TrackFeatures> = tracks(&folder.join("Album"), 5)
            .iter()
            .map(|p| row(p, &format!("Artist {a:02}"), &format!("Album {a:02}"), "Ambient", None))
            .collect();
        index_folder(&db, &folder, &rows);
    }

    let run = run_mix(
        &lib,
        &db,
        MixProfile::MoodVibe(MoodType::Chill),
        ProfileScanLimits::default(),
    );
    assert_eq!(counter(&run.gathered, "tag_reads"), 0, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "folders"), 0, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "index_folders"), 12, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "index_rows"), 60, "{}", run.gathered);
    // 12 folders: at most 4 matches each.
    assert_eq!(run.tracks.len(), 48);
    assert!(run.tracks.iter().all(|t| t.starts_with(&lib)));
    let unique: HashSet<&PathBuf> = run.tracks.iter().collect();
    assert_eq!(unique.len(), run.tracks.len(), "no track is sent twice");
}

/// An indexed folder gives no more matches than a walked one would (`per_subdir`), however
/// many of its rows match.
#[test]
fn the_per_folder_quota_holds_for_an_indexed_folder() {
    common::isolate();
    let lib = library("quota");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let big = lib.join("Big");
    let rows: Vec<TrackFeatures> = tracks(&big.join("Album"), 50)
        .iter()
        .map(|p| row(p, "Big", "Big Album", "House", Some(120.0)))
        .collect();
    index_folder(&db, &big, &rows);
    for o in 0..4 {
        let folder = lib.join(format!("Other {o}"));
        let rows: Vec<TrackFeatures> = tracks(&folder, 3)
            .iter()
            .map(|p| row(p, &format!("Other {o}"), &format!("Other {o}"), "Rock", Some(90.0)))
            .collect();
        index_folder(&db, &folder, &rows);
    }

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    let from_big = run.tracks.iter().filter(|t| t.starts_with(&big)).count();
    // 5 folders: per_subdir is 4.
    assert_eq!(from_big, 4, "{:?}", run.tracks);
    assert_eq!(counter(&run.gathered, "index_matches"), 4, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "folders"), 0);
}

/// With half the folders indexed, the mix takes from both halves: the indexed ones from their
/// rows, the others walked. Indexed folders count toward the folder limit.
#[test]
fn a_half_indexed_library_draws_from_both_halves_within_the_folder_limit() {
    common::isolate();
    let lib = library("half");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut indexed = Vec::new();
    let mut walked = Vec::new();
    for a in 0..10 {
        let folder = lib.join(format!("Chill Artist {a:02}"));
        let files = tracks(&folder.join("Album"), 5);
        if a % 2 == 0 {
            let rows: Vec<TrackFeatures> = files
                .iter()
                .map(|p| row(p, &format!("Chill Artist {a:02}"), "Album", "Ambient", None))
                .collect();
            index_folder(&db, &folder, &rows);
            indexed.push(folder);
        } else {
            walked.push(folder);
        }
    }

    let limits = ProfileScanLimits {
        max_folders: 6,
        ..ProfileScanLimits::default()
    };
    let run = run_mix(&lib, &db, MixProfile::MoodVibe(MoodType::Chill), limits);
    let (folders, index_folders) = (
        counter(&run.gathered, "folders"),
        counter(&run.gathered, "index_folders"),
    );
    assert_eq!(folders + index_folders, 6, "{}", run.gathered);
    assert!(folders >= 1 && index_folders >= 1, "{}", run.gathered);
    let under = |set: &[PathBuf]| {
        run.tracks
            .iter()
            .filter(|t| set.iter().any(|f| t.starts_with(f)))
            .count()
    };
    assert!(under(&indexed) > 0, "indexed folders contribute: {:?}", run.tracks);
    assert!(under(&walked) > 0, "walked folders contribute: {:?}", run.tracks);
    // Only walked folders' files had their tags read, at most 4 each.
    assert!(counter(&run.gathered, "tag_reads") <= 4 * folders, "{}", run.gathered);
}

/// Index rows are never pruned, so a row can outlive its file: every row is checked before it is
/// offered to the mix, and a missing one is dropped (and counted).
#[test]
fn a_missing_indexed_file_is_never_sent() {
    common::isolate();
    let lib = library("missing");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut gone = HashSet::new();
    for a in 0..6 {
        let folder = lib.join(format!("Artist {a}"));
        let files = tracks(&folder, 4);
        let rows: Vec<TrackFeatures> = files
            .iter()
            .map(|p| row(p, &format!("Artist {a}"), &format!("Album {a}"), "House", Some(121.0)))
            .collect();
        index_folder(&db, &folder, &rows);
        for path in &files[..2] {
            fs::remove_file(path).unwrap();
            gone.insert(path.clone());
        }
    }

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    assert!(run.tracks.iter().all(|t| !gone.contains(t)), "{:?}", run.tracks);
    assert_eq!(run.tracks.len(), 12, "every file still there is sent");
    assert_eq!(counter(&run.complete, "index_missing"), 12, "{}", run.complete);
}

/// Index rows still waiting for their acoustic pass are sent for analysis when the mix leaves
/// them out; complete rows are not.
#[test]
fn pending_index_rows_go_out_as_candidates() {
    common::isolate();
    let lib = library("pending");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut pending_others = HashSet::new();
    let mut analysed_others = HashSet::new();
    for a in 0..25 {
        let folder = lib.join(format!("Artist {a:02}"));
        let files = tracks(&folder, 5);
        let artist = format!("Artist {a:02}");
        let mut rows = Vec::new();
        for path in &files[..3] {
            // Analysed at 120 BPM, then retagged: the old BPM stays provisionally.
            let mut old = row(path, &artist, &format!("Album {a}-{}", rows.len()), "", Some(120.0));
            old.mtime -= 3600;
            db.upsert_track(&old).unwrap();
            rows.push(row(path, &artist, &old.album, "", None));
        }
        rows.push(row(&files[3], &artist, "Other", "", Some(90.0)));
        analysed_others.insert(files[3].clone());
        rows.push(row(&files[4], &artist, "Other", "", None));
        pending_others.insert(files[4].clone());
        index_folder(&db, &folder, &rows);
    }
    assert!(db.get_track(&lib.join("Artist 00/00 Song.wav")).unwrap().needs_acoustic);
    assert_eq!(
        db.get_track(&lib.join("Artist 00/00 Song.wav")).unwrap().bpm,
        Some(120.0)
    );

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    assert_eq!(run.tracks.len(), 60);
    let queued: HashSet<&PathBuf> = run.tracks.iter().collect();
    assert!(run.candidates.iter().all(|c| !queued.contains(c)));
    assert!(
        run.candidates.iter().all(|c| !analysed_others.contains(c)),
        "complete rows are not sent for analysis"
    );
    for p in &pending_others {
        assert!(run.candidates.contains(p), "{} is pending and not queued", p.display());
    }
    // 75 matched (3 per folder), 60 queued: 15 pending matches and 25 pending others are left.
    assert_eq!(run.candidates.len(), 40, "{:?}", run.candidates);
}

/// The index pass reads exactly the indexed folder's own rows: a sibling sharing its name as a
/// prefix (`Jazz Fusion` for `Jazz`) is walked as itself, never drawn from Jazz's rows.
#[test]
fn a_mix_never_takes_jazz_fusion_rows_for_jazz() {
    common::isolate();
    let lib = library("prefix");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let jazz = lib.join("Jazz");
    let rows: Vec<TrackFeatures> = tracks(&jazz.join("Album"), 3)
        .iter()
        .map(|p| row(p, "Jazz", "Jazz Album", "Jazz", Some(119.0)))
        .collect();
    index_folder(&db, &jazz, &rows);
    // Jazz Fusion has rows too (an index pass that didn't finish it), so it is walked.
    let fusion = lib.join("Jazz Fusion");
    let fusion_rows: Vec<TrackFeatures> = tracks(&fusion, 4)
        .iter()
        .map(|p| row(p, "Fusion", "Fusion Album", "Jazz", Some(120.0)))
        .collect();
    db.write_index_batch(&fusion_rows, None).unwrap();

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    assert_eq!(counter(&run.gathered, "index_folders"), 1, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "index_rows"), 3, "{}", run.gathered);
    assert_eq!(
        counter(&run.gathered, "folders"),
        1,
        "Jazz Fusion is walked: {}",
        run.gathered
    );
    assert_eq!(counter(&run.gathered, "tag_reads"), 0, "its rows spare the tag reads");
    assert_eq!(run.tracks.iter().filter(|t| t.starts_with(&jazz)).count(), 3);
    assert_eq!(run.tracks.iter().filter(|t| t.starts_with(&fusion)).count(), 4);
}

/// With nothing indexed, a mix walks as it did before the index: the same counters, and the
/// index ones at zero.
#[test]
fn an_empty_index_leaves_the_mix_as_before() {
    common::isolate();
    let lib = library("empty");
    for artist in 0..6 {
        for track in 0..3 {
            create_silent_wav(&lib.join(format!("Artist {}/Album/{:02} Song.wav", artist, track)));
        }
    }
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let run = run_mix(
        &lib,
        &db,
        MixProfile::MoodVibe(MoodType::Chill),
        ProfileScanLimits::default(),
    );
    assert_eq!(counter(&run.gathered, "folders"), 6, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "files"), 18, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "tag_reads"), 18, "{}", run.gathered);
    for field in ["index_folders", "index_rows", "index_matches", "index_missing"] {
        assert_eq!(counter(&run.gathered, field), 0, "{}", run.gathered);
    }
    assert_eq!(counter(&run.complete, "index_missing"), 0, "{}", run.complete);
}

/// Index-sourced tracks keep section 4's artist cooldown: after an artist plays, the next 8
/// tracks are by other artists.
#[test]
fn index_sourced_mixes_keep_the_artist_cooldown() {
    common::isolate();
    let lib = library("cooldown");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    for a in 0..70 {
        let folder = lib.join(format!("Artist {a:02}"));
        let rows: Vec<TrackFeatures> = tracks(&folder, 3)
            .iter()
            .enumerate()
            .map(|(i, p)| {
                row(
                    p,
                    &format!("Artist {a:02}"),
                    &format!("Album {a}-{i}"),
                    "House",
                    Some(120.0),
                )
            })
            .collect();
        index_folder(&db, &folder, &rows);
    }

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    assert_eq!(run.tracks.len(), 60);
    // 70 folders: 2 matches each, and gathering stops once 80 wait in the pool (the first
    // match went out at once).
    assert_eq!(counter(&run.gathered, "index_folders"), 41, "{}", run.gathered);
    // The matches taken: the first sent, and 81 in the pool.
    assert_eq!(counter(&run.gathered, "index_matches"), 82, "{}", run.gathered);
    let artist = |p: &PathBuf| p.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    for (i, track) in run.tracks.iter().enumerate() {
        for later in run.tracks.iter().skip(i + 1).take(8) {
            assert_ne!(
                artist(track),
                artist(later),
                "artist repeats within 8 at {i}: {:?}",
                run.tracks
            );
        }
    }
}

/// A row whose file is gone never takes a place in the mix: it is checked before it is offered,
/// so it neither counts toward the 80 matches nor gets a queue position the cooldowns count on.
/// Each of 70 folders holds 3 matching rows, one of them for a deleted file; the mix still
/// sends 60 tracks with section 4's artist cooldown intact (PR #145 review).
#[test]
fn missing_indexed_files_never_hold_a_place_in_the_mix() {
    common::isolate();
    let lib = library("phantoms");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut gone = HashSet::new();
    for a in 0..70 {
        let folder = lib.join(format!("Artist {a:02}"));
        let files = tracks(&folder, 3);
        let rows: Vec<TrackFeatures> = files
            .iter()
            .enumerate()
            .map(|(i, p)| {
                row(
                    p,
                    &format!("Artist {a:02}"),
                    &format!("Album {a}-{i}"),
                    "House",
                    Some(120.0),
                )
            })
            .collect();
        index_folder(&db, &folder, &rows);
        fs::remove_file(&files[0]).unwrap();
        gone.insert(files[0].clone());
    }

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    assert!(run.tracks.iter().all(|t| !gone.contains(t)), "{:?}", run.tracks);
    assert_eq!(run.tracks.len(), 60, "the mix is not cut short: {}", run.complete);
    let artist = |p: &PathBuf| p.parent().unwrap().file_name().unwrap().to_string_lossy().to_string();
    for (i, track) in run.tracks.iter().enumerate() {
        for later in run.tracks.iter().skip(i + 1).take(8) {
            assert_ne!(
                artist(track),
                artist(later),
                "artist repeats within 8 at {i}: {:?}",
                run.tracks
            );
        }
    }
    assert!(run.candidates.iter().all(|t| !gone.contains(t)), "{:?}", run.candidates);
}

/// Make `path` a different file than the one indexed: a few more bytes, so its size changes.
fn change_file(path: &Path) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&[0u8; 64]).unwrap();
}

/// An index row is current only for the file it was read from: the mix judges it against the
/// stamp of its own check of the file, so a file changed since it was indexed goes out as a
/// candidate for analysis, while an unchanged complete row does not (PR #145 review).
#[test]
fn a_file_changed_since_it_was_indexed_goes_out_for_analysis() {
    common::isolate();
    let lib = library("changed");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let mut changed = HashSet::new();
    let mut unchanged = HashSet::new();
    for a in 0..25 {
        let folder = lib.join(format!("Artist {a:02}"));
        let files = tracks(&folder, 5);
        let artist = format!("Artist {a:02}");
        let mut rows: Vec<TrackFeatures> = files[..3]
            .iter()
            .enumerate()
            .map(|(i, p)| row(p, &artist, &format!("Album {a}-{i}"), "", Some(120.0)))
            .collect();
        // Two complete rows that don't match: one file stays as indexed, one changes after.
        rows.push(row(&files[3], &artist, "Other", "", Some(90.0)));
        rows.push(row(&files[4], &artist, "Other", "", Some(90.0)));
        index_folder(&db, &folder, &rows);
        unchanged.insert(files[3].clone());
        change_file(&files[4]);
        changed.insert(files[4].clone());
    }

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    // 75 matches (3 a folder): enough for the 60, so no other track is queued.
    assert_eq!(run.tracks.len(), 60);
    assert!(run
        .tracks
        .iter()
        .all(|t| !changed.contains(t) && !unchanged.contains(t)));
    for path in &changed {
        assert!(
            run.candidates.contains(path),
            "{} changed since it was indexed: {:?}",
            path.display(),
            run.candidates
        );
    }
    assert!(
        run.candidates.iter().all(|c| !unchanged.contains(c)),
        "an unchanged complete row is current"
    );
}

/// For a file changed since it was indexed, the tags `MetadataCache` holds for the file as it is
/// now (its size) are used, not the row's (PR #145 review).
#[test]
fn a_changed_indexed_file_takes_its_cached_tags_at_its_new_size() {
    common::isolate();
    let lib = library("changed_cache");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let folder = lib.join("Artist");
    let files = tracks(&folder.join("Album"), 3);
    let rows: Vec<TrackFeatures> = files
        .iter()
        .map(|p| row(p, "Artist", "Album", "House", Some(120.0)))
        .collect();
    index_folder(&db, &folder, &rows);
    change_file(&files[0]);
    let cache = MetadataCache::default();
    cache.insert(AudioMetadata {
        filepath: files[0].clone(),
        filename: "00 Song.wav".to_string(),
        title: "Fresh Title".to_string(),
        artist: "Artist".to_string(),
        duration_seconds: 300.0,
        file_size: fs::metadata(&files[0]).unwrap().len(),
        ..Default::default()
    });

    let run = run_mix_with_cache(&lib, &db, bpm_flow(), ProfileScanLimits::default(), cache);
    assert_eq!(run.tracks.len(), 3, "{:?}", run.tracks);
    let sent = run.metas.iter().find(|m| m.filepath == files[0]).unwrap();
    assert_eq!(sent.title, "Fresh Title");
    assert_eq!(sent.duration_seconds, 300.0);
    assert_eq!(sent.file_size, fs::metadata(&files[0]).unwrap().len());
}

/// A folder changed since the index walked it (here a new album under an indexed artist: the
/// artist folder's mtime is newer than its record's `walked_at`) is walked instead of drawn
/// from its rows, so the new tracks reach the mix; a folder unchanged since is still drawn from
/// the index (PR #145 review).
#[test]
fn a_folder_changed_since_it_was_indexed_is_walked() {
    common::isolate();
    let lib = library("new_album");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let fresh = lib.join("Fresh Artist");
    let fresh_rows: Vec<TrackFeatures> = tracks(&fresh.join("Album"), 2)
        .iter()
        .map(|p| row(p, "Fresh Artist", "Album", "House", Some(120.0)))
        .collect();
    index_folder(&db, &fresh, &fresh_rows);

    let grown = lib.join("Grown Artist");
    let old_rows: Vec<TrackFeatures> = tracks(&grown.join("Old Album"), 2)
        .iter()
        .map(|p| row(p, "Grown Artist", "Old Album", "House", Some(120.0)))
        .collect();
    let new_album = tracks(&grown.join("New Album"), 2);
    // Indexed before the new album came: its record is older than the folder's mtime.
    let (grown_mtime, _) = stamp(&grown);
    index_folder_at(&db, &grown, &old_rows, grown_mtime - 1);

    let run = run_mix(&lib, &db, bpm_flow(), ProfileScanLimits::default());
    assert_eq!(counter(&run.gathered, "index_folders"), 1, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "folders"), 1, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "files"), 4, "{}", run.gathered);
    let reached: HashSet<&PathBuf> = run.tracks.iter().chain(&run.candidates).collect();
    for path in &new_album {
        assert!(reached.contains(path), "{} is new: {:?}", path.display(), reached);
    }
}

/// The index rows' files are checked on the mix's worker threads, not one at a time on the
/// scanner thread: with a check that waits until another runs beside it, the checks meet
/// (PR #145 review: 500 folders of checks over SMB would spend most of the 20 s budget).
#[test]
fn index_rows_are_checked_on_several_threads() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;
    common::isolate();
    let lib = library("parallel");
    let db = AnalysisDatabase::open_in_memory().unwrap();
    for a in 0..8 {
        let folder = lib.join(format!("Artist {a}"));
        let rows: Vec<TrackFeatures> = tracks(&folder, 4)
            .iter()
            .map(|p| row(p, &format!("Artist {a}"), &format!("Album {a}"), "House", Some(120.0)))
            .collect();
        index_folder(&db, &folder, &rows);
    }

    let (in_flight, peak, calls) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let gave_up = Arc::new(AtomicBool::new(false));
    let check: quickstream::library::IndexFileCheck = {
        let (in_flight, peak, calls, gave_up) = (in_flight.clone(), peak.clone(), calls.clone(), gave_up.clone());
        Arc::new(move |path: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(2);
            while peak.load(Ordering::SeqCst) < 2 && !gave_up.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    gave_up.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let found = fs::metadata(path)
                .ok()
                .filter(|md| md.is_file())
                .map(|md| quickstream::analysis::worker::stamp_of(&md));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            found
        })
    };
    let limits = ProfileScanLimits {
        index_check: Some(check),
        ..ProfileScanLimits::default()
    };
    let run = run_mix(&lib, &db, bpm_flow(), limits);
    assert!(!gave_up.load(Ordering::SeqCst), "the checks never ran side by side");
    assert!(peak.load(Ordering::SeqCst) >= 2);
    assert_eq!(calls.load(Ordering::SeqCst), 32, "one check per row offered");
    assert_eq!(run.tracks.len(), 32, "{}", run.gathered);
    assert_eq!(counter(&run.gathered, "index_folders"), 8, "{}", run.gathered);
}
