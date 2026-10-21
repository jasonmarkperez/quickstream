//! Integration tests for the SQLite analysis database lifecycle (issue #18):
//! per-open pragmas, transactional `user_version` migrations, and refusing databases
//! written by a newer build.
//!
//! Every database here lives in a scratch dir under the OS temp dir; nothing touches the
//! user's real `~/.cache/quickstream/track_analysis.db`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use quickstream::analysis::db::{SchemaTooNewError, CURRENT_SCHEMA_VERSION};
use quickstream::analysis::{AnalysisDatabase, TrackFeatures};

fn scratch_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "quickstream_analysis_db_{}_{}_{}",
        name,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user_version(path: &Path) -> u32 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

fn table_exists(path: &Path, table: &str) -> bool {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

fn sample_track(path: &str) -> TrackFeatures {
    TrackFeatures {
        filepath: PathBuf::from(path),
        title: "Strobe".to_string(),
        artist: "deadmau5".to_string(),
        bpm: Some(128.0),
        ..Default::default()
    }
}

#[test]
fn test_fresh_database_is_migrated_to_current_schema_version() {
    common::isolate();
    let dir = scratch_dir("fresh");
    let path = dir.join("track_analysis.db");
    let db = AnalysisDatabase::open_at(&path).unwrap();
    db.upsert_track(&sample_track("/music/a.flac")).unwrap();
    drop(db);

    assert_eq!(user_version(&path), CURRENT_SCHEMA_VERSION);
    assert!(table_exists(&path, "track_features"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_reopening_database_preserves_rows_and_uses_wal() {
    common::isolate();
    let dir = scratch_dir("reopen");
    let path = dir.join("track_analysis.db");
    {
        let db = AnalysisDatabase::open_at(&path).unwrap();
        db.upsert_track(&sample_track("/music/a.flac")).unwrap();
    }
    let db = AnalysisDatabase::open_at(&path).unwrap();
    assert_eq!(db.count_analyzed(), 1);
    assert_eq!(db.get_track(Path::new("/music/a.flac")).unwrap().title, "Strobe");

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let _ = fs::remove_dir_all(&dir);
}

/// A database created by the pre-#18 build (v1 schema, rollback journal, no WAL) must open
/// unchanged and keep its rows.
#[test]
fn test_legacy_v1_database_opens_without_data_loss() {
    common::isolate();
    let dir = scratch_dir("legacy");
    let path = dir.join("track_analysis.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE track_features (
                filepath TEXT PRIMARY KEY, mtime INTEGER NOT NULL, file_size INTEGER NOT NULL,
                title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT NOT NULL, genre TEXT NOT NULL,
                year INTEGER, bpm REAL, initial_key TEXT, camelot_key TEXT, energy REAL,
                loudness_lufs REAL, mood TEXT, tags_json TEXT NOT NULL DEFAULT '[]',
                analyzed_at INTEGER NOT NULL
            );
            INSERT INTO track_features VALUES
                ('/music/legacy.flac', 1, 2, 'Legacy', 'Old Artist', 'Old Album', 'House',
                 NULL, 124.0, 'Am', '8A', 0.7, NULL, NULL, '[]', 3);
            PRAGMA user_version = 1;
            "#,
        )
        .unwrap();
    }

    let db = AnalysisDatabase::open_at(&path).unwrap();
    let legacy = db
        .get_track(Path::new("/music/legacy.flac"))
        .expect("legacy row survives");
    assert_eq!(legacy.title, "Legacy");
    assert_eq!(legacy.bpm, Some(124.0));
    drop(db);
    assert_eq!(user_version(&path), CURRENT_SCHEMA_VERSION);
    let _ = fs::remove_dir_all(&dir);
}

/// A database written by a newer quickstream (higher `user_version`) must be refused with a
/// clear, typed error and left byte-for-byte untouched.
#[test]
fn test_database_from_newer_build_is_refused_untouched() {
    common::isolate();
    let dir = scratch_dir("newer");
    let path = dir.join("track_analysis.db");
    let future = CURRENT_SCHEMA_VERSION + 7;
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE future_table (x INTEGER); PRAGMA user_version = {};",
            future
        ))
        .unwrap();
    }
    let before = fs::read(&path).unwrap();

    let err = match AnalysisDatabase::open_at(&path) {
        Ok(_) => panic!("a database from a newer build must not be opened"),
        Err(e) => e,
    };
    let typed = err
        .downcast_ref::<SchemaTooNewError>()
        .unwrap_or_else(|| panic!("expected SchemaTooNewError, got: {err:#}"));
    assert_eq!(typed.found, future);
    assert_eq!(typed.supported, CURRENT_SCHEMA_VERSION);
    let msg = err.to_string();
    assert!(
        msg.contains(&future.to_string()) && msg.contains("newer"),
        "unclear error: {msg}"
    );

    assert_eq!(fs::read(&path).unwrap(), before, "newer database must not be modified");
    assert!(
        !table_exists(&path, "track_features"),
        "no tables may be created in a newer database"
    );
    assert!(
        !dir.join("track_analysis.db-wal").exists(),
        "journal mode must not be switched"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The app opens two handles (autopilot scanner + UI). Both must be able to use the same file
/// concurrently without `database is locked` errors.
#[test]
fn test_two_handles_on_same_file_can_write_concurrently() {
    common::isolate();
    let dir = scratch_dir("two_handles");
    let path = dir.join("track_analysis.db");
    let a = AnalysisDatabase::open_at(&path).unwrap();
    let b = AnalysisDatabase::open_at(&path).unwrap();

    let writer = |db: AnalysisDatabase, prefix: &'static str| {
        std::thread::spawn(move || {
            for i in 0..200 {
                db.upsert_track(&sample_track(&format!("/music/{prefix}/{i}.flac")))
                    .expect("concurrent upsert must not fail with SQLITE_BUSY");
            }
        })
    };
    let ta = writer(a.clone(), "a");
    let tb = writer(b.clone(), "b");
    ta.join().unwrap();
    tb.join().unwrap();

    assert_eq!(a.count_analyzed(), 400);
    assert_eq!(b.count_analyzed(), 400);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Schema v2: library index (#41)
// ---------------------------------------------------------------------------

use quickstream::analysis::db::IndexedDir;
use quickstream::analysis::worker::row_is_current;

/// The v1 schema as the pre-#41 build created it, with one fully analysed row.
fn write_v1_fixture(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE track_features (
            filepath TEXT PRIMARY KEY, mtime INTEGER NOT NULL, file_size INTEGER NOT NULL,
            title TEXT NOT NULL, artist TEXT NOT NULL, album TEXT NOT NULL, genre TEXT NOT NULL,
            year INTEGER, bpm REAL, initial_key TEXT, camelot_key TEXT, energy REAL,
            loudness_lufs REAL, mood TEXT, tags_json TEXT NOT NULL DEFAULT '[]',
            analyzed_at INTEGER NOT NULL
        );
        CREATE INDEX idx_features_bpm ON track_features(bpm);
        CREATE INDEX idx_features_genre ON track_features(genre);
        CREATE INDEX idx_features_artist ON track_features(artist);
        CREATE INDEX idx_features_mood ON track_features(mood);
        CREATE INDEX idx_features_energy ON track_features(energy);
        INSERT INTO track_features VALUES
            ('/music/legacy.flac', 11, 22, 'Legacy', 'Old Artist', 'Old Album', 'House',
             NULL, 124.0, 'Am', '8A', 0.7, NULL, NULL, '[]', 3);
        PRAGMA user_version = 1;
        "#,
    )
    .unwrap();
}

fn names(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    out.sort();
    out
}

fn columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    names(conn, &format!("SELECT name FROM pragma_table_info('{table}')"))
}

/// A tag-only index row for `path` stamped (`mtime`, `size`).
fn index_row(path: &str, mtime: u64, size: u64, bpm: Option<f64>) -> TrackFeatures {
    TrackFeatures {
        filepath: PathBuf::from(path),
        mtime,
        file_size: size,
        title: format!("Tagged {path}"),
        artist: "Index Artist".to_string(),
        album: "Index Album".to_string(),
        genre: "Ambient".to_string(),
        bpm,
        needs_acoustic: bpm.is_none(),
        analyzed_at: 50,
        ..Default::default()
    }
}

fn dir_record(dir: &str, files: usize, complete: bool) -> IndexedDir {
    IndexedDir {
        dirpath: PathBuf::from(dir),
        audio_files: files,
        complete,
        walked_at: 77,
    }
}

/// A v1 database opens at version 2: its row is kept, reads `needs_acoustic == false` (every v1
/// row is a full analysis) and stays current for the worker.
#[test]
fn test_v1_database_migrates_to_v2_keeping_rows_current() {
    common::isolate();
    let dir = scratch_dir("v1_to_v2");
    let path = dir.join("track_analysis.db");
    write_v1_fixture(&path);

    let db = AnalysisDatabase::open_at(&path).unwrap();
    assert_eq!(CURRENT_SCHEMA_VERSION, 2);
    let legacy = db.get_track(Path::new("/music/legacy.flac")).expect("row kept");
    assert_eq!(legacy.title, "Legacy");
    assert_eq!(legacy.bpm, Some(124.0));
    assert_eq!(legacy.energy, Some(0.7));
    assert!(!legacy.needs_acoustic);
    assert!(row_is_current(&legacy, (11, 22)));
    assert!(!row_is_current(&legacy, (12, 22)));
    drop(db);
    assert_eq!(user_version(&path), 2);
    let _ = fs::remove_dir_all(&dir);
}

/// The v2 step adds the `needs_acoustic` column and the `library_index_dirs` table, and nothing
/// else: no index on track_features, no other table.
#[test]
fn test_v2_step_adds_only_the_column_and_the_table() {
    common::isolate();
    let dir = scratch_dir("v2_shape");
    let path = dir.join("track_analysis.db");
    write_v1_fixture(&path);
    let before = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        (
            columns(&conn, "track_features"),
            names(&conn, "SELECT name FROM sqlite_master WHERE type = 'index'"),
        )
    };
    drop(AnalysisDatabase::open_at(&path).unwrap());

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut expected_columns = before.0.clone();
    expected_columns.push("needs_acoustic".to_string());
    expected_columns.sort();
    assert_eq!(columns(&conn, "track_features"), expected_columns);
    let indexes_after = names(&conn, "SELECT name FROM sqlite_master WHERE type = 'index'");
    let new_indexes: Vec<&String> = indexes_after.iter().filter(|i| !before.1.contains(i)).collect();
    assert!(
        new_indexes
            .iter()
            .all(|i| i.starts_with("sqlite_autoindex_library_index_dirs")),
        "only the new table's primary key index: {new_indexes:?}"
    );
    assert_eq!(
        names(&conn, "SELECT name FROM sqlite_master WHERE type = 'table'"),
        vec!["library_index_dirs".to_string(), "track_features".to_string()]
    );
    assert_eq!(
        columns(&conn, "library_index_dirs"),
        vec!["audio_files", "complete", "dirpath", "subtree_complete", "walked_at"]
    );
    let flag: i64 = conn
        .query_row(
            "SELECT needs_acoustic FROM track_features WHERE filepath = '/music/legacy.flac'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(flag, 0);
    let _ = fs::remove_dir_all(&dir);
}

/// `path()` names the file a database was opened at, so a second connection can open it; an
/// in-memory database has none.
#[test]
fn test_database_path_is_known_for_files_only() {
    common::isolate();
    let dir = scratch_dir("path");
    let path = dir.join("track_analysis.db");
    let db = AnalysisDatabase::open_at(&path).unwrap();
    assert_eq!(db.path(), Some(path.clone()));
    assert_eq!(AnalysisDatabase::open_in_memory().unwrap().path(), None);
    let _ = fs::remove_dir_all(&dir);
}

/// An index write never touches a row that is current for the file's stamp, whoever wrote it.
#[test]
fn test_index_write_never_touches_a_current_row() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let analysed = TrackFeatures {
        filepath: PathBuf::from("/music/a.flac"),
        mtime: 5,
        file_size: 6,
        title: "Analysed".to_string(),
        bpm: Some(101.0),
        energy: Some(0.4),
        analyzed_at: 9,
        ..Default::default()
    };
    db.upsert_track(&analysed).unwrap();

    let written = db
        .write_index_batch(&[index_row("/music/a.flac", 5, 6, None)], None)
        .unwrap();
    assert_eq!(written, 0, "a current row is not rewritten");
    let row = db.get_track(Path::new("/music/a.flac")).unwrap();
    assert_eq!(row, analysed);

    // A pending index row that is current is left alone too.
    db.write_index_batch(&[index_row("/music/b.flac", 1, 2, None)], None)
        .unwrap();
    let mut again = index_row("/music/b.flac", 1, 2, Some(90.0));
    again.title = "Changed".to_string();
    assert_eq!(db.write_index_batch(&[again], None).unwrap(), 0);
    let b = db.get_track(Path::new("/music/b.flac")).unwrap();
    assert!(b.needs_acoustic);
    assert_eq!(b.bpm, None);
}

/// Over a stale row (the file changed), an index write takes the new tags and stamp, keeps the
/// old BPM and energy provisionally and flags the row for re-analysis. A BPM tag replaces the
/// BPM and drops the energy measured for the old file, and needs no acoustic pass.
#[test]
fn test_index_write_over_a_stale_row_keeps_provisional_acoustics_and_flags_it() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let old = TrackFeatures {
        filepath: PathBuf::from("/music/a.flac"),
        mtime: 5,
        file_size: 6,
        title: "Old".to_string(),
        bpm: Some(101.0),
        energy: Some(0.4),
        analyzed_at: 9,
        ..Default::default()
    };
    db.upsert_track(&old).unwrap();
    db.upsert_track(&TrackFeatures {
        filepath: PathBuf::from("/music/b.flac"),
        ..old.clone()
    })
    .unwrap();

    let written = db
        .write_index_batch(
            &[
                index_row("/music/a.flac", 7, 6, None),
                index_row("/music/b.flac", 5, 8, Some(128.0)),
            ],
            None,
        )
        .unwrap();
    assert_eq!(written, 2);

    let a = db.get_track(Path::new("/music/a.flac")).unwrap();
    assert_eq!((a.mtime, a.file_size), (7, 6));
    assert_eq!(a.title, "Tagged /music/a.flac");
    assert_eq!(a.bpm, Some(101.0), "old BPM kept provisionally");
    assert_eq!(a.energy, Some(0.4), "old energy kept provisionally");
    assert!(a.needs_acoustic);
    assert!(
        !row_is_current(&a, (7, 6)),
        "a flagged row is not current to the worker"
    );

    let b = db.get_track(Path::new("/music/b.flac")).unwrap();
    assert_eq!(b.bpm, Some(128.0));
    assert_eq!(b.energy, None, "energy measured for the old file is dropped");
    assert!(!b.needs_acoustic);
    assert!(row_is_current(&b, (5, 8)));
}

/// Unknown BPM, energy and year are stored as NULL, never 0: an energy of 0.0 would pass the
/// Chill and Focus energy limits for every row.
#[test]
fn test_index_write_stores_unknown_values_as_null() {
    common::isolate();
    let dir = scratch_dir("nulls");
    let path = dir.join("track_analysis.db");
    let db = AnalysisDatabase::open_at(&path).unwrap();
    db.write_index_batch(&[index_row("/music/n.flac", 1, 2, None)], None)
        .unwrap();
    drop(db);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let types: (String, String, String, i64) = conn
        .query_row(
            "SELECT typeof(bpm), typeof(energy), typeof(year), needs_acoustic \
             FROM track_features WHERE filepath = '/music/n.flac'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(types, ("null".to_string(), "null".to_string(), "null".to_string(), 1));
    let _ = fs::remove_dir_all(&dir);
}

/// A full analysis always replaces a tag-only row and clears its flag.
#[test]
fn test_upsert_track_clears_the_acoustic_flag() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    db.write_index_batch(&[index_row("/music/a.flac", 1, 2, None)], None)
        .unwrap();
    assert!(db.get_track(Path::new("/music/a.flac")).unwrap().needs_acoustic);

    let mut analysed = index_row("/music/a.flac", 1, 2, None);
    analysed.bpm = Some(88.0);
    analysed.energy = Some(0.3);
    // Even a struct that still carries the flag is stored as a full analysis.
    db.upsert_track(&analysed).unwrap();
    let row = db.get_track(Path::new("/music/a.flac")).unwrap();
    assert!(!row.needs_acoustic);
    assert_eq!((row.bpm, row.energy), (Some(88.0), Some(0.3)));
    assert!(row_is_current(&row, (1, 2)));
}

/// `tracks_under` takes exactly the folder's own rows (a byte range on the key, never LIKE):
/// siblings that share its name as a prefix are left out, a trailing slash changes nothing, and
/// rows deeper than `max_depth` are dropped.
#[test]
fn test_tracks_under_takes_only_the_folders_own_rows() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let paths = [
        "/m/Jazz/a.flac",
        "/m/Jazz/Miles/Kind/b.flac",
        "/m/Jazz/Miles/Kind/Disc 1/c.flac",
        "/m/Jazz/1/2/3/d.flac",
        "/m/Jazz Fusion/e.flac",
        "/m/Jazz-Funk/f.flac",
        "/m/Jazz0/g.flac",
        "/m/Jazz.flac",
        "/m/jazz/h.flac",
        "/m/Jaz/i.flac",
    ];
    let rows: Vec<TrackFeatures> = paths.iter().map(|p| index_row(p, 1, 2, None)).collect();
    db.write_index_batch(&rows, None).unwrap();

    let under = |dir: &str, depth: usize| -> Vec<String> {
        let mut found: Vec<String> = db
            .tracks_under(Path::new(dir), depth)
            .into_iter()
            .map(|t| t.filepath.to_string_lossy().to_string())
            .collect();
        found.sort();
        found
    };
    assert_eq!(
        under("/m/Jazz", 3),
        vec!["/m/Jazz/Miles/Kind/b.flac", "/m/Jazz/a.flac"],
        "siblings and rows deeper than 3 are left out"
    );
    assert_eq!(under("/m/Jazz/", 3), under("/m/Jazz", 3));
    assert_eq!(under("/m/Jazz", 1), vec!["/m/Jazz/a.flac"]);
    assert_eq!(under("/m/Jazz", 4).len(), 4);
    assert_eq!(under("/m/Jazz Fusion", 3), vec!["/m/Jazz Fusion/e.flac"]);
    assert!(under("/m/Nothing", 3).is_empty());
}

/// `tracks_under` leaves out the rows the mix's walk would skip: any name below the folder that
/// starts with `.` (hidden folders, AppleDouble `._` files), `#recycle` and `$RECYCLE.BIN`, so a
/// mix never queues a recycled or hidden file from the index (PR #145 review).
#[test]
fn test_tracks_under_skips_what_the_walk_skips() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let paths = [
        "/m/Jazz/Album/a.flac",
        "/m/Jazz/.hidden/b.flac",
        "/m/Jazz/Album/._a.flac",
        "/m/Jazz/#recycle/c.flac",
        "/m/Jazz/Album/#recycle/d.flac",
        "/m/Jazz/$RECYCLE.BIN/e.flac",
        "/m/Jazz/Album.v2/f.flac",
        "/m/Jazz/recycle/g.flac",
    ];
    let rows: Vec<TrackFeatures> = paths.iter().map(|p| index_row(p, 1, 2, None)).collect();
    db.write_index_batch(&rows, None).unwrap();
    let mut found: Vec<String> = db
        .tracks_under(Path::new("/m/Jazz"), 3)
        .into_iter()
        .map(|t| t.filepath.to_string_lossy().to_string())
        .collect();
    found.sort();
    assert_eq!(
        found,
        vec![
            "/m/Jazz/Album.v2/f.flac",
            "/m/Jazz/Album/a.flac",
            "/m/Jazz/recycle/g.flac"
        ]
    );
    // The folder asked for may itself be hidden: only the names below it are checked.
    db.write_index_batch(&[index_row("/m/.Jazz/h.flac", 1, 2, None)], None)
        .unwrap();
    assert_eq!(db.tracks_under(Path::new("/m/.Jazz"), 3).len(), 1);
}

/// Over a stale row, an index write also clears what only the acoustic pass or tag enrichment
/// measured for the old file (loudness, mood) and takes the new row's tags list, so a mood
/// never matches a changed file on the old file's tags (PR #145 review).
#[test]
fn test_index_write_over_a_stale_row_clears_the_old_files_mood_and_tags() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    let old = TrackFeatures {
        filepath: PathBuf::from("/music/a.flac"),
        mtime: 5,
        file_size: 6,
        title: "Old".to_string(),
        bpm: Some(101.0),
        energy: Some(0.4),
        loudness_lufs: Some(-9.5),
        mood: Some("chill".to_string()),
        tags_json: r#"["chill","ambient"]"#.to_string(),
        analyzed_at: 9,
        ..Default::default()
    };
    db.upsert_track(&old).unwrap();
    db.upsert_track(&TrackFeatures {
        filepath: PathBuf::from("/music/b.flac"),
        ..old.clone()
    })
    .unwrap();

    let mut tagged = index_row("/music/b.flac", 7, 6, Some(128.0));
    tagged.tags_json = r#"["house"]"#.to_string();
    db.write_index_batch(&[index_row("/music/a.flac", 7, 6, None), tagged], None)
        .unwrap();

    let a = db.get_track(Path::new("/music/a.flac")).unwrap();
    assert_eq!((a.loudness_lufs, a.mood.as_deref()), (None, None));
    assert_eq!(a.tags_json, "[]");
    assert_eq!(a.bpm, Some(101.0), "the BPM is still kept provisionally");
    let b = db.get_track(Path::new("/music/b.flac")).unwrap();
    assert_eq!((b.loudness_lufs, b.mood.as_deref()), (None, None));
    assert_eq!(b.tags_json, r#"["house"]"#);

    // A current row keeps all of it.
    db.upsert_track(&old).unwrap();
    db.write_index_batch(&[index_row("/music/a.flac", 5, 6, None)], None)
        .unwrap();
    assert_eq!(db.get_track(Path::new("/music/a.flac")).unwrap(), old);
}

/// A folder's record commits in the same transaction as its rows: when the batch fails, neither
/// the rows nor the record are written, so a folder is never marked complete without its rows.
#[test]
fn test_folder_record_commits_together_with_its_rows() {
    common::isolate();
    let dir = scratch_dir("dir_record");
    let path = dir.join("track_analysis.db");
    let db = AnalysisDatabase::open_at(&path).unwrap();

    db.write_index_batch(
        &[
            index_row("/m/A/1.flac", 1, 2, None),
            index_row("/m/A/2.flac", 1, 2, None),
        ],
        Some(&dir_record("/m/A", 2, true)),
    )
    .unwrap();
    let (record, subtree) = db.index_dir(Path::new("/m/A")).expect("record written");
    assert_eq!(record, dir_record("/m/A", 2, true));
    assert!(!subtree, "subtree completion is set on its own");
    assert_eq!(db.index_dir(Path::new("/m/A/")).map(|r| r.0), Some(record));

    // A row that fails (a trigger stands in for a write error) rolls the whole batch back.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_bad BEFORE INSERT ON track_features WHEN NEW.filepath LIKE '%bad.flac' \
             BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
        .unwrap();
    }
    let failed = db.write_index_batch(
        &[
            index_row("/m/B/1.flac", 1, 2, None),
            index_row("/m/B/bad.flac", 1, 2, None),
        ],
        Some(&dir_record("/m/B", 2, true)),
    );
    assert!(failed.is_err());
    assert!(db.get_track(Path::new("/m/B/1.flac")).is_none());
    assert!(db.index_dir(Path::new("/m/B")).is_none());
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

/// `indexed_subtrees` names the folders whose whole subtree is indexed; a folder written again
/// as incomplete loses its subtree mark. `index_stamps` gives the stamps of the rows it has.
#[test]
fn test_indexed_subtrees_and_index_stamps() {
    common::isolate();
    let db = AnalysisDatabase::open_in_memory().unwrap();
    db.write_index_batch(
        &[index_row("/m/A/1.flac", 3, 4, None)],
        Some(&dir_record("/m/A", 1, true)),
    )
    .unwrap();
    db.write_index_batch(&[], Some(&dir_record("/m/B", 0, true))).unwrap();
    assert!(db.set_subtree_complete(Path::new("/m/A"), true).unwrap());
    assert!(db.set_subtree_complete(Path::new("/m/B/"), true).unwrap());
    assert!(
        !db.set_subtree_complete(Path::new("/m/C"), true).unwrap(),
        "no record, nothing set"
    );

    let asked = vec![PathBuf::from("/m/A"), PathBuf::from("/m/B"), PathBuf::from("/m/C")];
    let indexed = db.indexed_subtrees(&asked);
    assert_eq!(indexed.len(), 2);
    // Each with when its record was walked.
    assert_eq!(indexed.get(Path::new("/m/A")), Some(&77));
    assert_eq!(indexed.get(Path::new("/m/B")), Some(&77));

    // Written again as incomplete (a read failed), the folder's subtree is no longer complete.
    db.write_index_batch(&[], Some(&dir_record("/m/B", 1, false))).unwrap();
    let indexed = db.indexed_subtrees(&asked);
    assert_eq!(indexed.len(), 1);
    assert!(indexed.contains_key(Path::new("/m/A")));

    let stamps = db.index_stamps(&[PathBuf::from("/m/A/1.flac"), PathBuf::from("/m/A/none.flac")]);
    assert_eq!(stamps.len(), 1);
    assert_eq!(stamps.get("/m/A/1.flac"), Some(&(3, 4)));
}
