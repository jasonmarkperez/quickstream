//! Integration tests for the SQLite analysis database lifecycle (issue #18):
//! per-open pragmas, transactional `user_version` migrations, and refusing databases
//! written by a newer build.
//!
//! Every database here lives in a scratch dir under the OS temp dir; nothing touches the
//! user's real `~/.cache/quickstream/track_analysis.db`.

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
    let legacy = db.get_track(Path::new("/music/legacy.flac")).expect("legacy row survives");
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
    assert!(msg.contains(&future.to_string()) && msg.contains("newer"), "unclear error: {msg}");

    assert_eq!(fs::read(&path).unwrap(), before, "newer database must not be modified");
    assert!(!table_exists(&path, "track_features"), "no tables may be created in a newer database");
    assert!(!dir.join("track_analysis.db-wal").exists(), "journal mode must not be switched");
    let _ = fs::remove_dir_all(&dir);
}

/// The app opens two handles (autopilot scanner + UI). Both must be able to use the same file
/// concurrently without `database is locked` errors.
#[test]
fn test_two_handles_on_same_file_can_write_concurrently() {
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
