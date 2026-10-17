//! SQLite storage for track features, acoustic metrics, and mix vectors.
//! Strictly isolated inside ~/.cache/quickstream/track_analysis.db (never touches user library).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use rusqlite::{params, Connection, Result, TransactionBehavior};
use serde::{Deserialize, Serialize};

/// Current database schema version (tracked via PRAGMA user_version).
/// Always equal to `MIGRATIONS.len()` (checked at compile time below).
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// How long a connection waits on a lock held by another handle before failing with
/// `SQLITE_BUSY`. The app opens more than one handle (autopilot scanner and UI).
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Ordered schema migrations. `MIGRATIONS[i]` upgrades a database from `user_version = i` to
/// `i + 1`; each step runs in its own transaction together with its `user_version` bump.
///
/// To change the schema, append a step (e.g. `ALTER TABLE track_features ADD COLUMN ...`) and
/// bump `CURRENT_SCHEMA_VERSION`. Never edit or remove an existing step, and never drop user
/// tables. Steps must not change `journal_mode` (not allowed inside a transaction).
const MIGRATIONS: &[&str] = &[
    // v1: initial track_features table and indexes.
    r#"
    CREATE TABLE IF NOT EXISTS track_features (
        filepath TEXT PRIMARY KEY,
        mtime INTEGER NOT NULL,
        file_size INTEGER NOT NULL,
        title TEXT NOT NULL,
        artist TEXT NOT NULL,
        album TEXT NOT NULL,
        genre TEXT NOT NULL,
        year INTEGER,
        bpm REAL,
        initial_key TEXT,
        camelot_key TEXT,
        energy REAL,
        loudness_lufs REAL,
        mood TEXT,
        tags_json TEXT NOT NULL DEFAULT '[]',
        analyzed_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_features_bpm ON track_features(bpm);
    CREATE INDEX IF NOT EXISTS idx_features_genre ON track_features(genre);
    CREATE INDEX IF NOT EXISTS idx_features_artist ON track_features(artist);
    CREATE INDEX IF NOT EXISTS idx_features_mood ON track_features(mood);
    CREATE INDEX IF NOT EXISTS idx_features_energy ON track_features(energy);
    "#,
];

const _: () = assert!(MIGRATIONS.len() as u32 == CURRENT_SCHEMA_VERSION);

/// Returned when the database on disk was written by a newer quickstream build.
#[derive(Debug, thiserror::Error)]
#[error(
    "analysis database {path} has schema version {found}, which is newer than this build supports \
     (version {supported}); refusing to open it so it is not damaged. Upgrade quickstream, or move \
     the file aside to rebuild the analysis cache"
)]
pub struct SchemaTooNewError {
    pub path: String,
    pub found: u32,
    pub supported: u32,
}

/// Complete audio features and acoustic vector for intelligent mixing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackFeatures {
    pub filepath: PathBuf,
    pub mtime: u64,
    pub file_size: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: Option<u32>,
    pub bpm: Option<f64>,
    pub initial_key: Option<String>,
    pub camelot_key: Option<String>,
    pub energy: Option<f64>,        // 0.0 (very low energy / ambient) to 1.0 (very energetic / peak)
    pub loudness_lufs: Option<f64>, // Integrated LUFS
    pub mood: Option<String>,       // "chill", "workout", "focus", "groove"
    pub tags_json: String,          // JSON array of community / style tags
    pub analyzed_at: u64,
}

impl Default for TrackFeatures {
    fn default() -> Self {
        Self {
            filepath: PathBuf::new(),
            mtime: 0,
            file_size: 0,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            year: None,
            bpm: None,
            initial_key: None,
            camelot_key: None,
            energy: None,
            loudness_lufs: None,
            mood: None,
            tags_json: "[]".to_string(),
            analyzed_at: 0,
        }
    }
}

impl TrackFeatures {
    pub fn display_key(&self) -> Option<String> {
        match (&self.camelot_key, &self.initial_key) {
            (Some(c), Some(k)) => Some(format!("{} ({})", k, c)),
            (Some(c), None) => Some(c.clone()),
            (None, Some(k)) => Some(k.clone()),
            (None, None) => None,
        }
    }
}

fn read_user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn ensure_supported(found: u32, supported: u32, label: &str) -> std::result::Result<(), SchemaTooNewError> {
    if found > supported {
        return Err(SchemaTooNewError {
            path: label.to_string(),
            found,
            supported,
        });
    }
    Ok(())
}

/// Apply `steps[user_version..]` in order. Each step and its `user_version` bump commit
/// atomically, so a failing step leaves the database at the previous version.
fn apply_migrations(conn: &mut Connection, steps: &[&str], label: &str) -> anyhow::Result<()> {
    use anyhow::Context;

    let target = steps.len() as u32;
    if read_user_version(conn)? == target {
        return Ok(());
    }
    loop {
        // IMMEDIATE takes the write lock up front and the version is re-read inside the
        // transaction, so two handles opening the same fresh file never run a step twice.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version = read_user_version(&tx)?;
        ensure_supported(version, target, label)?;
        if version == target {
            tx.commit()?;
            return Ok(());
        }
        tx.execute_batch(steps[version as usize])
            .with_context(|| format!("migrating analysis database {label} to schema version {}", version + 1))?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
}

/// Thread-safe handle to the local analysis SQLite database.
#[derive(Clone)]
pub struct AnalysisDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl AnalysisDatabase {
    /// Open the standard user cache database at ~/.cache/quickstream/track_analysis.db.
    pub fn open_default() -> anyhow::Result<Self> {
        let cache_dir = crate::paths::get_cache_dir();
        std::fs::create_dir_all(&cache_dir)?;
        let db_path = cache_dir.join("track_analysis.db");
        Self::open_at(&db_path)
    }

    /// Open an in-memory database (ideal for unit and integration testing).
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, ":memory:")
    }

    /// Open or create SQLite database at a specific path.
    ///
    /// Fails with [`SchemaTooNewError`] (leaving the file untouched) if the database was written
    /// by a newer quickstream build.
    pub fn open_at(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn, &path.display().to_string())
    }

    /// Configure a freshly opened connection and bring its schema up to date.
    fn from_connection(mut conn: Connection, label: &str) -> anyhow::Result<Self> {
        // Per-connection settings: must be applied on every open, not only on first creation.
        conn.busy_timeout(BUSY_TIMEOUT)?;

        // Refuse a newer schema before changing anything persistent (journal mode is stored in
        // the file header).
        let version = read_user_version(&conn)?;
        ensure_supported(version, CURRENT_SCHEMA_VERSION, label)?;

        // journal_mode reports the resulting mode as a row ("wal", or "memory" in-memory).
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        apply_migrations(&mut conn, MIGRATIONS, label)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Upsert track analysis features.
    pub fn upsert_track(&self, features: &TrackFeatures) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO track_features (
                filepath, mtime, file_size, title, artist, album, genre,
                year, bpm, initial_key, camelot_key, energy, loudness_lufs,
                mood, tags_json, analyzed_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16
            )
            ON CONFLICT(filepath) DO UPDATE SET
                mtime = excluded.mtime,
                file_size = excluded.file_size,
                title = excluded.title,
                artist = excluded.artist,
                album = excluded.album,
                genre = excluded.genre,
                year = excluded.year,
                bpm = excluded.bpm,
                initial_key = excluded.initial_key,
                camelot_key = excluded.camelot_key,
                energy = excluded.energy,
                loudness_lufs = excluded.loudness_lufs,
                mood = excluded.mood,
                tags_json = excluded.tags_json,
                analyzed_at = excluded.analyzed_at;
            "#,
            params![
                features.filepath.to_string_lossy().to_string(),
                features.mtime as i64,
                features.file_size as i64,
                features.title,
                features.artist,
                features.album,
                features.genre,
                features.year,
                features.bpm,
                features.initial_key,
                features.camelot_key,
                features.energy,
                features.loudness_lufs,
                features.mood,
                features.tags_json,
                features.analyzed_at as i64,
            ],
        )?;
        Ok(())
    }

    /// Retrieve track features by filepath.
    pub fn get_features(&self, path: &Path) -> Option<TrackFeatures> {
        self.get_track(path)
    }

    /// Retrieve track features by filepath.
    pub fn get_track(&self, path: &Path) -> Option<TrackFeatures> {
        let conn = self.conn.lock().unwrap();
        let path_str = path.to_string_lossy().to_string();
        let mut stmt = conn.prepare(
            r#"
            SELECT filepath, mtime, file_size, title, artist, album, genre,
                   year, bpm, initial_key, camelot_key, energy, loudness_lufs,
                   mood, tags_json, analyzed_at
            FROM track_features
            WHERE filepath = ?1
            "#,
        ).ok()?;

        stmt.query_row(params![path_str], |row| {
            Ok(TrackFeatures {
                filepath: PathBuf::from(row.get::<_, String>(0)?),
                mtime: row.get::<_, i64>(1)? as u64,
                file_size: row.get::<_, i64>(2)? as u64,
                title: row.get(3)?,
                artist: row.get(4)?,
                album: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                bpm: row.get(8)?,
                initial_key: row.get(9)?,
                camelot_key: row.get(10)?,
                energy: row.get(11)?,
                loudness_lufs: row.get(12)?,
                mood: row.get(13)?,
                tags_json: row.get(14)?,
                analyzed_at: row.get::<_, i64>(15)? as u64,
            })
        }).ok()
    }

    /// Query tracks by tempo / BPM window.
    pub fn query_bpm_range(&self, min_bpm: f64, max_bpm: f64, limit: usize) -> Vec<TrackFeatures> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            r#"
            SELECT filepath, mtime, file_size, title, artist, album, genre,
                   year, bpm, initial_key, camelot_key, energy, loudness_lufs,
                   mood, tags_json, analyzed_at
            FROM track_features
            WHERE bpm IS NOT NULL AND bpm >= ?1 AND bpm <= ?2
            ORDER BY RANDOM()
            LIMIT ?3
            "#,
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = match stmt.query_map(params![min_bpm, max_bpm, limit as i64], |row| {
            Ok(TrackFeatures {
                filepath: PathBuf::from(row.get::<_, String>(0)?),
                mtime: row.get::<_, i64>(1)? as u64,
                file_size: row.get::<_, i64>(2)? as u64,
                title: row.get(3)?,
                artist: row.get(4)?,
                album: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                bpm: row.get(8)?,
                initial_key: row.get(9)?,
                camelot_key: row.get(10)?,
                energy: row.get(11)?,
                loudness_lufs: row.get(12)?,
                mood: row.get(13)?,
                tags_json: row.get(14)?,
                analyzed_at: row.get::<_, i64>(15)? as u64,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    /// Query tracks by mood (e.g. "chill", "workout", "focus", "groove").
    pub fn query_by_mood(&self, mood: &str, limit: usize) -> Vec<TrackFeatures> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            r#"
            SELECT filepath, mtime, file_size, title, artist, album, genre,
                   year, bpm, initial_key, camelot_key, energy, loudness_lufs,
                   mood, tags_json, analyzed_at
            FROM track_features
            WHERE mood = ?1 OR tags_json LIKE ?2
            ORDER BY RANDOM()
            LIMIT ?3
            "#,
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let tag_pattern = format!("%{}%", mood);
        let rows = match stmt.query_map(params![mood, tag_pattern, limit as i64], |row| {
            Ok(TrackFeatures {
                filepath: PathBuf::from(row.get::<_, String>(0)?),
                mtime: row.get::<_, i64>(1)? as u64,
                file_size: row.get::<_, i64>(2)? as u64,
                title: row.get(3)?,
                artist: row.get(4)?,
                album: row.get(5)?,
                genre: row.get(6)?,
                year: row.get(7)?,
                bpm: row.get(8)?,
                initial_key: row.get(9)?,
                camelot_key: row.get(10)?,
                energy: row.get(11)?,
                loudness_lufs: row.get(12)?,
                mood: row.get(13)?,
                tags_json: row.get(14)?,
                analyzed_at: row.get::<_, i64>(15)? as u64,
            })
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(|r| r.ok()).collect()
    }

    /// Total number of tracks indexed in local database.
    pub fn count_analyzed(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM track_features", [], |row| row.get(0))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_features_database_roundtrip() {
        let db = AnalysisDatabase::open_in_memory().unwrap();
        assert_eq!(db.count_analyzed(), 0);

        let f = TrackFeatures {
            filepath: PathBuf::from("/music/electronic/track1.flac"),
            title: "Strobe".to_string(),
            artist: "deadmau5".to_string(),
            bpm: Some(128.0),
            initial_key: Some("Am".to_string()),
            camelot_key: Some("8A".to_string()),
            energy: Some(0.85),
            mood: Some("workout".to_string()),
            ..Default::default()
        };

        db.upsert_track(&f).unwrap();
        assert_eq!(db.count_analyzed(), 1);

        let retrieved = db.get_track(&f.filepath).unwrap();
        assert_eq!(retrieved.title, "Strobe");
        assert_eq!(retrieved.bpm, Some(128.0));
        assert_eq!(retrieved.camelot_key.as_deref(), Some("8A"));

        // Query BPM
        let matches = db.query_bpm_range(125.0, 130.0, 10);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].title, "Strobe");

        // Query outside range
        let misses = db.query_bpm_range(70.0, 90.0, 10);
        assert_eq!(misses.len(), 0);

        // Query mood
        let mood_matches = db.query_by_mood("workout", 10);
        assert_eq!(mood_matches.len(), 1);
    }

    fn scratch_db_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quickstream_db_unit_{}_{}_{}", name, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("track_analysis.db")
    }

    fn pragma_i64(db: &AnalysisDatabase, name: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0)).unwrap()
    }

    /// `synchronous` and `busy_timeout` are per-connection settings, so they must be applied on
    /// every open, not only when the v1 migration runs on a brand-new file.
    #[test]
    fn test_connection_pragmas_applied_on_every_open() {
        let path = scratch_db_path("pragmas");
        drop(AnalysisDatabase::open_at(&path).unwrap()); // creates + migrates

        let reopened = AnalysisDatabase::open_at(&path).unwrap(); // no migration runs now
        assert_eq!(pragma_i64(&reopened, "synchronous"), 1, "synchronous must be NORMAL (1)");
        assert_eq!(pragma_i64(&reopened, "busy_timeout"), BUSY_TIMEOUT.as_millis() as i64);
        let mode: String = reopened
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let mem = AnalysisDatabase::open_in_memory().unwrap();
        assert_eq!(pragma_i64(&mem, "synchronous"), 1);
        assert_eq!(pragma_i64(&mem, "busy_timeout"), BUSY_TIMEOUT.as_millis() as i64);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_current_schema_version_matches_migration_steps() {
        assert_eq!(MIGRATIONS.len() as u32, CURRENT_SCHEMA_VERSION);
    }

    /// Each step runs in its own transaction together with its `user_version` bump: a failing
    /// step leaves the database at the previous version with none of the step's changes.
    #[test]
    fn test_failed_migration_step_is_rolled_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        let steps: &[&str] = &[
            "CREATE TABLE step_one (x INTEGER);",
            "CREATE TABLE step_two (y INTEGER); THIS IS NOT SQL;",
        ];
        assert!(apply_migrations(&mut conn, steps, ":memory:").is_err());

        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 1, "step 1 committed, step 2 rolled back");
        let count = |conn: &Connection, name: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(&conn, "step_one"), 1);
        assert_eq!(count(&conn, "step_two"), 0, "partial step must not be left behind");

        // Re-running resumes from the recorded version rather than replaying step 1.
        let fixed: &[&str] = &["CREATE TABLE step_one (x INTEGER);", "CREATE TABLE step_two (y INTEGER);"];
        apply_migrations(&mut conn, fixed, ":memory:").unwrap();
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        assert_eq!(count(&conn, "step_two"), 1);
    }
}
